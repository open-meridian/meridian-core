//! Reading the ledger. W2.7.
//!
//! One request answers two questions, and that is the design rather than a
//! convenience. "What do I hold" and "what could I not account for" have to be
//! answered together, because a view that answers only the first hides its own
//! gaps, and the gap is the thing an operator needs to see.

use meridian_pb::v1::{ListPositionsReply, ListPositionsRequest, UnresolvedHolding};

use crate::record::{to_wire_identifier, to_wire_position};
use crate::store::{Holding, Result, Store};

/// The most rows one reply will carry, whatever was asked for.
///
/// A page size is a request from a caller, not an instruction. Without a
/// ceiling, one caller asking for everything decides how much memory this
/// process uses.
const MAX_PAGE: usize = 500;
const DEFAULT_PAGE: usize = 100;

pub fn list_positions(
    store: &dyn Store,
    request: &ListPositionsRequest,
) -> Result<ListPositionsReply> {
    let limit = match request.page_size {
        size if size <= 0 => DEFAULT_PAGE,
        size => (size as usize).min(MAX_PAGE),
    };

    let page = store.page(
        &request.account_id,
        request.include_unresolved,
        limit,
        &request.cursor,
    )?;

    Ok(ListPositionsReply {
        positions: page.positions.iter().map(to_wire_position).collect(),
        unresolved: page.unresolved.iter().map(to_wire_unresolved).collect(),
        next_cursor: page.next_cursor,
    })
}

fn to_wire_unresolved(holding: &Holding) -> UnresolvedHolding {
    UnresolvedHolding {
        holding_id: holding.holding_id.clone(),
        account_id: holding.account_id.clone(),
        identifiers: holding
            .unresolved_identifiers
            .iter()
            .map(to_wire_identifier)
            .collect(),
        quantity_scaled_1e8: holding.quantity.scaled(),
        market_value_scaled_1e8: holding.market_value.scaled(),
        currency: holding.currency.clone(),
        source: String::new(),
        as_of_date: String::new(),
        escalated: holding.escalated,
    }
}

#[cfg(test)]
mod tests {
    use meridian_pb::v1::{
        Identifier as PbIdentifier, RecordHoldingRequest, RecordHoldingsStatementRequest,
    };

    use super::*;
    use crate::record::{open_statement, record_holding};
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    fn ledger() -> (MemoryStore, String) {
        let store = MemoryStore::new();
        let statement_id = open_statement(
            &store,
            &RecordHoldingsStatementRequest {
                source: "snaptrade".into(),
                external_statement_id: "st-2026-09-08-SNAP-ACC-1".into(),
                as_of_date: "2026-09-08".into(),
                read_at_ns: NOW,
            },
            NOW,
        )
        .unwrap()
        .statement_id;

        record_holding(
            &store,
            &RecordHoldingRequest {
                statement_id: statement_id.clone(),
                account_id: "SNAP-ACC-1".into(),
                instrument_id: "INS-01J8XQ4M7K0000000000AAPL".into(),
                unresolved_identifiers: vec![],
                quantity_scaled_1e8: 1_250_000_000,
                market_value_scaled_1e8: 281_250_000_000,
                currency: "USD".into(),
            },
            NOW,
        )
        .unwrap();

        record_holding(
            &store,
            &RecordHoldingRequest {
                statement_id: statement_id.clone(),
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
            },
            NOW,
        )
        .unwrap();

        (store, statement_id)
    }

    fn asking(include_unresolved: bool) -> ListPositionsRequest {
        ListPositionsRequest {
            account_id: "SNAP-ACC-1".into(),
            include_unresolved,
            page_size: 100,
            cursor: String::new(),
        }
    }

    #[test]
    fn one_request_answers_what_is_held_and_what_could_not_be_accounted_for() {
        let (store, _) = ledger();
        let reply = list_positions(&store, &asking(true)).unwrap();

        assert_eq!(reply.positions.len(), 1);
        assert_eq!(reply.positions[0].quantity_scaled_1e8, 1_250_000_000);

        assert_eq!(reply.unresolved.len(), 1);
        assert_eq!(reply.unresolved[0].identifiers[0].value, "ZZTOP");
        assert_eq!(reply.unresolved[0].quantity_scaled_1e8, 500_000_000);
        assert!(!reply.unresolved[0].escalated);
    }

    #[test]
    fn gaps_are_handed_back_only_when_they_were_asked_for() {
        // The reply's postcondition. A reader who did not ask is not handed
        // them silently.
        let (store, _) = ledger();
        let reply = list_positions(&store, &asking(false)).unwrap();

        assert_eq!(reply.positions.len(), 1);
        assert!(reply.unresolved.is_empty());
    }

    #[test]
    fn another_accounts_holdings_are_not_in_the_answer() {
        let (store, statement_id) = ledger();
        record_holding(
            &store,
            &RecordHoldingRequest {
                statement_id,
                account_id: "SNAP-ACC-2".into(),
                instrument_id: "INS-OTHER".into(),
                unresolved_identifiers: vec![],
                quantity_scaled_1e8: 100,
                market_value_scaled_1e8: 100,
                currency: "USD".into(),
            },
            NOW,
        )
        .unwrap();

        let reply = list_positions(&store, &asking(true)).unwrap();
        assert_eq!(reply.positions.len(), 1);
        assert!(reply
            .positions
            .iter()
            .all(|position| position.account_id == "SNAP-ACC-1"));
    }

    #[test]
    fn a_page_size_is_a_request_and_not_an_instruction() {
        // Without a ceiling, one caller asking for everything decides how much
        // memory this process uses.
        let (store, statement_id) = ledger();
        for n in 0..5 {
            record_holding(
                &store,
                &RecordHoldingRequest {
                    statement_id: statement_id.clone(),
                    account_id: "SNAP-ACC-1".into(),
                    instrument_id: format!("INS-{n:03}"),
                    unresolved_identifiers: vec![],
                    quantity_scaled_1e8: 100,
                    market_value_scaled_1e8: 100,
                    currency: "USD".into(),
                },
                NOW,
            )
            .unwrap();
        }

        let mut greedy = asking(false);
        greedy.page_size = 100_000;
        let reply = list_positions(&store, &greedy).unwrap();
        assert!(reply.positions.len() <= MAX_PAGE);

        let mut absent = asking(false);
        absent.page_size = 0;
        assert!(list_positions(&store, &absent).unwrap().positions.len() <= DEFAULT_PAGE);
    }

    #[test]
    fn a_page_carries_a_cursor_when_there_is_more_and_none_when_there_is_not() {
        let (store, statement_id) = ledger();
        for n in 0..4 {
            record_holding(
                &store,
                &RecordHoldingRequest {
                    statement_id: statement_id.clone(),
                    account_id: "SNAP-ACC-1".into(),
                    instrument_id: format!("INS-{n:03}"),
                    unresolved_identifiers: vec![],
                    quantity_scaled_1e8: 100,
                    market_value_scaled_1e8: 100,
                    currency: "USD".into(),
                },
                NOW,
            )
            .unwrap();
        }

        let mut small = asking(false);
        small.page_size = 2;
        let first = list_positions(&store, &small).unwrap();
        assert_eq!(first.positions.len(), 2);
        assert!(!first.next_cursor.is_empty());

        let mut rest = asking(false);
        rest.page_size = 100;
        rest.cursor = first.next_cursor.clone();
        let second = list_positions(&store, &rest).unwrap();

        assert!(second.next_cursor.is_empty());
        assert!(second
            .positions
            .iter()
            .all(|position| position.instrument_id > first.next_cursor));
    }
}
