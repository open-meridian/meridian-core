//! The registries' rows through the bus, against the stand-in platform:
//! install, launch with its verification again, update, a local instance
//! moved to a listing, a pull acknowledged, and the usage setting.

use std::sync::{Arc, Mutex};

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AcknowledgePullRequest, CreatePluginReply, CreatePluginRequest, InstallPluginRequest,
    LaunchPluginRequest, ListPluginUsageReportsReply, ListPluginUsageReportsRequest,
    ListRegistryPluginsReply, ListRegistryPluginsRequest, PluginCatalogue, PluginCatalogueRequest,
    PluginLaunch, PluginLaunchState, PluginMetadata, PluginVersion, PullAcknowledgement,
    RecordPluginUploadRequest, RemovePluginReply, ReportsPluginUsageSetting,
    SetReportsPluginUsageRequest, UpdatePluginRequest,
};
use prost::Message;
use serde_json::json;

use super::*;
use crate::plugins::{LAUNCH_PLUGIN, PLUGIN_CATALOGUE, RECORD_PLUGIN_UPLOAD};
use crate::registries::tests::{digest, metadata, Platform, FINGERPRINT, LISTING};
use crate::registries::verify::{LISTING_PREDICATE, METADATA_PREDICATE, PROVENANCE_PREDICATE};
use crate::registries::{Hold, Registries};
use crate::service::Clock;
use crate::{ArchiveGrant, MemoryStore};

const ADA: &str = "local|ada";

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

struct Harness {
    bus: Arc<Bus>,
    platform: Arc<Platform>,
    created: Arc<Mutex<Vec<CreatePluginRequest>>>,
}

fn harness() -> Harness {
    let bus = Arc::new(Bus::single(
        "conductor-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let platform = Platform::new();
    let store: Arc<dyn crate::Store> = Arc::new(MemoryStore::new());
    let clock = Arc::new(At(1_792_200_000_000_000_000));
    let hold = Arc::new(Hold::new(
        Arc::clone(&platform) as Arc<dyn Registries>,
        platform.signer.keys(),
        Arc::clone(&store),
        clock.clone(),
    ));
    crate::serve_plugins_and_registries(
        Arc::clone(&bus),
        store,
        clock,
        "localhost:5000".into(),
        ArchiveGrant::default(),
        None,
        Some(hold),
    );
    let created = Arc::new(Mutex::new(Vec::new()));
    let creating = Arc::clone(&created);
    bus.serve(CREATE_PLUGIN, move |envelope| {
        let request = CreatePluginRequest::decode(&envelope.payload[..]).unwrap();
        creating.lock().unwrap().push(request.clone());
        Ok((
            "meridian.v1.CreatePluginReply".into(),
            CreatePluginReply {
                workload: format!("plugin-{}", request.instance_id),
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(REMOVE_PLUGIN, |_| {
        Ok((
            "meridian.v1.RemovePluginReply".into(),
            RemovePluginReply { removed: true }.encode_to_vec(),
        ))
    });
    Harness {
        bus,
        platform,
        created,
    }
}

async fn ask<Q: Message, A: Message + Default>(
    h: &Harness,
    topic: &str,
    kind: &str,
    request: Q,
) -> Result<A, String> {
    let (_, bytes) = h
        .bus
        .call_for(topic, kind, request.encode_to_vec(), None, None, ADA)
        .await
        .map_err(|failed| failed.to_string())?;
    Ok(A::decode(&bytes[..]).unwrap())
}

async fn pull(h: &Harness) -> ListRegistryPluginsReply {
    ask(
        h,
        REGISTRY_PLUGINS,
        "meridian.v1.ListRegistryPluginsRequest",
        ListRegistryPluginsRequest {
            pull_now: true,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

async fn install(h: &Harness, version: &str) -> Result<PluginVersion, String> {
    ask(
        h,
        INSTALL_PLUGIN,
        "meridian.v1.InstallPluginRequest",
        InstallPluginRequest {
            listing: LISTING.into(),
            version: version.into(),
            registry: "community".into(),
            note: "SnapTrade for the brokerage accounts.".into(),
        },
    )
    .await
}

async fn launch(
    h: &Harness,
    name: &str,
    version: &str,
    roles: &[&str],
) -> Result<PluginLaunch, String> {
    ask(
        h,
        LAUNCH_PLUGIN,
        "meridian.v1.LaunchPluginRequest",
        LaunchPluginRequest {
            name: name.into(),
            version: version.into(),
            instance_id: "snaptrade-1".into(),
            approved_roles: roles.iter().map(|r| r.to_string()).collect(),
            note: "Launched.".into(),
            ..Default::default()
        },
    )
    .await
}

async fn catalogue(h: &Harness) -> PluginCatalogue {
    ask(
        h,
        PLUGIN_CATALOGUE,
        "meridian.v1.PluginCatalogueRequest",
        PluginCatalogueRequest {},
    )
    .await
    .unwrap()
}

/// A second listed version, 1.5.0, signed as the platform signs 1.4.0.
fn list_version_1_5(h: &Harness, roles: &[&str]) {
    let image = digest(0xcd);
    let signer = &h.platform.signer;
    let statement = signer.statement(LISTING_PREDICATE, &image, json!({
        "listing": LISTING, "version": "1.5.0", "registry": "community",
        "listed_at": "2026-10-16T13:00:00Z", "signer_key": FINGERPRINT, "roles": roles, "floor": "v25"}));
    let mut catalogue = h.platform.catalogue.lock().unwrap();
    let mut version = catalogue["listings"][0]["versions"][0].clone();
    version["version"] = json!("1.5.0");
    version["image_digest"] = json!(image);
    version["metadata"] = metadata(roles);
    version["listing_statement"] = json!(statement.clone());
    catalogue["listings"][0]["versions"]
        .as_array_mut()
        .unwrap()
        .push(version);
    *h.platform.resolution.lock().unwrap() = json!({"image_digest": image,
        "pull_from": "ghcr.io/open-meridian/plugins/open-meridian/snaptrade", "listing_statement": statement});
    *h.platform.referrers.lock().unwrap() = vec![
        signer.statement(PROVENANCE_PREDICATE, &image, json!({"signer": FINGERPRINT})),
        signer.statement(
            METADATA_PREDICATE,
            &image,
            json!({"metadata": metadata(roles),
            "documentation": [{"name": "overview", "text": "Reads accounts."}]}),
        ),
    ];
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_version_is_installed_launched_from_its_listing_s_repository_and_verified_again() {
    let h = harness();
    // Nothing pulled yet: nothing to install.
    assert!(install(&h, "1.4.0")
        .await
        .unwrap_err()
        .contains("not been pulled"));
    assert_eq!(pull(&h).await.listings.len(), 1);
    let installed = install(&h, "1.4.0").await.unwrap();
    assert_eq!(installed.uploaded_by, ADA);
    assert_eq!(installed.source_registry, "community");
    assert_eq!(installed.listing, LISTING);
    assert!(install(&h, "1.4.0")
        .await
        .unwrap_err()
        .contains("installed already"));
    assert_eq!(
        catalogue(&h).await.versions.len(),
        1,
        "install launches nothing"
    );
    let launched = launch(&h, LISTING, "1.4.0", &["custody"]).await.unwrap();
    assert_eq!(launched.state, PluginLaunchState::Launched as i32);
    assert_eq!(
        h.created.lock().unwrap()[0].image,
        format!("localhost:5000/plugins/{LISTING}@{}", digest(0xab))
    );
    assert!(h
        .platform
        .asked
        .lock()
        .unwrap()
        .iter()
        .any(|a| a == &format!("referrers of plugins/{LISTING}")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launch_refuses_a_version_that_no_longer_verifies() {
    let h = harness();
    pull(&h).await;
    install(&h, "1.4.0").await.unwrap();
    // The referrers in the local registry no longer carry the provenance.
    h.platform.referrers.lock().unwrap().remove(0);
    let refused = launch(&h, LISTING, "1.4.0", &["custody"])
        .await
        .unwrap_err();
    assert!(
        refused.contains("the verification at launch: the provenance"),
        "{refused}"
    );
    assert!(h.created.lock().unwrap().is_empty(), "nothing created");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_instance_is_moved_to_a_newer_version_from_its_source_registry_and_a_widening_approved_exactly(
) {
    let h = harness();
    pull(&h).await;
    install(&h, "1.4.0").await.unwrap();
    launch(&h, LISTING, "1.4.0", &["custody"]).await.unwrap();
    list_version_1_5(&h, &["custody", "operations"]);
    pull(&h).await;
    let update = |version: &str, roles: &[&str]| UpdatePluginRequest {
        instance_id: "snaptrade-1".into(),
        listing: LISTING.into(),
        version: version.into(),
        approved_roles: roles.iter().map(|r| r.to_string()).collect(),
        note: "The new statement format.".into(),
    };
    // A widening approved as anything but exactly the new roles waits.
    let refused: Result<PluginLaunch, String> = ask(
        &h,
        UPDATE_PLUGIN,
        "meridian.v1.UpdatePluginRequest",
        update("1.5.0", &["custody"]),
    )
    .await;
    assert!(refused.unwrap_err().contains("no approval"));
    // Not newer.
    let refused: Result<PluginLaunch, String> = ask(
        &h,
        UPDATE_PLUGIN,
        "meridian.v1.UpdatePluginRequest",
        update("1.4.0", &["custody"]),
    )
    .await;
    assert!(refused.unwrap_err().contains("not newer"));
    let moved: PluginLaunch = ask(
        &h,
        UPDATE_PLUGIN,
        "meridian.v1.UpdatePluginRequest",
        update("1.5.0", &["custody", "operations"]),
    )
    .await
    .unwrap();
    assert_eq!(
        (moved.instance_id.as_str(), moved.version.as_str()),
        ("snaptrade-1", "1.5.0")
    );
    let held = catalogue(&h).await;
    assert_eq!(
        held.versions.len(),
        2,
        "the newer version installed on the way"
    );
    let ended = held.launches.iter().find(|l| l.version == "1.4.0").unwrap();
    assert_eq!(ended.state, PluginLaunchState::Stopped as i32);
    assert!(
        ended
            .stopped_note
            .starts_with("moved to open-meridian/snaptrade 1.5.0"),
        "{}",
        ended.stopped_note
    );
    assert_eq!(
        h.created.lock().unwrap()[1].image,
        format!("localhost:5000/plugins/{LISTING}@{}", digest(0xcd))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_instance_moves_only_to_a_listing_of_its_own_plugin() {
    let h = harness();
    let local = RecordPluginUploadRequest {
        metadata: Some(PluginMetadata {
            name: "snaptrade".into(),
            version: "0.9.0".into(),
            roles: vec!["custody".into()],
            interface: true,
            sdk_version: "0.28.0".into(),
            declaration: None,
        }),
        image_digest: digest(0x11),
        documentation: Vec::new(),
        ..Default::default()
    };
    let _: PluginVersion = ask(
        &h,
        RECORD_PLUGIN_UPLOAD,
        "meridian.v1.RecordPluginUploadRequest",
        local,
    )
    .await
    .unwrap();
    launch(&h, "snaptrade", "0.9.0", &["custody"])
        .await
        .unwrap();
    pull(&h).await;
    let moved: PluginLaunch = ask(
        &h,
        UPDATE_PLUGIN,
        "meridian.v1.UpdatePluginRequest",
        UpdatePluginRequest {
            instance_id: "snaptrade-1".into(),
            listing: LISTING.into(),
            version: "1.4.0".into(),
            approved_roles: vec!["custody".into()],
            note: "Onto the listing.".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(moved.name, LISTING);
    // Another plugin's listing is refused.
    let refused: Result<PluginLaunch, String> = ask(
        &h,
        UPDATE_PLUGIN,
        "meridian.v1.UpdatePluginRequest",
        UpdatePluginRequest {
            instance_id: "snaptrade-1".into(),
            listing: "open-meridian/kraken".into(),
            version: "1.0.0".into(),
            ..Default::default()
        },
    )
    .await;
    assert!(refused.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pulled_version_s_instance_is_kept_only_with_a_pull_to_acknowledge() {
    let h = harness();
    pull(&h).await;
    install(&h, "1.4.0").await.unwrap();
    launch(&h, LISTING, "1.4.0", &["custody"]).await.unwrap();
    let acknowledge = AcknowledgePullRequest {
        instance_id: "snaptrade-1".into(),
        note: "Kept while we move.".into(),
    };
    let refused: Result<PullAcknowledgement, String> = ask(
        &h,
        ACKNOWLEDGE_PULL,
        "meridian.v1.AcknowledgePullRequest",
        acknowledge.clone(),
    )
    .await;
    assert!(refused.unwrap_err().contains("not pulled"));
    h.platform.catalogue.lock().unwrap()["listings"][0]["versions"][0]["pull"] = json!({
        "listing": LISTING, "version": "1.4.0", "kind": "PULL_REASON_KIND_TERMS_BREACH",
        "reason": "a breach of the terms", "pulled_at_ns": 9, "restored_at_ns": 0});
    pull(&h).await;
    let kept: PullAcknowledgement = ask(
        &h,
        ACKNOWLEDGE_PULL,
        "meridian.v1.AcknowledgePullRequest",
        acknowledge,
    )
    .await
    .unwrap();
    assert_eq!(kept.acknowledged_by, ADA);
    assert_eq!(kept.pull.unwrap().pulled_at_ns, 9);
    let launch = catalogue(&h)
        .await
        .launches
        .into_iter()
        .find(|l| l.instance_id == "snaptrade-1")
        .unwrap();
    assert_eq!(
        launch.pull_acknowledgement.unwrap().note,
        "Kept while we move."
    );
    // A pulled version is installed no more.
    assert!(install(&h, "1.4.0").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_usage_setting_is_on_until_turned_off_each_change_its_own_record() {
    let h = harness();
    let read = || async {
        let listed: ListPluginUsageReportsReply = ask(
            &h,
            LIST_PLUGIN_USAGE_REPORTS,
            "meridian.v1.ListPluginUsageReportsRequest",
            ListPluginUsageReportsRequest::default(),
        )
        .await
        .unwrap();
        listed
    };
    assert!(read().await.reports_plugin_usage);
    let set: ReportsPluginUsageSetting = ask(
        &h,
        SET_REPORTS_PLUGIN_USAGE,
        "meridian.v1.SetReportsPluginUsageRequest",
        SetReportsPluginUsageRequest {
            reports_plugin_usage: false,
            reason: "Not yet.".into(),
        },
    )
    .await
    .unwrap();
    assert!(!set.reports_plugin_usage);
    assert!(!read().await.reports_plugin_usage);
}

/// `meridian plugin upload --bundle`'s record of the stand-in's 1.4.0, its
/// image and referrers pushed into plugins/{listing} first.
fn bundle_upload(h: &Harness) -> RecordPluginUploadRequest {
    RecordPluginUploadRequest {
        metadata: Some(PluginMetadata {
            name: "snaptrade".into(),
            version: "1.4.0".into(),
            roles: vec!["custody".into()],
            interface: true,
            sdk_version: "0.29.0".into(),
            declaration: None,
        }),
        image_digest: digest(0xab),
        documentation: vec![meridian_domain::v1::DocumentationText {
            section: "overview".into(),
            markdown: crate::registries::tests::OVERVIEW.into(),
        }],
        listing: LISTING.into(),
        source_registry: "community".into(),
        key_status: h.platform.catalogue.lock().unwrap()["key_status"].to_string(),
        listing_statement: h.platform.resolution.lock().unwrap()["listing_statement"]
            .as_str()
            .unwrap()
            .into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_is_recorded_under_its_listing_with_its_key_status_s_age_and_launched() {
    let h = harness();
    // Nothing pulled, as on a deployment that reaches nothing.
    let recorded: PluginVersion = ask(
        &h,
        RECORD_PLUGIN_UPLOAD,
        "meridian.v1.RecordPluginUploadRequest",
        bundle_upload(&h),
    )
    .await
    .unwrap();
    assert_eq!(recorded.listing, LISTING);
    assert_eq!(recorded.source_registry, "community");
    assert_eq!(recorded.key_status_issued_at_ns, 1_792_000_000_000_000_000);
    assert_eq!(recorded.uploaded_by, ADA);
    let held = catalogue(&h).await;
    assert_eq!(held.versions.len(), 1);
    assert_eq!(held.versions[0].metadata.as_ref().unwrap().name, LISTING);
    assert_eq!(
        held.versions[0].key_status_issued_at_ns,
        1_792_000_000_000_000_000
    );
    // A version recorded is never replaced.
    let again: Result<PluginVersion, String> = ask(
        &h,
        RECORD_PLUGIN_UPLOAD,
        "meridian.v1.RecordPluginUploadRequest",
        bundle_upload(&h),
    )
    .await;
    assert!(again.unwrap_err().contains("already recorded"));
    // Launched from its listing's repository, verified again against the
    // bundle's key status, the newest held.
    let launched = launch(&h, LISTING, "1.4.0", &["custody"]).await.unwrap();
    assert_eq!(launched.state, PluginLaunchState::Launched as i32);
    assert_eq!(
        h.created.lock().unwrap()[0].image,
        format!("localhost:5000/plugins/{LISTING}@{}", digest(0xab))
    );
    assert!(!h
        .platform
        .asked
        .lock()
        .unwrap()
        .iter()
        .any(|a| a.starts_with("resolve") || a.starts_with("catalogue")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_whose_texts_are_not_the_image_s_is_refused() {
    let h = harness();
    let mut upload = bundle_upload(&h);
    upload.documentation[0].markdown = "Reads everything.".into();
    let refused: Result<PluginVersion, String> = ask(
        &h,
        RECORD_PLUGIN_UPLOAD,
        "meridian.v1.RecordPluginUploadRequest",
        upload,
    )
    .await;
    assert!(refused.unwrap_err().contains("the documentation"));
    assert!(catalogue(&h).await.versions.is_empty());
}
