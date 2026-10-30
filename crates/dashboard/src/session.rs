//! Who is signed in: kept in the dashboard's own table, so a restart or an
//! upgrade leaves every browser signed in (ruling 811), and held in this
//! process's memory between reads of it.
//!
//! A session is a server-side record keyed by an opaque token in an HTTP-only
//! cookie. It holds who the person is and the directory groups they presented
//! when they signed in, and nothing about what they may do: access is
//! evaluated from the records on every request, so a change to a permission
//! reaches a live session within the records' refresh, and a session cannot
//! carry a stale grant.
//!
//! Kept as the terminal sessions before delegations were (dashboard migration
//! 2): by the SHA-256 of its token, never the token, so a dump of the table
//! signs nobody in; the form token is derived from the session's token, so
//! it is kept nowhere either. Without a database (tests, a dashboard given
//! none) it is memory alone, and a restart signs every browser out.
//!
//! What is held in memory is read again from the table at most
//! [`RECHECK_NS`] after it was last read, so a session ended on another
//! replica -- a sign-out, a password reset -- ends here within that; and
//! its last use is written at most once a [`RECHECK_NS`], so a page is not
//! a write. Two bounds, stated in the contract (decisions/015) and not in
//! configuration: 30 minutes idle and 12 hours absolute. A person removed
//! from a directory group keeps access until one of them ends the session.
//!
//! **Idle is the person's, not the dashboard's** (ruling 854). The time the
//! dashboard was away -- a restart, an upgrade, the machine under it
//! stopped -- is nobody's idleness: a starting dashboard reads the latest
//! use the table holds, and every kept session's idle clock moves on by the
//! gap between that and when it started serving, until the session is next
//! used. The 12-hour bound counts from the sign-in, whatever happened.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore;

use crate::clock::{HOUR_NS, MINUTE_NS};

/// Decisions/015. Changing either amends that decision.
pub const IDLE_NS: i64 = 30 * MINUTE_NS;
pub const ABSOLUTE_NS: i64 = 12 * HOUR_NS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Deployment-local: the directory's issuer and subject.
    pub subject: String,
    pub display_name: String,
    /// As presented at this sign-in. Never refreshed: a new sign-in is.
    pub directory_groups: Vec<String>,
    pub started_at_ns: i64,
    pub last_seen_at_ns: i64,
    /// Every form this session posts carries it, so a page on another site
    /// cannot post as this person.
    pub form_token: String,
}

/// How long a session held in memory is served before the table is read
/// again; and how often its last use is written.
pub const RECHECK_NS: i64 = 30 * crate::clock::SECOND_NS;

/// Where sessions are kept. Blocking: [`Sessions`] calls it off the async
/// runtime's workers.
pub trait SessionStore: Send + Sync {
    /// Keep a new session by its token's hash.
    fn keep(&self, hash: &str, session: &Session) -> Result<(), String>;
    /// The session kept by this hash, its form token left empty.
    fn kept(&self, hash: &str) -> Result<Option<Session>, String>;
    /// Its last use, never moved back.
    fn touch(&self, hash: &str, last_seen_at_ns: i64) -> Result<(), String>;
    fn end(&self, hash: &str) -> Result<(), String>;
    /// End every session of a person; how many.
    fn end_person(&self, subject: &str) -> Result<usize, String>;
    /// Remove every session past either bound, its idle counted as
    /// `resumed` says.
    fn sweep(&self, now_ns: i64, resumed: Resumed) -> Result<(), String>;
    /// The latest use any kept session's row holds, if any is kept.
    fn latest_use(&self) -> Result<Option<i64>, String>;
}

/// When this process started serving, and how long the dashboard was away
/// before it: the gap between the latest use the table held and the start
/// (ruling 854). A kept session last used before the start has its idle
/// clock moved on by the gap; one used since, not at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resumed {
    pub started_at_ns: i64,
    pub gap_ns: i64,
}

impl Resumed {
    /// A kept last use, as idle is counted from it.
    pub fn last_use(self, kept_ns: i64) -> i64 {
        if kept_ns < self.started_at_ns {
            kept_ns + self.gap_ns
        } else {
            kept_ns
        }
    }

    fn within_bounds(self, session: &Session, now_ns: i64) -> bool {
        now_ns - self.last_use(session.last_seen_at_ns) <= IDLE_NS
            && now_ns - session.started_at_ns <= ABSOLUTE_NS
    }
}

/// A session held in memory: when the table was last read for it, and the
/// last use written there.
struct Held {
    session: Session,
    checked_at_ns: i64,
    written_seen_ns: i64,
}

/// Held in memory by the token's hash, as the table keeps them: a plugin
/// page's session names the browser session it came from by that alone
/// (ruling 855), so no token is kept anywhere.
#[derive(Default)]
pub struct Sessions {
    live: Mutex<HashMap<String, Held>>,
    store: Option<Arc<dyn SessionStore>>,
    resumed: Resumed,
}

/// Unguessable, and says nothing about whose it is.
pub fn token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A session's form token: derived from its token, so it is kept nowhere,
/// and says nothing about the token it is derived from.
fn form_token(key: &str) -> String {
    crate::terminal::hashed(&format!("form\u{1f}{key}"))
}

fn within_bounds(session: &Session, now_ns: i64) -> bool {
    now_ns - session.last_seen_at_ns <= IDLE_NS && now_ns - session.started_at_ns <= ABSOLUTE_NS
}

/// Run a store's blocking call: off the async runtime's workers where this
/// is one of them, so a page waiting on the table stalls no other task.
pub(crate) fn blocking<T>(call: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(call)
        }
        _ => call(),
    }
}

impl Sessions {
    /// Sessions kept in `store`, held in memory between reads of it, by a
    /// process starting to serve at `now_ns`: the dashboard's time away
    /// before it is read from the table's latest use (ruling 854).
    pub fn keeping(store: Arc<dyn SessionStore>, now_ns: i64) -> Self {
        let latest = match blocking(|| store.latest_use()) {
            Ok(latest) => latest,
            Err(failed) => {
                tracing::warn!("the sessions' latest use could not be read: {failed}");
                None
            }
        };
        let gap_ns = latest.map_or(0, |latest| (now_ns - latest).max(0));
        if gap_ns > IDLE_NS {
            tracing::info!(
                away_s = gap_ns / crate::clock::SECOND_NS,
                "the dashboard was away longer than a session may idle; kept sessions resume"
            );
        }
        Self {
            live: Mutex::default(),
            store: Some(store),
            resumed: Resumed {
                started_at_ns: now_ns,
                gap_ns,
            },
        }
    }

    /// Start a session for somebody the directory has just vouched for, and
    /// return its token. Kept before it is returned: a session the table
    /// does not hold would end at the next restart without a word.
    pub fn start(
        &self,
        subject: &str,
        display_name: &str,
        directory_groups: Vec<String>,
        now_ns: i64,
    ) -> String {
        let key = token();
        let hash = crate::terminal::hashed(&key);
        let session = Session {
            subject: subject.to_string(),
            display_name: display_name.to_string(),
            directory_groups,
            started_at_ns: now_ns,
            last_seen_at_ns: now_ns,
            form_token: String::new(),
        };
        if let Some(store) = &self.store {
            if let Err(failed) = blocking(|| store.keep(&hash, &session)) {
                tracing::warn!("a session was not kept, so a restart will sign it out: {failed}");
            }
        }
        self.live.lock().expect("session lock poisoned").insert(
            hash,
            Held {
                session,
                checked_at_ns: now_ns,
                written_seen_ns: now_ns,
            },
        );
        key
    }

    /// The session behind a token, touched, or nothing when there is none or
    /// it has passed either bound -- in which case it is gone.
    pub fn find(&self, key: &str, now_ns: i64) -> Option<Session> {
        self.find_hashed(&crate::terminal::hashed(key), now_ns)
            .map(|mut session| {
                session.form_token = form_token(key);
                session
            })
    }

    /// [`Sessions::find`] by the token's hash, as a plugin page's session
    /// names the one it came from: its form token left empty, since only
    /// the token derives it.
    pub fn find_hashed(&self, hash: &str, now_ns: i64) -> Option<Session> {
        let resumed = self.resumed;
        let Some(store) = &self.store else {
            let mut live = self.live.lock().expect("session lock poisoned");
            let held = live.get_mut(hash)?;
            if !within_bounds(&held.session, now_ns) {
                live.remove(hash);
                return None;
            }
            held.session.last_seen_at_ns = now_ns;
            return Some(held.session.clone());
        };
        // Held and read recently: answered from memory.
        let due = {
            let mut live = self.live.lock().expect("session lock poisoned");
            match live.get_mut(hash) {
                Some(held) if now_ns - held.checked_at_ns < RECHECK_NS => {
                    if !within_bounds(&held.session, now_ns) {
                        live.remove(hash);
                        drop(live);
                        let _ = blocking(|| store.end(hash));
                        return None;
                    }
                    held.session.last_seen_at_ns = now_ns;
                    return Some(held.session.clone());
                }
                Some(held) => Some(held.session.last_seen_at_ns),
                None => None,
            }
        };
        // Read again: ended elsewhere, or held here since before a restart.
        let kept = match blocking(|| store.kept(hash)) {
            Ok(kept) => kept,
            Err(failed) => {
                // The table briefly away signs nobody out: what is held is
                // served until it answers again.
                tracing::warn!("the sessions' table could not be read: {failed}");
                let mut live = self.live.lock().expect("session lock poisoned");
                let held = live.get_mut(hash)?;
                if !within_bounds(&held.session, now_ns) {
                    live.remove(hash);
                    return None;
                }
                held.session.last_seen_at_ns = now_ns;
                return Some(held.session.clone());
            }
        };
        let Some(mut session) = kept else {
            self.live
                .lock()
                .expect("session lock poisoned")
                .remove(hash);
            return None;
        };
        // Its last use as idle counts it: moved on by the dashboard's time
        // away when it was last used before this process (ruling 854).
        session.last_seen_at_ns = resumed
            .last_use(session.last_seen_at_ns)
            .max(due.unwrap_or(0));
        if !within_bounds(&session, now_ns) {
            self.live
                .lock()
                .expect("session lock poisoned")
                .remove(hash);
            let _ = blocking(|| store.end(hash));
            return None;
        }
        session.last_seen_at_ns = now_ns;
        if let Err(failed) = blocking(|| store.touch(hash, now_ns)) {
            tracing::warn!("a session's last use was not written: {failed}");
        }
        self.live.lock().expect("session lock poisoned").insert(
            hash.to_string(),
            Held {
                session: session.clone(),
                checked_at_ns: now_ns,
                written_seen_ns: now_ns,
            },
        );
        Some(session)
    }

    /// Whether a session is live, without touching it: for a sweep, which
    /// would otherwise keep alive everything it looked at. Answered from
    /// memory, as the sweep it serves runs often; a session held only in
    /// the table is read there.
    pub fn is_live(&self, key: &str, now_ns: i64) -> bool {
        self.is_live_hashed(&crate::terminal::hashed(key), now_ns)
    }

    /// [`Sessions::is_live`] by the token's hash.
    pub fn is_live_hashed(&self, hash: &str, now_ns: i64) -> bool {
        if let Some(held) = self.live.lock().expect("session lock poisoned").get(hash) {
            return within_bounds(&held.session, now_ns);
        }
        let Some(store) = &self.store else {
            return false;
        };
        blocking(|| store.kept(hash))
            .ok()
            .flatten()
            .is_some_and(|session| self.resumed.within_bounds(&session, now_ns))
    }

    pub fn end(&self, key: &str) {
        let hash = crate::terminal::hashed(key);
        self.live
            .lock()
            .expect("session lock poisoned")
            .remove(&hash);
        if let Some(store) = &self.store {
            if let Err(failed) = blocking(|| store.end(&hash)) {
                tracing::warn!("a session ended here was not removed from the table: {failed}");
            }
        }
    }

    /// End every browser session a person holds, and say how many: a
    /// password reset (W6.16) ends what the old password opened.
    pub fn end_person(&self, subject: &str) -> usize {
        let mut live = self.live.lock().expect("session lock poisoned");
        let before = live.len();
        live.retain(|_, held| held.session.subject != subject);
        let here = before - live.len();
        drop(live);
        match &self.store {
            Some(store) => match blocking(|| store.end_person(subject)) {
                Ok(kept) => kept.max(here),
                Err(failed) => {
                    tracing::warn!("a person's sessions were not removed from the table: {failed}");
                    here
                }
            },
            None => here,
        }
    }

    /// Forget every expired session, so memory tracks the people signed in
    /// rather than everyone who ever was; and write each held session's last
    /// use that has not been written, so a restart finds it as recent.
    pub fn sweep(&self, now_ns: i64) {
        let mut touched = Vec::new();
        {
            let mut live = self.live.lock().expect("session lock poisoned");
            live.retain(|_, held| within_bounds(&held.session, now_ns));
            for (hash, held) in live.iter_mut() {
                if held.session.last_seen_at_ns > held.written_seen_ns {
                    held.written_seen_ns = held.session.last_seen_at_ns;
                    touched.push((hash.clone(), held.session.last_seen_at_ns));
                }
            }
        }
        if let Some(store) = &self.store {
            let resumed = self.resumed;
            blocking(|| {
                for (hash, at) in &touched {
                    if let Err(failed) = store.touch(hash, *at) {
                        tracing::warn!("a session's last use was not written: {failed}");
                    }
                }
                if let Err(failed) = store.sweep(now_ns, resumed) {
                    tracing::warn!("the sessions' table was not swept: {failed}");
                }
            });
        }
    }
}

/// The sessions in memory alone, as a store: for tests of what reads the
/// table, and a second dashboard over the same memory.
#[derive(Default)]
pub struct InMemory {
    kept: Mutex<HashMap<String, Session>>,
}

impl SessionStore for InMemory {
    fn keep(&self, hash: &str, session: &Session) -> Result<(), String> {
        let mut session = session.clone();
        session.form_token = String::new();
        self.kept
            .lock()
            .expect("lock")
            .insert(hash.to_string(), session);
        Ok(())
    }
    fn kept(&self, hash: &str) -> Result<Option<Session>, String> {
        Ok(self.kept.lock().expect("lock").get(hash).cloned())
    }
    fn touch(&self, hash: &str, last_seen_at_ns: i64) -> Result<(), String> {
        if let Some(session) = self.kept.lock().expect("lock").get_mut(hash) {
            session.last_seen_at_ns = session.last_seen_at_ns.max(last_seen_at_ns);
        }
        Ok(())
    }
    fn end(&self, hash: &str) -> Result<(), String> {
        self.kept.lock().expect("lock").remove(hash);
        Ok(())
    }
    fn end_person(&self, subject: &str) -> Result<usize, String> {
        let mut kept = self.kept.lock().expect("lock");
        let before = kept.len();
        kept.retain(|_, s| s.subject != subject);
        Ok(before - kept.len())
    }
    fn sweep(&self, now_ns: i64, resumed: Resumed) -> Result<(), String> {
        self.kept
            .lock()
            .expect("lock")
            .retain(|_, s| resumed.within_bounds(s, now_ns));
        Ok(())
    }
    fn latest_use(&self) -> Result<Option<i64>, String> {
        Ok(self
            .kept
            .lock()
            .expect("lock")
            .values()
            .map(|s| s.last_seen_at_ns)
            .max())
    }
}

/// The sessions in the dashboard's own table (migration 12).
pub struct InPostgres {
    database: crate::database::Database,
}

impl InPostgres {
    /// On a database already verified at start.
    pub fn on(database: crate::database::Database) -> Self {
        Self { database }
    }
}

impl SessionStore for InPostgres {
    fn keep(&self, hash: &str, session: &Session) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "INSERT INTO dashboard_browser_session
                     (session_hash, subject, display_name, directory_groups,
                      signed_in_at_ns, last_seen_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (session_hash) DO NOTHING",
                &[
                    &hash,
                    &session.subject,
                    &session.display_name,
                    &session.directory_groups,
                    &session.started_at_ns,
                    &session.last_seen_at_ns,
                ],
            )
            .map_err(crate::database::said)?;
        Ok(())
    }

    fn kept(&self, hash: &str) -> Result<Option<Session>, String> {
        let row = self
            .database
            .conn()?
            .query_opt(
                "SELECT subject, display_name, directory_groups, signed_in_at_ns, last_seen_at_ns
                   FROM dashboard_browser_session WHERE session_hash = $1",
                &[&hash],
            )
            .map_err(crate::database::said)?;
        Ok(row.map(|row| Session {
            subject: row.get(0),
            display_name: row.get(1),
            directory_groups: row.get(2),
            started_at_ns: row.get(3),
            last_seen_at_ns: row.get(4),
            form_token: String::new(),
        }))
    }

    fn touch(&self, hash: &str, last_seen_at_ns: i64) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "UPDATE dashboard_browser_session
                    SET last_seen_at_ns = greatest(last_seen_at_ns, $2)
                  WHERE session_hash = $1",
                &[&hash, &last_seen_at_ns],
            )
            .map_err(crate::database::said)?;
        Ok(())
    }

    fn end(&self, hash: &str) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "DELETE FROM dashboard_browser_session WHERE session_hash = $1",
                &[&hash],
            )
            .map_err(crate::database::said)?;
        Ok(())
    }

    fn end_person(&self, subject: &str) -> Result<usize, String> {
        let ended = self
            .database
            .conn()?
            .execute(
                "DELETE FROM dashboard_browser_session WHERE subject = $1",
                &[&subject],
            )
            .map_err(crate::database::said)?;
        Ok(ended as usize)
    }

    fn sweep(&self, now_ns: i64, resumed: Resumed) -> Result<(), String> {
        // Idle counted as [`Resumed::last_use`] counts it (ruling 854).
        self.database
            .conn()?
            .execute(
                "DELETE FROM dashboard_browser_session
                  WHERE $1 - (CASE WHEN last_seen_at_ns < $4 THEN last_seen_at_ns + $5
                                   ELSE last_seen_at_ns END) > $2
                     OR $1 - signed_in_at_ns > $3",
                &[
                    &now_ns,
                    &IDLE_NS,
                    &ABSOLUTE_NS,
                    &resumed.started_at_ns,
                    &resumed.gap_ns,
                ],
            )
            .map_err(crate::database::said)?;
        Ok(())
    }

    fn latest_use(&self) -> Result<Option<i64>, String> {
        let row = self
            .database
            .conn()?
            .query_one(
                "SELECT max(last_seen_at_ns) FROM dashboard_browser_session",
                &[],
            )
            .map_err(crate::database::said)?;
        Ok(row.get::<_, Option<i64>>(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_790_380_800_000_000_000;

    #[test]
    fn a_session_ends_after_30_minutes_idle() {
        let sessions = Sessions::default();
        let key = sessions.start("ada", "Ada", vec![], T0);
        assert!(
            sessions.find(&key, T0 + IDLE_NS).is_some(),
            "exactly the bound is still in"
        );
        assert!(sessions.find(&key, T0 + 2 * IDLE_NS + 1).is_none());
        assert!(
            sessions.find(&key, T0 + 2 * IDLE_NS).is_none(),
            "and once gone it stays gone"
        );
    }

    #[test]
    fn use_keeps_a_session_alive_but_never_past_12_hours() {
        let sessions = Sessions::default();
        let key = sessions.start("ada", "Ada", vec![], T0);
        let mut now = T0;
        while now + 20 * MINUTE_NS <= T0 + ABSOLUTE_NS {
            now += 20 * MINUTE_NS;
            assert!(sessions.find(&key, now).is_some());
        }
        assert!(sessions.find(&key, T0 + ABSOLUTE_NS + 1).is_none());
    }

    #[test]
    fn tokens_are_unguessable_and_distinct() {
        let sessions = Sessions::default();
        let a = sessions.start("ada", "Ada", vec![], T0);
        let b = sessions.start("ada", "Ada", vec![], T0);
        assert_ne!(a, b);
        assert_eq!(a.len(), 43, "32 random bytes");
        assert_ne!(
            sessions.find(&a, T0).unwrap().form_token,
            a,
            "the form token is not the session token"
        );
    }

    #[test]
    fn a_sweep_forgets_only_the_expired() {
        let sessions = Sessions::default();
        let stale = sessions.start("old", "Old", vec![], T0);
        let fresh = sessions.start("new", "New", vec![], T0 + IDLE_NS);
        sessions.sweep(T0 + IDLE_NS + MINUTE_NS);
        assert!(sessions.find(&stale, T0 + IDLE_NS + MINUTE_NS).is_none());
        assert!(sessions.find(&fresh, T0 + IDLE_NS + MINUTE_NS).is_some());
    }

    fn kept_in(store: &Arc<InMemory>) -> Sessions {
        kept_at(store, T0)
    }

    fn kept_at(store: &Arc<InMemory>, now_ns: i64) -> Sessions {
        Sessions::keeping(Arc::clone(store) as Arc<dyn SessionStore>, now_ns)
    }

    #[test]
    fn a_session_survives_a_restart_with_its_form_token_and_its_bounds() {
        let store = Arc::new(InMemory::default());
        let before = kept_in(&store);
        let key = before.start("ada", "Ada", vec!["traders".into()], T0);
        let form = before.find(&key, T0).unwrap().form_token;
        // The process goes; a new one over the same table.
        drop(before);
        let after = kept_in(&store);
        let found = after.find(&key, T0 + MINUTE_NS).expect("still signed in");
        assert_eq!(found.subject, "ada");
        assert_eq!(found.directory_groups, vec!["traders".to_string()]);
        assert_eq!(found.form_token, form, "the same form token, kept nowhere");
        // The bounds count from the sign-in and the last use, as before.
        assert!(after.find(&key, T0 + MINUTE_NS + IDLE_NS + 1).is_none());
        assert!(
            kept_in(&store).find(&key, T0 + 2 * MINUTE_NS).is_none(),
            "a lapsed session is gone from the table too"
        );
    }

    #[test]
    fn the_table_holds_no_token_and_a_forged_one_finds_nothing() {
        let store = Arc::new(InMemory::default());
        let sessions = kept_in(&store);
        let key = sessions.start("ada", "Ada", vec![], T0);
        let kept = store.kept.lock().unwrap();
        assert!(!kept.contains_key(&key), "kept by hash, never the token");
        assert!(kept.values().all(|s| s.form_token.is_empty()));
        drop(kept);
        assert!(kept_in(&store)
            .find(&crate::terminal::hashed(&key), T0)
            .is_none());
    }

    #[test]
    fn a_sign_out_or_a_reset_on_one_replica_ends_the_session_on_another() {
        let store = Arc::new(InMemory::default());
        let a = kept_in(&store);
        let b = kept_in(&store);
        let key = a.start("ada", "Ada", vec![], T0);
        let other = a.start("ada", "Ada", vec![], T0);
        assert!(b.find(&key, T0).is_some());
        assert!(b.find(&other, T0).is_some());
        a.end(&key);
        // Within the recheck, b may still serve what it holds; past it, no.
        assert!(b.find(&key, T0 + RECHECK_NS).is_none());
        assert_eq!(a.end_person("ada"), 1);
        assert!(b.find(&other, T0 + 2 * RECHECK_NS).is_none());
    }

    #[test]
    fn the_dashboards_time_away_is_not_counted_as_idle() {
        // Ruling 854: two people, one using the dashboard up to the moment
        // it stops, one idle 20 minutes by then; it is away 45 minutes.
        let store = Arc::new(InMemory::default());
        let before = kept_in(&store);
        let idle = before.start("bo", "Bo", vec![], T0 - 20 * MINUTE_NS);
        let busy = before.start("ada", "Ada", vec![], T0 - 20 * MINUTE_NS);
        assert!(before.find(&busy, T0).is_some());
        before.sweep(T0);
        drop(before);
        let back = T0 + 45 * MINUTE_NS;
        let after = kept_at(&store, back);
        // Its first sweep keeps both: their idle clocks moved on by the gap.
        after.sweep(back);
        assert_eq!(store.kept.lock().unwrap().len(), 2);
        assert!(
            after.find(&busy, back + MINUTE_NS).is_some(),
            "a restart 45 minutes after the last use keeps the person signed in"
        );
        // Bo was idle 20 minutes when it stopped: 10 more after it is back.
        assert!(after.is_live(&idle, back + 9 * MINUTE_NS));
        assert!(!after.is_live(&idle, back + 11 * MINUTE_NS));
        assert!(after.find(&idle, back + 11 * MINUTE_NS).is_none());
        // Idle still counts after the restart, from the last use since.
        assert!(after.find(&busy, back + MINUTE_NS + IDLE_NS + 1).is_none());
    }

    #[test]
    fn the_absolute_bound_counts_from_the_sign_in_whatever_the_time_away() {
        let store = Arc::new(InMemory::default());
        let before = kept_in(&store);
        let key = before.start("ada", "Ada", vec![], T0);
        let late = T0 + ABSOLUTE_NS - 5 * MINUTE_NS;
        let mut now = T0;
        while now + 20 * MINUTE_NS < late {
            now += 20 * MINUTE_NS;
            assert!(before.find(&key, now).is_some());
        }
        assert!(before.find(&key, late).is_some());
        before.sweep(late);
        drop(before);
        let after = kept_at(&store, late + HOUR_NS);
        assert!(after.find(&key, late + HOUR_NS).is_none());
    }

    #[test]
    fn a_second_replica_starting_beside_a_serving_one_moves_no_idle_clock() {
        let store = Arc::new(InMemory::default());
        let serving = kept_in(&store);
        let idle = serving.start("bo", "Bo", vec![], T0);
        let busy = serving.start("ada", "Ada", vec![], T0);
        let now = T0 + 25 * MINUTE_NS;
        assert!(serving.find(&busy, now).is_some());
        let beside = kept_at(&store, now);
        assert!(!beside.is_live(&idle, T0 + IDLE_NS + MINUTE_NS));
    }

    #[test]
    fn use_is_written_so_a_restart_counts_idle_from_the_last_use() {
        let store = Arc::new(InMemory::default());
        let sessions = kept_in(&store);
        let key = sessions.start("ada", "Ada", vec![], T0);
        // Used for an hour, every ten minutes, answered from memory and the
        // table between.
        let mut now = T0;
        for _ in 0..6 {
            now += 10 * MINUTE_NS;
            assert!(sessions.find(&key, now).is_some());
        }
        sessions.sweep(now);
        let restarted = kept_in(&store);
        assert!(
            restarted.find(&key, now + IDLE_NS - MINUTE_NS).is_some(),
            "idle is counted from the last use, not the sign-in"
        );
    }
}
