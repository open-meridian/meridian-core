//! The book's control journal (step 3d; spec/a-deployment-scales-out-and-
//! fails-over-by-configuration section 6.3, option (c), ruled 2026-11-11):
//! the book's takeovers, its integrity findings and its partitions'
//! assignments to writer groups, each a `~control` entry carrying its record
//! in the shape `AccountMoved` is kept in, so the Resilience page's history
//! lists them beside every other store's.
//!
//! A takeover and a finding are written first where they happen -- a
//! takeover in its own fencing transaction, a finding by the scrub -- to
//! `book_writer_took_over` and `book_integrity_finding`; the control
//! partition's group's writer then journals each it has not, the book's own
//! act, idempotent by a key of the record's own. So every one kept before
//! 3d is backfilled the same way: the record keeps its own time, and the
//! entry is committed at the backfill's (decisions/031, honest about time).
//! An assignment is journalled where a partition's group is not the one its
//! last assignment named (none journalled: group 0), as the lake's are.

use meridian_bus::Bus;

use crate::decide::{Context, Draft};
use crate::journal::Body;
use crate::store::{Acted, Store, StoreError, CONTROL_ACCOUNT};

/// A record the control journal owes: its key, its kind, its protobuf and
/// a sentence for the entry's reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Owed {
    pub key: String,
    pub kind: &'static str,
    pub body: Vec<u8>,
    pub reason: String,
}

/// A partition's assignment's kind: the lake's control kind, the same record.
pub const OWNER: &str = "meridian.v1.PartitionOwner";

/// What the journal says when the entry is there already.
const JOURNALLED: &str = "the record is journalled already";

/// Journal every record the store says the control journal owes, each the
/// book's own act on `~control`. Says how many it journalled.
pub fn journal_owed(bus: &Bus, store: &dyn Store) -> Result<usize, String> {
    let owed = store
        .control_records_owed()
        .map_err(|failed| failed.to_string())?;
    let mut journalled = 0;
    for record in owed {
        match journal_one(bus, store, &record) {
            Ok(true) => journalled += 1,
            Ok(false) => {}
            // The control partition another group's writer holds.
            Err(StoreError::NotOwned { .. }) => return Ok(journalled),
            Err(failed) => return Err(failed.to_string()),
        }
    }
    Ok(journalled)
}

/// One record journalled in `~control`, the book's own act: whether it was
/// journalled now (false where it was already).
pub fn journal_one(bus: &Bus, store: &dyn Store, record: &Owed) -> Result<bool, StoreError> {
    let now = bus.clock().now_ns();
    let ctx = Context {
        instance_id: bus.instance_id().to_string(),
        event_time_ns: now,
        received_at_ns: now,
        committed_at_ns: now,
        ..Default::default()
    };
    let acted = store.act(
        CONTROL_ACCOUNT,
        "",
        &record.key,
        record.key.as_bytes(),
        &mut |book, head| {
            let draft = Draft {
                entry_id: ctx.mint(crate::ids::ENTRY),
                kind: record.kind,
                effective_date: ctx.today(),
                reason: record.reason.clone(),
                break_ids: Vec::new(),
                idempotency_key: record.key.clone(),
                actor: ctx.actor(),
                body: Body {
                    record: Some((record.kind.to_string(), record.body.clone())),
                    ..Default::default()
                },
            };
            let made = crate::decide::made(book, head, &ctx, draft)?;
            Ok(crate::service::decided(made, Vec::new()))
        },
    );
    match acted {
        Ok(Acted::Committed(_)) => Ok(true),
        Ok(Acted::Duplicate(_)) => Ok(false),
        Err(StoreError::Invalid(said)) if said == JOURNALLED => Ok(false),
        Err(failed) => Err(failed),
    }
}

/// The record a book change carries, where it is a `~control` entry with
/// one: its kind and its protobuf. What the conductor's history reads of
/// the book's control partition.
pub fn record_of_change(change: &meridian_changes::Change) -> Option<(String, Vec<u8>)> {
    let entry = crate::journal::Entry::from_change(change).ok()?;
    entry.body.record
}
