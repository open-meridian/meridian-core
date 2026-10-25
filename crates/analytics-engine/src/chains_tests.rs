//! v28's chain measures over a synthetic chain whose quotes a known
//! volatility priced: each measure recovers what priced it, and says why
//! where it cannot.

use serde_json::{json, Value};

use crate::answer::*;
use crate::chains::{self, inverse_cdf};
use crate::options::{american_binomial, black_scholes, Right};

const AS_OF: i64 = 1_792_800_000_000_000_000;
const DAY: i64 = 86_400_000_000_000;

fn osi(expiry: &str, right: Right, strike: f64) -> String {
    format!(
        "SPX   {}{}{:08}",
        &expiry.replace('-', "")[2..],
        if right == Right::Call { "C" } else { "P" },
        (strike * 1000.0).round() as i64
    )
}

/// A European index chain priced by Black-Scholes at `vol`, the forward
/// carried at `yield_`, with a spread of `spread` around each price.
fn index_chain(vol: f64, spread: f64, days: &[i64]) -> Value {
    let spot = 5000.0;
    let rate = 0.04;
    let yield_ = 0.015;
    let mut series = Vec::new();
    for &d in days {
        let t = (d * DAY) as f64 / 60e9 / 525_600.0;
        let expiry = meridian_domain::date::Date::of_utc_instant(AS_OF + d * DAY).to_string();
        for k in (0..21).map(|i| 4500.0 + 50.0 * i as f64) {
            for right in [Right::Call, Right::Put] {
                let p = black_scholes(right, spot, k, t, rate, yield_, vol).price;
                series.push(json!({
                    "series_key": [{"scheme": "occ_osi", "value": osi(&expiry, right, k)}],
                    "right": if right == Right::Call { "call" } else { "put" },
                    "strike": format!("{k:.2}"),
                    "expiration_date": expiry,
                    "settles_at_ns": (AS_OF + d * DAY).to_string(),
                    "exercise_style": "european",
                    "model_id": "black_scholes",
                    "multiplier": "100",
                    "bid": format!("{:.6}", (p - spread).max(0.0)),
                    "ask": format!("{:.6}", p + spread),
                    "quote_dataset": "tradier-1:options",
                    "quote_source_ns": AS_OF.to_string(),
                    "open_interest": format!("{}", 1000 + (k as i64 % 7) * 10),
                    "open_interest_prior": "1000",
                }));
            }
        }
    }
    json!({
        "underlying_instrument_id": "INS-SPX",
        "currency_instrument_id": "INS-USD",
        "spot": {"amount": format!("{spot}"), "dataset": "tradier-1:options", "source_ns": AS_OF.to_string()},
        "rate": {"value": format!("{rate}"), "dataset": "us-treasury-1:par", "subject_id": "INS-UST"},
        "dividends": [],
        "underlying_kind": "index",
        "today": "2026-10-24",
        "series": series,
        "datasets": ["tradier-1:options"],
        "underlying_volume": "3000000",
    })
}

fn inputs(chains: Vec<Value>, measures: Vec<(&str, i64, Vec<&str>)>) -> Value {
    json!({
        "engine_version": "test",
        "job": {
            "account_ids": [],
            "measures": measures.iter().map(|(d, v, b)| json!({
                "definition": d, "version": v, "breakdowns": b, "event_group_id": ""
            })).collect::<Vec<_>>(),
        },
        "lake_as_of_ns": AS_OF.to_string(),
        "chains": chains,
    })
}

fn figures<'a>(answer: &'a crate::Answer, definition: &str) -> Vec<&'a Figure> {
    answer
        .figures
        .iter()
        .filter(|f| f.measure.definition == definition)
        .collect()
}

fn value(f: &Figure) -> f64 {
    f.value.as_ref().unwrap().to_f64()
}

#[test]
fn implied_volatility_recovers_the_volatility_that_priced_the_chain() {
    let doc = inputs(
        vec![index_chain(0.22, 0.0, &[30])],
        vec![("implied_volatility", 1, vec!["series", "quote_side"])],
    );
    let answer = crate::compute(&doc).unwrap();
    let ivs = figures(&answer, "implied_volatility");
    assert!(!ivs.is_empty());
    for f in &ivs {
        assert_eq!(f.computed_as, MODEL);
        assert!(
            !f.series_key.is_empty(),
            "a series not held is named by its key"
        );
        // Deep options priced near intrinsic invert loosely; the rest exactly.
        assert!(
            (value(f) - 0.22).abs() < 2e-3,
            "{:?} {}",
            f.breakdown,
            value(f)
        );
    }
    let models = &answer.provenance["models"];
    assert!(models.to_string().contains("black_scholes"));
    assert!(answer.provenance["datasets"]
        .to_string()
        .contains("tradier-1:options"));
}

#[test]
fn a_quote_below_intrinsic_is_not_evaluated_and_says_so() {
    let mut chain = index_chain(0.22, 0.0, &[30]);
    // A deep call quoted under its discounted intrinsic.
    let call = chain["series"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s["strike"] == "4500.00" && s["right"] == "call")
        .unwrap();
    call["bid"] = json!("1.00");
    let doc = inputs(vec![chain], vec![("implied_volatility", 1, vec![])]);
    let answer = crate::compute(&doc).unwrap();
    let below: Vec<_> = answer
        .not_evaluated
        .iter()
        .filter(|u| u.reason == BELOW_INTRINSIC)
        .collect();
    // The bid, and the mid it drags under intrinsic with it; the ask stands.
    let sides: Vec<&str> = below.iter().map(|u| u.detail.as_str()).collect();
    assert_eq!(sides, vec!["bid", "mid"]);
    assert!(below[0].series_key[0].1.contains("C04500000"));
}

#[test]
fn greeks_are_in_their_declared_units() {
    let doc = inputs(
        vec![index_chain(0.20, 0.0, &[60])],
        vec![("greeks", 1, vec!["series", "greek"])],
    );
    let answer = crate::compute(&doc).unwrap();
    let atm = osi(
        &meridian_domain::date::Date::of_utc_instant(AS_OF + 60 * DAY).to_string(),
        Right::Call,
        5000.0,
    );
    let greek = |name: &str| {
        answer
            .figures
            .iter()
            .find(|f| {
                f.breakdown.contains(&("series".into(), atm.clone()))
                    && f.breakdown.contains(&("greek".into(), name.into()))
            })
            .map(value)
            .unwrap()
    };
    let t = (60 * DAY) as f64 / 60e9 / 525_600.0;
    let g = black_scholes(Right::Call, 5000.0, 5000.0, t, 0.04, 0.015, 0.20);
    assert!((greek("delta") - g.delta).abs() < 1e-4);
    assert!(
        (greek("vega") - g.vega / 100.0).abs() < 1e-2,
        "vega per vol point"
    );
    assert!(
        (greek("theta") - g.theta / 365.0).abs() < 1e-2,
        "theta per calendar day"
    );
    assert!((greek("rho") - g.rho / 100.0).abs() < 1e-2, "rho per 1%");
}

#[test]
fn the_forward_comes_from_parity_and_the_surface_fits_a_flat_smile() {
    let doc = inputs(
        vec![index_chain(0.25, 0.05, &[30, 60, 90])],
        vec![
            ("implied_forward", 1, vec!["expiry"]),
            ("surface_fit", 1, vec!["expiry", "parameter"]),
            ("term_and_skew", 1, vec!["expiry"]),
        ],
    );
    let answer = crate::compute(&doc).unwrap();
    let t = (30 * DAY) as f64 / 60e9 / 525_600.0;
    let expected = 5000.0 * ((0.04 - 0.015) * t).exp();
    let forward = figures(&answer, "implied_forward")
        .into_iter()
        .find(|f| {
            f.breakdown
                .contains(&("component".into(), "forward".into()))
        })
        .map(value)
        .unwrap();
    assert!(
        (forward - expected).abs() < 0.05,
        "{forward} against {expected}"
    );
    let rmse: Vec<f64> = figures(&answer, "surface_fit")
        .into_iter()
        .filter(|f| f.breakdown.contains(&("parameter".into(), "rmse".into())))
        .map(value)
        .collect();
    assert_eq!(rmse.len(), 3, "{:?}", answer.not_evaluated);
    assert!(rmse.iter().all(|r| *r < 0.5), "{rmse:?} vol points");
    let atm: Vec<f64> = figures(&answer, "term_and_skew")
        .into_iter()
        .filter(|f| f.breakdown.contains(&("component".into(), "atm_iv".into())))
        .map(value)
        .collect();
    assert!(atm.iter().all(|v| (v - 0.25).abs() < 5e-3), "{atm:?}");
}

#[test]
fn a_surface_fit_that_fails_says_which_expiration() {
    let mut chain = index_chain(0.25, 0.05, &[30]);
    // Too few quotes in the slice to fit five parameters.
    chain["series"]
        .as_array_mut()
        .unwrap()
        .retain(|s| ["4950.00", "5000.00", "5050.00"].contains(&s["strike"].as_str().unwrap()));
    let doc = inputs(vec![chain], vec![("surface_fit", 1, vec![])]);
    let answer = crate::compute(&doc).unwrap();
    let failed: Vec<_> = answer
        .not_evaluated
        .iter()
        .filter(|u| u.reason == FIT_FAILED)
        .collect();
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0].detail.starts_with("2026-"),
        "{}",
        failed[0].detail
    );
}

#[test]
fn the_volatility_index_of_a_flat_strip_is_its_volatility() {
    let doc = inputs(
        vec![index_chain(0.20, 0.0, &[23, 37])],
        vec![("volatility_index", 1, vec![])],
    );
    let answer = crate::compute(&doc).unwrap();
    let index = value(figures(&answer, "volatility_index")[0]);
    // A strip of 21 strikes truncates the integral's tails: within a point.
    assert!((index - 20.0).abs() < 1.0, "{index}");
}

#[test]
fn max_pain_and_the_change_in_open_interest_are_exact() {
    let doc = inputs(
        vec![index_chain(0.20, 0.0, &[30])],
        vec![
            ("max_pain", 1, vec!["expiry"]),
            ("open_interest_change", 1, vec!["series"]),
        ],
    );
    let answer = crate::compute(&doc).unwrap();
    let pain = figures(&answer, "max_pain");
    assert_eq!(pain.len(), 1);
    assert_eq!(pain[0].computed_as, EXACT);
    let changes = figures(&answer, "open_interest_change");
    assert_eq!(changes.len(), 42);
    assert!(changes.iter().all(|f| f.computed_as == EXACT));
    assert!(changes
        .iter()
        .any(|f| f.value.as_ref().unwrap().text() == "60"));
}

#[test]
fn positioning_signs_follow_the_basis_its_version_fixes() {
    let doc = inputs(
        vec![index_chain(0.20, 0.0, &[30])],
        vec![
            ("positioning_exposure", 1, vec!["expiry", "greek", "series"]),
            ("positioning_exposure", 2, vec!["expiry", "greek", "series"]),
            ("positioning_levels", 1, vec![]),
        ],
    );
    let answer = crate::compute(&doc).unwrap();
    let gex = |version: i64, right: &str| -> f64 {
        answer
            .figures
            .iter()
            .filter(|f| {
                f.measure.definition == "positioning_exposure" && f.measure.version == version
            })
            .filter(|f| f.breakdown.contains(&("greek".into(), "gex".into())))
            .filter(|f| f.series_key[0].1.contains(right))
            .map(value)
            .sum()
    };
    assert!(gex(1, "C0") > 0.0 && gex(1, "P0") < 0.0);
    assert!(gex(2, "C0") < 0.0 && gex(2, "P0") < 0.0);
    assert!(figures(&answer, "positioning_levels").iter().any(|f| f
        .breakdown
        .contains(&("component".into(), "total_gamma_strike".into()))));
}

#[test]
fn a_grid_revalues_each_position_at_each_point_keyed_by_its_axes() {
    let mut chain = index_chain(0.20, 0.0, &[30]);
    let expiry = meridian_domain::date::Date::of_utc_instant(AS_OF + 30 * DAY).to_string();
    let held = osi(&expiry, Right::Call, 5000.0);
    for s in chain["series"].as_array_mut().unwrap() {
        if s["series_key"][0]["value"] == held.as_str() {
            s["instrument_id"] = json!("INS-CALL");
        }
    }
    let mut doc = inputs(vec![chain], vec![("scenario_grid", 1, vec!["instrument"])]);
    doc["job"]["account_ids"] = json!(["ACC-1"]);
    doc["accounts"] = json!([{"account_id": "ACC-1", "base_currency_instrument_id": "INS-USD"}]);
    doc["positions"] = json!([
        {"account_id": "ACC-1", "instrument_id": "INS-CALL", "quantity": "2"},
        {"account_id": "ACC-1", "instrument_id": "INS-SPX", "quantity": "-10"},
    ]);
    let spot = |v: &str| {
        json!({"object": "SHOCK_OBJECT_SPOT", "subject_id": "", "every": true,
        "form": "SHOCK_FORM_RELATIVE", "unit": "SHOCK_UNIT_PERCENT", "value": v, "buckets": []})
    };
    doc["scenario"] = json!({"row_key": "inline", "version": "0", "axes": [
        {"name": "spot", "steps": [{"name": "-5%", "shocks": [spot("-5")]}, {"name": "0", "shocks": []}]},
        {"name": "date", "steps": [{"name": "now", "shocks": []}, {"name": "expiry",
            "shocks": [{"object": "SHOCK_OBJECT_TIME", "subject_id": "", "every": false,
                        "form": "SHOCK_FORM_TO_SETTLEMENT", "unit": "SHOCK_UNIT_UNSPECIFIED",
                        "value": null, "buckets": []}]}]}]});
    let answer = crate::compute(&doc).unwrap();
    let grid = figures(&answer, "scenario_grid");
    assert_eq!(
        grid.len(),
        8,
        "two positions by four points: {:?}",
        answer.not_evaluated
    );
    let at = |instrument: &str, positions: Vec<u32>| {
        grid.iter()
            .find(|f| f.breakdown[0].1 == instrument && f.scenario.axis_positions == positions)
            .map(|f| value(f))
            .unwrap()
    };
    assert_eq!(grid[0].scenario.kind, SCENARIO_POINT);
    assert_eq!(grid[0].scenario.scenario_row_key, "inline");
    let t = (30 * DAY) as f64 / 60e9 / 525_600.0;
    let base = 2.0 * 100.0 * black_scholes(Right::Call, 5000.0, 5000.0, t, 0.04, 0.015, 0.20).price;
    assert!(
        (at("INS-CALL", vec![1, 0]) - base).abs() < 2.0,
        "{} {base}",
        at("INS-CALL", vec![1, 0])
    );
    // At the expiration column, intrinsic: a call at the money is worth nothing.
    assert_eq!(at("INS-CALL", vec![1, 1]), 0.0);
    assert!((at("INS-SPX", vec![0, 0]) + 10.0 * 4750.0).abs() < 1e-6);
    // The same inputs answer the same hash.
    assert_eq!(
        crate::compute(&doc).unwrap().answer_hash(),
        answer.answer_hash()
    );
}

#[test]
fn the_american_tree_inverts_what_it_priced() {
    // An American put on a dividend payer, priced by the tree itself.
    let t = (45 * DAY) as f64 / 60e9 / 525_600.0;
    let mut chain = index_chain(0.30, 0.0, &[45]);
    chain["underlying_kind"] = json!("equity");
    chain["spot"]["amount"] = json!("5000");
    for s in chain["series"].as_array_mut().unwrap() {
        let k: f64 = s["strike"].as_str().unwrap().parse().unwrap();
        let right = if s["right"] == "put" {
            Right::Put
        } else {
            Right::Call
        };
        let p = american_binomial(right, 5000.0, k, t, 0.04, 0.0, 0.30, &[], 200, true);
        s["exercise_style"] = json!("american");
        s["model_id"] = json!("american_binomial");
        s["bid"] = json!(format!("{p:.6}"));
        s["ask"] = json!(format!("{p:.6}"));
    }
    let doc = inputs(vec![chain], vec![("implied_volatility", 1, vec![])]);
    let answer = crate::compute(&doc).unwrap();
    let ivs = figures(&answer, "implied_volatility");
    assert!(ivs.len() > 60, "{}", ivs.len());
    let near: Vec<f64> = ivs.iter().map(|f| value(f)).collect();
    let within = near.iter().filter(|v| (*v - 0.30).abs() < 0.01).count();
    assert!(within * 10 >= near.len() * 9, "{near:?}");
    assert!(answer.provenance["models"]
        .to_string()
        .contains("american_binomial"));
}

#[test]
fn the_inverse_normal_inverts_the_normal() {
    for p in [0.001, 0.25, 0.5, 0.75, 0.999] {
        assert!((crate::options::cdf(inverse_cdf(p)) - p).abs() < 1e-12);
    }
    assert!(chains::is_chain_measure("greeks") && !chains::is_chain_measure("scenario_grid"));
}
