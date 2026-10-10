//! A person's ask, carried to the platform and the answer back. W3.3.
//!
//! From contract v10 the conductor reaches the platform about an instrument
//! only when a person asks, from the dashboard's Instruments page
//! (decisions/030, choice 3: optional, never required, never on a timer). It
//! asks by the record's open identifiers alone, answers the person with what
//! came back -- or that the platform holds nothing, or could not be reached --
//! and publishes what came back, naming the deployment's record, for the
//! instrument store to keep as offers and an added identifier (W3.5). A
//! connector's miss is no longer carried anywhere: it is a conflict the
//! deployment's own person settles.

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::v1::{
    AskPlatformForInstrumentReply, AskPlatformForInstrumentRequest, PullInstrumentReply,
};
use meridian_symbology::open_identifiers_strongest_first;
use prost::Message;

use crate::platform::Platform;

/// W3.3. The dashboard asking, for a person, about one record.
pub const ASK_PLATFORM_FOR_INSTRUMENT: &str =
    "platform.reference.query.ask-platform-for-instrument";

/// W3.3. What the platform answered, for the instrument store to keep.
pub const INSTRUMENT_PULLED: &str = "platform.reference.event.instrument-pulled";

/// How long one ask may take before the person is told the platform did not
/// answer. Within the dashboard's own wait for the reply.
const ASKING: Duration = Duration::from_secs(8);

/// Where the time comes from: the deployment's one clock (decisions/024),
/// given by whoever wires this up.
pub use meridian_clock::Clock;

/// Answers a person's ask about a record.
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

    /// Register the ask on the bus. A handler runs on the bus's blocking pool,
    /// so the platform call is driven to its end there, bounded by [`ASKING`].
    pub fn serve(self) {
        let this = Arc::new(self);
        let bus = Arc::clone(&this.bus);
        bus.serve(ASK_PLATFORM_FOR_INSTRUMENT, move |envelope| {
            if envelope.payload_type != "meridian.v1.AskPlatformForInstrumentRequest" {
                return Err(format!(
                    "expected meridian.v1.AskPlatformForInstrumentRequest, got {}",
                    envelope.payload_type
                ));
            }
            let request = AskPlatformForInstrumentRequest::decode(&envelope.payload[..])
                .map_err(|failed| format!("undecodable ask: {failed}"))?;
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| "the conductor has no runtime to ask on".to_string())?;
            let meta = envelope.meta.clone().unwrap_or_default();
            let reply = handle.block_on(this.ask(&request, &meta.correlation_id, &meta.message_id));
            Ok((
                "meridian.v1.AskPlatformForInstrumentReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }

    /// Ask, answer, and publish what came back.
    pub async fn ask(
        &self,
        request: &AskPlatformForInstrumentRequest,
        correlation: &str,
        causation: &str,
    ) -> AskPlatformForInstrumentReply {
        if open_identifiers_strongest_first(&request.identifiers).is_empty() {
            return AskPlatformForInstrumentReply {
                reachable: true,
                found: false,
                instrument: None,
                detail: "the record carries no open identifier to ask by".into(),
            };
        }
        let now_ns = self.clock.now_ns();
        let as_of_ns = if request.as_of_ns > 0 {
            request.as_of_ns
        } else {
            now_ns
        };
        let asked = tokio::time::timeout(
            ASKING,
            self.platform
                .ask_with_venues(&request.identifiers, as_of_ns, now_ns),
        )
        .await;
        let (record, venues) = match asked {
            Err(_) => {
                return unreachable("the platform did not answer in time".into());
            }
            Ok(Err(failed)) => return unreachable(failed.to_string()),
            Ok(Ok(None)) => {
                return AskPlatformForInstrumentReply {
                    reachable: true,
                    found: false,
                    instrument: None,
                    detail: "the platform holds no record by these identifiers".into(),
                }
            }
            Ok(Ok(Some(record))) => record,
        };

        // In the ask's chain, so the ask, the answer and the record's new
        // version read as one arc.
        if let Err(failed) = self.bus.publish(
            INSTRUMENT_PULLED,
            "meridian.v1.PullInstrumentReply",
            PullInstrumentReply {
                found: true,
                instrument: Some(record.clone()),
                for_instrument_id: request.instrument_id.clone(),
                // The venues the platform's answer names (contract v18,
                // W3.5): its listing venue and that venue's operating venue.
                venues,
            }
            .encode_to_vec(),
            Some(correlation).filter(|c| !c.is_empty()),
            Some(causation).filter(|c| !c.is_empty()),
        ) {
            tracing::warn!(%failed, "the platform's answer could not be published; the person may ask again");
        }
        AskPlatformForInstrumentReply {
            reachable: true,
            found: true,
            instrument: Some(record),
            detail: String::new(),
        }
    }
}

fn unreachable(detail: String) -> AskPlatformForInstrumentReply {
    tracing::info!(
        detail,
        "the platform could not be asked; completion works without it"
    );
    AskPlatformForInstrumentReply {
        reachable: false,
        found: false,
        instrument: None,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use meridian_bus::MemoryBackend;
    use meridian_domain::v1::Identifier as PbIdentifier;

    use super::*;
    use crate::platform::tests::{failure, platform as platform_with, record_json, reply, Fake};

    const NOW: i64 = 1_757_376_000_000_000_000;

    fn bus() -> Arc<Bus> {
        Arc::new(Bus::single(
            "conductor-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::ManualClock::at(NOW)),
        ))
    }

    fn asking(identifiers: Vec<PbIdentifier>) -> AskPlatformForInstrumentRequest {
        AskPlatformForInstrumentRequest {
            instrument_id: "LCL-USD".into(),
            identifiers,
            as_of_ns: NOW,
        }
    }

    fn usd() -> PbIdentifier {
        PbIdentifier {
            scheme: "iso4217".into(),
            value: "USD".into(),
            source: String::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_is_given_to_the_person_and_published_naming_the_record() {
        let bus = bus();
        let mut pulled = bus.subscribe(INSTRUMENT_PULLED);
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-USD")))]);
        let conductor =
            Conductor::new(bus.clone(), Arc::new(platform_with(transport)), bus.clock());

        let reply = conductor.ask(&asking(vec![usd()]), "", "").await;
        assert!(reply.reachable && reply.found);
        assert_eq!(reply.instrument.unwrap().instrument_id, "INS-USD");

        let heard = pulled.recv().await.unwrap();
        let event = PullInstrumentReply::decode(&heard.envelope.payload[..]).unwrap();
        assert_eq!(event.for_instrument_id, "LCL-USD");
        assert_eq!(event.instrument.unwrap().instrument_id, "INS-USD");
    }

    #[tokio::test(start_paused = true)]
    async fn a_platform_away_is_said_and_nothing_is_published() {
        let bus = bus();
        let mut pulled = bus.subscribe(INSTRUMENT_PULLED);
        let transport = Fake::new(vec![
            failure("no route to host"),
            failure("no route to host"),
            failure("no route to host"),
        ]);
        let conductor =
            Conductor::new(bus.clone(), Arc::new(platform_with(transport)), bus.clock());

        let reply = conductor.ask(&asking(vec![usd()]), "", "").await;
        assert!(!reply.reachable);
        assert!(!reply.detail.is_empty());
        let quiet = tokio::time::timeout(Duration::from_millis(100), pulled.recv()).await;
        assert!(quiet.is_err(), "nothing to keep, so nothing published");
    }

    #[tokio::test]
    async fn a_record_with_no_open_identifier_is_not_asked_about() {
        let bus = bus();
        let transport = Fake::new(vec![]);
        let conductor = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(transport.clone())),
            bus.clock(),
        );
        let reply = conductor
            .ask(
                &asking(vec![PbIdentifier {
                    scheme: "cusip".into(),
                    value: "037833100".into(),
                    source: String::new(),
                }]),
                "",
                "",
            )
            .await;
        assert!(reply.reachable && !reply.found);
        assert_eq!(
            transport.calls(),
            0,
            "a licensed scheme never leaves the deployment"
        );
    }
}

/// W3.15, heard (contract v18): a venue a plugin's source named that the
/// deployment does not hold.
pub const VENUE_MISSING: &str = "platform.reference.event.venue-missing";

/// How long one venue's codes are not asked about again: a plugin reports a
/// miss each time it meets the venue, and the platform needs asking once
/// (W3.15: the conductor asks again when the venue is next reported, after
/// this).
const ASKED_AGAIN_AFTER: Duration = Duration::from_secs(600);

/// Asks the platform about each venue a plugin reports missing, by its public
/// codes alone, one venue at a time and never the whole list, and publishes
/// what came back for the instrument store to keep (W3.15, W3.3, W3.5). No
/// person asks: the ask carries no holding and no licensed identifier.
pub struct VenueAsker {
    bus: Arc<Bus>,
    platform: Arc<Platform>,
    clock: Arc<dyn Clock>,
}

impl VenueAsker {
    pub fn new(bus: Arc<Bus>, platform: Arc<Platform>, clock: Arc<dyn Clock>) -> Self {
        Self {
            bus,
            platform,
            clock,
        }
    }

    /// Subscribe before returning; the loop that asks is what is left.
    pub fn start(self) -> impl std::future::Future<Output = ()> {
        let mut missing = self.bus.subscribe(VENUE_MISSING);
        async move {
            let mut asked: std::collections::BTreeMap<(String, String, String), i64> =
                std::collections::BTreeMap::new();
            while let Some(delivery) = missing.recv().await {
                let Ok(event) = meridian_domain::v1::MissingVenueDetectedEvent::decode(
                    &delivery.envelope.payload[..],
                ) else {
                    tracing::warn!("a venue reported missing did not decode");
                    continue;
                };
                let now = self.clock.now_ns();
                asked.retain(|_, at| now - *at < ASKED_AGAIN_AFTER.as_nanos() as i64);
                for identifier in public_codes(&event.identifiers) {
                    let key = (
                        identifier.scheme.clone(),
                        identifier.value.clone(),
                        identifier.source.clone(),
                    );
                    if asked.contains_key(&key) {
                        continue;
                    }
                    asked.insert(key, now);
                    let as_of = if event.as_of_ns > 0 {
                        event.as_of_ns
                    } else {
                        now
                    };
                    let pulled = tokio::time::timeout(
                        ASKING,
                        self.platform.pull_venue(&identifier, as_of, now),
                    )
                    .await;
                    let venues = match pulled {
                        Err(_) => {
                            tracing::warn!(
                                scheme = identifier.scheme,
                                value = identifier.value,
                                "the platform did not answer a venue's ask in time"
                            );
                            continue;
                        }
                        Ok(Err(failed)) => {
                            tracing::warn!(scheme = identifier.scheme, value = identifier.value, %failed, "a venue's ask failed");
                            continue;
                        }
                        Ok(Ok(venues)) => venues,
                    };
                    if venues.is_empty() {
                        tracing::info!(
                            scheme = identifier.scheme,
                            value = identifier.value,
                            "the platform holds no venue by these codes yet"
                        );
                        continue;
                    }
                    let meta = delivery.envelope.meta.clone().unwrap_or_default();
                    if let Err(failed) = self.bus.publish(
                        INSTRUMENT_PULLED,
                        "meridian.v1.PullInstrumentReply",
                        PullInstrumentReply {
                            found: false,
                            instrument: None,
                            for_instrument_id: String::new(),
                            venues,
                        }
                        .encode_to_vec(),
                        Some(meta.correlation_id.as_str()).filter(|c| !c.is_empty()),
                        Some(meta.message_id.as_str()).filter(|c| !c.is_empty()),
                    ) {
                        tracing::warn!(%failed, "a venue the platform answered was not published");
                    }
                    break;
                }
            }
        }
    }
}

/// A venue's public codes, which alone are asked by: a MIC under `iso10383`,
/// and a vendor's code as a `symbol` with its source. A MIC first.
pub fn public_codes(
    identifiers: &[meridian_domain::v1::Identifier],
) -> Vec<meridian_domain::v1::Identifier> {
    let mut codes: Vec<_> = identifiers
        .iter()
        .filter(|i| {
            (i.scheme == "iso10383" && i.source.is_empty() && !i.value.is_empty())
                || (i.scheme == "symbol" && !i.source.is_empty() && !i.value.is_empty())
        })
        .cloned()
        .collect();
    codes.sort_by_key(|i| i.scheme != "iso10383");
    codes
}
