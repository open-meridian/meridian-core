//! A debt record's schedule and cash flows (`bond_cash_flows` v1; contract
//! v29; W12.3; ENG1 §3 "Cash flows from the template's calculation package,
//! which is exact").
//!
//! The coupon dates run back from maturity by the frequency's months, each
//! date the maturity's day of month (its month's last day where shorter, or
//! always where the terms' `end_of_month` holds), down to the first coupon
//! date, which opens no earlier regular date: a first period from the dated
//! date that is short or long is the dated date's to the first coupon's. A
//! period's coupon per 100 of face is the rate over the frequency on a
//! regular period, the rate times its year fraction otherwise -- a first
//! period's on actual/actual (ICMA) counted over its quasi-coupon periods.
//! Accrual and discounting run on the unadjusted dates (the street's
//! convention); a payment's own date rolls on the payment calendar.

use meridian_domain::date::Date;

use super::dates::{add_months, days, Calendar, Roll};
use super::daycount::{ratio, DayCount};
use crate::exact::big::Big;
use crate::exact::{Dec, Ratio};

/// One coupon period: its accrual dates, its payment date rolled, its
/// annual rate in percent and its coupon per 100 of face.
#[derive(Debug, Clone)]
pub struct Period {
    pub start: Date,
    pub end: Date,
    pub paid: Date,
    pub rate: Ratio,
    pub coupon: Ratio,
    /// Whether it is a whole regular period.
    pub regular: bool,
}

/// A fixed, step or zero coupon schedule.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub periods: Vec<Period>,
    pub maturity: Date,
    pub frequency: u32,
    pub day_count: DayCount,
    pub end_of_month: bool,
    /// The date interest accrues from: the dated date, else the issue date.
    pub accrues_from: Date,
}

/// What a schedule is built from.
#[derive(Debug, Clone)]
pub struct Shape {
    pub accrues_from: Date,
    pub first_coupon: Option<Date>,
    pub maturity: Date,
    pub frequency: u32,
    pub day_count: DayCount,
    pub end_of_month: bool,
    /// The annual rate in percent; each step a rate from its date.
    pub rate: Ratio,
    pub steps: Vec<(Date, Ratio)>,
    pub calendar: Calendar,
    pub roll: Roll,
}

/// A decimal as an exact ratio.
pub fn exact(value: &Dec) -> Ratio {
    value.to_ratio()
}

/// The months between regular coupon dates.
fn months(frequency: u32) -> i32 {
    12 / frequency.max(1) as i32
}

impl Shape {
    fn regular(&self, k: i32) -> Date {
        add_months(
            self.maturity,
            -k * months(self.frequency),
            self.end_of_month,
        )
    }

    /// The rate in force over a period from `start`.
    fn rate_at(&self, start: Date) -> Ratio {
        self.steps
            .iter()
            .filter(|(from, _)| *from <= start)
            .max_by_key(|(from, _)| *from)
            .map(|(_, rate)| rate.clone())
            .unwrap_or_else(|| self.rate.clone())
    }

    pub fn build(&self) -> Schedule {
        let mut ends: Vec<Date> = Vec::new();
        if self.frequency > 0 {
            let floor = self.first_coupon.unwrap_or(self.accrues_from);
            let mut k = 0;
            loop {
                let date = self.regular(k);
                if date <= floor || date <= self.accrues_from {
                    break;
                }
                ends.push(date);
                k += 1;
            }
            if let Some(first) = self.first_coupon {
                if first > self.accrues_from && !ends.contains(&first) {
                    ends.push(first);
                }
            }
            ends.sort();
        } else if self.maturity > self.accrues_from {
            // Paid at maturity: one period, dated to maturity.
            ends.push(self.maturity);
        }
        let mut periods = Vec::with_capacity(ends.len());
        let mut start = self.accrues_from;
        for end in ends {
            let rate = self.rate_at(start);
            let regular = self.frequency > 0 && self.regular_back(end) == start;
            let coupon = if rate.signum() == 0 {
                Ratio::zero()
            } else if regular && self.whole_period_pays_rate_over_frequency() {
                rate.div_small(self.frequency)
            } else {
                rate.mul(&self.fraction(start, end, end))
            };
            periods.push(Period {
                start,
                end,
                paid: self.calendar.adjust(end, self.roll),
                rate,
                coupon,
                regular,
            });
            start = end;
        }
        Schedule {
            periods,
            maturity: self.maturity,
            frequency: self.frequency,
            day_count: self.day_count,
            end_of_month: self.end_of_month,
            accrues_from: self.accrues_from,
        }
    }

    /// The regular date before `end` on the maturity's schedule.
    fn regular_back(&self, end: Date) -> Date {
        let mut k = 0;
        while self.regular(k) > end {
            k += 1;
        }
        if self.regular(k) == end {
            self.regular(k + 1)
        } else {
            add_months(end, -months(self.frequency), self.end_of_month)
        }
    }

    /// Whether a whole regular period pays exactly the rate over the
    /// frequency: on actual/actual (ICMA) and the 30/360 family.
    fn whole_period_pays_rate_over_frequency(&self) -> bool {
        self.day_count == DayCount::ActActIcma || self.day_count.is_thirty()
    }

    /// The year fraction from `from` to `to` within the period ending
    /// `anchor`: on actual/actual (ICMA) summed over the quasi-coupon
    /// periods ending at `anchor` (its first period short or long), the day
    /// count's own fraction otherwise.
    pub fn fraction(&self, from: Date, to: Date, anchor: Date) -> Ratio {
        if self.day_count != DayCount::ActActIcma || self.frequency == 0 {
            return self.day_count.fraction(from, to, None, Some(self.maturity));
        }
        let mut total = Ratio::zero();
        let mut end = anchor;
        let mut guard = 0;
        while end > from && guard < 1_000 {
            let start = add_months(end, -months(self.frequency), self.end_of_month);
            let lo = if start > from { start } else { from };
            let hi = if end < to { end } else { to };
            if hi > lo {
                total = total.add(&ratio(
                    days(lo, hi),
                    days(start, end) * i64::from(self.frequency),
                ));
            }
            end = start;
            guard += 1;
        }
        total
    }
}

impl Schedule {
    /// The period a settlement date falls in: the one it is on or after the
    /// start of and before the end of. None past maturity or before the
    /// accrual starts.
    pub fn period_at(&self, settle: Date) -> Option<usize> {
        self.periods
            .iter()
            .position(|p| p.start <= settle && settle < p.end)
    }
}

/// Accrued interest per 100 of face at `settle`, exact: the period's rate
/// times its year fraction from its start, the day count's.
pub fn accrued(shape: &Shape, schedule: &Schedule, settle: Date) -> Ratio {
    let Some(i) = schedule.period_at(settle) else {
        return Ratio::zero();
    };
    let period = &schedule.periods[i];
    if period.coupon.signum() == 0 {
        return Ratio::zero();
    }
    if period.regular && shape.whole_period_pays_rate_over_frequency() {
        // A whole period's coupon times the part of the period run, as the
        // day count counts it: actual days over the period's on ICMA, 30/360
        // days over 360 / frequency on the 30/360 family.
        let run = shape
            .day_count
            .days(period.start, settle, Some(shape.maturity));
        let whole = if shape.day_count.is_thirty() {
            360 / i64::from(shape.frequency.max(1))
        } else {
            days(period.start, period.end)
        };
        return period.coupon.mul(&ratio(run, whole));
    }
    period
        .rate
        .mul(&shape.fraction(period.start, settle, period.end))
}

/// One cash flow per 100 of face: its date (unadjusted) and amount.
#[derive(Debug, Clone)]
pub struct Flow {
    pub date: Date,
    pub amount: Ratio,
}

/// The flows after `settle` to a redemption on `on` at `price` per 100:
/// each coupon whose period ends by then, a period cut short at a
/// redemption inside it paying its interest to that date, and the price.
pub fn flows_to(
    shape: &Shape,
    schedule: &Schedule,
    settle: Date,
    on: Date,
    price: &Ratio,
) -> Vec<Flow> {
    let mut out = Vec::new();
    for period in &schedule.periods {
        if period.end <= settle {
            continue;
        }
        if period.end <= on {
            if period.coupon.signum() != 0 {
                out.push(Flow {
                    date: period.end,
                    amount: period.coupon.clone(),
                });
            }
            continue;
        }
        if period.start < on {
            let part = period
                .rate
                .mul(&shape.fraction(period.start, on, period.end));
            if part.signum() != 0 {
                out.push(Flow {
                    date: on,
                    amount: part,
                });
            }
        }
        break;
    }
    match out.last_mut() {
        Some(last) if last.date == on => last.amount = last.amount.add(price),
        _ => out.push(Flow {
            date: on,
            amount: price.clone(),
        }),
    }
    out
}

/// One hundred, exact.
pub fn hundred() -> Ratio {
    Ratio::new(Big::from_i128(100), Big::from_i128(1))
}
