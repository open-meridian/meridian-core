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

use meridian_domain::v1::{
    AccessGroup, AccessRecords, AccountGroup, AccountRecord, ExternalAccountLink, Permission,
    SignInRecord, UserGroup,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the configuration store is unavailable: {0}")]
    Unavailable(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A plugin the deployment has heard report (W4.8): what it is launched as.
/// Kept so an access entry can be refused for naming a tag the plugin does not
/// carry, including after a restart, before the plugin reports again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownPlugin {
    pub plugin_instance_id: String,
    pub role: String,
    pub tags: Vec<String>,
    pub last_reported_at_ns: i64,
}

impl KnownPlugin {
    /// A role is a grant-table entry like a tag, so an access entry may name
    /// either.
    pub fn carries(&self, tag: &str) -> bool {
        self.role == tag || self.tags.iter().any(|carried| carried == tag)
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

    fn add_permission(&self, permission: &Permission) -> Result<()>;

    /// Atomic: refuses the last permission to deployment admin.
    fn withdraw_permission(&self, permission_id: &str) -> Result<Withdrawal>;

    /// Atomic: writes the group and the permission only if no permission to
    /// deployment admin exists, and says whether it did.
    fn install_first_admin(&self, group: &UserGroup, permission: &Permission) -> Result<bool>;

    /// The latest sign-in stands; groups are never merged across sign-ins.
    fn record_sign_in(&self, record: &SignInRecord) -> Result<()>;

    fn record_plugin(&self, plugin: &KnownPlugin) -> Result<()>;

    /// An account this deployment holds, by the name somebody typed.
    ///
    /// Returns the hash and the lockout counters and never a password,
    /// because there is nowhere for one to be. Verifying is the service's,
    /// above this: a store that compared passwords would be a store that had
    /// to be told what hashing means.
    fn local_account(&self, name: &str) -> Result<Option<LocalAccount>>;

    /// Insert or replace, by name.
    fn put_local_account(&self, account: &LocalAccount) -> Result<()>;

    /// Count an attempt, and lock the account when there have been too many.
    ///
    /// Atomic, and it has to be: two sign-ins racing on a read-modify-write
    /// would each see the same count and store the same increment, so a
    /// threshold of five would admit an attacker running six at a time. The
    /// policy is the caller's and the arithmetic is here.
    fn count_sign_in_attempt(
        &self,
        name: &str,
        succeeded: bool,
        now_ns: i64,
        lock_after: i32,
        lock_for_ns: i64,
    ) -> Result<()>;
}

/// An account this deployment holds, as the store keeps it.
///
/// Not a protobuf message: nothing sends one of these anywhere. The wire
/// carries a name and a sealed password in and an answer out, and the hash
/// never leaves the process that reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalAccount {
    pub name: String,
    pub display_name: String,
    /// Argon2id in PHC string form, carrying its own salt and parameters.
    pub password_hash: String,
    pub groups: Vec<String>,
    pub failed_attempts: i32,
    /// Zero when not locked. A time rather than a flag, so a lock lifts by
    /// itself.
    pub locked_until_ns: i64,
    pub created_at_ns: i64,
}
