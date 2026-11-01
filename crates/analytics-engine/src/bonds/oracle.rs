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

// ── The Treasury curve (`monotone_convex` v1) ──────────────────────────────

/// The tolerances, in basis points of a continuous zero rate: 0.29 and
/// 8.70 seen on 2026-10-01 and 2026-10-16.
pub(super) const CURVE_VS_QUANTLIB_BP: f64 = 0.5;
pub(super) const CURVE_VS_BOARD_BP: f64 = 10.0;

const CURVES: &str = include_str!("../../oracle_cases/treasury-curve.json");

pub(super) fn fitted(case: &Value) -> super::curve::Curve {
    use super::curve::{maturity_of, Curve, ParPoint};
    let date = meridian_domain::date::Date::parse(case["date"].as_str().unwrap()).unwrap();
    let points = case["par"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let (count, unit) = tenor(p["tenor"].as_str().unwrap());
            ParPoint {
                tenor: p["tenor"].as_str().unwrap().to_string(),
                maturity: maturity_of(date, count, unit).unwrap(),
                par: p["par_yield"].as_str().unwrap().parse().unwrap(),
            }
        })
        .collect();
    Curve::fit(date, points).expect("the par curve fits")
}

#[test]
fn the_curve_reprices_treasurys_par_yields_and_stays_near_quantlibs_and_the_boards() {
    use super::curve::maturity_of;
    let cases: Value = serde_json::from_str(CURVES).unwrap();
    let mut worst_quantlib: f64 = 0.0;
    let mut worst_board: f64 = 0.0;
    for case in cases["cases"].as_array().unwrap() {
        let curve = fitted(case);
        // Treasury's own par yields, its method's inputs, round-trip.
        for p in &curve.points {
            let back = curve.par(p.maturity);
            assert!(
                (back - p.par).abs() < 1e-9,
                "{} {}: {back} for {}",
                case["date"],
                p.tenor,
                p.par
            );
        }
        for (label, zero) in case["quantlib_convex_monotone_forward_zero"]
            .as_object()
            .unwrap()
        {
            let (count, unit) = tenor(label);
            let on = maturity_of(curve.date, count, unit).unwrap();
            let ours = curve.zero(on) * 100.0;
            let theirs: f64 = zero.as_str().unwrap().parse().unwrap();
            println!(
                "{} {label}: ours {ours:.6} quantlib {theirs:.6} diff {:.4}bp",
                case["date"],
                (ours - theirs) * 100.0
            );
            worst_quantlib = worst_quantlib.max((ours - theirs).abs() * 100.0);
        }
        for (label, zero) in case["fed_zero"].as_object().unwrap() {
            let (count, unit) = tenor(label);
            let on = maturity_of(curve.date, count, unit).unwrap();
            let ours = curve.zero(on) * 100.0;
            let theirs: f64 = zero.as_str().unwrap().parse().unwrap();
            println!(
                "{} {label}: ours {ours:.6} board {theirs:.6} diff {:.4}bp",
                case["date"],
                (ours - theirs) * 100.0
            );
            worst_board = worst_board.max((ours - theirs).abs() * 100.0);
        }
    }
    println!("worst against QuantLib {worst_quantlib:.4}bp, against the Board {worst_board:.4}bp");
    // QuantLib's convex monotone forward is Hagan and West's ameliorated
    // form (its quadraticity and monotonicity blend); the Board's is
    // Gurkaynak, Sack and Wright's Svensson fit to off-the-run coupons.
    assert!(worst_quantlib < CURVE_VS_QUANTLIB_BP, "{worst_quantlib}bp");
    assert!(worst_board < CURVE_VS_BOARD_BP, "{worst_board}bp");
}

// ── Bond functions (`bond_cash_flows`, `bond_yield_solver`) ─────────────────

const BONDS: &str = include_str!("../../oracle_cases/bond-functions.json");

/// The tolerances against QuantLib: a yield in percent and a duration in
/// years on the street's formula, which QuantLib's compounding equals, to
/// the figure's six places (half of its last); a
/// Treasury's on Appendix B's simple interest over the fractional
/// half-year, which QuantLib compounds; a Z-spread in basis points over
/// the two curves (0.29bp apart at most, above).
pub(super) const STREET_YIELD_PERCENT: f64 = 5e-7;
pub(super) const TREASURY_YIELD_PERCENT: f64 = 1e-3;
pub(super) const DURATION_YEARS: f64 = 1e-6;
pub(super) const TREASURY_DURATION_YEARS: f64 = 2e-3;
pub(super) const Z_SPREAD_BP: f64 = 0.5;

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
fn bond_yields_risk_and_spreads_are_quantlibs_within_their_tolerances() {
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
        &["bond_yield", "bond_risk", "bond_spread"],
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
        let z = figure(&answer, "bond_spread", &id, "z_spread").unwrap();
        println!("{} z ours {z} quantlib {}", b["name"], q("z_spread_bp"));
        assert!(
            (z - q("z_spread_bp")).abs() <= Z_SPREAD_BP,
            "{} z_spread: {z} vs {}",
            b["name"],
            q("z_spread_bp")
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
