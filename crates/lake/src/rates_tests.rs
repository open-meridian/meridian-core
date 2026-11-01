//! The rates slice (contract v27; spec/a-public-rates-and-filings-data-plugin
//! §1 items 1 to 3 and 5, §6's rates cases): a rate recorded, revised,
//! confirmed, read as a curve for a date, by tenor, by convention, as the
//! source had published it, and from another source under the priority; and
//! the refusals W10.4 names. The instrument store and the conductor stood in
//! on the bus, as the lake's other tests stand them in.

use std::sync::Arc;

use meridian_bus::{Bus, Envelope, MemoryBackend, MessageMeta};
use meridian_clock::{Clock, ManualClock};
use meridian_domain::v1::{
    AssetClass, Compounding, DatasetEntitlement, DatasetRef, DayCount, EntitlementsChangedEvent,
    InstrumentRecord, InstrumentType, ListRatesRequest, Money, ObservationMeta, QueryLakeRequest,
    Rate, RateConvention, RateKind, RatePercentile, RatesRecordedEvent, ResolveIdentifierReply,
    ResolveInstrumentReply, ResolveInstrumentRequest, SetSourcePriorityRequest, Source,
    SourceChoice, SourceTime, SourceTimeKind, SubjectRef, Tenor, TenorUnit, UnansweredReason,
    UnitValue, ValueUnit,
};
use meridian_pb::v1::{DatasetDeclaration, DatasetLicence, Decimal, ObservationMode};
use prost::Message;

use crate::row::Observation;
use crate::service::{self, Lake};

const NOW: i64 = 1_791_417_600_000_000_000; // 2026-10-08T00:00Z
const HOUR: i64 = 3_600 * 1_000_000_000;
const DAY: i64 = 24 * HOUR;
const SOFR: &str = "INS-01JC27SOFR000000000000001";
const SOFR_INDEX: &str = "INS-01JC27SOFRINDEX000000001";
const PAR: &str = "INS-01JC27USTPAR00000000000001";
const BILLS: &str = "INS-01JC27USTBILLS0000000001";
const AAPL: &str = "INS-01JC27AAPL000000000000001";
const NYFED: &str = "ny-fed-1:nyfed_rates";
const UST: &str = "us-treasury-1:ust_par";
const FRED: &str = "fred-1:fred_public";

struct Harness {
    clock: Arc<ManualClock>,
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
    // The instrument store, stood in: referential records (W1.1) and one
    // equity, which no rate names.
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
            SOFR => held(AssetClass::Referential, InstrumentType::InterestRate),
            SOFR_INDEX => held(AssetClass::Referential, InstrumentType::Index),
            PAR | BILLS => held(AssetClass::Referential, InstrumentType::RateCurve),
            AAPL => held(AssetClass::Equity, InstrumentType::Unspecified),
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
    bus.serve(service::RESOLVE_IDENTIFIER, |_| {
        Ok((
            "meridian.v1.ResolveIdentifierReply".into(),
            ResolveIdentifierReply {
                found: true,
                instrument_id: "INS-USD".into(),
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
    let lake = service::serve(bus.clone(), store, clock.clone());
    lake.configured(&configuration());
    Harness { clock, bus, lake }
}

/// Three rates datasets, pulled daily in New York; signal-1 entitled to
/// every one, every field.
fn configuration() -> EntitlementsChangedEvent {
    let dataset = |id: &str| {
        let (instance, key) = id.split_once(':').unwrap();
        DatasetRef {
            dataset: id.into(),
            instance: instance.into(),
            vendor: "Public".into(),
            declaration: Some(DatasetDeclaration {
                key: key.into(),
                vendor: "Public".into(),
                data_types: vec!["meridian.v1.Rate".into()],
                modes: vec![ObservationMode::Pull as i32],
                cadence: 86_400,
                day_time_zone: "America/New_York".into(),
                licence_default: Some(DatasetLicence {
                    kept: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    };
    let entitled = |id: &str| DatasetEntitlement {
        dataset: id.into(),
        instance: "signal-1".into(),
        allowed: true,
        ..Default::default()
    };
    EntitlementsChangedEvent {
        datasets: vec![dataset(NYFED), dataset(UST), dataset(FRED)],
        licences: vec![],
        entitlements: vec![entitled(NYFED), entitled(UST), entitled(FRED)],
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

fn value_of(rate: &Rate) -> String {
    meridian_domain::exact::Exact::from_wire(rate.value.as_ref().unwrap().value.as_ref().unwrap())
        .unwrap()
        .to_string()
}

fn money_market() -> Option<RateConvention> {
    Some(RateConvention {
        day_count: DayCount::Act360 as i32,
        compounding: Compounding::Simple as i32,
        periods_per_year: 0,
    })
}

fn bond_equivalent() -> Option<RateConvention> {
    Some(RateConvention {
        day_count: DayCount::ActAct as i32,
        compounding: Compounding::Compounded as i32,
        periods_per_year: 2,
    })
}

fn tenor(count: i32, unit: TenorUnit) -> Option<Tenor> {
    Some(Tenor {
        count,
        unit: unit as i32,
    })
}

/// A rate as the SDK's helpers make it: its row key the series, kind, point,
/// convention and day; valid at the business day; published at `published`.
#[allow(clippy::too_many_arguments)]
fn rate(
    dataset: &str,
    subject: &str,
    key: &str,
    kind: RateKind,
    value: &str,
    date: &str,
    published: i64,
    raw: &str,
) -> Rate {
    let instance = dataset.split_once(':').unwrap().0;
    Rate {
        meta: Some(ObservationMeta {
            row_key: key.into(),
            subjects: vec![SubjectRef {
                entity_id: subject.into(),
                ..Default::default()
            }],
            source: Some(Source {
                instance: instance.into(),
                dataset: dataset.into(),
                ..Default::default()
            }),
            valid_from_ns: NOW - DAY,
            business_date: date.into(),
            source_times: vec![SourceTime {
                kind: SourceTimeKind::Published as i32,
                at_ns: published,
            }],
            raw: Some(meridian_pb::v1::RawRecordRef {
                instance_id: instance.into(),
                key: raw.into(),
            }),
            ..Default::default()
        }),
        kind: kind as i32,
        value: Some(UnitValue {
            value: decimal(value),
            unit: ValueUnit::Percent as i32,
            ..Default::default()
        }),
        convention: money_market(),
        ..Default::default()
    }
}

fn sofr(value: &str, published: i64, raw: &str) -> Rate {
    Rate {
        volume: Some(Money {
            amount: decimal("2412000000000"),
            currency_code: "USD".into(),
            instrument_id: String::new(),
        }),
        percentiles: [(1, "4.27"), (25, "4.29"), (75, "4.33"), (99, "4.40")]
            .into_iter()
            .map(|(percentile, value)| RatePercentile {
                percentile,
                value: decimal(value),
            })
            .collect(),
        ..rate(
            NYFED,
            SOFR,
            "SOFR:fixing:-:act_360/simple:2026-10-07@",
            RateKind::Fixing,
            value,
            "2026-10-07",
            published,
            raw,
        )
    }
}

fn par(dataset: &str, label: &str, count: i32, unit: TenorUnit, value: &str) -> Rate {
    Rate {
        tenor: tenor(count, unit),
        convention: bond_equivalent(),
        ..rate(
            dataset,
            PAR,
            &format!("PAR:par_yield:{label}:act_act/compounded/2:2026-10-07@{PAR}"),
            RateKind::ParYield,
            value,
            "2026-10-07",
            NOW - 6 * HOUR,
            &format!("par/2026-10-07/{label}"),
        )
    }
}

fn record(
    h: &Harness,
    instance: &str,
    rates: Vec<Rate>,
) -> Result<meridian_domain::v1::RecordObservationsReply, String> {
    tokio::task::block_in_place(|| {
        h.lake.record(
            "rates",
            rates.into_iter().map(Observation::Rate).collect(),
            "",
            &envelope(instance, true, ""),
        )
    })
}

fn list(h: &Harness, request: ListRatesRequest) -> meridian_domain::v1::ListRatesReply {
    tokio::task::block_in_place(|| h.lake.list_rates(request, &envelope("signal-1", true, "")))
        .unwrap()
}

fn subjects(ids: &[&str]) -> Vec<SubjectRef> {
    ids.iter()
        .map(|id| SubjectRef {
            entity_id: id.to_string(),
            ..Default::default()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rate_fixing_is_recorded_heard_and_read() {
    // a-rate-fixing: SOFR's fixing with its volume and percentiles.
    let h = harness();
    let mut heard = h
        .bus
        .subscribe_every(&format!("platform.lake.{NYFED}.event.rates-recorded"));
    let reply = record(&h, "ny-fed-1", vec![sofr("4.31", NOW - 2 * HOUR, "sofr/1")]).unwrap();
    assert_eq!((reply.recorded, reply.restated, reply.unchanged), (1, 0, 0));
    let event =
        RatesRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..]).unwrap();
    let heard = event.rate.unwrap();
    assert_eq!(value_of(&heard), "4.31");
    assert_eq!(heard.meta.as_ref().unwrap().version, 1);
    assert_eq!(
        heard.volume.as_ref().unwrap().instrument_id,
        "INS-USD",
        "the volume's code resolved to its cash instrument"
    );

    let read = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR]),
            ..Default::default()
        },
    );
    assert_eq!(read.rates.len(), 1);
    assert_eq!(read.rates[0].percentiles.len(), 4);
    assert_eq!(read.rates[0].kind, RateKind::Fixing as i32);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_same_day_revision_is_the_next_version_and_a_confirmation_no_restatement() {
    // a-same-day-revision, and the fixture's confirmation case.
    let h = harness();
    let first_published = NOW - 2 * HOUR;
    record(
        &h,
        "ny-fed-1",
        vec![sofr("4.31", first_published, "sofr/1")],
    )
    .unwrap();
    let before_revision = h.clock.now_ns();
    h.clock.advance(HOUR);
    let revised_published = NOW + HOUR / 2;
    let revised = record(
        &h,
        "ny-fed-1",
        vec![sofr("4.32", revised_published, "sofr/1")],
    )
    .unwrap();
    assert_eq!(
        (revised.recorded, revised.restated, revised.unchanged),
        (0, 1, 0)
    );

    // The same values, only the raw reference changed: recorded, counted
    // unchanged, never a restatement.
    h.clock.advance(HOUR);
    let confirmed = record(
        &h,
        "ny-fed-1",
        vec![sofr("4.32", revised_published, "sofr/2")],
    )
    .unwrap();
    assert_eq!(
        (confirmed.recorded, confirmed.restated, confirmed.unchanged),
        (0, 0, 1)
    );
    // The same again changes nothing at all.
    let again = record(
        &h,
        "ny-fed-1",
        vec![sofr("4.32", revised_published, "sofr/2")],
    )
    .unwrap();
    assert_eq!((again.recorded, again.restated, again.unchanged), (0, 0, 1));
    assert_eq!(
        confirmed.watermark.unwrap().partitions[0].sequence,
        3,
        "the confirmation is recorded, the repeat is not"
    );

    let now = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR]),
            business_date: "2026-10-07".into(),
            ..Default::default()
        },
    );
    assert_eq!(value_of(&now.rates[0]), "4.32");
    let meta = now.rates[0].meta.as_ref().unwrap();
    assert_eq!(meta.version, 3);
    assert_eq!(
        meta.raw.as_ref().unwrap().key,
        "sofr/2",
        "the confirmation's reference"
    );

    // As of a recorded time before the revision: what the lake knew then.
    let then = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR]),
            business_date: "2026-10-07".into(),
            as_of_ns: before_revision,
            ..Default::default()
        },
    );
    assert_eq!(value_of(&then.rates[0]), "4.31");

    // As the source had published it by a time between the two.
    let published = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR]),
            business_date: "2026-10-07".into(),
            published_as_of_ns: first_published + 1,
            ..Default::default()
        },
    );
    assert_eq!(value_of(&published.rates[0]), "4.31");
    let none_yet = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR]),
            business_date: "2026-10-07".into(),
            published_as_of_ns: first_published - 1,
            ..Default::default()
        },
    );
    assert!(none_yet.rates.is_empty(), "nothing was published by then");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backfill_recorded_today_still_answers_what_was_published_then() {
    // The published-time read (shared additions 1 and 2): a history recorded
    // now answers for an earlier date; the recorded-time cut stays the
    // default and the audit answer.
    let h = harness();
    let long_ago = NOW - 300 * DAY;
    record(
        &h,
        "ny-fed-1",
        vec![Rate {
            meta: Some(ObservationMeta {
                business_date: "2025-12-12".into(),
                row_key: "SOFR:fixing:-:act_360/simple:2025-12-12@".into(),
                ..sofr("3.90", long_ago, "sofr/old").meta.unwrap()
            }),
            ..sofr("3.90", long_ago, "sofr/old")
        }],
    )
    .unwrap();
    let as_published = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR]),
            business_date: "2025-12-12".into(),
            published_as_of_ns: long_ago + DAY,
            ..Default::default()
        },
    );
    assert_eq!(as_published.rates.len(), 1);
    let as_recorded = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR]),
            business_date: "2025-12-12".into(),
            as_of_ns: long_ago + DAY,
            ..Default::default()
        },
    );
    assert!(
        as_recorded.rates.is_empty(),
        "the lake knew nothing of it then"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_curve_for_a_date_is_its_points_and_a_tenor_not_published_is_source_silent() {
    // a-curve-for-a-date and a-tenor-not-published.
    let h = harness();
    record(
        &h,
        "us-treasury-1",
        vec![
            par(UST, "3M", 3, TenorUnit::Month, "4.05"),
            par(UST, "2Y", 2, TenorUnit::Year, "3.60"),
            par(UST, "10Y", 10, TenorUnit::Year, "4.12"),
        ],
    )
    .unwrap();
    let curve = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[PAR]),
            business_date: "2026-10-07".into(),
            ..Default::default()
        },
    );
    assert_eq!(curve.rates.len(), 3, "one row per tenor");

    let ten = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[PAR]),
            business_date: "2026-10-07".into(),
            tenors: vec![tenor(10, TenorUnit::Year).unwrap()],
            ..Default::default()
        },
    );
    assert_eq!(ten.rates.len(), 1);
    assert_eq!(value_of(&ten.rates[0]), "4.12");

    // The 20-year point was not published that day: no row, source silent,
    // never zero and never asked of the source.
    let twenty = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[PAR]),
            business_date: "2026-10-07".into(),
            tenors: vec![tenor(20, TenorUnit::Year).unwrap()],
            sources: Some(SourceChoice {
                named: vec![UST.into()],
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    assert!(twenty.rates.is_empty());
    assert!(twenty
        .unanswered
        .iter()
        .any(|u| u.dataset == UST && u.reason == UnansweredReason::SourceSilent as i32));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_conventions_for_one_tenor_are_two_rows() {
    // two-conventions-one-tenor: a bill's discount and coupon-equivalent
    // rates, told apart by kind and convention, both the latest.
    let h = harness();
    let bill = |kind: RateKind, key: &str, value: &str, convention: RateConvention| Rate {
        tenor: tenor(13, TenorUnit::Week),
        convention: Some(convention),
        ..rate(
            UST,
            BILLS,
            key,
            kind,
            value,
            "2026-10-07",
            NOW - 6 * HOUR,
            "bills/2026-10-07",
        )
    };
    record(
        &h,
        "us-treasury-1",
        vec![
            bill(
                RateKind::Discount,
                "BILLS:discount:13W:act_360/discount:2026-10-07@",
                "3.95",
                RateConvention {
                    day_count: DayCount::Act360 as i32,
                    compounding: Compounding::Discount as i32,
                    periods_per_year: 0,
                },
            ),
            bill(
                RateKind::CouponEquivalent,
                "BILLS:coupon_equivalent:13W:act_act/compounded/2:2026-10-07@",
                "4.04",
                bond_equivalent().unwrap(),
            ),
        ],
    )
    .unwrap();
    let latest = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[BILLS]),
            ..Default::default()
        },
    );
    assert_eq!(latest.rates.len(), 2, "{:?}", latest.rates);
    let kinds: Vec<i32> = latest.rates.iter().map(|r| r.kind).collect();
    assert!(kinds.contains(&(RateKind::Discount as i32)));
    assert!(kinds.contains(&(RateKind::CouponEquivalent as i32)));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_index_level_carries_its_unit_and_base() {
    // an-index-level: the SOFR Index, its value an index level.
    let h = harness();
    let level = Rate {
        value: Some(UnitValue {
            value: decimal("1.20554751"),
            unit: ValueUnit::Index as i32,
            index_base: "2018-04-02=1".into(),
            ucum_code: String::new(),
        }),
        convention: Some(RateConvention {
            day_count: DayCount::Act360 as i32,
            compounding: Compounding::Compounded as i32,
            periods_per_year: 360,
        }),
        ..rate(
            NYFED,
            SOFR_INDEX,
            "SOFRINDEX:index_level:-:act_360/compounded/360:2026-10-07@",
            RateKind::IndexLevel,
            "0",
            "2026-10-07",
            NOW - 2 * HOUR,
            "sofrai/2026-10-07",
        )
    };
    record(&h, "ny-fed-1", vec![level]).unwrap();
    let read = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR_INDEX]),
            kinds: vec![RateKind::IndexLevel as i32],
            ..Default::default()
        },
    );
    let value = read.rates[0].value.as_ref().unwrap();
    assert_eq!(value.unit, ValueUnit::Index as i32);
    assert_eq!(value.index_base, "2018-04-02=1");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_curve_point_from_another_source_sits_under_the_priority() {
    // a-curve-point-from-another-source: FRED's DGS10 recorded against
    // Treasury's par curve with its tenor; the default read takes the
    // priority, side by side answers both.
    let h = harness();
    record(
        &h,
        "us-treasury-1",
        vec![par(UST, "10Y", 10, TenorUnit::Year, "4.12")],
    )
    .unwrap();
    record(
        &h,
        "fred-1",
        vec![par(FRED, "10Y", 10, TenorUnit::Year, "4.13")],
    )
    .unwrap();
    tokio::task::block_in_place(|| {
        h.lake.set_priority(
            SetSourcePriorityRequest {
                data_type: "meridian.v1.Rate".into(),
                kind: 0,
                datasets: vec![FRED.into(), UST.into()],
                note: "FRED first.".into(),
                against_updated_at_ns: 0,
            },
            &envelope("dashboard-1", false, "local|ada"),
        )
    })
    .unwrap();
    let ten = || ListRatesRequest {
        subjects: subjects(&[PAR]),
        business_date: "2026-10-07".into(),
        ..Default::default()
    };
    let default = list(&h, ten());
    assert_eq!(default.rates.len(), 1);
    assert_eq!(value_of(&default.rates[0]), "4.13", "the priority's first");
    let both = list(
        &h,
        ListRatesRequest {
            sources: Some(SourceChoice {
                side_by_side: true,
                ..Default::default()
            }),
            ..ten()
        },
    );
    assert_eq!(both.rates.len(), 2);
    // A rate's priority names no kind.
    let kinded = tokio::task::block_in_place(|| {
        h.lake.set_priority(
            SetSourcePriorityRequest {
                data_type: "meridian.v1.Rate".into(),
                kind: 1,
                datasets: vec![UST.into()],
                note: "".into(),
                against_updated_at_ns: 0,
            },
            &envelope("dashboard-1", false, "local|ada"),
        )
    })
    .unwrap_err();
    assert!(kinded.contains("kind"), "{kinded}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rate_is_refused_naming_its_field() {
    // The fixture's refusals (W10.4).
    let h = harness();
    let refused = |rates: Vec<Rate>, instance: &str| record(&h, instance, rates).unwrap_err();

    let mut unkinded = sofr("4.31", NOW, "sofr/1");
    unkinded.kind = 0;
    assert!(refused(vec![unkinded], "ny-fed-1").starts_with("rates[0].kind"));

    let mut unitless = sofr("4.31", NOW, "sofr/1");
    unitless.value.as_mut().unwrap().unit = 0;
    assert!(refused(vec![unitless], "ny-fed-1").starts_with("rates[0].value.unit"));

    let mut no_compounding = sofr("4.31", NOW, "sofr/1");
    no_compounding.convention.as_mut().unwrap().compounding = 0;
    assert!(
        refused(vec![no_compounding], "ny-fed-1").starts_with("rates[0].convention.compounding")
    );

    let mut pointless = par(UST, "10Y", 10, TenorUnit::Year, "4.12");
    pointless.tenor = None;
    let why = refused(
        vec![par(UST, "2Y", 2, TenorUnit::Year, "3.60"), pointless],
        "us-treasury-1",
    );
    assert!(why.starts_with("rates[1].tenor"), "{why}");

    let mut no_rate_record = sofr("4.31", NOW, "sofr/1");
    no_rate_record.meta.as_mut().unwrap().subjects[0].entity_id = AAPL.into();
    let why = refused(vec![no_rate_record], "ny-fed-1");
    assert!(
        why.starts_with("rates[0].meta.subjects[0].entity_id")
            && why.contains("no referential record"),
        "{why}"
    );

    let mut out_of_range = sofr("4.31", NOW, "sofr/1");
    out_of_range.percentiles[0].percentile = 100;
    assert!(refused(vec![out_of_range], "ny-fed-1").starts_with("rates[0].percentiles[0]"));
}

#[tokio::test(flavor = "multi_thread")]
async fn query_lake_answers_rates_within_its_bounds() {
    // W10.10, from contract v27: the kind `rates`, named fields kept.
    let h = harness();
    record(&h, "ny-fed-1", vec![sofr("4.31", NOW - 2 * HOUR, "sofr/1")]).unwrap();
    let reply = tokio::task::block_in_place(|| {
        h.lake.query_lake(
            QueryLakeRequest {
                kind: "rates".into(),
                subjects: subjects(&[SOFR]),
                fields: vec!["meridian.v1.Rate.value".into()],
                ..Default::default()
            },
            &envelope("signal-1", true, ""),
        )
    })
    .unwrap();
    assert_eq!(reply.rates.len(), 1);
    assert!(reply.rates[0].value.is_some());
    assert!(reply.rates[0].percentiles.is_empty(), "a field not named");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_arriving_older_publication_is_kept_and_never_takes_force() {
    // Ruling 1006 (b): a rate's version in force is its latest by published
    // time, as a default read serves it, and the counts follow it. An older
    // publication heard late is the next version, kept and read as
    // published then, never in force; a newer one takes force.
    use meridian_domain::v1::QualityState;
    let h = harness();
    let revised_published = NOW - HOUR;
    let first_published = NOW - 3 * HOUR;
    record(
        &h,
        "ny-fed-1",
        vec![sofr("4.32", revised_published, "sofr/2")],
    )
    .unwrap();
    let before_late = h.clock.now_ns();
    h.clock.advance(HOUR);
    let mut late = sofr("4.31", first_published, "sofr/1");
    late.meta.as_mut().unwrap().quality = QualityState::Invalid as i32;
    late.meta.as_mut().unwrap().quality_reasons =
        vec![meridian_domain::v1::QualityReason::ImplausibleTimestamp as i32];
    let heard = record(&h, "ny-fed-1", vec![late]).unwrap();
    assert_eq!(
        (heard.recorded, heard.restated, heard.unchanged),
        (0, 1, 0),
        "kept, the next version"
    );

    let on_the_day = |request: ListRatesRequest| {
        let read = list(
            &h,
            ListRatesRequest {
                subjects: subjects(&[SOFR]),
                ..request
            },
        );
        assert_eq!(read.rates.len(), 1, "{:?}", read.unanswered);
        let meta = read.rates[0].meta.clone().unwrap();
        (value_of(&read.rates[0]), meta.version)
    };
    // The default read, for the date and the latest: the later publication.
    assert_eq!(
        on_the_day(ListRatesRequest {
            business_date: "2026-10-07".into(),
            ..Default::default()
        }),
        ("4.32".to_string(), 1)
    );
    assert_eq!(
        on_the_day(ListRatesRequest::default()),
        ("4.32".to_string(), 1)
    );
    // As of before it was heard, the same; as published then, the older.
    assert_eq!(
        on_the_day(ListRatesRequest {
            business_date: "2026-10-07".into(),
            as_of_ns: before_late,
            ..Default::default()
        }),
        ("4.32".to_string(), 1)
    );
    let as_published = list(
        &h,
        ListRatesRequest {
            subjects: subjects(&[SOFR]),
            business_date: "2026-10-07".into(),
            published_as_of_ns: first_published + 1,
            sources: Some(SourceChoice {
                named: vec![NYFED.into()],
                include_quarantined: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    assert_eq!(value_of(&as_published.rates[0]), "4.31");

    // The counts follow the version in force: the late, invalid one counts
    // nowhere.
    let counted = |h: &Harness| {
        let listed = tokio::task::block_in_place(|| {
            h.lake
                .list_datasets(&envelope("dashboard-1", false, ""))
                .unwrap()
        });
        let held = listed
            .datasets
            .iter()
            .find(|d| d.dataset == NYFED)
            .unwrap()
            .clone();
        (held.valid_count, held.suspicious_count, held.invalid_count)
    };
    assert_eq!(counted(&h), (1, 0, 0));

    // A newer publication takes force.
    h.clock.advance(HOUR);
    record(&h, "ny-fed-1", vec![sofr("4.33", NOW + HOUR, "sofr/3")]).unwrap();
    assert_eq!(
        on_the_day(ListRatesRequest::default()),
        ("4.33".to_string(), 3)
    );
    assert_eq!(counted(&h), (1, 0, 0));
}
