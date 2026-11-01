//! Price to yield and back (`bond_yield_solver` v1; contract v29; W12.1,
//! W12.3), by the instrument's day count and frequency.
//!
//! Three methods, each the market's own:
//!
//! - **Treasury notes and bonds** (31 CFR 356, Appendix B): the dirty price
//!   is each payment discounted a whole half-year at a time and the
//!   fraction of the current half-year at simple interest,
//!   `[C/2 + C/2 a_n + 100 v^n] / (1 + (r/s)(i/2))`, exact, the dirty price
//!   and the accrued interest each rounded half up to six places and the
//!   price their difference -- equal to every published auction's
//!   `price_per100` at its `high_yield`. A STRIPS component on the same
//!   half-year grid, its one payment discounted so.
//! - **Treasury bills** (Appendix B, section III): the price from the bank
//!   discount rate, `100 (1 - d t / 360)`, and the investment rate
//!   (bond-equivalent) from the price, on 365 days (366 where the year
//!   after the issue holds a February 29th).
//! - **Every other bond** (SIFMA's standard formula; MSRB Rule G-33(b)(i)
//!   for a municipal): with one coupon period or less to redemption, simple
//!   interest over the days left; otherwise each payment discounted at
//!   `(1 + y/m)^(k - 1 + DSC/E)`. A zero coupon on a half-year grid from its
//!   maturity, as a STRIPS component is.
//!
//! Yields are solved in f64 (the one-API spec's Q8: a model's float,
//! rounded once where it leaves), by Newton's step inside a bracket; a
//! price from a yield is exact where the method is (Treasury's), else f64.

use meridian_domain::date::Date;

use super::dates::{add_months, days};
use super::daycount::DayCount;
use super::schedule::{Flow, Schedule};
use crate::exact::big::Big;
use crate::exact::{Dec, Ratio};

/// How a bond's price and yield are related.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// 31 CFR 356 Appendix B: notes, bonds and STRIPS.
    Treasury,
    /// Appendix B, section III: a bill's discount and investment rates.
    Bill,
    /// SIFMA's standard formula; MSRB G-33's for a municipal.
    Street,
}

/// The half-year grid a bond's flows are discounted on: its periods a
/// year (two for a zero coupon), anchored at maturity.
#[derive(Debug, Clone, Copy)]
pub struct Grid {
    pub maturity: Date,
    pub frequency: u32,
    pub end_of_month: bool,
    pub day_count: DayCount,
}

impl Grid {
    pub fn of(schedule: &Schedule) -> Grid {
        Grid {
            maturity: schedule.maturity,
            frequency: if schedule.frequency == 0 {
                2
            } else {
                schedule.frequency
            },
            end_of_month: schedule.end_of_month,
            day_count: schedule.day_count,
        }
    }

    fn months(&self) -> i32 {
        12 / self.frequency as i32
    }

    fn at(&self, k: i32) -> Date {
        add_months(self.maturity, -k * self.months(), self.end_of_month)
    }

    /// The grid's period around `date`: the last grid date on or before it
    /// and the first after it.
    pub fn around(&self, date: Date) -> (Date, Date) {
        // From maturity back to the first grid date on or before `date`.
        let mut k = 0;
        while self.at(k) > date && k < 4_000 {
            k += 1;
        }
        if k == 0 {
            // At or past maturity: the period after it, forward.
            let mut j = 0;
            while add_months(self.maturity, j * self.months(), self.end_of_month) <= date
                && j < 4_000
            {
                j += 1;
            }
            return (
                add_months(self.maturity, (j - 1) * self.months(), self.end_of_month),
                add_months(self.maturity, j * self.months(), self.end_of_month),
            );
        }
        (self.at(k), self.at(k - 1))
    }

    /// A period's length as the day count counts a coupon period: 360 / m
    /// on the 30/360 family, its actual days otherwise.
    fn length(&self, start: Date, end: Date) -> f64 {
        if self.day_count.is_thirty() {
            360.0 / f64::from(self.frequency)
        } else {
            days(start, end) as f64
        }
    }

    /// The days from `from` to `to` as the day count counts them.
    fn counted(&self, from: Date, to: Date) -> f64 {
        if self.day_count.is_thirty() {
            self.day_count.days(from, to, Some(self.maturity)) as f64
        } else {
            days(from, to) as f64
        }
    }

    /// Periods from `settle` to `date`: the fraction of the current period
    /// left, whole periods, and the fraction into the last.
    pub fn periods(&self, settle: Date, date: Date) -> f64 {
        let (prev, next) = self.around(settle);
        if date <= next {
            return self.counted(settle, date) / self.length(prev, next);
        }
        let mut total = self.counted(settle, next) / self.length(prev, next);
        let mut at = next;
        loop {
            let (_, after) = self.around(at);
            if after >= date {
                return total + self.counted(at, date) / self.length(at, after);
            }
            total += 1.0;
            at = after;
        }
    }
}

/// A bond's flows from a settlement date, with how they are discounted.
#[derive(Debug, Clone)]
pub struct Priceable {
    pub method: Method,
    pub grid: Grid,
    pub settle: Date,
    pub flows: Vec<Flow>,
    /// Accrued interest per 100 at settlement, exact.
    pub accrued: Ratio,
}

fn f(r: &Ratio) -> f64 {
    r.to_f64()
}

impl Priceable {
    /// Each flow's amount and its time in periods from settlement.
    fn timed(&self) -> Vec<(f64, f64)> {
        self.flows
            .iter()
            .map(|flow| (f(&flow.amount), self.grid.periods(self.settle, flow.date)))
            .collect()
    }

    /// Whether one flow is left within the current period: G-33(b)(i)(A)'s
    /// simple interest.
    fn simple(&self) -> bool {
        self.flows.len() == 1 && {
            let (_, next) = self.grid.around(self.settle);
            self.flows[0].date <= next
        }
    }

    /// The dirty price per 100 at a yield (a decimal fraction, 0.05 for 5
    /// per cent), in f64.
    pub fn dirty_at(&self, y: f64) -> f64 {
        let m = f64::from(self.grid.frequency);
        match self.method {
            Method::Bill => {
                let t = days(
                    self.settle,
                    self.flows.last().map(|f| f.date).unwrap_or(self.settle),
                ) as f64;
                let amount = self.flows.iter().map(|flow| f(&flow.amount)).sum::<f64>();
                bill_price_from_investment_rate(amount, y, t, self.basis_days())
            }
            Method::Treasury => {
                let (prev, next) = self.grid.around(self.settle);
                let rs = days(self.settle, next) as f64 / days(prev, next) as f64;
                let mut sum = 0.0;
                for (amount, periods) in self.timed() {
                    let whole = (periods - rs).round();
                    sum += amount / (1.0 + y / m).powf(whole);
                }
                sum / (1.0 + rs * y / m)
            }
            Method::Street => {
                if self.simple() {
                    let (amount, periods) = self.timed()[0];
                    return amount / (1.0 + periods * y / m);
                }
                self.timed()
                    .into_iter()
                    .map(|(amount, periods)| amount / (1.0 + y / m).powf(periods))
                    .sum()
            }
        }
    }

    /// Treasury's dirty price at a yield in percent, exact (Appendix B);
    /// None where the flows are off the half-year grid.
    pub fn treasury_dirty_exact(&self, yield_percent: &Ratio) -> Option<Ratio> {
        let m = self.grid.frequency;
        let (prev, next) = self.grid.around(self.settle);
        let one = Ratio::new(Big::from_i128(1), Big::from_i128(1));
        let per = yield_percent.div_small(100).div_small(m);
        let step = one.add(&per);
        let rs_num = days(self.settle, next);
        let rs_den = days(prev, next);
        let rs = Ratio::new(
            Big::from_i128(i128::from(rs_num)),
            Big::from_i128(i128::from(rs_den)),
        );
        let mut sum = Ratio::zero();
        for flow in &self.flows {
            let whole = self.grid.periods(self.settle, flow.date) - rs_num as f64 / rs_den as f64;
            if (whole - whole.round()).abs() > 1e-9 {
                return None;
            }
            let mut discount = one.clone();
            for _ in 0..(whole.round() as i64) {
                discount = discount.mul(&step);
            }
            sum = sum.add(&flow.amount.div(&discount));
        }
        Some(sum.div(&one.add(&rs.mul(&per))))
    }

    fn basis_days(&self) -> f64 {
        // 366 where a February 29th falls in the year from settlement.
        let year_on = add_months(self.settle, 12, false);
        let leap = (self.settle.year()..=year_on.year()).any(|year| {
            Date::from_ymd(year, 2, 29).is_some_and(|d| d > self.settle && d <= year_on)
        });
        if leap {
            366.0
        } else {
            365.0
        }
    }

    /// The yield (a decimal fraction) a dirty price per 100 is at; None
    /// where no yield in (-50%, 200%) prices it.
    pub fn yield_of(&self, dirty: f64) -> Option<f64> {
        if self.flows.is_empty() || dirty.is_nan() || dirty <= 0.0 {
            return None;
        }
        let (mut lo, mut hi) = (-0.5, 2.0);
        let g = |y: f64| self.dirty_at(y) - dirty;
        if g(lo).signum() == g(hi).signum() {
            return None;
        }
        let mut y = 0.05;
        for _ in 0..200 {
            let value = g(y);
            if value.abs() < 1e-12 {
                return Some(y);
            }
            if value > 0.0 {
                lo = y;
            } else {
                hi = y;
            }
            let h = 1e-7;
            let slope = (g(y + h) - g(y - h)) / (2.0 * h);
            let newton = y - value / slope;
            y = if slope.is_finite() && slope != 0.0 && newton > lo && newton < hi {
                newton
            } else {
                0.5 * (lo + hi)
            };
            if hi - lo < 1e-15 {
                break;
            }
        }
        Some(y)
    }

    /// Macaulay duration in years, modified duration, and convexity in
    /// years squared, at a yield (a fraction) and its dirty price.
    pub fn risk(&self, y: f64) -> (f64, f64, f64) {
        let m = f64::from(self.grid.frequency);
        let price = self.dirty_at(y);
        if self.method == Method::Bill || self.simple() {
            let (_, periods) = self.timed().last().copied().unwrap_or((0.0, 0.0));
            let years = periods / m;
            let modified = years / (1.0 + periods * y / m);
            return (years, modified, 2.0 * modified * modified);
        }
        let mut weighted = 0.0;
        let mut curved = 0.0;
        for (amount, periods) in self.timed() {
            let pv = amount / (1.0 + y / m).powf(periods);
            weighted += periods * pv;
            curved += periods * (periods + 1.0) * pv;
        }
        let macaulay = weighted / price / m;
        let modified = macaulay / (1.0 + y / m);
        let convexity = curved / price / (m * m) / (1.0 + y / m).powi(2);
        (macaulay, modified, convexity)
    }
}

/// A bill's price from its bank discount rate (a fraction) over `t` days:
/// `100 (1 - d t / 360)`, exact.
pub fn bill_price_from_discount(discount_percent: &Ratio, t: i64) -> Ratio {
    let hundred = Ratio::new(Big::from_i128(100), Big::from_i128(1));
    let taken = discount_percent.mul(&Ratio::new(
        Big::from_i128(i128::from(t)),
        Big::from_i128(360),
    ));
    hundred.sub(&taken)
}

/// A bill's price at an investment rate (Appendix B, section III): simple
/// over a half-year or less, the half-year compounded once past it.
fn bill_price_from_investment_rate(amount: f64, i: f64, t: f64, basis: f64) -> f64 {
    if t <= basis / 2.0 {
        amount / (1.0 + i * t / basis)
    } else {
        amount / ((1.0 + i / 2.0) * (1.0 + (t / basis - 0.5) * i))
    }
}

/// Rounded half up to `scale` places (Appendix B's rounding).
pub fn round_half_up(value: &Ratio, scale: u32) -> Dec {
    value.round_half_up(scale)
}

/// MSRB Rule G-33's yield in percent: truncated to four places, then
/// rounded to three.
pub fn g33_yield(percent: &Dec) -> Dec {
    percent.to_ratio().truncate(4).to_ratio().round_half_up(3)
}
