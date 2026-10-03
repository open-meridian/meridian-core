//! Where the street store meets the bus.
//!
//! Two commands in, two queries in, one event heard, one query asked, two
//! events out. W2.2 and W2.3 arrive as commands from a connector, W2.7 and
//! W2.9 as queries from the dashboard or an `operations` plugin, a
//! placeholder's replacement (W3.8) as an event from the instrument store,
//! W3.6 is asked of the instrument store by the sweep below, W2.5 leaves when
//! a statement completes, and W2.6 whenever a position actually moved,
//! whether a statement moved it or a replacement did (W3.9).
//!
//! # Who caused it, and whose read it is
//!
//! Every change is recorded with its cause (Q3), read here off the command's
//! envelope: the instance that sent it, which its sidecar stamped as the
//! publisher, the person it was sent for, its correlation and its own
//! identifier. A query's envelope says whose read it is: a plugin's, its
//! scope marked as applying by its sidecar, answered only within it; or a
//! core component's, unmarked, answered for every account (W4.11).
//!
//! # A replacement is heard, and also asked about
//!
//! The replacement event is said once, and delivery is at most once. A street
//! store that was restarting when it was said would keep its positions under
//! the placeholder for good, and the connector's next statement, resolving to
//! the `INS-` ID, would add a second position beside them: one security held
//! twice. So the store also sweeps, at start and on an interval, asking the
//! instrument store what each placeholder it still holds has become (W3.6
//! answers a replaced placeholder with its replacement's record), and moves
//! what it finds exactly as the event would have.
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
use std::time::Duration;

use meridian_bus::{Bus, Delivery, Envelope};
use meridian_domain::v1::{
    InstrumentReplacedEvent, ListCustodialPositionsRequest, ListStatementsRequest,
    RecordHoldingRequest, RecordHoldingsStatementRequest, ResolveInstrumentReply,
    ResolveInstrumentRequest,
};
use prost::Message;

use crate::positions::{list_positions, list_statements};
use crate::record::{move_positions, open_statement, record_holding};
use crate::store::{Cause, Scope, Store};

/// W2.2. A connector opening a statement.
pub const RECORD_STATEMENT: &str = "platform.street.command.record-statement";

/// W2.3. A connector publishing one row.
pub const RECORD_HOLDING: &str = "platform.street.command.record-holding";

/// W2.6. A position moved.
pub const CUSTODIAL_POSITION_UPDATED: &str = "platform.street.event.custodial-position-updated";

/// W2.5. A statement has every row it said was coming.
pub const STATEMENT_RECORDED: &str = "platform.street.event.statement-recorded";

/// W2.7. The dashboard, or an `operations` plugin, asking what is held.
pub const LIST_CUSTODIAL_POSITIONS: &str = "platform.street.query.list-custodial-positions";

/// W2.9. An `operations` plugin asking for completed statements.
pub const LIST_STATEMENTS: &str = "platform.street.query.list-statements";

/// W3.8, heard. A placeholder's `INS-` ID arrived, and what was held under the
/// placeholder moves onto it (W3.9).
pub const INSTRUMENT_REPLACED: &str = "platform.reference.event.instrument-replaced";

/// W3.6, asked. What a placeholder the store still holds has become.
pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";

/// How often the placeholders still held are asked about again.
///
/// The same interval the instrument store announces them on, and for the same
/// reason: this is the recovery path for an event that did not arrive, not
/// the path a replacement normally takes, so it can be slow and must be
/// certain.
pub const SWEEP_EVERY: Duration = Duration::from_secs(15 * 60);

/// Where the time comes from: the deployment's one clock (decisions/024),
/// given by whoever wires this up, so a test does not wait for it.
pub use meridian_clock::Clock;

/// Who caused a change, read off the envelope of what caused it (Q3): the
/// sender, stamped by the bus as the publisher -- a plugin's sidecar is on
/// the bus as its plugin -- the person, the chain and the message.
pub fn cause_of(envelope: &Envelope, committed_at_ns: i64) -> Cause {
    let meta = envelope.meta.clone().unwrap_or_default();
    Cause {
        instance_id: meta.publisher_instance_id,
        acting_for_subject: meta.acting_for_subject,
        correlation_id: meta.correlation_id,
        causation_id: meta.message_id,
        committed_at_ns,
    }
}

/// Whose read a query is: a plugin's, which its sidecar marked as scoped, or
/// a core component's (W4.11).
pub fn scope_of(envelope: &Envelope) -> Scope {
    match envelope.meta.as_ref() {
        Some(meta) if meta.account_scope_applies => {
            Scope::Within(meta.account_scope.iter().cloned().collect())
        }
        _ => Scope::Everything,
    }
}

/// Register every handler the street store serves.
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

        let cause = cause_of(&envelope, statement_clock.now_ns());
        let opening = open_statement(statements.as_ref(), &request, &cause)
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

        let cause = cause_of(&envelope, holding_clock.now_ns());
        let recorded = record_holding(holdings.as_ref(), &request, &cause)
            .map_err(|failed| failed.to_string())?;

        let meta = envelope.meta.as_ref();
        let correlation = meta.map(|meta| meta.correlation_id.as_str());
        let causation = meta.map(|meta| meta.message_id.as_str());

        if let Some(event) = recorded.event {
            announcing
                .publish(
                    CUSTODIAL_POSITION_UPDATED,
                    "meridian.v1.CustodialPositionUpdatedEvent",
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

    let reading = store.clone();
    bus.serve(LIST_CUSTODIAL_POSITIONS, move |envelope| {
        expect(
            &envelope.payload_type,
            "meridian.v1.ListCustodialPositionsRequest",
        )?;

        let request = ListCustodialPositionsRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable query: {failed}"))?;

        let reply = list_positions(reading.as_ref(), &request, &scope_of(&envelope))
            .map_err(|failed| failed.to_string())?;

        Ok((
            "meridian.v1.ListCustodialPositionsReply".to_string(),
            reply.encode_to_vec(),
        ))
    });

    let completed = store;
    bus.serve(LIST_STATEMENTS, move |envelope| {
        expect(&envelope.payload_type, "meridian.v1.ListStatementsRequest")?;

        let request = ListStatementsRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable query: {failed}"))?;

        let reply = list_statements(completed.as_ref(), &request, &scope_of(&envelope))
            .map_err(|failed| failed.to_string())?;

        Ok((
            "meridian.v1.ListStatementsReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}

/// Subscribe to replacements, and hand back the loop that moves positions onto
/// them. W3.9.
///
/// Not an `async fn`, for the reason the instrument store's `start` is not:
/// the subscription is taken before this returns, because at-most-once
/// delivery drops what arrives before a subscriber exists, and silently.
///
/// Ends when the bus shuts down, and on nothing else: a replacement that could
/// not be applied is one replacement, and a loop that exited on one would need
/// a person to start it again.
pub fn follow_replacements(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
) -> impl std::future::Future<Output = ()> {
    let mut replaced = bus.subscribe(INSTRUMENT_REPLACED);

    async move {
        while let Some(delivery) = replaced.recv().await {
            match move_onto_replacement(&bus, &store, delivery).await {
                Ok(announced) => tracing::debug!(announced, "moved positions onto a replacement"),
                Err(why) => tracing::warn!(why, "could not move positions onto a replacement"),
            }
        }
    }
}

/// One replacement: move what the placeholder held, and announce each position
/// that now stands under the instrument because of it. Says how many were
/// announced.
///
/// Public so a test can drive it without a running loop.
pub async fn move_onto_replacement(
    bus: &Bus,
    store: &Arc<dyn Store>,
    delivery: Delivery,
) -> Result<usize, String> {
    let envelope = delivery.envelope;
    expect(
        &envelope.payload_type,
        "meridian.v1.InstrumentReplacedEvent",
    )?;

    let event = InstrumentReplacedEvent::decode(&envelope.payload[..])
        .map_err(|failed| format!("undecodable replacement: {failed}"))?;

    // In the replacement's chain, so a position's move reads back to the
    // placeholder's announcement and the escalation that answered it; and
    // caused by it, as the store records it.
    let cause = cause_of(&envelope, bus.clock().now_ns());
    let meta = envelope.meta.as_ref();
    move_and_announce(
        bus,
        store,
        event,
        &cause,
        meta.map(|meta| meta.correlation_id.as_str()),
        meta.map(|meta| meta.message_id.as_str()),
    )
    .await
}

/// Ask the instrument store what every instrument held has become, and move
/// the positions under each one replaced by a merge. Says how many positions
/// were announced.
///
/// Moves exactly as [`move_onto_replacement`] does, through the same path, so
/// a sweep that finds what an event already moved finds nothing, and one that
/// finds what an event missed does what the event would have. A record not
/// replaced answers itself and is left alone.
///
/// Stops at the first question that cannot be asked: an instrument store that
/// is away is away for all of them, and the next sweep asks again.
pub async fn sweep_replacements(
    bus: &Bus,
    store: &Arc<dyn Store>,
    clock: &dyn Clock,
) -> Result<usize, String> {
    let listing = Arc::clone(store);
    let placeholders = tokio::task::spawn_blocking(move || listing.instruments_held())
        .await
        .map_err(|failed| format!("the sweep task failed: {failed}"))?
        .map_err(|failed| failed.to_string())?;

    let mut announced = 0;
    for placeholder in placeholders {
        let now_ns = clock.now_ns();
        let (payload_type, payload) = bus
            .call(
                RESOLVE_INSTRUMENT,
                "meridian.v1.ResolveInstrumentRequest",
                ResolveInstrumentRequest {
                    instrument_id: placeholder.clone(),
                    as_of_ns: now_ns,
                }
                .encode_to_vec(),
                None,
                None,
            )
            .await
            .map_err(|failed| format!("could not ask about {placeholder}: {failed}"))?;
        expect(&payload_type, "meridian.v1.ResolveInstrumentReply")?;

        let reply = ResolveInstrumentReply::decode(&payload[..])
            .map_err(|failed| format!("undecodable answer about {placeholder}: {failed}"))?;
        let Some(instrument) = reply.instrument.filter(|_| reply.found) else {
            continue;
        };
        if instrument.instrument_id == placeholder {
            continue;
        }

        // A chain of its own: nothing caused it but the time, and the store
        // itself made the change.
        let cause = Cause {
            instance_id: bus.instance_id().to_string(),
            committed_at_ns: now_ns,
            ..Default::default()
        };
        announced += move_and_announce(
            bus,
            store,
            InstrumentReplacedEvent {
                replaced_instrument_id: placeholder,
                instrument: Some(instrument),
                replaced_at_ns: now_ns,
            },
            &cause,
            None,
            None,
        )
        .await?;
    }
    Ok(announced)
}

/// Sweep now, and then every `every`, for as long as the process runs.
///
/// Never returns and never gives up: a failed sweep is logged and the next
/// one tries again.
pub async fn sweep_forever(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    every: Duration,
) {
    loop {
        match sweep_replacements(&bus, &store, clock.as_ref()).await {
            Ok(announced) => tracing::debug!(announced, "swept the instruments held for merges"),
            Err(why) => tracing::warn!(why, "could not sweep the instruments held for merges"),
        }
        tokio::time::sleep(every).await;
    }
}

/// W3.9's one path, for the event and the sweep alike: move, then announce
/// what now stands under the instrument.
async fn move_and_announce(
    bus: &Bus,
    store: &Arc<dyn Store>,
    event: InstrumentReplacedEvent,
    cause: &Cause,
    correlation: Option<&str>,
    causation: Option<&str>,
) -> Result<usize, String> {
    // Off the runtime, because the store is synchronous and the Postgres one
    // blocks on a socket; calling it here aborts the process rather than
    // merely blocking a worker. The instrument store learned that first.
    let moving = Arc::clone(store);
    let cause = cause.clone();
    let announcements =
        tokio::task::spawn_blocking(move || move_positions(moving.as_ref(), &event, &cause))
            .await
            .map_err(|failed| format!("the move task failed: {failed}"))?
            .map_err(|failed| failed.to_string())?;

    for announcement in &announcements {
        bus.publish(
            CUSTODIAL_POSITION_UPDATED,
            "meridian.v1.CustodialPositionUpdatedEvent",
            announcement.encode_to_vec(),
            correlation,
            causation,
        )
        .map_err(|failed| failed.to_string())?;
    }
    Ok(announcements.len())
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
    use meridian_domain::v1::{
        CustodialPositionUpdatedEvent, HoldingSide, Identifier as PbIdentifier,
        ListCustodialPositionsReply, RecordHoldingReply, RecordHoldingsStatementReply,
        StatementRecordedEvent,
    };

    use super::*;
    use crate::amounts::testing::{quantity, read, usd};
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
        let bus = Arc::new(Bus::single(
            "street-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ));
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
                    ..Default::default()
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
            quantity: quantity("12.5"),
            market_value: usd("2812.5"),
            side: HoldingSide::Long as i32,
            ..Default::default()
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
            .custodial_position(
                "SNAP-ACC-1",
                "INS-01J8XQ4M7K0000000000AAPL",
                crate::Side::Long,
            )
            .unwrap()
            .unwrap();
        assert_eq!(position.quantity.to_string(), "12.5");
    }

    #[tokio::test]
    async fn a_moved_position_is_announced() {
        let (bus, _) = wired();
        let mut announced = bus.subscribe(CUSTODIAL_POSITION_UPDATED);
        let statement_id = open(&bus).await;

        record(&bus, row(&statement_id)).await;

        let delivered = next(&mut announced).await;
        assert_eq!(
            delivered.envelope.payload_type,
            "meridian.v1.CustodialPositionUpdatedEvent"
        );
        let event = CustodialPositionUpdatedEvent::decode(&delivered.envelope.payload[..]).unwrap();
        assert_eq!(event.statement_id, statement_id);
        assert_eq!(read(&event.previous_quantity), "0");
    }

    #[tokio::test]
    async fn an_unresolved_row_announces_nothing() {
        // The fixture's postcondition, over the wire this time.
        let (bus, _) = wired();
        let mut announced = bus.subscribe(CUSTODIAL_POSITION_UPDATED);
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
                    ..Default::default()
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
    async fn the_query_answers_from_the_street() {
        let (bus, _) = wired();
        let statement_id = open(&bus).await;
        record(&bus, row(&statement_id)).await;

        let (payload_type, payload) = bus
            .call(
                LIST_CUSTODIAL_POSITIONS,
                "meridian.v1.ListCustodialPositionsRequest",
                ListCustodialPositionsRequest {
                    account_id: "SNAP-ACC-1".into(),
                    include_unresolved: true,
                    page_size: 100,
                    cursor: String::new(),
                    since: None,
                }
                .encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(payload_type, "meridian.v1.ListCustodialPositionsReply");
        let reply = ListCustodialPositionsReply::decode(&payload[..]).unwrap();
        assert_eq!(reply.positions.len(), 1);
    }

    /// A plugin's read, as its sidecar sends it: the scope stamped and marked.
    async fn read_as_a_plugin(
        bus: &Bus,
        topic: &str,
        payload_type: &str,
        payload: Vec<u8>,
        scope: &[&str],
    ) -> Result<Vec<u8>, meridian_bus::BusError> {
        let stamp = meridian_bus::Stamp {
            acting_for_subject: String::new(),
            account_scope: Some(scope.iter().map(|a| a.to_string()).collect()),
            ..Default::default()
        };
        bus.call_stamped(topic, payload_type, payload, None, None, &stamp)
            .await
            .map(|(_, payload)| payload)
    }

    #[tokio::test]
    async fn a_plugins_read_is_answered_within_its_scope() {
        // W4.11: the store's second line, behind the sidecar's own check.
        let (bus, _) = wired();
        let statement_id = open(&bus).await;
        record(&bus, row(&statement_id)).await;
        let everything = ListCustodialPositionsRequest::default().encode_to_vec();
        let positions = "meridian.v1.ListCustodialPositionsRequest";

        let within = read_as_a_plugin(
            &bus,
            LIST_CUSTODIAL_POSITIONS,
            positions,
            everything.clone(),
            &["SNAP-ACC-1"],
        )
        .await
        .unwrap();
        assert_eq!(
            ListCustodialPositionsReply::decode(&within[..])
                .unwrap()
                .positions
                .len(),
            1
        );

        let none = read_as_a_plugin(&bus, LIST_CUSTODIAL_POSITIONS, positions, everything, &[])
            .await
            .unwrap();
        assert!(
            ListCustodialPositionsReply::decode(&none[..])
                .unwrap()
                .positions
                .is_empty(),
            "an empty scope marked as applying reads nothing"
        );

        let outside = read_as_a_plugin(
            &bus,
            LIST_CUSTODIAL_POSITIONS,
            positions,
            ListCustodialPositionsRequest {
                account_id: "SNAP-ACC-1".into(),
                ..Default::default()
            }
            .encode_to_vec(),
            &["ACC-2"],
        )
        .await
        .unwrap_err();
        assert!(
            outside
                .to_string()
                .contains("not in this plugin's read scope"),
            "{outside}"
        );
    }

    #[tokio::test]
    async fn completed_statements_are_read_with_their_cause() {
        let (bus, _) = wired();
        let mut statement = RecordHoldingsStatementRequest {
            source: "snaptrade".into(),
            external_statement_id: "st-empty".into(),
            as_of_date: "2026-09-08".into(),
            expected_rows: 0,
            account_id: "ACC-1".into(),
            external_account_id: "SNAP-ACC-1".into(),
            institution: "Interactive Brokers".into(),
            ..Default::default()
        };
        bus.call(
            RECORD_STATEMENT,
            "meridian.v1.RecordHoldingsStatementRequest",
            statement.encode_to_vec(),
            Some("CORR-STATEMENT"),
            None,
        )
        .await
        .unwrap();
        statement.external_statement_id = "st-open".into();
        statement.expected_rows = 3;
        open(&bus).await;

        let read = read_as_a_plugin(
            &bus,
            LIST_STATEMENTS,
            "meridian.v1.ListStatementsRequest",
            ListStatementsRequest::default().encode_to_vec(),
            &["ACC-1"],
        )
        .await
        .unwrap();
        let reply = meridian_domain::v1::ListStatementsReply::decode(&read[..]).unwrap();
        let [completed] = reply.statements.as_slice() else {
            panic!("one completed statement in scope: {:?}", reply.statements)
        };
        assert_eq!(completed.external_account_id, "SNAP-ACC-1");
        assert_eq!(completed.institution, "Interactive Brokers");
        let cause = completed.cause.as_ref().unwrap();
        assert_eq!(
            cause.instance_id, "street-1",
            "the sender, as its bus stamped it"
        );
        assert_eq!(cause.correlation_id, "CORR-STATEMENT");
        assert_eq!(completed.journal.as_ref().unwrap().sequence, 1);
    }

    #[tokio::test]
    async fn a_replacement_moves_the_placeholders_position_and_announces_it() {
        // W3.9, over the bus: the instrument store says the placeholder is
        // replaced, and the position moves onto the INS- ID.
        let (bus, store) = wired();
        let store: Arc<dyn Store> = store;
        let mut announced = bus.subscribe(CUSTODIAL_POSITION_UPDATED);
        tokio::spawn(follow_replacements(bus.clone(), store.clone()));

        let statement_id = open(&bus).await;
        let mut held = row(&statement_id);
        held.instrument_id = "LCL-01J8XQ4M7K0000000000ZZTP".into();
        record(&bus, held).await;
        next(&mut announced).await;

        bus.publish(
            INSTRUMENT_REPLACED,
            "meridian.v1.InstrumentReplacedEvent",
            InstrumentReplacedEvent {
                replaced_instrument_id: "LCL-01J8XQ4M7K0000000000ZZTP".into(),
                instrument: Some(meridian_domain::v1::InstrumentRecord {
                    instrument_id: "INS-01J8XQ4M7K0000000000ZZTP".into(),
                    ..Default::default()
                }),
                replaced_at_ns: NOW,
            }
            .encode_to_vec(),
            Some("CORR-REPLACED"),
            None,
        )
        .unwrap();

        let delivered = next(&mut announced).await;
        assert_eq!(
            delivered.envelope.meta.as_ref().unwrap().correlation_id,
            "CORR-REPLACED"
        );
        let event = CustodialPositionUpdatedEvent::decode(&delivered.envelope.payload[..]).unwrap();
        let position = event.position.unwrap();
        assert_eq!(position.instrument_id, "INS-01J8XQ4M7K0000000000ZZTP");
        assert_eq!(read(&position.quantity), "12.5");
        assert_eq!(event.statement_id, statement_id);

        assert!(store
            .custodial_position(
                "SNAP-ACC-1",
                "LCL-01J8XQ4M7K0000000000ZZTP",
                crate::Side::Long
            )
            .unwrap()
            .is_none());
    }

    /// The instrument store's W3.6, stood in for: every placeholder asked
    /// about has become `answer`, or is still itself when `answer` is `None`.
    /// Counts the questions.
    fn instrument_store(bus: &Bus, answer: Option<&'static str>) -> Arc<AtomicI64> {
        let asked = Arc::new(AtomicI64::new(0));
        let counting = asked.clone();
        bus.serve(RESOLVE_INSTRUMENT, move |envelope| {
            counting.fetch_add(1, Ordering::SeqCst);
            let request = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
            let instrument_id = answer.map(str::to_string).unwrap_or(request.instrument_id);
            Ok((
                "meridian.v1.ResolveInstrumentReply".to_string(),
                ResolveInstrumentReply {
                    found: true,
                    instrument: Some(meridian_domain::v1::InstrumentRecord {
                        instrument_id,
                        ..Default::default()
                    }),
                }
                .encode_to_vec(),
            ))
        });
        asked
    }

    #[tokio::test]
    async fn a_missed_replacement_is_recovered_by_the_sweep() {
        // The event was said while nobody was listening. The sweep asks, and
        // moves the position exactly as the event would have.
        let (bus, store) = wired();
        let store: Arc<dyn Store> = store;
        let mut announced = bus.subscribe(CUSTODIAL_POSITION_UPDATED);
        instrument_store(&bus, Some("INS-01J8XQ4M7K0000000000ZZTP"));

        let statement_id = open(&bus).await;
        let mut held = row(&statement_id);
        held.instrument_id = "LCL-01J8XQ4M7K0000000000ZZTP".into();
        record(&bus, held).await;
        next(&mut announced).await;

        let moved = sweep_replacements(&bus, &store, &Stopped(AtomicI64::new(NOW)))
            .await
            .unwrap();
        assert_eq!(
            moved, 2,
            "what stands under the instrument, and the removal"
        );

        let event =
            CustodialPositionUpdatedEvent::decode(&next(&mut announced).await.envelope.payload[..])
                .unwrap();
        let position = event.position.unwrap();
        assert_eq!(position.instrument_id, "INS-01J8XQ4M7K0000000000ZZTP");
        assert_eq!(event.statement_id, statement_id);
        assert!(store
            .custodial_position(
                "SNAP-ACC-1",
                "LCL-01J8XQ4M7K0000000000ZZTP",
                crate::Side::Long
            )
            .unwrap()
            .is_none());

        // Found once, moved once: the next sweep has nothing to ask about.
        assert_eq!(
            sweep_replacements(&bus, &store, &Stopped(AtomicI64::new(NOW)))
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_sweep_of_records_not_replaced_moves_nothing() {
        let (bus, store) = wired();
        let store: Arc<dyn Store> = store;
        let mut announced = bus.subscribe(CUSTODIAL_POSITION_UPDATED);
        let asked = instrument_store(&bus, None);

        let statement_id = open(&bus).await;
        let mut waiting = row(&statement_id);
        waiting.instrument_id = "LCL-01J8XQ4M7K0000000000ZZTP".into();
        record(&bus, waiting).await;
        record(&bus, row(&statement_id)).await;
        next(&mut announced).await;
        next(&mut announced).await;

        let moved = sweep_replacements(&bus, &store, &Stopped(AtomicI64::new(NOW)))
            .await
            .unwrap();

        assert_eq!(moved, 0);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            2,
            "every instrument held is asked about"
        );
        assert!(store
            .custodial_position(
                "SNAP-ACC-1",
                "LCL-01J8XQ4M7K0000000000ZZTP",
                crate::Side::Long
            )
            .unwrap()
            .is_some());
        let quiet =
            tokio::time::timeout(std::time::Duration::from_millis(100), announced.recv()).await;
        assert!(
            quiet.is_err(),
            "a sweep that moved nothing announced something"
        );
    }

    #[tokio::test]
    async fn a_replacement_under_the_wrong_type_name_moves_nothing() {
        let (bus, store) = wired();
        let store: Arc<dyn Store> = store;

        let refused = move_onto_replacement(
            &bus,
            &store,
            Delivery {
                envelope: meridian_bus::Envelope {
                    meta: None,
                    payload_type: "meridian.v1.InstrumentAppliedEvent".into(),
                    payload: Vec::new(),
                },
                sequence: 1,
            },
        )
        .await
        .unwrap_err();

        assert!(
            refused.contains("expected meridian.v1.InstrumentReplacedEvent"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn a_payload_under_the_wrong_type_name_is_refused_rather_than_decoded() {
        // A defaulted holding is a row with no account and no instrument, which
        // the store refuses; a defaulted query is a request for everything.
        let (bus, _) = wired();

        let failed = bus
            .call(
                RECORD_HOLDING,
                "meridian.v1.ListCustodialPositionsRequest",
                ListCustodialPositionsRequest::default().encode_to_vec(),
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
