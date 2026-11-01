//! The models' oracle (contract v29; W12.3; ENG1 §4; ruling 565): the
//! platform's packages' exact cases -- every Treasury auction's published
//! `high_yield` and `price_per100` under 31 CFR 356 Appendix B, MSRB
//! G-33's accrual, a pool's and a class's month, a CD's accrual and a TBA's
//! market value in its price basis -- and QuantLib 1.40's answers, pinned
//! here from the pinned release (never a dependency of this crate).

use serde_json::Value;

use super::package;
use crate::exact::Dec;

const PACKAGES: &str = include_str!("../../oracle_cases/platform-v29-packages.json");

fn equal(expected: &Value, answered: &Value) -> Result<(), String> {
    let expected = expected.as_object().ok_or("expected is an object")?;
    for (name, value) in expected {
        let want = Dec::parse(value.as_str().unwrap_or_default())?;
        let got = answered
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{name} not answered"))?;
        let got = Dec::parse(got)?;
        if want != got {
            return Err(format!(
                "{name}: expected {}, answered {}",
                want.text(),
                got.text()
            ));
        }
    }
    Ok(())
}

/// A tenor's label as its count and unit.
fn tenor(text: &str) -> (i32, &'static str) {
    let (count, unit) = text.split_at(text.len() - 1);
    let unit = match unit {
        "M" => "TENOR_UNIT_MONTH",
        "W" => "TENOR_UNIT_WEEK",
        "D" => "TENOR_UNIT_DAY",
        _ => "TENOR_UNIT_YEAR",
    };
    (count.parse().unwrap(), unit)
}

#[test]
fn every_platform_case_is_answered_exactly() {
    let packages: Value = serde_json::from_str(PACKAGES).unwrap();
    let mut count = 0;
    for template in packages.as_array().unwrap() {
        let package = &template["calculation_package"];
        for case in package["test_cases"].as_array().unwrap() {
            let inputs: Value = serde_json::from_str(case["inputs"].as_str().unwrap()).unwrap();
            let expected: Value = serde_json::from_str(case["expected"].as_str().unwrap()).unwrap();
            let answered =
                package::answer(&inputs).unwrap_or_else(|why| panic!("{}: {why}", case["name"]));
            if let Err(why) = equal(&expected, &answered) {
                panic!("{}: {why}", case["name"]);
            }
            count += 1;
        }
    }
    // Nine auctions, a bill, two G-33 accruals, a pool's and a class's
    // month, a TBA's market value and a CD's accrual: sixteen.
    assert_eq!(count, 16);
}

// ── Bond functions (`bond_cash_flows`, `bond_yield_solver`) ─────────────────

const CURVES: &str = include_str!("../../oracle_cases/treasury-curve.json");

const BONDS: &str = include_str!("../../oracle_cases/bond-functions.json");

/// The tolerances against QuantLib: a yield in percent and a duration in
/// years on the street's formula, which QuantLib's compounding equals, to
/// the figure's six places (half of its last); a
/// Treasury's on Appendix B's simple interest over the fractional
/// half-year, which QuantLib compounds.
pub(super) const STREET_YIELD_PERCENT: f64 = 5e-7;
pub(super) const TREASURY_YIELD_PERCENT: f64 = 1e-3;
pub(super) const DURATION_YEARS: f64 = 1e-6;
pub(super) const TREASURY_DURATION_YEARS: f64 = 2e-3;

/// An inputs document holding one account's bonds at their closes, each
/// 100 of face, the Treasury curve of `curve` and the measures asked.
pub(super) fn bonds_document(
    bonds: &[Value],
    curve: &Value,
    measures: &[&str],
    valuation: &str,
) -> Value {
    let mut instruments = vec![serde_json::json!({
        "instrument_id": "INS-USD", "model_id": "cash", "asset_class": "cash",
        "currency_instrument_id": "INS-USD", "currency_code": "USD",
    })];
    let mut positions = Vec::new();
    let mut market = Vec::new();
    for (i, b) in bonds.iter().enumerate() {
        let id = format!("INS-BOND{i}");
        let terms = serde_json::json!({
            "security_type": b["security_type"], "issuer_type": b["issuer_type"],
            "issue_date": b["issue"], "dated_date": b["dated"], "first_coupon_date": b["first"],
            "maturity_date": b["maturity"], "coupon_rate": b["coupon"],
            "coupon_type": "COUPON_TYPE_FIXED", "coupon_frequency": 2, "day_count": b["dc"],
        });
        instruments.push(serde_json::json!({
            "instrument_id": id, "model_id": "bond_cash_flows", "asset_class": "debt",
            "currency_instrument_id": "INS-USD", "price_kind": "close",
            "debt": {"type": "bond", "terms": terms, "missing_fields": []},
        }));
        positions.push(serde_json::json!({"account_id": "ACC-1", "instrument_id": id, "quantity": "100", "cost": "100"}));
        market.push(serde_json::json!({
            "subject_id": id, "kind": "close", "amount": b["clean"], "instrument_id": "INS-USD",
            "dataset": "bonds:closes", "valid_at_ns": "1", "row_key": id, "version": "1",
            "basis": "PRICE_BASIS_PERCENT_OF_PAR",
        }));
    }
    let points: Vec<Value> = curve["par"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let (count, unit) = tenor(p["tenor"].as_str().unwrap());
            serde_json::json!({"tenor": p["tenor"], "count": count, "unit": unit, "par_yield": p["par_yield"]})
        })
        .collect();
    serde_json::json!({
        "engine_version": "test",
        "job": {
            "account_ids": ["ACC-1"],
            "measures": measures.iter().map(|m| serde_json::json!({"definition": m, "version": 1})).collect::<Vec<_>>(),
        },
        "lake_as_of_ns": "1",
        "accounts": [{"account_id": "ACC-1", "base_currency_instrument_id": "INS-USD"}],
        "instruments": instruments,
        "positions": positions,
        "market": market,
        "valuation_date": valuation,
        "treasury_curve": {
            "subject_id": "INS-UST", "dataset": "ustreasury:par", "date": valuation,
            "points": points, "source_ns": "1",
        },
    })
}

/// A figure's value by its definition, instrument and last breakdown's
/// value.
pub(super) fn figure(
    answer: &crate::Answer,
    definition: &str,
    instrument: &str,
    last: &str,
) -> Option<f64> {
    answer
        .figures
        .iter()
        .find(|f| {
            f.measure.definition == definition
                && f.breakdown
                    .iter()
                    .any(|(b, v)| b == "instrument" && v == instrument)
                && f.breakdown.iter().any(|(_, v)| v == last)
        })
        .and_then(|f| f.value.as_ref())
        .map(Dec::to_f64)
}

#[test]
fn bond_yields_and_risk_are_quantlibs_within_their_tolerances() {
    let bonds: Value = serde_json::from_str(BONDS).unwrap();
    let curves: Value = serde_json::from_str(CURVES).unwrap();
    let curve = curves["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["date"] == bonds["valuation_date"])
        .unwrap();
    let list = bonds["bonds"].as_array().unwrap();
    let document = bonds_document(
        list,
        curve,
        &["bond_yield", "bond_risk"],
        bonds["valuation_date"].as_str().unwrap(),
    );
    let answer = crate::compute(&document).expect("answered");
    assert!(
        answer.not_evaluated.is_empty(),
        "{:?}",
        answer.not_evaluated
    );
    for (i, b) in list.iter().enumerate() {
        let id = format!("INS-BOND{i}");
        let ql = &b["quantlib"];
        let q = |name: &str| ql[name].as_str().unwrap().parse::<f64>().unwrap();
        let treasury = b["security_type"] == "DEBT_SECURITY_TYPE_TNOTE";
        let municipal = b["issuer_type"] == "ISSUER_TYPE_MUNICIPAL";
        let ytm = figure(&answer, "bond_yield", &id, "to_maturity").unwrap();
        let tolerance = if treasury {
            TREASURY_YIELD_PERCENT
        } else if municipal {
            // G-33: truncated to four places, then rounded to three.
            1e-3
        } else {
            STREET_YIELD_PERCENT
        };
        println!(
            "{} ytm ours {ytm} quantlib {}",
            b["name"],
            q("yield_percent")
        );
        assert!(
            (ytm - q("yield_percent")).abs() <= tolerance,
            "{}: {ytm} vs {}",
            b["name"],
            q("yield_percent")
        );
        let duration_tolerance = if treasury {
            TREASURY_DURATION_YEARS
        } else {
            DURATION_YEARS
        };
        for (component, name) in [
            ("macaulay_duration", "macaulay"),
            ("modified_duration", "modified"),
        ] {
            let ours = figure(&answer, "bond_risk", &id, component).unwrap();
            println!("{} {component} ours {ours} quantlib {}", b["name"], q(name));
            // A municipal's yield is G-33's, rounded; its duration is at the
            // solved yield, unrounded.
            assert!(
                (ours - q(name)).abs() <= duration_tolerance,
                "{} {component}: {ours} vs {}",
                b["name"],
                q(name)
            );
        }
        let convexity = figure(&answer, "bond_risk", &id, "convexity").unwrap();
        println!(
            "{} convexity ours {convexity} quantlib {}",
            b["name"],
            q("convexity")
        );
        assert!(
            (convexity - q("convexity")).abs() <= q("convexity") * 1e-4,
            "{} convexity",
            b["name"]
        );
        let dv01 = figure(&answer, "bond_risk", &id, "dv01_per_100").unwrap();
        println!("{} dv01 ours {dv01} quantlib {}", b["name"], -q("bps"));
        assert!(
            (dv01 + q("bps")).abs() <= 1e-4 * dv01.abs().max(1e-6) + 1e-6,
            "{} dv01",
            b["name"]
        );
    }
}

#[test]
fn accrued_interest_at_settlement_is_quantlibs_exactly() {
    let bonds: Value = serde_json::from_str(BONDS).unwrap();
    for b in bonds["bonds"].as_array().unwrap() {
        let terms = serde_json::json!({
            "security_type": b["security_type"], "issuer_type": b["issuer_type"],
            "issue_date": b["issue"], "dated_date": b["dated"], "first_coupon_date": b["first"],
            "maturity_date": b["maturity"], "coupon_rate": b["coupon"],
            "coupon_type": "COUPON_TYPE_FIXED", "coupon_frequency": 2, "day_count": b["dc"],
        });
        let held = super::read(&serde_json::json!({"type": "bond", "terms": terms})).unwrap();
        let shape = held.shape(&Default::default()).unwrap();
        let schedule = shape.build();
        let settle = meridian_domain::date::Date::parse("2026-10-19").unwrap();
        let ours = super::schedule::accrued(&shape, &schedule, settle).round_half_even(12);
        let theirs = Dec::parse(b["quantlib"]["accrued"].as_str().unwrap()).unwrap();
        assert_eq!(ours, theirs, "{}", b["name"]);
    }
}
