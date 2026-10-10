//! A calendar date, typed (contract v18; plans/the-lake-prices-the-book,
//! ruling 3 of 2026-10-09): a daily settlement or valuation price is keyed by
//! a plain date, never a moment. The wire keeps the ISO 8601 string
//! (`2026-10-09`); every component that reads one reads it here, so an
//! invalid date -- `2026-02-30`, `2026-9-8`, a moment -- is refused the same
//! way at the sidecar, in the lake and in the book.
//!
//! A date is a day the records stand for, not a time anywhere: which instant
//! a day starts at is the declaring dataset's zone and day end, and the
//! `dgm`'s to apply. What this offers besides reading and writing one is the
//! proleptic Gregorian arithmetic a store needs to order and step dates.

use std::fmt;
use std::str::FromStr;

/// One calendar day, year 1 to 9999.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: u16,
    month: u8,
    day: u8,
}

/// Why a text is not a date, in words a refusal can carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDate(pub String);

impl fmt::Display for InvalidDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a date: a plain date is YYYY-MM-DD, a day that exists",
            self.0
        )
    }
}

impl std::error::Error for InvalidDate {}

const NS_PER_DAY: i64 = 86_400 * 1_000_000_000;

fn days_in(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        _ => 28,
    }
}

impl Date {
    /// The date of a year, month and day, if that day exists.
    pub fn from_ymd(year: u16, month: u8, day: u8) -> Option<Date> {
        ((1..=9999).contains(&year)
            && (1..=12).contains(&month)
            && day >= 1
            && day <= days_in(year, month))
        .then_some(Date { year, month, day })
    }

    /// `YYYY-MM-DD`, a day that exists; anything else is refused.
    pub fn parse(text: &str) -> Result<Date, InvalidDate> {
        let invalid = || InvalidDate(text.to_string());
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(invalid());
        }
        let digits = |range: std::ops::Range<usize>| -> Option<u16> {
            let part = &text[range];
            part.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| part.parse().ok())
                .flatten()
        };
        let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10))
        else {
            return Err(invalid());
        };
        let (Ok(month), Ok(day)) = (u8::try_from(month), u8::try_from(day)) else {
            return Err(invalid());
        };
        Date::from_ymd(year, month, day).ok_or_else(invalid)
    }

    pub fn year(self) -> u16 {
        self.year
    }

    pub fn month(self) -> u8 {
        self.month
    }

    pub fn day(self) -> u8 {
        self.day
    }

    /// Days since 1970-01-01, negative before it (Howard Hinnant's
    /// days-from-civil).
    pub fn days_since_epoch(self) -> i64 {
        let (m, d) = (i64::from(self.month), i64::from(self.day));
        let y = i64::from(self.year) - i64::from(m <= 2);
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// The date `days` days after 1970-01-01, if it is within year 1 to 9999
    /// (civil-from-days).
    pub fn from_days(days: i64) -> Option<Date> {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        Date::from_ymd(
            u16::try_from(y).ok()?,
            u8::try_from(m).ok()?,
            u8::try_from(d).ok()?,
        )
    }

    /// The UTC calendar date an instant falls on.
    pub fn of_utc_instant(ns: i64) -> Date {
        Date::from_days(ns.div_euclid(NS_PER_DAY)).unwrap_or(Date {
            year: 1970,
            month: 1,
            day: 1,
        })
    }

    /// The day after, if there is one.
    pub fn next(self) -> Option<Date> {
        Date::from_days(self.days_since_epoch() + 1)
    }
}

impl FromStr for Date {
    type Err = InvalidDate;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Date::parse(text)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// Whether `text` is a date, `YYYY-MM-DD`, that exists.
pub fn is_date(text: &str) -> bool {
    Date::parse(text).is_ok()
}

/// An optional date as the wire carries one: empty for none, otherwise a
/// date or the refusal naming `field`.
pub fn optional(field: &str, text: &str) -> Result<Option<Date>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    Date::parse(text)
        .map(Some)
        .map_err(|invalid| format!("{field}: {invalid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_is_a_day_that_exists_and_reads_back_as_written() {
        for text in ["2026-10-09", "2024-02-29", "0001-01-01", "9999-12-31"] {
            assert_eq!(Date::parse(text).unwrap().to_string(), text);
        }
        for text in [
            "2026-02-29",
            "2026-9-8",
            "2026-13-01",
            "2026-00-10",
            "0000-01-01",
            "2026-10-09T00:00:00Z",
            "",
            "２０２６-10-09",
        ] {
            assert!(Date::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn days_round_trip_and_order_as_dates_do() {
        let epoch = Date::parse("1970-01-01").unwrap();
        assert_eq!(epoch.days_since_epoch(), 0);
        for days in [-719_162, -1, 0, 1, 20_000, 2_932_896] {
            let date = Date::from_days(days).unwrap();
            assert_eq!(date.days_since_epoch(), days, "{date}");
        }
        assert!(Date::parse("2026-10-09").unwrap() < Date::parse("2026-10-10").unwrap());
        assert_eq!(
            Date::parse("2024-02-28")
                .unwrap()
                .next()
                .unwrap()
                .to_string(),
            "2024-02-29"
        );
        assert_eq!(
            Date::of_utc_instant(1_790_553_600_000_000_000).to_string(),
            "2026-09-28"
        );
        assert_eq!(Date::of_utc_instant(-1).to_string(), "1969-12-31");
    }

    #[test]
    fn an_empty_date_is_none_and_an_invalid_one_names_its_field() {
        assert_eq!(optional("business_date", "").unwrap(), None);
        let refused = optional("prices[0].meta.business_date", "2026-02-30").unwrap_err();
        assert!(
            refused.starts_with("prices[0].meta.business_date:"),
            "{refused}"
        );
    }
}
