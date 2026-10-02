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
    /// Positions removed, after a placeholder's move (W9.9): kept as
    /// tombstones.
    pub tombstones: Vec<(String, i32)>,
    /// The entry this one reverses.
    pub reverses: String,
    /// A corporate action's reference, as reported, on an adjustment.
    pub event_reference: String,
    /// Each named position's encumbrances, the whole set, as recorded from a
    /// statement (W9.15): an attribute, never a movement.
    pub encumbrances: Vec<(String, i32, Vec<Encumbrance>)>,
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
