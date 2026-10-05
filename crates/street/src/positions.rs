//! Reading the street store. W2.7 and W2.9.
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
//!
//! Within the reader's scope (W4.11): a plugin's read, marked by its sidecar,
//! answers only the accounts in its scope, none when it is empty, and is
//! refused naming one outside it; a core component's reads every account.
//! Each page answers the watermark it was read at, and a read given one
//! answers what changed since, removed positions as tombstones: how a plugin
//! catches up (spec/plugins-hear-and-read, Q1).

use meridian_domain::v1::{
    ListCustodialPositionsReply, ListCustodialPositionsRequest, ListStatementsReply,
    ListStatementsRequest, PartitionSequence, UnresolvedHolding, Watermark,
};

use meridian_pb::bounds::{
    Range, LIST_CUSTODIAL_POSITIONS_REQUEST_PAGE_SIZE_RANGE,
    LIST_STATEMENTS_REQUEST_PAGE_SIZE_RANGE,
};

use crate::amounts::Money;
use crate::record::{statement_recorded, to_wire_identifier, to_wire_position};
use crate::store::{Holding, Read, Result, Scope, StatementsRead, Store, PARTITION};

/// The rows a reply carries when the caller asks for none.
const DEFAULT_PAGE: usize = 100;

/// A page's size: the default when none is asked, and never past the most
/// the read's page_size entry allows, whatever was asked for -- a page size
/// is a request from a caller, not an instruction, and without a ceiling one
/// caller asking for everything decides how much memory this process uses.
pub(crate) fn limit(page_size: i32, bound: Range) -> usize {
    match page_size {
        size if size <= 0 => DEFAULT_PAGE,
        size => (size as i64).min(bound.most) as usize,
    }
}

/// The street's sequence in a watermark a reader gave: everything when it
/// names none.
pub(crate) fn since(watermark: Option<&Watermark>) -> Option<u64> {
    watermark.map(|watermark| {
        watermark
            .partitions
            .iter()
            .find(|held| held.partition == PARTITION)
            .map(|held| held.sequence)
            .unwrap_or(0)
    })
}

pub(crate) fn as_of(sequence: u64) -> Watermark {
    Watermark {
        partitions: vec![PartitionSequence {
            partition: PARTITION.to_string(),
            sequence,
        }],
    }
}

pub fn list_positions(
    store: &dyn Store,
    request: &ListCustodialPositionsRequest,
    scope: &Scope,
) -> Result<ListCustodialPositionsReply> {
    let page = store.page(&Read {
        scope: scope.clone(),
        account_id: request.account_id.clone(),
        include_unresolved: request.include_unresolved,
        limit: limit(
            request.page_size,
            LIST_CUSTODIAL_POSITIONS_REQUEST_PAGE_SIZE_RANGE,
        ),
        cursor: request.cursor.clone(),
        since: since(request.since.as_ref()),
    })?;

    Ok(ListCustodialPositionsReply {
        positions: page.positions.iter().map(to_wire_position).collect(),
        unresolved: page.unresolved.iter().map(to_wire_unresolved).collect(),
        next_cursor: page.next_cursor,
        as_of: Some(as_of(page.as_of)),
    })
}

/// W2.9: completed statements, each as it was announced (W2.5).
pub fn list_statements(
    store: &dyn Store,
    request: &ListStatementsRequest,
    scope: &Scope,
) -> Result<ListStatementsReply> {
    let page = store.statements(&StatementsRead {
        scope: scope.clone(),
        account_id: request.account_id.clone(),
        as_of_date: request.as_of_date.clone(),
        limit: limit(request.page_size, LIST_STATEMENTS_REQUEST_PAGE_SIZE_RANGE),
        cursor: request.cursor.clone(),
        since: since(request.since.as_ref()),
    })?;

    Ok(ListStatementsReply {
        statements: page
            .statements
            .iter()
            .map(|(statement, counts)| statement_recorded(statement, *counts))
            .collect(),
        next_cursor: page.next_cursor,
        as_of: Some(as_of(page.as_of)),
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
        raw_record: crate::record::raw_to_wire(&holding.cost.raw_record),
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
    use crate::store::Scope;
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    fn at(now: i64) -> crate::store::Cause {
        crate::store::Cause {
            committed_at_ns: now,
            ..Default::default()
        }
    }

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
            &at(NOW),
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
            &at(NOW),
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
            &at(NOW),
        )
        .unwrap();

        (store, statement_id)
    }

    /// A statement of its own for `account`, which a row of it lands in: a
    /// statement is one account's (W2.2).
    fn statement_of(store: &MemoryStore, account: &str) -> String {
        open_statement(
            store,
            &RecordHoldingsStatementRequest {
                source: "snaptrade".into(),
                external_statement_id: format!("st-{account}"),
                as_of_date: "2026-09-08".into(),
                read_at_ns: NOW,
                expected_rows: 100,
                account_id: account.into(),
                ..Default::default()
            },
            &at(NOW),
        )
        .unwrap()
        .reply
        .statement_id
    }

    fn asking(include_unresolved: bool) -> ListCustodialPositionsRequest {
        ListCustodialPositionsRequest {
            account_id: "SNAP-ACC-1".into(),
            include_unresolved,
            page_size: 100,
            cursor: String::new(),
            since: None,
        }
    }

    #[test]
    fn one_request_answers_what_is_held_and_what_could_not_be_accounted_for() {
        let (store, _) = street();
        let reply = list_positions(&store, &asking(true), &Scope::Everything).unwrap();

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
        let reply = list_positions(&store, &asking(false), &Scope::Everything).unwrap();

        assert_eq!(reply.positions.len(), 1);
        assert!(reply.unresolved.is_empty());
    }

    #[test]
    fn another_accounts_holdings_are_not_in_the_answer() {
        let (store, _) = street();
        let statement_id = statement_of(&store, "SNAP-ACC-2");
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
            &at(NOW),
        )
        .unwrap();

        let reply = list_positions(&store, &asking(true), &Scope::Everything).unwrap();
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
                &at(NOW),
            )
            .unwrap();
        }

        let mut greedy = asking(false);
        greedy.page_size = 100_000;
        let reply = list_positions(&store, &greedy, &Scope::Everything).unwrap();
        assert!(
            reply.positions.len() <= LIST_CUSTODIAL_POSITIONS_REQUEST_PAGE_SIZE_RANGE.most as usize
        );

        let mut absent = asking(false);
        absent.page_size = 0;
        assert!(
            list_positions(&store, &absent, &Scope::Everything)
                .unwrap()
                .positions
                .len()
                <= DEFAULT_PAGE
        );
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
                &at(NOW),
            )
            .unwrap();
        }

        let mut small = asking(false);
        small.page_size = 2;
        let first = list_positions(&store, &small, &Scope::Everything).unwrap();
        assert_eq!(first.positions.len(), 2);
        assert!(!first.next_cursor.is_empty());

        let mut rest = asking(false);
        rest.page_size = 100;
        rest.cursor = first.next_cursor.clone();
        let second = list_positions(&store, &rest, &Scope::Everything).unwrap();

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
                    since: None,
                },
                &Scope::Everything,
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
        let (store, first) = street();
        let rows = [
            ("SNAP-ACC-1", "INS-Z", HoldingSide::Long, "1"),
            ("SNAP-ACC-2", "INS-A", HoldingSide::Long, "2"),
            ("SNAP-ACC-2", "INS-M", HoldingSide::Long, "3"),
            ("SNAP-ACC-2", "INS-M", HoldingSide::Short, "-4"),
            ("SNAP-ACC-3", "INS-A", HoldingSide::Short, "-5"),
        ];
        for (account, instrument, side, held) in rows {
            let statement_id = if account == "SNAP-ACC-1" {
                first.clone()
            } else {
                statement_of(&store, account)
            };
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
                &at(NOW),
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
            list_positions(&store, &forged, &Scope::Everything),
            Err(crate::StoreError::UnreadableCursor(_))
        ));
    }

    // ── Within the reader's scope, at a watermark (W2.7, W2.9, W4.11) ─────

    fn within(accounts: &[&str]) -> Scope {
        Scope::Within(accounts.iter().map(|a| a.to_string()).collect())
    }

    fn every_account() -> ListCustodialPositionsRequest {
        ListCustodialPositionsRequest {
            account_id: String::new(),
            include_unresolved: true,
            page_size: 100,
            cursor: String::new(),
            since: None,
        }
    }

    #[test]
    fn a_plugins_read_answers_its_scope_and_an_empty_one_nothing() {
        let (store, _) = street();
        let elsewhere = statement_of(&store, "SNAP-ACC-2");
        record_holding(
            &store,
            &RecordHoldingRequest {
                statement_id: elsewhere,
                account_id: "SNAP-ACC-2".into(),
                instrument_id: "INS-OTHER".into(),
                quantity: quantity("1"),
                side: HoldingSide::Long as i32,
                ..Default::default()
            },
            &at(NOW),
        )
        .unwrap();

        let scoped = list_positions(&store, &every_account(), &within(&["SNAP-ACC-1"])).unwrap();
        assert_eq!(scoped.positions.len(), 1);
        assert_eq!(scoped.unresolved.len(), 1);

        let none = list_positions(&store, &every_account(), &within(&[])).unwrap();
        assert!(
            none.positions.is_empty() && none.unresolved.is_empty(),
            "never everything"
        );

        let everything = list_positions(&store, &every_account(), &Scope::Everything).unwrap();
        assert_eq!(
            everything.positions.len(),
            2,
            "a core component reads every account"
        );
    }

    #[test]
    fn a_plugins_read_naming_an_account_outside_its_scope_is_refused() {
        let (store, _) = street();
        assert!(matches!(
            list_positions(&store, &asking(true), &within(&["ACC-2"])),
            Err(crate::StoreError::OutOfScope(account)) if account == "SNAP-ACC-1"
        ));
    }

    #[test]
    fn a_read_answers_its_watermark_and_a_read_since_one_what_changed() {
        let (store, statement_id) = street();
        let first = list_positions(&store, &every_account(), &Scope::Everything).unwrap();
        let at_first = first.as_of.unwrap().partitions[0].sequence;
        assert_eq!(
            at_first, 2,
            "the resolved row's position, and the statement's completion"
        );

        let mut moved = every_account();
        moved.since = Some(as_of(at_first));
        assert!(list_positions(&store, &moved, &Scope::Everything)
            .unwrap()
            .positions
            .is_empty());

        record_holding(
            &store,
            &RecordHoldingRequest {
                statement_id,
                account_id: "SNAP-ACC-1".into(),
                instrument_id: "INS-NEW".into(),
                quantity: quantity("3"),
                side: HoldingSide::Long as i32,
                ..Default::default()
            },
            &at(NOW),
        )
        .unwrap();
        let since = list_positions(&store, &moved, &Scope::Everything).unwrap();
        let changed: Vec<_> = since
            .positions
            .iter()
            .map(|p| p.instrument_id.as_str())
            .collect();
        assert_eq!(changed, ["INS-NEW"]);
        let journal = since.positions[0].last_change.as_ref().unwrap();
        assert_eq!(
            (journal.sequence, journal.previous_sequence),
            (3, 1),
            "chained to the account's last position, not its statement"
        );
    }

    #[test]
    fn a_removed_position_is_read_only_since_a_watermark() {
        let store = MemoryStore::new();
        let statement_id = open_statement(
            &store,
            &RecordHoldingsStatementRequest {
                source: "snaptrade".into(),
                external_statement_id: "st".into(),
                as_of_date: "2026-09-08".into(),
                expected_rows: 1,
                ..Default::default()
            },
            &at(NOW),
        )
        .unwrap()
        .reply
        .statement_id;
        record_holding(
            &store,
            &RecordHoldingRequest {
                statement_id,
                account_id: "ACC-1".into(),
                instrument_id: "LCL-1".into(),
                quantity: quantity("5"),
                side: HoldingSide::Long as i32,
                ..Default::default()
            },
            &at(NOW),
        )
        .unwrap();
        crate::record::move_positions(
            &store,
            &meridian_domain::v1::InstrumentReplacedEvent {
                replaced_instrument_id: "LCL-1".into(),
                instrument: Some(meridian_domain::v1::InstrumentRecord {
                    instrument_id: "INS-1".into(),
                    ..Default::default()
                }),
                replaced_at_ns: NOW,
            },
            &at(NOW),
        )
        .unwrap();

        let now = list_positions(&store, &every_account(), &Scope::Everything).unwrap();
        let held: Vec<_> = now
            .positions
            .iter()
            .map(|p| p.instrument_id.as_str())
            .collect();
        assert_eq!(held, ["INS-1"], "no tombstone without a watermark");

        let mut since = every_account();
        since.since = Some(as_of(1));
        let changed = list_positions(&store, &since, &Scope::Everything).unwrap();
        let removed: Vec<_> = changed
            .positions
            .iter()
            .map(|p| (p.instrument_id.as_str(), p.removed))
            .collect();
        assert_eq!(removed, [("INS-1", false), ("LCL-1", true)]);
    }

    fn statements(scope: &Scope, request: ListStatementsRequest) -> ListStatementsReply {
        let store = MemoryStore::new();
        for (n, (account, rows)) in [("ACC-1", 0), ("ACC-2", 0), ("ACC-1", 1)]
            .iter()
            .enumerate()
        {
            open_statement(
                &store,
                &RecordHoldingsStatementRequest {
                    source: "snaptrade".into(),
                    external_statement_id: format!("st-{n}"),
                    as_of_date: format!("2026-09-0{}", n + 1),
                    expected_rows: *rows,
                    account_id: (*account).into(),
                    ..Default::default()
                },
                &at(NOW + n as i64),
            )
            .unwrap();
        }
        list_statements(&store, &request, scope).unwrap()
    }

    #[test]
    fn completed_statements_are_read_within_the_scope_in_the_order_they_completed() {
        let all = statements(&Scope::Everything, ListStatementsRequest::default());
        let read: Vec<_> = all
            .statements
            .iter()
            .map(|s| s.account_id.as_str())
            .collect();
        assert_eq!(read, ["ACC-1", "ACC-2"], "the open one is not listed");
        assert_eq!(all.as_of.unwrap().partitions[0].sequence, 2);

        let scoped = statements(&within(&["ACC-2"]), ListStatementsRequest::default());
        assert_eq!(scoped.statements.len(), 1);
        assert_eq!(scoped.statements[0].account_id, "ACC-2");

        let since = statements(
            &Scope::Everything,
            ListStatementsRequest {
                since: Some(as_of(1)),
                ..Default::default()
            },
        );
        assert_eq!(since.statements.len(), 1);
        assert_eq!(since.statements[0].journal.as_ref().unwrap().sequence, 2);

        let dated = statements(
            &Scope::Everything,
            ListStatementsRequest {
                as_of_date: "2026-09-02".into(),
                ..Default::default()
            },
        );
        assert_eq!(dated.statements.len(), 1);

        let paged = statements(
            &Scope::Everything,
            ListStatementsRequest {
                page_size: 1,
                ..Default::default()
            },
        );
        assert_eq!(paged.statements.len(), 1);
        assert!(!paged.next_cursor.is_empty());
    }
}
