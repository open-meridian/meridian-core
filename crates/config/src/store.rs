//! What the configuration store holds, and what any store of it must provide.
//!
//! The records are the contract's own messages (`meridian.v1`, config.proto),
//! held as they are rather than mapped into a second set of types: they are
//! core's types already, and a copy would be a second place for a field to be
//! missing.
//!
//! Most rules are checked above the store, in [`crate::rules`], against a
//! snapshot. Two are not, because a check and a write in separate steps would
//! let two requests both pass: withdrawing the last permission to deployment
//! admin, and installing the first deployment admin. Those are single store
//! operations, and each store makes them atomic.

use std::collections::BTreeMap;

use meridian_domain::v1::{
    AccessGroup, AccessRecords, AccountGroup, AccountRecord, ExternalAccountLink, Hold,
    KnownPluginRoles, MoveRecord, Permission, PluginArchive, PluginCatalogue, PluginLaunch,
    PluginLaunchState, PluginVersion, SettingLastChange, SignInRecord, UserGroup,
};
use meridian_pb::v1::{MoveOutcome, SettingDeclaration, StoredSpan};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the configuration store is unavailable: {0}")]
    Unavailable(String),

    /// The database was migrated by a newer release than this binary. Its own
    /// variant because it is the one refusal at start that waiting never
    /// fixes: a starting conductor waits out every other one.
    #[error("{0}")]
    SchemaAhead(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A plugin the deployment has heard report (W4.8): what it is launched as.
/// Kept so an access entry can be refused for naming a plugin the deployment
/// has never heard of, including after a restart, before the plugin reports
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownPlugin {
    pub plugin_instance_id: String,
    pub roles: Vec<String>,
    pub last_reported_at_ns: i64,
}

/// A setting's value as the store holds it: as given, or sealed with the
/// deployment's settings key when the plugin declared it secret
/// ([`crate::sealing`]).
///
/// Its `Debug` says which and how long, never what: a value has no business
/// in a log line, and a secret is only ever held sealed.
#[derive(Clone, PartialEq, Eq)]
pub enum Held {
    Plain(String),
    Sealed(Vec<u8>),
}

impl std::fmt::Debug for Held {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Held::Plain(value) => write!(out, "Plain({} bytes)", value.len()),
            Held::Sealed(sealed) => write!(out, "Sealed({} bytes)", sealed.len()),
        }
    }
}

/// One setting a deployment admin gave a plugin (W6.11), and who last
/// changed it when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSetting {
    pub plugin_instance_id: String,
    pub name: String,
    pub held: Held,
    pub set_by: String,
    pub set_at_ns: i64,
}

/// A change to one setting: a value to hold, or `None` to clear it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingChange {
    pub name: String,
    pub held: Option<Held>,
}

/// Who made a change: the person the dashboard stamped, and the delegation
/// they acted through when they acted through a client, with the client's
/// name beside it (empty otherwise; the client from contract v17). Empty
/// `by` for a change no person made: a migration's, or a plugin's
/// re-declaration. A plugin sets none of its settings (W6.11, option A).
///
/// And why, where a change says (`note`, contract v17): kept with the
/// change's record, required through /mcp and optional at the page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Author {
    pub by: String,
    pub delegation: String,
    pub client: String,
    pub note: String,
}

/// The longest a change's note may be, as a note on an instrument record.
pub const MOST_NOTE: usize = 2_000;

/// The refusal of a change's note past its bound, naming `note`; None
/// within it.
pub fn note_refused(note: &str) -> Option<String> {
    (note.chars().count() > MOST_NOTE)
        .then(|| format!("note: a note is at most {MOST_NOTE} characters, and nothing was changed"))
}

/// Who made a settings change (W6.11).
pub type SettingsAuthor = Author;

/// What one change to an access group, or to a permission to one, left as its
/// own record (W6.7, W6.8, decisions/031; the plan's R1, contract v15): what
/// it was, what it became, who, through which delegation, and when. A gap
/// record says the group's history is not known before its time, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessChangeRecord {
    pub access_group_id: String,
    pub kind: AccessChangeKind,
    /// The group or the permission as it was; empty for one defined anew,
    /// granted, or a gap.
    pub was: String,
    /// As it became; empty for one withdrawn, or a gap.
    pub became: String,
    /// The permission granted or withdrawn; empty otherwise.
    pub permission_id: String,
    pub by: String,
    pub delegation: String,
    pub at_ns: i64,
    pub note: String,
}

/// An access group defined anew, or changed; its entries rewritten once at
/// the upgrade to contract v15; a permission to it granted or withdrawn; or a
/// gap, nothing known before the record's time (decisions/031, point 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessChangeKind {
    Defined,
    Changed,
    Rewritten,
    Granted,
    Withdrawn,
    NotKnownBefore,
}

impl AccessChangeKind {
    /// The `kind` column's number.
    pub fn code(self) -> i16 {
        match self {
            AccessChangeKind::Defined => 1,
            AccessChangeKind::Changed => 2,
            AccessChangeKind::Rewritten => 3,
            AccessChangeKind::Granted => 4,
            AccessChangeKind::Withdrawn => 5,
            AccessChangeKind::NotKnownBefore => 6,
        }
    }

    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(AccessChangeKind::Defined),
            2 => Some(AccessChangeKind::Changed),
            3 => Some(AccessChangeKind::Rewritten),
            4 => Some(AccessChangeKind::Granted),
            5 => Some(AccessChangeKind::Withdrawn),
            6 => Some(AccessChangeKind::NotKnownBefore),
            _ => None,
        }
    }
}

/// An access group as a change record says what it was and what it became:
/// its name, and each entry's plugin, role and level, in its order.
pub fn described_group(group: &AccessGroup) -> String {
    let entries: Vec<String> = group
        .entries
        .iter()
        .map(|entry| {
            let level = meridian_access::AccessLevel::try_from(entry.level)
                .map(meridian_access::level_name)
                .unwrap_or("no level");
            if entry.role.is_empty() {
                format!("{} {level}", entry.plugin_instance_id)
            } else {
                format!("{} {} {level}", entry.plugin_instance_id, entry.role)
            }
        })
        .collect();
    format!(
        "{:?}: {}",
        group.name,
        if entries.is_empty() {
            "no entries".to_string()
        } else {
            entries.join(", ")
        }
    )
}

/// A permission as a change record says it: the user group, and the account
/// group where it names one.
pub fn described_permission(permission: &Permission) -> String {
    if permission.account_group_id.is_empty() {
        format!("user group {}, no account group", permission.user_group_id)
    } else {
        format!(
            "user group {}, account group {}",
            permission.user_group_id, permission.account_group_id
        )
    }
}

/// The record of a change to an access group: defined when there was none
/// before, changed otherwise; None when nothing changed.
pub fn group_change(
    before: Option<&AccessGroup>,
    after: &AccessGroup,
    author: &Author,
    at_ns: i64,
) -> Option<AccessChangeRecord> {
    if before == Some(after) {
        return None;
    }
    Some(AccessChangeRecord {
        access_group_id: after.access_group_id.clone(),
        kind: if before.is_some() {
            AccessChangeKind::Changed
        } else {
            AccessChangeKind::Defined
        },
        was: before.map(described_group).unwrap_or_default(),
        became: described_group(after),
        permission_id: String::new(),
        by: author.by.clone(),
        delegation: author.delegation.clone(),
        at_ns,
        note: String::new(),
    })
}

/// The record of a permission granted or withdrawn.
pub fn permission_change(
    permission: &Permission,
    granted: bool,
    author: &Author,
    at_ns: i64,
) -> AccessChangeRecord {
    AccessChangeRecord {
        access_group_id: permission.access_group_id.clone(),
        kind: if granted {
            AccessChangeKind::Granted
        } else {
            AccessChangeKind::Withdrawn
        },
        was: if granted {
            String::new()
        } else {
            described_permission(permission)
        },
        became: if granted {
            described_permission(permission)
        } else {
            String::new()
        },
        permission_id: permission.permission_id.clone(),
        by: author.by.clone(),
        delegation: author.delegation.clone(),
        at_ns,
        note: String::new(),
    }
}

/// Why each access group's history before contract v15 is a gap.
pub const ACCESS_NOT_KNOWN_BEFORE: &str = "not known before: until contract v15 the store kept \
     each access group as it stood and when it was made, never a change to its entries or to \
     the permissions to it; each change since is its own record";

/// What the one-time rewrite's record says (W6.7; the spec's requirement 27).
pub const REWRITTEN_TO_NAME_ITS_ROLE: &str = "the one-time rewrite at the upgrade to contract \
     v15, no person: each entry naming no role on a plugin holding exactly one now names it, \
     as its sidecar last reported or its latest launch said; nobody's access moved";

/// The one role an entry naming none is rewritten to name (W6.7): the
/// plugin's, where it holds exactly one as its sidecar last reported, or --
/// where no sidecar has reported -- as its latest launch said. None where it
/// holds none or several, or nothing is known of it: the entry is left.
pub fn the_one_role<'a>(
    reported: Option<&'a [String]>,
    launched: Option<&'a [String]>,
) -> Option<&'a str> {
    match reported.or(launched) {
        Some([one]) => Some(one.as_str()),
        _ => None,
    }
}

/// What a change to one setting left as its own record (W6.11,
/// decisions/031): which setting, set or cleared, the value a setting that
/// is not secret was set to, who, through which delegation, and when.
/// A gap record names no setting's value: it says what is not known before
/// its time, and why (`note`), as a backfill does what it filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingChangeRecord {
    pub plugin_instance_id: String,
    pub name: String,
    pub kind: ChangeKind,
    /// Set for a setting that is not secret, set: what it was set to.
    pub value: Option<String>,
    pub secret: bool,
    pub by: String,
    pub delegation: String,
    /// The client's name beside the delegation (contract v17).
    pub client: String,
    pub at_ns: i64,
    pub backfilled: bool,
    pub note: String,
}

/// Set or replaced, cleared, a gap (nothing known before this record), or a
/// redaction: the values earlier records held blanked, because the setting
/// became secret (the product owner, 2026-10-05). A redaction blanks the value
/// and nothing else: no record is deleted, and none changes who or when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Set,
    Cleared,
    NotKnownBefore,
    Redacted,
}

impl ChangeKind {
    /// The `action` column's number.
    pub fn code(self) -> i16 {
        match self {
            ChangeKind::Set => 1,
            ChangeKind::Cleared => 2,
            ChangeKind::NotKnownBefore => 3,
            ChangeKind::Redacted => 4,
        }
    }

    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(ChangeKind::Set),
            2 => Some(ChangeKind::Cleared),
            3 => Some(ChangeKind::NotKnownBefore),
            4 => Some(ChangeKind::Redacted),
            _ => None,
        }
    }
}

/// Everything, read at once, so rules and derivations see one state.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Accounts, the three dimensions, permissions and the latest sign-in of
    /// each person: what access is evaluated from.
    pub records: AccessRecords,
    pub links: Vec<ExternalAccountLink>,
    pub plugins: Vec<KnownPlugin>,
    /// Every plugin version uploaded and every launch, live or ended (W8).
    pub catalogue: PluginCatalogue,
    /// What each plugin declared when it last registered, by instance (W4.8):
    /// what a setting is checked against, and how a secret is told.
    pub declared_settings: BTreeMap<String, Vec<SettingDeclaration>>,
    /// Every setting given to any plugin, secrets sealed (W6.11).
    pub settings: Vec<StoredSetting>,
    /// Each plugin's latest change to its settings, a clear included, from
    /// the change records: who the settings record says last changed them,
    /// and when. Empty `by` for a clear the plugin's re-declaration made.
    pub settings_changed: BTreeMap<String, LastChange>,
    /// Each setting's latest change, set or cleared, by instance, in name
    /// order (W6.11, contract v17): who, when, and the delegation and client,
    /// a secret's included and never a value.
    pub setting_changes: BTreeMap<String, Vec<SettingLastChange>>,
    /// The holds on raw records standing now (W6.25, contract v16), one per
    /// edge role or one for every role (an empty role), each as its latest
    /// record says it; a hold cleared (0 days) is not among them.
    pub holds: Vec<Hold>,
    /// Each instance's archive as its latest record says it (W8.7, contract
    /// v16), allowed or withdrawn; an instance never allowed one has none.
    pub archives: Vec<PluginArchive>,
}

/// One move of raw records as the store keeps it (W4.13, contract v16): its
/// number, which the moves' pages count back from, the instance, and the
/// move as recorded, the delegation and client the person acted through in
/// it where they used one (contract v17).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedMove {
    pub move_id: i64,
    pub instance_id: String,
    pub record: MoveRecord,
}

/// What a launch's or a stop's own record keeps beside the launch (W8.3,
/// W8.4, decisions/031, contract v17): the note it was made with, or for a
/// launch or stop made before v17, a gap record saying the delegation and
/// client it was made through are not known (`gap`), at the moment the
/// migration ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchNote {
    pub instance_id: String,
    pub launched_at_ns: i64,
    pub act: LaunchAct,
    pub note: String,
    pub gap: bool,
    pub at_ns: i64,
}

/// A launch, or its stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchAct {
    Launched,
    Stopped,
}

impl LaunchAct {
    /// The `act` column's number.
    pub fn code(self) -> i16 {
        match self {
            LaunchAct::Launched => 1,
            LaunchAct::Stopped => 2,
        }
    }

    pub fn from_code(code: i16) -> Option<Self> {
        match code {
            1 => Some(LaunchAct::Launched),
            2 => Some(LaunchAct::Stopped),
            _ => None,
        }
    }
}

/// What a gap record of a launch or a stop before v17 says (decisions/031,
/// point 4).
pub const LAUNCH_THROUGH_NOT_KNOWN: &str = "not known: before contract v17 a launch or a stop \
     was made through the terminal on a delegation, and the delegation and client it was made \
     through were not recorded; each since names them";

/// Whether the archive holds a unit whose latest move this is: archived,
/// restored from it, or returned to it; not once deleted.
pub fn in_the_archive(outcome: i32) -> bool {
    [
        MoveOutcome::Archived as i32,
        MoveOutcome::Restored as i32,
        MoveOutcome::Returned as i32,
    ]
    .contains(&outcome)
}

/// What the archive holds of each kind (W6.9): the units whose latest move
/// leaves them in it, summed per kind, from the first record received to
/// the last. Given each unit's latest move, in any order.
pub fn archived_spans<'a>(
    latest: impl IntoIterator<Item = &'a meridian_pb::v1::RecordMoveRequest>,
) -> Vec<StoredSpan> {
    let mut kinds: BTreeMap<String, StoredSpan> = BTreeMap::new();
    for held in latest {
        if !in_the_archive(held.outcome) {
            continue;
        }
        let span = kinds
            .entry(held.record_kind.clone())
            .or_insert_with(|| StoredSpan {
                record_kind: held.record_kind.clone(),
                ..Default::default()
            });
        span.record_count += held.record_count;
        if span.first_received_ns == 0 || held.first_received_ns < span.first_received_ns {
            span.first_received_ns = held.first_received_ns;
        }
        span.last_received_ns = span.last_received_ns.max(held.last_received_ns);
    }
    kinds.into_values().collect()
}

/// Each known plugin's roles as the records carry them (W6.1, contract v15).
pub fn known_plugins(plugins: &[KnownPlugin]) -> Vec<KnownPluginRoles> {
    plugins
        .iter()
        .map(|plugin| KnownPluginRoles {
            plugin_instance_id: plugin.plugin_instance_id.clone(),
            roles: plugin.roles.clone(),
        })
        .collect()
}

/// Who made a plugin's latest settings change, and when.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastChange {
    pub by: String,
    pub at_ns: i64,
}

/// Why a re-declaration clears a setting's held value: its type or whether it
/// is secret changed, or it was declared again after a declaration without
/// it, so the value held was set under a declaration no longer known.
/// `None` when the value held still stands under the new declaration.
pub fn redeclared(before: Option<&SettingDeclaration>, now: &SettingDeclaration) -> Option<String> {
    let kind = |declaration: &SettingDeclaration| -> String {
        let named = meridian_pb::v1::SettingType::try_from(declaration.r#type)
            .map(|kind| {
                kind.as_str_name()
                    .trim_start_matches("SETTING_TYPE_")
                    .to_ascii_lowercase()
            })
            .unwrap_or_else(|_| format!("type {}", declaration.r#type));
        if declaration.secret {
            format!("a secret {named}")
        } else {
            format!("{named}, not secret")
        }
    };
    match before {
        None => Some(format!(
            "cleared by the plugin's re-declaration, no person: {} was declared again, as {}, \
             after a declaration without it, and its value was set under one no longer known",
            now.name,
            kind(now)
        )),
        Some(before) if before.r#type != now.r#type || before.secret != now.secret => {
            Some(format!(
                "cleared by the plugin's re-declaration, no person: {} was {} and is now {}, \
                 and its value was set under the earlier declaration",
                now.name,
                kind(before),
                kind(now)
            ))
        }
        Some(_) => None,
    }
}

/// What a redaction record says: which records' values it blanked, and why.
pub fn redaction_note(change_ids: &[i64], why: &str) -> String {
    let ids: Vec<String> = change_ids.iter().map(ToString::to_string).collect();
    format!(
        "redacted the value of change record{} {}: {why}",
        if ids.len() == 1 { "" } else { "s" },
        ids.join(", ")
    )
}

/// Why a re-declaration redacts: the setting became secret.
pub const REDACTED_BY_REDECLARATION: &str =
    "the setting became secret; by the plugin's re-declaration, no person";

/// Why storing a sealed value redacts: the setting's value became sealed.
pub const REDACTED_BY_SEALING: &str =
    "the setting's value became sealed, a secret; by its being set so";

/// How a live launch ended: stopped by an administrator, through the
/// delegation and client they used and with their note (contract v17), or
/// failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    pub state: PluginLaunchState,
    pub by: String,
    pub delegation: String,
    pub client: String,
    pub note: String,
    pub at_ns: i64,
    pub failure: String,
}

impl Ending {
    /// A launch the launcher failed, no person's.
    pub fn failed(at_ns: i64, failure: String) -> Ending {
        Ending {
            state: PluginLaunchState::Failed,
            by: String::new(),
            delegation: String::new(),
            client: String::new(),
            note: String::new(),
            at_ns,
            failure,
        }
    }

    /// The launch as it reads once ended this way.
    pub fn applied_to(&self, launch: &PluginLaunch) -> PluginLaunch {
        PluginLaunch {
            state: self.state as i32,
            stopped_by: self.by.clone(),
            stopped_at_ns: self.at_ns,
            failure: self.failure.clone(),
            stopped_through_delegation: self.delegation.clone(),
            stopped_client_name: self.client.clone(),
            stopped_note: self.note.clone(),
            ..launch.clone()
        }
    }
}

/// What withdrawing a permission did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withdrawal {
    Withdrawn,
    Unknown,
    /// Refused: it is the last permission to deployment admin, and a
    /// deployment is never left without an administrator.
    LastAdmin,
}

pub trait Store: Send + Sync {
    fn snapshot(&self) -> Result<Snapshot>;

    /// Insert or replace, by identifier.
    fn put_account(&self, account: &AccountRecord) -> Result<()>;
    fn put_user_group(&self, group: &UserGroup) -> Result<()>;
    fn put_account_group(&self, group: &AccountGroup) -> Result<()>;
    /// Insert or replace an access group, and record the change as its own
    /// record, in one step (W6.7, decisions/031): defined or changed, what it
    /// was and what it became, by `author` at `at_ns`. Nothing is recorded
    /// when nothing changed.
    fn put_access_group(&self, group: &AccessGroup, author: &Author, at_ns: i64) -> Result<()>;

    /// Insert or replace one link; an empty `account_id` removes it.
    fn put_link(&self, link: &ExternalAccountLink) -> Result<()>;

    /// Atomic: a new account and the link naming it, both or neither, so a
    /// link that creates its account is never left half-done (W6.4).
    fn put_account_and_link(
        &self,
        account: &AccountRecord,
        link: &ExternalAccountLink,
    ) -> Result<()>;

    /// A permission granted, recorded as its own record in the same step
    /// (W6.8).
    fn add_permission(&self, permission: &Permission, author: &Author, at_ns: i64) -> Result<()>;

    /// Atomic: refuses the last permission to deployment admin. A
    /// withdrawal is recorded as its own record in the same step (W6.8).
    fn withdraw_permission(
        &self,
        permission_id: &str,
        author: &Author,
        at_ns: i64,
    ) -> Result<Withdrawal>;

    /// Atomic: writes the group and its permissions -- to deployment admin
    /// and to All plugins (admin) -- only if no permission to deployment
    /// admin exists, and says whether it did; each permission recorded as
    /// granted by `author`.
    fn install_first_admin(
        &self,
        group: &UserGroup,
        permissions: &[Permission],
        author: &Author,
        at_ns: i64,
    ) -> Result<bool>;

    /// Every change recorded for one access group and the permissions to it,
    /// gap and rewrite records included, in the order recorded.
    fn access_changes(&self, access_group_id: &str) -> Result<Vec<AccessChangeRecord>>;

    /// The latest sign-in stands; groups are never merged across sign-ins.
    fn record_sign_in(&self, record: &SignInRecord) -> Result<()>;

    fn record_plugin(&self, plugin: &KnownPlugin) -> Result<()>;

    /// Replace what a plugin declared it needs. Written only from the report
    /// of a registered plugin, so one that is between registrations keeps
    /// what it last declared.
    ///
    /// In the same step, a value held for a setting [`redeclared`] is
    /// cleared, the clear its own change record naming the re-declaration,
    /// no person, as its cause; and a setting declared secret has every
    /// value its earlier change records hold redacted, the redaction its own
    /// record ([`ChangeKind::Redacted`]). Returns the settings cleared.
    fn record_declared_settings(
        &self,
        plugin_instance_id: &str,
        declared: &[SettingDeclaration],
        at_ns: i64,
    ) -> Result<Vec<String>>;

    /// Apply every change to one plugin's settings, and record each as its
    /// own change -- the value of one that is not secret, never a secret's --
    /// with who made it and when, all in one step. A sealed value stored
    /// redacts the values that setting's earlier records hold, as a
    /// re-declaration as secret does.
    fn put_plugin_settings(
        &self,
        plugin_instance_id: &str,
        changes: &[SettingChange],
        author: &SettingsAuthor,
        at_ns: i64,
    ) -> Result<()>;

    /// Every change recorded for one plugin's settings, gap records
    /// included, in the order recorded.
    fn plugin_setting_changes(&self, plugin_instance_id: &str) -> Result<Vec<SettingChangeRecord>>;

    /// Record an uploaded version, unless that name and version is recorded
    /// already: false then, and the recorded one stands. An uploaded version
    /// is never replaced (W8).
    fn record_plugin_version(&self, version: &PluginVersion) -> Result<bool>;

    /// Record a launch as live, unless its instance has a live launch
    /// already: false then. Checked and written in one step, so two launches
    /// of one instance cannot both pass. Who launched it, through which
    /// delegation and client, are the launch's own; its note is kept with it
    /// (contract v17).
    fn begin_launch(&self, launch: &PluginLaunch, note: &str) -> Result<bool>;

    /// End an instance's live launch, returning it as ended, or None when
    /// none was live; a stop's note kept with it.
    fn end_launch(&self, instance_id: &str, ending: &Ending) -> Result<Option<PluginLaunch>>;

    /// Each launch's and stop's note of one instance, and the gap records of
    /// those made before v17, in the order recorded (contract v17).
    fn launch_notes(&self, instance_id: &str) -> Result<Vec<LaunchNote>>;

    /// A hold set, changed or cleared (0 days), as its own record (W6.25,
    /// decisions/031): the role, the days and write-once as set, who
    /// (`hold.updated_by`), through which delegation and client, and when
    /// (`hold.updated_at_ns`), and why (`note`). The snapshot's holds are
    /// each role's latest.
    fn set_hold(&self, hold: &Hold, note: &str) -> Result<()>;

    /// Every hold change, in the order recorded, each with its note.
    fn hold_changes(&self) -> Result<Vec<(Hold, String)>>;

    /// An instance's archive allowed, its bound changed, or withdrawn, as its
    /// own record (W8.7, decisions/031): who, through what and when are the
    /// archive's own, and why `note`.
    fn put_archive(&self, archive: &PluginArchive, note: &str) -> Result<()>;

    /// Every change to one instance's archive, in the order recorded, each
    /// with its note.
    fn archive_changes(&self, instance_id: &str) -> Result<Vec<(PluginArchive, String)>>;

    /// A move of raw records, as its own record (W4.13, decisions/031), the
    /// delegation and client the record names with it; false, and nothing
    /// recorded, when the unit's latest move is the same outcome -- a retry,
    /// answered as recorded.
    fn record_move(&self, instance_id: &str, record: &MoveRecord) -> Result<bool>;

    /// The latest move of one unit of a kind, if it ever moved.
    fn latest_move(
        &self,
        instance_id: &str,
        record_kind: &str,
        unit: &str,
    ) -> Result<Option<MoveRecord>>;

    /// One instance's moves, newest first: at most `limit`, those numbered
    /// below `before` where it is not 0.
    fn moves(&self, instance_id: &str, before: i64, limit: usize) -> Result<Vec<RecordedMove>>;

    /// What the archive holds of each of an instance's kinds
    /// ([`archived_spans`]).
    fn archived(&self, instance_id: &str) -> Result<Vec<StoredSpan>>;
}
