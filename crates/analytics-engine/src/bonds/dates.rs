//! Dates as a debt record's terms move them (contract v29; ENG1 §4): months
//! added with a coupon schedule's end-of-month rule, and business days on
//! the record's payment calendar, its holidays read from core's settlement
//! calendars by the component (never QuantLib's), weekends always closed.

use std::collections::BTreeSet;

use meridian_domain::date::Date;

/// The days in a month.
pub fn days_in(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        _ => 28,
    }
}

/// Whether a date is its month's last day.
pub fn is_month_end(date: Date) -> bool {
    date.day() == days_in(date.year(), date.month())
}

/// `date` moved by `months` (negative goes back), its day kept where the
/// month has it and the month's last day where it does not; on the month's
/// last day where `end_of_month` holds.
pub fn add_months(date: Date, months: i32, end_of_month: bool) -> Date {
    let index = i32::from(date.year()) * 12 + i32::from(date.month()) - 1 + months;
    let year = u16::try_from(index.div_euclid(12)).unwrap_or(1);
    let month = u8::try_from(index.rem_euclid(12) + 1).unwrap_or(1);
    let last = days_in(year, month);
    let day = if end_of_month {
        last
    } else {
        date.day().min(last)
    };
    Date::from_ymd(year, month, day).unwrap_or(date)
}

/// Calendar days from `start` to `end`.
pub fn days(start: Date, end: Date) -> i64 {
    end.days_since_epoch() - start.days_since_epoch()
}

/// The date `n` calendar days on.
pub fn plus_days(date: Date, n: i64) -> Date {
    Date::from_days(date.days_since_epoch() + n).unwrap_or(date)
}

/// Monday 1 to Sunday 7 (1970-01-01 was a Thursday).
pub fn weekday(date: Date) -> i64 {
    (date.days_since_epoch() + 3).rem_euclid(7) + 1
}

/// A payment calendar: its holidays, weekends always closed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Calendar {
    pub holidays: BTreeSet<Date>,
}

/// How a date rolls on a calendar (`BusinessDayConvention`'s values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Roll {
    Following,
    ModifiedFollowing,
    Preceding,
    Unadjusted,
}

impl Roll {
    /// From the terms' name; following where none is stated (the bond's
    /// field: "following unless the terms say otherwise").
    pub fn named(name: &str) -> Roll {
        match name {
            "BUSINESS_DAY_CONVENTION_MODIFIED_FOLLOWING" => Roll::ModifiedFollowing,
            "BUSINESS_DAY_CONVENTION_PRECEDING" => Roll::Preceding,
            "BUSINESS_DAY_CONVENTION_NONE" => Roll::Unadjusted,
            _ => Roll::Following,
        }
    }
}

impl Calendar {
    pub fn is_business(&self, date: Date) -> bool {
        weekday(date) <= 5 && !self.holidays.contains(&date)
    }

    /// `date` rolled by `roll`.
    pub fn adjust(&self, date: Date, roll: Roll) -> Date {
        let step = |from: Date, by: i64| {
            let mut at = from;
            while !self.is_business(at) {
                at = plus_days(at, by);
            }
            at
        };
        match roll {
            Roll::Unadjusted => date,
            Roll::Following => step(date, 1),
            Roll::Preceding => step(date, -1),
            Roll::ModifiedFollowing => {
                let on = step(date, 1);
                if on.month() == date.month() {
                    on
                } else {
                    step(date, -1)
                }
            }
        }
    }

    /// The `n`th business day after `date` (`n` > 0).
    pub fn business_days_after(&self, date: Date, n: u32) -> Date {
        let mut at = date;
        let mut left = n;
        while left > 0 {
            at = plus_days(at, 1);
            if self.is_business(at) {
                left -= 1;
            }
        }
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(text: &str) -> Date {
        Date::parse(text).unwrap()
    }

    #[test]
    fn months_keep_their_day_or_the_months_last() {
        assert_eq!(add_months(d("2031-05-31"), -6, false), d("2030-11-30"));
        assert_eq!(add_months(d("2028-08-31"), -6, false), d("2028-02-29"));
        assert_eq!(add_months(d("2028-02-29"), 6, true), d("2028-08-31"));
        assert_eq!(add_months(d("2028-02-29"), 6, false), d("2028-08-29"));
        assert_eq!(add_months(d("2026-01-15"), -1, false), d("2025-12-15"));
    }

    #[test]
    fn a_holiday_and_a_weekend_roll() {
        let calendar = Calendar {
            holidays: [d("2026-11-26")].into_iter().collect(),
        };
        // Thanksgiving, then a Saturday.
        assert_eq!(
            calendar.adjust(d("2026-11-26"), Roll::Following),
            d("2026-11-27")
        );
        assert_eq!(
            calendar.adjust(d("2026-10-31"), Roll::Following),
            d("2026-11-02")
        );
        assert_eq!(
            calendar.adjust(d("2026-10-31"), Roll::ModifiedFollowing),
            d("2026-10-30")
        );
        assert_eq!(
            calendar.business_days_after(d("2026-11-25"), 1),
            d("2026-11-27")
        );
        assert_eq!(weekday(d("2026-10-31")), 6);
    }
}
