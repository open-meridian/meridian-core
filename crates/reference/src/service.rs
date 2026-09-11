//! Where the replica meets the bus.
//!
//! Two questions answered from what is held, and one event reacted to. That is
//! the whole surface: W3.1 and W3.6 are calls, W3.2 arrives as an event, and
//! W3.5 leaves as one.
//!
//! # The two halves behave differently on purpose
//!
//! A query is answered synchronously from the store and cannot fail because of
//! anything outside this process. It never calls the platform, never waits, and
//! never blocks on a network. That is what makes a platform outage invisible to
//! a connector resolving a holding.
//!
//! The reaction to a miss is the opposite: it talks to the platform, it can
//! fail, and when it fails it does nothing except say so. It does not retry in
//! a loop, does not queue the miss, and does not stop consuming. The connector
//! reports the same miss again on the next statement, which is a better
//! recovery mechanism than anything this could hold in memory, because it
//! survives a restart.
//!
//! # Misses are handled one at a time
//!
//! Deliberately. A miss reaction is throttled and rare, and handling them in
//! order keeps the throttle's behaviour something you can reason about. The
//! cost is that a slow platform lets the subscriber queue fill and deliveries
//! drop, and that cost is affordable here for the same reason: a dropped miss
//! is re-reported by the connector, so at-most-once delivery loses nothing that
//! does not come back.
//!
//! # Nothing here decides what an instrument is
//!
//! A pulled record and a minted one both go through [`apply`], version-gated,
//! and are announced the same way. The authority to create identity stays on
//! the platform, and the authority to decide what the replica holds stays in
//! the version number.

use std::sync::Arc;

use meridian_bus::{Bus, Delivery};
use meridian_pb::v1::{
    InstrumentRecord as PbInstrument, MissingInstrumentDetectedEvent, ResolveIdentifierRequest,
    ResolveInstrumentRequest,
};
use prost::Message;

use crate::apply::apply;
use crate::platform::{Platform, Reaction};
use crate::resolve::{resolve_identifier, resolve_instrument};
use crate::store::{Applied, Store};

/// W3.1. A connector asking what a set of identifiers meant.
pub const RESOLVE_IDENTIFIER: &str = "platform.reference.query.resolve-identifier";

/// W3.6. The kernel asking for a record so a holding can be shown with a name.
pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";

/// W3.2. A connector reporting that a resolution missed.
pub const INSTRUMENT_MISSING: &str = "platform.reference.event.instrument-missing";

/// W3.5. The replica announcing what it did with a record.
pub const INSTRUMENT_APPLIED: &str = "platform.reference.event.instrument-applied";

/// Where the time comes from.
///
/// Injectable because every duration in this crate is a decision -- assertion
/// lifetimes, backoff, the throttle -- and a test that waits for one is a test
/// people stop running.
pub trait Clock: Send + Sync {
    fn now_ns(&self) -> i64;
}

/// The wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos() as i64)
            .unwrap_or_default()
    }
}

/// Register the two local queries on the bus.
///
/// Answered from the store alone. Nothing here reaches the platform, which is
/// the property that keeps a resolve working while the platform is away.
pub fn serve_queries(bus: &Bus, store: Arc<dyn Store>) {
    let by_identifier = store.clone();
    bus.serve(RESOLVE_IDENTIFIER, move |envelope| {
        expect(
            &envelope.payload_type,
            "meridian.v1.ResolveIdentifierRequest",
        )?;

        let request = ResolveIdentifierRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable resolve request: {failed}"))?;

        let reply = resolve_identifier(by_identifier.as_ref(), &request)
            .map_err(|failed| failed.to_string())?;

        Ok((
            "meridian.v1.ResolveIdentifierReply".to_string(),
            reply.encode_to_vec(),
        ))
    });

    let by_id = store;
    bus.serve(RESOLVE_INSTRUMENT, move |envelope| {
        expect(
            &envelope.payload_type,
            "meridian.v1.ResolveInstrumentRequest",
        )?;

        let request = ResolveInstrumentRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable resolve request: {failed}"))?;

        let reply =
            resolve_instrument(by_id.as_ref(), &request).map_err(|failed| failed.to_string())?;

        Ok((
            "meridian.v1.ResolveInstrumentReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}

/// What one delivery came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Handled {
    /// Not a miss event, or not a readable one. Nothing was sent anywhere.
    ///
    /// A payload arriving under the wrong type name is refused rather than
    /// decoded: protobuf will happily read one message as another and hand back
    /// defaults, and defaults here would mean escalating an empty identifier
    /// set to the platform.
    Ignored(String),

    /// The platform answered. What it said, and what the replica did with it.
    Reacted(Reaction, Option<Applied>),

    /// The platform could not be reached, so nothing happened.
    ///
    /// Not an error to anybody upstream. The connector reports the same miss
    /// again on its next statement, and the throttle entry has already been
    /// cleared so that report is acted on.
    PlatformAway(String),
}

/// Consumes misses and reacts to them. W3.2 in; W3.3, W3.4 and W3.5 out.
pub struct Reactor {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    platform: Arc<Platform>,
    clock: Arc<dyn Clock>,
}

impl Reactor {
    pub fn new(
        bus: Arc<Bus>,
        store: Arc<dyn Store>,
        platform: Arc<Platform>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            bus,
            store,
            platform,
            clock,
        }
    }

    /// Consume misses until the bus shuts down.
    ///
    /// Never returns early on a failure. A platform that is away is the case
    /// this loop exists to survive, and a loop that exited on one would need a
    /// person to start it again, which is exactly what the deployment must not
    /// need.
    pub async fn run(self) {
        let misses = self.bus.subscribe(INSTRUMENT_MISSING);
        self.consume(misses).await
    }

    /// The same loop over a subscription the caller made.
    ///
    /// Separate so a caller can be subscribed before anything is published.
    /// Subscribing inside the task that consumes is a race: at-most-once
    /// delivery drops what arrives before the subscriber exists, and the loss
    /// is silent by design.
    pub async fn consume(self, mut misses: meridian_bus::Subscription) {
        while let Some(delivery) = misses.recv().await {
            let handled = self.react(delivery).await;

            match handled {
                Handled::Ignored(why) => tracing::warn!(why, "ignored a delivery"),
                Handled::PlatformAway(why) => {
                    // Expected, not exceptional. Logged so an operator can see
                    // how long it has been going on, not so somebody acts.
                    tracing::info!(why, "the platform is away; the miss will be reported again")
                }
                Handled::Reacted(reaction, applied) => {
                    tracing::debug!(?reaction, ?applied, "reacted to a miss")
                }
            }
        }
    }

    /// React to one delivery.
    ///
    /// Public so a test can drive it without a running loop, and so a caller
    /// that wants its own scheduling is not forced through [`Reactor::run`].
    pub async fn react(&self, delivery: Delivery) -> Handled {
        let envelope = delivery.envelope;
        if envelope.payload_type != "meridian.v1.MissingInstrumentDetectedEvent" {
            return Handled::Ignored(format!("payload type {}", envelope.payload_type));
        }

        let event = match MissingInstrumentDetectedEvent::decode(&envelope.payload[..]) {
            Ok(event) => event,
            Err(failed) => return Handled::Ignored(format!("undecodable miss: {failed}")),
        };

        let now_ns = self.clock.now_ns();
        let reaction = match self.platform.react_to_miss(&event, now_ns).await {
            Ok(reaction) => reaction,
            Err(failed) => return Handled::PlatformAway(failed.to_string()),
        };

        let record = match &reaction {
            Reaction::Pulled(record) | Reaction::Minted(record) => (**record).clone(),
            _ => return Handled::Reacted(reaction, None),
        };

        match self.apply_and_announce(record, &envelope, now_ns) {
            Ok(applied) => Handled::Reacted(reaction, Some(applied)),
            Err(failed) => Handled::Ignored(failed),
        }
    }

    /// W3.5. Write it through the one inbound path, and say what happened.
    ///
    /// The announcement carries the miss's correlation, so the whole arc -- the
    /// resolution that missed, the pull that answered it, the apply that
    /// followed -- reads as one chain rather than three unrelated events.
    fn apply_and_announce(
        &self,
        record: PbInstrument,
        caused_by: &meridian_bus::Envelope,
        now_ns: i64,
    ) -> Result<Applied, String> {
        let outcome =
            apply(self.store.as_ref(), record, now_ns).map_err(|failed| failed.to_string())?;

        let meta = caused_by.meta.as_ref();
        self.bus
            .publish(
                INSTRUMENT_APPLIED,
                "meridian.v1.InstrumentAppliedEvent",
                outcome.event.encode_to_vec(),
                meta.map(|meta| meta.correlation_id.as_str()),
                meta.map(|meta| meta.message_id.as_str()),
            )
            .map_err(|failed| failed.to_string())?;

        Ok(outcome.applied)
    }
}

/// Refuse a payload arriving under a type name that is not the one served.
fn expect(arrived: &str, wanted: &str) -> Result<(), String> {
    if arrived == wanted {
        return Ok(());
    }
    Err(format!("expected {wanted}, got {arrived}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use meridian_bus::{Envelope, MemoryBackend, MessageMeta};
    use meridian_pb::v1::{
        Identifier as PbIdentifier, InstrumentAppliedEvent, MissReason, ResolveIdentifierReply,
        ResolveInstrumentReply,
    };

    use super::*;
    use crate::platform::tests::{failure, platform as platform_with, record_json, Fake, AS_OF};
    use crate::store::Identifier;
    use crate::{Instrument, MemoryStore};

    const NOW: i64 = 1_757_376_000_000_000_000;

    /// A clock a test moves by hand.
    struct Stopped(AtomicI64);

    impl Stopped {
        fn at(now_ns: i64) -> Arc<Self> {
            Arc::new(Self(AtomicI64::new(now_ns)))
        }

        fn advance(&self, by_ns: i64) {
            self.0.fetch_add(by_ns, Ordering::SeqCst);
        }
    }

    impl Clock for Stopped {
        fn now_ns(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn bus() -> Arc<Bus> {
        Arc::new(Bus::single("reference-1", Arc::new(MemoryBackend::new())))
    }

    fn held() -> Instrument {
        Instrument {
            instrument_id: "INS-HELD".into(),
            identifiers: vec![Identifier {
                scheme: "figi".into(),
                value: "BBG000B9XRY4".into(),
                source: String::new(),
                valid_from_ns: AS_OF - 1,
                valid_to_ns: None,
            }],
            asset_class: "EQUITY".into(),
            currency: "USD".into(),
            exchange_mic: "XNAS".into(),
            description: "Apple Inc. common stock".into(),
            lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
            version: 1,
            valid_from_ns: AS_OF - 1,
            record_time_ns: AS_OF - 1,
        }
    }

    fn miss_event() -> MissingInstrumentDetectedEvent {
        MissingInstrumentDetectedEvent {
            source: "snaptrade".into(),
            asset_class: "EQUITY".into(),
            identifiers: vec![PbIdentifier {
                scheme: "figi".into(),
                value: "BBG000ZZTOP1".into(),
                source: String::new(),
            }],
            as_of_ns: AS_OF,
            publisher_instance_id: "custody-snaptrade-1".into(),
            reason: MissReason::NotFound as i32,
            observed_at_ns: NOW,
        }
    }

    /// The next delivery, or a failure rather than a hang.
    ///
    /// A bare `recv().await` on a message that never arrives waits forever, and
    /// a test suite that hangs is one nobody waits for twice.
    async fn next(subscription: &mut meridian_bus::Subscription) -> Delivery {
        tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .expect("nothing was published within five seconds")
            .expect("the bus shut down")
    }

    /// A delivery as the bus would make one.
    fn delivery(payload_type: &str, payload: Vec<u8>, correlation: &str) -> Delivery {
        Delivery {
            envelope: Envelope {
                meta: Some(MessageMeta {
                    message_id: "MSG-MISS".into(),
                    correlation_id: correlation.into(),
                    causation_id: String::new(),
                    publisher_instance_id: "custody-snaptrade-1".into(),
                    topic: INSTRUMENT_MISSING.into(),
                    schema_version: "v1".into(),
                    published_at_ns: NOW,
                }),
                payload_type: payload_type.into(),
                payload,
            },
            sequence: 1,
        }
    }

    fn reactor(
        bus: Arc<Bus>,
        store: Arc<dyn Store>,
        transport: Arc<Fake>,
        clock: Arc<Stopped>,
    ) -> Reactor {
        Reactor::new(bus, store, Arc::new(platform_with(transport)), clock)
    }

    #[tokio::test]
    async fn a_resolve_is_answered_from_the_store() {
        let bus = bus();
        let store = Arc::new(MemoryStore::new());
        store.apply(held()).unwrap();
        serve_queries(&bus, store);

        let request = ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "figi".into(),
                value: "BBG000B9XRY4".into(),
                source: String::new(),
            }],
            as_of_ns: AS_OF,
            exchange_mic: String::new(),
            currency: String::new(),
        };

        let (payload_type, payload) = bus
            .call(
                RESOLVE_IDENTIFIER,
                "meridian.v1.ResolveIdentifierRequest",
                request.encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(payload_type, "meridian.v1.ResolveIdentifierReply");
        let reply = ResolveIdentifierReply::decode(&payload[..]).unwrap();
        assert!(reply.found);
        assert_eq!(reply.instrument_id, "INS-HELD");
    }

    #[tokio::test]
    async fn a_forward_resolve_is_answered_from_the_store() {
        let bus = bus();
        let store = Arc::new(MemoryStore::new());
        store.apply(held()).unwrap();
        serve_queries(&bus, store);

        let (_, payload) = bus
            .call(
                RESOLVE_INSTRUMENT,
                "meridian.v1.ResolveInstrumentRequest",
                ResolveInstrumentRequest {
                    instrument_id: "INS-HELD".into(),
                    as_of_ns: AS_OF,
                }
                .encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();

        let reply = ResolveInstrumentReply::decode(&payload[..]).unwrap();
        assert!(reply.found);
        assert_eq!(
            reply.instrument.unwrap().description,
            "Apple Inc. common stock"
        );
    }

    #[tokio::test]
    async fn a_payload_under_the_wrong_type_name_is_refused_rather_than_decoded() {
        // Protobuf will read one message as another and hand back defaults. A
        // defaulted resolve request is an empty identifier set, which would
        // answer "not found" to a question nobody asked.
        let bus = bus();
        serve_queries(&bus, Arc::new(MemoryStore::new()));

        let failed = bus
            .call(
                RESOLVE_IDENTIFIER,
                "meridian.v1.ResolveInstrumentRequest",
                ResolveInstrumentRequest::default().encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap_err();

        assert!(
            failed
                .to_string()
                .contains("expected meridian.v1.ResolveIdentifierRequest"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn a_miss_is_pulled_applied_and_announced() {
        let bus = bus();
        let store = Arc::new(MemoryStore::new());
        let transport = Fake::new(vec![Ok(crate::platform::tests::reply(
            200,
            &record_json("INS-PULLED"),
        ))]);
        let mut announced = bus.subscribe(INSTRUMENT_APPLIED);

        let reactor = reactor(bus.clone(), store.clone(), transport, Stopped::at(NOW));
        let handled = reactor
            .react(delivery(
                "meridian.v1.MissingInstrumentDetectedEvent",
                miss_event().encode_to_vec(),
                "COR-1",
            ))
            .await;

        assert!(matches!(
            handled,
            Handled::Reacted(Reaction::Pulled(_), Some(Applied::Stored))
        ));
        assert_eq!(store.version_of("INS-PULLED").unwrap(), Some(4));

        let delivered = next(&mut announced).await;
        assert_eq!(
            delivered.envelope.payload_type,
            "meridian.v1.InstrumentAppliedEvent"
        );
        let event = InstrumentAppliedEvent::decode(&delivered.envelope.payload[..]).unwrap();
        assert!(event.applied);
        assert_eq!(event.instrument.unwrap().instrument_id, "INS-PULLED");
    }

    #[tokio::test]
    async fn the_announcement_is_caused_by_the_miss_that_prompted_it() {
        // So the resolution that missed, the pull that answered it and the
        // apply that followed read as one chain rather than three unrelated
        // events.
        let bus = bus();
        let transport = Fake::new(vec![Ok(crate::platform::tests::reply(
            200,
            &record_json("INS-PULLED"),
        ))]);
        let mut announced = bus.subscribe(INSTRUMENT_APPLIED);

        reactor(
            bus.clone(),
            Arc::new(MemoryStore::new()),
            transport,
            Stopped::at(NOW),
        )
        .react(delivery(
            "meridian.v1.MissingInstrumentDetectedEvent",
            miss_event().encode_to_vec(),
            "COR-1",
        ))
        .await;

        let meta = next(&mut announced).await.envelope.meta.unwrap();
        assert_eq!(meta.correlation_id, "COR-1");
        assert_eq!(meta.causation_id, "MSG-MISS");
    }

    #[tokio::test(start_paused = true)]
    async fn a_platform_outage_stops_the_reaction_and_nothing_else() {
        let bus = bus();
        let store = Arc::new(MemoryStore::new());
        store.apply(held()).unwrap();
        serve_queries(&bus, store.clone());

        let handled = reactor(
            bus.clone(),
            store.clone(),
            Fake::new(vec![failure("connection reset")]),
            Stopped::at(NOW),
        )
        .react(delivery(
            "meridian.v1.MissingInstrumentDetectedEvent",
            miss_event().encode_to_vec(),
            "COR-1",
        ))
        .await;

        assert!(matches!(handled, Handled::PlatformAway(_)));
        assert_eq!(store.count().unwrap(), 1);

        // And the query surface never noticed.
        let (_, payload) = bus
            .call(
                RESOLVE_INSTRUMENT,
                "meridian.v1.ResolveInstrumentRequest",
                ResolveInstrumentRequest {
                    instrument_id: "INS-HELD".into(),
                    as_of_ns: AS_OF,
                }
                .encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(ResolveInstrumentReply::decode(&payload[..]).unwrap().found);
    }

    #[tokio::test]
    async fn a_delivery_that_is_not_a_miss_is_ignored() {
        let handled = reactor(
            bus(),
            Arc::new(MemoryStore::new()),
            Fake::new(vec![]),
            Stopped::at(NOW),
        )
        .react(delivery("meridian.v1.SyncStatusEvent", vec![], "COR-1"))
        .await;

        match handled {
            Handled::Ignored(why) => assert!(why.contains("SyncStatusEvent"), "{why}"),
            other => panic!("expected the delivery to be ignored, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreadable_miss_is_ignored_rather_than_acted_on() {
        let handled = reactor(
            bus(),
            Arc::new(MemoryStore::new()),
            Fake::new(vec![]),
            Stopped::at(NOW),
        )
        .react(delivery(
            "meridian.v1.MissingInstrumentDetectedEvent",
            vec![0xff, 0xff, 0xff],
            "COR-1",
        ))
        .await;

        assert!(matches!(handled, Handled::Ignored(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_survives_an_outage_and_resumes_with_nothing_restarted() {
        // The requirement the deployment is built to: take the platform away,
        // put it back, and nobody intervenes.
        let bus = bus();
        let store = Arc::new(MemoryStore::new());
        let transport = Fake::new(vec![
            failure("connection reset"),
            failure("connection reset"),
            failure("connection reset"),
            Ok(crate::platform::tests::reply(
                200,
                &record_json("INS-PULLED"),
            )),
        ]);
        let clock = Stopped::at(NOW);
        let reactor = reactor(bus.clone(), store.clone(), transport.clone(), clock.clone());

        let miss = delivery(
            "meridian.v1.MissingInstrumentDetectedEvent",
            miss_event().encode_to_vec(),
            "COR-1",
        );

        // Away: three attempts, no record, and the throttle cleared so the next
        // report is acted on rather than skipped.
        assert!(matches!(
            reactor.react(miss.clone()).await,
            Handled::PlatformAway(_)
        ));
        assert_eq!(store.count().unwrap(), 0);

        // Back. Same process, same configuration, no restart.
        clock.advance(1_000_000_000);
        assert!(matches!(
            reactor.react(miss).await,
            Handled::Reacted(Reaction::Pulled(_), Some(Applied::Stored))
        ));
        assert_eq!(store.version_of("INS-PULLED").unwrap(), Some(4));
        assert_eq!(transport.calls(), 4);
    }

    #[tokio::test]
    async fn a_wired_replica_answers_and_reacts() {
        // Everything this crate does, through the surface a running deployment
        // actually uses.
        let bus = bus();
        let store = Arc::new(MemoryStore::new());
        store.apply(held()).unwrap();
        let transport = Fake::new(vec![Ok(crate::platform::tests::reply(
            200,
            &record_json("INS-PULLED"),
        ))]);
        let mut announced = bus.subscribe(INSTRUMENT_APPLIED);

        // `start` registers before it returns, so there is no window in which
        // the replica is running and answers nothing.
        let running = tokio::spawn(
            crate::Replica::new(
                bus.clone(),
                store.clone(),
                Arc::new(platform_with(transport)),
                Stopped::at(NOW),
            )
            .start(),
        );

        // It answers from what it holds.
        let (_, payload) = bus
            .call(
                RESOLVE_INSTRUMENT,
                "meridian.v1.ResolveInstrumentRequest",
                ResolveInstrumentRequest {
                    instrument_id: "INS-HELD".into(),
                    as_of_ns: AS_OF,
                }
                .encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(ResolveInstrumentReply::decode(&payload[..]).unwrap().found);

        // And it reacts to what it does not.
        bus.publish(
            INSTRUMENT_MISSING,
            "meridian.v1.MissingInstrumentDetectedEvent",
            miss_event().encode_to_vec(),
            None,
            None,
        )
        .unwrap();

        next(&mut announced).await;
        assert_eq!(store.version_of("INS-PULLED").unwrap(), Some(4));

        running.abort();
    }

    #[tokio::test]
    async fn the_reaction_loop_consumes_what_the_bus_delivers() {
        let bus = bus();
        let store = Arc::new(MemoryStore::new());
        let transport = Fake::new(vec![Ok(crate::platform::tests::reply(
            200,
            &record_json("INS-PULLED"),
        ))]);
        let mut announced = bus.subscribe(INSTRUMENT_APPLIED);

        let misses = bus.subscribe(INSTRUMENT_MISSING);
        let running = tokio::spawn(
            reactor(bus.clone(), store.clone(), transport, Stopped::at(NOW)).consume(misses),
        );

        bus.publish(
            INSTRUMENT_MISSING,
            "meridian.v1.MissingInstrumentDetectedEvent",
            miss_event().encode_to_vec(),
            None,
            None,
        )
        .unwrap();

        let delivered = next(&mut announced).await;
        assert_eq!(
            delivered.envelope.payload_type,
            "meridian.v1.InstrumentAppliedEvent"
        );
        assert_eq!(store.version_of("INS-PULLED").unwrap(), Some(4));

        running.abort();
    }
}
