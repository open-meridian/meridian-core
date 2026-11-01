//! Day counts, exact (contract v29; ruling 678: FIX CouponDayCount (1950)'s
//! values, each by its variant; R1).
//!
//! A day count gives a period's year fraction as an exact ratio, so accrued
//! interest is exact before its one rounding (decisions/023). Actual/actual
//! (ICMA) reads the coupon period the days fall in, and so takes the
//! schedule's reference period; every other reads the dates alone.
//!
//! - 30/360 bond basis (FIX 1): MSRB Rule G-33(e)'s count -- D1 31 becomes
//!   30; D2 31 becomes 30 where D1 is 30 or 31;
//! - 30/360 (SIA) (FIX 2): the bond basis with SIFMA's February rule, D1 the
//!   last of February becoming 30, and D2 too where D1 was;
//! - 30/360M (FIX 3): counted as the bond basis, the municipal market's
//!   (G-33(e));
//! - 30E/360 (FIX 4), 30E/360 (ISDA) (FIX 5) and 30E+/360 (FIX 13);
//! - actual over 360, 364 and 365 fixed, NL/360 and NL/365 (February 29th
//!   left out), actual/365L, and actual/actual (ISDA), by calendar year;
//! - 1/1: one.
//!
//! Not computed in v29, answered as such: BUS/252 (it needs a calendar's
//! business days), actual/actual (AFB) and (ICMA ultimo), and the two
//! values naming no variant, which a record never holds (678).

use meridian_domain::date::Date;

use super::dates::{days, days_in, is_month_end};
use crate::exact::big::Big;
use crate::exact::Ratio;

/// A day count by its contract value's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayCount {
    Thirty360BondBasis,
    Thirty360Sia,
    Thirty360M,
    ThirtyE360,
    ThirtyE360Isda,
    ThirtyEPlus360,
    Act360,
    Act364,
    Act365Fixed,
    Act365L,
    Nl360,
    Nl365,
    ActActIcma,
    ActActIsda,
    OneOne,
}

/// Why a day count is not computed.
pub fn not_computed(name: &str) -> String {
    format!("day_count: {name} is not computed by bond_cash_flows v1")
}

impl DayCount {
    /// The day count a terms' name states, or why it is not computed.
    pub fn named(name: &str) -> Result<DayCount, String> {
        Ok(match name {
            "DAY_COUNT_THIRTY_360_BOND_BASIS" => DayCount::Thirty360BondBasis,
            "DAY_COUNT_THIRTY_360_SIA" => DayCount::Thirty360Sia,
            "DAY_COUNT_THIRTY_360_M" => DayCount::Thirty360M,
            "DAY_COUNT_THIRTY_E_360" => DayCount::ThirtyE360,
            "DAY_COUNT_THIRTY_E_360_ISDA" => DayCount::ThirtyE360Isda,
            "DAY_COUNT_THIRTY_E_PLUS_360" => DayCount::ThirtyEPlus360,
            "DAY_COUNT_ACT_360" => DayCount::Act360,
            "DAY_COUNT_ACT_364" => DayCount::Act364,
            "DAY_COUNT_ACT_365_FIXED" => DayCount::Act365Fixed,
            "DAY_COUNT_ACT_365_L" => DayCount::Act365L,
            "DAY_COUNT_NL_360" => DayCount::Nl360,
            "DAY_COUNT_NL_365" => DayCount::Nl365,
            "DAY_COUNT_ACT_ACT_ICMA" => DayCount::ActActIcma,
            "DAY_COUNT_ACT_ACT_ISDA" => DayCount::ActActIsda,
            "DAY_COUNT_ONE_ONE" => DayCount::OneOne,
            "" => return Err("day_count".to_string()),
            other => return Err(not_computed(other)),
        })
    }

    /// Whether the count is one of the 30/360 family, whose year is 360
    /// days of twelve 30-day months.
    pub fn is_thirty(self) -> bool {
        matches!(
            self,
            DayCount::Thirty360BondBasis
                | DayCount::Thirty360Sia
                | DayCount::Thirty360M
                | DayCount::ThirtyE360
                | DayCount::ThirtyE360Isda
                | DayCount::ThirtyEPlus360
        )
    }

    /// The days from `start` to `end` as the count counts them: 30/360's
    /// days for the 30/360 family, leap days left out for NL, actual days
    /// otherwise. `maturity` is the bond's, for 30E/360 (ISDA)'s February
    /// rule.
    pub fn days(self, start: Date, end: Date, maturity: Option<Date>) -> i64 {
        let (y1, m1, mut d1) = (
            i64::from(start.year()),
            i64::from(start.month()),
            i64::from(start.day()),
        );
        let (mut y2, mut m2, mut d2) = (
            i64::from(end.year()),
            i64::from(end.month()),
            i64::from(end.day()),
        );
        let feb_end = |date: Date| date.month() == 2 && is_month_end(date);
        let thirty = |y1: i64, m1: i64, d1: i64, y2: i64, m2: i64, d2: i64| {
            360 * (y2 - y1) + 30 * (m2 - m1) + (d2 - d1)
        };
        match self {
            DayCount::Thirty360BondBasis | DayCount::Thirty360M => {
                if d1 == 31 {
                    d1 = 30;
                }
                if d2 == 31 && d1 == 30 {
                    d2 = 30;
                }
                thirty(y1, m1, d1, y2, m2, d2)
            }
            DayCount::Thirty360Sia => {
                if feb_end(start) && feb_end(end) {
                    d2 = 30;
                }
                if feb_end(start) {
                    d1 = 30;
                }
                if d2 == 31 && d1 >= 30 {
                    d2 = 30;
                }
                if d1 == 31 {
                    d1 = 30;
                }
                thirty(y1, m1, d1, y2, m2, d2)
            }
            DayCount::ThirtyE360 => {
                d1 = d1.min(30);
                d2 = d2.min(30);
                thirty(y1, m1, d1, y2, m2, d2)
            }
            DayCount::ThirtyE360Isda => {
                if is_month_end(start) {
                    d1 = 30;
                }
                if is_month_end(end) && !(Some(end) == maturity && end.month() == 2) {
                    d2 = 30;
                }
                thirty(y1, m1, d1, y2, m2, d2)
            }
            DayCount::ThirtyEPlus360 => {
                if d1 == 31 {
                    d1 = 30;
                }
                if d2 == 31 {
                    d2 = 1;
                    m2 += 1;
                    if m2 == 13 {
                        m2 = 1;
                        y2 += 1;
                    }
                }
                thirty(y1, m1, d1, y2, m2, d2)
            }
            DayCount::Nl360 | DayCount::Nl365 => days(start, end) - leap_days(start, end),
            _ => days(start, end),
        }
    }

    /// The year fraction from `start` to `end`, exact. `period` is the
    /// regular coupon period the days fall in and the payments a year
    /// (actual/actual (ICMA)); `maturity` the bond's.
    pub fn fraction(
        self,
        start: Date,
        end: Date,
        period: Option<(Date, Date, u32)>,
        maturity: Option<Date>,
    ) -> Ratio {
        let n = self.days(start, end, maturity);
        match self {
            DayCount::OneOne => ratio(1, 1),
            DayCount::Act360 | DayCount::Nl360 => ratio(n, 360),
            DayCount::Act364 => ratio(n, 364),
            DayCount::Act365Fixed | DayCount::Nl365 => ratio(n, 365),
            DayCount::Act365L => {
                // 366 where the period's end year is leap (annual: where
                // a February 29th falls in the period).
                let leap = match period {
                    Some((_, _, 1)) => leap_days(start, end) > 0,
                    _ => days_in(end.year(), 2) == 29,
                };
                ratio(n, if leap { 366 } else { 365 })
            }
            DayCount::ActActIcma => match period {
                Some((from, to, frequency)) if frequency > 0 => {
                    ratio(n, days(from, to) * i64::from(frequency))
                }
                _ => ratio(n, 365),
            },
            DayCount::ActActIsda => {
                let mut total = Ratio::zero();
                let mut at = start;
                while at < end {
                    let year_end = Date::from_ymd(at.year() + 1, 1, 1).unwrap_or(end);
                    let until = if year_end < end { year_end } else { end };
                    let basis = if days_in(at.year(), 2) == 29 {
                        366
                    } else {
                        365
                    };
                    total = total.add(&ratio(days(at, until), basis));
                    at = until;
                }
                total
            }
            _ => ratio(n, 360),
        }
    }
}

/// February 29ths after `start` up to and including `end`.
fn leap_days(start: Date, end: Date) -> i64 {
    let mut count = 0;
    for year in start.year()..=end.year() {
        if let Some(leap) = Date::from_ymd(year, 2, 29) {
            if leap > start && leap <= end {
                count += 1;
            }
        }
    }
    count
}

/// `n / d`, exact.
pub fn ratio(n: i64, d: i64) -> Ratio {
    Ratio::new(Big::from_i128(i128::from(n)), Big::from_i128(i128::from(d)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(text: &str) -> Date {
        Date::parse(text).unwrap()
    }

    #[test]
    fn g33s_thirty_360_counts_as_the_platforms_cases_do() {
        let basis = DayCount::Thirty360BondBasis;
        // MSRB Rule G-33's cases, as the platform's bond template holds them.
        assert_eq!(basis.days(d("2026-05-15"), d("2026-07-27"), None), 72);
        assert_eq!(basis.days(d("2026-01-31"), d("2026-03-31"), None), 60);
        assert_eq!(basis.days(d("2026-02-28"), d("2026-03-31"), None), 33);
        // SIA's February rule.
        assert_eq!(
            DayCount::Thirty360Sia.days(d("2026-02-28"), d("2026-08-31"), None),
            180
        );
        assert_eq!(
            DayCount::ThirtyE360.days(d("2026-01-31"), d("2026-03-31"), None),
            60
        );
        assert_eq!(
            DayCount::ThirtyEPlus360.days(d("2026-01-30"), d("2026-03-31"), None),
            61
        );
        assert_eq!(
            DayCount::Nl365.days(d("2028-02-01"), d("2028-03-01"), None),
            28
        );
    }

    #[test]
    fn actual_actual_reads_the_period() {
        let f = DayCount::ActActIcma.fraction(
            d("2026-08-15"),
            d("2026-08-17"),
            Some((d("2026-08-15"), d("2027-02-15"), 2)),
            None,
        );
        assert_eq!(f, ratio(2, 368));
        let isda = DayCount::ActActIsda.fraction(d("2027-12-01"), d("2028-01-31"), None, None);
        assert_eq!(isda, ratio(31, 365).add(&ratio(30, 366)));
    }
}
