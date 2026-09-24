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
//! Optional, and that matters for the other two branches: a deployment
//! signing people in through a provider or through LDAP configures no
//! database here, runs no migration, and keeps a dashboard that holds nothing
//! but its sessions.

use std::collections::HashMap;
use std::sync::Mutex;

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
        subject: format!("local|{}", account.name),
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

/// The accounts, in the database the deployment already has.
///
/// Its own tables, prefixed `dashboard_`, beside the conductor's `config_`
/// ones. Two components sharing a table is not supported; two components
/// keeping their own tables in one database is the arrangement this
/// deployment already runs.
pub struct InPostgres {
    pool: r2d2::Pool<r2d2_postgres::PostgresConnectionManager<postgres::NoTls>>,
}

/// Names this store's schema lock, distinct from every other component's, so
/// a deployment migrating them together does not have one wait on another.
const SCHEMA_LOCK: i64 = 0x6461_7368_626f_6172_u64 as i64;

const MIGRATION: &str = include_str!("../migrations/0001_local_account.sql");

impl InPostgres {
    pub fn connect(url: &str, pool_size: u32) -> Result<Self, String> {
        let config: postgres::Config = url.parse().map_err(|failed| format!("{failed}"))?;
        let manager = r2d2_postgres::PostgresConnectionManager::new(config, postgres::NoTls);
        let pool = r2d2::Pool::builder()
            .max_size(pool_size.max(1))
            .build(manager)
            .map_err(|failed| format!("{failed}"))?;
        Ok(Self { pool })
    }

    /// Apply the schema, under an advisory lock.
    ///
    /// Safe to call at every start: the statement creates if absent, and the
    /// lock means two starts cannot race. The dashboard runs at one replica,
    /// so this is belt and braces rather than the only thing holding.
    pub fn migrate(&self) -> Result<(), String> {
        let mut conn = self.conn()?;
        conn.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK])
            .map_err(|failed| format!("{failed}"))?;
        let outcome = conn
            .batch_execute(MIGRATION)
            .map_err(|failed| format!("the accounts table could not be made: {failed}"));
        let _ = conn.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK]);
        outcome
    }

    fn conn(
        &self,
    ) -> Result<
        r2d2::PooledConnection<r2d2_postgres::PostgresConnectionManager<postgres::NoTls>>,
        String,
    > {
        self.pool
            .get()
            .map_err(|failed| format!("no connection to the accounts database: {failed}"))
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
