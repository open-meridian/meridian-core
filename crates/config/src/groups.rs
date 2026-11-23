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

/// A group defined anew, changed, or held when its records began
/// (backfilled, decisions/031).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupChangeKind {
    Defined,
    Changed,
    HeldWhenRecordsBegan,
}

impl GroupChangeKind {
    /// The `kind` column's number.
    pub fn code(self) -> i16 {
        match self {
            GroupChangeKind::Defined => 1,
            GroupChangeKind::Changed => 2,
            GroupChangeKind::HeldWhenRecordsBegan => 3,
        }
    }

    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(GroupChangeKind::Defined),
            2 => Some(GroupChangeKind::Changed),
            3 => Some(GroupChangeKind::HeldWhenRecordsBegan),
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
    pub at_ns: i64,
    /// True for the record a group standing when the records began was
    /// given.
    pub backfilled: bool,
    pub note: String,
}

/// What the backfilled record says (migration 35).
pub const HELD_WHEN_RECORDS_BEGAN: &str = "backfilled by migration 35: the group as it stood \
     when each user-group and account-group change began to be its own record; who defined it \
     or last changed it, and when, is not known";

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
        backfilled: false,
        note: String::new(),
    }
}
