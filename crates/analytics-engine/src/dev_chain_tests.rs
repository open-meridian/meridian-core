//! A chain shaped as the dev survey saw it (rulings 790 and 791, 2026-10-27:
//! TQQQ, Tradier's delayed chain): an American single-name chain struck at
//! the previous session's close, read the next morning, quoted in cents
//! with the spreads a delayed retail feed carries -- wide deep in the
//! money, a zero bid far out of it. Its quotes a known surface priced on
//! the engine's own tree, so each measure is held to what priced it.

use serde_json::{json, Value};

use crate::answer::*;
use crate::options::{american_binomial, Right, Ssvi};

const DAY: i64 = 86_400_000_000_000;
const HOUR: i64 = 3_600_000_000_000;

/// 2026-10-26 20:00 UTC, the session's close (16:00 New York): every quote
/// was struck here.
pub const CLOSE: i64 = 1_793_044_800_000_000_000;
/// The next morning, 08:05 New York: the job's as-of.
pub const MORNING: i64 = CLOSE + 12 * HOUR + 5 * 60_000_000_000;
pub const SPOT: f64 = 68.43;
pub const RATE: f64 = 0.039;

/// The surface that priced the chain: power-law SSVI over an at-the-money
/// volatility falling from the near weekly's to the month's.
pub const TRUTH: Ssvi = Ssvi {
    rho: -0.55,
    eta: 1.0,
    gamma: 0.45,
};

pub fn atm_vol(t: f64) -> f64 {
    0.78 + 0.25 * (-t / 0.02).exp()
}

pub fn true_vol(strike: f64, t: f64) -> f64 {
    let theta = atm_vol(t).powi(2) * t;
    let k = (strike / (SPOT * (RATE * t).exp())).ln();
    (TRUTH.total_variance(k, theta) / t).sqrt()
}

/// The expirations within 45 days, and each one's settlement: 16:00 New
/// York, 20:00 UTC to November 1st and 21:00 after.
pub fn expiries(count: usize) -> Vec<(String, i64)> {
    [
        ("2026-10-28", 2),
        ("2026-10-30", 4),
        ("2026-11-06", 11),
        ("2026-11-13", 18),
        ("2026-11-20", 25),
        ("2026-11-27", 32),
        ("2026-12-04", 39),
        ("2026-12-11", 46),
    ]
    .iter()
    .take(count)
    .map(|(e, d)| {
        let settles = CLOSE + d * DAY + if *d > 5 { HOUR } else { 0 };
        (e.to_string(), settles)
    })
    .collect()
}

fn osi(expiry: &str, right: Right, strike: f64) -> String {
    format!(
        "TQQQ  {}{}{:08}",
        &expiry.replace('-', "")[2..],
        if right == Right::Call { "C" } else { "P" },
        (strike * 1000.0).round() as i64
    )
}

fn cents_down(x: f64) -> f64 {
    (x * 100.0 + 1e-9).floor() / 100.0
}

fn cents_up(x: f64) -> f64 {
    (x * 100.0 - 1e-9).ceil() / 100.0
}

/// The quote a delayed retail feed shows around a theoretical price: two
/// percent wide, a cent at least; half a dollar either side deep in the
/// money; a zero bid under half a cent.
pub fn quote(theo: f64, intrinsic: f64) -> (f64, f64) {
    if theo < 0.005 {
        return (0.0, 0.01);
    }
    let half = if intrinsic > 10.0 {
        0.25
    } else {
        (0.01 * theo).max(0.005)
    };
    ((cents_down(theo - half)).max(0.0), cents_up(theo + half))
}

/// The chain: `count` expirations, strikes from `low` to `high` a dollar
/// apart, priced at the close on a `steps`-step tree.
pub fn chain(count: usize, low: i64, high: i64, steps: usize) -> Value {
    let mut series = Vec::new();
    for (expiry, settles) in expiries(count) {
        let t = (settles - CLOSE) as f64 / 60e9 / 525_600.0;
        for k in low..=high {
            let strike = k as f64;
            let vol = true_vol(strike, t);
            for right in [Right::Call, Right::Put] {
                let theo =
                    american_binomial(right, SPOT, strike, t, RATE, 0.0, vol, &[], steps, true);
                let w = if right == Right::Call { 1.0 } else { -1.0 };
                let (bid, ask) = quote(theo, (w * (SPOT - strike)).max(0.0));
                series.push(json!({
                    "series_key": [{"scheme": "occ_osi", "value": osi(&expiry, right, strike)}],
                    "right": if right == Right::Call { "call" } else { "put" },
                    "strike": format!("{strike:.2}"),
                    "expiration_date": expiry,
                    "settles_at_ns": settles.to_string(),
                    "exercise_style": "american",
                    "model_id": "american_binomial",
                    "multiplier": "100",
                    "bid": format!("{bid:.2}"),
                    "ask": format!("{ask:.2}"),
                    "quote_dataset": "tradier:options_delayed",
                    "quote_source_ns": CLOSE.to_string(),
                    "open_interest": format!("{}", 200 + (k * 37 + if w > 0.0 { 11 } else { 3 }) % 900),
                    "open_interest_prior": "200",
                }));
            }
        }
    }
    json!({
        "underlying_instrument_id": "INS-TQQQ",
        "currency_instrument_id": "INS-USD",
        // The last sale at 16:15, after the options' close.
        "spot": {"amount": format!("{SPOT}"), "dataset": "tradier:live_delayed",
                 "source_ns": (CLOSE + 15 * 60_000_000_000i64).to_string()},
        "rate": {
            "dataset": "us-treasury:ust_par", "subject_id": "INS-UST",
            "points": [{"years": "0.083333", "rate": format!("{RATE}")}, {"years": "1", "rate": format!("{RATE}")}],
        },
        "dividends": [],
        "underlying_kind": "equity",
        "today": "2026-10-27",
        "series": series,
        "datasets": ["tradier:options_delayed"],
        "underlying_volume": "60000000",
    })
}

pub fn inputs(chains: Vec<Value>, as_of: i64, measures: Vec<(&str, i64, Vec<&str>)>) -> Value {
    json!({
        "engine_version": "test",
        "job": {
            "account_ids": [],
            "measures": measures.iter().map(|(d, v, b)| json!({
                "definition": d, "version": v, "breakdowns": b, "event_group_id": ""
            })).collect::<Vec<_>>(),
        },
        "lake_as_of_ns": as_of.to_string(),
        "chains": chains,
    })
}

fn has(f: &Figure, breakdown: &str, value: &str) -> bool {
    f.breakdown
        .iter()
        .any(|(b, v)| b == breakdown && v == value)
}

fn value(f: &Figure) -> f64 {
    f.value.as_ref().map(|v| v.to_f64()).unwrap_or(f64::NAN)
}

/// What the survey read, on the dev-like chain: printed, never asserted.
///
///     cargo test -p meridian-analytics-engine --release dev_chain_tests::report -- --ignored --nocapture
#[test]
#[ignore]
fn report() {
    let doc = inputs(
        vec![chain(8, 35, 100, 200)],
        MORNING,
        vec![
            ("implied_forward", 1, vec!["expiry"]),
            ("implied_volatility", 1, vec!["series", "quote_side"]),
            ("surface_fit", 1, vec!["expiry", "parameter"]),
        ],
    );
    let started = std::time::Instant::now();
    let answer = crate::compute(&doc).unwrap();
    println!("computed in {:?}", started.elapsed());
    for (expiry, settles) in expiries(8) {
        let t = (settles - CLOSE) as f64 / 60e9 / 525_600.0;
        let get = |definition: &str, b: &str, v: &str| {
            answer
                .figures
                .iter()
                .find(|f| {
                    f.measure.definition == definition && has(f, "expiry", &expiry) && has(f, b, v)
                })
                .map(value)
        };
        let fit = |p: &str| get("surface_fit", "parameter", p);
        let inside = answer
            .figures
            .iter()
            .find(|f| {
                f.measure.definition == "surface_fit"
                    && has(f, "expiry", &expiry)
                    && has(f, "parameter", "inside_bid_ask")
            })
            .map(|f| {
                format!(
                    "{}/{}",
                    f.numerator.as_ref().unwrap().to_f64(),
                    f.denominator.as_ref().unwrap().to_f64()
                )
            });
        println!(
            "{expiry}: forward {:?} (true {:.4}) borrow {:?} | rmse {:?} inside {:?} butterfly {:?} calendar {:?} atm truth {:.4}",
            get("implied_forward", "component", "forward"),
            SPOT * (RATE * t).exp(),
            get("implied_forward", "component", "borrow"),
            fit("rmse"),
            inside,
            fit("butterfly"),
            fit("calendar"),
            atm_vol(t),
        );
    }
    for p in ["ssvi_rho", "ssvi_eta", "ssvi_gamma"] {
        let v = answer
            .figures
            .iter()
            .find(|f| f.measure.definition == "surface_fit" && has(f, "parameter", p))
            .map(value);
        println!("{p}: {v:?}");
    }
    let mut reasons: std::collections::BTreeMap<(String, &str), usize> = Default::default();
    for u in &answer.not_evaluated {
        *reasons
            .entry((u.measure.definition.clone(), u.reason))
            .or_default() += 1;
    }
    for ((d, r), n) in &reasons {
        println!("not evaluated: {d} {r}: {n}");
    }
    for u in answer
        .not_evaluated
        .iter()
        .filter(|u| u.measure.definition == "surface_fit")
    {
        println!("  surface_fit: {}", u.detail);
    }
    // Mid IV against the truth, out of the money.
    let mut errors = Vec::new();
    for f in answer
        .figures
        .iter()
        .filter(|f| f.measure.definition == "implied_volatility" && has(f, "quote_side", "mid"))
    {
        let name = &f.breakdown[0].1;
        let strike: f64 = name[name.len() - 8..].parse::<f64>().unwrap() / 1000.0;
        let call = name.as_bytes()[name.len() - 9] == b'C';
        if (call && strike < SPOT) || (!call && strike > SPOT) {
            continue;
        }
        let expiry = format!("20{}-{}-{}", &name[6..8], &name[8..10], &name[10..12]);
        let settles = expiries(8)
            .into_iter()
            .find(|(e, _)| *e == expiry)
            .unwrap()
            .1;
        let t = (settles - CLOSE) as f64 / 60e9 / 525_600.0;
        errors.push((value(f) - true_vol(strike, t)).abs());
    }
    errors.sort_by(f64::total_cmp);
    println!(
        "mid IV against the truth, out of the money: {} figures, median error {:.4}, 90th {:.4}",
        errors.len(),
        errors[errors.len() / 2],
        errors[errors.len() * 9 / 10]
    );
}

fn surface_of(answer: &crate::Answer, expiry: &str, parameter: &str) -> Option<f64> {
    answer
        .figures
        .iter()
        .find(|f| {
            f.measure.definition == "surface_fit"
                && has(f, "expiry", expiry)
                && has(f, "parameter", parameter)
        })
        .map(value)
}

/// Ruling 791 on the dev-like chain: every expiry's raw SVI converges --
/// the near weeklies too -- inside its quotes, and SSVI fitted to every
/// slice's quotes moves off its starting point (rho -0.3, gamma 0.25) to
/// the surface that priced the chain; each quote's volatility is measured
/// from the close that struck it, not the morning that read it.
#[test]
fn the_dev_chain_fits_every_expiry_and_ssvi_finds_its_surface() {
    let doc = inputs(
        vec![chain(3, 50, 88, 200)],
        MORNING,
        vec![
            ("surface_fit", 1, vec!["expiry", "parameter"]),
            ("implied_volatility", 1, vec!["series", "quote_side"]),
        ],
    );
    let answer = crate::compute(&doc).unwrap();
    assert!(
        answer
            .not_evaluated
            .iter()
            .all(|u| u.measure.definition != "surface_fit"),
        "{:?}",
        answer
            .not_evaluated
            .iter()
            .filter(|u| u.measure.definition == "surface_fit")
            .collect::<Vec<_>>()
    );
    for (expiry, _) in expiries(3) {
        let rmse = surface_of(&answer, &expiry, "rmse").unwrap();
        assert!(rmse < 1.5, "{expiry}: rmse {rmse} vol points");
        assert_eq!(surface_of(&answer, &expiry, "butterfly"), Some(1.0));
        assert_eq!(surface_of(&answer, &expiry, "calendar"), Some(1.0));
    }
    let ssvi = |p: &str| surface_of(&answer, "", p).unwrap();
    assert!(
        (ssvi("ssvi_rho") - TRUTH.rho).abs() < 0.03,
        "{}",
        ssvi("ssvi_rho")
    );
    assert!(
        (ssvi("ssvi_eta") - TRUTH.eta).abs() < 0.08,
        "{}",
        ssvi("ssvi_eta")
    );
    assert!(
        (ssvi("ssvi_gamma") - TRUTH.gamma).abs() < 0.03,
        "{}",
        ssvi("ssvi_gamma")
    );
    // The mid IVs at the money, against the truth at the close's time.
    for (expiry, settles) in expiries(3) {
        let t = (settles - CLOSE) as f64 / 60e9 / 525_600.0;
        let osi = osi(&expiry, Right::Put, 68.0);
        let iv = answer
            .figures
            .iter()
            .find(|f| has(f, "series", &osi) && has(f, "quote_side", "mid"))
            .map(value)
            .unwrap();
        assert!(
            (iv - true_vol(68.0, t)).abs() < 0.01,
            "{expiry}: {iv} against {}",
            true_vol(68.0, t)
        );
    }
}

/// Ruling 791: a later expiry quoted under an earlier one is fitted above
/// it where it can be, and is never dropped: kept, its calendar check
/// failed and its strikes named where the quotes will not allow it.
#[test]
fn a_calendar_conflict_keeps_its_expiry_and_names_its_strikes() {
    let mut c = chain(2, 50, 88, 200);
    // The second expiry's quotes repriced 40% under the first's volatility.
    let (second, settles) = expiries(2)[1].clone();
    let t = (settles - CLOSE) as f64 / 60e9 / 525_600.0;
    for s in c["series"].as_array_mut().unwrap() {
        if s["expiration_date"] != second.as_str() {
            continue;
        }
        let k: f64 = s["strike"].as_str().unwrap().parse().unwrap();
        let right = if s["right"] == "call" {
            Right::Call
        } else {
            Right::Put
        };
        let theo = american_binomial(
            right,
            SPOT,
            k,
            t,
            RATE,
            0.0,
            0.6 * true_vol(k, t),
            &[],
            200,
            true,
        );
        let w = if right == Right::Call { 1.0 } else { -1.0 };
        let (bid, ask) = quote(theo, (w * (SPOT - k)).max(0.0));
        s["bid"] = json!(format!("{bid:.2}"));
        s["ask"] = json!(format!("{ask:.2}"));
    }
    let doc = inputs(
        vec![c],
        MORNING,
        vec![("surface_fit", 1, vec!["expiry", "parameter"])],
    );
    let answer = crate::compute(&doc).unwrap();
    assert!(
        surface_of(&answer, &second, "rmse").is_some(),
        "the later expiry is kept: {:?}",
        answer.not_evaluated
    );
    let calendar = surface_of(&answer, &second, "calendar").unwrap();
    let named: Vec<&Unevaluated> = answer
        .not_evaluated
        .iter()
        .filter(|u| {
            u.detail
                .contains("calendar arbitrage against 2026-10-28 at strikes")
        })
        .collect();
    if calendar == 0.0 {
        assert_eq!(named.len(), 1, "{:?}", answer.not_evaluated);
        assert_eq!(named[0].reason, FIT_FAILED);
        assert!(named[0].detail.starts_with(&second));
    } else {
        assert!(named.is_empty());
    }
}
