//! Business dates: ISO 8601 calendar dates, as text, compared as text.
//!
//! The book keeps a business date as the `YYYY-MM-DD` it was given, which
//! sorts correctly as text, and reads none as an instant: a business date is
//! a day the firm's records stand for, not a time anywhere.

/// Whether `text` is a calendar date, `YYYY-MM-DD`, that exists.
pub fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = &text[range];
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return false;
    };
    (1..=12).contains(&month) && day >= 1 && day <= days_in(year, month)
}

fn days_in(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        _ => 28,
    }
}

/// The UTC calendar date of an instant: what an act a person takes today,
/// such as setting an attribute, stands for (W9.13).
pub fn date_of(ns: i64) -> String {
    let days = ns.div_euclid(86_400 * 1_000_000_000);
    // Howard Hinnant's civil-from-days, on the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_is_a_day_that_exists() {
        assert!(is_date("2026-09-08"));
        assert!(is_date("2024-02-29"));
        assert!(!is_date("2026-02-29"));
        assert!(!is_date("2026-9-8"));
        assert!(!is_date("2026-13-01"));
        assert!(!is_date(""));
    }

    #[test]
    fn an_instant_falls_on_its_utc_date() {
        assert_eq!(date_of(1_757_376_000_000_000_000), "2025-09-09");
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(1_790_553_600_000_000_000), "2026-09-28");
    }
}
