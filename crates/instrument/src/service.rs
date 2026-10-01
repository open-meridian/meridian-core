//! Where the instrument store meets the bus.
//!
//! Two questions answered from what is held, and one event reacted to. That is
//! the whole surface: W3.1 and W3.6 are calls, the conductor's pulled record
//! arrives as an event, and W3.5 leaves as one. Around them, W3.7 and W3.8
//! leave as events too: a placeholder announced when a resolve mints it and
//! again until it is replaced, and its replacement once.
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
//! the platform, and the authority to decide what the instrument store holds stays in
//! the version number.

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, Delivery};
use meridian_domain::v1::{
    InstrumentRecord as PbInstrument, PullInstrumentReply, ResolveIdentifierRequest,
    ResolveInstrumentRequest,
};
use prost::Message;

use crate::apply::apply;
use crate::placeholder::{self, announcement};
use crate::resolve::{resolve_identifier, resolve_instrument};
use crate::store::{Applied, Store};

/// W3.1. A connector asking what a set of identifiers meant.
pub const RESOLVE_IDENTIFIER: &str = "platform.reference.query.resolve-identifier";

/// W3.6. The street store asking for a record so a holding can be shown with a name.
pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";

/// W3.2 and W3.7. A connector reporting that a resolution was ambiguous, and
/// this store announcing a placeholder, on one topic: the conductor reacts to
/// both the same way.
pub const INSTRUMENT_MISSING: &str = "platform.reference.event.instrument-missing";

/// W3.3 and W3.4. What the conductor got from the platform, for applying.
///
/// The instrument store subscribes rather than fetching. It holds no key and therefore
/// cannot reach the platform at all, which is the property decision 011 bought:
/// a defect in instrument storage is no longer a defect in the process holding
/// the deployment's identity.
pub const INSTRUMENT_PULLED: &str = "platform.reference.event.instrument-pulled";

/// W3.5. The instrument store announcing what it did with a record.
pub const INSTRUMENT_APPLIED: &str = "platform.reference.event.instrument-applied";

/// W3.8. A placeholder's `INS-` ID arrived, for the stores keyed by instrument
/// to move onto it (W3.9).
pub const INSTRUMENT_REPLACED: &str = "platform.reference.event.instrument-replaced";

/// How often every outstanding placeholder is announced again. W3.7.
///
/// Long beside the conductor's five-minute throttle, so each announcement
/// reaches the platform rather than being skipped, and short beside how long
/// a holding can reasonably wait for its identity after an outage ends. A
/// placeholder announced when it was minted normally needs no second
/// announcement: the interval is the recovery path, not the delivery path.
pub const ANNOUNCE_EVERY: Duration = Duration::from_secs(15 * 60);

/// Where the time comes from: the deployment's one clock (decisions/024),
/// given by whoever wires this up. Injected because every duration in this
/// crate is a decision -- assertion lifetimes, backoff, the throttle -- and a
/// test that waits for one is a test people stop running.
pub use meridian_clock::Clock;

/// Register the two local queries on the bus.
///
/// Answered from the store alone. Nothing here reaches the platform, which is
/// the property that keeps a resolve working while the platform is away.
pub fn serve_queries(bus: &Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    let by_identifier = store.clone();
    let announcing = Arc::clone(bus);
    bus.serve(RESOLVE_IDENTIFIER, move |envelope| {
        expect(
            &envelope.payload_type,
            "meridian.v1.ResolveIdentifierRequest",
        )?;

        let request = ResolveIdentifierRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable resolve request: {failed}"))?;

        let now_ns = clock.now_ns();
        let resolution = resolve_identifier(by_identifier.as_ref(), &request, now_ns)
            .map_err(|failed| failed.to_string())?;

        // W3.7. Minted by this resolve, so announced by it, in the resolve's
        // own chain: the holding recorded against the placeholder, the
        // escalation, and the replacement read as one arc.
        //
        // A failure to announce does not fail the resolve. The placeholder is
        // stored and the answer is right; what did not happen is the
        // announcement, and the interval makes it again.
        if let Some(minted) = resolution.minted {
            let meta = envelope.meta.as_ref();
            if let Err(failed) = announcing.publish(
                INSTRUMENT_MISSING,
                "meridian.v1.MissingInstrumentDetectedEvent",
                announcement(&minted, announcing.instance_id(), now_ns).encode_to_vec(),
                meta.map(|meta| meta.correlation_id.as_str()),
                meta.map(|meta| meta.message_id.as_str()),
            ) {
                tracing::warn!(
                    %failed,
                    placeholder = minted.placeholder_id,
                    "a minted placeholder could not be announced; it will be again"
                );
            }
        }

        Ok((
            "meridian.v1.ResolveIdentifierReply".to_string(),
            resolution.reply.encode_to_vec(),
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

/// W3.7, again. Announce every placeholder not yet replaced, and the legacy
/// path's `LCL-` instruments with them, and say how many.
///
/// Each in a chain of its own: nothing caused it but the time.
pub async fn announce_outstanding(
    bus: &Bus,
    store: Arc<dyn Store>,
    clock: &dyn Clock,
) -> Result<usize, String> {
    let publisher = bus.instance_id().to_string();
    let now_ns = clock.now_ns();

    // Off the runtime, for the reason `apply_and_announce` gives.
    let events = tokio::task::spawn_blocking(move || {
        placeholder::outstanding(store.as_ref(), &publisher, now_ns)
    })
    .await
    .map_err(|failed| format!("the announcing task failed: {failed}"))?
    .map_err(|failed| failed.to_string())?;

    for event in &events {
        bus.publish(
            INSTRUMENT_MISSING,
            "meridian.v1.MissingInstrumentDetectedEvent",
            event.encode_to_vec(),
            None,
            None,
        )
        .map_err(|failed| failed.to_string())?;
    }
    Ok(events.len())
}

/// Announce what is outstanding now, and then every `every`, for as long as
/// the process runs.
///
/// At start because a restart, a restore and an outage all leave placeholders
/// nobody is asking about, and a start is the first moment anything can.
/// Never returns and never gives up: a failed round is logged and the next one
/// tries again.
pub async fn announce_forever(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    every: Duration,
) {
    loop {
        match announce_outstanding(&bus, store.clone(), clock.as_ref()).await {
            Ok(count) => tracing::debug!(count, "announced the outstanding placeholders"),
            Err(failed) => {
                tracing::warn!(failed, "could not announce the outstanding placeholders")
            }
        }
        tokio::time::sleep(every).await;
    }
}

/// What one delivery came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Handled {
    /// Not a pulled record, or not a readable one. Nothing was written.
    ///
    /// A payload arriving under the wrong type name is refused rather than
    /// decoded: protobuf will happily read one message as another and hand back
    /// defaults, and a defaulted record here would be an instrument with no
    /// identity written into the instrument store.
    Ignored(String),

    /// A record was applied, or was an equal-or-older version and was not.
    Applied(Applied),

    /// As [`Handled::Applied`], and the placeholder the record names is now
    /// recorded as replaced by it and announced so (W3.8).
    Replaced(Applied),

    /// As [`Handled::Applied`], and the placeholder the record names could not
    /// be recorded as replaced. It stays outstanding, so its next
    /// announcement brings the same pairing back and this is tried again.
    NotReplaced(Applied, String),
}

/// Consumes pulled records and applies them. W3.3 and W3.4 in; W3.5 out.
pub struct Reactor {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
}

impl Reactor {
    pub fn new(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self {
        Self { bus, store, clock }
    }

    /// Consume pulled records until the bus shuts down.
    ///
    /// Never returns early on a failure. An undecodable delivery is one
    /// delivery, and a loop that exited on one would need a person to start it
    /// again, which is exactly what a deployment must not need.
    pub async fn run(self) {
        let pulled = self.bus.subscribe(INSTRUMENT_PULLED);
        self.consume(pulled).await
    }

    /// The same loop over a subscription the caller made.
    ///
    /// Separate so a caller can be subscribed before anything is published.
    /// Subscribing inside the task that consumes is a race: at-most-once
    /// delivery drops what arrives before the subscriber exists, and the loss
    /// is silent by design.
    pub async fn consume(self, mut pulled: meridian_bus::Subscription) {
        while let Some(delivery) = pulled.recv().await {
            match self.react(delivery).await {
                Handled::Ignored(why) => tracing::warn!(why, "ignored a delivery"),
                Handled::Applied(applied) => tracing::debug!(?applied, "applied a pulled record"),
                Handled::Replaced(applied) => {
                    tracing::info!(
                        ?applied,
                        "applied a pulled record that replaced a placeholder"
                    )
                }
                Handled::NotReplaced(applied, why) => tracing::warn!(
                    ?applied,
                    why,
                    "applied a pulled record, and could not record the placeholder it replaced"
                ),
            }
        }
    }

    /// Apply one delivery.
    ///
    /// Public so a test can drive it without a running loop, and so a caller
    /// that wants its own scheduling is not forced through [`Reactor::run`].
    pub async fn react(&self, delivery: Delivery) -> Handled {
        let envelope = delivery.envelope;
        if envelope.payload_type != "meridian.v1.PullInstrumentReply" {
            return Handled::Ignored(format!("payload type {}", envelope.payload_type));
        }

        let reply = match PullInstrumentReply::decode(&envelope.payload[..]) {
            Ok(reply) => reply,
            Err(failed) => return Handled::Ignored(format!("undecodable record: {failed}")),
        };

        // `found: false` is not published -- the conductor stays silent when the
        // platform knew nothing -- so one arriving is a publisher that does not
        // agree with this one about what the topic means. Refused rather than
        // treated as a deletion.
        let Some(record) = reply.instrument.filter(|_| reply.found) else {
            return Handled::Ignored("a pulled record carrying no instrument".into());
        };

        let now_ns = self.clock.now_ns();
        let applied = match self
            .apply_and_announce(record.clone(), &envelope, now_ns)
            .await
        {
            Ok(applied) => applied,
            Err(failed) => return Handled::Ignored(failed),
        };

        if reply.replaces_instrument_id.is_empty() {
            return Handled::Applied(applied);
        }

        // Whether or not the apply changed anything: an equal version is the
        // platform pairing the placeholder with an instrument already held,
        // and the pairing is news even when the record is not.
        match self
            .replace_and_announce(reply.replaces_instrument_id, record, &envelope, now_ns)
            .await
        {
            Ok(true) => Handled::Replaced(applied),
            Ok(false) => Handled::Applied(applied),
            Err(failed) => Handled::NotReplaced(applied, failed),
        }
    }

    /// W3.5. Write it through the one inbound path, and say what happened.
    ///
    /// The announcement carries the miss's correlation, so the whole arc -- the
    /// resolution that missed, the pull that answered it, the apply that
    /// followed -- reads as one chain rather than three unrelated events.
    async fn apply_and_announce(
        &self,
        record: PbInstrument,
        caused_by: &meridian_bus::Envelope,
        now_ns: i64,
    ) -> Result<Applied, String> {
        // Off the runtime, because the store is synchronous and a real one
        // blocks on a socket. Calling it directly from here would block a
        // worker thread, and with the Postgres driver it does not merely block:
        // its own runtime refuses to start inside this one, and the process
        // aborts. Found by running the end-to-end test against a database
        // rather than a HashMap.
        let store = self.store.clone();
        let outcome = tokio::task::spawn_blocking(move || apply(store.as_ref(), record, now_ns))
            .await
            .map_err(|failed| format!("the apply task failed: {failed}"))?
            .map_err(|failed| failed.to_string())?;

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

    /// W3.8. Announce the replacement if it is new, then record it. `true`
    /// when it was announced.
    ///
    /// Announced first and recorded second; [`placeholder::replacement`] says
    /// why that order. In the pulled record's chain, like the apply, so the
    /// placeholder's announcement, the escalation, the apply and the
    /// replacement read as one.
    async fn replace_and_announce(
        &self,
        replaced_id: String,
        record: PbInstrument,
        caused_by: &meridian_bus::Envelope,
        now_ns: i64,
    ) -> Result<bool, String> {
        let store = self.store.clone();
        let asking = replaced_id.clone();
        let event = tokio::task::spawn_blocking(move || {
            placeholder::replacement(store.as_ref(), &asking, &record, now_ns)
        })
        .await
        .map_err(|failed| format!("the replace task failed: {failed}"))?
        .map_err(|failed| failed.to_string())?;

        let Some(event) = event else {
            return Ok(false);
        };
        let replaced_by = event
            .instrument
            .as_ref()
            .map(|instrument| instrument.instrument_id.clone())
            .unwrap_or_default();

        let meta = caused_by.meta.as_ref();
        self.bus
            .publish(
                INSTRUMENT_REPLACED,
                "meridian.v1.InstrumentReplacedEvent",
                event.encode_to_vec(),
                meta.map(|meta| meta.correlation_id.as_str()),
                meta.map(|meta| meta.message_id.as_str()),
            )
            .map_err(|failed| failed.to_string())?;

        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.replace(&replaced_id, &replaced_by, now_ns))
            .await
            .map_err(|failed| format!("the replace task failed: {failed}"))?
            .map_err(|failed| failed.to_string())?;

        Ok(true)
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
    use meridian_domain::v1::{
        AssetClass, Identifier as PbIdentifier, InstrumentAppliedEvent, InstrumentLifecycleState,
        InstrumentReplacedEvent, MissReason, MissingInstrumentDetectedEvent,
        ResolveIdentifierReply, ResolveIdentifierRequest as PbResolveIdentifierRequest,
        ResolveInstrumentReply,
    };

    use super::*;
    use crate::store::Identifier;
    use crate::{Instrument, MemoryStore};

    /// The fixture's as-of.
    const AS_OF: i64 = 1_757_289_600_000_000_000;
    const NOW: i64 = 1_757_376_000_000_000_000;

    struct Stopped(AtomicI64);

    impl Stopped {
        fn at(now_ns: i64) -> Arc<Self> {
            Arc::new(Self(AtomicI64::new(now_ns)))
        }
    }

    impl Clock for Stopped {
        fn now_ns(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn bus() -> Arc<Bus> {
        Arc::new(Bus::single(
            "reference-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        ))
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
            asset_class: "ASSET_CLASS_EQUITY".into(),
            currency: "USD".into(),
            exchange_mic: "XNAS".into(),
            description: "Apple Inc. common stock".into(),
            lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
            version: 1,
            valid_from_ns: AS_OF - 1,
            record_time_ns: AS_OF - 1,
        }
    }

    /// What the conductor publishes after the platform answered.
    fn pulled(instrument_id: &str, version: i64) -> PullInstrumentReply {
        PullInstrumentReply {
            found: true,
            instrument: Some(PbInstrument {
                instrument_id: instrument_id.into(),
                identifiers: vec![PbIdentifier {
                    scheme: "figi".into(),
                    value: "BBG000ZZTOP1".into(),
                    source: String::new(),
                }],
                asset_class: AssetClass::Equity as i32,
                currency: "USD".into(),
                exchange_mic: "XNAS".into(),
                lifecycle_state: InstrumentLifecycleState::Active as i32,
                version,
                valid_from_ns: AS_OF - 1,
                ..Default::default()
            }),
            replaces_instrument_id: String::new(),
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
                    message_id: "MSG-PULLED".into(),
                    correlation_id: correlation.into(),
                    causation_id: String::new(),
                    publisher_instance_id: "conductor-1".into(),
                    topic: INSTRUMENT_PULLED.into(),
                    schema_version: "v1".into(),
                    published_at_ns: NOW,
                    ..Default::default()
                }),
                payload_type: payload_type.into(),
                payload,
            },
            sequence: 1,
        }
    }

    fn reactor(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<Stopped>) -> Reactor {
        Reactor::new(bus, store, clock)
    }

    /// A set nothing held matches: the fixture's ZZTOP.
    fn unknown() -> PbResolveIdentifierRequest {
        PbResolveIdentifierRequest {
            identifiers: vec![
                PbIdentifier {
                    scheme: "symbol".into(),
                    value: "ZZTOP".into(),
                    source: "snaptrade".into(),
                },
                PbIdentifier {
                    scheme: "figi".into(),
                    value: "BBG000ZZTOP1".into(),
                    source: String::new(),
                },
            ],
            as_of_ns: AS_OF,
            exchange_mic: String::new(),
            currency: String::new(),
        }
    }

    async fn resolve_over(
        bus: &Bus,
        request: &PbResolveIdentifierRequest,
    ) -> ResolveIdentifierReply {
        let (_, payload) = bus
            .call(
                RESOLVE_IDENTIFIER,
                "meridian.v1.ResolveIdentifierRequest",
                request.encode_to_vec(),
                None,
                None,
            )
            .await
            .unwrap();
        ResolveIdentifierReply::decode(&payload[..]).unwrap()
    }

    /// Nothing more within a moment. Silence is the assertion.
    async fn quiet(subscription: &mut meridian_bus::Subscription) -> bool {
        tokio::time::timeout(std::time::Duration::from_millis(100), subscription.recv())
            .await
            .is_err()
    }

    /// The conductor's pulled record, naming the placeholder it replaces.
    fn pairing(instrument_id: &str, replaces: &str) -> PullInstrumentReply {
        let mut reply = pulled(instrument_id, 1);
        reply.replaces_instrument_id = replaces.into();
        reply
    }

    #[tokio::test]
    async fn a_resolve_is_answered_from_the_store() {
        // No platform anywhere in this test, and there is no longer one to
        // stub: the instrument store cannot reach the platform at all. What used to be
        // asserted by faking an outage is now structural.
        let store = Arc::new(MemoryStore::new());
        store.apply(held()).unwrap();

        let bus = bus();
        serve_queries(&bus, store, Stopped::at(NOW));

        let (payload_type, payload) = bus
            .call(
                RESOLVE_IDENTIFIER,
                "meridian.v1.ResolveIdentifierRequest",
                PbResolveIdentifierRequest {
                    identifiers: vec![PbIdentifier {
                        scheme: "figi".into(),
                        value: "BBG000B9XRY4".into(),
                        source: String::new(),
                    }],
                    as_of_ns: AS_OF,
                    exchange_mic: String::new(),
                    currency: String::new(),
                }
                .encode_to_vec(),
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
        let store = Arc::new(MemoryStore::new());
        store.apply(held()).unwrap();

        let bus = bus();
        serve_queries(&bus, store, Stopped::at(NOW));

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
        assert_eq!(reply.instrument.unwrap().instrument_id, "INS-HELD");
    }

    #[tokio::test]
    async fn a_minted_placeholder_is_announced_once() {
        // W3.7. The set is asked about on every statement until its identity
        // arrives, and only the first ask mints.
        let store = Arc::new(MemoryStore::new());
        let bus = bus();
        let mut missing = bus.subscribe(INSTRUMENT_MISSING);
        serve_queries(&bus, store, Stopped::at(NOW));

        let reply = resolve_over(&bus, &unknown()).await;
        assert!(reply.found);
        assert!(reply.placeholder);

        let delivered = next(&mut missing).await;
        assert_eq!(
            delivered.envelope.payload_type,
            "meridian.v1.MissingInstrumentDetectedEvent"
        );
        let event =
            MissingInstrumentDetectedEvent::decode(&delivered.envelope.payload[..]).unwrap();
        assert_eq!(event.placeholder_instrument_id, reply.instrument_id);
        assert_eq!(event.reason, MissReason::NotFound as i32);
        assert_eq!(event.publisher_instance_id, "reference-1");
        assert_eq!(event.source, "snaptrade");
        assert_eq!(event.identifiers.len(), 2);
        assert_eq!(event.as_of_ns, AS_OF);
        assert_eq!(event.observed_at_ns, NOW);

        let again = resolve_over(&bus, &unknown()).await;
        assert_eq!(again.instrument_id, reply.instrument_id);
        assert!(quiet(&mut missing).await, "announced as minted twice");
    }

    #[tokio::test]
    async fn only_placeholders_not_yet_replaced_are_announced_again() {
        let store = Arc::new(MemoryStore::new());
        let outstanding = crate::resolve_identifier(store.as_ref(), &unknown(), NOW)
            .unwrap()
            .reply
            .instrument_id;

        let mut other = unknown();
        other.identifiers.truncate(1);
        other.identifiers[0].value = "GONE".into();
        let gone = crate::resolve_identifier(store.as_ref(), &other, NOW)
            .unwrap()
            .reply
            .instrument_id;
        store.replace(&gone, "INS-GONE", NOW).unwrap();

        let bus = bus();
        let mut missing = bus.subscribe(INSTRUMENT_MISSING);

        let announced = announce_outstanding(&bus, store, Stopped::at(NOW).as_ref())
            .await
            .unwrap();
        assert_eq!(announced, 1);

        let event =
            MissingInstrumentDetectedEvent::decode(&next(&mut missing).await.envelope.payload[..])
                .unwrap();
        assert_eq!(event.placeholder_instrument_id, outstanding);
        assert!(
            quiet(&mut missing).await,
            "a replaced placeholder was announced"
        );
    }

    #[tokio::test]
    async fn a_started_service_announces_what_is_outstanding() {
        // A restart, a restore and an outage all leave placeholders nobody is
        // asking about, and a start is the first moment anything can.
        let store = Arc::new(MemoryStore::new());
        let placeholder = crate::resolve_identifier(store.as_ref(), &unknown(), NOW)
            .unwrap()
            .reply
            .instrument_id;

        let bus = bus();
        let mut missing = bus.subscribe(INSTRUMENT_MISSING);
        tokio::spawn(
            crate::InstrumentService::new(Arc::clone(&bus), store, Stopped::at(NOW)).start(),
        );

        let event =
            MissingInstrumentDetectedEvent::decode(&next(&mut missing).await.envelope.payload[..])
                .unwrap();
        assert_eq!(event.placeholder_instrument_id, placeholder);
    }

    #[tokio::test]
    async fn a_record_that_replaces_a_placeholder_is_announced_once() {
        // W3.8. The pairing arrives, and arrives again when the re-announced
        // placeholder is escalated a second time before the first answer lands.
        let store = Arc::new(MemoryStore::new());
        let placeholder = crate::resolve_identifier(store.as_ref(), &unknown(), NOW)
            .unwrap()
            .reply
            .instrument_id;

        let bus = bus();
        let mut replaced = bus.subscribe(INSTRUMENT_REPLACED);
        let reactor = reactor(Arc::clone(&bus), store.clone(), Stopped::at(NOW));

        let handled = reactor
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                pairing("INS-ZZTOP", &placeholder).encode_to_vec(),
                "CORR-1",
            ))
            .await;
        assert_eq!(handled, Handled::Replaced(Applied::Stored));

        let delivered = next(&mut replaced).await;
        assert_eq!(
            delivered.envelope.payload_type,
            "meridian.v1.InstrumentReplacedEvent"
        );
        let meta = delivered.envelope.meta.clone().unwrap();
        assert_eq!(meta.correlation_id, "CORR-1");
        assert_eq!(meta.causation_id, "MSG-PULLED");
        let event = InstrumentReplacedEvent::decode(&delivered.envelope.payload[..]).unwrap();
        assert_eq!(event.replaced_instrument_id, placeholder);
        assert_eq!(event.instrument.unwrap().instrument_id, "INS-ZZTOP");
        assert_eq!(event.replaced_at_ns, NOW);

        // Resolving the set now answers the instrument.
        let reply = crate::resolve_identifier(store.as_ref(), &unknown(), NOW)
            .unwrap()
            .reply;
        assert_eq!(reply.instrument_id, "INS-ZZTOP");
        assert!(!reply.placeholder);

        let redelivered = reactor
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                pairing("INS-ZZTOP", &placeholder).encode_to_vec(),
                "CORR-2",
            ))
            .await;
        assert_eq!(redelivered, Handled::Applied(Applied::AlreadyCurrent));
        assert!(
            quiet(&mut replaced).await,
            "a replacement was announced twice"
        );
    }

    #[tokio::test]
    async fn a_legacy_lcl_record_is_replaced_the_same_way() {
        // The legacy path: an LCL- instrument the platform minted before it
        // minted only INS-, announced, paired, and replaced.
        let store = Arc::new(MemoryStore::new());
        let bus = bus();
        let mut missing = bus.subscribe(INSTRUMENT_MISSING);
        let mut replaced = bus.subscribe(INSTRUMENT_REPLACED);
        let reactor = reactor(Arc::clone(&bus), store.clone(), Stopped::at(NOW));

        reactor
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                pulled("LCL-LEGACY", 1).encode_to_vec(),
                "CORR-1",
            ))
            .await;

        announce_outstanding(&bus, store.clone(), Stopped::at(NOW).as_ref())
            .await
            .unwrap();
        let event =
            MissingInstrumentDetectedEvent::decode(&next(&mut missing).await.envelope.payload[..])
                .unwrap();
        assert_eq!(event.placeholder_instrument_id, "LCL-LEGACY");
        assert_eq!(event.identifiers[0].value, "BBG000ZZTOP1");

        let handled = reactor
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                pairing("INS-MOVED", "LCL-LEGACY").encode_to_vec(),
                "CORR-2",
            ))
            .await;
        assert_eq!(handled, Handled::Replaced(Applied::Stored));
        let event =
            InstrumentReplacedEvent::decode(&next(&mut replaced).await.envelope.payload[..])
                .unwrap();
        assert_eq!(event.replaced_instrument_id, "LCL-LEGACY");

        // Both rows carry the FIGI; the set answers one instrument, the new one.
        let reply = crate::resolve_identifier(
            store.as_ref(),
            &PbResolveIdentifierRequest {
                identifiers: vec![PbIdentifier {
                    scheme: "figi".into(),
                    value: "BBG000ZZTOP1".into(),
                    source: String::new(),
                }],
                as_of_ns: AS_OF,
                exchange_mic: String::new(),
                currency: String::new(),
            },
            NOW,
        )
        .unwrap()
        .reply;
        assert_eq!(reply.instrument_id, "INS-MOVED");

        // And it is no longer announced.
        assert_eq!(
            announce_outstanding(&bus, store, Stopped::at(NOW).as_ref())
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_pulled_record_is_applied_and_announced() {
        let store = Arc::new(MemoryStore::new());
        let bus = bus();
        let mut applied = bus.subscribe(INSTRUMENT_APPLIED);

        let handled = reactor(Arc::clone(&bus), store.clone(), Stopped::at(NOW))
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                pulled("INS-ZZTOP", 1).encode_to_vec(),
                "CORR-1",
            ))
            .await;

        assert!(matches!(handled, Handled::Applied(_)), "{handled:?}");
        assert!(store.by_id("INS-ZZTOP").unwrap().is_some());

        let announced = next(&mut applied).await;
        let event = InstrumentAppliedEvent::decode(&announced.envelope.payload[..]).unwrap();
        assert_eq!(event.instrument.unwrap().instrument_id, "INS-ZZTOP");
    }

    #[tokio::test]
    async fn the_announcement_is_caused_by_the_record_that_prompted_it() {
        // The arc reads as one chain across three processes: the resolve that
        // missed, the pull that answered it, the apply that followed. The
        // correlation travels; the causation points at the delivery in hand.
        let store = Arc::new(MemoryStore::new());
        let bus = bus();
        let mut applied = bus.subscribe(INSTRUMENT_APPLIED);

        reactor(Arc::clone(&bus), store, Stopped::at(NOW))
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                pulled("INS-ZZTOP", 1).encode_to_vec(),
                "CORR-1",
            ))
            .await;

        let meta = next(&mut applied).await.envelope.meta.unwrap();
        assert_eq!(meta.correlation_id, "CORR-1");
        assert_eq!(meta.causation_id, "MSG-PULLED");
    }

    #[tokio::test]
    async fn an_older_version_is_applied_as_a_no_op() {
        let store = Arc::new(MemoryStore::new());
        let bus = bus();

        let reactor = reactor(Arc::clone(&bus), store.clone(), Stopped::at(NOW));
        reactor
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                pulled("INS-ZZTOP", 4).encode_to_vec(),
                "CORR-1",
            ))
            .await;
        reactor
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                pulled("INS-ZZTOP", 2).encode_to_vec(),
                "CORR-2",
            ))
            .await;

        assert_eq!(store.by_id("INS-ZZTOP").unwrap().unwrap().version, 4);
    }

    #[tokio::test]
    async fn a_payload_under_the_wrong_type_name_is_refused_rather_than_decoded() {
        // protobuf will read one message as another and hand back defaults. A
        // defaulted record here would be an instrument with no identity,
        // written into the instrument store because a type name was wrong.
        let store = Arc::new(MemoryStore::new());
        let bus = bus();

        let handled = reactor(bus, store.clone(), Stopped::at(NOW))
            .react(delivery(
                "meridian.v1.InstrumentAppliedEvent",
                pulled("INS-ZZTOP", 1).encode_to_vec(),
                "CORR-1",
            ))
            .await;

        assert!(matches!(handled, Handled::Ignored(_)), "{handled:?}");
        assert!(store.by_id("INS-ZZTOP").unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unreadable_record_is_ignored_rather_than_applied() {
        let store = Arc::new(MemoryStore::new());
        let bus = bus();

        let handled = reactor(bus, store, Stopped::at(NOW))
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                vec![0xff, 0xff, 0xff],
                "CORR-1",
            ))
            .await;

        assert!(matches!(handled, Handled::Ignored(_)), "{handled:?}");
    }

    #[tokio::test]
    async fn a_reply_carrying_no_instrument_is_refused() {
        // The conductor stays silent when the platform knew nothing, so one of
        // these means a publisher that disagrees about what the topic means.
        // Refused rather than treated as a deletion.
        let store = Arc::new(MemoryStore::new());
        let bus = bus();

        let handled = reactor(bus, store, Stopped::at(NOW))
            .react(delivery(
                "meridian.v1.PullInstrumentReply",
                PullInstrumentReply {
                    found: false,
                    instrument: None,
                    replaces_instrument_id: String::new(),
                }
                .encode_to_vec(),
                "CORR-1",
            ))
            .await;

        assert!(matches!(handled, Handled::Ignored(_)), "{handled:?}");
    }

    #[tokio::test]
    async fn the_loop_consumes_what_the_bus_delivers() {
        let store = Arc::new(MemoryStore::new());
        let bus = bus();
        let pulled_sub = bus.subscribe(INSTRUMENT_PULLED);
        let mut applied = bus.subscribe(INSTRUMENT_APPLIED);

        tokio::spawn(
            reactor(Arc::clone(&bus), store.clone(), Stopped::at(NOW)).consume(pulled_sub),
        );

        bus.publish(
            INSTRUMENT_PULLED,
            "meridian.v1.PullInstrumentReply",
            pulled("INS-ZZTOP", 1).encode_to_vec(),
            None,
            None,
        )
        .unwrap();

        let announced = next(&mut applied).await;
        let event = InstrumentAppliedEvent::decode(&announced.envelope.payload[..]).unwrap();
        assert_eq!(event.instrument.unwrap().instrument_id, "INS-ZZTOP");
    }

    #[tokio::test]
    async fn a_wired_store_answers_and_applies() {
        let store = Arc::new(MemoryStore::new());
        store.apply(held()).unwrap();

        let bus = bus();
        let mut applied = bus.subscribe(INSTRUMENT_APPLIED);

        let service =
            crate::InstrumentService::new(Arc::clone(&bus), store.clone(), Stopped::at(NOW));
        tokio::spawn(service.start());

        // The queries it registered.
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

        // And the subscription it took before returning.
        bus.publish(
            INSTRUMENT_PULLED,
            "meridian.v1.PullInstrumentReply",
            pulled("INS-ZZTOP", 1).encode_to_vec(),
            None,
            None,
        )
        .unwrap();

        let announced = next(&mut applied).await;
        let event = InstrumentAppliedEvent::decode(&announced.envelope.payload[..]).unwrap();
        assert_eq!(event.instrument.unwrap().instrument_id, "INS-ZZTOP");
    }
}
