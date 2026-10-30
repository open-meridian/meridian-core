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
            SPX => held(AssetClass::Referential, InstrumentType::Index),
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
                window: Default::default(),
                fields: Vec::new(),
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
    // A window's page is filled after the window is applied: a page of one
    // in November holds the November call, never an empty page with a
    // cursor, though the December put was recorded between them.
    let mut more = Vec::new();
    for strike in ["240", "250", "260"] {
        more.push(Observation::Series(series(
            &format!("AAPL  261218P00{strike}000"),
            "put",
            strike,
            "2026-12-18",
        )));
    }
    more.push(Observation::Series(series(
        "AAPL  261120C00240000",
        "call",
        "240",
        "2026-11-20",
    )));
    assert_eq!(record(&h, "series", more).unwrap(), (4, 0, 0));
    let page = |cursor: &str| {
        tokio::task::block_in_place(|| {
            h.lake.list_series(
                ListSeriesRequest {
                    subjects: subjects(AAPL),
                    expiration_from: "2026-11-01".into(),
                    expiration_to: "2026-11-30".into(),
                    page_size: 1,
                    cursor: cursor.into(),
                    ..Default::default()
                },
                &envelope("signal-1"),
            )
        })
        .unwrap()
    };
    let first = page("");
    assert_eq!(first.series.len(), 1);
    let second = page(&first.next_cursor);
    assert_eq!(second.series.len(), 1, "no short page while rows remain");
    assert_ne!(
        first.series[0].meta.as_ref().unwrap().series_key,
        second.series[0].meta.as_ref().unwrap().series_key
    );
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

/// The underlying's own quotes in a dataset of their own, before the
/// chain's in the default order (alphabetical, no priority set).
const LIVE: &str = "tradier-1:live";

/// Found on dev 2026-10-26: the underlying's own quote in
/// tradier:live_delayed hid every series quote in tradier:options_delayed,
/// the default read taking the first dataset with any row for the subject.
/// A read that asks for series chooses its source per series: one source
/// per series, never mixed within one (the intent's Q4).
#[tokio::test(flavor = "multi_thread")]
async fn a_default_read_of_a_chain_chooses_its_source_per_series() {
    let h = harness();
    let mut c = configuration();
    let mut live = c.datasets[1].clone();
    live.dataset = LIVE.into();
    if let Some(declaration) = live.declaration.as_mut() {
        declaration.key = "live".into();
    }
    c.datasets.push(live);
    c.entitlements.push(DatasetEntitlement {
        dataset: LIVE.into(),
        instance: "signal-1".into(),
        allowed: true,
        ..Default::default()
    });
    h.lake.configured(&c);
    let mut own_meta = meta("quote:AAPL", AAPL, vec![], "");
    if let Some(source) = own_meta.as_mut().and_then(|m| m.source.as_mut()) {
        source.dataset = LIVE.into();
    }
    let own = Quote {
        meta: own_meta,
        bid: usd("227.50"),
        ask: usd("227.52"),
        ..Default::default()
    };
    assert_eq!(
        record(&h, "quotes", vec![Observation::Quote(own)]).unwrap(),
        (1, 0, 0)
    );
    let chain = vec![
        Observation::Quote(quote(OSI_CALL, "4.10", "4.20", NOW - HOUR)),
        Observation::Quote(quote(OSI_PUT, "1.05", "1.10", NOW - HOUR)),
    ];
    assert_eq!(record(&h, "quotes", chain).unwrap(), (2, 0, 0));

    let every = read_quotes(&h, vec![], true);
    assert_eq!(every.len(), 3, "the underlying's quote and each series'");
    let from = |quote: &Quote| {
        quote
            .meta
            .as_ref()
            .unwrap()
            .source
            .as_ref()
            .unwrap()
            .dataset
            .clone()
    };
    for quote in &every {
        let keyed = !quote.meta.as_ref().unwrap().series_key.is_empty();
        assert_eq!(from(quote), if keyed { CHAIN } else { LIVE });
    }
    // A v27 reader of the underlying still hears its own quote alone.
    let plain = read_quotes(&h, vec![], false);
    assert_eq!(plain.len(), 1);
    assert_eq!(from(&plain[0]), LIVE);
    // One series by its key, beside the underlying's own.
    let call = read_quotes(&h, osi(OSI_CALL), false);
    assert_eq!(call.len(), 2);
    assert!(call
        .iter()
        .any(|q| q.meta.as_ref().unwrap().series_key == osi(OSI_CALL) && from(q) == CHAIN));
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
    // A futures key in the third amendment's spelling, a symbol beside a
    // MIC as two identifiers; a MIC alone; a ticker with no MIC in source
    // (DESIGN28's sixth amendment: one ticker carrying its venue).
    let keyed = |key: Vec<Identifier>| {
        let mut row = quote(OSI_CALL, "4.10", "4.20", NOW - HOUR);
        row.meta.as_mut().unwrap().series_key = key;
        record(&h, "quotes", vec![Observation::Quote(row)]).unwrap_err()
    };
    let identifier = |scheme: &str, value: &str, source: &str| Identifier {
        scheme: scheme.into(),
        value: value.into(),
        source: source.into(),
        ..Default::default()
    };
    let said = keyed(vec![
        identifier("symbol", "ESZ6", ""),
        identifier("iso10383", "XCME", ""),
    ]);
    assert!(
        said.contains("meta.series_key: names 2; one identifier"),
        "{said}"
    );
    let said = keyed(vec![identifier("mic", "XCME", "")]);
    assert!(said.contains("meta.series_key[0].scheme"), "{said}");
    let said = keyed(vec![identifier("ticker", "ESZ6", "")]);
    assert!(said.contains("meta.series_key[0].source"), "{said}");
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
    // A futures option's chain keyed under its future, by its ticker
    // carrying the venue's MIC.
    let mut future_chain = quote(OSI_CALL, "12.5", "12.75", NOW - HOUR);
    let m = future_chain.meta.as_mut().unwrap();
    m.subjects = subjects(ES);
    m.series_key = vec![Identifier {
        scheme: "ticker".into(),
        value: "ESZ6 C5800".into(),
        source: "XCME".into(),
        ..Default::default()
    }];
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
    // An edit is the next version of its key; a job may name either.
    let mut edited = grid.clone();
    edited.scenario_set.as_mut().unwrap().axes.pop();
    assert_eq!(
        record_as("scenarios", vec![Observation::Scenario(edited)]).unwrap(),
        (0, 1, 0)
    );
    let at = |version: u64| {
        tokio::task::block_in_place(|| {
            h.lake.list_scenarios(
                ListScenariosRequest {
                    row_keys: vec!["grid-1".into()],
                    version,
                },
                &envelope("signal-1"),
            )
        })
        .unwrap()
        .scenarios
    };
    let axes = |rows: &[SavedScenario]| rows[0].scenario_set.as_ref().unwrap().axes.len();
    assert_eq!(axes(&at(0)), 1, "the latest");
    assert_eq!(axes(&at(1)), 2, "the version asked, though not the latest");
    assert_eq!(axes(&at(2)), 1);
    assert!(at(3).is_empty());
}

/// Rulings 798, 808: a read in pages answers its whole answer once, at the
/// first page, and cuts each page after it from that answer, held until the
/// last page is read; the pages are the answer a single read gives. A change
/// to the configuration between pages is answered afresh, never from what
/// was held under the old one.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_in_pages_is_answered_once_and_held_until_its_last_page() {
    let h = harness();
    let keys: Vec<String> = (0..250)
        .map(|i| format!("AAPL  261120C{:08}", 100_000 + i * 1000))
        .collect();
    let rows: Vec<Observation> = keys
        .iter()
        .map(|key| Observation::Quote(quote(key, "1.00", "1.10", NOW - HOUR)))
        .collect();
    for chunk in rows.chunks(100) {
        record(&h, "quotes", chunk.to_vec()).unwrap();
    }
    let page = |cursor: &str, size: u32| {
        tokio::task::block_in_place(|| {
            h.lake.read(
                ReadRequest {
                    data_type: DataType::Quote,
                    subjects: subjects(AAPL),
                    kinds: vec![],
                    interval_ns: 0,
                    sources: None,
                    at_ns: 0,
                    business_date: String::new(),
                    valid_from_ns: 0,
                    valid_until_ns: 0,
                    as_of_ns: 0,
                    page_size: size,
                    cursor: cursor.into(),
                    after: None,
                    tenors: vec![],
                    published_as_of_ns: 0,
                    series_keys: vec![],
                    all_series: true,
                    dates: None,
                    window: Default::default(),
                    fields: Vec::new(),
                },
                &envelope("signal-1"),
            )
        })
        .unwrap()
    };
    let key_of = |row: &Observation| row.meta().series_key[0].value.clone();
    let whole: Vec<String> = page("", 500).rows.iter().map(key_of).collect();
    assert_eq!(whole.len(), 250);
    assert_eq!(h.lake.held_pages(), 0, "one page: nothing held");
    let first = page("", 100);
    assert_eq!(h.lake.held_pages(), 1, "held for the pages after it");
    let second = page(&first.next_cursor, 100);
    assert_eq!(h.lake.held_pages(), 1);
    let third = page(&second.next_cursor, 100);
    assert!(third.next_cursor.is_empty());
    assert_eq!(h.lake.held_pages(), 0, "the last page read: let go");
    let paged: Vec<String> = [first.rows, second.rows, third.rows]
        .iter()
        .flatten()
        .map(key_of)
        .collect();
    assert_eq!(paged, whole, "the pages are the one answer");
    // A change to the entitlements between pages: what was held is not
    // served; the page is answered afresh under the change.
    let first = page("", 100);
    let mut withdrawn = configuration();
    withdrawn.entitlements.retain(|e| e.dataset != CHAIN);
    h.lake.configured(&withdrawn);
    let after = page(&first.next_cursor, 100);
    assert!(
        after.rows.is_empty(),
        "not entitled now: nothing served from the held answer"
    );
}

/// A read over the bus as core reads, every field entitled: the handler's
/// own checks and narrowing (contract v28, amended 2026-10-28).
async fn asked<Q: Message, R: Message + Default>(
    h: &Harness,
    topic: &str,
    kind: &str,
    request: Q,
) -> Result<R, String> {
    h.bus
        .call(topic, kind, request.encode_to_vec(), None, None)
        .await
        .map(|(_, payload)| R::decode(&payload[..]).unwrap())
        .map_err(|failed| failed.to_string())
}

fn refused_naming(said: &str) -> Vec<String> {
    let at = said.find("[refusal-status").expect(said);
    meridian_bus::read_refusal_status(&said[at..])
        .map(|(_, fields, _)| fields)
        .unwrap_or_default()
}

const OSI_UNLISTED: &str = "AAPL  261120P00210000";

/// Rulings 821, 833 and 835 to 838 (spec/one-data-access-model, step 1): a
/// chain read names its expiration window, answered from the lake's listing,
/// and the fields it shows; the envelope always, a field not asked unset and
/// never named unanswered; a window only with all_series and never beside
/// series_keys; a field of another message refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_read_names_its_window_and_its_fields() {
    let h = harness();
    let listed = vec![
        Observation::Series(series(OSI_CALL, "call", "230", "2026-11-20")),
        Observation::Series(series(OSI_PUT, "put", "200", "2026-12-18")),
    ];
    assert_eq!(record(&h, "series", listed).unwrap(), (2, 0, 0));
    let sized = |key: &str| {
        let mut q = quote(key, "4.10", "4.20", NOW - HOUR);
        q.bid_quantity = decimal("10");
        q.ask_quantity = decimal("12");
        Observation::Quote(q)
    };
    let own = Quote {
        meta: meta("quote:AAPL", AAPL, vec![], ""),
        bid: usd("227.50"),
        ask: usd("227.52"),
        ..Default::default()
    };
    let quotes = vec![
        Observation::Quote(own),
        sized(OSI_CALL),
        sized(OSI_PUT),
        sized(OSI_UNLISTED),
    ];
    assert_eq!(record(&h, "quotes", quotes).unwrap(), (4, 0, 0));
    let november = |fields: Vec<&str>| meridian_domain::v1::ListQuotesRequest {
        subjects: subjects(AAPL),
        all_series: true,
        expiration_from: "2026-11-20".into(),
        expiration_to: "2026-11-20".into(),
        fields: fields.into_iter().map(String::from).collect(),
        ..Default::default()
    };
    let reply: meridian_domain::v1::ListQuotesReply = asked(
        &h,
        service::LIST_QUOTES,
        "meridian.v1.ListQuotesRequest",
        november(vec!["meridian.v1.Quote.bid", "meridian.v1.Quote.ask"]),
    )
    .await
    .unwrap();
    // The underlying's own quote and the November call; never the December
    // put, nor the November put the listing does not hold.
    let keys: Vec<Vec<Identifier>> = reply
        .quotes
        .iter()
        .map(|q| q.meta.as_ref().unwrap().series_key.clone())
        .collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.contains(&vec![]));
    assert!(keys.contains(&osi(OSI_CALL)));
    for q in &reply.quotes {
        assert!(q.meta.is_some(), "the envelope always");
        assert!(q.bid.is_some() && q.ask.is_some());
        assert!(q.bid_quantity.is_none() && q.ask_quantity.is_none());
    }
    assert!(
        reply.unanswered.iter().all(|u| u.field.is_empty()),
        "a field not asked is not unanswered: {:?}",
        reply.unanswered
    );
    // Every field, none asked; the unlisted series by its key alone.
    let whole: meridian_domain::v1::ListQuotesReply = asked(
        &h,
        service::LIST_QUOTES,
        "meridian.v1.ListQuotesRequest",
        november(vec![]),
    )
    .await
    .unwrap();
    assert!(whole.quotes.iter().any(|q| q.bid_quantity.is_some()));
    let unlisted: meridian_domain::v1::ListQuotesReply = asked(
        &h,
        service::LIST_QUOTES,
        "meridian.v1.ListQuotesRequest",
        meridian_domain::v1::ListQuotesRequest {
            subjects: subjects(AAPL),
            series_keys: osi(OSI_UNLISTED),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(unlisted
        .quotes
        .iter()
        .any(|q| q.meta.as_ref().unwrap().series_key == osi(OSI_UNLISTED)));
    // The refusals, each naming its field.
    let refused = |request: meridian_domain::v1::ListQuotesRequest| {
        let h = &h;
        async move {
            asked::<_, meridian_domain::v1::ListQuotesReply>(
                h,
                service::LIST_QUOTES,
                "meridian.v1.ListQuotesRequest",
                request,
            )
            .await
            .unwrap_err()
        }
    };
    let mut no_all = november(vec![]);
    no_all.all_series = false;
    assert_eq!(refused_naming(&refused(no_all).await), ["expiration_from"]);
    let mut keyed = november(vec![]);
    keyed.series_keys = osi(OSI_CALL);
    assert_eq!(refused_naming(&refused(keyed).await), ["series_keys"]);
    let other = november(vec!["meridian.v1.Trade.price"]);
    assert_eq!(refused_naming(&refused(other).await), ["fields"]);
    let mut backwards = november(vec![]);
    backwards.expiration_from = "2026-12-01".into();
    assert_eq!(refused_naming(&refused(backwards).await), ["expiration_to"]);
    // A chain's trades refuse a window without all_series as quotes do.
    let said = asked::<_, meridian_domain::v1::ListTradesReply>(
        &h,
        service::LIST_TRADES,
        "meridian.v1.ListTradesRequest",
        meridian_domain::v1::ListTradesRequest {
            subjects: subjects(AAPL),
            valid_from_ns: NOW - 2 * HOUR,
            valid_until_ns: NOW,
            expiration_from: "2026-11-20".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(refused_naming(&said), ["expiration_from"]);
    // A chain's statistics, one expiration's.
    let stats = vec![
        Observation::Statistic(open_interest(OSI_CALL, "2026-10-07", "1200")),
        Observation::Statistic(open_interest(OSI_PUT, "2026-10-07", "800")),
    ];
    assert_eq!(record(&h, "statistics", stats).unwrap(), (2, 0, 0));
    let reply: meridian_domain::v1::ListStatisticsReply = asked(
        &h,
        service::LIST_STATISTICS,
        "meridian.v1.ListStatisticsRequest",
        ListStatisticsRequest {
            subjects: subjects(AAPL),
            all_series: true,
            expiration_to: "2026-11-30".into(),
            fields: vec!["meridian.v1.Statistic.value".into()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(reply.statistics.len(), 1);
    let only = &reply.statistics[0];
    assert_eq!(only.meta.as_ref().unwrap().series_key, osi(OSI_CALL));
    assert!(only.value.is_some());
    assert_eq!(only.kind, 0, "the kind not asked is left unset");
}

/// A field asked and not entitled is answered as before: unset and named
/// NOT_ENTITLED; one not entitled and not asked is not named.
#[tokio::test(flavor = "multi_thread")]
async fn a_field_asked_and_not_entitled_is_named_one_not_asked_is_not() {
    let h = harness();
    let mut configuration = configuration();
    configuration.entitlements[0].fields = vec![
        "meridian.v1.Quote.meta".into(),
        "meridian.v1.Quote.bid".into(),
    ];
    h.lake.configured(&configuration);
    let mut q = quote(OSI_CALL, "4.10", "4.20", NOW - HOUR);
    q.bid_quantity = decimal("10");
    assert_eq!(
        record(&h, "quotes", vec![Observation::Quote(q)]).unwrap(),
        (1, 0, 0)
    );
    let answer = tokio::task::block_in_place(|| {
        h.lake.read(
            ReadRequest {
                data_type: DataType::Quote,
                subjects: subjects(AAPL),
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
                series_keys: vec![],
                all_series: true,
                dates: None,
                window: Default::default(),
                fields: vec![
                    "meridian.v1.Quote.bid".into(),
                    "meridian.v1.Quote.ask".into(),
                ],
            },
            &envelope("signal-1"),
        )
    })
    .unwrap();
    let Observation::Quote(answered) = &answer.rows[0] else {
        panic!("a quote")
    };
    assert!(answered.bid.is_some());
    assert!(answered.ask.is_none() && answered.bid_quantity.is_none());
    let named: Vec<&str> = answer.unanswered.iter().map(|u| u.field.as_str()).collect();
    assert_eq!(named, ["meridian.v1.Quote.ask"], "{:?}", answer.unanswered);
}

/// A listing read names its terms by template path (ruling 836), and a page
/// asked no size is filled to about 512 KiB (rulings 816 and 823): a chain
/// in a few pages, each within the budget, every series once.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_read_names_its_paths_and_fills_its_pages() {
    let h = harness();
    let mut rows = Vec::new();
    for expiry in 0..10 {
        for strike in 0..300 {
            let key = format!("AAPL  2612{:02}C00{:03}000", expiry + 1, 100 + strike);
            let date = format!("2026-12-{:02}", expiry + 1);
            rows.push(Observation::Series(series(
                &key,
                "call",
                &strike.to_string(),
                &date,
            )));
        }
    }
    for batch in rows.chunks(500) {
        record(&h, "series", batch.to_vec()).unwrap();
    }
    let list = |page_size: u32, cursor: &str, paths: Vec<&str>| {
        tokio::task::block_in_place(|| {
            h.lake.list_series(
                ListSeriesRequest {
                    subjects: subjects(AAPL),
                    page_size,
                    cursor: cursor.into(),
                    paths: paths.into_iter().map(String::from).collect(),
                    ..Default::default()
                },
                &envelope("signal-1"),
            )
        })
        .unwrap()
    };
    // Named paths: those terms alone, the rest always.
    let named = list(
        5,
        "",
        vec!["put_or_call", "strike_price", "expiration_date"],
    );
    assert_eq!(named.series.len(), 5);
    for s in &named.series {
        let paths: Vec<&str> = s.fields.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["put_or_call", "strike_price", "expiration_date"]);
        assert!(s.meta.is_some() && !s.template_id.is_empty() && !s.template_version.is_empty());
    }
    // Asked no size: filled to the budget.
    let (mut cursor, mut pages, mut seen) = (String::new(), 0, std::collections::BTreeSet::new());
    loop {
        let page = list(0, &cursor, vec![]);
        pages += 1;
        assert!(
            page.encoded_len() <= 512 * 1024,
            "page {pages}: {} bytes",
            page.encoded_len()
        );
        for s in &page.series {
            assert!(
                seen.insert(s.meta.as_ref().unwrap().row_key.clone()),
                "once each"
            );
        }
        cursor = page.next_cursor;
        if cursor.is_empty() {
            break;
        }
    }
    assert_eq!(seen.len(), 3_000);
    assert!(pages <= 4, "{pages} pages");
    assert!(pages > 1, "the budget ends a page");
    // Asked a size: that many, never past 500.
    assert_eq!(list(100, "", vec![]).series.len(), 100);
    assert_eq!(list(900, "", vec![]).series.len(), 500);
}

async fn next_want(
    wanted: &mut meridian_bus::Subscription,
) -> Option<meridian_domain::v1::ObservationsWantedEvent> {
    tokio::time::timeout(std::time::Duration::from_millis(500), wanted.recv())
        .await
        .ok()
        .flatten()
        .map(|delivered| {
            meridian_domain::v1::ObservationsWantedEvent::decode(&delivered.envelope.payload[..])
                .unwrap()
        })
}

/// A want made from a chain read carries its window and all_series, told
/// apart from the underlying's own quote want (ruling 821, the v29 plan's
/// item E9); a reader asking outside it widens it to the union, asked again
/// under the same want.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_want_carries_its_window_and_is_told_apart() {
    let h = harness();
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let read = |all_series: bool, from: &str, to: &str| {
        tokio::task::block_in_place(|| {
            h.lake.read(
                ReadRequest {
                    data_type: DataType::Quote,
                    subjects: subjects(ES),
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
                    series_keys: vec![],
                    all_series,
                    dates: None,
                    window: crate::read::Window {
                        from: meridian_domain::date::optional("from", from).unwrap(),
                        to: meridian_domain::date::optional("to", to).unwrap(),
                    },
                    fields: Vec::new(),
                },
                &envelope("signal-1"),
            )
        })
        .unwrap()
    };
    read(true, "2026-12-18", "2026-12-18");
    let chain = next_want(&mut wanted).await.expect("a chain want");
    assert!(chain.all_series);
    assert_eq!(
        (chain.expiration_from.as_str(), chain.expiration_to.as_str()),
        ("2026-12-18", "2026-12-18")
    );
    read(false, "", "");
    let own = next_want(&mut wanted)
        .await
        .expect("the underlying's own want");
    assert!(!own.all_series && own.expiration_from.is_empty());
    assert_ne!(own.want_id, chain.want_id, "told apart");
    // The same window again: coalesced, nothing published.
    read(true, "2026-12-18", "2026-12-18");
    assert!(next_want(&mut wanted).await.is_none());
    // A wider one: the same want, its window the union.
    read(true, "2026-11-20", "2026-11-20");
    let widened = next_want(&mut wanted).await.expect("the want asked again");
    assert_eq!(widened.want_id, chain.want_id);
    assert_eq!(
        (
            widened.expiration_from.as_str(),
            widened.expiration_to.as_str()
        ),
        ("2026-11-20", "2026-12-18")
    );
}

const SPX: &str = "INS-01JC28SPX0000000000000001";

/// A listed series under `underlying`, keyed by `key` and expiring on
/// `date`: a future where it names its product code (its last trade date
/// its expiry), else an option.
fn listed(underlying: &str, key: Vec<Identifier>, date: &str, future: bool) -> Observation {
    let row_key = format!(
        "series:{}",
        key.iter().map(|k| k.value.as_str()).collect::<String>()
    );
    let mut fields = vec![field("underlying_instrument_id", underlying)];
    if future {
        fields.push(field("product_code", "ES"));
        fields.push(field("last_trade_date", date));
    } else {
        fields.push(field("put_or_call", "call"));
        fields.push(field("expiration_date", date));
    }
    Observation::Series(ListedSeries {
        meta: meta(&row_key, underlying, key, ""),
        template_id: "TPL-LISTED".into(),
        template_version: "1".into(),
        fields,
        instrument_id: String::new(),
    })
}

fn ticker(value: &str) -> Vec<Identifier> {
    vec![Identifier {
        scheme: "ticker".into(),
        value: value.into(),
        source: "XCME".into(),
        ..Default::default()
    }]
}

/// Rulings 856, 858 and 859: an underlying's expirations read from the
/// lake's listing alone, each date once in date order with its series
/// counted; narrowed by window and listed type -- a future by its product
/// code, an option on a future record a future option; an adjustment
/// counted at its new expiry only; refused naming the field; a reader not
/// entitled named in unanswered; nothing listed answered as ListSeries
/// answers, its source asked.
#[tokio::test(flavor = "multi_thread")]
async fn an_underlyings_expirations_are_read_from_the_listing() {
    use meridian_domain::v1::{ListExpirationsReply, ListExpirationsRequest, UnansweredReason};
    let h = harness();
    let mut rows = Vec::new();
    for (day, strikes) in [("2026-11-20", 3), ("2026-12-18", 2), ("2027-01-15", 1)] {
        for strike in 0..strikes {
            let key = format!(
                "AAPL  {}C00{:03}000",
                &day[2..].replace('-', ""),
                200 + strike
            );
            rows.push(listed(AAPL, osi(&key), day, false));
        }
    }
    rows.push(listed(SPX, ticker("ESZ6"), "2026-12-18", true));
    rows.push(listed(SPX, ticker("ESH7"), "2027-03-19", true));
    rows.push(listed(
        SPX,
        osi("SPX   261218C05000000"),
        "2026-12-18",
        false,
    ));
    rows.push(listed(ES, ticker("ESZ6 C5000"), "2026-12-18", false));
    assert_eq!(record(&h, "series", rows).unwrap(), (10, 0, 0));
    // An adjustment: the next version of a November series' key, now
    // expiring in December, counted there alone.
    let moved = listed(AAPL, osi("AAPL  261120C00202000"), "2026-12-18", false);
    assert_eq!(record(&h, "series", vec![moved]).unwrap(), (0, 1, 0));

    let list = |underlying: &str, kind: InstrumentType, from: &str, to: &str, by: &str| {
        tokio::task::block_in_place(|| {
            h.lake.list_expirations(
                ListExpirationsRequest {
                    underlying_id: underlying.into(),
                    instrument_type: kind as i32,
                    expiration_from: from.into(),
                    expiration_to: to.into(),
                },
                &envelope(by),
            )
        })
        .unwrap()
    };
    let dates = |reply: &ListExpirationsReply| -> Vec<(String, u32)> {
        reply
            .expirations
            .iter()
            .map(|e| (e.expiration_date.clone(), e.series_count))
            .collect()
    };
    let every = list(AAPL, InstrumentType::Unspecified, "", "", "signal-1");
    assert_eq!(
        dates(&every),
        [
            ("2026-11-20".to_string(), 2),
            ("2026-12-18".to_string(), 3),
            ("2027-01-15".to_string(), 1)
        ]
    );
    assert!(every.unanswered.is_empty());
    assert_eq!(every.datasets.len(), 1);
    assert_eq!(every.datasets[0].dataset, CHAIN);
    assert!(every.watermark.is_some());
    // A window, inclusive.
    let december = list(
        AAPL,
        InstrumentType::ListedOption,
        "2026-12-01",
        "2026-12-31",
        "signal-1",
    );
    assert_eq!(dates(&december), [("2026-12-18".to_string(), 3)]);
    // A window with series listed only outside it: none, nothing unanswered.
    let empty = list(
        AAPL,
        InstrumentType::Unspecified,
        "2028-01-01",
        "",
        "signal-1",
    );
    assert!(empty.expirations.is_empty() && empty.unanswered.is_empty());
    // By type: the index's futures apart from its options; an option on a
    // future record is a future option.
    let futures = list(SPX, InstrumentType::Future, "", "", "signal-1");
    assert_eq!(
        dates(&futures),
        [("2026-12-18".to_string(), 1), ("2027-03-19".to_string(), 1)]
    );
    let options = list(SPX, InstrumentType::ListedOption, "", "", "signal-1");
    assert_eq!(dates(&options), [("2026-12-18".to_string(), 1)]);
    let on_es = list(ES, InstrumentType::FutureOption, "", "", "signal-1");
    assert_eq!(dates(&on_es), [("2026-12-18".to_string(), 1)]);
    assert!(list(ES, InstrumentType::ListedOption, "", "", "signal-1")
        .expirations
        .is_empty());
    assert!(list(AAPL, InstrumentType::Future, "", "", "signal-1")
        .expirations
        .is_empty());
    // A reader entitled to no dataset of series: named, never an empty list.
    let other = list(AAPL, InstrumentType::Unspecified, "", "", "other-1");
    assert!(other.expirations.is_empty());
    assert_eq!(other.unanswered.len(), 1);
    assert_eq!(
        other.unanswered[0].reason,
        UnansweredReason::NotEntitled as i32
    );

    // Refused naming the field, over the bus as a plugin's sidecar asks.
    let refused = |request: ListExpirationsRequest| {
        let h = &h;
        async move {
            asked::<_, ListExpirationsReply>(
                h,
                service::LIST_EXPIRATIONS,
                "meridian.v1.ListExpirationsRequest",
                request,
            )
            .await
            .map(|_| ())
            .map_err(|said| refused_naming(&said))
        }
    };
    assert_eq!(
        refused(ListExpirationsRequest::default()).await,
        Err(vec!["underlying_id".to_string()])
    );
    assert_eq!(
        refused(ListExpirationsRequest {
            underlying_id: AAPL.into(),
            instrument_type: InstrumentType::Index as i32,
            ..Default::default()
        })
        .await,
        Err(vec!["instrument_type".to_string()])
    );
    assert_eq!(
        refused(ListExpirationsRequest {
            underlying_id: AAPL.into(),
            expiration_from: "2026-12-18".into(),
            expiration_to: "2026-11-20".into(),
            ..Default::default()
        })
        .await,
        Err(vec!["expiration_to".to_string()])
    );
    assert_eq!(
        refused(ListExpirationsRequest {
            underlying_id: AAPL.into(),
            expiration_from: "2026-13-01".into(),
            ..Default::default()
        })
        .await,
        Err(vec!["expiration_from".to_string()])
    );
}

/// Nothing listed for the underlying: answered as ListSeries answers it,
/// its source asked for the chain (W10.7, ruling 856).
#[tokio::test(flavor = "multi_thread")]
async fn nothing_listed_is_answered_as_list_series_answers_it() {
    use meridian_domain::v1::ListExpirationsRequest;
    let h = harness();
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let reply = tokio::task::block_in_place(|| {
        h.lake.list_expirations(
            ListExpirationsRequest {
                underlying_id: AAPL.into(),
                ..Default::default()
            },
            &envelope("signal-1"),
        )
    })
    .unwrap();
    assert!(reply.expirations.is_empty());
    assert!(
        reply.unanswered.iter().any(|u| u.dataset == CHAIN),
        "{:?}",
        reply.unanswered
    );
    let want = next_want(&mut wanted)
        .await
        .expect("the chain asked of its source");
    assert!(want.all_series);
}
