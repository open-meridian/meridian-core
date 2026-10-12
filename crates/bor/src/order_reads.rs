//! The order path's reads (W11.16 to W11.18, contract v22), the parts every
//! store shares: the order records are paged in, their cursors, and the
//! reads answered from books replayed in memory.
//!
//! ListOrders answers each kind of record the order path delivers in its own
//! list -- orders, placements, executions, unplaced facts -- paged as one
//! stream in that order, each kind by account and identifier, so a catch-up
//! since a watermark reads the records its rows carry (the build's shapes,
//! settled 2026-10-12). A cursor is the whole key of a page's last record.

use meridian_domain::v1::{EntryMeta, JournalRef, Locate, TradingInstruction};

use crate::book::Book;
use crate::orders;
use crate::reads::{above, parts};
use crate::store::{
    is_account, HistoryRead, InstructionsRead, LocatesRead, Mark, OrdersPage, OrdersRead, Page,
    Result, Scope, StoreError,
};

/// The four kinds, in the order a page reads them.
pub const KINDS: [&str; 4] = ["order", "placement", "execution", "unplaced"];

fn prefixed(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| format!("{}:{part}", part.len()))
        .collect()
}

/// A record's place in the stream: its kind's index, its account and its ID.
pub type StreamKey = (usize, String, String);

pub fn stream_cursor(key: &StreamKey) -> String {
    prefixed(&[&key.0.to_string(), &key.1, &key.2])
}

pub fn from_stream_cursor(cursor: &str) -> Result<StreamKey> {
    let held = parts(cursor, 3)?;
    let kind = held[0]
        .parse()
        .map_err(|_| StoreError::UnreadableCursor(cursor.to_string()))?;
    Ok((kind, held[1].clone(), held[2].clone()))
}

/// Whether a book's records answer a read naming `named`: an account's
/// within the scope; the facts naming no account only a core reader's, and
/// only when the read names no account (W11.13).
pub fn book_answers(scope: &Scope, named: &str, key: &str) -> bool {
    if is_account(key) {
        return scope.answers(named, key);
    }
    key == crate::store::NO_ACCOUNT && named.is_empty() && *scope == Scope::Everything
}

/// One record of the stream, whole.
pub enum Item {
    Order(Box<meridian_domain::v1::Order>),
    Placement(meridian_domain::v1::Placement),
    Execution(meridian_domain::v1::Execution),
    Unplaced(meridian_domain::v1::UnplacedFact),
}

fn wanted(since: &Option<Mark>, last_change: Option<&JournalRef>) -> bool {
    match since {
        Some(since) => above(last_change, since),
        None => true,
    }
}

/// Every record of one book a read answers, with its stream key.
pub fn items_of(key: &str, book: &Book, since: &Option<Mark>) -> Result<Vec<(StreamKey, Item)>> {
    let ledger = &book.ledger;
    let mut out = Vec::new();
    for (id, held) in &ledger.orders {
        if wanted(since, held.last_change.as_ref()) {
            out.push((
                (0, key.to_string(), id.clone()),
                Item::Order(Box::new(ledger.order_record(id, true)?)),
            ));
        }
    }
    for (id, held) in &ledger.placements {
        if wanted(since, held.last_change.as_ref()) {
            out.push((
                (1, key.to_string(), id.clone()),
                Item::Placement(ledger.placement_record(id)?),
            ));
        }
    }
    for (id, held) in &ledger.executions {
        if wanted(since, held.execution.last_change.as_ref()) {
            out.push((
                (2, key.to_string(), id.clone()),
                Item::Execution(held.execution.clone()),
            ));
        }
    }
    for (id, held) in &ledger.unplaced {
        if wanted(since, held.last_change.as_ref()) {
            out.push((
                (3, key.to_string(), id.clone()),
                Item::Unplaced(held.clone()),
            ));
        }
    }
    Ok(out)
}

/// A page of the stream from records already gathered.
pub fn orders_page(
    mut items: Vec<(StreamKey, Item)>,
    read: &OrdersRead,
    as_of: Mark,
) -> Result<OrdersPage> {
    items.sort_by(|a, b| a.0.cmp(&b.0));
    if !read.cursor.is_empty() {
        let after = from_stream_cursor(&read.cursor)?;
        items.retain(|(key, _)| *key > after);
    }
    let more = items.len() > read.limit;
    items.truncate(read.limit);
    let next_cursor = if more {
        items
            .last()
            .map(|(key, _)| stream_cursor(key))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let mut page = OrdersPage {
        next_cursor,
        as_of,
        ..Default::default()
    };
    for (_, item) in items {
        match item {
            Item::Order(order) => page.orders.push(*order),
            Item::Placement(placement) => page.placements.push(placement),
            Item::Execution(execution) => page.executions.push(execution),
            Item::Unplaced(unplaced) => page.unplaced.push(unplaced),
        }
    }
    Ok(page)
}

/// An order's history from its account's journal: every entry that changed
/// it, from the first, each with its actor and times.
pub fn history_page(entries: Vec<EntryMeta>, read: &HistoryRead) -> Result<Page<EntryMeta>> {
    let start = if read.cursor.is_empty() {
        0
    } else {
        parts(&read.cursor, 1)?[0]
            .parse::<usize>()
            .map_err(|_| StoreError::UnreadableCursor(read.cursor.clone()))?
    };
    let end = (start + read.limit).min(entries.len());
    let records = entries.get(start..end).unwrap_or_default().to_vec();
    Ok(Page {
        next_cursor: if end < entries.len() {
            prefixed(&[&end.to_string()])
        } else {
            String::new()
        },
        records,
        as_of: Mark::new(),
    })
}

/// One book's locates a read answers.
pub fn locates_of(key: &str, book: &Book, read: &LocatesRead) -> Result<Vec<Locate>> {
    if !book_answers(&read.scope, &read.account_id, key) || !is_account(key) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (id, held) in &book.ledger.locates {
        if wanted(&read.since, held.locate.last_change.as_ref()) {
            out.push(book.ledger.locate_record(id)?);
        }
    }
    Ok(out)
}

/// One book's trading instructions a read answers: every one in force,
/// whatever the reader's scope, or those changed since a watermark.
pub fn instructions_of(book: &Book, read: &InstructionsRead) -> Vec<(String, TradingInstruction)> {
    book.ledger
        .instructions
        .iter()
        .filter(|(_, held)| wanted(&read.since, held.last_change.as_ref()))
        .map(|(key, held)| (key.clone(), held.clone()))
        .collect()
}

/// A page of instructions, keyed by their standing key.
pub fn keyed_page<T>(
    mut records: Vec<(String, T)>,
    cursor: &str,
    limit: usize,
    as_of: Mark,
) -> Result<Page<T>> {
    records.sort_by(|a, b| a.0.cmp(&b.0));
    if !cursor.is_empty() {
        let after = parts(cursor, 1)?.remove(0);
        records.retain(|(key, _)| *key > after);
    }
    let more = records.len() > limit;
    records.truncate(limit);
    let next_cursor = if more {
        records
            .last()
            .map(|(key, _)| prefixed(&[key]))
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Page {
        records: records.into_iter().map(|(_, record)| record).collect(),
        next_cursor,
        as_of,
    })
}

/// Each live order of a book come due to expire by `now_ns` (W11.13: rows
/// i4 and i7).
pub fn expiring_of(key: &str, book: &Book, now_ns: i64) -> Result<Vec<(String, String)>> {
    use meridian_domain::v1::OrderState;
    let mut out = Vec::new();
    for (id, held) in &book.ledger.orders {
        let Some(at) = orders::due_at(held) else {
            continue;
        };
        if at > now_ns {
            continue;
        }
        if matches!(
            book.ledger.state(id)?,
            OrderState::Proposed | OrderState::Released | OrderState::Working
        ) {
            out.push((key.to_string(), id.clone()));
        }
    }
    Ok(out)
}
