//! An account moved to another book partition (step 3d; the resilience
//! spec's section 6.8; W9.8 "An account moves between partitions by
//! continuation", W6.39; `MoveAccount` and `AccountMoved` approved by 1057,
//! fixtures/book/move-account.yaml).
//!
//! Two acts, each in its own partition, linked by value -- no act writes
//! two partitions (decisions/024):
//!
//! 1. **The close**, in the account's partition, by its writer: an entry
//!    carrying the move's `AccountMoved` (its old and new partitions, the
//!    closing sequence), which moves the account's placement to the new
//!    partition in its own transaction. From then on every act on the
//!    account is the new partition's, and is held -- refused UNAVAILABLE
//!    with a retry-after -- until the open.
//! 2. **The open**, in the new partition, by its writer: an entry carrying
//!    the same `AccountMoved`, now with its opening sequence. Where another
//!    writer group writes the new partition, the move is handed to it and
//!    it makes the open.
//!
//! The account's journal reads across both in the order written
//! ([`crate::journal::in_order`]), so it replays the same. The move is its
//! own record in the book's `control` partition: journalled by the control
//! partition's writer, as each takeover is, from every open not journalled
//! there yet ([`crate::control`]).

use crate::book::Book;
use crate::decide::{Context, Draft, Made};
use crate::journal::{Body, ACCOUNT_MOVED};
use crate::store::{Result, StoreError};

/// How long an act on an account waits, at most, while its move is between
/// its close and its open: the retry-after it is answered with.
pub const HELD_RETRY_SECONDS: u32 = 1;

/// The close's idempotency key: one close for each move of the account to
/// that partition from where it is.
pub fn close_key(account_id: &str, from: &str, to: &str) -> String {
    format!("account-move:{account_id}:{from}:{to}")
}

/// The open's idempotency key: one open for each close.
pub fn open_key(account_id: &str, from: &str, closing_sequence: u64) -> String {
    format!("account-open:{account_id}:{from}:{closing_sequence}")
}

/// Whether `key` is a move's open: the one act an account between its
/// close and its open takes.
pub fn is_open_key(key: &str) -> bool {
    key.starts_with("account-open:")
}

/// Where an account's chain stands, read from its journal.
#[derive(Debug, Clone, PartialEq)]
pub enum Standing {
    /// Never moved, or its last move opened: in `partition`.
    In(String),
    /// Closed in its old partition, not yet opened in the new: the move.
    Between(meridian_domain::v1::AccountMoved),
}

/// Where `entries`, an account's journal in order, leave it.
pub fn standing(entries: &[crate::journal::Entry], placed: &str) -> Standing {
    match entries.iter().rev().find_map(|entry| {
        crate::journal::moved_of(entry).map(|moved| (entry.partition.clone(), moved))
    }) {
        Some((partition, moved)) if partition == moved.from_partition => Standing::Between(moved),
        Some((partition, _)) => Standing::In(partition),
        None => Standing::In(
            entries
                .last()
                .map(|entry| entry.partition.clone())
                .unwrap_or_else(|| placed.to_string()),
        ),
    }
}

/// The close: the book's own act in the account's partition, on a
/// deployment admin's word.
pub fn close(book: &Book, head: u64, ctx: &Context, to: &str, reason: &str) -> Result<Made> {
    ctx.justified(reason, "moving an account")?;
    if book.partition == to {
        return Err(StoreError::Malformed {
            fields: vec!["target_partition".into()],
            words: format!("account {} is in {to} already", book.account_id),
        });
    }
    let moved = meridian_domain::v1::AccountMoved {
        account_id: book.account_id.clone(),
        from_partition: book.partition.clone(),
        closing_sequence: head + 1,
        to_partition: to.to_string(),
        opening_sequence: 0,
        moved_by: ctx.acting_for.clone(),
        reason: reason.to_string(),
        moved_at_ns: ctx.received_at_ns,
    };
    entry(
        book,
        head,
        ctx,
        &moved,
        close_key(&book.account_id, &book.partition, to),
        format!(
            "account {} moves from {} to {}: {reason}",
            book.account_id, book.partition, to
        ),
    )
}

/// The open: the book's own act in the new partition, continuing the chain
/// its close ended.
pub fn open(
    book: &Book,
    head: u64,
    ctx: &Context,
    closed: &meridian_domain::v1::AccountMoved,
) -> Result<Made> {
    let mut there = book.clone();
    there.partition = closed.to_partition.clone();
    let moved = meridian_domain::v1::AccountMoved {
        opening_sequence: head + 1,
        ..closed.clone()
    };
    entry(
        &there,
        head,
        ctx,
        &moved,
        open_key(
            &closed.account_id,
            &closed.from_partition,
            closed.closing_sequence,
        ),
        format!(
            "account {} continues in {} from {} at {}",
            closed.account_id, closed.to_partition, closed.from_partition, closed.closing_sequence
        ),
    )
}

fn entry(
    book: &Book,
    head: u64,
    ctx: &Context,
    moved: &meridian_domain::v1::AccountMoved,
    key: String,
    reason: String,
) -> Result<Made> {
    use prost::Message;
    let draft = Draft {
        entry_id: ctx.mint(crate::ids::ENTRY),
        kind: ACCOUNT_MOVED,
        effective_date: ctx.today(),
        reason,
        break_ids: Vec::new(),
        idempotency_key: key,
        actor: ctx.actor(),
        body: Body {
            record: Some((ACCOUNT_MOVED.to_string(), moved.encode_to_vec())),
            ..Default::default()
        },
    };
    crate::decide::made(book, head, ctx, draft)
}

// ── The act on the bus ──────────────────────────────────────────────────────

/// A deployment admin moves an account (W6.39; topic approved 2026-11-09).
pub const MOVE_ACCOUNT: &str = "platform.book.command.move-account";

/// The move's record in the book's `control` partition: its key there.
pub fn control_key(moved: &meridian_domain::v1::AccountMoved) -> String {
    format!(
        "control-record:account-moved:{}:{}:{}",
        moved.account_id, moved.from_partition, moved.closing_sequence
    )
}

/// Serve [`MOVE_ACCOUNT`]: answered by the writer of the account's partition
/// with the close, and by the writer of the new one with the open, the
/// request handed from one to the other where two groups write them.
pub fn serve(
    bus: &std::sync::Arc<meridian_bus::Bus>,
    store: &std::sync::Arc<dyn crate::store::Store>,
    clock: &std::sync::Arc<dyn meridian_clock::Clock>,
) {
    let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
    bus.serve(MOVE_ACCOUNT, move |envelope| {
        let request: meridian_domain::v1::MoveAccountRequest =
            crate::service::decode(&envelope, "meridian.v1.MoveAccountRequest")?;
        let ctx = crate::service::context_of(&envelope, clock.as_ref());
        move_account(&bus2, store.as_ref(), &ctx, &request)
            .map(|moved| {
                (
                    "meridian.v1.MoveAccountReply".to_string(),
                    prost::Message::encode_to_vec(&meridian_domain::v1::MoveAccountReply {
                        moved: Some(moved),
                    }),
                )
            })
            .map_err(|failed| failed.on_the_bus())
    });
}

fn invalid(field: &str, words: String) -> StoreError {
    StoreError::Malformed {
        fields: vec![field.to_string()],
        words,
    }
}

/// The move, from wherever it stands: closed where it was not, opened where
/// it was closed, answered as it opened where it is done.
pub fn move_account(
    bus: &meridian_bus::Bus,
    store: &dyn crate::store::Store,
    ctx: &Context,
    request: &meridian_domain::v1::MoveAccountRequest,
) -> Result<meridian_domain::v1::AccountMoved> {
    let account = request.account_id.as_str();
    let target = request.target_partition.as_str();
    if !crate::store::is_account(account) {
        return Err(invalid(
            "account_id",
            format!("{account:?} is not an account a person moves"),
        ));
    }
    let count = store.partition_count();
    if !(0..count).any(|n| crate::store::partition_named(n) == target) {
        return Err(invalid(
            "target_partition",
            format!(
                "the book has no partition {target:?}: P0 to P{}",
                count.saturating_sub(1)
            ),
        ));
    }
    let journal = store.journal(account)?;
    if journal.is_empty() {
        return Err(invalid(
            "account_id",
            format!("the book holds no account {account}"),
        ));
    }
    let placed = store
        .partition_of(account)
        .unwrap_or_else(|| crate::store::FIRST_PARTITION.to_string());
    let closed = match standing(&journal, &placed) {
        Standing::Between(moved) if moved.to_partition != target => {
            return Err(StoreError::Precondition {
                field: "account_id",
                words: format!(
                    "a move of account {account} to {} is under way: ask again once it is there",
                    moved.to_partition
                ),
            })
        }
        Standing::Between(moved) => moved,
        Standing::In(partition) if partition == target => {
            // Done already, by this same request: answered as it opened.
            if let Some(opened) = journal
                .iter()
                .rev()
                .filter_map(crate::journal::moved_of)
                .find(|moved| moved.to_partition == target && moved.opening_sequence > 0)
                .filter(|moved| moved.reason == request.reason)
            {
                // Settled again: a member that stopped after the open may
                // not have let go where the account was (step 3d).
                store.settle_move(account)?;
                return Ok(opened);
            }
            return Err(invalid(
                "target_partition",
                format!("account {account} is in {target} already"),
            ));
        }
        Standing::In(_) => {
            let key = close_key(account, &placed, target);
            let acted = store.act(account, "", &key, key.as_bytes(), &mut |book, head| {
                close(book, head, ctx, target, &request.reason)
                    .map(|made| crate::service::decided(made, Vec::new()))
            })?;
            let entry = match acted {
                crate::store::Acted::Committed(decided) => {
                    crate::service::announce(bus, &decided, None, None);
                    decided.entry
                }
                crate::store::Acted::Duplicate(_) => store
                    .journal(account)?
                    .into_iter()
                    .rev()
                    .find(|entry| entry.idempotency_key == key)
                    .ok_or_else(|| {
                        StoreError::Unavailable("the move's close is not read back".into())
                    })?,
            };
            crate::journal::moved_of(&entry)
                .ok_or_else(|| StoreError::Unavailable("the move's close carries no move".into()))?
        }
    };
    // The open, in the new partition: handed to its writer where another
    // group writes it.
    let key = open_key(account, &closed.from_partition, closed.closing_sequence);
    let acted = store.act(account, "", &key, key.as_bytes(), &mut |book, head| {
        open(book, head, ctx, &closed).map(|made| crate::service::decided(made, Vec::new()))
    })?;
    let opened = match acted {
        crate::store::Acted::Committed(decided) => {
            crate::service::announce(bus, &decided, None, None);
            crate::journal::moved_of(&decided.entry)
        }
        crate::store::Acted::Duplicate(_) => store
            .journal(account)?
            .iter()
            .rev()
            .find(|entry| entry.idempotency_key == key)
            .and_then(crate::journal::moved_of),
    }
    .ok_or_else(|| StoreError::Unavailable("the move's open is not read back".into()))?;
    // Where it crossed databases, the one it left lets go of what it
    // projected of it (step 3d; section 6.4).
    store.settle_move(account)?;
    // Its record in `control`, now where this writer holds it; the control
    // partition's writer journals it otherwise, from the open.
    record(bus, store, &opened);
    Ok(opened)
}

/// The move's own record in the book's `control` partition, the book's own
/// act, idempotent by the move: where this member writes `control`.
pub fn record(
    bus: &meridian_bus::Bus,
    store: &dyn crate::store::Store,
    moved: &meridian_domain::v1::AccountMoved,
) {
    let owed = crate::control::Owed {
        key: control_key(moved),
        kind: ACCOUNT_MOVED,
        body: prost::Message::encode_to_vec(moved),
        reason: format!(
            "account {} moved from {} to {}: {}",
            moved.account_id, moved.from_partition, moved.to_partition, moved.reason
        ),
    };
    if let Err(failed) = crate::control::journal_one(bus, store, &owed) {
        tracing::debug!(%failed, "the account's move is journalled in control by its writer");
    }
}
