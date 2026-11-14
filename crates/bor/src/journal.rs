//! An entry of the journal, and how it is kept.
//!
//! An entry records what the book did, not the command that asked: its
//! movement lines with every lot identifier the book minted already in them,
//! each record it set whole, and what it reversed. Replaying the entries of
//! an account in order is the only way a projection is made, so the same
//! entries always make the same projections (W9.8; decisions/024), and a
//! later release reading an earlier entry needs no rule the earlier one had.
//!
//! Kept as JSON whose records are their protobuf encodings: the record types
//! are the contract's own, and the book defines no second shape for them.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use meridian_domain::v1::{
    AccountAttributes, AccountFigures, BasisAdjustment, Break, ChangeCause, Encumbrance, EntryMeta,
    MovementLine, OpeningBalance, OpeningSource,
};
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::store::{Result, StoreError};

/// One entry, numbered.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub entry_id: String,
    pub account_id: String,
    pub partition: String,
    /// The number of the first record it changed, which is the entry's own,
    /// and of the last: one per record, in order, no holes.
    pub first_sequence: u64,
    pub last_sequence: u64,
    /// The command's envelope identifier, empty for the book's own act.
    pub message_id: String,
    pub idempotency_key: String,
    pub meta: EntryMeta,
    pub cause: ChangeCause,
    pub body: Body,
    /// Its place in its partition's chain (step 3b, integrity at rest): the
    /// previous entry's digest, and its own, SHA-256 of that and its
    /// canonical form ([`Entry::canonical`]). Empty for an entry not yet
    /// kept, or kept before digests until the migration chains it.
    pub previous_digest: Vec<u8>,
    pub digest: Vec<u8>,
}

/// What an entry did, as the projection replays it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Body {
    /// Every position move, as lines (Q19), lot identifiers minted.
    pub lines: Vec<MovementLine>,
    pub basis_adjustments: Vec<BasisAdjustment>,
    /// Each basis adjustment's lot's holding period start before it, so its
    /// reversal can put it back.
    pub prior_holding_period_starts: Vec<String>,
    /// The lots whose cost this makes unknown again: a reversal of a stated
    /// cost.
    pub restored_unknown_costs: Vec<String>,
    /// An opening balance's sources: every position its lines touch is opened
    /// from them.
    pub sources: Vec<OpeningSource>,
    /// The standing opening balance this sets (W9.1, Q21); its journal is the
    /// entry's own, filled in as it is replayed.
    pub opening: Option<OpeningBalance>,
    /// The standing opening balance this clears: a reversal of it.
    pub clears_opening: bool,
    /// Each break as it stands after the entry, whole.
    pub breaks: Vec<Break>,
    /// The breaks this entry resolves by itself (an adjustment, a reversal):
    /// their resolution names the entry, filled in as it is replayed.
    pub resolved_by_this: Vec<String>,
    /// Each agreement's figures for its business date, whole.
    pub figures: Vec<AccountFigures>,
    /// The account's attributes as they stand after it, whole.
    pub attributes: Option<AccountAttributes>,
    /// Positions removed, after a merged record's move (W9.9): kept as
    /// tombstones.
    pub tombstones: Vec<(String, i32)>,
    /// The entry this one reverses.
    pub reverses: String,
    /// A corporate action's reference, as reported, on an adjustment.
    pub event_reference: String,
    /// Each named position's encumbrances, the whole set, as recorded from a
    /// statement (W9.15): an attribute, never a movement.
    pub encumbrances: Vec<(String, i32, Vec<Encumbrance>)>,
    /// What it did on the order path (W11, contract v22): orders opened and
    /// changed, placements, executions, charges, unplaced facts, locates and
    /// trading instructions, from which the fold derives every status.
    pub acts: Vec<crate::orders::Act>,
    /// Its lines are a fill's or a charge's (W11.13): a cash line may take
    /// the account's cash below zero, which a fact is never refused for.
    pub trade: bool,
    /// It states whether releases wait for compliance (contract v22): a new
    /// account's first entry, the attribute set, or the upgrade's backfill.
    pub states_waits: bool,
    /// Contract v28 (W9.16 to W9.18): each lifecycle event as it stands
    /// after the entry, whole.
    pub lifecycle_events: Vec<meridian_domain::v1::LifecycleEvent>,
    /// A `~control` entry's record (step 3d; section 6.3, option (c)): its
    /// kind -- `meridian.v1.WriterTookOver`, `meridian.v1.IntegrityFinding`,
    /// `meridian.v1.PartitionOwner` -- and its protobuf, in the shape the
    /// book's `AccountMoved` is kept in. None on every other entry.
    pub record: Option<(String, Vec<u8>)>,
    /// The lines the entry's partition's own clearing account,
    /// `~clearing/<partition>`, takes against this entry's (step 3d; section
    /// 6.3, ruled 2026-11-13): with more than one partition, a booking's leg
    /// moving value to or from another account balances in its own entry, so
    /// every partition balances at every sequence. Signed as the account's
    /// lines are, no lot named. Empty with one partition, and on every entry
    /// not a booking's leg.
    pub clearing: Vec<MovementLine>,
    /// A booking's pending marker (step 3d; section 6.3): on its
    /// `booking-booked` (or a replace's) entry, each child it awaits in
    /// another account -- the account, the allocation and the partition the
    /// account was in when booked, empty where it was not placed yet. The
    /// booking posts (`booking-posted`) once each is journalled. Empty with
    /// one partition.
    pub pending: Vec<(String, String, String)>,
}

#[derive(Serialize, Deserialize, Default)]
struct Kept {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lines: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    basis_adjustments: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    prior_holding_period_starts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    restored_unknown_costs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opening: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    clears_opening: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    breaks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resolved_by_this: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    figures: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attributes: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tombstones: Vec<(String, i32)>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    reverses: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    event_reference: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    encumbrances: Vec<(String, i32, Vec<String>)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    acts: Vec<crate::orders::Act>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    trade: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    states_waits: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lifecycle_events: Vec<String>,
    /// A `~control` entry's record, its kind and its protobuf (step 3d).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record: Option<(String, String)>,
    /// A booking leg's clearing lines and a booking's pending children
    /// (step 3d).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    clearing: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<(String, String, String)>,
}

fn encoded<M: Message>(message: &M) -> String {
    STANDARD.encode(message.encode_to_vec())
}

fn decoded<M: Message + Default>(text: &str) -> Result<M> {
    let bytes = STANDARD
        .decode(text)
        .map_err(|failed| StoreError::Unavailable(format!("an entry did not read: {failed}")))?;
    M::decode(bytes.as_slice())
        .map_err(|failed| StoreError::Unavailable(format!("an entry did not read: {failed}")))
}

fn all<M: Message + Default>(texts: &[String]) -> Result<Vec<M>> {
    texts.iter().map(|text| decoded(text)).collect()
}

impl Body {
    pub fn to_bytes(&self) -> Vec<u8> {
        let kept = Kept {
            lines: self.lines.iter().map(encoded).collect(),
            basis_adjustments: self.basis_adjustments.iter().map(encoded).collect(),
            prior_holding_period_starts: self.prior_holding_period_starts.clone(),
            restored_unknown_costs: self.restored_unknown_costs.clone(),
            sources: self.sources.iter().map(encoded).collect(),
            opening: self.opening.as_ref().map(encoded),
            clears_opening: self.clears_opening,
            breaks: self.breaks.iter().map(encoded).collect(),
            resolved_by_this: self.resolved_by_this.clone(),
            figures: self.figures.iter().map(encoded).collect(),
            attributes: self.attributes.as_ref().map(encoded),
            tombstones: self.tombstones.clone(),
            reverses: self.reverses.clone(),
            event_reference: self.event_reference.clone(),
            encumbrances: self
                .encumbrances
                .iter()
                .map(|(instrument, side, held)| {
                    (
                        instrument.clone(),
                        *side,
                        held.iter().map(encoded).collect(),
                    )
                })
                .collect(),
            acts: self.acts.clone(),
            trade: self.trade,
            states_waits: self.states_waits,
            lifecycle_events: self.lifecycle_events.iter().map(encoded).collect(),
            record: self
                .record
                .as_ref()
                .map(|(kind, body)| (kind.clone(), STANDARD.encode(body))),
            clearing: self.clearing.iter().map(encoded).collect(),
            pending: self.pending.clone(),
        };
        serde_json::to_vec(&kept).expect("an entry's body is plain data")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Body> {
        let kept: Kept = serde_json::from_slice(bytes).map_err(|failed| {
            StoreError::Unavailable(format!("an entry did not read: {failed}"))
        })?;
        Ok(Body {
            lines: all(&kept.lines)?,
            basis_adjustments: all(&kept.basis_adjustments)?,
            prior_holding_period_starts: kept.prior_holding_period_starts,
            restored_unknown_costs: kept.restored_unknown_costs,
            sources: all(&kept.sources)?,
            opening: kept.opening.as_deref().map(decoded).transpose()?,
            clears_opening: kept.clears_opening,
            breaks: all(&kept.breaks)?,
            resolved_by_this: kept.resolved_by_this,
            figures: all(&kept.figures)?,
            attributes: kept.attributes.as_deref().map(decoded).transpose()?,
            tombstones: kept.tombstones,
            reverses: kept.reverses,
            event_reference: kept.event_reference,
            encumbrances: kept
                .encumbrances
                .iter()
                .map(|(instrument, side, held)| Ok((instrument.clone(), *side, all(held)?)))
                .collect::<Result<_>>()?,
            acts: kept.acts,
            trade: kept.trade,
            states_waits: kept.states_waits,
            lifecycle_events: all(&kept.lifecycle_events)?,
            record: kept
                .record
                .map(|(kind, body)| {
                    STANDARD
                        .decode(body)
                        .map(|bytes| (kind, bytes))
                        .map_err(|failed| {
                            StoreError::Unavailable(format!("an entry did not read: {failed}"))
                        })
                })
                .transpose()?,
            clearing: all(&kept.clearing)?,
            pending: kept.pending,
        })
    }

    /// Whether the entry moves a position: what a reversal of the opening
    /// balance is refused over while it stands (W9.1).
    pub fn moves_positions(&self) -> bool {
        !self.lines.is_empty() || !self.basis_adjustments.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{HoldingSide, SettlementBucket};

    #[test]
    fn a_body_reads_back_as_it_was_kept() {
        let body = Body {
            lines: vec![MovementLine {
                instrument_id: "INS-1".into(),
                side: HoldingSide::Long as i32,
                bucket: SettlementBucket::Settled as i32,
                quantity: Some(
                    meridian_domain::exact::Exact::new(125, 1)
                        .unwrap()
                        .to_wire(),
                ),
                lot_id: "LOT-1".into(),
                ..Default::default()
            }],
            tombstones: vec![("LCL-1".into(), 1)],
            reverses: "ENT-1".into(),
            ..Default::default()
        };
        assert_eq!(Body::from_bytes(&body.to_bytes()).unwrap(), body);
    }
}

/// An entry as its partition's change (step 3a): carried whole, its records
/// in their protobuf encodings, so a follower applies it as the book does.
#[derive(Serialize, Deserialize)]
struct Carried {
    entry_id: String,
    account_id: String,
    first_sequence: u64,
    last_sequence: u64,
    message_id: String,
    idempotency_key: String,
    meta: String,
    cause: String,
    body: String,
    /// The previous entry's digest in its partition (step 3b), so the
    /// entry carried is the entry kept, and its own digest is taken of it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    previous_digest: String,
}

/// The columns an entry is kept with, but its reply, as stored.
pub struct Stored<'a> {
    pub entry_id: &'a str,
    pub account_id: &'a str,
    pub first_sequence: u64,
    pub last_sequence: u64,
    pub message_id: &'a str,
    pub idempotency_key: &'a str,
    pub meta: &'a [u8],
    pub cause: &'a [u8],
    pub body: &'a [u8],
    pub previous_digest: &'a [u8],
}

impl Stored<'_> {
    /// The entry as its digest is taken: its change's body, built from the
    /// bytes it is kept as, so an entry is chained and checked without
    /// being decoded (step 3b, integrity at rest).
    pub fn canonical(&self) -> Vec<u8> {
        let carried = Carried {
            entry_id: self.entry_id.to_string(),
            account_id: self.account_id.to_string(),
            first_sequence: self.first_sequence,
            last_sequence: self.last_sequence,
            message_id: self.message_id.to_string(),
            idempotency_key: self.idempotency_key.to_string(),
            meta: STANDARD.encode(self.meta),
            cause: STANDARD.encode(self.cause),
            body: STANDARD.encode(self.body),
            previous_digest: STANDARD.encode(self.previous_digest),
        };
        serde_json::to_vec(&carried).expect("an entry is plain data")
    }
}

impl Entry {
    /// The entry as its digest is taken: its change's body, which names
    /// every column it was kept with but its reply.
    pub fn canonical(&self) -> Vec<u8> {
        self.to_change(0, "").body
    }

    /// Whether the entry is the one its digest was taken of, after the
    /// previous digest it names: what a replay checks of every entry it
    /// reads. An entry kept before digests is taken as it is.
    pub fn verify(&self) -> Result<()> {
        if self.digest.is_empty()
            || meridian_changes::integrity::chained(&self.previous_digest, &self.canonical())
                == self.digest
        {
            return Ok(());
        }
        Err(StoreError::Unavailable(format!(
            "entry {} of partition {} (account {}) does not match its digest {}: the record \
             changed beneath the book, and it is not replayed from it",
            self.last_sequence,
            self.partition,
            self.account_id,
            meridian_changes::integrity::short(&self.digest)
        )))
    }

    /// The entry as its partition's change, written under `epoch` in the
    /// deployment `origin`.
    pub fn to_change(&self, epoch: u64, origin: &str) -> meridian_changes::Change {
        let carried = Carried {
            entry_id: self.entry_id.clone(),
            account_id: self.account_id.clone(),
            first_sequence: self.first_sequence,
            last_sequence: self.last_sequence,
            message_id: self.message_id.clone(),
            idempotency_key: self.idempotency_key.clone(),
            meta: STANDARD.encode(self.meta.encode_to_vec()),
            cause: STANDARD.encode(self.cause.encode_to_vec()),
            body: STANDARD.encode(self.body.to_bytes()),
            previous_digest: STANDARD.encode(&self.previous_digest),
        };
        meridian_changes::Change {
            partition: self.partition.clone(),
            sequence: self.last_sequence,
            epoch,
            origin: origin.to_string(),
            committed_at_ns: self.cause.committed_at_ns,
            kind: self.meta.kind.clone(),
            body: serde_json::to_vec(&carried).expect("an entry is plain data"),
            digest: self.digest.clone(),
        }
    }

    /// The entry a change carries.
    pub fn from_change(change: &meridian_changes::Change) -> Result<Entry> {
        let unreadable =
            |failed: String| StoreError::Unavailable(format!("a change did not read: {failed}"));
        let carried: Carried =
            serde_json::from_slice(&change.body).map_err(|e| unreadable(e.to_string()))?;
        let bytes = |text: &str| STANDARD.decode(text).map_err(|e| unreadable(e.to_string()));
        Ok(Entry {
            entry_id: carried.entry_id,
            account_id: carried.account_id,
            partition: change.partition.clone(),
            first_sequence: carried.first_sequence,
            last_sequence: carried.last_sequence,
            message_id: carried.message_id,
            idempotency_key: carried.idempotency_key,
            meta: EntryMeta::decode(bytes(&carried.meta)?.as_slice())
                .map_err(|e| unreadable(e.to_string()))?,
            cause: ChangeCause::decode(bytes(&carried.cause)?.as_slice())
                .map_err(|e| unreadable(e.to_string()))?,
            body: Body::from_bytes(&bytes(&carried.body)?)?,
            previous_digest: bytes(&carried.previous_digest)?,
            digest: change.digest.clone(),
        })
    }
}

/// An account's move's entries' kind (step 3d; the resilience spec's
/// section 6.8; W9.8, "An account moves between partitions by
/// continuation"): the close in its old partition and the open in its new,
/// each carrying the move's `AccountMoved` as a `~control` entry carries its
/// record.
pub const ACCOUNT_MOVED: &str = "meridian.v1.AccountMoved";

/// An entry's `AccountMoved`, where it is a move's close or open.
pub fn moved_of(entry: &Entry) -> Option<meridian_domain::v1::AccountMoved> {
    if entry.meta.kind != ACCOUNT_MOVED {
        return None;
    }
    let (_, body) = entry.body.record.as_ref()?;
    meridian_domain::v1::AccountMoved::decode(body.as_slice()).ok()
}

/// Whether `entry` closes its account's chain in its partition, naming the
/// partition it continues in.
pub fn closes(entry: &Entry) -> Option<String> {
    moved_of(entry)
        .filter(|moved| moved.from_partition == entry.partition)
        .map(|moved| moved.to_partition)
}

/// Whether `entry` opens its account's chain in its partition after a move.
pub fn opens(entry: &Entry) -> bool {
    moved_of(entry).is_some_and(|moved| moved.to_partition == entry.partition)
}

/// An account's journal in the order it was written (step 3d; W9.8): within
/// a partition by sequence, and across a move from the close in the old
/// partition to the open in the new, which numbers from its own head. An
/// account that never moved is in one partition, in sequence order.
pub fn in_order(mut entries: Vec<Entry>) -> Vec<Entry> {
    // An account moved between databases is read from each, its entries
    // before the move from both: each once (step 3d; section 6.4).
    let mut seen = std::collections::HashSet::new();
    entries.retain(|entry| seen.insert(entry.entry_id.clone()));
    if !entries.iter().any(|entry| entry.meta.kind == ACCOUNT_MOVED) {
        entries.sort_by_key(|entry| entry.first_sequence);
        return entries;
    }
    let mut by: std::collections::BTreeMap<String, std::collections::VecDeque<Entry>> =
        std::collections::BTreeMap::new();
    entries.sort_by(|a, b| {
        (a.partition.as_str(), a.first_sequence).cmp(&(b.partition.as_str(), b.first_sequence))
    });
    for entry in entries {
        by.entry(entry.partition.clone())
            .or_default()
            .push_back(entry);
    }
    // The chain starts in the partition whose first entry opens nothing.
    let mut at = by
        .iter()
        .find(|(_, held)| held.front().is_some_and(|first| !opens(first)))
        .map(|(partition, _)| partition.clone())
        .or_else(|| by.keys().next().cloned());
    let mut out = Vec::new();
    while let Some(partition) = at.take() {
        let Some(held) = by.get_mut(&partition) else {
            break;
        };
        while let Some(entry) = held.pop_front() {
            let next = closes(&entry);
            out.push(entry);
            if let Some(next) = next {
                at = Some(next);
                break;
            }
        }
    }
    // What no move links, after the chain, in its own order: never lost.
    for (_, held) in by {
        out.extend(held);
    }
    out
}
