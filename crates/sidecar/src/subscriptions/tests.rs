//! Subscriptions through the sidecar (W4.3; contract v28, amended
//! 2026-10-30), against an in-memory bus with a lake and a conductor stood
//! in: a selector's rows delivered naming it, its fields masked, a window held
//! to the listing and widened by a new series, the refusals, renewals.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, Identifier, ListSeriesReply,
    ListedSeries, ObservationMeta, PluginConfiguration, Quote, QuotesRecordedEvent,
    RenewWantsReply, RenewWantsRequest, SeriesRecordedEvent, Source, SubjectRef,
    TemplateFieldValue,
};
use meridian_pb::plugin::v1 as plugin;
use meridian_pb::plugin::v1::plugin_operations_server::PluginOperations;
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::RegisterRequest;
use prost::Message;
use tokio_stream::StreamExt;
use tonic::{Code, Request};

use crate::grants::Contract;
use crate::service::{Identity, Sidecar};

const DATASET: &str = "tradier-1:options";
const AAPL: &str = "INS-AAPL";
const MSFT: &str = "INS-MSFT";
const NOV: &str = "AAPL  261120C00230000";
const DEC: &str = "AAPL  261218C00230000";
const LATER: &str = "AAPL  261127C00235000";

fn contract() -> Contract {
    Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n\
         platform.lake.{dataset}.event.quotes-recorded\tevent\tlake\tsignal\n\
         platform.lake.{dataset}.event.series-recorded\tevent\tlake\tsignal\n\
         platform.lake.query.list-series\tquery\tsignal\tlake\n\
         platform.lake.command.renew-wants\tcommand\tsidecar\tlake\n\
         platform.street.event.custodial-position-updated\tevent\tstreet\toperations\n\
         platform.config.event.entitlements-changed\tevent\tconductor\tlake,sidecar\n\
         platform.config.query.plugin-configuration\tquery\tsidecar\tconductor\n",
        "name\tkind\nsignal\trole\noperations\trole\nlake\tcomponent\nstreet\tcomponent\n\
         sidecar\tcomponent\nconductor\tcomponent\n",
    )
    .unwrap()
}

struct World {
    sidecar: Sidecar,
    bus: Arc<Bus>,
    renewals: Arc<Mutex<Vec<RenewWantsRequest>>>,
}

fn osi(value: &str) -> Identifier {
    Identifier {
        scheme: "occ_osi".into(),
        value: value.into(),
        ..Default::default()
    }
}

fn listed(key: &str, expires: &str) -> ListedSeries {
    ListedSeries {
        meta: Some(ObservationMeta {
            subjects: vec![SubjectRef {
                entity_id: AAPL.into(),
                ..Default::default()
            }],
            series_key: vec![osi(key)],
            source: Some(Source {
                dataset: DATASET.into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        fields: vec![TemplateFieldValue {
            path: "expiration_date".into(),
            value: expires.into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn entitled(fields: &[&str]) -> Vec<u8> {
    EntitlementsChangedEvent {
        datasets: vec![DatasetRef {
            dataset: DATASET.into(),
            ..Default::default()
        }],
        entitlements: vec![DatasetEntitlement {
            dataset: DATASET.into(),
            instance: "signal-1".into(),
            allowed: true,
            fields: fields.iter().map(|f| f.to_string()).collect(),
            ..Default::default()
        }],
        ..Default::default()
    }
    .encode_to_vec()
}

/// A registered `signal-1`, entitled to the chain's bid and ask, with the
/// lake's listing and renewals stood in.
async fn world() -> World {
    let bus = Arc::new(Bus::single(
        "signal-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve(crate::configuration::PLUGIN_CONFIGURATION, move |_| {
        Ok((
            "meridian.v1.PluginConfiguration".into(),
            PluginConfiguration {
                plugin_instance_id: "signal-1".into(),
                read_account_ids: vec!["ACC-1".into()],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve("platform.lake.query.list-series", |_| {
        Ok((
            "meridian.v1.ListSeriesReply".into(),
            ListSeriesReply {
                series: vec![listed(NOV, "2026-11-20")],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let renewals = Arc::new(Mutex::new(Vec::new()));
    let heard = Arc::clone(&renewals);
    bus.serve(super::RENEW_WANTS, move |envelope| {
        heard
            .lock()
            .unwrap()
            .push(RenewWantsRequest::decode(&envelope.payload[..]).unwrap());
        Ok((
            "meridian.v1.RenewWantsReply".into(),
            RenewWantsReply {}.encode_to_vec(),
        ))
    });
    let sidecar = Sidecar::under(
        &contract(),
        Arc::clone(&bus),
        "DEP-test",
        Identity::new("signal-1", vec!["signal".into()]),
    );
    sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v28".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    bus.publish(
        crate::lake::ENTITLEMENTS_CHANGED,
        "meridian.v1.EntitlementsChangedEvent",
        entitled(&[
            "meridian.v1.Quote.meta",
            "meridian.v1.Quote.bid",
            "meridian.v1.Quote.ask",
        ]),
        None,
        None,
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    World {
        sidecar,
        bus,
        renewals,
    }
}

fn money(_amount: &str) -> Option<meridian_domain::v1::Money> {
    Some(meridian_domain::v1::Money {
        currency_code: "USD".into(),
        ..Default::default()
    })
}

fn quote(subject: &str, series: Option<&str>, recorded_at_ns: i64) -> Vec<u8> {
    QuotesRecordedEvent {
        quote: Some(Quote {
            meta: Some(ObservationMeta {
                row_key: format!("{subject}:{series:?}:{recorded_at_ns}"),
                subjects: vec![SubjectRef {
                    entity_id: subject.into(),
                    ..Default::default()
                }],
                series_key: series.map(osi).into_iter().collect(),
                source: Some(Source {
                    dataset: DATASET.into(),
                    ..Default::default()
                }),
                recorded_at_ns,
                ..Default::default()
            }),
            bid: money("1.10"),
            ask: money("1.20"),
            ..Default::default()
        }),
    }
    .encode_to_vec()
}

fn publish_quote(bus: &Bus, subject: &str, series: Option<&str>) {
    bus.publish(
        &format!("platform.lake.{DATASET}.event.quotes-recorded"),
        "meridian.v1.QuotesRecordedEvent",
        quote(subject, series, now_ns()),
        None,
        None,
    )
    .unwrap();
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

async fn next(stream: &mut crate::receive::Deliveries) -> plugin::Delivery {
    tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("nothing was delivered within two seconds")
        .expect("the stream ended")
        .expect("a delivery, not a refusal")
}

async fn nothing_more(stream: &mut crate::receive::Deliveries) {
    assert!(
        tokio::time::timeout(Duration::from_millis(150), stream.next())
            .await
            .is_err(),
        "nothing else was delivered"
    );
}

async fn stream_of(sidecar: &Sidecar, rows: &[&str]) -> crate::receive::Deliveries {
    let stream = sidecar
        .receive(Request::new(plugin::ReceiveRequest {
            rows: rows.iter().map(|r| r.to_string()).collect(),
            subjects: Vec::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    tokio::time::sleep(Duration::from_millis(50)).await;
    stream
}

fn chain(id: &str) -> plugin::SubscribeRequest {
    plugin::SubscribeRequest {
        subscription_id: id.into(),
        row: "QuotesRecorded".into(),
        subjects: vec![AAPL.into()],
        all_series: true,
        expiration_from: "2026-11-01".into(),
        expiration_to: "2026-11-30".into(),
        fields: vec!["meridian.v1.Quote.bid".into()],
        ..Default::default()
    }
}

fn own(id: &str) -> plugin::SubscribeRequest {
    plugin::SubscribeRequest {
        subscription_id: id.into(),
        row: "QuotesRecorded".into(),
        subjects: vec![AAPL.into()],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_subscription_with_no_stream_open_is_refused() {
    let w = world().await;
    let refused = w
        .sidecar
        .subscribe(Request::new(chain("view-1")))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition, "{refused:?}");
}

/// The fixture's base case: a chain's quotes in a window, the bid alone.
#[tokio::test]
async fn a_chains_quotes_in_its_window_are_delivered_naming_the_subscription_masked() {
    let w = world().await;
    let mut stream = stream_of(&w.sidecar, &["QuotesRecorded"]).await;
    w.sidecar
        .subscribe(Request::new(chain("view-1")))
        .await
        .unwrap();

    // Outside the window, another underlying, then in it.
    publish_quote(&w.bus, AAPL, Some(DEC));
    publish_quote(&w.bus, MSFT, Some(NOV));
    publish_quote(&w.bus, AAPL, Some(NOV));
    let delivery = next(&mut stream).await;
    assert_eq!(
        delivery.meta.as_ref().unwrap().subscription_ids,
        vec!["view-1"]
    );
    let Some(plugin::delivery::Item::QuotesRecorded(heard)) = delivery.item else {
        panic!("{delivery:?}");
    };
    let quote = heard.quote.unwrap();
    assert_eq!(quote.meta.unwrap().series_key[0].value, NOV);
    assert!(quote.bid.is_some(), "the field asked kept");
    assert!(quote.ask.is_none(), "a field entitled but not asked stripped");
    nothing_more(&mut stream).await;

    // Its want renewed at once.
    let renewed = w.renewals.lock().unwrap().clone();
    assert_eq!(renewed.len(), 1);
    let want = &renewed[0].wants[0];
    assert_eq!(want.data_type, "meridian.v1.Quote");
    assert!(want.all_series);
    assert_eq!(
        (want.expiration_from.as_str(), want.expiration_to.as_str()),
        ("2026-11-01", "2026-11-30")
    );
}

#[tokio::test]
async fn a_series_listed_later_in_the_window_is_heard_from_then() {
    let w = world().await;
    let mut stream = stream_of(&w.sidecar, &["QuotesRecorded"]).await;
    w.sidecar
        .subscribe(Request::new(chain("view-1")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    publish_quote(&w.bus, AAPL, Some(LATER));
    nothing_more(&mut stream).await;

    w.bus
        .publish(
            &format!("platform.lake.{DATASET}.event.series-recorded"),
            "meridian.v1.SeriesRecordedEvent",
            SeriesRecordedEvent {
                series: Some(listed(LATER, "2026-11-27")),
            }
            .encode_to_vec(),
            None,
            None,
        )
        .unwrap();
    // The plugin's stream did not ask for SeriesRecorded: heard by the
    // sidecar, not delivered.
    nothing_more(&mut stream).await;
    publish_quote(&w.bus, AAPL, Some(LATER));
    let delivery = next(&mut stream).await;
    assert_eq!(delivery.meta.unwrap().subscription_ids, vec!["view-1"]);
}

#[tokio::test]
async fn the_same_id_again_is_the_first_and_another_selector_under_it_is_refused() {
    let w = world().await;
    let _stream = stream_of(&w.sidecar, &["QuotesRecorded"]).await;
    w.sidecar
        .subscribe(Request::new(chain("view-1")))
        .await
        .unwrap();
    w.sidecar
        .subscribe(Request::new(chain("view-1")))
        .await
        .expect("the same id and selector is answered as the first");
    let mut other = chain("view-1");
    other.fields = vec!["meridian.v1.Quote.ask".into()];
    let refused = w.sidecar.subscribe(Request::new(other)).await.unwrap_err();
    assert_eq!(refused.code(), Code::AlreadyExists);

    for (label, request, code) in [
        (
            "a row its roles do not hear",
            plugin::SubscribeRequest {
                row: "CustodialPositionUpdated".into(),
                ..chain("view-2")
            },
            Code::PermissionDenied,
        ),
        (
            "a window without all_series",
            plugin::SubscribeRequest {
                all_series: false,
                ..chain("view-3")
            },
            Code::InvalidArgument,
        ),
        (
            "a field of another message",
            plugin::SubscribeRequest {
                fields: vec!["meridian.v1.Price.price".into()],
                ..chain("view-4")
            },
            Code::InvalidArgument,
        ),
        ("no id", chain(""), Code::InvalidArgument),
    ] {
        let refused = w.sidecar.subscribe(Request::new(request)).await.unwrap_err();
        assert_eq!(refused.code(), code, "{label}: {refused:?}");
    }
}

#[tokio::test]
async fn past_the_bound_a_subscription_is_refused() {
    let w = world().await;
    let _stream = stream_of(&w.sidecar, &["QuotesRecorded"]).await;
    for i in 0..super::MOST_OPEN {
        w.sidecar
            .subscribe(Request::new(plugin::SubscribeRequest {
                subjects: vec![format!("INS-{i}")],
                ..own(&format!("view-{i}"))
            }))
            .await
            .unwrap();
    }
    let refused = w
        .sidecar
        .subscribe(Request::new(own("one-more")))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::ResourceExhausted);
}

/// A row only a subscription hears is delivered only where it matches;
/// once dropped, nothing more is queued for it; an id not open is
/// answered as dropped.
#[tokio::test]
async fn an_unsubscribed_selector_hears_nothing_more() {
    let w = world().await;
    // The stream names another row: quotes are the subscription's alone.
    let mut stream = stream_of(&w.sidecar, &["SeriesRecorded"]).await;
    w.sidecar
        .subscribe(Request::new(own("own")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    publish_quote(&w.bus, AAPL, None);
    let delivery = next(&mut stream).await;
    let meta = delivery.meta.unwrap();
    assert_eq!(meta.subscription_ids, vec!["own"]);
    let Some(plugin::delivery::Item::QuotesRecorded(heard)) = delivery.item else {
        panic!("a quote");
    };
    assert!(
        heard.quote.unwrap().ask.is_some(),
        "no fields asked: every field entitled"
    );
    // A chain's row is not the underlying's own subscription's.
    publish_quote(&w.bus, AAPL, Some(NOV));
    nothing_more(&mut stream).await;

    w.sidecar
        .unsubscribe(Request::new(plugin::UnsubscribeRequest {
            subscription_id: "own".into(),
        }))
        .await
        .unwrap();
    w.sidecar
        .unsubscribe(Request::new(plugin::UnsubscribeRequest {
            subscription_id: "never-opened".into(),
        }))
        .await
        .expect("an id not open is answered as dropped");
    publish_quote(&w.bus, AAPL, None);
    nothing_more(&mut stream).await;
}

/// Entitlements decided again on EntitlementsChanged, for an open selector.
#[tokio::test]
async fn entitlements_changed_while_open_change_what_it_hears() {
    let w = world().await;
    let mut stream = stream_of(&w.sidecar, &["QuotesRecorded"]).await;
    w.sidecar
        .subscribe(Request::new(own("own")))
        .await
        .unwrap();
    w.bus
        .publish(
            crate::lake::ENTITLEMENTS_CHANGED,
            "meridian.v1.EntitlementsChangedEvent",
            entitled(&["meridian.v1.Quote.meta", "meridian.v1.Quote.ask"]),
            None,
            None,
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    publish_quote(&w.bus, AAPL, None);
    let delivery = next(&mut stream).await;
    let Some(plugin::delivery::Item::QuotesRecorded(heard)) = delivery.item else {
        panic!("a quote");
    };
    let quote = heard.quote.unwrap();
    assert!(quote.bid.is_none() && quote.ask.is_some(), "{quote:?}");
}

/// The quote-to-screen budget's sidecar part (ruling 799): from a quote's
/// recorded time to its delivery to the plugin, p99 within 500 ms over a
/// thousand quotes on twenty series of one open chain.
#[tokio::test(flavor = "multi_thread")]
async fn a_subscriptions_quotes_reach_the_plugin_within_the_budget() {
    let w = world().await;
    let mut stream = stream_of(&w.sidecar, &["QuotesRecorded"]).await;
    w.sidecar
        .subscribe(Request::new(plugin::SubscribeRequest {
            all_series: true,
            ..own("chain")
        }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let keys: Vec<String> = (0..20)
        .map(|i| format!("AAPL  261120C00{:03}000", 200 + i))
        .collect();
    let mut ages_ms = Vec::new();
    for round in 0..50 {
        for key in &keys {
            publish_quote(&w.bus, AAPL, Some(key));
        }
        let mut seen = 0;
        while seen < keys.len() {
            let Ok(Some(Ok(delivery))) =
                tokio::time::timeout(Duration::from_millis(300), stream.next()).await
            else {
                break;
            };
            let Some(plugin::delivery::Item::QuotesRecorded(heard)) = delivery.item else {
                continue;
            };
            let recorded = heard.quote.unwrap().meta.unwrap().recorded_at_ns;
            ages_ms.push((now_ns() - recorded) as f64 / 1e6);
            seen += 1;
        }
        assert!(seen > 0, "round {round} delivered nothing");
    }
    ages_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p99 = ages_ms[(ages_ms.len() * 99 / 100).min(ages_ms.len() - 1)];
    eprintln!(
        "quote_to_delivery: {} quotes, p50 {:.2} ms, p99 {:.2} ms",
        ages_ms.len(),
        ages_ms[ages_ms.len() / 2],
        p99
    );
    assert!(p99 <= 500.0, "p99 {p99} ms");
}

#[test]
fn a_listed_series_expiry_is_read_from_its_terms() {
    assert_eq!(
        super::expiry(&listed(NOV, "2026-11-20")).map(|d| d.to_string()),
        Some("2026-11-20".to_string())
    );
    assert_eq!(super::expiry(&ListedSeries::default()), None);
    assert_eq!(super::series_named(&osi(NOV)), format!("occ_osi={NOV}"));
}

/// A subscription opens on the newest stream and is delivered there alone;
/// a stream closed drops its own subscriptions, not another's.
#[tokio::test]
async fn a_subscription_lives_on_the_stream_it_opened_on() {
    let w = world().await;
    let mut first = stream_of(&w.sidecar, &["SeriesRecorded"]).await;
    let second = stream_of(&w.sidecar, &["SeriesRecorded"]).await;
    w.sidecar
        .subscribe(Request::new(own("on-second")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    publish_quote(&w.bus, AAPL, None);
    nothing_more(&mut first).await;
    drop(second);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Dropped with its stream: opened again on the one left.
    w.sidecar
        .subscribe(Request::new(own("on-first")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    publish_quote(&w.bus, AAPL, None);
    let delivery = next(&mut first).await;
    assert_eq!(delivery.meta.unwrap().subscription_ids, vec!["on-first"]);
}
