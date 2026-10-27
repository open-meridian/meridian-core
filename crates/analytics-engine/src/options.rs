//! v28's option models and calibrations (contract v28, track B; ruling
//! 565; the engine's first release, section 3, "v28: derivatives"; the
//! options sample's section 2.2).
//!
//! f64 inside a model only (the one-API spec's Q8): every figure a job
//! answers is rounded once into the engine's exact decimal where it leaves
//! a model, and nothing here is money. QuantLib is the oracle, in the SDK's
//! conformance tooling, never here and never in the image; these tests hold
//! the models to closed forms, to the textbook cases and to the properties
//! the oracle cannot give (put-call parity, monotonicity, no butterfly or
//! calendar arbitrage on a fitted surface).
//!
//! - [`black_scholes`]: a European option on a share or an index with a
//!   continuous yield (cash-settled index options), price and greeks in
//!   closed form;
//! - [`black_76`]: a European option on a future;
//! - [`american_binomial`]: an American option on a share paying discrete
//!   cash dividends, on a Cox-Ross-Rubinstein tree of the share less the
//!   present value of the dividends to expiry (the escrowed-dividend
//!   model), early exercise tested at every node against the share with
//!   those dividends added back; its steps are the definition's parameter;
//!   delta, gamma and theta from the tree's first nodes, vega, rho, vanna
//!   and charm by bump and reprice on the same tree;
//! - [`implied_vol`]: the volatility a model prices a quote at, by a
//!   bracketed Brent root (bid, mid and ask each their own inversion);
//! - [`SviRaw`] and [`fit_svi_above`]: Gatheral's raw SVI per expiry slice,
//!   by Levenberg-Marquardt from a spread of starts on total variance
//!   relative to the slice's at-the-money level, its least variance held
//!   non-negative by its parametrisation, the butterfly condition, Lee's
//!   wing bound and (against the expiry before it) the calendar condition
//!   as penalties (ruling 791);
//! - [`Ssvi`] and [`fit_ssvi`]: the power-law SSVI across slices under
//!   Gatheral and Jacquier's conditions (Quantitative Finance 14(1), 2014),
//!   free of static arbitrage by construction, fitted to every slice's
//!   quotes (ruling 791: never the at-the-money points alone, which leave
//!   rho and gamma where they started);
//! - [`butterfly_free`] and [`calendar_free`]: the checks every fitted
//!   surface is held to.

/// A call or a put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Right {
    Call,
    Put,
}

/// A price and the greeks a model declares (contract v28; OPT 2.2): delta,
/// gamma, theta per year, vega and rho per unit (1.00 = 100 vol points or
/// 100% of rate), vanna (d delta / d vol) and charm (d delta / d time, per
/// year, as time passes).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Greeks {
    pub price: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub rho: f64,
    pub vanna: f64,
    pub charm: f64,
}

/// The standard normal density.
pub fn pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// The standard normal distribution, to double precision: Hart's algorithm
/// 5666 as Graeme West gives it ("Better approximations to cumulative normal
/// functions", Wilmott, 2005).
pub fn cdf(x: f64) -> f64 {
    let z = x.abs();
    let c = if z > 37.0 {
        0.0
    } else {
        let e = (-z * z / 2.0).exp();
        if z < 7.071_067_811_865_475 {
            let n = (((((3.526_249_659_989_11e-2 * z + 0.700_383_064_443_688) * z
                + 6.373_962_203_531_65)
                * z
                + 33.912_866_078_383)
                * z
                + 112.079_291_497_871)
                * z
                + 221.213_596_169_931)
                * z
                + 220.206_867_912_376;
            let d = ((((((8.838_834_764_831_84e-2 * z + 1.755_667_163_182_64) * z
                + 16.064_177_579_207)
                * z
                + 86.780_732_202_946_1)
                * z
                + 296.564_248_779_674)
                * z
                + 637.333_633_378_831)
                * z
                + 793.826_512_519_948)
                * z
                + 440.413_735_824_752;
            e * n / d
        } else {
            let f = z + 1.0 / (z + 2.0 / (z + 3.0 / (z + 4.0 / (z + 13.0 / 20.0))));
            e / (f * 2.506_628_274_631)
        }
    };
    if x <= 0.0 {
        c
    } else {
        1.0 - c
    }
}

fn sign(right: Right) -> f64 {
    match right {
        Right::Call => 1.0,
        Right::Put => -1.0,
    }
}

/// Black-Scholes-Merton: a European option on `spot` with a continuous
/// rate and yield, `t` in years. At expiry, or at no volatility, the
/// discounted intrinsic value with a step delta.
pub fn black_scholes(
    right: Right,
    spot: f64,
    strike: f64,
    t: f64,
    rate: f64,
    yield_: f64,
    vol: f64,
) -> Greeks {
    let w = sign(right);
    let dq = (-yield_ * t).exp();
    let dr = (-rate * t).exp();
    if t <= 0.0 || vol <= 0.0 {
        let forward = spot * dq / dr;
        let intrinsic = (w * (forward - strike)).max(0.0) * dr;
        let itm = w * (forward - strike) > 0.0;
        return Greeks {
            price: intrinsic,
            delta: if itm { w * dq } else { 0.0 },
            ..Greeks::default()
        };
    }
    let root = t.sqrt();
    let d1 = ((spot / strike).ln() + (rate - yield_ + 0.5 * vol * vol) * t) / (vol * root);
    let d2 = d1 - vol * root;
    let n1 = pdf(d1);
    let price = w * (spot * dq * cdf(w * d1) - strike * dr * cdf(w * d2));
    let delta = w * dq * cdf(w * d1);
    let gamma = dq * n1 / (spot * vol * root);
    let vega = spot * dq * n1 * root;
    let theta = -spot * dq * n1 * vol / (2.0 * root) + w * yield_ * spot * dq * cdf(w * d1)
        - w * rate * strike * dr * cdf(w * d2);
    let rho = w * strike * t * dr * cdf(w * d2);
    let vanna = -dq * n1 * d2 / vol;
    // As time passes (minus d delta / d t to expiry).
    let charm = w * yield_ * dq * cdf(w * d1)
        - dq * n1 * (2.0 * (rate - yield_) * t - d2 * vol * root) / (2.0 * t * vol * root);
    Greeks {
        price,
        delta,
        gamma,
        theta,
        vega,
        rho,
        vanna,
        charm,
    }
}

/// Black-76: a European option on a future at `forward`, discounted at
/// `rate`. Delta and gamma are the future's.
pub fn black_76(right: Right, forward: f64, strike: f64, t: f64, rate: f64, vol: f64) -> Greeks {
    // Black-76 is Black-Scholes on the forward with the yield equal to the
    // rate; rho is the discounting's alone, as the forward is held.
    let g = black_scholes(right, forward, strike, t, rate, rate, vol);
    Greeks {
        rho: -t * g.price,
        ..g
    }
}

/// A discrete cash dividend: when it goes ex, in years from now, and its
/// amount per share.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dividend {
    pub t: f64,
    pub amount: f64,
}

/// The present value at `from` of the dividends going ex after it and by
/// `to`.
fn dividends_pv(dividends: &[Dividend], from: f64, to: f64, rate: f64) -> f64 {
    dividends
        .iter()
        .filter(|d| d.t > from + 1e-12 && d.t <= to + 1e-12)
        .map(|d| d.amount * (-rate * (d.t - from)).exp())
        .sum()
}

/// An American option on a share paying discrete cash dividends (ruling
/// 565: the tree with discrete cash dividends, its steps a definition
/// parameter), on a Cox-Ross-Rubinstein tree; a European one where
/// `american` is false. Price only; [`american_greeks`] for the greeks.
#[allow(clippy::too_many_arguments)]
pub fn american_binomial(
    right: Right,
    spot: f64,
    strike: f64,
    t: f64,
    rate: f64,
    borrow: f64,
    vol: f64,
    dividends: &[Dividend],
    steps: usize,
    american: bool,
) -> f64 {
    tree(
        right, spot, strike, t, rate, borrow, vol, dividends, steps, american,
    )
    .0
}

/// The tree's value and, from its first three levels, delta, gamma and
/// theta.
#[allow(clippy::too_many_arguments)]
fn tree(
    right: Right,
    spot: f64,
    strike: f64,
    t: f64,
    rate: f64,
    borrow: f64,
    vol: f64,
    dividends: &[Dividend],
    steps: usize,
    american: bool,
) -> (f64, f64, f64, f64) {
    let w = sign(right);
    let steps = steps.max(3);
    if t <= 0.0 {
        let value = (w * (spot - strike)).max(0.0);
        return (value, if value > 0.0 { w } else { 0.0 }, 0.0, 0.0);
    }
    let vol = vol.max(1e-8);
    let dt = t / steps as f64;
    let u = (vol * dt.sqrt()).exp();
    let d = 1.0 / u;
    let growth = ((rate - borrow) * dt).exp();
    let p = ((growth - d) / (u - d)).clamp(0.0, 1.0);
    let disc = (-rate * dt).exp();
    // The escrowed share: spot less the dividends' present value to expiry.
    let escrowed = spot - dividends_pv(dividends, 0.0, t, rate);
    // Each level's lowest node, then up by u^2: no power taken per node.
    let up2 = u * u;
    let (pu, pd) = (disc * p, disc * (1.0 - p));
    let mut lowest = escrowed * d.powi(steps as i32);
    let mut values: Vec<f64> = Vec::with_capacity(steps + 1);
    let mut s = lowest;
    for _ in 0..=steps {
        values.push((w * (s - strike)).max(0.0));
        s *= up2;
    }
    let mut level2 = [0.0; 3];
    let mut level1 = [0.0; 2];
    for i in (0..steps).rev() {
        lowest *= u;
        let now = i as f64 * dt;
        let added = if american {
            dividends_pv(dividends, now, t, rate)
        } else {
            0.0
        };
        let mut s = lowest + 0.0;
        for j in 0..=i {
            let held = pu * values[j + 1] + pd * values[j];
            values[j] = if american {
                held.max(w * (s + added - strike))
            } else {
                held
            };
            s *= up2;
        }
        if i == 2 {
            level2.copy_from_slice(&values[..3]);
        }
        if i == 1 {
            level1.copy_from_slice(&values[..2]);
        }
    }
    let at = |level: usize, j: usize| {
        escrowed * u.powi(j as i32) * d.powi((level - j) as i32)
            + dividends_pv(dividends, level as f64 * dt, t, rate)
    };
    let delta = (level1[1] - level1[0]) / (at(1, 1) - at(1, 0));
    let up = (level2[2] - level2[1]) / (at(2, 2) - at(2, 1));
    let down = (level2[1] - level2[0]) / (at(2, 1) - at(2, 0));
    let gamma = (up - down) / (0.5 * (at(2, 2) - at(2, 0)));
    let theta = (level2[1] - values[0]) / (2.0 * dt);
    (values[0], delta, gamma, theta)
}

/// The American tree's greeks: delta, gamma and theta from its nodes; vega,
/// rho, vanna and charm by bump and reprice on the same tree.
#[allow(clippy::too_many_arguments)]
pub fn american_greeks(
    right: Right,
    spot: f64,
    strike: f64,
    t: f64,
    rate: f64,
    borrow: f64,
    vol: f64,
    dividends: &[Dividend],
    steps: usize,
) -> Greeks {
    let (price, delta, gamma, theta) = tree(
        right, spot, strike, t, rate, borrow, vol, dividends, steps, true,
    );
    let at = |s: f64, t: f64, r: f64, v: f64| {
        tree(right, s, strike, t, r, borrow, v, dividends, steps, true)
    };
    let dv = 0.001;
    let dr = 0.0001;
    let up = at(spot, t, rate, vol + dv);
    let down = at(spot, t, rate, (vol - dv).max(1e-6));
    let vega = (up.0 - down.0) / (2.0 * dv);
    let vanna = (up.1 - down.1) / (2.0 * dv);
    let rho = (at(spot, t, rate + dr, vol).0 - at(spot, t, rate - dr, vol).0) / (2.0 * dr);
    let dt = (t / 365.0).min(t / 2.0);
    let charm = if dt > 0.0 {
        (at(spot, t - dt, rate, vol).1 - delta) / dt
    } else {
        0.0
    };
    Greeks {
        price,
        delta,
        gamma,
        theta,
        vega,
        rho,
        vanna,
        charm,
    }
}

/// The volatility at which `price_at` equals `target`, by Brent's method
/// within 0.01% to 500%: `None` where the quote lies outside what any
/// volatility prices (below intrinsic, or above the bound).
pub fn implied_vol(target: f64, price_at: impl Fn(f64) -> f64) -> Option<f64> {
    let (mut a, mut b) = (1e-4, 5.0);
    let (mut fa, mut fb) = (price_at(a) - target, price_at(b) - target);
    if !target.is_finite() || fa * fb > 0.0 {
        return None;
    }
    if fa.abs() < fb.abs() {
        std::mem::swap(&mut a, &mut b);
        std::mem::swap(&mut fa, &mut fb);
    }
    let (mut c, mut fc) = (a, fa);
    let mut bisected = true;
    let mut last = 0.0;
    for _ in 0..200 {
        if fb.abs() < 1e-12 || (b - a).abs() < 1e-12 {
            return Some(b);
        }
        let mut s = if fa != fc && fb != fc {
            a * fb * fc / ((fa - fb) * (fa - fc))
                + b * fa * fc / ((fb - fa) * (fb - fc))
                + c * fa * fb / ((fc - fa) * (fc - fb))
        } else {
            b - fb * (b - a) / (fb - fa)
        };
        let between = {
            let low = (3.0 * a + b) / 4.0;
            (s > low.min(b)) && (s < low.max(b))
        };
        if !between
            || (bisected && (s - b).abs() >= (b - c).abs() / 2.0)
            || (!bisected && (s - b).abs() >= (c - last).abs() / 2.0)
        {
            s = (a + b) / 2.0;
            bisected = true;
        } else {
            bisected = false;
        }
        let fs = price_at(s) - target;
        last = c;
        c = b;
        fc = fb;
        if fa * fs < 0.0 {
            b = s;
            fb = fs;
        } else {
            a = s;
            fa = fs;
        }
        if fa.abs() < fb.abs() {
            std::mem::swap(&mut a, &mut b);
            std::mem::swap(&mut fa, &mut fb);
        }
    }
    Some(b)
}

/// Gatheral's raw SVI for one expiry: total implied variance at log-moneyness
/// k is a + b (rho (k - m) + sqrt((k - m)^2 + sigma^2)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SviRaw {
    pub a: f64,
    pub b: f64,
    pub rho: f64,
    pub m: f64,
    pub sigma: f64,
}

impl SviRaw {
    pub fn total_variance(&self, k: f64) -> f64 {
        let x = k - self.m;
        self.a + self.b * (self.rho * x + (x * x + self.sigma * self.sigma).sqrt())
    }

    /// Whether its parameters stand: b and sigma positive, |rho| below one,
    /// and its least total variance not negative.
    pub fn admissible(&self) -> bool {
        self.b >= 0.0
            && self.sigma > 0.0
            && self.rho.abs() < 1.0
            && self.a + self.b * self.sigma * (1.0 - self.rho * self.rho).sqrt() >= -1e-12
    }

    /// Gatheral's g(k) from the closed-form derivatives: the risk-neutral
    /// density's sign, never negative on a slice free of butterfly
    /// arbitrage.
    pub fn density_sign(&self, k: f64) -> f64 {
        let x = k - self.m;
        let root = (x * x + self.sigma * self.sigma).sqrt();
        let w = self.total_variance(k);
        let w1 = self.b * (self.rho + x / root);
        let w2 = self.b * self.sigma * self.sigma / root.powi(3);
        density_sign(w, w1, w2, k)
    }

    /// The parametrisation the fit moves in: b and sigma by their logs, rho
    /// by its hyperbolic tangent, and a through the least total variance
    /// (its log, against `scale`), so every point it tries stands.
    fn from(p: &[f64; 5], scale: f64) -> SviRaw {
        // |rho| short of one and sigma above nothing, where a flat slice
        // leaves them free to run.
        let (b, rho, sigma) = (
            p[1].exp(),
            p[2].tanh().clamp(-0.999_999, 0.999_999),
            p[4].exp().max(1e-8),
        );
        let least = scale * p[0].exp();
        SviRaw {
            a: least - b * sigma * (1.0 - rho * rho).sqrt(),
            b,
            rho,
            m: p[3],
            sigma,
        }
    }
}

/// Gatheral's g(k) for a slice's total variance: the risk-neutral density's
/// sign, never negative on a slice free of butterfly arbitrage.
fn density_sign(w: f64, w1: f64, w2: f64, k: f64) -> f64 {
    let a = 1.0 - k * w1 / (2.0 * w);
    a * a - w1 * w1 / 4.0 * (1.0 / w + 0.25) + w2 / 2.0
}

/// Whether a slice is free of butterfly arbitrage at each log-moneyness
/// given: its total variance positive and Gatheral's g not negative.
pub fn butterfly_free(w: impl Fn(f64) -> f64, ks: &[f64]) -> bool {
    let h = 1e-4;
    ks.iter().all(|&k| {
        let (lo, mid, hi) = (w(k - h), w(k), w(k + h));
        let w1 = (hi - lo) / (2.0 * h);
        let w2 = (hi - 2.0 * mid + lo) / (h * h);
        mid > 0.0 && density_sign(mid, w1, w2, k) >= -1e-9
    })
}

/// Whether slices in order of expiry are free of calendar arbitrage at each
/// log-moneyness given: total variance never falls as expiry lengthens.
pub fn calendar_free(slices: &[&dyn Fn(f64) -> f64], ks: &[f64]) -> bool {
    slices
        .windows(2)
        .all(|pair| ks.iter().all(|&k| (pair[1])(k) >= (pair[0])(k) - 1e-12))
}

/// A Levenberg-Marquardt least squares over `n` parameters (Marquardt's
/// diagonal scaling): `residuals(p)` the misfit at each point, always the
/// same count; finite-difference Jacobian. Its parameters and its cost.
fn levenberg_marquardt<const N: usize>(
    start: [f64; N],
    residuals: impl Fn(&[f64; N]) -> Vec<f64>,
) -> ([f64; N], f64) {
    let cost = |p: &[f64; N]| residuals(p).iter().map(|r| r * r).sum::<f64>();
    let mut p = start;
    let mut lambda = 1e-3;
    let mut current = cost(&p);
    if !current.is_finite() {
        return (p, f64::INFINITY);
    }
    for _ in 0..400 {
        let r = residuals(&p);
        let mut jac = vec![[0.0; N]; r.len()];
        for k in 0..N {
            let mut q = p;
            let h = 1e-7 * (1.0 + p[k].abs());
            q[k] += h;
            let rq = residuals(&q);
            for i in 0..r.len() {
                jac[i][k] = (rq[i] - r[i]) / h;
            }
        }
        let mut jtj = [[0.0; N]; N];
        let mut jtr = [0.0; N];
        for i in 0..r.len() {
            for a in 0..N {
                jtr[a] += jac[i][a] * r[i];
                for b in 0..N {
                    jtj[a][b] += jac[i][a] * jac[i][b];
                }
            }
        }
        let mut improved = false;
        for _ in 0..30 {
            let mut m = jtj;
            for a in 0..N {
                m[a][a] += lambda * (1e-12 + jtj[a][a]);
            }
            let Some(step) = solve(m, jtr) else {
                lambda *= 10.0;
                continue;
            };
            let mut q = p;
            for a in 0..N {
                q[a] -= step[a];
            }
            let next = cost(&q);
            if next.is_finite() && next < current {
                p = q;
                lambda = (lambda / 3.0).max(1e-15);
                improved = current - next > 1e-14 * current;
                current = next;
                break;
            }
            lambda *= 10.0;
        }
        if !improved {
            break;
        }
    }
    (p, current)
}

/// Gaussian elimination with partial pivoting; `None` for a singular system.
fn solve<const N: usize>(mut m: [[f64; N]; N], mut v: [f64; N]) -> Option<[f64; N]> {
    for col in 0..N {
        let pivot = (col..N).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[pivot][col].abs() < 1e-300 {
            return None;
        }
        m.swap(col, pivot);
        v.swap(col, pivot);
        for row in col + 1..N {
            let f = m[row][col] / m[col][col];
            for k in col..N {
                m[row][k] -= f * m[col][k];
            }
            v[row] -= f * v[col];
        }
    }
    let mut x = [0.0; N];
    for row in (0..N).rev() {
        let known: f64 = m[row]
            .iter()
            .zip(x.iter())
            .skip(row + 1)
            .map(|(a, b)| a * b)
            .sum();
        x[row] = (v[row] - known) / m[row][row];
    }
    Some(x)
}

/// Each point's weight over the slice's mean weight, held within a
/// hundredth and a hundred times it: one tight quote never fits alone.
fn normalised(points: &[(f64, f64, f64)]) -> Vec<f64> {
    let mean = points.iter().map(|p| p.2.max(0.0)).sum::<f64>() / points.len() as f64;
    points
        .iter()
        .map(|p| {
            if mean > 0.0 && mean.is_finite() {
                (p.2.max(0.0) / mean).clamp(0.01, 100.0)
            } else {
                1.0
            }
        })
        .collect()
}

/// How heavily a no-arbitrage condition weighs against the misfit.
const PENALTY: f64 = 1_000.0;

/// The least density sign the fit aims for on its grid.
const DENSITY_MARGIN: f64 = 1e-3;

/// Raw SVI fitted to one slice's (log-moneyness, total variance, weight)
/// points: `None` for fewer than five points. See [`fit_svi_above`].
pub fn fit_svi(points: &[(f64, f64, f64)]) -> Option<SviRaw> {
    fit_svi_above(points, None)
}

/// Raw SVI fitted to one slice's (log-moneyness, total variance, weight)
/// points, each misfit relative to the slice's at-the-money total variance
/// so a one-day slice weighs as a one-year one, by Levenberg-Marquardt from
/// a spread of starting points (the slice's level, its wings' slopes, its
/// least variance left and right of the money), keeping the best:
///
/// - its least total variance held non-negative by its parametrisation;
/// - Gatheral's butterfly condition (g >= 0) and Lee's wing bound
///   (b (1 + |rho|) <= 2) as penalties over its strikes and past them;
/// - with `below`, the slice before it in expiry, its total variance held
///   at or above that slice's over the same range (the calendar
///   condition), as a penalty.
///
/// `None` for fewer than five points, or no start that stands.
pub fn fit_svi_above(
    points: &[(f64, f64, f64)],
    below: Option<&dyn Fn(f64) -> f64>,
) -> Option<SviRaw> {
    if points.len() < 5 {
        return None;
    }
    let weights = normalised(points);
    let lo = points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let hi = points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let span = (hi - lo).max(1e-3);
    let (atm_k, atm_w) = points
        .iter()
        .min_by(|a, b| a.0.abs().total_cmp(&b.0.abs()))
        .map(|p| (p.0, p.1))?;
    let scale = atm_w.max(1e-8);
    let least = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    // The conditions' grid: the quoted range and a half-span past each end.
    let grid: Vec<f64> = (0..=60)
        .map(|i| lo - span / 2.0 + 2.0 * span * i as f64 / 60.0)
        .collect();
    let residuals = |p: &[f64; 5]| -> Vec<f64> {
        let svi = SviRaw::from(p, scale);
        let mut r: Vec<f64> = points
            .iter()
            .zip(&weights)
            .map(|((k, w, _), weight)| weight.sqrt() * (svi.total_variance(*k) - w) / scale)
            .collect();
        for &k in &grid {
            // A margin, so the density stands between the grid's points.
            r.push(PENALTY * (svi.density_sign(k) - DENSITY_MARGIN).min(0.0));
            if let Some(before) = below {
                let floor = before(k);
                r.push(PENALTY * ((floor - svi.total_variance(k)) / scale).max(0.0));
            }
        }
        r.push(PENALTY * (svi.b * (1.0 + svi.rho.abs()) - 2.0).max(0.0));
        r
    };
    // The wings' slopes from the outermost points, and the smile's depth.
    let slope = |far: f64| -> f64 {
        points
            .iter()
            .min_by(|a, b| (a.0 - far).abs().total_cmp(&(b.0 - far).abs()))
            .map_or(0.0, |p| (p.1 - atm_w) / (p.0 - atm_k).abs().max(1e-6))
    };
    let (left, right) = (slope(lo), slope(hi));
    let b0 = ((left.max(0.0) + right.max(0.0)) / 2.0).max(1e-3 * scale / span);
    let rho0 = ((right - left) / (right.abs() + left.abs()).max(1e-12)).clamp(-0.9, 0.9);
    let mut best: Option<(SviRaw, f64)> = None;
    for m in [atm_k, atm_k - span / 5.0, atm_k + span / 5.0] {
        for sigma in [span / 10.0, span / 3.0] {
            for rho in [rho0, -0.6, 0.0] {
                let start = [
                    (0.5 * least.max(1e-12) / scale).ln(),
                    b0.ln(),
                    rho.atanh(),
                    m,
                    sigma.ln(),
                ];
                let (p, cost) = levenberg_marquardt(start, residuals);
                let svi = SviRaw::from(&p, scale);
                let stands = svi.admissible()
                    && [svi.a, svi.b, svi.rho, svi.m, svi.sigma]
                        .iter()
                        .all(|x| x.is_finite());
                if stands && cost.is_finite() && best.is_none_or(|(_, c)| cost < c) {
                    best = Some((svi, cost));
                }
            }
        }
    }
    best.map(|(svi, _)| svi)
}

/// The power-law SSVI across slices (Gatheral and Jacquier, 2014, 4.5):
/// w(k, theta) = theta / 2 (1 + rho phi k + sqrt((phi k + rho)^2 + 1 - rho^2)),
/// phi(theta) = eta / (theta^gamma (1 + theta)^(1 - gamma)), each slice its
/// at-the-money total variance theta.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ssvi {
    pub rho: f64,
    pub eta: f64,
    pub gamma: f64,
}

impl Ssvi {
    fn phi(&self, theta: f64) -> f64 {
        self.eta / (theta.powf(self.gamma) * (1.0 + theta).powf(1.0 - self.gamma))
    }

    pub fn total_variance(&self, k: f64, theta: f64) -> f64 {
        let pk = self.phi(theta) * k;
        theta / 2.0
            * (1.0 + self.rho * pk + ((pk + self.rho).powi(2) + 1.0 - self.rho * self.rho).sqrt())
    }

    /// Gatheral and Jacquier's sufficient conditions for no static
    /// arbitrage on the power law: |rho| below one, eta (1 + |rho|) at most
    /// two, gamma in (0, 1/2].
    pub fn free_of_static_arbitrage(&self) -> bool {
        self.rho.abs() < 1.0
            && self.eta > 0.0
            && self.eta * (1.0 + self.rho.abs()) <= 2.0 + 1e-12
            && self.gamma > 0.0
            && self.gamma <= 0.5 + 1e-12
    }
}

/// A point a slice is fitted to: its log-moneyness, its total variance and
/// its weight.
pub type Point = (f64, f64, f64);

/// One expiry's slice for [`fit_ssvi`]: its at-the-money total variance and
/// its (log-moneyness, total variance, weight) points.
pub type Slice = (f64, Vec<Point>);

/// Thetas made non-decreasing in expiry by pooling adjacent violators (the
/// least-squares isotonic fit): SSVI's calendar condition needs them so,
/// and a noisy at-the-money level must not refuse the surface.
pub fn non_decreasing(thetas: &[f64]) -> Vec<f64> {
    let mut blocks: Vec<(f64, usize)> = Vec::new();
    for &theta in thetas {
        blocks.push((theta, 1));
        while blocks.len() > 1 {
            let (b, nb) = blocks[blocks.len() - 1];
            let (a, na) = blocks[blocks.len() - 2];
            if a <= b {
                break;
            }
            blocks.pop();
            blocks.pop();
            let n = na + nb;
            blocks.push(((a * na as f64 + b * nb as f64) / n as f64, n));
        }
    }
    blocks
        .into_iter()
        .flat_map(|(v, n)| std::iter::repeat_n(v, n))
        .collect()
}

/// SSVI fitted across slices to every slice's points, each misfit relative
/// to its slice's theta and each slice's weights normalised, so every
/// expiry counts alike; the slices' thetas (made non-decreasing first) held,
/// and the fit held to Gatheral and Jacquier's conditions by its
/// parametrisation, from a spread of starting points, keeping the best.
/// `None` with no point.
pub fn fit_ssvi(slices: &[Slice]) -> Option<Ssvi> {
    if slices.is_empty() || slices.iter().all(|(_, points)| points.is_empty()) {
        return None;
    }
    let thetas = non_decreasing(&slices.iter().map(|s| s.0).collect::<Vec<_>>());
    let held: Vec<(f64, &Vec<Point>, Vec<f64>)> = slices
        .iter()
        .zip(&thetas)
        .filter(|((_, points), _)| !points.is_empty())
        .map(|((_, points), &theta)| (theta.max(1e-12), points, normalised(points)))
        .collect();
    let make = |p: &[f64; 3]| {
        let rho = p[0].tanh();
        let gamma = 0.5 / (1.0 + (-p[2]).exp());
        // eta in (0, 2 / (1 + |rho|)).
        let eta = 2.0 / (1.0 + rho.abs()) / (1.0 + (-p[1]).exp());
        Ssvi { rho, eta, gamma }
    };
    let residuals = |p: &[f64; 3]| -> Vec<f64> {
        let ssvi = make(p);
        held.iter()
            .flat_map(|(theta, points, weights)| {
                let n = (points.len() as f64).sqrt();
                points.iter().zip(weights).map(move |((k, w, _), weight)| {
                    weight.sqrt() * (ssvi.total_variance(*k, *theta) - w) / theta / n
                })
            })
            .collect()
    };
    let mut best: Option<(Ssvi, f64)> = None;
    for rho in [-0.7f64, -0.3, 0.3] {
        for eta in [0.0f64, 2.0] {
            for gamma in [-1.0f64, 1.0] {
                let (p, cost) = levenberg_marquardt([rho.atanh(), eta, gamma], residuals);
                if cost.is_finite() && best.is_none_or(|(_, c)| cost < c) {
                    best = Some((make(&p), cost));
                }
            }
        }
    }
    best.map(|(ssvi, _)| ssvi)
        .filter(Ssvi::free_of_static_arbitrage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() <= tolerance
    }

    #[test]
    fn the_normal_distribution_is_double_precision() {
        assert!(close(cdf(0.0), 0.5, 1e-15));
        assert!(close(cdf(1.96), 0.975_002_104_851_780, 1e-14));
        assert!(close(cdf(-1.0), 0.158_655_253_931_457, 1e-14));
        // Past seven standard deviations, the continued fraction: tiny in
        // absolute terms, close in relative ones.
        assert!((cdf(-8.0) / 6.220_960_574_271_78e-16 - 1.0).abs() < 1e-3);
        assert!(cdf(-8.0) < 1e-15);
        assert!(cdf(40.0) == 1.0 && cdf(-40.0) == 0.0);
    }

    #[test]
    fn black_scholes_prices_the_textbook_case_and_keeps_parity() {
        // S 100, K 100, r 5%, no yield, vol 20%, one year.
        let call = black_scholes(Right::Call, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        let put = black_scholes(Right::Put, 100.0, 100.0, 1.0, 0.05, 0.0, 0.2);
        assert!(
            close(call.price, 10.450_583_572_185_565, 1e-10),
            "{}",
            call.price
        );
        assert!(
            close(put.price, 5.573_526_022_256_971, 1e-10),
            "{}",
            put.price
        );
        // Parity with a yield: C - P = S e^-qT - K e^-rT.
        for (s, k, t, r, q, v) in [
            (95.0, 110.0, 0.4, 0.03, 0.015, 0.31),
            (120.0, 80.0, 2.0, -0.005, 0.0, 0.55),
        ] {
            let c = black_scholes(Right::Call, s, k, t, r, q, v).price;
            let p = black_scholes(Right::Put, s, k, t, r, q, v).price;
            assert!(close(c - p, s * (-q * t).exp() - k * (-r * t).exp(), 1e-10));
        }
    }

    #[test]
    fn each_greek_is_its_price_bumped() {
        let (s, k, t, r, q, v) = (103.0, 100.0, 0.75, 0.04, 0.012, 0.27);
        for right in [Right::Call, Right::Put] {
            let g = black_scholes(right, s, k, t, r, q, v);
            let price = |s: f64, t: f64, r: f64, v: f64| black_scholes(right, s, k, t, r, q, v);
            let h = 1e-4;
            assert!(close(
                g.delta,
                (price(s + h, t, r, v).price - price(s - h, t, r, v).price) / (2.0 * h),
                1e-6
            ));
            assert!(close(
                g.gamma,
                (price(s + h, t, r, v).delta - price(s - h, t, r, v).delta) / (2.0 * h),
                1e-6
            ));
            assert!(close(
                g.vega,
                (price(s, t, r, v + h).price - price(s, t, r, v - h).price) / (2.0 * h),
                1e-5
            ));
            assert!(close(
                g.rho,
                (price(s, t, r + h, v).price - price(s, t, r - h, v).price) / (2.0 * h),
                1e-5
            ));
            assert!(close(
                g.theta,
                -(price(s, t + h, r, v).price - price(s, t - h, r, v).price) / (2.0 * h),
                1e-5
            ));
            assert!(close(
                g.vanna,
                (price(s, t, r, v + h).delta - price(s, t, r, v - h).delta) / (2.0 * h),
                1e-6
            ));
            assert!(close(
                g.charm,
                -(price(s, t + h, r, v).delta - price(s, t - h, r, v).delta) / (2.0 * h),
                1e-6
            ));
        }
    }

    #[test]
    fn black_76_is_the_discounted_black_formula() {
        // F 100, K 100, r 5%, vol 20%, one year: e^-0.05 (F N(d1) - K N(d2)).
        let call = black_76(Right::Call, 100.0, 100.0, 1.0, 0.05, 0.2);
        let undiscounted = 100.0 * (cdf(0.1) - cdf(-0.1));
        assert!(
            close(call.price, (-0.05f64).exp() * undiscounted, 1e-12),
            "{}",
            call.price
        );
        let put = black_76(Right::Put, 100.0, 100.0, 1.0, 0.05, 0.2);
        assert!(
            close(call.price - put.price, 0.0, 1e-12),
            "at the money, parity is zero"
        );
        let h = 1e-5;
        let bumped = (black_76(Right::Call, 100.0, 100.0, 1.0, 0.05 + h, 0.2).price
            - black_76(Right::Call, 100.0, 100.0, 1.0, 0.05 - h, 0.2).price)
            / (2.0 * h);
        assert!(close(call.rho, bumped, 1e-6));
    }

    #[test]
    fn the_tree_prices_hulls_american_put_and_a_european_call_as_black_scholes_does() {
        // Hull's American put: S 50, K 50, r 10%, vol 40%, five months;
        // 4.28 to the cent on a fine tree, 4.49 on his five steps.
        let put = american_binomial(
            Right::Put,
            50.0,
            50.0,
            5.0 / 12.0,
            0.10,
            0.0,
            0.40,
            &[],
            2_000,
            true,
        );
        assert!(close(put, 4.284, 0.003), "{put}");
        let five = american_binomial(
            Right::Put,
            50.0,
            50.0,
            5.0 / 12.0,
            0.10,
            0.0,
            0.40,
            &[],
            5,
            true,
        );
        assert!(close(five, 4.49, 0.005), "{five}");
        // An American call on a share paying nothing is never exercised early.
        let call = american_binomial(
            Right::Call,
            50.0,
            50.0,
            5.0 / 12.0,
            0.10,
            0.0,
            0.40,
            &[],
            2_000,
            true,
        );
        let european = black_scholes(Right::Call, 50.0, 50.0, 5.0 / 12.0, 0.10, 0.0, 0.40).price;
        assert!(close(call, european, 0.005), "{call} against {european}");
    }

    #[test]
    fn a_discrete_dividend_lowers_a_call_and_may_bring_its_exercise_forward() {
        let dividends = [Dividend {
            t: 0.2,
            amount: 2.0,
        }];
        let paying = american_binomial(
            Right::Call,
            100.0,
            95.0,
            0.5,
            0.05,
            0.0,
            0.25,
            &dividends,
            500,
            true,
        );
        let none = american_binomial(
            Right::Call,
            100.0,
            95.0,
            0.5,
            0.05,
            0.0,
            0.25,
            &[],
            500,
            true,
        );
        let european = american_binomial(
            Right::Call,
            100.0,
            95.0,
            0.5,
            0.05,
            0.0,
            0.25,
            &dividends,
            500,
            false,
        );
        assert!(paying < none);
        assert!(paying >= european - 1e-12, "American at least European");
        // Escrowed European call against Black-Scholes on the share less the
        // dividend's present value.
        let escrow = 100.0 - 2.0 * (-0.05f64 * 0.2).exp();
        let closed = black_scholes(Right::Call, escrow, 95.0, 0.5, 0.05, 0.0, 0.25).price;
        assert!(close(european, closed, 0.01), "{european} against {closed}");
        // Monotone in strike and in expiry.
        let higher = american_binomial(
            Right::Call,
            100.0,
            105.0,
            0.5,
            0.05,
            0.0,
            0.25,
            &dividends,
            500,
            true,
        );
        let longer = american_binomial(
            Right::Call,
            100.0,
            95.0,
            1.0,
            0.05,
            0.0,
            0.25,
            &dividends,
            500,
            true,
        );
        assert!(higher < paying && longer > paying);
    }

    #[test]
    fn the_trees_greeks_agree_with_black_scholes_where_nothing_is_exercised_early() {
        let g = american_greeks(Right::Call, 100.0, 100.0, 0.5, 0.03, 0.0, 0.3, &[], 800);
        let b = black_scholes(Right::Call, 100.0, 100.0, 0.5, 0.03, 0.0, 0.3);
        assert!(close(g.price, b.price, 0.01));
        assert!(close(g.delta, b.delta, 0.005), "{} {}", g.delta, b.delta);
        assert!(close(g.gamma, b.gamma, 0.002), "{} {}", g.gamma, b.gamma);
        assert!(close(g.vega, b.vega, 0.1), "{} {}", g.vega, b.vega);
        assert!(close(g.rho, b.rho, 0.1), "{} {}", g.rho, b.rho);
        assert!(close(g.theta, b.theta, 0.2), "{} {}", g.theta, b.theta);
        assert!(close(g.vanna, b.vanna, 0.02), "{} {}", g.vanna, b.vanna);
    }

    #[test]
    fn an_implied_vol_inverts_each_model_and_refuses_a_quote_below_intrinsic() {
        let target = black_scholes(Right::Put, 98.0, 100.0, 0.3, 0.04, 0.01, 0.237).price;
        let found = implied_vol(target, |v| {
            black_scholes(Right::Put, 98.0, 100.0, 0.3, 0.04, 0.01, v).price
        })
        .unwrap();
        assert!(close(found, 0.237, 1e-9), "{found}");
        let tree_price = american_binomial(
            Right::Put,
            98.0,
            100.0,
            0.3,
            0.04,
            0.0,
            0.31,
            &[Dividend {
                t: 0.1,
                amount: 0.5,
            }],
            200,
            true,
        );
        let found = implied_vol(tree_price, |v| {
            american_binomial(
                Right::Put,
                98.0,
                100.0,
                0.3,
                0.04,
                0.0,
                v,
                &[Dividend {
                    t: 0.1,
                    amount: 0.5,
                }],
                200,
                true,
            )
        })
        .unwrap();
        assert!(close(found, 0.31, 1e-6), "{found}");
        let futures = black_76(Right::Call, 5800.0, 5900.0, 0.1, 0.045, 0.16).price;
        let found = implied_vol(futures, |v| {
            black_76(Right::Call, 5800.0, 5900.0, 0.1, 0.045, v).price
        })
        .unwrap();
        assert!(close(found, 0.16, 1e-9));
        // Below intrinsic: no volatility prices it.
        assert!(implied_vol(1.0, |v| black_scholes(
            Right::Call,
            120.0,
            100.0,
            0.5,
            0.0,
            0.0,
            v
        )
        .price)
        .is_none());
    }

    fn slice(svi: &SviRaw) -> Vec<(f64, f64, f64)> {
        (-20..=20)
            .map(|i| {
                let k = i as f64 * 0.025;
                (k, svi.total_variance(k), 1.0)
            })
            .collect()
    }

    #[test]
    fn svi_is_fitted_back_from_its_own_slice_and_checked_for_butterflies() {
        let truth = SviRaw {
            a: 0.02,
            b: 0.12,
            rho: -0.45,
            m: 0.03,
            sigma: 0.18,
        };
        let fitted = fit_svi(&slice(&truth)).unwrap();
        for i in -20..=20 {
            let k = i as f64 * 0.025;
            assert!(
                close(fitted.total_variance(k), truth.total_variance(k), 1e-7),
                "{k}"
            );
        }
        let ks: Vec<f64> = (-40..=40).map(|i| i as f64 * 0.025).collect();
        assert!(butterfly_free(|k| fitted.total_variance(k), &ks));
        // A slice whose wings bend too sharply admits a butterfly.
        let bent = SviRaw {
            a: -0.05,
            b: 1.2,
            rho: 0.95,
            m: 0.0,
            sigma: 0.05,
        };
        assert!(!butterfly_free(|k| bent.total_variance(k), &ks));
        assert!(
            fit_svi(&slice(&truth)[..4]).is_none(),
            "five points at least"
        );
    }

    /// Raw SVI against QuantLib 1.40's SviSmileSection (the SDK's pinned
    /// oracle) on Gatheral and Jacquier's example 3.1 (Vogt's slice, 2014):
    /// the same total variance, and the butterfly arbitrage the paper shows
    /// it to admit, found by the closed-form g.
    #[test]
    fn raw_svi_is_quantlibs_and_finds_vogts_butterfly() {
        let vogt = SviRaw {
            a: -0.0410,
            b: 0.1331,
            rho: 0.3060,
            m: 0.3586,
            sigma: 0.4153,
        };
        for (k, quantlib) in [
            (-0.5, 0.050_976_571_690),
            (0.0, 0.017_426_252_555),
            (0.3586, 0.014_276_430_000),
            (1.0, 0.086_826_709_752),
            (1.5, 0.167_151_699_886),
        ] {
            assert!(close(vogt.total_variance(k), quantlib, 1e-11), "{k}");
        }
        let ks: Vec<f64> = (-150..=150).map(|i| i as f64 * 0.01).collect();
        assert!(!butterfly_free(|k| vogt.total_variance(k), &ks));
        assert!(ks.iter().any(|&k| vogt.density_sign(k) < 0.0));
        // The closed form agrees with the finite differences.
        for &k in &ks {
            let h = 1e-4;
            let (lo, mid, hi) = (
                vogt.total_variance(k - h),
                vogt.total_variance(k),
                vogt.total_variance(k + h),
            );
            let g = density_sign(
                mid,
                (hi - lo) / (2.0 * h),
                (hi - 2.0 * mid + lo) / (h * h),
                k,
            );
            assert!(close(vogt.density_sign(k), g, 1e-5), "{k}");
        }
        // Fitted to Vogt's own points, the butterfly penalty keeps the fit
        // free of it: close to the slice, and arbitrage-free.
        let points: Vec<(f64, f64, f64)> = (-30..=30)
            .map(|i| {
                let k = i as f64 * 0.05;
                (k, vogt.total_variance(k), 1.0)
            })
            .collect();
        let repaired = fit_svi(&points).unwrap();
        let least = ks
            .iter()
            .map(|&k| repaired.density_sign(k))
            .fold(f64::INFINITY, f64::min);
        assert!(
            butterfly_free(|k| repaired.total_variance(k), &ks),
            "{repaired:?}: least g {least}"
        );
        let errors: Vec<f64> = points
            .iter()
            .map(|(k, w, _)| ((repaired.total_variance(*k) - w) / w).abs())
            .collect();
        let rms = (errors.iter().map(|e| e * e).sum::<f64>() / errors.len() as f64).sqrt();
        let worst = errors.iter().copied().fold(0.0, f64::max);
        assert!(rms < 0.15 && worst < 0.45, "rms {rms}, worst {worst}");
    }

    /// A slice whose parameters a flat smile leaves free still fits: a
    /// short-dated slice of a thousandth of a year's variance, flat.
    #[test]
    fn a_flat_short_dated_slice_fits() {
        let points: Vec<(f64, f64, f64)> = (-10..=10)
            .map(|i| (i as f64 * 0.01, 0.8f64.powi(2) * 0.003, 1.0))
            .collect();
        let fitted = fit_svi(&points).unwrap();
        for (k, w, _) in &points {
            assert!(close(fitted.total_variance(*k), *w, 1e-6 * w), "{k}");
        }
    }

    #[test]
    fn ssvi_is_fitted_across_slices_free_of_static_arbitrage() {
        let truth = Ssvi {
            rho: -0.6,
            eta: 1.1,
            gamma: 0.4,
        };
        let thetas = [0.005, 0.012, 0.025, 0.05, 0.09];
        let slices: Vec<Slice> = thetas
            .iter()
            .map(|&theta| {
                (
                    theta,
                    (-15..=15)
                        .map(|i| {
                            let k = i as f64 * 0.03;
                            (k, truth.total_variance(k, theta), 1.0)
                        })
                        .collect(),
                )
            })
            .collect();
        let fitted = fit_ssvi(&slices).unwrap();
        assert!(fitted.free_of_static_arbitrage());
        assert!(
            close(fitted.rho, truth.rho, 1e-4)
                && close(fitted.eta, truth.eta, 1e-3)
                && close(fitted.gamma, truth.gamma, 1e-3),
            "{fitted:?}"
        );
        let ks: Vec<f64> = (-30..=30).map(|i| i as f64 * 0.03).collect();
        let curves: Vec<Box<dyn Fn(f64) -> f64>> = thetas
            .iter()
            .map(|&theta| {
                Box::new(move |k| fitted.total_variance(k, theta)) as Box<dyn Fn(f64) -> f64>
            })
            .collect();
        let refs: Vec<&dyn Fn(f64) -> f64> = curves.iter().map(|c| c.as_ref()).collect();
        assert!(calendar_free(&refs, &ks));
        for curve in &curves {
            assert!(butterfly_free(curve, &ks));
        }
        // Falling thetas are pooled to non-decreasing ones, not refused.
        assert_eq!(
            non_decreasing(&[1.0, 3.0, 2.0, 4.0]),
            vec![1.0, 2.5, 2.5, 4.0]
        );
        assert_eq!(non_decreasing(&[3.0, 2.0, 1.0]), vec![2.0, 2.0, 2.0]);
        let mut noisy = slices.clone();
        noisy.swap(1, 2);
        let pooled = fit_ssvi(&noisy).unwrap();
        assert!(pooled.free_of_static_arbitrage(), "{pooled:?}");
    }

    /// The tree on a T1 chain refresh (the plan's Track B: benchmark the
    /// tree; Andersen-Lake only if too slow). Run by hand, optimised:
    ///
    ///     cargo test -p meridian-analytics-engine --release options::tests::bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_a_t1_chain_refresh_on_the_tree() {
        // A large single-name chain: 12 expiries by 80 strikes, calls and
        // puts, each inverted at bid, mid and ask (5,760 inversions) on a
        // 200-step tree with one dividend, and the mid's greeks.
        let started = std::time::Instant::now();
        let mut inversions = 0;
        for e in 1..=12 {
            let t = e as f64 * 30.0 / 365.0;
            for s in 0..80 {
                let strike = 60.0 + s as f64;
                for right in [Right::Call, Right::Put] {
                    let price = |v: f64| {
                        american_binomial(
                            right,
                            100.0,
                            strike,
                            t,
                            0.045,
                            0.0,
                            v,
                            &[Dividend {
                                t: 0.12,
                                amount: 0.24,
                            }],
                            200,
                            true,
                        )
                    };
                    let mid = price(0.25 + 0.001 * s as f64);
                    for quote in [mid * 0.98, mid, mid * 1.02] {
                        if implied_vol(quote, price).is_some() {
                            inversions += 1;
                        }
                    }
                    let _ = american_greeks(
                        right,
                        100.0,
                        strike,
                        t,
                        0.045,
                        0.0,
                        0.25,
                        &[Dividend {
                            t: 0.12,
                            amount: 0.24,
                        }],
                        200,
                    );
                }
            }
        }
        let took = started.elapsed();
        println!("T1 chain refresh: {inversions} inversions and 1,920 greeks on a 200-step tree in {took:?}");
        // The inversions alone, and on a 500-step tree.
        for steps in [200, 500] {
            let started = std::time::Instant::now();
            let mut count = 0;
            for e in 1..=12 {
                let t = e as f64 * 30.0 / 365.0;
                for s in 0..80 {
                    let strike = 60.0 + s as f64;
                    for right in [Right::Call, Right::Put] {
                        let price = |v: f64| {
                            american_binomial(
                                right,
                                100.0,
                                strike,
                                t,
                                0.045,
                                0.0,
                                v,
                                &[Dividend {
                                    t: 0.12,
                                    amount: 0.24,
                                }],
                                steps,
                                true,
                            )
                        };
                        if implied_vol(price(0.25), price).is_some() {
                            count += 1;
                        }
                    }
                }
            }
            println!(
                "{count} mid inversions on a {steps}-step tree in {:?}",
                started.elapsed()
            );
        }
    }
}
