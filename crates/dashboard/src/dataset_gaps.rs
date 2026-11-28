//! A gap's notice (W10.14; contract v30, stage D; the resilience spec's
//! section 4, "a notice in the deployment admin's inbox"; the notice kind
//! `gap_found`, ruled 2026-11-11).
//!
//! Every [`ASK_EVERY`] the dashboard reads the lake's gaps
//! (`ListDatasetGaps`), and files `gap_found` to every deployment admin for
//! each gap it has not filed before: the unattributed one the moment the
//! writer records it (its dataset not yet known), and each dataset's gap
//! found from it as the dataset writes next. Once per gap: the gaps filed are
//! read back from the notices at start, so a restart files none again. A
//! gap's recovery files nothing else; the gap's line on Data sources says
//! its state. On the latency judge's path: the Inbox and
//! `dashboard__read_inbox`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use meridian_domain::v1::{AccessRecords, DatasetGap, GapCause, GapState};

use crate::latency::notices::Notice;
use crate::web::App;

/// The notice's kind (ruled 2026-11-11), a lowercase past tense as the
/// inbox's kinds are.
pub const GAP_FOUND: &str = "gap_found";

/// How often the dashboard asks the lake: a gap's notice is filed within
/// this of the lake recording it.
pub const ASK_EVERY: Duration = Duration::from_secs(10);

/// The most pages of gaps read at one ask, the lake's largest each.
const PAGES_MOST: usize = 8;

/// The gap's ID as a notice names it, first in its words, so the judge
/// reads back which gaps it filed.
fn gap_id_of(about: &str) -> Option<&str> {
    let at = about.find("GAP-")?;
    let rest = &about[at..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// A gap's notice's ID for one person: a ULID whose time is the gap's
/// recording and whose rest is the digest of the gap's ID and the person,
/// so every dashboard copy and every restart files the same notice, which
/// the store keeps once (ha runs two copies, each its own judge).
pub fn notice_id(gap: &DatasetGap, subject: &str) -> String {
    crate::latency::notices::keyed_id(gap.recorded_at_ns, &gap.gap_id, subject)
}

/// A time as a person reads it, to the second, in UTC.
fn at(ns: i64) -> String {
    crate::custody::utc(ns)
}

/// The dashboard's words for a gap: true of a run the stream dropped at
/// its bound and of a stream lost whole under its writer alike (the
/// consensus of 2026-11-28; its review's finding 1) -- rows lost before the
/// lake took them, which rows not known, possibly none. A gap that names
/// no dataset says its window, by the stream's time, as a dataset's says
/// its valid range.
pub fn about(gap: &DatasetGap) -> String {
    let cause = match GapCause::try_from(gap.cause).unwrap_or(GapCause::Unspecified) {
        GapCause::WriteAheadBound => {
            "rows the lake's write-ahead stream held were lost before the lake took them; \
             which rows is not known, possibly none"
        }
        GapCause::Unspecified => "its cause was not said",
    };
    if gap.dataset.is_empty() {
        format!(
            "{} in the lake's write-ahead stream, from {} to {} by the stream's time, \
             dataset not yet known: a dataset whose next write follows it is named then ({cause})",
            gap.gap_id,
            at(gap.valid_from_ns),
            at(gap.valid_until_ns)
        )
    } else {
        let state = match GapState::try_from(gap.state).unwrap_or(GapState::Unspecified) {
            GapState::Refetching => "asked again of its source",
            GapState::Recovered => "recovered",
            GapState::Unrecovered => "unrecovered: its source is not asked again",
            GapState::Unspecified => "its state not said",
        };
        format!(
            "{} in {}, valid from {} to {}: {state} ({cause})",
            gap.gap_id,
            gap.dataset,
            at(gap.valid_from_ns),
            at(gap.valid_until_ns)
        )
    }
}

/// Who is told: every deployment admin the records name -- each person who
/// has signed in, and each login a user group names -- so an admin who has
/// not signed in since finds it in their inbox.
pub fn told(records: &AccessRecords) -> BTreeSet<String> {
    records
        .people
        .iter()
        .map(|person| person.subject.clone())
        .chain(
            records
                .user_groups
                .iter()
                .flat_map(|group| group.logins.iter().cloned()),
        )
        .filter(|subject| crate::prepared::access_of(records, subject).deployment_admin)
        .collect()
}

/// The gaps not yet filed, oldest first, and their IDs marked filed.
pub fn unfiled(gaps: &[DatasetGap], filed: &mut BTreeSet<String>) -> Vec<DatasetGap> {
    let mut new: Vec<DatasetGap> = gaps
        .iter()
        .filter(|gap| !gap.gap_id.is_empty() && !filed.contains(&gap.gap_id))
        .cloned()
        .collect();
    new.sort_by(|a, b| (a.recorded_at_ns, &a.gap_id).cmp(&(b.recorded_at_ns, &b.gap_id)));
    new.dedup_by(|a, b| a.gap_id == b.gap_id);
    for gap in &new {
        filed.insert(gap.gap_id.clone());
    }
    new
}

/// Every gap the lake holds, its latest state each, up to [`PAGES_MOST`]
/// pages.
async fn every_gap(app: &App) -> Result<Vec<DatasetGap>, String> {
    let mut gaps = Vec::new();
    let mut cursor = String::new();
    for _ in 0..PAGES_MOST {
        let reply = crate::admin::data_sources::gaps(&app.bus, "", "", 0, 500, &cursor).await?;
        gaps.extend(reply.gaps);
        if reply.next_cursor.is_empty() {
            break;
        }
        cursor = reply.next_cursor;
    }
    Ok(gaps)
}

/// Ask the lake every [`ASK_EVERY`] and file what is new. The gaps already
/// filed are read from the notices first; until they are, nothing is filed.
/// The task is handed back, for the component to end before its runtime
/// stops.
pub fn judge_forever(app: Arc<App>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut filed: Option<BTreeSet<String>> = None;
        let mut every = tokio::time::interval(ASK_EVERY);
        loop {
            every.tick().await;
            let held = match filed.as_mut() {
                Some(held) => held,
                None => match crate::latency::notices::abouts(GAP_FOUND).await {
                    Ok(abouts) => filed.insert(
                        abouts
                            .iter()
                            .filter_map(|about| gap_id_of(about).map(str::to_string))
                            .collect(),
                    ),
                    Err(failed) => {
                        tracing::debug!(%failed, "the gaps' notices filed were not read; asked again");
                        continue;
                    }
                },
            };
            let gaps = match every_gap(&app).await {
                Ok(gaps) => gaps,
                Err(failed) => {
                    tracing::debug!(%failed, "the lake did not answer ListDatasetGaps; asked again");
                    continue;
                }
            };
            let now = app.clock.now_ns();
            let Ok(records) = app.records.current(now) else {
                tracing::warn!("a gap's notice waits for the access records, stale now");
                continue;
            };
            let mut before = held.clone();
            let new = unfiled(&gaps, &mut before);
            if new.is_empty() {
                continue;
            }
            let people = told(&records);
            let notices: Vec<Notice> = new
                .iter()
                .flat_map(|gap| {
                    tracing::warn!(
                        gap = gap.gap_id,
                        dataset = gap.dataset,
                        "a gap in the lake: its notice filed"
                    );
                    people.iter().map(|subject| Notice {
                        notice_id: notice_id(gap, subject),
                        subject: subject.clone(),
                        plugin_instance_id: "lake".to_string(),
                        kind: GAP_FOUND.to_string(),
                        about: about(gap),
                        changed_ns: now,
                    })
                })
                .collect();
            if notices.is_empty() {
                // Nobody to tell yet: kept unfiled, told once an admin is.
                continue;
            }
            let kept =
                tokio::task::spawn_blocking(move || crate::latency::notices::keep(&notices)).await;
            if matches!(kept, Ok(Ok(()))) {
                *held = before;
            } else {
                tracing::error!("a gap's notices were not kept; filed again at the next ask");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gap(id: &str, dataset: &str, at: i64, state: GapState) -> DatasetGap {
        DatasetGap {
            gap_id: id.into(),
            dataset: dataset.into(),
            valid_from_ns: 1,
            valid_until_ns: 2,
            cause: GapCause::WriteAheadBound as i32,
            state: state as i32,
            recorded_at_ns: at,
            ..Default::default()
        }
    }

    /// The review of the lost stream's gap, finding 1: a gap that names no
    /// dataset says its window, both times, and its words are true of a
    /// stream lost whole too -- which rows not known, possibly none --
    /// never that a writer was away past a bound.
    #[test]
    fn a_gap_naming_no_dataset_says_its_window_and_that_its_rows_are_not_known() {
        let mut lost = gap("GAP-01L", "", 5, GapState::Unrecovered);
        lost.valid_from_ns = 1_795_834_034_000_000_000;
        lost.valid_until_ns = 1_795_834_099_000_000_000;
        let words = about(&lost);
        for said in [
            at(lost.valid_from_ns),
            at(lost.valid_until_ns),
            "possibly none".to_string(),
            "dataset not yet known".to_string(),
        ] {
            assert!(words.contains(&said), "{said}: {words}");
        }
        assert_ne!(at(lost.valid_from_ns), at(lost.valid_until_ns));
        assert!(!words.contains("away past"), "{words}");
        // A dataset's own, of a lost stream: the same cause's words.
        let mut named = lost.clone();
        named.dataset = "drive-data:drive".into();
        let words = about(&named);
        assert!(
            words.contains("possibly none") && !words.contains("away past"),
            "{words}"
        );
    }

    /// Once per gap, oldest first: the unattributed one, then the dataset's;
    /// its recovery files nothing; read back from the notices' words.
    #[test]
    fn each_gap_is_filed_once_the_unattributed_first() {
        let mut filed = BTreeSet::new();
        let first = vec![gap("GAP-01B", "", 5, GapState::Unrecovered)];
        let new = unfiled(&first, &mut filed);
        assert_eq!(new.len(), 1);
        assert!(
            about(&new[0]).contains("dataset not yet known"),
            "{}",
            about(&new[0])
        );
        let later = vec![
            gap("GAP-01C", "drive-data:drive", 9, GapState::Unrecovered),
            gap("GAP-01B", "", 5, GapState::Unrecovered),
        ];
        let new = unfiled(&later, &mut filed);
        assert_eq!(
            new.iter().map(|g| g.gap_id.as_str()).collect::<Vec<_>>(),
            ["GAP-01C"]
        );
        assert!(about(&new[0]).starts_with("GAP-01C in drive-data:drive"));
        // Recovered later: nothing filed again.
        let recovered = vec![gap("GAP-01C", "drive-data:drive", 9, GapState::Recovered)];
        assert!(unfiled(&recovered, &mut filed).is_empty());
        // After a restart, read back from what was filed.
        let words = [about(&first[0]), about(&later[0])];
        let mut again: BTreeSet<String> = words
            .iter()
            .filter_map(|w| gap_id_of(w).map(str::to_string))
            .collect();
        assert!(unfiled(&later, &mut again).is_empty());
    }

    /// Every copy files the same notice for a gap and a person, a ULID
    /// sorted by when the gap was recorded; another person's is its own.
    #[test]
    fn a_gaps_notice_is_the_same_from_every_copy() {
        let one = gap(
            "GAP-01B",
            "",
            1_793_000_000_000_000_000,
            GapState::Unrecovered,
        );
        let id = notice_id(&one, "local|alice");
        assert_eq!(id, notice_id(&one, "local|alice"));
        assert_ne!(id, notice_id(&one, "local|bob"));
        assert_eq!(id.len(), 26);
        let later = gap(
            "GAP-01C",
            "",
            1_793_000_000_001_000_000,
            GapState::Unrecovered,
        );
        assert!(notice_id(&later, "local|alice") > id);
    }

    /// Every deployment admin the records name is told, signed in or not.
    #[test]
    fn every_deployment_admin_is_told() {
        use meridian_domain::v1::{SignInRecord, UserGroup};
        let records = AccessRecords {
            people: vec![SignInRecord {
                subject: "local|bob".into(),
                ..Default::default()
            }],
            user_groups: vec![UserGroup {
                user_group_id: "UG-ADMIN".into(),
                logins: vec!["local|alice".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        // Neither holds deployment admin without a permission: told nobody.
        assert!(told(&records).is_empty());
    }
}
