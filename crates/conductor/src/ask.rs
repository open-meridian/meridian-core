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
use prost::Message;

use crate::platform::Platform;

/// W3.3. The dashboard asking, for a person, about one record.
pub const ASK_PLATFORM_FOR_INSTRUMENT: &str =
    "platform.reference.query.ask-platform-for-instrument";

/// W3.3. What the platform answered, for the instrument store to keep.
pub const INSTRUMENT_PULLED: &str = "platform.reference.event.instrument-pulled";

/// W3.29 (contract v26): the instrument store asking about a venue's group
/// of event contracts.
pub const ASK_PLATFORM_FOR_EVENT_GROUP: &str =
    "platform.reference.query.ask-platform-for-event-group";

/// W3.32 (contract v28; ruling 612): the instrument store asking about a
/// company it does not hold, by its open identifiers alone.
pub const ASK_PLATFORM_FOR_COMPANY: &str = "platform.reference.query.ask-platform-for-company";

/// W3.33 (contract v28; rulings 757, 763, 765): the instrument picker's
/// search, the dashboard's ask.
pub const ASK_PLATFORM_FOR_INSTRUMENT_CANDIDATES: &str =
    "platform.reference.query.ask-platform-for-instrument-candidates";

/// How long one ask may take before the person is told the platform did not
/// answer. Within the dashboard's own wait for the reply (12 s), and longer
/// than a resolve's bounded ask (8 s, the one-ID spec's requirement 15), so
/// a platform slower than a resolve waits is still heard in the background:
/// a currency pulled for the next resolve (the 2026-10-11 ruling), a late
/// match-or-mint answer resolving the provisional record (W3.4).
const ASKING: Duration = Duration::from_secs(11);

/// Where the time comes from: the deployment's one clock (decisions/024),
/// given by whoever wires this up.
pub use meridian_clock::Clock;

/// Answers a person's ask about a record.
pub struct Conductor {
    bus: Arc<Bus>,
    platform: Arc<Platform>,
    clock: Arc<dyn Clock>,
    /// What pulls a currency's calendar and a venue's with them (W3.16,
    /// contract v22), where one is given.
    calendars: Option<Arc<crate::calendars::CalendarAsker>>,
    /// The licence list read last (contract v24, W3.25): what may be sent.
    licence: crate::reference::Licence,
}

impl Conductor {
    pub fn new(bus: Arc<Bus>, platform: Arc<Platform>, clock: Arc<dyn Clock>) -> Self {
        Self {
            bus,
            platform,
            clock,
            calendars: None,
            licence: Arc::default(),
        }
    }

    /// The same, sending by the licence list the reference puller reads
    /// (contract v24, W3.25).
    pub fn with_licence(mut self, licence: crate::reference::Licence) -> Self {
        self.licence = licence;
        self
    }

    /// The same, pulling with each answer the calendars of the cash
    /// instrument and the venues it carries (W3.16, contract v22).
    pub fn with_calendars(mut self, calendars: Arc<crate::calendars::CalendarAsker>) -> Self {
        self.calendars = Some(calendars);
        self
    }

    /// Register the ask on the bus. A handler runs on the bus's blocking pool,
    /// so the platform call is driven to its end there, bounded by [`ASKING`].
    pub fn serve(self) {
        let this = Arc::new(self);
        let bus = Arc::clone(&this.bus);
        let grouping = Arc::clone(&this);
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
        let naming = Arc::clone(&grouping);
        bus.serve(ASK_PLATFORM_FOR_COMPANY, move |envelope| {
            if envelope.payload_type != "meridian.v1.MatchOrMintCompanyRequest" {
                return Err(format!(
                    "expected meridian.v1.MatchOrMintCompanyRequest, got {}",
                    envelope.payload_type
                ));
            }
            let request =
                meridian_domain::v1::MatchOrMintCompanyRequest::decode(&envelope.payload[..])
                    .map_err(|failed| format!("undecodable ask: {failed}"))?;
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| "the conductor has no runtime to ask on".to_string())?;
            // The platform not reached is a refusal saying so, never a reply
            // (W3.32): the reply carries the platform's answer alone.
            let reply = handle.block_on(naming.company(&request))?;
            Ok((
                "meridian.v1.MatchOrMintCompanyReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
        let searching = Arc::clone(&grouping);
        bus.serve(ASK_PLATFORM_FOR_INSTRUMENT_CANDIDATES, move |envelope| {
            if envelope.payload_type != "meridian.v1.InstrumentCandidatesRequest" {
                return Err(format!(
                    "expected meridian.v1.InstrumentCandidatesRequest, got {}",
                    envelope.payload_type
                ));
            }
            let request =
                meridian_domain::v1::InstrumentCandidatesRequest::decode(&envelope.payload[..])
                    .map_err(|failed| format!("undecodable ask: {failed}"))?;
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| "the conductor has no runtime to ask on".to_string())?;
            let reply = handle.block_on(searching.candidates(&request));
            Ok((
                "meridian.v1.AskPlatformForInstrumentCandidatesReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
        let this = grouping;
        bus.serve(ASK_PLATFORM_FOR_EVENT_GROUP, move |envelope| {
            if envelope.payload_type != "meridian.v1.AskPlatformForEventGroupRequest" {
                return Err(format!(
                    "expected meridian.v1.AskPlatformForEventGroupRequest, got {}",
                    envelope.payload_type
                ));
            }
            let request =
                meridian_domain::v1::AskPlatformForEventGroupRequest::decode(&envelope.payload[..])
                    .map_err(|failed| format!("undecodable ask: {failed}"))?;
            let asking = request.match_or_mint.unwrap_or_default();
            // Members by the platform's own IDs alone: never a deployment's
            // LCL- record (W3.29; decisions/037).
            if let Some(i) = asking
                .member_instrument_ids
                .iter()
                .position(|member| !member.starts_with("INS-"))
            {
                return Err(meridian_bus::refusal_naming(
                    // REFUSAL_REASON_UNSPECIFIED: a malformed ask, no code.
                    0,
                    &[format!("match_or_mint.member_instrument_ids[{i}]")],
                    "only the platform's INS- IDs travel; a group with a local member is the \
                     deployment's own",
                ));
            }
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| "the conductor has no runtime to ask on".to_string())?;
            let reply = handle.block_on(this.event_group(&asking));
            Ok((
                "meridian.v1.AskPlatformForEventGroupReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }

    /// W3.32, W1.32 (contract v28; ruling 612): the platform asked to match
    /// or mint a company, bounded by [`ASKING`], only identifiers in a scheme
    /// on the licence list sent. Its answer whole; a platform without the
    /// route, or not answering in time, is the refusal's words, and the
    /// store's resolve a miss.
    pub async fn company(
        &self,
        asking: &meridian_domain::v1::MatchOrMintCompanyRequest,
    ) -> Result<meridian_domain::v1::MatchOrMintCompanyReply, String> {
        let licence = self.licence.read().map(|held| held.clone()).ok().flatten();
        let asking = meridian_domain::v1::MatchOrMintCompanyRequest {
            // A company is named by a CIK or an LEI (W1.32), never a
            // source's own code nor the deployment's.
            identifiers: crate::reference::sendable_on(&asking.identifiers, licence.as_ref())
                .into_iter()
                .filter(|identifier| {
                    identifier.source.is_empty()
                        && matches!(identifier.scheme.as_str(), "cik" | "lei")
                })
                .collect(),
            ..asking.clone()
        };
        if asking.identifiers.is_empty() {
            return Err(meridian_bus::refusal_naming(
                // REFUSAL_REASON_UNSPECIFIED: nothing it may send, no code.
                0,
                &["identifiers".to_string()],
                "no identifier in a scheme on the licence list to ask the platform by",
            ));
        }
        let now_ns = self.clock.now_ns();
        match tokio::time::timeout(ASKING, self.platform.match_or_mint_company(&asking, now_ns))
            .await
        {
            Err(_) => Err("the platform did not answer in time".into()),
            Ok(Err(failed)) => Err(format!("the platform could not be reached: {failed}")),
            Ok(Ok(answer)) => Ok(answer),
        }
    }

    /// W3.33 (contract v28; ruling 757): the platform's candidates for what
    /// a person typed into the picker, sent as typed and nothing else -- an
    /// ISIN or a CUSIP among it, the licensed-scheme rule's one exception
    /// (ruling 761) -- bounded by [`ASKING`]. Not reached, said so, and the
    /// picker answers from the deployment's own records.
    pub async fn candidates(
        &self,
        asking: &meridian_domain::v1::InstrumentCandidatesRequest,
    ) -> meridian_domain::v1::AskPlatformForInstrumentCandidatesReply {
        use meridian_domain::v1::AskPlatformForInstrumentCandidatesReply;
        let query = asking
            .query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if query.is_empty() || query.chars().count() > 64 {
            return AskPlatformForInstrumentCandidatesReply {
                reachable: false,
                detail: "the search is one line of 1 to 64 characters".into(),
                answer: None,
            };
        }
        let asking = meridian_domain::v1::InstrumentCandidatesRequest {
            query,
            limit: asking.limit.clamp(0, 25),
            ..asking.clone()
        };
        let now_ns = self.clock.now_ns();
        match tokio::time::timeout(ASKING, self.platform.instrument_candidates(&asking, now_ns))
            .await
        {
            Err(_) => AskPlatformForInstrumentCandidatesReply {
                reachable: false,
                detail: "the platform did not answer in time".into(),
                answer: None,
            },
            Ok(Err(failed)) => AskPlatformForInstrumentCandidatesReply {
                reachable: false,
                detail: failed.to_string(),
                answer: None,
            },
            Ok(Ok(answer)) => AskPlatformForInstrumentCandidatesReply {
                reachable: true,
                detail: String::new(),
                answer: Some(answer),
            },
        }
    }

    /// W3.29, W1.30 (contract v26): the platform asked to match or mint a
    /// venue's group, bounded by [`ASKING`]; a platform without v26's route,
    /// or not answering in time, is unreachable and the store keeps its own
    /// group.
    pub async fn event_group(
        &self,
        asking: &meridian_domain::v1::EscalateEventGroupRequest,
    ) -> meridian_domain::v1::AskPlatformForEventGroupReply {
        use meridian_domain::v1::AskPlatformForEventGroupReply;
        let now_ns = self.clock.now_ns();
        match tokio::time::timeout(ASKING, self.platform.escalate_event_group(asking, now_ns)).await
        {
            Err(_) => AskPlatformForEventGroupReply {
                reachable: false,
                detail: "the platform did not answer in time".into(),
                answer: None,
            },
            Ok(Err(failed)) => AskPlatformForEventGroupReply {
                reachable: false,
                detail: failed.to_string(),
                answer: None,
            },
            Ok(Ok(answer)) => AskPlatformForEventGroupReply {
                reachable: true,
                detail: if answer.pending_review {
                    "queued for the platform's staff to review".into()
                } else {
                    String::new()
                },
                answer: Some(answer),
            },
        }
    }

    /// Ask, answer, and publish what came back.
    pub async fn ask(
        &self,
        request: &AskPlatformForInstrumentRequest,
        correlation: &str,
        causation: &str,
    ) -> AskPlatformForInstrumentReply {
        // Contract v24 (W3.4): the store's matching and minting request.
        if let Some(asking) = request.match_or_mint.as_ref() {
            return self.match_or_mint(asking, correlation, causation).await;
        }
        // Only schemes on the licence list (W3.3 from v24, W3.25).
        let list = self.licence.read().ok().and_then(|held| held.clone());
        let sendable = crate::reference::sendable_on(&request.identifiers, list.as_ref());
        let request = &AskPlatformForInstrumentRequest {
            identifiers: sendable,
            ..request.clone()
        };
        if crate::platform::pulled_by(&request.identifiers).is_empty() {
            return AskPlatformForInstrumentReply {
                reachable: true,
                found: false,
                instrument: None,
                detail: "the record carries no open identifier to ask by".into(),
                match_or_mint_answer: None,
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
        let (record, venues): (_, Vec<meridian_domain::v1::VenueRecord>) = match asked {
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
                    match_or_mint_answer: None,
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
                venues: venues.clone(),
            }
            .encode_to_vec(),
            Some(correlation).filter(|c| !c.is_empty()),
            Some(causation).filter(|c| !c.is_empty()),
        ) {
            tracing::warn!(%failed, "the platform's answer could not be published; the person may ask again");
        }
        if let Some(calendars) = &self.calendars {
            pull_calendars_with(calendars, Some(&record), &venues, correlation, causation).await;
        }
        AskPlatformForInstrumentReply {
            reachable: true,
            found: true,
            instrument: Some(record),
            detail: String::new(),
            match_or_mint_answer: None,
        }
    }
}

impl Conductor {
    /// W3.4 (contract v24): the platform asked to match or mint, by what the
    /// licence list lets travel, bounded by [`ASKING`]. A platform without
    /// v24's route, or not answering in time, is unreachable: the store keeps
    /// a provisional record and asks again (the build plan's step 4).
    pub async fn match_or_mint(
        &self,
        asking: &meridian_domain::v1::EscalateInstrumentRequest,
        correlation: &str,
        causation: &str,
    ) -> AskPlatformForInstrumentReply {
        let list = self.licence.read().ok().and_then(|held| held.clone());
        let identifiers = crate::reference::sendable_on(&asking.identifiers, list.as_ref())
            .into_iter()
            // The platform's own ID is never a matching request's.
            .filter(|identifier| identifier.scheme != meridian_symbology::GLOBAL_ID)
            .collect::<Vec<_>>();
        if identifiers.is_empty() {
            return AskPlatformForInstrumentReply {
                reachable: true,
                found: false,
                instrument: None,
                detail: "nothing on the licence list to look for".into(),
                match_or_mint_answer: None,
            };
        }
        let asking = meridian_domain::v1::EscalateInstrumentRequest {
            identifiers,
            ..asking.clone()
        };
        let now_ns = self.clock.now_ns();
        let answered =
            tokio::time::timeout(ASKING, self.platform.escalate_with_venues(&asking, now_ns)).await;
        let (answer, venues) = match answered {
            Err(_) => return unreachable("the platform did not answer in time".into()),
            Ok(Err(failed)) => return unreachable(failed.to_string()),
            Ok(Ok(answer)) => answer,
        };
        // The venues the answer names, for the store to keep (W3.5, contract
        // v18): published as a venue's pull is, naming no record.
        if !venues.is_empty() {
            if let Err(failed) = self.bus.publish(
                INSTRUMENT_PULLED,
                "meridian.v1.PullInstrumentReply",
                PullInstrumentReply {
                    found: false,
                    instrument: None,
                    for_instrument_id: String::new(),
                    venues: venues.clone(),
                }
                .encode_to_vec(),
                Some(correlation).filter(|c| !c.is_empty()),
                Some(causation).filter(|c| !c.is_empty()),
            ) {
                tracing::warn!(%failed, "the venues the platform named could not be published");
            }
        }
        if let (Some(calendars), Some(record)) = (&self.calendars, answer.instrument.as_ref()) {
            pull_calendars_with(calendars, Some(record), &venues, correlation, causation).await;
        }
        AskPlatformForInstrumentReply {
            reachable: true,
            found: answer.instrument.is_some(),
            instrument: answer.instrument.clone(),
            detail: if answer.pending_review {
                "queued for the platform's staff to review".into()
            } else {
                String::new()
            },
            match_or_mint_answer: Some(answer),
        }
    }
}

/// The calendars that come with what was pulled (W3.16, contract v22): a
/// cash instrument's currency calendar, and each venue's trading calendar.
async fn pull_calendars_with(
    calendars: &crate::calendars::CalendarAsker,
    record: Option<&meridian_domain::v1::InstrumentRecord>,
    venues: &[meridian_domain::v1::VenueRecord],
    correlation: &str,
    causation: &str,
) {
    use crate::calendars::Subject;
    if let Some(record) =
        record.filter(|record| record.asset_class == meridian_domain::v1::AssetClass::Cash as i32)
    {
        calendars
            .pull(
                Subject::Currency(record.instrument_id.clone()),
                correlation,
                causation,
            )
            .await;
    }
    for venue in venues {
        calendars
            .pull(
                Subject::Venue(venue.venue_id.clone()),
                correlation,
                causation,
            )
            .await;
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
        match_or_mint_answer: None,
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
    calendars: Option<Arc<crate::calendars::CalendarAsker>>,
}

impl VenueAsker {
    pub fn new(bus: Arc<Bus>, platform: Arc<Platform>, clock: Arc<dyn Clock>) -> Self {
        Self {
            bus,
            platform,
            clock,
            calendars: None,
        }
    }

    /// The same, pulling each venue's trading calendar with it (W3.16,
    /// contract v22).
    pub fn with_calendars(mut self, calendars: Arc<crate::calendars::CalendarAsker>) -> Self {
        self.calendars = Some(calendars);
        self
    }

    /// Subscribe before returning; the loop that asks is what is left.
    pub fn start(self) -> impl std::future::Future<Output = ()> {
        let mut missing = self.bus.subscribe_shared(VENUE_MISSING, "venue-asker");
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
                            venues: venues.clone(),
                        }
                        .encode_to_vec(),
                        Some(meta.correlation_id.as_str()).filter(|c| !c.is_empty()),
                        Some(meta.message_id.as_str()).filter(|c| !c.is_empty()),
                    ) {
                        tracing::warn!(%failed, "a venue the platform answered was not published");
                    }
                    if let Some(calendars) = &self.calendars {
                        pull_calendars_with(
                            calendars,
                            None,
                            &venues,
                            &meta.correlation_id,
                            &meta.message_id,
                        )
                        .await;
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
            ..Default::default()
        }
    }

    fn usd() -> PbIdentifier {
        PbIdentifier {
            scheme: "iso4217".into(),
            value: "USD".into(),
            source: String::new(),
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_is_given_to_the_person_and_published_naming_the_record() {
        let bus = bus();
        let mut pulled = bus.subscribe_every(INSTRUMENT_PULLED);
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
        let mut pulled = bus.subscribe_every(INSTRUMENT_PULLED);
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
                    ..Default::default()
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

    // ── Contract v24 (W3.4, W3.25) ────────────────────────────────────────

    fn matching(identifiers: Vec<PbIdentifier>) -> AskPlatformForInstrumentRequest {
        AskPlatformForInstrumentRequest {
            identifiers: identifiers.clone(),
            as_of_ns: NOW,
            match_or_mint: Some(meridian_domain::v1::EscalateInstrumentRequest {
                source: "custody-snaptrade-1".into(),
                asset_class: meridian_domain::v1::AssetClass::Equity as i32,
                identifiers,
                as_of_ns: NOW,
                template_id: "TPL-equity".into(),
                template_version: "1".into(),
                request_key: "a1b2c3".into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn figi() -> PbIdentifier {
        PbIdentifier {
            scheme: "share_class_figi".into(),
            value: "BBG001S5N8V8".into(),
            ..Default::default()
        }
    }

    fn cusip() -> PbIdentifier {
        PbIdentifier {
            scheme: "cusip".into(),
            value: "037833100".into(),
            ..Default::default()
        }
    }

    /// The build plan's step 4: a platform without v24's routes (404) is
    /// unreachable -- never an answer, nothing minted there -- so the store
    /// keeps a provisional record and asks again on reconnecting.
    #[tokio::test(start_paused = true)]
    async fn a_platform_without_v24s_routes_is_unreachable() {
        let bus = bus();
        let transport = Fake::new(vec![Ok(reply(404, "{\"detail\": \"Not Found\"}"))]);
        let conductor = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(transport.clone())),
            bus.clock(),
        );
        let reply = conductor.ask(&matching(vec![figi()]), "", "").await;
        assert!(!reply.reachable, "{reply:?}");
        assert!(reply.match_or_mint_answer.is_none() && reply.instrument.is_none());
        assert!(reply.detail.contains("404"), "{}", reply.detail);
        assert!(transport.urls()[0].ends_with("/api/v1/reference/escalate-instrument"));

        // Nothing unreachable is ever published as pulled reference data.
        let puller = crate::reference::ReferencePuller::new(
            bus.clone(),
            Arc::new(platform_with(Fake::new(vec![Ok(reply_404())]))),
            bus.clock(),
            Arc::default(),
        );
        let mut listed = bus.subscribe_every(crate::reference::LICENCE_LIST_PULLED);
        assert!(puller.pull_once().await.unwrap_err().is_outage());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listed.recv())
                .await
                .is_err()
        );
    }

    fn reply_404() -> crate::platform::Response {
        reply(404, "{}")
    }

    // ── Contract v28 (W3.33; rulings 757, 761) ────────────────────────────

    #[tokio::test]
    async fn the_pickers_search_is_sent_as_typed_and_answered_whole() {
        let bus = bus();
        let answered = serde_json::json!({
            "candidates": [
                {"instrument_id": "INS-01JSPY", "ticker": "SPY", "mic": "ARCX", "country": "US",
                 "name": "SPDR S&P 500 ETF Trust", "asset_class": "ASSET_CLASS_FUND",
                 "currency": "USD", "composite_figi": "BBG000BDTBL9",
                 "share_class_figi": "BBG001S72SM3", "identifiers": []},
                {"instrument_id": "", "ticker": "QQQ", "mic": "", "country": "US",
                 "name": "Invesco QQQ Trust", "asset_class": "ASSET_CLASS_UNSPECIFIED",
                 "currency": "", "composite_figi": "BBG000BSWKH7", "share_class_figi": "",
                 "identifiers": [{"scheme": "composite_figi", "value": "BBG000BSWKH7", "source": ""}]}
            ],
            "sources": [{"source": "https://api.openfigi.com", "outcome": "found",
                         "read_at": "2026-10-26T20:00:00Z"}]
        });
        let transport = Fake::new(vec![Ok(reply(200, &answered.to_string()))]);
        let conductor = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(transport.clone())),
            bus.clock(),
        );
        // An ISIN a person typed passes as typed (ruling 761), and nothing
        // else of the deployment's goes with it.
        let reply = conductor
            .candidates(&meridian_domain::v1::InstrumentCandidatesRequest {
                query: "  US78462F1030 ".into(),
                limit: 99,
                ..Default::default()
            })
            .await;
        assert!(reply.reachable, "{reply:?}");
        let answer = reply.answer.unwrap();
        assert_eq!(answer.candidates.len(), 2);
        assert_eq!(answer.candidates[0].instrument_id, "INS-01JSPY");
        assert_eq!(
            answer.candidates[0].asset_class,
            meridian_domain::v1::AssetClass::Fund as i32
        );
        assert_eq!(answer.candidates[1].identifiers[0].scheme, "composite_figi");
        assert_eq!(answer.sources[0].outcome, "found");
        assert!(transport.urls()[0].ends_with("/api/v1/reference/instrument-candidates"));
        let sent: serde_json::Value = serde_json::from_str(&transport.bodies()[0]).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({"query": "US78462F1030", "limit": 25})
        );
    }

    #[tokio::test]
    async fn a_platform_without_the_search_is_unreachable_and_said_so() {
        let bus = bus();
        let conductor = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(Fake::new(vec![Ok(reply_404())]))),
            bus.clock(),
        );
        let reply = conductor
            .candidates(&meridian_domain::v1::InstrumentCandidatesRequest {
                query: "SPY".into(),
                ..Default::default()
            })
            .await;
        assert!(!reply.reachable && reply.answer.is_none());
        assert!(reply.detail.contains("404"), "{}", reply.detail);
        // A blank search is not sent at all.
        let transport = Fake::new(vec![]);
        let conductor = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(transport.clone())),
            bus.clock(),
        );
        let reply = conductor
            .candidates(&meridian_domain::v1::InstrumentCandidatesRequest::default())
            .await;
        assert!(!reply.reachable);
        assert_eq!(transport.calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_platforms_match_or_mint_answer_is_carried_back_whole() {
        let bus = bus();
        let minted = serde_json::json!({
            "minted": true,
            "near_match": false,
            "instrument": serde_json::from_str::<serde_json::Value>(&record_json("INS-01J8ZZTP")).unwrap()["instrument"],
        })
        .to_string();
        let transport = Fake::new(vec![Ok(reply(200, &minted))]);
        let conductor = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(transport.clone())),
            bus.clock(),
        );
        let reply = conductor
            .ask(&matching(vec![figi(), cusip()]), "", "")
            .await;
        assert!(reply.reachable && reply.found);
        let answer = reply.match_or_mint_answer.unwrap();
        assert!(answer.minted);
        assert_eq!(answer.instrument.unwrap().instrument_id, "INS-01J8ZZTP");
        // The unlisted scheme never travels; the request key does.
        let body = &transport.bodies()[0];
        assert!(
            body.contains("share_class_figi") && body.contains("a1b2c3"),
            "{body}"
        );
        assert!(!body.contains("037833100"), "{body}");
        assert!(!body.contains("LCL-"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_scheme_the_licence_list_adds_is_sent_from_the_next_read() {
        let bus = bus();
        let pending = "{\"minted\": false, \"pending_review\": true}";
        let transport = Fake::new(vec![Ok(reply(200, pending))]);
        let licence: crate::reference::Licence = Arc::default();
        let conductor = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(transport.clone())),
            bus.clock(),
        )
        .with_licence(Arc::clone(&licence));
        // Before: CUSIP alone is nothing to look for, and nothing is sent.
        let before = conductor.ask(&matching(vec![cusip()]), "", "").await;
        assert!(before.reachable && !before.found && transport.calls() == 0);
        *licence.write().unwrap() = Some(meridian_domain::v1::LicenceList {
            version: 4,
            schemes: vec!["figi".into(), "cusip".into()],
            pace: None,
            administrators: Vec::new(),
        });
        let after = conductor.ask(&matching(vec![cusip()]), "", "").await;
        assert!(after.match_or_mint_answer.unwrap().pending_review);
        assert!(transport.bodies()[0].contains("037833100"));
    }

    #[tokio::test]
    async fn a_company_is_asked_by_a_cik_or_an_lei_on_the_list_alone() {
        // Contract v28 (W3.32; ruling 612).
        use meridian_domain::v1::{LicenceList, MatchOrMintCompanyRequest};
        let bus = bus();
        let answer = r#"{"company": {"company_id": "ENT-01JC28APPLEINC000000000001",
            "name": "Apple Inc.", "identifiers": [{"scheme": "cik", "value": "0000320193"}],
            "version": 1}, "minted": false}"#;
        let transport = Fake::new(vec![Ok(reply(200, answer))]);
        let licence: crate::reference::Licence = Arc::default();
        let conductor = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(transport.clone())),
            bus.clock(),
        )
        .with_licence(licence.clone());
        let id = |scheme: &str, value: &str, source: &str| PbIdentifier {
            scheme: scheme.into(),
            value: value.into(),
            source: source.into(),
            ..Default::default()
        };
        let asking = MatchOrMintCompanyRequest {
            identifiers: vec![
                id("cik", "0000320193", ""),
                id("symbol", "AAPL", "snaptrade"),
            ],
            name: String::new(),
            request_key: "k".into(),
        };
        // A list read that names no CIK: nothing to ask by.
        *licence.write().unwrap() = Some(LicenceList {
            version: 2,
            schemes: vec!["lei".into()],
            ..Default::default()
        });
        assert!(conductor.company(&asking).await.is_err());
        assert_eq!(transport.calls(), 0);
        // No list read: from C2 the CIK is among the open schemes (W3.25).
        *licence.write().unwrap() = None;
        let answered = conductor.company(&asking).await.unwrap();
        assert_eq!(
            answered.company.unwrap().company_id,
            "ENT-01JC28APPLEINC000000000001"
        );
        let body = &transport.bodies()[0];
        assert!(
            body.contains("0000320193") && !body.contains("AAPL"),
            "{body}"
        );
        // The platform not reached: the words, never a reply.
        let away = Conductor::new(
            bus.clone(),
            Arc::new(platform_with(Fake::new(vec![Ok(reply(404, "{}"))]))),
            bus.clock(),
        )
        .with_licence(licence);
        let refused = away.company(&asking).await.unwrap_err();
        assert!(refused.contains("could not be reached"), "{refused}");
    }
}
