//! The component on an in-memory bus, the stores stood up as their reads
//! answer: a job resolved on the client's behalf and answered inline and
//! later, by the pool and by the coordinator alike, and a routed model
//! priced through a `pricing` plugin's batch.

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_analytics_engine::graph::Cache;
use meridian_bus::{Bus, MemoryBackend, Stamp};
use meridian_clock::ManualClock;
use meridian_domain::exact::Exact;
use meridian_domain::v1::*;
use meridian_pb::v1::MessageMeta;
use prost::Message;

use crate::service::{self, Config, Coordinator};
use crate::{topics, worker};

const NOW: i64 = 1_792_300_000_000_000_000;
const USD: &str = "INS-CASHUSD";
const USDT: &str = "INS-USDT";
const AAPL: &str = "INS-AAPL";
const BTC: &str = "INS-BTC";
const YES: &str = "INS-E1";
const NO: &str = "INS-E2";

fn d(text: &str) -> Option<meridian_pb::v1::Decimal> {
    Some(Exact::from_str(text).unwrap().to_wire())
}

fn money(amount: &str, instrument: &str, code: &str) -> Option<Money> {
    Some(Money {
        amount: d(amount),
        currency_code: code.into(),
        instrument_id: instrument.into(),
    })
}

fn price(subject: &str, kind: PriceKind, amount: &str, instrument: &str, dataset: &str) -> Price {
    Price {
        meta: Some(ObservationMeta {
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
            }],
            source: Some(Source {
                dataset: dataset.into(),
                ..Default::default()
            }),
            valid_from_ns: NOW - 10,
            business_date: "2026-10-16".into(),
            ..Default::default()
        }),
        kind: kind as i32,
        price: money(amount, instrument, ""),
        basis: PriceBasis::PerUnit as i32,
    }
}

fn record(id: &str, class: AssetClass, currency: &str, template: &str) -> InstrumentRecord {
    InstrumentRecord {
        instrument_id: id.into(),
        asset_class: class as i32,
        currency: currency.into(),
        version: 3,
        template_id: template.into(),
        template_version: "2".into(),
        binary_event_contract: (class == AssetClass::EventContract).then(|| BinaryEventContract {
            payout: money("1.00", USD, "USD"),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// What the stores were asked, and how: every envelope's stamp.
#[derive(Default)]
struct Heard {
    stamps: Vec<(String, MessageMeta)>,
}

struct Stores {
    bus: Arc<Bus>,
    heard: Arc<Mutex<Heard>>,
    routing: Arc<Mutex<Vec<ModelRouting>>>,
}

fn serve_store<Req, Rep, F>(
    stores: &Stores,
    topic: &'static str,
    reply_type: &'static str,
    answer: F,
) where
    Req: Message + Default,
    Rep: Message,
    F: Fn(Req) -> Rep + Send + Sync + 'static,
{
    let heard = Arc::clone(&stores.heard);
    stores.bus.serve(topic, move |envelope| {
        heard
            .lock()
            .unwrap()
            .stamps
            .push((topic.to_string(), envelope.meta.clone().unwrap_or_default()));
        let request = Req::decode(&envelope.payload[..]).map_err(|e| e.to_string())?;
        Ok((reply_type.to_string(), answer(request).encode_to_vec()))
    });
}

/// The book, the instrument store, the lake and the conductor, as their
/// reads answer, for one account: AAPL with a lot in USD, cash, half a BTC
/// priced in USDT, a local line, and a member of an event group.
fn stores() -> Stores {
    let bus = Arc::new(Bus::single(
        crate::INSTANCE,
        Arc::new(MemoryBackend::new()),
        Arc::new(ManualClock::at(NOW)),
    ));
    let stores = Stores {
        bus,
        heard: Arc::default(),
        routing: Arc::new(Mutex::new(Vec::new())),
    };
    serve_store(
        &stores,
        topics::LIST_ACCOUNT_ATTRIBUTES,
        "meridian.v1.ListAccountAttributesReply",
        |request: ListAccountAttributesRequest| ListAccountAttributesReply {
            attributes: vec![AccountAttributes {
                account_id: request.account_id,
                base_currency_code: "USD".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    serve_store(
        &stores,
        topics::LIST_POSITIONS,
        "meridian.v1.ListPositionsReply",
        |request: ListPositionsRequest| {
            let lot = |quantity: &str, cost: &str, sleeve: &str| Lot {
                open_quantity: d(quantity),
                terms: Some(LotTerms {
                    cost: money(cost, USD, "USD"),
                    ..Default::default()
                }),
                sleeve: sleeve.into(),
                ..Default::default()
            };
            let position = |id: &str, quantity: &str, lots: Vec<Lot>| BookPosition {
                account_id: request.account_id.clone(),
                instrument_id: id.into(),
                side: HoldingSide::Long as i32,
                trade_date_quantity: d(quantity),
                lots,
                ..Default::default()
            };
            ListPositionsReply {
                positions: vec![
                    position(
                        AAPL,
                        "10",
                        vec![lot("6", "1200.00", "core"), lot("4", "900.00", "hedge")],
                    ),
                    position(USD, "5000", vec![]),
                    position(BTC, "0.5", vec![lot("0.5", "25000.00", "")]),
                    position("LCL-01JX", "3", vec![]),
                    position(YES, "100", vec![lot("100", "42.00", "")]),
                ],
                next_cursor: String::new(),
                as_of: Some(Watermark {
                    partitions: vec![PartitionSequence {
                        partition: request.account_id,
                        sequence: 81234,
                    }],
                }),
            }
        },
    );
    serve_store(
        &stores,
        topics::RESOLVE_INSTRUMENT,
        "meridian.v1.ResolveInstrumentReply",
        |request: ResolveInstrumentRequest| {
            let found = match request.instrument_id.as_str() {
                USD => Some(record(USD, AssetClass::Cash, "USD", "TPL-cash")),
                USDT => Some(record(USDT, AssetClass::Cash, "USDT", "TPL-cash")),
                AAPL => Some(record(AAPL, AssetClass::Equity, "USD", "TPL-equity")),
                BTC => Some(record(BTC, AssetClass::CryptoAsset, "USDT", "TPL-crypto")),
                YES | NO => Some(record(
                    &request.instrument_id,
                    AssetClass::EventContract,
                    "USD",
                    "TPL-event",
                )),
                _ => None,
            };
            ResolveInstrumentReply {
                found: found.is_some(),
                instrument: found,
            }
        },
    );
    serve_store(
        &stores,
        topics::READ_TEMPLATE,
        "meridian.v1.ReadTemplateReply",
        |request: ReadTemplateRequest| {
            let (model, inputs): (&str, &[&str]) = match request.template_id.as_str() {
                "TPL-cash" => ("cash", &["fx"]),
                "TPL-event" => ("binary_event", &["last", "settlement_value"]),
                _ => ("quoted_price", &["close", "fx"]),
            };
            ReadTemplateReply {
                template: Some(SecurityTemplate {
                    template_id: request.template_id,
                    template_version: request.template_version,
                    calculation_package: Some(CalculationPackage {
                        model_id: model.into(),
                        model_version: 1,
                        pricing_inputs: inputs.iter().map(|i| i.to_string()).collect(),
                        calculation_version: "1".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            }
        },
    );
    serve_store(
        &stores,
        topics::LIST_EVENT_GROUPS,
        "meridian.v1.ListEventGroupsReply",
        |_request: ListEventGroupsRequest| ListEventGroupsReply {
            event_groups: vec![EventGroup {
                event_group_id: "EVG-1".into(),
                member_instrument_ids: vec![YES.into(), NO.into()],
                mutually_exclusive: EventGroupFlag::True as i32,
                exhaustive: EventGroupFlag::True as i32,
                version: 2,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    serve_store(
        &stores,
        topics::LIST_PRICES,
        "meridian.v1.ListPricesReply",
        |request: ListPricesRequest| {
            let all = vec![
                price(AAPL, PriceKind::Close, "227.50", USD, "alpaca-1:daily"),
                price(BTC, PriceKind::Close, "60000.50", USDT, "coinbase-1:daily"),
                price(USDT, PriceKind::Close, "0.9998", USD, "coinbase-1:daily"),
                price(YES, PriceKind::Bid, "0.41", USD, "kalshi-1:quotes"),
                price(YES, PriceKind::Ask, "0.43", USD, "kalshi-1:quotes"),
                price(YES, PriceKind::Last, "0.42", USD, "kalshi-1:live"),
                price(NO, PriceKind::Last, "0.57", USD, "kalshi-1:live"),
            ];
            let wanted: Vec<&str> = request
                .subjects
                .iter()
                .map(|s| s.entity_id.as_str())
                .collect();
            ListPricesReply {
                prices: all
                    .into_iter()
                    .filter(|p| {
                        wanted.contains(&p.meta.as_ref().unwrap().subjects[0].entity_id.as_str())
                            && (request.kinds.is_empty() || request.kinds.contains(&p.kind))
                    })
                    .collect(),
                ..Default::default()
            }
        },
    );
    serve_store(
        &stores,
        topics::LIST_OUTCOMES,
        "meridian.v1.ListOutcomesReply",
        |_request: ListOutcomesRequest| ListOutcomesReply::default(),
    );
    let routing = Arc::clone(&stores.routing);
    serve_store(
        &stores,
        topics::LIST_MODEL_ROUTING,
        "meridian.v1.ListModelRoutingReply",
        move |_request: ListModelRoutingRequest| {
            let routes = routing.lock().unwrap().clone();
            ListModelRoutingReply {
                version: 1 + routes.len() as i64,
                routes,
            }
        },
    );
    stores
}

fn coordinator(stores: &Stores, config: Config, pooled: bool) -> Arc<Coordinator> {
    let handle = tokio::runtime::Handle::current();
    let executor = if pooled {
        worker::serve(&stores.bus, Arc::new(Cache::new(64)));
        service::pool(&stores.bus, handle.clone(), Duration::from_secs(5))
    } else {
        service::local()
    };
    let coordinator = Coordinator::new(
        Arc::clone(&stores.bus),
        handle,
        Arc::new(ManualClock::at(NOW)),
        Config {
            engine_version: "0.1.0".into(),
            ..config
        },
        executor,
    );
    service::serve(&stores.bus, &coordinator);
    coordinator
}

fn job(measures: &[(&str, &[&str], &str)]) -> RunJobRequest {
    RunJobRequest {
        job: Some(Job {
            account_ids: vec!["ACC-1".into()],
            inputs: None,
            measures: measures
                .iter()
                .map(|(definition, breakdowns, group)| MeasureRef {
                    definition: definition.to_string(),
                    version: 1,
                    breakdowns: breakdowns.iter().map(|b| b.to_string()).collect(),
                    event_group_id: group.to_string(),
                })
                .collect(),
            what_if: None,
        }),
    }
}

/// Asked as a client's sidecar asks, its read scope stamped as applying.
/// One in-memory bus serves every party here, so the client is the bus's
/// own instance.
async fn ask_as<R: Message + Default>(
    bus: &Bus,
    topic: &str,
    kind: &str,
    request: impl Message,
    scope: &[&str],
) -> Result<R, String> {
    let (_, payload) = bus
        .call_stamped(
            topic,
            kind,
            request.encode_to_vec(),
            None,
            Some(Duration::from_secs(10)),
            &Stamp {
                account_scope: Some(scope.iter().map(|s| s.to_string()).collect()),
                ..Stamp::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(R::decode(&payload[..]).unwrap())
}

fn value(answer: &JobAnswer, definition: &str, breakdown: &[(&str, &str)]) -> String {
    let figure = answer
        .figures
        .iter()
        .find(|f| {
            f.measure.as_ref().unwrap().definition == definition
                && f.breakdown.len() == breakdown.len()
                && f.breakdown
                    .iter()
                    .zip(breakdown)
                    .all(|(b, (name, v))| b.breakdown == *name && b.value == *v)
        })
        .unwrap_or_else(|| panic!("no {definition} figure for {breakdown:?}"));
    Exact::from_wire(figure.value.as_ref().unwrap())
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_is_resolved_on_the_clients_behalf_and_answered_inline() {
    let stores = stores();
    coordinator(&stores, Config::default(), false);
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        job(&[
            ("market_value", &[], ""),
            ("market_value", &["sleeve"], ""),
            ("implied_distribution", &[], "EVG-1"),
            ("pnl", &[], ""),
        ]),
        &["ACC-1"],
    )
    .await
    .unwrap();
    assert!(reply.answered_inline);
    assert!(reply.job_id.starts_with("JOB-") && reply.job_id.len() == 30);
    let answer = reply.answer.unwrap();
    // 10 x 227.50 + 5000 + 0.5 x 60000.50 x 0.9998 + 100 x 0.42 (last).
    assert_eq!(value(&answer, "market_value", &[]), "37311.25");
    assert_eq!(
        value(&answer, "market_value", &[("sleeve", "core")]),
        "1365.00"
    );
    // The local line is not evaluated, never zero; the cash has no sleeve.
    let reasons: Vec<(String, i32)> = answer
        .not_evaluated
        .iter()
        .map(|u| (u.instrument_id.clone(), u.reason))
        .collect();
    assert!(reasons.contains(&("LCL-01JX".into(), NotEvaluatedReason::LocalRecord as i32)));
    assert!(reasons.contains(&(USD.into(), NotEvaluatedReason::SchemeNotHeld as i32)));
    // pnl version 1 is unrealized alone (ruling 320): one component per
    // bucket, and nothing said of realized.
    let pnl: Vec<&Figure> = answer
        .figures
        .iter()
        .filter(|f| f.measure.as_ref().unwrap().definition == "pnl")
        .collect();
    assert_eq!(pnl.len(), 1);
    assert_eq!(pnl[0].breakdown[0].value, "unrealized");
    assert!(!answer
        .not_evaluated
        .iter()
        .any(|u| u.detail.contains("realized")));
    let provenance = answer.provenance.unwrap();
    assert_eq!(provenance.engine_version, "0.1.0");
    assert!(provenance.answer_hash.starts_with("sha256:"));
    assert_eq!(provenance.routing_version, 1);
    assert_eq!(
        provenance.book_watermark.unwrap().partitions[0].sequence,
        81234
    );
    assert!(provenance
        .datasets
        .contains(&"coinbase-1:daily".to_string()));
    assert_eq!(provenance.event_groups[0].version, 2);
    // Every store read carried the client (here the bus's own instance)
    // and the scope its sidecar stamped (W12.2).
    let heard = stores.heard.lock().unwrap();
    for (topic, meta) in heard
        .stamps
        .iter()
        .filter(|(t, _)| t != topics::LIST_MODEL_ROUTING)
    {
        assert_eq!(meta.publisher_instance_id, crate::INSTANCE, "{topic}");
        assert_eq!(meta.on_behalf_of_instance_id, crate::INSTANCE, "{topic}");
        assert!(meta.account_scope_applies, "{topic}");
        assert_eq!(meta.account_scope, vec!["ACC-1".to_string()], "{topic}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_outside_the_scope_and_a_definition_not_served_are_refused_naming_them() {
    let stores = stores();
    coordinator(&stores, Config::default(), false);
    let outside: Result<RunJobReply, _> = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        job(&[("market_value", &[], "")]),
        &["ACC-2"],
    )
    .await;
    let refused = outside.unwrap_err();
    assert!(
        refused.contains("job.account_ids[0]") && refused.contains("permission-denied"),
        "{refused}"
    );
    let mut bad = job(&[("market_value", &[], "")]);
    bad.job.as_mut().unwrap().measures[0].version = 9;
    let refused = ask_as::<RunJobReply>(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        bad,
        &["ACC-1"],
    )
    .await
    .unwrap_err();
    assert!(refused.contains("job.measures[0].version"), "{refused}");
    let definitions: ListMeasureDefinitionsReply = ask_as(
        &stores.bus,
        topics::LIST_MEASURE_DEFINITIONS,
        "meridian.v1.ListMeasureDefinitionsRequest",
        ListMeasureDefinitionsRequest {},
        &[],
    )
    .await
    .unwrap();
    assert_eq!(definitions.definitions.len(), 8);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pool_and_the_coordinator_give_the_same_answer_and_hash() {
    let alone = stores();
    coordinator(&alone, Config::default(), false);
    let pooled = stores();
    coordinator(&pooled, Config::default(), true);
    let request = || {
        job(&[
            ("exposure", &["asset_class"], ""),
            ("concentration", &["instrument"], ""),
            ("resolution_payoff", &[], "EVG-1"),
        ])
    };
    let mut answers = Vec::new();
    for stores in [&alone, &pooled] {
        let reply: RunJobReply = ask_as(
            &stores.bus,
            topics::RUN_JOB,
            "meridian.v1.RunJobRequest",
            request(),
            &["ACC-1"],
        )
        .await
        .unwrap();
        let mut answer = reply.answer.unwrap();
        answer.job_id.clear();
        answers.push(answer);
    }
    assert_eq!(answers[0], answers[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_past_its_window_finishes_later_and_its_answer_is_read() {
    let stores = stores();
    let mut finished = stores.bus.subscribe_every(topics::JOB_FINISHED);
    coordinator(
        &stores,
        Config {
            inline: Duration::ZERO,
            ..Config::default()
        },
        false,
    );
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        job(&[("market_value", &[], "")]),
        &["ACC-1"],
    )
    .await
    .unwrap();
    assert!(!reply.answered_inline && reply.answer.is_none());
    let told = tokio::time::timeout(Duration::from_secs(10), finished.recv())
        .await
        .unwrap()
        .unwrap();
    let event = JobFinishedEvent::decode(&told.envelope.payload[..]).unwrap();
    assert_eq!(event.job_id, reply.job_id);
    assert_eq!(event.client_instance_id, crate::INSTANCE);
    assert_eq!(event.sequence, 1);
    let answer: JobAnswer = ask_as(
        &stores.bus,
        topics::READ_JOB_ANSWER,
        "meridian.v1.ReadJobAnswerRequest",
        ReadJobAnswerRequest {
            job_id: reply.job_id.clone(),
        },
        &["ACC-1"],
    )
    .await
    .unwrap();
    assert_eq!(answer.provenance.unwrap().answer_hash, event.answer_hash);
    let caught_up: ListFinishedJobsReply = ask_as(
        &stores.bus,
        topics::LIST_FINISHED_JOBS,
        "meridian.v1.ListFinishedJobsRequest",
        ListFinishedJobsRequest { after_sequence: 0 },
        &["ACC-1"],
    )
    .await
    .unwrap();
    assert_eq!(caught_up.sequence, 1);
    assert_eq!(caught_up.finished[0].job_id, reply.job_id);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_routed_model_is_priced_by_its_plugin_once_per_batch_or_not_evaluated() {
    let stores = stores();
    stores.routing.lock().unwrap().push(ModelRouting {
        model_id: "quoted_price".into(),
        implementation: "sample-pricing-1".into(),
        ..Default::default()
    });
    coordinator(
        &stores,
        Config {
            price_deadline: Duration::from_secs(5),
            ..Config::default()
        },
        false,
    );
    // The pricing plugin: hears each batch, answers it as the built-in
    // model would, from exactly the market objects it was handed.
    let mut batches = stores.bus.subscribe_every(topics::PRICE_BATCH);
    let bus = Arc::clone(&stores.bus);
    let handed = Arc::new(Mutex::new(Vec::<PriceBatchRequest>::new()));
    let held = Arc::clone(&handed);
    tokio::spawn(async move {
        while let Some(delivery) = batches.recv().await {
            let batch = PriceBatchRequest::decode(&delivery.envelope.payload[..]).unwrap();
            held.lock().unwrap().push(batch.clone());
            let values = batch
                .units
                .iter()
                .map(|unit| {
                    let inputs: Vec<_> = unit
                        .inputs
                        .iter()
                        .map(|i| {
                            let m = i.value.as_ref().unwrap();
                            (
                                i.subject_id.clone(),
                                i.kind.clone(),
                                meridian_analytics_engine::exact::Dec::parse(
                                    &Exact::from_wire(m.amount.as_ref().unwrap())
                                        .unwrap()
                                        .to_string(),
                                )
                                .unwrap(),
                                m.instrument_id.clone(),
                            )
                        })
                        .collect();
                    let (value, held) = meridian_analytics_engine::model::value_per_unit(
                        &batch.model_id,
                        &unit.instrument_id,
                        &inputs,
                    )
                    .unwrap();
                    PricedUnit {
                        instrument_id: unit.instrument_id.clone(),
                        scenario_key: unit.scenario_key.clone(),
                        value_per_unit: money(&value.text(), &held, ""),
                    }
                })
                .collect();
            let reply = PriceBatchReply {
                batch_id: batch.batch_id.clone(),
                values,
                sensitivities: Vec::new(),
                pricing_instance_id: "sample-pricing-1".into(),
            };
            bus.call(
                topics::ANSWER_PRICE_BATCH,
                "meridian.v1.PriceBatchReply",
                reply.encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();
        }
    });
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        job(&[("market_value", &[], "")]),
        &["ACC-1"],
    )
    .await
    .unwrap();
    let answer = reply.answer.unwrap();
    // The same figure the built-in model gives: the seam's identity.
    assert_eq!(value(&answer, "market_value", &[]), "37311.25");
    let provenance = answer.provenance.unwrap();
    assert!(provenance
        .models
        .iter()
        .any(|m| m.model_id == "quoted_price" && m.implementation == "sample-pricing-1"));
    let handed = handed.lock().unwrap();
    assert_eq!(
        handed.len(),
        1,
        "one batch for the model, never one per instrument"
    );
    assert_eq!(handed[0].units.len(), 2);
    assert_eq!(handed[0].pricing_instance_id, "sample-pricing-1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_routed_plugin_that_does_not_answer_leaves_its_figures_not_evaluated() {
    let stores = stores();
    stores.routing.lock().unwrap().push(ModelRouting {
        model_id: "quoted_price".into(),
        implementation: "sample-pricing-1".into(),
        ..Default::default()
    });
    coordinator(
        &stores,
        Config {
            price_deadline: Duration::from_millis(50),
            ..Config::default()
        },
        false,
    );
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        job(&[("market_value", &[], "")]),
        &["ACC-1"],
    )
    .await
    .unwrap();
    let answer = reply.answer.unwrap();
    for instrument in [AAPL, BTC] {
        assert!(answer
            .not_evaluated
            .iter()
            .any(|u| u.instrument_id == instrument
                && u.reason == NotEvaluatedReason::ImplementationUnavailable as i32
                && u.detail == "sample-pricing-1"));
    }
    // What the built-in models price still answers: cash and the event.
    assert_eq!(value(&answer, "market_value", &[]), "5042.00");
}
