//! The debt measures over an inputs document (contract v29; W12.1): what a
//! job answers, and what it leaves not evaluated and why.

use serde_json::{json, Value};

use super::oracle::{bonds_document, figure};
use crate::answer::*;

const CURVES: &str = include_str!("../../oracle_cases/treasury-curve.json");
const BONDS: &str = include_str!("../../oracle_cases/bond-functions.json");

fn curve() -> Value {
    let curves: Value = serde_json::from_str(CURVES).unwrap();
    curves["cases"][0].clone()
}

fn corporate() -> Value {
    let bonds: Value = serde_json::from_str(BONDS).unwrap();
    bonds["bonds"][0].clone()
}

fn reasons(answer: &crate::Answer) -> Vec<(&str, &str, &str)> {
    answer
        .not_evaluated
        .iter()
        .map(|u| (u.measure.definition.as_str(), u.reason, u.detail.as_str()))
        .collect()
}

#[test]
fn a_bonds_market_value_is_face_times_its_clean_price_and_its_accrued_its_own() {
    let doc = bonds_document(
        &[corporate()],
        &curve(),
        &["market_value", "accrued_interest"],
        "2026-10-16",
    );
    let answer = crate::compute(&doc).unwrap();
    assert!(answer.not_evaluated.is_empty(), "{:?}", reasons(&answer));
    let mv = answer
        .figures
        .iter()
        .find(|f| f.measure.definition == "market_value")
        .unwrap();
    // 100 of face at 90.5: 90.50.
    assert_eq!(mv.value.as_ref().unwrap().text(), "90.50");
    // 4.65% on 30/360 from 2026-08-23 to 2026-10-16: 53 days, 0.684583...
    // per 100 as of the job's date.
    let accrued = figure(&answer, "accrued_interest", "INS-BOND0", "accrued").unwrap();
    let clean = figure(&answer, "accrued_interest", "INS-BOND0", "clean").unwrap();
    let dirty = figure(&answer, "accrued_interest", "INS-BOND0", "dirty").unwrap();
    assert_eq!(accrued, 0.68);
    assert_eq!(clean, 90.5);
    assert_eq!(dirty, 91.18);
}

#[test]
fn a_callable_bond_minted_without_its_calls_is_not_evaluated_naming_them() {
    let mut doc = bonds_document(
        &[corporate()],
        &curve(),
        &["bond_yield", "market_value", "accrued_interest"],
        "2026-10-16",
    );
    doc["instruments"][1]["debt"]["missing_fields"] =
        json!(["calls", "minimum_denomination", "payment_calendar_id"]);
    let answer = crate::compute(&doc).unwrap();
    assert_eq!(
        reasons(&answer),
        vec![("bond_yield", TERMS_INCOMPLETE, "calls, payment_calendar_id")],
        "never a yield to maturity in its place"
    );
    // Its close still marks it, and its accrual reads its coupon's terms
    // alone; a denomination holds no figure back.
    assert!(answer
        .figures
        .iter()
        .any(|f| f.measure.definition == "accrued_interest"));
    assert!(answer
        .figures
        .iter()
        .any(|f| f.measure.definition == "market_value"));
    assert!(!answer
        .figures
        .iter()
        .any(|f| f.measure.definition == "bond_yield"));
}

#[test]
fn yield_to_worst_names_the_call_date_it_is_to() {
    let mut bond = corporate();
    bond["clean"] = json!("104");
    let mut doc = bonds_document(&[bond], &curve(), &["bond_yield"], "2026-10-16");
    doc["instruments"][1]["debt"]["terms"]["calls"] = json!([
        {"kind": "REDEMPTION_KIND_MAKE_WHOLE", "from_date": "2026-02-23", "to_date": "2028-02-22",
         "make_whole_spread_bps": "15", "style": "REDEMPTION_STYLE_CONTINUOUS"},
        {"kind": "REDEMPTION_KIND_PAR", "from_date": "2028-02-23", "price": "100",
         "style": "REDEMPTION_STYLE_CONTINUOUS"},
    ]);
    let answer = crate::compute(&doc).unwrap();
    assert!(answer.not_evaluated.is_empty(), "{:?}", reasons(&answer));
    let worst = answer
        .figures
        .iter()
        .find(|f| {
            f.breakdown
                .contains(&("yield_kind".into(), "to_worst".into()))
        })
        .unwrap();
    // Above par, the par call is the worst; the make-whole window is never
    // a yield's date.
    assert!(
        worst
            .breakdown
            .contains(&("to_date".into(), "2028-02-23".into())),
        "{:?}",
        worst.breakdown
    );
    let to_call = figure(&answer, "bond_yield", "INS-BOND0", "to_call").unwrap();
    let to_maturity = figure(&answer, "bond_yield", "INS-BOND0", "to_maturity").unwrap();
    assert_eq!(worst.value.as_ref().unwrap().to_f64(), to_call);
    assert!(to_call < to_maturity);
}

#[test]
fn a_price_from_a_quoted_yield_is_the_inverse_of_the_yield_and_a_treasurys_exact() {
    // Treasury's 10-year note, a when-issued print in yield alone.
    let bonds: Value = serde_json::from_str(BONDS).unwrap();
    let note = bonds["bonds"][1].clone();
    let mut doc = bonds_document(&[note], &curve(), &["bond_price"], "2026-10-16");
    doc["market"] = json!([]);
    doc["quoted_yields"] = json!([{
        "subject_id": "INS-BOND0", "kind": "last", "instrument_id": "INS-USD",
        "dataset": "trace:prints", "valid_at_ns": "1", "row_key": "p1", "version": "1",
        "basis": "PRICE_BASIS_YIELD",
        "yield": {"value": "4.687528", "kind": "YIELD_KIND_TO_MATURITY"},
    }]);
    let answer = crate::compute(&doc).unwrap();
    assert!(answer.not_evaluated.is_empty(), "{:?}", reasons(&answer));
    let clean = figure(&answer, "bond_price", "INS-BOND0", "clean").unwrap();
    // The yield 99.5 is at, to six places, back to 99.5 within its rounding.
    assert!((clean - 99.5).abs() < 1e-4, "{clean}");
    let exact = answer
        .figures
        .iter()
        .find(|f| f.breakdown.contains(&("component".into(), "clean".into())))
        .unwrap();
    assert_eq!(exact.computed_as, EXACT, "Appendix B's price is exact");
}

#[test]
fn the_treasury_curve_answers_par_zero_and_discount_at_each_tenor() {
    let doc = bonds_document(&[], &curve(), &["treasury_curve"], "2026-10-16");
    let mut doc = doc;
    doc["job"]["account_ids"] = json!([]);
    let answer = crate::compute(&doc).unwrap();
    assert!(answer.not_evaluated.is_empty(), "{:?}", reasons(&answer));
    let at = |kind: &str, tenor: &str| {
        answer
            .figures
            .iter()
            .find(|f| {
                f.breakdown
                    == vec![
                        ("curve".to_string(), kind.to_string()),
                        ("tenor".to_string(), tenor.to_string()),
                    ]
            })
            .and_then(|f| f.value.as_ref())
            .map(|v| v.to_f64())
            .unwrap()
    };
    // Treasury's 10-year par yield on 2026-10-16 was 5.09.
    assert!((at("par", "10Y") - 5.09).abs() < 1e-8);
    assert_eq!(answer.figures.len(), 14 * 3);
    let zero = at("zero", "10Y") / 100.0;
    let discount = at("discount", "10Y");
    // 2026-10-16 to 2036-10-16: 3,653 days, three February 29ths.
    let years = 3653.0 / 365.0;
    assert!((discount - (-zero * years).exp()).abs() < 1e-7);
    let models: Vec<&str> = answer.provenance["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(models, vec!["monotone_convex"]);
}

#[test]
fn a_treasury_curve_missing_is_not_evaluated_naming_it() {
    let mut doc = bonds_document(&[], &curve(), &["treasury_curve"], "2026-10-16");
    doc["job"]["account_ids"] = json!([]);
    doc["treasury_curve"] =
        json!({"missing": "the risk-free curve ust_cmt: no par yields at the as-of"});
    let answer = crate::compute(&doc).unwrap();
    assert_eq!(answer.not_evaluated[0].reason, PRICE_MISSING);
}

#[test]
fn key_rate_durations_sum_to_about_the_effective_duration() {
    let doc = bonds_document(
        &[corporate()],
        &curve(),
        &["key_rate_durations", "bond_risk"],
        "2026-10-16",
    );
    let answer = crate::compute(&doc).unwrap();
    assert!(answer.not_evaluated.is_empty(), "{:?}", reasons(&answer));
    let sum: f64 = answer
        .figures
        .iter()
        .filter(|f| f.measure.definition == "key_rate_durations")
        .map(|f| f.value.as_ref().unwrap().to_f64())
        .sum();
    let modified = figure(&answer, "bond_risk", "INS-BOND0", "modified_duration").unwrap();
    // Bumping each par yield a basis point is a parallel shift in sum; a
    // spread-duration, so within a few per cent of the yield's modified
    // duration.
    assert!(
        (sum - modified).abs() / modified < 0.05,
        "{sum} vs {modified}"
    );
}

/// A pool of 1,000,000 original face at 6%, 360 months from 2025-01-01,
/// with factors for `months` months before 2026-10.
fn pool_document(months: &[(&str, &str)], measures: &[&str]) -> Value {
    let mut doc = bonds_document(&[], &curve(), measures, "2026-10-16");
    doc["instruments"].as_array_mut().unwrap().push(json!({
        "instrument_id": "INS-POOL", "model_id": "bond_cash_flows", "asset_class": "debt",
        "currency_instrument_id": "INS-USD", "price_kind": "close",
        "debt": {"type": "mbs_pool", "terms": {
            "agency": "MORTGAGE_AGENCY_FNMA", "program": "UMBS 30-year", "pool_number": "MA0001",
            "pass_through_rate": "6", "issue_date": "2025-01-01", "maturity_date": "2055-01-01",
            "original_face": "1000000", "original_term_months": 360,
            "day_count": "DAY_COUNT_THIRTY_360_BOND_BASIS", "payment_delay_days": 55, "payment_day": 25,
        }, "missing_fields": []},
    }));
    doc["positions"] = json!([{"account_id": "ACC-1", "instrument_id": "INS-POOL", "quantity": "1000000", "cost": "1000000"}]);
    doc["market"] = json!([{
        "subject_id": "INS-POOL", "kind": "close", "amount": "101.5", "instrument_id": "INS-USD",
        "dataset": "pools:closes", "valid_at_ns": "1", "row_key": "INS-POOL", "version": "1",
        "basis": "PRICE_BASIS_PERCENT_OF_PAR",
    }]);
    doc["factors"] = Value::Array(
        months
            .iter()
            .map(|(month, factor)| json!({
                "subject_id": "INS-POOL", "factor": factor, "factor_month": month,
                "dataset": "gse:factors", "row_key": format!("INS-POOL:{month}"), "version": "1", "source_ns": "1",
            }))
            .collect(),
    );
    doc
}

#[test]
fn a_pool_with_four_months_of_factors_is_not_evaluated_for_six_and_twelve() {
    let doc = pool_document(
        &[
            ("202606", "0.9501"),
            ("202607", "0.9452"),
            ("202608", "0.9398"),
            ("202609", "0.9351"),
        ],
        &["historical_prepayment", "market_value", "wal_at_cpr"],
    );
    let answer = crate::compute(&doc).unwrap();
    let insufficient: Vec<&str> = answer
        .not_evaluated
        .iter()
        .filter(|u| u.reason == HISTORY_INSUFFICIENT)
        .map(|u| u.detail.split(':').next().unwrap())
        .collect();
    assert_eq!(insufficient, vec!["12m", "6m"], "{:?}", reasons(&answer));
    for window in ["1m", "3m", "life"] {
        let cpr = answer
            .figures
            .iter()
            .find(|f| {
                f.breakdown.contains(&("window".into(), window.into()))
                    && f.breakdown.contains(&("component".into(), "cpr".into()))
            })
            .and_then(|f| f.value.as_ref())
            .unwrap()
            .to_f64();
        assert!(cpr > 0.0 && cpr < 100.0, "{window}: {cpr}");
    }
    // Its market value: original face times the factor times the close.
    let mv = answer
        .figures
        .iter()
        .find(|f| f.measure.definition == "market_value")
        .unwrap();
    assert_eq!(mv.value.as_ref().unwrap().text(), "949126.50");
    let wal = figure(&answer, "wal_at_cpr", "INS-POOL", "wal").unwrap();
    assert!(wal > 5.0 && wal < 20.0, "{wal}");
    assert!(figure(&answer, "wal_at_cpr", "INS-POOL", "yield").is_some());
}

#[test]
fn a_pool_with_no_factor_is_not_valued() {
    let doc = pool_document(&[], &["market_value"]);
    let answer = crate::compute(&doc).unwrap();
    assert_eq!(
        reasons(&answer),
        vec![("market_value", PRICE_MISSING, "factor")]
    );
}

#[test]
fn a_tba_reads_its_price_basis() {
    let mut doc = pool_document(&[], &["market_value"]);
    doc["instruments"][1] = json!({
        "instrument_id": "INS-POOL", "model_id": "quoted_price", "asset_class": "debt",
        "currency_instrument_id": "INS-USD", "price_kind": "close",
        "debt": {"type": "tba", "terms": {"agency": "MORTGAGE_AGENCY_FNMA",
            "program": "UMBS 30-year", "pass_through_rate": "6", "original_term_months": 360,
            "settlement_month": "202611"}, "missing_fields": []},
    });
    doc["market"][0]["amount"] = json!("101.15625");
    let answer = crate::compute(&doc).unwrap();
    // The platform's case: 1,000,000 at 101.15625 percent of par.
    assert_eq!(
        answer.figures[0].value.as_ref().unwrap().text(),
        "1011562.50"
    );
    doc["market"][0]["basis"] = json!("PRICE_BASIS_YIELD");
    let answer = crate::compute(&doc).unwrap();
    assert_eq!(answer.not_evaluated[0].reason, PRICE_MISSING);
    assert!(answer.not_evaluated[0].detail.contains("PRICE_BASIS_YIELD"));
}

#[test]
fn the_make_whole_amount_is_par_or_more_and_discounts_at_the_constant_maturity_plus_its_spread() {
    let mut bond = corporate();
    bond["clean"] = json!("90.5");
    let mut doc = bonds_document(&[bond], &curve(), &["make_whole_amount"], "2026-10-16");
    doc["instruments"][1]["debt"]["terms"]["calls"] = json!([
        {"kind": "REDEMPTION_KIND_MAKE_WHOLE", "from_date": "2026-02-23", "to_date": "2045-08-22",
         "make_whole_spread_bps": "15", "make_whole_benchmark_instrument_id": "INS-UST",
         "style": "REDEMPTION_STYLE_CONTINUOUS"},
        {"kind": "REDEMPTION_KIND_PAR", "from_date": "2045-08-23", "price": "100",
         "style": "REDEMPTION_STYLE_CONTINUOUS"},
    ]);
    doc["par_curves"] = json!([doc["treasury_curve"].clone()]);
    let answer = crate::compute(&doc).unwrap();
    assert!(answer.not_evaluated.is_empty(), "{:?}", reasons(&answer));
    // 4.65% against the Treasury rate near 5.4% plus 15: below par, so par.
    assert_eq!(
        figure(&answer, "make_whole_amount", "INS-BOND0", "price").unwrap(),
        100.0
    );
    // At 3% it is above par.
    for point in doc["par_curves"][0]["points"].as_array_mut().unwrap() {
        point["par_yield"] = json!("3");
    }
    let answer = crate::compute(&doc).unwrap();
    let price = figure(&answer, "make_whole_amount", "INS-BOND0", "price").unwrap();
    assert!(price > 115.0 && price < 130.0, "{price}");
    let amount = figure(&answer, "make_whole_amount", "INS-BOND0", "amount").unwrap();
    assert!((amount - price).abs() < 1e-2, "100 of face");
}

#[test]
fn g33s_one_period_formula_prices_a_municipal_in_its_last_period() {
    // MSRB Rule G-33(b)(i)(A): P = (RV + 100R/M) / (1 + ((E - A)/E)(Y/M))
    // - (A/E)(100R/M), a 5% municipal maturing 2027-01-15, settled
    // 2026-10-19 at a yield of 3.5%.
    let bonds: Value = serde_json::from_str(BONDS).unwrap();
    let mut muni = bonds["bonds"][2].clone();
    muni["issue"] = json!("2025-01-15");
    muni["dated"] = json!("2025-01-15");
    muni["first"] = json!("2025-07-15");
    muni["maturity"] = json!("2027-01-15");
    let (e, a) = (180.0, 94.0); // 2026-07-15 to 2026-10-19 on 30/360.
    let (r, y, m) = (0.05, 0.035, 2.0);
    let price =
        (100.0 + 100.0 * r / m) / (1.0 + ((e - a) / e) * (y / m)) - (a / e) * (100.0 * r / m);
    muni["clean"] = json!(format!("{price:.9}"));
    let doc = bonds_document(&[muni], &curve(), &["bond_yield"], "2026-10-16");
    let answer = crate::compute(&doc).unwrap();
    let ytm = figure(&answer, "bond_yield", "INS-BOND0", "to_maturity").unwrap();
    assert_eq!(ytm, 3.5);
}

/// The CPR a figure states, by its window.
fn cpr_of(answer: &crate::Answer, definition: &str, window: &str) -> Option<f64> {
    answer
        .figures
        .iter()
        .find(|f| {
            f.measure.definition == definition
                && f.breakdown.contains(&("window".into(), window.into()))
                && f.breakdown.contains(&("component".into(), "cpr".into()))
        })
        .and_then(|f| f.value.as_ref())
        .map(|v| v.to_f64())
}

#[test]
fn wal_at_cpr_runs_at_the_pools_own_three_month_speed_and_names_it() {
    // Ruling 996: the 3-month historical CPR, named; then life where three
    // months are not held; not evaluated where neither is.
    let months = [
        ("202606", "0.9501"),
        ("202607", "0.9452"),
        ("202608", "0.9398"),
        ("202609", "0.9351"),
    ];
    let doc = pool_document(&months, &["historical_prepayment", "wal_at_cpr"]);
    let answer = crate::compute(&doc).unwrap();
    let three = cpr_of(&answer, "historical_prepayment", "3m").unwrap();
    assert_eq!(cpr_of(&answer, "wal_at_cpr", "3m"), Some(three));
    let wal = answer
        .figures
        .iter()
        .find(|f| {
            f.measure.definition == "wal_at_cpr"
                && f.breakdown.contains(&("window".into(), "3m".into()))
                && f.breakdown.contains(&("component".into(), "wal".into()))
        })
        .unwrap();
    assert!(wal.value.as_ref().unwrap().to_f64() > 1.0);

    let doc = pool_document(&[("202609", "0.9351")], &["wal_at_cpr"]);
    let answer = crate::compute(&doc).unwrap();
    assert!(cpr_of(&answer, "wal_at_cpr", "3m").is_none());
    assert!(
        cpr_of(&answer, "wal_at_cpr", "life").is_some(),
        "{:?}",
        reasons(&answer)
    );

    // A pool in its issue month has no speed of its own.
    let mut doc = pool_document(&[("202609", "1")], &["wal_at_cpr"]);
    doc["instruments"][1]["debt"]["terms"]["issue_date"] = json!("2026-09-01");
    let answer = crate::compute(&doc).unwrap();
    assert_eq!(answer.not_evaluated.len(), 1, "{:?}", reasons(&answer));
    assert_eq!(answer.not_evaluated[0].reason, HISTORY_INSUFFICIENT);
    assert!(answer
        .figures
        .iter()
        .all(|f| f.measure.definition != "wal_at_cpr"));
}

#[test]
fn a_pool_amortizing_on_its_stated_wac_over_wala_and_wam_prepays_nothing() {
    // Ruling 995: the disclosure's WAC 6.75 over WALA 21 + WAM 339 months;
    // factors falling exactly on that schedule are no prepayment, which the
    // coupon's 6% over 360 from issue would not read so.
    let at = |age: u32| super::mbs::scheduled(0.0675, 360, age);
    let months: Vec<(String, String)> = (0..4)
        .map(|k| (format!("20260{}", 6 + k), format!("{:.12}", at(18 + k))))
        .collect();
    let named: Vec<(&str, &str)> = months
        .iter()
        .map(|(m, f)| (m.as_str(), f.as_str()))
        .collect();
    let mut doc = pool_document(&named, &["historical_prepayment"]);
    let without = cpr_of(
        &crate::compute(&doc).unwrap(),
        "historical_prepayment",
        "3m",
    )
    .unwrap();
    assert!(
        without.abs() > 0.01,
        "the coupon's schedule reads a speed: {without}"
    );
    for row in doc["factors"].as_array_mut().unwrap() {
        if row["factor_month"] == "202609" {
            row["wac"] = json!("6.75");
            row["wam"] = json!("339");
            row["wala"] = json!("21");
        }
    }
    let with = cpr_of(
        &crate::compute(&doc).unwrap(),
        "historical_prepayment",
        "3m",
    )
    .unwrap();
    assert!(with.abs() < 1e-6, "{with}");
}

#[test]
fn a_term_the_engine_does_not_compute_is_not_supported_and_named() {
    // Ruling 997: BUS/252, actual/actual (AFB) and (ICMA ultimo), a
    // floater's index projection and a TIPS's index ratio.
    for count in [
        "DAY_COUNT_BUS_252",
        "DAY_COUNT_ACT_ACT_AFB",
        "DAY_COUNT_ACT_ACT_ICMA_ULTIMO",
    ] {
        let mut doc = bonds_document(&[corporate()], &curve(), &["bond_yield"], "2026-10-16");
        doc["instruments"][1]["debt"]["terms"]["day_count"] = json!(count);
        let answer = crate::compute(&doc).unwrap();
        let said = reasons(&answer);
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].1, NOT_SUPPORTED, "{said:?}");
        assert!(said[0].2.contains(count), "{said:?}");
    }
    let mut doc = bonds_document(&[corporate()], &curve(), &["bond_yield"], "2026-10-16");
    doc["instruments"][1]["debt"]["terms"]["coupon_type"] = json!("COUPON_TYPE_FLOATING");
    let answer = crate::compute(&doc).unwrap();
    assert_eq!(answer.not_evaluated[0].reason, NOT_SUPPORTED);
    assert!(answer.not_evaluated[0].detail.contains("index projection"));
    // An empty count is the terms' incompleteness, as before.
    let mut doc = bonds_document(&[corporate()], &curve(), &["bond_yield"], "2026-10-16");
    doc["instruments"][1]["debt"]["terms"]["day_count"] = json!("");
    let answer = crate::compute(&doc).unwrap();
    assert_eq!(answer.not_evaluated[0].reason, TERMS_INCOMPLETE);
}
