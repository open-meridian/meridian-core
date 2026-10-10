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
use crate::resolve::{conflict_reported, resolve_identifier, resolve_instrument};
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
        bus.serve(RESOLVE_IDENTIFIER, move |envelope| {
            let request: ResolveIdentifierRequest =
                decode(&envelope, "meridian.v1.ResolveIdentifierRequest")?;
            let instance = envelope
                .meta
                .as_ref()
                .map(|meta| meta.publisher_instance_id.clone())
                .unwrap_or_default();
            let now_ns = clock.now_ns();
            let resolution = {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                resolve_identifier(store.as_ref(), &request, &instance, now_ns)
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
                let Some(record) = reply.instrument.filter(|_| reply.found) else {
                    return Handled::Ignored("an answer carrying no record".into());
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

    fn snap() -> ResolveIdentifierRequest {
        ResolveIdentifierRequest {
            identifiers: vec![PbIdentifier {
                scheme: "symbol".into(),
                value: "SNAP1".into(),
                source: "snaptrade".into(),
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
}
