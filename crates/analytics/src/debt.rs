//! A job's debt inputs, resolved on its client's behalf (contract v29;
//! W12.1, W12.2; ruling 688): each debt record's terms and the fields it
//! lacks, from the instrument store's record; the job's date; the payment
//! calendars' closed days (ENG1 §4: core's settlement calendars, never
//! QuantLib's); each pool's and class's `Factor` rows (`ListFactors`, W10.6);
//! and the par curves -- Treasury's, found by its administrator's symbol as
//! the risk-free curve is (608, 611), and each make-whole or price
//! benchmark's by its record -- through the deployment's source priority
//! for rates. Every read is one the contract names `analytics` an asker of,
//! and each carries the client's instance and scope, as every read does.
//!
//! Each is written into the inputs document only where a debt record or a
//! debt measure needs it, so every earlier job's document, and its hash, is
//! unchanged.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::date::Date;
use meridian_domain::debt;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{self as v1, InstrumentType, SubjectRef};
use prost::Message;
use serde_json::{json, Value};

use crate::resolve::Ask;
use crate::topics;

/// `platform.lake.query.list-factors` (W10.6), which names `analytics`
/// among its askers.
pub const LIST_FACTORS: &str = "platform.lake.query.list-factors";

fn ask<Q: Message, R: Message + Default>(
    asker: &dyn Ask,
    topic: &str,
    kind: &str,
    request: &Q,
) -> Result<R, String> {
    let (reply_type, payload) = asker.ask(topic, kind, request.encode_to_vec())?;
    R::decode(&payload[..]).map_err(|failed| format!("{reply_type} did not read: {failed}"))
}

fn text_of(decimal: Option<&meridian_pb::v1::Decimal>) -> Option<String> {
    decimal
        .and_then(|d| Exact::from_wire(d).ok())
        .map(|e| e.to_string())
}

/// A debt record's terms as the engine reads them: its type, its terms as
/// the instrument store writes them, and the fields it lacks (574). None
/// for any other record.
pub fn record_json(record: &v1::InstrumentRecord) -> Option<Value> {
    let kind = InstrumentType::try_from(record.instrument_type).ok()?;
    let (name, terms) = match kind {
        InstrumentType::Bond => ("bond", debt::bond_json(record.bond.as_ref()?)),
        InstrumentType::MbsPool => ("mbs_pool", debt::mbs_pool_json(record.mbs_pool.as_ref()?)),
        InstrumentType::Tba => ("tba", debt::tba_json(record.tba.as_ref()?)),
        InstrumentType::CmoTranche => (
            "cmo_tranche",
            debt::cmo_tranche_json(record.cmo_tranche.as_ref()?),
        ),
        InstrumentType::CertificateOfDeposit => (
            "certificate_of_deposit",
            debt::certificate_of_deposit_json(record.certificate_of_deposit.as_ref()?),
        ),
        _ => return None,
    };
    Some(json!({
        "type": name,
        "terms": terms,
        "missing_fields": record.missing_fields,
    }))
}

/// The payment calendar a debt record's terms name; empty where none.
pub fn calendar_of(debt: &Value) -> String {
    debt["terms"]["payment_calendar_id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Whether a debt record is a pool or a class, valued with its factor.
pub fn factored(debt: &Value) -> bool {
    matches!(debt["type"].as_str(), Some("mbs_pool" | "cmo_tranche"))
}

/// The benchmarks a record's make-whole windows name.
pub fn benchmarks_of(debt: &Value) -> Vec<String> {
    debt["terms"]["calls"]
        .as_array()
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| c["make_whole_benchmark_instrument_id"].as_str())
                .filter(|b| !b.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The job's date: its business date, else the as-of's date in New York,
/// where the engine's daily cut is (the first release's Q9).
pub fn valuation_date(business_date: &str, as_of: i64) -> String {
    if !business_date.is_empty() {
        return business_date.to_string();
    }
    meridian_domain::zone::Zone::load("America/New_York")
        .ok()
        .and_then(|zone| zone.local(as_of))
        .map(|(date, _)| date)
        .unwrap_or_else(|| Date::of_utc_instant(as_of))
        .to_string()
}

/// Each payment calendar's closed days from the job's date over the month
/// a settlement can reach: `[{calendar_id, holidays}]`. A calendar not held
/// is left out, so its record settles on weekdays alone.
pub fn calendars(asker: &dyn Ask, ids: &BTreeSet<String>, date: &str) -> Vec<Value> {
    let Ok(from) = Date::parse(date) else {
        return Vec::new();
    };
    let to = Date::from_days(from.days_since_epoch() + 31).unwrap_or(from);
    let mut out = Vec::new();
    for id in ids.iter().filter(|id| !id.is_empty()) {
        let reply: Result<v1::ReadCalendarReply, String> = ask(
            asker,
            topics::READ_CALENDAR,
            "meridian.v1.ReadCalendarRequest",
            &v1::ReadCalendarRequest {
                calendar_id: id.clone(),
                from_date: from.to_string(),
                to_date: to.to_string(),
                as_of_ns: 0,
            },
        );
        let Ok(reply) = reply else {
            continue;
        };
        if !reply.found {
            continue;
        }
        let holidays: Vec<String> = reply
            .days
            .iter()
            .filter(|d| d.kind == v1::CalendarDayKind::Closed as i32)
            .map(|d| d.date.clone())
            .collect();
        out.push(json!({"calendar_id": id, "holidays": holidays}));
    }
    out
}

/// Each pool's and class's factor rows at the as-of, every month the lake
/// holds to the job's date: `[{subject_id, factor, factor_month, dataset,
/// row_key, version, source_ns}]`, with `wac`, `wam` and `wala` where the
/// row states them (995), in one order.
pub fn factors(
    asker: &dyn Ask,
    subjects: &[String],
    date: &str,
    as_of: i64,
    sources: &Option<v1::SourceChoice>,
) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for chunk in subjects.chunks(500) {
        let mut cursor = String::new();
        loop {
            let reply: v1::ListFactorsReply = ask(
                asker,
                LIST_FACTORS,
                "meridian.v1.ListFactorsRequest",
                &v1::ListFactorsRequest {
                    subjects: chunk
                        .iter()
                        .map(|s| SubjectRef {
                            entity_id: s.clone(),
                            ..Default::default()
                        })
                        .collect(),
                    sources: sources.clone(),
                    as_of_ns: as_of,
                    page_size: 500,
                    cursor: cursor.clone(),
                    ..Default::default()
                },
            )
            .map_err(|failed| format!("the lake's factors could not be read: {failed}"))?;
            for row in &reply.factors {
                let meta = row.meta.clone().unwrap_or_default();
                let Some(subject) = meta.subjects.first().map(|s| s.entity_id.clone()) else {
                    continue;
                };
                let Some(factor) = text_of(row.factor.as_ref()) else {
                    continue;
                };
                // A month after the job's date's is not yet in force.
                let month = date.get(..7).unwrap_or_default().replace('-', "");
                if !month.is_empty() && row.factor_month > month {
                    continue;
                }
                let source_ns = meta
                    .source_times
                    .iter()
                    .map(|t| t.at_ns)
                    .min()
                    .unwrap_or(meta.valid_from_ns);
                let mut kept = json!({
                    "subject_id": subject,
                    "factor": factor,
                    "factor_month": row.factor_month,
                    "dataset": meta.source.map(|s| s.dataset).unwrap_or_default(),
                    "row_key": meta.row_key,
                    "version": meta.version.to_string(),
                    "source_ns": source_ns.to_string(),
                });
                // Contract v29 (995): the disclosure's WAC, WAM and WALA,
                // where the agency states them.
                if let Some(wac) = text_of(row.wac.as_ref()) {
                    kept["wac"] = json!(wac);
                }
                if let Some(wam) = row.wam {
                    kept["wam"] = json!(wam.to_string());
                }
                if let Some(wala) = row.wala {
                    kept["wala"] = json!(wala.to_string());
                }
                out.push(kept);
            }
            if reply.next_cursor.is_empty() {
                break;
            }
            cursor = reply.next_cursor;
        }
    }
    out.sort_by_key(|f| {
        (
            f["subject_id"].as_str().unwrap_or_default().to_string(),
            f["factor_month"].as_str().unwrap_or_default().to_string(),
            f["version"].as_str().unwrap_or_default().to_string(),
        )
    });
    out.dedup();
    Ok(out)
}

/// A tenor's label and unit as the engine reads them.
fn tenor(t: &v1::Tenor) -> Option<(String, i32, String)> {
    let unit = v1::TenorUnit::try_from(t.unit).ok()?;
    let letter = match unit {
        v1::TenorUnit::Day => "D",
        v1::TenorUnit::Week => "W",
        v1::TenorUnit::Month => "M",
        v1::TenorUnit::Year => "Y",
        _ => return None,
    };
    Some((
        format!("{}{letter}", t.count),
        t.count,
        unit.as_str_name().to_string(),
    ))
}

/// A par curve's points at a time, read through the source priority: the
/// latest par yield at each tenor. `subject` is its record; `date` the
/// job's. `{subject_id, dataset, date, points, source_ns}`, or `{missing,
/// missing_reason}` where none was read.
pub fn par_curve(
    asker: &dyn Ask,
    subject: &str,
    at: i64,
    as_of: i64,
    sources: &Option<v1::SourceChoice>,
    date: &str,
) -> Value {
    let missing = |why: String| json!({"subject_id": subject, "missing": why});
    let mut points: BTreeMap<String, (i64, Value)> = BTreeMap::new();
    let mut dataset = String::new();
    let mut unentitled: BTreeSet<String> = BTreeSet::new();
    let mut source_ns = 0i64;
    let mut cursor = String::new();
    loop {
        let reply: v1::ListRatesReply = match ask(
            asker,
            topics::LIST_RATES,
            "meridian.v1.ListRatesRequest",
            &v1::ListRatesRequest {
                subjects: vec![SubjectRef {
                    entity_id: subject.to_string(),
                    ..Default::default()
                }],
                kinds: vec![v1::RateKind::ParYield as i32],
                sources: sources.clone(),
                at_ns: at,
                as_of_ns: as_of,
                page_size: 500,
                cursor: cursor.clone(),
                ..Default::default()
            },
        ) {
            Ok(reply) => reply,
            Err(failed) => {
                return missing(format!(
                    "{subject}'s par yields could not be read: {failed}"
                ))
            }
        };
        unentitled.extend(
            reply
                .unanswered
                .iter()
                .filter(|u| u.reason == v1::UnansweredReason::NotEntitled as i32)
                .filter(|u| !u.dataset.is_empty())
                .map(|u| u.dataset.clone()),
        );
        for r in &reply.rates {
            let Some((label, count, unit)) = r.tenor.as_ref().and_then(tenor) else {
                continue;
            };
            let Some(value) = r.value.as_ref().and_then(|v| text_of(v.value.as_ref())) else {
                continue;
            };
            let meta = r.meta.clone().unwrap_or_default();
            if dataset.is_empty() {
                dataset = meta
                    .source
                    .as_ref()
                    .map(|s| s.dataset.clone())
                    .unwrap_or_default();
            }
            let held = points.get(&label).map(|(at, _)| *at).unwrap_or(i64::MIN);
            if meta.valid_from_ns >= held {
                let src = meta
                    .source_times
                    .iter()
                    .map(|t| t.at_ns)
                    .min()
                    .unwrap_or(meta.valid_from_ns);
                points.insert(
                    label.clone(),
                    (
                        meta.valid_from_ns,
                        json!({"tenor": label, "count": count, "unit": unit, "par_yield": value, "source_ns": src}),
                    ),
                );
            }
        }
        if reply.next_cursor.is_empty() {
            break;
        }
        cursor = reply.next_cursor;
    }
    if points.is_empty() {
        if !unentitled.is_empty() {
            let mut said = missing(format!(
                "{} is not entitled to {}",
                asker.client(),
                unentitled.into_iter().collect::<Vec<_>>().join(", ")
            ));
            said["missing_reason"] = json!("not_entitled");
            return said;
        }
        return missing(format!("{subject}: no par yields at the as-of"));
    }
    for (_, point) in points.values() {
        let src: i64 = point["source_ns"].as_i64().unwrap_or(0);
        source_ns = if source_ns == 0 {
            src
        } else {
            source_ns.min(src)
        };
    }
    let mut ordered: Vec<Value> = points
        .into_values()
        .map(|(_, mut p)| {
            if let Some(object) = p.as_object_mut() {
                object.remove("source_ns");
            }
            p
        })
        .collect();
    ordered.sort_by_key(|p| p["tenor"].as_str().unwrap_or_default().to_string());
    json!({
        "subject_id": subject,
        "dataset": dataset,
        "date": date,
        "points": ordered,
        "source_ns": source_ns.to_string(),
    })
}

/// The Treasury par curve's record, found as the risk-free curve's is (608,
/// 611): `ust_cmt`, by its administrator's symbol, read-only.
pub fn treasury_curve_record(asker: &dyn Ask, as_of: i64) -> Result<String, String> {
    let reply: v1::ResolveIdentifierReply = ask(
        asker,
        topics::RESOLVE_IDENTIFIER,
        "meridian.v1.ResolveIdentifierRequest",
        &v1::ResolveIdentifierRequest {
            identifiers: vec![v1::Identifier {
                scheme: "symbol".into(),
                value: "Daily Treasury Par Yield Curve Rates".into(),
                source: "ustreasury".into(),
                ..Default::default()
            }],
            as_of_ns: as_of,
            ..Default::default()
        },
    )
    .map_err(|failed| format!("the Treasury par curve ust_cmt: not resolved: {failed}"))?;
    if reply.found && !reply.instrument_id.is_empty() {
        Ok(reply.instrument_id)
    } else {
        Err("the Treasury par curve ust_cmt: the deployment holds no record of it".to_string())
    }
}

/// A row's fixed-income fields (contract v29, W10.4), added to its market
/// row only where set, so every earlier row's text is unchanged.
pub fn extras(price: &v1::Price, row: &mut Value) {
    let basis = v1::PriceBasis::try_from(price.basis).unwrap_or(v1::PriceBasis::Unspecified);
    if !matches!(basis, v1::PriceBasis::Unspecified | v1::PriceBasis::PerUnit) {
        row["basis"] = json!(basis.as_str_name());
    }
    if let Some(y) = price.r#yield.as_ref() {
        if let Some(value) = y.value.as_ref().and_then(|v| text_of(v.value.as_ref())) {
            row["yield"] = json!({
                "value": value,
                "kind": v1::YieldKind::try_from(y.kind).map(|k| k.as_str_name()).unwrap_or(""),
                "to_date": y.to_date,
            });
        }
    }
    if let Some(spread) = text_of(price.spread_bps.as_ref()) {
        row["spread_bps"] = json!(spread);
    }
    if !price.benchmark_instrument_id.is_empty() {
        row["benchmark_instrument_id"] = json!(price.benchmark_instrument_id);
    }
}
