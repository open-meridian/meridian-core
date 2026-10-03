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
    let bus = Arc::new(Bus::single(
        "conductor-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    serve_plugins(
        Arc::clone(&bus),
        Arc::new(MemoryStore::new()),
        Arc::new(At(1_790_380_800_000_000_000)),
        "localhost:5000".into(),
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
