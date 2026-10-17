//! The snapshot is the projection it was made from, and a snapshot plus its
//! tail is what a full replay makes (W9.10, contract v25; the plan's
//! question 8). Every act of every test in this crate passes through
//! [`holds_across`] on the memory store, so each flow they drive -- opening
//! balances, breaks, figures, the order path, booking, block orders,
//! corrections -- is held to it; [`holds_at_every_split`] checks a whole
//! journal at every point.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::journal::Entry;

/// Whether every act checks the snapshot across it; the benchmark, which
/// makes thousands, turns it off.
pub(crate) static CHECK_EVERY_ACT: AtomicBool = AtomicBool::new(true);

/// The test binary's allocator, counting the bytes held and the most held
/// since the last [`Counting::reset_peak`]: what the benchmark measures a
/// replay's memory by.
pub(crate) struct Counting;

static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let at = unsafe { System.alloc(layout) };
        if !at.is_null() {
            let now = HELD.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        at
    }

    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        unsafe { System.dealloc(at, layout) };
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

impl Counting {
    pub(crate) fn held() -> usize {
        HELD.load(Ordering::Relaxed)
    }

    /// Start a measurement: the peak from now, above what is held now.
    pub(crate) fn reset_peak() -> usize {
        let now = Self::held();
        PEAK.store(now, Ordering::Relaxed);
        now
    }

    pub(crate) fn peak() -> usize {
        PEAK.load(Ordering::Relaxed)
    }
}

/// A book through a snapshot's bytes is the same book, and that plus the
/// next entry is the book the act made.
pub(crate) fn holds_across(before: &Book, entry: &Entry, after: &Book) {
    if !CHECK_EVERY_ACT.load(Ordering::Relaxed) {
        return;
    }
    let kept = decode(&encode(before)).expect("a snapshot reads back");
    assert_eq!(
        &kept, before,
        "a snapshot is the projection it was made from"
    );
    let resumed = resume(kept, std::slice::from_ref(entry)).expect("its tail applies");
    assert_eq!(
        &resumed, after,
        "a snapshot and its tail are the full replay"
    );
}

/// For a whole journal: a snapshot at every entry, plus the rest, is the
/// full replay.
pub(crate) fn holds_at_every_split(account_id: &str, partition: &str, journal: &[Entry]) {
    let whole = Book::replay(account_id, partition, journal).unwrap();
    for at in 0..=journal.len() {
        let prefix = Book::replay(account_id, partition, &journal[..at]).unwrap();
        assert_eq!(
            last_sequence(&prefix),
            journal[..at].last().map(|e| e.first_sequence).unwrap_or(0)
        );
        let kept = decode(&encode(&prefix)).unwrap();
        let resumed = resume(kept, &journal[at..]).unwrap();
        assert_eq!(resumed, whole, "split at {at} of {}", journal.len());
    }
}

#[test]
fn an_empty_book_round_trips_and_another_format_is_no_snapshot() {
    let book = Book::new("ACC-1", "P0");
    assert_eq!(decode(&encode(&book)).unwrap(), book);
    let mut shape: serde_json::Value = serde_json::from_slice(&encode(&book)).unwrap();
    shape["format"] = (FORMAT + 1).into();
    let refused = decode(&serde_json::to_vec(&shape).unwrap()).unwrap_err();
    assert!(refused.to_string().contains("format"), "{refused}");
    assert!(decode(b"not a snapshot").is_err());
}

#[test]
fn the_interval_is_the_charts_or_the_default_within_its_range() {
    assert_eq!(interval_from(None), Ok(DEFAULT_INTERVAL));
    assert_eq!(interval_from(Some(" ")), Ok(DEFAULT_INTERVAL));
    assert_eq!(interval_from(Some("250")), Ok(250));
    for wrong in ["9", "100001", "-5", "many"] {
        assert!(
            interval_from(Some(wrong))
                .unwrap_err()
                .contains("MERIDIAN_BOR_SNAPSHOT_INTERVAL"),
            "{wrong}"
        );
    }
}
