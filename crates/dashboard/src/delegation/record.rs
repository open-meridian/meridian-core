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
//! who made the change, and when it was made (its valid time) as well as when
//! it was recorded. A delegation standing when the records began is given,
//! from its row, the records the row holds the facts of -- made, last
//! renewed, its tools filled in, revoked -- each marked backfilled, at the
//! moment they are written, with the valid time the row holds. Store-only:
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
    /// Lapsed at its expiry, unrevoked: found by the sweep, valid when it
    /// expired.
    Lapsed,
    /// Removed from the list a month after it ended; or found removed by
    /// a copy of the release before still serving as the upgrade ran
    /// (backfilled: who and when not known).
    Removed,
    /// Found changed since its last record, by a copy of the release before
    /// still serving as the upgrade ran (backfilled): what it was and
    /// became are known, who changed it and when are not.
    ChangedByAnEarlierRelease,
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
            ChangeKind::Lapsed => "lapsed",
            ChangeKind::Removed => "removed",
            ChangeKind::ChangedByAnEarlierRelease => "changed_by_an_earlier_release",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [
            ChangeKind::Made,
            ChangeKind::Renewed,
            ChangeKind::Revoked,
            ChangeKind::Rewritten,
            ChangeKind::ActingFilled,
            ChangeKind::Lapsed,
            ChangeKind::Removed,
            ChangeKind::ChangedByAnEarlierRelease,
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
    /// The delegation as it was; empty for one made, and where it is not
    /// known (a backfilled renewal's).
    pub was: String,
    /// As it became; empty for one removed, and where it is not known (a
    /// backfilled making's, renewed since).
    pub became: String,
    /// Who made the change: the person consenting, the subject revoking (or
    /// `client`, or `dashboard`); empty for an upgrade's or the sweep's, and
    /// where it is not known (a backfill).
    pub by: String,
    /// When it was recorded, by the deployment's clock: never back-dated.
    pub at_ns: i64,
    /// When the change was made (its valid time): the moment recorded for a
    /// change recorded as it was made; the row's for a backfilled one; None
    /// where it is not known.
    pub valid_at_ns: Option<i64>,
    /// True for a record written after the change, from what its row held
    /// (decisions/031).
    pub backfilled: bool,
    pub note: String,
}

/// What a rewrite's record says (contract v15, W6.17).
pub const REWRITTEN: &str = "the one-time rewrite at contract v15, no person: each row \
     naming a plugin and a level now names the plugin's role where it holds exactly one";

/// What a fill's record says (contract v18).
pub const ACTING_FILLED: &str = "filled in once, no person: a delegation narrowed before \
     contract v18 given the tools that change something it reached when first evaluated";

/// What a lapse's record says (decisions/031 names it).
pub const LAPSED: &str = "lapsed at its expiry, no person: found by the sweep, valid when it \
     expired; what it covered is unchanged, and it is no longer live";

/// The record of a delegation lapsing at its expiry, found by the sweep at
/// `at_ns`: valid when it expired, by no person, the delegation itself
/// unchanged.
pub fn lapsed(delegation: &Delegation, at_ns: i64) -> DelegationChange {
    DelegationChange {
        valid_at_ns: Some(delegation.expires_at_ns),
        ..change(
            ChangeKind::Lapsed,
            Some(delegation),
            Some(delegation),
            "",
            at_ns,
            LAPSED,
        )
    }
}

/// What a removal's record says.
pub const REMOVED: &str = "removed from the list by the sweep, no person: it ended more than \
     30 days before";

/// What a backfilled making's record says (migration 16).
pub const MADE_BACKFILLED: &str = "backfilled from the delegation's row, which holds when it \
     was made, at its person's consent; what it covered then is what the row holds, or not \
     known where it was renewed since, and any rewrite at contract v15 is not known";

/// What a backfilled renewal's record says.
pub const RENEWED_BACKFILLED: &str = "backfilled from the delegation's row, which holds when it \
     was last renewed, at its person's consent; what it covered before, and any renewal \
     before this one, are not known";

/// What a backfilled fill's record says.
pub const ACTING_FILLED_BACKFILLED: &str = "backfilled from the delegation's row, which holds \
     when its consented tools were filled in once, no person (contract v18)";

/// What a gap record says: the delegation changed since its last record by
/// a copy of the release before, still serving while the upgrade ran.
pub const CHANGED_BY_AN_EARLIER_RELEASE: &str = "backfilled: changed by an earlier release \
     still serving as the upgrade ran, so no record was written with it; who changed it, and \
     when, are not known beyond what it became says";

/// What a removal's gap record says: removed by a copy of the release
/// before, still serving as the upgrade ran, which wrote no record.
pub const REMOVED_BY_AN_EARLIER_RELEASE: &str = "backfilled: removed from the list by an \
     earlier release still serving as the upgrade ran, so no record was written with it; what \
     it was is its last record's, and who removed it, and when, are not known";

/// What a backfilled revocation's record says.
pub const REVOKED_BACKFILLED: &str = "backfilled from the delegation's row, which holds when \
     it was revoked, by whom and why";

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
        valid_at_ns: Some(at_ns),
        backfilled: false,
        note: note.to_string(),
    }
}

/// The records a delegation's row holds the facts of, for one that has none
/// (decisions/031): made, by its person, when the row says; renewed, by its
/// person, where it was renewed since; its tools filled in, where they
/// were; revoked, by whom the row says. Each backfilled, recorded at
/// `at_ns`, valid when the row says, each one's `was` the one before's
/// `became`.
pub fn backfilled(delegation: &Delegation, at_ns: i64) -> Vec<DelegationChange> {
    let renewed = delegation.renewed_at_ns != delegation.made_at_ns;
    // As it stood before it was revoked, and before its tools were filled.
    let unrevoked = Delegation {
        revoked: None,
        ..delegation.clone()
    };
    let unfilled = match delegation.acting_backfilled_at_ns {
        Some(_) => Delegation {
            covers: Covers {
                acting: None,
                ..unrevoked.covers.clone()
            },
            acting_backfilled_at_ns: None,
            ..unrevoked.clone()
        },
        None => unrevoked.clone(),
    };
    let one =
        |kind, was: String, became: String, by: &str, valid_at_ns, note: &str| DelegationChange {
            delegation_id: delegation.id.clone(),
            subject: delegation.subject.clone(),
            client_id: delegation.client_id.clone(),
            kind,
            was,
            became,
            by: by.to_string(),
            at_ns,
            valid_at_ns: Some(valid_at_ns),
            backfilled: true,
            note: note.to_string(),
        };
    let mut records = vec![one(
        ChangeKind::Made,
        String::new(),
        if renewed {
            String::new()
        } else {
            described(&unfilled)
        },
        &delegation.subject,
        delegation.made_at_ns,
        MADE_BACKFILLED,
    )];
    if renewed {
        records.push(one(
            ChangeKind::Renewed,
            String::new(),
            described(&unfilled),
            &delegation.subject,
            delegation.renewed_at_ns,
            RENEWED_BACKFILLED,
        ));
    }
    if let Some(filled_at) = delegation.acting_backfilled_at_ns {
        records.push(one(
            ChangeKind::ActingFilled,
            described(&unfilled),
            described(&unrevoked),
            "",
            filled_at,
            ACTING_FILLED_BACKFILLED,
        ));
    }
    if let Some(revoked) = &delegation.revoked {
        records.push(one(
            ChangeKind::Revoked,
            described(&unrevoked),
            described(delegation),
            &revoked.by,
            revoked.at_ns,
            REVOKED_BACKFILLED,
        ));
    }
    records
}

/// The records a delegation is owed before its change is recorded, found as
/// it stands as `standing` (decisions/031; review-core-3 1), recorded at
/// `at_ns`: those its row holds the facts of where it has none, `latest`
/// being None ([`backfilled`]); one saying it was changed by an earlier
/// release where it stands otherwise than its latest record became; none
/// where its records are whole.
pub fn owed_before_change(
    standing: &Delegation,
    latest: Option<&str>,
    at_ns: i64,
) -> Vec<DelegationChange> {
    let now = described(standing);
    match latest {
        None => backfilled(standing, at_ns),
        Some(became) if became == now => Vec::new(),
        Some(became) => vec![DelegationChange {
            delegation_id: standing.id.clone(),
            subject: standing.subject.clone(),
            client_id: standing.client_id.clone(),
            kind: ChangeKind::ChangedByAnEarlierRelease,
            was: became.to_string(),
            became: now,
            by: String::new(),
            at_ns,
            valid_at_ns: None,
            backfilled: true,
            note: CHANGED_BY_AN_EARLIER_RELEASE.to_string(),
        }],
    }
}
