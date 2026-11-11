//! A new database timeline raises the epoch (contract v30, stage C; the
//! resilience spec's section 3, approved 2026-11-10; Kafka's KIP-101).
//!
//! A database failover or restore can lose a tail its old primary had
//! committed -- the lake's local commits, an external provider's
//! asynchronous failover, any restore -- and a writer's next commit would
//! then take sequences a follower already applied for other changes. So a
//! writer reads its database's identity -- its system identifier and the
//! timeline it writes on -- and keeps the one it last saw beside its
//! `control` partition's head row (`database_identity`). On a new one it
//! raises the epoch on its partitions' head rows, as a takeover does, and
//! records `WriterTookOver` with that cause, before it commits anything
//! there: a follower that meets the higher epoch reads its last applied
//! change back, and where the record no longer holds it, drops what it
//! applied of the partition and reloads it, so it never holds a change the
//! record does not.
//!
//! When it looks: on every connection its pool opens ([`Watch`], an r2d2
//! customizer), so a reconnect after a failover is seen before the
//! connection is used; and once as the writer starts, so a restore or a
//! restart on a new timeline is seen too. A connection the writer takes
//! after a new identity was seen raises the epoch first ([`Watch::moved`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use postgres::GenericClient;

/// Why a writer raised its epoch on a new database timeline, as its
/// `WriterTookOver` record says (`TAKEOVER_REASON_DATABASE_TIMELINE`).
pub const DATABASE_TIMELINE: &str = "the writer's database is on a new timeline";

/// How many times this process's stores raised their epoch on a new
/// timeline: what the runtime watches to forget what it committed past the
/// record ([`raised`]).
static RAISED: AtomicU64 = AtomicU64::new(0);

/// How many times a store of this process raised its epoch on a new
/// database timeline.
pub fn raised() -> u64 {
    RAISED.load(Ordering::SeqCst)
}

/// What a writer found of its database's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Timeline {
    /// The first the record has seen: kept, and nothing raised, since
    /// nothing says what came before it.
    First,
    /// The one the record last saw.
    Same,
    /// A new timeline or system identifier: the epoch raised to this, and
    /// the takeover recorded with [`DATABASE_TIMELINE`].
    Moved(u64),
    /// The record's epoch is not this writer's: nothing done; another took
    /// it over, and this one steps down.
    NotMine(u64),
}

/// The database's identity: its system identifier, and the timeline its
/// current write position is on (the first eight hex digits of its WAL
/// file's name), as `<system identifier>/<timeline>`. Empty on a server in
/// recovery -- a read replica, or a standby not yet promoted -- which has no
/// write position, and where no writer commits.
pub fn database_identity(client: &mut impl GenericClient) -> Result<String, postgres::Error> {
    Ok(client
        .query_one(
            "SELECT CASE WHEN pg_is_in_recovery() THEN ''
                    ELSE (SELECT system_identifier FROM pg_control_system())::text || '/'
                         || substr(pg_walfile_name(pg_current_wal_lsn()), 1, 8) END",
            &[],
        )?
        .get(0))
}

/// A store's pool's watch on its database's identity: each connection the
/// pool opens reads it, and one unlike the last seen marks the timeline
/// moved, for the writer to raise its epoch before it uses a connection.
#[derive(Debug, Default)]
pub struct Watch {
    seen: Mutex<Option<String>>,
    moved: AtomicBool,
    /// Held while a writer follows the timeline: one at a time, so a look
    /// and a connection's raise never read each other's raise as another
    /// writer's (CI 42958602978: the street's writer stepped down so).
    following: Mutex<()>,
}

impl Watch {
    /// Follow the timeline alone: the writer's look and each connection's
    /// raise one after the other.
    pub fn alone(&self) -> std::sync::MutexGuard<'_, ()> {
        self.following
            .lock()
            .unwrap_or_else(|held| held.into_inner())
    }

    /// Whether a connection opened since the identity was last settled saw
    /// a new one; answered once.
    pub fn moved(&self) -> bool {
        self.moved.swap(false, Ordering::SeqCst)
    }

    /// The identity a writer settled on: what later connections are held
    /// to.
    pub fn settled(&self, identity: &str) {
        *self.seen.lock().expect("watch poisoned") = Some(identity.to_string());
    }

    /// The identity could not be settled: the next connection taken looks
    /// again.
    pub fn unsettled(&self) {
        self.moved.store(true, Ordering::SeqCst);
    }

    /// A connection opened, `identity` its database's; empty from a server
    /// in recovery, where no writer commits.
    pub fn opened(&self, identity: &str) {
        if identity.is_empty() {
            return;
        }
        let mut seen = self.seen.lock().expect("watch poisoned");
        match seen.as_deref() {
            None => *seen = Some(identity.to_string()),
            Some(held) if held == identity => {}
            Some(held) => {
                tracing::warn!(
                    was = held,
                    now = identity,
                    "a connection reached the database on a new timeline: the writer raises its epoch before it commits there"
                );
                self.moved.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// The pool's half of the watch: each connection it opens reads its
/// database's identity. One that cannot -- a server still in recovery --
/// is refused, and the pool opens another.
#[derive(Debug)]
pub struct Watching(pub std::sync::Arc<Watch>);

impl r2d2::CustomizeConnection<postgres::Client, postgres::Error> for Watching {
    fn on_acquire(&self, client: &mut postgres::Client) -> Result<(), postgres::Error> {
        let identity = database_identity(client)?;
        self.0.opened(&identity);
        Ok(())
    }
}

/// Follow the database's timeline in `tx`, against the identity the record
/// keeps in `head`'s `control` row (`database_identity`), for a writer at
/// epoch `mine`: kept where it is the first, nothing where it is the same,
/// and where it is new, `raise` called in the same transaction -- which
/// raises the epoch, records the takeover and answers the epoch -- and the
/// new identity kept. `control` names the control row's key column value.
/// The caller commits.
pub fn follow(
    tx: &mut postgres::Transaction<'_>,
    head: &str,
    control: &str,
    mine: u64,
    raise: impl FnOnce(&mut postgres::Transaction<'_>) -> Result<u64, postgres::Error>,
) -> Result<(Timeline, String), postgres::Error> {
    let identity = database_identity(tx)?;
    if identity.is_empty() {
        // In recovery: no writer commits here, and nothing is settled.
        return Ok((Timeline::Same, identity));
    }
    // As it was, with no lock: the same, as it nearly always is, takes none.
    let seen = tx.query_one(
        &format!("SELECT epoch, database_identity FROM {head} WHERE partition = $1"),
        &[&control],
    )?;
    if seen.get::<_, i64>(0) as u64 == mine && seen.get::<_, String>(1) == identity {
        return Ok((Timeline::Same, identity));
    }
    let row = tx.query_one(
        &format!("SELECT epoch, database_identity FROM {head} WHERE partition = $1 FOR UPDATE"),
        &[&control],
    )?;
    let epoch = row.get::<_, i64>(0) as u64;
    let kept: String = row.get(1);
    if epoch != mine {
        return Ok((Timeline::NotMine(epoch), identity));
    }
    if kept == identity {
        return Ok((Timeline::Same, identity));
    }
    let found = if kept.is_empty() {
        Timeline::First
    } else {
        let epoch = raise(tx)?;
        RAISED.fetch_add(1, Ordering::SeqCst);
        Timeline::Moved(epoch)
    };
    tx.execute(
        &format!("UPDATE {head} SET database_identity = $1 WHERE partition = $2"),
        &[&identity, &control],
    )?;
    Ok((found, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_to_a_new_identity_marks_the_timeline_moved_once() {
        let watch = Watch::default();
        watch.opened("1/00000001");
        assert!(!watch.moved(), "the first is settled, nothing moved");
        watch.opened("1/00000001");
        assert!(!watch.moved());
        watch.opened("1/00000002");
        assert!(watch.moved());
        assert!(!watch.moved(), "answered once");
        watch.settled("1/00000002");
        watch.opened("1/00000002");
        assert!(!watch.moved());
    }
}
