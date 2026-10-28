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
//! before, by the whole read again.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::read::NotServed;
use crate::row::Observation;

/// How long an answer is held for its next page.
pub const HELD_FOR: Duration = Duration::from_secs(30);

/// The most every held answer may take together, by its rows' encoded size.
pub const MOST_BYTES: usize = 32 * 1024 * 1024;

/// A read's whole answer, before its page is cut.
#[derive(Debug, Clone, Default)]
pub struct Whole {
    pub rows: Vec<Observation>,
    pub not_served: Vec<NotServed>,
}

struct Entry {
    key: String,
    whole: Arc<Whole>,
    bytes: usize,
    held_at: Instant,
}

/// The answers held, oldest first.
#[derive(Default)]
pub struct Held {
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl Held {
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
        let bytes: usize = whole
            .rows
            .iter()
            .map(Observation::encoded_len)
            .sum::<usize>()
            + whole.rows.len() * std::mem::size_of::<Observation>();
        if bytes > MOST_BYTES {
            return;
        }
        self.forget(&key);
        while self.bytes + bytes > MOST_BYTES {
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
}
