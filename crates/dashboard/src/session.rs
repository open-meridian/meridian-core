//! Who is signed in, held in this process's memory.
//!
//! A session is a server-side record keyed by an opaque token in an HTTP-only
//! cookie. It holds who the person is and the directory groups they presented
//! when they signed in, and nothing about what they may do: access is
//! evaluated from the records on every request, so a change to a permission
//! reaches a live session within the records' refresh, and a session cannot
//! carry a stale grant.
//!
//! In memory, on one replica: a restart signs everyone out, which the spec
//! accepts. Two bounds, stated in the contract (decisions/015) and not in
//! configuration: 30 minutes idle and 12 hours absolute. A person removed
//! from a directory group keeps access until one of them ends the session.

use std::collections::HashMap;
use std::sync::Mutex;

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

#[derive(Default)]
pub struct Sessions {
    live: Mutex<HashMap<String, Session>>,
}

/// Unguessable, and says nothing about whose it is.
pub fn token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Sessions {
    /// Start a session for somebody the directory has just vouched for, and
    /// return its token.
    pub fn start(
        &self,
        subject: &str,
        display_name: &str,
        directory_groups: Vec<String>,
        now_ns: i64,
    ) -> String {
        let key = token();
        let session = Session {
            subject: subject.to_string(),
            display_name: display_name.to_string(),
            directory_groups,
            started_at_ns: now_ns,
            last_seen_at_ns: now_ns,
            form_token: token(),
        };
        self.live
            .lock()
            .expect("session lock poisoned")
            .insert(key.clone(), session);
        key
    }

    /// The session behind a token, touched, or nothing when there is none or
    /// it has passed either bound -- in which case it is gone.
    pub fn find(&self, key: &str, now_ns: i64) -> Option<Session> {
        let mut live = self.live.lock().expect("session lock poisoned");
        let session = live.get_mut(key)?;
        let idle = now_ns - session.last_seen_at_ns > IDLE_NS;
        let old = now_ns - session.started_at_ns > ABSOLUTE_NS;
        if idle || old {
            live.remove(key);
            return None;
        }
        session.last_seen_at_ns = now_ns;
        Some(session.clone())
    }

    pub fn end(&self, key: &str) {
        self.live.lock().expect("session lock poisoned").remove(key);
    }

    /// Forget every expired session, so memory tracks the people signed in
    /// rather than everyone who ever was.
    pub fn sweep(&self, now_ns: i64) {
        self.live
            .lock()
            .expect("session lock poisoned")
            .retain(|_, s| {
                now_ns - s.last_seen_at_ns <= IDLE_NS && now_ns - s.started_at_ns <= ABSOLUTE_NS
            });
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
}
