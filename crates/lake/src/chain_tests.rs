//! Listed options and futures in the lake (contract v28; ruling 561, G9 to
//! G11; W10.4 to W10.6): a series' terms recorded and read as a chain, never
//! minted; chain quotes keyed by the underlying and the series, answered
//! only to a read that asks for series; statistics per series and trading
//! date, published and read by date; a trade's quote at print; and the
//! refusals W10.4 names. The instrument store and the conductor stood in on
//! the bus, as the lake's other tests stand them in.

use std::sync::Arc;

use meridian_bus::{Bus, Envelope, MemoryBackend, MessageMeta};
use meridian_clock::ManualClock;
use meridian_domain::v1::{
    AssetClass, DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, Identifier,
    InstrumentRecord, InstrumentType, ListSeriesRequest, ListStatisticsRequest, ListedSeries,
    Money, ObservationMeta, Quote, QuoteAtTrade, ResolveIdentifierReply, ResolveIdentifierRequest,
    ResolveInstrumentReply, ResolveInstrumentRequest, Source, Statistic, StatisticKind,
    StatisticsRecordedEvent, SubjectRef, TemplateFieldValue, Trade, TradeAttributes, UnitValue,
    ValueUnit,
};
use meridian_pb::v1::{DatasetDeclaration, DatasetLicence, Decimal, ObservationMode};
use prost::Message;

use crate::row::{DataType, Observation};
use crate::service::{self, Lake, ReadRequest};

const NOW: i64 = 1_791_417_600_000_000_000; // 2026-10-08T00:00Z
const HOUR: i64 = 3_600 * 1_000_000_000;
const AAPL: &str = "INS-01JC28AAPL000000000000001";
const ES: &str = "INS-01JC28ESZ6000000000000001";
const HELD_CALL: &str = "INS-01JC28AAPLC230000000000001";
const CHAIN: &str = "tradier-1:options";
const OSI_CALL: &str = "AAPL  261120C00230000";
const OSI_PUT: &str = "AAPL  261218P00200000";
const DERIVED: &str = "signal-1:derived";

struct Harness {
    bus: Arc<Bus>,
    lake: Arc<Lake>,
}

fn harness() -> Harness {
    let clock = Arc::new(ManualClock::at(NOW));
    let bus = Arc::new(Bus::single(
        "lake-1",
        Arc::new(MemoryBackend::new()),
        clock.clone(),
    ));
    bus.serve(service::RESOLVE_INSTRUMENT, |envelope| {
        let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        let held = |class: AssetClass, kind: InstrumentType| {
            Some(InstrumentRecord {
                instrument_id: asked.instrument_id.clone(),
                asset_class: class as i32,
                instrument_type: kind as i32,
                ..Default::default()
            })
        };
        let instrument = match asked.instrument_id.as_str() {
            AAPL => held(AssetClass::Equity, InstrumentType::Unspecified),
            ES => held(AssetClass::Derivative, InstrumentType::Future),
            HELD_CALL => held(AssetClass::Derivative, InstrumentType::ListedOption),
            _ => None,
        };
        Ok((
            "meridian.v1.ResolveInstrumentReply".into(),
            ResolveInstrumentReply {
                found: instrument.is_some(),
                instrument,
            }
            .encode_to_vec(),
        ))
    });
    // The store holds the call's record by its OSI symbol and nothing for
    // the put; a currency resolves to its cash instrument.
    bus.serve(service::RESOLVE_IDENTIFIER, |envelope| {
        let asked = ResolveIdentifierRequest::decode(&envelope.payload[..]).unwrap();
        let first = asked.identifiers.first().cloned().unwrap_or_default();
        let found = match (first.scheme.as_str(), first.value.as_str()) {
            ("iso4217", _) => Some("INS-USD"),
            ("occ_osi", OSI_CALL) => Some(HELD_CALL),
            _ => None,
        };
        Ok((
            "meridian.v1.ResolveIdentifierReply".into(),
            ResolveIdentifierReply {
                found: found.is_some(),
                instrument_id: found.unwrap_or_default().into(),
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(service::PLUGIN_CATALOGUE, |_| {
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            meridian_domain::v1::PluginCatalogue::default().encode_to_vec(),
        ))
    });
    let store = Arc::new(crate::MemoryStore::new());
    let lake = service::serve(bus.clone(), store, clock);
    lake.configured(&configuration());
    Harness { bus, lake }
}

/// One options dataset serving a chain's series, quotes, trades and
/// statistics; signal-1 entitled to every field.
fn configuration() -> EntitlementsChangedEvent {
    let (instance, key) = CHAIN.split_once(':').unwrap();
    let derived = DatasetRef {
        dataset: DERIVED.into(),
        instance: "signal-1".into(),
        vendor: "Derived".into(),
        declaration: Some(DatasetDeclaration {
            key: "derived".into(),
            vendor: "Derived".into(),
            data_types: vec![
                "meridian.v1.DerivedSeries".into(),
                "meridian.v1.SavedScenario".into(),
            ],
            modes: vec![ObservationMode::Push as i32],
            licence_default: Some(DatasetLicence {
                kept: true,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    EntitlementsChangedEvent {
        datasets: vec![
            derived,
            DatasetRef {
                dataset: CHAIN.into(),
                instance: instance.into(),
                vendor: "Tradier".into(),
                declaration: Some(DatasetDeclaration {
                    key: key.into(),
                    vendor: "Tradier".into(),
                    data_types: vec![
                        "meridian.v1.ListedSeries".into(),
                        "meridian.v1.Quote".into(),
                        "meridian.v1.Trade".into(),
                        "meridian.v1.Statistic".into(),
                    ],
                    modes: vec![ObservationMode::Stream as i32],
                    licence_default: Some(DatasetLicence {
                        kept: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ],
        licences: vec![],
        entitlements: vec![
            DatasetEntitlement {
                dataset: CHAIN.into(),
                instance: "signal-1".into(),
                allowed: true,
                ..Default::default()
            },
            DatasetEntitlement {
                dataset: DERIVED.into(),
                instance: "signal-1".into(),
                allowed: true,
                ..Default::default()
            },
        ],
        changed_at_ns: NOW,
    }
}

fn envelope(instance: &str) -> Envelope {
    Envelope {
        meta: Some(MessageMeta {
            publisher_instance_id: instance.into(),
            account_scope_applies: true,
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

fn usd(amount: &str) -> Option<Money> {
    Some(Money {
        amount: decimal(amount),
        currency_code: "USD".into(),
        instrument_id: String::new(),
    })
}

fn osi(value: &str) -> Vec<Identifier> {
    vec![Identifier {
        scheme: "occ_osi".into(),
        value: value.into(),
        ..Default::default()
    }]
}

fn meta(row_key: &str, subject: &str, key: Vec<Identifier>, date: &str) -> Option<ObservationMeta> {
    Some(ObservationMeta {
        row_key: row_key.into(),
        subjects: vec![SubjectRef {
            entity_id: subject.into(),
            ..Default::default()
        }],
        source: Some(Source {
            instance: "tradier-1".into(),
            dataset: CHAIN.into(),
            ..Default::default()
        }),
        valid_from_ns: NOW - HOUR,
        business_date: date.into(),
        series_key: key,
        ..Default::default()
    })
}

fn field(path: &str, value: &str) -> TemplateFieldValue {
    TemplateFieldValue {
        path: path.into(),
        value: value.into(),
        ..Default::default()
    }
}

fn series(key: &str, put_or_call: &str, strike: &str, expiration: &str) -> ListedSeries {
    ListedSeries {
        meta: meta(&format!("series:{key}"), AAPL, osi(key), ""),
        template_id: "TPL-LISTED-OPTION".into(),
        template_version: "1".into(),
        fields: vec![
            field("underlying_instrument_id", AAPL),
            field("put_or_call", put_or_call),
            field("strike_price", strike),
            field("expiration_date", expiration),
            field("exercise_style", "american"),
            field("settlement_method", "physical"),
            field("contract_multiplier", "100"),
            field("option_root", "AAPL"),
        ],
        instrument_id: String::new(),
    }
}

fn quote(key: &str, bid: &str, ask: &str, at: i64) -> Quote {
    let mut meta = meta(&format!("quote:{key}"), AAPL, osi(key), "");
    meta.as_mut().unwrap().valid_from_ns = at;
    Quote {
        meta,
        bid: usd(bid),
        ask: usd(ask),
        ..Default::default()
    }
}

fn open_interest(key: &str, date: &str, contracts: &str) -> Statistic {
    Statistic {
        meta: meta(&format!("oi:{key}:{date}"), AAPL, osi(key), date),
        kind: StatisticKind::OpenInterest as i32,
        value: Some(UnitValue {
            value: decimal(contracts),
            unit: ValueUnit::Count as i32,
            ..Default::default()
        }),
    }
}

fn record(h: &Harness, field: &str, rows: Vec<Observation>) -> Result<(u32, u32, u32), String> {
    tokio::task::block_in_place(|| h.lake.record(field, rows, "", &envelope("tradier-1")))
        .map(|reply| (reply.recorded, reply.restated, reply.unchanged))
}

fn read_quotes(h: &Harness, keys: Vec<Identifier>, all: bool) -> Vec<Quote> {
    tokio::task::block_in_place(|| {
        h.lake.read(
            ReadRequest {
                data_type: DataType::Quote,
                subjects: vec![SubjectRef {
                    entity_id: AAPL.into(),
                    ..Default::default()
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
                tenors: vec![],
                published_as_of_ns: 0,
                series_keys: keys,
                all_series: all,
                dates: None,
            },
            &envelope("signal-1"),
        )
    })
    .unwrap()
    .rows
    .into_iter()
    .filter_map(|row| match row {
        Observation::Quote(quote) => Some(quote),
        _ => None,
    })
    .collect()
}

fn subjects(id: &str) -> Vec<SubjectRef> {
    vec![SubjectRef {
        entity_id: id.into(),
        ..Default::default()
    }]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_is_recorded_and_read_by_its_underlying_never_minted() {
    let h = harness();
    let rows = vec![
        Observation::Series(series(OSI_CALL, "call", "230", "2026-11-20")),
        Observation::Series(series(OSI_PUT, "put", "200", "2026-12-18")),
    ];
    assert_eq!(record(&h, "series", rows).unwrap(), (2, 0, 0));
    let list = |request: ListSeriesRequest| {
        tokio::task::block_in_place(|| h.lake.list_series(request, &envelope("signal-1")))
            .unwrap()
            .series
    };
    let chain = list(ListSeriesRequest {
        subjects: subjects(AAPL),
        ..Default::default()
    });
    assert_eq!(chain.len(), 2);
    // The call's record held, matched by its key; the put's not, and not
    // minted.
    let by_key = |key: &str| {
        chain
            .iter()
            .find(|s| s.meta.as_ref().unwrap().series_key == osi(key))
            .unwrap()
            .instrument_id
            .clone()
    };
    assert_eq!(by_key(OSI_CALL), HELD_CALL);
    assert_eq!(by_key(OSI_PUT), "");
    // An expiration window; a type no series of it is; a key.
    let november = list(ListSeriesRequest {
        subjects: subjects(AAPL),
        expiration_from: "2026-11-01".into(),
        expiration_to: "2026-11-30".into(),
        ..Default::default()
    });
    assert_eq!(november.len(), 1);
    assert_eq!(november[0].meta.as_ref().unwrap().series_key, osi(OSI_CALL));
    let futures = list(ListSeriesRequest {
        subjects: subjects(AAPL),
        instrument_type: InstrumentType::Future as i32,
        ..Default::default()
    });
    assert!(futures.is_empty());
    let put = list(ListSeriesRequest {
        subjects: subjects(AAPL),
        series_keys: osi(OSI_PUT),
        ..Default::default()
    });
    assert_eq!(put.len(), 1);
    assert_eq!(put[0].meta.as_ref().unwrap().series_key, osi(OSI_PUT));
    // An adjustment is the next version of the row key.
    let mut adjusted = series(OSI_CALL, "call", "230", "2026-11-20");
    adjusted.fields[6] = field("contract_multiplier", "150");
    assert_eq!(
        record(&h, "series", vec![Observation::Series(adjusted)]).unwrap(),
        (0, 1, 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chain_quotes_are_answered_only_to_a_read_asking_for_series() {
    let h = harness();
    let own = Quote {
        meta: meta("quote:AAPL", AAPL, vec![], ""),
        bid: usd("227.50"),
        ask: usd("227.52"),
        ..Default::default()
    };
    let rows = vec![
        Observation::Quote(own),
        Observation::Quote(quote(OSI_CALL, "4.10", "4.20", NOW - HOUR)),
        Observation::Quote(quote(OSI_PUT, "1.05", "1.10", NOW - HOUR)),
    ];
    assert_eq!(record(&h, "quotes", rows).unwrap(), (3, 0, 0));
    // A v27 reader of the underlying hears its own quote alone.
    let plain = read_quotes(&h, vec![], false);
    assert_eq!(plain.len(), 1);
    assert!(plain[0].meta.as_ref().unwrap().series_key.is_empty());
    // Every series beside it: the latest per series, never one for both.
    assert_eq!(read_quotes(&h, vec![], true).len(), 3);
    let call = read_quotes(&h, osi(OSI_CALL), false);
    assert_eq!(call.len(), 2);
    assert!(call
        .iter()
        .any(|q| q.meta.as_ref().unwrap().series_key == osi(OSI_CALL)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_statistic_is_recorded_per_series_and_date_heard_and_read() {
    let h = harness();
    let mut heard = h
        .bus
        .subscribe_every(&format!("platform.lake.{CHAIN}.event.statistics-recorded"));
    let rows = vec![
        Observation::Statistic(open_interest(OSI_CALL, "2026-10-06", "1200")),
        Observation::Statistic(open_interest(OSI_CALL, "2026-10-07", "1350")),
        Observation::Statistic(open_interest(OSI_PUT, "2026-10-07", "800")),
    ];
    assert_eq!(record(&h, "statistics", rows).unwrap(), (3, 0, 0));
    let event =
        StatisticsRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..]).unwrap();
    assert_eq!(
        event.statistic.unwrap().kind,
        StatisticKind::OpenInterest as i32
    );
    let list = |request: ListStatisticsRequest| {
        tokio::task::block_in_place(|| h.lake.list_statistics(request, &envelope("signal-1")))
    };
    let on_the_seventh = list(ListStatisticsRequest {
        subjects: subjects(AAPL),
        business_date: "2026-10-07".into(),
        all_series: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(on_the_seventh.statistics.len(), 2);
    let calls = list(ListStatisticsRequest {
        subjects: subjects(AAPL),
        from_date: "2026-10-01".into(),
        to_date: "2026-10-07".into(),
        series_keys: osi(OSI_CALL),
        kinds: vec![StatisticKind::OpenInterest as i32],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(calls.statistics.len(), 2);
    // No series asked: the chain's statistics are left out.
    let none = list(ListStatisticsRequest {
        subjects: subjects(AAPL),
        business_date: "2026-10-07".into(),
        ..Default::default()
    })
    .unwrap();
    assert!(none.statistics.is_empty());
    assert!(list(ListStatisticsRequest {
        subjects: subjects(AAPL),
        business_date: "2026-10-07".into(),
        from_date: "2026-10-01".into(),
        ..Default::default()
    })
    .unwrap_err()
    .contains("at most one"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trade_keeps_the_quote_in_force_at_its_print() {
    let h = harness();
    let trade = Trade {
        meta: meta("trade:1", AAPL, osi(OSI_CALL), ""),
        price: usd("4.15"),
        quantity: decimal("10"),
        attributes: Some(TradeAttributes::default()),
        quote_at_trade: Some(QuoteAtTrade {
            bid: usd("4.10"),
            ask: usd("4.20"),
            source_time_ns: NOW - HOUR - 5,
            as_last_received: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        record(&h, "trades", vec![Observation::Trade(trade)]).unwrap(),
        (1, 0, 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn what_w10_4_refuses_of_a_chain_is_refused_naming_the_field() {
    let h = harness();
    // A key on a row about the series' own record.
    let mut on_held = quote(OSI_CALL, "4.10", "4.20", NOW - HOUR);
    on_held.meta.as_mut().unwrap().subjects = subjects(HELD_CALL);
    let said = record(&h, "quotes", vec![Observation::Quote(on_held)]).unwrap_err();
    assert!(said.contains("meta.series_key"), "{said}");
    assert!(said.contains("own record"), "{said}");
    // A MIC alone.
    let mut mic = quote(OSI_CALL, "4.10", "4.20", NOW - HOUR);
    mic.meta.as_mut().unwrap().series_key = vec![Identifier {
        scheme: "mic".into(),
        value: "XCME".into(),
        ..Default::default()
    }];
    let said = record(&h, "quotes", vec![Observation::Quote(mic)]).unwrap_err();
    assert!(said.contains("a MIC alone"), "{said}");
    // A series with no key; one naming its record.
    let mut keyless = series(OSI_CALL, "call", "230", "2026-11-20");
    keyless.meta.as_mut().unwrap().series_key.clear();
    let said = record(&h, "series", vec![Observation::Series(keyless)]).unwrap_err();
    assert!(
        said.contains("series[0].meta.series_key is required"),
        "{said}"
    );
    let mut named = series(OSI_CALL, "call", "230", "2026-11-20");
    named.instrument_id = HELD_CALL.into();
    let said = record(&h, "series", vec![Observation::Series(named)]).unwrap_err();
    assert!(said.contains("series[0].instrument_id"), "{said}");
    // A statistic with no trading date; a negative count.
    let mut undated = open_interest(OSI_CALL, "2026-10-07", "10");
    undated.meta.as_mut().unwrap().business_date.clear();
    let said = record(&h, "statistics", vec![Observation::Statistic(undated)]).unwrap_err();
    assert!(said.contains("business_date is required"), "{said}");
    let negative = open_interest(OSI_CALL, "2026-10-07", "-1");
    let said = record(&h, "statistics", vec![Observation::Statistic(negative)]).unwrap_err();
    assert!(said.contains("never negative"), "{said}");
    // A futures option's chain keyed under its future, by symbol and MIC.
    let mut future_chain = quote(OSI_CALL, "12.5", "12.75", NOW - HOUR);
    let m = future_chain.meta.as_mut().unwrap();
    m.subjects = subjects(ES);
    m.series_key = vec![
        Identifier {
            scheme: "symbol".into(),
            value: "ESZ6 C5800".into(),
            ..Default::default()
        },
        Identifier {
            scheme: "mic".into(),
            value: "XCME".into(),
            ..Default::default()
        },
    ];
    assert_eq!(
        record(&h, "quotes", vec![Observation::Quote(future_chain)]).unwrap(),
        (1, 0, 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signal_plugins_derived_series_and_saved_scenarios_are_kept_and_read() {
    // Contract v28, track B (W10.4, W10.6, W12.8; rulings 566, 581 to 584).
    use meridian_domain::v1::{
        DerivedInput, DerivedSeries, ListDerivedSeriesRequest, ListScenariosRequest, MeasureRef,
        SavedScenario, Scenario, ScenarioAxis, ScenarioSet, Shock, ShockForm, ShockObject,
        ShockTarget, ShockUnit,
    };
    let h = harness();
    let own = |row_key: &str, date: &str, subjects: &[&str]| {
        Some(ObservationMeta {
            row_key: row_key.into(),
            subjects: subjects
                .iter()
                .map(|s| SubjectRef {
                    entity_id: s.to_string(),
                    ..Default::default()
                })
                .collect(),
            source: Some(Source {
                instance: "signal-1".into(),
                dataset: DERIVED.into(),
                ..Default::default()
            }),
            valid_from_ns: NOW - HOUR,
            business_date: date.into(),
            ..Default::default()
        })
    };
    let iv30 = |date: &str, value: &str| DerivedSeries {
        meta: own(&format!("iv30:{date}"), date, &[AAPL]),
        series: "iv30".into(),
        value: Some(UnitValue {
            value: decimal(value),
            unit: ValueUnit::Percent as i32,
            ..Default::default()
        }),
        rule: Some(MeasureRef {
            definition: "implied_vol_30d".into(),
            version: 1,
            ..Default::default()
        }),
        inputs: vec![DerivedInput {
            dataset: CHAIN.into(),
            as_of_ns: NOW - HOUR,
        }],
    };
    let record_as = |field: &str, rows: Vec<Observation>| {
        tokio::task::block_in_place(|| h.lake.record(field, rows, "", &envelope("signal-1")))
            .map(|reply| (reply.recorded, reply.restated, reply.unchanged))
    };
    assert_eq!(
        record_as(
            "values",
            vec![
                Observation::Derived(iv30("2026-10-06", "27.4")),
                Observation::Derived(iv30("2026-10-07", "28.1")),
            ]
        )
        .unwrap(),
        (2, 0, 0)
    );
    let mut ruleless = iv30("2026-10-07", "28.1");
    ruleless.rule = None;
    assert!(record_as("values", vec![Observation::Derived(ruleless)])
        .unwrap_err()
        .contains("values[0].rule"));
    let listed = tokio::task::block_in_place(|| {
        h.lake.list_derived_series(
            ListDerivedSeriesRequest {
                subjects: subjects(AAPL),
                series: vec!["iv30".into()],
                from_date: "2026-10-07".into(),
                to_date: String::new(),
            },
            &envelope("signal-1"),
        )
    })
    .unwrap();
    assert_eq!(listed.values.len(), 1);
    assert_eq!(
        listed.values[0].meta.as_ref().unwrap().business_date,
        "2026-10-07"
    );
    // A scenario set: spot by +-10% against vol by +-5 points.
    let shock = |object: ShockObject, form: ShockForm, unit: ShockUnit, value: &str| Shock {
        object: object as i32,
        target: Some(ShockTarget {
            subject_id: AAPL.into(),
            every: false,
        }),
        form: form as i32,
        unit: unit as i32,
        value: decimal(value),
        buckets: vec![],
    };
    let step = |name: &str, shocks: Vec<Shock>| Scenario {
        name: name.into(),
        shocks,
    };
    let grid = SavedScenario {
        meta: own("grid-1", "", &[]),
        scenario: None,
        scenario_set: Some(ScenarioSet {
            name: "Spot by vol".into(),
            axes: vec![
                ScenarioAxis {
                    name: "spot".into(),
                    steps: ["-10", "0", "10"]
                        .iter()
                        .map(|v| {
                            step(
                                v,
                                vec![shock(
                                    ShockObject::Spot,
                                    ShockForm::Relative,
                                    ShockUnit::Percent,
                                    v,
                                )],
                            )
                        })
                        .collect(),
                },
                ScenarioAxis {
                    name: "vol".into(),
                    steps: ["-5", "5"]
                        .iter()
                        .map(|v| {
                            step(
                                v,
                                vec![shock(
                                    ShockObject::VolSurface,
                                    ShockForm::Parallel,
                                    ShockUnit::VolPoints,
                                    v,
                                )],
                            )
                        })
                        .collect(),
                },
            ],
        }),
        description: "The scenario grid's default".into(),
    };
    assert_eq!(
        record_as("scenarios", vec![Observation::Scenario(grid.clone())]).unwrap(),
        (1, 0, 0)
    );
    // A spot shock in basis points, and an object not held, are refused.
    let mut wrong = grid.clone();
    wrong.scenario_set = None;
    wrong.scenario = Some(step(
        "bad",
        vec![shock(
            ShockObject::Spot,
            ShockForm::Relative,
            ShockUnit::BasisPoints,
            "1",
        )],
    ));
    assert!(
        record_as("scenarios", vec![Observation::Scenario(wrong.clone())])
            .unwrap_err()
            .contains("scenarios[0].scenario.shocks[0].form and unit")
    );
    let mut unheld = shock(
        ShockObject::Spot,
        ShockForm::Relative,
        ShockUnit::Percent,
        "1",
    );
    unheld.target = Some(ShockTarget {
        subject_id: "INS-NOT-HELD".into(),
        every: false,
    });
    wrong.scenario = Some(step("unheld", vec![unheld]));
    assert!(record_as("scenarios", vec![Observation::Scenario(wrong)])
        .unwrap_err()
        .contains("target.subject_id"));
    let saved = tokio::task::block_in_place(|| {
        h.lake.list_scenarios(
            ListScenariosRequest {
                row_keys: vec!["grid-1".into()],
                version: 0,
            },
            &envelope("signal-1"),
        )
    })
    .unwrap();
    assert_eq!(saved.scenarios.len(), 1);
    assert_eq!(
        saved.scenarios[0].scenario_set.as_ref().unwrap().axes.len(),
        2
    );
}
