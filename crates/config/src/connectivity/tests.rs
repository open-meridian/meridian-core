//! A connectivity plugin's `ems`, its venue account map and the venue's
//! catalogue (contract v22; W11.20 to W11.22), over the bus on the memory
//! store.

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccountRecord, DefineAccountRequest, DiagnosticBundle, DiagnosticBundleReceipt,
    LinkConnectivityRequest, MapVenueAccountRequest, PluginConfiguration,
    PluginConfigurationRequest, ReadVenueCatalogueReply, ReadVenueCatalogueRequest,
    RedeemClaimCodeReply, SetConnectivityAvailableRequest, VenueCatalogue, VenueCatalogueEvent,
    VenueOrderType,
};
use meridian_pb::v1::{
    connectivity_user, ConnectivityLink, ConnectivityUser, RefusalReason, VenueAccountMap,
};
use prost::Message;

use crate::service::{serve, Clock, Upstream, DEFINE_ACCOUNT, PLUGIN_CONFIGURATION};
use crate::store::{Author, KnownPlugin, Store};
use crate::{MemoryStore, SettingsKey};

use super::*;

const ADA: &str = "https://directory.example.org|8812";
const NOW: i64 = 1_790_380_800_000_000_000;

struct FixedClock;
impl Clock for FixedClock {
    fn now_ns(&self) -> i64 {
        NOW
    }
}

struct NoPlatform;
impl Upstream for NoPlatform {
    fn honour_claim_code(&self, _: &str, _: i32) -> Result<RedeemClaimCodeReply, String> {
        Err("not in this test".into())
    }
    fn submit_diagnostic_bundle(
        &self,
        _: &DiagnosticBundle,
    ) -> Result<DiagnosticBundleReceipt, String> {
        Err("not in this test".into())
    }
}

/// One conductor's service per instance a test speaks as, over one store:
/// a question is answered in the process that asks it, and what each
/// conductor is asked names its bus's instance as the publisher, as a
/// sidecar's does.
struct Harness {
    buses: std::collections::BTreeMap<&'static str, Arc<Bus>>,
    store: Arc<MemoryStore>,
}

fn harness() -> Harness {
    let backend = Arc::new(MemoryBackend::new());
    let store = Arc::new(MemoryStore::new());
    let mut buses = std::collections::BTreeMap::new();
    for instance in ["dashboard", "ems-1", "ems-2", "fix-1", "snap-1"] {
        let bus = Arc::new(Bus::single(
            instance,
            backend.clone(),
            Arc::new(meridian_clock::SystemClock),
        ));
        serve(
            Arc::clone(&bus),
            store.clone() as Arc<dyn Store>,
            Arc::new(FixedClock),
            Arc::new(NoPlatform),
            Arc::new(SettingsKey::holding(&[7u8; 32])),
        );
        buses.insert(instance, bus);
    }
    for (instance, roles) in [
        ("snap-1", vec!["custody", "connectivity"]),
        ("fix-1", vec!["connectivity"]),
        ("ems-1", vec!["ems"]),
        ("ems-2", vec!["ems"]),
        ("oms-1", vec!["oms"]),
    ] {
        store
            .record_plugin(&KnownPlugin {
                plugin_instance_id: instance.into(),
                roles: roles.into_iter().map(String::from).collect(),
                last_reported_at_ns: 0,
            })
            .unwrap();
    }
    Harness { buses, store }
}

impl Harness {
    /// The bus that publishes as `instance`, as its sidecar's does.
    fn as_instance(&self, instance: &str) -> &Bus {
        &self.buses[instance]
    }
}

async fn ask<Req: Message, Rep: Message + Default>(
    bus: &Bus,
    topic: &str,
    request_type: &str,
    request: Req,
    person: &str,
) -> Result<Rep, String> {
    let (_, bytes) = bus
        .call_for(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            None,
            person,
        )
        .await
        .map_err(|failed| failed.to_string())?;
    Ok(Rep::decode(&bytes[..]).expect("decodes"))
}

async fn available(
    h: &Harness,
    instance: &str,
    available: bool,
) -> Result<ConnectivityLink, String> {
    ask(
        h.as_instance("dashboard"),
        SET_CONNECTIVITY_AVAILABLE,
        "meridian.v1.SetConnectivityAvailableRequest",
        SetConnectivityAvailableRequest {
            connectivity_instance_id: instance.into(),
            available,
            note: "the desk trades through it".into(),
        },
        ADA,
    )
    .await
}

async fn link(
    h: &Harness,
    instance: &str,
    ems: &str,
    users: Vec<ConnectivityUser>,
) -> Result<ConnectivityLink, String> {
    ask(
        h.as_instance("dashboard"),
        LINK_CONNECTIVITY,
        "meridian.v1.LinkConnectivityRequest",
        LinkConnectivityRequest {
            connectivity_instance_id: instance.into(),
            ems_instance_id: ems.into(),
            users,
            note: String::new(),
        },
        ADA,
    )
    .await
}

async fn map(
    h: &Harness,
    ems: &str,
    instance: &str,
    code: &str,
    account: &str,
) -> Result<VenueAccountMap, String> {
    ask(
        h.as_instance(ems),
        MAP_VENUE_ACCOUNT,
        "meridian.v1.MapVenueAccountRequest",
        MapVenueAccountRequest {
            connectivity_instance_id: instance.into(),
            venue_account_code: code.into(),
            account_id: account.into(),
            ems_instance_id: String::new(),
        },
        ADA,
    )
    .await
}

async fn configuration(h: &Harness, instance: &str) -> PluginConfiguration {
    let (_, bytes) = h
        .as_instance(instance)
        .call(
            PLUGIN_CONFIGURATION,
            "meridian.v1.PluginConfigurationRequest",
            PluginConfigurationRequest {}.encode_to_vec(),
            None,
            None,
        )
        .await
        .unwrap();
    PluginConfiguration::decode(&bytes[..]).unwrap()
}

async fn granted_account(h: &Harness, name: &str, to: &str) -> AccountRecord {
    let account: AccountRecord = ask(
        h.as_instance("dashboard"),
        DEFINE_ACCOUNT,
        "meridian.v1.DefineAccountRequest",
        DefineAccountRequest {
            name: name.into(),
            ..Default::default()
        },
        ADA,
    )
    .await
    .unwrap();
    h.store
        .set_linkable_accounts(
            to,
            std::slice::from_ref(&account.account_id),
            &[],
            &Author::default(),
            1,
        )
        .unwrap();
    account
}

fn person(subject: &str) -> ConnectivityUser {
    ConnectivityUser {
        user: Some(connectivity_user::User::Subject(subject.into())),
    }
}

fn reason(refused: &str, reason: RefusalReason) -> bool {
    refused.contains(&format!("[refusal-reason {}", reason as i32))
}

#[tokio::test]
async fn a_deployment_admin_makes_it_available_and_an_ems_admin_links_it_to_one_ems() {
    let h = harness();
    // Nothing is available by default: a link before is refused, naming why.
    let refused = link(&h, "fix-1", "ems-1", vec![]).await.unwrap_err();
    assert!(refused.contains("is not available"), "{refused}");
    // Only an instance holding connectivity.
    let refused = available(&h, "oms-1", true).await.unwrap_err();
    assert!(refused.contains("connectivity_instance_id"), "{refused}");
    let refused = available(&h, "nobody-1", true).await.unwrap_err();
    assert!(refused.contains("connectivity_instance_id"), "{refused}");

    let made = available(&h, "fix-1", true).await.unwrap();
    assert!(made.available);
    assert!(made.ems_instance_id.is_empty());

    let linked = link(&h, "fix-1", "ems-1", vec![person(ADA), person(ADA)])
        .await
        .unwrap();
    assert_eq!(linked.ems_instance_id, "ems-1");
    assert_eq!(linked.users, vec![person(ADA)], "each user once");

    // Each connectivity plugin is linked to exactly one ems.
    let refused = link(&h, "fix-1", "ems-2", vec![]).await.unwrap_err();
    assert!(
        reason(&refused, RefusalReason::ConnectivityLinked),
        "{refused}"
    );
    // An ems that is none, and a user naming nobody, are refused.
    assert!(link(&h, "fix-1", "oms-1", vec![])
        .await
        .unwrap_err()
        .contains("ems_instance_id"));
    let nobody = ConnectivityUser { user: None };
    assert!(link(&h, "fix-1", "ems-1", vec![nobody])
        .await
        .unwrap_err()
        .contains("users[0]"));

    // Unlinking takes its users with it; then another ems may link it.
    let unlinked = link(&h, "fix-1", "", vec![]).await.unwrap();
    assert!(unlinked.ems_instance_id.is_empty() && unlinked.users.is_empty());
    link(&h, "fix-1", "ems-2", vec![]).await.unwrap();

    // Each change its own record, the link before and after, who and why.
    let changes = h.store.connectivity_link_changes("fix-1").unwrap();
    assert_eq!(changes.len(), 4);
    assert!(changes[0].from_link.is_none());
    assert!(changes[0].to_link.available);
    assert_eq!(changes[0].by, ADA);
    assert_eq!(changes[0].note, "the desk trades through it");
    assert_eq!(
        changes[3].from_link.as_ref().unwrap().ems_instance_id,
        "",
        "the record before the relink says it was unlinked"
    );
    assert_eq!(changes[3].to_link.ems_instance_id, "ems-2");

    // Setting what stands records nothing.
    link(&h, "fix-1", "ems-2", vec![]).await.unwrap();
    assert_eq!(h.store.connectivity_link_changes("fix-1").unwrap().len(), 4);
}

#[tokio::test]
async fn an_ems_maps_only_granted_accounts_at_its_own_plugins_and_the_map_is_the_plugins_scope() {
    let h = harness();
    let growth = granted_account(&h, "Growth", "ems-1").await;
    let other = granted_account(&h, "Not the ems's", "oms-1").await;

    // Not linked to this ems: refused, naming the plugin.
    let refused = map(&h, "ems-1", "fix-1", "ACCT-7", &growth.account_id)
        .await
        .unwrap_err();
    assert!(
        reason(&refused, RefusalReason::ConnectivityNotAllowed),
        "{refused}"
    );

    available(&h, "fix-1", true).await.unwrap();
    link(&h, "fix-1", "ems-1", vec![person(ADA)]).await.unwrap();

    // An account the deployment admin did not grant the ems: refused.
    let refused = map(&h, "ems-1", "fix-1", "ACCT-7", &other.account_id)
        .await
        .unwrap_err();
    assert!(
        reason(&refused, RefusalReason::AccountNotLinkable),
        "{refused}"
    );

    let mapped = map(&h, "ems-1", "fix-1", "ACCT-7", &growth.account_id)
        .await
        .unwrap();
    assert_eq!(mapped.ems_instance_id, "ems-1");
    assert_eq!(mapped.mappings.len(), 1);
    assert_eq!(mapped.mappings[0].venue_account_code, "ACCT-7");

    // A code once per plugin: mapping it to another account is refused.
    let second = granted_account(&h, "Income", "ems-1").await;
    let refused = map(&h, "ems-1", "fix-1", "ACCT-7", &second.account_id)
        .await
        .unwrap_err();
    assert!(refused.contains("already exists"), "{refused}");

    // The connectivity plugin is told its link and its ems's mappings on it,
    // and its scope is those accounts; the ems its plugins and its map.
    let told = configuration(&h, "fix-1").await;
    assert_eq!(told.read_account_ids, vec![growth.account_id.clone()]);
    assert_eq!(told.write_account_ids, vec![growth.account_id.clone()]);
    assert_eq!(
        told.connectivity_link.as_ref().unwrap().ems_instance_id,
        "ems-1"
    );
    assert_eq!(told.venue_account_map.as_ref().unwrap().mappings.len(), 1);
    let ems = configuration(&h, "ems-1").await;
    assert_eq!(ems.connectivity_links.len(), 1);
    assert_eq!(ems.venue_account_map.as_ref().unwrap().mappings.len(), 1);

    // Withdrawn, it reaches nothing; made available again, the map stands.
    available(&h, "fix-1", false).await.unwrap();
    assert!(configuration(&h, "fix-1").await.read_account_ids.is_empty());
    available(&h, "fix-1", true).await.unwrap();
    assert_eq!(configuration(&h, "fix-1").await.read_account_ids.len(), 1);

    // Relinked to another ems, the first's mappings reach nothing, and the
    // new ems may map the code afresh.
    link(&h, "fix-1", "", vec![]).await.unwrap();
    link(&h, "fix-1", "ems-2", vec![]).await.unwrap();
    assert!(configuration(&h, "fix-1").await.read_account_ids.is_empty());
    assert!(configuration(&h, "ems-1")
        .await
        .venue_account_map
        .unwrap()
        .mappings
        .is_empty());
    let income = granted_account(&h, "Desk two", "ems-2").await;
    map(&h, "ems-2", "fix-1", "ACCT-7", &income.account_id)
        .await
        .unwrap();
    assert_eq!(
        configuration(&h, "fix-1").await.read_account_ids,
        vec![income.account_id.clone()]
    );

    // Removed with an empty account; each change its own record.
    map(&h, "ems-2", "fix-1", "ACCT-7", "").await.unwrap();
    assert!(configuration(&h, "fix-1").await.read_account_ids.is_empty());
    let changes = h.store.venue_account_map_changes("ems-1").unwrap();
    assert_eq!(changes.len(), 1);
    assert!(changes[0].from_mapping.is_none());
    assert_eq!(changes[0].by, ADA);
    let changes = h.store.venue_account_map_changes("ems-2").unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(
        changes[0].from_mapping.as_ref().unwrap().account_id,
        growth.account_id,
        "the replaced mapping is the first ems's"
    );
    assert!(changes[1].to_mapping.is_none());
}

#[tokio::test]
async fn a_plugin_holding_custody_keeps_its_links_and_gains_its_mapped_accounts() {
    // Q42: its custody links are never the map, and the map adds to them.
    let h = harness();
    let mapped = granted_account(&h, "Mapped", "ems-1").await;
    available(&h, "snap-1", true).await.unwrap();
    link(&h, "snap-1", "ems-1", vec![]).await.unwrap();
    map(&h, "ems-1", "snap-1", "SNAP-A", &mapped.account_id)
        .await
        .unwrap();
    let told = configuration(&h, "snap-1").await;
    assert!(told.read_account_ids.contains(&mapped.account_id));
    assert!(told.links.is_empty());
}

#[tokio::test]
async fn the_venue_catalogue_is_kept_latest_per_plugin_and_code_and_read_as_itself() {
    let h = harness();
    let publishing = h.as_instance("fix-1");
    let catalogue = |order_types: usize| VenueCatalogueEvent {
        catalogue: Some(VenueCatalogue {
            // Whatever the plugin wrote, the stamped publisher's.
            connectivity_instance_id: "someone-else".into(),
            venue_account_code: "ACCT-7".into(),
            order_types: (0..order_types)
                .map(|i| VenueOrderType {
                    order_type: 1 + i as i32,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }),
        publisher_instance_id: "fix-1".into(),
    };
    for event in [catalogue(1), catalogue(2), catalogue(2)] {
        publishing
            .publish(
                VENUE_CATALOGUE,
                "meridian.v1.VenueCatalogueEvent",
                event.encode_to_vec(),
                None,
                None,
            )
            .unwrap();
    }
    let read = |code: &str| ReadVenueCatalogueRequest {
        connectivity_instance_id: "fix-1".into(),
        venue_account_code: code.into(),
    };
    let reader = h.as_instance("ems-1");
    let mut answered = None;
    for _ in 0..100 {
        let reply: ReadVenueCatalogueReply = ask(
            reader,
            READ_VENUE_CATALOGUE,
            "meridian.v1.ReadVenueCatalogueRequest",
            read("ACCT-7"),
            "",
        )
        .await
        .unwrap();
        if reply.catalogue.as_ref().unwrap().order_types.len() == 2 {
            answered = reply.catalogue;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let answered = answered.expect("the latest catalogue is read");
    assert_eq!(answered.connectivity_instance_id, "fix-1");
    // Two different catalogues, two records; the repeat changed nothing.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!h.store.keep_venue_catalogue(&answered, NOW).unwrap());

    // A code not published is answered empty, never refused.
    let empty: ReadVenueCatalogueReply = ask(
        reader,
        READ_VENUE_CATALOGUE,
        "meridian.v1.ReadVenueCatalogueRequest",
        read("ACCT-9"),
        "",
    )
    .await
    .unwrap();
    let empty = empty.catalogue.unwrap();
    assert!(empty.order_types.is_empty());
    assert_eq!(empty.venue_account_code, "ACCT-9");
}

#[tokio::test]
async fn the_dashboard_reads_each_link_and_each_ems_map_with_the_access_records() {
    // AccessRecords.connectivity_links and venue_account_maps (contract v22,
    // approved 2026-10-12): for the dashboard's pages and its read tool.
    let h = harness();
    let growth = granted_account(&h, "Growth", "ems-1").await;
    available(&h, "fix-1", true).await.unwrap();
    link(&h, "fix-1", "ems-1", vec![person(ADA)]).await.unwrap();
    map(&h, "ems-1", "fix-1", "ACCT-7", &growth.account_id)
        .await
        .unwrap();
    let records: meridian_domain::v1::AccessRecords = ask(
        h.as_instance("dashboard"),
        crate::service::ACCESS_RECORDS,
        "meridian.v1.AccessRecordsRequest",
        meridian_domain::v1::AccessRecordsRequest {},
        ADA,
    )
    .await
    .unwrap();
    assert_eq!(records.connectivity_links.len(), 1);
    assert_eq!(records.connectivity_links[0].ems_instance_id, "ems-1");
    assert_eq!(records.venue_account_maps.len(), 1);
    assert_eq!(records.venue_account_maps[0].ems_instance_id, "ems-1");
    assert_eq!(
        records.venue_account_maps[0].mappings[0].account_id,
        growth.account_id
    );
}
