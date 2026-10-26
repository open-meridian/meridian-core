//! v28's measures over a chain (contract v28, track B; W12.1 to W12.3;
//! ruling 565; the options sample's section 2.2 and G14): implied
//! volatility at bid, mid and ask, the implied forward, greeks from the same
//! node, the SVI and SSVI surface and its residuals, term and skew, the
//! volatility index, the positioning measures, max pain and the change in
//! open interest, and the scenario grid over an account's positions.
//!
//! A job names its chains (`Job.chains`); the component resolves each into
//! the inputs document as `chains`: the underlying's price at the as-of,
//! the rate, the declared dividends, and every series in the window with its
//! terms, its quote, its open interest and its settlement instant -- found
//! through the row's venue's trading sessions, the dataset's calendar the
//! fallback (ruling 570 (i)) -- so the engine reads no store and no clock. A
//! series not held is named on its figures by its key, never minted (561).
//!
//! f64 inside the models only (the one-API spec's Q8): every figure leaves
//! rounded once, half to even, at its definition's scale, `COMPUTED_AS_MODEL`;
//! max pain and the change in open interest are exact.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::answer::*;
use crate::case::{decimal, Case, CaseError, MeasureKey, Result};
use crate::definitions::{Definition, COMPONENT};
use crate::exact::Dec;
use crate::measures::Part;
use crate::options::{
    american_binomial, american_greeks, black_76, black_scholes, butterfly_free, calendar_free,
    cdf, fit_ssvi, fit_svi, implied_vol, Dividend, Greeks, Right, SviRaw,
};
use crate::scenario::{Applied, MINUTES_A_YEAR};

pub const AMERICAN_BINOMIAL: &str = "american_binomial";
pub const BLACK_SCHOLES: &str = "black_scholes";
pub const BLACK_76: &str = "black_76";
pub const SVI: &str = "svi";
pub const SSVI: &str = "ssvi";

/// The tree's steps where a definition does not say (ruling 565: its steps a
/// definition parameter, measured over a T1 chain refresh; BENCHMARK.md).
pub const STEPS: usize = 200;

/// One series of a chain, as the document carries it.
#[derive(Debug, Clone)]
pub struct Series {
    pub key: SeriesKey,
    /// Its OCC symbol, or the first identifier's value: the `series`
    /// breakdown's value.
    pub name: String,
    /// Its `INS-` record where one is held; empty for a series not held.
    pub instrument_id: String,
    pub right: Right,
    pub strike: f64,
    pub strike_exact: Dec,
    pub expiry: String,
    /// The settlement instant: AM at the session's open, PM at 16:00 in the
    /// venue's zone; 0 where no session was found (`unsessioned` says why).
    pub settles_at_ns: i64,
    pub unsessioned: String,
    pub american: bool,
    /// The model its template's package binds (ruling 576); empty where
    /// none binds, `unbound` saying why.
    pub model: String,
    pub unbound: String,
    pub multiplier: f64,
    pub multiplier_exact: Dec,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub quote_dataset: String,
    pub quote_source_ns: i64,
    /// Contract v28 (ruling 421): the dataset whose only quote is invalid.
    pub quarantined: String,
    pub open_interest: Option<Dec>,
    pub open_interest_prior: Option<Dec>,
}

/// One chain: its underlying's market, and its series.
#[derive(Debug, Clone)]
pub struct Chain {
    pub underlying: String,
    pub currency_instrument_id: String,
    pub spot: Option<f64>,
    pub spot_source_ns: i64,
    pub spot_dataset: String,
    /// Why no price of the underlying was read, where the read said
    /// (ruling 734: a dataset served, not kept, named with its licence);
    /// "the underlying's price" otherwise.
    pub spot_missing: String,
    /// The risk-free curve (ruling 608; 611's `risk_free_rate`): each point
    /// its time in years and its rate, continuous; empty where none was
    /// read, `rate_missing` saying why, and every option not evaluated.
    pub curve: Vec<(f64, f64)>,
    pub rate_missing: String,
    pub rate_dataset: String,
    pub rate_subject: String,
    /// Each declared cash dividend: its ex instant and amount.
    pub dividends: Vec<(i64, f64)>,
    pub dividends_dataset: String,
    /// `equity`, `index` or `future`.
    pub underlying_kind: String,
    /// The as-of's date in the venue's zone (the 0DTE share's today).
    pub today: String,
    pub series: Vec<Series>,
    pub datasets: Vec<String>,
    /// The underlying's volume on the latest trading date, for gamma to
    /// liquidity; None where no statistic is held.
    pub underlying_volume: Option<f64>,
}

fn text(value: &Value, name: &str) -> String {
    match value.get(name) {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn int(value: &Value, name: &str) -> i64 {
    text(value, name).parse().unwrap_or(0)
}

fn optional(value: &Value, name: &str, what: &str) -> Result<Option<Dec>> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(t)) if t.is_empty() => Ok(None),
        some => decimal(some, what).map(Some),
    }
}

fn items<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The chains an inputs document carries.
pub fn read(inputs: &Value) -> Result<Vec<Chain>> {
    let mut out = Vec::new();
    for c in items(inputs, "chains") {
        let mut series = Vec::new();
        for s in items(c, "series") {
            let key: SeriesKey = items(s, "series_key")
                .iter()
                .map(|i| (text(i, "scheme"), text(i, "value")))
                .collect();
            let name = key
                .iter()
                .find(|(scheme, _)| scheme == "occ_osi")
                .or(key.first())
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let strike_exact = decimal(s.get("strike"), "a series' strike")?;
            let multiplier_exact = optional(s, "multiplier", "a series' multiplier")?
                .unwrap_or_else(|| Dec::from_int(1));
            series.push(Series {
                name,
                key,
                instrument_id: text(s, "instrument_id"),
                right: if text(s, "right") == "put" {
                    Right::Put
                } else {
                    Right::Call
                },
                strike: strike_exact.to_f64(),
                strike_exact,
                expiry: text(s, "expiration_date"),
                settles_at_ns: int(s, "settles_at_ns"),
                unsessioned: text(s, "unsessioned"),
                american: text(s, "exercise_style") != "european",
                model: text(s, "model_id"),
                unbound: text(s, "unbound"),
                multiplier: multiplier_exact.to_f64(),
                multiplier_exact,
                bid: optional(s, "bid", "a series' bid")?.map(|d| d.to_f64()),
                ask: optional(s, "ask", "a series' ask")?.map(|d| d.to_f64()),
                quote_dataset: text(s, "quote_dataset"),
                quote_source_ns: int(s, "quote_source_ns"),
                quarantined: text(s, "quarantined"),
                open_interest: optional(s, "open_interest", "open interest")?,
                open_interest_prior: optional(s, "open_interest_prior", "open interest")?,
            });
        }
        let spot = c.get("spot").filter(|v| !v.is_null());
        let rate = c.get("rate").filter(|v| !v.is_null());
        let mut curve: Vec<(f64, f64)> = Vec::new();
        if let Some(r) = rate {
            for point in items(r, "points") {
                curve.push((
                    decimal(point.get("years"), "a curve point's time")?.to_f64(),
                    decimal(point.get("rate"), "a curve point's rate")?.to_f64(),
                ));
            }
        }
        curve.sort_by(|a, b| a.0.total_cmp(&b.0));
        curve.dedup_by(|a, b| a.0 == b.0);
        out.push(Chain {
            underlying: text(c, "underlying_instrument_id"),
            currency_instrument_id: text(c, "currency_instrument_id"),
            spot: match spot {
                Some(s) => Some(decimal(s.get("amount"), "the underlying's price")?.to_f64()),
                None => None,
            },
            spot_source_ns: spot.map(|s| int(s, "source_ns")).unwrap_or(0),
            spot_dataset: spot.map(|s| text(s, "dataset")).unwrap_or_default(),
            spot_missing: Some(text(c, "spot_missing"))
                .filter(|said| !said.is_empty())
                .unwrap_or_else(|| "the underlying's price".to_string()),
            rate_missing: if curve.is_empty() {
                match rate.map(|r| text(r, "missing")).filter(|m| !m.is_empty()) {
                    Some(said) => said,
                    None => "the risk-free curve: none read at the as-of".to_string(),
                }
            } else {
                String::new()
            },
            curve,
            rate_dataset: rate.map(|r| text(r, "dataset")).unwrap_or_default(),
            rate_subject: rate.map(|r| text(r, "subject_id")).unwrap_or_default(),
            dividends: items(c, "dividends")
                .iter()
                .map(|d| {
                    Ok((
                        int(d, "ex_ns"),
                        decimal(d.get("amount"), "a dividend")?.to_f64(),
                    ))
                })
                .collect::<Result<_>>()?,
            dividends_dataset: text(c, "dividends_dataset"),
            underlying_kind: text(c, "underlying_kind"),
            today: text(c, "today"),
            series,
            datasets: items(c, "datasets")
                .iter()
                .map(|d| d.as_str().unwrap_or_default().to_string())
                .collect(),
            underlying_volume: optional(c, "underlying_volume", "the underlying's volume")?
                .map(|d| d.to_f64()),
        });
    }
    Ok(out)
}

/// Whether a definition is over a chain (W12.1): its units are the job's
/// chains, not its accounts.
pub fn is_chain_measure(definition: &str) -> bool {
    matches!(
        definition,
        "implied_volatility"
            | "implied_forward"
            | "greeks"
            | "surface_fit"
            | "surface_residual"
            | "term_and_skew"
            | "volatility_index"
            | "positioning_exposure"
            | "positioning_levels"
            | "max_pain"
            | "open_interest_change"
    )
}

/// A float into the engine's exact decimal, rounded once, half to even.
pub fn exact(x: f64, scale: u32) -> Option<Dec> {
    if !x.is_finite() {
        return None;
    }
    Dec::parse(&x.to_string())
        .ok()
        .map(|d| d.round_half_even(scale))
}

/// The inverse of the standard normal distribution (Acklam's rational
/// approximation, refined by one Halley step on [`cdf`]).
pub fn inverse_cdf(p: f64) -> f64 {
    let a = [
        -3.969683028665376e1,
        2.209460984245205e2,
        -2.759285104469687e2,
        1.383_577_518_672_69e2,
        -3.066479806614716e1,
        2.506628277459239,
    ];
    let b = [
        -5.447609879822406e1,
        1.615858368580409e2,
        -1.556989798598866e2,
        6.680131188771972e1,
        -1.328068155288572e1,
    ];
    let c = [
        -7.784894002430293e-3,
        -3.223964580411365e-1,
        -2.400758277161838,
        -2.549732539343734,
        4.374664141464968,
        2.938163982698783,
    ];
    let d = [
        7.784695709041462e-3,
        3.224671290700398e-1,
        2.445134137142996,
        3.754408661907416,
    ];
    let low = 0.02425;
    let x = if p < low {
        let q = (-2.0 * p.ln()).sqrt();
        (((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5])
            / ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0)
    } else if p <= 1.0 - low {
        let q = p - 0.5;
        let r = q * q;
        (((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5]) * q
            / (((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5])
            / ((((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0)
    };
    let e = cdf(x) - p;
    let u = e * (2.0 * std::f64::consts::PI).sqrt() * (x * x / 2.0).exp();
    x - u / (1.0 + x * u / 2.0)
}

/// Why a figure is not evaluated: a reason and its detail.
type Why = (&'static str, String);

/// One series, solved at the as-of: its time, its model's inputs, and its
/// implied volatility at each side.
#[derive(Debug, Clone)]
pub struct Solved {
    pub t: f64,
    pub spot: f64,
    pub rate: f64,
    /// The tree's borrow or the closed forms' yield, from parity.
    pub carry: f64,
    pub forward: f64,
    pub dividends: Vec<Dividend>,
    pub iv: [std::result::Result<f64, Why>; 3],
}

/// The quote sides, in the order the breakdown `quote_side` names them.
pub const SIDES: [&str; 3] = ["bid", "mid", "ask"];

/// A chain's solution: each series' (by its place in the chain) and each
/// expiry's forward.
pub struct ChainSolution {
    pub series: Vec<std::result::Result<Solved, Why>>,
    /// Per expiry: the forward and the implied carry (borrow or yield).
    pub forwards: BTreeMap<String, std::result::Result<(f64, f64), Why>>,
}

fn years(chain_as_of: i64, settles_at_ns: i64) -> f64 {
    (settles_at_ns - chain_as_of) as f64 / 60e9 / MINUTES_A_YEAR
}

fn mid(s: &Series) -> Option<f64> {
    match (s.bid, s.ask) {
        (Some(b), Some(a)) if a >= b && a > 0.0 => Some((b + a) / 2.0),
        _ => None,
    }
}

/// An option's model price at a volatility.
#[allow(clippy::too_many_arguments)]
pub fn price(
    model: &str,
    right: Right,
    spot: f64,
    strike: f64,
    t: f64,
    rate: f64,
    carry: f64,
    vol: f64,
    dividends: &[Dividend],
    steps: usize,
    american: bool,
) -> f64 {
    match model {
        BLACK_76 => black_76(right, spot, strike, t, rate, vol).price,
        BLACK_SCHOLES => black_scholes(right, spot, strike, t, rate, carry, vol).price,
        _ => american_binomial(
            right, spot, strike, t, rate, carry, vol, dividends, steps, american,
        ),
    }
}

/// An option's greeks at a volatility, from the same node as its IV.
#[allow(clippy::too_many_arguments)]
pub fn greeks_at(
    model: &str,
    right: Right,
    spot: f64,
    strike: f64,
    t: f64,
    rate: f64,
    carry: f64,
    vol: f64,
    dividends: &[Dividend],
    steps: usize,
) -> Greeks {
    match model {
        BLACK_76 => black_76(right, spot, strike, t, rate, vol),
        BLACK_SCHOLES => black_scholes(right, spot, strike, t, rate, carry, vol),
        _ => american_greeks(right, spot, strike, t, rate, carry, vol, dividends, steps),
    }
}

fn intrinsic(s: &Series, spot: f64, forward: f64, rate: f64, t: f64) -> f64 {
    let w = if s.right == Right::Call { 1.0 } else { -1.0 };
    if s.american {
        (w * (spot - s.strike)).max(0.0)
    } else {
        (w * (forward - s.strike)).max(0.0) * (-rate * t).exp()
    }
}

/// A natural cubic spline through the curve's points, read at `t` years
/// (ruling 608; 611's `rate_interpolation` `cubic_spline`, as Cboe's VIX
/// method derives a yield at each expiration from Treasury's curve); flat
/// before the first point and after the last. None for no curve.
pub fn spline_at(points: &[(f64, f64)], t: f64) -> Option<f64> {
    let n = points.len();
    let (first, last) = (points.first()?, points.last()?);
    if n == 1 || t <= first.0 {
        return Some(first.1);
    }
    if t >= last.0 {
        return Some(last.1);
    }
    // The second derivatives, natural at both ends (a tridiagonal solve).
    let h: Vec<f64> = (0..n - 1).map(|i| points[i + 1].0 - points[i].0).collect();
    let mut m = vec![0.0; n];
    let mut c = vec![0.0; n];
    let mut d = vec![0.0; n];
    for i in 1..n - 1 {
        let slope = |j: usize| (points[j + 1].1 - points[j].1) / h[j];
        let a = h[i - 1];
        let b = 2.0 * (h[i - 1] + h[i]);
        let rhs = 6.0 * (slope(i) - slope(i - 1));
        let denominator = b - a * c[i - 1];
        c[i] = h[i] / denominator;
        d[i] = (rhs - a * d[i - 1]) / denominator;
    }
    for i in (1..n - 1).rev() {
        m[i] = d[i] - c[i] * m[i + 1];
    }
    let i = (0..n - 1).find(|&i| t <= points[i + 1].0)?;
    let (x0, y0, x1, y1) = (points[i].0, points[i].1, points[i + 1].0, points[i + 1].1);
    let (a, b) = ((x1 - t) / h[i], (t - x0) / h[i]);
    Some(
        a * y0 + b * y1 + ((a.powi(3) - a) * m[i] + (b.powi(3) - b) * m[i + 1]) * h[i] * h[i] / 6.0,
    )
}

impl Chain {
    /// The risk-free rate to `t` years, continuous: the curve's spline
    /// there, or why there is none.
    pub fn rate_at(&self, t: f64) -> std::result::Result<f64, (&'static str, String)> {
        spline_at(&self.curve, t).ok_or((PRICE_MISSING, self.rate_missing.clone()))
    }
}

/// Solve a chain at the job's as-of: each expiry's forward from put-call
/// parity at the strike with the smallest |C - P| (W12.1), and each
/// series' implied volatility at bid, mid and ask.
pub fn solve(chain: &Chain, as_of_ns: i64, steps: usize) -> ChainSolution {
    let spot = chain.spot;
    let mut by_expiry: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in chain.series.iter().enumerate() {
        by_expiry.entry(s.expiry.as_str()).or_default().push(i);
    }
    let mut forwards = BTreeMap::new();
    for (expiry, members) in &by_expiry {
        let found = (|| {
            let Some(spot) = spot else {
                return Err((PRICE_MISSING, chain.spot_missing.clone()));
            };
            let first = &chain.series[members[0]];
            if first.settles_at_ns == 0 {
                return Err((PRICE_MISSING, first.unsessioned.clone()));
            }
            let t = years(as_of_ns, first.settles_at_ns);
            if t <= 0.0 {
                return Err((PRICE_MISSING, format!("{expiry} has settled")));
            }
            let rate = chain.rate_at(t)?;
            if chain.underlying_kind == "future" {
                return Ok((spot, rate));
            }
            let mut best: Option<(f64, f64, f64)> = None;
            for &i in members {
                let call = &chain.series[i];
                if call.right != Right::Call {
                    continue;
                }
                let Some(put) = members
                    .iter()
                    .map(|&j| &chain.series[j])
                    .find(|p| p.right == Right::Put && p.strike == call.strike)
                else {
                    continue;
                };
                if let (Some(c), Some(p)) = (mid(call), mid(put)) {
                    let gap = (c - p).abs();
                    if best.is_none_or(|b| gap < (b.1 - b.2).abs()) {
                        best = Some((call.strike, c, p));
                    }
                }
            }
            // Parity at the curve's rate: the forward and the borrow it
            // implies, never the rate itself (ruling 608's cross-check).
            let forward = match best {
                Some((k, c, p)) => k + (c - p) * (rate * t).exp(),
                None => spot * (rate * t).exp(),
            };
            if forward <= 0.0 {
                return Err((PRICE_MISSING, format!("{expiry}: no forward")));
            }
            Ok((forward, rate - (forward / spot).ln() / t))
        })();
        forwards.insert(expiry.to_string(), found);
    }
    let series = chain
        .series
        .iter()
        .map(|s| {
            let spot = spot.ok_or((PRICE_MISSING, chain.spot_missing.clone()))?;
            if !s.quarantined.is_empty() {
                return Err((INPUT_QUARANTINED, s.quarantined.clone()));
            }
            if s.model.is_empty() {
                return Err((IMPLEMENTATION_UNAVAILABLE, s.unbound.clone()));
            }
            if s.settles_at_ns == 0 {
                return Err((PRICE_MISSING, s.unsessioned.clone()));
            }
            let t = years(as_of_ns, s.settles_at_ns);
            if t <= 0.0 {
                return Err((PRICE_MISSING, format!("{} has settled", s.expiry)));
            }
            let (forward, implied) = forwards
                .get(&s.expiry)
                .cloned()
                .unwrap_or(Err((PRICE_MISSING, "no forward".into())))?;
            let rate = chain.rate_at(t)?;
            let dividends: Vec<Dividend> = chain
                .dividends
                .iter()
                .map(|(ex, amount)| Dividend {
                    t: years(as_of_ns, *ex),
                    amount: *amount,
                })
                .filter(|d| d.t > 0.0 && d.t <= t)
                .collect();
            // The tree's borrow: what makes its forward, the share less the
            // dividends' present value grown at r - b, the parity forward.
            let carry = match s.model.as_str() {
                BLACK_SCHOLES => implied,
                BLACK_76 => rate,
                _ => {
                    let pv: f64 = dividends
                        .iter()
                        .map(|d| d.amount * (-rate * d.t).exp())
                        .sum();
                    let net = spot - pv;
                    if net > 0.0 {
                        rate - (forward / net).ln() / t
                    } else {
                        0.0
                    }
                }
            };
            let floor = intrinsic(s, spot, forward, rate, t);
            let quotes = [s.bid, mid(s), s.ask];
            let iv = std::array::from_fn(|side| {
                let Some(quote) = quotes[side] else {
                    return Err((PRICE_MISSING, SIDES[side].to_string()));
                };
                if quote <= floor + 1e-9 {
                    return Err((BELOW_INTRINSIC, SIDES[side].to_string()));
                }
                implied_vol(quote, |vol| {
                    price(
                        &s.model, s.right, spot, s.strike, t, rate, carry, vol, &dividends, steps,
                        s.american,
                    )
                })
                .ok_or((
                    FIT_FAILED,
                    format!(
                        "{}: no volatility within 0.01% to 500% prices it",
                        SIDES[side]
                    ),
                ))
            });
            Ok(Solved {
                t,
                spot,
                rate,
                carry,
                forward,
                dividends,
                iv,
            })
        })
        .collect();
    ChainSolution { series, forwards }
}

/// One expiry's fitted slice: raw SVI, its time, and its forward.
#[derive(Debug, Clone, Copy)]
pub struct Fitted {
    pub svi: SviRaw,
    pub t: f64,
    pub forward: f64,
}

impl Fitted {
    pub fn vol(&self, strike: f64) -> f64 {
        let k = (strike / self.forward).ln();
        (self.svi.total_variance(k).max(1e-12) / self.t).sqrt()
    }
}

/// The surface: each expiry's raw SVI (out-of-the-money quotes, weights the
/// inverse IV bid-ask width), each held to Gatheral's butterfly condition
/// and the slices together to the calendar condition; SSVI across them.
pub struct Surface {
    pub slices: BTreeMap<String, std::result::Result<Fitted, Why>>,
    pub ssvi: Option<crate::options::Ssvi>,
    /// Per expiry: the RMSE in vol points and the strikes inside the IV
    /// bid-ask over the strikes fitted.
    pub quality: BTreeMap<String, (f64, usize, usize)>,
}

fn otm(s: &Series, forward: f64) -> bool {
    (s.right == Right::Call && s.strike >= forward) || (s.right == Right::Put && s.strike < forward)
}

pub fn surface(chain: &Chain, solution: &ChainSolution) -> Surface {
    let mut slices = BTreeMap::new();
    let mut quality = BTreeMap::new();
    let mut held: Vec<(String, Fitted)> = Vec::new();
    for (expiry, forward) in &solution.forwards {
        let Ok((forward, _)) = forward else {
            continue;
        };
        let mut points = Vec::new();
        let mut t = 0.0;
        for (s, solved) in chain.series.iter().zip(&solution.series) {
            let Ok(solved) = solved else { continue };
            if &s.expiry != expiry || !otm(s, *forward) {
                continue;
            }
            if let (Ok(bid), Ok(m), Ok(ask)) = (&solved.iv[0], &solved.iv[1], &solved.iv[2]) {
                t = solved.t;
                let width = (ask - bid).max(1e-4);
                points.push((
                    (s.strike / forward).ln(),
                    m * m * solved.t,
                    1.0 / width,
                    *bid,
                    *m,
                    *ask,
                ));
            }
        }
        let fit = fit_svi(&points.iter().map(|p| (p.0, p.1, p.2)).collect::<Vec<_>>());
        let Some(svi) = fit else {
            slices.insert(
                expiry.clone(),
                Err((
                    FIT_FAILED,
                    format!(
                        "{expiry}: raw SVI did not converge over {} strikes",
                        points.len()
                    ),
                )),
            );
            continue;
        };
        let ks: Vec<f64> = {
            let lo = points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
            let hi = points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
            (0..=40).map(|i| lo + (hi - lo) * i as f64 / 40.0).collect()
        };
        if !butterfly_free(|k| svi.total_variance(k), &ks) {
            slices.insert(
                expiry.clone(),
                Err((FIT_FAILED, format!("{expiry}: butterfly arbitrage"))),
            );
            continue;
        }
        let fitted = Fitted {
            svi,
            t,
            forward: *forward,
        };
        let mut squares = 0.0;
        let mut inside = 0;
        for p in &points {
            let v = (svi.total_variance(p.0).max(0.0) / t).sqrt();
            squares += ((v - p.4) * 100.0).powi(2);
            if v >= p.3 && v <= p.5 {
                inside += 1;
            }
        }
        quality.insert(
            expiry.clone(),
            ((squares / points.len() as f64).sqrt(), inside, points.len()),
        );
        held.push((expiry.clone(), fitted));
        slices.insert(expiry.clone(), Ok(fitted));
    }
    // Calendar: total variance never falls as expiry lengthens.
    held.sort_by(|a, b| a.1.t.total_cmp(&b.1.t));
    let grid: Vec<f64> = (-20..=20).map(|i| i as f64 * 0.05).collect();
    for pair in held.windows(2) {
        let (a, b) = (pair[0].1.svi, pair[1].1.svi);
        let w0 = move |k: f64| a.total_variance(k);
        let w1 = move |k: f64| b.total_variance(k);
        if !calendar_free(&[&w0, &w1], &grid) {
            slices.insert(
                pair[1].0.clone(),
                Err((
                    FIT_FAILED,
                    format!("{}: calendar arbitrage against {}", pair[1].0, pair[0].0),
                )),
            );
        }
    }
    let ssvi_slices: Vec<crate::options::Slice> = held
        .iter()
        .filter(|(e, _)| matches!(slices.get(e), Some(Ok(_))))
        .map(|(_, f)| {
            let theta = f.svi.total_variance(0.0);
            (theta, vec![(0.0, theta, 1.0)])
        })
        .collect();
    let ssvi = if ssvi_slices.len() >= 2 {
        fit_ssvi(&ssvi_slices)
    } else {
        None
    };
    Surface {
        slices,
        ssvi,
        quality,
    }
}

// ── The measures ────────────────────────────────────────────────────────────

fn steps(definition: &Definition) -> usize {
    definition
        .parameter("steps")
        .and_then(|s| s.parse().ok())
        .unwrap_or(STEPS)
}

struct Out<'a> {
    key: &'a MeasureKey,
    scale: u32,
    part: &'a mut Part,
}

impl Out<'_> {
    #[allow(clippy::too_many_arguments)]
    fn figure(
        &mut self,
        breakdown: Vec<(&str, String)>,
        series: &SeriesKey,
        value: f64,
        currency: &str,
        source_ns: i64,
    ) {
        match exact(value, self.scale) {
            Some(v) => self.part.figures.push(Figure {
                measure: self.key.clone(),
                breakdown: breakdown
                    .into_iter()
                    .map(|(b, v)| (b.to_string(), v))
                    .collect(),
                value: Some(v),
                currency_instrument_id: currency.to_string(),
                computed_as: MODEL,
                series_key: series.clone(),
                oldest_input_source_ns: source_ns,
                ..Default::default()
            }),
            None => self.not(series, "", FIT_FAILED, "not a finite number".into()),
        }
    }

    fn exact_figure(
        &mut self,
        breakdown: Vec<(&str, String)>,
        series: &SeriesKey,
        value: Dec,
        currency: &str,
    ) {
        self.part.figures.push(Figure {
            measure: self.key.clone(),
            breakdown: breakdown
                .into_iter()
                .map(|(b, v)| (b.to_string(), v))
                .collect(),
            value: Some(value.round_half_even(self.scale)),
            currency_instrument_id: currency.to_string(),
            series_key: series.clone(),
            ..Default::default()
        });
    }

    fn not(&mut self, series: &SeriesKey, instrument: &str, reason: &'static str, detail: String) {
        self.part.not_evaluated.push(Unevaluated {
            account_id: String::new(),
            instrument_id: instrument.to_string(),
            measure: self.key.clone(),
            reason,
            detail,
            series_key: series.clone(),
        });
    }
}

fn read_chain(part: &mut Part, chain: &Chain, model: bool) {
    part.reads.datasets.extend(chain.datasets.iter().cloned());
    for d in [
        &chain.spot_dataset,
        &chain.rate_dataset,
        &chain.dividends_dataset,
    ] {
        if !d.is_empty() {
            part.reads.datasets.insert(d.clone());
        }
    }
    if model {
        for s in &chain.series {
            if !s.model.is_empty() {
                part.reads.models.insert((s.model.clone(), 1));
            }
        }
    }
}

const GREEKS: [&str; 7] = ["delta", "gamma", "theta", "vega", "rho", "vanna", "charm"];

/// The greeks in their declared units (W12.1): delta and gamma per 1 of the
/// underlying, vega per vol point, theta per calendar day, rho per 1%, vanna
/// per vol point and charm per calendar day.
pub fn in_units(g: &Greeks) -> [f64; 7] {
    [
        g.delta,
        g.gamma,
        g.theta / 365.0,
        g.vega / 100.0,
        g.rho / 100.0,
        g.vanna / 100.0,
        g.charm / 365.0,
    ]
}

fn source(chain: &Chain, s: &Series) -> i64 {
    match (chain.spot_source_ns, s.quote_source_ns) {
        (0, q) => q,
        (p, 0) => p,
        (p, q) => p.min(q),
    }
}

/// One chain measure over every chain of the job: its part.
pub fn compute(case: &Case, key: &MeasureKey, definition: &Definition) -> Result<Part> {
    let mut part = Part::default();
    let unknown: Vec<&String> = key
        .breakdowns
        .iter()
        .filter(|b| !definition.breakdowns.contains(&b.as_str()))
        .collect();
    if !unknown.is_empty() {
        for name in unknown {
            part.not_evaluated.push(Unevaluated {
                account_id: String::new(),
                instrument_id: String::new(),
                measure: key.clone(),
                reason: SCHEME_NOT_HELD,
                detail: name.clone(),
                series_key: Vec::new(),
            });
        }
        return Ok(part);
    }
    let steps = steps(definition);
    let as_of = case.lake_as_of_ns;
    for chain in &case.chains {
        let model = !matches!(definition.definition, "max_pain" | "open_interest_change");
        read_chain(
            &mut part,
            chain,
            model && definition.definition != "volatility_index",
        );
        let mut out = Out {
            key,
            scale: definition.scale,
            part: &mut part,
        };
        match definition.definition {
            "max_pain" => max_pain(chain, &mut out),
            "open_interest_change" => open_interest_change(chain, &mut out),
            "volatility_index" => volatility_index(chain, as_of, &mut out),
            _ => {
                let solution = solve(chain, as_of, steps);
                match definition.definition {
                    "implied_volatility" => implied_volatility(chain, &solution, &mut out),
                    "implied_forward" => implied_forward(chain, &solution, as_of, &mut out),
                    "greeks" => greeks(chain, &solution, steps, &mut out),
                    "surface_fit" | "surface_residual" | "term_and_skew" => {
                        let fitted = surface(chain, &solution);
                        out.part.reads.models.insert((SVI.to_string(), 1));
                        match definition.definition {
                            "surface_fit" => {
                                out.part.reads.models.insert((SSVI.to_string(), 1));
                                surface_fit(chain, &fitted, &mut out)
                            }
                            "surface_residual" => {
                                surface_residual(chain, &solution, &fitted, &mut out)
                            }
                            _ => term_and_skew(chain, &solution, &fitted, as_of, &mut out),
                        }
                    }
                    "positioning_exposure" => {
                        let short_all = definition.parameter("positioning_basis")
                            == Some("convention_short_all");
                        positioning_exposure(chain, &solution, short_all, steps, &mut out)
                    }
                    "positioning_levels" => positioning_levels(chain, &solution, steps, &mut out),
                    other => return Err(CaseError(format!("no computation for {other}"))),
                }
            }
        }
    }
    Ok(part)
}

fn implied_volatility(chain: &Chain, solution: &ChainSolution, out: &mut Out<'_>) {
    for (s, solved) in chain.series.iter().zip(&solution.series) {
        match solved {
            Err((reason, detail)) => out.not(&s.key, &s.instrument_id, reason, detail.clone()),
            Ok(solved) => {
                for (side, iv) in solved.iv.iter().enumerate() {
                    match iv {
                        Ok(v) => out.figure(
                            vec![
                                ("series", s.name.clone()),
                                ("quote_side", SIDES[side].into()),
                            ],
                            &s.key,
                            *v,
                            "",
                            source(chain, s),
                        ),
                        Err((reason, detail)) => {
                            out.not(&s.key, &s.instrument_id, reason, detail.clone())
                        }
                    }
                }
            }
        }
    }
}

fn implied_forward(chain: &Chain, solution: &ChainSolution, as_of: i64, out: &mut Out<'_>) {
    for (expiry, found) in &solution.forwards {
        match found {
            Ok((forward, carry)) => {
                let src = chain
                    .series
                    .iter()
                    .filter(|s| &s.expiry == expiry)
                    .map(|s| source(chain, s))
                    .filter(|n| *n > 0)
                    .min()
                    .unwrap_or(chain.spot_source_ns);
                let _ = as_of;
                out.figure(
                    vec![("expiry", expiry.clone()), (COMPONENT, "forward".into())],
                    &Vec::new(),
                    *forward,
                    &chain.currency_instrument_id,
                    src,
                );
                out.figure(
                    vec![("expiry", expiry.clone()), (COMPONENT, "borrow".into())],
                    &Vec::new(),
                    *carry,
                    "",
                    src,
                );
            }
            Err((reason, detail)) => {
                out.not(&Vec::new(), "", reason, format!("{expiry}: {detail}"))
            }
        }
    }
}

fn greeks(chain: &Chain, solution: &ChainSolution, steps: usize, out: &mut Out<'_>) {
    for (s, solved) in chain.series.iter().zip(&solution.series) {
        let solved = match solved {
            Err((reason, detail)) => {
                out.not(&s.key, &s.instrument_id, reason, detail.clone());
                continue;
            }
            Ok(solved) => solved,
        };
        let vol = match &solved.iv[1] {
            Ok(v) => *v,
            Err((reason, detail)) => {
                out.not(&s.key, &s.instrument_id, reason, detail.clone());
                continue;
            }
        };
        let g = greeks_at(
            &s.model,
            s.right,
            solved.spot,
            s.strike,
            solved.t,
            solved.rate,
            solved.carry,
            vol,
            &solved.dividends,
            steps,
        );
        for (name, value) in GREEKS.iter().zip(in_units(&g)) {
            out.figure(
                vec![("series", s.name.clone()), ("greek", name.to_string())],
                &s.key,
                value,
                "",
                source(chain, s),
            );
        }
    }
}

fn surface_fit(chain: &Chain, surface: &Surface, out: &mut Out<'_>) {
    let src = chain.spot_source_ns;
    for (expiry, slice) in &surface.slices {
        match slice {
            Err((reason, detail)) => out.not(&Vec::new(), "", reason, detail.clone()),
            Ok(f) => {
                let p = &f.svi;
                for (name, value) in [
                    ("a", p.a),
                    ("b", p.b),
                    ("rho", p.rho),
                    ("m", p.m),
                    ("sigma", p.sigma),
                ] {
                    out.figure(
                        vec![("expiry", expiry.clone()), ("parameter", name.into())],
                        &Vec::new(),
                        value,
                        "",
                        src,
                    );
                }
                if let Some((rmse, inside, of)) = surface.quality.get(expiry) {
                    out.figure(
                        vec![("expiry", expiry.clone()), ("parameter", "rmse".into())],
                        &Vec::new(),
                        *rmse,
                        "",
                        src,
                    );
                    out.part.figures.push(Figure {
                        measure: out.key.clone(),
                        breakdown: vec![
                            ("expiry".into(), expiry.clone()),
                            ("parameter".into(), "inside_bid_ask".into()),
                        ],
                        numerator: Some(Dec::from_int(*inside as i128)),
                        denominator: Some(Dec::from_int(*of as i128)),
                        computed_as: MODEL,
                        ..Default::default()
                    });
                }
                for check in ["butterfly", "calendar"] {
                    out.figure(
                        vec![("expiry", expiry.clone()), ("parameter", check.into())],
                        &Vec::new(),
                        1.0,
                        "",
                        src,
                    );
                }
            }
        }
    }
    match surface.ssvi {
        Some(ssvi) => {
            for (name, value) in [
                ("ssvi_rho", ssvi.rho),
                ("ssvi_eta", ssvi.eta),
                ("ssvi_gamma", ssvi.gamma),
            ] {
                out.figure(
                    vec![("expiry", String::new()), ("parameter", name.into())],
                    &Vec::new(),
                    value,
                    "",
                    src,
                );
            }
        }
        None if surface.slices.values().filter(|s| s.is_ok()).count() >= 2 => out.not(
            &Vec::new(),
            "",
            FIT_FAILED,
            "SSVI across the expiries did not converge".into(),
        ),
        None => {}
    }
}

fn surface_residual(chain: &Chain, solution: &ChainSolution, surface: &Surface, out: &mut Out<'_>) {
    for (s, solved) in chain.series.iter().zip(&solution.series) {
        let Ok(solved) = solved else { continue };
        let Some(Ok(fitted)) = surface.slices.get(&s.expiry) else {
            continue;
        };
        let (Ok(bid), Ok(m), Ok(ask)) = (&solved.iv[0], &solved.iv[1], &solved.iv[2]) else {
            continue;
        };
        let fit = fitted.vol(s.strike);
        out.figure(
            vec![("series", s.name.clone()), (COMPONENT, "residual".into())],
            &s.key,
            (m - fit) * 100.0,
            "",
            source(chain, s),
        );
        // Rich or cheap only outside the IV bid-ask (W12.1).
        let signal = if *bid > fit {
            1.0
        } else if *ask < fit {
            -1.0
        } else {
            0.0
        };
        out.figure(
            vec![
                ("series", s.name.clone()),
                (COMPONENT, "rich_or_cheap".into()),
            ],
            &s.key,
            signal,
            "",
            source(chain, s),
        );
    }
}

/// The log-moneyness of a slice's strike at a forward delta, by fixed
/// point: d1 = Phi^-1(N(d1)) and k = w/2 - d1 sqrt(w).
fn delta_k(f: &Fitted, d1: f64) -> f64 {
    let mut k = 0.0;
    for _ in 0..50 {
        let w = f.svi.total_variance(k).max(1e-12);
        k = w / 2.0 - d1 * w.sqrt();
    }
    k
}

fn term_and_skew(
    chain: &Chain,
    solution: &ChainSolution,
    surface: &Surface,
    as_of: i64,
    out: &mut Out<'_>,
) {
    let src = chain.spot_source_ns;
    let mut atm: Vec<(f64, f64)> = Vec::new();
    for (expiry, slice) in &surface.slices {
        let f = match slice {
            Ok(f) => *f,
            Err((reason, detail)) => {
                out.not(&Vec::new(), "", reason, detail.clone());
                continue;
            }
        };
        let atm_w = f.svi.total_variance(0.0).max(1e-12);
        let atm_iv = (atm_w / f.t).sqrt();
        atm.push((f.t, atm_w));
        let up = (f.svi.total_variance(delta_k(&f, inverse_cdf(0.25))) / f.t).sqrt();
        let down = (f.svi.total_variance(delta_k(&f, inverse_cdf(0.75))) / f.t).sqrt();
        let e = |name: &str| vec![("expiry", expiry.clone()), (COMPONENT, name.to_string())];
        out.figure(e("atm_iv"), &Vec::new(), atm_iv, "", src);
        out.figure(e("risk_reversal_25d"), &Vec::new(), up - down, "", src);
        out.figure(
            e("butterfly_25d"),
            &Vec::new(),
            (up + down) / 2.0 - atm_iv,
            "",
            src,
        );
        if let Some(spot) = chain.spot {
            out.figure(
                e("expected_move_iv"),
                &Vec::new(),
                spot * atm_iv * f.t.sqrt(),
                &chain.currency_instrument_id,
                src,
            );
        }
        // The straddle's move: the call's and the put's mids at the strike
        // nearest the forward.
        let at = chain
            .series
            .iter()
            .filter(|s| &s.expiry == expiry)
            .min_by(|a, b| {
                (a.strike - f.forward)
                    .abs()
                    .total_cmp(&(b.strike - f.forward).abs())
            })
            .map(|s| s.strike);
        if let Some(k) = at {
            let leg = |right| {
                chain
                    .series
                    .iter()
                    .find(|s| &s.expiry == expiry && s.strike == k && s.right == right)
                    .and_then(mid)
            };
            if let (Some(c), Some(p)) = (leg(Right::Call), leg(Right::Put)) {
                out.figure(
                    e("expected_move_straddle"),
                    &Vec::new(),
                    c + p,
                    &chain.currency_instrument_id,
                    src,
                );
            }
        }
    }
    let _ = (solution, as_of);
    atm.sort_by(|a, b| a.0.total_cmp(&b.0));
    for days in [30.0, 60.0, 90.0] {
        let t = days / 365.0;
        let name = format!("atm_iv_{}d", days as i64);
        let found = atm
            .windows(2)
            .find(|p| p[0].0 <= t && t <= p[1].0)
            .map(|p| {
                let w = p[0].1 + (p[1].1 - p[0].1) * (t - p[0].0) / (p[1].0 - p[0].0);
                (w / t).sqrt()
            });
        match found {
            Some(v) => out.figure(
                vec![("expiry", String::new()), (COMPONENT, name)],
                &Vec::new(),
                v,
                "",
                src,
            ),
            None => out.not(
                &Vec::new(),
                "",
                PRICE_MISSING,
                format!("{name}: no fitted expiries either side of {days} days"),
            ),
        }
    }
}

/// Cboe's published method on a listed strip (W12.1): no option model.
fn volatility_index(chain: &Chain, as_of: i64, out: &mut Out<'_>) {
    if chain.curve.is_empty() {
        out.not(&Vec::new(), "", PRICE_MISSING, chain.rate_missing.clone());
        return;
    }
    let mut terms: Vec<(f64, f64)> = Vec::new();
    let mut by_expiry: BTreeMap<&str, Vec<&Series>> = BTreeMap::new();
    for s in &chain.series {
        if s.settles_at_ns > 0 {
            by_expiry.entry(&s.expiry).or_default().push(s);
        }
    }
    let mut src = chain.spot_source_ns;
    for members in by_expiry.values() {
        let t = years(as_of, members[0].settles_at_ns);
        if t <= 0.0 {
            continue;
        }
        let mut strikes: Vec<f64> = members.iter().map(|s| s.strike).collect();
        strikes.sort_by(f64::total_cmp);
        strikes.dedup();
        let at = |k: f64, right: Right| {
            members
                .iter()
                .find(|s| s.strike == k && s.right == right)
                .copied()
        };
        let pair = strikes
            .iter()
            .filter_map(|&k| {
                match (
                    at(k, Right::Call).and_then(mid),
                    at(k, Right::Put).and_then(mid),
                ) {
                    (Some(c), Some(p)) => Some((k, c, p)),
                    _ => None,
                }
            })
            .min_by(|a, b| (a.1 - a.2).abs().total_cmp(&(b.1 - b.2).abs()));
        let Some((k, c, p)) = pair else { continue };
        let Ok(r) = chain.rate_at(t) else { continue };
        let forward = k + (r * t).exp() * (c - p);
        let Some(&k0) = strikes.iter().rev().find(|&&s| s <= forward) else {
            continue;
        };
        // The out-of-the-money quotes, puts below K0 and calls above, each
        // side stopped at two zero bids in a row; at K0 the two averaged.
        let mut q: Vec<(f64, f64)> = Vec::new();
        for (right, list) in [
            (
                Right::Put,
                strikes
                    .iter()
                    .rev()
                    .filter(|&&s| s < k0)
                    .copied()
                    .collect::<Vec<_>>(),
            ),
            (
                Right::Call,
                strikes
                    .iter()
                    .filter(|&&s| s > k0)
                    .copied()
                    .collect::<Vec<_>>(),
            ),
        ] {
            let mut zeros = 0;
            for s in list {
                let Some(o) = at(s, right) else { continue };
                if o.bid.unwrap_or(0.0) <= 0.0 {
                    zeros += 1;
                    if zeros >= 2 {
                        break;
                    }
                    continue;
                }
                zeros = 0;
                if let Some(m) = mid(o) {
                    q.push((s, m));
                    src = src.min(o.quote_source_ns.max(1));
                }
            }
        }
        if let (Some(c0), Some(p0)) = (
            at(k0, Right::Call).and_then(mid),
            at(k0, Right::Put).and_then(mid),
        ) {
            q.push((k0, (c0 + p0) / 2.0));
        }
        q.sort_by(|a, b| a.0.total_cmp(&b.0));
        if q.len() < 3 {
            continue;
        }
        let mut sum = 0.0;
        for i in 0..q.len() {
            let dk = if i == 0 {
                q[1].0 - q[0].0
            } else if i == q.len() - 1 {
                q[i].0 - q[i - 1].0
            } else {
                (q[i + 1].0 - q[i - 1].0) / 2.0
            };
            sum += dk / (q[i].0 * q[i].0) * (r * t).exp() * q[i].1;
        }
        let variance = 2.0 / t * sum - (forward / k0 - 1.0).powi(2) / t;
        terms.push((t, variance));
    }
    let target = 30.0 / 365.0;
    terms.sort_by(|a, b| a.0.total_cmp(&b.0));
    let near = terms.iter().rev().find(|(t, _)| *t <= target);
    let next = terms.iter().find(|(t, _)| *t >= target);
    match (near, next) {
        (Some(&(t1, v1)), Some(&(t2, v2))) => {
            let index = if (t2 - t1).abs() < 1e-12 {
                v1
            } else {
                (t1 * v1 * (t2 - target) / (t2 - t1) + t2 * v2 * (target - t1) / (t2 - t1)) / target
            };
            out.figure(
                Vec::new(),
                &Vec::new(),
                100.0 * index.max(0.0).sqrt(),
                "",
                src,
            );
        }
        _ => out.not(
            &Vec::new(),
            "",
            PRICE_MISSING,
            format!(
                "{}: no listed terms either side of 30 days",
                chain.underlying
            ),
        ),
    }
}

fn oi(s: &Series) -> Option<f64> {
    s.open_interest.as_ref().map(Dec::to_f64)
}

/// GEX, DEX, VEX and CHEX per series (W12.1; G14): each greek times the
/// open interest and the multiplier, GEX per 1% move (gamma S^2 / 100),
/// DEX in the underlying's currency (delta S); the basis signs each side,
/// long calls and short puts by default, every series short in version 2.
fn positioning_exposure(
    chain: &Chain,
    solution: &ChainSolution,
    short_all: bool,
    steps: usize,
    out: &mut Out<'_>,
) {
    for (s, solved) in chain.series.iter().zip(&solution.series) {
        let Some(open) = oi(s) else {
            out.not(
                &s.key,
                &s.instrument_id,
                PRICE_MISSING,
                "open interest".into(),
            );
            continue;
        };
        let solved = match solved {
            Ok(v) => v,
            Err((reason, detail)) => {
                out.not(&s.key, &s.instrument_id, reason, detail.clone());
                continue;
            }
        };
        let Ok(vol) = solved.iv[1].clone() else {
            out.not(&s.key, &s.instrument_id, PRICE_MISSING, "mid".into());
            continue;
        };
        let g = greeks_at(
            &s.model,
            s.right,
            solved.spot,
            s.strike,
            solved.t,
            solved.rate,
            solved.carry,
            vol,
            &solved.dividends,
            steps,
        );
        let sign = if short_all || s.right == Right::Put {
            -1.0
        } else {
            1.0
        };
        let size = sign * open * s.multiplier;
        let spot = solved.spot;
        for (name, value, currency) in [
            (
                "gex",
                size * g.gamma * spot * spot / 100.0,
                chain.currency_instrument_id.as_str(),
            ),
            (
                "dex",
                size * g.delta * spot,
                chain.currency_instrument_id.as_str(),
            ),
            (
                "vex",
                size * g.vega / 100.0,
                chain.currency_instrument_id.as_str(),
            ),
            (
                "chex",
                size * g.charm / 365.0 * spot,
                chain.currency_instrument_id.as_str(),
            ),
        ] {
            out.figure(
                vec![
                    ("expiry", s.expiry.clone()),
                    ("greek", name.into()),
                    ("series", s.name.clone()),
                ],
                &s.key,
                value,
                currency,
                source(chain, s),
            );
        }
    }
}

/// The zero-gamma crossings on a spot grid of plus or minus 15% in 0.5%
/// steps, the call, put and total gamma concentration strikes, gamma to
/// liquidity and the 0DTE share (W12.1), on the default basis.
fn positioning_levels(chain: &Chain, solution: &ChainSolution, steps: usize, out: &mut Out<'_>) {
    let Some(spot) = chain.spot else {
        out.not(&Vec::new(), "", PRICE_MISSING, chain.spot_missing.clone());
        return;
    };
    let held: Vec<(&Series, &Solved, f64, f64)> = chain
        .series
        .iter()
        .zip(&solution.series)
        .filter_map(|(s, solved)| {
            let solved = solved.as_ref().ok()?;
            let vol = *solved.iv[1].as_ref().ok()?;
            Some((s, solved, vol, oi(s)?))
        })
        .collect();
    if held.is_empty() {
        out.not(
            &Vec::new(),
            "",
            PRICE_MISSING,
            "no series with a mid IV and open interest".into(),
        );
        return;
    }
    let gamma_at = |s: &Series, solved: &Solved, vol: f64, at: f64| {
        let g = black_scholes(
            s.right,
            at,
            s.strike,
            solved.t,
            solved.rate,
            solved.carry,
            vol,
        );
        g.gamma * at * at / 100.0
    };
    let _ = steps;
    let total_at = |at: f64| -> f64 {
        held.iter()
            .map(|(s, solved, vol, open)| {
                let sign = if s.right == Right::Put { -1.0 } else { 1.0 };
                sign * open * s.multiplier * gamma_at(s, solved, *vol, at)
            })
            .sum()
    };
    let grid: Vec<f64> = (-30..=30)
        .map(|i| spot * (1.0 + i as f64 * 0.005))
        .collect();
    let values: Vec<f64> = grid.iter().map(|&at| total_at(at)).collect();
    let src = chain.spot_source_ns;
    let currency = chain.currency_instrument_id.as_str();
    let mut crossings = 0;
    for i in 1..grid.len() {
        if values[i - 1] == 0.0 || values[i - 1].signum() != values[i].signum() {
            let (x0, x1, y0, y1) = (grid[i - 1], grid[i], values[i - 1], values[i]);
            let x = if (y1 - y0).abs() < 1e-12 {
                x0
            } else {
                x0 - y0 * (x1 - x0) / (y1 - y0)
            };
            crossings += 1;
            out.figure(
                vec![(COMPONENT, format!("zero_gamma_{crossings}"))],
                &Vec::new(),
                x,
                currency,
                src,
            );
        }
    }
    if crossings == 0 {
        out.not(
            &Vec::new(),
            "",
            PRICE_MISSING,
            "zero_gamma: no crossing within 15%".into(),
        );
    }
    let mut by_strike: BTreeMap<i64, (f64, f64, f64)> = BTreeMap::new();
    let mut total = 0.0;
    let mut zero_dte = 0.0;
    for (s, solved, vol, open) in &held {
        let g = open * s.multiplier * gamma_at(s, solved, *vol, spot);
        let entry = by_strike
            .entry((s.strike * 1e6).round() as i64)
            .or_default();
        if s.right == Right::Call {
            entry.0 += g;
            entry.2 += g;
        } else {
            entry.1 += g;
            entry.2 -= g;
        }
        total += g;
        if s.expiry == chain.today {
            zero_dte += g;
        }
    }
    for (name, pick) in [
        ("call_gamma_strike", 0usize),
        ("put_gamma_strike", 1),
        ("total_gamma_strike", 2),
    ] {
        if let Some((k, _)) = by_strike.iter().max_by(|a, b| {
            let v = |e: &(f64, f64, f64)| [e.0, e.1, e.2.abs()][pick];
            v(a.1).total_cmp(&v(b.1))
        }) {
            out.figure(
                vec![(COMPONENT, name.into())],
                &Vec::new(),
                *k as f64 / 1e6,
                currency,
                src,
            );
        }
    }
    match chain.underlying_volume {
        Some(volume) if volume > 0.0 => out.figure(
            vec![(COMPONENT, "gamma_to_liquidity".into())],
            &Vec::new(),
            total_at(spot) / (volume * spot),
            "",
            src,
        ),
        _ => out.not(
            &Vec::new(),
            "",
            PRICE_MISSING,
            "gamma_to_liquidity: the underlying's volume".into(),
        ),
    }
    if total > 0.0 {
        out.figure(
            vec![(COMPONENT, "zero_dte_share".into())],
            &Vec::new(),
            zero_dte / total,
            "",
            src,
        );
    }
}

/// Per expiry, the strike at which the holders' total payout is least
/// (W12.1), exact over the lake's open interest.
fn max_pain(chain: &Chain, out: &mut Out<'_>) {
    let mut by_expiry: BTreeMap<&str, Vec<&Series>> = BTreeMap::new();
    for s in &chain.series {
        by_expiry.entry(&s.expiry).or_default().push(s);
    }
    for (expiry, members) in by_expiry {
        let held: Vec<&&Series> = members
            .iter()
            .filter(|s| s.open_interest.is_some())
            .collect();
        if held.is_empty() {
            out.not(
                &Vec::new(),
                "",
                PRICE_MISSING,
                format!("{expiry}: open interest"),
            );
            continue;
        }
        let mut best: Option<(Dec, Dec)> = None;
        let mut strikes: Vec<&Dec> = members.iter().map(|s| &s.strike_exact).collect();
        strikes.sort_by(|a, b| a.to_f64().total_cmp(&b.to_f64()));
        strikes.dedup_by(|a, b| a.sub(b).is_zero());
        for at in strikes {
            let mut pay = Dec::zero();
            for s in &held {
                let gap = match s.right {
                    Right::Call => at.sub(&s.strike_exact),
                    Right::Put => s.strike_exact.sub(at),
                };
                if gap.signum() > 0 {
                    let oi = s.open_interest.clone().unwrap_or_else(Dec::zero);
                    pay = pay.add(&gap.mul(&oi).mul(&s.multiplier_exact));
                }
            }
            if best.as_ref().is_none_or(|(_, b)| pay.sub(b).signum() < 0) {
                best = Some((at.clone(), pay));
            }
        }
        if let Some((strike, _)) = best {
            out.exact_figure(
                vec![("expiry", expiry.to_string())],
                &Vec::new(),
                strike,
                &chain.currency_instrument_id,
            );
        }
    }
}

/// Per series, open interest on the latest date less the date before it,
/// exact.
fn open_interest_change(chain: &Chain, out: &mut Out<'_>) {
    for s in &chain.series {
        match (&s.open_interest, &s.open_interest_prior) {
            (Some(now), Some(before)) => out.exact_figure(
                vec![("series", s.name.clone())],
                &s.key,
                now.sub(before),
                "",
            ),
            _ => out.not(
                &s.key,
                &s.instrument_id,
                PRICE_MISSING,
                "open interest on two trading dates".into(),
            ),
        }
    }
}

// ── The scenario grid (W12.1, W12.8) ─────────────────────────────────────────

/// One figure per position per `scenario_key` (W12.1): each option held
/// revalued on the fitted surface -- sticky strike, the slice's volatility at
/// its strike, or its own mid IV where the slice is not fitted -- with the
/// point's shocks applied, intrinsic at an expiration column; each
/// underlying held at its shocked price. A position in neither is not
/// evaluated.
pub fn scenario_grid(
    case: &Case,
    key: &MeasureKey,
    definition: &Definition,
    account: &str,
) -> Result<Part> {
    let mut part = Part::default();
    let Some(set) = &case.scenario else {
        return Err(CaseError(
            "scenario_grid: the inputs carry no scenario".into(),
        ));
    };
    let steps = steps(definition);
    let as_of = case.lake_as_of_ns;
    let mut solved: Vec<(ChainSolution, Surface)> = Vec::new();
    for chain in &case.chains {
        let solution = solve(chain, as_of, steps);
        let fitted = surface(chain, &solution);
        read_chain(&mut part, chain, true);
        solved.push((solution, fitted));
    }
    let points = set.points();
    let sticky_moneyness = definition.parameter("surface_dynamics") == Some("sticky_moneyness");
    for position in case.positions.iter().filter(|p| p.account_id == account) {
        let quantity = position.quantity.to_f64();
        let found = case.chains.iter().enumerate().find_map(|(c, chain)| {
            if chain.underlying == position.instrument_id {
                return Some((c, None));
            }
            chain
                .series
                .iter()
                .position(|s| s.instrument_id == position.instrument_id)
                .map(|i| (c, Some(i)))
        });
        if found.is_none() && case.fiat(&position.instrument_id) {
            continue;
        }
        let Some((c, series)) = found else {
            part.not_evaluated.push(Unevaluated {
                account_id: account.to_string(),
                instrument_id: position.instrument_id.clone(),
                measure: key.clone(),
                reason: PRICE_MISSING,
                detail: "not an option or an underlying of a chain the job reads".into(),
                series_key: Vec::new(),
            });
            continue;
        };
        let chain = &case.chains[c];
        let (solution, fitted) = &solved[c];
        let Some(spot) = chain.spot else {
            part.not_evaluated.push(Unevaluated {
                account_id: account.to_string(),
                instrument_id: position.instrument_id.clone(),
                measure: key.clone(),
                reason: PRICE_MISSING,
                detail: chain.spot_missing.clone(),
                series_key: Vec::new(),
            });
            continue;
        };
        part.reads
            .instruments
            .insert(position.instrument_id.clone());
        let currency = case
            .instruments
            .get(&position.instrument_id)
            .map(|i| i.currency_instrument_id.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| chain.currency_instrument_id.clone());
        // The option's base: its solved inputs and its volatility.
        let base = match series {
            None => None,
            Some(i) => {
                let s = &chain.series[i];
                match &solution.series[i] {
                    Err((reason, detail)) => {
                        part.not_evaluated.push(Unevaluated {
                            account_id: account.to_string(),
                            instrument_id: position.instrument_id.clone(),
                            measure: key.clone(),
                            reason,
                            detail: detail.clone(),
                            series_key: s.key.clone(),
                        });
                        continue;
                    }
                    Ok(v) => {
                        let vol = match fitted.slices.get(&s.expiry) {
                            Some(Ok(f)) => Ok(f.vol(s.strike)),
                            _ => v.iv[1].clone(),
                        };
                        match vol {
                            Ok(vol) => Some((
                                s,
                                v,
                                vol,
                                fitted
                                    .slices
                                    .get(&s.expiry)
                                    .and_then(|f| f.as_ref().ok().copied()),
                            )),
                            Err((reason, detail)) => {
                                part.not_evaluated.push(Unevaluated {
                                    account_id: account.to_string(),
                                    instrument_id: position.instrument_id.clone(),
                                    measure: key.clone(),
                                    reason,
                                    detail,
                                    series_key: s.key.clone(),
                                });
                                continue;
                            }
                        }
                    }
                }
            }
        };
        for (positions, shocks) in &points {
            let applied = Applied(shocks);
            let moved = applied.spot(&chain.underlying, spot);
            let value = match &base {
                None => quantity * moved,
                Some((s, v, vol, slice)) => {
                    let (minutes, to_settlement) = applied.time();
                    let t = if to_settlement {
                        0.0
                    } else {
                        v.t - minutes / MINUTES_A_YEAR
                    };
                    let days = v.t * 365.0;
                    let rate = applied.rate(&chain.rate_subject, days, v.rate);
                    let base_vol = match (sticky_moneyness, slice) {
                        (true, Some(f)) => f.vol(s.strike * spot / moved),
                        _ => *vol,
                    };
                    let shocked = applied.vol(&chain.underlying, days, base_vol);
                    let per = if t <= 0.0 {
                        let w = if s.right == Right::Call { 1.0 } else { -1.0 };
                        (w * (moved - s.strike)).max(0.0)
                    } else {
                        let dividends: Vec<Dividend> = v
                            .dividends
                            .iter()
                            .map(|d| Dividend {
                                t: d.t - (v.t - t),
                                amount: d.amount,
                            })
                            .filter(|d| d.t > 0.0)
                            .collect();
                        price(
                            &s.model, s.right, moved, s.strike, t, rate, v.carry, shocked,
                            &dividends, steps, s.american,
                        )
                    };
                    quantity * s.multiplier * per
                }
            };
            let Some(v) = exact(value, definition.scale) else {
                continue;
            };
            part.figures.push(Figure {
                measure: key.clone(),
                account_id: account.to_string(),
                breakdown: key
                    .breakdowns
                    .iter()
                    .map(|b| {
                        (
                            b.clone(),
                            match b.as_str() {
                                "account" => account.to_string(),
                                _ => position.instrument_id.clone(),
                            },
                        )
                    })
                    .collect(),
                scenario: Scenario::point(&set.row_key, set.version, positions.clone()),
                value: Some(v),
                currency_instrument_id: currency.clone(),
                computed_as: MODEL,
                series_key: base.as_ref().map(|b| b.0.key.clone()).unwrap_or_default(),
                oldest_input_source_ns: chain.spot_source_ns,
                ..Default::default()
            });
        }
    }
    Ok(part)
}
