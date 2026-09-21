//! Misses in, records out. W3.2 in; W3.3, W3.4 and the handoff out.

use std::sync::Arc;

use meridian_bus::{Bus, Delivery};
use meridian_domain::v1::{MissingInstrumentDetectedEvent, PullInstrumentReply};
use prost::Message;

use crate::platform::{Platform, Reaction};

/// W3.2. A connector reporting that a resolution missed.
pub const INSTRUMENT_MISSING: &str = "platform.reference.event.instrument-missing";

/// W3.3, and W3.4's outcome on the same topic. What the platform gave us.
///
/// One topic for both because the instrument store applies a pulled record and a minted
/// one by the same version-gated path and has no reason to tell them apart.
/// Two topics would be two things to keep in step for no reader's benefit.
pub const INSTRUMENT_PULLED: &str = "platform.reference.event.instrument-pulled";

/// Where the time comes from.
///
/// Injectable because every duration here is a decision -- assertion lifetimes,
/// backoff, the throttle -- and a test that waits for one is a test people stop
/// running.
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

/// What one delivery came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Carried {
    /// Not a miss event, or not a readable one. Nothing was sent anywhere.
    ///
    /// A payload arriving under the wrong type name is refused rather than
    /// decoded: protobuf will happily read one message as another and hand back
    /// defaults, and defaults here would mean escalating an empty identifier
    /// set to the platform.
    Ignored(String),

    /// The platform answered, and this is what it said. `true` when a record
    /// was published for the instrument store to apply.
    Answered(Reaction, bool),

    /// The platform could not be reached, so nothing happened.
    ///
    /// Not an error to anybody upstream. The connector reports the same miss
    /// again on its next statement, and the throttle entry has already been
    /// cleared so that report is acted on.
    PlatformAway(String),
}

/// Consumes misses, asks the platform, and publishes what it gets.
pub struct Conductor {
    bus: Arc<Bus>,
    platform: Arc<Platform>,
    clock: Arc<dyn Clock>,
}

impl Conductor {
    pub fn new(bus: Arc<Bus>, platform: Arc<Platform>, clock: Arc<dyn Clock>) -> Self {
        Self {
            bus,
            platform,
            clock,
        }
    }

    /// Consume misses until the bus shuts down.
    ///
    /// Never returns early on a failure. A platform that is away is the case
    /// this loop exists to survive, and a loop that exited on one would need a
    /// person to start it again, which is exactly what a deployment must not
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
            match self.carry(delivery).await {
                Carried::Ignored(why) => tracing::warn!(why, "ignored a delivery"),
                Carried::PlatformAway(why) => {
                    // Expected, not exceptional. Logged so an operator can see
                    // how long it has been going on, not so somebody acts.
                    tracing::info!(why, "the platform is away; the miss will be reported again")
                }
                Carried::Answered(reaction, published) => {
                    tracing::debug!(?reaction, published, "carried a miss to the platform")
                }
            }
        }
    }

    /// React to one delivery.
    ///
    /// Public so a test can drive it without a running loop, and so a caller
    /// that wants its own scheduling is not forced through [`Conductor::run`].
    pub async fn carry(&self, delivery: Delivery) -> Carried {
        let envelope = delivery.envelope;
        if envelope.payload_type != "meridian.v1.MissingInstrumentDetectedEvent" {
            return Carried::Ignored(format!("payload type {}", envelope.payload_type));
        }

        let event = match MissingInstrumentDetectedEvent::decode(&envelope.payload[..]) {
            Ok(event) => event,
            Err(failed) => return Carried::Ignored(format!("undecodable miss: {failed}")),
        };

        let now_ns = self.clock.now_ns();
        let reaction = match self.platform.react_to_miss(&event, now_ns).await {
            Ok(reaction) => reaction,
            Err(failed) => return Carried::PlatformAway(failed.to_string()),
        };

        let record = match &reaction {
            Reaction::Pulled(record) | Reaction::Minted(record) => (**record).clone(),
            // Throttled, ambiguous or declined. Silence rather than an event
            // saying nothing happened: the instrument store has nothing to do with any
            // of those, and a topic nobody acts on is a topic that drifts.
            _ => return Carried::Answered(reaction, false),
        };

        let reply = PullInstrumentReply {
            found: true,
            instrument: Some(record),
        };

        // The miss's correlation travels with it, so the whole arc -- the
        // resolution that missed, the pull that answered it, the apply that
        // follows -- reads as one chain across three processes rather than as
        // three unrelated events in three logs.
        let meta = envelope.meta.as_ref();
        if let Err(failed) = self.bus.publish(
            INSTRUMENT_PULLED,
            "meridian.v1.PullInstrumentReply",
            reply.encode_to_vec(),
            meta.map(|meta| meta.correlation_id.as_str()),
            meta.map(|meta| meta.message_id.as_str()),
        ) {
            // The platform answered and the record is now nowhere. Said plainly
            // rather than swallowed: the connector will report the same miss
            // again, but only after the throttle window, so this is a real
            // delay and an operator should be able to see why.
            return Carried::Ignored(format!(
                "the pulled record could not be published: {failed}"
            ));
        }

        Carried::Answered(reaction, true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use meridian_bus::{Bus, Delivery, Envelope, MemoryBackend, MessageMeta};
    use meridian_domain::v1::{Identifier as PbIdentifier, MissReason};

    use super::*;
    use crate::platform::tests::{failure, platform as platform_with, record_json, reply, Fake};

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
        Arc::new(Bus::single("conductor-1", Arc::new(MemoryBackend::new())))
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

    fn delivery(payload_type: &str, payload: Vec<u8>) -> Delivery {
        Delivery {
            envelope: Envelope {
                meta: Some(MessageMeta {
                    message_id: "MSG-MISS".into(),
                    correlation_id: "CORR-1".into(),
                    causation_id: String::new(),
                    publisher_instance_id: "custody-snaptrade-1".into(),
                    topic: INSTRUMENT_MISSING.into(),
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

    fn conductor(bus: Arc<Bus>, transport: Arc<Fake>) -> Conductor {
        Conductor::new(bus, Arc::new(platform_with(transport)), Stopped::at(NOW))
    }

    /// What arrived on the pulled topic, or a failure rather than a hang.
    async fn next(subscription: &mut meridian_bus::Subscription) -> Delivery {
        tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .expect("nothing was published within five seconds")
            .expect("the bus shut down")
    }

    #[tokio::test]
    async fn a_record_the_platform_knew_is_published_for_the_store() {
        let bus = bus();
        let mut pulled = bus.subscribe(INSTRUMENT_PULLED);
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ZZTOP")))]);

        let carried = conductor(Arc::clone(&bus), transport)
            .carry(delivery(
                "meridian.v1.MissingInstrumentDetectedEvent",
                miss_event().encode_to_vec(),
            ))
            .await;

        assert!(
            matches!(carried, Carried::Answered(Reaction::Pulled(_), true)),
            "{carried:?}"
        );

        let published = next(&mut pulled).await;
        assert_eq!(
            published.envelope.payload_type,
            "meridian.v1.PullInstrumentReply"
        );
        let reply = PullInstrumentReply::decode(&published.envelope.payload[..]).unwrap();
        assert!(reply.found);
        assert_eq!(reply.instrument.unwrap().instrument_id, "INS-ZZTOP");
    }

    #[tokio::test]
    async fn the_published_record_carries_the_miss_that_caused_it() {
        // Three processes, one chain. Without this the resolve that missed, the
        // pull that answered it and the apply that followed are three unrelated
        // events in three logs.
        let bus = bus();
        let mut pulled = bus.subscribe(INSTRUMENT_PULLED);
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ZZTOP")))]);

        conductor(Arc::clone(&bus), transport)
            .carry(delivery(
                "meridian.v1.MissingInstrumentDetectedEvent",
                miss_event().encode_to_vec(),
            ))
            .await;

        let meta = next(&mut pulled).await.envelope.meta.unwrap();
        assert_eq!(meta.correlation_id, "CORR-1");
        assert_eq!(meta.causation_id, "MSG-MISS");
    }

    #[tokio::test]
    async fn an_outage_publishes_nothing_and_says_so() {
        let bus = bus();
        let mut pulled = bus.subscribe(INSTRUMENT_PULLED);
        let transport = Fake::new(vec![failure("no route to host")]);

        let carried = conductor(Arc::clone(&bus), transport)
            .carry(delivery(
                "meridian.v1.MissingInstrumentDetectedEvent",
                miss_event().encode_to_vec(),
            ))
            .await;

        assert!(matches!(carried, Carried::PlatformAway(_)), "{carried:?}");

        // Nothing published: the instrument store must not see an empty record and take
        // it for an answer.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), pulled.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_second_miss_inside_the_window_publishes_nothing() {
        // The throttle moved here with the platform connection. A burst of
        // misses is not a burst of pulls, and it is not a burst of events for
        // the instrument store to apply either.
        let bus = bus();
        let mut pulled = bus.subscribe(INSTRUMENT_PULLED);
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ZZTOP")))]);
        let conductor = conductor(Arc::clone(&bus), transport);

        conductor
            .carry(delivery(
                "meridian.v1.MissingInstrumentDetectedEvent",
                miss_event().encode_to_vec(),
            ))
            .await;
        next(&mut pulled).await;

        let again = conductor
            .carry(delivery(
                "meridian.v1.MissingInstrumentDetectedEvent",
                miss_event().encode_to_vec(),
            ))
            .await;

        assert!(
            matches!(again, Carried::Answered(Reaction::Throttled, false)),
            "{again:?}"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), pulled.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_delivery_that_is_not_a_miss_is_ignored() {
        // Refused by type name rather than decoded: protobuf reads one message
        // as another and hands back defaults, and a defaulted miss here would
        // escalate an empty identifier set to the platform.
        let bus = bus();
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ZZTOP")))]);
        let calls = Arc::clone(&transport);

        let carried = conductor(bus, transport)
            .carry(delivery(
                "meridian.v1.InstrumentAppliedEvent",
                miss_event().encode_to_vec(),
            ))
            .await;

        assert!(matches!(carried, Carried::Ignored(_)), "{carried:?}");
        assert_eq!(calls.calls(), 0, "the platform was asked anyway");
    }

    #[tokio::test]
    async fn an_unreadable_miss_is_ignored_rather_than_acted_on() {
        let bus = bus();
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ZZTOP")))]);
        let calls = Arc::clone(&transport);

        let carried = conductor(bus, transport)
            .carry(delivery(
                "meridian.v1.MissingInstrumentDetectedEvent",
                vec![0xff, 0xff, 0xff],
            ))
            .await;

        assert!(matches!(carried, Carried::Ignored(_)), "{carried:?}");
        assert_eq!(calls.calls(), 0);
    }

    #[tokio::test]
    async fn the_loop_consumes_what_the_bus_delivers() {
        let bus = bus();
        let misses = bus.subscribe(INSTRUMENT_MISSING);
        let mut pulled = bus.subscribe(INSTRUMENT_PULLED);
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ZZTOP")))]);

        tokio::spawn(conductor(Arc::clone(&bus), transport).consume(misses));

        bus.publish(
            INSTRUMENT_MISSING,
            "meridian.v1.MissingInstrumentDetectedEvent",
            miss_event().encode_to_vec(),
            None,
            None,
        )
        .unwrap();

        let published = next(&mut pulled).await;
        let reply = PullInstrumentReply::decode(&published.envelope.payload[..]).unwrap();
        assert_eq!(reply.instrument.unwrap().instrument_id, "INS-ZZTOP");
    }
}
