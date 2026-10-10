//! Business dates: ISO 8601 calendar dates, as text, compared as text.
//!
//! The book keeps a business date as the `YYYY-MM-DD` it was given, which
//! sorts correctly as text, and reads none as an instant: a business date is
//! a day the firm's records stand for, not a time anywhere.

/// Whether `text` is a calendar date, `YYYY-MM-DD`, that exists: the
/// deployment's one reading of a date (meridian_domain::date, contract v18).
pub fn is_date(text: &str) -> bool {
    meridian_domain::date::is_date(text)
}

/// The UTC calendar date of an instant: what an act a person takes today,
/// such as setting an attribute, stands for (W9.13).
pub fn date_of(ns: i64) -> String {
    meridian_domain::date::Date::of_utc_instant(ns).to_string()
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
