//! The working budget and each request's reservation against it.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::{Overloaded, MIB};

/// How long a reservation already running waits to grow before it is
/// refused: briefly, since it holds memory while it waits.
const GROW_WAIT: Duration = Duration::from_millis(250);

/// A component's working budget: what its requests may reserve together.
#[derive(Clone)]
pub struct Budget {
    inner: Arc<Inner>,
}

struct Inner {
    /// None: no limit known, every reservation granted and counted.
    working: Option<u64>,
    state: Mutex<State>,
    /// Woken whenever a reservation shrinks or ends.
    room: Condvar,
    room_async: tokio::sync::Notify,
    counts: Counts,
}

/// What a budget has done since its process started: kept in one place, for
/// the component's report.
#[derive(Default)]
struct Counts {
    granted: AtomicU64,
    refused: AtomicU64,
    shed: AtomicU64,
    peak_reserved: AtomicU64,
}

/// [`Budget::counts`]: reservations granted and refused, requests shed, and
/// the most reserved at once, since the process started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetCounts {
    pub granted: u64,
    pub refused: u64,
    pub shed: u64,
    pub peak_reserved: u64,
}

#[derive(Default)]
struct State {
    reserved: u64,
    next: u64,
    held: BTreeMap<u64, Held>,
}

struct Held {
    bytes: u64,
    cancelled: Arc<AtomicBool>,
    what: String,
}

/// What [`Budget::shed_largest`] cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shed {
    pub what: String,
    pub bytes: u64,
}

impl Budget {
    pub fn new(working: Option<u64>) -> Self {
        Self {
            inner: Arc::new(Inner {
                working,
                state: Mutex::new(State::default()),
                room: Condvar::new(),
                room_async: tokio::sync::Notify::new(),
                counts: Counts::default(),
            }),
        }
    }

    /// The working budget, none where no limit is known.
    pub fn working(&self) -> Option<u64> {
        self.inner.working
    }

    /// What is reserved now, all requests together.
    pub fn reserved(&self) -> u64 {
        self.inner.lock().reserved
    }

    /// What it has done since the process started.
    pub fn counts(&self) -> BudgetCounts {
        let counts = &self.inner.counts;
        BudgetCounts {
            granted: counts.granted.load(Ordering::Relaxed),
            refused: counts.refused.load(Ordering::Relaxed),
            shed: counts.shed.load(Ordering::Relaxed),
            peak_reserved: counts.peak_reserved.load(Ordering::Relaxed),
        }
    }

    /// How many reservations are held now.
    pub fn in_flight(&self) -> usize {
        self.inner.lock().held.len()
    }

    /// Reserve `bytes` for `what`, waiting up to `wait` for room; refused
    /// with a retry-after where none comes. For work on a blocking thread.
    pub fn reserve(
        &self,
        bytes: u64,
        what: &str,
        wait: Duration,
    ) -> Result<Reservation, Overloaded> {
        let until = Instant::now() + wait;
        let mut state = self.inner.lock();
        loop {
            if self.inner.fits(&state, bytes) {
                return Ok(self.inner.take(&mut state, bytes, what));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(self.inner.full(&state, bytes, what));
            }
            state = self
                .inner
                .room
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// [`Budget::reserve`], for a task on the async runtime.
    pub async fn reserve_async(
        &self,
        bytes: u64,
        what: &str,
        wait: Duration,
    ) -> Result<Reservation, Overloaded> {
        let until = tokio::time::Instant::now() + wait;
        loop {
            // Registered before looking, so a release between the look and
            // the wait is not missed.
            let notified = self.inner.room_async.notified();
            {
                let mut state = self.inner.lock();
                if self.inner.fits(&state, bytes) {
                    return Ok(self.inner.take(&mut state, bytes, what));
                }
                if tokio::time::Instant::now() >= until {
                    return Err(self.inner.full(&state, bytes, what));
                }
            }
            let _ = tokio::time::timeout_at(until, notified).await;
        }
    }

    /// The last line (section 10a): cancel the largest reservation not
    /// cancelled already, which its request reads at its next [`grow`] or
    /// [`check`] and answers with the overload refusal.
    pub fn shed_largest(&self) -> Option<Shed> {
        let state = self.inner.lock();
        let (_, largest) = state
            .held
            .iter()
            .filter(|(_, held)| !held.cancelled.load(Ordering::Relaxed))
            .max_by_key(|(id, held)| (held.bytes, std::cmp::Reverse(**id)))?;
        largest.cancelled.store(true, Ordering::Relaxed);
        self.inner.counts.shed.fetch_add(1, Ordering::Relaxed);
        Some(Shed {
            what: largest.what.clone(),
            bytes: largest.bytes,
        })
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A reservation's bookkeeping is a few integers: one left half
        // written by a panic is still the best count there is.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn fits(&self, state: &State, bytes: u64) -> bool {
        match self.working {
            None => true,
            // Alone, a request bigger than the whole budget is still let
            // in at its floor's worth, never turned away for ever: it grows
            // from its rows and is refused where that does not fit.
            Some(working) => {
                state.reserved.saturating_add(bytes) <= working
                    || (state.held.is_empty() && bytes > working)
            }
        }
    }

    fn take(self: &Arc<Self>, state: &mut State, bytes: u64, what: &str) -> Reservation {
        let id = state.next;
        state.next += 1;
        state.reserved += bytes;
        self.counts.granted.fetch_add(1, Ordering::Relaxed);
        self.counts
            .peak_reserved
            .fetch_max(state.reserved, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        state.held.insert(
            id,
            Held {
                bytes,
                cancelled: Arc::clone(&cancelled),
                what: what.to_string(),
            },
        );
        Reservation {
            handle: Handle {
                inner: Arc::clone(self),
                id,
                cancelled,
            },
        }
    }

    fn full(&self, state: &State, bytes: u64, what: &str) -> Overloaded {
        self.counts.refused.fetch_add(1, Ordering::Relaxed);
        let working = self.working.unwrap_or(0);
        // About as long as the requests in flight take to finish, by how far
        // past the budget this one would go: a second for each budget's
        // worth over, at least one.
        let over = state.reserved.saturating_add(bytes).saturating_sub(working);
        let seconds = 1 + (over / working.max(1)) as u32;
        Overloaded::new(
            seconds,
            format!(
                "overloaded: {what} needs {} MiB and {} of {} MiB are reserved by {} request(s)",
                bytes.div_ceil(MIB),
                state.reserved.div_ceil(MIB),
                working / MIB,
                state.held.len()
            ),
        )
    }

    fn release(&self, id: u64) {
        let mut state = self.lock();
        if let Some(held) = state.held.remove(&id) {
            state.reserved -= held.bytes;
        }
        drop(state);
        self.room.notify_all();
        self.room_async.notify_waiters();
    }
}

/// A request's working memory, held against its component's budget until
/// it is dropped.
pub struct Reservation {
    handle: Handle,
}

impl std::ops::Deref for Reservation {
    type Target = Handle;
    fn deref(&self) -> &Handle {
        &self.handle
    }
}

/// A reservation as the code answering its request reaches it: what grows,
/// shrinks and reads whether it was cancelled. Only the [`Reservation`]
/// releases it.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<Inner>,
    id: u64,
    cancelled: Arc<AtomicBool>,
}

impl Handle {
    /// What it holds now.
    pub fn bytes(&self) -> u64 {
        self.inner
            .lock()
            .held
            .get(&self.id)
            .map_or(0, |held| held.bytes)
    }

    /// Whether the shed line cancelled it.
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Hold `more` bytes too, from the rows just read: refused where the
    /// shed line cancelled it, or where the budget has no room within a
    /// brief wait -- the request is then answered with the overload
    /// refusal and frees what it held, never grown past the budget.
    pub fn grow(&self, more: u64) -> Result<(), Overloaded> {
        if self.cancelled() {
            return Err(self.shed_said());
        }
        let until = Instant::now() + GROW_WAIT;
        let mut state = self.inner.lock();
        loop {
            // Released already: nothing is held for it, so nothing grows
            // (a grow racing its release never leaves bytes reserved).
            if !state.held.contains_key(&self.id) {
                return Ok(());
            }
            let fits = match self.inner.working {
                None => true,
                Some(working) => state.reserved.saturating_add(more) <= working,
            };
            if fits {
                state.reserved += more;
                self.inner
                    .counts
                    .peak_reserved
                    .fetch_max(state.reserved, Ordering::Relaxed);
                if let Some(held) = state.held.get_mut(&self.id) {
                    held.bytes += more;
                }
                return Ok(());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let what = state
                    .held
                    .get(&self.id)
                    .map(|held| held.what.clone())
                    .unwrap_or_default();
                return Err(self.inner.full(&state, more, &what));
            }
            state = self
                .inner
                .room
                .wait_timeout(state, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// Give back `less` bytes it no longer holds: a page sent, a chunk
    /// dropped.
    pub fn shrink(&self, less: u64) {
        let mut state = self.inner.lock();
        let given = match state.held.get_mut(&self.id) {
            Some(held) => {
                let given = less.min(held.bytes);
                held.bytes -= given;
                given
            }
            None => 0,
        };
        state.reserved -= given;
        drop(state);
        self.inner.room.notify_all();
        self.inner.room_async.notify_waiters();
    }

    fn shed_said(&self) -> Overloaded {
        let state = self.inner.lock();
        let (what, bytes) = state
            .held
            .get(&self.id)
            .map(|held| (held.what.clone(), held.bytes))
            .unwrap_or_default();
        Overloaded::new(
            2,
            format!(
                "overloaded: {what}, holding {} MiB, was the largest request in flight when the \
                 component passed its shed line, and was cancelled",
                bytes.div_ceil(MIB)
            ),
        )
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.handle.inner.release(self.handle.id);
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Handle>> = const { RefCell::new(None) };
}

/// Run `work` on this thread with `reservation` as its request's own, so the
/// store code it calls reaches it by [`grow`] and [`check`] without being
/// handed it.
pub fn within<R>(reservation: &Reservation, work: impl FnOnce() -> R) -> R {
    let before = CURRENT.with(|current| current.replace(Some(reservation.handle.clone())));
    struct Restore(Option<Handle>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|current| *current.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(before);
    work()
}

/// Grow this thread's request's reservation by `more` bytes, from rows just
/// read; nothing where the thread answers no admitted request.
pub fn grow(more: u64) -> Result<(), Overloaded> {
    CURRENT.with(|current| match current.borrow().as_ref() {
        Some(reservation) => reservation.grow(more),
        None => Ok(()),
    })
}

/// Give back `less` of this thread's request's reservation.
pub fn shrink(less: u64) {
    CURRENT.with(|current| {
        if let Some(reservation) = current.borrow().as_ref() {
            reservation.shrink(less);
        }
    })
}

/// Refused where the shed line cancelled this thread's request: called
/// between a long read's chunks.
pub fn check() -> Result<(), Overloaded> {
    CURRENT.with(|current| match current.borrow().as_ref() {
        Some(reservation) if reservation.cancelled() => Err(reservation.shed_said()),
        _ => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reservation_holds_its_bytes_until_dropped_and_one_past_the_budget_waits_then_is_refused() {
        let budget = Budget::new(Some(10 * MIB));
        let first = budget
            .reserve(6 * MIB, "list-statistics for data-a", Duration::ZERO)
            .unwrap();
        assert_eq!(budget.reserved(), 6 * MIB);
        let started = Instant::now();
        let refused = budget
            .reserve(6 * MIB, "list-quotes for data-b", Duration::from_millis(50))
            .err()
            .expect("past the budget");
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(refused.retry_after_seconds >= 1);
        assert!(
            refused.words.contains("list-quotes for data-b"),
            "{refused}"
        );
        drop(first);
        assert_eq!(budget.reserved(), 0);
        let second = budget
            .reserve(6 * MIB, "list-quotes for data-b", Duration::ZERO)
            .unwrap();
        assert_eq!(second.bytes(), 6 * MIB);
    }

    #[test]
    fn a_grow_after_its_reservation_is_released_reserves_nothing() {
        let budget = Budget::new(Some(10 * MIB));
        let held = budget
            .reserve(2 * MIB, "a standing job", Duration::ZERO)
            .unwrap();
        let handle = (*held).clone();
        drop(held);
        assert_eq!(budget.reserved(), 0);
        handle.grow(4 * MIB).unwrap();
        assert_eq!(
            budget.reserved(),
            0,
            "nothing left reserved by a released one"
        );
    }

    #[test]
    fn a_waiting_reservation_is_granted_when_room_is_freed() {
        let budget = Budget::new(Some(10 * MIB));
        let first = budget.reserve(8 * MIB, "first", Duration::ZERO).unwrap();
        let waiting = {
            let budget = budget.clone();
            std::thread::spawn(move || {
                budget
                    .reserve(8 * MIB, "second", Duration::from_secs(5))
                    .map(|r| r.bytes())
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        drop(first);
        assert_eq!(waiting.join().unwrap(), Ok(8 * MIB));
    }

    #[test]
    fn growing_within_the_budget_holds_more_and_past_it_is_refused_never_grown() {
        let budget = Budget::new(Some(10 * MIB));
        let read = budget.reserve(MIB, "read", Duration::ZERO).unwrap();
        within(&read, || {
            grow(4 * MIB).unwrap();
            assert_eq!(budget.reserved(), 5 * MIB);
            let refused = grow(6 * MIB).unwrap_err();
            assert!(refused.words.contains("read"), "{refused}");
            shrink(2 * MIB);
        });
        assert_eq!(read.bytes(), 3 * MIB);
        // Outside any request, growing and checking are nothing.
        assert!(grow(100 * MIB).is_ok() && check().is_ok());
        drop(read);
        assert_eq!(budget.reserved(), 0);
    }

    #[test]
    fn the_shed_line_cancels_the_largest_request_which_reads_it_at_its_next_check() {
        let budget = Budget::new(None);
        let small = budget.reserve(MIB, "small", Duration::ZERO).unwrap();
        let large = budget.reserve(40 * MIB, "large", Duration::ZERO).unwrap();
        assert_eq!(
            budget.shed_largest(),
            Some(Shed {
                what: "large".into(),
                bytes: 40 * MIB
            })
        );
        within(&large, || {
            let refused = check().unwrap_err();
            assert!(refused.words.contains("shed line"), "{refused}");
            assert!(grow(1).is_err());
        });
        within(&small, || assert!(check().is_ok()));
        // The next shed takes the next largest, never the same one twice.
        assert_eq!(
            budget.shed_largest().map(|shed| shed.what),
            Some("small".into())
        );
        assert_eq!(budget.shed_largest(), None);
    }

    #[test]
    fn a_request_bigger_than_the_whole_budget_alone_is_let_in_and_never_waits_for_ever() {
        let budget = Budget::new(Some(4 * MIB));
        let alone = budget.reserve(8 * MIB, "alone", Duration::ZERO).unwrap();
        assert!(budget.reserve(MIB, "beside", Duration::ZERO).is_err());
        drop(alone);
        assert!(budget.reserve(MIB, "beside", Duration::ZERO).is_ok());
    }

    #[tokio::test]
    async fn an_async_reservation_waits_for_room_then_is_granted_or_refused() {
        let budget = Budget::new(Some(10 * MIB));
        let first = budget
            .reserve_async(8 * MIB, "first", Duration::ZERO)
            .await
            .unwrap();
        assert!(budget
            .reserve_async(8 * MIB, "second", Duration::from_millis(20))
            .await
            .is_err());
        let waiting = {
            let budget = budget.clone();
            tokio::spawn(async move {
                budget
                    .reserve_async(8 * MIB, "second", Duration::from_secs(5))
                    .await
                    .map(|r| r.bytes())
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(first);
        assert_eq!(waiting.await.unwrap(), Ok(8 * MIB));
    }
}
