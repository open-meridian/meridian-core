//! Each delegation change, its own record (decision 47, decisions/031;
//! roadmap v31a item 13).
//!
//! A delegation's row holds what stands now: a renewal replaces what it
//! covers and its dates, a revocation marks it, and the sweep removes it a
//! month after it ended. From migration 16 each of those -- made, renewed,
//! revoked, its rows rewritten or its tools filled in by an upgrade, removed
//! from the list -- is also a record of its own, written with the change and
//! never updated or removed: which delegation, whose and to which client,
//! what it was and what it became (the dates a renewal replaced among them),
//! who made the change, and when. A delegation standing when the records
//! began has one backfilled record saying it as it stood then. Store-only:
//! the records are read here, and no page or message carries them yet.
//!
//! What a delegation is evaluated with as the person's directory says it
//! (its groups read at a sign-in or a token's issue) and when it was last
//! used or refused are readings, not changes to it, and leave no record.

use super::{Covers, Delegation};

/// What a change did to a delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Made at its person's consent.
    Made,
    /// Renewed in place at a fresh consent: what it covers and its dates
    /// replaced.
    Renewed,
    /// Revoked, by its person, an admin, its client or the dashboard.
    Revoked,
    /// Its rows rewritten once to name each plugin's role (contract v15).
    Rewritten,
    /// Its consented tools filled in once from what it reached (contract
    /// v18).
    ActingFilled,
    /// Removed from the list a month after it ended.
    Removed,
    /// As it stood when its records began (backfilled).
    HeldWhenRecordsBegan,
}

impl ChangeKind {
    /// The `kind` column's word.
    pub fn name(self) -> &'static str {
        match self {
            ChangeKind::Made => "made",
            ChangeKind::Renewed => "renewed",
            ChangeKind::Revoked => "revoked",
            ChangeKind::Rewritten => "rewritten",
            ChangeKind::ActingFilled => "acting_filled",
            ChangeKind::Removed => "removed",
            ChangeKind::HeldWhenRecordsBegan => "held_when_records_began",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [
            ChangeKind::Made,
            ChangeKind::Renewed,
            ChangeKind::Revoked,
            ChangeKind::Rewritten,
            ChangeKind::ActingFilled,
            ChangeKind::Removed,
            ChangeKind::HeldWhenRecordsBegan,
        ]
        .into_iter()
        .find(|kind| kind.name() == name)
    }
}

/// One change to a delegation, as its record says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationChange {
    pub delegation_id: String,
    /// Whose delegation it is.
    pub subject: String,
    pub client_id: String,
    pub kind: ChangeKind,
    /// The delegation as it was; empty for one made, or held when the
    /// records began.
    pub was: String,
    /// As it became; empty for one removed.
    pub became: String,
    /// Who made the change: the person consenting, the subject revoking (or
    /// `client`, or `dashboard`); empty for an upgrade's or the sweep's, and
    /// where it is not known (a backfill).
    pub by: String,
    pub at_ns: i64,
    pub backfilled: bool,
    pub note: String,
}

/// What a rewrite's record says (contract v15, W6.17).
pub const REWRITTEN: &str = "the one-time rewrite at contract v15, no person: each row \
     naming a plugin and a level now names the plugin's role where it holds exactly one";

/// What a fill's record says (contract v18).
pub const ACTING_FILLED: &str = "filled in once, no person: a delegation narrowed before \
     contract v18 given the tools that change something it reached when first evaluated";

/// What a removal's record says.
pub const REMOVED: &str = "removed from the list by the sweep, no person: it ended more than \
     30 days before";

/// What the backfilled record says (migration 16).
pub const HELD_WHEN_RECORDS_BEGAN: &str = "backfilled by migration 16: the delegation as it \
     stood when each delegation change began to be its own record; it was made at its \
     person's consent, but any renewal before the last one shown, what each changed, and who \
     made each change other than a revocation shown, are not known";

/// A delegation as a record says it: what it covers, its dates and, once
/// revoked, by whom, when and why.
pub fn described(delegation: &Delegation) -> String {
    let at = crate::terminal::rfc3339;
    let mut said = format!(
        "{}; made {}; renewed {}; expires {}",
        covered(&delegation.covers),
        at(delegation.made_at_ns),
        at(delegation.renewed_at_ns),
        at(delegation.expires_at_ns)
    );
    if let Some(revoked) = &delegation.revoked {
        said.push_str(&format!(
            "; revoked {} by {}: {}",
            at(revoked.at_ns),
            revoked.by,
            revoked.why
        ));
    }
    said
}

fn covered(covers: &Covers) -> String {
    if covers.everything {
        return "covers everything the person holds".to_string();
    }
    let listed = |names: Vec<String>| {
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    };
    let mut said = format!(
        "covers deployment admin {}; plugins {}; account groups {}",
        if covers.deployment_admin { "yes" } else { "no" },
        listed(
            covers
                .plugins
                .iter()
                .map(|(instance, role, level)| match role.is_empty() {
                    true => format!("{instance} {level}"),
                    false => format!("{instance} {role} {level}"),
                })
                .chain(
                    covers
                        .unmatched
                        .iter()
                        .map(|(instance, level)| format!("{instance} {level} (unmatched)"))
                )
                .collect()
        ),
        listed(covers.account_groups.iter().cloned().collect())
    );
    if let Some(acting) = &covers.acting {
        said.push_str(&format!(
            "; acting {}",
            listed(acting.iter().cloned().collect())
        ));
    }
    if !covers.prepare_only.is_empty() {
        said.push_str(&format!(
            "; prepare only {}",
            listed(covers.prepare_only.iter().cloned().collect())
        ));
    }
    said
}

/// The record of `before` becoming `after` (None for one made, or removed),
/// by `by` at `at_ns`.
pub fn change(
    kind: ChangeKind,
    before: Option<&Delegation>,
    after: Option<&Delegation>,
    by: &str,
    at_ns: i64,
    note: &str,
) -> DelegationChange {
    let either = after.or(before).expect("a change has a before or an after");
    DelegationChange {
        delegation_id: either.id.clone(),
        subject: either.subject.clone(),
        client_id: either.client_id.clone(),
        kind,
        was: before.map(described).unwrap_or_default(),
        became: after.map(described).unwrap_or_default(),
        by: by.to_string(),
        at_ns,
        backfilled: false,
        note: note.to_string(),
    }
}
