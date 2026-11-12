//! The four reads (W9.10 to W9.12, W9.14), the parts every store shares: the
//! order records are paged in, their cursors, what a read since a watermark
//! and a read by business date answer.
//!
//! Every read answers within the reader's scope, and an empty scope is
//! nothing (W4.11). Records are paged in key order -- account, then the
//! record's own key -- and a cursor is the whole key of a page's last record,
//! each part prefixed with its length, so no identifier's own characters can
//! be read as a separator. Each page answers the watermark it was read at.

use meridian_domain::v1::{AccountAttributes, AccountFigures, BookPosition, Break, JournalRef};

use crate::book::agreement_key;
use crate::store::{FiguresRead, Mark, Page, PositionsRead, Result, Store, StoreError};

fn prefixed(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| format!("{}:{part}", part.len()))
        .collect()
}

/// A cursor read back into its parts, or refused: one this store did not
/// write would start the page somewhere nobody asked for.
pub fn parts(cursor: &str, count: usize) -> Result<Vec<String>> {
    let unreadable = || StoreError::UnreadableCursor(cursor.to_string());
    let mut rest = cursor;
    let mut out = Vec::new();
    for _ in 0..count {
        let (length, after) = rest.split_once(':').ok_or_else(unreadable)?;
        let length: usize = length.parse().map_err(|_| unreadable())?;
        out.push(after.get(..length).ok_or_else(unreadable)?.to_string());
        rest = after.get(length..).ok_or_else(unreadable)?;
    }
    if !rest.is_empty() {
        return Err(unreadable());
    }
    Ok(out)
}

/// A position's key: account, instrument, side and, from contract v22, its
/// sleeve.
pub fn position_key(record: &BookPosition) -> (String, String, i32, String) {
    (
        record.account_id.clone(),
        record.instrument_id.clone(),
        record.side,
        record.sleeve.clone(),
    )
}

pub fn position_cursor(record: &BookPosition) -> String {
    prefixed(&[
        &record.account_id,
        &record.instrument_id,
        &record.side.to_string(),
        &record.sleeve,
    ])
}

/// A cursor read back: four parts from contract v22, three from before
/// (the sleeve then empty).
pub fn from_position_cursor(cursor: &str) -> Result<(String, String, i32, String)> {
    let held = parts(cursor, 4).or_else(|_| {
        parts(cursor, 3).map(|mut held| {
            held.push(String::new());
            held
        })
    })?;
    let side = held[2]
        .parse()
        .map_err(|_| StoreError::UnreadableCursor(cursor.to_string()))?;
    Ok((held[0].clone(), held[1].clone(), side, held[3].clone()))
}

pub fn break_cursor(record: &Break) -> String {
    prefixed(&[&record.account_id, &record.break_id])
}

pub fn figures_key(record: &AccountFigures) -> (String, String, String) {
    (
        record.account_id.clone(),
        agreement_key(record.agreement.as_ref()),
        record.business_date.clone(),
    )
}

pub fn figures_cursor(record: &AccountFigures) -> String {
    let (account, agreement, date) = figures_key(record);
    prefixed(&[&account, &agreement, &date])
}

pub fn attributes_cursor(record: &AccountAttributes) -> String {
    prefixed(&[&record.account_id])
}

/// Whether a record's last change is above the watermark a read is since: a
/// partition the watermark does not name is read from its start.
pub fn above(last_change: Option<&JournalRef>, since: &Mark) -> bool {
    let Some(change) = last_change else {
        return false;
    };
    change.sequence > since.get(&change.partition).copied().unwrap_or(0)
}

/// A page of records already in key order and within the read: the first
/// `limit`, and the cursor of the last when more follow.
pub fn paged<T>(
    records: Vec<T>,
    limit: usize,
    cursor_of: impl Fn(&T) -> String,
    as_of: Mark,
) -> Page<T> {
    let more = records.len() > limit;
    let mut records = records;
    records.truncate(limit);
    let next_cursor = if more {
        records.last().map(&cursor_of).unwrap_or_default()
    } else {
        String::new()
    };
    Page {
        records,
        next_cursor,
        as_of,
    }
}

/// W9.10 by replay (Q22): the positions at the end of a business date as
/// known at a watermark, or now, from the journal, since no dated projection
/// exists in step 4. A re-run at the same watermark reproduces it.
pub fn positions_as_of(store: &dyn Store, read: &PositionsRead) -> Result<Page<BookPosition>> {
    let heads = store.heads()?;
    let as_of = read.at.clone().unwrap_or_else(|| heads.clone());
    let until = (!read.business_date.is_empty()).then_some(read.business_date.as_str());
    let after = if read.cursor.is_empty() {
        None
    } else {
        Some(from_position_cursor(&read.cursor)?)
    };
    // An account at a time, in the key's order -- its first part is the
    // account -- from the cursor's on, until the page and one more are held
    // (stage M; the store memory audit of 2026-11-09, item 7): one book
    // replayed at a time and a page's records kept, never every account's.
    let mut records = Vec::new();
    for account in accounts_from(
        replayed_accounts(store, &read.scope, &read.account_id)?,
        after.as_ref().map(|after| after.0.as_str()),
    ) {
        if records.len() > read.limit {
            break;
        }
        // From the latest snapshot holding nothing later (contract v25).
        let book = store.book_as_of(&account, &as_of, until)?;
        let mut held = Vec::new();
        for position in book.positions.values() {
            if position.removed {
                continue;
            }
            let record = position.record(&account)?;
            if after
                .as_ref()
                .is_some_and(|after| position_key(&record) <= *after)
            {
                continue;
            }
            held.push(record);
        }
        held.sort_by_key(position_key);
        records.extend(held);
    }
    records.truncate(read.limit + 1);
    Ok(paged(records, read.limit, position_cursor, as_of))
}

/// W9.12 at a watermark: each agreement's figures as they stood then.
pub fn figures_as_of(store: &dyn Store, read: &FiguresRead) -> Result<Page<AccountFigures>> {
    let heads = store.heads()?;
    let as_of = read.at.clone().unwrap_or_else(|| heads.clone());
    let after = if read.cursor.is_empty() {
        None
    } else {
        let held = parts(&read.cursor, 3)?;
        Some((held[0].clone(), held[1].clone(), held[2].clone()))
    };
    // An account at a time, in the key's order, until the page and one more
    // are held (stage M; audit item 7), as the positions are.
    let mut records: Vec<AccountFigures> = Vec::new();
    for account in accounts_from(
        replayed_accounts(store, &read.scope, &read.account_id)?,
        after.as_ref().map(|after| after.0.as_str()),
    ) {
        if records.len() > read.limit {
            break;
        }
        let book = store.book_as_of(&account, &as_of, None)?;
        let mut held: Vec<AccountFigures> = book
            .figures
            .into_values()
            .filter(|record| figures_match(record, read))
            .filter(|record| {
                after
                    .as_ref()
                    .is_none_or(|after| figures_key(record) > *after)
            })
            .collect();
        held.sort_by_key(figures_key);
        records.extend(held);
    }
    records.truncate(read.limit + 1);
    Ok(paged(records, read.limit, figures_cursor, as_of))
}

/// Whether a figures record answers a read's agreement and dates.
pub fn figures_match(record: &AccountFigures, read: &FiguresRead) -> bool {
    let wanted = agreement_key(read.agreement.as_ref());
    (wanted.is_empty() || agreement_key(record.agreement.as_ref()) == wanted)
        && (read.from_date.is_empty() || record.business_date >= read.from_date)
        && (read.to_date.is_empty() || record.business_date <= read.to_date)
}

/// The accounts in order, from `from` on (inclusive): a page's walk.
fn accounts_from(mut accounts: Vec<String>, from: Option<&str>) -> Vec<String> {
    accounts.sort();
    if let Some(from) = from {
        accounts.retain(|account| account.as_str() >= from);
    }
    accounts
}

fn replayed_accounts(
    store: &dyn Store,
    scope: &crate::store::Scope,
    named: &str,
) -> Result<Vec<String>> {
    scope.admit(named)?;
    Ok(store
        .accounts()?
        .into_iter()
        .filter(|account| scope.answers(named, account))
        .collect())
}
