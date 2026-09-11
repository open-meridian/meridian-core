//! Where the ledger meets the bus.
//!
//! Two commands in, one query in, one event out. W2.2 and W2.3 arrive as
//! commands from a connector, W2.7 as a query from a dashboard, and W2.6 leaves
//! as an event whenever a position actually moved.
//!
//! # Why the position event is published from here
//!
//! Recording a row and announcing that it moved something are one act as far as
//! a subscriber is concerned, and splitting them across two callers would make
//! the announcement depend on which caller remembered. The store decides
//! whether a position moved; this publishes when it did, and stays silent when
//! it did not.
//!
//! A row that says exactly what the position already held publishes nothing.
//! That is deliberate and worth stating, because the opposite convention is
//! also defensible: an event per row would let a subscriber count rows. It
//! would also mean a statement that changed nothing looks identical to one that
//! changed everything, and the fixture's postcondition says an unresolved
//! holding produces no event, which is the same instinct.

use std::sync::Arc;

use meridian_bus::Bus;
use meridian_pb::v1::{ListPositionsRequest, RecordHoldingRequest, RecordHoldingsStatementRequest};
use prost::Message;

use crate::positions::list_positions;
use crate::record::{open_statement, record_holding};
use crate::store::Store;

/// W2.2. A connector opening a statement.
pub const RECORD_STATEMENT: &str = "platform.kernel.command.record-statement";

/// W2.3. A connector publishing one row.
pub const RECORD_HOLDING: &str = "platform.kernel.command.record-holding";

/// W2.6. A position moved.
pub const POSITION_UPDATED: &str = "platform.kernel.event.position-updated";

/// W2.5. A statement has every row it said was coming.
pub const STATEMENT_RECORDED: &str = "platform.kernel.event.statement-recorded";

/// W2.7. A dashboard asking what is held.
pub const LIST_POSITIONS: &str = "platform.kernel.query.list-positions";

/// Where the time comes from, so a test does not wait for it.
pub trait Clock: Send + Sync {
    fn now_ns(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos() as i64)
            .unwrap_or_default()
    }
}

/// Register every handler the kernel serves.
pub fn serve(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    let statements = store.clone();
    let statement_clock = clock.clone();
    let opening_bus = bus.clone();
    bus.serve(RECORD_STATEMENT, move |envelope| {
        expect(
            &envelope.payload_type,
            "meridian.v1.RecordHoldingsStatementRequest",
        )?;

        let request = RecordHoldingsStatementRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable statement: {failed}"))?;

        let now_ns = statement_clock.now_ns();
        let opening = open_statement(statements.as_ref(), &request, now_ns)
            .map_err(|failed| failed.to_string())?;

        // A statement promising no rows is complete at once, so W2.5 can fire
        // here as well as on a row. An account that holds nothing is a real
        // answer, and waiting for a row that was never coming would leave it
        // open forever, which reads as a stuck connector.
        if let Some(event) = opening.completed {
            let meta = envelope.meta.as_ref();
            opening_bus
                .publish(
                    STATEMENT_RECORDED,
                    "meridian.v1.StatementRecordedEvent",
                    event.encode_to_vec(),
                    meta.map(|meta| meta.correlation_id.as_str()),
                    meta.map(|meta| meta.message_id.as_str()),
                )
                .map_err(|failed| failed.to_string())?;
        }

        Ok((
            "meridian.v1.RecordHoldingsStatementReply".to_string(),
            opening.reply.encode_to_vec(),
        ))
    });

    let holdings = store.clone();
    let holding_clock = clock.clone();
    let announcing = bus.clone();
    bus.serve(RECORD_HOLDING, move |envelope| {
        expect(&envelope.payload_type, "meridian.v1.RecordHoldingRequest")?;

        let request = RecordHoldingRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable holding: {failed}"))?;

        let recorded = record_holding(holdings.as_ref(), &request, holding_clock.now_ns())
            .map_err(|failed| failed.to_string())?;

        let meta = envelope.meta.as_ref();
        let correlation = meta.map(|meta| meta.correlation_id.as_str());
        let causation = meta.map(|meta| meta.message_id.as_str());

        if let Some(event) = recorded.event {
            announcing
                .publish(
                    POSITION_UPDATED,
                    "meridian.v1.PositionUpdatedEvent",
                    event.encode_to_vec(),
                    correlation,
                    causation,
                )
                .map_err(|failed| failed.to_string())?;
        }

        // W2.5, on the row that completed the statement and no other.
        if let Some(event) = recorded.completed {
            announcing
                .publish(
                    STATEMENT_RECORDED,
                    "meridian.v1.StatementRecordedEvent",
                    event.encode_to_vec(),
                    correlation,
                    causation,
                )
                .map_err(|failed| failed.to_string())?;
        }

        Ok((
            "meridian.v1.RecordHoldingReply".to_string(),
            recorded.reply.encode_to_vec(),
        ))
    });

    let reading = store;
    bus.serve(LIST_POSITIONS, move |envelope| {
        expect(&envelope.payload_type, "meridian.v1.ListPositionsRequest")?;

        let request = ListPositionsRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable query: {failed}"))?;

        let reply =
            list_positions(reading.as_ref(), &request).map_err(|failed| failed.to_string())?;

        Ok((
            "meridian.v1.ListPositionsReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}

/// Refuse a payload arriving under a type name that is not the one served.
///
/// Protobuf will read one message as another and hand back defaults. A
/// defaulted holding is a row with no account, no instrument and a quantity of
/// zero, which the store would refuse, and a defaulted query is a request for
/// everything.
fn expect(arrived: &str, wanted: &str) -> Result<(), String> {
    if arrived == wanted {
        return Ok(());
    }
    Err(format!("expected {wanted}, got {arrived}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use meridian_bus::{MemoryBackend, Subscription};
    use meridian_pb::v1::{
        Identifier as PbIdentifier, ListPositionsReply, PositionUpdatedEvent, RecordHoldingReply,
        RecordHoldingsStatementReply, StatementRecordedEvent,
    };

    use super::*;
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    struct Stopped(AtomicI64);

    impl Clock for Stopped {
        fn now_ns(&self) -> i64 {
            // Moves on every read, so two identifiers minted in one test differ
            // for the same reason they would in production.
            self.0.fetch_add(1_000_000, Ordering::SeqCst)
        }
    }

    fn wired() -> (Arc<Bus>, Arc<MemoryStore>) {
        let bus = Arc::new(Bus::single("kernel-1", Arc::new(MemoryBackend::new())));
        let store = Arc::new(MemoryStore::new());
        serve(
            bus.clone(),
            store.clone(),
            Arc::new(Stopped(AtomicI64::new(NOW))),
        );
        (bus, store)
    }

    async fn next(subscription: &mut Subscription) -> meridian_bus::Delivery {
        tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .expect("nothing was published within five seconds")
            .expect("the bus shut down")
    }

    async fn open(bus: &Bus) -> String {
        let (_, payload) = bus
            .call(
                RECORD_STATEMENT,
                "meridian.v1.RecordHoldingsStatementRequest",
                RecordHoldingsStatementRequest {
                    source: "snaptrade".into(),
                    external_statement_id: "st-2026-09-08-SNAP-ACC-1".into(),
                    as_of_date: "2026-09-08".into(),
                    read_at_ns: NOW,
                    expected_rows: 4,
                }
                .encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();

        RecordHoldingsStatementReply::decode(&payload[..])
            .unwrap()
            .statement_id
    }

    fn row(statement_id: &str) -> RecordHoldingRequest {
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

    async fn record(bus: &Bus, request: RecordHoldingRequest) -> RecordHoldingReply {
        let (_, payload) = bus
            .call(
                RECORD_HOLDING,
                "meridian.v1.RecordHoldingRequest",
                request.encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();
        RecordHoldingReply::decode(&payload[..]).unwrap()
    }

    #[tokio::test]
    async fn a_statement_and_its_row_arrive_over_the_bus() {
        let (bus, store) = wired();
        let statement_id = open(&bus).await;

        let reply = record(&bus, row(&statement_id)).await;
        assert!(reply.resolved);

        let position = store
            .position("SNAP-ACC-1", "INS-01J8XQ4M7K0000000000AAPL")
            .unwrap()
            .unwrap();
        assert_eq!(position.quantity.scaled(), 1_250_000_000);
    }

    #[tokio::test]
    async fn a_moved_position_is_announced() {
        let (bus, _) = wired();
        let mut announced = bus.subscribe(POSITION_UPDATED);
        let statement_id = open(&bus).await;

        record(&bus, row(&statement_id)).await;

        let delivered = next(&mut announced).await;
        assert_eq!(
            delivered.envelope.payload_type,
            "meridian.v1.PositionUpdatedEvent"
        );
        let event = PositionUpdatedEvent::decode(&delivered.envelope.payload[..]).unwrap();
        assert_eq!(event.statement_id, statement_id);
        assert_eq!(event.previous_quantity_scaled_1e8, 0);
    }

    #[tokio::test]
    async fn an_unresolved_row_announces_nothing() {
        // The fixture's postcondition, over the wire this time.
        let (bus, _) = wired();
        let mut announced = bus.subscribe(POSITION_UPDATED);
        let statement_id = open(&bus).await;

        let mut unresolved = row(&statement_id);
        unresolved.instrument_id = String::new();
        unresolved.unresolved_identifiers = vec![PbIdentifier {
            scheme: "symbol".into(),
            value: "ZZTOP".into(),
            source: "snaptrade".into(),
        }];

        assert!(!record(&bus, unresolved).await.resolved);

        let quiet =
            tokio::time::timeout(std::time::Duration::from_millis(100), announced.recv()).await;
        assert!(
            quiet.is_err(),
            "an unresolved row published a position update"
        );
    }

    #[tokio::test]
    async fn a_completed_statement_is_announced_over_the_bus() {
        let (bus, _) = wired();
        let mut recorded = bus.subscribe(STATEMENT_RECORDED);
        let statement_id = open(&bus).await;

        for n in 0..3 {
            let mut early = row(&statement_id);
            early.instrument_id = format!("INS-{n}");
            record(&bus, early).await;
        }

        let quiet =
            tokio::time::timeout(std::time::Duration::from_millis(100), recorded.recv()).await;
        assert!(quiet.is_err(), "announced before every row had landed");

        let mut last = row(&statement_id);
        last.instrument_id = "INS-3".into();
        record(&bus, last).await;

        let delivered = next(&mut recorded).await;
        assert_eq!(
            delivered.envelope.payload_type,
            "meridian.v1.StatementRecordedEvent"
        );
        let event = StatementRecordedEvent::decode(&delivered.envelope.payload[..]).unwrap();
        assert_eq!(event.statement_id, statement_id);
        assert_eq!(event.rows_received, 4);
    }

    #[tokio::test]
    async fn an_empty_statement_is_announced_when_it_opens() {
        let (bus, _) = wired();
        let mut recorded = bus.subscribe(STATEMENT_RECORDED);

        let (_, _payload) = bus
            .call(
                RECORD_STATEMENT,
                "meridian.v1.RecordHoldingsStatementRequest",
                RecordHoldingsStatementRequest {
                    source: "snaptrade".into(),
                    external_statement_id: "st-empty".into(),
                    as_of_date: "2026-09-08".into(),
                    read_at_ns: NOW,
                    expected_rows: 0,
                }
                .encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();

        let event = StatementRecordedEvent::decode(&next(&mut recorded).await.envelope.payload[..])
            .unwrap();
        assert_eq!(event.rows_received, 0);
    }

    #[tokio::test]
    async fn the_query_answers_from_the_ledger() {
        let (bus, _) = wired();
        let statement_id = open(&bus).await;
        record(&bus, row(&statement_id)).await;

        let (payload_type, payload) = bus
            .call(
                LIST_POSITIONS,
                "meridian.v1.ListPositionsRequest",
                ListPositionsRequest {
                    account_id: "SNAP-ACC-1".into(),
                    include_unresolved: true,
                    page_size: 100,
                    cursor: String::new(),
                }
                .encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(payload_type, "meridian.v1.ListPositionsReply");
        let reply = ListPositionsReply::decode(&payload[..]).unwrap();
        assert_eq!(reply.positions.len(), 1);
    }

    #[tokio::test]
    async fn a_payload_under_the_wrong_type_name_is_refused_rather_than_decoded() {
        // A defaulted holding is a row with no account and no instrument, which
        // the store refuses; a defaulted query is a request for everything.
        let (bus, _) = wired();

        let failed = bus
            .call(
                RECORD_HOLDING,
                "meridian.v1.ListPositionsRequest",
                ListPositionsRequest::default().encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(
            failed
                .to_string()
                .contains("expected meridian.v1.RecordHoldingRequest"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn a_row_for_an_unopened_statement_is_refused_over_the_bus_too() {
        let (bus, _) = wired();
        let failed = bus
            .call(
                RECORD_HOLDING,
                "meridian.v1.RecordHoldingRequest",
                row("STMT-nobody-opened").encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(failed.to_string().contains("no statement"), "{failed}");
    }
}
