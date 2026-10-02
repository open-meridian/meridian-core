//! Opening a statement and recording its rows. W2.2, W2.3 and W2.4; and
//! moving the positions they made off a placeholder once it is replaced, W3.9.
//!
//! The wire's shapes translated into the street store's, and back. A deliberate
//! translation rather than storing the generated types: the store's shape is
//! the street store's business and the wire's is the contract's, and letting one be
//! the other means a schema change reaches into the street store without passing
//! anything that could object.

use std::collections::BTreeSet;

use meridian_domain::v1::{
    ChangeCause, CollateralDirection, CustodialPositionUpdatedEvent, HoldingSide,
    Identifier as PbIdentifier, InstrumentReplacedEvent, JournalRef, RecordHoldingReply,
    RecordHoldingRequest, RecordHoldingsStatementReply, RecordHoldingsStatementRequest,
    ReportedCollateral, ReportedEncumbrance, ReportedLot, StatementFigures, StatementRecordedEvent,
};

use crate::amounts::{Money, Quantity};
use crate::ids;
use crate::store::{
    Cause, Change, Collateral, Completion, Cost, Counts, CustodialPosition, Direction, Encumbrance,
    Figures, Holding, Identifier, Lot, Opened, Result, Settled, Side, Statement, Store, StoreError,
    PARTITION,
};

/// What opening a statement produced.
#[derive(Debug, Clone)]
pub struct Opening {
    pub reply: RecordHoldingsStatementReply,

    /// Present when the statement promised no rows, which completes it at
    /// once. An account that holds nothing is a real answer.
    pub completed: Option<StatementRecordedEvent>,
}

/// W2.2. Open a statement, or recognise one the rail has sent before.
///
/// Redelivery is a no-op and not a duplicate, which is the fixture's second
/// case. The reply says which happened, so a connector that cannot tell whether
/// its last attempt landed can simply send it again.
pub fn open_statement(
    store: &dyn Store,
    request: &RecordHoldingsStatementRequest,
    cause: &Cause,
) -> Result<Opening> {
    let (statement, opened, completion) = store.open(
        Statement {
            statement_id: ids::statement(cause.committed_at_ns),
            source: request.source.clone(),
            external_statement_id: request.external_statement_id.clone(),
            as_of_date: request.as_of_date.clone(),
            read_at_ns: request.read_at_ns,
            expected_rows: request.expected_rows.max(0) as u32,
            account_id: request.account_id.clone(),
            external_account_id: request.external_account_id.clone(),
            institution: request.institution.clone(),
            // As the venue reported them, each absent where it reported none.
            // Nothing here computes one from the rows: that would be our figure
            // presented as the custodian's.
            figures: figures_from_wire(request)?,
            currency_assumed: request.currency_assumed,
            security_interest: request.security_interest,
            completed: None,
        },
        cause,
    )?;

    Ok(Opening {
        reply: RecordHoldingsStatementReply {
            statement_id: statement.statement_id.clone(),
            already_recorded: matches!(opened, Opened::AlreadyRecorded),
        },
        completed: match completion {
            Completion::Nothing => None,
            Completion::JustCompleted(_) => Some(statement_recorded(&statement, Counts::default())),
        },
    })
}

/// A statement's figures, one set per segment: from a plugin before v7 the
/// three it sent flat, read as the set with no segment; from one at v7 its
/// sets, refused beside the flat three, or naming a segment twice, or with a
/// collateral balance neither posted nor received or naming both or neither
/// of an instrument and identifiers (W2.2). The sidecar refused each of
/// these already from a plugin; this is every other sender.
fn figures_from_wire(request: &RecordHoldingsStatementRequest) -> Result<Vec<Figures>> {
    let flat = [
        ("buying_power", &request.buying_power),
        ("margin_requirement", &request.margin_requirement),
        ("maintenance_excess", &request.maintenance_excess),
    ];
    if request.figures.is_empty() {
        if flat.iter().all(|(_, figure)| figure.is_none()) {
            return Ok(Vec::new());
        }
        return Ok(vec![Figures {
            segment: String::new(),
            buying_power: Money::reported("buying_power", request.buying_power.as_ref())?,
            margin_requirement: Money::reported(
                "margin_requirement",
                request.margin_requirement.as_ref(),
            )?,
            maintenance_excess: Money::reported(
                "maintenance_excess",
                request.maintenance_excess.as_ref(),
            )?,
            ..Default::default()
        }]);
    }
    if let Some((name, _)) = flat.iter().find(|(_, figure)| figure.is_some()) {
        return Err(StoreError::Figures(format!(
            "{name} is read from a plugin before v7; send it in figures"
        )));
    }
    let mut named = BTreeSet::new();
    request
        .figures
        .iter()
        .enumerate()
        .map(|(i, figures)| {
            if !named.insert(figures.segment.clone()) {
                return Err(StoreError::Figures(format!(
                    "figures[{i}].segment \"{}\" is named twice; a statement has one set per \
                     segment",
                    figures.segment
                )));
            }
            Ok(Figures {
                segment: figures.segment.clone(),
                buying_power: Money::reported(
                    "figures.buying_power",
                    figures.buying_power.as_ref(),
                )?,
                margin_requirement: Money::reported(
                    "figures.margin_requirement",
                    figures.margin_requirement.as_ref(),
                )?,
                maintenance_excess: Money::reported(
                    "figures.maintenance_excess",
                    figures.maintenance_excess.as_ref(),
                )?,
                initial_margin: Money::reported(
                    "figures.initial_margin",
                    figures.initial_margin.as_ref(),
                )?,
                variation_margin: Money::reported(
                    "figures.variation_margin",
                    figures.variation_margin.as_ref(),
                )?,
                net_liquidation: Money::reported(
                    "figures.net_liquidation",
                    figures.net_liquidation.as_ref(),
                )?,
                collateral: figures
                    .collateral
                    .iter()
                    .enumerate()
                    .map(|(j, balance)| collateral_from_wire(i, j, balance))
                    .collect::<Result<_>>()?,
            })
        })
        .collect()
}

fn collateral_from_wire(i: usize, j: usize, balance: &ReportedCollateral) -> Result<Collateral> {
    let direction = match CollateralDirection::try_from(balance.direction) {
        Ok(CollateralDirection::Posted) => Direction::Posted,
        Ok(CollateralDirection::Received) => Direction::Received,
        Ok(CollateralDirection::Unspecified) | Err(_) => {
            return Err(StoreError::Figures(format!(
                "figures[{i}].collateral[{j}].direction is unspecified; collateral is posted or \
                 received"
            )))
        }
    };
    let instrument_id = Some(balance.instrument_id.clone()).filter(|id| !id.is_empty());
    if instrument_id.is_some() != balance.unresolved_identifiers.is_empty() {
        return Err(StoreError::Figures(format!(
            "figures[{i}].collateral[{j}] names {}: exactly one of an instrument or the \
             identifiers that did not resolve",
            if instrument_id.is_some() {
                "both"
            } else {
                "neither"
            }
        )));
    }
    Ok(Collateral {
        direction,
        instrument_id,
        unresolved_identifiers: balance
            .unresolved_identifiers
            .iter()
            .map(from_wire_identifier)
            .collect(),
        quantity: Quantity::from_wire("collateral.quantity", balance.quantity.as_ref())?,
        value: Money::reported("collateral.value", balance.value.as_ref())?,
        haircut: Quantity::reported("collateral.haircut", balance.haircut.as_ref())?,
        value_after_haircut: Money::reported(
            "collateral.value_after_haircut",
            balance.value_after_haircut.as_ref(),
        )?,
        held_at: balance.held_at.clone(),
        reusable: balance.reusable,
    })
}

/// What recording a row did, and what to announce about it.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub reply: RecordHoldingReply,

    /// Present only when a position moved. An unresolved row publishes nothing
    /// here, because it updates no position, and neither does a row that says
    /// exactly what the position already held.
    pub event: Option<CustodialPositionUpdatedEvent>,

    /// W2.5. Present on the row that completes the statement, and on no other.
    ///
    /// A statement whose rows never all arrive produces this never, which is
    /// the intended behaviour: counts published early would be wrong, and wrong
    /// quietly, and the unresolved figure is the one an operator watches.
    pub completed: Option<StatementRecordedEvent>,
}

/// W2.3 and W2.4. Persist a row, and settle the position behind it.
pub fn record_holding(
    store: &dyn Store,
    request: &RecordHoldingRequest,
    cause: &Cause,
) -> Result<Recorded> {
    let holding_id = ids::holding(cause.committed_at_ns);

    let holding = Holding {
        holding_id: holding_id.clone(),
        statement_id: request.statement_id.clone(),
        account_id: request.account_id.clone(),

        // Empty is absent. The store refuses a row that names both or neither,
        // so a request carrying an instrument and identifiers fails rather than
        // being quietly reduced to one of them.
        instrument_id: Some(request.instrument_id.clone()).filter(|id| !id.is_empty()),
        unresolved_identifiers: request
            .unresolved_identifiers
            .iter()
            .map(from_wire_identifier)
            .collect(),

        side: side_from_wire(request.side)?,

        // Refused naming the field when the wire form does not allow it. The
        // sidecar refused a plugin's already; this is every other sender. The
        // trade-date quantity is required: unset would read as zero, and a
        // holding of nothing is a thing a venue can say.
        quantity: Quantity::from_wire(
            "quantity",
            Some(request.quantity.as_ref().ok_or(StoreError::NoQuantity)?),
        )?,
        settle_date_quantity: Quantity::reported(
            "settle_date_quantity",
            request.settle_date_quantity.as_ref(),
        )?,
        market_value: Money::reported("market_value", request.market_value.as_ref())?,
        currency_assumed: request.currency_assumed,
        also_counted_in_cash: request.also_counted_in_cash,

        // As the venue reported them, each absent where it did not, and
        // neither cost computed from the other or from the quantity (Q-A).
        // Lots not summing to the holding are kept as reported (resolved
        // point 6); none is not one lot.
        cost: Cost {
            cost_basis: Money::reported("cost_basis", request.cost_basis.as_ref())?,
            average_cost: Money::reported("average_cost", request.average_cost.as_ref())?,
            lots: request
                .lots
                .iter()
                .map(lot_from_wire)
                .collect::<Result<_>>()?,
            margin_requirement: Money::reported(
                "margin_requirement",
                request.margin_requirement.as_ref(),
            )?,
            // As reported, and available plus not available, or a sub-balance,
            // not fitting the holding is kept as reported: the custodian's
            // data, which the reconciliation flags (W9.3, Q5).
            available_quantity: Quantity::reported(
                "available_quantity",
                request.available_quantity.as_ref(),
            )?,
            not_available_quantity: Quantity::reported(
                "not_available_quantity",
                request.not_available_quantity.as_ref(),
            )?,
            available_basis: request.available_basis,
            encumbrances: request
                .encumbrances
                .iter()
                .enumerate()
                .map(|(at, held)| encumbrance_from_wire(at, held))
                .collect::<Result<_>>()?,
        },

        // Nothing has asked the platform about these identifiers yet. W3.2 is
        // the connector's obligation and it happens before this.
        escalated: false,
    };

    let resolved = holding.resolved();
    let (settled, completion) = store.record(holding, cause)?;

    let completed = match completion {
        Completion::Nothing => None,
        Completion::JustCompleted(_) => {
            let statement = store
                .statement(&request.statement_id)?
                .ok_or_else(|| StoreError::UnknownStatement(request.statement_id.clone()))?;
            let counts = store.counts(&request.statement_id)?;
            Some(statement_recorded(&statement, counts))
        }
    };

    Ok(Recorded {
        reply: RecordHoldingReply {
            holding_id,
            resolved,
        },
        event: match settled {
            Settled::Changed {
                position,
                previous_quantity,
            } => Some(position_updated(
                &position,
                request.statement_id.clone(),
                previous_quantity,
                cause,
            )),
            Settled::Unchanged { .. } | Settled::Unresolved => None,
        },
        completed,
    })
}

fn lot_from_wire(lot: &ReportedLot) -> Result<Lot> {
    Ok(Lot {
        // Signed as the holding's; required, as a holding's quantity is.
        quantity: Quantity::from_wire("lots.quantity", lot.quantity.as_ref())?,
        // Its sign as reported, never flipped (Q-D).
        cost: Money::reported("lots.cost", lot.cost.as_ref())?,
        acquired_date: lot.acquired_date.clone(),
    })
}

/// A sub-balance as reported; refused for no kind, or OTHER with no code.
fn encumbrance_from_wire(at: usize, held: &ReportedEncumbrance) -> Result<Encumbrance> {
    use meridian_domain::v1::EncumbranceKind;
    match EncumbranceKind::try_from(held.kind) {
        Ok(EncumbranceKind::Unspecified) | Err(_) => {
            return Err(StoreError::Encumbrance(format!(
                "encumbrances[{at}].kind is unspecified; a sub-balance says which it is"
            )))
        }
        Ok(EncumbranceKind::Other) if held.source_code.is_empty() => {
            return Err(StoreError::Encumbrance(format!(
                "encumbrances[{at}].source_code is required for OTHER: the source's own code, \
                 verbatim"
            )))
        }
        Ok(_) => {}
    }
    Ok(Encumbrance {
        kind: held.kind,
        quantity: Quantity::from_wire("encumbrances.quantity", held.quantity.as_ref())?,
        available: held.available,
        source_code: held.source_code.clone(),
        pledgee: held.pledgee.clone(),
        held_at: held.held_at.clone(),
        segment: held.segment.clone(),
        detail: held.detail.clone(),
    })
}

pub(crate) fn encumbrance_to_wire(held: &Encumbrance) -> ReportedEncumbrance {
    ReportedEncumbrance {
        kind: held.kind,
        quantity: held.quantity.to_wire(),
        available: held.available,
        source_code: held.source_code.clone(),
        pledgee: held.pledgee.clone(),
        held_at: held.held_at.clone(),
        segment: held.segment.clone(),
        detail: held.detail.clone(),
    }
}

/// W3.9. Move what a replaced placeholder held onto the instrument that
/// replaced it, and produce the W2.6 events for what now stands under it and
/// for each placeholder position it removed, a tombstone.
///
/// An event with no placeholder, no instrument, or an instrument naming the
/// placeholder itself moves nothing: moving a position onto its own key would
/// only delete it.
///
/// Each event names the statement that stated the position, which is still
/// what the custodian said; the move changed its name and not its content.
pub fn move_positions(
    store: &dyn Store,
    event: &InstrumentReplacedEvent,
    cause: &Cause,
) -> Result<Vec<CustodialPositionUpdatedEvent>> {
    let replaced_id = event.replaced_instrument_id.as_str();
    let instrument_id = event
        .instrument
        .as_ref()
        .map(|instrument| instrument.instrument_id.as_str())
        .unwrap_or_default();

    if replaced_id.is_empty() || instrument_id.is_empty() || replaced_id == instrument_id {
        return Ok(Vec::new());
    }

    Ok(store
        .move_positions(replaced_id, instrument_id, cause)?
        .into_iter()
        .filter_map(|settled| match settled {
            Settled::Changed {
                position,
                previous_quantity,
            } => Some(position_updated(
                &position,
                position.last_statement_id.clone(),
                previous_quantity,
                cause,
            )),
            Settled::Unchanged { .. } | Settled::Unresolved => None,
        })
        .collect())
}

/// W2.6: a position's change, as announced.
fn position_updated(
    position: &CustodialPosition,
    statement_id: String,
    previous_quantity: Quantity,
    cause: &Cause,
) -> CustodialPositionUpdatedEvent {
    CustodialPositionUpdatedEvent {
        position: Some(to_wire_position(position)),
        statement_id,
        previous_quantity: previous_quantity.to_wire(),
        journal: Some(to_wire_journal(position.last_change)),
        cause: Some(to_wire_cause(cause)),
    }
}

/// W2.5: a completed statement, as announced and as read (W2.9).
pub(crate) fn statement_recorded(statement: &Statement, counts: Counts) -> StatementRecordedEvent {
    let completed = statement.completed.clone().unwrap_or_default();
    StatementRecordedEvent {
        statement_id: statement.statement_id.clone(),
        source: statement.source.clone(),
        as_of_date: statement.as_of_date.clone(),
        rows_received: counts.received as i32,
        rows_resolved: counts.resolved as i32,
        rows_unresolved: counts.unresolved as i32,
        recorded_at_ns: completed.cause.committed_at_ns,
        account_id: statement.account_id.clone(),
        figures: statement.figures.iter().map(figures_to_wire).collect(),
        currency_assumed: statement.currency_assumed,
        journal: Some(to_wire_journal(completed.change)),
        cause: Some(to_wire_cause(&completed.cause)),
        external_account_id: statement.external_account_id.clone(),
        institution: statement.institution.clone(),
        security_interest: statement.security_interest,
    }
}

fn figures_to_wire(figures: &Figures) -> StatementFigures {
    StatementFigures {
        segment: figures.segment.clone(),
        buying_power: figures.buying_power.as_ref().and_then(Money::to_wire),
        margin_requirement: figures.margin_requirement.as_ref().and_then(Money::to_wire),
        maintenance_excess: figures.maintenance_excess.as_ref().and_then(Money::to_wire),
        initial_margin: figures.initial_margin.as_ref().and_then(Money::to_wire),
        variation_margin: figures.variation_margin.as_ref().and_then(Money::to_wire),
        net_liquidation: figures.net_liquidation.as_ref().and_then(Money::to_wire),
        collateral: figures
            .collateral
            .iter()
            .map(|balance| ReportedCollateral {
                direction: match balance.direction {
                    Direction::Posted => CollateralDirection::Posted as i32,
                    Direction::Received => CollateralDirection::Received as i32,
                },
                instrument_id: balance.instrument_id.clone().unwrap_or_default(),
                unresolved_identifiers: balance
                    .unresolved_identifiers
                    .iter()
                    .map(to_wire_identifier)
                    .collect(),
                quantity: balance.quantity.to_wire(),
                value: balance.value.as_ref().and_then(Money::to_wire),
                haircut: balance.haircut.and_then(Quantity::to_wire),
                value_after_haircut: balance
                    .value_after_haircut
                    .as_ref()
                    .and_then(Money::to_wire),
                held_at: balance.held_at.clone(),
                reusable: balance.reusable,
            })
            .collect(),
    }
}

pub(crate) fn to_wire_journal(change: Change) -> JournalRef {
    JournalRef {
        partition: PARTITION.to_string(),
        sequence: change.sequence,
        previous_sequence: change.previous,
    }
}

fn to_wire_cause(cause: &Cause) -> ChangeCause {
    ChangeCause {
        instance_id: cause.instance_id.clone(),
        acting_for_subject: cause.acting_for_subject.clone(),
        correlation_id: cause.correlation_id.clone(),
        causation_id: cause.causation_id.clone(),
        committed_at_ns: cause.committed_at_ns,
    }
}

/// A side as the wire states it, or the refusal for a row that states none.
fn side_from_wire(side: i32) -> Result<Side> {
    match HoldingSide::try_from(side) {
        Ok(HoldingSide::Long) => Ok(Side::Long),
        Ok(HoldingSide::Short) => Ok(Side::Short),
        Ok(HoldingSide::Unspecified) | Err(_) => Err(StoreError::NoSide),
    }
}

pub(crate) fn side_to_wire(side: Side) -> i32 {
    match side {
        Side::Long => HoldingSide::Long as i32,
        Side::Short => HoldingSide::Short as i32,
    }
}

pub(crate) fn from_wire_identifier(identifier: &PbIdentifier) -> Identifier {
    Identifier {
        scheme: identifier.scheme.clone(),
        value: identifier.value.clone(),
        source: identifier.source.clone(),
    }
}

pub(crate) fn to_wire_identifier(identifier: &Identifier) -> PbIdentifier {
    PbIdentifier {
        scheme: identifier.scheme.clone(),
        value: identifier.value.clone(),
        source: identifier.source.clone(),
    }
}

pub(crate) fn to_wire_position(
    position: &CustodialPosition,
) -> meridian_domain::v1::CustodialPosition {
    meridian_domain::v1::CustodialPosition {
        account_id: position.account_id.clone(),
        instrument_id: position.instrument_id.clone(),
        side: side_to_wire(position.side),
        quantity: position.quantity.to_wire(),
        settle_date_quantity: position
            .settle_date_quantity
            .and_then(|quantity| quantity.to_wire()),
        market_value: position.market_value.as_ref().and_then(Money::to_wire),
        also_counted_in_cash: position.also_counted_in_cash,
        last_statement_id: position.last_statement_id.clone(),
        as_of_date: position.as_of_date.clone(),
        updated_at_ns: position.updated_at_ns,
        cost_basis: position.cost.cost_basis.as_ref().and_then(Money::to_wire),
        lots: position
            .cost
            .lots
            .iter()
            .map(|lot| ReportedLot {
                quantity: lot.quantity.to_wire(),
                cost: lot.cost.as_ref().and_then(Money::to_wire),
                acquired_date: lot.acquired_date.clone(),
            })
            .collect(),
        margin_requirement: position
            .cost
            .margin_requirement
            .as_ref()
            .and_then(Money::to_wire),
        last_change: Some(to_wire_journal(position.last_change)),
        removed: position.removed,
        average_cost: position.cost.average_cost.as_ref().and_then(Money::to_wire),
        available_quantity: position
            .cost
            .available_quantity
            .and_then(|quantity| quantity.to_wire()),
        not_available_quantity: position
            .cost
            .not_available_quantity
            .and_then(|quantity| quantity.to_wire()),
        available_basis: position.cost.available_basis,
        encumbrances: position
            .cost
            .encumbrances
            .iter()
            .map(encumbrance_to_wire)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amounts::testing::{quantity, read, read_money, usd};
    use crate::store::{Counts, StoreError};
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    /// A change made at `now` by the fixture's custody plugin.
    fn at(now: i64) -> Cause {
        Cause {
            instance_id: "custody-snaptrade-1".into(),
            correlation_id: "corr-1".into(),
            causation_id: "msg-1".into(),
            committed_at_ns: now,
            ..Default::default()
        }
    }

    /// The fixture's statement.
    fn statement_request() -> RecordHoldingsStatementRequest {
        RecordHoldingsStatementRequest {
            source: "snaptrade".into(),
            external_statement_id: "SNAP-ACC-1/1757376000000000000".into(),
            as_of_date: "2026-09-08".into(),
            read_at_ns: NOW,
            expected_rows: 4,
            buying_power: usd("25000.00"),
            ..Default::default()
        }
    }

    /// The fixture's resolved row: 12.5 shares worth 2812.50, long.
    fn holding_request(statement_id: &str) -> RecordHoldingRequest {
        RecordHoldingRequest {
            statement_id: statement_id.into(),
            account_id: "SNAP-ACC-1".into(),
            instrument_id: "INS-01J8XQ4M7K0000000000AAPL".into(),
            quantity: quantity("12.5"),
            market_value: usd("2812.5"),
            side: HoldingSide::Long as i32,
            ..Default::default()
        }
    }

    /// The fixture's named case: the instrument did not resolve.
    fn unresolved_request(statement_id: &str) -> RecordHoldingRequest {
        RecordHoldingRequest {
            statement_id: statement_id.into(),
            account_id: "SNAP-ACC-1".into(),
            instrument_id: String::new(),
            unresolved_identifiers: vec![PbIdentifier {
                scheme: "symbol".into(),
                value: "ZZTOP".into(),
                source: "snaptrade".into(),
            }],
            quantity: quantity("5"),
            market_value: usd("0"),
            side: HoldingSide::Long as i32,
            ..Default::default()
        }
    }

    fn opened(store: &MemoryStore) -> String {
        open_statement(store, &statement_request(), &at(NOW))
            .unwrap()
            .reply
            .statement_id
    }

    #[test]
    fn opening_a_statement_mints_one_and_says_it_is_new() {
        let store = MemoryStore::new();
        let reply = open_statement(&store, &statement_request(), &at(NOW))
            .unwrap()
            .reply;

        assert!(reply.statement_id.starts_with("STMT-"));
        assert!(!reply.already_recorded);
    }

    #[test]
    fn a_redelivered_statement_is_a_no_op_and_not_a_duplicate() {
        // The fixture's named case. A connector that cannot tell whether its
        // last attempt landed sends it again, and nothing is doubled.
        let store = MemoryStore::new();
        let first = open_statement(&store, &statement_request(), &at(NOW))
            .unwrap()
            .reply;
        let again = open_statement(&store, &statement_request(), &at(NOW + 1))
            .unwrap()
            .reply;

        assert_eq!(again.statement_id, first.statement_id);
        assert!(again.already_recorded);
    }

    #[test]
    fn the_same_external_identifier_from_another_source_is_another_statement() {
        // A rail's identifiers are its own. Two rails may number theirs alike.
        let store = MemoryStore::new();
        let first = open_statement(&store, &statement_request(), &at(NOW))
            .unwrap()
            .reply;

        let mut elsewhere = statement_request();
        elsewhere.source = "another-rail".into();
        let second = open_statement(&store, &elsewhere, &at(NOW)).unwrap().reply;

        assert_ne!(second.statement_id, first.statement_id);
        assert!(!second.already_recorded);
    }

    #[test]
    fn a_resolved_row_is_recorded_and_moves_a_position() {
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let recorded = record_holding(&store, &holding_request(&statement_id), &at(NOW)).unwrap();

        assert!(recorded.reply.resolved);
        assert!(recorded.reply.holding_id.starts_with("HLD-"));

        let event = recorded.event.expect("a new position is a change");
        let position = event.position.unwrap();
        assert_eq!(read(&position.quantity), "12.5");
        assert_eq!(read_money(&position.market_value), "2812.5 USD");
        assert_eq!(position.as_of_date, "2026-09-08");
        assert_eq!(position.last_statement_id, statement_id);
        assert_eq!(read(&event.previous_quantity), "0");
    }

    #[test]
    fn an_unresolved_row_is_recorded_and_moves_nothing() {
        // The fixture's postcondition, twice over: recorded rather than
        // dropped, and producing no position event because no position can
        // exist until the deployment knows what it holds.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let recorded =
            record_holding(&store, &unresolved_request(&statement_id), &at(NOW)).unwrap();

        assert!(!recorded.reply.resolved);
        assert!(recorded.event.is_none());
        assert_eq!(
            store.counts(&statement_id).unwrap(),
            Counts {
                received: 1,
                resolved: 0,
                unresolved: 1
            }
        );
    }

    #[test]
    fn a_row_naming_both_an_instrument_and_identifiers_is_refused() {
        // "Exactly one of instrument_id or unresolved_identifiers. Never both,
        // never neither" is the fixture's own header.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut both = holding_request(&statement_id);
        both.unresolved_identifiers = vec![PbIdentifier {
            scheme: "symbol".into(),
            value: "AAPL".into(),
            source: "snaptrade".into(),
        }];

        assert!(matches!(
            record_holding(&store, &both, &at(NOW)),
            Err(StoreError::BothResolvedAndNot)
        ));
    }

    #[test]
    fn a_row_naming_neither_is_refused() {
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut neither = holding_request(&statement_id);
        neither.instrument_id = String::new();
        neither.unresolved_identifiers = vec![];

        assert!(matches!(
            record_holding(&store, &neither, &at(NOW)),
            Err(StoreError::NeitherResolvedNorIdentified)
        ));
    }

    #[test]
    fn a_row_for_a_statement_nobody_opened_is_refused() {
        let store = MemoryStore::new();
        assert!(matches!(
            record_holding(&store, &holding_request("STMT-nobody-opened"), &at(NOW)),
            Err(StoreError::UnknownStatement(_))
        ));
    }

    #[test]
    fn a_position_is_replaced_by_the_latest_statement_and_never_accumulated() {
        // The rule that is invisible when it is wrong. A row states a quantity
        // as of a date; it is not a change to one. Adding them would double
        // anything that appeared in two reads, and 25 shares looks as plausible
        // as 12.5.
        let store = MemoryStore::new();

        let first = opened(&store);
        record_holding(&store, &holding_request(&first), &at(NOW)).unwrap();

        let mut later = statement_request();
        later.external_statement_id = "SNAP-ACC-1/1757462400000000000".into();
        later.as_of_date = "2026-09-09".into();
        let second = open_statement(&store, &later, &at(NOW + 1))
            .unwrap()
            .reply
            .statement_id;
        record_holding(&store, &holding_request(&second), &at(NOW + 1)).unwrap();

        let position = store
            .custodial_position("SNAP-ACC-1", "INS-01J8XQ4M7K0000000000AAPL", Side::Long)
            .unwrap()
            .unwrap();

        assert_eq!(position.quantity.to_string(), "12.5");
        assert_eq!(position.as_of_date, "2026-09-09");
        assert_eq!(position.last_statement_id, second);
    }

    #[test]
    fn the_event_carries_what_the_position_was_before() {
        // So a subscriber renders a change without keeping its own history.
        let store = MemoryStore::new();

        let first = opened(&store);
        record_holding(&store, &holding_request(&first), &at(NOW)).unwrap();

        let mut later = statement_request();
        later.external_statement_id = "SNAP-ACC-1/1757462400000000000".into();
        let second = open_statement(&store, &later, &at(NOW + 1))
            .unwrap()
            .reply
            .statement_id;

        let mut grown = holding_request(&second);
        grown.quantity = quantity("20");
        let recorded = record_holding(&store, &grown, &at(NOW + 1)).unwrap();

        let event = recorded.event.unwrap();
        assert_eq!(read(&event.previous_quantity), "12.5");
        assert_eq!(read(&event.position.unwrap().quantity), "20");
    }

    #[test]
    fn a_row_that_changes_nothing_announces_nothing() {
        let store = MemoryStore::new();

        let first = opened(&store);
        record_holding(&store, &holding_request(&first), &at(NOW)).unwrap();

        let mut again = statement_request();
        again.external_statement_id = "SNAP-ACC-1/1757462400000000000".into();
        let second = open_statement(&store, &again, &at(NOW + 1))
            .unwrap()
            .reply
            .statement_id;
        let unchanged = record_holding(&store, &holding_request(&second), &at(NOW + 1)).unwrap();

        assert!(unchanged.event.is_none());
        assert!(unchanged.reply.resolved);
    }

    #[test]
    fn a_short_position_is_a_position() {
        // "A negative quantity is a short position, not an error", and its
        // side says so.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut short = holding_request(&statement_id);
        short.quantity = quantity("-5");
        short.side = HoldingSide::Short as i32;
        let recorded = record_holding(&store, &short, &at(NOW)).unwrap();

        assert!(recorded.reply.resolved);
        let position = recorded.event.unwrap().position.unwrap();
        assert_eq!(read(&position.quantity), "-5");
        assert_eq!(position.side, HoldingSide::Short as i32);
    }

    #[test]
    fn a_holding_that_says_no_side_is_refused() {
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut sideless = holding_request(&statement_id);
        sideless.side = HoldingSide::Unspecified as i32;

        assert!(matches!(
            record_holding(&store, &sideless, &at(NOW)),
            Err(StoreError::NoSide)
        ));
    }

    #[test]
    fn a_side_its_quantity_contradicts_is_refused_naming_both() {
        // Neither is believed over the other: the connector got one of them
        // wrong, and nothing here can tell which.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut contradicted = holding_request(&statement_id);
        contradicted.side = HoldingSide::Short as i32;
        let refused = record_holding(&store, &contradicted, &at(NOW)).unwrap_err();
        assert!(matches!(refused, StoreError::SideContradictsSign { .. }));
        assert!(
            refused
                .to_string()
                .contains("short side states a quantity of 12.5"),
            "{refused}"
        );

        let mut negative_long = holding_request(&statement_id);
        negative_long.quantity = quantity("-1");
        assert!(matches!(
            record_holding(&store, &negative_long, &at(NOW)),
            Err(StoreError::SideContradictsSign { .. })
        ));
    }

    #[test]
    fn a_holding_that_states_no_quantity_is_refused_rather_than_read_as_zero() {
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut unstated = holding_request(&statement_id);
        unstated.quantity = None;

        assert!(matches!(
            record_holding(&store, &unstated, &at(NOW)),
            Err(StoreError::NoQuantity)
        ));
        assert_eq!(store.counts(&statement_id).unwrap().received, 0);
    }

    #[test]
    fn a_venue_reporting_long_and_short_of_one_instrument_holds_two_positions() {
        // Schwab's shape: separate long and short figures, two rows, never
        // netted to one.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut long = holding_request(&statement_id);
        long.quantity = quantity("200");
        let mut short = holding_request(&statement_id);
        short.side = HoldingSide::Short as i32;
        short.quantity = quantity("-50");
        record_holding(&store, &long, &at(NOW)).unwrap();
        record_holding(&store, &short, &at(NOW)).unwrap();

        let instrument = "INS-01J8XQ4M7K0000000000AAPL";
        let held_long = store
            .custodial_position("SNAP-ACC-1", instrument, Side::Long)
            .unwrap()
            .unwrap();
        let held_short = store
            .custodial_position("SNAP-ACC-1", instrument, Side::Short)
            .unwrap()
            .unwrap();
        assert_eq!(held_long.quantity.to_string(), "200");
        assert_eq!(held_short.quantity.to_string(), "-50");
    }

    #[test]
    fn a_kalshi_no_is_a_short_row_of_the_markets_one_contract() {
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut no = holding_request(&statement_id);
        no.instrument_id = "INS-01J8XQ4M7K00000000KXRAIN".into();
        no.side = HoldingSide::Short as i32;
        no.quantity = quantity("-15.25");
        no.market_value = None;
        let recorded = record_holding(&store, &no, &at(NOW)).unwrap();

        let position = recorded.event.unwrap().position.unwrap();
        assert_eq!(read(&position.quantity), "-15.25");
        assert_eq!(position.side, HoldingSide::Short as i32);
        assert!(position.market_value.is_none(), "Kalshi reports none");
    }

    #[test]
    fn what_the_venue_did_not_report_is_absent_and_never_zero() {
        // SnapTrade reports no market value; the row and the position say so.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut unvalued = holding_request(&statement_id);
        unvalued.market_value = None;
        let recorded = record_holding(&store, &unvalued, &at(NOW)).unwrap();

        let position = recorded.event.unwrap().position.unwrap();
        assert!(position.market_value.is_none());
        assert!(position.settle_date_quantity.is_none());
        let held = store
            .custodial_position("SNAP-ACC-1", "INS-01J8XQ4M7K0000000000AAPL", Side::Long)
            .unwrap()
            .unwrap();
        assert_eq!(held.market_value, None);
    }

    #[test]
    fn cash_is_a_holding_with_its_settled_part_and_an_assumed_currency_kept() {
        // E*TRADE's shape: one cash figure and no currency, so the row names
        // the USD cash instrument on the connector's stated assumption.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut cash = holding_request(&statement_id);
        cash.instrument_id = "INS-01J8XQ4M7K00000000CASHUSD".into();
        cash.quantity = quantity("1520.35");
        cash.settle_date_quantity = quantity("1020.35");
        cash.market_value = None;
        cash.currency_assumed = true;
        record_holding(&store, &cash, &at(NOW)).unwrap();

        let held = store
            .custodial_position("SNAP-ACC-1", "INS-01J8XQ4M7K00000000CASHUSD", Side::Long)
            .unwrap()
            .unwrap();
        assert_eq!(held.quantity.to_string(), "1520.35");
        assert_eq!(held.settle_date_quantity.unwrap().to_string(), "1020.35");
    }

    #[test]
    fn a_fund_also_counted_in_cash_is_kept_as_reported_and_marked() {
        // SnapTrade counts a money-market fund in cash and lists it as a
        // position too. Both rows stand as reported; the position says so.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut cash = holding_request(&statement_id);
        cash.instrument_id = "INS-01J8XQ4M7K00000000CASHUSD".into();
        cash.quantity = quantity("1520.35");
        cash.market_value = None;
        record_holding(&store, &cash, &at(NOW)).unwrap();

        let mut fund = holding_request(&statement_id);
        fund.instrument_id = "INS-01J8XQ4M7K0000000000SPAXX".into();
        fund.quantity = quantity("500");
        fund.market_value = usd("500.00");
        fund.also_counted_in_cash = true;
        let recorded = record_holding(&store, &fund, &at(NOW)).unwrap();

        assert!(
            recorded
                .event
                .unwrap()
                .position
                .unwrap()
                .also_counted_in_cash
        );
        let held_cash = store
            .custodial_position("SNAP-ACC-1", "INS-01J8XQ4M7K00000000CASHUSD", Side::Long)
            .unwrap()
            .unwrap();
        assert_eq!(
            held_cash.quantity.to_string(),
            "1520.35",
            "cash is not reduced here"
        );
        assert!(!held_cash.also_counted_in_cash);
    }

    #[test]
    fn a_trade_settling_is_a_change_though_the_trade_date_quantity_is_not() {
        let store = MemoryStore::new();
        let first = opened(&store);
        let mut unsettled = holding_request(&first);
        unsettled.settle_date_quantity = quantity("10");
        record_holding(&store, &unsettled, &at(NOW)).unwrap();

        let mut later = statement_request();
        later.external_statement_id = "SNAP-ACC-1/1757462400000000000".into();
        let second = open_statement(&store, &later, &at(NOW + 1))
            .unwrap()
            .reply
            .statement_id;
        let mut settled = holding_request(&second);
        settled.settle_date_quantity = quantity("12.5");
        let recorded = record_holding(&store, &settled, &at(NOW + 1)).unwrap();

        let event = recorded.event.expect("settling moved the settled quantity");
        assert_eq!(read(&event.previous_quantity), "12.5");
        assert_eq!(read(&event.position.unwrap().settle_date_quantity), "12.5");
    }

    #[test]
    fn a_statements_figures_are_kept_as_the_venue_reported_them() {
        let store = MemoryStore::new();
        let mut etrade = statement_request();
        etrade.buying_power = usd("41250.00");
        etrade.margin_requirement = usd("18250.00");
        etrade.maintenance_excess = None;
        etrade.currency_assumed = true;
        let statement_id = open_statement(&store, &etrade, &at(NOW))
            .unwrap()
            .reply
            .statement_id;

        // From a plugin before v7, sent flat: read as the set with no segment.
        let statement = store.statement(&statement_id).unwrap().unwrap();
        let [figures] = statement.figures.as_slice() else {
            panic!("one set, the account's as a whole: {:?}", statement.figures)
        };
        assert_eq!(figures.segment, "");
        assert_eq!(
            figures.buying_power.as_ref().unwrap().to_string(),
            "41250.00 USD"
        );
        assert_eq!(
            figures.margin_requirement.as_ref().unwrap().to_string(),
            "18250.00 USD"
        );
        assert_eq!(figures.maintenance_excess, None, "not reported, not zero");
        assert!(statement.currency_assumed);
    }

    #[test]
    fn a_statement_completes_on_the_row_that_reaches_its_count() {
        // W2.5. The fixture's statement says four rows follow.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        for n in 0..3 {
            let mut row = holding_request(&statement_id);
            row.instrument_id = format!("INS-{n}");
            let early = record_holding(&store, &row, &at(NOW)).unwrap();
            assert!(early.completed.is_none(), "announced after {} rows", n + 1);
        }

        let mut last = holding_request(&statement_id);
        last.instrument_id = String::new();
        last.unresolved_identifiers = vec![PbIdentifier {
            scheme: "symbol".into(),
            value: "ZZTOP".into(),
            source: "snaptrade".into(),
        }];
        let fourth = record_holding(&store, &last, &at(NOW)).unwrap();

        let event = fourth.completed.expect("the fourth row completes it");
        assert_eq!(event.statement_id, statement_id);
        assert_eq!(event.source, "snaptrade");
        assert_eq!(event.as_of_date, "2026-09-08");
        assert_eq!(event.rows_received, 4);
        assert_eq!(event.rows_resolved, 3);
        assert_eq!(event.rows_unresolved, 1);
        assert_eq!(
            event.rows_resolved + event.rows_unresolved,
            event.rows_received
        );
    }

    #[test]
    fn a_statement_missing_a_row_is_never_announced() {
        // Intended rather than a gap. Counts published early are wrong, and
        // wrong quietly, and the unresolved figure is the one an operator
        // watches. It stays open, and open is observable.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        for n in 0..3 {
            let mut row = holding_request(&statement_id);
            row.instrument_id = format!("INS-{n}");
            assert!(record_holding(&store, &row, &at(NOW))
                .unwrap()
                .completed
                .is_none());
        }
    }

    #[test]
    fn a_row_beyond_the_count_does_not_announce_it_again() {
        // A subscriber's arithmetic should not depend on how many times it
        // heard.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        for n in 0..4 {
            let mut row = holding_request(&statement_id);
            row.instrument_id = format!("INS-{n}");
            record_holding(&store, &row, &at(NOW)).unwrap();
        }

        let mut extra = holding_request(&statement_id);
        extra.instrument_id = "INS-surplus".into();
        let beyond = record_holding(&store, &extra, &at(NOW)).unwrap();

        assert!(beyond.completed.is_none());
        assert!(beyond.reply.resolved, "the row is still recorded");
        assert_eq!(store.counts(&statement_id).unwrap().received, 5);
    }

    #[test]
    fn a_statement_promising_no_rows_is_complete_when_it_opens() {
        // An account that holds nothing today is a real answer, and a different
        // one from not having read the account. Waiting for a row that was
        // never coming would leave it open forever, which reads as a stuck
        // connector.
        let store = MemoryStore::new();

        let mut empty = statement_request();
        empty.expected_rows = 0;
        let opening = open_statement(&store, &empty, &at(NOW)).unwrap();

        let event = opening.completed.expect("nothing is outstanding");
        assert_eq!(event.rows_received, 0);
        assert_eq!(event.rows_resolved, 0);
        assert_eq!(event.rows_unresolved, 0);
        assert!(!opening.reply.already_recorded);
    }

    #[test]
    fn a_redelivered_empty_statement_does_not_announce_twice() {
        let store = MemoryStore::new();
        let mut empty = statement_request();
        empty.expected_rows = 0;

        assert!(open_statement(&store, &empty, &at(NOW))
            .unwrap()
            .completed
            .is_some());
        let again = open_statement(&store, &empty, &at(NOW + 1)).unwrap();

        assert!(again.reply.already_recorded);
        assert!(again.completed.is_none());
    }

    #[test]
    fn the_counts_always_add_up() {
        // The fixture's postcondition for W2.5, which nothing publishes yet.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        record_holding(&store, &holding_request(&statement_id), &at(NOW)).unwrap();
        record_holding(&store, &unresolved_request(&statement_id), &at(NOW)).unwrap();

        let mut other = holding_request(&statement_id);
        other.instrument_id = "INS-OTHER".into();
        record_holding(&store, &other, &at(NOW)).unwrap();

        let counts = store.counts(&statement_id).unwrap();
        assert_eq!(
            counts,
            Counts {
                received: 3,
                resolved: 2,
                unresolved: 1
            }
        );
        assert!(counts.consistent());
    }

    const PLACEHOLDER: &str = "LCL-01J8XQ4M7K0000000000ZZTP";
    const REPLACEMENT: &str = "INS-01J8XQ4M7K0000000000ZZTP";

    /// A statement as of `as_of_date`, holding `quantity` of `instrument_id`
    /// in the fixture's account.
    fn stated(store: &MemoryStore, as_of_date: &str, instrument_id: &str, held: &str) {
        let mut statement = statement_request();
        statement.external_statement_id = format!("st-{as_of_date}-{instrument_id}");
        statement.as_of_date = as_of_date.into();
        let statement_id = open_statement(store, &statement, &at(NOW))
            .unwrap()
            .reply
            .statement_id;

        let mut row = holding_request(&statement_id);
        row.instrument_id = instrument_id.into();
        row.quantity = quantity(held);
        record_holding(store, &row, &at(NOW)).unwrap();
    }

    /// The fixture's replacement.
    fn replaced() -> InstrumentReplacedEvent {
        InstrumentReplacedEvent {
            replaced_instrument_id: PLACEHOLDER.into(),
            instrument: Some(meridian_domain::v1::InstrumentRecord {
                instrument_id: REPLACEMENT.into(),
                ..Default::default()
            }),
            replaced_at_ns: NOW,
        }
    }

    #[test]
    fn a_placeholders_position_moves_onto_its_instrument() {
        let store = MemoryStore::new();
        stated(&store, "2026-09-08", PLACEHOLDER, "5");

        let events = move_positions(&store, &replaced(), &at(NOW)).unwrap();

        // What stands under the instrument, and the placeholder's removal.
        assert_eq!(events.len(), 2);
        let position = events[0].position.as_ref().unwrap();
        assert_eq!(position.instrument_id, REPLACEMENT);
        assert_eq!(read(&position.quantity), "5");
        assert_eq!(position.as_of_date, "2026-09-08");
        assert_eq!(events[0].statement_id, position.last_statement_id);
        // New under its instrument, so changed from nothing.
        assert_eq!(read(&events[0].previous_quantity), "0");
        let removed = events[1].position.as_ref().unwrap();
        assert_eq!(removed.instrument_id, PLACEHOLDER);
        assert!(removed.removed, "a tombstone");
        assert_eq!(read(&events[1].previous_quantity), "5");

        assert!(store
            .custodial_position("SNAP-ACC-1", PLACEHOLDER, Side::Long)
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .custodial_position("SNAP-ACC-1", REPLACEMENT, Side::Long)
                .unwrap()
                .unwrap()
                .quantity
                .to_string(),
            "5"
        );
    }

    #[test]
    fn where_both_are_held_a_later_placeholder_statement_stands() {
        let store = MemoryStore::new();
        stated(&store, "2026-09-08", REPLACEMENT, "2");
        stated(&store, "2026-09-09", PLACEHOLDER, "5");

        let events = move_positions(&store, &replaced(), &at(NOW)).unwrap();

        assert_eq!(events.len(), 2, "the instrument's change and the removal");
        assert_eq!(read(&events[0].previous_quantity), "2");
        let standing = store
            .custodial_position("SNAP-ACC-1", REPLACEMENT, Side::Long)
            .unwrap()
            .unwrap();
        assert_eq!(standing.quantity.to_string(), "5");
        assert_eq!(standing.as_of_date, "2026-09-09");
        assert!(store
            .custodial_position("SNAP-ACC-1", PLACEHOLDER, Side::Long)
            .unwrap()
            .is_none());
    }

    #[test]
    fn where_both_are_held_a_later_instrument_statement_stands() {
        // The connector's next statement already resolved to the INS- ID
        // before the replacement was heard. What it said is newer, so it
        // stands, and nothing under the instrument changed to announce.
        let store = MemoryStore::new();
        stated(&store, "2026-09-08", PLACEHOLDER, "5");
        stated(&store, "2026-09-09", REPLACEMENT, "2");

        let events = move_positions(&store, &replaced(), &at(NOW)).unwrap();

        // Nothing under the instrument changed; the placeholder's is removed.
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].position.as_ref().unwrap().removed);
        let standing = store
            .custodial_position("SNAP-ACC-1", REPLACEMENT, Side::Long)
            .unwrap()
            .unwrap();
        assert_eq!(standing.quantity.to_string(), "2");
        assert!(store
            .custodial_position("SNAP-ACC-1", PLACEHOLDER, Side::Long)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_second_hearing_of_one_replacement_moves_nothing() {
        let store = MemoryStore::new();
        stated(&store, "2026-09-08", PLACEHOLDER, "5");

        move_positions(&store, &replaced(), &at(NOW)).unwrap();
        assert!(move_positions(&store, &replaced(), &at(NOW))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_replacement_naming_itself_moves_nothing() {
        let store = MemoryStore::new();
        stated(&store, "2026-09-08", PLACEHOLDER, "5");

        let mut itself = replaced();
        itself.instrument.as_mut().unwrap().instrument_id = PLACEHOLDER.into();

        assert!(move_positions(&store, &itself, &at(NOW))
            .unwrap()
            .is_empty());
        assert!(store
            .custodial_position("SNAP-ACC-1", PLACEHOLDER, Side::Long)
            .unwrap()
            .is_some());
    }

    // ── Every change numbered (W2.4, contract v7) ─────────────────────────

    fn journal(event: &CustodialPositionUpdatedEvent) -> (u64, u64) {
        let journal = event.journal.as_ref().expect("numbered");
        assert_eq!(journal.partition, "street");
        (journal.sequence, journal.previous_sequence)
    }

    #[test]
    fn each_change_takes_the_partitions_next_number_chained_per_account() {
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let first = record_holding(&store, &holding_request(&statement_id), &at(NOW)).unwrap();
        let mut other_account = holding_request(&statement_id);
        other_account.account_id = "SNAP-ACC-1".into();
        other_account.instrument_id = "INS-OTHER".into();
        let second = record_holding(&store, &other_account, &at(NOW)).unwrap();

        assert_eq!(
            journal(&first.event.unwrap()),
            (1, 0),
            "the account's first"
        );
        assert_eq!(
            journal(&second.event.unwrap()),
            (2, 1),
            "chained to the first"
        );
    }

    #[test]
    fn another_accounts_changes_do_not_break_an_accounts_chain() {
        // A plugin hearing only ACC-1 sees 1 then 3, its previous 1: no gap.
        let store = MemoryStore::new();
        let mut statements = Vec::new();
        for account in ["ACC-1", "ACC-2", "ACC-1"] {
            let mut statement = statement_request();
            statement.external_statement_id = format!("st-{}-{account}", statements.len());
            statement.account_id = account.into();
            let id = open_statement(&store, &statement, &at(NOW))
                .unwrap()
                .reply
                .statement_id;
            statements.push((id, account));
        }
        let mut changes = Vec::new();
        for (n, (statement_id, account)) in statements.iter().enumerate() {
            let mut row = holding_request(statement_id);
            row.account_id = (*account).into();
            row.instrument_id = format!("INS-{n}");
            changes.push(journal(
                &record_holding(&store, &row, &at(NOW))
                    .unwrap()
                    .event
                    .unwrap(),
            ));
        }
        assert_eq!(changes, vec![(1, 0), (2, 0), (3, 1)]);
    }

    #[test]
    fn a_statements_completion_is_its_own_chain() {
        // Per row (Q3 clarified 2026-10-01): a plugin hearing only statements
        // sees no false gap from positions.
        let store = MemoryStore::new();
        let mut empty = statement_request();
        empty.expected_rows = 1;
        empty.account_id = "SNAP-ACC-1".into();
        let statement_id = open_statement(&store, &empty, &at(NOW))
            .unwrap()
            .reply
            .statement_id;
        let recorded = record_holding(&store, &holding_request(&statement_id), &at(NOW)).unwrap();

        assert_eq!(journal(&recorded.event.unwrap()), (1, 0));
        let completed = recorded.completed.expect("one row of one");
        let journal = completed.journal.unwrap();
        assert_eq!((journal.sequence, journal.previous_sequence), (2, 0));
        let cause = completed.cause.unwrap();
        assert_eq!(cause.instance_id, "custody-snaptrade-1");
        assert_eq!(cause.causation_id, "msg-1", "the row that completed it");
        assert_eq!(completed.account_id, "SNAP-ACC-1");
    }

    #[test]
    fn a_row_saying_what_the_position_held_takes_no_number() {
        let store = MemoryStore::new();
        let first = opened(&store);
        record_holding(&store, &holding_request(&first), &at(NOW)).unwrap();
        let mut again = statement_request();
        again.external_statement_id = "SNAP-ACC-1/again".into();
        let second = open_statement(&store, &again, &at(NOW))
            .unwrap()
            .reply
            .statement_id;
        assert!(record_holding(&store, &holding_request(&second), &at(NOW))
            .unwrap()
            .event
            .is_none());

        let mut moved = holding_request(&second);
        moved.quantity = quantity("13");
        let changed = record_holding(&store, &moved, &at(NOW))
            .unwrap()
            .event
            .unwrap();
        assert_eq!(journal(&changed), (2, 1), "no hole for the unchanged row");
    }

    #[test]
    fn a_tombstones_removal_is_numbered_and_a_position_returning_is_a_change() {
        let store = MemoryStore::new();
        stated(&store, "2026-09-08", PLACEHOLDER, "5");
        let events = move_positions(&store, &replaced(), &at(NOW)).unwrap();
        assert_eq!(journal(&events[0]), (2, 1));
        assert_eq!(journal(&events[1]), (3, 2), "the removal, chained after");

        stated(&store, "2026-09-10", PLACEHOLDER, "1");
        let back = store
            .custodial_position("SNAP-ACC-1", PLACEHOLDER, Side::Long)
            .unwrap()
            .expect("restated, it stands again");
        assert!(!back.removed);
        assert_eq!(back.last_change.sequence, 4);
    }

    // ── The statement's account and figures (W2.2, contract v7) ───────────

    #[test]
    fn a_statement_from_a_plugin_before_v7_takes_its_first_rows_account() {
        let store = MemoryStore::new();
        let statement_id = opened(&store);
        assert_eq!(
            store.statement(&statement_id).unwrap().unwrap().account_id,
            ""
        );
        record_holding(&store, &holding_request(&statement_id), &at(NOW)).unwrap();
        assert_eq!(
            store.statement(&statement_id).unwrap().unwrap().account_id,
            "SNAP-ACC-1"
        );

        let mut elsewhere = holding_request(&statement_id);
        elsewhere.account_id = "ACC-9".into();
        assert!(matches!(
            record_holding(&store, &elsewhere, &at(NOW)),
            Err(StoreError::AnotherAccount { .. })
        ));
    }

    fn segment(name: &str) -> StatementFigures {
        StatementFigures {
            segment: name.into(),
            buying_power: usd("1.00"),
            ..Default::default()
        }
    }

    #[test]
    fn a_statements_figures_are_a_set_per_segment_each_with_its_collateral() {
        let store = MemoryStore::new();
        let mut statement = statement_request();
        statement.buying_power = None;
        statement.account_id = "ACC-1".into();
        statement.external_account_id = "SNAP-ACC-1".into();
        statement.institution = "Interactive Brokers".into();
        statement.figures = vec![
            StatementFigures {
                collateral: vec![ReportedCollateral {
                    direction: CollateralDirection::Posted as i32,
                    instrument_id: "INS-UST10Y".into(),
                    quantity: quantity("500000"),
                    haircut: quantity("0.02"),
                    ..Default::default()
                }],
                ..segment("securities")
            },
            segment("commodities"),
        ];
        statement.expected_rows = 0;
        let opening = open_statement(&store, &statement, &at(NOW)).unwrap();

        let event = opening.completed.expect("no rows: complete at once");
        assert_eq!(event.account_id, "ACC-1");
        assert_eq!(event.external_account_id, "SNAP-ACC-1");
        assert_eq!(event.institution, "Interactive Brokers");
        let segments: Vec<_> = event.figures.iter().map(|f| f.segment.as_str()).collect();
        assert_eq!(segments, ["securities", "commodities"]);
        let collateral = &event.figures[0].collateral[0];
        assert_eq!(read(&collateral.haircut), "0.02");
        assert!(collateral.value.is_none(), "not reported, not zero");
    }

    #[test]
    fn a_statements_figures_that_cannot_stand_are_refused() {
        let store = MemoryStore::new();
        let refused = |figures: Vec<StatementFigures>, flat: bool| {
            let mut statement = statement_request();
            if !flat {
                statement.buying_power = None;
            }
            statement.figures = figures;
            match open_statement(&store, &statement, &at(NOW)) {
                Err(StoreError::Figures(why)) => why,
                other => panic!("not refused: {other:?}"),
            }
        };
        assert!(refused(vec![segment("")], true)
            .starts_with("buying_power is read from a plugin before v7"));
        assert!(refused(vec![segment("a"), segment("a")], false)
            .contains("figures[1].segment \"a\" is named twice"));
        let no_direction = StatementFigures {
            collateral: vec![ReportedCollateral {
                instrument_id: "INS-1".into(),
                quantity: quantity("1"),
                ..Default::default()
            }],
            ..segment("")
        };
        assert!(refused(vec![no_direction], false)
            .contains("figures[0].collateral[0].direction is unspecified"));
    }

    // ── A holding's cost (W2.3, sdk-contract/a-holding-carries-its-cost) ──

    #[test]
    fn a_holdings_cost_and_lots_are_carried_as_reported_and_a_change_to_them_is_a_change() {
        let store = MemoryStore::new();
        let first = opened(&store);
        let mut row = holding_request(&first);
        row.average_cost = usd("150.00");
        row.lots = vec![
            ReportedLot {
                quantity: quantity("10"),
                cost: usd("1500.00"),
                acquired_date: "2024-03-11".into(),
            },
            ReportedLot {
                quantity: quantity("2.5"),
                cost: None,
                acquired_date: String::new(),
            },
        ];
        let event = record_holding(&store, &row, &at(NOW))
            .unwrap()
            .event
            .unwrap();
        let position = event.position.unwrap();
        assert!(position.cost_basis.is_none(), "an average is not a total");
        assert_eq!(read_money(&position.average_cost), "150.00 USD");
        assert_eq!(position.lots.len(), 2);
        assert!(position.lots[1].cost.is_none());

        let mut again = statement_request();
        again.external_statement_id = "SNAP-ACC-1/again".into();
        let second = open_statement(&store, &again, &at(NOW))
            .unwrap()
            .reply
            .statement_id;
        let mut repriced = row.clone();
        repriced.statement_id = second;
        repriced.lots.pop();
        assert!(
            record_holding(&store, &repriced, &at(NOW))
                .unwrap()
                .event
                .is_some(),
            "a lot gone is a change, though the quantity is not"
        );
    }

    // ── What cannot move (W2.3, contract v8) ──

    #[test]
    fn available_and_its_sub_balances_are_carried_as_reported_and_a_change_is_a_change() {
        use meridian_domain::v1::{AvailableBasis, EncumbranceKind};
        let store = MemoryStore::new();
        let first = opened(&store);
        let mut row = holding_request(&first);
        row.available_quantity = quantity("8.5");
        row.not_available_quantity = quantity("4");
        row.available_basis = AvailableBasis::Settled as i32;
        row.encumbrances = vec![ReportedEncumbrance {
            kind: EncumbranceKind::Pledged as i32,
            quantity: quantity("4"),
            available: Some(false),
            source_code: "PLED".into(),
            pledgee: "Interactive Brokers".into(),
            held_at: "DTC".into(),
            ..Default::default()
        }];
        let event = record_holding(&store, &row, &at(NOW))
            .unwrap()
            .event
            .unwrap();
        let position = event.position.unwrap();
        // Available plus not available need not make the holding: as reported.
        assert_eq!(read(&position.available_quantity), "8.5");
        assert_eq!(read(&position.not_available_quantity), "4");
        assert_eq!(position.available_basis, AvailableBasis::Settled as i32);
        assert_eq!(position.encumbrances.len(), 1);
        assert_eq!(position.encumbrances[0].source_code, "PLED");
        assert_eq!(position.encumbrances[0].available, Some(false));

        let mut again = statement_request();
        again.external_statement_id = "SNAP-ACC-1/released".into();
        let second = open_statement(&store, &again, &at(NOW))
            .unwrap()
            .reply
            .statement_id;
        let mut released = row.clone();
        released.statement_id = second;
        released.encumbrances.clear();
        released.available_quantity = None;
        released.not_available_quantity = None;
        let event = record_holding(&store, &released, &at(NOW))
            .unwrap()
            .event
            .expect("a sub-balance released is a change, though the quantity is not");
        let position = event.position.unwrap();
        assert!(
            position.available_quantity.is_none(),
            "not stated: unset, never derived"
        );
        assert!(position.encumbrances.is_empty());

        // No kind, or OTHER with no code, is refused naming the field.
        let mut unnamed = row.clone();
        unnamed.encumbrances[0].kind = EncumbranceKind::Unspecified as i32;
        let refused = record_holding(&store, &unnamed, &at(NOW)).unwrap_err();
        assert!(
            refused.to_string().contains("encumbrances[0].kind"),
            "{refused}"
        );
        let mut other = row.clone();
        other.encumbrances[0].kind = EncumbranceKind::Other as i32;
        other.encumbrances[0].source_code.clear();
        let refused = record_holding(&store, &other, &at(NOW)).unwrap_err();
        assert!(refused.to_string().contains("source_code"), "{refused}");
    }
}
