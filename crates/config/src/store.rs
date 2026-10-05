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
    AccessGroup, AccessRecords, AccountGroup, AccountRecord, ExternalAccountLink, Permission,
    PluginCatalogue, PluginLaunch, PluginLaunchState, PluginVersion, SignInRecord, UserGroup,
};
use meridian_pb::v1::SettingDeclaration;

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

/// Who made a settings change: the person the dashboard stamped, and the
/// delegation they acted through when they acted through a client (empty
/// otherwise). A plugin sets none of its settings (W6.11, option A).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsAuthor {
    pub by: String,
    pub delegation: String,
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

/// How a live launch ended: stopped by an administrator, or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    pub state: PluginLaunchState,
    pub by: String,
    pub at_ns: i64,
    pub failure: String,
}

impl Ending {
    /// The launch as it reads once ended this way.
    pub fn applied_to(&self, launch: &PluginLaunch) -> PluginLaunch {
        PluginLaunch {
            state: self.state as i32,
            stopped_by: self.by.clone(),
            stopped_at_ns: self.at_ns,
            failure: self.failure.clone(),
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
    fn put_access_group(&self, group: &AccessGroup) -> Result<()>;

    /// Insert or replace one link; an empty `account_id` removes it.
    fn put_link(&self, link: &ExternalAccountLink) -> Result<()>;

    /// Atomic: a new account and the link naming it, both or neither, so a
    /// link that creates its account is never left half-done (W6.4).
    fn put_account_and_link(
        &self,
        account: &AccountRecord,
        link: &ExternalAccountLink,
    ) -> Result<()>;

    fn add_permission(&self, permission: &Permission) -> Result<()>;

    /// Atomic: refuses the last permission to deployment admin.
    fn withdraw_permission(&self, permission_id: &str) -> Result<Withdrawal>;

    /// Atomic: writes the group and its permissions -- to deployment admin
    /// and to All plugins (admin) -- only if no permission to deployment
    /// admin exists, and says whether it did.
    fn install_first_admin(&self, group: &UserGroup, permissions: &[Permission]) -> Result<bool>;

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
    /// of one instance cannot both pass.
    fn begin_launch(&self, launch: &PluginLaunch) -> Result<bool>;

    /// End an instance's live launch, returning it as ended, or None when
    /// none was live.
    fn end_launch(&self, instance_id: &str, ending: &Ending) -> Result<Option<PluginLaunch>>;
}
