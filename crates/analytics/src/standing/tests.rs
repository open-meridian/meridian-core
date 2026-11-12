//! Standing analytics (W12.1, W12.4; contract v30; spec/one-data-access-model
//! section 4's "Tested"): two clients with one key share, a client with
//! other entitlements does not, a per-change figure recomputes only what a
//! delivery dirtied, a cadence figure waits for its cadence or a material
//! move, the last renewal's lapse stops the job, the figures reach each
//! holder in its own dense sequence, a standing figure equals `RunJob`'s on
//! the same inputs whatever order they changed in (a property, over seeded
//! cases), and across coordinators a job lives where its key falls.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use meridian_bus::{read_refusal_status, Bus, RefusalStatus};
use meridian_clock::ManualClock;
use meridian_domain::exact::Exact;
use meridian_domain::v1::*;
use prost::Message;
use rand::{Rng, SeedableRng};

use super::*;
use crate::service::{self, Config, Coordinator};
use crate::tests::{
    chain_stores, d, money, price, quote_row, serve_store, stores, Stores, AAPL, BTC, NOW,
    OSI_CALL, OSI_DEEP, OSI_PUT, USD, USDT,
};
use crate::topics;

const SECOND: i64 = 1_000_000_000;

// ─── The parts alone ────────────────────────────────────────────────────────

fn measure(
    definition: &str,
    breakdowns: &[&str],
    refresh: Refresh,
    cadence: i64,
    bp: u32,
) -> MeasureRef {
    MeasureRef {
        definition: definition.into(),
        version: 1,
        breakdowns: breakdowns.iter().map(|b| b.to_string()).collect(),
        refresh: refresh as i32,
        cadence_ns: cadence,
        material_move_bp: bp,
        ..Default::default()
    }
}

fn standing_job(measures: Vec<MeasureRef>) -> Job {
    Job {
        account_ids: vec!["ACC-1".into()],
        measures,
        standing: true,
        ..Default::default()
    }
}

fn refusal(refused: &str) -> (RefusalStatus, Vec<String>) {
    let (status, fields, _) = read_refusal_status(refused).expect("a refusal with a status");
    (status, fields)
}

#[test]
fn a_standing_job_names_no_as_of_and_every_measure_states_its_refresh() {
    let per_change = measure("market_value", &[], Refresh::PerChange, 0, 0);
    assert!(refused(&standing_job(vec![per_change.clone()])).is_ok());
    let mut as_of = standing_job(vec![per_change.clone()]);
    as_of.inputs = Some(InputRefs {
        lake_as_of_ns: NOW,
        ..Default::default()
    });
    let mut dated = standing_job(vec![per_change.clone()]);
    dated.business_date = "2026-10-16".into();
    let mut what_if = standing_job(vec![per_change.clone()]);
    what_if.what_if = Some(WhatIf::default());
    let mut watermark = standing_job(vec![per_change.clone()]);
    watermark.inputs = Some(InputRefs {
        book_at: Some(Watermark::default()),
        ..Default::default()
    });
    for job in [as_of, dated, what_if, watermark] {
        assert_eq!(
            refusal(&refused(&job).unwrap_err()),
            (RefusalStatus::InvalidArgument, vec!["job.standing".into()])
        );
    }
    // No refresh, and no definition's default: refused naming it.
    let none = standing_job(vec![
        per_change.clone(),
        measure("pnl", &[], Refresh::Unspecified, 0, 0),
    ]);
    assert_eq!(
        refusal(&refused(&none).unwrap_err()),
        (
            RefusalStatus::InvalidArgument,
            vec!["job.measures[1].refresh".into()]
        )
    );
    let no_cadence = standing_job(vec![measure("pnl", &[], Refresh::Cadence, 0, 0)]);
    assert_eq!(
        refusal(&refused(&no_cadence).unwrap_err()).1,
        vec!["job.measures[0].cadence_ns".to_string()]
    );
    let moved = standing_job(vec![measure("pnl", &[], Refresh::PerChange, 0, 50)]);
    assert_eq!(
        refusal(&refused(&moved).unwrap_err()).1,
        vec!["job.measures[0].material_move_bp".to_string()]
    );
    // A job not standing: its measures' refresh is not read.
    let mut once = none.clone();
    once.standing = false;
    once.inputs = Some(InputRefs {
        lake_as_of_ns: NOW,
        ..Default::default()
    });
    assert!(refused(&once).is_ok());
}

#[test]
fn a_key_is_the_jobs_form_its_scope_and_its_clients_datasets() {
    let job = standing_job(vec![measure("market_value", &[], Refresh::PerChange, 0, 0)]);
    let scope = Some(vec!["ACC-2".to_string(), "ACC-1".to_string()]);
    let datasets: BTreeSet<String> = ["alpaca-1:daily".to_string()].into();
    let one = key(&job, &scope, &datasets);
    // The scope's order is not the key's.
    assert_eq!(
        one,
        key(&job, &Some(vec!["ACC-1".into(), "ACC-2".into()]), &datasets)
    );
    assert_ne!(one, key(&job, &Some(vec!["ACC-1".into()]), &datasets));
    assert_ne!(one, key(&job, &None, &datasets));
    let more: BTreeSet<String> = ["alpaca-1:daily".into(), "coinbase-1:daily".into()].into();
    assert_ne!(one, key(&job, &scope, &more));
    let mut cadence = job.clone();
    cadence.measures[0] = measure("market_value", &[], Refresh::Cadence, 60 * SECOND, 0);
    assert_ne!(one, key(&cadence, &scope, &datasets));
}

#[test]
fn rendezvous_moves_only_the_keys_a_member_gained_or_lost() {
    let members: Vec<String> = ["c-a", "c-b", "c-c"]
        .iter()
        .map(|m| m.to_string())
        .collect();
    let keys: Vec<String> = (0..3_000).map(|i| format!("sha256:{i:064x}")).collect();
    let of = |members: &[String]| -> Vec<String> {
        keys.iter()
            .map(|key| owner(key, members).unwrap().clone())
            .collect()
    };
    let three = of(&members);
    // Every member computes the same choice: it is the keys' alone.
    assert_eq!(three, of(&members));
    let mut counts: BTreeMap<&String, usize> = BTreeMap::new();
    for held in &three {
        *counts.entry(held).or_default() += 1;
    }
    assert!(counts.values().all(|n| *n > 800), "{counts:?}");
    // One lost: only its keys move.
    let two = of(&members[..2]);
    for (before, after) in three.iter().zip(&two) {
        if before != "c-c" {
            assert_eq!(before, after);
        }
    }
    // One added: about a quarter move, each to it.
    let mut four = members.clone();
    four.push("c-d".into());
    let added = of(&four);
    let moved = three.iter().zip(&added).filter(|(b, a)| b != a).count();
    assert!(three.iter().zip(&added).all(|(b, a)| b == a || a == "c-d"));
    assert!((600..900).contains(&moved), "{moved} of 3000 moved");
    assert!(owner("k", &[]).is_none());
}

#[test]
fn a_material_move_is_measured_exactly_in_basis_points() {
    let e = |text: &str| Exact::from_str(text).unwrap();
    assert!(!moved_bp(e("227.50"), e("227.60"), 50));
    assert!(moved_bp(e("227.50"), e("229.00"), 50));
    // 50 bp of 200 is exactly 1.
    assert!(moved_bp(e("200"), e("201.0000"), 50));
    assert!(moved_bp(e("200"), e("199"), 50));
    assert!(!moved_bp(e("200"), e("200.99"), 50));
    assert!(!moved_bp(e("0"), e("5"), 1));
}

fn value_entry(symbol: &str, amount: &str) -> Entry {
    Entry::Value(Box::new(DerivedSeries {
        meta: Some(ObservationMeta {
            row_key: format!("implied_volatility:mid:{symbol}"),
            series_key: vec![Identifier {
                scheme: "occ_osi".into(),
                value: symbol.into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        series: "implied_volatility:mid".into(),
        value: Some(UnitValue {
            value: d(amount),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

#[test]
fn a_large_change_is_split_each_event_numbered_next_for_its_holder() {
    let changes: Vec<Entry> = (0..2_000)
        .map(|i| value_entry(&format!("AAPL  261120C{i:08}"), "0.287500"))
        .collect();
    let mut sequence = 41;
    let events = events_within(
        "JOB-1",
        "sample-options-1",
        &mut sequence,
        &changes,
        64 * 1024,
    );
    assert!(events.len() > 2, "{}", events.len());
    let numbers: Vec<i64> = events.iter().map(|e| e.sequence).collect();
    assert_eq!(numbers, (42..42 + events.len() as i64).collect::<Vec<_>>());
    assert_eq!(sequence, 41 + events.len() as i64);
    for event in &events {
        assert!(event.encoded_len() <= 64 * 1024, "{}", event.encoded_len());
        assert_eq!(event.client_instance_id, "sample-options-1");
    }
    let carried: usize = events.iter().map(|e| e.values.len()).sum();
    assert_eq!(carried, 2_000);
    // Past 4,096 entries a list, split however small they are.
    let many: Vec<Entry> = (0..5_000)
        .map(|i| {
            Entry::NotEvaluated(Box::new(NotEvaluated {
                instrument_id: format!("INS-{i}"),
                ..Default::default()
            }))
        })
        .collect();
    let mut sequence = 0;
    let split = events_within("JOB-1", "c", &mut sequence, &many, usize::MAX);
    assert_eq!(split.len(), 2);
    assert_eq!(split[0].not_evaluated.len(), MOST_ENTRIES);
    // Nothing changed, nothing sent, and the sequence stays dense.
    let mut sequence = 7;
    assert!(events_within("JOB-1", "c", &mut sequence, &[], EVENT_BUDGET).is_empty());
    assert_eq!(sequence, 7);
}

#[test]
fn only_what_changed_is_published_and_a_recomputed_time_alone_is_no_change() {
    let mut answer = JobAnswer {
        job_id: "JOB-1".into(),
        ..Default::default()
    };
    for entry in [value_entry("A", "0.20"), value_entry("B", "0.30")] {
        if let Entry::Value(value) = entry {
            answer.values.push(*value);
        }
    }
    let before = published(&answer);
    assert!(changed(&answer, &before).is_empty());
    let mut later = answer.clone();
    later.values[0].meta.as_mut().unwrap().valid_from_ns = NOW + 5;
    assert!(changed(&later, &before).is_empty());
    later.values[1].value.as_mut().unwrap().value = d("0.31");
    let moved = changed(&later, &before);
    assert_eq!(moved.len(), 1);
    assert!(
        matches!(&moved[0], Entry::Value(v) if v.meta.as_ref().unwrap().row_key.ends_with(":B"))
    );
}

// ─── A coordinator over live stores ─────────────────────────────────────────

type Prices = BTreeMap<(String, i32), (String, String, String)>;

/// Prices by subject and kind, positions by account and instrument, and
/// the datasets each client may read: the stores as a test moves them.
#[derive(Clone, Default)]
struct Live {
    prices: Arc<Mutex<Prices>>,
    positions: Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>,
    entitled: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
}

impl Live {
    fn set_price(&self, subject: &str, kind: PriceKind, amount: &str) {
        let mut prices = self.prices.lock().unwrap();
        let held = prices
            .get(&(subject.to_string(), kind as i32))
            .cloned()
            .unwrap_or_else(|| (String::new(), USD.into(), "alpaca-1:daily".into()));
        prices.insert(
            (subject.to_string(), kind as i32),
            (amount.into(), held.1, held.2),
        );
    }

    fn priced(&self, subject: &str, kind: PriceKind) -> Price {
        let prices = self.prices.lock().unwrap();
        let (amount, instrument, dataset) = &prices[&(subject.to_string(), kind as i32)];
        price(subject, kind, amount, instrument, dataset)
    }
}

/// The stores, their prices, positions and entitlements held where a test
/// moves them; the lake's listing answering each client its own datasets.
fn live_stores(base: Stores) -> (Stores, Live) {
    let live = Live::default();
    {
        let mut prices = live.prices.lock().unwrap();
        for (subject, kind, amount, instrument, dataset) in [
            (AAPL, PriceKind::Close, "227.50", USD, "alpaca-1:daily"),
            (BTC, PriceKind::Close, "60000.50", USDT, "coinbase-1:daily"),
            (USDT, PriceKind::Close, "0.9998", USD, "coinbase-1:daily"),
        ] {
            prices.insert(
                (subject.into(), kind as i32),
                (amount.into(), instrument.into(), dataset.into()),
            );
        }
        let mut positions = live.positions.lock().unwrap();
        for account in ["ACC-1", "ACC-2"] {
            positions.insert(
                account.into(),
                [
                    (AAPL.to_string(), "10".to_string()),
                    (BTC.into(), "0.5".into()),
                    (USD.into(), "5000".into()),
                ]
                .into(),
            );
        }
    }
    let prices = Arc::clone(&live.prices);
    serve_store(
        &base,
        topics::LIST_PRICES,
        "meridian.v1.ListPricesReply",
        move |request: ListPricesRequest| {
            let prices = prices.lock().unwrap();
            ListPricesReply {
                prices: prices
                    .iter()
                    .filter(|((subject, kind), _)| {
                        request.subjects.iter().any(|s| &s.entity_id == subject)
                            && (request.kinds.is_empty() || request.kinds.contains(kind))
                    })
                    .map(|((subject, kind), (amount, instrument, dataset))| {
                        price(
                            subject,
                            PriceKind::try_from(*kind).unwrap(),
                            amount,
                            instrument,
                            dataset,
                        )
                    })
                    .collect(),
                ..Default::default()
            }
        },
    );
    let positions = Arc::clone(&live.positions);
    serve_store(
        &base,
        topics::LIST_POSITIONS,
        "meridian.v1.ListPositionsReply",
        move |request: ListPositionsRequest| {
            let held = positions
                .lock()
                .unwrap()
                .get(&request.account_id)
                .cloned()
                .unwrap_or_default();
            ListPositionsReply {
                positions: held
                    .iter()
                    .map(|(instrument, quantity)| BookPosition {
                        account_id: request.account_id.clone(),
                        instrument_id: instrument.clone(),
                        side: HoldingSide::Long as i32,
                        trade_date_quantity: d(quantity),
                        lots: if instrument == USD {
                            Vec::new()
                        } else {
                            vec![Lot {
                                open_quantity: d(quantity),
                                terms: Some(LotTerms {
                                    cost: money("1000.00", USD, "USD"),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }]
                        },
                        ..Default::default()
                    })
                    .collect(),
                as_of: Some(Watermark {
                    partitions: vec![PartitionSequence {
                        partition: request.account_id,
                        sequence: 7,
                    }],
                }),
                ..Default::default()
            }
        },
    );
    let entitled = Arc::clone(&live.entitled);
    base.bus.serve(topics::LIST_DATASETS, move |envelope| {
        let client = envelope
            .meta
            .clone()
            .unwrap_or_default()
            .on_behalf_of_instance_id;
        let datasets = entitled
            .lock()
            .unwrap()
            .get(&client)
            .cloned()
            .unwrap_or_else(|| {
                vec![
                    "alpaca-1:daily".into(),
                    "coinbase-1:daily".into(),
                    "tradier-1:options".into(),
                ]
            });
        Ok((
            "meridian.v1.ListDatasetsReply".into(),
            ListDatasetsReply {
                datasets: datasets
                    .into_iter()
                    .map(|dataset| DatasetRef {
                        // Each declares the data type it serves; one
                        // declaring none is counted in every key.
                        declaration: Some(meridian_pb::v1::DatasetDeclaration {
                            data_types: vec![if dataset.ends_with(":options") {
                                "meridian.v1.Quote".into()
                            } else if dataset.ends_with(":filings") {
                                "meridian.v1.Filing".into()
                            } else if dataset.ends_with(":bare") {
                                return DatasetRef {
                                    dataset,
                                    ..Default::default()
                                };
                            } else {
                                "meridian.v1.Price".into()
                            }],
                            ..Default::default()
                        }),
                        dataset,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    (base, live)
}

/// A coordinator over `stores`, its clock the test's.
fn coordinator(stores: &Stores, clock: &Arc<ManualClock>, settings: Settings) -> Arc<Coordinator> {
    Coordinator::new(
        Arc::clone(&stores.bus),
        tokio::runtime::Handle::current(),
        Arc::clone(clock) as Arc<dyn meridian_clock::Clock>,
        Config {
            engine_version: "0.1.0".into(),
            standing: settings,
            ..Config::default()
        },
        service::local(),
    )
}

/// Run `f` off the runtime's own threads, as the bus runs a handler.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

fn scope() -> Option<Vec<String>> {
    Some(vec!["ACC-1".into(), "ACC-2".into()])
}

async fn register(c: &Arc<Coordinator>, client: &str, job: Job) -> Result<RunJobReply, String> {
    let c = Arc::clone(c);
    let client = client.to_string();
    blocking(move || c.run_job_for(RunJobRequest { job: Some(job) }, client, scope(), false)).await
}

async fn tick(c: &Arc<Coordinator>) {
    let c = Arc::clone(c);
    blocking(move || c.standing_tick()).await;
}

async fn read(c: &Arc<Coordinator>, client: &str, job_id: &str) -> Result<JobAnswer, String> {
    let c = Arc::clone(c);
    let (client, job_id) = (client.to_string(), job_id.to_string());
    blocking(move || {
        let mut whole: Option<JobAnswer> = None;
        let mut cursor = String::new();
        loop {
            let page = c.read_job_answer_for(
                ReadJobAnswerRequest {
                    job_id: job_id.clone(),
                    page_size: 0,
                    cursor: cursor.clone(),
                },
                &client,
                &scope(),
                false,
            )?;
            cursor = page.next_cursor.clone();
            match whole.as_mut() {
                None => whole = Some(page),
                Some(held) => {
                    held.figures.extend(page.figures);
                    held.values.extend(page.values);
                    held.not_evaluated.extend(page.not_evaluated);
                }
            }
            if cursor.is_empty() {
                let mut whole = whole.expect("a first page");
                whole.next_cursor.clear();
                return Ok(whole);
            }
        }
    })
    .await
}

async fn renew(
    c: &Arc<Coordinator>,
    client: &str,
    job_ids: &[&str],
) -> Result<Vec<String>, String> {
    let c = Arc::clone(c);
    let client = client.to_string();
    let request = RenewJobsRequest {
        job_ids: job_ids.iter().map(|j| j.to_string()).collect(),
    };
    blocking(move || c.renew_jobs(request, &client, false))
        .await
        .map(|reply| reply.unknown_job_ids)
}

/// A one-off job's answer for the same job, as `RunJob` gives it.
async fn once(c: &Arc<Coordinator>, client: &str, job: &Job) -> JobAnswer {
    let mut job = job.clone();
    job.standing = false;
    for measure in job.measures.iter_mut() {
        measure.refresh = 0;
        measure.cadence_ns = 0;
        measure.material_move_bp = 0;
    }
    let c = Arc::clone(c);
    let client = client.to_string();
    let reply =
        blocking(move || c.run_job_for(RunJobRequest { job: Some(job) }, client, scope(), false))
            .await
            .unwrap();
    assert!(reply.answered_inline);
    reply.answer.unwrap()
}

/// What a delivery of `price` is, as the lake publishes it.
fn delivered(c: &Arc<Coordinator>, price: Price) {
    c.standing_heard(
        "meridian.v1.PricesRecordedEvent",
        &PricesRecordedEvent { price: Some(price) }.encode_to_vec(),
    );
}

fn moved_position(c: &Arc<Coordinator>, account: &str) {
    c.standing_heard(
        "meridian.v1.PositionChangedEvent",
        &PositionChangedEvent {
            position: Some(BookPosition {
                account_id: account.into(),
                instrument_id: AAPL.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
        .encode_to_vec(),
    );
}

/// Every `DerivedSeriesUpdated` published, from now.
struct Published(Arc<Mutex<Vec<DerivedSeriesUpdatedEvent>>>);

impl Published {
    fn hear(bus: &Arc<Bus>) -> Published {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let mut subscription = bus.subscribe_every(topics::DERIVED_SERIES_UPDATED);
        let keeping = Arc::clone(&heard);
        tokio::spawn(async move {
            while let Some(delivery) = subscription.recv().await {
                keeping.lock().unwrap().push(
                    DerivedSeriesUpdatedEvent::decode(&delivery.envelope.payload[..]).unwrap(),
                );
            }
        });
        Published(heard)
    }

    async fn settled(&self) -> Vec<DerivedSeriesUpdatedEvent> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.0.lock().unwrap().clone()
    }
}

fn recomputed(c: &Arc<Coordinator>, job_id: &str) -> u64 {
    c.standing_held()
        .into_iter()
        .find(|(id, _, _)| id == job_id)
        .map(|(_, _, n)| n)
        .unwrap_or_else(|| panic!("{job_id} is not held"))
}

fn value_of_figure(answer: &JobAnswer, definition: &str) -> String {
    let figure = answer
        .figures
        .iter()
        .find(|f| f.measure.as_ref().unwrap().definition == definition && f.breakdown.is_empty())
        .unwrap_or_else(|| panic!("no {definition} figure"));
    Exact::from_wire(figure.value.as_ref().unwrap())
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_with_one_key_share_one_computation_and_other_entitlements_do_not() {
    let (stores, live) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let job = standing_job(vec![measure("market_value", &[], Refresh::PerChange, 0, 0)]);
    let first = register(&c, "reporting-1", job.clone()).await.unwrap();
    assert!(first.answered_inline);
    assert!(first.job_id.starts_with("JOB-"));
    let second = register(&c, "reporting-2", job.clone()).await.unwrap();
    assert_eq!(second.job_id, first.job_id, "one key, one job_id");
    assert_eq!(second.answer, first.answer, "the one answer, from memory");
    // One computation: its measure computed once, for both holders.
    assert_eq!(c.standing_held(), vec![(first.job_id.clone(), 2, 1)]);
    // Another client entitled to other datasets: its own job.
    live.entitled
        .lock()
        .unwrap()
        .insert("reporting-3".into(), vec!["alpaca-1:daily".into()]);
    let third = register(&c, "reporting-3", job.clone()).await.unwrap();
    assert_ne!(third.job_id, first.job_id);
    assert_eq!(c.standing_held().len(), 2);
    // Entitled besides to datasets the job never reads -- a filings
    // dataset, and a chain's quotes for a job over an account alone -- the
    // same key (narrowed 2026-11-10).
    live.entitled.lock().unwrap().insert(
        "reporting-4".into(),
        vec![
            "alpaca-1:daily".into(),
            "coinbase-1:daily".into(),
            "tradier-1:options".into(),
            "edgar-1:filings".into(),
        ],
    );
    assert_eq!(
        register(&c, "reporting-4", job.clone())
            .await
            .unwrap()
            .job_id,
        first.job_id
    );
    // Entitled besides to another price dataset, or one declaring nothing:
    // another key.
    for (client, extra) in [
        ("reporting-5", "kraken-1:daily"),
        ("reporting-6", "own-1:bare"),
    ] {
        live.entitled.lock().unwrap().insert(
            client.into(),
            vec![
                "alpaca-1:daily".into(),
                "coinbase-1:daily".into(),
                extra.into(),
            ],
        );
        assert_ne!(
            register(&c, client, job.clone()).await.unwrap().job_id,
            first.job_id,
            "{client}"
        );
    }
    assert_eq!(c.standing_held().len(), 4);
    // A one-off job of the same is no standing job.
    let mut not_standing = job.clone();
    not_standing.standing = false;
    let one_off = register(&c, "reporting-1", not_standing).await.unwrap();
    assert_ne!(one_off.job_id, first.job_id);
    assert_eq!(c.standing_held().len(), 4);
}

/// Stage M (approved 2026-11-10): a standing answer reserves its bytes
/// against the component's budget while it is held, refused for load with a
/// retry-after where there is no room, and gives them back when it stops.
#[tokio::test(flavor = "multi_thread")]
async fn a_standing_answer_holds_its_memory_within_the_budget_or_is_refused_for_load() {
    let (stores, _) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let job = standing_job(vec![
        measure("market_value", &["instrument"], Refresh::PerChange, 0, 0),
        measure("pnl", &["instrument"], Refresh::PerChange, 0, 0),
    ]);
    // A budget another request fills, with no room for its answer:
    // refused, nothing held.
    let tight = Arc::new(meridian_budget::Budget::new(Some(4 * 1024)));
    c.use_standing_budget(Arc::clone(&tight));
    let other = tight
        .reserve(3 * 1024, "another request", Duration::ZERO)
        .unwrap();
    let refused = register(&c, "reporting-1", job.clone()).await.unwrap_err();
    let (reason, _) = meridian_bus::read_refusal(&refused).expect("a refusal for load");
    assert_eq!(
        reason,
        meridian_pb::v1::RefusalReason::Overloaded as i32,
        "{refused}"
    );
    assert!(
        meridian_bus::refusal_retry_after(&refused).unwrap_or(0) >= 1,
        "{refused}"
    );
    assert!(c.standing_held().is_empty());
    assert_eq!(tight.reserved(), 3 * 1024, "the other request's alone");
    drop(other);
    // Room for it: held, its bytes reserved while it is.
    let roomy = Arc::new(meridian_budget::Budget::new(Some(64 * 1024 * 1024)));
    c.use_standing_budget(Arc::clone(&roomy));
    let reply = register(&c, "reporting-1", job.clone()).await.unwrap();
    let answer = read(&c, "reporting-1", &reply.job_id).await.unwrap();
    let held = roomy.reserved();
    assert!(
        held >= answer.encoded_len() as u64,
        "{held} for {}",
        answer.encoded_len()
    );
    assert!(
        held <= 4 * answer.encoded_len() as u64,
        "{held} for {}",
        answer.encoded_len()
    );
    // Stopped: given back.
    clock.advance(31 * SECOND);
    tick(&c).await;
    assert!(c.standing_held().is_empty());
    assert_eq!(roomy.reserved(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_jobs_refusals_name_standing_and_its_measures_refresh() {
    let (stores, _) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(
        &stores,
        &clock,
        Settings {
            most: 1,
            ..Settings::default()
        },
    );
    let mut as_of = standing_job(vec![measure("market_value", &[], Refresh::PerChange, 0, 0)]);
    as_of.inputs = Some(InputRefs {
        lake_as_of_ns: NOW,
        ..Default::default()
    });
    assert_eq!(
        refusal(&register(&c, "r-1", as_of).await.unwrap_err()),
        (RefusalStatus::InvalidArgument, vec!["job.standing".into()])
    );
    let unstated = standing_job(vec![measure(
        "market_value",
        &[],
        Refresh::Unspecified,
        0,
        0,
    )]);
    assert_eq!(
        refusal(&register(&c, "r-1", unstated).await.unwrap_err()),
        (
            RefusalStatus::InvalidArgument,
            vec!["job.measures[0].refresh".into()]
        )
    );
    let held = standing_job(vec![measure("market_value", &[], Refresh::PerChange, 0, 0)]);
    register(&c, "r-1", held.clone()).await.unwrap();
    // The same key joins, past the most or not.
    register(&c, "r-2", held).await.unwrap();
    let another = standing_job(vec![measure("pnl", &[], Refresh::PerChange, 0, 0)]);
    assert_eq!(
        refusal(&register(&c, "r-1", another).await.unwrap_err()),
        (
            RefusalStatus::FailedPrecondition,
            vec!["job.standing".into()]
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_per_change_figure_recomputes_only_what_a_delivery_dirtied() {
    let (stores, live) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let published = Published::hear(&stores.bus);
    let job = standing_job(vec![
        measure("market_value", &[], Refresh::PerChange, 0, 0),
        measure("pnl", &[], Refresh::Cadence, 60 * SECOND, 0),
    ]);
    let reply = register(&c, "reporting-1", job.clone()).await.unwrap();
    let job_id = reply.job_id.clone();
    assert_eq!(
        recomputed(&c, &job_id),
        2,
        "both, once, for the first answer"
    );
    // Nothing heard: nothing recomputed.
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 2);
    // A subject no input names: nothing dirtied.
    delivered(
        &c,
        price(
            "INS-MSFT",
            PriceKind::Close,
            "410.00",
            USD,
            "alpaca-1:daily",
        ),
    );
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 2);
    assert!(published.settled().await.is_empty());
    // An account no input names: nothing dirtied.
    moved_position(&c, "ACC-9");
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 2);
    // AAPL moves: the per-change measure alone, at once.
    live.set_price(AAPL, PriceKind::Close, "230.00");
    delivered(&c, live.priced(AAPL, PriceKind::Close));
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 3, "market_value alone");
    let events = published.settled().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].job_id, job_id);
    assert_eq!(events[0].client_instance_id, "reporting-1");
    assert_eq!(events[0].sequence, 1);
    assert!(!events[0].figures.is_empty());
    assert!(events[0]
        .figures
        .iter()
        .all(|f| f.measure.as_ref().unwrap().definition == "market_value"));
    let answer = read(&c, "reporting-1", &job_id).await.unwrap();
    let one_off = once(&c, "reporting-1", &job).await;
    assert_eq!(
        value_of_figure(&answer, "market_value"),
        value_of_figure(&one_off, "market_value")
    );
    // Conflated: deliveries before the next tick are one recomputation.
    for amount in ["231.00", "232.00", "233.00"] {
        live.set_price(AAPL, PriceKind::Close, amount);
        delivered(&c, live.priced(AAPL, PriceKind::Close));
    }
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 4);
    let events = published.settled().await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].sequence, 2);
    // The same value again: recomputed, nothing published.
    delivered(&c, live.priced(AAPL, PriceKind::Close));
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 5);
    assert_eq!(published.settled().await.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cadence_figure_waits_for_its_cadence() {
    let (stores, live) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let job = standing_job(vec![measure(
        "market_value",
        &[],
        Refresh::Cadence,
        60 * SECOND,
        0,
    )]);
    let job_id = register(&c, "reporting-1", job).await.unwrap().job_id;
    live.set_price(AAPL, PriceKind::Close, "230.00");
    delivered(&c, live.priced(AAPL, PriceKind::Close));
    clock.advance(20 * SECOND);
    assert!(renew(&c, "reporting-1", &[&job_id])
        .await
        .unwrap()
        .is_empty());
    tick(&c).await;
    assert_eq!(
        recomputed(&c, &job_id),
        1,
        "dirty, but its cadence has not passed"
    );
    clock.advance(20 * SECOND);
    assert!(renew(&c, "reporting-1", &[&job_id])
        .await
        .unwrap()
        .is_empty());
    clock.advance(20 * SECOND);
    assert!(renew(&c, "reporting-1", &[&job_id])
        .await
        .unwrap()
        .is_empty());
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 2, "its cadence passed");
    // Its cadence passed again but nothing dirtied it: nothing recomputed.
    for _ in 0..4 {
        clock.advance(20 * SECOND);
        assert!(renew(&c, "reporting-1", &[&job_id])
            .await
            .unwrap()
            .is_empty());
    }
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 2);
}

fn chain_job(refresh: Refresh, cadence: i64, bp: u32) -> Job {
    let mut job = standing_job(vec![measure(
        "implied_volatility",
        &["series", "quote_side"],
        refresh,
        cadence,
        bp,
    )]);
    job.account_ids.clear();
    job.chains = vec![ChainRef {
        underlying_instrument_id: AAPL.into(),
        expiration_from: "2026-11-20".into(),
        expiration_to: "2026-11-20".into(),
        series_keys: Vec::new(),
    }];
    job
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cadence_figure_on_a_chain_recomputes_sooner_on_a_material_move() {
    let (stores, live) = live_stores(chain_stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let job = chain_job(Refresh::Cadence, 3_600 * SECOND, 50);
    let job_id = register(&c, "signal-1", job).await.unwrap().job_id;
    // Each series' IV a value, keyed by subject and series (question 1).
    let answer = read(&c, "signal-1", &job_id).await.unwrap();
    assert!(answer.figures.is_empty(), "{:?}", answer.figures);
    assert_eq!(answer.values.len(), 6);
    let value = &answer.values[0];
    let meta = value.meta.as_ref().unwrap();
    assert_eq!(meta.subjects[0].entity_id, AAPL);
    assert_eq!(meta.series_key[0].scheme, "occ_osi");
    assert!(value.series.starts_with("implied_volatility:"));
    assert_eq!(
        meta.row_key,
        format!("{}:{}", value.series, meta.series_key[0].value)
    );
    assert_eq!(
        value.rule.as_ref().unwrap().definition,
        "implied_volatility"
    );
    assert!(value
        .inputs
        .iter()
        .any(|i| i.dataset == "tradier-1:options" && i.as_of_ns == NOW));
    // 227.50 to 227.60: under 50 bp; nothing before the hour.
    live.set_price(AAPL, PriceKind::Close, "227.60");
    delivered(&c, live.priced(AAPL, PriceKind::Close));
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 1);
    // To 229.00: past 50 bp of 227.50, recomputed before its cadence.
    live.set_price(AAPL, PriceKind::Close, "229.00");
    delivered(&c, live.priced(AAPL, PriceKind::Close));
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 2);
    // The fit's reference is now 229.00: 229.50 is under 50 bp of it.
    live.set_price(AAPL, PriceKind::Close, "229.50");
    delivered(&c, live.priced(AAPL, PriceKind::Close));
    tick(&c).await;
    assert_eq!(recomputed(&c, &job_id), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_renewal_lapses_and_the_job_stops() {
    let (stores, _) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let job = standing_job(vec![measure("market_value", &[], Refresh::PerChange, 0, 0)]);
    let job_id = register(&c, "reporting-1", job.clone())
        .await
        .unwrap()
        .job_id;
    register(&c, "reporting-2", job).await.unwrap();
    // An instance holding none of it renews it: unknown, never held for it.
    assert_eq!(
        renew(&c, "reporting-9", &[&job_id]).await.unwrap(),
        vec![job_id.clone()]
    );
    // One holder renews; the other lapses alone.
    clock.advance(20 * SECOND);
    assert!(renew(&c, "reporting-1", &[&job_id])
        .await
        .unwrap()
        .is_empty());
    clock.advance(20 * SECOND);
    tick(&c).await;
    assert_eq!(c.standing_held(), vec![(job_id.clone(), 1, 1)]);
    assert!(read(&c, "reporting-2", &job_id).await.is_err());
    // The last renewal lapses: the job stops.
    clock.advance(31 * SECOND);
    tick(&c).await;
    assert!(c.standing_held().is_empty());
    assert_eq!(
        renew(&c, "reporting-1", &[&job_id, "JOB-NEVER"])
            .await
            .unwrap(),
        vec![job_id.clone(), "JOB-NEVER".to_string()]
    );
    let gone = read(&c, "reporting-1", &job_id).await.unwrap_err();
    assert_eq!(
        refusal(&gone),
        (RefusalStatus::NotFound, vec!["job_id".into()])
    );
    // An empty renewal names job_ids.
    assert_eq!(
        refusal(&renew(&c, "reporting-1", &[]).await.unwrap_err()),
        (RefusalStatus::InvalidArgument, vec!["job_ids".into()])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_holder_hears_its_own_dense_sequence() {
    let (stores, live) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let published = Published::hear(&stores.bus);
    let job = standing_job(vec![measure("market_value", &[], Refresh::PerChange, 0, 0)]);
    let job_id = register(&c, "portfolio-1", job.clone())
        .await
        .unwrap()
        .job_id;
    for amount in ["228.00", "229.00"] {
        live.set_price(AAPL, PriceKind::Close, amount);
        delivered(&c, live.priced(AAPL, PriceKind::Close));
        tick(&c).await;
    }
    register(&c, "portfolio-2", job).await.unwrap();
    live.set_price(AAPL, PriceKind::Close, "230.00");
    delivered(&c, live.priced(AAPL, PriceKind::Close));
    tick(&c).await;
    let events = published.settled().await;
    let of = |client: &str| -> Vec<i64> {
        events
            .iter()
            .filter(|e| e.client_instance_id == client)
            .map(|e| e.sequence)
            .collect()
    };
    assert_eq!(of("portfolio-1"), vec![1, 2, 3]);
    assert_eq!(of("portfolio-2"), vec![1], "its own count, from its first");
    assert!(events.iter().all(|e| e.job_id == job_id));
}

fn normal(answer: &JobAnswer) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut figures: Vec<Vec<u8>> = answer.figures.iter().map(|f| f.encode_to_vec()).collect();
    figures.sort();
    let mut unevaluated: Vec<Vec<u8>> = answer
        .not_evaluated
        .iter()
        .map(|n| n.encode_to_vec())
        .collect();
    unevaluated.sort();
    (figures, unevaluated)
}

/// The property (W12's requirement 25; the spec's "Tested"): over seeded
/// cases, after every input change -- a price, a quantity, in any order --
/// the standing job's figures are `RunJob`'s for the same job on the same
/// inputs.
#[tokio::test(flavor = "multi_thread")]
async fn a_standing_figure_equals_run_jobs_on_the_same_inputs_whatever_order_they_change_in() {
    let (stores, live) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let mut job = standing_job(vec![
        measure("market_value", &[], Refresh::PerChange, 0, 0),
        measure("market_value", &["asset_class"], Refresh::PerChange, 0, 0),
        measure("exposure", &["asset_class"], Refresh::PerChange, 0, 0),
        measure("pnl", &[], Refresh::PerChange, 0, 0),
        measure("concentration", &["instrument"], Refresh::PerChange, 0, 0),
    ]);
    job.account_ids = vec!["ACC-1".into(), "ACC-2".into()];
    let job_id = register(&c, "reporting-1", job.clone())
        .await
        .unwrap()
        .job_id;
    for seed in 0..12u64 {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let mut changes: Vec<u8> = (0..6).map(|_| rng.gen_range(0..4u8)).collect();
        // Half the cases take their changes in the other order.
        if seed % 2 == 1 {
            changes.reverse();
        }
        for change in changes {
            match change {
                0 => {
                    let amount =
                        format!("{}.{:02}", rng.gen_range(150..300), rng.gen_range(0..100));
                    live.set_price(AAPL, PriceKind::Close, &amount);
                    delivered(&c, live.priced(AAPL, PriceKind::Close));
                }
                1 => {
                    let amount = format!("{}.50", rng.gen_range(40_000..90_000));
                    live.set_price(BTC, PriceKind::Close, &amount);
                    delivered(&c, live.priced(BTC, PriceKind::Close));
                }
                2 => {
                    let amount = format!("0.99{:02}", rng.gen_range(0..100));
                    live.set_price(USDT, PriceKind::Close, &amount);
                    delivered(&c, live.priced(USDT, PriceKind::Close));
                }
                _ => {
                    let account = if rng.gen_bool(0.5) { "ACC-1" } else { "ACC-2" };
                    let quantity = rng.gen_range(1..40).to_string();
                    live.positions
                        .lock()
                        .unwrap()
                        .get_mut(account)
                        .unwrap()
                        .insert(AAPL.into(), quantity);
                    moved_position(&c, account);
                }
            }
            tick(&c).await;
            let standing = read(&c, "reporting-1", &job_id).await.unwrap();
            let one_off = once(&c, "reporting-1", &job).await;
            assert_eq!(
                normal(&standing),
                normal(&one_off),
                "seed {seed}: the standing figures are RunJob's on the same inputs"
            );
            assert!(
                standing.values.is_empty(),
                "no figure here is keyed by series"
            );
        }
    }
}

/// The same property over a chain, its figures carried as values: after a
/// quote moves, each standing IV is `RunJob`'s for its series and side.
#[tokio::test(flavor = "multi_thread")]
async fn a_standing_chains_values_equal_run_jobs_figures_after_its_quotes_move() {
    let (stores, _) = live_stores(chain_stores());
    let quotes: Arc<Mutex<Vec<Quote>>> = Arc::new(Mutex::new(vec![
        quote_row(OSI_CALL, "8.10", "8.30"),
        quote_row(OSI_PUT, "9.40", "9.60"),
        quote_row(OSI_DEEP, "70.00", "71.00"),
    ]));
    let held = Arc::clone(&quotes);
    serve_store(
        &stores,
        topics::LIST_QUOTES,
        "meridian.v1.ListQuotesReply",
        move |_request: ListQuotesRequest| ListQuotesReply {
            quotes: held.lock().unwrap().clone(),
            ..Default::default()
        },
    );
    let clock = Arc::new(ManualClock::at(NOW));
    let c = coordinator(&stores, &clock, Settings::default());
    let published = Published::hear(&stores.bus);
    let job = chain_job(Refresh::PerChange, 0, 0);
    let job_id = register(&c, "signal-1", job.clone()).await.unwrap().job_id;
    for (bid, ask) in [("8.40", "8.60"), ("7.90", "8.05"), ("8.20", "8.50")] {
        quotes.lock().unwrap()[0] = quote_row(OSI_CALL, bid, ask);
        let mut moved = quote_row(OSI_CALL, bid, ask);
        moved.meta.as_mut().unwrap().subjects = vec![SubjectRef {
            entity_id: AAPL.into(),
            ..Default::default()
        }];
        c.standing_heard(
            "meridian.v1.QuotesRecordedEvent",
            &QuotesRecordedEvent { quote: Some(moved) }.encode_to_vec(),
        );
        tick(&c).await;
        let standing = read(&c, "signal-1", &job_id).await.unwrap();
        let one_off = once(&c, "signal-1", &job).await;
        let by_key = |series_key: &[Identifier], side: &str| -> Option<meridian_pb::v1::Decimal> {
            one_off
                .figures
                .iter()
                .find(|f| {
                    f.series_key == series_key
                        && f.breakdown
                            .iter()
                            .any(|b| b.breakdown == "quote_side" && b.value == side)
                })
                .and_then(|f| f.value)
        };
        assert_eq!(standing.values.len(), 6);
        for value in &standing.values {
            let meta = value.meta.as_ref().unwrap();
            let side = value.series.rsplit(':').next().unwrap();
            assert_eq!(
                value.value.as_ref().unwrap().value,
                by_key(&meta.series_key, side),
                "{} {side}",
                meta.series_key[0].value
            );
        }
        assert_eq!(normal(&standing).1, normal(&one_off).1);
    }
    // Each move published the values it changed as values -- the call's
    // sides, and the put's, whose forward from put-call parity moved with
    // it -- never the deep call, not evaluated throughout.
    let events = published.settled().await;
    assert_eq!(events.len(), 3);
    for (i, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, i as i64 + 1);
        assert!(event.figures.is_empty());
        assert!(event
            .values
            .iter()
            .any(|v| v.meta.as_ref().unwrap().series_key[0].value == OSI_CALL));
        assert!(event
            .values
            .iter()
            .all(|v| v.meta.as_ref().unwrap().series_key[0].value != OSI_DEEP));
    }
}

// ─── Across coordinators (question 13) ──────────────────────────────────────

/// Coordinators in one process, each asked directly as its own topic would
/// be, the live set as a test says.
#[derive(Clone, Default)]
struct Cluster {
    members: Arc<Mutex<BTreeMap<String, Weak<Coordinator>>>>,
    live: Arc<Mutex<Vec<String>>>,
}

struct Member {
    me: String,
    cluster: Cluster,
}

impl Peers for Member {
    fn me(&self) -> String {
        self.me.clone()
    }
    fn live(&self) -> Vec<String> {
        self.cluster.live.lock().unwrap().clone()
    }
    fn ask(
        &self,
        member: &str,
        asked: Asked,
        payload: Vec<u8>,
        client: &str,
        scope: &Option<Vec<String>>,
    ) -> Result<Vec<u8>, String> {
        if !self.live().iter().any(|m| m == member) {
            return Err(format!("{member} is gone"));
        }
        let target = self
            .cluster
            .members
            .lock()
            .unwrap()
            .get(member)
            .and_then(Weak::upgrade)
            .ok_or_else(|| format!("{member} is gone"))?;
        let envelope = Envelope {
            meta: Some(meridian_pb::v1::MessageMeta {
                publisher_instance_id: crate::INSTANCE.into(),
                on_behalf_of_instance_id: client.into(),
                account_scope: scope.clone().unwrap_or_default(),
                account_scope_applies: scope.is_some(),
                ..Default::default()
            }),
            payload_type: asked.payload_type().into(),
            payload,
        };
        let (kind, reply) = target.handed(&envelope)?;
        assert_ne!(kind, REFUSED_TYPE);
        Ok(reply)
    }
}

fn member(
    cluster: &Cluster,
    name: &str,
    stores: &Stores,
    clock: &Arc<ManualClock>,
) -> Arc<Coordinator> {
    let c = coordinator(stores, clock, Settings::default());
    c.use_peers(Arc::new(Member {
        me: name.into(),
        cluster: cluster.clone(),
    }));
    cluster
        .members
        .lock()
        .unwrap()
        .insert(name.into(), Arc::downgrade(&c));
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_job_lives_where_its_key_falls_and_moves_when_the_coordinators_change() {
    let (stores, _) = live_stores(stores());
    let clock = Arc::new(ManualClock::at(NOW));
    let cluster = Cluster::default();
    *cluster.live.lock().unwrap() = vec!["c-a".into(), "c-b".into()];
    let a = member(&cluster, "c-a", &stores, &clock);
    let b = member(&cluster, "c-b", &stores, &clock);
    let job = standing_job(vec![measure("market_value", &[], Refresh::PerChange, 0, 0)]);
    let datasets: BTreeSet<String> = [
        "alpaca-1:daily".into(),
        "coinbase-1:daily".into(),
        "tradier-1:options".into(),
    ]
    .into();
    let its_key = key(&job, &scope(), &datasets);
    let two: Vec<String> = vec!["c-a".into(), "c-b".into()];
    let (held_by, other, survivor) = if owner(&its_key, &two).unwrap() == "c-a" {
        (&a, &b, "c-b")
    } else {
        (&b, &a, "c-a")
    };
    // Registered at the coordinator it does not fall to: held by its owner.
    let reply = register(other, "reporting-1", job.clone()).await.unwrap();
    assert!(reply.answered_inline);
    assert_eq!(held_by.standing_held(), vec![(reply.job_id.clone(), 1, 1)]);
    assert!(other.standing_held().is_empty());
    // Registered at its owner by another client: the same job.
    assert_eq!(
        register(held_by, "reporting-2", job.clone())
            .await
            .unwrap()
            .job_id,
        reply.job_id
    );
    // A renewal and a read reaching the other coordinator are asked of it.
    assert!(renew(other, "reporting-1", &[&reply.job_id])
        .await
        .unwrap()
        .is_empty());
    let answer = read(other, "reporting-1", &reply.job_id).await.unwrap();
    assert_eq!(answer.job_id, reply.job_id);
    // Its owner lost: renewals are answered unknown, and registered again
    // the job is held by the one left.
    *cluster.live.lock().unwrap() = vec![survivor.into()];
    assert_eq!(
        renew(other, "reporting-1", &[&reply.job_id]).await.unwrap(),
        vec![reply.job_id.clone()]
    );
    let again = register(other, "reporting-1", job.clone()).await.unwrap();
    assert_ne!(again.job_id, reply.job_id);
    assert_eq!(other.standing_held(), vec![(again.job_id.clone(), 1, 1)]);
    // A coordinator added: where the key now falls to it, the holder lets
    // the job go at its next renewal, answered unknown, to be registered
    // with it; else it stays.
    let c = member(&cluster, "c-c", &stores, &clock);
    let mut added = vec![survivor.to_string(), "c-c".to_string()];
    added.sort();
    *cluster.live.lock().unwrap() = added.clone();
    let unknown = renew(other, "reporting-1", &[&again.job_id]).await.unwrap();
    if owner(&its_key, &added).unwrap() == "c-c" {
        assert_eq!(unknown, vec![again.job_id.clone()]);
        assert!(other.standing_held().is_empty());
        let moved = register(other, "reporting-1", job).await.unwrap();
        assert_eq!(c.standing_held(), vec![(moved.job_id, 1, 1)]);
    } else {
        assert!(unknown.is_empty(), "the key stays where it was");
        assert_eq!(other.standing_held().len(), 1);
    }
}
