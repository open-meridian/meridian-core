//! A job's answer, one page at a time (W12.4; contract v28's amendment,
//! ruling 794).
//!
//! An answer grows with its figures, and a listed chain's positioning over
//! a 45-day window answered 1,199,742 bytes on dev, 2026-10-27, past the
//! broker's 1 MiB: its read failed. Now a page ends at [`PAGE_BUDGET`],
//! whatever the request asks, and says where the next one starts.
//!
//! The order is the figures, then what was not evaluated, each in the
//! answer's own order; every page carries the job's ID and its whole
//! provenance, so a page is checked against the answer's hash as the answer
//! is. A cursor names the job, the answer's hash and how many entries the
//! pages before held: an answer is never changed once kept, so a cursor
//! read twice answers the same page, and one from another job is refused.

use meridian_bus::{refusal_with_status, RefusalStatus};
use meridian_domain::v1::{JobAnswer, ReadJobAnswerRequest};
use prost::Message;

/// About 512 KiB: half the broker's 1 MiB, so a page and its envelope fit
/// with room to spare (ruling 794, as the catalogue's 696).
pub(crate) const PAGE_BUDGET: usize = 512 * 1024;

/// The most entries a page holds where a request names a page size, as
/// every paged read caps it.
const MOST_PAGE_SIZE: usize = 500;

/// Room kept for next_cursor and the envelope around the page.
const KEPT_FOR_THE_REST: usize = 4 * 1024;

/// Between a cursor's parts: no job ID (`JOB-` and a ULID) or hexadecimal
/// holds one.
const PART: char = '/';

/// How much of the answer's hash a cursor carries: enough that a cursor
/// paged from another answer is told apart.
const HASH_MARK: usize = 16;

/// Whether an answer goes whole in one reply: one past a page's budget is
/// never given inline, but answered later and read in pages (W12.1's choice
/// by size, Q5).
pub(crate) fn whole_fits(answer: &JobAnswer, budget: usize) -> bool {
    answer.encoded_len() + KEPT_FOR_THE_REST <= budget
}

fn hash_mark(answer: &JobAnswer) -> String {
    let hash = answer
        .provenance
        .as_ref()
        .map(|p| p.answer_hash.as_str())
        .unwrap_or_default();
    let hex = hash.strip_prefix("sha256:").unwrap_or(hash);
    hex.chars().take(HASH_MARK).collect()
}

fn cursor_after(answer: &JobAnswer, given: usize) -> String {
    format!("{}{PART}{}{PART}{given}", answer.job_id, hash_mark(answer))
}

fn invalid(field: &str, words: impl std::fmt::Display) -> String {
    refusal_with_status(RefusalStatus::InvalidArgument, &[field.to_string()], words)
}

/// How many entries the pages before held.
fn read_cursor(cursor: &str, answer: &JobAnswer, entries: usize) -> Result<usize, String> {
    if cursor.is_empty() {
        return Ok(0);
    }
    let mut parts = cursor.splitn(3, PART);
    let (Some(job), Some(mark), Some(given)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(invalid(
            "cursor",
            "not a cursor a job's answer gave: read again from the first page",
        ));
    };
    if job != answer.job_id {
        return Err(invalid(
            "cursor",
            format!(
                "a cursor from {job}'s answer, not {}'s: a cursor is read with the job that gave it",
                answer.job_id
            ),
        ));
    }
    if mark != hash_mark(answer) {
        return Err(invalid(
            "cursor",
            format!(
                "not a cursor {}'s answer gave: read again from the first page",
                answer.job_id
            ),
        ));
    }
    given
        .parse::<usize>()
        .ok()
        .filter(|given| *given <= entries)
        .ok_or_else(|| {
            invalid(
                "cursor",
                format!(
                    "not a cursor {}'s answer gave: read again from the first page",
                    answer.job_id
                ),
            )
        })
}

/// The bytes an entry adds to the page: its tag, its length and itself.
fn added(len: usize) -> usize {
    1 + prost::encoding::encoded_len_varint(len as u64) + len
}

/// One page of `answer`, as `request` asks for it, within `budget` bytes
/// ([`PAGE_BUDGET`] but in the tests).
pub(crate) fn page(
    answer: &JobAnswer,
    request: &ReadJobAnswerRequest,
    budget: usize,
) -> Result<JobAnswer, String> {
    if request.page_size < 0 {
        return Err(invalid(
            "page_size",
            "from 0, where 0 is as many as a page holds",
        ));
    }
    let figures = answer.figures.len();
    let entries = figures + answer.not_evaluated.len();
    let start = read_cursor(&request.cursor, answer, entries)?;

    // 0 is as many as the budget takes; past 500, answered at 500.
    let most = match request.page_size {
        0 => usize::MAX,
        n => (n as usize).min(MOST_PAGE_SIZE),
    };
    let mut page = JobAnswer {
        job_id: answer.job_id.clone(),
        provenance: answer.provenance.clone(),
        ..JobAnswer::default()
    };
    let room = budget.saturating_sub(KEPT_FOR_THE_REST);
    let mut used = page.encoded_len();
    let mut at = start;
    // A page holds at least one entry, whatever its size, so a page always
    // moves the reader on.
    while at < entries && at - start < most {
        let more = if at < figures {
            added(answer.figures[at].encoded_len())
        } else {
            added(answer.not_evaluated[at - figures].encoded_len())
        };
        if at > start && used + more > room {
            break;
        }
        used += more;
        if at < figures {
            page.figures.push(answer.figures[at].clone());
        } else {
            page.not_evaluated
                .push(answer.not_evaluated[at - figures].clone());
        }
        at += 1;
    }
    if at < entries {
        page.next_cursor = cursor_after(answer, at);
    }
    Ok(page)
}

#[cfg(test)]
mod tests;
