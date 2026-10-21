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
        let found =
            [BTC, USD, USDC, AAPL, "LCL-OLD", LOCAL].contains(&asked.instrument_id.as_str());
        Ok((
            "meridian.v1.ResolveInstrumentReply".into(),
            ResolveInstrumentReply {
                found,
                instrument: found.then(|| InstrumentRecord {
                    instrument_id: asked.instrument_id.clone(),
                    // A provisional record (contract v24): a local line.
                    provisional: asked.instrument_id == LOCAL,
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
                ..Default::default()
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
                after: None,
                tenors: Vec::new(),
                published_as_of_ns: 0,
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
        .subscribe_every("platform.lake.coinbase-1:daily.event.prices-recorded");
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
                after: None,
                tenors: Vec::new(),
                published_as_of_ns: 0,
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
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
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

/// A provisional record (contract v24).
const LOCAL: &str = "LCL-01J8LOCAL0000000000000000";

/// Contract v27 (ruling 484; W10.6, W10.7): a read naming a provisional
/// (`LCL-`) subject the lake does not keep is wanted of its data plugin, as
/// any other subject is -- never answered ASKED_SOURCE without the want
/// sent -- and the rows recorded against it are followed to the `INS-`
/// record it becomes.
#[tokio::test(flavor = "multi_thread")]
async fn a_provisional_subject_is_wanted_and_followed_to_the_record_it_becomes() {
    let h = harness();
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let answer = read(
        &h,
        "reporting-1",
        &[LOCAL],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    assert!(answer
        .unanswered
        .iter()
        .any(|u| u.reason == UnansweredReason::AskedSource as i32));
    let want = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    let subjects: Vec<&str> = want.subjects.iter().map(|s| s.entity_id.as_str()).collect();
    assert_eq!(subjects, vec![LOCAL]);
    // The data plugin answers it by the record's identifiers, then the
    // record is resolved to the platform's: the row is followed.
    record(
        &h,
        "coinbase-1",
        vec![close("coinbase-1", "k", LOCAL, "2026-10-07", "3")],
        &want.want_id,
    )
    .unwrap();
    h.lake.hear(Envelope {
        meta: None,
        payload_type: "meridian.v1.InstrumentReplacedEvent".into(),
        payload: InstrumentReplacedEvent {
            replaced_instrument_id: LOCAL.into(),
            instrument: Some(InstrumentRecord {
                instrument_id: AAPL.into(),
                ..Default::default()
            }),
            replaced_at_ns: NOW,
            ..Default::default()
        }
        .encode_to_vec(),
    });
    let followed = read(
        &h,
        "reporting-1",
        &[AAPL],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    assert_eq!(followed.rows.len(), 1);
    assert_eq!(followed.rows[0].subject(), LOCAL, "the row as recorded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_want_no_reader_asks_within_its_cadence_is_withdrawn() {
    let h = harness();
    let mut c = configuration(true, &[]);
    c.datasets[0].declaration.as_mut().unwrap().cadence = 60;
    h.lake.configured(&c);
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let mut withdrawn = h.bus.subscribe_every(service::WANT_WITHDRAWN);
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
        .subscribe_every("platform.lake.coinbase-1:daily.event.prices-recorded");
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
            ..Default::default()
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
        after: None,
        tenors: Vec::new(),
        published_as_of_ns: 0,
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

// ── Contract v19: the lake's 1b ────────────────────────────────────────────

const VENUE: &str = "VEN-01JA0000000000000CBEXC";

/// The 1a's configuration, with coinbase-1:live beside it: trades and quotes,
/// streamed, its day UTC; signal-1 entitled to it, every field.
fn live_configuration() -> EntitlementsChangedEvent {
    let mut c = configuration(true, &[]);
    c.datasets.push(DatasetRef {
        dataset: "coinbase-1:live".into(),
        instance: "coinbase-1".into(),
        vendor: "Coinbase".into(),
        declaration: Some(DatasetDeclaration {
            key: "live".into(),
            vendor: "Coinbase".into(),
            data_types: vec!["meridian.v1.Trade".into(), "meridian.v1.Quote".into()],
            modes: vec![ObservationMode::Stream as i32],
            day_time_zone: "Etc/UTC".into(),
            ..Default::default()
        }),
        ..Default::default()
    });
    c.entitlements.push(DatasetEntitlement {
        dataset: "coinbase-1:live".into(),
        instance: "signal-1".into(),
        allowed: true,
        ..Default::default()
    });
    c
}

fn live_meta(row_key: &str, valid_from_ns: i64) -> Option<ObservationMeta> {
    Some(ObservationMeta {
        row_key: row_key.into(),
        subjects: vec![SubjectRef {
            entity_id: BTC.into(),
        }],
        source: Some(Source {
            instance: "coinbase-1".into(),
            dataset: "coinbase-1:live".into(),
            venue_id: VENUE.into(),
            ..Default::default()
        }),
        valid_from_ns,
        ..Default::default()
    })
}

fn trade(row_key: &str, valid_from_ns: i64, price: &str) -> meridian_domain::v1::Trade {
    meridian_domain::v1::Trade {
        meta: live_meta(row_key, valid_from_ns),
        price: money(price, "USD", ""),
        quantity: decimal("0.5"),
        attributes: Some(meridian_domain::v1::TradeAttributes::default()),
        aggressor: meridian_domain::v1::Aggressor::Buy as i32,
        source_sequence: 7,
        cancelled: false,
    }
}

fn quote(
    row_key: &str,
    valid_from_ns: i64,
    bid: Option<Money>,
    ask: Option<Money>,
) -> meridian_domain::v1::Quote {
    meridian_domain::v1::Quote {
        meta: live_meta(row_key, valid_from_ns),
        bid,
        ask,
        bid_quantity: decimal("1"),
        ask_quantity: decimal("2"),
        characteristics: vec![],
    }
}

fn record_rows(
    h: &Harness,
    field: &str,
    rows: Vec<Observation>,
) -> Result<meridian_domain::v1::RecordObservationsReply, String> {
    tokio::task::block_in_place(|| {
        h.lake
            .record(field, rows, "", &envelope("coinbase-1", true, ""))
    })
}

fn live_read(
    h: &Harness,
    data_type: DataType,
    range: (i64, i64),
    at_ns: i64,
    after: Option<meridian_domain::v1::Watermark>,
) -> Result<service::ReadAnswer, String> {
    tokio::task::block_in_place(|| {
        h.lake.read(
            ReadRequest {
                data_type,
                subjects: vec![SubjectRef {
                    entity_id: BTC.into(),
                }],
                kinds: vec![],
                interval_ns: 0,
                sources: None,
                at_ns,
                business_date: String::new(),
                valid_from_ns: range.0,
                valid_until_ns: range.1,
                as_of_ns: 0,
                page_size: 0,
                cursor: String::new(),
                after,
                tenors: Vec::new(),
                published_as_of_ns: 0,
            },
            &envelope("signal-1", true, ""),
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn trades_are_heard_every_one_and_a_loss_is_caught_up_by_watermark_late_print_included() {
    let h = harness();
    h.lake.configured(&live_configuration());
    let mut heard = h
        .bus
        .subscribe_every("platform.lake.coinbase-1:live.event.trades-recorded");
    let first = record_rows(
        &h,
        "trades",
        vec![
            Observation::Trade(trade("t1", NOW + 1_000, "62000")),
            Observation::Trade(trade("t2", NOW + 2_000, "62001")),
        ],
    )
    .unwrap();
    assert_eq!(first.recorded, 2);
    let seen = first.watermark.unwrap();
    for wanted in ["t1", "t2"] {
        let event = meridian_domain::v1::TradesRecordedEvent::decode(
            &heard.recv().await.unwrap().envelope.payload[..],
        )
        .unwrap();
        let trade = event.trade.unwrap();
        assert_eq!(
            trade.meta.as_ref().unwrap().row_key,
            wanted,
            "every one, in order"
        );
        assert_eq!(trade.price.as_ref().unwrap().instrument_id, USD);
    }

    // A late print, its valid time before the reader's range had passed,
    // recorded after the watermark the reader last saw; and a withdrawal.
    h.clock.advance(5_000);
    let mut withdrawn = trade("t1", NOW + 1_000, "62000");
    withdrawn.cancelled = true;
    record_rows(
        &h,
        "trades",
        vec![
            Observation::Trade(trade("late", NOW - DAY / 2, "61990")),
            Observation::Trade(withdrawn),
        ],
    )
    .unwrap();
    let caught = live_read(&h, DataType::Trade, (0, 0), 0, Some(seen.clone())).unwrap();
    let keys: Vec<(String, u64, bool)> = caught
        .rows
        .iter()
        .map(|row| match row {
            Observation::Trade(t) => (
                t.meta.as_ref().unwrap().row_key.clone(),
                t.meta.as_ref().unwrap().version,
                t.cancelled,
            ),
            _ => panic!("a trade"),
        })
        .collect();
    assert!(keys.contains(&("late".to_string(), 1, false)), "{keys:?}");
    assert!(
        keys.contains(&("t1".to_string(), 2, true)),
        "the withdrawal, a new version"
    );
    assert!(
        !keys.iter().any(|(k, _, _)| k == "t2"),
        "nothing seen before"
    );
    assert!(caught.unanswered.is_empty(), "{:?}", caught.unanswered);

    // A range read of the day answers by valid time; past a day, refused.
    let day = live_read(&h, DataType::Trade, (NOW, NOW + DAY), 0, None).unwrap();
    assert_eq!(day.rows.len(), 2, "t1 at its latest version, and t2");
    let wide = live_read(&h, DataType::Trade, (NOW - DAY / 2, NOW + DAY / 2), 0, None);
    assert!(wide.unwrap_err().contains("more than one day"));
    let both = live_read(&h, DataType::Trade, (NOW, NOW + DAY), 0, Some(seen));
    assert!(both.unwrap_err().contains("exactly one"));

    let n = h
        .lake
        .log_trades_recorded(std::time::Duration::from_secs(60));
    assert!(n > 0.0, "the band reads the trades recorded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_data_plugin_reads_its_own_last_trade_and_its_datasets_licences() {
    // Contract v20 (W10.6, W10.9): its own rows, by no entitlement.
    let h = harness();
    h.lake.configured(&live_configuration());
    record_rows(
        &h,
        "trades",
        vec![
            Observation::Trade(trade("t1", NOW + 1_000, "62000")),
            Observation::Trade(trade("t2", NOW + 2_000, "62001")),
        ],
    )
    .unwrap();
    let asked = meridian_domain::v1::ListTradesRequest {
        subjects: vec![SubjectRef {
            entity_id: BTC.into(),
        }],
        latest_recorded: true,
        ..Default::default()
    };
    let own = tokio::task::block_in_place(|| {
        h.lake
            .latest_recorded(&asked, &envelope("coinbase-1", true, ""))
    })
    .unwrap();
    assert_eq!(
        own.trades.len(),
        1,
        "the last one per subject, dataset and venue"
    );
    assert_eq!(own.trades[0].meta.as_ref().unwrap().row_key, "t2");
    assert!(own.watermark.unwrap().partitions[0].sequence > 0);
    let refused = tokio::task::block_in_place(|| {
        h.lake
            .latest_recorded(&asked, &envelope("signal-1", true, ""))
    })
    .unwrap_err();
    assert!(
        refused.contains("declares no dataset of trades"),
        "{refused}"
    );
    let ranged = tokio::task::block_in_place(|| {
        h.lake.latest_recorded(
            &meridian_domain::v1::ListTradesRequest {
                valid_from_ns: NOW,
                ..asked.clone()
            },
            &envelope("coinbase-1", true, ""),
        )
    })
    .unwrap_err();
    assert!(ranged.contains("left empty"), "{ranged}");

    let listed = h
        .lake
        .list_datasets(&envelope("coinbase-1", true, ""))
        .unwrap();
    assert!(listed
        .datasets
        .iter()
        .any(|d| d.dataset == "coinbase-1:live"));
    assert!(listed
        .licences
        .iter()
        .any(|l| l.dataset == "coinbase-1:live"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trade_or_quote_is_refused_naming_its_field() {
    let h = harness();
    h.lake.configured(&live_configuration());
    let refused = |row: Observation, field: &str| record_rows(&h, field, vec![row]).unwrap_err();
    let mut nothing = trade("t", NOW, "1");
    nothing.quantity = decimal("0");
    assert!(refused(Observation::Trade(nothing), "trades").contains("trades[0].quantity"));
    let mut dated = trade("t", NOW, "1");
    dated.meta.as_mut().unwrap().business_date = "2026-10-08".into();
    assert!(refused(Observation::Trade(dated), "trades").contains("business_date"));
    let mut unconverted = trade("t", NOW, "1");
    unconverted.attributes = Some(meridian_domain::v1::TradeAttributes {
        characteristics: vec![0],
        ..Default::default()
    });
    assert!(refused(Observation::Trade(unconverted), "trades")
        .contains("trades[0].attributes.characteristics[0]"));
    let mut priceless = trade("t", NOW, "1");
    priceless.price = None;
    assert!(refused(Observation::Trade(priceless), "trades").contains("trades[0].price"));
    let two = quote("q", NOW, money("1", "USD", ""), money("1.01", "", USDC));
    assert!(refused(Observation::Quote(two), "quotes").contains("one asset"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_latest_is_chosen_per_asset_for_prices_and_quotes() {
    let h = harness();
    h.lake.configured(&live_configuration());
    // BTC's last in USD and in USDC on one venue: both answer (approved
    // 2026-10-10), never one for both.
    let on_venue = |key: &str, at: i64, money: Option<Money>| Price {
        meta: Some(ObservationMeta {
            row_key: key.into(),
            subjects: vec![SubjectRef {
                entity_id: BTC.into(),
            }],
            source: Some(Source {
                instance: "coinbase-1".into(),
                dataset: "coinbase-1:daily".into(),
                venue_id: VENUE.into(),
                ..Default::default()
            }),
            valid_from_ns: at,
            ..Default::default()
        }),
        kind: PriceKind::Last as i32,
        price: money,
        basis: PriceBasis::PerUnit as i32,
    };
    record(
        &h,
        "coinbase-1",
        vec![
            on_venue("usd-1", NOW - 10, money("62000", "USD", "")),
            on_venue("usd-2", NOW - 5, money("62010", "USD", "")),
            on_venue("usdc-1", NOW - 7, money("61990", "", USDC)),
        ],
        "",
    )
    .unwrap();
    let latest = read(&h, "reporting-1", &[BTC], "", SourceChoice::default(), 0);
    let mut keys: Vec<String> = latest
        .rows
        .iter()
        .map(|row| row.meta().row_key.clone())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["usd-2", "usdc-1"]);

    // Quotes likewise, per subject, dataset, venue and the quote's asset.
    record_rows(
        &h,
        "quotes",
        vec![
            Observation::Quote(quote(
                "q-usd-1",
                NOW - 3,
                money("1", "USD", ""),
                money("2", "USD", ""),
            )),
            Observation::Quote(quote("q-usd-2", NOW - 2, money("1.5", "USD", ""), None)),
            Observation::Quote(quote(
                "q-usdc",
                NOW - 4,
                money("1", "", USDC),
                money("2", "", USDC),
            )),
        ],
    )
    .unwrap();
    let quotes = live_read(&h, DataType::Quote, (0, 0), 0, None).unwrap();
    let mut keys: Vec<String> = quotes
        .rows
        .iter()
        .map(|row| row.meta().row_key.clone())
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["q-usd-2", "q-usdc"], "a one-sided quote stands");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_decline_stands_for_the_date_asked_only_and_a_streamed_latest_is_standing() {
    let h = harness();
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    read(
        &h,
        "reporting-1",
        &[AAPL],
        "2026-10-07",
        SourceChoice::default(),
        0,
    );
    let want = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    tokio::task::block_in_place(|| {
        h.lake.decline(
            DeclineWantRequest {
                want_id: want.want_id,
                subjects: vec![SubjectRef {
                    entity_id: AAPL.into(),
                }],
                reason: UnansweredReason::NotCovered as i32,
            },
            &envelope("coinbase-1", true, ""),
        )
    })
    .unwrap();
    let named = SourceChoice {
        named: vec!["coinbase-1:daily".into()],
        ..Default::default()
    };
    let declined = read(&h, "reporting-1", &[AAPL], "2026-10-07", named.clone(), 0);
    assert_eq!(
        declined.unanswered[0].reason,
        UnansweredReason::NotCovered as i32
    );
    // Another date of the same dataset and subject is asked again.
    let other = read(&h, "reporting-1", &[AAPL], "2026-10-06", named, 0);
    assert_eq!(
        other.unanswered[0].reason,
        UnansweredReason::AskedSource as i32
    );
    let again = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    assert_eq!(again.business_date, "2026-10-06");

    // The latest of a streamed dataset, with no cadence: a standing want,
    // kept current from the vendor's stream (W10.7).
    h.lake.configured(&live_configuration());
    let none = live_read(&h, DataType::Quote, (0, 0), 0, None).unwrap();
    assert_eq!(
        none.unanswered[0].reason,
        UnansweredReason::AskedSource as i32
    );
    let streamed =
        ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
            .unwrap();
    assert_eq!(streamed.data_type, "meridian.v1.Quote");
    assert!(streamed.standing);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_want_is_delivered_again_under_its_id_so_a_restarted_data_plugin_resumes() {
    // A data plugin holds nothing it cannot lose: one restarted hears its standing
    // wants again, the same want and ID, within a minute; one that holds the
    // want already hears what it holds.
    let h = harness();
    h.lake.configured(&live_configuration());
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    live_read(&h, DataType::Quote, (0, 0), 0, None).unwrap();
    let first = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    assert!(first.standing);
    h.clock.advance(30 * 1_000_000_000);
    h.lake.sweep_wants();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), wanted.recv())
            .await
            .is_err(),
        "not again within the minute"
    );
    h.clock.advance(31 * 1_000_000_000);
    live_read(&h, DataType::Quote, (0, 0), 0, None).unwrap();
    h.lake.sweep_wants();
    let again = ObservationsWantedEvent::decode(&wanted.recv().await.unwrap().envelope.payload[..])
        .unwrap();
    assert_eq!(again, first, "the same want, under the same ID");
    let asked = h
        .store
        .want_changes()
        .unwrap()
        .iter()
        .filter(|c| c.want_id == first.want_id && c.kind == crate::store::WantChangeKind::Asked)
        .count();
    assert_eq!(asked, 1, "delivered again, not asked again");
}

// ── Contract v23: the lake's 1c, an event contract's outcome ───────────────

/// The 1a's configuration, with kalshi-1:outcomes beside it: outcomes,
/// polled; each of outcomes' readers entitled to it but signal-1.
fn outcomes_configuration() -> EntitlementsChangedEvent {
    let mut c = configuration(true, &[]);
    c.datasets.push(DatasetRef {
        dataset: "kalshi-1:outcomes".into(),
        instance: "kalshi-1".into(),
        vendor: "Kalshi".into(),
        declaration: Some(DatasetDeclaration {
            key: "outcomes".into(),
            vendor: "Kalshi".into(),
            data_types: vec!["meridian.v1.Outcome".into()],
            modes: vec![ObservationMode::Pull as i32],
            day_time_zone: "Etc/UTC".into(),
            ..Default::default()
        }),
        ..Default::default()
    });
    for reader in ["reporting-1", "portfolio-1", "compliance-1", "operations-1"] {
        c.entitlements.push(DatasetEntitlement {
            dataset: "kalshi-1:outcomes".into(),
            instance: reader.into(),
            allowed: true,
            ..Default::default()
        });
    }
    c
}

fn outcome(
    valid_from_ns: i64,
    stage: meridian_domain::v1::OutcomeStage,
    determination: meridian_domain::v1::OutcomeDetermination,
    value: &str,
) -> meridian_domain::v1::Outcome {
    meridian_domain::v1::Outcome {
        meta: Some(ObservationMeta {
            row_key: "KXFED-26OCT-T4.25".into(),
            subjects: vec![SubjectRef {
                entity_id: AAPL.into(),
            }],
            source: Some(Source {
                instance: "kalshi-1".into(),
                dataset: "kalshi-1:outcomes".into(),
                ..Default::default()
            }),
            valid_from_ns,
            ..Default::default()
        }),
        determination: determination as i32,
        stage: stage as i32,
        settlement_value: money(value, "USD", ""),
        settled_at_ns: 0,
        determination_as_reported: Some(AsReported {
            scheme: "kalshi:result".into(),
            code: "yes".into(),
            text: String::new(),
        }),
        challenge_ends_at_ns: 0,
    }
}

fn record_outcomes(
    h: &Harness,
    rows: Vec<meridian_domain::v1::Outcome>,
) -> Result<meridian_domain::v1::RecordObservationsReply, String> {
    tokio::task::block_in_place(|| {
        h.lake.record(
            "outcomes",
            rows.into_iter().map(Observation::Outcome).collect(),
            "",
            &envelope("kalshi-1", true, ""),
        )
    })
}

fn read_outcomes(
    h: &Harness,
    reader: &str,
    as_of_ns: i64,
    after: Option<meridian_domain::v1::Watermark>,
) -> Result<meridian_domain::v1::ListOutcomesReply, String> {
    tokio::task::block_in_place(|| {
        h.lake.list_outcomes(
            meridian_domain::v1::ListOutcomesRequest {
                subjects: vec![SubjectRef {
                    entity_id: AAPL.into(),
                }],
                after_watermark: after,
                as_of_ns,
            },
            &envelope(reader, true, ""),
        )
    })
}

fn stages(reply: &meridian_domain::v1::ListOutcomesReply) -> Vec<(i32, u64)> {
    reply
        .outcomes
        .iter()
        .map(|o| (o.stage, o.meta.as_ref().unwrap().version))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_outcome_is_restated_per_stage_every_version_heard_and_read_as_of_or_after() {
    use meridian_domain::v1::{OutcomeDetermination as D, OutcomeStage as S};
    let h = harness();
    h.lake.configured(&outcomes_configuration());
    let mut heard = h
        .bus
        .subscribe_every("platform.lake.kalshi-1:outcomes.event.outcomes-recorded");
    // Determined; then disputed and restated as version 2; then final.
    let first = record_outcomes(&h, vec![outcome(NOW, S::Determined, D::Pays, "1.00")]).unwrap();
    assert_eq!(first.recorded, 1);
    let seen = first.watermark.unwrap();
    let before_dispute = h.clock.now_ns();
    h.clock.advance(1_000);
    let disputed =
        record_outcomes(&h, vec![outcome(NOW + 10, S::Disputed, D::Pays, "1.00")]).unwrap();
    assert_eq!(
        disputed.restated, 1,
        "one row key per contract: a new version"
    );
    h.clock.advance(1_000);
    record_outcomes(&h, vec![outcome(NOW + 20, S::Final, D::PaysPart, "0.50")]).unwrap();
    // The same again changes nothing.
    let again =
        record_outcomes(&h, vec![outcome(NOW + 20, S::Final, D::PaysPart, "0.50")]).unwrap();
    assert_eq!(again.unchanged, 1);
    // Every version delivered, in order, never conflated.
    for (stage, version) in [(S::Determined, 1), (S::Disputed, 2), (S::Final, 3)] {
        let event = meridian_domain::v1::OutcomesRecordedEvent::decode(
            &heard.recv().await.unwrap().envelope.payload[..],
        )
        .unwrap()
        .outcome
        .unwrap();
        assert_eq!(event.stage, stage as i32);
        assert_eq!(event.meta.as_ref().unwrap().version, version);
        assert_eq!(event.settlement_value.as_ref().unwrap().instrument_id, USD);
    }
    // The version in force; as of a recorded time before the restatement;
    // and the versions recorded after the watermark a reader last saw.
    for reader in ["reporting-1", "portfolio-1", "compliance-1", "operations-1"] {
        let now = read_outcomes(&h, reader, 0, None).unwrap();
        assert_eq!(stages(&now), vec![(S::Final as i32, 3)], "{reader}");
        assert!(now
            .watermark
            .unwrap()
            .partitions
            .iter()
            .any(|p| p.partition == "kalshi-1:outcomes"));
    }
    let then = read_outcomes(&h, "reporting-1", before_dispute, None).unwrap();
    assert_eq!(stages(&then), vec![(S::Determined as i32, 1)]);
    let caught = read_outcomes(&h, "reporting-1", 0, Some(seen)).unwrap();
    assert_eq!(
        stages(&caught),
        vec![(S::Disputed as i32, 2), (S::Final as i32, 3)]
    );
    // A reader not entitled reads none.
    assert!(read_outcomes(&h, "signal-1", 0, None)
        .unwrap()
        .outcomes
        .is_empty());
    // ListOutcomes is the only read of them.
    assert!(tokio::task::block_in_place(|| h.lake.read(
        ReadRequest {
            data_type: DataType::Outcome,
            subjects: vec![SubjectRef {
                entity_id: AAPL.into()
            }],
            kinds: vec![],
            interval_ns: 0,
            sources: None,
            at_ns: 0,
            business_date: String::new(),
            valid_from_ns: 0,
            valid_until_ns: 0,
            as_of_ns: 0,
            page_size: 0,
            cursor: String::new(),
            after: None,
            tenors: Vec::new(),
            published_as_of_ns: 0,
        },
        &envelope("reporting-1", true, "")
    ))
    .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_outcome_is_refused_naming_its_field() {
    use meridian_domain::v1::{OutcomeDetermination as D, OutcomeStage as S};
    let h = harness();
    h.lake.configured(&outcomes_configuration());
    let good = || outcome(NOW, S::Proposed, D::Pays, "1.00");
    let mut dated = good();
    dated.meta.as_mut().unwrap().business_date = "2026-10-08".into();
    let mut undetermined = good();
    undetermined.determination = 0;
    let mut unstaged = good();
    unstaged.stage = 0;
    let mut valueless = good();
    valueless.settlement_value = None;
    let mut unsaid = good();
    unsaid.determination_as_reported = None;
    for (row, field) in [
        (dated, "outcomes[0].meta.business_date"),
        (undetermined, "outcomes[0].determination"),
        (unstaged, "outcomes[0].stage"),
        (valueless, "outcomes[0].settlement_value"),
        (unsaid, "outcomes[0].determination_as_reported"),
    ] {
        let refused = record_outcomes(&h, vec![row]).unwrap_err();
        assert!(refused.starts_with(field), "{field}: {refused}");
    }
    assert!(record_outcomes(&h, vec![])
        .unwrap_err()
        .contains("1 to 500"));
}

// ── Contract v23: the closed-day flag (W10.4, W10.9) ────────────────────────

/// The 1a's configuration, coinbase-1:daily following CAL-USSS.
fn calendar_configuration() -> EntitlementsChangedEvent {
    let mut c = configuration(true, &[]);
    for dataset in c.datasets.iter_mut() {
        if dataset.dataset == "coinbase-1:daily" {
            dataset.declaration.as_mut().unwrap().calendar_id = "CAL-USSS".into();
        }
    }
    c
}

/// The instrument store's ReadCalendar, stood in: CAL-USSS at the version
/// `held` says (0: not held), Columbus Day closed, its coverage to
/// 2027-02-01, and from version 2 an ad hoc closure on 2026-10-14.
fn calendar_held(h: &Harness) -> Arc<std::sync::atomic::AtomicI64> {
    use meridian_domain::v1::{
        CalendarCoverage, CalendarDay, CalendarDayKind, CalendarWeekend, ReadCalendarReply,
        ReadCalendarRequest, Weekday,
    };
    let held = Arc::new(std::sync::atomic::AtomicI64::new(1));
    let version = held.clone();
    h.bus.serve(service::READ_CALENDAR, move |envelope| {
        let asked = ReadCalendarRequest::decode(&envelope.payload[..]).unwrap();
        let at = version.load(std::sync::atomic::Ordering::SeqCst);
        let day = |date: &str, name: &str| CalendarDay {
            date: date.into(),
            kind: CalendarDayKind::Closed as i32,
            name: name.into(),
            source: String::new(),
        };
        let mut days = vec![day("2026-10-12", "Columbus Day")];
        if at >= 2 {
            days.push(day("2026-10-14", "A national day of mourning"));
        }
        let reply = ReadCalendarReply {
            found: at > 0 && asked.calendar_id == "CAL-USSS",
            calendar_id: asked.calendar_id.clone(),
            version: at,
            days,
            weekends: vec![CalendarWeekend {
                weekdays: vec![Weekday::Saturday as i32, Weekday::Sunday as i32],
                valid_from_date: "2000-01-01".into(),
            }],
            coverage: Some(CalendarCoverage {
                from_date: "2026-01-01".into(),
                to_date: "2027-02-01".into(),
            }),
            sources: vec![],
            trading_sessions: None,
        };
        Ok((
            "meridian.v1.ReadCalendarReply".into(),
            reply.encode_to_vec(),
        ))
    });
    held
}

fn flags(rows: &[Observation]) -> Vec<(String, i32, String)> {
    let mut out: Vec<(String, i32, String)> = rows
        .iter()
        .map(|row| {
            let meta = row.meta();
            let check = meta.closed_day.clone().unwrap_or_default();
            (meta.business_date.clone(), check.reason, check.holiday_name)
        })
        .collect();
    out.sort();
    out
}

fn kept_rows(h: &Harness, as_of_ns: i64) -> Vec<Observation> {
    h.store
        .read(&crate::store::Query {
            datasets: vec!["coinbase-1:daily".into()],
            subjects: vec![BTC.into()],
            data_type: DataType::Price,
            kinds: vec![],
            interval_ns: 0,
            when: crate::store::When::Range {
                from_ns: 0,
                until_ns: 0,
            },
            as_of_ns,
            published_as_of_ns: 0,
        })
        .unwrap()
}

fn counts(h: &Harness) -> (u64, u64) {
    let listed = tokio::task::block_in_place(|| {
        h.lake
            .list_datasets(&envelope("dashboard-1", false, ""))
            .unwrap()
    });
    let held = listed
        .datasets
        .iter()
        .find(|d| d.dataset == "coinbase-1:daily")
        .unwrap();
    (held.closed_day_count, held.outside_coverage_count)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daily_value_on_a_closed_day_is_recorded_flagged_and_counted() {
    use meridian_domain::v1::DateCheckReason as R;
    let h = harness();
    h.lake.configured(&calendar_configuration());
    calendar_held(&h);
    // A plugin's own flag is never its word: the lake decides it.
    let mut claimed = close("coinbase-1", "tue", BTC, "2026-10-13", "62000");
    claimed.meta.as_mut().unwrap().closed_day = Some(meridian_domain::v1::DateCheck {
        reason: R::Holiday as i32,
        ..Default::default()
    });
    let done = record(
        &h,
        "coinbase-1",
        vec![
            claimed,
            close("coinbase-1", "sat", BTC, "2026-10-10", "62001"),
            close("coinbase-1", "columbus", BTC, "2026-10-12", "62002"),
            close("coinbase-1", "late", BTC, "2027-03-01", "62003"),
        ],
        "",
    )
    .unwrap();
    assert_eq!(done.recorded, 4, "recorded as reported, never refused");
    assert_eq!(
        flags(&kept_rows(&h, 0)),
        vec![
            ("2026-10-10".into(), R::Weekend as i32, String::new()),
            (
                "2026-10-12".into(),
                R::Holiday as i32,
                "Columbus Day".into()
            ),
            ("2026-10-13".into(), 0, String::new()),
            (
                "2027-03-01".into(),
                R::OutsideCoverage as i32,
                String::new()
            ),
        ]
    );
    let columbus = kept_rows(&h, 0)
        .into_iter()
        .find(|row| row.meta().business_date == "2026-10-12")
        .unwrap();
    let check = columbus.meta().closed_day.clone().unwrap();
    assert_eq!(check.date, "2026-10-12");
    assert_eq!(
        (
            check.calendars[0].calendar_id.as_str(),
            check.calendars[0].version
        ),
        ("CAL-USSS", 1)
    );
    assert_eq!(
        counts(&h),
        (2, 1),
        "closed days, and outside coverage apart"
    );
    // A reader reads the flag; reads are otherwise unchanged.
    let answer = read(
        &h,
        "reporting-1",
        &[BTC],
        "2026-10-12",
        SourceChoice::default(),
        0,
    );
    assert_eq!(answer.rows.len(), 1);
    assert_eq!(
        answer.rows[0].meta().closed_day.as_ref().unwrap().reason,
        R::Holiday as i32
    );
    // A dataset naming no calendar flags nothing.
    record(
        &h,
        "kraken-1",
        vec![close("kraken-1", "sat", BTC, "2026-10-10", "1")],
        "",
    )
    .unwrap();
    let kraken = h
        .store
        .read(&crate::store::Query {
            datasets: vec!["kraken-1:daily".into()],
            subjects: vec![BTC.into()],
            data_type: DataType::Price,
            kinds: vec![],
            interval_ns: 0,
            when: crate::store::When::Range {
                from_ns: 0,
                until_ns: 0,
            },
            as_of_ns: 0,
            published_as_of_ns: 0,
        })
        .unwrap();
    assert!(kraken[0].meta().closed_day.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_calendar_applied_re_checks_the_rows_it_changes_as_amendments_never_versions() {
    use meridian_domain::v1::DateCheckReason as R;
    let h = harness();
    h.lake.configured(&calendar_configuration());
    let held = calendar_held(&h);
    // The calendar not held yet: recorded unflagged.
    held.store(0, std::sync::atomic::Ordering::SeqCst);
    record(
        &h,
        "coinbase-1",
        vec![
            close("coinbase-1", "columbus", BTC, "2026-10-12", "62002"),
            close("coinbase-1", "tue", BTC, "2026-10-13", "62000"),
            close("coinbase-1", "wed", BTC, "2026-10-14", "62005"),
        ],
        "",
    )
    .unwrap();
    assert!(kept_rows(&h, 0)
        .iter()
        .all(|row| row.meta().closed_day.is_none()));
    let head = h.store.heads().unwrap()["coinbase-1:daily"];
    // Version 1 lands: Columbus Day is re-checked, the rest unchanged.
    h.clock.advance(1_000);
    let before_first = h.clock.now_ns() - 1;
    held.store(1, std::sync::atomic::Ordering::SeqCst);
    let applied = |version: i64| meridian_domain::v1::CalendarAppliedEvent {
        calendar_id: "CAL-USSS".into(),
        version,
        rule: None,
        applied_at_ns: 0,
    };
    tokio::task::block_in_place(|| h.lake.recheck(&applied(1)));
    assert_eq!(h.store.closed_day_amendments().unwrap().len(), 1);
    // Version 2 announces a closure on the 14th: that row alone.
    h.clock.advance(1_000);
    let before_second = h.clock.now_ns() - 1;
    held.store(2, std::sync::atomic::Ordering::SeqCst);
    tokio::task::block_in_place(|| h.lake.recheck(&applied(2)));
    let amendments = h.store.closed_day_amendments().unwrap();
    assert_eq!(
        amendments.len(),
        2,
        "only the rows whose day the version changes"
    );
    assert!(amendments[1].why.contains("version 2"));
    assert_eq!(
        flags(&kept_rows(&h, 0)),
        vec![
            (
                "2026-10-12".into(),
                R::Holiday as i32,
                "Columbus Day".into()
            ),
            ("2026-10-13".into(), 0, String::new()),
            (
                "2026-10-14".into(),
                R::Holiday as i32,
                "A national day of mourning".into()
            ),
        ]
    );
    // A point-in-time read shows the flag as it was known then.
    assert!(kept_rows(&h, before_first)
        .iter()
        .all(|row| row.meta().closed_day.is_none()));
    assert_eq!(
        flags(&kept_rows(&h, before_second))
            .iter()
            .filter(|(_, reason, _)| *reason != 0)
            .count(),
        1
    );
    // Never a new version: nothing numbered, every row at version 1.
    assert_eq!(h.store.heads().unwrap()["coinbase-1:daily"], head);
    assert!(kept_rows(&h, 0).iter().all(|row| row.meta().version == 1));
    assert_eq!(counts(&h), (2, 0));
    // The same version again amends nothing.
    tokio::task::block_in_place(|| h.lake.recheck(&applied(2)));
    assert_eq!(h.store.closed_day_amendments().unwrap().len(), 2);
}

/// W10.10 (contract v26): QueryLake, as fixture lake/query-lake.yaml has
/// it -- one kind over business dates, only the fields named, cut at its row
/// limit and saying so; a dataset not entitled unanswered with its reason;
/// its bounds refused naming the field; the dashboard's asked for a named
/// instance, a plugin's for itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_query_reads_one_kind_within_bounds_and_says_where_it_was_cut() {
    let h = harness();
    record(
        &h,
        "coinbase-1",
        vec![
            close("coinbase-1", "d5", BTC, "2026-10-05", "1"),
            close("coinbase-1", "d6", BTC, "2026-10-06", "2"),
            close("coinbase-1", "d7", BTC, "2026-10-07", "3"),
        ],
        "",
    )
    .unwrap();
    let asked = |instance: &str, plugin: bool, request: meridian_domain::v1::QueryLakeRequest| {
        tokio::task::block_in_place(|| h.lake.query_lake(request, &envelope(instance, plugin, "")))
    };
    let request = meridian_domain::v1::QueryLakeRequest {
        kind: "prices".into(),
        subjects: vec![SubjectRef {
            entity_id: BTC.into(),
        }],
        from_date: "2026-10-05".into(),
        to_date: "2026-10-07".into(),
        fields: vec![
            "meridian.v1.Price.price".into(),
            "meridian.v1.Price.kind".into(),
        ],
        row_limit: 2,
        ..Default::default()
    };
    let cut = asked("reporting-1", true, request.clone()).unwrap();
    assert_eq!(cut.prices.len(), 2);
    assert!(cut.truncated, "more rows matched than the limit");
    assert_eq!(amount(&cut.prices[0]), "1");
    assert_eq!(cut.prices[0].basis, 0, "a field not named is left out");
    assert!(cut.prices[0].meta.is_some(), "the row's meta always kept");
    assert!(cut.datasets.iter().any(|d| d.dataset == "coinbase-1:daily"));
    let whole = asked(
        "reporting-1",
        true,
        meridian_domain::v1::QueryLakeRequest {
            row_limit: 0,
            ..request.clone()
        },
    )
    .unwrap();
    assert_eq!(whole.prices.len(), 3);
    assert!(!whole.truncated);
    // Not entitled: unanswered for the dataset, with its reason.
    let stranger = asked("reporting-2", true, request.clone()).unwrap();
    assert!(stranger.prices.is_empty());
    assert!(stranger
        .unanswered
        .iter()
        .any(|u| u.reason == UnansweredReason::NotEntitled as i32));
    // Bounds, each refused naming its field.
    let refused = |request: meridian_domain::v1::QueryLakeRequest| {
        asked("reporting-1", true, request).unwrap_err()
    };
    assert!(refused(meridian_domain::v1::QueryLakeRequest {
        subjects: vec![SubjectRef::default(); 501],
        ..request.clone()
    })
    .contains("subjects"));
    assert!(refused(meridian_domain::v1::QueryLakeRequest {
        row_limit: 10_001,
        ..request.clone()
    })
    .contains("row_limit"));
    assert!(refused(meridian_domain::v1::QueryLakeRequest {
        kind: "candles".into(),
        ..request.clone()
    })
    .contains("kind"));
    assert!(refused(meridian_domain::v1::QueryLakeRequest {
        fields: vec!["meridian.v1.Trade.price".into()],
        ..request.clone()
    })
    .contains("fields[0]"));
    // The dashboard's tool names the instance; a plugin asks as itself.
    assert!(asked("dashboard-1", false, request.clone())
        .unwrap_err()
        .contains("instance_id"));
    let for_person = asked(
        "dashboard-1",
        false,
        meridian_domain::v1::QueryLakeRequest {
            instance_id: "reporting-1".into(),
            ..request.clone()
        },
    )
    .unwrap();
    assert_eq!(for_person.prices.len(), 2);
    assert!(asked(
        "reporting-2",
        true,
        meridian_domain::v1::QueryLakeRequest {
            instance_id: "reporting-1".into(),
            ..request
        },
    )
    .unwrap_err()
    .contains("instance_id"));
}

/// Contract v26 (W12.2): the analytics component reads for its client --
/// the client's entitlements, never its own -- and the field is read from
/// that component alone.
#[test]
fn a_jobs_read_is_the_clients_and_the_field_is_the_analytics_components_alone() {
    let on_behalf = |publisher: &str, client: &str| Envelope {
        meta: Some(MessageMeta {
            publisher_instance_id: publisher.into(),
            account_scope_applies: true,
            on_behalf_of_instance_id: client.into(),
            ..Default::default()
        }),
        payload_type: String::new(),
        payload: Vec::new(),
    };
    assert_eq!(
        crate::service::reader_of(&on_behalf(crate::service::ANALYTICS, "reporting-1")),
        crate::config::Reader::Plugin("reporting-1".into())
    );
    // From a plugin's sidecar the field is ignored: the reader is itself.
    assert_eq!(
        crate::service::reader_of(&on_behalf("reporting-2", "reporting-1")),
        crate::config::Reader::Plugin("reporting-2".into())
    );
    assert_eq!(
        crate::service::reader_of(&on_behalf(crate::service::ANALYTICS, "")),
        crate::config::Reader::Plugin(crate::service::ANALYTICS.into())
    );
}

/// Contract v27 (W10.11; ruling 481): execution costs are the analytics
/// component's alone to record, keyed by execution and benchmark -- the same
/// value a repeat, another the next version -- and read for one account,
/// within the reader's scope, by execution, order, trade date or benchmark.
#[tokio::test(flavor = "multi_thread")]
async fn execution_costs_are_the_components_rows_read_within_the_readers_account_scope() {
    use meridian_domain::v1::{
        ExecutionCost, ListExecutionCostsRequest, RecordExecutionCostsRequest, TcaBenchmark,
    };
    let h = harness();
    let cost = |execution: &str, account: &str, benchmark: TcaBenchmark, bps: &str| ExecutionCost {
        meta: Some(ObservationMeta {
            subjects: vec![SubjectRef {
                entity_id: BTC.into(),
            }],
            business_date: "2026-10-07".into(),
            valid_from_ns: NOW,
            ..Default::default()
        }),
        execution_id: execution.into(),
        order_id: format!("ORD-{execution}"),
        account_id: account.into(),
        benchmark: benchmark as i32,
        benchmark_price: money("62000.00", "USD", ""),
        execution_price: money("62010.00", "USD", ""),
        notional: money("62010.00", "USD", ""),
        cost_bps: decimal(bps),
        ..Default::default()
    };
    let batch = RecordExecutionCostsRequest {
        costs: vec![
            cost("EXE-1", "ACC-1", TcaBenchmark::Arrival, "1.6129"),
            cost("EXE-1", "ACC-1", TcaBenchmark::MidAtFill, "1.6129"),
            cost("EXE-2", "ACC-2", TcaBenchmark::Arrival, "1.6129"),
        ],
    };
    // Not the component: refused.
    let refused = h
        .lake
        .record_execution_costs(batch.clone(), &envelope("reporting-1", true, ""))
        .unwrap_err();
    assert!(refused.contains("analytics component"), "{refused}");
    let done = h
        .lake
        .record_execution_costs(batch.clone(), &envelope(service::ANALYTICS, false, ""))
        .unwrap();
    assert_eq!(done.recorded, 3);
    let again = h
        .lake
        .record_execution_costs(batch, &envelope(service::ANALYTICS, false, ""))
        .unwrap();
    assert_eq!((again.recorded, again.unchanged), (0, 3), "a repeat");
    let changed = cost("EXE-1", "ACC-1", TcaBenchmark::Arrival, "2.0000");
    let restated = h
        .lake
        .record_execution_costs(
            RecordExecutionCostsRequest {
                costs: vec![changed],
            },
            &envelope(service::ANALYTICS, false, ""),
        )
        .unwrap();
    assert_eq!(restated.restated, 1, "another value is the next version");

    let mut scoped = envelope("reporting-1", true, "");
    scoped.meta.as_mut().unwrap().account_scope = vec!["ACC-1".into()];
    let read = h
        .lake
        .list_execution_costs(
            ListExecutionCostsRequest {
                account_id: "ACC-1".into(),
                ..Default::default()
            },
            &scoped,
        )
        .unwrap();
    assert_eq!(read.costs.len(), 2, "ACC-1's rows alone");
    assert!(read.costs.iter().all(|c| c.account_id == "ACC-1"));
    let arrival = read
        .costs
        .iter()
        .find(|c| c.benchmark == TcaBenchmark::Arrival as i32)
        .unwrap();
    assert_eq!(arrival.meta.as_ref().unwrap().version, 2);
    let one = h
        .lake
        .list_execution_costs(
            ListExecutionCostsRequest {
                account_id: "ACC-1".into(),
                benchmarks: vec![TcaBenchmark::MidAtFill as i32],
                trade_date: "2026-10-07".into(),
                ..Default::default()
            },
            &scoped,
        )
        .unwrap();
    assert_eq!(one.costs.len(), 1);
    let outside = h
        .lake
        .list_execution_costs(
            ListExecutionCostsRequest {
                account_id: "ACC-2".into(),
                ..Default::default()
            },
            &scoped,
        )
        .unwrap_err();
    let (status, fields, _) = meridian_bus::read_refusal_status(&outside).unwrap();
    assert_eq!(status, meridian_bus::RefusalStatus::PermissionDenied);
    assert_eq!(fields, vec!["account_id".to_string()]);
}
