//! Where clients, delegations and their tokens are kept: the dashboard's own
//! tables in the deployment's database, so a restart or an upgrade leaves
//! them standing and revoking one ends it on every replica; or this
//! process's memory, for tests and for a dashboard given no database.
//!
//! Tokens by fingerprint, never themselves. Every change that must not
//! interleave with another -- spending a refresh token, making or renewing a
//! delegation -- is one statement or one transaction, so two replicas, or a
//! request and the sweep, cannot both win.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use super::{
    Client, Covers, Delegation, Grant, Kind, Revoked, Token, ToolCall, CALLS_KEPT_NS, KEPT_NS,
    UNCONSENTED_NS,
};
use crate::database::{said, Database};
use crate::session::token;

/// A store for delegations. Blocking: [`super::Delegations`] calls it off the
/// async runtime.
pub trait DelegationStore: Send + Sync {
    /// Keep a new client, making room at the cap by removing the oldest
    /// nobody consented to. False when there is no room even so.
    fn register(&self, client: &Client, max: usize) -> Result<bool, String>;

    fn client(&self, client_id: &str) -> Result<Option<Client>, String>;

    /// Make the person's delegation to the client, or renew the standing one
    /// (requirement 2): renewing keeps its id, takes what the consent covers,
    /// and removes every token issued on it before, so only the new pair
    /// works. The client is marked consented.
    fn grant(&self, grant: &Grant, now_ns: i64) -> Result<Delegation, String>;

    fn delegation(&self, id: &str) -> Result<Option<Delegation>, String>;

    fn issue(&self, token: &Token) -> Result<(), String>;

    fn token(&self, fingerprint: &str) -> Result<Option<Token>, String>;

    /// Spend a refresh token: true when this call spent it, false when it
    /// had been spent already (or is gone).
    fn spend(&self, fingerprint: &str, now_ns: i64) -> Result<bool, String>;

    /// Revoke one delegation, if it is not already. True when this revoked it.
    fn revoke(&self, id: &str, by: &str, why: &str, now_ns: i64) -> Result<bool, String>;

    /// Revoke every delegation a person holds, and say how many.
    fn revoke_person(
        &self,
        subject: &str,
        by: &str,
        why: &str,
        now_ns: i64,
    ) -> Result<usize, String>;

    /// A person's delegations, live first, then the newest.
    fn of_person(&self, subject: &str, now_ns: i64) -> Result<Vec<Delegation>, String>;

    /// Who holds a live delegation, by subject, with a name and a count.
    fn holders(&self, now_ns: i64) -> Result<Vec<(String, String, usize)>, String>;

    fn used(&self, id: &str, now_ns: i64) -> Result<(), String>;

    fn refused(&self, id: &str, why: &str, now_ns: i64) -> Result<(), String>;

    /// Every delegation narrowed at consent, revoked or not, by when made.
    fn narrowed(&self) -> Result<Vec<Delegation>, String>;

    /// Replace a delegation's rows with the same rows rewritten to name each
    /// plugin's role (W6.17, contract v15); nothing else about it changes.
    fn rewrite(&self, id: &str, covers: &Covers) -> Result<(), String>;

    fn groups_read(&self, id: &str, groups: &[String], now_ns: i64) -> Result<(), String>;

    /// A fresh sign-in: the person's groups on every unrevoked delegation of
    /// theirs.
    fn signed_in(&self, subject: &str, groups: &[String], now_ns: i64) -> Result<(), String>;

    /// Remove tokens past their life, delegations revoked or lapsed for
    /// longer than they stay listed, and clients nobody consented to within
    /// a day.
    fn sweep(&self, now_ns: i64) -> Result<(), String>;

    /// Record one call through the deployment's MCP surface (W6.20,
    /// contract v12): never an argument or an answer.
    fn record_call(&self, call: &ToolCall) -> Result<(), String>;

    /// The latest calls, newest first, at most `limit`: on one delegation,
    /// or every one of a person's.
    fn calls(&self, of: &CallsOf, limit: usize) -> Result<Vec<ToolCall>, String>;
}

/// Whose calls to list.
#[derive(Clone, Debug)]
pub enum CallsOf {
    Delegation(String),
    Person(String),
}

/// How long a token is kept past its life, so a request on one just expired
/// is told it expired rather than that it is unknown.
const TOKEN_KEPT_NS: i64 = crate::clock::HOUR_NS;

fn order(delegations: &mut [Delegation], now_ns: i64) {
    delegations.sort_by(|a, b| {
        b.live(now_ns)
            .cmp(&a.live(now_ns))
            .then(b.renewed_at_ns.cmp(&a.renewed_at_ns))
            .then(a.id.cmp(&b.id))
    });
}

// ── In memory ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Kept {
    clients: HashMap<String, Client>,
    delegations: HashMap<String, Delegation>,
    tokens: HashMap<String, Token>,
    calls: Vec<ToolCall>,
}

impl Kept {
    fn with_name(&self, mut delegation: Delegation) -> Delegation {
        delegation.client_name = self
            .clients
            .get(&delegation.client_id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        delegation
    }

    fn revoke(&mut self, id: &str, by: &str, why: &str, now_ns: i64) -> bool {
        match self.delegations.get_mut(id) {
            Some(delegation) if delegation.revoked.is_none() => {
                delegation.revoked = Some(Revoked {
                    at_ns: now_ns,
                    by: by.to_string(),
                    why: why.to_string(),
                });
                true
            }
            _ => false,
        }
    }
}

/// For tests, and for a dashboard given no database, where a restart ends
/// every delegation.
#[derive(Default)]
pub struct InMemory {
    kept: Mutex<Kept>,
}

impl InMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().expect("delegation store lock poisoned")
    }

    /// Every token's fingerprint, for a test that no token is kept.
    #[cfg(test)]
    pub(crate) fn fingerprints(&self) -> Vec<String> {
        self.lock().tokens.keys().cloned().collect()
    }
}

impl DelegationStore for InMemory {
    fn register(&self, client: &Client, max: usize) -> Result<bool, String> {
        let mut kept = self.lock();
        while kept.clients.len() >= max {
            let oldest = kept
                .clients
                .values()
                .filter(|c| !c.consented)
                .min_by_key(|c| c.registered_at_ns)
                .map(|c| c.client_id.clone());
            match oldest {
                Some(id) => {
                    kept.clients.remove(&id);
                }
                None => return Ok(false),
            }
        }
        kept.clients
            .insert(client.client_id.clone(), client.clone());
        Ok(true)
    }

    fn client(&self, client_id: &str) -> Result<Option<Client>, String> {
        Ok(self.lock().clients.get(client_id).cloned())
    }

    fn grant(&self, grant: &Grant, now_ns: i64) -> Result<Delegation, String> {
        let mut kept = self.lock();
        let Some(client) = kept.clients.get_mut(&grant.client_id) else {
            return Err(format!("no client {}", grant.client_id));
        };
        client.consented = true;
        let standing = kept
            .delegations
            .values()
            .find(|d| {
                d.subject == grant.subject && d.client_id == grant.client_id && d.revoked.is_none()
            })
            .map(|d| d.id.clone());
        let delegation = match standing {
            Some(id) => {
                kept.tokens.retain(|_, t| t.delegation_id != id);
                let delegation = kept.delegations.get_mut(&id).expect("found above");
                delegation.display_name = grant.display_name.clone();
                delegation.covers = grant.covers.clone();
                delegation.renewed_at_ns = now_ns;
                delegation.expires_at_ns = grant.expires_at_ns;
                delegation.directory_groups = grant.directory_groups.clone();
                delegation.groups_read_at_ns = now_ns;
                delegation.clone()
            }
            None => {
                let delegation = Delegation {
                    id: token(),
                    subject: grant.subject.clone(),
                    display_name: grant.display_name.clone(),
                    client_id: grant.client_id.clone(),
                    client_name: String::new(),
                    covers: grant.covers.clone(),
                    made_at_ns: now_ns,
                    renewed_at_ns: now_ns,
                    expires_at_ns: grant.expires_at_ns,
                    revoked: None,
                    last_used_at_ns: None,
                    last_refusal: None,
                    directory_groups: grant.directory_groups.clone(),
                    groups_read_at_ns: now_ns,
                };
                kept.delegations
                    .insert(delegation.id.clone(), delegation.clone());
                delegation
            }
        };
        Ok(kept.with_name(delegation))
    }

    fn delegation(&self, id: &str) -> Result<Option<Delegation>, String> {
        let kept = self.lock();
        Ok(kept.delegations.get(id).cloned().map(|d| kept.with_name(d)))
    }

    fn issue(&self, token: &Token) -> Result<(), String> {
        self.lock()
            .tokens
            .insert(token.fingerprint.clone(), token.clone());
        Ok(())
    }

    fn token(&self, fingerprint: &str) -> Result<Option<Token>, String> {
        Ok(self.lock().tokens.get(fingerprint).cloned())
    }

    fn spend(&self, fingerprint: &str, _now_ns: i64) -> Result<bool, String> {
        Ok(match self.lock().tokens.get_mut(fingerprint) {
            Some(token) if !token.spent => {
                token.spent = true;
                true
            }
            _ => false,
        })
    }

    fn revoke(&self, id: &str, by: &str, why: &str, now_ns: i64) -> Result<bool, String> {
        Ok(self.lock().revoke(id, by, why, now_ns))
    }

    fn revoke_person(
        &self,
        subject: &str,
        by: &str,
        why: &str,
        now_ns: i64,
    ) -> Result<usize, String> {
        let mut kept = self.lock();
        let theirs: Vec<String> = kept
            .delegations
            .values()
            .filter(|d| d.subject == subject)
            .map(|d| d.id.clone())
            .collect();
        Ok(theirs
            .iter()
            .filter(|id| kept.revoke(id, by, why, now_ns))
            .count())
    }

    fn of_person(&self, subject: &str, now_ns: i64) -> Result<Vec<Delegation>, String> {
        let kept = self.lock();
        let mut theirs: Vec<Delegation> = kept
            .delegations
            .values()
            .filter(|d| d.subject == subject)
            .cloned()
            .map(|d| kept.with_name(d))
            .collect();
        order(&mut theirs, now_ns);
        Ok(theirs)
    }

    fn holders(&self, now_ns: i64) -> Result<Vec<(String, String, usize)>, String> {
        let kept = self.lock();
        let mut counted: HashMap<&str, (&str, usize)> = HashMap::new();
        for delegation in kept.delegations.values().filter(|d| d.live(now_ns)) {
            let entry = counted
                .entry(delegation.subject.as_str())
                .or_insert((delegation.display_name.as_str(), 0));
            entry.0 = entry.0.min(delegation.display_name.as_str());
            entry.1 += 1;
        }
        let mut holders: Vec<_> = counted
            .into_iter()
            .map(|(subject, (name, n))| (subject.to_string(), name.to_string(), n))
            .collect();
        holders.sort();
        Ok(holders)
    }

    fn used(&self, id: &str, now_ns: i64) -> Result<(), String> {
        if let Some(delegation) = self.lock().delegations.get_mut(id) {
            delegation.last_used_at_ns = Some(delegation.last_used_at_ns.unwrap_or(0).max(now_ns));
        }
        Ok(())
    }

    fn refused(&self, id: &str, why: &str, now_ns: i64) -> Result<(), String> {
        if let Some(delegation) = self.lock().delegations.get_mut(id) {
            delegation.last_refusal = Some((now_ns, why.to_string()));
        }
        Ok(())
    }

    fn narrowed(&self) -> Result<Vec<Delegation>, String> {
        let mut narrowed: Vec<Delegation> = self
            .lock()
            .delegations
            .values()
            .filter(|delegation| !delegation.covers.everything)
            .cloned()
            .collect();
        narrowed.sort_by_key(|delegation| delegation.made_at_ns);
        Ok(narrowed)
    }

    fn rewrite(&self, id: &str, covers: &Covers) -> Result<(), String> {
        if let Some(delegation) = self.lock().delegations.get_mut(id) {
            delegation.covers.plugins = covers.plugins.clone();
            delegation.covers.unmatched = covers.unmatched.clone();
        }
        Ok(())
    }

    fn groups_read(&self, id: &str, groups: &[String], now_ns: i64) -> Result<(), String> {
        if let Some(delegation) = self.lock().delegations.get_mut(id) {
            delegation.directory_groups = groups.to_vec();
            delegation.groups_read_at_ns = now_ns;
        }
        Ok(())
    }

    fn signed_in(&self, subject: &str, groups: &[String], now_ns: i64) -> Result<(), String> {
        for delegation in self
            .lock()
            .delegations
            .values_mut()
            .filter(|d| d.subject == subject && d.revoked.is_none())
        {
            delegation.directory_groups = groups.to_vec();
            delegation.groups_read_at_ns = now_ns;
        }
        Ok(())
    }

    fn sweep(&self, now_ns: i64) -> Result<(), String> {
        let mut kept = self.lock();
        kept.tokens
            .retain(|_, t| now_ns <= t.expires_at_ns + TOKEN_KEPT_NS);
        kept.delegations.retain(|_, d| {
            let ended = d
                .revoked
                .as_ref()
                .map(|r| r.at_ns)
                .unwrap_or(d.expires_at_ns)
                .min(d.expires_at_ns);
            now_ns <= ended + KEPT_NS
        });
        let delegations: BTreeSet<String> = kept.delegations.keys().cloned().collect();
        kept.tokens
            .retain(|_, t| delegations.contains(&t.delegation_id));
        kept.clients
            .retain(|_, c| c.consented || now_ns - c.registered_at_ns <= UNCONSENTED_NS);
        kept.calls
            .retain(|call| now_ns - call.called_at_ns <= CALLS_KEPT_NS);
        Ok(())
    }

    fn record_call(&self, call: &ToolCall) -> Result<(), String> {
        self.lock().calls.push(call.clone());
        Ok(())
    }

    fn calls(&self, of: &CallsOf, limit: usize) -> Result<Vec<ToolCall>, String> {
        let kept = self.lock();
        Ok(kept
            .calls
            .iter()
            .rev()
            .filter(|call| match of {
                CallsOf::Delegation(id) => &call.delegation_id == id,
                CallsOf::Person(subject) => &call.subject == subject,
            })
            .take(limit)
            .cloned()
            .collect())
    }
}

// ── In Postgres ──────────────────────────────────────────────────────────────

/// Delegations in `dashboard_oauth_client`, `dashboard_delegation` and
/// `dashboard_delegation_token`.
pub struct InPostgres {
    database: Database,
}

impl InPostgres {
    /// On a database already verified at start.
    pub fn on(database: Database) -> Self {
        Self { database }
    }
}

const DELEGATION_COLUMNS: &str = "d.delegation_id, d.subject, d.display_name, d.client_id, \
     c.name, d.covers_everything, d.covers_deployment_admin, d.covers_plugins, \
     d.covers_account_groups, d.made_at_ns, d.renewed_at_ns, d.expires_at_ns, \
     d.revoked_at_ns, d.revoked_by, d.revoked_why, d.last_used_at_ns, \
     d.last_refused_at_ns, d.last_refusal, d.directory_groups, d.groups_read_at_ns";

fn delegation_of(row: &postgres::Row) -> Delegation {
    let plugins: Vec<String> = row.get(7);
    let revoked_at: Option<i64> = row.get(12);
    let refused_at: Option<i64> = row.get(16);
    Delegation {
        id: row.get(0),
        subject: row.get(1),
        display_name: row.get(2),
        client_id: row.get(3),
        client_name: row.get(4),
        covers: Covers {
            everything: row.get(5),
            deployment_admin: row.get(6),
            plugins: rows_of(&plugins).0,
            unmatched: rows_of(&plugins).1,
            account_groups: row.get::<_, Vec<String>>(8).into_iter().collect(),
        },
        made_at_ns: row.get(9),
        renewed_at_ns: row.get(10),
        expires_at_ns: row.get(11),
        revoked: revoked_at.map(|at_ns| Revoked {
            at_ns,
            by: row.get::<_, Option<String>>(13).unwrap_or_default(),
            why: row.get::<_, Option<String>>(14).unwrap_or_default(),
        }),
        last_used_at_ns: row.get(15),
        last_refusal: refused_at
            .map(|at| (at, row.get::<_, Option<String>>(17).unwrap_or_default())),
        directory_groups: row.get(18),
        groups_read_at_ns: row.get(19),
    }
}

/// A delegation's rows as kept: `instance:role:level` from v15, the role
/// empty for a plugin holding none, and an unmatched row from before as it
/// was recorded, `instance:level`.
fn plugins_of(covers: &Covers) -> Vec<String> {
    covers
        .plugins
        .iter()
        .map(|(instance, role, level)| format!("{instance}:{role}:{level}"))
        .chain(
            covers
                .unmatched
                .iter()
                .map(|(instance, level)| format!("{instance}:{level}")),
        )
        .collect()
}

/// The rows as kept, read back: three parts a row of v15, two a row recorded
/// before, unmatched until the rewrite names its role.
pub(crate) fn rows_of(kept: &[String]) -> (BTreeSet<(String, String, String)>, BTreeSet<(String, String)>) {
    let mut rows = BTreeSet::new();
    let mut unmatched = BTreeSet::new();
    for entry in kept {
        let parts: Vec<&str> = entry.split(':').collect();
        match parts[..] {
            [instance, role, level] => {
                rows.insert((instance.to_string(), role.to_string(), level.to_string()));
            }
            [instance, level] => {
                unmatched.insert((instance.to_string(), level.to_string()));
            }
            _ => {}
        }
    }
    (rows, unmatched)
}

fn client_of(row: &postgres::Row) -> Client {
    Client {
        client_id: row.get(0),
        name: row.get(1),
        redirect_uris: row.get(2),
        software_id: row.get(3),
        registered_at_ns: row.get(4),
        consented: row.get(5),
    }
}

fn token_of(row: &postgres::Row) -> Token {
    let kind: String = row.get(1);
    let spent: Option<i64> = row.get(6);
    Token {
        fingerprint: row.get(0),
        kind: if kind == "refresh" {
            Kind::Refresh
        } else {
            Kind::Access
        },
        resource: row.get(2),
        delegation_id: row.get(3),
        client_id: row.get(4),
        expires_at_ns: row.get(5),
        spent: spent.is_some(),
    }
}

impl InPostgres {
    fn find(
        conn: &mut impl postgres::GenericClient,
        id: &str,
    ) -> Result<Option<Delegation>, String> {
        Ok(conn
            .query_opt(
                &format!(
                    "SELECT {DELEGATION_COLUMNS} FROM dashboard_delegation d
                       JOIN dashboard_oauth_client c USING (client_id)
                      WHERE d.delegation_id = $1"
                ),
                &[&id],
            )
            .map_err(said)?
            .as_ref()
            .map(delegation_of))
    }
}

impl DelegationStore for InPostgres {
    fn register(&self, client: &Client, max: usize) -> Result<bool, String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        // One registration at a time decides the cap, so two at once cannot
        // both take the last place.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext('dashboard_oauth_client'))",
            &[],
        )
        .map_err(said)?;
        let held: i64 = tx
            .query_one("SELECT count(*) FROM dashboard_oauth_client", &[])
            .map_err(said)?
            .get(0);
        let over = held + 1 - max as i64;
        if over > 0 {
            let removed = tx
                .execute(
                    "DELETE FROM dashboard_oauth_client WHERE client_id IN (
                         SELECT client_id FROM dashboard_oauth_client WHERE NOT consented
                          ORDER BY registered_at_ns LIMIT $1)",
                    &[&over],
                )
                .map_err(said)?;
            if (removed as i64) < over {
                return Ok(false);
            }
        }
        tx.execute(
            "INSERT INTO dashboard_oauth_client
                 (client_id, name, redirect_uris, software_id, registered_at_ns, consented)
             VALUES ($1, $2, $3, $4, $5, false)",
            &[
                &client.client_id,
                &client.name,
                &client.redirect_uris,
                &client.software_id,
                &client.registered_at_ns,
            ],
        )
        .map_err(said)?;
        tx.commit().map_err(said)?;
        Ok(true)
    }

    fn client(&self, client_id: &str) -> Result<Option<Client>, String> {
        Ok(self
            .database
            .conn()?
            .query_opt(
                "SELECT client_id, name, redirect_uris, software_id, registered_at_ns, consented
                   FROM dashboard_oauth_client WHERE client_id = $1",
                &[&client_id],
            )
            .map_err(said)?
            .as_ref()
            .map(client_of))
    }

    fn grant(&self, grant: &Grant, now_ns: i64) -> Result<Delegation, String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(said)?;
        let consented = tx
            .execute(
                "UPDATE dashboard_oauth_client SET consented = true WHERE client_id = $1",
                &[&grant.client_id],
            )
            .map_err(said)?;
        if consented == 0 {
            return Err(format!("no client {}", grant.client_id));
        }
        let plugins = plugins_of(&grant.covers);
        let groups: Vec<String> = grant.covers.account_groups.iter().cloned().collect();
        // The standing one renewed, or a new one: the partial unique index
        // makes the two one statement, whichever replica asks first.
        let id: String = tx
            .query_one(
                "INSERT INTO dashboard_delegation
                     (delegation_id, subject, display_name, client_id, covers_everything,
                      covers_deployment_admin, covers_plugins, covers_account_groups,
                      made_at_ns, renewed_at_ns, expires_at_ns, directory_groups,
                      groups_read_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9, $10, $11, $9)
                 ON CONFLICT (subject, client_id) WHERE revoked_at_ns IS NULL DO UPDATE
                    SET display_name = excluded.display_name,
                        covers_everything = excluded.covers_everything,
                        covers_deployment_admin = excluded.covers_deployment_admin,
                        covers_plugins = excluded.covers_plugins,
                        covers_account_groups = excluded.covers_account_groups,
                        renewed_at_ns = excluded.renewed_at_ns,
                        expires_at_ns = excluded.expires_at_ns,
                        directory_groups = excluded.directory_groups,
                        groups_read_at_ns = excluded.groups_read_at_ns
                 RETURNING delegation_id",
                &[
                    &token(),
                    &grant.subject,
                    &grant.display_name,
                    &grant.client_id,
                    &grant.covers.everything,
                    &grant.covers.deployment_admin,
                    &plugins,
                    &groups,
                    &now_ns,
                    &grant.expires_at_ns,
                    &grant.directory_groups,
                ],
            )
            .map_err(said)?
            .get(0);
        // Renewed: only the pair this consent is about to issue works.
        tx.execute(
            "DELETE FROM dashboard_delegation_token WHERE delegation_id = $1",
            &[&id],
        )
        .map_err(said)?;
        let delegation = Self::find(&mut tx, &id)?.ok_or("the delegation just made is gone")?;
        tx.commit().map_err(said)?;
        Ok(delegation)
    }

    fn delegation(&self, id: &str) -> Result<Option<Delegation>, String> {
        let mut conn = self.database.conn()?;
        Self::find(&mut *conn, id)
    }

    fn issue(&self, token: &Token) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "INSERT INTO dashboard_delegation_token
                     (fingerprint, kind, resource, delegation_id, client_id, expires_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &token.fingerprint,
                    &token.kind.name(),
                    &token.resource,
                    &token.delegation_id,
                    &token.client_id,
                    &token.expires_at_ns,
                ],
            )
            .map_err(said)?;
        Ok(())
    }

    fn token(&self, fingerprint: &str) -> Result<Option<Token>, String> {
        Ok(self
            .database
            .conn()?
            .query_opt(
                "SELECT fingerprint, kind, resource, delegation_id, client_id, expires_at_ns,
                        spent_at_ns
                   FROM dashboard_delegation_token WHERE fingerprint = $1",
                &[&fingerprint],
            )
            .map_err(said)?
            .as_ref()
            .map(token_of))
    }

    fn spend(&self, fingerprint: &str, now_ns: i64) -> Result<bool, String> {
        // One statement: of two presentations at once, exactly one spends it.
        let spent = self
            .database
            .conn()?
            .execute(
                "UPDATE dashboard_delegation_token SET spent_at_ns = $2
                  WHERE fingerprint = $1 AND kind = 'refresh' AND spent_at_ns IS NULL",
                &[&fingerprint, &now_ns],
            )
            .map_err(said)?;
        Ok(spent == 1)
    }

    fn revoke(&self, id: &str, by: &str, why: &str, now_ns: i64) -> Result<bool, String> {
        let revoked = self
            .database
            .conn()?
            .execute(
                "UPDATE dashboard_delegation
                    SET revoked_at_ns = $2, revoked_by = $3, revoked_why = $4
                  WHERE delegation_id = $1 AND revoked_at_ns IS NULL",
                &[&id, &now_ns, &by, &why],
            )
            .map_err(said)?;
        Ok(revoked == 1)
    }

    fn revoke_person(
        &self,
        subject: &str,
        by: &str,
        why: &str,
        now_ns: i64,
    ) -> Result<usize, String> {
        let revoked = self
            .database
            .conn()?
            .execute(
                "UPDATE dashboard_delegation
                    SET revoked_at_ns = $2, revoked_by = $3, revoked_why = $4
                  WHERE subject = $1 AND revoked_at_ns IS NULL",
                &[&subject, &now_ns, &by, &why],
            )
            .map_err(said)?;
        Ok(revoked as usize)
    }

    fn of_person(&self, subject: &str, now_ns: i64) -> Result<Vec<Delegation>, String> {
        let rows = self
            .database
            .conn()?
            .query(
                &format!(
                    "SELECT {DELEGATION_COLUMNS} FROM dashboard_delegation d
                       JOIN dashboard_oauth_client c USING (client_id)
                      WHERE d.subject = $1"
                ),
                &[&subject],
            )
            .map_err(said)?;
        let mut theirs: Vec<Delegation> = rows.iter().map(delegation_of).collect();
        order(&mut theirs, now_ns);
        Ok(theirs)
    }

    fn holders(&self, now_ns: i64) -> Result<Vec<(String, String, usize)>, String> {
        let rows = self
            .database
            .conn()?
            .query(
                "SELECT subject, min(display_name), count(*) FROM dashboard_delegation
                  WHERE revoked_at_ns IS NULL AND expires_at_ns >= $1
                  GROUP BY subject ORDER BY subject",
                &[&now_ns],
            )
            .map_err(said)?;
        Ok(rows
            .iter()
            .map(|row| {
                let n: i64 = row.get(2);
                (row.get(0), row.get(1), n as usize)
            })
            .collect())
    }

    fn used(&self, id: &str, now_ns: i64) -> Result<(), String> {
        // GREATEST, so a replica whose clock is a little behind never moves
        // a last use backwards.
        self.database
            .conn()?
            .execute(
                "UPDATE dashboard_delegation
                    SET last_used_at_ns = GREATEST(coalesce(last_used_at_ns, 0), $2)
                  WHERE delegation_id = $1",
                &[&id, &now_ns],
            )
            .map_err(said)?;
        Ok(())
    }

    fn refused(&self, id: &str, why: &str, now_ns: i64) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "UPDATE dashboard_delegation SET last_refused_at_ns = $2, last_refusal = $3
                  WHERE delegation_id = $1",
                &[&id, &now_ns, &why],
            )
            .map_err(said)?;
        Ok(())
    }

    fn narrowed(&self) -> Result<Vec<Delegation>, String> {
        let rows = self
            .database
            .conn()?
            .query(
                &format!(
                    "SELECT {DELEGATION_COLUMNS} FROM dashboard_delegation d
                       JOIN dashboard_oauth_client c ON c.client_id = d.client_id
                      WHERE NOT d.covers_everything
                      ORDER BY d.made_at_ns"
                ),
                &[],
            )
            .map_err(said)?;
        Ok(rows.iter().map(delegation_of).collect())
    }

    fn rewrite(&self, id: &str, covers: &Covers) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "UPDATE dashboard_delegation SET covers_plugins = $2 WHERE delegation_id = $1",
                &[&id, &plugins_of(covers)],
            )
            .map_err(said)?;
        Ok(())
    }

    fn groups_read(&self, id: &str, groups: &[String], now_ns: i64) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "UPDATE dashboard_delegation SET directory_groups = $2, groups_read_at_ns = $3
                  WHERE delegation_id = $1",
                &[&id, &groups, &now_ns],
            )
            .map_err(said)?;
        Ok(())
    }

    fn signed_in(&self, subject: &str, groups: &[String], now_ns: i64) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "UPDATE dashboard_delegation SET directory_groups = $2, groups_read_at_ns = $3
                  WHERE subject = $1 AND revoked_at_ns IS NULL",
                &[&subject, &groups, &now_ns],
            )
            .map_err(said)?;
        Ok(())
    }

    fn sweep(&self, now_ns: i64) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        conn.execute(
            "DELETE FROM dashboard_delegation_token WHERE expires_at_ns + $2::bigint < $1::bigint",
            &[&now_ns, &TOKEN_KEPT_NS],
        )
        .map_err(said)?;
        conn.execute(
            "DELETE FROM dashboard_delegation
              WHERE least(coalesce(revoked_at_ns, expires_at_ns), expires_at_ns) + $2::bigint
                    < $1::bigint",
            &[&now_ns, &KEPT_NS],
        )
        .map_err(said)?;
        conn.execute(
            "DELETE FROM dashboard_oauth_client
              WHERE NOT consented AND registered_at_ns + $2::bigint < $1::bigint",
            &[&now_ns, &UNCONSENTED_NS],
        )
        .map_err(said)?;
        conn.execute(
            "DELETE FROM dashboard_tool_call WHERE called_at_ns + $2::bigint < $1::bigint",
            &[&now_ns, &CALLS_KEPT_NS],
        )
        .map_err(said)?;
        Ok(())
    }

    fn record_call(&self, call: &ToolCall) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        conn.execute(
            "INSERT INTO dashboard_tool_call (call_id, called_at_ns, subject, delegation_id,
                 client_name, owner, tool, level, outcome, reason, duration_ms)
             VALUES ($11, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &call.called_at_ns,
                &call.subject,
                &call.delegation_id,
                &call.client_name,
                &call.owner,
                &call.tool,
                &call.level,
                &call.outcome,
                &call.reason,
                &call.duration_ms,
                &token(),
            ],
        )
        .map_err(said)?;
        Ok(())
    }

    fn calls(&self, of: &CallsOf, limit: usize) -> Result<Vec<ToolCall>, String> {
        let mut conn = self.database.conn()?;
        let (column, value) = match of {
            CallsOf::Delegation(id) => ("delegation_id", id),
            CallsOf::Person(subject) => ("subject", subject),
        };
        let rows = conn
            .query(
                &format!(
                    "SELECT called_at_ns, subject, delegation_id, client_name, owner, tool,
                            level, outcome, reason, duration_ms
                       FROM dashboard_tool_call WHERE {column} = $1
                      ORDER BY called_at_ns DESC LIMIT $2"
                ),
                &[value, &(limit as i64)],
            )
            .map_err(said)?;
        Ok(rows
            .iter()
            .map(|row| ToolCall {
                called_at_ns: row.get(0),
                subject: row.get(1),
                delegation_id: row.get(2),
                client_name: row.get(3),
                owner: row.get(4),
                tool: row.get(5),
                level: row.get(6),
                outcome: row.get(7),
                reason: row.get(8),
                duration_ms: row.get(9),
            })
            .collect())
    }
}
