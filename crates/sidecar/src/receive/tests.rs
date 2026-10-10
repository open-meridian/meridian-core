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
