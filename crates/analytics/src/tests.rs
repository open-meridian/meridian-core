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
                ..Default::default()
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
        confidence: None,
        ..Default::default()
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
    /// Contract v28 (ruling 619): AAPL's close invalid, left out of a
    /// default read and answered to one naming alpaca-1:daily with
    /// include_quarantined.
    quarantined: Arc<Mutex<bool>>,
    /// The same, of the chain's deep call's quote in tradier-1:options.
    quarantined_quote: Arc<Mutex<bool>>,
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
        quarantined: Arc::default(),
        quarantined_quote: Arc::default(),
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
            // listed_option v1 binds its model by term (ruling 576; W1.24):
            // American on an equity the tree, European Black-Scholes.
            let bind = |style: ExerciseStyle, class: AssetClass, model: &str| ModelBinding {
                exercise_style: style as i32,
                underlying_asset_class: class as i32,
                model_id: model.into(),
                model_version: 1,
            };
            let model_bindings = if request.template_id == "TPL-listed-option" {
                vec![
                    bind(
                        ExerciseStyle::American,
                        AssetClass::Equity,
                        "american_binomial",
                    ),
                    bind(
                        ExerciseStyle::European,
                        AssetClass::Unspecified,
                        "black_scholes",
                    ),
                ]
            } else {
                Vec::new()
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
                        model_bindings,
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
    let quarantined = Arc::clone(&stores.quarantined);
    serve_store(
        &stores,
        topics::LIST_PRICES,
        "meridian.v1.ListPricesReply",
        move |request: ListPricesRequest| {
            let all = vec![
                price(AAPL, PriceKind::Close, "227.50", USD, "alpaca-1:daily"),
                price(BTC, PriceKind::Close, "60000.50", USDT, "coinbase-1:daily"),
                price(USDT, PriceKind::Close, "0.9998", USD, "coinbase-1:daily"),
                price(YES, PriceKind::Bid, "0.41", USD, "kalshi-1:quotes"),
                price(YES, PriceKind::Ask, "0.43", USD, "kalshi-1:quotes"),
                price(YES, PriceKind::Last, "0.42", USD, "kalshi-1:live"),
                price(NO, PriceKind::Last, "0.57", USD, "kalshi-1:live"),
                // Contract v27 (C4): AAPL's mid, for execution_cost.
                price(AAPL, PriceKind::Mid, "227.50", USD, "alpaca-1:quotes"),
            ];
            let wanted: Vec<&str> = request
                .subjects
                .iter()
                .map(|s| s.entity_id.as_str())
                .collect();
            let mut reply = ListPricesReply {
                prices: all
                    .into_iter()
                    .filter(|p| {
                        wanted.contains(&p.meta.as_ref().unwrap().subjects[0].entity_id.as_str())
                            && (request.kinds.is_empty() || request.kinds.contains(&p.kind))
                    })
                    .collect(),
                ..Default::default()
            };
            if *quarantined.lock().unwrap() && wanted.contains(&AAPL) {
                let sources = request.sources.clone().unwrap_or_default();
                let named = sources.include_quarantined
                    && sources.named == vec!["alpaca-1:daily".to_string()];
                for p in reply.prices.iter_mut() {
                    let meta = p.meta.as_mut().unwrap();
                    if meta.subjects[0].entity_id == AAPL && p.kind == PriceKind::Close as i32 {
                        meta.quality = QualityState::Invalid as i32;
                        meta.quality_reasons = vec![QualityReason::PriceOutlier as i32];
                    }
                }
                if named {
                    reply.prices.retain(|p| p.kind == PriceKind::Close as i32);
                } else {
                    reply
                        .prices
                        .retain(|p| p.meta.as_ref().unwrap().quality == 0);
                    reply.unanswered.push(Unanswered {
                        subject: Some(SubjectRef {
                            entity_id: AAPL.into(),
                            ..Default::default()
                        }),
                        dataset: "alpaca-1:daily".into(),
                        reason: UnansweredReason::SourceSilent as i32,
                        ..Default::default()
                    });
                }
            }
            reply
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
            business_date: String::new(),
            reporting_currency: String::new(),
            chains: Vec::new(),
            scenario_set: None,
            scenario_ref: None,
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

/// Contract v28 (W12.2; rulings 421 and 619): AAPL's only close in force is
/// invalid, left out of the job's default read; read again naming the
/// dataset with include_quarantined, it is not evaluated,
/// INPUT_QUARANTINED naming alpaca-1:daily, and never valued.
#[tokio::test(flavor = "multi_thread")]
async fn a_price_whose_only_row_in_force_is_invalid_is_not_evaluated_as_quarantined() {
    let stores = stores();
    *stores.quarantined.lock().unwrap() = true;
    coordinator(&stores, Config::default(), false);
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
    let quarantined: Vec<&NotEvaluated> = answer
        .not_evaluated
        .iter()
        .filter(|u| u.reason == NotEvaluatedReason::InputQuarantined as i32)
        .collect();
    assert!(!quarantined.is_empty(), "{:?}", answer.not_evaluated);
    assert!(quarantined
        .iter()
        .all(|u| u.instrument_id == AAPL && u.detail == "alpaca-1:daily"));
    // 5000 + 0.5 x 60000.50 x 0.9998 + 100 x 0.42: AAPL left out, never zero.
    assert_eq!(value(&answer, "market_value", &[]), "35036.25");
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
    // v26's eight, from contract v27 execution_cost v1 and pnl v2, and from
    // contract v28 the twelve options measures, positioning_exposure at a
    // version per basis (ruling 591), and from contract v29 the eleven
    // curve and bond measures (ruling 688).
    assert_eq!(definitions.definitions.len(), 33);
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
            ..ReadJobAnswerRequest::default()
        },
        &["ACC-1"],
    )
    .await
    .unwrap();
    assert!(
        answer.next_cursor.is_empty(),
        "within a page's budget: one page"
    );
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

/// Every page of a job's answer, read in turn from the first (ruling 794).
async fn read_every_page(
    bus: &Bus,
    job_id: &str,
    page_size: i32,
) -> Result<Vec<JobAnswer>, String> {
    let mut pages = Vec::new();
    let mut cursor = String::new();
    loop {
        let page: JobAnswer = ask_as(
            bus,
            topics::READ_JOB_ANSWER,
            "meridian.v1.ReadJobAnswerRequest",
            ReadJobAnswerRequest {
                job_id: job_id.to_string(),
                page_size,
                cursor: cursor.clone(),
            },
            &["ACC-1"],
        )
        .await?;
        cursor = page.next_cursor.clone();
        pages.push(page);
        if cursor.is_empty() {
            return Ok(pages);
        }
    }
}

fn reassembled(pages: &[JobAnswer]) -> JobAnswer {
    let mut whole = JobAnswer {
        job_id: pages[0].job_id.clone(),
        provenance: pages[0].provenance.clone(),
        ..JobAnswer::default()
    };
    for page in pages {
        assert_eq!(page.job_id, whole.job_id);
        assert_eq!(
            page.provenance, whole.provenance,
            "the provenance on every page"
        );
        whole.figures.extend(page.figures.iter().cloned());
        whole
            .not_evaluated
            .extend(page.not_evaluated.iter().cloned());
    }
    whole
}

fn paged_job() -> RunJobRequest {
    job(&[
        ("exposure", &["asset_class"], ""),
        ("concentration", &["instrument"], ""),
        ("market_value", &[], ""),
    ])
}

/// Contract v28's amendment (W12.4; ruling 794): a job's answer read one
/// page at a time reassembles equal to the answer given whole, each page
/// with the job's provenance; a cursor is the job's own.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_answer_is_read_in_pages_and_a_cursor_is_its_jobs_own() {
    let stores = stores();
    coordinator(&stores, Config::default(), false);
    let mut replies = Vec::new();
    for _ in 0..2 {
        let reply: RunJobReply = ask_as(
            &stores.bus,
            topics::RUN_JOB,
            "meridian.v1.RunJobRequest",
            paged_job(),
            &["ACC-1"],
        )
        .await
        .unwrap();
        assert!(reply.answered_inline);
        replies.push(reply);
    }
    let whole = replies[0].answer.clone().unwrap();
    assert!(whole.next_cursor.is_empty(), "an inline answer is whole");
    assert!(whole.figures.len() + whole.not_evaluated.len() > 2);
    let pages = read_every_page(&stores.bus, &replies[0].job_id, 1)
        .await
        .unwrap();
    assert_eq!(pages.len(), whole.figures.len() + whole.not_evaluated.len());
    assert_eq!(reassembled(&pages), whole);

    // The other job's cursor, read with this one: refused naming it.
    let other: JobAnswer = ask_as(
        &stores.bus,
        topics::READ_JOB_ANSWER,
        "meridian.v1.ReadJobAnswerRequest",
        ReadJobAnswerRequest {
            job_id: replies[1].job_id.clone(),
            page_size: 1,
            ..ReadJobAnswerRequest::default()
        },
        &["ACC-1"],
    )
    .await
    .unwrap();
    let refused = ask_as::<JobAnswer>(
        &stores.bus,
        topics::READ_JOB_ANSWER,
        "meridian.v1.ReadJobAnswerRequest",
        ReadJobAnswerRequest {
            job_id: replies[0].job_id.clone(),
            page_size: 1,
            cursor: other.next_cursor,
        },
        &["ACC-1"],
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("invalid-argument") && refused.contains("cursor"),
        "{refused}"
    );
}

/// Ruling 794: an answer past a page's budget is never given inline -- the
/// client is told it finished and reads it in pages -- and one that left
/// the cache between two pages is not held, said so.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_past_a_page_is_answered_later_and_one_gone_between_pages_is_not_held() {
    let whole = {
        let alone = stores();
        coordinator(&alone, Config::default(), false);
        let reply: RunJobReply = ask_as(
            &alone.bus,
            topics::RUN_JOB,
            "meridian.v1.RunJobRequest",
            paged_job(),
            &["ACC-1"],
        )
        .await
        .unwrap();
        reply.answer.unwrap()
    };
    let stores = stores();
    let mut finished = stores.bus.subscribe_every(topics::JOB_FINISHED);
    // A page's budget smaller than any answer: every page one entry.
    coordinator(
        &stores,
        Config {
            page_budget: 1,
            answers_kept: 1,
            ..Config::default()
        },
        false,
    );
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        paged_job(),
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
    let pages = read_every_page(&stores.bus, &reply.job_id, 0)
        .await
        .unwrap();
    assert_eq!(pages.len(), whole.figures.len() + whole.not_evaluated.len());
    let mut read = reassembled(&pages);
    assert_eq!(
        read.provenance.as_ref().unwrap().answer_hash,
        event.answer_hash
    );
    read.job_id = whole.job_id.clone();
    assert_eq!(read, whole);

    // Another job pushes this one's answer out of the cache (one kept):
    // its next page is not held, and said to be gone between pages.
    let first: JobAnswer = ask_as(
        &stores.bus,
        topics::READ_JOB_ANSWER,
        "meridian.v1.ReadJobAnswerRequest",
        ReadJobAnswerRequest {
            job_id: reply.job_id.clone(),
            ..ReadJobAnswerRequest::default()
        },
        &["ACC-1"],
    )
    .await
    .unwrap();
    let _: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        paged_job(),
        &["ACC-1"],
    )
    .await
    .unwrap();
    let gone = ask_as::<JobAnswer>(
        &stores.bus,
        topics::READ_JOB_ANSWER,
        "meridian.v1.ReadJobAnswerRequest",
        ReadJobAnswerRequest {
            job_id: reply.job_id.clone(),
            cursor: first.next_cursor,
            ..ReadJobAnswerRequest::default()
        },
        &["ACC-1"],
    )
    .await
    .unwrap_err();
    assert!(gone.contains("between pages"), "{gone}");
    assert!(gone.contains("[refusal-status not-found"), "{gone}");
}

/// The fixture's NOT_FOUND (fixtures/analytics/read-job-answer): an answer
/// no longer held, or never, and another instance's job alike -- the second
/// never told apart from the first, so nobody learns another's job exists.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_not_held_and_another_instances_job_are_both_not_found() {
    let stores = stores();
    let c = coordinator(&stores, Config::default(), false);
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        paged_job(),
        &["ACC-1"],
    )
    .await
    .unwrap();
    let as_instance = |instance: &str| meridian_bus::Envelope {
        meta: Some(meridian_bus::MessageMeta {
            publisher_instance_id: instance.into(),
            account_scope_applies: true,
            account_scope: vec!["ACC-1".into()],
            ..Default::default()
        }),
        payload_type: String::new(),
        payload: Vec::new(),
    };
    let read = |job_id: &str, instance: &str| {
        c.read_job_answer(
            ReadJobAnswerRequest {
                job_id: job_id.into(),
                ..ReadJobAnswerRequest::default()
            },
            &as_instance(instance),
        )
    };
    // Its own instance reads it.
    let own = stores.bus.instance_id().to_string();
    assert!(read(&reply.job_id, &own).is_ok());
    for (job_id, instance) in [
        (reply.job_id.as_str(), "another-instance-1"),
        ("JOB-01JAG0A1B2C30000000000009", own.as_str()),
    ] {
        let refused = read(job_id, instance).unwrap_err();
        let (status, fields, words) =
            meridian_bus::read_refusal_status(&refused).expect("a status");
        assert_eq!(status, meridian_bus::RefusalStatus::NotFound, "{refused}");
        assert_eq!(fields, vec!["job_id".to_string()]);
        assert!(!words.contains("another"), "never says whose: {words}");
    }
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

/// The price kind a record is marked at, from its package (the first
/// release's Q8): the first price named; a fund's close where it is listed
/// on a venue, its NAV where it is not or is a money-market fund.
#[test]
fn the_price_kind_is_the_packages_and_a_fund_reads_close_or_nav_by_its_listing() {
    use crate::resolve::price_kind_of;
    let inputs = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let plain = InstrumentRecord::default();
    assert_eq!(price_kind_of("cash", &inputs(&["fx"]), &plain), "");
    assert_eq!(
        price_kind_of("quoted_price", &inputs(&["close", "fx"]), &plain),
        "close"
    );
    assert_eq!(
        price_kind_of("binary_event", &inputs(&["last", "mid", "outcome"]), &plain),
        "last"
    );
    assert_eq!(
        price_kind_of("binary_event", &inputs(&["mid", "last", "outcome"]), &plain),
        "mid"
    );
    let fund = inputs(&["close", "nav"]);
    let etf = InstrumentRecord {
        exchange_mic: "ARCX".into(),
        ..Default::default()
    };
    assert_eq!(price_kind_of("quoted_price", &fund, &etf), "close");
    assert_eq!(price_kind_of("quoted_price", &fund, &plain), "nav");
    let money_market = InstrumentRecord {
        exchange_mic: "XNAS".into(),
        instrument_type: InstrumentType::MoneyMarketFund as i32,
        ..Default::default()
    };
    assert_eq!(price_kind_of("quoted_price", &fund, &money_market), "nav");
    assert_eq!(price_kind_of("quoted_price", &inputs(&[]), &plain), "close");
}

/// Contract v27 (W10.11, W11.17; ruling 510): an execution_cost job reads
/// the account's orders, placements and executions and each order's
/// history, as the client, and records a row per execution per benchmark in
/// the lake as the component; a fill answered don't know, or superseded, is
/// not measured.
#[tokio::test(flavor = "multi_thread")]
async fn an_execution_cost_job_reads_the_order_path_and_records_its_rows() {
    let stores = stores();
    let fill = |id: &str, state: ExecutionState, supersedes: &str| Execution {
        execution_id: id.into(),
        order_id: "ORD-1".into(),
        placement_id: "PLC-1".into(),
        account_id: "ACC-1".into(),
        instrument_id: AAPL.into(),
        side: Side::Buy as i32,
        quantity: d("10"),
        price: money("228.00", USD, "USD"),
        trade_time_ns: NOW - 3,
        trade_date: "2026-10-16".into(),
        fees: vec![Fee {
            kind: 0,
            amount: money("1.00", USD, "USD"),
            capitalised: false,
        }],
        venue_id: "VEN-1".into(),
        liquidity_indicator: LiquidityIndicator::Maker as i32,
        state: state as i32,
        supersedes: supersedes.into(),
        ..Default::default()
    };
    serve_store(
        &stores,
        topics::LIST_ORDERS,
        "meridian.v1.ListOrdersReply",
        move |request: ListOrdersRequest| ListOrdersReply {
            orders: vec![Order {
                order_id: "ORD-1".into(),
                account_id: request.account_id.clone(),
                instrument_id: AAPL.into(),
                side: Side::Buy as i32,
                ..Default::default()
            }],
            placements: vec![Placement {
                placement_id: "PLC-1".into(),
                order_id: "ORD-1".into(),
                account_id: request.account_id.clone(),
                routed_at_ns: NOW - 4,
                routed_precision: TimePrecision::Nanosecond as i32,
                ..Default::default()
            }],
            executions: vec![
                fill("EXE-1", ExecutionState::Matched, ""),
                fill("EXE-2", ExecutionState::DontKnow, ""),
                fill("EXE-3", ExecutionState::Matched, ""),
                fill("EXE-4", ExecutionState::Matched, "EXE-3"),
            ],
            ..Default::default()
        },
    );
    serve_store(
        &stores,
        topics::ORDER_HISTORY,
        "meridian.v1.OrderHistoryReply",
        |_request: OrderHistoryRequest| OrderHistoryReply {
            entries: vec![
                EntryMeta {
                    kind: "order-proposed".into(),
                    event_time_ns: NOW - 1_000,
                    ..Default::default()
                },
                EntryMeta {
                    kind: "order-released".into(),
                    event_time_ns: NOW - 5,
                    ..Default::default()
                },
            ],
            next_cursor: String::new(),
        },
    );
    let recorded: Arc<Mutex<Vec<ExecutionCost>>> = Arc::default();
    let held = Arc::clone(&recorded);
    serve_store(
        &stores,
        topics::RECORD_EXECUTION_COSTS,
        "meridian.v1.RecordObservationsReply",
        move |request: RecordExecutionCostsRequest| {
            held.lock().unwrap().extend(request.costs);
            RecordObservationsReply::default()
        },
    );
    coordinator(&stores, Config::default(), false);
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        job(&[("execution_cost", &[], "")]),
        &["ACC-1"],
    )
    .await
    .unwrap();
    assert!(reply.answered_inline);
    let rows = recorded.lock().unwrap().clone();
    // EXE-1 and EXE-4 (EXE-4 superseding EXE-3), four benchmarks each; the
    // DK'd fill and the superseded one not measured.
    let measured: std::collections::BTreeSet<&str> =
        rows.iter().map(|r| r.execution_id.as_str()).collect();
    assert_eq!(measured, ["EXE-1", "EXE-4"].into_iter().collect());
    assert_eq!(rows.len(), 8);
    let row = |execution: &str, benchmark: TcaBenchmark| {
        rows.iter()
            .find(|r| r.execution_id == execution && r.benchmark == benchmark as i32)
            .unwrap_or_else(|| panic!("no {execution} row for {benchmark:?}"))
            .clone()
    };
    // The proposal before the mid was in force: no quote, never imputed.
    let decision = row("EXE-1", TcaBenchmark::Decision);
    assert_eq!(decision.unanswered, TcaUnanswered::NoQuote as i32);
    assert_eq!(decision.benchmark_at_ns, NOW - 1_000);
    assert!(decision.cost_bps.is_none());
    // At its release: s x (228.00 - 227.50) / 227.50 x 10^4.
    let arrival = row("EXE-1", TcaBenchmark::Arrival);
    assert_eq!(arrival.benchmark_at_ns, NOW - 5);
    assert_eq!(
        Exact::from_wire(arrival.cost_bps.as_ref().unwrap())
            .unwrap()
            .to_string(),
        "21.9780"
    );
    assert_eq!(
        Exact::from_wire(arrival.fees_bps.as_ref().unwrap())
            .unwrap()
            .to_string(),
        "4.3860"
    );
    assert_eq!(arrival.benchmark_dataset, "alpaca-1:quotes");
    assert_eq!(
        row("EXE-1", TcaBenchmark::PlacementArrival).benchmark_at_ns,
        NOW - 4
    );
    assert_eq!(row("EXE-1", TcaBenchmark::MidAtFill).time_to_fill_ns, 1);
    // Under its execution's instrument, dated by its trade date.
    let meta = arrival.meta.unwrap();
    assert_eq!(meta.subjects[0].entity_id, AAPL);
    assert_eq!(meta.business_date, "2026-10-16");
    assert_eq!(
        arrival.liquidity_indicator,
        LiquidityIndicator::Maker as i32
    );
    // The orders and their history read as the client, within its scope.
    let heard = stores.heard.lock().unwrap();
    for topic in [topics::LIST_ORDERS, topics::ORDER_HISTORY] {
        let (_, meta) = heard
            .stamps
            .iter()
            .find(|(t, _)| t == topic)
            .unwrap_or_else(|| panic!("{topic} not asked"));
        assert!(meta.account_scope_applies, "{topic}");
        assert_eq!(meta.account_scope, vec!["ACC-1".to_string()], "{topic}");
    }
}

// ── Contract v28: a job over a chain, and a grid on a saved scenario set ────

const OSI_CALL: &str = "AAPL  261120C00230000";
const OSI_PUT: &str = "AAPL  261120P00230000";
const OSI_DEEP: &str = "AAPL  261120C00150000";
const VENUE: &str = "VEN-XCBO";

fn series_row(osi: &str, right: &str, strike: &str) -> ListedSeries {
    ListedSeries {
        meta: Some(ObservationMeta {
            row_key: osi.into(),
            subjects: vec![SubjectRef {
                entity_id: AAPL.into(),
                ..Default::default()
            }],
            series_key: vec![Identifier {
                scheme: "occ_osi".into(),
                value: osi.into(),
                ..Default::default()
            }],
            source: Some(Source {
                dataset: "tradier-1:options".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        template_id: "TPL-listed-option".into(),
        template_version: "1".into(),
        fields: [
            ("underlying_instrument_id", AAPL.to_string()),
            ("put_or_call", format!("PUT_OR_CALL_{right}")),
            ("strike_price", format!("{strike} USD")),
            ("expiration_date", "2026-11-20".into()),
            ("exercise_style", "EXERCISE_STYLE_AMERICAN".into()),
            ("contract_multiplier", "100".into()),
        ]
        .into_iter()
        .map(|(path, value)| TemplateFieldValue {
            path: path.into(),
            value,
            ..Default::default()
        })
        .collect(),
        instrument_id: String::new(),
    }
}

fn quote_row(osi: &str, bid: &str, ask: &str) -> Quote {
    Quote {
        meta: Some(ObservationMeta {
            series_key: vec![Identifier {
                scheme: "occ_osi".into(),
                value: osi.into(),
                ..Default::default()
            }],
            source: Some(Source {
                dataset: "tradier-1:options".into(),
                venue_id: VENUE.into(),
                ..Default::default()
            }),
            valid_from_ns: NOW - 1_000,
            ..Default::default()
        }),
        bid: money(bid, USD, "USD"),
        ask: money(ask, USD, "USD"),
        ..Default::default()
    }
}

fn chain_stores() -> Stores {
    chain_stores_with(true)
}

/// Treasury's par curve, as the platform seeds it and the rates slice
/// records it (rulings 608 and 611): held, or not.
const UST: &str = "INS-UST-PAR";

fn par(count: i32, unit: TenorUnit, percent: &str) -> Rate {
    Rate {
        meta: Some(ObservationMeta {
            source: Some(Source {
                dataset: "us-treasury-1:ust_par".into(),
                ..Default::default()
            }),
            valid_from_ns: NOW - 86_400_000_000_000,
            ..Default::default()
        }),
        kind: RateKind::ParYield as i32,
        value: Some(UnitValue {
            value: d(percent),
            ..Default::default()
        }),
        tenor: Some(Tenor {
            count,
            unit: unit as i32,
        }),
        ..Default::default()
    }
}

fn chain_stores_with(curve_held: bool) -> Stores {
    chain_stores_paying(curve_held, Vec::new())
}

/// The chain's stores, the lake holding these declared dividends of the
/// underlying (contract v28, the filings slice; W10.6).
fn chain_stores_paying(
    curve_held: bool,
    dividends: Vec<meridian_domain::v1::CorporateActionAnnounced>,
) -> Stores {
    let stores = stores();
    let quarantined = Arc::clone(&stores.quarantined_quote);
    serve_store(
        &stores,
        topics::RESOLVE_IDENTIFIER,
        "meridian.v1.ResolveIdentifierReply",
        move |request: ResolveIdentifierRequest| {
            let named = &request.identifiers[0];
            assert_eq!(
                (
                    named.scheme.as_str(),
                    named.value.as_str(),
                    named.source.as_str()
                ),
                (
                    "symbol",
                    "Daily Treasury Par Yield Curve Rates",
                    "ustreasury"
                ),
                "the default curve, by its administrator's symbol"
            );
            ResolveIdentifierReply {
                found: curve_held,
                instrument_id: if curve_held {
                    UST.into()
                } else {
                    String::new()
                },
                ..Default::default()
            }
        },
    );
    serve_store(
        &stores,
        topics::LIST_RATES,
        "meridian.v1.ListRatesReply",
        |request: ListRatesRequest| {
            assert_eq!(request.subjects[0].entity_id, UST);
            assert_eq!(request.kinds, vec![RateKind::ParYield as i32]);
            ListRatesReply {
                rates: vec![
                    par(1, TenorUnit::Month, "4.20"),
                    par(3, TenorUnit::Month, "4.25"),
                    par(1, TenorUnit::Year, "4.10"),
                ],
                ..Default::default()
            }
        },
    );
    serve_store(
        &stores,
        topics::LIST_SERIES,
        "meridian.v1.ListSeriesReply",
        |_request: ListSeriesRequest| ListSeriesReply {
            series: vec![
                series_row(OSI_CALL, "CALL", "230.00"),
                series_row(OSI_PUT, "PUT", "230.00"),
                series_row(OSI_DEEP, "CALL", "150.00"),
            ],
            ..Default::default()
        },
    );
    serve_store(
        &stores,
        topics::LIST_QUOTES,
        "meridian.v1.ListQuotesReply",
        move |request: ListQuotesRequest| {
            assert!(
                request.all_series,
                "a chain with no keys reads every series"
            );
            let mut reply = ListQuotesReply {
                quotes: vec![
                    quote_row(OSI_CALL, "8.10", "8.30"),
                    quote_row(OSI_PUT, "9.40", "9.60"),
                    // Under its intrinsic of 77.50 on every side.
                    quote_row(OSI_DEEP, "70.00", "71.00"),
                ],
                ..Default::default()
            };
            // Contract v28 (ruling 619): the deep call's quote invalid,
            // left out of a default read, the dataset source silent; and
            // answered with its state to a read naming it with
            // include_quarantined.
            if *quarantined.lock().unwrap() {
                let sources = request.sources.clone().unwrap_or_default();
                if sources.include_quarantined {
                    assert_eq!(sources.named, vec!["tradier-1:options".to_string()]);
                    let deep = reply.quotes[2].meta.as_mut().unwrap();
                    deep.quality = QualityState::Invalid as i32;
                    deep.quality_reasons = vec![QualityReason::CrossedQuote as i32];
                } else {
                    reply.quotes.truncate(2);
                    reply.unanswered.push(Unanswered {
                        subject: Some(SubjectRef {
                            entity_id: AAPL.into(),
                            ..Default::default()
                        }),
                        dataset: "tradier-1:options".into(),
                        reason: UnansweredReason::SourceSilent as i32,
                        ..Default::default()
                    });
                }
            }
            reply
        },
    );
    serve_store(
        &stores,
        topics::LIST_STATISTICS,
        "meridian.v1.ListStatisticsReply",
        |_request: ListStatisticsRequest| ListStatisticsReply::default(),
    );
    serve_store(
        &stores,
        topics::LIST_CORPORATE_ACTIONS,
        "meridian.v1.ListCorporateActionsReply",
        move |request: ListCorporateActionsRequest| {
            assert_eq!(request.subjects[0].entity_id, AAPL);
            ListCorporateActionsReply {
                actions: dividends.clone(),
                ..Default::default()
            }
        },
    );
    // The lake's listing (ruling 767): the options dataset's declaration,
    // which a read's answer leaves unset.
    serve_store(
        &stores,
        topics::LIST_DATASETS,
        "meridian.v1.ListDatasetsReply",
        |_request: meridian_domain::v1::ListDatasetsRequest| {
            meridian_domain::v1::ListDatasetsReply {
                datasets: vec![meridian_domain::v1::DatasetRef {
                    dataset: "tradier-1:options".into(),
                    instance: "tradier-1".into(),
                    declaration: Some(meridian_pb::v1::DatasetDeclaration {
                        key: "options".into(),
                        day_time_zone: "America/New_York".into(),
                        day_end_minute: 960,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }
        },
    );
    serve_store(
        &stores,
        topics::READ_CALENDAR,
        "meridian.v1.ReadCalendarReply",
        |request: ReadCalendarRequest| ReadCalendarReply {
            // The venue's trading sessions, found through the quote's venue
            // (ruling 570 (i)).
            found: request.calendar_id == VENUE,
            calendar_id: request.calendar_id,
            trading_sessions: Some(TradingSessions {
                time_zone: "America/New_York".into(),
                sessions: vec![TradingSession {
                    weekdays: (1..=5).collect(),
                    start_minute: 570,
                    end_minute: 960,
                    extended: false,
                    ..Default::default()
                }],
                early_closes: Vec::new(),
            }),
            ..Default::default()
        },
    );
    stores
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_job_answers_iv_and_greeks_naming_each_series_by_its_key() {
    let stores = chain_stores();
    coordinator(&stores, Config::default(), false);
    let mut request = job(&[
        ("implied_volatility", &["series", "quote_side"], ""),
        ("greeks", &["series", "greek"], ""),
    ]);
    let asked = request.job.as_mut().unwrap();
    asked.account_ids.clear();
    asked.chains = vec![ChainRef {
        underlying_instrument_id: AAPL.into(),
        expiration_from: "2026-11-20".into(),
        expiration_to: "2026-11-20".into(),
        series_keys: Vec::new(),
    }];
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        request,
        &[],
    )
    .await
    .unwrap();
    let answer = reply.answer.unwrap();
    let ivs: Vec<&Figure> = answer
        .figures
        .iter()
        .filter(|f| f.measure.as_ref().unwrap().definition == "implied_volatility")
        .collect();
    assert_eq!(ivs.len(), 6, "{:?}", answer.not_evaluated);
    for f in &ivs {
        assert_eq!(f.computed_as, ComputedAs::Model as i32);
        assert_eq!(f.series_key[0].scheme, "occ_osi");
        let iv: f64 = Exact::from_wire(f.value.as_ref().unwrap())
            .unwrap()
            .to_string()
            .parse()
            .unwrap();
        assert!(iv > 0.05 && iv < 1.5, "{iv}");
    }
    let below: Vec<&NotEvaluated> = answer
        .not_evaluated
        .iter()
        .filter(|u| u.reason == NotEvaluatedReason::BelowIntrinsic as i32)
        .collect();
    // The deep call's three sides for IV, and its greeks, which need a mid.
    assert_eq!(below.len(), 4);
    assert!(below.iter().all(|u| u.series_key[0].value == OSI_DEEP));
    let greeks = answer
        .figures
        .iter()
        .filter(|f| f.measure.as_ref().unwrap().definition == "greeks")
        .count();
    assert_eq!(greeks, 14, "two series' seven greeks");
    let provenance = answer.provenance.unwrap();
    assert!(provenance
        .models
        .iter()
        .any(|m| m.model_id == "american_binomial"));
    assert!(provenance
        .datasets
        .contains(&"tradier-1:options".to_string()));
    // The lake's listing read once for the job, on the client's behalf
    // (ruling 767), so within its entitlements.
    let heard = stores.heard.lock().unwrap();
    let listed: Vec<&MessageMeta> = heard
        .stamps
        .iter()
        .filter(|(t, _)| t == topics::LIST_DATASETS)
        .map(|(_, meta)| meta)
        .collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].on_behalf_of_instance_id, crate::INSTANCE);
    assert!(listed[0].account_scope_applies);
}

/// Contract v28 (W12.2; rulings 421 and 619): a series whose only quote in
/// force is invalid is not evaluated, INPUT_QUARANTINED naming the dataset;
/// the others are answered as before.
#[tokio::test(flavor = "multi_thread")]
async fn a_series_whose_only_quote_in_force_is_invalid_is_not_evaluated_as_quarantined() {
    let stores = chain_stores();
    *stores.quarantined_quote.lock().unwrap() = true;
    coordinator(&stores, Config::default(), false);
    let mut request = job(&[("implied_volatility", &["series", "quote_side"], "")]);
    let asked = request.job.as_mut().unwrap();
    asked.account_ids.clear();
    asked.chains = vec![ChainRef {
        underlying_instrument_id: AAPL.into(),
        expiration_from: "2026-11-20".into(),
        expiration_to: "2026-11-20".into(),
        series_keys: Vec::new(),
    }];
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        request,
        &[],
    )
    .await
    .unwrap();
    let answer = reply.answer.unwrap();
    let quarantined: Vec<&NotEvaluated> = answer
        .not_evaluated
        .iter()
        .filter(|u| u.reason == NotEvaluatedReason::InputQuarantined as i32)
        .collect();
    assert!(!quarantined.is_empty(), "{:?}", answer.not_evaluated);
    assert!(quarantined
        .iter()
        .all(|u| u.series_key[0].value == OSI_DEEP && u.detail == "tradier-1:options"));
    let ivs = answer
        .figures
        .iter()
        .filter(|f| f.measure.as_ref().unwrap().definition == "implied_volatility")
        .count();
    assert_eq!(ivs, 6, "{:?}", answer.not_evaluated);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grid_on_a_saved_set_answers_each_point_and_an_inline_set_beside_it_is_refused() {
    let stores = chain_stores();
    let spot = |value: &str| Shock {
        object: ShockObject::Spot as i32,
        target: Some(ShockTarget {
            subject_id: String::new(),
            every: true,
        }),
        form: ShockForm::Relative as i32,
        unit: ShockUnit::Percent as i32,
        value: d(value),
        buckets: Vec::new(),
    };
    let set = ScenarioSet {
        name: "price by date".into(),
        axes: vec![ScenarioAxis {
            name: "spot".into(),
            steps: vec![
                Scenario {
                    name: "-10%".into(),
                    shocks: vec![spot("-10")],
                },
                Scenario {
                    name: "+10%".into(),
                    shocks: vec![spot("10")],
                },
            ],
        }],
    };
    let saved = set.clone();
    serve_store(
        &stores,
        topics::LIST_SCENARIOS,
        "meridian.v1.ListScenariosReply",
        move |request: ListScenariosRequest| ListScenariosReply {
            scenarios: vec![SavedScenario {
                meta: Some(ObservationMeta {
                    row_key: request.row_keys[0].clone(),
                    version: 1,
                    source: Some(Source {
                        dataset: "sample-options-1:scenarios".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                scenario_set: Some(saved.clone()),
                ..Default::default()
            }],
        },
    );
    coordinator(&stores, Config::default(), false);
    let mut request = job(&[("scenario_grid", &["instrument"], "")]);
    request.job.as_mut().unwrap().chains = vec![ChainRef {
        underlying_instrument_id: AAPL.into(),
        ..Default::default()
    }];
    request.job.as_mut().unwrap().scenario_ref = Some(ScenarioRef {
        row_key: "grid:price-by-date".into(),
        dataset: "sample-options-1:scenarios".into(),
        version: 1,
    });
    let run = |request: RunJobRequest| async {
        ask_as::<RunJobReply>(
            &stores.bus,
            topics::RUN_JOB,
            "meridian.v1.RunJobRequest",
            request,
            &["ACC-1"],
        )
        .await
    };
    let first = run(request.clone()).await.unwrap().answer.unwrap();
    let grid: Vec<&Figure> = first
        .figures
        .iter()
        .filter(|f| f.measure.as_ref().unwrap().definition == "scenario_grid")
        .collect();
    // AAPL's positions held, each at the two points; the rest are in no
    // chain the job reads, each not evaluated saying so.
    assert_eq!(grid.len() % 2, 0, "{:?}", first.not_evaluated);
    assert!(!grid.is_empty() && grid.iter().all(|f| f.breakdown[0].value == AAPL));
    assert!(first
        .not_evaluated
        .iter()
        .any(|u| u.instrument_id == BTC && u.reason == NotEvaluatedReason::PriceMissing as i32));
    let key = grid[0].scenario_key.as_ref().unwrap();
    assert_eq!(key.kind, ScenarioKind::ScenarioPoint as i32);
    assert_eq!(key.scenario_row_key, "grid:price-by-date");
    assert_eq!(key.scenario_version, 1);
    // The same saved version run twice answers the same hash.
    let second = run(request.clone()).await.unwrap().answer.unwrap();
    assert_eq!(
        first.provenance.unwrap().answer_hash,
        second.provenance.unwrap().answer_hash
    );
    request.job.as_mut().unwrap().scenario_set = Some(set);
    let refused = run(request).await.unwrap_err();
    assert!(refused.contains("scenario_ref"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_reads_treasurys_curve_by_its_symbol_and_none_held_is_not_evaluated() {
    // Rulings 608 and 611 and the fourth amendment: the default curve,
    // ust_cmt, resolved read-only by its administrator's symbol, its par
    // yields read at the as-of and named in the provenance.
    for held in [true, false] {
        let stores = chain_stores_with(held);
        coordinator(&stores, Config::default(), false);
        let mut request = job(&[("implied_volatility", &["series", "quote_side"], "")]);
        let asked = request.job.as_mut().unwrap();
        asked.account_ids.clear();
        asked.chains = vec![ChainRef {
            underlying_instrument_id: AAPL.into(),
            expiration_from: "2026-11-20".into(),
            expiration_to: "2026-11-20".into(),
            series_keys: Vec::new(),
        }];
        let reply: RunJobReply = ask_as(
            &stores.bus,
            topics::RUN_JOB,
            "meridian.v1.RunJobRequest",
            request,
            &[],
        )
        .await
        .unwrap();
        let answer = reply.answer.unwrap();
        if held {
            assert!(!answer.figures.is_empty(), "{:?}", answer.not_evaluated);
            assert!(answer
                .provenance
                .unwrap()
                .datasets
                .contains(&"us-treasury-1:ust_par".to_string()));
        } else {
            assert!(answer.figures.is_empty());
            assert!(answer.not_evaluated.iter().all(|u| {
                u.reason == NotEvaluatedReason::PriceMissing as i32 && u.detail.contains("ust_cmt")
            }));
        }
    }
}

/// A declared cash dividend of the underlying, as the lake answers it.
fn declared(ex_date: &str, amount: &str) -> meridian_domain::v1::CorporateActionAnnounced {
    meridian_domain::v1::CorporateActionAnnounced {
        meta: Some(ObservationMeta {
            row_key: format!("dividend:{ex_date}"),
            subjects: vec![SubjectRef {
                entity_id: AAPL.into(),
                ..Default::default()
            }],
            source: Some(Source {
                dataset: "sec-edgar-1:dividends".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        kind: meridian_domain::v1::CorporateActionKind::CashDividend as i32,
        amount_per_share: money(amount, USD, "USD"),
        declared_date: "2026-10-01".into(),
        ex_date: ex_date.into(),
        record_date: ex_date.into(),
        payment_date: "2026-11-27".into(),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dividend_going_ex_the_day_before_expiry_moves_an_american_calls_price() {
    // Contract v28 (W12.2; the filings slice's rows, read tolerantly since
    // pass 2): the tree takes the lake's declared dividends, escrowed, and
    // an American call worth exercising before the ex-date is priced so.
    // The same quotes then solve to another implied volatility: the
    // model's price at any one volatility moved. Which way is the chain's
    // to say, since the forward and borrow come from put-call parity with
    // the dividends' present value taken out (ruling 608).
    let iv_of = |dividends: Vec<meridian_domain::v1::CorporateActionAnnounced>| async move {
        let stores = chain_stores_paying(true, dividends);
        coordinator(&stores, Config::default(), false);
        let mut request = job(&[("implied_volatility", &["series", "quote_side"], "")]);
        let asked = request.job.as_mut().unwrap();
        asked.account_ids.clear();
        asked.chains = vec![ChainRef {
            underlying_instrument_id: AAPL.into(),
            expiration_from: "2026-11-20".into(),
            expiration_to: "2026-11-20".into(),
            series_keys: Vec::new(),
        }];
        let reply: RunJobReply = ask_as(
            &stores.bus,
            topics::RUN_JOB,
            "meridian.v1.RunJobRequest",
            request,
            &[],
        )
        .await
        .unwrap();
        let answer = reply.answer.unwrap();
        let mid = answer
            .figures
            .iter()
            .find(|f| {
                f.series_key.first().is_some_and(|k| k.value == OSI_CALL)
                    && f.breakdown.iter().any(|b| b.value == "mid")
            })
            .unwrap_or_else(|| panic!("{:?}", answer.not_evaluated));
        let iv: f64 = Exact::from_wire(mid.value.as_ref().unwrap())
            .unwrap()
            .to_string()
            .parse()
            .unwrap();
        (iv, answer.provenance.unwrap().datasets)
    };
    let (none, _) = iv_of(Vec::new()).await;
    let (paying, datasets) = iv_of(vec![declared("2026-11-19", "3.00")]).await;
    assert!(
        (paying - none).abs() > 1e-4,
        "the dividend moved nothing: {none} and {paying}"
    );
    assert!(datasets.contains(&"sec-edgar-1:dividends".to_string()));
    // Going ex after expiry, it is no dividend of this option's.
    let (after, _) = iv_of(vec![declared("2026-11-23", "3.00")]).await;
    assert!((after - none).abs() < 1e-9, "{none} and {after}");
}

// ── Contract v29: the curve and bond measures (W12.1, W12.2) ───────────────

const BOND: &str = "INS-BOND1";
const POOL: &str = "INS-POOL1";
const SIFMA: &str = "CAL-SIFMA";

/// The stores for a bond fund: a corporate bond and an agency pool held,
/// their closes in percent of par, the pool's factors, Treasury's par
/// curve and the SIFMA calendar.
fn bond_stores() -> Stores {
    let bus = Arc::new(Bus::single(
        crate::INSTANCE,
        Arc::new(MemoryBackend::new()),
        Arc::new(ManualClock::at(NOW)),
    ));
    let stores = Stores {
        bus,
        heard: Arc::default(),
        routing: Arc::new(Mutex::new(Vec::new())),
        quarantined: Arc::default(),
        quarantined_quote: Arc::default(),
    };
    serve_store(
        &stores,
        topics::LIST_ACCOUNT_ATTRIBUTES,
        "meridian.v1.ListAccountAttributesReply",
        |request: ListAccountAttributesRequest| ListAccountAttributesReply {
            attributes: vec![AccountAttributes {
                account_id: request.account_id,
                base_currency_code: "USD".into(),
                base_cash_instrument_id: USD.into(),
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
            let position = |id: &str, quantity: &str, cost: &str| BookPosition {
                account_id: request.account_id.clone(),
                instrument_id: id.into(),
                side: HoldingSide::Long as i32,
                trade_date_quantity: d(quantity),
                lots: vec![Lot {
                    open_quantity: d(quantity),
                    terms: Some(LotTerms {
                        cost: money(cost, USD, "USD"),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            };
            ListPositionsReply {
                positions: vec![
                    position(BOND, "50000", "49187.50"),
                    position(POOL, "1000000", "1015000.00"),
                ],
                ..Default::default()
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
                BOND => Some(InstrumentRecord {
                    instrument_type: InstrumentType::Bond as i32,
                    bond: Some(
                        meridian_domain::debt::bond_from(
                            &serde_json::json!({
                                "security_type": "DEBT_SECURITY_TYPE_CORP",
                                "issuer_type": "ISSUER_TYPE_CORPORATE",
                                "issue_date": "2026-02-23", "dated_date": "2026-02-23",
                                "maturity_date": "2046-02-23", "coupon_rate": "4.65",
                                "coupon_type": "COUPON_TYPE_FIXED", "coupon_frequency": 2,
                                "day_count": "DAY_COUNT_THIRTY_360_BOND_BASIS",
                                "payment_calendar_id": SIFMA,
                            }),
                            "bond",
                        )
                        .unwrap(),
                    ),
                    ..record(BOND, AssetClass::Debt, "USD", "TPL-bond")
                }),
                POOL => Some(InstrumentRecord {
                    instrument_type: InstrumentType::MbsPool as i32,
                    mbs_pool: Some(
                        meridian_domain::debt::mbs_pool_from(
                            &serde_json::json!({
                                "agency": "MORTGAGE_AGENCY_FNMA", "program": "UMBS 30-year",
                                "pool_number": "MA0001", "pass_through_rate": "6",
                                "issue_date": "2025-01-01", "maturity_date": "2055-01-01",
                                "original_face": "1000000", "original_term_months": 360,
                                "day_count": "DAY_COUNT_THIRTY_360_BOND_BASIS",
                                "payment_delay_days": 55, "payment_day": 25,
                            }),
                            "mbs_pool",
                        )
                        .unwrap(),
                    ),
                    ..record(POOL, AssetClass::Debt, "USD", "TPL-mbs-pool")
                }),
                UST => Some(record(UST, AssetClass::Unspecified, "USD", "")),
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
                "TPL-mbs-pool" => ("bond_cash_flows", &["close", "factor"]),
                _ => ("bond_cash_flows", &["close"]),
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
        topics::LIST_PRICES,
        "meridian.v1.ListPricesReply",
        |request: ListPricesRequest| {
            let par = |subject: &str, amount: &str| Price {
                basis: PriceBasis::PercentOfPar as i32,
                ..price(subject, PriceKind::Close, amount, USD, "bonds-1:closes")
            };
            let wanted: Vec<&str> = request
                .subjects
                .iter()
                .map(|s| s.entity_id.as_str())
                .collect();
            ListPricesReply {
                prices: [par(BOND, "90.5"), par(POOL, "101.5")]
                    .into_iter()
                    .filter(|p| {
                        wanted.contains(&p.meta.as_ref().unwrap().subjects[0].entity_id.as_str())
                    })
                    .collect(),
                ..Default::default()
            }
        },
    );
    serve_store(
        &stores,
        crate::debt::LIST_FACTORS,
        "meridian.v1.ListFactorsReply",
        |_request: ListFactorsRequest| {
            let row = |month: &str, factor: &str| Factor {
                meta: Some(ObservationMeta {
                    subjects: vec![SubjectRef {
                        entity_id: POOL.into(),
                        ..Default::default()
                    }],
                    source: Some(Source {
                        dataset: "gse-1:factors".into(),
                        ..Default::default()
                    }),
                    row_key: format!("{POOL}:{month}"),
                    version: 1,
                    valid_from_ns: NOW - 10,
                    ..Default::default()
                }),
                factor: d(factor),
                factor_month: month.into(),
                ..Default::default()
            };
            ListFactorsReply {
                factors: vec![
                    row("202606", "0.9501"),
                    row("202607", "0.9452"),
                    row("202608", "0.9398"),
                    row("202609", "0.9351"),
                    // Not yet in force at the job's date.
                    row("202611", "0.9200"),
                ],
                ..Default::default()
            }
        },
    );
    serve_store(
        &stores,
        topics::RESOLVE_IDENTIFIER,
        "meridian.v1.ResolveIdentifierReply",
        |_request: ResolveIdentifierRequest| ResolveIdentifierReply {
            found: true,
            instrument_id: UST.into(),
            ..Default::default()
        },
    );
    serve_store(
        &stores,
        topics::LIST_RATES,
        "meridian.v1.ListRatesReply",
        |_request: ListRatesRequest| ListRatesReply {
            rates: vec![
                par(3, TenorUnit::Month, "4.26"),
                par(1, TenorUnit::Year, "4.46"),
                par(2, TenorUnit::Year, "4.74"),
                par(5, TenorUnit::Year, "4.89"),
                par(10, TenorUnit::Year, "5.09"),
                par(30, TenorUnit::Year, "5.43"),
            ],
            ..Default::default()
        },
    );
    serve_store(
        &stores,
        topics::READ_CALENDAR,
        "meridian.v1.ReadCalendarReply",
        |request: ReadCalendarRequest| ReadCalendarReply {
            found: request.calendar_id == SIFMA,
            calendar_id: request.calendar_id,
            days: vec![CalendarDay {
                date: "2026-11-11".into(),
                kind: CalendarDayKind::Closed as i32,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    serve_store(
        &stores,
        topics::LIST_OUTCOMES,
        "meridian.v1.ListOutcomesReply",
        |_request: ListOutcomesRequest| ListOutcomesReply::default(),
    );
    serve_store(
        &stores,
        topics::LIST_MODEL_ROUTING,
        "meridian.v1.ListModelRoutingReply",
        |_request: ListModelRoutingRequest| ListModelRoutingReply {
            version: 1,
            routes: Vec::new(),
        },
    );
    stores
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bond_funds_job_reads_its_debt_inputs_on_the_clients_behalf() {
    let stores = bond_stores();
    coordinator(&stores, Config::default(), false);
    let mut request = job(&[
        ("market_value", &["instrument"], ""),
        ("accrued_interest", &[], ""),
        ("bond_yield", &[], ""),
        ("bond_spread", &[], ""),
        ("historical_prepayment", &[], ""),
        ("treasury_curve", &[], ""),
    ]);
    request.job.as_mut().unwrap().business_date = "2026-10-16".into();
    let reply: RunJobReply = ask_as(
        &stores.bus,
        topics::RUN_JOB,
        "meridian.v1.RunJobRequest",
        request,
        &["ACC-1"],
    )
    .await
    .unwrap();
    let answer = reply.answer.expect("answered inline");
    // 50,000 of face at 90.5; 1,000,000 of original face at 101.5 times
    // the factor in force on 2026-10-16, September's 0.9351.
    assert_eq!(
        value(&answer, "market_value", &[("instrument", BOND)]),
        "45250.00"
    );
    assert_eq!(
        value(&answer, "market_value", &[("instrument", POOL)]),
        "949126.50"
    );
    let has = |definition: &str| {
        answer
            .figures
            .iter()
            .any(|f| f.measure.as_ref().unwrap().definition == definition)
    };
    for definition in [
        "accrued_interest",
        "bond_yield",
        "bond_spread",
        "historical_prepayment",
        "treasury_curve",
    ] {
        assert!(has(definition), "{definition}: {:?}", answer.not_evaluated);
    }
    // Four months of factors: 6 and 12 months not evaluated, as the
    // fixture's case says.
    let short: Vec<&str> = answer
        .not_evaluated
        .iter()
        .filter(|u| u.reason == NotEvaluatedReason::HistoryInsufficient as i32)
        .map(|u| u.detail.split(':').next().unwrap())
        .collect();
    assert_eq!(short, vec!["12m", "6m"]);
    assert_eq!(answer.not_evaluated.len(), 2, "{:?}", answer.not_evaluated);
    // Each debt read on the client's behalf, as every read is (W12.2).
    let heard = stores.heard.lock().unwrap();
    for topic in [
        crate::debt::LIST_FACTORS,
        topics::LIST_RATES,
        topics::READ_CALENDAR,
    ] {
        let (_, meta) = heard
            .stamps
            .iter()
            .find(|(t, _)| t == topic)
            .unwrap_or_else(|| panic!("{topic} was not asked"));
        assert_eq!(meta.on_behalf_of_instance_id, crate::INSTANCE, "{topic}");
    }
    let provenance = answer.provenance.unwrap();
    for dataset in ["bonds-1:closes", "gse-1:factors", "us-treasury-1:ust_par"] {
        assert!(
            provenance.datasets.contains(&dataset.to_string()),
            "{dataset}"
        );
    }
    let models: Vec<&str> = provenance
        .models
        .iter()
        .map(|m| m.model_id.as_str())
        .collect();
    for model in ["bond_cash_flows", "bond_yield_solver", "monotone_convex"] {
        assert!(models.contains(&model), "{models:?}");
    }
}
