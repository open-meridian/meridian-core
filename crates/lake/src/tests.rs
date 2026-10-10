//! The lake's rules, on the in-memory store, through its handlers' code with
//! envelopes as sidecars stamp them; the instrument store and the conductor
//! stood in on the bus.

use std::sync::Arc;

use meridian_bus::{Bus, Envelope, MemoryBackend, MessageMeta};
use meridian_clock::{Clock, ManualClock};
use meridian_domain::v1::{
    Bar, DatasetEntitlement, DatasetRef, DeclineWantRequest, EntitlementsChangedEvent,
    InstrumentRecord, InstrumentReplacedEvent, Money, ObservationMeta, ObservationsWantedEvent,
    Price, PriceBasis, PriceKind, ResolveIdentifierReply, ResolveIdentifierRequest,
    ResolveInstrumentReply, ResolveInstrumentRequest, SetSourcePriorityRequest, Source,
    SourceChoice, SubjectRef, UnansweredReason, WantWithdrawnEvent,
};
use meridian_pb::v1::{AsReported, DatasetDeclaration, DatasetLicence, Decimal, ObservationMode};
use prost::Message;

use crate::row::{DataType, Observation};
use crate::service::{self, Lake, ReadRequest};
use crate::store::Store;
use crate::MemoryStore;

const NOW: i64 = 1_791_417_600_000_000_000; // 2026-10-08T00:00Z
const DAY: i64 = 86_400 * 1_000_000_000;
const BTC: &str = "LCL-BTC";
const USD: &str = "LCL-USD";
const USDC: &str = "LCL-USDC";
const AAPL: &str = "LCL-AAPL";

struct Harness {
    bus: Arc<Bus>,
    store: Arc<MemoryStore>,
    clock: Arc<ManualClock>,
    lake: Arc<Lake>,
}

fn harness() -> Harness {
    let clock = Arc::new(ManualClock::at(NOW));
    let bus = Arc::new(Bus::single(
        "lake-1",
        Arc::new(MemoryBackend::new()),
        clock.clone(),
    ));
    // The instrument store, stood in: four records, USD by its code.
    bus.serve(service::RESOLVE_INSTRUMENT, |envelope| {
        let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        let found = [BTC, USD, USDC, AAPL, "LCL-OLD"].contains(&asked.instrument_id.as_str());
        Ok((
            "meridian.v1.ResolveInstrumentReply".into(),
            ResolveInstrumentReply {
                found,
                instrument: found.then(|| InstrumentRecord {
                    instrument_id: asked.instrument_id.clone(),
                    ..Default::default()
                }),
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(service::RESOLVE_IDENTIFIER, |envelope| {
        let asked = ResolveIdentifierRequest::decode(&envelope.payload[..]).unwrap();
        let usd = asked.identifiers[0].scheme == "iso4217" && asked.identifiers[0].value == "USD";
        Ok((
            "meridian.v1.ResolveIdentifierReply".into(),
            ResolveIdentifierReply {
                found: usd,
                instrument_id: if usd { USD.into() } else { String::new() },
                miss_reason: if usd { 0 } else { 1 },
                minted: false,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(service::PLUGIN_CATALOGUE, |_| {
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            meridian_domain::v1::PluginCatalogue {
                launches: vec![meridian_domain::v1::PluginLaunch {
                    instance_id: "coinbase-1".into(),
                    version: "0.3.1".into(),
                    state: meridian_domain::v1::PluginLaunchState::Launched as i32,
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    let store = Arc::new(MemoryStore::new());
    let lake = service::serve(bus.clone(), store.clone(), clock.clone());
    lake.configured(&configuration(true, &[]));
    Harness {
        bus,
        store,
        clock,
        lake,
    }
}

fn declaration(key: &str, cadence: u32, kept: bool) -> DatasetDeclaration {
    DatasetDeclaration {
        key: key.into(),
        vendor: "Coinbase".into(),
        data_types: vec!["meridian.v1.Price".into(), "meridian.v1.Bar".into()],
        modes: vec![ObservationMode::Pull as i32],
        cadence,
        history: 3650,
        day_time_zone: "Etc/UTC".into(),
        licence_default: Some(DatasetLicence {
            kept,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// coinbase-1:daily and kraken-1:daily; reporting-1 entitled to both, every
/// field, unless `bar_fields` narrows coinbase's; reporting-2 to neither.
fn configuration(kept: bool, bar_fields: &[&str]) -> EntitlementsChangedEvent {
    let dataset = |instance: &str, key: &str| DatasetRef {
        dataset: format!("{instance}:{key}"),
        instance: instance.into(),
        vendor: "Coinbase".into(),
        declaration: Some(declaration(key, 0, kept)),
        ..Default::default()
    };
    let entitled = |dataset: &str, fields: &[&str]| DatasetEntitlement {
        dataset: dataset.into(),
        instance: "reporting-1".into(),
        allowed: true,
        fields: fields.iter().map(|f| f.to_string()).collect(),
        ..Default::default()
    };
    EntitlementsChangedEvent {
        datasets: vec![dataset("coinbase-1", "daily"), dataset("kraken-1", "daily")],
        licences: vec![],
        entitlements: vec![
            entitled("coinbase-1:daily", bar_fields),
            entitled("kraken-1:daily", &[]),
        ],
        changed_at_ns: NOW,
    }
}

fn envelope(instance: &str, plugin: bool, person: &str) -> Envelope {
    Envelope {
        meta: Some(MessageMeta {
            publisher_instance_id: instance.into(),
            account_scope_applies: plugin,
            acting_for_subject: person.into(),
            published_at_ns: NOW,
            ..Default::default()
        }),
        payload_type: String::new(),
        payload: Vec::new(),
    }
}

fn decimal(text: &str) -> Option<Decimal> {
    Some(
        text.parse::<meridian_domain::exact::Exact>()
            .unwrap()
            .to_wire(),
    )
}

fn money(text: &str, code: &str, instrument: &str) -> Option<Money> {
    Some(Money {
        amount: decimal(text),
        currency_code: code.into(),
        instrument_id: instrument.into(),
    })
}

fn close(instance: &str, row_key: &str, subject: &str, date: &str, amount: &str) -> Price {
    Price {
        meta: Some(ObservationMeta {
            row_key: row_key.into(),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
            }],
            source: Some(Source {
                instance: instance.into(),
                dataset: format!("{instance}:daily"),
                ..Default::default()
            }),
            valid_from_ns: NOW - DAY,
            valid_until_ns: NOW,
            business_date: date.into(),
            ..Default::default()
        }),
        kind: PriceKind::Close as i32,
        price: money(amount, "USD", ""),
        basis: PriceBasis::PerUnit as i32,
    }
}

fn record(
    h: &Harness,
    instance: &str,
    prices: Vec<Price>,
    want: &str,
) -> Result<meridian_domain::v1::RecordObservationsReply, String> {
    tokio::task::block_in_place(|| {
        h.lake.record(
            "prices",
            prices.into_iter().map(Observation::Price).collect(),
            want,
            &envelope(instance, true, ""),
        )
    })
}

fn read(
    h: &Harness,
    reader: &str,
    subjects: &[&str],
    date: &str,
    sources: SourceChoice,
    as_of_ns: i64,
) -> service::ReadAnswer {
    tokio::task::block_in_place(|| {
        h.lake.read(
            ReadRequest {
                data_type: DataType::Price,
                subjects: subjects
                    .iter()
                    .map(|s| SubjectRef {
                        entity_id: s.to_string(),
                    })
                    .collect(),
                kinds: vec![],
                interval_ns: 0,
                sources: Some(sources),
                at_ns: 0,
                business_date: date.into(),
                valid_from_ns: 0,
                valid_until_ns: 0,
                as_of_ns,
                page_size: 0,
                cursor: String::new(),
            },
            &envelope(reader, reader != "dashboard-1", ""),
        )
    })
    .unwrap()
}

fn amount(price: &Price) -> String {
    meridian_domain::exact::Exact::from_wire(price.price.as_ref().unwrap().amount.as_ref().unwrap())
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_close_is_recorded_heard_restated_and_read_as_of_before_its_restatement() {
    let h = harness();
    let mut heard = h
        .bus
        .subscribe("platform.lake.coinbase-1:daily.event.prices-recorded");
    let first = record(
        &h,
        "coinbase-1",
        vec![close(
            "coinbase-1",
            "BTC-USD:2026-10-07",
            BTC,
            "2026-10-07",
            "62000.5",
        )],
        "",
    )
    .unwrap();
    assert_eq!((first.recorded, first.restated, first.unchanged), (1, 0, 0));
    assert_eq!(first.watermark.unwrap().partitions[0].sequence, 1);
    let event = meridian_domain::v1::PricesRecordedEvent::decode(
        &heard.recv().await.unwrap().envelope.payload[..],
    )
    .unwrap();
    let price = event.price.unwrap();
    let meta = price.meta.as_ref().unwrap();
    assert_eq!(
        (meta.version, meta.sequence, meta.previous_sequence),
        (1, 1, 0)
    );
    assert_eq!(meta.recorded_at_ns, NOW);
    assert_eq!(meta.sent_at_ns, NOW, "the envelope's publication time");
    assert_eq!(
        meta.source.as_ref().unwrap().plugin_version,
        "0.3.1",
        "the launched version"
    );
    assert_eq!(
        price.price.as_ref().unwrap().instrument_id,
        USD,
        "a code resolved to its cash instrument"
    );

    // The same value at another scale changes nothing (Q31).
    let same = record(
        &h,
        "coinbase-1",
        vec![close(
            "coinbase-1",
            "BTC-USD:2026-10-07",
            BTC,
            "2026-10-07",
            "62000.50",
        )],
        "",
    )
    .unwrap();
    assert_eq!(same.unchanged, 1);

    let before = NOW;
    h.clock.advance(1_000);
    let restated = record(
        &h,
        "coinbase-1",
        vec![close(
            "coinbase-1",
            "BTC-USD:2026-10-07",
            BTC,
            "2026-10-07",
            "62100",
        )],
        "",
    )
    .unwrap();
    assert_eq!(restated.restated, 1);

    let now = read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    assert_eq!(now.rows.len(), 1);
    let Observation::Price(latest) = &now.rows[0] else {
        panic!()
    };
    assert_eq!(latest.meta.as_ref().unwrap().version, 2);
    assert_eq!(amount(latest), "62100");
    let then = read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        before,
    );
    let Observation::Price(old) = &then.rows[0] else {
        panic!()
    };
    assert_eq!(
        (old.meta.as_ref().unwrap().version, amount(old)),
        (1, "62000.5".to_string())
    );
    assert_eq!(now.datasets[0].dataset, "coinbase-1:daily");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_not_entitled_is_refused_per_dataset_and_a_field_it_may_not_read_is_stripped() {
    let h = harness();
    record(
        &h,
        "coinbase-1",
        vec![close("coinbase-1", "k1", BTC, "2026-10-07", "1")],
        "",
    )
    .unwrap();
    let stranger = read(
        &h,
        "reporting-2",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    assert!(stranger.rows.is_empty());
    assert_eq!(
        stranger.unanswered[0].reason,
        UnansweredReason::NotEntitled as i32
    );
    let named = read(
        &h,
        "reporting-2",
        &[BTC],
        "2026-10-07",
        SourceChoice {
            named: vec!["coinbase-1:daily".into()],
            ..Default::default()
        },
        0,
    );
    assert!(named.unanswered.iter().any(
        |u| u.dataset == "coinbase-1:daily" && u.reason == UnansweredReason::NotEntitled as i32
    ));

    // Entitled to a bar's open and close alone: its vwap stripped, named.
    h.lake.configured(&configuration(
        true,
        &["meridian.v1.Bar.open", "meridian.v1.Bar.close"],
    ));
    let bar = Bar {
        meta: close("coinbase-1", "bar-1", BTC, "2026-10-07", "1").meta,
        open: money("1", "USD", ""),
        high: money("3", "USD", ""),
        low: money("1", "USD", ""),
        close: money("2", "USD", ""),
        volume: decimal("10"),
        vwap: money("2.1", "USD", ""),
        trade_count: Some(4),
    };
    tokio::task::block_in_place(|| {
        h.lake.record(
            "bars",
            vec![Observation::Bar(bar)],
            "",
            &envelope("coinbase-1", true, ""),
        )
    })
    .unwrap();
    let answer = h
        .lake
        .read(
            ReadRequest {
                data_type: DataType::Bar,
                subjects: vec![SubjectRef {
                    entity_id: BTC.into(),
                }],
                kinds: vec![],
                interval_ns: 0,
                sources: None,
                at_ns: 0,
                business_date: "2026-10-07".into(),
                valid_from_ns: 0,
                valid_until_ns: 0,
                as_of_ns: 0,
                page_size: 0,
                cursor: String::new(),
            },
            &envelope("reporting-1", true, ""),
        )
        .unwrap();
    let Observation::Bar(bar) = &answer.rows[0] else {
        panic!()
    };
    assert!(bar.vwap.is_none() && bar.high.is_none() && bar.close.is_some());
    assert!(answer
        .unanswered
        .iter()
        .any(|u| u.field == "meridian.v1.Bar.vwap"
            && u.reason == UnansweredReason::NotEntitled as i32));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_not_answered_is_wanted_once_for_two_readers_answered_and_a_decline_is_said() {
    let h = harness();
    let mut wanted = h.bus.subscribe(service::OBSERVATIONS_WANTED);
    let first = read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    assert_eq!(
        first.unanswered[0].reason,
        UnansweredReason::AskedSource as i32
    );
    let want = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    assert_eq!(
        (want.dataset.as_str(), want.business_date.as_str()),
        ("coinbase-1:daily", "2026-10-07")
    );
    assert!(!want.standing, "a date's read is not standing");
    // A second reader's identical ask coalesces: nothing published again.
    h.lake.configured(&{
        let mut c = configuration(true, &[]);
        c.entitlements.push(DatasetEntitlement {
            dataset: "coinbase-1:daily".into(),
            instance: "reporting-2".into(),
            allowed: true,
            ..Default::default()
        });
        c
    });
    read(
        &h,
        "reporting-2",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), wanted.recv())
            .await
            .is_err()
    );

    record(
        &h,
        "coinbase-1",
        vec![close("coinbase-1", "k", BTC, "2026-10-07", "5")],
        &want.want_id,
    )
    .unwrap();
    let changes = h.store.want_changes().unwrap();
    assert_eq!(
        changes.len(),
        2,
        "asked, then answered, each its own record"
    );

    // AAPL is not covered by coinbase: declined, and the next read says so.
    read(
        &h,
        "reporting-1",
        &[AAPL],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    let aapl = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    let wrong = tokio::task::block_in_place(|| {
        h.lake.decline(
            DeclineWantRequest {
                want_id: aapl.want_id.clone(),
                subjects: vec![SubjectRef {
                    entity_id: AAPL.into(),
                }],
                reason: UnansweredReason::NotCovered as i32,
            },
            &envelope("kraken-1", true, ""),
        )
    });
    assert!(wrong.unwrap_err().contains("does not serve"));
    h.lake
        .decline(
            DeclineWantRequest {
                want_id: aapl.want_id,
                subjects: vec![SubjectRef {
                    entity_id: AAPL.into(),
                }],
                reason: UnansweredReason::NotCovered as i32,
            },
            &envelope("coinbase-1", true, ""),
        )
        .unwrap();
    let again = read(
        &h,
        "reporting-1",
        &[AAPL],
        "2026-10-07",
        SourceChoice {
            named: vec!["coinbase-1:daily".into()],
            ..Default::default()
        },
        0,
    );
    assert_eq!(
        again.unanswered[0].reason,
        UnansweredReason::NotCovered as i32
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_want_no_reader_asks_within_its_cadence_is_withdrawn() {
    let h = harness();
    let mut c = configuration(true, &[]);
    c.datasets[0].declaration.as_mut().unwrap().cadence = 60;
    h.lake.configured(&c);
    let mut wanted = h.bus.subscribe(service::OBSERVATIONS_WANTED);
    let mut withdrawn = h.bus.subscribe(service::WANT_WITHDRAWN);
    read(&h, "reporting-1", &[BTC], "", SourceChoice::default(), 0);
    let want = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    assert!(want.standing, "the latest, of a dataset with a cadence");
    h.clock.advance(60 * 1_000_000_000);
    h.lake.sweep_wants();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), withdrawn.recv())
            .await
            .is_err(),
        "still within the window"
    );
    h.clock.advance(121 * 1_000_000_000);
    h.lake.sweep_wants();
    let gone =
        WantWithdrawnEvent::decode(&withdrawn.recv().await.unwrap().envelope.payload[..]).unwrap();
    assert_eq!(gone.want_id, want.want_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dataset_not_kept_is_served_and_only_that_it_was_served_is_kept() {
    let h = harness();
    h.lake.configured(&configuration(false, &[]));
    let mut heard = h
        .bus
        .subscribe("platform.lake.coinbase-1:daily.event.prices-recorded");
    let done = record(
        &h,
        "coinbase-1",
        vec![close("coinbase-1", "k", BTC, "2026-10-07", "5")],
        "",
    )
    .unwrap();
    assert_eq!(done.recorded, 1);
    heard.recv().await.unwrap();
    assert!(h.store.counts().unwrap().is_empty(), "no row kept");
    let served = h.store.served().unwrap();
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].subjects, vec![BTC]);
    assert_eq!(served[0].readers, vec!["reporting-1"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_is_refused_whole_naming_the_item_and_field() {
    let h = harness();
    let ok = || close("coinbase-1", "k", BTC, "2026-10-07", "1");
    let cases: Vec<(Vec<Price>, &str)> = vec![
        ((0..501).map(|_| ok()).collect(), "a batch is 1 to 500"),
        (
            vec![ok(), close("kraken-1", "k", BTC, "2026-10-07", "1")],
            "prices[1].meta.source.instance",
        ),
        (
            vec![Price {
                price: money("1", "USDC", ""),
                ..ok()
            }],
            "prices[0].price.currency_code",
        ),
        (
            vec![Price {
                price: money("1", "", ""),
                ..ok()
            }],
            "prices[0].price names no asset",
        ),
        (
            vec![close("coinbase-1", "k", BTC, "2026-02-30", "1")],
            "prices[0].meta.business_date",
        ),
        (
            vec![close("coinbase-1", "k", "LCL-NONE", "2026-10-07", "1")],
            "prices[0].meta.subjects[0].entity_id",
        ),
        (
            vec![Price { kind: 0, ..ok() }],
            "prices[0].kind is unspecified",
        ),
        (
            vec![Price {
                price: money("1", "EUR", ""),
                ..ok()
            }],
            "EUR names no one cash instrument",
        ),
        (
            vec![Price {
                price: money("1", "USD", USDC),
                ..ok()
            }],
            "both set name the same asset",
        ),
    ];
    for (prices, words) in cases {
        let refused = record(&h, "coinbase-1", prices, "").unwrap_err();
        assert!(refused.contains(words), "{words}: {refused}");
    }
    let mut eight = ok();
    eight.meta.as_mut().unwrap().subjects = (0..9)
        .map(|_| SubjectRef {
            entity_id: BTC.into(),
        })
        .collect();
    assert!(record(&h, "coinbase-1", vec![eight], "")
        .unwrap_err()
        .contains("1 to 8"));
    let mut undeclared = ok();
    undeclared
        .meta
        .as_mut()
        .unwrap()
        .source
        .as_mut()
        .unwrap()
        .dataset = "coinbase-1:live".into();
    assert!(record(&h, "coinbase-1", vec![undeclared], "")
        .unwrap_err()
        .contains("no dataset this instance's catalogue declares"));
    let two_assets = Bar {
        meta: ok().meta,
        open: money("1", "USD", ""),
        high: money("1", "", USDC),
        low: money("1", "USD", ""),
        close: money("1", "USD", ""),
        volume: decimal("1"),
        vwap: None,
        trade_count: None,
    };
    let refused = tokio::task::block_in_place(|| {
        h.lake.record(
            "bars",
            vec![Observation::Bar(two_assets)],
            "",
            &envelope("coinbase-1", true, ""),
        )
    })
    .unwrap_err();
    assert!(refused.contains("one asset"), "{refused}");
    assert!(
        h.store.counts().unwrap().is_empty(),
        "nothing recorded by any refusal"
    );

    // A USDC price is kept on the token's own instrument, never a fiat code;
    // a value that failed conversion is kept as reported.
    let usdc = Price {
        price: money("0.9998", "", USDC),
        ..ok()
    };
    record(&h, "coinbase-1", vec![usdc], "").unwrap();
    let mut unconverted = close("coinbase-1", "k2", BTC, "2026-10-07", "1");
    unconverted.price = None;
    unconverted.meta.as_mut().unwrap().unconverted = vec![AsReported {
        scheme: "coinbase".into(),
        code: String::new(),
        text: "n/a".into(),
    }];
    record(&h, "coinbase-1", vec![unconverted], "").unwrap();
    let listed =
        tokio::task::block_in_place(|| h.lake.list_datasets(&envelope("dashboard-1", false, "")))
            .unwrap();
    let coinbase = listed
        .datasets
        .iter()
        .find(|d| d.dataset == "coinbase-1:daily")
        .unwrap();
    assert_eq!(coinbase.unconverted_count, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_default_read_takes_the_priority_and_names_why_it_fell_through() {
    let h = harness();
    record(
        &h,
        "kraken-1",
        vec![close("kraken-1", "k", BTC, "2026-10-07", "7")],
        "",
    )
    .unwrap();
    let set = |datasets: &[&str], against: i64| {
        tokio::task::block_in_place(|| {
            h.lake.set_priority(
                SetSourcePriorityRequest {
                    data_type: "meridian.v1.Price".into(),
                    kind: PriceKind::Close as i32,
                    datasets: datasets.iter().map(|d| d.to_string()).collect(),
                    note: "Coinbase first.".into(),
                    against_updated_at_ns: against,
                },
                &envelope("dashboard-1", false, "local|ada"),
            )
        })
    };
    let priority = set(&["coinbase-1:daily", "kraken-1:daily"], 0).unwrap();
    assert_eq!(priority.updated_by, "local|ada");
    let stale = set(&["kraken-1:daily"], 0).unwrap_err();
    assert!(stale.contains("against_updated_at_ns"), "{stale}");
    assert!(set(&["nowhere:daily"], priority.updated_at_ns)
        .unwrap_err()
        .contains("datasets[0]"));
    let answer = read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    let Observation::Price(price) = &answer.rows[0] else {
        panic!()
    };
    assert_eq!(
        price
            .meta
            .as_ref()
            .unwrap()
            .source
            .as_ref()
            .unwrap()
            .dataset,
        "kraken-1:daily"
    );
    assert_eq!(answer.unanswered.len(), 1);
    assert_eq!(answer.unanswered[0].dataset, "coinbase-1:daily");
    assert_eq!(
        answer.unanswered[0].reason,
        UnansweredReason::AskedSource as i32
    );
    // Side by side: each dataset its own row or its reason.
    let both = read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-07",
        SourceChoice {
            side_by_side: true,
            ..Default::default()
        },
        0,
    );
    assert_eq!(both.rows.len(), 1);
    assert!(both
        .unanswered
        .iter()
        .any(|u| u.dataset == "coinbase-1:daily"));
    assert_eq!(
        h.store.priority_changes().unwrap().len(),
        1,
        "the stale one changed nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merged_record_is_followed_by_alias_and_retention_removes_rows_recording_it() {
    let h = harness();
    record(
        &h,
        "coinbase-1",
        vec![close("coinbase-1", "k", "LCL-OLD", "2026-10-07", "3")],
        "",
    )
    .unwrap();
    h.lake.hear(Envelope {
        meta: None,
        payload_type: "meridian.v1.InstrumentReplacedEvent".into(),
        payload: InstrumentReplacedEvent {
            replaced_instrument_id: "LCL-OLD".into(),
            instrument: Some(InstrumentRecord {
                instrument_id: BTC.into(),
                ..Default::default()
            }),
            replaced_at_ns: NOW,
        }
        .encode_to_vec(),
    });
    let answer = read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    assert_eq!(answer.rows.len(), 1);
    assert_eq!(answer.rows[0].subject(), "LCL-OLD", "each row as recorded");

    let mut c = configuration(true, &[]);
    c.licences.push(DatasetLicence {
        dataset: "coinbase-1:daily".into(),
        kept: true,
        retention_days: 1,
        ..Default::default()
    });
    h.lake.configured(&c);
    h.clock.advance(2 * DAY);
    h.lake.apply_retention();
    let removals = h.store.removals().unwrap();
    assert_eq!(removals.len(), 1);
    assert_eq!(removals[0].rows, 1);
    assert!(removals[0].why.contains("1 days"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_with_an_invalid_date_or_two_times_is_refused_and_misses_are_counted() {
    let h = harness();
    let request = |date: &str, at_ns: i64| ReadRequest {
        data_type: DataType::Price,
        subjects: vec![SubjectRef {
            entity_id: BTC.into(),
        }],
        kinds: vec![],
        interval_ns: 0,
        sources: None,
        at_ns,
        business_date: date.into(),
        valid_from_ns: 0,
        valid_until_ns: 0,
        as_of_ns: 0,
        page_size: 0,
        cursor: String::new(),
    };
    let invalid = tokio::task::block_in_place(|| {
        h.lake
            .read(request("2026-13-01", 0), &envelope("reporting-1", true, ""))
    })
    .err()
    .unwrap();
    assert!(invalid.starts_with("business_date:"), "{invalid}");
    let two = tokio::task::block_in_place(|| {
        h.lake.read(
            request("2026-10-07", NOW),
            &envelope("reporting-1", true, ""),
        )
    })
    .err()
    .unwrap();
    assert!(two.contains("at most one"), "{two}");
    h.lake.hear(Envelope {
        meta: None,
        payload_type: "meridian.v1.MissingVenueDetectedEvent".into(),
        payload: meridian_domain::v1::MissingVenueDetectedEvent {
            publisher_instance_id: "coinbase-1".into(),
            ..Default::default()
        }
        .encode_to_vec(),
    });
    let listed =
        tokio::task::block_in_place(|| h.lake.list_datasets(&envelope("dashboard-1", false, "")))
            .unwrap();
    assert_eq!(
        listed
            .datasets
            .iter()
            .find(|d| d.instance == "coinbase-1")
            .unwrap()
            .miss_count,
        1
    );
    // To a plugin: its own datasets and entitlements only.
    let mine =
        tokio::task::block_in_place(|| h.lake.list_datasets(&envelope("reporting-2", true, "")))
            .unwrap();
    assert!(mine.datasets.is_empty() && mine.entitlements.is_empty());
    let _ = h.clock.now_ns();
}
