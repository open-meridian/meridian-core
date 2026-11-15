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

pub use meridian_changes::moves::{hold, HELD_RETRY_SECONDS, HOLD_SECONDS, HOLD_SECONDS_VAR};

/// How long before its caller stops waiting a held write is answered: its
/// refusal has to reach the caller, which otherwise hears nothing.
pub const ANSWERED_BEFORE: std::time::Duration = std::time::Duration::from_millis(500);

/// How long a write held by a move waits, as of now (section 6.8, the
/// hold): `moves.holdSeconds`, and never past its caller's
/// `Meridian-Timeout-Ms` -- it is answered OVERLOADED with a retry-after
/// [`ANSWERED_BEFORE`] the caller's deadline, where that comes first (PR CI
/// 43675491409, ha: a partition's switch between databases held its writes
/// 5.4 s; an order whose caller waited 5 s was held the hold's 5 s, and
/// its caller timed out with no answer).
pub fn hold_now() -> std::time::Duration {
    let hold = hold();
    match meridian_bus::read_after::deadline() {
        Some(deadline) => hold.min(
            deadline
                .saturating_duration_since(std::time::Instant::now())
                .saturating_sub(ANSWERED_BEFORE),
        ),
        None => hold,
    }
}

/// An act tried again while its account is held, for at most `limit`
/// (section 6.8, the hold): its writes wait for the move's open, never
/// refused inside the hold; past it, refused OVERLOADED with a retry-after.
pub fn holding<T>(limit: std::time::Duration, mut act: impl FnMut() -> Result<T>) -> Result<T> {
    let until = std::time::Instant::now() + limit;
    let mut wait = std::time::Duration::from_millis(5);
    loop {
        match act() {
            Err(StoreError::Held(_)) if std::time::Instant::now() + wait < until => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(std::time::Duration::from_millis(100));
            }
            other => return other,
        }
    }
}

/// The close's idempotency key: one close for each request to move the
/// account to that partition from where it is -- the request's message ID
/// in it, so a move back to a partition it once left is a move of its own,
/// never the first one's repeat (the review's C7). A request asked again
/// under its message ID finds its own close.
pub fn close_key(account_id: &str, from: &str, to: &str, message_id: &str) -> String {
    if message_id.is_empty() {
        format!("account-move:{account_id}:{from}:{to}")
    } else {
        format!("account-move:{account_id}:{from}:{to}:{message_id}")
    }
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
        close_key(&book.account_id, &book.partition, to, &ctx.message_id),
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
    move_account_with(
        bus,
        store,
        ctx,
        request,
        &meridian_changes::moves::Settings::from_env(),
    )
}

/// An account's move aborted before its close (step 3d; section 6.8;
/// ruled 2026-11-14): nothing changed, the account where it was, and
/// PartitionMoveAborted journalled on `~control` naming the account and its
/// target partition -- with an IntegrityFinding for a digest. Answered
/// OVERLOADED with a retry-after for lag and timeout, FAILED_PRECONDITION
/// naming `account_id` for a digest; the move may be asked again.
#[allow(clippy::too_many_arguments)]
fn aborted(
    bus: &meridian_bus::Bus,
    store: &dyn crate::store::Store,
    ctx: &Context,
    account: &str,
    placed: &str,
    target: &str,
    cause: meridian_domain::v1::MoveAbortCause,
    warmed: Option<crate::store::Warmed>,
) -> StoreError {
    use meridian_domain::v1::MoveAbortCause;
    let at_ns = ctx.received_at_ns.max(ctx.event_time_ns);
    let record = meridian_domain::v1::PartitionMoveAborted {
        component: "bor".into(),
        partition: placed.to_string(),
        target_group: store.group_of_partition(target),
        account_id: account.to_string(),
        target_partition: target.to_string(),
        cause: cause as i32,
        aborted_at_ns: at_ns,
    };
    let finding = warmed.as_ref().map(|warmed| {
        let at = warmed
            .differs_at
            .as_ref()
            .map(|entry| format!(" from entry {entry}"))
            .unwrap_or_default();
        meridian_changes::records::finding(
            "bor",
            meridian_domain::v1::IntegrityCheck::MemberDigest,
            &meridian_changes::integrity::Finding {
                partition: placed.to_string(),
                sequence: 0,
                committed_at_ns: at_ns,
                what: format!(
                    "a move of account {account} to {target}: its copy's digest differs from \
                     the source's{at} ({} against {})",
                    meridian_changes::moves::short(&warmed.copy),
                    meridian_changes::moves::short(&warmed.source)
                ),
            },
            at_ns,
        )
    });
    crate::partition_moves::journal(
        bus,
        store,
        &crate::partition_moves::Ended::Aborted(record, finding),
    );
    let why = match cause {
        MoveAbortCause::Lag => "its target's lag stayed above readyLagMs",
        MoveAbortCause::Timeout => "its close did not finish within moves.switchSeconds",
        _ => "the target's digest differs from the source's",
    };
    let words = format!(
        "the move of account {account} to {target} was aborted: {why}; the account stays in \
         {placed}; ask again"
    );
    match cause {
        MoveAbortCause::Digest => StoreError::Precondition {
            field: "account_id",
            words,
        },
        _ => StoreError::Held(words),
    }
}

/// [`move_account`] under `settings` (`moves.switchSeconds`, readyLagMs,
/// CI's lag and digest files).
pub fn move_account_with(
    bus: &meridian_bus::Bus,
    store: &dyn crate::store::Store,
    ctx: &Context,
    request: &meridian_domain::v1::MoveAccountRequest,
    settings: &meridian_changes::moves::Settings,
) -> Result<meridian_domain::v1::AccountMoved> {
    use meridian_domain::v1::MoveAbortCause;
    let account = request.account_id.as_str();
    let target = request.target_partition.as_str();
    if !crate::store::is_account(account) {
        return Err(invalid(
            "account_id",
            format!("{account:?} is not an account a person moves"),
        ));
    }
    // The partitions the book holds a head of, as its databases say -- never
    // this member's count alone, which a member started before a raised
    // `bor.partitions` still holds (CI 43636763392, ha: "the book has no
    // partition P1: P0 to P0" after the raise to four).
    let count = store.partition_count();
    let held = (0..count).any(|n| crate::store::partition_named(n) == target)
        || store.heads().is_ok_and(|heads| {
            heads.contains_key(target) && target != crate::store::CONTROL_PARTITION
        });
    if !held {
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
            // To another database, only once the book's rollout is
            // complete (ruled 2026-11-15): until then nothing is closed,
            // and it is answered OVERLOADED with a retry-after, as a move
            // aborted for lag is.
            if let Some(behind) = store.rollout_holds(account, target)? {
                return Err(StoreError::Held(format!(
                    "the move of account {account} to {target} waits for a rollout in \
                     progress: {behind}; the account stays in {placed}; ask again"
                )));
            }
            // Gate: the target's lag under readyLagMs within switchSeconds
            // (section 6.8); else aborted before anything is held.
            let gate = std::time::Instant::now();
            while settings
                .injected_lag()
                .is_some_and(|lag| lag >= settings.ready_lag)
            {
                if gate.elapsed() >= settings.switch {
                    return Err(aborted(
                        bus,
                        store,
                        ctx,
                        account,
                        &placed,
                        target,
                        MoveAbortCause::Lag,
                        None,
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // Warm, compare, close, within switchSeconds: across databases
            // the account's entries are copied to its target's and compared
            // there, and the close commits only where the account's last
            // entry is the one the copy was compared at -- the digest at the
            // close. An act since is copied and compared again; its
            // partition held past the bound aborts the move, nothing closed.
            let key = close_key(account, &placed, target, &ctx.message_id);
            let switch = std::time::Instant::now();
            let timed_out = || {
                aborted(
                    bus,
                    store,
                    ctx,
                    account,
                    &placed,
                    target,
                    MoveAbortCause::Timeout,
                    None,
                )
            };
            let acted = loop {
                let warmed = store.warm_account(account, target)?;
                if settings.digest_differs() || warmed.as_ref().is_some_and(|w| w.source != w.copy)
                {
                    let mut warmed = warmed.unwrap_or_default();
                    if settings.digest_differs() {
                        warmed.copy.push(0xff);
                    }
                    return Err(aborted(
                        bus,
                        store,
                        ctx,
                        account,
                        &placed,
                        target,
                        MoveAbortCause::Digest,
                        Some(warmed),
                    ));
                }
                let changed = std::cell::Cell::new(false);
                let left = settings.switch.saturating_sub(switch.elapsed());
                let acted = holding(left, || {
                    store.act(account, "", &key, key.as_bytes(), &mut |book, head| {
                        if let Some(warmed) = &warmed {
                            let last = book.entries.last().map(|entry| entry.entry_id.as_str());
                            if last != Some(warmed.last_entry.as_str()) {
                                changed.set(true);
                                return Err(StoreError::Precondition {
                                    field: "account_id",
                                    words: format!(
                                        "account {account} changed since its copy was compared"
                                    ),
                                });
                            }
                        }
                        close(book, head, ctx, target, &request.reason)
                            .map(|made| crate::service::decided(made, Vec::new()))
                    })
                });
                match acted {
                    Err(_) if changed.get() && switch.elapsed() < settings.switch => continue,
                    Err(_) if changed.get() => return Err(timed_out()),
                    Err(StoreError::Held(_)) => return Err(timed_out()),
                    other => break other?,
                }
            };
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
            let closed = crate::journal::moved_of(&entry).ok_or_else(|| {
                StoreError::Unavailable("the move's close carries no move".into())
            })?;
            meridian_changes::moves::points::reached("book.account.closed", account)
                .map_err(StoreError::Unavailable)?;
            closed
        }
    };
    // The open, in the new partition: handed to its writer where another
    // group writes it.
    let key = open_key(account, &closed.from_partition, closed.closing_sequence);
    // The open is its own entry, not the request's: the close carries the
    // request's message ID, and the book holds a message once (its unique
    // book_entry_message). Within one database the open under the same ID
    // was refused "duplicate key ... book_entry_message" (CI 43621875619
    // and 43631481777). The open is kept once by its own key.
    let opening = Context {
        message_id: String::new(),
        ..ctx.clone()
    };
    let acted = store
        .act(account, "", &key, key.as_bytes(), &mut |book, head| {
            open(book, head, &opening, &closed)
                .map(|made| crate::service::decided(made, Vec::new()))
        })
        .inspect_err(|failed| {
            // Another group's partition: the request goes on to its writer
            // whether or not it was handed here for its close -- the close
            // names where the account goes, so this is the record's word,
            // never a stale route (CI 43662032539, tolerant: asked of the
            // other group's member, handed here, closed, and then refused
            // "handed to this writer group, which does not own its
            // partition"; the account stayed closed and never opened).
            if matches!(failed, StoreError::NotOwned { .. }) {
                meridian_bus::owners::named_by_the_record();
            }
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

/// Every account move closed and never opened, its open made (step 3d;
/// section 6.8): the control group's sweep, over every database of the
/// book, for a close committed before `before_ns` -- its member stopped,
/// or the open's answer lost, between the two. Through `store`, which hands
/// each open to its partition's writer. Says how many were opened.
pub fn finish_opens(
    bus: &meridian_bus::Bus,
    store: &dyn crate::store::Store,
    databases: &[std::sync::Arc<crate::postgres::PostgresStore>],
    settings: &meridian_changes::moves::Settings,
    before_ns: i64,
) -> usize {
    let mut opened = 0;
    for database in databases {
        let closes = match database.closes_without_opens(before_ns) {
            Ok(closes) => closes,
            Err(failed) => {
                tracing::warn!(%failed, "the account moves owed an open were not read");
                continue;
            }
        };
        for close in closes {
            let Some(moved) = crate::journal::moved_of(&close) else {
                continue;
            };
            let key = open_key(
                &moved.account_id,
                &moved.from_partition,
                moved.closing_sequence,
            );
            let elsewhere = databases
                .iter()
                .any(|other| other.holds_entry(&moved.account_id, &key).unwrap_or(true));
            if elsewhere {
                continue;
            }
            let ctx = Context {
                acting_for: moved.moved_by.clone(),
                received_at_ns: meridian_changes::now_ns(),
                ..Default::default()
            };
            let request = meridian_domain::v1::MoveAccountRequest {
                account_id: moved.account_id.clone(),
                target_partition: moved.to_partition.clone(),
                reason: moved.reason.clone(),
            };
            let asked = match move_account_with(bus, store, &ctx, &request, settings) {
                // Its new partition another group's: asked of that group's
                // writer, as the request itself is handed on from a member
                // answering it. The sweep answers no request, so nothing
                // hands it on for it (CI 43662032539: such an open stayed
                // owed, and the account unwritable).
                Err(StoreError::NotOwned { group, .. }) => {
                    use prost::Message;
                    let envelope = meridian_bus::Envelope {
                        meta: Some(meridian_bus::MessageMeta {
                            topic: MOVE_ACCOUNT.into(),
                            acting_for_subject: moved.moved_by.clone(),
                            publisher_instance_id: bus.instance_id().to_string(),
                            message_id: format!("{key}:sweep:{}", ctx.received_at_ns),
                            published_at_ns: ctx.received_at_ns,
                            ..Default::default()
                        }),
                        payload_type: "meridian.v1.MoveAccountRequest".into(),
                        payload: request.encode_to_vec(),
                    };
                    bus.hand_to_owner(group, MOVE_ACCOUNT, envelope)
                        .map(|_| ())
                        .map_err(StoreError::Unavailable)
                }
                other => other.map(|_| ()),
            };
            match asked {
                Ok(()) => {
                    opened += 1;
                    tracing::info!(
                        account_id = moved.account_id,
                        to = moved.to_partition,
                        "an account's move closed and never opened is opened"
                    );
                }
                Err(failed) => tracing::warn!(
                    account_id = moved.account_id,
                    %failed,
                    "an account's move closed and never opened is not opened yet"
                ),
            }
        }
    }
    opened
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// An act held while its account moves is tried again until the open,
    /// never refused inside the hold; past it, refused OVERLOADED with a
    /// retry-after (section 6.8).
    #[test]
    fn a_held_act_waits_for_the_open_and_past_the_hold_is_overloaded() {
        let mut tries = 0;
        let acted = holding(Duration::from_secs(5), || {
            tries += 1;
            if tries < 3 {
                Err(StoreError::Held("moving".into()))
            } else {
                Ok(tries)
            }
        });
        assert_eq!(acted.unwrap(), 3);
        let started = std::time::Instant::now();
        let held = holding(Duration::from_millis(60), || -> Result<()> {
            Err(StoreError::Held("account ACC-1 is moving to P1".into()))
        })
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        let said = held.on_the_bus();
        assert_eq!(
            meridian_bus::refusal_retry_after(&said),
            Some(HELD_RETRY_SECONDS),
            "{said}"
        );
        assert!(
            said.contains(&(meridian_pb::v1::RefusalReason::Overloaded as i32).to_string()),
            "{said}"
        );
        // Anything else is answered at once.
        let mut once = 0;
        let refused = holding(Duration::from_secs(5), || -> Result<()> {
            once += 1;
            Err(StoreError::Invalid("no".into()))
        });
        assert!(refused.is_err() && once == 1);
        assert_eq!(hold(), Duration::from_secs(HOLD_SECONDS));
    }
}
