//! Opening a statement and recording its rows. W2.2, W2.3 and W2.4.
//!
//! The wire's shapes translated into the ledger's, and back. A deliberate
//! translation rather than storing the generated types: the store's shape is
//! the ledger's business and the wire's is the contract's, and letting one be
//! the other means a schema change reaches into the ledger without passing
//! anything that could object.

use meridian_pb::v1::{
    Identifier as PbIdentifier, PositionUpdatedEvent, RecordHoldingReply, RecordHoldingRequest,
    RecordHoldingsStatementReply, RecordHoldingsStatementRequest,
};

use crate::amounts::{Money, Quantity};
use crate::ids;
use crate::store::{Holding, Identifier, Opened, Position, Result, Settled, Statement, Store};

/// W2.2. Open a statement, or recognise one the rail has sent before.
///
/// Redelivery is a no-op and not a duplicate, which is the fixture's second
/// case. The reply says which happened, so a connector that cannot tell whether
/// its last attempt landed can simply send it again.
pub fn open_statement(
    store: &dyn Store,
    request: &RecordHoldingsStatementRequest,
    now_ns: i64,
) -> Result<RecordHoldingsStatementReply> {
    let (statement, opened) = store.open(Statement {
        statement_id: ids::statement(now_ns),
        source: request.source.clone(),
        external_statement_id: request.external_statement_id.clone(),
        as_of_date: request.as_of_date.clone(),
        read_at_ns: request.read_at_ns,
    })?;

    Ok(RecordHoldingsStatementReply {
        statement_id: statement.statement_id,
        already_recorded: matches!(opened, Opened::AlreadyRecorded),
    })
}

/// What recording a row did, and what to announce about it.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub reply: RecordHoldingReply,

    /// Present only when a position moved. An unresolved row publishes nothing
    /// here, because it updates no position, and neither does a row that says
    /// exactly what the position already held.
    pub event: Option<PositionUpdatedEvent>,
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

        quantity: Quantity::from_scaled(request.quantity_scaled_1e8),
        market_value: Money::from_scaled(request.market_value_scaled_1e8),
        currency: request.currency.clone(),

        // Nothing has asked the platform about these identifiers yet. W3.2 is
        // the connector's obligation and it happens before this.
        escalated: false,
    };

    let resolved = holding.resolved();
    let settled = store.record(holding, now_ns)?;

    Ok(Recorded {
        reply: RecordHoldingReply {
            holding_id,
            resolved,
        },
        event: match settled {
            Settled::Changed {
                position,
                previous_quantity,
            } => Some(PositionUpdatedEvent {
                position: Some(to_wire_position(&position)),
                statement_id: request.statement_id.clone(),
                previous_quantity_scaled_1e8: previous_quantity.scaled(),
            }),
            Settled::Unchanged { .. } | Settled::Unresolved => None,
        },
    })
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

pub(crate) fn to_wire_position(position: &Position) -> meridian_pb::v1::Position {
    meridian_pb::v1::Position {
        account_id: position.account_id.clone(),
        instrument_id: position.instrument_id.clone(),
        quantity_scaled_1e8: position.quantity.scaled(),
        market_value_scaled_1e8: position.market_value.scaled(),
        currency: position.currency.clone(),
        last_statement_id: position.last_statement_id.clone(),
        as_of_date: position.as_of_date.clone(),
        updated_at_ns: position.updated_at_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Counts, StoreError};
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    /// The fixture's statement.
    fn statement_request() -> RecordHoldingsStatementRequest {
        RecordHoldingsStatementRequest {
            source: "snaptrade".into(),
            external_statement_id: "st-2026-09-08-SNAP-ACC-1".into(),
            as_of_date: "2026-09-08".into(),
            read_at_ns: NOW,
        }
    }

    /// The fixture's resolved row: 12.5 shares worth 2812.50.
    fn holding_request(statement_id: &str) -> RecordHoldingRequest {
        RecordHoldingRequest {
            statement_id: statement_id.into(),
            account_id: "SNAP-ACC-1".into(),
            instrument_id: "INS-01J8XQ4M7K0000000000AAPL".into(),
            unresolved_identifiers: vec![],
            quantity_scaled_1e8: 1_250_000_000,
            market_value_scaled_1e8: 281_250_000_000,
            currency: "USD".into(),
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
            quantity_scaled_1e8: 500_000_000,
            market_value_scaled_1e8: 0,
            currency: "USD".into(),
        }
    }

    fn opened(store: &MemoryStore) -> String {
        open_statement(store, &statement_request(), NOW)
            .unwrap()
            .statement_id
    }

    #[test]
    fn opening_a_statement_mints_one_and_says_it_is_new() {
        let store = MemoryStore::new();
        let reply = open_statement(&store, &statement_request(), NOW).unwrap();

        assert!(reply.statement_id.starts_with("STMT-"));
        assert!(!reply.already_recorded);
    }

    #[test]
    fn a_redelivered_statement_is_a_no_op_and_not_a_duplicate() {
        // The fixture's named case. A connector that cannot tell whether its
        // last attempt landed sends it again, and nothing is doubled.
        let store = MemoryStore::new();
        let first = open_statement(&store, &statement_request(), NOW).unwrap();
        let again = open_statement(&store, &statement_request(), NOW + 1).unwrap();

        assert_eq!(again.statement_id, first.statement_id);
        assert!(again.already_recorded);
    }

    #[test]
    fn the_same_external_identifier_from_another_source_is_another_statement() {
        // A rail's identifiers are its own. Two rails may number theirs alike.
        let store = MemoryStore::new();
        let first = open_statement(&store, &statement_request(), NOW).unwrap();

        let mut elsewhere = statement_request();
        elsewhere.source = "another-rail".into();
        let second = open_statement(&store, &elsewhere, NOW).unwrap();

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
        assert_eq!(position.quantity_scaled_1e8, 1_250_000_000);
        assert_eq!(position.market_value_scaled_1e8, 281_250_000_000);
        assert_eq!(position.as_of_date, "2026-09-08");
        assert_eq!(position.last_statement_id, statement_id);
        assert_eq!(event.previous_quantity_scaled_1e8, 0);
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
        later.external_statement_id = "st-2026-09-09-SNAP-ACC-1".into();
        later.as_of_date = "2026-09-09".into();
        let second = open_statement(&store, &later, NOW + 1)
            .unwrap()
            .statement_id;
        record_holding(&store, &holding_request(&second), NOW + 1).unwrap();

        let position = store
            .position("SNAP-ACC-1", "INS-01J8XQ4M7K0000000000AAPL")
            .unwrap()
            .unwrap();

        assert_eq!(position.quantity.scaled(), 1_250_000_000);
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
        later.external_statement_id = "st-2026-09-09-SNAP-ACC-1".into();
        let second = open_statement(&store, &later, NOW + 1)
            .unwrap()
            .statement_id;

        let mut grown = holding_request(&second);
        grown.quantity_scaled_1e8 = 2_000_000_000;
        let recorded = record_holding(&store, &grown, NOW + 1).unwrap();

        let event = recorded.event.unwrap();
        assert_eq!(event.previous_quantity_scaled_1e8, 1_250_000_000);
        assert_eq!(event.position.unwrap().quantity_scaled_1e8, 2_000_000_000);
    }

    #[test]
    fn a_row_that_changes_nothing_announces_nothing() {
        let store = MemoryStore::new();

        let first = opened(&store);
        record_holding(&store, &holding_request(&first), NOW).unwrap();

        let mut again = statement_request();
        again.external_statement_id = "st-2026-09-09-SNAP-ACC-1".into();
        let second = open_statement(&store, &again, NOW + 1)
            .unwrap()
            .statement_id;
        let unchanged = record_holding(&store, &holding_request(&second), NOW + 1).unwrap();

        assert!(unchanged.event.is_none());
        assert!(unchanged.reply.resolved);
    }

    #[test]
    fn a_short_position_is_a_position() {
        // "A negative quantity is a short position, not an error."
        let store = MemoryStore::new();
        let statement_id = opened(&store);

        let mut short = holding_request(&statement_id);
        short.quantity_scaled_1e8 = -500_000_000;
        let recorded = record_holding(&store, &short, NOW).unwrap();

        assert!(recorded.reply.resolved);
        assert_eq!(
            recorded
                .event
                .unwrap()
                .position
                .unwrap()
                .quantity_scaled_1e8,
            -500_000_000
        );
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
}
