//! A statement's rules the generated operations do not carry (W2.2), and how
//! a plugin built before contract v7 is read.
//!
//! From v7 a statement names the external account it was read for, which
//! this sidecar translates through the link as it does a row's, refusing it
//! when nobody has linked it (stamped.tsv); and its figures are a set per
//! margin segment, each with the collateral held under it. A plugin built
//! before v7 sends a statement naming no external account, with its three
//! figures flat: it is admitted with no account, which the statement takes
//! from its first row in the street store, and its figures are read there as
//! the set with no segment (resolved point 5, accepted 2026-10-01).
//!
//! Three things about the figures are refused here, naming the field, before
//! the statement leaves: the flat three beside `figures`, which only a plugin
//! built for v7 can send; two sets naming one segment; and a collateral
//! balance neither posted nor received. The SDK refuses the same before it
//! sends; this is for a plugin that built its params by hand, and the street
//! store refuses each again.

use std::collections::BTreeSet;

use meridian_domain::v1::{CollateralDirection, RecordHoldingsStatementRequest};
use prost::Message;
use tonic::Status;

use crate::service::Sidecar;

/// The statement's message, whose rules these are.
pub(crate) const STATEMENT: &str = "meridian.v1.RecordHoldingsStatementRequest";

/// The first contract whose statements name their external account.
pub(crate) const STATEMENT_NAMES_ITS_ACCOUNT_FROM: u32 = 7;

impl Sidecar {
    /// Whether the plugin registered with a contract before `version`.
    pub(crate) fn built_before(&self, version: u32) -> bool {
        self.registration()
            .map(|registration| crate::contract::declared(&registration.contract_version) < version)
            .unwrap_or(false)
    }

    /// Whether `payload_type`, naming no external account, is admitted with
    /// no account: a statement from a plugin built before v7.
    pub(crate) fn names_none_before_v7(&self, payload_type: &str) -> bool {
        payload_type == STATEMENT && self.built_before(STATEMENT_NAMES_ITS_ACCOUNT_FROM)
    }

    /// The refusal of a statement's figures that cannot stand, naming the
    /// field; nothing for any other message.
    #[allow(clippy::result_large_err)]
    pub(crate) fn statement_stands(
        &self,
        payload_type: &str,
        payload: &[u8],
    ) -> Result<(), Status> {
        if payload_type != STATEMENT {
            return Ok(());
        }
        let statement = RecordHoldingsStatementRequest::decode(payload)
            .map_err(|failed| Status::internal(format!("the statement did not read: {failed}")))?;
        match figures_refused(&statement) {
            None => Ok(()),
            Some(refusal) => {
                self.note_refusal(&refusal);
                Err(Status::invalid_argument(refusal))
            }
        }
    }
}

/// Why a statement's figures cannot stand, or nothing (W2.2).
pub(crate) fn figures_refused(statement: &RecordHoldingsStatementRequest) -> Option<String> {
    if !statement.figures.is_empty() {
        let flat = [
            ("buying_power", statement.buying_power.is_some()),
            ("margin_requirement", statement.margin_requirement.is_some()),
            ("maintenance_excess", statement.maintenance_excess.is_some()),
        ];
        if let Some((name, _)) = flat.iter().find(|(_, sent)| *sent) {
            return Some(format!(
                "{name} is read from a plugin before v7; send it in figures"
            ));
        }
    }
    let mut named = BTreeSet::new();
    for (i, figures) in statement.figures.iter().enumerate() {
        if !named.insert(figures.segment.as_str()) {
            return Some(format!(
                "figures[{i}].segment \"{}\" is named twice; a statement has one set per segment",
                figures.segment
            ));
        }
        for (j, collateral) in figures.collateral.iter().enumerate() {
            if collateral.direction == CollateralDirection::Unspecified as i32 {
                return Some(format!(
                    "figures[{i}].collateral[{j}].direction is unspecified; collateral is \
                     posted or received"
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use meridian_domain::v1::{Money, ReportedCollateral, StatementFigures};

    use super::*;

    fn segment(name: &str) -> StatementFigures {
        StatementFigures {
            segment: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_statement_with_its_figures_in_sets_stands() {
        let statement = RecordHoldingsStatementRequest {
            figures: vec![segment(""), segment("commodities")],
            ..Default::default()
        };
        assert_eq!(figures_refused(&statement), None);
        assert_eq!(
            figures_refused(&RecordHoldingsStatementRequest {
                buying_power: Some(Money::default()),
                ..Default::default()
            }),
            None,
            "flat alone is a plugin before v7's, read as the set with no segment"
        );
    }

    #[test]
    fn the_flat_figures_beside_sets_are_refused_naming_the_first() {
        let statement = RecordHoldingsStatementRequest {
            maintenance_excess: Some(Money::default()),
            figures: vec![segment("")],
            ..Default::default()
        };
        assert_eq!(
            figures_refused(&statement).unwrap(),
            "maintenance_excess is read from a plugin before v7; send it in figures"
        );
    }

    #[test]
    fn a_segment_named_twice_and_collateral_with_no_direction_are_refused() {
        let twice = RecordHoldingsStatementRequest {
            figures: vec![segment("securities"), segment("securities")],
            ..Default::default()
        };
        assert_eq!(
            figures_refused(&twice).unwrap(),
            "figures[1].segment \"securities\" is named twice; a statement has one set per segment"
        );
        let undirected = RecordHoldingsStatementRequest {
            figures: vec![StatementFigures {
                collateral: vec![ReportedCollateral::default()],
                ..segment("")
            }],
            ..Default::default()
        };
        assert_eq!(
            figures_refused(&undirected).unwrap(),
            "figures[0].collateral[0].direction is unspecified; collateral is posted or received"
        );
    }
}
