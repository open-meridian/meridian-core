//! Typed delivery through the sidecar (W4.3), against an in-memory bus with a
//! conductor that holds the plugin's read scope.

use std::sync::Arc;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    ChangeCause, CustodialPosition, CustodialPositionUpdatedEvent, JournalRef, PluginConfiguration,
    StatementRecordedEvent,
};
use meridian_pb::plugin::v1 as plugin;
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::RegisterRequest;
use prost::Message;
use tokio_stream::StreamExt;
use tonic::{Code, Request};

use super::{Loss, Waiting, QUEUE};
use crate::grants::Contract;
use crate::service::{Identity, Sidecar};

const POSITIONS: &str = "platform.street.event.custodial-position-updated";
const STATEMENTS: &str = "platform.street.event.statement-recorded";

fn contract() -> Contract {
    Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.street.event.custodial-position-updated\tevent\tstreet\tdashboard,operations\n\
         platform.street.event.statement-recorded\tevent\tstreet\tdashboard,operations\n\
         platform.reference.event.instrument-applied\tevent\tinstrument\tdashboard\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n",
        "name\tkind\noperations\trole\ncustody\trole\nstreet\tcomponent\nsidecar\tcomponent\n\
         conductor\tcomponent\ndashboard\tcomponent\ninstrument\tcomponent\n",
    )
    .unwrap()
}

/// A registered `operations-1` whose read scope is `scope`.
async fn registered(roles: &[&str], scope: &[&str]) -> (Sidecar, Arc<Bus>) {
    let bus = Arc::new(Bus::single(
        "operations-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    let read: Vec<String> = scope.iter().map(|a| a.to_string()).collect();
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, move |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "operations-1".into(),
                read_account_ids: read.clone(),
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let sidecar = Sidecar::under(
        &contract(),
        Arc::clone(&bus),
        "DEP-test",
        Identity::new(
            "operations-1",
            roles.iter().map(|r| r.to_string()).collect(),
        ),
    );
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v7".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    (sidecar, bus)
}

fn changed(account: &str, sequence: u64, previous: u64, by: &str) -> Vec<u8> {
    CustodialPositionUpdatedEvent {
        position: Some(CustodialPosition {
            account_id: account.into(),
            instrument_id: "INS-1".into(),
            ..Default::default()
        }),
        statement_id: "STMT-1".into(),
        journal: Some(JournalRef {
            partition: "street".into(),
            sequence,
            previous_sequence: previous,
        }),
        cause: Some(ChangeCause {
            instance_id: by.into(),
            causation_id: "msg-cmd".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
    .encode_to_vec()
}

async fn next(stream: &mut super::Deliveries) -> plugin::Delivery {
    tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("nothing was delivered within two seconds")
        .expect("the stream ended")
        .expect("a delivery, not a refusal")
}

#[tokio::test]
async fn a_change_in_the_scope_is_delivered_typed_with_what_is_known_of_it() {
    let (sidecar, bus) = registered(&["operations"], &["ACC-1"]).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest {
            rows: vec!["CustodialPositionUpdated".into()],
            subjects: Vec::new(),
        }))
        .await
        .unwrap()
        .into_inner();

    // Outside the scope first: never delivered, and no gap for it either.
    bus.publish(
        POSITIONS,
        "meridian.v1.CustodialPositionUpdatedEvent",
        changed("ACC-9", 41, 0, "custody-1"),
        Some("corr-9"),
        None,
    )
    .unwrap();
    bus.publish(
        POSITIONS,
        "meridian.v1.CustodialPositionUpdatedEvent",
        changed("ACC-1", 42, 37, "custody-1"),
        Some("corr-1"),
        Some("msg-cmd"),
    )
    .unwrap();

    let delivery = next(&mut stream).await;
    let meta = delivery.meta.unwrap();
    assert_eq!(meta.row, "CustodialPositionUpdated");
    assert_eq!(meta.correlation_id, "corr-1");
    assert_eq!(meta.causation_id, "msg-cmd");
    assert!(!meta.message_id.is_empty() && meta.published_at_ns > 0);
    let journal = meta.journal.unwrap();
    assert_eq!((journal.sequence, journal.previous_sequence), (42, 37));
    assert_eq!(meta.cause.unwrap().instance_id, "custody-1");
    assert!(!meta.own);
    match delivery.item {
        Some(plugin::delivery::Item::CustodialPositionUpdated(event)) => {
            assert_eq!(event.position.unwrap().account_id, "ACC-1")
        }
        other => panic!("not the position: {other:?}"),
    }
}

#[tokio::test]
async fn its_own_act_is_marked_and_every_row_is_heard_when_none_is_named() {
    let (sidecar, bus) = registered(&["operations"], &["ACC-1"]).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();

    bus.publish(
        POSITIONS,
        "meridian.v1.CustodialPositionUpdatedEvent",
        changed("ACC-1", 1, 0, "operations-1"),
        None,
        None,
    )
    .unwrap();
    assert!(
        next(&mut stream).await.meta.unwrap().own,
        "the plugin's own act (Q6)"
    );

    bus.publish(
        STATEMENTS,
        "meridian.v1.StatementRecordedEvent",
        StatementRecordedEvent {
            account_id: "ACC-1".into(),
            ..Default::default()
        }
        .encode_to_vec(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        next(&mut stream).await.meta.unwrap().row,
        "StatementRecorded"
    );
}

#[tokio::test]
async fn a_row_its_roles_do_not_hear_is_refused_naming_it() {
    let (sidecar, _) = registered(&["operations"], &["ACC-1"]).await;
    let refused = sidecar
        .receive(Request::new(plugin::ReceiveRequest {
            rows: vec!["InstrumentApplied".into()],
            subjects: Vec::new(),
        }))
        .await
        .err()
        .expect("refused");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(
        refused.message(),
        "InstrumentApplied is not a row this plugin's roles hear"
    );

    let (custody, _) = registered(&["custody"], &["ACC-1"]).await;
    let refused = custody
        .receive(Request::new(plugin::ReceiveRequest {
            rows: vec!["StatementRecorded".into()],
            subjects: Vec::new(),
        }))
        .await
        .err()
        .expect("custody hears no street row");
    assert_eq!(refused.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn an_empty_scope_hears_nothing_scoped() {
    let (sidecar, bus) = registered(&["operations"], &[]).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();
    bus.publish(
        POSITIONS,
        "meridian.v1.CustodialPositionUpdatedEvent",
        changed("ACC-1", 1, 0, "c"),
        None,
        None,
    )
    .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(400), stream.next())
            .await
            .is_err(),
        "nothing, never everything"
    );
}

fn delivery(n: u64) -> plugin::Delivery {
    plugin::Delivery {
        meta: Some(plugin::DeliveryMeta {
            message_id: format!("msg-{n}"),
            ..Default::default()
        }),
        item: None,
    }
}

fn lost(item: Option<plugin::Delivery>) -> plugin::Lost {
    match item.and_then(|d| d.item) {
        Some(plugin::delivery::Item::Lost(lost)) => lost,
        other => panic!("not a Lost: {other:?}"),
    }
}

#[test]
fn a_full_queue_drops_and_says_so_in_order_where_it_dropped() {
    let waiting = Waiting::default();
    for n in 0..(QUEUE as u64 + 3) {
        waiting.push(delivery(n), "CustodialPositionUpdated");
    }
    for n in 0..QUEUE as u64 {
        assert_eq!(
            waiting.pop().unwrap().meta.unwrap().message_id,
            format!("msg-{n}")
        );
    }
    // At the end of the stream: told at once, with no later delivery.
    let said = lost(waiting.pop());
    assert_eq!(said.dropped, 3);
    assert_eq!(said.rows, ["CustodialPositionUpdated"]);
    assert!(waiting.pop().is_none(), "said once");

    // With room again, the next is queued after what was lost before it.
    waiting.push(delivery(9), "CustodialPositionUpdated");
    assert_eq!(waiting.pop().unwrap().meta.unwrap().message_id, "msg-9");
}

#[test]
fn a_bus_drop_is_said_with_no_count_and_no_row_which_is_every_row() {
    let waiting = Waiting::default();
    waiting.lose(Some(2), Some("StatementRecorded"));
    waiting.lose(None, None);
    waiting.push(delivery(1), "StatementRecorded");
    let said = lost(waiting.pop());
    assert_eq!((said.dropped, said.rows.len()), (0, 0));
    assert_eq!(waiting.pop().unwrap().meta.unwrap().message_id, "msg-1");

    let mut known = Loss::default();
    known.add(Some(1), Some("A"));
    known.add(Some(2), Some("B"));
    let said = lost(Some(known.delivered()));
    assert_eq!(
        (said.dropped, said.rows),
        (3, vec!["A".to_string(), "B".to_string()])
    );
}

/// Contract v18 (W10.1, W10.5; spec/the-lake, Q12): a reader hears the
/// recorded rows of the datasets it is entitled to, about the subjects it
/// named, the fields it may not read stripped; a dataset it is not entitled
/// to, and a subject it did not name, are not delivered.
#[tokio::test]
async fn a_reader_hears_its_entitled_datasets_rows_about_its_subjects_stripped() {
    use meridian_domain::v1::{
        DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, ObservationMeta, Price,
        PricesRecordedEvent, Source, SubjectRef,
    };
    let contract = Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.lake.{dataset}.event.prices-recorded\tevent\tlake\treporting\n\
         platform.config.event.entitlements-changed\tevent\tconductor\tlake,sidecar\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n",
        "name\tkind\nreporting\trole\nlake\tcomponent\nsidecar\tcomponent\nconductor\tcomponent\n",
    )
    .unwrap();
    let bus = Arc::new(Bus::single(
        "reporting-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, move |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration::default().encode_to_vec(),
        ))
    });
    let sidecar = Sidecar::under(
        &contract,
        Arc::clone(&bus),
        "DEP-test",
        Identity::new("reporting-1", vec!["reporting".into()]),
    );
    sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v18".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    let configuration = EntitlementsChangedEvent {
        datasets: ["coinbase-1:daily", "kraken-1:daily"]
            .iter()
            .map(|d| DatasetRef {
                dataset: d.to_string(),
                ..Default::default()
            })
            .collect(),
        entitlements: vec![DatasetEntitlement {
            dataset: "coinbase-1:daily".into(),
            instance: "reporting-1".into(),
            allowed: true,
            fields: vec![
                "meridian.v1.Price.price".into(),
                "meridian.v1.Price.kind".into(),
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    bus.publish(
        crate::lake::ENTITLEMENTS_CHANGED,
        "meridian.v1.EntitlementsChangedEvent",
        configuration.encode_to_vec(),
        None,
        None,
    )
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest {
            rows: vec!["PricesRecorded".into()],
            subjects: vec!["LCL-BTC".into()],
        }))
        .await
        .unwrap()
        .into_inner();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let price = |dataset: &str, subject: &str| PricesRecordedEvent {
        price: Some(Price {
            meta: Some(ObservationMeta {
                subjects: vec![SubjectRef {
                    entity_id: subject.into(),
                    ..Default::default()
                }],
                source: Some(Source {
                    dataset: dataset.into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            kind: 1,
            basis: 1,
            ..Default::default()
        }),
    };
    for (dataset, subject) in [
        ("kraken-1:daily", "LCL-BTC"),
        ("coinbase-1:daily", "LCL-ETH"),
        ("coinbase-1:daily", "LCL-BTC"),
    ] {
        bus.publish(
            &format!("platform.lake.{dataset}.event.prices-recorded"),
            "meridian.v1.PricesRecordedEvent",
            price(dataset, subject).encode_to_vec(),
            None,
            None,
        )
        .unwrap();
    }
    let delivery = next(&mut stream).await;
    let Some(plugin::delivery::Item::PricesRecorded(heard)) = delivery.item else {
        panic!("{delivery:?}");
    };
    let heard = heard.price.unwrap();
    let meta = heard.meta.unwrap();
    assert_eq!(meta.source.unwrap().dataset, "coinbase-1:daily");
    assert_eq!(meta.subjects[0].entity_id, "LCL-BTC");
    assert_eq!(heard.kind, 1, "an entitled field kept");
    assert_eq!(heard.basis, 0, "a field not entitled stripped");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), stream.next())
            .await
            .is_err(),
        "nothing else was delivered"
    );
}

#[test]
fn a_conflated_rows_later_value_replaces_the_one_waiting() {
    let waiting = Waiting::default();
    let delivery = |row: &str| plugin::Delivery {
        meta: Some(plugin::DeliveryMeta {
            row: row.into(),
            ..Default::default()
        }),
        item: None,
    };
    waiting.push_keyed(delivery("first"), "PricesRecorded", Some("k".into()));
    waiting.push_keyed(delivery("other"), "PricesRecorded", Some("j".into()));
    waiting.push_keyed(delivery("latest"), "PricesRecorded", Some("k".into()));
    assert_eq!(waiting.pop().unwrap().meta.unwrap().row, "latest");
    assert_eq!(waiting.pop().unwrap().meta.unwrap().row, "other");
    assert!(waiting.pop().is_none());
}

/// A registered data plugin `feed-1`, as shard `mine` of its instance.
async fn shard(bus: &Arc<Bus>, mine: u32) -> (Sidecar, Arc<crate::shards::Shards>) {
    let contract = Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.lake.event.observations-wanted\tevent\tlake\tdata\n\
         platform.lake.event.want-withdrawn\tevent\tlake\tdata\n\
         platform.config.event.entitlements-changed\tevent\tconductor\tlake,sidecar\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n",
        "name\tkind\ndata\trole\nlake\tcomponent\nsidecar\tcomponent\nconductor\tcomponent\n",
    )
    .unwrap();
    let shards = Arc::new(crate::shards::Shards::new(mine));
    let sidecar = Sidecar::under(
        &contract,
        Arc::clone(bus),
        "DEP-test",
        Identity::new("feed-1", vec!["data".into()]),
    )
    .as_shard(Arc::clone(&shards));
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v7".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    (sidecar, shards)
}

fn wanted_subjects(delivery: &plugin::Delivery) -> (String, Vec<String>) {
    match delivery.item.as_ref().expect("an item") {
        plugin::delivery::Item::ObservationsWanted(want) => (
            want.want_id.clone(),
            want.subjects.iter().map(|s| s.entity_id.clone()).collect(),
        ),
        other => panic!("not a want: {other:?}"),
    }
}

/// Step 3d (the resilience spec's section 7): two shards of one instance
/// hear the same want, and each hands its plugin its own subjects alone;
/// when one's Lease lapses the other hands its plugin the rest at once, and
/// gives them back when it returns.
#[tokio::test]
async fn two_shards_share_a_want_s_subjects_and_a_lost_shard_s_are_handed_on_at_once() {
    use meridian_domain::v1::{ObservationsWantedEvent, SubjectRef, WantWithdrawnEvent};
    let bus = Arc::new(Bus::single(
        "feed-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, move |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration::default().encode_to_vec(),
        ))
    });
    let (first, at_first) = shard(&bus, 0).await;
    let (second, at_second) = shard(&bus, 1).await;
    at_first.set_live([1]);
    at_second.set_live([0]);
    let mut to_first = first
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();
    let mut to_second = second
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();

    let names: Vec<String> = (0..40).map(|i| format!("INS-{i:03}")).collect();
    let want = ObservationsWantedEvent {
        want_id: "want-1".into(),
        dataset: "feed-1:prices".into(),
        data_type: "meridian.v1.Price".into(),
        subjects: names
            .iter()
            .map(|entity| SubjectRef {
                entity_id: entity.clone(),
                ..Default::default()
            })
            .collect(),
        standing: true,
        ..Default::default()
    };
    bus.publish(
        "platform.lake.event.observations-wanted",
        "meridian.v1.ObservationsWantedEvent",
        want.encode_to_vec(),
        None,
        None,
    )
    .unwrap();
    let (id, mut held_first) = wanted_subjects(&next(&mut to_first).await);
    let (_, held_second) = wanted_subjects(&next(&mut to_second).await);
    assert_eq!(id, "want-1", "the want's own ID at each shard");
    assert!(!held_first.is_empty() && !held_second.is_empty());
    let mut both: Vec<String> = held_first.iter().chain(&held_second).cloned().collect();
    both.sort();
    assert_eq!(both, names, "each subject handed to exactly one shard");

    // The second shard's Lease lapsed: the first is handed every subject,
    // without the lake saying anything again.
    at_first.set_live([]);
    let (id, all) = wanted_subjects(&next(&mut to_first).await);
    assert_eq!((id.as_str(), &all), ("want-1", &names));

    // It returned: the first holds its own again.
    at_first.set_live([1]);
    let (_, mut again) = wanted_subjects(&next(&mut to_first).await);
    again.sort();
    held_first.sort();
    assert_eq!(again, held_first);

    // The lake withdraws it: each shard's plugin hears it, having held some.
    bus.publish(
        "platform.lake.event.want-withdrawn",
        "meridian.v1.WantWithdrawnEvent",
        WantWithdrawnEvent {
            want_id: "want-1".into(),
            dataset: "feed-1:prices".into(),
        }
        .encode_to_vec(),
        None,
        None,
    )
    .unwrap();
    for stream in [&mut to_first, &mut to_second] {
        match next(stream).await.item.unwrap() {
            plugin::delivery::Item::WantWithdrawn(gone) => assert_eq!(gone.want_id, "want-1"),
            other => panic!("not a withdrawal: {other:?}"),
        }
    }

    // A want none of whose subjects is the first shard's is the second's
    // alone: the first's plugin hears nothing of it.
    let theirs = names
        .iter()
        .find(|name| !at_first.keeps(name))
        .unwrap()
        .clone();
    bus.publish(
        "platform.lake.event.observations-wanted",
        "meridian.v1.ObservationsWantedEvent",
        ObservationsWantedEvent {
            want_id: "want-2".into(),
            subjects: vec![SubjectRef {
                entity_id: theirs.clone(),
                ..Default::default()
            }],
            ..want.clone()
        }
        .encode_to_vec(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        wanted_subjects(&next(&mut to_second).await),
        ("want-2".to_string(), vec![theirs])
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), to_first.next())
            .await
            .is_err(),
        "the first shard's plugin was handed a want that is not its own"
    );
}

/// `feed-1`'s bus in memory, the conductor's answer to its configuration
/// served on it.
fn feed_bus() -> Arc<Bus> {
    let bus = Arc::new(Bus::single(
        "feed-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, move |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration::default().encode_to_vec(),
        ))
    });
    bus
}

/// The lake's standing want `id` of `feed-1`'s prices, naming `names`.
fn publish_want(bus: &Bus, id: &str, names: &[String]) {
    use meridian_domain::v1::{ObservationsWantedEvent, SubjectRef};
    bus.publish(
        "platform.lake.event.observations-wanted",
        "meridian.v1.ObservationsWantedEvent",
        ObservationsWantedEvent {
            want_id: id.into(),
            dataset: "feed-1:prices".into(),
            data_type: "meridian.v1.Price".into(),
            subjects: names
                .iter()
                .map(|entity| SubjectRef {
                    entity_id: entity.clone(),
                    ..Default::default()
                })
                .collect(),
            standing: true,
            ..Default::default()
        }
        .encode_to_vec(),
        None,
        None,
    )
    .unwrap();
}

fn publish_withdrawn(bus: &Bus, id: &str) {
    bus.publish(
        "platform.lake.event.want-withdrawn",
        "meridian.v1.WantWithdrawnEvent",
        meridian_domain::v1::WantWithdrawnEvent {
            want_id: id.into(),
            dataset: "feed-1:prices".into(),
        }
        .encode_to_vec(),
        None,
        None,
    )
    .unwrap();
}

/// What the stream delivers until it is quiet for `quiet`: each want's ID
/// and subjects, each withdrawal's ID with none, each loss as "lost".
async fn drained(
    stream: &mut super::Deliveries,
    quiet: std::time::Duration,
) -> Vec<(String, Vec<String>)> {
    let mut heard = Vec::new();
    while let Ok(Some(delivery)) = tokio::time::timeout(quiet, stream.next()).await {
        match delivery.expect("a delivery").item.expect("an item") {
            plugin::delivery::Item::ObservationsWanted(want) => heard.push((
                want.want_id,
                want.subjects.into_iter().map(|s| s.entity_id).collect(),
            )),
            plugin::delivery::Item::WantWithdrawn(gone) => heard.push((gone.want_id, Vec::new())),
            plugin::delivery::Item::Lost(_) => heard.push(("lost".into(), Vec::new())),
            _ => {}
        }
    }
    heard
}

/// Ruling 184 (option B): a taker's hold is handed to its plugin as its
/// stream opens, through the shard's own path -- its share alone, each want
/// once though the take's delivery and the sweep's both came, none
/// withdrawn in the window -- and the stream hears on from where the hold
/// left off: a withdrawal after it reaches the plugin.
#[tokio::test(flavor = "multi_thread")]
async fn a_taker_s_hold_is_handed_as_its_stream_opens_each_want_once() {
    crate::shards::clear_fenced();
    let bus = feed_bus();
    let (sidecar, shards) = shard(&bus, 0).await;
    shards.set_live([1]);
    shards.hold_wants(&bus, "feed-1", std::time::Duration::from_secs(60));
    let names: Vec<String> = (0..40).map(|i| format!("INS-{i:03}")).collect();
    let mine: Vec<String> = names.iter().filter(|n| shards.keeps(n)).cloned().collect();
    assert!(!mine.is_empty() && mine.len() < names.len());
    publish_want(&bus, "want-1", &names);
    publish_want(&bus, "want-2", &names);
    // Held, then withdrawn.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    publish_withdrawn(&bus, "want-2");
    // The sweep's redelivery of want-1 in the same window.
    publish_want(&bus, "want-1", &names);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        drained(&mut stream, std::time::Duration::from_millis(300)).await,
        vec![("want-1".to_string(), mine)],
        "not want-1's share alone, once"
    );
    publish_withdrawn(&bus, "want-1");
    assert_eq!(
        drained(&mut stream, std::time::Duration::from_millis(300)).await,
        vec![("want-1".to_string(), Vec::new())],
        "the withdrawal after the hold did not reach the plugin"
    );
}

/// Publishes `n` standing wants of `feed-1`'s naming `subject`,
/// `want-<from + i>`, a few at a time, so the bus's own queue never drops,
/// and waits for the sidecar to read them.
async fn publish_many(bus: &Bus, from: usize, n: usize, subject: &str) {
    for i in from..from + n {
        publish_want(bus, &format!("want-{i}"), &[subject.to_string()]);
        if i % 100 == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

/// Review c5u2 N1, a withdrawal: a shard's plugin was handed a want, and its
/// queue is full when the lake withdraws it. The withdrawal is never
/// dropped: nothing would tell it again, and the want would be served for
/// good.
#[tokio::test(flavor = "multi_thread")]
async fn a_shard_s_withdrawal_of_a_want_handed_is_never_dropped_from_a_full_queue() {
    let bus = feed_bus();
    let (sidecar, _shards) = shard(&bus, 0).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();
    publish_want(&bus, "first", &["INS-1".to_string()]);
    assert_eq!(wanted_subjects(&next(&mut stream).await).0, "first");
    // The queue filled, and past it.
    publish_many(&bus, 0, QUEUE + 5, "INS-1").await;
    publish_withdrawn(&bus, "first");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let heard = drained(&mut stream, std::time::Duration::from_millis(500)).await;
    assert!(
        heard.contains(&("first".to_string(), Vec::new())),
        "the withdrawal was dropped: {} delivered, the last {:?}",
        heard.len(),
        heard.last()
    );
}

/// Review c5u2 N1, a part: a want heard while the queue is full is dropped,
/// and so is not kept as handed: the next sharing hands it, rather than
/// skipping it as handed until the lake's next redelivery.
#[tokio::test(flavor = "multi_thread")]
async fn a_shard_s_part_dropped_from_a_full_queue_is_handed_at_the_next_sharing() {
    let bus = feed_bus();
    let (sidecar, shards) = shard(&bus, 0).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();
    // A subject shard 0 keeps whether or not shard 1 lives: its part is the
    // same either side of the change below.
    let ours = (0..200)
        .map(|i| format!("INS-{i:03}"))
        .find(|name| {
            crate::shards::keeper(name, &std::collections::BTreeSet::from([0, 1])) == Some(0)
        })
        .unwrap();
    // The queue filled and past it (the stream's own sending holds two
    // more), every filler's part unchanged by the change below too.
    publish_many(&bus, 0, QUEUE + 10, &ours).await;
    publish_want(&bus, "dropped", std::slice::from_ref(&ours));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let heard = drained(&mut stream, std::time::Duration::from_millis(500)).await;
    assert!(
        !heard.iter().any(|(id, _)| id == "dropped"),
        "the queue was not full: {} delivered",
        heard.len()
    );
    // Shard 1 comes up: every standing want is shared again.
    shards.set_live([1]);
    let again = drained(&mut stream, std::time::Duration::from_millis(500)).await;
    assert!(
        again.contains(&("dropped".to_string(), vec![ours])),
        "the dropped part was kept as handed: {} delivered, {:?}",
        again.len(),
        again.iter().take(3).collect::<Vec<_>>()
    );
}

/// Review c5z F1: the live shards change just as a taker's stream is
/// handed its hold, before the stream would hear their next change. The
/// change is shared all the same: the want the hold handed is shared again
/// over the shards now live.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_change_at_the_hold_s_hand_over_is_shared_again() {
    crate::shards::clear_fenced();
    let bus = feed_bus();
    let (sidecar, shards) = shard(&bus, 0).await;
    shards.set_live([1]);
    shards.hold_wants(&bus, "feed-1", std::time::Duration::from_secs(60));
    let names: Vec<String> = (0..40).map(|i| format!("INS-{i:03}")).collect();
    let mine: Vec<String> = names.iter().filter(|n| shards.keeps(n)).cloned().collect();
    assert!(!mine.is_empty() && mine.len() < names.len());
    publish_want(&bus, "want-1", &names);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // The other shard is gone at the hand-over.
    let at = Arc::downgrade(&shards);
    shards.at_hand_over(move || {
        if let Some(shards) = at.upgrade() {
            shards.set_live([]);
        }
    });
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();
    let heard = drained(&mut stream, std::time::Duration::from_millis(500)).await;
    assert_eq!(
        heard.last(),
        Some(&("want-1".to_string(), names)),
        "the change at the hand-over was not shared: {} of 40 handed",
        heard.last().map_or(0, |(_, subjects)| subjects.len())
    );
}

/// Review c5z F2 (b): a want and its withdrawal are two topics, read by two
/// forwarders, so the withdrawal can be heard first. The want heard after
/// it is never handed: the lake has forgotten it, and nothing would
/// withdraw it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_want_heard_on_the_stream_after_its_withdrawal_is_never_handed() {
    let bus = feed_bus();
    let (sidecar, _shards) = shard(&bus, 0).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();
    publish_withdrawn(&bus, "late");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    publish_want(&bus, "late", &["INS-1".to_string()]);
    let heard = drained(&mut stream, std::time::Duration::from_millis(500)).await;
    assert!(heard.is_empty(), "handed after its withdrawal: {heard:?}");
}

/// Review c5z, a nit: a want the take's hold handed carries the message it
/// was heard in, as every other delivery does.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_want_is_handed_with_the_message_it_was_heard_in() {
    crate::shards::clear_fenced();
    let bus = feed_bus();
    let (sidecar, shards) = shard(&bus, 0).await;
    shards.hold_wants(&bus, "feed-1", std::time::Duration::from_secs(60));
    publish_want(&bus, "want-1", &["INS-1".to_string()]);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest::default()))
        .await
        .unwrap()
        .into_inner();
    let delivery = next(&mut stream).await;
    assert_eq!(wanted_subjects(&delivery).0, "want-1");
    let meta = delivery.meta.expect("its meta");
    assert!(
        !meta.message_id.is_empty() && meta.published_at_ns > 0,
        "handed without its message: {meta:?}"
    );
}
