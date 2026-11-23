//! Each user-group and account-group change, its own record (decision 47,
//! decisions/031; roadmap v31a item 13).
//!
//! Until migration 35 the store replaced a user group or an account group in
//! place and kept nothing of what it had been, who changed it or when. From
//! there each one defined or changed leaves a record: which group, what it
//! was, what it became, who made the change and through which delegation,
//! and when. A group standing when the records began has one backfilled
//! record, at the moment the migration ran, saying it as it stood then and
//! that who made it, and when, is not known. Store-only: the records are
//! read here, and no message carries them yet.

use meridian_domain::v1::{AccountGroup, UserGroup};

use crate::store::Author;

/// Which kind of group a record is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    User,
    Account,
}

impl GroupKind {
    /// The `group_kind` column's number.
    pub fn code(self) -> i16 {
        match self {
            GroupKind::User => 1,
            GroupKind::Account => 2,
        }
    }

    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(GroupKind::User),
            2 => Some(GroupKind::Account),
            _ => None,
        }
    }
}

/// A group defined anew, changed, held when its records began (migration
/// 35's own run), or defined or changed by an earlier release as an upgrade
/// ran (the last two backfilled, decisions/031).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupChangeKind {
    Defined,
    Changed,
    HeldWhenRecordsBegan,
    /// Found changed since its last record, or with none after migration 35
    /// ran (its `was` empty), by a copy of the release before still serving
    /// as the upgrade ran: what it was and became are known, who changed it
    /// and when are not.
    ChangedByAnEarlierRelease,
}

impl GroupChangeKind {
    /// The `kind` column's number.
    pub fn code(self) -> i16 {
        match self {
            GroupChangeKind::Defined => 1,
            GroupChangeKind::Changed => 2,
            GroupChangeKind::HeldWhenRecordsBegan => 3,
            GroupChangeKind::ChangedByAnEarlierRelease => 4,
        }
    }

    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(GroupChangeKind::Defined),
            2 => Some(GroupChangeKind::Changed),
            3 => Some(GroupChangeKind::HeldWhenRecordsBegan),
            4 => Some(GroupChangeKind::ChangedByAnEarlierRelease),
            _ => None,
        }
    }
}

/// One change to a user group or an account group, as its record says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupChangeRecord {
    pub group: GroupKind,
    pub group_id: String,
    pub kind: GroupChangeKind,
    /// The group as it was; empty for one defined anew, or held when the
    /// records began.
    pub was: String,
    /// As it became, or as it stood when the records began.
    pub became: String,
    /// Who made the change; empty where that is not known (a backfill).
    pub by: String,
    pub delegation: String,
    /// When it was recorded, by the deployment's clock: never back-dated.
    pub at_ns: i64,
    /// When the change was made (its valid time): the moment recorded for a
    /// change recorded as it was made; None where it is not known, as for
    /// every backfilled record (a group's row holds no date).
    pub valid_at_ns: Option<i64>,
    /// True for the record a group standing when the records began was
    /// given.
    pub backfilled: bool,
    pub note: String,
}

/// What a group standing as migration 35 ran is given, by that run alone
/// (kind 3). One found with no record later was defined by an earlier
/// release ([`DEFINED_BY_AN_EARLIER_RELEASE`], kind 4; review-core-3b D).
pub const HELD_WHEN_RECORDS_BEGAN: &str = "backfilled: the group as it stood when migration 35 \
     ran, each user-group and account-group change its own record from there; who defined it \
     or last changed it, and when, is not known";

/// What a gap record says: the group changed since its last record by a copy
/// of the release before, still serving while the upgrade ran.
pub const CHANGED_BY_AN_EARLIER_RELEASE: &str = "backfilled: changed by an earlier release \
     still serving as the upgrade ran, so no record was written with it; who changed it, and \
     when, are not known";

/// What a gap record says of a group found with no record after migration
/// 35 ran: defined by a copy of the release before, still serving while the
/// upgrade ran (kind 4, its `was` empty; review-core-3b D).
pub const DEFINED_BY_AN_EARLIER_RELEASE: &str = "backfilled: defined by an earlier release \
     still serving as the upgrade ran, so no record was written with it; who defined it, and \
     when, are not known";

/// A user group as a record says it: its name, its directory groups and its
/// logins.
pub fn described_user_group(group: &UserGroup) -> String {
    format!(
        "{:?}: directory groups {}; logins {}",
        group.name,
        listed(&group.directory_groups),
        listed(&group.logins)
    )
}

/// An account group as a record says it: its name and its accounts, or that
/// it is the built-in group holding every account.
pub fn described_account_group(group: &AccountGroup) -> String {
    if group.built_in {
        format!("{:?}: every account (built in)", group.name)
    } else {
        format!("{:?}: accounts {}", group.name, listed(&group.account_ids))
    }
}

fn listed(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// The record of a user group defined or changed; None when nothing changed.
pub fn user_group_change(
    before: Option<&UserGroup>,
    after: &UserGroup,
    author: &Author,
    at_ns: i64,
) -> Option<GroupChangeRecord> {
    if before == Some(after) {
        return None;
    }
    Some(record(
        GroupKind::User,
        &after.user_group_id,
        before.map(described_user_group),
        described_user_group(after),
        author,
        at_ns,
    ))
}

/// The record of an account group defined or changed; None when nothing
/// changed. Only the name and the accounts are compared: the built-in flag
/// is the store's, never a writer's.
pub fn account_group_change(
    before: Option<&AccountGroup>,
    after: &AccountGroup,
    author: &Author,
    at_ns: i64,
) -> Option<GroupChangeRecord> {
    let built_in = before.is_some_and(|group| group.built_in);
    let after = AccountGroup {
        built_in,
        ..after.clone()
    };
    if before == Some(&after) {
        return None;
    }
    Some(record(
        GroupKind::Account,
        &after.account_group_id,
        before.map(described_account_group),
        described_account_group(&after),
        author,
        at_ns,
    ))
}

/// The record a group is owed before its change is recorded, or as the
/// conductor starts, found as it stands as `standing` (decisions/031):
/// where it has none, `latest` being None, one saying an earlier release
/// defined it (kind 4, its `was` empty: migration 35's own run gave every
/// group standing then its kind 3, so one with none was defined after;
/// review-core-3b D); changed by an earlier release where it stands
/// otherwise than its latest record became; None where the records are
/// whole.
pub fn owed_before_change(
    group: GroupKind,
    group_id: &str,
    standing: &str,
    latest: Option<&str>,
    at_ns: i64,
) -> Option<GroupChangeRecord> {
    let (kind, was, note) = match latest {
        None => (
            GroupChangeKind::ChangedByAnEarlierRelease,
            String::new(),
            DEFINED_BY_AN_EARLIER_RELEASE,
        ),
        Some(became) if became == standing => return None,
        Some(became) => (
            GroupChangeKind::ChangedByAnEarlierRelease,
            became.to_string(),
            CHANGED_BY_AN_EARLIER_RELEASE,
        ),
    };
    Some(GroupChangeRecord {
        group,
        group_id: group_id.to_string(),
        kind,
        was,
        became: standing.to_string(),
        by: String::new(),
        delegation: String::new(),
        at_ns,
        valid_at_ns: None,
        backfilled: true,
        note: note.to_string(),
    })
}

fn record(
    group: GroupKind,
    group_id: &str,
    was: Option<String>,
    became: String,
    author: &Author,
    at_ns: i64,
) -> GroupChangeRecord {
    GroupChangeRecord {
        group,
        group_id: group_id.to_string(),
        kind: if was.is_some() {
            GroupChangeKind::Changed
        } else {
            GroupChangeKind::Defined
        },
        was: was.unwrap_or_default(),
        became,
        by: author.by.clone(),
        delegation: author.delegation.clone(),
        at_ns,
        valid_at_ns: Some(at_ns),
        backfilled: false,
        note: String::new(),
    }
}
