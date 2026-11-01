//! A pool's or a class's prepayment measures (contract v29; W12.1; ruling
//! 688): historical SMM and CPR from its `Factor` rows, and its weighted
//! average life and yield at a stated CPR -- static, no prepayment model.
//!
//! **Scheduled amortization.** A factor's fall is scheduled principal and
//! prepayment together. The pool's terms hold no WAC or WAM, so the
//! schedule is a level-payment mortgage at the pass-through rate (a class's
//! coupon) over the months left to its maturity date, the definitions' stated
//! parameter: over a window of `n` months from factor `F0` to `F1`, the
//! survival from prepayment is `(F1 / F0) / (S1 / S0)`, `S` the scheduled
//! balance, and `SMM = 1 - survival^(1/n)`, `CPR = 1 - (1 - SMM)^12`
//! (SIFMA's Uniform Practices' definitions). A window reaching before the
//! pool's issue month, or past a month whose factor is not held, is not
//! evaluated.

/// A level-payment balance per 1 of original after `elapsed` of `term`
/// months at `rate` a year (a fraction).
pub fn scheduled(rate: f64, term: u32, elapsed: u32) -> f64 {
    let c = rate / 12.0;
    let n = f64::from(term);
    let k = f64::from(elapsed.min(term));
    if c == 0.0 {
        return 1.0 - k / n;
    }
    ((1.0 + c).powf(n) - (1.0 + c).powf(k)) / ((1.0 + c).powf(n) - 1.0)
}

/// SMM and CPR, as fractions, over `months` from `start` to `end` factor,
/// `start_age` months after issue, on a `term`-month schedule at `rate`.
pub fn smm_cpr(
    start: f64,
    end: f64,
    months: u32,
    start_age: u32,
    term: u32,
    rate: f64,
) -> Option<(f64, f64)> {
    if months == 0 || start <= 0.0 {
        return None;
    }
    let s0 = scheduled(rate, term, start_age);
    let s1 = scheduled(rate, term, start_age + months);
    if s0 <= 0.0 || s1 <= 0.0 {
        return None;
    }
    let survival = (end / start) / (s1 / s0);
    let smm = 1.0 - survival.max(0.0).powf(1.0 / f64::from(months));
    let cpr = 1.0 - (1.0 - smm).powi(12);
    Some((smm, cpr))
}

/// A pool's projected months at a constant CPR: each month's interest and
/// principal per 1 of current face.
#[derive(Debug, Clone)]
pub struct Projection {
    /// Each month's (months from now, interest, principal).
    pub months: Vec<(u32, f64, f64)>,
}

/// The pool projected from now, `left` months to run, at the coupon `rate`
/// (a fraction) and `cpr` (a fraction): scheduled principal on a level
/// payment over the months left, prepayment the SMM of what remains.
pub fn project(rate: f64, left: u32, cpr: f64) -> Projection {
    let smm = 1.0 - (1.0 - cpr).powf(1.0 / 12.0);
    let c = rate / 12.0;
    let mut balance = 1.0;
    let mut months = Vec::with_capacity(left as usize);
    for k in 1..=left {
        if balance <= 1e-15 {
            break;
        }
        let remaining = f64::from(left - k + 1);
        let payment = if c == 0.0 {
            balance / remaining
        } else {
            balance * c / (1.0 - (1.0 + c).powf(-remaining))
        };
        let interest = balance * c;
        let scheduled = (payment - interest).min(balance);
        let prepaid = smm * (balance - scheduled);
        let principal = scheduled + prepaid;
        months.push((k, interest, principal));
        balance -= principal;
    }
    Projection { months }
}

impl Projection {
    /// Weighted average life in years: each month's principal times its
    /// time, over the principal.
    pub fn wal(&self) -> f64 {
        let (weighted, total) = self.months.iter().fold((0.0, 0.0), |(w, t), (k, _, p)| {
            (w + f64::from(*k) / 12.0 * p, t + p)
        });
        if total == 0.0 {
            0.0
        } else {
            weighted / total
        }
    }

    /// The price per 1 of current face at a monthly-compounded yield `y`
    /// (a fraction a year), each month paid `delay` days late (a payment
    /// delay of 30 days is none: the month's own).
    pub fn price(&self, y: f64, delay: u32) -> f64 {
        let lag = (f64::from(delay) - 30.0).max(0.0) / 30.0;
        self.months
            .iter()
            .map(|(k, i, p)| (i + p) / (1.0 + y / 12.0).powf(f64::from(*k) + lag))
            .sum()
    }

    /// The monthly-compounded yield a price per 1 of current face is at,
    /// and its bond-equivalent: `2 ((1 + y/12)^6 - 1)`.
    pub fn yield_of(&self, price: f64, delay: u32) -> Option<(f64, f64)> {
        let (mut lo, mut hi) = (-0.5, 2.0);
        let g = |y: f64| self.price(y, delay) - price;
        if g(lo).signum() == g(hi).signum() {
            return None;
        }
        for _ in 0..300 {
            let mid = 0.5 * (lo + hi);
            if g(mid) > 0.0 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let y = 0.5 * (lo + hi);
        Some((y, 2.0 * ((1.0 + y / 12.0).powi(6) - 1.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_paying_only_its_schedule_prepays_nothing() {
        let (rate, term) = (0.06, 360);
        let (f0, f1) = (scheduled(rate, term, 24), scheduled(rate, term, 27));
        let (smm, cpr) = smm_cpr(f0, f1, 3, 24, term, rate).unwrap();
        assert!(smm.abs() < 1e-12 && cpr.abs() < 1e-12);
        // A further 1% of the balance a month is an SMM of 1%, a CPR of
        // 11.36%.
        let f1 = f0 * (f1 / f0) * 0.99f64.powi(3);
        let (smm, cpr) = smm_cpr(f0, f1, 3, 24, term, rate).unwrap();
        assert!((smm - 0.01).abs() < 1e-12);
        assert!((cpr - (1.0 - 0.99f64.powi(12))).abs() < 1e-12);
    }

    #[test]
    fn a_zero_cpr_projection_is_its_schedule_and_its_yield_its_coupon_at_par() {
        let projected = project(0.06, 360, 0.0);
        let principal: f64 = projected.months.iter().map(|(_, _, p)| p).sum();
        assert!((principal - 1.0).abs() < 1e-9);
        // A 30-year 6% mortgage's WAL is about 19.3 years.
        assert!((projected.wal() - 19.3).abs() < 0.1, "{}", projected.wal());
        let (monthly, bey) = projected.yield_of(1.0, 30).unwrap();
        assert!((monthly - 0.06).abs() < 1e-10);
        assert!((bey - 2.0 * (1.005f64.powi(6) - 1.0)).abs() < 1e-10);
    }
}
