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
//! on again ([`handed`]).

use std::cell::Cell;

/// The target writer group's number on a hand-off (approved 2026-11-12),
/// beside `Meridian-Timeout-Ms`.
pub const WRITER_GROUP_HEADER: &str = "Meridian-Writer-Group";

thread_local! {
    static HAND_TO: Cell<Option<u32>> = const { Cell::new(None) };
    static HANDED: Cell<bool> = const { Cell::new(false) };
    static ARRIVING: Cell<bool> = const { Cell::new(false) };
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
    HANDED.with(|was| was.set(handed));
}

/// The words a request handed here that is not this group's is refused
/// with: the route was stale, and the caller asks again.
pub const NOT_THIS_GROUPS: &str =
    "handed to this writer group, which does not own its partition: ask again";
