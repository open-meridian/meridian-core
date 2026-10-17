//! W8 on the memory store, with a stand-in launcher that answers as told and
//! keeps what it was asked.

use std::sync::{Arc, Mutex};

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    CreatePluginReply, CreatePluginRequest, LaunchPluginRequest, PluginCatalogue,
    PluginCatalogueRequest, PluginLaunch, PluginLaunchState, PluginMetadata, PluginVersion,
    RecordPluginUploadRequest, RemovePluginReply, RemovePluginRequest, StopPluginRequest,
};
use prost::Message;

use super::*;
use crate::service::Clock;
use crate::MemoryStore;

const ADA: &str = "local|ada";
const DIGEST: &str = "sha256:3f1c0e7a9b2d4c6e8f0a1b3c5d7e9f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e";

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

#[derive(Default)]
struct Launcher {
    created: Mutex<Vec<CreatePluginRequest>>,
    removed: Mutex<Vec<String>>,
    refuse: Mutex<Option<String>>,
}

fn harness() -> (Arc<Bus>, Arc<Launcher>) {
    harness_with(ArchiveGrant::default())
}

fn harness_with(archive: ArchiveGrant) -> (Arc<Bus>, Arc<Launcher>) {
    let bus = Arc::new(Bus::single(
        "conductor-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    serve_plugins_with(
        Arc::clone(&bus),
        Arc::new(MemoryStore::new()),
        Arc::new(At(1_790_380_800_000_000_000)),
        "localhost:5000".into(),
        archive,
    );
    let launcher = Arc::new(Launcher::default());
    let creating = Arc::clone(&launcher);
    bus.serve(CREATE_PLUGIN, move |envelope| {
        if let Some(refusal) = creating.refuse.lock().unwrap().clone() {
            return Err(refusal);
        }
        let request = CreatePluginRequest::decode(&envelope.payload[..]).unwrap();
        let workload = format!("plugin-{}", request.instance_id);
        creating.created.lock().unwrap().push(request);
        Ok((
            "meridian.v1.CreatePluginReply".into(),
            CreatePluginReply { workload }.encode_to_vec(),
        ))
    });
    let removing = Arc::clone(&launcher);
    bus.serve(REMOVE_PLUGIN, move |envelope| {
        let request = RemovePluginRequest::decode(&envelope.payload[..]).unwrap();
        removing.removed.lock().unwrap().push(request.instance_id);
        Ok((
            "meridian.v1.RemovePluginReply".into(),
            RemovePluginReply { removed: true }.encode_to_vec(),
        ))
    });
    (bus, launcher)
}

async fn ask<Q: Message, A: Message + Default>(
    bus: &Bus,
    topic: &str,
    kind: &str,
    request: Q,
) -> Result<A, String> {
    let (_, bytes) = bus
        .call_for(topic, kind, request.encode_to_vec(), None, None, ADA)
        .await
        .map_err(|failed| failed.to_string())?;
    Ok(A::decode(&bytes[..]).unwrap())
}

fn snaptrade(version: &str) -> RecordPluginUploadRequest {
    RecordPluginUploadRequest {
        metadata: Some(PluginMetadata {
            name: "snaptrade".into(),
            version: version.into(),
            roles: vec!["custody".into()],
            interface: true,
            sdk_version: "0.2.0".into(),
            declaration: None,
        }),
        image_digest: DIGEST.into(),
        documentation: Vec::new(),
        ..Default::default()
    }
}

async fn upload(bus: &Bus, request: RecordPluginUploadRequest) -> Result<PluginVersion, String> {
    ask(
        bus,
        RECORD_PLUGIN_UPLOAD,
        "meridian.v1.RecordPluginUploadRequest",
        request,
    )
    .await
}

fn launching(instance: &str, roles: &[&str]) -> LaunchPluginRequest {
    LaunchPluginRequest {
        name: "snaptrade".into(),
        version: "0.1.0".into(),
        instance_id: instance.into(),
        approved_roles: roles.iter().map(|r| r.to_string()).collect(),
        ..Default::default()
    }
}

async fn launch(bus: &Bus, request: LaunchPluginRequest) -> Result<PluginLaunch, String> {
    ask(
        bus,
        LAUNCH_PLUGIN,
        "meridian.v1.LaunchPluginRequest",
        request,
    )
    .await
}

async fn catalogue(bus: &Bus) -> PluginCatalogue {
    ask(
        bus,
        PLUGIN_CATALOGUE,
        "meridian.v1.PluginCatalogueRequest",
        PluginCatalogueRequest {},
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_version_is_recorded_once_with_who_uploaded_it() {
    let (bus, _) = harness();
    let recorded = upload(&bus, snaptrade("0.1.0")).await.unwrap();
    assert_eq!(recorded.uploaded_by, ADA);
    assert_eq!(recorded.image_digest, DIGEST);
    let again = upload(&bus, snaptrade("0.1.0")).await.unwrap_err();
    assert!(again.contains("already recorded"), "{again}");
    assert_eq!(catalogue(&bus).await.versions.len(), 1, "the first stands");
}

/// One way to spoil an upload.
type Breaking = Box<dyn Fn(&mut RecordPluginUploadRequest)>;

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_is_refused_where_the_chart_carries_no_platform_key() {
    let (bus, _) = harness();
    let mut bundle = snaptrade("1.4.0");
    bundle.listing = "open-meridian/snaptrade".into();
    bundle.source_registry = "community".into();
    let refused = upload(&bus, bundle).await.unwrap_err();
    assert!(refused.contains("carries none"), "{refused}");
    assert!(catalogue(&bus).await.versions.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_is_held_to_the_vocabulary_whatever_the_cli_checked() {
    let (bus, _) = harness();
    let cases: Vec<(&str, Breaking)> = vec![
        (
            "not a role",
            Box::new(|r| r.metadata.as_mut().unwrap().roles = vec!["trader".into()]),
        ),
        (
            "own components",
            Box::new(|r| r.metadata.as_mut().unwrap().roles = vec!["dashboard".into()]),
        ),
        (
            "declared twice",
            Box::new(|r| {
                r.metadata.as_mut().unwrap().roles = vec!["custody".into(), "custody".into()]
            }),
        ),
        (
            "not a plugin's name",
            Box::new(|r| r.metadata.as_mut().unwrap().name = "Snap_Trade".into()),
        ),
        (
            "not a plugin's name",
            Box::new(|r| r.metadata.as_mut().unwrap().name = "snap--trade".into()),
        ),
        (
            "not a version",
            Box::new(|r| r.metadata.as_mut().unwrap().version = "1.0 beta".into()),
        ),
        (
            "not an image digest",
            Box::new(|r| r.image_digest = "latest".into()),
        ),
        (
            "no SDK version",
            Box::new(|r| r.metadata.as_mut().unwrap().sdk_version = String::new()),
        ),
    ];
    for (says, breaking) in cases {
        let mut request = snaptrade("0.1.0");
        breaking(&mut request);
        let refused = upload(&bus, request).await.unwrap_err();
        assert!(refused.contains(says), "{says}: {refused}");
    }
    assert!(catalogue(&bus).await.versions.is_empty());
    // A plugin naming no role is launched with no topics, not refused.
    let mut reference = snaptrade("0.1.0");
    reference.metadata.as_mut().unwrap().roles.clear();
    upload(&bus, reference).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launch_runs_what_was_approved_by_digest_from_the_deployments_registry() {
    let (bus, launcher) = harness();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    let launched = launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap();
    assert_eq!(launched.state, PluginLaunchState::Launched as i32);
    assert_eq!(launched.launched_by, ADA);
    let created = launcher.created.lock().unwrap();
    assert_eq!(
        created[0].image,
        format!("localhost:5000/plugins/snaptrade@{DIGEST}")
    );
    assert_eq!(created[0].roles, vec!["custody"]);
    assert!(created[0].interface);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_of_anything_but_the_declaration_is_refused_before_the_launcher_hears() {
    let (bus, launcher) = harness();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    for (roles, says) in [
        (
            vec!["custody", "reporting"],
            "names roles custody, reporting",
        ),
        (vec![], "names roles none"),
    ] {
        let refused = launch(&bus, launching("snaptrade-1", &roles))
            .await
            .unwrap_err();
        assert!(
            refused.contains(says) && refused.contains("declares"),
            "{refused}"
        );
    }
    let unknown = launch(
        &bus,
        LaunchPluginRequest {
            version: "9.9.9".into(),
            ..launching("snaptrade-1", &["custody"])
        },
    )
    .await
    .unwrap_err();
    assert!(unknown.contains("not in the catalogue"), "{unknown}");
    assert!(launcher.created.lock().unwrap().is_empty());
    // Order aside: an approval is a set.
    let mut two_roles = snaptrade("0.1.1");
    two_roles.metadata.as_mut().unwrap().roles = vec!["custody".into(), "reporting".into()];
    upload(&bus, two_roles).await.unwrap();
    launch(
        &bus,
        LaunchPluginRequest {
            version: "0.1.1".into(),
            ..launching("snaptrade-1", &["reporting", "custody"])
        },
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_instance_runs_once_and_is_free_again_when_stopped() {
    let (bus, launcher) = harness();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap();
    let twice = launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap_err();
    assert!(twice.contains("already launched"), "{twice}");
    // Another instance of the same version is its own.
    launch(&bus, launching("snaptrade-2", &["custody"]))
        .await
        .unwrap();

    let stopped: PluginLaunch = ask(
        &bus,
        STOP_PLUGIN,
        "meridian.v1.StopPluginRequest",
        StopPluginRequest {
            instance_id: "snaptrade-1".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(stopped.state, PluginLaunchState::Stopped as i32);
    assert_eq!(stopped.stopped_by, ADA);
    assert_eq!(*launcher.removed.lock().unwrap(), vec!["snaptrade-1"]);
    let again: Result<PluginLaunch, _> = ask(
        &bus,
        STOP_PLUGIN,
        "meridian.v1.StopPluginRequest",
        StopPluginRequest {
            instance_id: "snaptrade-1".into(),
            ..Default::default()
        },
    )
    .await;
    assert!(again
        .unwrap_err()
        .contains("no launch of snaptrade-1 is live"));

    launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap();
    let catalogue = catalogue(&bus).await;
    assert_eq!(
        catalogue.launches.len(),
        3,
        "each launch is kept, stopped or not"
    );
    assert_eq!(catalogue.versions.len(), 1, "and the version stays");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launch_the_launcher_refuses_is_recorded_as_failed_and_frees_the_instance() {
    let (bus, launcher) = harness();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    *launcher.refuse.lock().unwrap() = Some("a workload for snaptrade-1 already exists".into());
    let refused = launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap_err();
    assert!(refused.contains("already exists"), "{refused}");
    let recorded = &catalogue(&bus).await.launches[0];
    assert_eq!(recorded.state, PluginLaunchState::Failed as i32);
    assert!(recorded.failure.contains("already exists"));

    *launcher.refuse.lock().unwrap() = None;
    launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap();
}

#[test]
fn a_name_is_a_host_label_a_letter_first() {
    for good in ["snaptrade", "snap-trade-2", "a"] {
        assert!(is_name(good), "{good}");
    }
    for bad in [
        "",
        "2fast",
        "-x",
        "x-",
        "a--b",
        "A",
        "a_b",
        "a.b",
        &"a".repeat(64),
    ] {
        assert!(!is_name(bad), "{bad}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_launch_is_recorded_as_live_and_asked_of_the_launcher_as_live() {
    let (bus, launcher) = harness();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    let mut request = launching("snaptrade-1", &["custody"]);
    request.live = true;
    let launched = launch(&bus, request).await.unwrap();
    assert!(launched.live);
    assert!(launcher.created.lock().unwrap()[0].live);
    let held = catalogue(&bus).await;
    assert!(held
        .launches
        .iter()
        .any(|l| l.instance_id == "snaptrade-1" && l.live));

    // And an ordinary launch is not.
    upload(&bus, snaptrade("0.2.0")).await.unwrap();
    let mut request = launching("snaptrade-2", &["custody"]);
    request.version = "0.2.0".into();
    assert!(!launch(&bus, request).await.unwrap().live);
    assert!(!launcher.created.lock().unwrap()[1].live);
}

/// W8.7, contract v16: a deployment admin allows a launched edge instance an
/// archive; it is recorded, and the instance restarted through CreatePlugin
/// carrying it; withdrawn, restarted without it; a later launch carries what
/// stands. Refused with no archive installed, and for an instance at no edge
/// role.
#[tokio::test(flavor = "multi_thread")]
async fn allowing_an_archive_restarts_the_instance_carrying_it() {
    use meridian_domain::v1::{AllowArchiveRequest, PluginArchive, WithdrawArchiveRequest};
    let local = ArchiveGrant {
        kind: ArchiveKind::Path,
        locks: false,
    };
    let (bus, launcher) = harness_with(local);
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap();
    assert_eq!(launcher.created.lock().unwrap()[0].archive, None);

    let allowed: PluginArchive = ask(
        &bus,
        ALLOW_ARCHIVE,
        "meridian.v1.AllowArchiveRequest",
        AllowArchiveRequest {
            instance_id: "snaptrade-1".into(),
            most_bytes: 53_687_091_200,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(allowed.allowed);
    assert_eq!(allowed.updated_by, ADA);
    assert_eq!(*launcher.removed.lock().unwrap(), ["snaptrade-1"]);
    let made = launcher.created.lock().unwrap()[1].clone();
    assert_eq!(made.archive, Some(allowed.clone()));
    assert_eq!(made.instance_id, "snaptrade-1");

    let withdrawn: PluginArchive = ask(
        &bus,
        WITHDRAW_ARCHIVE,
        "meridian.v1.WithdrawArchiveRequest",
        WithdrawArchiveRequest {
            instance_id: "snaptrade-1".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!withdrawn.allowed);
    assert_eq!(
        withdrawn.most_bytes, 53_687_091_200,
        "its bound is kept with it"
    );
    let again = launcher.created.lock().unwrap()[2].clone();
    assert!(again
        .archive
        .as_ref()
        .is_none_or(|archive| !archive.allowed));

    // Withdrawn again: answered as it stands, nothing recorded or restarted.
    let _: PluginArchive = ask(
        &bus,
        WITHDRAW_ARCHIVE,
        "meridian.v1.WithdrawArchiveRequest",
        WithdrawArchiveRequest {
            instance_id: "snaptrade-1".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(launcher.created.lock().unwrap().len(), 3);

    // An instance nobody launched or heard from is refused.
    let unknown = ask::<_, PluginArchive>(
        &bus,
        ALLOW_ARCHIVE,
        "meridian.v1.AllowArchiveRequest",
        AllowArchiveRequest {
            instance_id: "nobody-1".into(),
            most_bytes: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(unknown.contains("no plugin this deployment"), "{unknown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_archive_is_refused_with_none_installed_and_at_no_edge_role() {
    use meridian_domain::v1::{AllowArchiveRequest, PluginArchive};
    let (bus, _) = harness();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap();
    let none = ask::<_, PluginArchive>(
        &bus,
        ALLOW_ARCHIVE,
        "meridian.v1.AllowArchiveRequest",
        AllowArchiveRequest {
            instance_id: "snaptrade-1".into(),
            most_bytes: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(none.contains("installed with no archive"), "{none}");

    let (bus, _) = harness_with(ArchiveGrant {
        kind: ArchiveKind::Path,
        locks: false,
    });
    let mut inner = snaptrade("0.1.0");
    inner.metadata.as_mut().unwrap().roles = vec!["operations".into()];
    upload(&bus, inner).await.unwrap();
    launch(&bus, launching("ops-1", &["operations"]))
        .await
        .unwrap();
    let refused = ask::<_, PluginArchive>(
        &bus,
        ALLOW_ARCHIVE,
        "meridian.v1.AllowArchiveRequest",
        AllowArchiveRequest {
            instance_id: "ops-1".into(),
            most_bytes: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("holds operations, no edge role"),
        "{refused}"
    );
}

/// One request, stamped as the dashboard sends it for a person through a
/// delegation and its client (W4.9): the terminal's and core's tools'.
async fn ask_through<Q: Message, A: Message + Default>(
    bus: &Bus,
    topic: &str,
    kind: &str,
    request: Q,
) -> Result<A, String> {
    let stamp = meridian_bus::Stamp {
        prepared_call_id: Default::default(),
        acting_for_subject: ADA.into(),
        acting_through_delegation: "DLG-1".into(),
        acting_through_client: "meridian on ada-laptop".into(),
        account_scope: None,
        ..meridian_bus::Stamp::default()
    };
    let (_, bytes) = bus
        .call_stamped(topic, kind, request.encode_to_vec(), None, None, &stamp)
        .await
        .map_err(|failed| failed.to_string())?;
    Ok(A::decode(&bytes[..]).unwrap())
}

#[tokio::test]
async fn a_launch_and_a_stop_through_a_delegation_record_it_and_the_client() {
    let (bus, _) = harness();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    let launched: PluginLaunch = ask_through(
        &bus,
        LAUNCH_PLUGIN,
        "meridian.v1.LaunchPluginRequest",
        LaunchPluginRequest {
            note: "Bringing SnapTrade in for the Growth accounts.".into(),
            ..launching("snaptrade-1", &["custody"])
        },
    )
    .await
    .unwrap();
    assert_eq!(launched.launched_by, ADA);
    assert_eq!(launched.acting_through_delegation, "DLG-1");
    assert_eq!(launched.client_name, "meridian on ada-laptop");
    assert_eq!(
        launched.note,
        "Bringing SnapTrade in for the Growth accounts."
    );
    let stopped: PluginLaunch = ask_through(
        &bus,
        STOP_PLUGIN,
        "meridian.v1.StopPluginRequest",
        StopPluginRequest {
            instance_id: "snaptrade-1".into(),
            note: "Retired.".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(stopped.stopped_by, ADA);
    assert_eq!(stopped.stopped_through_delegation, "DLG-1");
    assert_eq!(stopped.stopped_client_name, "meridian on ada-laptop");
    // Each note is read back with the launch, the stop's beside the launch's
    // (contract v17).
    assert_eq!(
        stopped.note,
        "Bringing SnapTrade in for the Growth accounts."
    );
    assert_eq!(stopped.stopped_note, "Retired.");
    assert_eq!(catalogue(&bus).await.launches[0], stopped);
    // A browser's call names neither.
    launch(&bus, launching("snaptrade-2", &["custody"]))
        .await
        .map(|launched| {
            assert!(
                launched.acting_through_delegation.is_empty() && launched.client_name.is_empty()
            )
        })
        .unwrap();
}

#[tokio::test]
async fn an_archive_allowed_through_a_delegation_records_it_and_the_client() {
    use meridian_domain::v1::{AllowArchiveRequest, PluginArchive, WithdrawArchiveRequest};
    let (bus, _) = harness_with(ArchiveGrant {
        kind: ArchiveKind::Path,
        locks: false,
    });
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap();
    let allowed: PluginArchive = ask_through(
        &bus,
        ALLOW_ARCHIVE,
        "meridian.v1.AllowArchiveRequest",
        AllowArchiveRequest {
            instance_id: "snaptrade-1".into(),
            most_bytes: 0,
            note: "Keep seven years.".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(allowed.acting_through_delegation, "DLG-1");
    assert_eq!(allowed.client_name, "meridian on ada-laptop");
    let withdrawn: PluginArchive = ask_through(
        &bus,
        WITHDRAW_ARCHIVE,
        "meridian.v1.WithdrawArchiveRequest",
        WithdrawArchiveRequest {
            instance_id: "snaptrade-1".into(),
            note: "Not needed.".into(),
        },
    )
    .await
    .unwrap();
    assert!(!withdrawn.allowed);
    assert_eq!(withdrawn.client_name, "meridian on ada-laptop");
}

/// v17's security review, Nit 3 (contract v18): core's own names are no
/// instance's, and a stopped instance's ID is never relaunched as another
/// plugin, whose code its settings, secrets and access would then reach.
#[tokio::test(flavor = "multi_thread")]
async fn core_names_are_reserved_and_an_instance_id_names_one_plugin() {
    let (bus, _launcher) = harness();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    for name in [
        "dashboard",
        "dashboard-1",
        "lake",
        "conductor",
        "runtime",
        "launcher-1",
    ] {
        let refused = launch(&bus, launching(name, &["custody"]))
            .await
            .unwrap_err();
        assert!(refused.contains("core's own names"), "{name}: {refused}");
    }
    assert!(!super::is_reserved(Contract::embedded(), "dashboards"));
    assert!(!super::is_reserved(Contract::embedded(), "snaptrade-1"));

    launch(&bus, launching("broker-1", &["custody"]))
        .await
        .unwrap();
    let _: PluginLaunch = ask(
        &bus,
        STOP_PLUGIN,
        "meridian.v1.StopPluginRequest",
        StopPluginRequest {
            instance_id: "broker-1".into(),
            note: "Moving it.".into(),
        },
    )
    .await
    .unwrap();
    let mut other = snaptrade("0.1.0");
    other.metadata.as_mut().unwrap().name = "imposter".into();
    upload(&bus, other).await.unwrap();
    let reused = launch(
        &bus,
        LaunchPluginRequest {
            name: "imposter".into(),
            ..launching("broker-1", &["custody"])
        },
    )
    .await
    .unwrap_err();
    assert!(reused.contains("launched as snaptrade before"), "{reused}");
    // The same plugin takes its ID again.
    launch(&bus, launching("broker-1", &["custody"]))
        .await
        .unwrap();
}

// ── Retiring a version (W8.15, contract v25) ────────────────────────────────

/// A stand-in for the deployment's own registry, keeping what it released.
#[derive(Default)]
struct Releasing(Mutex<Vec<(String, String)>>);

impl LocalRegistry for Releasing {
    fn release<'a>(
        &'a self,
        repository: &'a str,
        digest: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        self.0
            .lock()
            .unwrap()
            .push((repository.to_string(), digest.to_string()));
        Box::pin(async { Ok(()) })
    }
}

fn harness_releasing() -> (Arc<Bus>, Arc<Releasing>, Arc<MemoryStore>) {
    let bus = Arc::new(Bus::single(
        "conductor-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let store = Arc::new(MemoryStore::new());
    let released = Arc::new(Releasing::default());
    serve_plugins_reaching(
        Arc::clone(&bus),
        store.clone(),
        Arc::new(At(1_790_380_800_000_000_000)),
        "localhost:5000".into(),
        ArchiveGrant::default(),
        Some(released.clone()),
    );
    bus.serve(CREATE_PLUGIN, move |envelope| {
        let request = CreatePluginRequest::decode(&envelope.payload[..]).unwrap();
        Ok((
            "meridian.v1.CreatePluginReply".into(),
            CreatePluginReply {
                workload: format!("plugin-{}", request.instance_id),
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(REMOVE_PLUGIN, move |_| {
        Ok((
            "meridian.v1.RemovePluginReply".into(),
            RemovePluginReply { removed: true }.encode_to_vec(),
        ))
    });
    (bus, released, store)
}

async fn retire(bus: &Bus, version: &str, note: &str) -> Result<PluginVersion, String> {
    ask(
        bus,
        RETIRE_PLUGIN_VERSION,
        "meridian.v1.RetirePluginVersionRequest",
        meridian_domain::v1::RetirePluginVersionRequest {
            name: "snaptrade".into(),
            version: version.into(),
            note: note.into(),
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_version_no_instance_ran_is_retired_its_record_kept_and_its_image_released() {
    let (bus, released, store) = harness_releasing();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    let mut other = snaptrade("0.2.0");
    other.image_digest = format!("sha256:{}", "b".repeat(64));
    upload(&bus, other).await.unwrap();

    let retired = retire(&bus, "0.2.0", "never launched").await.unwrap();
    assert_eq!(retired.retired_by, ADA);
    assert_eq!(retired.retired_note, "never launched");
    assert!(retired.retired_at_ns > 0);
    // No longer listed, its record kept, and its image released.
    let listed = catalogue(&bus).await;
    assert_eq!(listed.versions.len(), 1);
    assert_eq!(
        listed.versions[0].metadata.as_ref().unwrap().version,
        "0.1.0"
    );
    assert_eq!(store.snapshot().unwrap().catalogue.versions.len(), 2);
    assert_eq!(
        released.0.lock().unwrap().clone(),
        vec![(
            "plugins/snaptrade".to_string(),
            format!("sha256:{}", "b".repeat(64))
        )]
    );
    assert_eq!(
        store
            .retirement("snaptrade", "0.2.0")
            .unwrap()
            .unwrap()
            .note,
        "never launched"
    );
    // Never launchable, never replaced, never retired twice.
    let mut asked = launching("snaptrade-2", &["custody"]);
    asked.version = "0.2.0".into();
    let refused = launch(&bus, asked).await.unwrap_err();
    assert!(refused.contains("retired"), "{refused}");
    let again = upload(&bus, snaptrade("0.2.0")).await.unwrap_err();
    assert!(again.contains("already recorded"), "{again}");
    let twice = retire(&bus, "0.2.0", "again").await.unwrap_err();
    assert!(twice.contains("retired already"), "{twice}");
    let unknown = retire(&bus, "9.9.9", "x").await.unwrap_err();
    assert!(unknown.contains("not in the catalogue"), "{unknown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_version_an_instance_runs_or_ran_is_refused_naming_each() {
    let (bus, released, _) = harness_releasing();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    launch(&bus, launching("snaptrade-1", &["custody"]))
        .await
        .unwrap();
    let running = retire(&bus, "0.1.0", "tidy").await.unwrap_err();
    assert!(running.contains("snaptrade-1 (running)"), "{running}");
    // Stopped, its launch is still kept in history and names it.
    ask::<_, PluginLaunch>(
        &bus,
        STOP_PLUGIN,
        "meridian.v1.StopPluginRequest",
        StopPluginRequest {
            instance_id: "snaptrade-1".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let stopped = retire(&bus, "0.1.0", "tidy").await.unwrap_err();
    assert!(stopped.contains("snaptrade-1 (stopped)"), "{stopped}");
    assert!(released.0.lock().unwrap().is_empty());
    assert_eq!(catalogue(&bus).await.versions.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_image_another_version_still_runs_is_not_released() {
    let (bus, released, _) = harness_releasing();
    upload(&bus, snaptrade("0.1.0")).await.unwrap();
    // The same image recorded under a second version.
    upload(&bus, snaptrade("0.1.1")).await.unwrap();
    retire(&bus, "0.1.1", "a duplicate").await.unwrap();
    assert!(released.0.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_affirmation_deadline_is_operations_and_a_time_of_day() {
    // Contract v25 (W8.1, W11.34): 1 to 1440 minutes after midnight, 0 for
    // none, refused on a version not holding operations.
    let (bus, _) = harness();
    let with = |roles: &[&str], deadline: u32, version: &str| {
        let mut upload = snaptrade(version);
        let metadata = upload.metadata.as_mut().unwrap();
        metadata.roles = roles.iter().map(|r| r.to_string()).collect();
        metadata.declaration = Some(meridian_pb::v1::PluginDeclaration {
            affirmation_deadline: deadline,
            ..Default::default()
        });
        upload
    };
    let refused = upload(&bus, with(&["custody"], 1260, "0.1.0"))
        .await
        .unwrap_err();
    assert!(refused.contains("not holding operations"), "{refused}");
    let refused = upload(&bus, with(&["operations"], 1441, "0.1.1"))
        .await
        .unwrap_err();
    assert!(
        refused.contains("affirmation_deadline is 1441"),
        "{refused}"
    );
    let recorded = upload(&bus, with(&["operations"], 1260, "0.1.2"))
        .await
        .unwrap();
    assert_eq!(
        recorded
            .metadata
            .unwrap()
            .declaration
            .unwrap()
            .affirmation_deadline,
        1260
    );
    upload(&bus, with(&["custody"], 0, "0.1.3")).await.unwrap();
}

/// A version naming its documentation, with the texts the CLI read.
fn documented(version: &str, texts: &[(&str, &str)]) -> RecordPluginUploadRequest {
    use sha2::Digest;
    let mut request = snaptrade(version);
    request.metadata.as_mut().unwrap().declaration = Some(meridian_pb::v1::PluginDeclaration {
        documentation: texts
            .iter()
            .map(
                |(section, markdown)| meridian_pb::v1::DocumentationSection {
                    name: section.to_string(),
                    digest: format!(
                        "sha256:{}",
                        hex::encode(sha2::Sha256::digest(markdown.as_bytes()))
                    ),
                    bytes: markdown.len() as u64,
                },
            )
            .collect(),
        ..Default::default()
    });
    request.documentation = texts
        .iter()
        .map(
            |(section, markdown)| meridian_domain::v1::DocumentationText {
                section: section.to_string(),
                markdown: markdown.to_string(),
            },
        )
        .collect();
    request
}

async fn documentation(
    bus: &Bus,
    version: &str,
) -> Result<meridian_domain::v1::ReadPluginDocumentationReply, String> {
    ask(
        bus,
        PLUGIN_DOCUMENTATION,
        "meridian.v1.ReadPluginDocumentationRequest",
        meridian_domain::v1::ReadPluginDocumentationRequest {
            name: "snaptrade".into(),
            version: version.into(),
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_versions_documentation_is_kept_with_it_and_read_before_launch() {
    let (bus, _) = harness();
    let mut request = documented(
        "0.1.0",
        &[
            ("overview", "# SnapTrade\n\nReads your brokerage accounts."),
            ("setup", "Enter the client id."),
        ],
    );
    // Sent out of order: kept in the declaration's.
    request.documentation.reverse();
    upload(&bus, request).await.unwrap();
    let read = documentation(&bus, "0.1.0").await.unwrap();
    assert_eq!(read.uploaded_by, ADA);
    assert_eq!(read.uploaded_at_ns, 1_790_380_800_000_000_000);
    assert_eq!(read.declaration.unwrap().documentation.len(), 2);
    let sections: Vec<&str> = read
        .documentation
        .iter()
        .map(|t| t.section.as_str())
        .collect();
    assert_eq!(sections, ["overview", "setup"]);
    assert_eq!(read.documentation[1].markdown, "Enter the client id.");
    // Never replaced: the same version again is refused and the texts stand.
    let again = upload(&bus, documented("0.1.0", &[("overview", "Other words.")]))
        .await
        .unwrap_err();
    assert!(again.contains("already recorded"), "{again}");
    assert_eq!(
        documentation(&bus, "0.1.0")
            .await
            .unwrap()
            .documentation
            .len(),
        2
    );
    // A version that carried none reads none; one not recorded is refused.
    upload(&bus, snaptrade("0.2.0")).await.unwrap();
    let none = documentation(&bus, "0.2.0").await.unwrap();
    assert!(none.documentation.is_empty() && none.declaration.is_none());
    let missing = documentation(&bus, "9.9.9").await.unwrap_err();
    assert!(missing.contains("not in the catalogue"), "{missing}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uploads_texts_are_held_to_what_its_declaration_names() {
    let (bus, _) = harness();
    let big = "x".repeat(65_536);
    let cases: Vec<(&str, Breaking)> = vec![
        (
            "not one of the sections",
            Box::new(|r| {
                *r = documented("0.1.0", &[("readme", "Hello.")]);
            }),
        ),
        (
            "named twice",
            Box::new(|r| {
                *r = documented("0.1.0", &[("setup", "A."), ("setup", "B.")]);
            }),
        ),
        (
            "is not what the declaration names",
            Box::new(|r| r.documentation[0].markdown.push('!')),
        ),
        (
            "which the declaration does not name",
            Box::new(|r| {
                r.documentation
                    .push(meridian_domain::v1::DocumentationText {
                        section: "terms".into(),
                        markdown: "No warranty.".into(),
                    })
            }),
        ),
        (
            "sent twice",
            Box::new(|r| {
                let first = r.documentation[0].clone();
                r.documentation.push(first);
            }),
        ),
        ("sent no text for it", Box::new(|r| r.documentation.clear())),
        (
            "not `sha256:<hex>`",
            Box::new(|r| {
                r.metadata
                    .as_mut()
                    .unwrap()
                    .declaration
                    .as_mut()
                    .unwrap()
                    .documentation[0]
                    .digest = "md5:abc".into()
            }),
        ),
        (
            "a section is 1 to 65536",
            Box::new(|r| {
                r.metadata
                    .as_mut()
                    .unwrap()
                    .declaration
                    .as_mut()
                    .unwrap()
                    .documentation[0]
                    .bytes = 65_537
            }),
        ),
        (
            "at most 262144",
            Box::new(move |r| {
                let big = "x".repeat(65_536);
                *r = documented(
                    "0.1.0",
                    &[
                        ("overview", &big),
                        ("setup", &big),
                        ("troubleshooting", &big),
                        ("terms", &big),
                        ("pages", "y"),
                    ],
                );
            }),
        ),
    ];
    for (says, breaking) in cases {
        let mut request = documented("0.1.0", &[("overview", "What it does.")]);
        breaking(&mut request);
        let refused = upload(&bus, request).await.unwrap_err();
        assert!(refused.contains(says), "{says}: {refused}");
    }
    assert!(catalogue(&bus).await.versions.is_empty());
    // Every section at its most, 256 KiB in all, is admitted.
    upload(
        &bus,
        documented(
            "0.1.0",
            &[
                ("overview", &big),
                ("setup", &big),
                ("troubleshooting", &big),
                ("terms", &big),
            ],
        ),
    )
    .await
    .unwrap();
}
