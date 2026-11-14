//! A write handed to its partition's owner (step 3d; spec/a-deployment-
//! scales-out-and-fails-over-by-configuration section 6.7): every member of
//! a store serves its public write topics in the component's queue group,
//! and one that does not own a write's partition hands the request on, on
//! the store's internal topic (`platform.lake.internal.record`,
//! `platform.book.internal.command`), addressed to the owning writer group
//! by [`WRITER_GROUP_HEADER`]. Each group's writer offers the internal topic
//! in a queue group of its own, and leaves a request addressed to another
//! group unanswered.
//!
//! A store's code says a request is another group's with [`hand_to`]; the
//! bus then hands the request on, unchanged, and answers with the owner's
//! reply. A request handed here is answered here or refused: never handed
//! on again ([`handed`]) on a guess, only to the group the record names now
//! ([`hand_to_named`]), as after a move.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The target writer group's number on a hand-off (approved 2026-11-12),
/// beside `Meridian-Timeout-Ms`.
pub const WRITER_GROUP_HEADER: &str = "Meridian-Writer-Group";

thread_local! {
    static HAND_TO: Cell<Option<u32>> = const { Cell::new(None) };
    static HANDED: Cell<bool> = const { Cell::new(false) };
    static ARRIVING: Cell<bool> = const { Cell::new(false) };
    static NAMED: Cell<bool> = const { Cell::new(false) };
}

/// The next write this thread answers was handed here.
pub(crate) fn arriving() {
    ARRIVING.with(|arriving| arriving.set(true));
}

/// Whether the write this thread begins was handed here, cleared.
pub(crate) fn take_arriving() -> bool {
    ARRIVING.with(Cell::take)
}

/// The request this thread answers is writer group `group`'s: handed to it
/// once its handler returns, its own answer set aside.
pub fn hand_to(group: u32) {
    HAND_TO.with(|to| to.set(Some(group)));
}

/// The request this thread answers is writer group `group`'s as the store's
/// record names it now, read under the partition's lock or from its head
/// just now (step 3d; ruled 2026-11-14): a partition moved since the member
/// that handed it here looked. Handed on once more even where it was handed
/// here, as the record, never a stale route, says so.
pub fn hand_to_named(group: u32) {
    HAND_TO.with(|to| to.set(Some(group)));
    NAMED.with(|named| named.set(true));
}

/// Whether the group said is the one the record names now, cleared.
pub(crate) fn take_named() -> bool {
    NAMED.with(Cell::take)
}

/// Whether the request this thread answers was handed here by another
/// member: answered here, or refused, never handed on again.
pub fn handed() -> bool {
    HANDED.with(Cell::get)
}

/// The group the request was said to be, cleared.
pub(crate) fn take_hand_to() -> Option<u32> {
    HAND_TO.with(Cell::take)
}

/// A request begins on this thread: handed here or not, said to no group.
pub(crate) fn begin(handed: bool) {
    HAND_TO.with(|to| to.set(None));
    NAMED.with(|named| named.set(false));
    HANDED.with(|was| was.set(handed));
}

/// The words a request handed here that is not this group's is refused
/// with: the route was stale, and the caller asks again.
pub const NOT_THIS_GROUPS: &str =
    "handed to this writer group, which does not own its partition: ask again";

/// The writes this process handed to their owners and answered: how many,
/// their time in all, and the longest.
static HANDED_COUNT: AtomicU64 = AtomicU64::new(0);
static HANDED_NS: AtomicU64 = AtomicU64::new(0);
static HANDED_MOST_NS: AtomicU64 = AtomicU64::new(0);

/// One write passed to its owner before it was admitted, the owner
/// answering its caller (step 3d): counted with the writes handed on, in
/// the time the pass took here.
pub(crate) fn note_passed(took: Duration) {
    note_handed(took);
}

/// One write handed on and answered, in `took`.
pub(crate) fn note_handed(took: Duration) {
    let ns = took.as_nanos() as u64;
    HANDED_COUNT.fetch_add(1, Ordering::Relaxed);
    HANDED_NS.fetch_add(ns, Ordering::Relaxed);
    HANDED_MOST_NS.fetch_max(ns, Ordering::Relaxed);
}

/// The writes this process has handed to their owners and had answered:
/// how many, their time in all, and the longest (the hand-off's cost, the
/// resilience spec's section 6.7, measured against decisions/024).
pub fn handed_on() -> (u64, Duration, Duration) {
    (
        HANDED_COUNT.load(Ordering::Relaxed),
        Duration::from_nanos(HANDED_NS.load(Ordering::Relaxed)),
        Duration::from_nanos(HANDED_MOST_NS.load(Ordering::Relaxed)),
    )
}
