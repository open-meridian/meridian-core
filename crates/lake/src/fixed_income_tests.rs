//! Fixed income in the lake (contract v29; W10.4 to W10.6; rulings 685 and
//! 900): a bond's print in percent of par with the yield beside it, a size
//! TRACE disseminates capped and its characteristics; a when-issued print
//! in yield alone; a spread over its benchmark; an ATS's firm quote with
//! bucketed sizes; and what W10.4 refuses of each, naming the field. The
//! instrument store stood in on the bus, as the lake's other tests stand it
//! in.

use std::sync::Arc;

use meridian_bus::{Bus, Envelope, MemoryBackend, MessageMeta};
use meridian_clock::ManualClock;
use meridian_domain::v1::{
    AssetClass, Compounding, DatasetEntitlement, DatasetRef, DayCount, EntitlementsChangedEvent,
    InstrumentRecord, InstrumentType, Money, ObservationMeta, Price, PriceBasis, PriceKind, Quote,
    QuoteCharacteristic, QuotedYield, RateConvention, ResolveIdentifierReply,
    ResolveIdentifierRequest, ResolveInstrumentReply, ResolveInstrumentRequest, Source, SubjectRef,
    Trade, TradeAttributes, TradeCharacteristic, TradesRecordedEvent, UnansweredReason, UnitValue,
    ValueUnit, YieldKind,
};
use meridian_pb::v1::{DatasetDeclaration, DatasetLicence, Decimal, ObservationMode};
use prost::Message;

use crate::row::{DataType, Observation};
use crate::service::{self, Lake, ReadRequest};

const NOW: i64 = 1_793_577_600_000_000_000; // 2026-11-02T00:00Z
const MINUTE: i64 = 60 * 1_000_000_000;
/// A corporate bond, a Treasury on the run its spreads are over, and an
/// agency pool, as the harness platform mints them.
pub(crate) const BOND: &str = "INS-01JBOND00000000000000000001";
pub(crate) const UST: &str = "INS-01JUST000000000000000000010";
pub(crate) const POOL: &str = "INS-01JPOOL00000000000000000001";
const USD: &str = "INS-USD";
/// An alternative trading system, a venue of the venue master by its MIC.
const ATS: &str = "VEN-01JC29ATS00000000000000001";
const TRACE: &str = "finra-1:trace";
const QUOTES: &str = "finra-1:ats_quotes";

pub(crate) struct Harness {
    pub(crate) bus: Arc<Bus>,
    pub(crate) lake: Arc<Lake>,
}

/// The lake over a stand-in instrument store holding the bond, the pool and
/// the Treasury, dollars by their code; `datasets` configured.
pub(crate) fn harness(configuration: EntitlementsChangedEvent) -> Harness {
    let clock = Arc::new(ManualClock::at(NOW));
    let bus = Arc::new(Bus::single(
        "lake-1",
        Arc::new(MemoryBackend::new()),
        clock.clone(),
    ));
    bus.serve(service::RESOLVE_INSTRUMENT, |envelope| {
        let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        let held = match asked.instrument_id.as_str() {
            BOND | UST => Some((AssetClass::Debt, InstrumentType::Bond)),
            POOL | "INS-01JPOOL00000000000000000002" => {
                Some((AssetClass::Debt, InstrumentType::MbsPool))
            }
            USD => Some((AssetClass::Cash, InstrumentType::Unspecified)),
            _ => None,
        };
        Ok((
            "meridian.v1.ResolveInstrumentReply".into(),
            ResolveInstrumentReply {
                found: held.is_some(),
                instrument: held.map(|(class, kind)| InstrumentRecord {
                    instrument_id: asked.instrument_id.clone(),
                    asset_class: class as i32,
                    instrument_type: kind as i32,
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
    lake.configured(&configuration);
    Harness { bus, lake }
}

/// A dataset of `instance` serving `types`, kept, its day UTC; `reader`
/// entitled to it with `fields` (none for every field).
pub(crate) fn dataset(
    id: &str,
    types: &[&str],
    modes: &[ObservationMode],
    reader: &str,
    fields: &[&str],
) -> (DatasetRef, DatasetEntitlement) {
    let (instance, key) = id.split_once(':').unwrap();
    (
        DatasetRef {
            dataset: id.into(),
            instance: instance.into(),
            vendor: "FINRA".into(),
            declaration: Some(DatasetDeclaration {
                key: key.into(),
                vendor: "FINRA".into(),
                data_types: types.iter().map(|t| t.to_string()).collect(),
                modes: modes.iter().map(|m| *m as i32).collect(),
                day_time_zone: "Etc/UTC".into(),
                licence_default: Some(DatasetLicence {
                    kept: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        DatasetEntitlement {
            dataset: id.into(),
            instance: reader.into(),
            allowed: true,
            fields: fields.iter().map(|f| f.to_string()).collect(),
            ..Default::default()
        },
    )
}

fn configuration(quote_fields: &[&str]) -> EntitlementsChangedEvent {
    let (trace, trace_reader) = dataset(
        TRACE,
        &["meridian.v1.Trade", "meridian.v1.Price"],
        &[ObservationMode::Push],
        "signal-1",
        &[],
    );
    let (quotes, quotes_reader) = dataset(
        QUOTES,
        &["meridian.v1.Quote"],
        &[ObservationMode::Push],
        "signal-1",
        quote_fields,
    );
    EntitlementsChangedEvent {
        datasets: vec![trace, quotes],
        licences: vec![],
        entitlements: vec![trace_reader, quotes_reader],
        changed_at_ns: NOW,
    }
}

pub(crate) fn envelope(instance: &str) -> Envelope {
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

pub(crate) fn decimal(text: &str) -> Option<Decimal> {
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

/// A yield as TRACE disseminates one: semi-annual bond-equivalent on 30/360.
fn quoted(percent: &str, kind: YieldKind, to_date: &str) -> Option<QuotedYield> {
    Some(QuotedYield {
        value: Some(UnitValue {
            value: decimal(percent),
            unit: ValueUnit::Percent as i32,
            ..Default::default()
        }),
        convention: Some(RateConvention {
            day_count: DayCount::Thirty360BondBasis as i32,
            compounding: Compounding::Compounded as i32,
            periods_per_year: 2,
        }),
        kind: kind as i32,
        to_date: to_date.into(),
    })
}

fn meta(dataset: &str, row_key: &str, subject: &str, venue: &str, at: i64) -> ObservationMeta {
    ObservationMeta {
        row_key: row_key.into(),
        subjects: vec![SubjectRef {
            entity_id: subject.into(),
            ..Default::default()
        }],
        source: Some(Source {
            instance: "finra-1".into(),
            dataset: dataset.into(),
            venue_id: venue.into(),
            ..Default::default()
        }),
        valid_from_ns: at,
        ..Default::default()
    }
}

/// TRACE's print of the bond: 5MM+ at 98.375 per 100 of face, a dealer the
/// contra and the reporting dealer principal, its yield to worst beside it.
fn print(row_key: &str) -> Trade {
    Trade {
        meta: Some(meta(TRACE, row_key, BOND, "", NOW + MINUTE)),
        price: usd("98.375"),
        quantity: decimal("5000000"),
        quantity_capped: true,
        basis: PriceBasis::PercentOfPar as i32,
        r#yield: quoted("5.093", YieldKind::ToWorst, "2034-02-05"),
        attributes: Some(TradeAttributes {
            characteristics: vec![
                TradeCharacteristic::ContraDealer as i32,
                TradeCharacteristic::PrincipalCapacity as i32,
            ],
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn record(h: &Harness, field: &str, rows: Vec<Observation>) -> Result<(u32, u32, u32), String> {
    tokio::task::block_in_place(|| h.lake.record(field, rows, "", &envelope("finra-1")))
        .map(|reply| (reply.recorded, reply.restated, reply.unchanged))
}

fn read(h: &Harness, data_type: DataType, subject: &str) -> service::ReadAnswer {
    let real_time = data_type.real_time() && data_type == DataType::Trade;
    tokio::task::block_in_place(|| {
        h.lake.read(
            ReadRequest {
                data_type,
                subjects: vec![SubjectRef {
                    entity_id: subject.into(),
                    ..Default::default()
                }],
                kinds: vec![],
                interval_ns: 0,
                sources: None,
                at_ns: 0,
                business_date: String::new(),
                valid_from_ns: if real_time { NOW } else { 0 },
                valid_until_ns: if real_time { NOW + 60 * MINUTE } else { 0 },
                as_of_ns: 0,
                page_size: 0,
                cursor: String::new(),
                after: None,
                tenors: Vec::new(),
                published_as_of_ns: 0,
                series_keys: Vec::new(),
                all_series: false,
                dates: None,
                window: Default::default(),
                fields: Vec::new(),
            },
            &envelope("signal-1"),
        )
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bonds_print_keeps_its_basis_yield_cap_and_characteristics_and_a_new_yield_restates_it() {
    let h = harness(configuration(&[]));
    let mut heard = h
        .bus
        .subscribe_every(&format!("platform.lake.{TRACE}.event.trades-recorded"));
    assert_eq!(
        record(&h, "trades", vec![Observation::Trade(print("T-1"))]).unwrap(),
        (1, 0, 0)
    );
    let published = TradesRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..])
        .unwrap()
        .trade
        .unwrap();
    assert!(published.quantity_capped, "5MM+ is read as at least");
    assert_eq!(published.basis, PriceBasis::PercentOfPar as i32);
    assert_eq!(
        published.r#yield.as_ref().unwrap().kind,
        YieldKind::ToWorst as i32
    );
    let answer = read(&h, DataType::Trade, BOND);
    let Observation::Trade(kept) = &answer.rows[0] else {
        panic!("a trade: {:?}", answer.rows)
    };
    assert_eq!(
        kept.attributes.as_ref().unwrap().characteristics,
        vec![
            TradeCharacteristic::ContraDealer as i32,
            TradeCharacteristic::PrincipalCapacity as i32
        ]
    );
    assert_eq!(kept.price.as_ref().unwrap().instrument_id, USD);
    // The same print again is a repeat; its yield corrected, a restatement,
    // never counted unchanged.
    assert_eq!(
        record(&h, "trades", vec![Observation::Trade(print("T-1"))]).unwrap(),
        (0, 0, 1)
    );
    let mut corrected = print("T-1");
    corrected.r#yield = quoted("5.094", YieldKind::ToWorst, "2034-02-05");
    assert_eq!(
        record(&h, "trades", vec![Observation::Trade(corrected)]).unwrap(),
        (0, 1, 0)
    );
    let mut uncapped = print("T-1");
    uncapped.quantity_capped = false;
    uncapped.r#yield = quoted("5.094", YieldKind::ToWorst, "2034-02-05");
    assert_eq!(
        record(&h, "trades", vec![Observation::Trade(uncapped)]).unwrap(),
        (0, 1, 0),
        "a cap lifted is a restatement"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_when_issued_print_in_yield_and_a_spread_over_its_benchmark_need_no_money_price() {
    let h = harness(configuration(&[]));
    let mut when_issued = print("T-WI");
    when_issued.price = None;
    when_issued.quantity_capped = false;
    when_issued.basis = PriceBasis::Yield as i32;
    when_issued.r#yield = quoted("4.250", YieldKind::ToMaturity, "");
    when_issued.attributes = Some(TradeAttributes {
        characteristics: vec![TradeCharacteristic::WhenIssued as i32],
        ..Default::default()
    });
    let mut spread = print("T-SP");
    spread.price = None;
    spread.r#yield = None;
    spread.basis = PriceBasis::Spread as i32;
    spread.spread_bps = decimal("112.5");
    spread.benchmark_instrument_id = UST.into();
    assert_eq!(
        record(
            &h,
            "trades",
            vec![Observation::Trade(when_issued), Observation::Trade(spread)]
        )
        .unwrap(),
        (2, 0, 0)
    );
    // A price in yield, its money price unset: Treasury's high yield.
    let high_yield = Price {
        meta: Some(ObservationMeta {
            business_date: "2026-11-02".into(),
            ..meta(TRACE, "P-HY", UST, "", NOW)
        }),
        kind: PriceKind::Close as i32,
        basis: PriceBasis::Yield as i32,
        r#yield: quoted("4.180", YieldKind::ToMaturity, ""),
        ..Default::default()
    };
    assert_eq!(
        record(&h, "prices", vec![Observation::Price(high_yield)]).unwrap(),
        (1, 0, 0)
    );
    let answer = read(&h, DataType::Price, UST);
    let Observation::Price(kept) = &answer.rows[0] else {
        panic!("a price: {:?}", answer.rows)
    };
    assert!(kept.price.is_none() && kept.r#yield.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn what_w10_4_refuses_of_a_fixed_income_row_is_refused_naming_the_field() {
    let h = harness(configuration(&[]));
    let refused = |change: &dyn Fn(&mut Trade)| {
        let mut row = print("T-X");
        change(&mut row);
        record(&h, "trades", vec![Observation::Trade(row)]).unwrap_err()
    };
    let words = refused(&|row| {
        row.basis = PriceBasis::Yield as i32;
        row.r#yield = None;
    });
    assert!(words.starts_with("trades[0].yield"), "{words}");
    let words = refused(&|row| {
        row.basis = PriceBasis::Yield as i32;
        row.r#yield = None;
        row.price = None;
    });
    assert!(words.starts_with("trades[0].yield"), "{words}");
    let words = refused(&|row| row.r#yield.as_mut().unwrap().kind = 0);
    assert!(words.starts_with("trades[0].yield.kind"), "{words}");
    let words = refused(&|row| row.r#yield.as_mut().unwrap().convention = None);
    assert!(words.starts_with("trades[0].yield.convention"), "{words}");
    let words = refused(&|row| row.spread_bps = decimal("50"));
    assert!(words.starts_with("trades[0].spread_bps"), "{words}");
    let words = refused(&|row| {
        row.basis = PriceBasis::Spread as i32;
        row.spread_bps = decimal("50");
    });
    assert!(
        words.starts_with("trades[0].benchmark_instrument_id"),
        "{words}"
    );
    let words = refused(&|row| {
        row.basis = PriceBasis::Spread as i32;
        row.spread_bps = decimal("50");
        row.benchmark_instrument_id = "INS-01JNOTHELD000000000000000".into();
    });
    assert!(words.contains("no record the deployment holds"), "{words}");
    let words = refused(&|row| row.basis = 9);
    assert!(words.starts_with("trades[0].basis"), "{words}");
    let words = refused(&|row| {
        row.r#yield = None;
        row.benchmark_instrument_id = UST.into();
    });
    assert!(
        words.starts_with("trades[0].benchmark_instrument_id"),
        "{words}"
    );
    // A price in spread with no spread; a quote's capped size stating no
    // floor.
    let spreadless = Price {
        meta: Some(ObservationMeta {
            business_date: "2026-11-02".into(),
            ..meta(TRACE, "P-S", BOND, "", NOW)
        }),
        kind: PriceKind::Close as i32,
        basis: PriceBasis::Spread as i32,
        benchmark_instrument_id: UST.into(),
        ..Default::default()
    };
    let words = record(&h, "prices", vec![Observation::Price(spreadless)]).unwrap_err();
    assert!(words.starts_with("prices[0].price"), "{words}");
    let mut floorless = firm_quote("Q-X");
    floorless.ask_quantity = None;
    let words = record(&h, "quotes", vec![Observation::Quote(floorless)]).unwrap_err();
    assert!(words.starts_with("quotes[0].ask_size_capped"), "{words}");
}

/// An ATS's firm quote (ruling 900 R6): the ATS its venue, each size a
/// bucket's floor read as at least (R2), and each side's yield beside it.
fn firm_quote(row_key: &str) -> Quote {
    Quote {
        meta: Some(meta(QUOTES, row_key, BOND, ATS, NOW + 2 * MINUTE)),
        bid: usd("98.25"),
        ask: usd("98.50"),
        bid_quantity: decimal("1000000"),
        ask_quantity: decimal("1000000"),
        bid_size_capped: true,
        ask_size_capped: true,
        basis: PriceBasis::PercentOfPar as i32,
        bid_yield: quoted("5.110", YieldKind::ToWorst, "2034-02-05"),
        ask_yield: quoted("5.076", YieldKind::ToWorst, "2034-02-05"),
        characteristics: vec![],
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ats_firm_quote_is_a_quote_on_its_venue_its_sizes_at_least_and_its_yields_cut_by_field()
{
    // signal-1 may read the quote's prices and sizes, not its yields.
    let h = harness(configuration(&[
        "meridian.v1.Quote.bid",
        "meridian.v1.Quote.ask",
        "meridian.v1.Quote.bid_quantity",
        "meridian.v1.Quote.ask_quantity",
        "meridian.v1.Quote.bid_size_capped",
        "meridian.v1.Quote.ask_size_capped",
        "meridian.v1.Quote.basis",
        "meridian.v1.Quote.characteristics",
        "meridian.v1.Quote.bid_spread_bps",
        "meridian.v1.Quote.ask_spread_bps",
        "meridian.v1.Quote.benchmark_instrument_id",
    ]));
    assert_eq!(
        record(&h, "quotes", vec![Observation::Quote(firm_quote("Q-1"))]).unwrap(),
        (1, 0, 0)
    );
    let answer = read(&h, DataType::Quote, BOND);
    let Observation::Quote(kept) = &answer.rows[0] else {
        panic!("a quote: {:?}", answer.rows)
    };
    assert_eq!(
        kept.meta
            .as_ref()
            .unwrap()
            .source
            .as_ref()
            .unwrap()
            .venue_id,
        ATS
    );
    assert!(kept.bid_size_capped && kept.ask_size_capped);
    assert!(kept.characteristics.is_empty(), "none is firm and trading");
    assert!(
        kept.bid_yield.is_none() && kept.ask_yield.is_none(),
        "the yields stripped"
    );
    let named: Vec<&str> = answer
        .unanswered
        .iter()
        .filter(|u| u.reason == UnansweredReason::NotEntitled as i32)
        .map(|u| u.field.as_str())
        .collect();
    assert_eq!(
        named,
        vec!["meridian.v1.Quote.bid_yield", "meridian.v1.Quote.ask_yield"]
    );
    // An indicative quote says so, as any quote's state does.
    let mut indicative = firm_quote("Q-2");
    indicative.characteristics = vec![QuoteCharacteristic::Indicative as i32];
    assert_eq!(
        record(&h, "quotes", vec![Observation::Quote(indicative)]).unwrap(),
        (1, 0, 0)
    );
}

// ── The factor (contract v29, ruling 680; W10.4 to W10.7) ──

const FANNIE: &str = "gse-disclosures-1:fannie_mae";
const GINNIE: &str = "ginnie-mae-1:factors";
/// A pool the deployment holds and no source covers.
const UNCOVERED: &str = "INS-01JPOOL00000000000000000002";

/// Fannie Mae's factors, pushed and asked for, kept, read by servicing-1;
/// Ginnie Mae's beside it, read by nobody.
fn factor_configuration() -> EntitlementsChangedEvent {
    let (mut fannie, reader) = dataset(
        FANNIE,
        &["meridian.v1.Factor"],
        &[ObservationMode::Push, ObservationMode::Pull],
        "servicing-1",
        &[],
    );
    // Its files are read daily: what keeps a want current.
    fannie.declaration.as_mut().unwrap().cadence = 86_400;
    let (ginnie, _) = dataset(
        GINNIE,
        &["meridian.v1.Factor"],
        &[ObservationMode::Push],
        "nobody-1",
        &[],
    );
    EntitlementsChangedEvent {
        datasets: vec![fannie, ginnie],
        licences: vec![],
        entitlements: vec![reader],
        changed_at_ns: NOW,
    }
}

/// The fixture's factor: Fannie Mae's for a UMBS pool, its month, record
/// and payment dates, published on the fourth business day.
fn factor(subject: &str, month: &str, value: &str, payment: &str) -> meridian_domain::v1::Factor {
    meridian_domain::v1::Factor {
        meta: Some(ObservationMeta {
            row_key: format!("FNMA:MA5123:{month}"),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
                ..Default::default()
            }],
            source: Some(Source {
                instance: "gse-disclosures-1".into(),
                dataset: FANNIE.into(),
                ..Default::default()
            }),
            valid_from_ns: NOW,
            business_date: "2026-11-04".into(),
            ..Default::default()
        }),
        factor: decimal(value),
        factor_month: month.into(),
        record_date: "2026-10-31".into(),
        payment_date: payment.into(),
        coupon_rate: None,
        wac: None,
        wam: None,
        wala: None,
    }
}

fn record_factors(
    h: &Harness,
    rows: Vec<meridian_domain::v1::Factor>,
) -> Result<(u32, u32, u32), String> {
    tokio::task::block_in_place(|| {
        h.lake.record(
            "factors",
            rows.into_iter().map(Observation::Factor).collect(),
            "",
            &envelope("gse-disclosures-1"),
        )
    })
    .map(|reply| (reply.recorded, reply.restated, reply.unchanged))
}

fn list_factors(
    h: &Harness,
    request: meridian_domain::v1::ListFactorsRequest,
) -> Result<meridian_domain::v1::ListFactorsReply, String> {
    tokio::task::block_in_place(|| h.lake.list_factors(request, &envelope("servicing-1")))
}

fn subjects(ids: &[&str]) -> Vec<SubjectRef> {
    ids.iter()
        .map(|id| SubjectRef {
            entity_id: (*id).into(),
            ..Default::default()
        })
        .collect()
}

fn values(reply: &meridian_domain::v1::ListFactorsReply) -> Vec<(String, String, u64)> {
    reply
        .factors
        .iter()
        .map(|f| {
            (
                f.factor_month.clone(),
                meridian_domain::exact::Exact::from_wire(f.factor.as_ref().unwrap())
                    .unwrap()
                    .to_string(),
                f.meta.as_ref().unwrap().version,
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pools_factor_is_recorded_heard_every_version_and_read_by_payment_date_or_watermark() {
    use meridian_domain::v1::{
        FactorsRecordedEvent, ListFactorsRequest, PartitionSequence, Watermark,
    };
    let h = harness(factor_configuration());
    let mut heard = h
        .bus
        .subscribe_every(&format!("platform.lake.{FANNIE}.event.factors-recorded"));
    assert_eq!(
        record_factors(
            &h,
            vec![
                factor(POOL, "202609", "0.84017733", "2026-10-27"),
                factor(POOL, "202610", "0.83102210", "2026-11-25"),
            ]
        )
        .unwrap(),
        (2, 0, 0)
    );
    let first = FactorsRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..])
        .unwrap()
        .factor
        .unwrap();
    assert_eq!(first.factor_month, "202609");
    heard.recv().await.unwrap();

    // A year of the pool's factors by payment date; November's alone.
    let year = list_factors(
        &h,
        ListFactorsRequest {
            subjects: subjects(&[POOL]),
            from_date: "2025-11-01".into(),
            to_date: "2026-11-30".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        values(&year),
        vec![
            ("202609".into(), "0.84017733".into(), 1),
            ("202610".into(), "0.83102210".into(), 1)
        ]
    );
    assert_eq!(year.datasets[0].dataset, FANNIE);
    let november = list_factors(
        &h,
        ListFactorsRequest {
            subjects: subjects(&[POOL]),
            from_date: "2026-11-01".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(november.factors.len(), 1);
    let watermark = november.watermark.clone().unwrap();

    // Republished: the next version of its row key, heard, never conflated,
    // and the read answers it; a reader that lost it catches up after the
    // watermark it last saw, whatever the dates.
    assert_eq!(
        record_factors(&h, vec![factor(POOL, "202610", "0.83102211", "2026-11-25")]).unwrap(),
        (0, 1, 0)
    );
    let republished =
        FactorsRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..])
            .unwrap()
            .factor
            .unwrap();
    assert_eq!(republished.meta.as_ref().unwrap().version, 2);
    let caught_up = list_factors(
        &h,
        ListFactorsRequest {
            subjects: subjects(&[POOL]),
            after_watermark: Some(watermark.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        values(&caught_up),
        vec![("202610".into(), "0.83102211".into(), 2)]
    );
    let every = list_factors(
        &h,
        ListFactorsRequest {
            subjects: subjects(&[POOL]),
            after_watermark: Some(Watermark {
                partitions: vec![PartitionSequence {
                    partition: FANNIE.into(),
                    sequence: 0,
                }],
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(every.factors.len(), 3, "every version after the watermark");
    let refused = list_factors(
        &h,
        ListFactorsRequest {
            subjects: subjects(&[POOL]),
            after_watermark: Some(watermark),
            from_date: "2026-11-01".into(),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(refused.contains("after_watermark"), "{refused}");
    // A page holds at most 500 rows, as every lake read's.
    let paged = list_factors(
        &h,
        ListFactorsRequest {
            subjects: subjects(&[POOL]),
            page_size: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(paged.factors.len(), 1);
    assert!(!paged.next_cursor.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pool_no_source_covers_is_unanswered_never_a_factor_of_one_and_an_invalid_one_quarantined(
) {
    use meridian_domain::v1::{ListFactorsRequest, QualityReason, QualityState};
    let h = harness(factor_configuration());
    // Nothing recorded: the source is asked, standing, and nothing answers.
    let mut wanted = h.bus.subscribe_every(service::OBSERVATIONS_WANTED);
    let nothing = list_factors(
        &h,
        ListFactorsRequest {
            subjects: subjects(&[UNCOVERED]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(nothing.factors.is_empty());
    assert_eq!(
        nothing
            .unanswered
            .iter()
            .map(|u| (u.dataset.as_str(), u.reason))
            .collect::<Vec<_>>(),
        vec![(FANNIE, UnansweredReason::AskedSource as i32)]
    );
    let want = meridian_domain::v1::ObservationsWantedEvent::decode(
        &wanted.recv().await.unwrap().envelope.payload[..],
    )
    .unwrap();
    assert_eq!(want.data_type, "meridian.v1.Factor");
    assert!(want.standing, "a pool held is wanted standing (W10.7)");

    // The pool's only row invalid: left out, named quarantined.
    let mut judged = factor(POOL, "202610", "8.3102210", "2026-11-25");
    let meta = judged.meta.as_mut().unwrap();
    meta.quality = QualityState::Invalid as i32;
    meta.quality_reasons = vec![QualityReason::PriceOutlier as i32];
    record_factors(&h, vec![judged]).unwrap();
    let quarantined = list_factors(
        &h,
        ListFactorsRequest {
            subjects: subjects(&[POOL]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(quarantined.factors.is_empty());
    assert!(quarantined
        .unanswered
        .iter()
        .any(|u| u.dataset == FANNIE && u.reason == UnansweredReason::Quarantined as i32));
}

#[tokio::test(flavor = "multi_thread")]
async fn what_w10_4_refuses_of_a_factor_is_refused_naming_the_field() {
    let h = harness(factor_configuration());
    let refused = |change: &dyn Fn(&mut meridian_domain::v1::Factor)| {
        let mut row = factor(POOL, "202610", "0.83102210", "2026-11-25");
        change(&mut row);
        record_factors(&h, vec![row]).unwrap_err()
    };
    let words = refused(&|row| row.factor_month.clear());
    assert!(words.starts_with("factors[0].factor_month"), "{words}");
    let words = refused(&|row| row.factor_month = "202613".into());
    assert!(words.starts_with("factors[0].factor_month"), "{words}");
    let words = refused(&|row| row.factor = decimal("-0.1"));
    assert!(words.starts_with("factors[0].factor"), "{words}");
    let words = refused(&|row| row.factor = None);
    assert!(words.starts_with("factors[0].factor"), "{words}");
    let words = refused(&|row| row.payment_date = "2026-11-31".into());
    assert!(words.contains("factors[0].payment_date"), "{words}");
    // A bond's record is no pool; a record the deployment does not hold is
    // never minted by a factor.
    let words = refused(&|row| row.meta.as_mut().unwrap().subjects[0].entity_id = BOND.into());
    assert!(words.contains("no pool or REMIC class"), "{words}");
    let words = refused(&|row| {
        row.meta.as_mut().unwrap().subjects[0].entity_id = "INS-01JNOTHELD000000000000000".into()
    });
    assert!(words.contains("no record the deployment holds"), "{words}");
    // An accreting Z class's factor is above 1, and a floater's coupon is
    // the period's.
    let mut z = factor(POOL, "202610", "1.0425", "2026-11-25");
    z.coupon_rate = decimal("5.125");
    assert_eq!(record_factors(&h, vec![z]).unwrap(), (1, 0, 0));
}
