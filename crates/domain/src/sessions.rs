//! Whether a moment falls in a venue's trading session (contract v27, W1.20,
//! W3.19; ruling 419): what makes staleness count only in session, read
//! alike by the analytics component and core's monitor, as the SDK's
//! `roles.in_session` reads it for a plugin.
//!
//! A `TradingSessions` names its IANA zone, its sessions by weekday (start
//! and end minutes after local midnight, the end exclusive and at most
//! 1440; an overnight session two sessions, one each side of midnight;
//! extended sessions marked) and its dated early closes, whose sessions
//! replace that day's (none: closed), the last listed winning where a date
//! appears twice. Extended sessions count only where asked for.

use crate::date::Date;
use crate::v1::{TradingSession, TradingSessions};
use crate::zone::Zone;

/// The weekday of a date, Monday 1 to Sunday 7 (`meridian.v1.Weekday`).
pub fn weekday(date: Date) -> i32 {
    // 1970-01-01 was a Thursday.
    (date.days_since_epoch() + 3).rem_euclid(7) as i32 + 1
}

fn runs(session: &TradingSession, minute: u32, extended: bool) -> bool {
    (extended || !session.extended) && session.start_minute <= minute && minute < session.end_minute
}

/// The weekly sessions in force on `date` (contract v29, 657; W3.19): those
/// naming its weekday, and of them, for regular and for extended hours
/// apart, the rows whose `valid_from_date` is the latest on or before the
/// date -- hours that changed are two rows, each from its date; an empty
/// date is in force since the calendar's coverage began, and a row from a
/// later date not yet. Its early closes are the caller's.
pub fn in_force(sessions: &TradingSessions, date: Date) -> Vec<&TradingSession> {
    let day = weekday(date);
    let today = date.to_string();
    let named: Vec<&TradingSession> = sessions
        .sessions
        .iter()
        .filter(|s| s.weekdays.contains(&day))
        .filter(|s| s.valid_from_date.is_empty() || s.valid_from_date <= today)
        .collect();
    let latest = |extended: bool| {
        named
            .iter()
            .filter(|s| s.extended == extended)
            .map(|s| s.valid_from_date.as_str())
            .max()
            .unwrap_or("")
            .to_string()
    };
    let (regular, extended) = (latest(false), latest(true));
    named
        .into_iter()
        .filter(|s| {
            s.valid_from_date
                == if s.extended {
                    extended.as_str()
                } else {
                    regular.as_str()
                }
        })
        .collect()
}

/// Whether `at_ns` falls in one of the calendar's sessions, read in its zone
/// (`zone` the calendar's `time_zone`, loaded). An early close's sessions
/// stand for its date in place of the weekday's; otherwise the weekday's
/// sessions in force on the date ([`in_force`]).
pub fn in_session(sessions: &TradingSessions, zone: &Zone, at_ns: i64, extended: bool) -> bool {
    let Some((date, seconds)) = zone.local(at_ns) else {
        return false;
    };
    let minute = (seconds / 60) as u32;
    let today = date.to_string();
    if let Some(day) = sessions.early_closes.iter().rev().find(|d| d.date == today) {
        return day.sessions.iter().any(|s| runs(s, minute, extended));
    }
    in_force(sessions, date)
        .into_iter()
        .any(|s| runs(s, minute, extended))
}

/// [`in_session`] with the zone loaded from the host's tz database; a zone
/// the host does not hold is said, never assumed.
pub fn in_session_here(
    sessions: &TradingSessions,
    at_ns: i64,
    extended: bool,
) -> Result<bool, String> {
    let zone = Zone::load(&sessions.time_zone)?;
    Ok(in_session(sessions, &zone, at_ns, extended))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{TradingSessionDay, Weekday};

    fn session(days: &[Weekday], start: u32, end: u32, extended: bool) -> TradingSession {
        TradingSession {
            weekdays: days.iter().map(|d| *d as i32).collect(),
            start_minute: start,
            end_minute: end,
            extended,
            // Contract v29 (657): in force from the calendar's first date.
            valid_from_date: String::new(),
        }
    }

    /// FX's week (W1.20): Sunday 17:00 to Friday 17:00 New York; and an
    /// equity day 09:30 to 16:00 with a post-market extended to 20:00.
    fn fx() -> TradingSessions {
        TradingSessions {
            time_zone: "America/New_York".into(),
            sessions: vec![
                session(&[Weekday::Sunday], 1020, 1440, false),
                session(
                    &[
                        Weekday::Monday,
                        Weekday::Tuesday,
                        Weekday::Wednesday,
                        Weekday::Thursday,
                    ],
                    0,
                    1440,
                    false,
                ),
                session(&[Weekday::Friday], 0, 1020, false),
                session(&[Weekday::Monday], 960, 1200, true),
            ],
            early_closes: vec![TradingSessionDay {
                date: "2026-12-24".into(),
                sessions: vec![session(&[Weekday::Thursday], 0, 780, false)],
                source: "SRC-1".into(),
            }],
        }
    }

    fn at(zone: &Zone, date: &str, hh: i64, mm: i64) -> i64 {
        zone.instant_of(Date::parse(date).unwrap(), hh * 3600 + mm * 60)
    }

    #[test]
    fn a_moment_is_in_session_by_its_local_weekday_minute_and_early_closes() {
        let zone = Zone::of_rule("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let fx = fx();
        assert_eq!(
            weekday(Date::parse("2026-10-17").unwrap()),
            Weekday::Saturday as i32
        );
        // Saturday: closed all day; Sunday from 17:00.
        assert!(!in_session(
            &fx,
            &zone,
            at(&zone, "2026-10-17", 12, 0),
            false
        ));
        assert!(!in_session(
            &fx,
            &zone,
            at(&zone, "2026-10-18", 16, 59),
            false
        ));
        assert!(in_session(
            &fx,
            &zone,
            at(&zone, "2026-10-18", 17, 0),
            false
        ));
        // Friday until 17:00, the end exclusive.
        assert!(in_session(
            &fx,
            &zone,
            at(&zone, "2026-10-16", 16, 59),
            false
        ));
        assert!(!in_session(
            &fx,
            &zone,
            at(&zone, "2026-10-16", 17, 0),
            false
        ));
        // Christmas Eve, a Thursday, closes at 13:00.
        assert!(in_session(
            &fx,
            &zone,
            at(&zone, "2026-12-24", 12, 59),
            false
        ));
        assert!(!in_session(
            &fx,
            &zone,
            at(&zone, "2026-12-24", 13, 0),
            false
        ));
        // In daylight saving and out, by the zone's offset at the moment.
        assert!(in_session(
            &fx,
            &zone,
            at(&zone, "2026-07-19", 17, 30),
            false
        ));
        assert!(in_session(
            &fx,
            &zone,
            at(&zone, "2026-01-18", 17, 30),
            false
        ));
    }

    #[test]
    fn an_extended_session_counts_only_where_asked_for() {
        let zone = Zone::of_rule("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let equity = TradingSessions {
            time_zone: "America/New_York".into(),
            sessions: vec![
                session(&[Weekday::Monday], 570, 960, false),
                session(&[Weekday::Monday], 960, 1200, true),
            ],
            early_closes: vec![],
        };
        let evening = at(&zone, "2026-10-19", 17, 0);
        assert!(!in_session(&equity, &zone, evening, false));
        assert!(in_session(&equity, &zone, evening, true));
        assert!(in_session(
            &equity,
            &zone,
            at(&zone, "2026-10-19", 9, 30),
            false
        ));
    }

    #[test]
    fn hours_that_changed_are_two_rows_each_in_force_from_its_date() {
        // Contract v29 (657): a venue's regular hours from 2016, its
        // after-hours session moved from 18:30 to 20:00 on 2020-03-02 -- two
        // rows, each from its date -- the regular row untouched.
        let zone = Zone::of_rule("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let dated = |start, end, extended, from: &str| TradingSession {
            valid_from_date: from.into(),
            ..session(&[Weekday::Monday], start, end, extended)
        };
        let venue = TradingSessions {
            time_zone: "America/New_York".into(),
            sessions: vec![
                dated(570, 960, false, ""),
                dated(960, 1110, true, "2016-01-04"),
                dated(960, 1200, true, "2020-03-02"),
            ],
            early_closes: vec![],
        };
        let monday = |date: &str| Date::parse(date).unwrap();
        let ends = |date: &str| {
            in_force(&venue, monday(date))
                .iter()
                .map(|s| (s.extended, s.end_minute))
                .collect::<Vec<_>>()
        };
        assert_eq!(ends("2019-03-04"), vec![(false, 960), (true, 1110)]);
        assert_eq!(ends("2020-03-02"), vec![(false, 960), (true, 1200)]);
        // Before its first row's date, no extended hours were in force.
        assert_eq!(ends("2015-03-02"), vec![(false, 960)]);
        let seven_pm = |date: &str| at(&zone, date, 19, 0);
        assert!(!in_session(&venue, &zone, seven_pm("2019-03-04"), true));
        assert!(in_session(&venue, &zone, seven_pm("2026-10-19"), true));
    }
}
