//! Accounts this deployment holds itself.
//!
//! Decision 018, for a firm with no directory of its own. The other two
//! branches never reach here: a firm's provider and a firm's LDAP each check
//! their own passwords, and this deployment learns only who signed in.
//!
//! Here rather than in the conductor's store, which is where the access
//! records live, for one reason that settles it: **the dashboard already has
//! the password.** It served the form the password arrived on. Keeping the
//! hashes anywhere else would mean sending that password somewhere to be
//! compared against them -- a key exchange, a sealed credential on the bus,
//! and a key rotation to handle on every restart -- to protect a value the
//! component doing the sealing had in hand the whole time.
//!
//! Used only on this branch: a deployment signing people in through a
//! provider or through LDAP keeps no account here. Its dashboard still uses
//! the same database, for terminal sessions ([`crate::database`]).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::database::Database;

/// Failures before an account is locked.
///
/// Low, because these accounts belong to the handful of people administering
/// a deployment and nobody types five wrong passwords in earnest.
pub const LOCK_AFTER: i32 = 5;

/// And for how long: long enough that guessing costs more than it is worth,
/// short enough that somebody locked out before lunch is working after it.
pub const LOCK_FOR_NS: i64 = 15 * 60 * 1_000_000_000;

/// A hash to check nothing against.
///
/// Verified when no account matches, so an unknown name costs what a known
/// one does. Argon2 takes long enough to measure, and skipping it for names
/// that are not there turns the sign-in page into a way to ask whether
/// somebody works here.
const ABSENT: &str = "$argon2id$v=19$m=19456,t=2,p=1$YWJzZW50YWJzZW50YWI$\
                      3f5nQ2ZqRMHK0mVxvVeU3xPxCTLVxWAMd3lDbGLSJhQ";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalAccount {
    pub name: String,
    pub display_name: String,
    /// Argon2id, PHC string form. Never a password.
    pub password_hash: String,
    pub groups: Vec<String>,
    pub failed_attempts: i32,
    /// Zero when not locked.
    pub locked_until_ns: i64,
    pub created_at_ns: i64,
}

/// Where the accounts are kept.
pub trait Accounts: Send + Sync {
    fn by_name(&self, name: &str) -> Result<Option<LocalAccount>, String>;
    fn put(&self, account: &LocalAccount) -> Result<(), String>;

    /// Count an attempt, and lock when there have been too many.
    ///
    /// One statement where it can be: two attempts racing on a
    /// read-modify-write each see the same count and store the same
    /// increment, so a threshold of five admits somebody running six at once
    /// -- which is the shape of guessing the threshold exists for.
    fn count_attempt(&self, name: &str, succeeded: bool, now_ns: i64) -> Result<(), String>;
}

/// Failed sign-ins by the name typed, whether or not an account has it, so the
/// warning before the lock reads the same for every name: an account that
/// exists is said to only by the lock, as it always was (W6.16). Advisory and
/// in memory; the account's own count is what locks.
#[derive(Default)]
pub struct Failures {
    by_name: Mutex<HashMap<String, (i32, i64)>>,
}

impl Failures {
    /// One more failure for this name; how many there have been within the
    /// lock's span.
    pub fn failed(&self, name: &str, now_ns: i64) -> i32 {
        let mut held = self.by_name.lock().expect("failures lock poisoned");
        held.retain(|_, (_, last)| now_ns - *last <= LOCK_FOR_NS);
        let entry = held.entry(keyed(name)).or_insert((0, now_ns));
        entry.0 += 1;
        entry.1 = now_ns;
        entry.0
    }

    pub fn clear(&self, name: &str) {
        self.by_name
            .lock()
            .expect("failures lock poisoned")
            .remove(&keyed(name));
    }
}

/// What the sign-in page adds after a failure: nothing until the third, then
/// how many attempts are left before the lock.
pub fn warning(failures: i32) -> String {
    let left = LOCK_AFTER - failures;
    match left {
        l if l >= LOCK_AFTER - 2 => String::new(),
        1 => " One more attempt locks this username for 15 minutes.".to_string(),
        l if l > 1 => format!(" {l} more attempts before this username is locked for 15 minutes."),
        _ => String::new(),
    }
}

/// The name as it is stored and matched, so one person is one login.
fn keyed(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Hash a password for storage, with a fresh salt.
pub fn hash_password(password: &str) -> Result<String, String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    let salt = SaltString::generate(&mut OsRng);
    argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|failed| format!("a password could not be hashed: {failed}"))
}

/// Who signed in, or why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    SignedIn {
        subject: String,
        display_name: String,
        groups: Vec<String>,
    },
    /// The name or the password. Which, deliberately unsaid.
    Refused,
    /// Said plainly: somebody locked out and not told keeps trying and cannot
    /// tell it from a wrong password.
    Locked,
    /// Ours, not theirs.
    Unavailable(String),
}

/// W6.1, the branch where this deployment holds the account.
pub fn authenticate(accounts: &dyn Accounts, name: &str, password: &str, now_ns: i64) -> Outcome {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};

    let account = match accounts.by_name(name) {
        Ok(found) => found,
        Err(failed) => return Outcome::Unavailable(failed),
    };

    // Before the hash: a locked account should not learn whether it guessed
    // right, and verifying would cost time for nothing.
    if let Some(account) = &account {
        if account.locked_until_ns > now_ns {
            return Outcome::Locked;
        }
    }

    let stored = account
        .as_ref()
        .map(|a| a.password_hash.as_str())
        .unwrap_or(ABSENT);
    let matched = PasswordHash::new(stored)
        .map(|parsed| {
            argon2::Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false);

    // An empty password never matches a stored hash, so it needs no guard
    // here -- unlike LDAP, where an empty one is a bind the server may answer
    // with success.
    let Some(account) = account else {
        return Outcome::Refused;
    };

    if let Err(failed) = accounts.count_attempt(&account.name, matched, now_ns) {
        // A store that cannot be written is a store that cannot lock, and
        // admitting somebody then removes the limit exactly when it matters.
        return Outcome::Unavailable(failed);
    }

    if !matched {
        return Outcome::Refused;
    }

    Outcome::SignedIn {
        // `local` and the name, because an account here has no issuer of its
        // own. Half of every permission ever granted, so it is settled once
        // and never changed ([[design/naming-a-person-before-they-sign-in]]).
        subject: meridian_access::local_login(&account.name),
        display_name: account.display_name.clone(),
        groups: account.groups.clone(),
    }
}

/// For tests, and for a deployment that has configured no database because it
/// signs people in some other way.
#[derive(Default)]
pub struct InMemory {
    held: Mutex<HashMap<String, LocalAccount>>,
}

impl Accounts for InMemory {
    fn by_name(&self, name: &str) -> Result<Option<LocalAccount>, String> {
        Ok(self
            .held
            .lock()
            .expect("accounts lock poisoned")
            .get(&keyed(name))
            .cloned())
    }

    fn put(&self, account: &LocalAccount) -> Result<(), String> {
        let mut held = self.held.lock().expect("accounts lock poisoned");
        let mut stored = account.clone();
        stored.name = keyed(&account.name);
        // A replace leaves the counters alone: changing somebody's password
        // should not lift a lock.
        if let Some(existing) = held.get(&stored.name) {
            stored.failed_attempts = existing.failed_attempts;
            stored.locked_until_ns = existing.locked_until_ns;
        }
        held.insert(stored.name.clone(), stored);
        Ok(())
    }

    fn count_attempt(&self, name: &str, succeeded: bool, now_ns: i64) -> Result<(), String> {
        let mut held = self.held.lock().expect("accounts lock poisoned");
        let Some(account) = held.get_mut(&keyed(name)) else {
            return Ok(());
        };
        if succeeded {
            account.failed_attempts = 0;
            account.locked_until_ns = 0;
            return Ok(());
        }
        account.failed_attempts += 1;
        if account.failed_attempts >= LOCK_AFTER {
            account.locked_until_ns = now_ns + LOCK_FOR_NS;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "accounts/tests.rs"]
mod tests;

// ── In Postgres ──────────────────────────────────────────────────────────────

/// The accounts, in the dashboard's own tables ([`crate::database`]).
pub struct InPostgres {
    database: Database,
}

impl InPostgres {
    /// On a database already verified at start.
    pub fn on(database: Database) -> Self {
        Self { database }
    }

    fn conn(&self) -> Result<crate::database::Connection, String> {
        self.database.conn()
    }
}

impl Accounts for InPostgres {
    fn by_name(&self, name: &str) -> Result<Option<LocalAccount>, String> {
        let rows = self
            .conn()?
            .query(
                "SELECT name, display_name, password_hash, groups, failed_attempts,
                        locked_until_ns, created_at_ns
                   FROM dashboard_local_account WHERE name = $1",
                &[&keyed(name)],
            )
            .map_err(|failed| format!("{failed}"))?;
        Ok(rows.first().map(|row| LocalAccount {
            name: row.get(0),
            display_name: row.get(1),
            password_hash: row.get(2),
            groups: row.get(3),
            failed_attempts: row.get(4),
            locked_until_ns: row.get(5),
            created_at_ns: row.get(6),
        }))
    }

    fn put(&self, account: &LocalAccount) -> Result<(), String> {
        self.conn()?
            .execute(
                "INSERT INTO dashboard_local_account
                     (name, display_name, password_hash, groups, failed_attempts,
                      locked_until_ns, created_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (name) DO UPDATE
                    SET display_name = excluded.display_name,
                        password_hash = excluded.password_hash,
                        groups = excluded.groups",
                &[
                    &keyed(&account.name),
                    &account.display_name,
                    &account.password_hash,
                    &account.groups,
                    &account.failed_attempts,
                    &account.locked_until_ns,
                    &account.created_at_ns,
                ],
            )
            .map_err(|failed| format!("{failed}"))?;
        Ok(())
    }

    fn count_attempt(&self, name: &str, succeeded: bool, now_ns: i64) -> Result<(), String> {
        let name = keyed(name);
        if succeeded {
            self.conn()?
                .execute(
                    "UPDATE dashboard_local_account
                        SET failed_attempts = 0, locked_until_ns = 0
                      WHERE name = $1",
                    &[&name],
                )
                .map_err(|failed| format!("{failed}"))?;
            return Ok(());
        }
        // One statement, so two attempts racing cannot both read the same
        // count and store the same increment.
        self.conn()?
            .execute(
                "UPDATE dashboard_local_account
                    SET failed_attempts = failed_attempts + 1,
                        -- Cast, because two bare parameters added together
                        -- give Postgres nothing to infer from: `unknown +
                        -- unknown` has no unique operator, and the statement
                        -- fails at execution rather than at compile time.
                        locked_until_ns = CASE
                            WHEN failed_attempts + 1 >= $2::integer
                            THEN $3::bigint + $4::bigint
                            ELSE locked_until_ns
                        END
                  WHERE name = $1",
                &[&name, &LOCK_AFTER, &now_ns, &LOCK_FOR_NS],
            )
            .map_err(|failed| format!("{failed}"))?;
        Ok(())
    }
}
