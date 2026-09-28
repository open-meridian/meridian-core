//! Opening a statement and recording its rows. W2.2, W2.3 and W2.4; and
//! moving the positions they made off a placeholder once it is replaced, W3.9.
//!
//! The wire's shapes translated into the street store's, and back. A deliberate
//! translation rather than storing the generated types: the store's shape is
//! the street store's business and the wire's is the contract's, and letting one be
//! the other means a schema change reaches into the street store without passing
//! anything that could object.

use meridian_domain::v1::{
    CustodialPositionUpdatedEvent, HoldingSide, Identifier as PbIdentifier,
    InstrumentReplacedEvent, RecordHoldingReply, RecordHoldingRequest,
    RecordHoldingsStatementReply, RecordHoldingsStatementRequest, StatementRecordedEvent,
};

use crate::amounts::{Money, Quantity};
use crate::ids;
use crate::store::{
    Completion, CustodialPosition, Figures, Holding, Identifier, Opened, Result, Settled, Side,
    Statement, Store, StoreError,
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
    now_ns: i64,
) -> Result<Opening> {
    let (statement, opened, completion) = store.open(Statement {
        statement_id: ids::statement(now_ns),
        source: request.source.clone(),
        external_statement_id: request.external_statement_id.clone(),
        as_of_date: request.as_of_date.clone(),
        read_at_ns: request.read_at_ns,
        expected_rows: request.expected_rows.max(0) as u32,

        // As the venue reported them, each absent where it reported none.
        // Nothing here computes one from the rows: that would be our figure
        // presented as the custodian's.
        figures: Figures {
            buying_power: Money::reported("buying_power", request.buying_power.as_ref())?,
            margin_requirement: Money::reported(
                "margin_requirement",
                request.margin_requirement.as_ref(),
            )?,
            maintenance_excess: Money::reported(
                "maintenance_excess",
                request.maintenance_excess.as_ref(),
            )?,
            currency_assumed: request.currency_assumed,
        },
    })?;

    Ok(Opening {
        reply: RecordHoldingsStatementReply {
            statement_id: statement.statement_id.clone(),
            already_recorded: matches!(opened, Opened::AlreadyRecorded),
        },
        completed: match completion {
            Completion::Nothing => None,
            Completion::JustCompleted => Some(StatementRecordedEvent {
                statement_id: statement.statement_id,
                source: statement.source,
                as_of_date: statement.as_of_date,
                rows_received: 0,
                rows_resolved: 0,
                rows_unresolved: 0,
                recorded_at_ns: now_ns,
            }),
        },
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
    now_ns: i64,
) -> Result<Recorded> {
    let holding_id = ids::holding(now_ns);

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

        // Nothing has asked the platform about these identifiers yet. W3.2 is
        // the connector's obligation and it happens before this.
        escalated: false,
    };

    let resolved = holding.resolved();
    let (settled, completion) = store.record(holding, now_ns)?;

    let completed = match completion {
        Completion::Nothing => None,
        Completion::JustCompleted => {
            let statement = store
                .statement(&request.statement_id)?
                .ok_or_else(|| StoreError::UnknownStatement(request.statement_id.clone()))?;
            let counts = store.counts(&request.statement_id)?;

            Some(StatementRecordedEvent {
                statement_id: statement.statement_id,
                source: statement.source,
                as_of_date: statement.as_of_date,
                rows_received: counts.received as i32,
                rows_resolved: counts.resolved as i32,
                rows_unresolved: counts.unresolved as i32,
                recorded_at_ns: now_ns,
            })
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
            } => Some(CustodialPositionUpdatedEvent {
                position: Some(to_wire_position(&position)),
                statement_id: request.statement_id.clone(),
                previous_quantity: previous_quantity.to_wire(),
            }),
            Settled::Unchanged { .. } | Settled::Unresolved => None,
        },
        completed,
    })
}

/// W3.9. Move what a replaced placeholder held onto the instrument that
/// replaced it, and produce the W2.6 events for what now stands under it.
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
        .move_positions(replaced_id, instrument_id)?
        .into_iter()
        .filter_map(|settled| match settled {
            Settled::Changed {
                position,
                previous_quantity,
            } => Some(CustodialPositionUpdatedEvent {
                statement_id: position.last_statement_id.clone(),
                position: Some(to_wire_position(&position)),
                previous_quantity: previous_quantity.to_wire(),
            }),
            Settled::Unchanged { .. } | Settled::Unresolved => None,
        })
        .collect())
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amounts::testing::{quantity, read, read_money, usd};
    use crate::store::{Counts, StoreError};
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

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
        open_statement(store, &statement_request(), NOW)
            .unwrap()
            .reply
            .statement_id
    }

    #[test]
    fn opening_a_statement_mints_one_and_says_it_is_new() {
        let store = MemoryStore::new();
        let reply = open_statement(&store, &statement_request(), NOW)
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
        let first = open_statement(&store, &statement_request(), NOW)
            .unwrap()
            .reply;
        let again = open_statement(&store, &statement_request(), NOW + 1)
            .unwrap()
            .reply;

        assert_eq!(again.statement_id, first.statement_id);
        assert!(again.already_recorded);
    }

    #[test]
    fn the_same_external_identifier_from_another_source_is_another_statement() {
        // A rail's identifiers are its own. Two rails may number theirs alike.
        let store = MemoryStore::new();
        let first = open_statement(&store, &statement_request(), NOW)
            .unwrap()
            .reply;

        let mut elsewhere = statement_request();
        elsewhere.source = "another-rail".into();
        let second = open_statement(&store, &elsewhere, NOW).unwrap().reply;

        assert_ne!(second.statement_id, first.statement_id);
        assert!(!second.already_recorded);
    }

    #[test]
    fn a_resolved_row_is_recorded_and_moves_a_position() {
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let recorded = record_holding(&store, &holding_request(&statement_id), NOW).unwrap();

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

        let recorded = record_holding(&store, &unresolved_request(&statement_id), NOW).unwrap();

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
            record_holding(&store, &both, NOW),
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
            record_holding(&store, &neither, NOW),
            Err(StoreError::NeitherResolvedNorIdentified)
        ));
    }

    #[test]
    fn a_row_for_a_statement_nobody_opened_is_refused() {
        let store = MemoryStore::new();
        assert!(matches!(
            record_holding(&store, &holding_request("STMT-nobody-opened"), NOW),
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
        record_holding(&store, &holding_request(&first), NOW).unwrap();

        let mut later = statement_request();
        later.external_statement_id = "SNAP-ACC-1/1757462400000000000".into();
        later.as_of_date = "2026-09-09".into();
        let second = open_statement(&store, &later, NOW + 1)
            .unwrap()
            .reply
            .statement_id;
        record_holding(&store, &holding_request(&second), NOW + 1).unwrap();

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
        record_holding(&store, &holding_request(&first), NOW).unwrap();

        let mut later = statement_request();
        later.external_statement_id = "SNAP-ACC-1/1757462400000000000".into();
        let second = open_statement(&store, &later, NOW + 1)
            .unwrap()
            .reply
            .statement_id;

        let mut grown = holding_request(&second);
        grown.quantity = quantity("20");
        let recorded = record_holding(&store, &grown, NOW + 1).unwrap();

        let event = recorded.event.unwrap();
        assert_eq!(read(&event.previous_quantity), "12.5");
        assert_eq!(read(&event.position.unwrap().quantity), "20");
    }

    #[test]
    fn a_row_that_changes_nothing_announces_nothing() {
        let store = MemoryStore::new();

        let first = opened(&store);
        record_holding(&store, &holding_request(&first), NOW).unwrap();

        let mut again = statement_request();
        again.external_statement_id = "SNAP-ACC-1/1757462400000000000".into();
        let second = open_statement(&store, &again, NOW + 1)
            .unwrap()
            .reply
            .statement_id;
        let unchanged = record_holding(&store, &holding_request(&second), NOW + 1).unwrap();

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
        let recorded = record_holding(&store, &short, NOW).unwrap();

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
            record_holding(&store, &sideless, NOW),
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
        let refused = record_holding(&store, &contradicted, NOW).unwrap_err();
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
            record_holding(&store, &negative_long, NOW),
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
            record_holding(&store, &unstated, NOW),
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
        record_holding(&store, &long, NOW).unwrap();
        record_holding(&store, &short, NOW).unwrap();

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
        let recorded = record_holding(&store, &no, NOW).unwrap();

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
        let recorded = record_holding(&store, &unvalued, NOW).unwrap();

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
        record_holding(&store, &cash, NOW).unwrap();

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
        record_holding(&store, &cash, NOW).unwrap();

        let mut fund = holding_request(&statement_id);
        fund.instrument_id = "INS-01J8XQ4M7K0000000000SPAXX".into();
        fund.quantity = quantity("500");
        fund.market_value = usd("500.00");
        fund.also_counted_in_cash = true;
        let recorded = record_holding(&store, &fund, NOW).unwrap();

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
        record_holding(&store, &unsettled, NOW).unwrap();

        let mut later = statement_request();
        later.external_statement_id = "SNAP-ACC-1/1757462400000000000".into();
        let second = open_statement(&store, &later, NOW + 1)
            .unwrap()
            .reply
            .statement_id;
        let mut settled = holding_request(&second);
        settled.settle_date_quantity = quantity("12.5");
        let recorded = record_holding(&store, &settled, NOW + 1).unwrap();

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
        let statement_id = open_statement(&store, &etrade, NOW)
            .unwrap()
            .reply
            .statement_id;

        let figures = store.statement(&statement_id).unwrap().unwrap().figures;
        assert_eq!(figures.buying_power.unwrap().to_string(), "41250.00 USD");
        assert_eq!(
            figures.margin_requirement.unwrap().to_string(),
            "18250.00 USD"
        );
        assert_eq!(figures.maintenance_excess, None, "not reported, not zero");
        assert!(figures.currency_assumed);
    }

    #[test]
    fn a_statement_completes_on_the_row_that_reaches_its_count() {
        // W2.5. The fixture's statement says four rows follow.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        for n in 0..3 {
            let mut row = holding_request(&statement_id);
            row.instrument_id = format!("INS-{n}");
            let early = record_holding(&store, &row, NOW).unwrap();
            assert!(early.completed.is_none(), "announced after {} rows", n + 1);
        }

        let mut last = holding_request(&statement_id);
        last.instrument_id = String::new();
        last.unresolved_identifiers = vec![PbIdentifier {
            scheme: "symbol".into(),
            value: "ZZTOP".into(),
            source: "snaptrade".into(),
        }];
        let fourth = record_holding(&store, &last, NOW).unwrap();

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
            assert!(record_holding(&store, &row, NOW)
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
            record_holding(&store, &row, NOW).unwrap();
        }

        let mut extra = holding_request(&statement_id);
        extra.instrument_id = "INS-surplus".into();
        let beyond = record_holding(&store, &extra, NOW).unwrap();

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
        let opening = open_statement(&store, &empty, NOW).unwrap();

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

        assert!(open_statement(&store, &empty, NOW)
            .unwrap()
            .completed
            .is_some());
        let again = open_statement(&store, &empty, NOW + 1).unwrap();

        assert!(again.reply.already_recorded);
        assert!(again.completed.is_none());
    }

    #[test]
    fn the_counts_always_add_up() {
        // The fixture's postcondition for W2.5, which nothing publishes yet.
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        record_holding(&store, &holding_request(&statement_id), NOW).unwrap();
        record_holding(&store, &unresolved_request(&statement_id), NOW).unwrap();

        let mut other = holding_request(&statement_id);
        other.instrument_id = "INS-OTHER".into();
        record_holding(&store, &other, NOW).unwrap();

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
        let statement_id = open_statement(store, &statement, NOW)
            .unwrap()
            .reply
            .statement_id;

        let mut row = holding_request(&statement_id);
        row.instrument_id = instrument_id.into();
        row.quantity = quantity(held);
        record_holding(store, &row, NOW).unwrap();
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

        let events = move_positions(&store, &replaced()).unwrap();

        assert_eq!(events.len(), 1);
        let position = events[0].position.as_ref().unwrap();
        assert_eq!(position.instrument_id, REPLACEMENT);
        assert_eq!(read(&position.quantity), "5");
        assert_eq!(position.as_of_date, "2026-09-08");
        assert_eq!(events[0].statement_id, position.last_statement_id);
        // New under its instrument, so changed from nothing.
        assert_eq!(read(&events[0].previous_quantity), "0");

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

        let events = move_positions(&store, &replaced()).unwrap();

        assert_eq!(events.len(), 1);
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

        let events = move_positions(&store, &replaced()).unwrap();

        assert!(events.is_empty(), "{events:?}");
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

        move_positions(&store, &replaced()).unwrap();
        assert!(move_positions(&store, &replaced()).unwrap().is_empty());
    }

    #[test]
    fn a_replacement_naming_itself_moves_nothing() {
        let store = MemoryStore::new();
        stated(&store, "2026-09-08", PLACEHOLDER, "5");

        let mut itself = replaced();
        itself.instrument.as_mut().unwrap().instrument_id = PLACEHOLDER.into();

        assert!(move_positions(&store, &itself).unwrap().is_empty());
        assert!(store
            .custodial_position("SNAP-ACC-1", PLACEHOLDER, Side::Long)
            .unwrap()
            .is_some());
    }
}
