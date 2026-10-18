//! A time zone's offset from UTC at an instant, read from the tz database the
//! host holds (contract v22; W3.20, the calendars spec's Q7): what turns an
//! execution's moment into its trade date through a spot-date rule's cut,
//! 17:00 America/New_York for FX.
//!
//! Data, not a dependency: the kernel holds no time zone library
//! (`meridian_domain::zones`). This reads the database's compiled files
//! (TZif, RFC 8536) from `/usr/share/zoneinfo`, which the runtime image
//! installs, in the kernel's own code -- their transitions, and the POSIX
//! rule that follows the last of them for every later instant. A zone the
//! host does not hold, or a file that does not read, is said, and nothing is
//! computed from it: never assumed to be UTC.

use crate::date::Date;

/// Where the host keeps the tz database's compiled files.
pub const ZONEINFO: &str = "/usr/share/zoneinfo";

/// One zone, as its file says it.
#[derive(Debug, Clone, PartialEq)]
pub struct Zone {
    /// Each transition, in seconds since the epoch, and the local time type
    /// in force from it.
    transitions: Vec<(i64, usize)>,
    /// Each local time type's offset from UTC, in seconds east.
    offsets: Vec<i32>,
    /// The type before the first transition.
    initial: usize,
    /// The rule for every instant after the last transition.
    rule: Option<Rule>,
}

/// A POSIX TZ rule (the file's footer): a standard offset and, where the
/// zone keeps daylight saving, its offset and the two changes each year.
#[derive(Debug, Clone, PartialEq)]
struct Rule {
    /// Seconds east of UTC.
    standard: i32,
    daylight: Option<(i32, Change, Change)>,
}

/// One yearly change, `Mm.w.d/time`: the `w`th `d`ay (0 Sunday; week 5 the
/// last) of month `m`, at `time` seconds past local midnight.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Change {
    month: u8,
    week: u8,
    weekday: u8,
    time: i32,
}

impl Zone {
    /// The zone `name` from the host's database, or why not.
    pub fn load(name: &str) -> Result<Zone, String> {
        if !crate::zones::is_zone(name) || name.contains("..") {
            return Err(format!("{name:?} is not an IANA time zone"));
        }
        let path = format!("{ZONEINFO}/{name}");
        let bytes = std::fs::read(&path).map_err(|failed| {
            format!("the time zone {name} is not held on this host ({path}: {failed})")
        })?;
        Zone::read(&bytes).map_err(|why| format!("the time zone {name} did not read: {why}"))
    }

    /// A zone from its compiled file.
    pub fn read(bytes: &[u8]) -> Result<Zone, String> {
        let (version, counts, after) = header(bytes, 0)?;
        // A version 2 or later file repeats its data with 64-bit times after
        // the first block, then its POSIX rule; the first block is skipped.
        let (wide, counts, at) = if version >= b'2' {
            let skip = after + block_len(&counts, 4);
            let (_, counts, at) = header(bytes, skip)?;
            (true, counts, at)
        } else {
            (false, counts, after)
        };
        let [_isut, _isstd, leap, timecnt, typecnt, charcnt] = counts;
        let width = if wide { 8 } else { 4 };
        let mut cursor = at;
        let take = |cursor: &mut usize, n: usize| -> Result<&[u8], String> {
            let end = cursor.checked_add(n).ok_or("a count overflows")?;
            let slice = bytes.get(*cursor..end).ok_or("the file ends early")?;
            *cursor = end;
            Ok(slice)
        };
        let mut times = Vec::with_capacity(timecnt);
        for chunk in take(&mut cursor, timecnt * width)?.chunks(width) {
            times.push(if wide {
                i64::from_be_bytes(chunk.try_into().map_err(|_| "a time is short")?)
            } else {
                i64::from(i32::from_be_bytes(
                    chunk.try_into().map_err(|_| "a time is short")?,
                ))
            });
        }
        let indices = take(&mut cursor, timecnt)?.to_vec();
        let mut offsets = Vec::with_capacity(typecnt);
        let mut daylight = Vec::with_capacity(typecnt);
        for chunk in take(&mut cursor, typecnt * 6)?.chunks(6) {
            offsets.push(i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            daylight.push(chunk[4] != 0);
        }
        if offsets.is_empty() {
            return Err("it names no local time type".into());
        }
        take(&mut cursor, charcnt)?;
        take(&mut cursor, leap * (width + 4))?;
        take(&mut cursor, counts[0] + counts[1])?;
        let mut transitions = Vec::with_capacity(timecnt);
        for (time, index) in times.into_iter().zip(indices) {
            let index = usize::from(index);
            if index >= offsets.len() {
                return Err("a transition names a type the file does not hold".into());
            }
            transitions.push((time, index));
        }
        let rule = if wide {
            let footer = &bytes[cursor..];
            let text = std::str::from_utf8(footer).map_err(|_| "its rule is not text")?;
            let text = text.trim_matches('\n');
            if text.is_empty() {
                None
            } else {
                Some(Rule::parse(text)?)
            }
        } else {
            None
        };
        let initial = daylight.iter().position(|dst| !dst).unwrap_or(0);
        Ok(Zone {
            transitions,
            offsets,
            initial,
            rule,
        })
    }

    /// The offset from UTC, in seconds east, at `utc` seconds since the
    /// epoch.
    pub fn offset_at(&self, utc: i64) -> i32 {
        match self.transitions.last() {
            Some((last, _)) if utc >= *last => match &self.rule {
                Some(rule) => rule.offset_at(utc),
                None => self.offsets[self.transitions.last().map_or(self.initial, |t| t.1)],
            },
            Some(_) => {
                let at = self.transitions.partition_point(|(time, _)| *time <= utc);
                if at == 0 {
                    self.offsets[self.initial]
                } else {
                    self.offsets[self.transitions[at - 1].1]
                }
            }
            None => match &self.rule {
                Some(rule) => rule.offset_at(utc),
                None => self.offsets[self.initial],
            },
        }
    }

    /// The local date and the seconds past local midnight at an instant.
    pub fn local(&self, utc_ns: i64) -> Option<(Date, i64)> {
        let utc = utc_ns.div_euclid(1_000_000_000);
        let local = utc + i64::from(self.offset_at(utc));
        let days = local.div_euclid(86_400);
        Some((Date::from_days(days)?, local.rem_euclid(86_400)))
    }
}

fn header(bytes: &[u8], at: usize) -> Result<(u8, [usize; 6], usize), String> {
    let head = bytes
        .get(at..at + 44)
        .ok_or("the file ends before its header")?;
    if &head[..4] != b"TZif" {
        return Err("it is not a TZif file".into());
    }
    let mut counts = [0usize; 6];
    for (i, count) in counts.iter_mut().enumerate() {
        let from = 20 + i * 4;
        *count = u32::from_be_bytes([head[from], head[from + 1], head[from + 2], head[from + 3]])
            as usize;
    }
    Ok((head[4], counts, at + 44))
}

fn block_len(counts: &[usize; 6], width: usize) -> usize {
    let [isut, isstd, leap, timecnt, typecnt, charcnt] = *counts;
    timecnt * width + timecnt + typecnt * 6 + charcnt + leap * (width + 4) + isstd + isut
}

impl Rule {
    fn parse(text: &str) -> Result<Rule, String> {
        let mut rest = text;
        name(&mut rest)?;
        let standard = -offset(&mut rest)?;
        if rest.is_empty() {
            return Ok(Rule {
                standard,
                daylight: None,
            });
        }
        name(&mut rest)?;
        let daylight = if rest.starts_with(',') {
            standard + 3600
        } else {
            -offset(&mut rest)?
        };
        let rest = rest
            .strip_prefix(',')
            .ok_or_else(|| format!("its rule {text:?} names no changes"))?;
        let (start, end) = rest
            .split_once(',')
            .ok_or_else(|| format!("its rule {text:?} names one change"))?;
        Ok(Rule {
            standard,
            daylight: Some((daylight, change(start)?, change(end)?)),
        })
    }

    fn offset_at(&self, utc: i64) -> i32 {
        let Some((daylight, start, end)) = self.daylight else {
            return self.standard;
        };
        let year = Date::from_days(utc.div_euclid(86_400)).map_or(1970, |d| i64::from(d.year()));
        // The start is given in standard local time, the end in daylight
        // local time.
        let begins = start.instant(year) - i64::from(self.standard);
        let ends = end.instant(year) - i64::from(daylight);
        let in_daylight = if begins < ends {
            utc >= begins && utc < ends
        } else {
            utc >= begins || utc < ends
        };
        if in_daylight {
            daylight
        } else {
            self.standard
        }
    }
}

/// A zone abbreviation, `EST` or `<+03>`, consumed.
fn name(rest: &mut &str) -> Result<(), String> {
    if let Some(quoted) = rest.strip_prefix('<') {
        let end = quoted.find('>').ok_or("a quoted name is not closed")?;
        *rest = &quoted[end + 1..];
        return Ok(());
    }
    let length = rest.chars().take_while(|c| c.is_ascii_alphabetic()).count();
    if length < 3 {
        return Err(format!("{rest:?} begins with no zone name"));
    }
    *rest = &rest[length..];
    Ok(())
}

/// A POSIX offset, `[+-]hh[:mm[:ss]]`, in seconds as written (west
/// positive), consumed.
fn offset(rest: &mut &str) -> Result<i32, String> {
    let length = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | ':'))
        .count();
    let text = &rest[..length];
    *rest = &rest[length..];
    seconds(text)
}

fn seconds(text: &str) -> Result<i32, String> {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(digits) => (-1, digits),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let mut parts = digits.split(':');
    let mut total = 0i32;
    for scale in [3600, 60, 1] {
        match parts.next() {
            Some(part) => {
                let value: i32 = part
                    .parse()
                    .map_err(|_| format!("{text:?} is not an offset"))?;
                total += value * scale;
            }
            None => break,
        }
    }
    if digits.is_empty() {
        return Err("an offset is empty".into());
    }
    Ok(sign * total)
}

/// One change, `Mm.w.d[/time]`; the other two forms are not read.
fn change(text: &str) -> Result<Change, String> {
    let (date, time) = match text.split_once('/') {
        Some((date, time)) => (date, seconds(time)?),
        None => (text, 7200),
    };
    let fields: Vec<&str> = date
        .strip_prefix('M')
        .ok_or_else(|| format!("the change {text:?} is not in the Mm.w.d form"))?
        .split('.')
        .collect();
    let [month, week, weekday] = fields[..] else {
        return Err(format!("the change {text:?} is not in the Mm.w.d form"));
    };
    let read = |field: &str, most: u8| -> Result<u8, String> {
        field
            .parse::<u8>()
            .ok()
            .filter(|value| *value <= most)
            .ok_or_else(|| format!("the change {text:?} does not read"))
    };
    let change = Change {
        month: read(month, 12)?,
        week: read(week, 5)?,
        weekday: read(weekday, 6)?,
        time,
    };
    if change.month == 0 || change.week == 0 {
        return Err(format!("the change {text:?} does not read"));
    }
    Ok(change)
}

impl Change {
    /// Its local instant in `year`, as seconds since the epoch counted as
    /// though the local time were UTC.
    fn instant(self, year: i64) -> i64 {
        let year = year.clamp(1, 9999) as u16;
        let first = Date::from_ymd(year, self.month, 1).map_or(0, Date::days_since_epoch);
        // 1970-01-01 was a Thursday (4, Sunday 0).
        let weekday_of_first = (first + 4).rem_euclid(7);
        let mut day = first + (i64::from(self.weekday) - weekday_of_first).rem_euclid(7);
        day += 7 * (i64::from(self.week) - 1);
        let next_month = if self.month == 12 {
            Date::from_ymd(year.saturating_add(1), 1, 1)
        } else {
            Date::from_ymd(year, self.month + 1, 1)
        }
        .map_or(i64::MAX, Date::days_since_epoch);
        while day >= next_month {
            day -= 7;
        }
        day * 86_400 + i64::from(self.time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_yorks_rule_keeps_daylight_from_marchs_second_sunday_to_novembers_first() {
        let rule = Rule::parse("EST5EDT,M3.2.0,M11.1.0").unwrap();
        // 2025-10-13 21:30 UTC, the fixture's execution: 17:30 EDT.
        assert_eq!(rule.offset_at(1_760_391_000), -4 * 3600);
        // 2025-12-15 12:00 UTC: EST.
        assert_eq!(rule.offset_at(1_765_800_000), -5 * 3600);
        // 2026-03-08 06:59:59 UTC is 01:59:59 EST; a second later, 03:00 EDT.
        assert_eq!(rule.offset_at(1_772_953_199), -5 * 3600);
        assert_eq!(rule.offset_at(1_772_953_200), -4 * 3600);
    }

    #[test]
    fn a_southern_rule_and_a_rule_with_no_daylight_read() {
        let sydney = Rule::parse("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        // January is summer in Sydney.
        assert_eq!(sydney.offset_at(1_768_000_000), 11 * 3600);
        assert_eq!(sydney.offset_at(1_782_000_000), 10 * 3600);
        let tokyo = Rule::parse("JST-9").unwrap();
        assert_eq!(tokyo.offset_at(1_760_391_000), 9 * 3600);
        assert_eq!(Rule::parse("<+03>-3").unwrap().standard, 3 * 3600);
        assert!(Rule::parse("EST5EDT,J60,J300").is_err());
    }

    #[test]
    fn a_file_with_a_transition_and_its_rule_reads() {
        // Built by hand: v2, one transition in its 64-bit block, two types,
        // and New York's rule after it.
        let mut file = Vec::new();
        let header = |file: &mut Vec<u8>, timecnt: u32, typecnt: u32, charcnt: u32| {
            file.extend_from_slice(b"TZif2");
            file.extend_from_slice(&[0; 15]);
            for count in [0, 0, 0, timecnt, typecnt, charcnt] {
                file.extend_from_slice(&count.to_be_bytes());
            }
        };
        header(&mut file, 0, 1, 4);
        file.extend_from_slice(&0i32.to_be_bytes());
        file.extend_from_slice(&[0, 0]);
        file.extend_from_slice(b"UTC\0");
        header(&mut file, 1, 2, 8);
        file.extend_from_slice(&1_000_000_000i64.to_be_bytes());
        file.push(1);
        for (offset, dst, at) in [(0i32, 0u8, 0u8), (-5 * 3600, 0, 4)] {
            file.extend_from_slice(&offset.to_be_bytes());
            file.extend_from_slice(&[dst, at]);
        }
        file.extend_from_slice(b"UTC\0EST\0");
        file.extend_from_slice(b"\nEST5EDT,M3.2.0,M11.1.0\n");
        let zone = Zone::read(&file).unwrap();
        assert_eq!(zone.offset_at(999_999_999), 0);
        assert_eq!(zone.offset_at(1_760_391_000), -4 * 3600);
        let (date, seconds) = zone.local(1_760_391_000_000_000_000).unwrap();
        assert_eq!(date.to_string(), "2025-10-13");
        assert_eq!(seconds, 17 * 3600 + 30 * 60);
        assert!(Zone::read(b"not a zone").is_err());
    }

    #[test]
    fn a_name_that_is_no_zone_is_refused_before_any_file_is_read() {
        assert!(Zone::load("../etc/passwd")
            .unwrap_err()
            .contains("not an IANA"));
        assert!(Zone::load("Mars/Olympus")
            .unwrap_err()
            .contains("not an IANA"));
    }
}
