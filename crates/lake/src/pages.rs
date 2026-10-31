//! A read's answer held for its next pages (rulings 798, 808).
//!
//! A read answers in pages at one cut-off, the cursor carrying it, so every
//! page reads as one. Each page used to answer the whole read again and cut
//! its page from it: a chain of 1,440 series read 100 at a time was fifteen
//! reads of the whole chain, and its open interest fifteen more (found on
//! dev, chart 0.1.325: a chain page's first byte at 4 to 12 s, the lake at
//! 690 to 830 MiB). The answer is now made once, at the first page, and the
//! rows the pages after it cut are held here until the last is read, for
//! [`HELD_FOR`] at most, within [`MOST_BYTES`] in all.
//!
//! What is held is exactly what the read would answer again: the rows at
//! the cursor's cut-off, read by the same reader with the same request, and
//! the configuration it was answered under -- a change to the entitlements,
//! the licences or the priorities is a new generation, and an answer held
//! from before it is read again, never served. A cursor whose answer is not
//! held (expired, evicted, another replica's, a restart) is answered as
//! before, by the whole read again, and that answer is held in turn.
//!
//! A whole answer is built a few at a time ([`Building`]; rulings 957, 958):
//! building one copies its rows through several vectors on the way, about
//! three times what the answer holds at the end, and eight readers' chain
//! loads at once took the lake past its 1 GiB on dev (2026-10-31). A read
//! past the most waits for one being built to finish; its rows are read
//! when it starts building, so it answers as of its own cut-off as before.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::read::NotServed;
use crate::row::Observation;

/// How long an answer is held for its next page.
pub const HELD_FOR: Duration = Duration::from_secs(30);

/// The most every held answer may take together, by what each holds in
/// memory ([`Whole::held_bytes`]). 64 MiB (ruling 957): a chain's
/// statistics, about 23,000 to 25,000 rows, hold 34 to 37 MiB counted so,
/// and were held under the 32 MiB before only because each was counted at
/// its encoded length.
pub const MOST_BYTES: usize = 64 * 1024 * 1024;

/// How many whole answers are built at once where the chart says nothing
/// (`lake.maxConcurrentAnswers`; ruling 958).
pub const ANSWERS_AT_ONCE: usize = 2;

/// The fewest and the most `lake.maxConcurrentAnswers` may be (ruling 958).
pub const ANSWERS_AT_ONCE_RANGE: std::ops::RangeInclusive<usize> = 1..=8;

/// The chart's `lake.maxConcurrentAnswers`, as the lake's process reads it
/// from its environment: [`ANSWERS_AT_ONCE`] where it is not set, refused
/// outside [`ANSWERS_AT_ONCE_RANGE`] rather than read as something else.
pub fn answers_at_once(value: Option<&str>) -> Result<usize, String> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(ANSWERS_AT_ONCE);
    };
    value
        .parse::<usize>()
        .ok()
        .filter(|n| ANSWERS_AT_ONCE_RANGE.contains(n))
        .ok_or_else(|| {
            format!(
                "lake.maxConcurrentAnswers is {value:?}; a whole number from {} to {}",
                ANSWERS_AT_ONCE_RANGE.start(),
                ANSWERS_AT_ONCE_RANGE.end()
            )
        })
}

/// A read's whole answer, before its page is cut.
#[derive(Debug, Clone, Default)]
pub struct Whole {
    pub rows: Vec<Observation>,
    pub not_served: Vec<NotServed>,
}

impl Whole {
    /// What the answer holds in memory: every row's place in the vector,
    /// at its capacity, and what each row holds beyond it. A row's own
    /// strings and lists are counted at what the allocator gives them, not
    /// at their encoded length: each is an allocation of its own, rounded
    /// up and carrying the allocator's header (measured against a counting
    /// allocator in tests/postgres.rs, `a_held_answer_is_counted_at_what_it_holds`).
    pub fn held_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<Observation>()
            + self
                .rows
                .iter()
                .map(|row| row.encoded_len() * heap_per_encoded_byte(row))
                .sum::<usize>()
            + self.not_served.capacity() * std::mem::size_of::<NotServed>()
    }
}

/// A row's heap per byte of its encoding, at least (see
/// [`Whole::held_bytes`]). Measured on a chain read back from Postgres
/// (2026-10-31): a quote 544 bytes of heap to 299 encoded, a statistic 512
/// to 262 -- short strings, each its own allocation of 32 bytes or more --
/// and a listed series 3,184 to 486, its template's fields a list of
/// messages each far larger in memory than encoded. A row of a kind that
/// holds lists of messages is counted as the series is.
fn heap_per_encoded_byte(row: &Observation) -> usize {
    match row {
        Observation::Series(_)
        | Observation::Derived(_)
        | Observation::Scenario(_)
        | Observation::ReportedFact(_)
        | Observation::Rate(_) => 7,
        _ => 3,
    }
}

/// Whole answers built at once, at most so many (rulings 957, 958).
pub struct Building {
    most: AtomicUsize,
    /// Those being built now, and the most there have been at once.
    now: Mutex<(usize, usize)>,
    freed: Condvar,
}

/// One whole answer being built; the next waiting may start once it drops.
pub struct Permit<'a>(&'a Building);

impl Building {
    pub fn new(most: usize) -> Self {
        Building {
            most: AtomicUsize::new(most.max(1)),
            now: Mutex::new((0, 0)),
            freed: Condvar::new(),
        }
    }

    /// How many may be built at once from now.
    pub fn build_at_most(&self, most: usize) {
        self.most.store(most.max(1), Ordering::SeqCst);
        let _now = self.now.lock().expect("building poisoned");
        self.freed.notify_all();
    }

    pub fn most(&self) -> usize {
        self.most.load(Ordering::SeqCst)
    }

    /// Wait for a place, then hold it until the permit drops.
    pub fn enter(&self) -> Permit<'_> {
        let mut now = self.now.lock().expect("building poisoned");
        while now.0 >= self.most() {
            now = self.freed.wait(now).expect("building poisoned");
        }
        now.0 += 1;
        now.1 = now.1.max(now.0);
        Permit(self)
    }

    /// Run `then` only while nothing is being built, none starting until it
    /// returns; None, without running it, while anything is.
    pub fn when_idle<R>(&self, then: impl FnOnce() -> R) -> Option<R> {
        let now = self.now.lock().expect("building poisoned");
        let ran = (now.0 == 0).then(then);
        drop(now);
        ran
    }

    /// The most that have been built at once.
    pub fn peak(&self) -> usize {
        self.now.lock().expect("building poisoned").1
    }
}

impl Default for Building {
    fn default() -> Self {
        Building::new(ANSWERS_AT_ONCE)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut now = self.0.now.lock().expect("building poisoned");
        now.0 -= 1;
        drop(now);
        self.0.freed.notify_one();
    }
}

struct Entry {
    key: String,
    whole: Arc<Whole>,
    bytes: usize,
    held_at: Instant,
}

/// The answers held, oldest first, within [`MOST_BYTES`].
pub struct Held {
    entries: VecDeque<Entry>,
    bytes: usize,
    most: usize,
}

impl Default for Held {
    fn default() -> Self {
        Held::within(MOST_BYTES)
    }
}

impl Held {
    /// Answers held within so many bytes in all.
    pub fn within(most: usize) -> Self {
        Held {
            entries: VecDeque::new(),
            bytes: 0,
            most,
        }
    }

    fn expire(&mut self, now: Instant) {
        while let Some(oldest) = self.entries.front() {
            if now.duration_since(oldest.held_at) < HELD_FOR {
                break;
            }
            let gone = self.entries.pop_front().expect("looked at");
            self.bytes -= gone.bytes;
        }
    }

    /// Hold a read's answer for its next pages; an answer past the budget
    /// alone is not held. The oldest make room.
    pub fn hold(&mut self, key: String, whole: Arc<Whole>, now: Instant) {
        self.expire(now);
        let bytes = whole.held_bytes();
        if bytes > self.most {
            return;
        }
        self.forget(&key);
        while self.bytes + bytes > self.most {
            let Some(gone) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= gone.bytes;
        }
        self.bytes += bytes;
        self.entries.push_back(Entry {
            key,
            whole,
            bytes,
            held_at: now,
        });
    }

    /// The answer held for this read, if it is.
    pub fn find(&mut self, key: &str, now: Instant) -> Option<Arc<Whole>> {
        self.expire(now);
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| Arc::clone(&entry.whole))
    }

    /// Its last page read: no longer held.
    pub fn forget(&mut self, key: &str) {
        if let Some(at) = self.entries.iter().position(|entry| entry.key == key) {
            let gone = self.entries.remove(at).expect("found");
            self.bytes -= gone.bytes;
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{ObservationMeta, Quote};

    fn whole(rows: usize) -> Arc<Whole> {
        Arc::new(Whole {
            rows: (0..rows)
                .map(|i| {
                    Observation::Quote(Quote {
                        meta: Some(ObservationMeta {
                            row_key: format!("q{i}"),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                })
                .collect(),
            not_served: Vec::new(),
        })
    }

    #[test]
    fn an_answer_is_held_until_its_last_page_or_its_time() {
        let mut held = Held::default();
        let t0 = Instant::now();
        held.hold("a".into(), whole(3), t0);
        assert_eq!(held.find("a", t0).unwrap().rows.len(), 3);
        assert!(held.find("b", t0).is_none());
        held.forget("a");
        assert!(held.find("a", t0).is_none());
        assert_eq!(held.bytes(), 0);
        held.hold("a".into(), whole(3), t0);
        assert!(held.find("a", t0 + HELD_FOR).is_none(), "expired");
        assert_eq!(held.bytes(), 0);
        assert!(held.is_empty());
    }

    #[test]
    fn the_oldest_make_room_and_none_passes_the_budget() {
        let mut held = Held::default();
        let t0 = Instant::now();
        // About a third of the budget each.
        let per = MOST_BYTES / 3 / (std::mem::size_of::<Observation>() + 8);
        for key in ["a", "b", "c", "d"] {
            held.hold(key.into(), whole(per), t0);
            assert!(held.bytes() <= MOST_BYTES);
        }
        assert!(held.find("a", t0).is_none(), "the oldest made room");
        assert!(held.find("d", t0).is_some());
        held.hold("huge".into(), whole(per * 4), t0);
        assert!(held.find("huge", t0).is_none(), "past the budget alone");
    }

    /// Ruling 957: a held answer is counted at its vector's capacity, not
    /// only at the rows in it, and at more than its rows' encoded length.
    #[test]
    fn a_held_answer_counts_its_capacity_and_its_rows_heap() {
        let mut rows = whole(10).rows.clone();
        let encoded: usize = rows.iter().map(Observation::encoded_len).sum();
        rows.reserve_exact(90);
        let roomy = Whole {
            rows,
            not_served: Vec::new(),
        };
        assert!(roomy.rows.capacity() >= 100);
        assert!(
            roomy.held_bytes()
                >= roomy.rows.capacity() * std::mem::size_of::<Observation>() + encoded
        );
        let roomy = Arc::new(roomy);
        let mut held = Held::default();
        held.hold("roomy".into(), Arc::clone(&roomy), Instant::now());
        assert_eq!(held.bytes(), roomy.held_bytes());
    }

    /// Rulings 957, 958: lake.maxConcurrentAnswers, 2 where it is not set,
    /// from 1 to 8, and anything else refused rather than read as something.
    #[test]
    fn the_answers_at_once_are_2_unless_the_chart_says_1_to_8() {
        assert_eq!(answers_at_once(None), Ok(2));
        assert_eq!(answers_at_once(Some("")), Ok(2));
        assert_eq!(answers_at_once(Some("1")), Ok(1));
        assert_eq!(answers_at_once(Some(" 8 ")), Ok(8));
        for refused in ["0", "9", "-1", "two", "2.5"] {
            let said = answers_at_once(Some(refused)).unwrap_err();
            assert!(said.contains("lake.maxConcurrentAnswers"), "{said}");
            assert!(said.contains("1 to 8"), "{said}");
        }
    }

    /// Ruling 957 (a): however many ask at once, no more than the most are
    /// built at once, and every one is built.
    #[test]
    fn no_more_than_the_most_are_built_at_once() {
        for most in [1, 2, 3] {
            let building = Arc::new(Building::new(most));
            let at_once = Arc::new(AtomicUsize::new(0));
            let highest = Arc::new(AtomicUsize::new(0));
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let (building, at_once, highest) = (
                        Arc::clone(&building),
                        Arc::clone(&at_once),
                        Arc::clone(&highest),
                    );
                    std::thread::spawn(move || {
                        let _permit = building.enter();
                        let now = at_once.fetch_add(1, Ordering::SeqCst) + 1;
                        highest.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(20));
                        at_once.fetch_sub(1, Ordering::SeqCst);
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
            assert!(highest.load(Ordering::SeqCst) <= most, "at most {most}");
            assert!((1..=most).contains(&building.peak()));
        }
    }

    /// Ruling 957 (b): what runs when idle runs only while nothing is being
    /// built, and nothing starts building until it is done.
    #[test]
    fn what_runs_when_idle_runs_only_while_nothing_is_built() {
        let building = Arc::new(Building::new(2));
        let permit = building.enter();
        assert_eq!(building.when_idle(|| ()), None, "one being built");
        drop(permit);
        let started = Arc::new(AtomicUsize::new(0));
        let ran = building.when_idle(|| {
            let (building, starting) = (Arc::clone(&building), Arc::clone(&started));
            let waiting = std::thread::spawn(move || {
                let _permit = building.enter();
                starting.store(1, Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(50));
            let before = started.load(Ordering::SeqCst);
            (before, waiting)
        });
        let (before, waiting) = ran.expect("nothing was being built");
        assert_eq!(before, 0, "no build started while it ran");
        waiting.join().unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 1, "and started after");
    }
}
