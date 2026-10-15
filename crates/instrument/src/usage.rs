//! The usage counts the conductor's report carries (contract v24, W3.28):
//! per identifier scheme, how many instruments carry it and how many resolve
//! only through it as the period ends, and how many misses arrived with it
//! as their only identifier within the period. Counts only: never a value,
//! a holding, a quantity or an account.
//!
//! A scheme is counted by its family, as the licence list names schemes
//! (`one_id::family`: the three FIGI levels one, a ticker with its venue
//! `ticker_and_venue`), never the deployment's own IDs or the platform's
//! (`open_meridian`, `open_meridian_local`). A miss leaves no record, so the
//! store counts each as it answers it, per day and family, in its own state.

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::date::Date;
use meridian_domain::v1::{
    Identifier as PbIdentifier, ListIdentifierUsageReply, ListIdentifierUsageRequest,
    SchemeUsageCount,
};

use crate::one_id::{family, LOCAL_SCHEME};
use crate::store::{Instrument, Result, Store};

/// A day's misses, per family, kept as JSON under this key and the date.
pub const MISSES_ON: &str = "usage_misses:";

/// The most schemes a report carries, as the platform reads it.
pub const MOST_SCHEMES: usize = 64;

/// The longest period one ask covers.
pub const MOST_DAYS: i64 = 366;

/// The families a set of identifiers names, the deployment's and the
/// platform's own IDs left out; only those a report can name.
fn families<'a>(schemes: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    schemes
        .filter(|scheme| *scheme != meridian_symbology::GLOBAL_ID && *scheme != LOCAL_SCHEME)
        .map(|scheme| family(scheme).to_ascii_lowercase())
        .filter(|name| reportable(name))
        .collect()
}

/// A scheme name the platform takes: letters, digits, `_` and `-`, at most
/// 32, starting with a letter or a digit.
fn reportable(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// A miss answered (W3.1, W3.2): counted against its one family where its
/// identifiers name only one, on the day it arrived. Called under the
/// store's write lock.
pub fn note_miss(store: &dyn Store, identifiers: &[PbIdentifier], now_ns: i64) -> Result<()> {
    let named = families(
        identifiers
            .iter()
            .map(|identifier| identifier.scheme.as_str()),
    );
    if named.len() != 1 {
        return Ok(());
    }
    let Some(only) = named.into_iter().next() else {
        return Ok(());
    };
    let key = format!("{MISSES_ON}{}", Date::of_utc_instant(now_ns));
    let mut day: BTreeMap<String, i64> = store
        .state(&key)?
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    *day.entry(only).or_default() += 1;
    let text = serde_json::to_string(&day).unwrap_or_default();
    store.set_state(&key, &text)
}

/// W3.28: the counts for a period, refused naming the field for a date that
/// is none or a period that runs backwards or past a year.
pub fn list_identifier_usage(
    store: &dyn Store,
    request: &ListIdentifierUsageRequest,
) -> Result<std::result::Result<ListIdentifierUsageReply, (String, String)>> {
    let start = match Date::parse(request.period_start.trim()) {
        Ok(date) => date,
        Err(_) => return Ok(Err(("period_start".into(), "a date, YYYY-MM-DD".into()))),
    };
    let end = match Date::parse(request.period_end.trim()) {
        Ok(date) => date,
        Err(_) => return Ok(Err(("period_end".into(), "a date, YYYY-MM-DD".into()))),
    };
    let days = end.days_since_epoch() - start.days_since_epoch();
    if days < 0 {
        return Ok(Err((
            "period_end".into(),
            "on or after period_start".into(),
        )));
    }
    if days >= MOST_DAYS {
        return Ok(Err((
            "period_end".into(),
            format!("a period of at most {MOST_DAYS} days"),
        )));
    }
    let mut counts: BTreeMap<String, SchemeUsageCount> = BTreeMap::new();
    // The records as the period ends: those standing, not replaced.
    for record in store.all()? {
        if !counts_as_standing(store, &record)? {
            continue;
        }
        let named = families(
            record
                .held_now()
                .map(|identifier| identifier.scheme.as_str()),
        );
        for name in &named {
            slot(&mut counts, name).instruments_carrying += 1;
        }
        if named.len() == 1 {
            if let Some(only) = named.iter().next() {
                slot(&mut counts, only).instruments_only_by += 1;
            }
        }
    }
    let mut day = Some(start);
    while let Some(date) = day.filter(|date| *date <= end) {
        let kept: BTreeMap<String, i64> = store
            .state(&format!("{MISSES_ON}{date}"))?
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        for (name, missed) in kept {
            if reportable(&name) {
                slot(&mut counts, &name).misses_only_by += missed;
            }
        }
        day = date.next();
    }
    let mut scheme_counts: Vec<SchemeUsageCount> = counts.into_values().collect();
    if scheme_counts.len() > MOST_SCHEMES {
        // Those only they name first: what the report is for.
        scheme_counts.sort_by(|a, b| {
            (
                b.instruments_only_by + b.misses_only_by,
                b.instruments_carrying,
            )
                .cmp(&(
                    a.instruments_only_by + a.misses_only_by,
                    a.instruments_carrying,
                ))
                .then_with(|| a.scheme.cmp(&b.scheme))
        });
        scheme_counts.truncate(MOST_SCHEMES);
        scheme_counts.sort_by(|a, b| a.scheme.cmp(&b.scheme));
    }
    Ok(Ok(ListIdentifierUsageReply { scheme_counts }))
}

/// The counts of one scheme, begun at none.
fn slot<'a>(
    counts: &'a mut BTreeMap<String, SchemeUsageCount>,
    name: &str,
) -> &'a mut SchemeUsageCount {
    counts
        .entry(name.to_string())
        .or_insert_with(|| SchemeUsageCount {
            scheme: name.to_string(),
            ..Default::default()
        })
}

/// A record counted: standing as the deployment's, neither replaced nor
/// decommissioned.
fn counts_as_standing(store: &dyn Store, record: &Instrument) -> Result<bool> {
    Ok(!record.local.decommissioned && store.replacement_of(&record.instrument_id)?.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    const NOW: i64 = 1_791_590_400_000_000_000; // 2026-10-10

    fn identifier(scheme: &str, value: &str) -> PbIdentifier {
        PbIdentifier {
            scheme: scheme.into(),
            value: value.into(),
            ..Default::default()
        }
    }

    fn ask(start: &str, end: &str) -> ListIdentifierUsageRequest {
        ListIdentifierUsageRequest {
            period_start: start.into(),
            period_end: end.into(),
        }
    }

    #[test]
    fn a_miss_counts_against_its_one_scheme_on_its_day_and_nothing_else() {
        let store = MemoryStore::default();
        note_miss(&store, &[identifier("cusip", "99999Z999")], NOW).unwrap();
        note_miss(&store, &[identifier("cusip", "99999Z998")], NOW).unwrap();
        // Two schemes: no single one to count it against.
        note_miss(
            &store,
            &[identifier("cusip", "1"), identifier("isin", "XX0000000000")],
            NOW,
        )
        .unwrap();
        // The platform's own ID beside one scheme still counts as that one.
        note_miss(
            &store,
            &[
                identifier("isin", "XX0000000001"),
                identifier(meridian_symbology::GLOBAL_ID, "x"),
            ],
            NOW,
        )
        .unwrap();
        let reply = list_identifier_usage(&store, &ask("2026-10-05", "2026-10-11"))
            .unwrap()
            .unwrap();
        let by: BTreeMap<_, _> = reply
            .scheme_counts
            .iter()
            .map(|count| (count.scheme.as_str(), count.misses_only_by))
            .collect();
        assert_eq!(by.get("cusip"), Some(&2));
        assert_eq!(by.get("isin"), Some(&1));
        // Outside the period: none.
        let before = list_identifier_usage(&store, &ask("2026-09-28", "2026-10-04"))
            .unwrap()
            .unwrap();
        assert!(before.scheme_counts.is_empty(), "{before:?}");
        // Never a value: the reply's text is scheme names and counts.
        let text = format!("{reply:?}");
        assert!(!text.contains("99999Z999") && !text.contains("XX0000000000"));
    }

    #[test]
    fn a_period_is_two_dates_in_order() {
        let store = MemoryStore::default();
        let refused = list_identifier_usage(&store, &ask("2026-10-11", "2026-10-05"))
            .unwrap()
            .unwrap_err();
        assert_eq!(refused.0, "period_end");
        let refused = list_identifier_usage(&store, &ask("last week", "2026-10-05"))
            .unwrap()
            .unwrap_err();
        assert_eq!(refused.0, "period_start");
    }

    #[test]
    fn the_families_are_what_a_report_names() {
        let named = families(
            [
                "figi",
                "share_class_figi",
                LOCAL_SCHEME,
                "open_meridian",
                "CUSIP",
            ]
            .into_iter(),
        );
        assert!(named.contains("cusip"));
        assert!(!named.contains(LOCAL_SCHEME) && !named.contains("open_meridian"));
    }
}
