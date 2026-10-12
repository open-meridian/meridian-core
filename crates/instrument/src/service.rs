//! Where the instrument store meets the bus.
//!
//! Queries and commands answered from the store: a plugin's resolve (W3.1) and
//! read (W3.6), and the dashboard's completion, merge, list and history for a
//! deployment admin (W3.10 to W3.13). Two events heard: a connector's
//! ambiguous miss, listed as a conflict (W3.2), and the platform's answer to a
//! person's ask, kept as offers and an added identifier (W3.3, W3.5). Every
//! new version of a record leaves as InstrumentApplied, and a merge's
//! replacement as InstrumentReplaced (W3.8).
//!
//! # Nothing here reaches the platform
//!
//! Every answer is from the store alone, so a resolve never waits on anything
//! outside this process. Asking the platform is a person's choice, carried by
//! the conductor, which alone holds the key (decisions/011, decisions/030).
//!
//! # One writer at a time
//!
//! A resolve that joins an identifier checks no other record carries it, and
//! then writes; a completion does the same. Writes go through one lock, so two
//! of them cannot both pass a check the other's write would have failed, and
//! each is version-gated in the store besides.

use std::sync::{Arc, Mutex};

use meridian_bus::{Bus, Delivery, Envelope};
use meridian_domain::v1::{
    CompleteInstrumentsReply, CompleteInstrumentsRequest, InstrumentAppliedEvent,
    ListInstrumentsToCompleteRequest, MergeInstrumentsReply, MergeInstrumentsRequest,
    MissingInstrumentDetectedEvent, PullInstrumentReply, ReadInstrumentHistoryRequest,
    ResolveIdentifierRequest, ResolveInstrumentRequest,
};
use prost::Message;

use crate::complete::{complete_as, keep_platform_answer, merge_as};
use crate::list::{history, list_to_complete};
use crate::record::to_wire;
use crate::replace::replaced;
use crate::resolve::{conflict_reported, resolve_instrument};
use crate::store::{Instrument, Store};

/// W3.1. A connector asking what a set of identifiers meant.
pub const RESOLVE_IDENTIFIER: &str = "platform.reference.query.resolve-identifier";

/// W3.6. A record, so a holding can be shown with a name and the book can read
/// its asset class and currency.
pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";

/// W3.2. A connector reporting that a resolve was ambiguous.
pub const INSTRUMENT_MISSING: &str = "platform.reference.event.instrument-missing";

/// W3.3. What the conductor got from the platform when a person asked.
pub const INSTRUMENT_PULLED: &str = "platform.reference.event.instrument-pulled";

/// W3.5. A record's new version.
pub const INSTRUMENT_APPLIED: &str = "platform.reference.event.instrument-applied";

/// W3.8. A record merged into another, for the stores keyed by instrument to
/// move what they hold (W3.9, W9.9).
pub const INSTRUMENT_REPLACED: &str = "platform.reference.event.instrument-replaced";

/// W3.10. The dashboard completing records for a deployment admin.
pub const COMPLETE_INSTRUMENTS: &str = "platform.reference.command.complete-instruments";

/// W3.13. The dashboard merging two records for a deployment admin.
pub const MERGE_INSTRUMENTS: &str = "platform.reference.command.merge-instruments";

/// W3.11. The records to complete, and the conflicts.
pub const LIST_INSTRUMENTS_TO_COMPLETE: &str =
    "platform.reference.query.list-instruments-to-complete";

/// W3.14 (contract v18). Which venue a set of identifiers names.
pub const RESOLVE_VENUE: &str = "platform.reference.query.resolve-venue";

/// W3.19 (contract v22). A calendar's days over a range.
pub const READ_CALENDAR: &str = "platform.reference.query.read-calendar";

/// W3.20 (contract v22). A date, worked out one way.
pub const COMPUTE_DATE: &str = "platform.reference.query.compute-date";

/// W3.21 (contract v22). The calendars the deployment holds, for a
/// deployment admin.
pub const LIST_CALENDARS: &str = "platform.reference.query.list-calendars";

/// W3.16 (contract v22). What the conductor pulled from the platform.
pub const CALENDAR_PULLED: &str = "platform.reference.event.calendar-pulled";

/// W3.17 (contract v22). A calendar's or a rule's version kept.
pub const CALENDAR_APPLIED: &str = "platform.reference.event.calendar-applied";

/// W3.18 (contract v22). A calendar or a rule not held, by its subject.
pub const CALENDAR_MISSING: &str = "platform.reference.event.calendar-missing";

/// How often the subjects held are named to the conductor to refresh
/// (W3.16): daily, and once soon after a start, so a conductor that started
/// since learns what the deployment holds.
pub const REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(86_400);
pub const REFRESH_FIRST_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// W3.12. A record's history.
pub const READ_INSTRUMENT_HISTORY: &str = "platform.reference.query.read-instrument-history";

/// Where the time comes from: the deployment's one clock (decisions/024),
/// given by whoever wires this up.
pub use meridian_clock::Clock;

/// The lock every write takes (see the module's note).
pub type Writing = Arc<Mutex<()>>;

/// Register the queries and commands on the bus.
pub fn serve_queries(bus: &Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    serve_all(bus, store, clock, Arc::new(Mutex::new(())));
}

/// As [`serve_queries`], sharing `writing` with the [`Reactor`].
pub fn serve_all(bus: &Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>, writing: Writing) {
    {
        let (store, clock, writing, announcing) = (
            store.clone(),
            clock.clone(),
            writing.clone(),
            Arc::clone(bus),
        );
        let asking = Arc::new(Asking::new(Arc::clone(bus)));
        bus.serve(RESOLVE_IDENTIFIER, move |envelope| {
            let request: ResolveIdentifierRequest =
                decode(&envelope, "meridian.v1.ResolveIdentifierRequest")?;
            let instance = envelope
                .meta
                .as_ref()
                .map(|meta| meta.publisher_instance_id.clone())
                .unwrap_or_default();
            let now_ns = clock.now_ns();
            // Read-only for an instance none of whose launched roles may mint
            // (contract v20, W3.1): `reporting`, by the launch records, never
            // by anything the plugin sends.
            if asking.reads_only(&instance, now_ns) {
                let resolution =
                    crate::resolve::resolve_identifier_read_only(store.as_ref(), &request, now_ns)
                        .map_err(|failed| failed.to_string())?;
                return Ok((
                    "meridian.v1.ResolveIdentifierReply".to_string(),
                    resolution.reply.encode_to_vec(),
                ));
            }
            // A currency the deployment does not hold, pulled from the
            // platform's master before anything is minted (contract v20);
            // while the master is slow or not reached, a miss and nothing
            // minted (the 2026-10-11 ruling), the next resolve finding what
            // the pull brought.
            let pulled = match crate::resolve::currency_code(&request) {
                Some(code)
                    if crate::resolve::resolve_identifier_read_only(
                        store.as_ref(),
                        &request,
                        now_ns,
                    )
                    .map(|found| found.reply.instrument_id.is_empty())
                    .unwrap_or(false) =>
                {
                    match asking.currency(&code, now_ns) {
                        Pull::Found(record) => Some(*record),
                        Pull::NotHeld => None,
                        Pull::NotYet => {
                            let resolution = crate::resolve::resolve_identifier_read_only(
                                store.as_ref(),
                                &request,
                                now_ns,
                            )
                            .map_err(|failed| failed.to_string())?;
                            return Ok((
                                "meridian.v1.ResolveIdentifierReply".to_string(),
                                resolution.reply.encode_to_vec(),
                            ));
                        }
                    }
                }
                _ => None,
            };
            let resolution = {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                crate::resolve::resolve_identifier_pulled(
                    store.as_ref(),
                    &request,
                    &instance,
                    now_ns,
                    pulled.as_ref(),
                )
                .map_err(|failed| failed.to_string())?
            };
            if let Some(changed) = &resolution.changed {
                announce(&announcing, changed, &envelope, now_ns);
            }
            Ok((
                "meridian.v1.ResolveIdentifierReply".to_string(),
                resolution.reply.encode_to_vec(),
            ))
        });
    }
    serve_calendars(bus, store.clone(), clock.clone());
    {
        let (store, clock) = (store.clone(), clock.clone());
        bus.serve(RESOLVE_VENUE, move |envelope| {
            let request: meridian_domain::v1::ResolveVenueRequest =
                decode(&envelope, "meridian.v1.ResolveVenueRequest")?;
            if request.identifiers.is_empty() {
                return Err("identifiers: at least one names the venue".into());
            }
            let reply = crate::venues::resolve_venue(store.as_ref(), &request, clock.now_ns())
                .map_err(|failed| failed.to_string())?;
            Ok((
                "meridian.v1.ResolveVenueReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }
    {
        let store = store.clone();
        bus.serve(RESOLVE_INSTRUMENT, move |envelope| {
            let request: ResolveInstrumentRequest =
                decode(&envelope, "meridian.v1.ResolveInstrumentRequest")?;
            let reply = resolve_instrument(store.as_ref(), &request)
                .map_err(|failed| failed.to_string())?;
            Ok((
                "meridian.v1.ResolveInstrumentReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }
    {
        let (store, clock, writing, announcing) = (
            store.clone(),
            clock.clone(),
            writing.clone(),
            Arc::clone(bus),
        );
        bus.serve(COMPLETE_INSTRUMENTS, move |envelope| {
            let request: CompleteInstrumentsRequest =
                decode(&envelope, "meridian.v1.CompleteInstrumentsRequest")?;
            let actor = actor_of(&envelope);
            let person = actor.person.clone();
            let now_ns = clock.now_ns();
            let done = {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                complete_as(store.as_ref(), &request, &actor, now_ns)
                    .map_err(|failed| failed.to_string())?
            }
            .map_err(|refused| meridian_bus::refusal(refused.reason as i32, refused.words))?;
            for changed in &done.changed {
                announce(&announcing, changed, &envelope, now_ns);
            }
            tracing::info!(
                by = person,
                completed = done.changed.len(),
                asked = request.completions.len(),
                "instrument records completed"
            );
            Ok((
                "meridian.v1.CompleteInstrumentsReply".to_string(),
                CompleteInstrumentsReply {
                    results: done.results,
                }
                .encode_to_vec(),
            ))
        });
    }
    {
        let (store, clock, writing, announcing) =
            (store.clone(), clock.clone(), writing, Arc::clone(bus));
        bus.serve(MERGE_INSTRUMENTS, move |envelope| {
            let request: MergeInstrumentsRequest =
                decode(&envelope, "meridian.v1.MergeInstrumentsRequest")?;
            let actor = actor_of(&envelope);
            let person = actor.person.clone();
            let now_ns = clock.now_ns();
            let merged = {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                let merged = merge_as(store.as_ref(), &request, &actor, now_ns)
                    .map_err(|failed| failed.to_string())?
                    .map_err(|refused| {
                        meridian_bus::refusal(refused.reason as i32, refused.words)
                    })?;
                // Recorded before it is announced: a store keyed by instrument
                // that misses the event finds it by its sweep, which asks what
                // each ID it holds has become.
                store
                    .replace(
                        &merged.merged.instrument_id,
                        &merged.stays.instrument_id,
                        now_ns,
                    )
                    .map_err(|failed| failed.to_string())?;
                merged
            };
            let meta = envelope.meta.as_ref();
            if let Err(failed) = announcing.publish(
                INSTRUMENT_REPLACED,
                "meridian.v1.InstrumentReplacedEvent",
                replaced(&merged.merged.instrument_id, &merged.stays, now_ns).encode_to_vec(),
                meta.map(|meta| meta.correlation_id.as_str()),
                meta.map(|meta| meta.message_id.as_str()),
            ) {
                tracing::warn!(%failed, "a merge's replacement could not be announced; the sweeps find it");
            }
            announce(&announcing, &merged.stays, &envelope, now_ns);
            announce(&announcing, &merged.merged, &envelope, now_ns);
            tracing::info!(
                by = person,
                kept = merged.stays.instrument_id,
                merged = merged.merged.instrument_id,
                "instrument records merged"
            );
            Ok((
                "meridian.v1.MergeInstrumentsReply".to_string(),
                MergeInstrumentsReply {
                    instrument: Some(to_wire(&merged.stays)),
                }
                .encode_to_vec(),
            ))
        });
    }
    {
        let store = store.clone();
        bus.serve(LIST_INSTRUMENTS_TO_COMPLETE, move |envelope| {
            let request: ListInstrumentsToCompleteRequest =
                decode(&envelope, "meridian.v1.ListInstrumentsToCompleteRequest")?;
            let reply =
                list_to_complete(store.as_ref(), &request).map_err(|failed| failed.to_string())?;
            Ok((
                "meridian.v1.ListInstrumentsToCompleteReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }
    bus.serve(READ_INSTRUMENT_HISTORY, move |envelope| {
        let request: ReadInstrumentHistoryRequest =
            decode(&envelope, "meridian.v1.ReadInstrumentHistoryRequest")?;
        let reply = history(store.as_ref(), &request).map_err(|failed| failed.to_string())?;
        Ok((
            "meridian.v1.ReadInstrumentHistoryReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}

/// W3.5. Announce a record's new version, in the chain that caused it. A
/// failure is logged and does not fail what caused it: the record is stored,
/// and the dashboard reads it again when it next draws.
fn announce(bus: &Bus, record: &Instrument, caused_by: &Envelope, now_ns: i64) {
    let meta = caused_by.meta.as_ref();
    if let Err(failed) = bus.publish(
        INSTRUMENT_APPLIED,
        "meridian.v1.InstrumentAppliedEvent",
        InstrumentAppliedEvent {
            instrument: Some(to_wire(record)),
            applied: true,
            applied_at_ns: now_ns,
        }
        .encode_to_vec(),
        meta.map(|meta| meta.correlation_id.as_str()),
        meta.map(|meta| meta.message_id.as_str()),
    ) {
        tracing::warn!(%failed, instrument = record.instrument_id, "a record's new version could not be announced");
    }
}

/// The conductor's catalogue, the launch records (W8.2).
pub const PLUGIN_CATALOGUE: &str = "platform.config.query.plugin-catalogue";
/// W3.3, asked by the store itself for a currency (contract v20).
pub const ASK_PLATFORM_FOR_INSTRUMENT: &str =
    "platform.reference.query.ask-platform-for-instrument";

/// The roles that may mint a record by resolving (W3.1, W3.7): `custody`
/// and `data`. Any other asks read-only.
pub const MINTING_ROLES: [&str; 2] = ["custody", "data"];

const LAUNCHES_FOR_NS: i64 = 300 * 1_000_000_000;
const UNKNOWN_AGAIN_NS: i64 = 30 * 1_000_000_000;
const NOT_HELD_AGAIN_NS: i64 = 600 * 1_000_000_000;
/// How long a resolve waits on the master for a currency: well under the
/// five seconds a sidecar gives a resolve (the coordinator, 2026-10-11).
pub const CURRENCY_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);
/// How long the pull itself may take in the background.
const CURRENCY_PULL_WITHIN: std::time::Duration = std::time::Duration::from_secs(20);
/// After the master could not be reached, a currency is asked again no
/// sooner than this.
const UNREACHED_AGAIN_NS: i64 = 10 * 1_000_000_000;
/// A pull still in flight after this is asked again.
const IN_FLIGHT_FOR_NS: i64 = 60 * 1_000_000_000;

/// What the master said of a currency the deployment does not hold
/// (contract v20; the 2026-10-11 ruling: never mint a stub when the master
/// holds it).
#[derive(Debug, Clone, PartialEq)]
pub enum Pull {
    /// The master's record, kept as the deployment's.
    Found(Box<meridian_domain::v1::InstrumentRecord>),
    /// The master answered and holds none: minted as before.
    NotHeld,
    /// Not known yet -- the master slow or not reached: nothing minted, the
    /// resolve answered as a miss, the pull finishing in the background so
    /// a later resolve finds the record.
    NotYet,
}

#[derive(Debug, Clone)]
enum Pulling {
    InFlight(i64),
    Found(Box<meridian_domain::v1::InstrumentRecord>),
    Unreached(i64),
}

/// What the store asks others, from a resolve's handler on the blocking
/// pool: each launched instance's roles, from the catalogue, read again
/// every five minutes or for an instance not seen; and the master, for a
/// currency not held, each code it does not hold not asked again for ten
/// minutes.
pub struct Asking {
    bus: Arc<Bus>,
    launched: Mutex<(std::collections::BTreeMap<String, Vec<String>>, i64)>,
    not_held: Mutex<std::collections::BTreeMap<String, i64>>,
    pulling: Arc<Mutex<std::collections::BTreeMap<String, Pulling>>>,
    wait: std::time::Duration,
}

impl Asking {
    pub fn new(bus: Arc<Bus>) -> Self {
        Self {
            bus,
            launched: Mutex::new((Default::default(), i64::MIN / 2)),
            not_held: Mutex::new(Default::default()),
            pulling: Arc::default(),
            wait: CURRENCY_WAIT,
        }
    }

    /// The same, waiting `wait` on the master (for the tests).
    pub fn waiting(bus: Arc<Bus>, wait: std::time::Duration) -> Self {
        Self {
            wait,
            ..Self::new(bus)
        }
    }

    fn ask<R: Message + Default>(
        &self,
        topic: &str,
        request_type: &str,
        request: impl Message,
    ) -> Result<R, String> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| "no runtime".to_string())?;
        let (_, bytes) = handle
            .block_on(self.bus.call(
                topic,
                request_type,
                request.encode_to_vec(),
                None,
                Some(std::time::Duration::from_secs(10)),
            ))
            .map_err(|failed| failed.to_string())?;
        R::decode(&bytes[..]).map_err(|failed| failed.to_string())
    }

    /// Whether `instance` resolves read-only: launched with roles, none of
    /// them minting. An instance the launch records do not name -- one run
    /// outside the catalogue, or a catalogue not read -- resolves as before.
    pub fn reads_only(&self, instance: &str, now_ns: i64) -> bool {
        match self.roles_of(instance, now_ns) {
            Some(roles) => {
                !roles.is_empty()
                    && !roles
                        .iter()
                        .any(|role| MINTING_ROLES.contains(&role.as_str()))
            }
            None => false,
        }
    }

    fn roles_of(&self, instance: &str, now_ns: i64) -> Option<Vec<String>> {
        {
            let held = self.launched.lock().ok()?;
            let known = held.0.contains_key(instance);
            let fresh = now_ns - held.1 < LAUNCHES_FOR_NS;
            if fresh && (known || now_ns - held.1 < UNKNOWN_AGAIN_NS) {
                return held.0.get(instance).cloned();
            }
        }
        let read: Result<meridian_domain::v1::PluginCatalogue, String> = self.ask(
            PLUGIN_CATALOGUE,
            "meridian.v1.PluginCatalogueRequest",
            meridian_domain::v1::PluginCatalogueRequest {},
        );
        let mut held = self.launched.lock().ok()?;
        if let Ok(catalogue) = read {
            held.0 = catalogue
                .launches
                .into_iter()
                .filter(|launch| {
                    launch.state == meridian_domain::v1::PluginLaunchState::Launched as i32
                })
                .map(|launch| {
                    (
                        launch.instance_id,
                        meridian_domain::roles_now(&launch.roles),
                    )
                })
                .collect();
        }
        held.1 = now_ns;
        held.0.get(instance).cloned()
    }

    /// What the master says of a currency the deployment does not hold,
    /// waiting no longer than [`CURRENCY_WAIT`]: its record, that it holds
    /// none, or not yet -- the pull left to finish in the background, its
    /// record kept for the next resolve.
    pub fn currency(&self, code: &str, now_ns: i64) -> Pull {
        if let Ok(not_held) = self.not_held.lock() {
            if not_held
                .get(code)
                .is_some_and(|at| now_ns - at < NOT_HELD_AGAIN_NS)
            {
                return Pull::NotHeld;
            }
        }
        let Ok(mut pulling) = self.pulling.lock() else {
            return Pull::NotYet;
        };
        match pulling.get(code).cloned() {
            Some(Pulling::Found(record)) => {
                pulling.remove(code);
                return Pull::Found(record);
            }
            Some(Pulling::InFlight(at)) if now_ns - at < IN_FLIGHT_FOR_NS => return Pull::NotYet,
            Some(Pulling::Unreached(at)) if now_ns - at < UNREACHED_AGAIN_NS => {
                return Pull::NotYet
            }
            _ => {}
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Pull::NotYet;
        };
        pulling.insert(code.to_string(), Pulling::InFlight(now_ns));
        drop(pulling);
        let (done, heard) = std::sync::mpsc::channel::<Option<bool>>();
        let (bus, kept, code_owned) = (
            Arc::clone(&self.bus),
            Arc::clone(&self.pulling),
            code.to_string(),
        );
        let request = meridian_domain::v1::AskPlatformForInstrumentRequest {
            instrument_id: String::new(),
            identifiers: vec![meridian_domain::v1::Identifier {
                scheme: "iso4217".into(),
                value: code.to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        handle.spawn(async move {
            let asked = bus
                .call(
                    ASK_PLATFORM_FOR_INSTRUMENT,
                    "meridian.v1.AskPlatformForInstrumentRequest",
                    request.encode_to_vec(),
                    None,
                    Some(CURRENCY_PULL_WITHIN),
                )
                .await
                .map_err(|failed| failed.to_string())
                .and_then(|(_, bytes)| {
                    meridian_domain::v1::AskPlatformForInstrumentReply::decode(&bytes[..])
                        .map_err(|failed| failed.to_string())
                });
            // Some(true): found; Some(false): the master holds none; None:
            // not reached.
            let said = match asked {
                Ok(reply) if reply.found && reply.instrument.is_some() => {
                    tracing::info!(
                        code = code_owned,
                        "a currency the deployment did not hold, pulled from the master"
                    );
                    if let Ok(mut kept) = kept.lock() {
                        kept.insert(
                            code_owned.clone(),
                            Pulling::Found(Box::new(reply.instrument.expect("found"))),
                        );
                    }
                    Some(true)
                }
                Ok(reply) if reply.reachable => {
                    if let Ok(mut kept) = kept.lock() {
                        kept.remove(&code_owned);
                    }
                    Some(false)
                }
                Ok(reply) => {
                    tracing::info!(code = code_owned, detail = reply.detail, "the master could not be asked for a currency; nothing minted, asked again later");
                    if let Ok(mut kept) = kept.lock() {
                        kept.insert(code_owned.clone(), Pulling::Unreached(now_ns));
                    }
                    None
                }
                Err(failed) => {
                    tracing::info!(code = code_owned, %failed, "the master could not be asked for a currency; nothing minted, asked again later");
                    if let Ok(mut kept) = kept.lock() {
                        kept.insert(code_owned.clone(), Pulling::Unreached(now_ns));
                    }
                    None
                }
            };
            let _ = done.send(said);
        });
        match heard.recv_timeout(self.wait) {
            Ok(Some(true)) => match self.pulling.lock() {
                Ok(mut pulling) => match pulling.remove(code) {
                    Some(Pulling::Found(record)) => Pull::Found(record),
                    _ => Pull::NotYet,
                },
                Err(_) => Pull::NotYet,
            },
            Ok(Some(false)) => {
                if let Ok(mut not_held) = self.not_held.lock() {
                    not_held.insert(code.to_string(), now_ns);
                }
                Pull::NotHeld
            }
            Ok(None) | Err(_) => Pull::NotYet,
        }
    }
}

/// Who a command was sent for, as the dashboard stamped them: the person,
/// and the delegation and client they acted through, if any (W4.9).
fn actor_of(envelope: &Envelope) -> crate::complete::Actor {
    envelope
        .meta
        .as_ref()
        .map(|meta| crate::complete::Actor {
            person: meta.acting_for_subject.clone(),
            // A delegation stands only beside a person, never alone.
            delegation: if meta.acting_for_subject.is_empty() {
                String::new()
            } else {
                meta.acting_through_delegation.clone()
            },
            client: if meta.acting_for_subject.is_empty()
                || meta.acting_through_delegation.is_empty()
            {
                String::new()
            } else {
                meta.acting_through_client.clone()
            },
            prepared_call_id: if meta.acting_for_subject.is_empty() {
                String::new()
            } else {
                meta.prepared_call_id.clone()
            },
        })
        .unwrap_or_default()
}

/// What one delivery came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Handled {
    /// Not a message this reads, or not a readable one. Nothing was written.
    ///
    /// A payload arriving under the wrong type name is refused rather than
    /// decoded: protobuf will happily read one message as another and hand back
    /// defaults.
    Ignored(String),

    /// The platform's answer was kept on the record it names, at a new
    /// version; or `false`, it changed nothing.
    Kept(bool),

    /// A connector's ambiguous miss was listed as a conflict, or `false`, its
    /// identifiers meet fewer than two records.
    Listed(bool),
}

/// Hears the platform's answers and connectors' ambiguous misses.
pub struct Reactor {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    writing: Writing,
}

impl Reactor {
    pub fn new(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) -> Self {
        Self::sharing(bus, store, clock, Arc::new(Mutex::new(())))
    }

    /// As [`Reactor::new`], sharing the lock [`serve_all`] writes under.
    pub fn sharing(
        bus: Arc<Bus>,
        store: Arc<dyn Store>,
        clock: Arc<dyn Clock>,
        writing: Writing,
    ) -> Self {
        Self {
            bus,
            store,
            clock,
            writing,
        }
    }

    /// Consume both subscriptions until the bus shuts down.
    ///
    /// Taken by the caller before anything is published: at-most-once
    /// delivery drops what arrives before a subscriber exists, silently.
    pub async fn consume(
        self,
        mut pulled: meridian_bus::Subscription,
        mut missing: meridian_bus::Subscription,
    ) {
        loop {
            let delivery = tokio::select! {
                next = pulled.recv() => next,
                next = missing.recv() => next,
            };
            let Some(delivery) = delivery else { return };
            match self.react(delivery).await {
                Handled::Ignored(why) => tracing::warn!(why, "ignored a delivery"),
                Handled::Kept(changed) => {
                    tracing::info!(changed, "kept the platform's answer to a person's ask")
                }
                Handled::Listed(listed) => tracing::debug!(listed, "heard an ambiguous miss"),
            }
        }
    }

    /// Act on one delivery. Public so a test can drive it without a loop.
    pub async fn react(&self, delivery: Delivery) -> Handled {
        let envelope = delivery.envelope;
        let now_ns = self.clock.now_ns();
        match envelope.payload_type.as_str() {
            "meridian.v1.PullInstrumentReply" => {
                let reply = match PullInstrumentReply::decode(&envelope.payload[..]) {
                    Ok(reply) => reply,
                    Err(failed) => {
                        return Handled::Ignored(format!("undecodable answer: {failed}"))
                    }
                };
                // The venues the answer carries, kept whatever else it
                // carries (W3.5, contract v18): a venue asked about by its
                // codes arrives with no instrument (W3.15).
                if !reply.venues.is_empty() {
                    let (store, venues) = (self.store.clone(), reply.venues.clone());
                    let kept = tokio::task::spawn_blocking(move || {
                        venues
                            .iter()
                            .map(|venue| store.keep_venue(venue, now_ns))
                            .collect::<crate::Result<Vec<bool>>>()
                    })
                    .await;
                    match kept {
                        Ok(Ok(kept)) => tracing::info!(
                            kept = kept.iter().filter(|k| **k).count(),
                            carried = kept.len(),
                            "venues the platform answered with"
                        ),
                        Ok(Err(failed)) => tracing::warn!(%failed, "venues were not kept"),
                        Err(failed) => tracing::warn!(%failed, "venues were not kept"),
                    }
                }
                let Some(record) = reply.instrument.filter(|_| reply.found) else {
                    return if reply.venues.is_empty() {
                        Handled::Ignored("an answer carrying no record".into())
                    } else {
                        Handled::Kept(true)
                    };
                };
                if reply.for_instrument_id.is_empty() {
                    // Before v10 a pulled record was applied as a record of
                    // its own; from v10 it is kept only on the record a
                    // person asked about.
                    return Handled::Ignored(
                        "an answer naming no record of the deployment's".into(),
                    );
                }
                let (store, writing) = (self.store.clone(), self.writing.clone());
                let kept = tokio::task::spawn_blocking(move || {
                    let _held = writing
                        .lock()
                        .map_err(|_| "the write lock is poisoned".to_string())?;
                    keep_platform_answer(store.as_ref(), &reply.for_instrument_id, &record, now_ns)
                        .map_err(|failed| failed.to_string())
                })
                .await;
                match kept {
                    Ok(Ok(Some(changed))) => {
                        announce(&self.bus, &changed, &envelope, now_ns);
                        Handled::Kept(true)
                    }
                    Ok(Ok(None)) => Handled::Kept(false),
                    Ok(Err(failed)) => Handled::Ignored(failed),
                    Err(failed) => Handled::Ignored(format!("the keeping task failed: {failed}")),
                }
            }
            "meridian.v1.MissingInstrumentDetectedEvent" => {
                let event = match MissingInstrumentDetectedEvent::decode(&envelope.payload[..]) {
                    Ok(event) => event,
                    Err(failed) => return Handled::Ignored(format!("undecodable miss: {failed}")),
                };
                let store = self.store.clone();
                match tokio::task::spawn_blocking(move || {
                    conflict_reported(store.as_ref(), &event, now_ns)
                })
                .await
                {
                    Ok(Ok(listed)) => Handled::Listed(listed),
                    Ok(Err(failed)) => Handled::Ignored(failed.to_string()),
                    Err(failed) => Handled::Ignored(format!("the listing task failed: {failed}")),
                }
            }
            other => Handled::Ignored(format!("payload type {other}")),
        }
    }
}

/// What a date read needs of an instrument the store holds: its asset class,
/// and its market's country and venue from the venue it is listed on.
fn market_of(store: &dyn Store, instrument_id: &str) -> Option<crate::calendars::Market> {
    let instrument = store.by_id(instrument_id).ok().flatten()?;
    let venue = (!instrument.listing_venue_id.is_empty())
        .then(|| {
            store
                .venues()
                .ok()?
                .into_iter()
                .find(|venue| venue.venue_id == instrument.listing_venue_id)
        })
        .flatten();
    Some(crate::calendars::Market {
        asset_class: crate::record::asset_class_value(&instrument.asset_class),
        country_code: venue
            .as_ref()
            .map(|venue| venue.country_code.clone())
            .unwrap_or_default(),
        venue_id: instrument.listing_venue_id.clone(),
    })
}

/// A calendar or a rule not held, reported by its subject for the
/// conductor to pull (W3.18).
fn report_missing(
    bus: &Bus,
    missing: &meridian_domain::v1::MissingCalendarDetectedEvent,
    caused_by: &Envelope,
) {
    let meta = caused_by.meta.as_ref();
    if let Err(failed) = bus.publish(
        CALENDAR_MISSING,
        "meridian.v1.MissingCalendarDetectedEvent",
        missing.encode_to_vec(),
        meta.map(|meta| meta.correlation_id.as_str()),
        meta.map(|meta| meta.message_id.as_str()),
    ) {
        tracing::warn!(%failed, "a calendar not held could not be reported");
    }
}

/// The calendar read, the date read and the list (W3.19 to W3.21, contract
/// v22): reference reads, answered whatever the asker's scope, from the
/// versions held as of the recorded time asked for.
fn serve_calendars(bus: &Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    let held = |store: &dyn Store, as_of_ns: i64| -> Result<crate::calendars::Held, String> {
        let calendars = store
            .calendar_versions()
            .map_err(|failed| failed.to_string())?;
        let rules = store
            .date_rule_versions()
            .map_err(|failed| failed.to_string())?;
        Ok(crate::calendars::Held::as_of(&calendars, &rules, as_of_ns))
    };
    {
        let (store, clock, reporting) = (store.clone(), clock.clone(), Arc::clone(bus));
        bus.serve(READ_CALENDAR, move |envelope| {
            let request: meridian_domain::v1::ReadCalendarRequest =
                decode(&envelope, "meridian.v1.ReadCalendarRequest")?;
            let now_ns = clock.now_ns();
            let held = held(store.as_ref(), request.as_of_ns)?;
            let (reply, missing) = crate::calendars::read(&held, &request, now_ns)?;
            if let Some(missing) = &missing {
                report_missing(&reporting, missing, &envelope);
            }
            Ok((
                "meridian.v1.ReadCalendarReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }
    {
        let (store, clock, reporting) = (store.clone(), clock.clone(), Arc::clone(bus));
        bus.serve(COMPUTE_DATE, move |envelope| {
            let request: meridian_domain::v1::ComputeDateRequest =
                decode(&envelope, "meridian.v1.ComputeDateRequest")?;
            let now_ns = clock.now_ns();
            let held = held(store.as_ref(), request.as_of_ns)?;
            let answered = crate::calendars::compute(
                &held,
                &request,
                |instrument| market_of(store.as_ref(), instrument),
                now_ns,
            )?;
            if let Some(missing) = &answered.missing {
                report_missing(&reporting, missing, &envelope);
            }
            Ok((
                "meridian.v1.ComputeDateReply".to_string(),
                answered.reply.encode_to_vec(),
            ))
        });
    }
    {
        let store = store.clone();
        bus.serve(LIST_CALENDARS, move |envelope| {
            let request: meridian_domain::v1::ListCalendarsRequest =
                decode(&envelope, "meridian.v1.ListCalendarsRequest")?;
            let calendars = store
                .calendar_versions()
                .map_err(|failed| failed.to_string())?;
            let rules = store
                .date_rule_versions()
                .map_err(|failed| failed.to_string())?;
            let reply = crate::calendars::list(&calendars, &rules, &request)?;
            Ok((
                "meridian.v1.ListCalendarsReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }
}

/// Keep each calendar and rule version the conductor pulled, announcing
/// each one kept (W3.16, W3.17); and name the subjects held to the conductor
/// soon after a start and daily, for it to ask the platform for versions
/// after those held. Subscribed before this returns; the loop is what is
/// left to await.
pub fn keep_calendars(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
) -> impl std::future::Future<Output = ()> {
    let mut pulled = bus.subscribe(CALENDAR_PULLED);
    async move {
        let refreshing = {
            let (bus, store, clock) = (Arc::clone(&bus), store.clone(), clock.clone());
            async move {
                tokio::time::sleep(REFRESH_FIRST_AFTER).await;
                let mut every = tokio::time::interval(REFRESH_EVERY);
                loop {
                    every.tick().await;
                    let (store, now_ns) = (store.clone(), clock.now_ns());
                    let subjects = tokio::task::spawn_blocking(move || {
                        let calendars = store.calendar_versions()?;
                        let rules = store.date_rule_versions()?;
                        Ok::<_, crate::StoreError>(crate::calendars::held_subjects(
                            &calendars, &rules, now_ns,
                        ))
                    })
                    .await;
                    match subjects {
                        Ok(Ok(subjects)) => {
                            for subject in subjects {
                                if let Err(failed) = bus.publish(
                                    CALENDAR_MISSING,
                                    "meridian.v1.MissingCalendarDetectedEvent",
                                    subject.encode_to_vec(),
                                    None,
                                    None,
                                ) {
                                    tracing::warn!(%failed, "a held calendar was not named for refreshing");
                                }
                            }
                        }
                        Ok(Err(failed)) => {
                            tracing::warn!(%failed, "the calendars held were not read")
                        }
                        Err(failed) => tracing::warn!(%failed, "the calendars held were not read"),
                    }
                }
            }
        };
        let keeping = async move {
            while let Some(delivery) = pulled.recv().await {
                let envelope = delivery.envelope;
                let reply: meridian_domain::v1::PullCalendarReply =
                    match decode(&envelope, "meridian.v1.PullCalendarReply") {
                        Ok(reply) => reply,
                        Err(failed) => {
                            tracing::warn!(%failed, "a pulled calendar did not read");
                            continue;
                        }
                    };
                let (store, now_ns) = (store.clone(), clock.now_ns());
                let kept = tokio::task::spawn_blocking(move || {
                    keep_pulled(store.as_ref(), &reply, now_ns)
                })
                .await;
                let applied = match kept {
                    Ok(Ok(applied)) => applied,
                    Ok(Err(failed)) => {
                        tracing::warn!(%failed, "a pulled calendar was not kept");
                        continue;
                    }
                    Err(failed) => {
                        tracing::warn!(%failed, "a pulled calendar was not kept");
                        continue;
                    }
                };
                let meta = envelope.meta.as_ref();
                for event in applied {
                    if let Err(failed) = bus.publish(
                        CALENDAR_APPLIED,
                        "meridian.v1.CalendarAppliedEvent",
                        event.encode_to_vec(),
                        meta.map(|meta| meta.correlation_id.as_str()),
                        meta.map(|meta| meta.message_id.as_str()),
                    ) {
                        tracing::warn!(%failed, "a calendar's version kept could not be announced");
                    }
                }
            }
        };
        tokio::join!(refreshing, keeping);
    }
}

/// Each version the platform's answer carried kept, a version already held
/// changing nothing; the announcement of each one kept. A record that is not
/// the platform's -- no `CAL-` ID, no version, a rule naming no kind or
/// subject -- is left out and said.
pub fn keep_pulled(
    store: &dyn Store,
    reply: &meridian_domain::v1::PullCalendarReply,
    now_ns: i64,
) -> crate::Result<Vec<meridian_domain::v1::CalendarAppliedEvent>> {
    let mut applied = Vec::new();
    for record in &reply.calendars {
        if !record.calendar_id.starts_with("CAL-") || record.version < 1 {
            tracing::warn!(
                calendar = record.calendar_id,
                version = record.version,
                "a pulled calendar is not the platform's; not kept"
            );
            continue;
        }
        if store.keep_calendar(record, now_ns)? {
            applied.push(meridian_domain::v1::CalendarAppliedEvent {
                calendar_id: record.calendar_id.clone(),
                version: record.version,
                rule: None,
                applied_at_ns: now_ns,
            });
        }
    }
    for rule in &reply.rules {
        let kind = meridian_pb::v1::DateRuleKind::try_from(rule.kind)
            .unwrap_or(meridian_pb::v1::DateRuleKind::Unspecified);
        if kind == meridian_pb::v1::DateRuleKind::Unspecified
            || rule.subject.is_empty()
            || rule.version < 1
            || rule.parameters.is_none()
        {
            tracing::warn!(
                subject = rule.subject,
                version = rule.version,
                "a pulled rule is not the platform's; not kept"
            );
            continue;
        }
        if store.keep_date_rule(rule, now_ns)? {
            applied.push(meridian_domain::v1::CalendarAppliedEvent {
                calendar_id: String::new(),
                version: rule.version,
                rule: Some(meridian_pb::v1::DateRuleRef {
                    kind: rule.kind,
                    subject: rule.subject.clone(),
                    version: rule.version,
                }),
                applied_at_ns: now_ns,
            });
        }
    }
    Ok(applied)
}

fn decode<M: Message + Default>(envelope: &Envelope, wanted: &str) -> Result<M, String> {
    if envelope.payload_type != wanted {
        return Err(format!("expected {wanted}, got {}", envelope.payload_type));
    }
    M::decode(&envelope.payload[..]).map_err(|failed| format!("undecodable {wanted}: {failed}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use meridian_bus::{MemoryBackend, Stamp};
    use meridian_domain::v1::{
        instrument_value, AssetClass, CompleteInstrumentsReply, Identifier as PbIdentifier,
        InstrumentCompletion, InstrumentRecord as PbInstrument, InstrumentReplacedEvent,
        InstrumentValue, ListInstrumentsToCompleteReply, ResolveIdentifierReply,
        ResolveInstrumentReply,
    };

    use super::*;
    use crate::MemoryStore;

    const NOW: i64 = 1_757_376_000_000_000_000;

    fn bus() -> Arc<Bus> {
        Arc::new(Bus::single(
            "reference-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::ManualClock::at(NOW)),
        ))
    }

    async fn ask<R: Message + Default>(
        bus: &Bus,
        topic: &str,
        kind: &str,
        message: impl Message,
        person: &str,
    ) -> Result<R, String> {
        let stamp = Stamp {
            acting_for_subject: person.into(),
            ..Stamp::default()
        };
        let (_, bytes) = bus
            .call_stamped(
                topic,
                kind,
                message.encode_to_vec(),
                None,
                Some(Duration::from_secs(5)),
                &stamp,
            )
            .await
            .map_err(|failed| failed.to_string())?;
        Ok(R::decode(&bytes[..]).unwrap())
    }

    /// A store's bus and a plugin instance's beside it, one backend.
    /// One in-memory bus, asking as `instance` (its handlers its own).
    fn two(instance: &str) -> (Arc<Bus>, Arc<Bus>) {
        let clock: Arc<dyn meridian_clock::Clock> = Arc::new(meridian_clock::ManualClock::at(NOW));
        let bus = Arc::new(Bus::single(instance, Arc::new(MemoryBackend::new()), clock));
        (bus.clone(), bus)
    }

    fn launched(bus: &Bus, launches: &[(&str, &[&str])]) {
        let launches: Vec<meridian_domain::v1::PluginLaunch> = launches
            .iter()
            .map(|(instance, roles)| meridian_domain::v1::PluginLaunch {
                instance_id: instance.to_string(),
                roles: roles.iter().map(|r| r.to_string()).collect(),
                state: meridian_domain::v1::PluginLaunchState::Launched as i32,
                ..Default::default()
            })
            .collect();
        bus.serve(PLUGIN_CATALOGUE, move |_| {
            Ok((
                "meridian.v1.PluginCatalogue".to_string(),
                meridian_domain::v1::PluginCatalogue {
                    launches: launches.clone(),
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
        });
    }

    fn currency(code: &str) -> ResolveIdentifierRequest {
        ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "iso4217".into(),
                value: code.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reporting_resolves_read_only_by_its_launched_roles_and_custody_mints() {
        // Contract v20 (W3.1): by the launch records, never by what is sent.
        for (instance, roles, mints) in [
            ("rep-1", &["reporting"][..], false),
            ("cust-1", &["custody"][..], true),
            ("both-1", &["reporting", "custody"][..], true),
            ("data-1", &["dgm"][..], true),
            ("unlaunched-1", &[][..], true),
        ] {
            let (store_bus, plugin_bus) = two(instance);
            let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
            launched(&store_bus, &[(instance, roles)]);
            serve_queries(
                &store_bus,
                store.clone(),
                Arc::new(meridian_clock::ManualClock::at(NOW)),
            );
            let reply: ResolveIdentifierReply = ask(
                &plugin_bus,
                RESOLVE_IDENTIFIER,
                "meridian.v1.ResolveIdentifierRequest",
                snap(),
                "",
            )
            .await
            .unwrap();
            assert_eq!(!reply.instrument_id.is_empty(), mints, "{instance}");
            assert_eq!(store.count().unwrap(), usize::from(mints), "{instance}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_currency_not_held_is_pulled_from_the_master_never_minted_while_it_holds_it() {
        let (store_bus, plugin_bus) = two("cust-1");
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        launched(&store_bus, &[("cust-1", &["custody"])]);
        let asked = Arc::new(Mutex::new(0));
        let counting = asked.clone();
        store_bus.serve(ASK_PLATFORM_FOR_INSTRUMENT, move |envelope| {
            *counting.lock().unwrap() += 1;
            let request =
                meridian_domain::v1::AskPlatformForInstrumentRequest::decode(&envelope.payload[..])
                    .unwrap();
            assert!(
                request.instrument_id.is_empty(),
                "asked for a record not held"
            );
            let code = request.identifiers[0].value.clone();
            let found = code == "CHF";
            Ok((
                "meridian.v1.AskPlatformForInstrumentReply".to_string(),
                meridian_domain::v1::AskPlatformForInstrumentReply {
                    reachable: true,
                    found,
                    instrument: found.then(|| PbInstrument {
                        instrument_id: "INS-CHF0000000000000000000001".into(),
                        asset_class: AssetClass::Cash as i32,
                        currency: "CHF".into(),
                        description: "Swiss franc".into(),
                        identifiers: vec![PbIdentifier {
                            scheme: "iso4217".into(),
                            value: "CHF".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    detail: String::new(),
                }
                .encode_to_vec(),
            ))
        });
        serve_queries(
            &store_bus,
            store.clone(),
            Arc::new(meridian_clock::ManualClock::at(NOW)),
        );
        let reply: ResolveIdentifierReply = ask(
            &plugin_bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            currency("CHF"),
            "",
        )
        .await
        .unwrap();
        assert!(!reply.minted, "found, not minted");
        let record = store.by_id(&reply.instrument_id).unwrap().unwrap();
        assert_eq!(record.asset_class, "ASSET_CLASS_CASH");
        assert_eq!(record.currency, "CHF");
        assert_eq!(record.description, "Swiss franc");
        assert!(record
            .identifiers
            .iter()
            .any(|i| i.scheme == meridian_symbology::GLOBAL_ID));
        // Held now: asked no more.
        let again: ResolveIdentifierReply = ask(
            &plugin_bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            currency("CHF"),
            "",
        )
        .await
        .unwrap();
        assert_eq!(again.instrument_id, reply.instrument_id);
        assert_eq!(*asked.lock().unwrap(), 1);
        // One the master does not hold is minted, as before.
        let stub: ResolveIdentifierReply = ask(
            &plugin_bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            currency("XQQ"),
            "",
        )
        .await
        .unwrap();
        assert!(stub.minted);
    }

    /// The 2026-10-11 ruling: a master slow past the wait, or not reached,
    /// mints nothing -- the resolve is a miss -- and once the pull brings the
    /// record, a later resolve finds it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_or_absent_master_mints_no_currency_and_a_later_resolve_finds_the_pulled_one() {
        let (store_bus, plugin_bus) = two("cust-1");
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        launched(&store_bus, &[("cust-1", &["custody"])]);
        store_bus.serve(ASK_PLATFORM_FOR_INSTRUMENT, move |envelope| {
            let request =
                meridian_domain::v1::AskPlatformForInstrumentRequest::decode(&envelope.payload[..])
                    .unwrap();
            let code = request.identifiers[0].value.clone();
            if code == "XNR" {
                // Not reached.
                return Ok((
                    "meridian.v1.AskPlatformForInstrumentReply".to_string(),
                    meridian_domain::v1::AskPlatformForInstrumentReply {
                        reachable: false,
                        detail: "the platform did not answer in time".into(),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ));
            }
            // Slow: longer than a resolve waits.
            std::thread::sleep(std::time::Duration::from_millis(3500));
            Ok((
                "meridian.v1.AskPlatformForInstrumentReply".to_string(),
                meridian_domain::v1::AskPlatformForInstrumentReply {
                    reachable: true,
                    found: true,
                    instrument: Some(PbInstrument {
                        instrument_id: "INS-USD0000000000000000000001".into(),
                        asset_class: AssetClass::Cash as i32,
                        currency: "USD".into(),
                        description: "US dollar".into(),
                        identifiers: vec![PbIdentifier {
                            scheme: "iso4217".into(),
                            value: "USD".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    detail: String::new(),
                }
                .encode_to_vec(),
            ))
        });
        serve_queries(
            &store_bus,
            store.clone(),
            Arc::new(meridian_clock::ManualClock::at(NOW)),
        );
        let resolve = |code: &'static str| {
            let plugin_bus = plugin_bus.clone();
            async move {
                ask::<ResolveIdentifierReply>(
                    &plugin_bus,
                    RESOLVE_IDENTIFIER,
                    "meridian.v1.ResolveIdentifierRequest",
                    currency(code),
                    "",
                )
                .await
                .unwrap()
            }
        };
        let started = std::time::Instant::now();
        let first = resolve("USD").await;
        assert!(
            started.elapsed() < std::time::Duration::from_millis(3200),
            "a resolve waits on the master no longer than {CURRENCY_WAIT:?}: {:?}",
            started.elapsed()
        );
        assert!(
            first.instrument_id.is_empty() && !first.minted,
            "a miss: {first:?}"
        );
        assert_eq!(store.count().unwrap(), 0, "nothing minted behind it");
        // The pull finishes in the background; the next resolve finds it.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let later = resolve("USD").await;
        assert!(!later.minted, "found, not minted: {later:?}");
        let record = store.by_id(&later.instrument_id).unwrap().unwrap();
        assert_eq!(record.description, "US dollar");
        // A master not reached: a miss, nothing minted, asked again later.
        let unreached = resolve("XNR").await;
        assert!(unreached.instrument_id.is_empty() && !unreached.minted);
        let again = resolve("XNR").await;
        assert!(again.instrument_id.is_empty() && !again.minted);
        assert_eq!(store.count().unwrap(), 1, "only the pulled record");
    }

    fn snap() -> ResolveIdentifierRequest {
        ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "symbol".into(),
                value: "SNAP1".into(),
                source: "snaptrade".into(),
                ..Default::default()
            }],
            as_of_ns: NOW,
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_minted_record_is_announced_completed_by_a_person_and_announced_again() {
        let bus = bus();
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let mut applied = bus.subscribe(INSTRUMENT_APPLIED);
        serve_queries(&bus, store.clone(), bus.clock());

        let minted: ResolveIdentifierReply = ask(
            &bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            snap(),
            "",
        )
        .await
        .unwrap();
        assert!(minted.minted);
        let heard = applied.recv().await.unwrap();
        let event = InstrumentAppliedEvent::decode(&heard.envelope.payload[..]).unwrap();
        assert_eq!(event.instrument.unwrap().version, 1);

        let completing = CompleteInstrumentsRequest {
            completions: vec![InstrumentCompletion {
                instrument_id: minted.instrument_id.clone(),
                against_version: 1,
                values: vec![
                    InstrumentValue {
                        value: Some(instrument_value::Value::AssetClass(
                            AssetClass::Equity as i32,
                        )),
                        source: "a statement".into(),
                    },
                    InstrumentValue {
                        value: Some(instrument_value::Value::Currency("USD".into())),
                        source: "a statement".into(),
                    },
                ],
                note: String::new(),
            }],
        };
        let refused = ask::<CompleteInstrumentsReply>(
            &bus,
            COMPLETE_INSTRUMENTS,
            "meridian.v1.CompleteInstrumentsRequest",
            completing.clone(),
            "",
        )
        .await
        .unwrap_err();
        assert!(refused.contains("a person completes"), "{refused}");

        let done: CompleteInstrumentsReply = ask(
            &bus,
            COMPLETE_INSTRUMENTS,
            "meridian.v1.CompleteInstrumentsRequest",
            completing,
            "local|ada",
        )
        .await
        .unwrap();
        assert!(
            done.results[0].refusal.is_none(),
            "{}",
            done.results[0].detail
        );
        let heard = applied.recv().await.unwrap();
        let event = InstrumentAppliedEvent::decode(&heard.envelope.payload[..]).unwrap();
        let record = event.instrument.unwrap();
        assert_eq!(record.version, 2);
        assert_eq!(
            record
                .sources
                .iter()
                .filter(|s| s.person == "local|ada")
                .count(),
            2
        );

        let read: ResolveInstrumentReply = ask(
            &bus,
            RESOLVE_INSTRUMENT,
            "meridian.v1.ResolveInstrumentRequest",
            ResolveInstrumentRequest {
                instrument_id: minted.instrument_id.clone(),
                as_of_ns: NOW,
            },
            "",
        )
        .await
        .unwrap();
        assert_eq!(
            read.instrument.unwrap().asset_class,
            AssetClass::Equity as i32
        );

        let listed: ListInstrumentsToCompleteReply = ask(
            &bus,
            LIST_INSTRUMENTS_TO_COMPLETE,
            "meridian.v1.ListInstrumentsToCompleteRequest",
            ListInstrumentsToCompleteRequest::default(),
            "",
        )
        .await
        .unwrap();
        assert_eq!(listed.incomplete_for_book, 0);
        assert_eq!(listed.incomplete, 1, "a description is still wanted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_merge_replaces_the_merged_record_and_says_so() {
        let bus = bus();
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let mut replaced_heard = bus.subscribe(INSTRUMENT_REPLACED);
        serve_queries(&bus, store.clone(), bus.clock());
        let first: ResolveIdentifierReply = ask(
            &bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            snap(),
            "",
        )
        .await
        .unwrap();
        let mut other = snap();
        other.identifiers[0].value = "SNAP2".into();
        let second: ResolveIdentifierReply = ask(
            &bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            other,
            "",
        )
        .await
        .unwrap();

        let merged: MergeInstrumentsReply = ask(
            &bus,
            MERGE_INSTRUMENTS,
            "meridian.v1.MergeInstrumentsRequest",
            MergeInstrumentsRequest {
                kept_instrument_id: first.instrument_id.clone(),
                kept_version: 1,
                merged_instrument_id: second.instrument_id.clone(),
                merged_version: 1,
                take_from_merged: vec![],
                note: "one security under two symbols".into(),
            },
            "local|ada",
        )
        .await
        .unwrap();
        assert_eq!(merged.instrument.unwrap().version, 2);
        let heard = replaced_heard.recv().await.unwrap();
        let event = InstrumentReplacedEvent::decode(&heard.envelope.payload[..]).unwrap();
        assert_eq!(event.replaced_instrument_id, second.instrument_id);
        assert_eq!(event.instrument.unwrap().instrument_id, first.instrument_id);
        assert_eq!(
            store.replacement_of(&second.instrument_id).unwrap(),
            Some(first.instrument_id)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_platforms_answer_is_kept_only_on_the_record_it_names() {
        let bus = bus();
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        serve_queries(&bus, store.clone(), bus.clock());
        let minted: ResolveIdentifierReply = ask(
            &bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            snap(),
            "",
        )
        .await
        .unwrap();
        let reactor = Reactor::new(bus.clone(), store.clone(), bus.clock());
        let delivery = |reply: PullInstrumentReply| Delivery {
            envelope: Envelope {
                meta: None,
                payload_type: "meridian.v1.PullInstrumentReply".into(),
                payload: reply.encode_to_vec(),
            },
            sequence: 1,
        };
        let record = PbInstrument {
            instrument_id: "INS-SNAP".into(),
            asset_class: AssetClass::Equity as i32,
            version: 3,
            ..Default::default()
        };
        assert!(matches!(
            reactor
                .react(delivery(PullInstrumentReply {
                    found: true,
                    instrument: Some(record.clone()),
                    for_instrument_id: String::new(),
                    venues: Vec::new(),
                }))
                .await,
            Handled::Ignored(_)
        ));
        assert_eq!(
            reactor
                .react(delivery(PullInstrumentReply {
                    found: true,
                    instrument: Some(record),
                    for_instrument_id: minted.instrument_id.clone(),
                    venues: Vec::new(),
                }))
                .await,
            Handled::Kept(true)
        );
        assert_eq!(store.count().unwrap(), 1, "no record of the platform's own");
        let held = store.by_id(&minted.instrument_id).unwrap().unwrap();
        assert!(held.identifiers.iter().any(|i| i.value == "INS-SNAP"));
        assert!(held.asset_class.is_empty());
    }

    /// Contract v18: a venue the platform answered by its codes arrives with
    /// no instrument and is kept; a plugin then resolves it (W3.14); an
    /// instrument's answer names its listing venue, which the record keeps.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pulled_venue_is_kept_and_resolved_and_a_record_keeps_its_listing_venue() {
        use meridian_domain::v1::{ResolveVenueReply, ResolveVenueRequest, VenueRecord};
        let bus = bus();
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        serve_queries(&bus, store.clone(), bus.clock());
        let reactor = Reactor::new(bus.clone(), store.clone(), bus.clock());
        let delivery = |reply: PullInstrumentReply| Delivery {
            envelope: Envelope {
                meta: None,
                payload_type: "meridian.v1.PullInstrumentReply".into(),
                payload: reply.encode_to_vec(),
            },
            sequence: 1,
        };
        let mic = PbIdentifier {
            scheme: "iso10383".into(),
            value: "XNYS".into(),
            source: String::new(),
            ..Default::default()
        };
        let venue = VenueRecord {
            venue_id: "VEN-XNYS".into(),
            name: "New York Stock Exchange".into(),
            identifiers: vec![mic.clone()],
            version: 1,
            ..Default::default()
        };
        let asked = ResolveVenueRequest {
            identifiers: vec![mic],
            as_of_ns: NOW,
        };
        let missed: ResolveVenueReply = ask(
            &bus,
            RESOLVE_VENUE,
            "meridian.v1.ResolveVenueRequest",
            asked.clone(),
            "",
        )
        .await
        .unwrap();
        assert!(!missed.found);
        assert_eq!(
            reactor
                .react(delivery(PullInstrumentReply {
                    found: false,
                    instrument: None,
                    for_instrument_id: String::new(),
                    venues: vec![venue],
                }))
                .await,
            Handled::Kept(true)
        );
        let found: ResolveVenueReply = ask(
            &bus,
            RESOLVE_VENUE,
            "meridian.v1.ResolveVenueRequest",
            asked,
            "",
        )
        .await
        .unwrap();
        assert_eq!(found.venue.unwrap().venue_id, "VEN-XNYS");

        let minted: ResolveIdentifierReply = ask(
            &bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            snap(),
            "",
        )
        .await
        .unwrap();
        reactor
            .react(delivery(PullInstrumentReply {
                found: true,
                instrument: Some(PbInstrument {
                    instrument_id: "INS-SNAP".into(),
                    listing_venue_id: "VEN-XNYS".into(),
                    version: 1,
                    ..Default::default()
                }),
                for_instrument_id: minted.instrument_id.clone(),
                venues: Vec::new(),
            }))
            .await;
        let held: ResolveInstrumentReply = ask(
            &bus,
            RESOLVE_INSTRUMENT,
            "meridian.v1.ResolveInstrumentRequest",
            ResolveInstrumentRequest {
                instrument_id: minted.instrument_id,
                as_of_ns: NOW,
            },
            "",
        )
        .await
        .unwrap();
        assert_eq!(held.instrument.unwrap().listing_venue_id, "VEN-XNYS");
    }
}
