//! One judge per deployment (the review of v31a-core-1, findings 1 and 3;
//! the architecture review's single-writer judge, M19).
//!
//! ha runs two dashboards. Each judged the latency monitor
//! ([`crate::latency::notices`]) and each member's health
//! ([`crate::component_health`]) and filed what it judged, so where the two
//! named one event differently -- one shed judged either side of an hour's
//! boundary -- it was filed twice. Now every copy judges, so its own reads
//! say what the deployment's judge says, but only the copy holding the
//! judges' lease files. It renews the lease every [`RENEW_EVERY`]; another
//! copy takes it only once it has gone [`LASTS_NS`] unrenewed, under the
//! next term.
//!
//! The lease is on one clock: the database's. A copy's own clock says when
//! it next asks, never whether the lease has lapsed, so two copies whose
//! clocks disagree by more than the lease lasts (a fault pass stepped one
//! by 40 s) still find one holder between them.
//!
//! The holder keeps each judge's place beside the notices it files, in one
//! transaction that first finds its term still holding the lease
//! ([`NoticeStore::file`]). A copy that takes the lease up restores each
//! judge's place from what the last holder kept with its last notices, and
//! judges on from there: what the last holder filed is not judged again,
//! and what it judged and never filed is judged again and filed. An
//! episode is filed neither twice nor never as the lease passes. The
//! notices' keyed IDs stay a second guard.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use crate::latency::notices::{Notice, NoticeStore};

/// The judges' one lease: the latency judge's and the health judge's.
pub const LEASE: &str = "judges";

/// How long the lease lasts unrenewed: three renewals.
pub const LASTS_NS: i64 = 45 * 1_000_000_000;

/// How often its holder renews it, and every other copy asks for it.
pub const RENEW_EVERY: Duration = Duration::from_secs(15);

/// This copy's name as the lease's holder, made once at its start.
pub fn holder() -> &'static str {
    static HOLDER: OnceLock<String> = OnceLock::new();
    HOLDER.get_or_init(|| {
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

/// The term this copy holds the lease under, as it last renewed it; 0 for
/// none.
static TERM: AtomicI64 = AtomicI64::new(0);

/// The term this copy holds the lease under, where it does.
pub fn term() -> Option<i64> {
    match TERM.load(Ordering::Relaxed) {
        0 => None,
        term => Some(term),
    }
}

/// Ask for the lease, or renew it, every [`RENEW_EVERY`] from now on. The
/// task is handed back, for the component to end before its runtime stops.
pub fn lead_forever() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(RENEW_EVERY);
        loop {
            every.tick().await;
            let asked = tokio::task::spawn_blocking(move || {
                crate::latency::notices::store().lead(LEASE, holder(), LASTS_NS)
            })
            .await
            .map_err(|failed| failed.to_string())
            .and_then(|asked| asked);
            let term = match asked {
                Ok(term) => term.unwrap_or(0),
                Err(failed) => {
                    // Not renewed, so not filed by: the filing finds that
                    // too, and another copy takes the lease once it lapses.
                    tracing::warn!(%failed, "the judges' lease could not be asked for");
                    0
                }
            };
            let was = TERM.swap(term, Ordering::Relaxed);
            if was != term && term != 0 {
                tracing::info!(term, "this dashboard now judges for the deployment");
            } else if was != 0 && term == 0 {
                tracing::info!(was, "this dashboard no longer judges for the deployment");
            }
        }
    })
}

/// One judge's part in the lease: the term it last took its place up
/// under, and the place it last kept.
#[derive(Debug)]
pub struct Judging {
    judge: &'static str,
    taken: Option<i64>,
    kept: String,
}

impl Judging {
    pub fn new(judge: &'static str) -> Self {
        Self {
            judge,
            taken: None,
            kept: String::new(),
        }
    }

    /// Before judging. Where this copy holds the lease under `term` and the
    /// judge has not yet taken its place up under it: the place the last
    /// holder kept, `Some(None)` where none was ever kept, for the judge to
    /// restore. Otherwise none.
    pub fn take_up(
        &mut self,
        store: &dyn NoticeStore,
        term: Option<i64>,
    ) -> Option<Option<String>> {
        let Some(term) = term else {
            self.taken = None;
            return None;
        };
        if self.taken == Some(term) {
            return None;
        }
        match store.place(self.judge) {
            Ok(place) => {
                self.taken = Some(term);
                self.kept = place.clone().unwrap_or_default();
                Some(place)
            }
            Err(failed) => {
                // Judged as a copy that does not hold the lease, filing
                // nothing, and taken up again next time.
                tracing::warn!(judge = self.judge, %failed, "a judge's place could not be read");
                None
            }
        }
    }

    /// Its place taken up again before it next judges: what it judged
    /// now is judged again, from the place last kept.
    pub fn again(&mut self) {
        self.taken = None;
    }

    /// Whether this judge files: its place is taken up under the lease.
    pub fn files(&self) -> bool {
        self.taken.is_some()
    }

    /// After judging: `notices` filed and `place` kept, together, where
    /// this judge files and has something new to keep. A copy that has lost
    /// the lease files nothing, and one whose filing failed takes its place
    /// up again before it next judges, so what it judged since is judged
    /// again rather than lost.
    pub fn file(
        &mut self,
        store: &dyn NoticeStore,
        holder: &str,
        now_ns: i64,
        notices: &[Notice],
        place: String,
    ) -> Result<bool, String> {
        let Some(term) = self.taken else {
            return Ok(false);
        };
        if notices.is_empty() && place == self.kept {
            return Ok(true);
        }
        match store.file(LEASE, holder, term, now_ns, self.judge, notices, &place) {
            Ok(true) => {
                self.kept = place;
                Ok(true)
            }
            Ok(false) => {
                self.taken = None;
                Ok(false)
            }
            Err(failed) => {
                self.taken = None;
                Err(failed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latency::notices::InMemory;

    #[test]
    fn a_lease_is_held_by_one_copy_until_it_lapses_then_taken_under_the_next_term() {
        let store = InMemory::default();
        let lead = |holder: &str, at: i64| {
            store.at(at);
            store.lead(LEASE, holder, LASTS_NS).unwrap()
        };
        assert_eq!(lead("a", 0), Some(1));
        assert_eq!(lead("b", 10), None);
        assert_eq!(lead("a", 20), Some(1));
        // a stops renewing; b takes it once it has lapsed.
        let lapsed = 20 + LASTS_NS + 1;
        assert_eq!(lead("b", lapsed), Some(2));
        // a, back, files nothing under its old term.
        let mut a = Judging::new("latency");
        a.taken = Some(1);
        assert!(!a.file(&store, "a", lapsed, &[], "{}".into()).unwrap());
        assert!(!a.files());
        assert_eq!(lead("a", lapsed), None);
    }
}
