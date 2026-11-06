//! What the dashboard holds between two requests that may reach different
//! copies of it (step 3b; spec/a-deployment-scales-out-and-fails-over-by-
//! configuration section 4: "the dashboard, once its remaining in-memory
//! state is found and moved"): a sign-in begun with the directory and
//! finished at its callback, a plugin page's one-time code minted on one
//! page and presented on the plugin's host, and an agent's authorisation
//! from its asking through the person's sign-in and consent to the code its
//! client trades for a token.
//!
//! Each is short-lived and spent once, and kept in the dashboard's own
//! table (migration 14) where it has one, so any copy answers any step, and
//! in this process's memory where it has none (tests, a dashboard given no
//! database). Kept by the SHA-256 of its key (a state, a code, an id) and
//! never the key, as sessions are: a dump of the table answers no callback.
//! Every step takes or marks its row in one statement, so two copies never
//! both spend one.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

/// The kinds of thing in flight, each its own namespace of keys.
pub mod kind {
    /// A directory sign-in: its PKCE verifier and nonce, by its state.
    pub const SIGN_IN: &str = "sign-in";
    /// A plugin page's one-time code.
    pub const PLUGIN_CODE: &str = "plugin-code";
    /// An agent's authorisation waiting for its person, by its id.
    pub const AUTHORISATION: &str = "authorisation";
    /// A provider sign-in made for an authorisation, by its state.
    pub const PROVIDER_STATE: &str = "provider-state";
    /// An authorisation code issued to an agent's client.
    pub const AGENT_CODE: &str = "agent-code";
}

/// A mark's outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum Marked {
    /// Nothing held by that key, or it has expired.
    Gone,
    /// Marked before: what it holds, unchanged.
    Already(Value),
    /// Marked now: what it holds, the mark included.
    Now(Value),
}

/// Where things in flight are kept. Blocking: callers use
/// [`crate::session::blocking`].
pub trait InFlightStore: Send + Sync {
    /// Keep `held` by `key` until `expires_at_ns`, keeping at most `most` of
    /// its kind: past it the oldest go.
    fn put(
        &self,
        kind: &str,
        key: &str,
        held: &Value,
        expires_at_ns: i64,
        most: usize,
    ) -> Result<(), String>;
    /// What `key` holds, if it has not expired; nothing is spent.
    fn get(&self, kind: &str, key: &str, now_ns: i64) -> Result<Option<Value>, String>;
    /// What `key` holds, spent: gone whatever the answer, and nothing where
    /// it had expired.
    fn take(&self, kind: &str, key: &str, now_ns: i64) -> Result<Option<Value>, String>;
    /// Set `field` to `value` where it is not set yet, in one step.
    fn mark(
        &self,
        kind: &str,
        key: &str,
        field: &str,
        value: &Value,
        now_ns: i64,
    ) -> Result<Marked, String>;
    /// Remove whatever has expired.
    fn sweep(&self, now_ns: i64) -> Result<(), String>;
    /// How many of a kind are kept, expired or not.
    fn count(&self, kind: &str) -> Result<usize, String>;
}

fn unset(held: &Value, field: &str) -> bool {
    matches!(
        held.get(field),
        None | Some(Value::Null) | Some(Value::Bool(false))
    )
}

/// What one thing in flight holds in memory: its value, when it expires and
/// its place in the order things were put, by its kind and its key's hash.
type Held = HashMap<(String, String), (Value, i64, i64)>;

/// In this process alone: tests, and a dashboard given no database.
#[derive(Default)]
pub struct InMemory {
    held: Mutex<Held>,
    /// Puts so far, ordering each kind's oldest.
    put: Mutex<i64>,
}

impl InFlightStore for InMemory {
    fn put(
        &self,
        kind: &str,
        key: &str,
        held: &Value,
        expires_at_ns: i64,
        most: usize,
    ) -> Result<(), String> {
        let order = {
            let mut put = self.put.lock().expect("lock");
            *put += 1;
            *put
        };
        let mut all = self.held.lock().expect("lock");
        loop {
            let of_kind: Vec<_> = all.iter().filter(|((k, _), _)| k == kind).collect();
            if of_kind.len() < most.max(1) {
                break;
            }
            let oldest = of_kind
                .iter()
                .min_by_key(|(_, (_, _, at))| *at)
                .map(|(id, _)| (*id).clone())
                .expect("a full kind has an oldest");
            all.remove(&oldest);
        }
        all.insert(
            (kind.to_string(), crate::terminal::hashed(key)),
            (held.clone(), expires_at_ns, order),
        );
        Ok(())
    }

    fn get(&self, kind: &str, key: &str, now_ns: i64) -> Result<Option<Value>, String> {
        let all = self.held.lock().expect("lock");
        Ok(all
            .get(&(kind.to_string(), crate::terminal::hashed(key)))
            .filter(|(_, expires, _)| *expires >= now_ns)
            .map(|(held, _, _)| held.clone()))
    }

    fn take(&self, kind: &str, key: &str, now_ns: i64) -> Result<Option<Value>, String> {
        let mut all = self.held.lock().expect("lock");
        Ok(all
            .remove(&(kind.to_string(), crate::terminal::hashed(key)))
            .filter(|(_, expires, _)| *expires >= now_ns)
            .map(|(held, _, _)| held))
    }

    fn mark(
        &self,
        kind: &str,
        key: &str,
        field: &str,
        value: &Value,
        now_ns: i64,
    ) -> Result<Marked, String> {
        let mut all = self.held.lock().expect("lock");
        let Some((held, expires, _)) =
            all.get_mut(&(kind.to_string(), crate::terminal::hashed(key)))
        else {
            return Ok(Marked::Gone);
        };
        if *expires < now_ns {
            return Ok(Marked::Gone);
        }
        if !unset(held, field) {
            return Ok(Marked::Already(held.clone()));
        }
        held[field] = value.clone();
        Ok(Marked::Now(held.clone()))
    }

    fn sweep(&self, now_ns: i64) -> Result<(), String> {
        self.held
            .lock()
            .expect("lock")
            .retain(|_, (_, expires, _)| *expires >= now_ns);
        Ok(())
    }

    fn count(&self, kind: &str) -> Result<usize, String> {
        Ok(self
            .held
            .lock()
            .expect("lock")
            .keys()
            .filter(|(k, _)| k == kind)
            .count())
    }
}

/// In the dashboard's own table (migration 14).
pub struct InPostgres {
    database: crate::database::Database,
}

impl InPostgres {
    pub fn on(database: crate::database::Database) -> Self {
        Self { database }
    }
}

fn read(text: String) -> Result<Value, String> {
    serde_json::from_str(&text)
        .map_err(|failed| format!("a thing in flight did not read: {failed}"))
}

impl InFlightStore for InPostgres {
    fn put(
        &self,
        kind: &str,
        key: &str,
        held: &Value,
        expires_at_ns: i64,
        most: usize,
    ) -> Result<(), String> {
        let mut conn = self.database.conn()?;
        let mut tx = conn.transaction().map_err(crate::database::said)?;
        tx.execute(
            "INSERT INTO dashboard_in_flight (kind, key_hash, held, expires_at_ns, put_at)
             VALUES ($1, $2, $3::text::jsonb, $4, clock_timestamp())
             ON CONFLICT (kind, key_hash) DO UPDATE
                SET held = excluded.held, expires_at_ns = excluded.expires_at_ns,
                    put_at = excluded.put_at",
            &[
                &kind,
                &crate::terminal::hashed(key),
                &held.to_string(),
                &expires_at_ns,
            ],
        )
        .map_err(crate::database::said)?;
        // The oldest of its kind past the most, as the map before it did.
        tx.execute(
            "DELETE FROM dashboard_in_flight WHERE kind = $1 AND key_hash IN (
                 SELECT key_hash FROM dashboard_in_flight WHERE kind = $1
                  ORDER BY put_at DESC OFFSET $2)",
            &[&kind, &(most.max(1) as i64)],
        )
        .map_err(crate::database::said)?;
        tx.commit().map_err(crate::database::said)
    }

    fn get(&self, kind: &str, key: &str, now_ns: i64) -> Result<Option<Value>, String> {
        self.database
            .conn()?
            .query_opt(
                "SELECT held::text FROM dashboard_in_flight
                  WHERE kind = $1 AND key_hash = $2 AND expires_at_ns >= $3",
                &[&kind, &crate::terminal::hashed(key), &now_ns],
            )
            .map_err(crate::database::said)?
            .map(|row| read(row.get(0)))
            .transpose()
    }

    fn take(&self, kind: &str, key: &str, now_ns: i64) -> Result<Option<Value>, String> {
        let row = self
            .database
            .conn()?
            .query_opt(
                "DELETE FROM dashboard_in_flight WHERE kind = $1 AND key_hash = $2
                 RETURNING held::text, expires_at_ns",
                &[&kind, &crate::terminal::hashed(key)],
            )
            .map_err(crate::database::said)?;
        match row {
            Some(row) if row.get::<_, i64>(1) >= now_ns => Ok(Some(read(row.get(0))?)),
            _ => Ok(None),
        }
    }

    fn mark(
        &self,
        kind: &str,
        key: &str,
        field: &str,
        value: &Value,
        now_ns: i64,
    ) -> Result<Marked, String> {
        let mut conn = self.database.conn()?;
        let hash = crate::terminal::hashed(key);
        let marked = conn
            .query_opt(
                "UPDATE dashboard_in_flight
                    SET held = jsonb_set(held, ARRAY[$3::text], $4::text::jsonb)
                  WHERE kind = $1 AND key_hash = $2 AND expires_at_ns >= $5
                    AND coalesce(held -> $3::text, 'null'::jsonb) IN ('null'::jsonb, 'false'::jsonb)
                 RETURNING held::text",
                &[&kind, &hash, &field, &value.to_string(), &now_ns],
            )
            .map_err(crate::database::said)?;
        if let Some(row) = marked {
            return Ok(Marked::Now(read(row.get(0))?));
        }
        let held = conn
            .query_opt(
                "SELECT held::text FROM dashboard_in_flight
                  WHERE kind = $1 AND key_hash = $2 AND expires_at_ns >= $3",
                &[&kind, &hash, &now_ns],
            )
            .map_err(crate::database::said)?;
        Ok(match held {
            Some(row) => Marked::Already(read(row.get(0))?),
            None => Marked::Gone,
        })
    }

    fn sweep(&self, now_ns: i64) -> Result<(), String> {
        self.database
            .conn()?
            .execute(
                "DELETE FROM dashboard_in_flight WHERE expires_at_ns < $1",
                &[&now_ns],
            )
            .map_err(crate::database::said)?;
        Ok(())
    }

    fn count(&self, kind: &str) -> Result<usize, String> {
        let row = self
            .database
            .conn()?
            .query_one(
                "SELECT count(*) FROM dashboard_in_flight WHERE kind = $1",
                &[&kind],
            )
            .map_err(crate::database::said)?;
        Ok(row.get::<_, i64>(0) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_thing_in_flight_is_spent_once_and_marked_once() {
        let store = InMemory::default();
        store
            .put(kind::SIGN_IN, "state", &json!({"v": 1}), 100, 10)
            .unwrap();
        assert_eq!(
            store.get(kind::SIGN_IN, "state", 50).unwrap(),
            Some(json!({"v": 1}))
        );
        assert_eq!(store.get(kind::PLUGIN_CODE, "state", 50).unwrap(), None);
        assert_eq!(
            store.take(kind::SIGN_IN, "state", 50).unwrap(),
            Some(json!({"v": 1}))
        );
        assert_eq!(store.take(kind::SIGN_IN, "state", 50).unwrap(), None);

        store
            .put(
                kind::AGENT_CODE,
                "code",
                &json!({"redeemed": false}),
                100,
                10,
            )
            .unwrap();
        assert_eq!(
            store
                .mark(kind::AGENT_CODE, "code", "redeemed", &json!(true), 50)
                .unwrap(),
            Marked::Now(json!({"redeemed": true}))
        );
        assert_eq!(
            store
                .mark(kind::AGENT_CODE, "code", "redeemed", &json!(true), 50)
                .unwrap(),
            Marked::Already(json!({"redeemed": true}))
        );
        assert_eq!(
            store
                .mark(kind::AGENT_CODE, "other", "redeemed", &json!(true), 50)
                .unwrap(),
            Marked::Gone
        );
    }

    #[test]
    fn expired_is_gone_and_the_oldest_go_past_the_most() {
        let store = InMemory::default();
        store.put(kind::SIGN_IN, "a", &json!(1), 10, 2).unwrap();
        assert_eq!(store.take(kind::SIGN_IN, "a", 11).unwrap(), None);
        store.put(kind::SIGN_IN, "a", &json!(1), 100, 2).unwrap();
        store.put(kind::SIGN_IN, "b", &json!(2), 100, 2).unwrap();
        store.put(kind::SIGN_IN, "c", &json!(3), 100, 2).unwrap();
        assert_eq!(store.get(kind::SIGN_IN, "a", 0).unwrap(), None);
        assert_eq!(store.get(kind::SIGN_IN, "c", 0).unwrap(), Some(json!(3)));
        store.sweep(200).unwrap();
        assert_eq!(store.get(kind::SIGN_IN, "c", 0).unwrap(), None);
    }
}
