//! The Treasury curve (`monotone_convex` v1; contract v29; W12.1, W12.3;
//! ENG1 §3 "Treasury curve fit"): a calibration node over the par yields
//! the lake holds, producing par, zero and discount curves at a date.
//!
//! **The interpolation** is Hagan and West's monotone convex method
//! ("Interpolation methods for curve construction", Applied Mathematical
//! Finance 13(2), 2006, section 4), Treasury's own for its par curve (its
//! yield curve methodology): the instantaneous forward is piecewise
//! quadratic between the knots, each interval's average the discrete
//! forward, positive and monotone where the discrete forwards are, and its
//! integral the knot's `r(t) t` exactly.
//!
//! **The fit**: each tenor's par yield is a bond issued at par on the
//! curve's date, maturing the tenor on, paying half its par yield twice a
//! year on a half-year grid from maturity (a first period short, on
//! actual/actual (ICMA)); the knots' zero rates are found so every such
//! bond prices to 100, a pass over the knots repeated until none moves by
//! more than 1e-13 -- the interpolation between knots reads its neighbours,
//! so one pass is not enough. Time is actual/365 fixed from the date, and
//! zero rates are continuously compounded, as the Board's zero curve
//! (`fed_zero`) states its own.

use meridian_domain::date::Date;

use super::dates::{add_months, days, plus_days};
use super::daycount::DayCount;
use super::schedule::Shape;

/// One par point: its tenor's label, its maturity and its par yield in
/// percent, bond-equivalent.
#[derive(Debug, Clone)]
pub struct ParPoint {
    pub tenor: String,
    pub maturity: Date,
    pub par: f64,
}

/// A tenor's maturity from a date: months and years by calendar months,
/// weeks and days by days.
pub fn maturity_of(date: Date, count: i32, unit: &str) -> Option<Date> {
    match unit {
        "TENOR_UNIT_MONTH" => Some(add_months(date, count, false)),
        "TENOR_UNIT_YEAR" => Some(add_months(date, 12 * count, false)),
        "TENOR_UNIT_WEEK" => Some(plus_days(date, 7 * i64::from(count))),
        "TENOR_UNIT_DAY" => Some(plus_days(date, i64::from(count))),
        _ => None,
    }
}

/// A tenor's label: `10Y`, `3M`, `6W`.
pub fn label(count: i32, unit: &str) -> String {
    let letter = match unit {
        "TENOR_UNIT_MONTH" => "M",
        "TENOR_UNIT_YEAR" => "Y",
        "TENOR_UNIT_WEEK" => "W",
        "TENOR_UNIT_DAY" => "D",
        _ => "",
    };
    format!("{count}{letter}")
}

/// Hagan and West's monotone convex interpolation over knots `t` (from 0)
/// and the integrated forward `r(t) t` at each.
#[derive(Debug, Clone)]
pub struct MonotoneConvex {
    t: Vec<f64>,
    rt: Vec<f64>,
    fd: Vec<f64>,
    f: Vec<f64>,
}

impl MonotoneConvex {
    /// Over knots `t_1 < ... < t_n` (each > 0) and their zero rates, the
    /// knot at 0 added.
    pub fn new(times: &[f64], zeros: &[f64]) -> MonotoneConvex {
        let mut t = vec![0.0];
        let mut rt = vec![0.0];
        for (time, zero) in times.iter().zip(zeros) {
            t.push(*time);
            rt.push(zero * time);
        }
        let n = t.len() - 1;
        let mut fd = vec![0.0; n + 1];
        for i in 1..=n {
            fd[i] = (rt[i] - rt[i - 1]) / (t[i] - t[i - 1]);
        }
        let mut f = vec![0.0; n + 1];
        if n == 1 {
            f[0] = fd[1];
            f[1] = fd[1];
        } else {
            for i in 1..n {
                f[i] = (t[i] - t[i - 1]) / (t[i + 1] - t[i - 1]) * fd[i + 1]
                    + (t[i + 1] - t[i]) / (t[i + 1] - t[i - 1]) * fd[i];
            }
            f[0] = fd[1] - 0.5 * (f[1] - fd[1]);
            f[n] = fd[n] - 0.5 * (f[n - 1] - fd[n]);
        }
        MonotoneConvex { t, rt, fd, f }
    }

    /// `g` and its integral `G` on an interval at `x` in [0, 1], from the
    /// interval's end values `g0`, `g1` (section 4's four sectors).
    fn g(g0: f64, g1: f64, x: f64) -> (f64, f64) {
        if g0 == 0.0 && g1 == 0.0 {
            return (0.0, 0.0);
        }
        let sector_i = (g0 < 0.0 && -0.5 * g0 <= g1 && g1 <= -2.0 * g0)
            || (g0 > 0.0 && -0.5 * g0 >= g1 && g1 >= -2.0 * g0);
        let sector_ii = (g0 < 0.0 && g1 > -2.0 * g0) || (g0 > 0.0 && g1 < -2.0 * g0);
        let sector_iii =
            (g0 > 0.0 && 0.0 > g1 && g1 > -0.5 * g0) || (g0 < 0.0 && 0.0 < g1 && g1 < -0.5 * g0);
        if sector_i {
            let g = g0 * (1.0 - 4.0 * x + 3.0 * x * x) + g1 * (-2.0 * x + 3.0 * x * x);
            let big = g0 * (x - 2.0 * x * x + x * x * x) + g1 * (-x * x + x * x * x);
            (g, big)
        } else if sector_ii {
            let eta = (g1 + 2.0 * g0) / (g1 - g0);
            if x <= eta {
                (g0, g0 * x)
            } else {
                let s = (x - eta) / (1.0 - eta);
                (
                    g0 + (g1 - g0) * s * s,
                    g0 * x + (g1 - g0) * (x - eta).powi(3) / (3.0 * (1.0 - eta).powi(2)),
                )
            }
        } else if sector_iii {
            let eta = 3.0 * g1 / (g1 - g0);
            if x < eta {
                let s = (eta - x) / eta;
                (
                    g1 + (g0 - g1) * s * s,
                    g1 * x + (g0 - g1) * (eta - (eta - x).powi(3) / (eta * eta)) / 3.0,
                )
            } else {
                (g1, g1 * x + (g0 - g1) * eta / 3.0)
            }
        } else {
            // Sector (iv): the same sign at both ends.
            let eta = g1 / (g1 + g0);
            let a = -g0 * g1 / (g0 + g1);
            if x <= eta {
                let s = (eta - x) / eta;
                (
                    a + (g0 - a) * s * s,
                    a * x + (g0 - a) * (eta - (eta - x).powi(3) / (eta * eta)) / 3.0,
                )
            } else {
                let s = (x - eta) / (1.0 - eta);
                (
                    a + (g1 - a) * s * s,
                    a * x
                        + (g0 - a) * eta / 3.0
                        + (g1 - a) * (x - eta).powi(3) / (3.0 * (1.0 - eta).powi(2)),
                )
            }
        }
    }

    /// The integrated forward `r(t) t` at `time`: past the last knot, flat
    /// at the last knot's instantaneous forward.
    pub fn integral(&self, time: f64) -> f64 {
        let n = self.t.len() - 1;
        if time <= 0.0 {
            return 0.0;
        }
        if time >= self.t[n] {
            return self.rt[n] + self.f[n] * (time - self.t[n]);
        }
        let i = self.t.partition_point(|knot| *knot < time).max(1);
        let width = self.t[i] - self.t[i - 1];
        let x = (time - self.t[i - 1]) / width;
        let (_, big) = Self::g(self.f[i - 1] - self.fd[i], self.f[i] - self.fd[i], x);
        self.rt[i - 1] + width * (self.fd[i] * x + big)
    }

    /// The instantaneous forward at `time`.
    pub fn forward(&self, time: f64) -> f64 {
        let n = self.t.len() - 1;
        if time >= self.t[n] {
            return self.f[n];
        }
        let i = self.t.partition_point(|knot| *knot < time).max(1);
        let x = (time - self.t[i - 1]) / (self.t[i] - self.t[i - 1]);
        let (g, _) = Self::g(self.f[i - 1] - self.fd[i], self.f[i] - self.fd[i], x);
        self.fd[i] + g
    }
}

/// A fitted curve at a date.
#[derive(Debug, Clone)]
pub struct Curve {
    pub date: Date,
    pub points: Vec<ParPoint>,
    pub times: Vec<f64>,
    pub zeros: Vec<f64>,
    pub interpolation: MonotoneConvex,
}

/// Years from `from` to `to`, actual/365 fixed.
pub fn years(from: Date, to: Date) -> f64 {
    days(from, to) as f64 / 365.0
}

/// A par bond's flows from `date` to `maturity` at a coupon of `percent`,
/// half of it a whole half-year: each flow's date and amount per 100.
pub fn par_flows(date: Date, maturity: Date, percent: f64) -> Vec<(Date, f64)> {
    let shape = Shape {
        accrues_from: date,
        first_coupon: None,
        maturity,
        frequency: 2,
        day_count: DayCount::ActActIcma,
        end_of_month: false,
        rate: crate::exact::Ratio::zero(),
        steps: Vec::new(),
        calendar: Default::default(),
        roll: super::dates::Roll::Unadjusted,
    };
    let schedule = shape.build();
    let mut out: Vec<(Date, f64)> = schedule
        .periods
        .iter()
        .map(|p| {
            let fraction = shape.fraction(p.start, p.end, p.end).to_f64();
            (p.end, percent * fraction)
        })
        .collect();
    match out.last_mut() {
        Some(last) if last.0 == maturity => last.1 += 100.0,
        _ => out.push((maturity, 100.0)),
    }
    out
}

impl Curve {
    /// The discount factor to `on`.
    pub fn discount(&self, on: Date) -> f64 {
        (-self.interpolation.integral(years(self.date, on))).exp()
    }

    /// The continuously compounded zero rate to `on`.
    pub fn zero(&self, on: Date) -> f64 {
        let t = years(self.date, on);
        if t <= 0.0 {
            return self.interpolation.forward(0.0);
        }
        self.interpolation.integral(t) / t
    }

    /// The par yield in percent, bond-equivalent, of a bond issued on the
    /// curve's date maturing `on`.
    pub fn par(&self, on: Date) -> f64 {
        let mut annuity = 0.0;
        let mut principal = 0.0;
        for (date, amount) in par_flows(self.date, on, 1.0) {
            let d = self.discount(date);
            if date == on {
                annuity += (amount - 100.0) * d;
                principal = 100.0 * d;
            } else {
                annuity += amount * d;
            }
        }
        (100.0 - principal) / annuity
    }

    fn price(&self, flows: &[(Date, f64)]) -> f64 {
        flows
            .iter()
            .map(|(date, amount)| amount * self.discount(*date))
            .sum()
    }

    /// The curve through the par yields at `date`; None where they cannot
    /// be fitted (no point, a maturity on or before the date, or no
    /// convergence).
    pub fn fit(date: Date, mut points: Vec<ParPoint>) -> Option<Curve> {
        points.sort_by_key(|p| p.maturity);
        points.dedup_by_key(|p| p.maturity);
        if points.is_empty() || points[0].maturity <= date {
            return None;
        }
        let times: Vec<f64> = points.iter().map(|p| years(date, p.maturity)).collect();
        let flows: Vec<Vec<(Date, f64)>> = points
            .iter()
            .map(|p| par_flows(date, p.maturity, p.par))
            .collect();
        // A start: each par yield, continuous.
        let mut zeros: Vec<f64> = points
            .iter()
            .map(|p| 2.0 * (1.0 + p.par / 200.0).ln())
            .collect();
        let mut curve = Curve {
            date,
            points: points.clone(),
            times: times.clone(),
            zeros: zeros.clone(),
            interpolation: MonotoneConvex::new(&times, &zeros),
        };
        for _pass in 0..500 {
            let mut moved: f64 = 0.0;
            for i in 0..points.len() {
                // Newton on the i-th knot, the others held.
                for _ in 0..50 {
                    curve.interpolation = MonotoneConvex::new(&times, &zeros);
                    let error = curve.price(&flows[i]) - 100.0;
                    let h = 1e-7;
                    zeros[i] += h;
                    curve.interpolation = MonotoneConvex::new(&times, &zeros);
                    let bumped = curve.price(&flows[i]) - 100.0;
                    zeros[i] -= h;
                    let slope = (bumped - error) / h;
                    if slope == 0.0 || !slope.is_finite() {
                        return None;
                    }
                    let step = error / slope;
                    zeros[i] -= step;
                    moved = moved.max(step.abs());
                    if step.abs() < 1e-15 {
                        break;
                    }
                }
            }
            if moved < 1e-13 {
                curve.interpolation = MonotoneConvex::new(&times, &zeros);
                curve.zeros = zeros;
                return Some(curve);
            }
        }
        None
    }

    /// The same fit with one tenor's par yield moved by `bp` basis points
    /// (a key rate's bump, through the graph, 2026-10-12's ruling).
    pub fn bumped(&self, tenor: usize, bp: f64) -> Option<Curve> {
        let mut points = self.points.clone();
        points[tenor].par += bp / 100.0;
        Curve::fit(self.date, points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_par_curve_fits_flat_and_reprices_its_points() {
        let date = Date::parse("2026-10-16").unwrap();
        let points: Vec<ParPoint> = [
            (3, "TENOR_UNIT_MONTH"),
            (2, "TENOR_UNIT_YEAR"),
            (10, "TENOR_UNIT_YEAR"),
            (30, "TENOR_UNIT_YEAR"),
        ]
        .iter()
        .map(|(count, unit)| ParPoint {
            tenor: label(*count, unit),
            maturity: maturity_of(date, *count, unit).unwrap(),
            par: 4.0,
        })
        .collect();
        let curve = Curve::fit(date, points.clone()).unwrap();
        for p in &points {
            assert!((curve.par(p.maturity) - 4.0).abs() < 1e-10, "{}", p.tenor);
        }
        // A flat 4% semi-annual par curve is a flat 2 ln(1.02) zero curve
        // beyond its first half-year.
        let ten = maturity_of(date, 10, "TENOR_UNIT_YEAR").unwrap();
        assert!((curve.zero(ten) - 2.0 * 1.02f64.ln()).abs() < 2e-4);
    }
}
