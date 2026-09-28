//! Reading the street store. W2.7.
//!
//! One request answers two questions, and that is the design rather than a
//! convenience. "What do I hold" and "what could I not account for" have to be
//! answered together, because a view that answers only the first hides its own
//! gaps, and the gap is the thing an operator needs to see.
//!
//! Paged by the whole key -- account, instrument and side -- because a read of
//! every account is what a plugin reading its scope makes, and a cursor of the
//! instrument alone skipped every row in a later account whose instrument
//! sorted before it. The unresolved rows come with the first page and no
//! other, so a reader going page by page sees each once.

use meridian_domain::v1::{
    ListCustodialPositionsReply, ListCustodialPositionsRequest, UnresolvedHolding,
};

use crate::amounts::Money;
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
    request: &ListCustodialPositionsRequest,
) -> Result<ListCustodialPositionsReply> {
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

    Ok(ListCustodialPositionsReply {
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
        quantity: holding.quantity.to_wire(),
        market_value: holding.market_value.as_ref().and_then(Money::to_wire),
        source: String::new(),
        as_of_date: String::new(),
        escalated: holding.escalated,
    }
}

#[cfg(test)]
mod tests {
    use meridian_domain::v1::{
        HoldingSide, Identifier as PbIdentifier, RecordHoldingRequest,
        RecordHoldingsStatementRequest,
    };

    use super::*;
    use crate::amounts::testing::{quantity, read, usd};
    use crate::record::{open_statement, record_holding};
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    fn street() -> (MemoryStore, String) {
        let store = MemoryStore::new();
        let statement_id = open_statement(
            &store,
            &RecordHoldingsStatementRequest {
                source: "snaptrade".into(),
                external_statement_id: "st-2026-09-08-SNAP-ACC-1".into(),
                as_of_date: "2026-09-08".into(),
                read_at_ns: NOW,
                expected_rows: 2,
                ..Default::default()
            },
            NOW,
        )
        .unwrap()
        .reply
        .statement_id;

        record_holding(
            &store,
            &RecordHoldingRequest {
                statement_id: statement_id.clone(),
                account_id: "SNAP-ACC-1".into(),
                instrument_id: "INS-01J8XQ4M7K0000000000AAPL".into(),
                quantity: quantity("12.5"),
                market_value: usd("2812.5"),
                side: HoldingSide::Long as i32,
                ..Default::default()
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
                quantity: quantity("5"),
                market_value: usd("0"),
                side: HoldingSide::Long as i32,
                ..Default::default()
            },
            NOW,
        )
        .unwrap();

        (store, statement_id)
    }

    fn asking(include_unresolved: bool) -> ListCustodialPositionsRequest {
        ListCustodialPositionsRequest {
            account_id: "SNAP-ACC-1".into(),
            include_unresolved,
            page_size: 100,
            cursor: String::new(),
        }
    }

    #[test]
    fn one_request_answers_what_is_held_and_what_could_not_be_accounted_for() {
        let (store, _) = street();
        let reply = list_positions(&store, &asking(true)).unwrap();

        assert_eq!(reply.positions.len(), 1);
        assert_eq!(read(&reply.positions[0].quantity), "12.5");

        assert_eq!(reply.unresolved.len(), 1);
        assert_eq!(reply.unresolved[0].identifiers[0].value, "ZZTOP");
        assert_eq!(read(&reply.unresolved[0].quantity), "5");
        assert!(!reply.unresolved[0].escalated);
    }

    #[test]
    fn gaps_are_handed_back_only_when_they_were_asked_for() {
        // The reply's postcondition. A reader who did not ask is not handed
        // them silently.
        let (store, _) = street();
        let reply = list_positions(&store, &asking(false)).unwrap();

        assert_eq!(reply.positions.len(), 1);
        assert!(reply.unresolved.is_empty());
    }

    #[test]
    fn another_accounts_holdings_are_not_in_the_answer() {
        let (store, statement_id) = street();
        record_holding(
            &store,
            &RecordHoldingRequest {
                statement_id,
                account_id: "SNAP-ACC-2".into(),
                instrument_id: "INS-OTHER".into(),
                quantity: quantity("0.000001"),
                market_value: usd("0.000001"),
                side: HoldingSide::Long as i32,
                ..Default::default()
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
        let (store, statement_id) = street();
        for n in 0..5 {
            record_holding(
                &store,
                &RecordHoldingRequest {
                    statement_id: statement_id.clone(),
                    account_id: "SNAP-ACC-1".into(),
                    instrument_id: format!("INS-{n:03}"),
                    quantity: quantity("0.000001"),
                    market_value: usd("0.000001"),
                    side: HoldingSide::Long as i32,
                    ..Default::default()
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
        let (store, statement_id) = street();
        for n in 0..4 {
            record_holding(
                &store,
                &RecordHoldingRequest {
                    statement_id: statement_id.clone(),
                    account_id: "SNAP-ACC-1".into(),
                    instrument_id: format!("INS-{n:03}"),
                    quantity: quantity("0.000001"),
                    market_value: usd("0.000001"),
                    side: HoldingSide::Long as i32,
                    ..Default::default()
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
        let mut seen: Vec<String> = first
            .positions
            .iter()
            .chain(second.positions.iter())
            .map(|position| position.instrument_id.clone())
            .collect();
        let read = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), read, "no row twice");
        assert_eq!(read, 5, "and none missed: the fixture's and the four");
    }

    /// Every account, a page of `size` at a time, until the store says there
    /// is no more.
    fn read_everything(store: &MemoryStore, size: i32) -> ListCustodialPositionsReply {
        let mut everything = ListCustodialPositionsReply::default();
        let mut cursor = String::new();
        loop {
            let page = list_positions(
                store,
                &ListCustodialPositionsRequest {
                    account_id: String::new(),
                    include_unresolved: true,
                    page_size: size,
                    cursor: cursor.clone(),
                },
            )
            .unwrap();
            everything.positions.extend(page.positions);
            everything.unresolved.extend(page.unresolved);
            if page.next_cursor.is_empty() {
                return everything;
            }
            cursor = page.next_cursor;
        }
    }

    #[test]
    fn a_read_of_every_account_across_pages_sees_each_position_once() {
        // kernel/position-paging-skips-rows: the cursor was the instrument
        // alone, so ACC-B's INS-A, sorting before ACC-A's INS-Z, was skipped
        // by the page after ACC-A's. And both sides of one instrument are two
        // rows, which a page boundary may fall between.
        let (store, statement_id) = street();
        let rows = [
            ("SNAP-ACC-1", "INS-Z", HoldingSide::Long, "1"),
            ("SNAP-ACC-2", "INS-A", HoldingSide::Long, "2"),
            ("SNAP-ACC-2", "INS-M", HoldingSide::Long, "3"),
            ("SNAP-ACC-2", "INS-M", HoldingSide::Short, "-4"),
            ("SNAP-ACC-3", "INS-A", HoldingSide::Short, "-5"),
        ];
        for (account, instrument, side, held) in rows {
            record_holding(
                &store,
                &RecordHoldingRequest {
                    statement_id: statement_id.clone(),
                    account_id: account.into(),
                    instrument_id: instrument.into(),
                    quantity: quantity(held),
                    side: side as i32,
                    ..Default::default()
                },
                NOW,
            )
            .unwrap();
        }

        for size in 1..=7 {
            let everything = read_everything(&store, size);
            let keys: Vec<(String, String, i32)> = everything
                .positions
                .iter()
                .map(|p| (p.account_id.clone(), p.instrument_id.clone(), p.side))
                .collect();
            let mut sorted = keys.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(keys, sorted, "in key order, none twice, at {size} a page");
            assert_eq!(keys.len(), 6, "none skipped at {size} a page: {keys:?}");
            assert_eq!(
                everything.unresolved.len(),
                1,
                "the unresolved row once, at {size} a page"
            );
        }
    }

    #[test]
    fn a_cursor_this_store_did_not_write_is_refused() {
        let (store, _) = street();
        let mut forged = asking(false);
        forged.cursor = "INS-01J8XQ4M7K0000000000AAPL".into();
        assert!(matches!(
            list_positions(&store, &forged),
            Err(crate::StoreError::UnreadableCursor(_))
        ));
    }
}
