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

/// W3.22 (contract v24). The replacements after a sequence, for a role
/// that missed the events.
pub const LIST_REPLACEMENTS: &str = "platform.reference.query.list-replacements";

/// W3.23 (contract v24). A `reference` plugin offering a record's template
/// fields.
pub const OFFER_TEMPLATE_FIELDS: &str = "platform.reference.command.offer-template-fields";

/// W3.26 (contract v24). The deployment admin making the migration's lines.
pub const APPLY_INSTRUMENT_MIGRATION: &str =
    "platform.reference.command.apply-instrument-migration";

/// W3.28 (contract v24). The conductor asking for the usage report's counts.
pub const LIST_IDENTIFIER_USAGE: &str = "platform.reference.query.list-identifier-usage";

/// W3.24 (contract v24). The template catalogue the store holds.
pub const LIST_TEMPLATES: &str = "platform.reference.query.list-templates";
pub const READ_TEMPLATE: &str = "platform.reference.query.read-template";

/// W3.24, W3.25, W3.27 (contract v24). What the conductor pulled from the
/// platform, for the store to keep and follow.
pub const TEMPLATES_PULLED: &str = "platform.reference.event.templates-pulled";
pub const LICENCE_LIST_PULLED: &str = "platform.reference.event.licence-list-pulled";
pub const REPLACEMENTS_PULLED: &str = "platform.reference.event.replacements-pulled";

/// Where the time comes from: the deployment's one clock (decisions/024),
/// given by whoever wires this up.
pub use meridian_clock::Clock;

/// The lock every write takes (see the module's note).
pub type Writing = Arc<Mutex<()>>;

/// Register the queries and commands on the bus.
pub fn serve_queries(bus: &Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    serve_all(bus, store, clock, Arc::new(Mutex::new(())));
}

/// [`serve_queries`] with the bounded ask shortened, for a test that shows
/// what a slow platform does without waiting the eight seconds.
pub fn serve_queries_within(
    bus: &Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    bound: std::time::Duration,
) {
    serve_all_within(bus, store, clock, Arc::new(Mutex::new(())), bound);
}

/// As [`serve_queries`], sharing `writing` with the [`Reactor`].
pub fn serve_all(bus: &Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>, writing: Writing) {
    serve_all_within(bus, store, clock, writing, BOUNDED_ASK);
}

/// [`serve_all`], a resolve waiting on the platform no longer than `bound`.
pub fn serve_all_within(
    bus: &Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    writing: Writing,
    bound: std::time::Duration,
) {
    {
        let (store, clock, writing, announcing) = (
            store.clone(),
            clock.clone(),
            writing.clone(),
            Arc::clone(bus),
        );
        let asking = Arc::new(Asking::waiting(Arc::clone(bus), bound));
        bus.serve(RESOLVE_IDENTIFIER, move |envelope| {
            // One bounded ask for the whole resolve (the spec's requirement
            // 15): a currency's pull and the matching request share it.
            let started = std::time::Instant::now();
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
            let matched = {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                match pulled.as_ref() {
                    // The master's currency, kept under its own ID (W3.1,
                    // W3.5; from v24 the `INS-` record is the deployment's).
                    Some(master) => crate::resolve::Matched::Answered(Box::new(
                        crate::resolve::resolve_matched(
                            store.as_ref(),
                            &request,
                            &instance,
                            now_ns,
                        )
                        .and_then(|matched| match matched {
                            crate::resolve::Matched::Unmatched(asked) => {
                                crate::resolve::resolve_unmatched(
                                    store.as_ref(),
                                    &request,
                                    asked,
                                    &instance,
                                    now_ns,
                                    crate::resolve::PlatformSaid::Record(Box::new(master.clone())),
                                )
                            }
                            crate::resolve::Matched::Answered(answered) => Ok(*answered),
                        })
                        .map_err(|failed| failed.to_string())?,
                    )),
                    None => {
                        crate::resolve::resolve_matched(store.as_ref(), &request, &instance, now_ns)
                            .map_err(|failed| failed.to_string())?
                    }
                }
            };
            let mut resolution = match matched {
                crate::resolve::Matched::Answered(resolution) => *resolution,
                crate::resolve::Matched::Unmatched(asked) => {
                    // Nothing matched (contract v24, W3.4): the platform
                    // asked to match or mint by what is sendable, within
                    // the bounded ask; a later answer resolves the
                    // provisional record minted meanwhile.
                    let template = crate::resolve::stated_template(store.as_ref(), &request);
                    let asking_for = crate::one_id::escalation(
                        &asked,
                        &request,
                        template.as_ref().map(|(template, _)| template),
                        &crate::one_id::licence(store.as_ref()),
                        &instance,
                        now_ns,
                    );
                    let said = match asking_for {
                        None => crate::resolve::PlatformSaid::NotAsked,
                        Some(asking_for) => {
                            let late = late_answer(
                                Arc::clone(&announcing),
                                store.clone(),
                                clock.clone(),
                                writing.clone(),
                                request.clone(),
                                asked.clone(),
                                instance.clone(),
                            );
                            asking.match_or_mint(asking_for, late, started)
                        }
                    };
                    let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                    let now_ns = clock.now_ns();
                    crate::resolve::resolve_unmatched(
                        store.as_ref(),
                        &request,
                        asked,
                        &instance,
                        now_ns,
                        said,
                    )
                    .map_err(|failed| failed.to_string())?
                }
            };
            // A currency pulled from the platform's master is its record,
            // found and never minted, however the pull's timing fell (W3.1;
            // approved 2026-10-15).
            if pulled.is_some() {
                resolution.reply.minted = false;
            }
            if let Some(changed) = &resolution.changed {
                announce(&announcing, changed, &envelope, now_ns);
            }
            // A miss leaves no record: counted for the usage report (W3.28),
            // against its one scheme, never its value.
            if resolution.reply.miss_reason != 0 {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                if let Err(failed) =
                    crate::usage::note_miss(store.as_ref(), &request.identifiers, now_ns)
                {
                    tracing::warn!(%failed, "a miss could not be counted for the usage report");
                }
                // A source thinner than the minimum, answered not found, is
                // the store's own miss (contract v25, W3.2; item 81), by the
                // identifiers asked; never sent. A plugin reports nothing for
                // it, and a report of one is the same miss, a no-op.
                if resolution.reply.miss_reason == meridian_domain::v1::MissReason::NotFound as i32
                    && !crate::one_id::meets_minimum(&request)
                {
                    if let Err(failed) = crate::usage::note_below_minimum(
                        store.as_ref(),
                        &request.identifiers,
                        now_ns,
                    ) {
                        tracing::warn!(%failed, "a below-minimum miss could not be recorded");
                    }
                }
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
            // By its ID (contract v25, D9), set instead of identifiers.
            if !request.venue_id.is_empty() && !request.identifiers.is_empty() {
                return Err(
                    "venue_id: set instead of identifiers, never beside them (W3.14)".into(),
                );
            }
            if request.identifiers.is_empty() && request.venue_id.is_empty() {
                return Err("identifiers: at least one names the venue, or venue_id".into());
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
        let (store, clock, writing, announcing) = (
            store.clone(),
            clock.clone(),
            writing.clone(),
            Arc::clone(bus),
        );
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
                {
                    // Its cause and its place in the deployment's sequence
                    // (contract v24, W3.8, W3.22).
                    let mut event = replaced(&merged.merged.instrument_id, &merged.stays, now_ns);
                    event.cause = meridian_domain::v1::ReplacementCause::Merged as i32;
                    event.sequence = store
                        .replacement_record(&merged.merged.instrument_id)
                        .ok()
                        .flatten()
                        .map(|kept| kept.sequence)
                        .unwrap_or_default();
                    event
                }
                .encode_to_vec(),
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
    serve_one_id(bus, store.clone(), clock.clone(), writing.clone());
    // Event groups (contract v26, W3.29, W3.30): a report asks the platform
    // through the conductor within the same bounded ask a resolve takes.
    crate::event_groups::serve(
        bus,
        store.clone(),
        clock.clone(),
        writing.clone(),
        Arc::new(crate::event_groups::BusAskGroup {
            bus: Arc::clone(bus),
            within: bound,
        }),
    );
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

/// The one-ID queries and command (contract v24, W3.22 to W3.24).
fn serve_one_id(bus: &Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>, writing: Writing) {
    {
        let store = store.clone();
        bus.serve(LIST_REPLACEMENTS, move |envelope| {
            let request: meridian_domain::v1::ListReplacementsRequest =
                decode(&envelope, "meridian.v1.ListReplacementsRequest")?;
            let reply = crate::one_id::list_replacements(store.as_ref(), request.after_sequence)
                .map_err(|failed| failed.to_string())?;
            Ok((
                "meridian.v1.ListReplacementsReply".to_string(),
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
        bus.serve(APPLY_INSTRUMENT_MIGRATION, move |envelope| {
            let request: meridian_domain::v1::ApplyInstrumentMigrationRequest =
                decode(&envelope, "meridian.v1.ApplyInstrumentMigrationRequest")?;
            let actor = actor_of(&envelope);
            let now_ns = clock.now_ns();
            let (made, counts) = {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                let made = crate::reask::apply_migration(store.as_ref(), &request, &actor, now_ns)
                    .map_err(|failed| failed.to_string())?
                    .map_err(|refused| match refused {
                        crate::reask::LinesRefused::ActorRequired => meridian_bus::refusal(
                            meridian_pb::v1::RefusalReason::ActorRequired as i32,
                            "a person makes the migration's lines, and none was named",
                        ),
                        crate::reask::LinesRefused::NoteRequired => meridian_bus::refusal(
                            meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                            "through a delegation a change to the migration's lines says why",
                        ),
                        crate::reask::LinesRefused::TooMany(field) => {
                            meridian_bus::refusal_with_status(
                                meridian_bus::RefusalStatus::InvalidArgument,
                                std::slice::from_ref(&field),
                                if field == "note" {
                                    format!("note: at most {} characters", crate::reask::MOST_NOTE)
                                } else {
                                    format!(
                                        "{field}: at most {} lines in one change",
                                        crate::reask::MOST_LINES
                                    )
                                },
                            )
                        }
                        crate::reask::LinesRefused::NotALine { path, words } => {
                            meridian_bus::refusal_naming(
                                meridian_pb::v1::RefusalReason::Unspecified as i32,
                                &[path],
                                words,
                            )
                        }
                    })?;
                let mut live = Vec::new();
                for record in store.all().map_err(|failed| failed.to_string())? {
                    if store
                        .replacement_of(&record.instrument_id)
                        .map_err(|failed| failed.to_string())?
                        .is_none()
                    {
                        live.push(record);
                    }
                }
                let counts = crate::reask::migration_counts(store.as_ref(), &live)
                    .map_err(|failed| failed.to_string())?
                    .unwrap_or_default();
                (made, counts)
            };
            for applied in &made.applied {
                publish_applied(&announcing, applied, now_ns);
            }
            for held in &made.held_back {
                announce(&announcing, held, &envelope, now_ns);
            }
            tracing::info!(
                by = actor.person,
                made = made.applied.len(),
                held_back = made.held_back.len(),
                "the migration's lines made"
            );
            Ok((
                "meridian.v1.MigrationCounts".to_string(),
                counts.encode_to_vec(),
            ))
        });
    }
    {
        let store = store.clone();
        bus.serve(LIST_IDENTIFIER_USAGE, move |envelope| {
            let request: meridian_domain::v1::ListIdentifierUsageRequest =
                decode(&envelope, "meridian.v1.ListIdentifierUsageRequest")?;
            let reply = crate::usage::list_identifier_usage(store.as_ref(), &request)
                .map_err(|failed| failed.to_string())?
                .map_err(|(field, words)| {
                    meridian_bus::refusal_with_status(
                        meridian_bus::RefusalStatus::InvalidArgument,
                        std::slice::from_ref(&field),
                        format!("{field}: {words}"),
                    )
                })?;
            Ok((
                "meridian.v1.ListIdentifierUsageReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }
    {
        let store = store.clone();
        bus.serve(LIST_TEMPLATES, move |envelope| {
            let request: meridian_domain::v1::ListTemplatesRequest =
                decode(&envelope, "meridian.v1.ListTemplatesRequest")?;
            let templates = crate::one_id::latest(&crate::one_id::catalogue(store.as_ref()))
                .into_iter()
                .filter(|template| {
                    request.asset_class == 0 || template.asset_class == request.asset_class
                })
                .collect();
            Ok((
                "meridian.v1.ListTemplatesReply".to_string(),
                meridian_domain::v1::ListTemplatesReply { templates }.encode_to_vec(),
            ))
        });
    }
    {
        let store = store.clone();
        bus.serve(READ_TEMPLATE, move |envelope| {
            let request: meridian_domain::v1::ReadTemplateRequest =
                decode(&envelope, "meridian.v1.ReadTemplateRequest")?;
            if request.template_id.trim().is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::Unspecified as i32,
                    &["template_id".to_string()],
                    "template_id: the template to read",
                ));
            }
            let template = crate::one_id::read_template(
                &crate::one_id::catalogue(store.as_ref()),
                request.template_id.trim(),
                request.template_version.trim(),
            );
            Ok((
                "meridian.v1.ReadTemplateReply".to_string(),
                meridian_domain::v1::ReadTemplateReply { template }.encode_to_vec(),
            ))
        });
    }
    {
        let (store, announcing) = (store, Arc::clone(bus));
        bus.serve(OFFER_TEMPLATE_FIELDS, move |envelope| {
            let request: meridian_domain::v1::OfferTemplateFieldsRequest =
                decode(&envelope, "meridian.v1.OfferTemplateFieldsRequest")?;
            let instance = envelope
                .meta
                .as_ref()
                .map(|meta| meta.publisher_instance_id.clone())
                .unwrap_or_default();
            let now_ns = clock.now_ns();
            let offered = {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                crate::one_id::offer_template_fields(store.as_ref(), &request, &instance, now_ns)
                    .map_err(|failed| failed.to_string())?
            };
            match offered {
                Ok(record) => {
                    announce(&announcing, &record, &envelope, now_ns);
                    Ok((
                        "meridian.v1.OfferTemplateFieldsReply".to_string(),
                        meridian_domain::v1::OfferTemplateFieldsReply {
                            instrument: Some(to_wire(&record)),
                        }
                        .encode_to_vec(),
                    ))
                }
                Err((field, words)) => Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::Unspecified as i32,
                    std::slice::from_ref(&field),
                    format!("{field}: {words}"),
                )),
            }
        });
    }
}

/// Subscribe to what the conductor pulls (contract v24) now, and hand back
/// what consumes it once the write lock is known.
pub fn keep_reference_forever(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
) -> impl FnOnce(Writing) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    let mut templates = bus.subscribe_shared(TEMPLATES_PULLED, "reference");
    let mut licence = bus.subscribe_shared(LICENCE_LIST_PULLED, "reference");
    let mut replacements = bus.subscribe_shared(REPLACEMENTS_PULLED, "reference");
    move |writing: Writing| {
        Box::pin(async move {
            loop {
                let delivery = tokio::select! {
                    next = templates.recv() => next,
                    next = licence.recv() => next,
                    next = replacements.recv() => next,
                };
                let Some(delivery) = delivery else { return };
                let (bus2, store2, writing2) = (Arc::clone(&bus), store.clone(), writing.clone());
                let now_ns = clock.now_ns();
                let handled = tokio::task::spawn_blocking(move || {
                    keep_reference(
                        &bus2,
                        store2.as_ref(),
                        &writing2,
                        &delivery.envelope,
                        now_ns,
                    )
                })
                .await;
                match handled {
                    Ok(Handled::Ignored(why)) => tracing::warn!(why, "ignored reference data"),
                    Ok(other) => tracing::debug!(?other, "kept the platform's reference data"),
                    Err(failed) => tracing::warn!(%failed, "keeping reference data failed"),
                }
            }
        })
    }
}

/// What the conductor pulled from the platform, kept and followed (contract
/// v24): the catalogue's new versions (W3.24), the licence list read last
/// (W3.25; a scheme it adds is backfilled by the re-ask), and the
/// replacement feed (W3.27), each replacement announced to every role.
pub fn keep_reference(
    bus: &Bus,
    store: &dyn Store,
    writing: &Writing,
    envelope: &Envelope,
    now_ns: i64,
) -> Handled {
    match envelope.payload_type.as_str() {
        "meridian.v1.PullTemplatesReply" => {
            let Ok(reply) = meridian_domain::v1::PullTemplatesReply::decode(&envelope.payload[..])
            else {
                return Handled::Ignored("undecodable templates".into());
            };
            let mut kept = false;
            for template in &reply.templates {
                match store.keep_template(template, now_ns) {
                    Ok(new) => kept |= new,
                    Err(failed) => return Handled::Ignored(failed.to_string()),
                }
            }
            Handled::Kept(kept)
        }
        "meridian.v1.LicenceList" => {
            let Ok(list) = meridian_domain::v1::LicenceList::decode(&envelope.payload[..]) else {
                return Handled::Ignored("undecodable licence list".into());
            };
            // Never a scheme that names the deployment's own records.
            if list
                .schemes
                .iter()
                .any(|scheme| scheme == crate::one_id::LOCAL_SCHEME)
            {
                return Handled::Ignored("a licence list naming open_meridian_local".into());
            }
            match store.keep_licence_list(&list, now_ns) {
                Ok(kept) => {
                    if kept {
                        let _ =
                            store.set_state(crate::one_id::LICENCE_KEPT_AT, &now_ns.to_string());
                    }
                    Handled::Kept(kept)
                }
                Err(failed) => Handled::Ignored(failed.to_string()),
            }
        }
        "meridian.v1.PullReplacementsReply" => {
            let Ok(reply) =
                meridian_domain::v1::PullReplacementsReply::decode(&envelope.payload[..])
            else {
                return Handled::Ignored("undecodable replacements".into());
            };
            let applied = {
                let Ok(_held) = writing.lock() else {
                    return Handled::Ignored("the write lock is poisoned".into());
                };
                match crate::one_id::apply_feed(store, &reply.entries, now_ns) {
                    Ok(applied) => applied,
                    Err(failed) => return Handled::Ignored(failed.to_string()),
                }
            };
            for each in &applied {
                publish_applied(bus, each, now_ns);
            }
            Handled::Kept(!applied.is_empty())
        }
        other => Handled::Ignored(format!("payload type {other}")),
    }
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
/// The bounded ask (the one-ID spec's requirement 15, W3.1: "8 seconds
/// today, as v20's currency pull does, never longer"): the longest a resolve
/// waits on the platform, a currency's pull and the matching request
/// together. The conductor bounds its own call to the platform the same
/// (its ASKING), and a plugin's sidecar gives a resolve longer than this.
pub const BOUNDED_ASK: std::time::Duration = std::time::Duration::from_secs(8);
/// The name v20 gave it.
pub const CURRENCY_WAIT: std::time::Duration = BOUNDED_ASK;
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

/// What to do with an answer that came after the resolve stopped waiting.
pub type Late = Box<dyn FnOnce(crate::resolve::PlatformSaid) + Send + 'static>;

/// How long the conductor's ask may take in the background: its own bound,
/// 8 seconds today (the spec's requirement 15), and the bus's way back.
const MATCH_OR_MINT_WITHIN: std::time::Duration = std::time::Duration::from_secs(12);

impl Asking {
    /// W3.4 (contract v24): the platform asked, through the conductor, to
    /// match or mint what a resolve found nothing for, waiting no longer
    /// than what is left of the bounded ask begun at `started`; an answer
    /// after that goes to `late`, which keeps it on the
    /// provisional record minted meanwhile. A platform without v24's routes
    /// answers as unreachable (the conductor's rule).
    pub fn match_or_mint(
        &self,
        asking_for: meridian_domain::v1::EscalateInstrumentRequest,
        late: Late,
        started: std::time::Instant,
    ) -> crate::resolve::PlatformSaid {
        use crate::resolve::PlatformSaid;
        let within = self.wait.saturating_sub(started.elapsed());
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return PlatformSaid::Unreachable;
        };
        let request = meridian_domain::v1::AskPlatformForInstrumentRequest {
            identifiers: asking_for.identifiers.clone(),
            as_of_ns: asking_for.as_of_ns,
            match_or_mint: Some(asking_for),
            ..Default::default()
        };
        // Who takes the answer: the resolve while it waits, `late` after.
        let taker: Arc<Mutex<Option<std::sync::mpsc::Sender<PlatformSaid>>>> =
            Arc::new(Mutex::new(None));
        let (done, heard) = std::sync::mpsc::channel::<PlatformSaid>();
        if let Ok(mut held) = taker.lock() {
            *held = Some(done);
        }
        let (bus, taking) = (Arc::clone(&self.bus), Arc::clone(&taker));
        handle.spawn(async move {
            let asked = bus
                .call(
                    ASK_PLATFORM_FOR_INSTRUMENT,
                    "meridian.v1.AskPlatformForInstrumentRequest",
                    request.encode_to_vec(),
                    None,
                    Some(MATCH_OR_MINT_WITHIN),
                )
                .await
                .map_err(|failed| failed.to_string())
                .and_then(|(_, bytes)| {
                    meridian_domain::v1::AskPlatformForInstrumentReply::decode(&bytes[..])
                        .map_err(|failed| failed.to_string())
                });
            let said = said_of(asked);
            let waiting = taking.lock().ok().and_then(|mut held| held.take());
            match waiting {
                Some(resolve) => {
                    let _ = resolve.send(said);
                }
                None => {
                    // The resolve answered already: kept on its record.
                    let _ = tokio::task::spawn_blocking(move || late(said)).await;
                }
            }
        });
        match heard.recv_timeout(within) {
            Ok(said) => said,
            Err(_) => {
                // Taken back, unless the answer is in hand this instant.
                let gone = taker.lock().ok().and_then(|mut held| held.take());
                match gone {
                    Some(_) => PlatformSaid::Unreachable,
                    None => heard
                        .recv_timeout(std::time::Duration::from_millis(50))
                        .unwrap_or(PlatformSaid::Unreachable),
                }
            }
        }
    }
}

/// The conductor's reply, read with which answer a record was (W3.4).
pub fn answer_of(
    asked: Result<meridian_domain::v1::AskPlatformForInstrumentReply, String>,
) -> crate::reask::Answer {
    let (minted, near_match) = asked
        .as_ref()
        .ok()
        .and_then(|reply| reply.match_or_mint_answer.as_ref())
        .map(|answer| (answer.minted, answer.near_match))
        .unwrap_or((false, false));
    crate::reask::Answer {
        said: said_of(asked),
        minted,
        near_match,
    }
}

/// The conductor asked from a blocking thread, for the re-ask and the
/// migration (W3.4, W3.26).
pub struct BusAsk {
    pub bus: Arc<Bus>,
}

impl crate::reask::Ask for BusAsk {
    fn ask(&self, request: meridian_domain::v1::EscalateInstrumentRequest) -> crate::reask::Answer {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return crate::resolve::PlatformSaid::Unreachable.into();
        };
        let request = meridian_domain::v1::AskPlatformForInstrumentRequest {
            identifiers: request.identifiers.clone(),
            as_of_ns: request.as_of_ns,
            match_or_mint: Some(request),
            ..Default::default()
        };
        let asked = handle
            .block_on(self.bus.call(
                ASK_PLATFORM_FOR_INSTRUMENT,
                "meridian.v1.AskPlatformForInstrumentRequest",
                request.encode_to_vec(),
                None,
                Some(MATCH_OR_MINT_WITHIN),
            ))
            .map_err(|failed| failed.to_string())
            .and_then(|(_, bytes)| {
                meridian_domain::v1::AskPlatformForInstrumentReply::decode(&bytes[..])
                    .map_err(|failed| failed.to_string())
            });
        answer_of(asked)
    }
}

/// Ask again and migrate, forever, at the licence list's pace (W3.4, W3.25,
/// W3.26): one batch per beat, `batch_size` records at most, never more
/// than `batches_per_minute` beats a minute.
pub async fn ask_again_forever(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    writing: Writing,
) {
    let asked = Arc::new(Mutex::new(std::collections::HashMap::<String, i64>::new()));
    let mut beat = std::time::Duration::from_secs(10);
    loop {
        let (bus2, store2, clock2, writing2, asked2) = (
            Arc::clone(&bus),
            store.clone(),
            clock.clone(),
            writing.clone(),
            Arc::clone(&asked),
        );
        // The store's reads block: every one on the blocking pool, the
        // licence list's pace among them.
        let ran = tokio::task::spawn_blocking(move || {
            let pace = crate::one_id::licence(store2.as_ref())
                .pace
                .unwrap_or_default();
            let batch = pace.batch_size.clamp(1, 100) as usize;
            let beat =
                std::time::Duration::from_secs(60 / pace.batches_per_minute.clamp(1, 60) as u64);
            let ask = BusAsk {
                bus: Arc::clone(&bus2),
            };
            let now_ns = clock2.now_ns();
            let (sent, changed) =
                crate::reask::migrate(store2.as_ref(), &ask, &writing2, batch, now_ns)?;
            for record in &changed {
                publish_applied(
                    &bus2,
                    &crate::one_id::Applied {
                        replaced: None,
                        changed: vec![record.clone()],
                    },
                    now_ns,
                );
            }
            let left = batch.saturating_sub(sent);
            let mut asked = asked2
                .lock()
                .map_err(|_| crate::store::StoreError::Unavailable("the asked list".into()))?;
            let applied = crate::reask::ask_again(
                store2.as_ref(),
                &ask,
                &writing2,
                &mut asked,
                left,
                now_ns,
            )?;
            for each in &applied {
                publish_applied(&bus2, each, now_ns);
            }
            crate::Result::Ok(beat)
        })
        .await;
        match ran {
            Ok(Err(failed)) => {
                tracing::warn!(%failed, "asking the platform again failed; next beat")
            }
            Err(failed) => tracing::warn!(%failed, "asking the platform again failed; next beat"),
            Ok(Ok(paced)) => beat = paced,
        }
        tokio::time::sleep(beat.max(std::time::Duration::from_secs(10))).await;
    }
}

/// The conductor's reply, read: a record, the staff's queue, or not reached.
pub fn said_of(
    asked: Result<meridian_domain::v1::AskPlatformForInstrumentReply, String>,
) -> crate::resolve::PlatformSaid {
    use crate::resolve::PlatformSaid;
    match asked {
        Ok(reply) if reply.reachable => match reply.match_or_mint_answer {
            Some(answer) => match answer.instrument {
                Some(record) if record.instrument_id.starts_with("INS-") => {
                    PlatformSaid::Record(Box::new(record))
                }
                _ if answer.pending_review => PlatformSaid::Pending,
                _ => PlatformSaid::Unreachable,
            },
            None => match reply.instrument {
                Some(record) if reply.found && record.instrument_id.starts_with("INS-") => {
                    PlatformSaid::Record(Box::new(record))
                }
                _ => PlatformSaid::Unreachable,
            },
        },
        Ok(reply) => {
            tracing::info!(detail = reply.detail, "the platform was not reached to match or mint; a provisional record, asked again later");
            PlatformSaid::Unreachable
        }
        Err(failed) => {
            tracing::info!(%failed, "the conductor did not answer to match or mint; a provisional record, asked again later");
            PlatformSaid::Unreachable
        }
    }
}

/// A late answer for a resolve's set, kept (W3.4, W3.8): the provisional
/// record minted meanwhile resolved to the platform's, or marked as waiting
/// for its staff; where nothing was minted (a source below the minimum), the
/// platform's record kept for the set.
fn late_answer(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    writing: Writing,
    request: ResolveIdentifierRequest,
    asked: Vec<crate::store::Asked>,
    instance: String,
) -> Late {
    Box::new(move |said| {
        let Ok(_held) = writing.lock() else { return };
        let now_ns = clock.now_ns();
        let found = crate::resolve::resolve_identifier_read_only(store.as_ref(), &request, now_ns)
            .ok()
            .filter(|found| found.reply.found)
            .map(|found| found.reply.instrument_id);
        let applied = match (found, &said) {
            (Some(instrument_id), _) => {
                crate::one_id::apply_answer(store.as_ref(), &instrument_id, &said, now_ns)
            }
            (None, crate::resolve::PlatformSaid::Record(_)) => crate::resolve::resolve_unmatched(
                store.as_ref(),
                &request,
                asked,
                &instance,
                now_ns,
                said,
            )
            .map(|resolution| crate::one_id::Applied {
                replaced: None,
                changed: resolution.changed.into_iter().collect(),
            }),
            (None, _) => Ok(crate::one_id::Applied::default()),
        };
        match applied {
            Ok(applied) => publish_applied(&bus, &applied, now_ns),
            Err(failed) => tracing::warn!(%failed, "a late answer was not kept; asked again later"),
        }
    })
}

/// Announce what keeping the platform's answer did: the replacement first,
/// heard by every role (W3.8, W3.9), then each record's new version.
pub fn publish_applied(bus: &Bus, applied: &crate::one_id::Applied, now_ns: i64) {
    if let Some(event) = &applied.replaced {
        if let Err(failed) = bus.publish(
            INSTRUMENT_REPLACED,
            "meridian.v1.InstrumentReplacedEvent",
            event.encode_to_vec(),
            None,
            None,
        ) {
            tracing::warn!(%failed, "a replacement could not be announced; the feed and the sweeps find it");
        }
    }
    for changed in &applied.changed {
        if let Err(failed) = bus.publish(
            INSTRUMENT_APPLIED,
            "meridian.v1.InstrumentAppliedEvent",
            InstrumentAppliedEvent {
                instrument: Some(to_wire(changed)),
                applied: true,
                applied_at_ns: now_ns,
            }
            .encode_to_vec(),
            None,
            None,
        ) {
            tracing::warn!(%failed, instrument = changed.instrument_id, "a record's new version could not be announced");
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
                let (store, writing, bus) = (
                    self.store.clone(),
                    self.writing.clone(),
                    Arc::clone(&self.bus),
                );
                let kept = tokio::task::spawn_blocking(move || {
                    let _held = writing
                        .lock()
                        .map_err(|_| "the write lock is poisoned".to_string())?;
                    // A provisional record the platform holds is resolved to
                    // its record (contract v24, W3.8); one awaiting its
                    // migration line waits for the deployment admin.
                    let became = crate::replace::current(store.as_ref(), &reply.for_instrument_id)
                        .map_err(|failed| failed.to_string())?;
                    let provisional = store
                        .by_id(&became)
                        .map_err(|failed| failed.to_string())?
                        .is_some_and(|held| {
                            held.local.provisional && !crate::reask::awaits_its_line(&held)
                        });
                    if provisional && record.instrument_id.starts_with("INS-") {
                        let applied = crate::one_id::apply_answer(
                            store.as_ref(),
                            &became,
                            &crate::resolve::PlatformSaid::Record(Box::new(record.clone())),
                            now_ns,
                        )
                        .map_err(|failed| failed.to_string())?;
                        // The replacement, and the replaced record's new
                        // version; the record that stays is announced below.
                        let stays = applied.changed.first().cloned();
                        publish_applied(
                            &bus,
                            &crate::one_id::Applied {
                                replaced: applied.replaced.clone(),
                                changed: applied.changed.iter().skip(1).cloned().collect(),
                            },
                            now_ns,
                        );
                        return Ok(stays);
                    }
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

/// What the deployment's own records say of a calendar's subject, for the
/// list's text match (W3.21, contract v23): the ISO 4217 code of the record
/// carrying its currency's platform ID, and each venue's MICs and name.
fn subject_codes(
    store: &dyn Store,
    venues: &[meridian_domain::v1::VenueRecord],
    record: &meridian_domain::v1::CalendarRecord,
) -> Vec<String> {
    let mut codes = Vec::new();
    if !record.instrument_id.is_empty() {
        // The record the calendar names, matched directly: from contract v24
        // the currency's record is the deployment's under its `INS-` ID
        // (requirement 19). From contract v25 nothing is looked up by an
        // `open_meridian` identifier: a record kept under an `LCL-` key is
        // matched once its migration line is made (W3.26).
        if let Some(held) = store.by_id(&record.instrument_id).ok().flatten() {
            codes.extend(
                held.held_now()
                    .filter(|identifier| identifier.scheme == "iso4217")
                    .map(|identifier| identifier.value.clone()),
            );
            if !held.currency.is_empty() {
                codes.push(held.currency.clone());
            }
        }
    }
    for venue in venues
        .iter()
        .filter(|venue| record.venue_ids.contains(&venue.venue_id))
    {
        codes.push(venue.name.clone());
        codes.extend(venue.identifiers.iter().map(|held| held.value.clone()));
    }
    codes
}

/// The `INS-` record a record this deployment holds became (contract v24,
/// requirement 19, one rule: follow the replacement). None for an `INS-` ID,
/// which is asked as it is, and for a record not resolved, which is kept as
/// asked: from contract v25 no `open_meridian` identifier is read for it.
fn became_platform(store: &dyn Store, instrument_id: &str) -> Option<String> {
    if instrument_id.starts_with("INS-") {
        return None;
    }
    let became = crate::replace::current(store, instrument_id).ok()?;
    became.starts_with("INS-").then_some(became)
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
            let request = crate::calendars::pair_followed(&request, |instrument| {
                became_platform(store.as_ref(), instrument)
            });
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
            let venues = if request.query.trim().is_empty() {
                Vec::new()
            } else {
                store.venues().map_err(|failed| failed.to_string())?
            };
            let codes = |record: &meridian_domain::v1::CalendarRecord| {
                subject_codes(store.as_ref(), &venues, record)
            };
            let reply = crate::calendars::list_with(&calendars, &rules, &request, &codes)?;
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
    let mut pulled = bus.subscribe_shared(CALENDAR_PULLED, "calendars");
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
                    ..Default::default()
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
        // Kept under the platform's own ID from the start (contract v24,
        // W3.1, W3.5): the master's record, found and not minted.
        assert!(
            !reply.minted && reply.instrument_id.starts_with("INS-") && !reply.provisional,
            "{reply:?}"
        );
        let record = store.by_id(&reply.instrument_id).unwrap().unwrap();
        assert_eq!(record.asset_class, "ASSET_CLASS_CASH");
        assert_eq!(record.currency, "CHF");
        assert_eq!(record.description, "Swiss franc");
        assert!(
            !record
                .identifiers
                .iter()
                .any(|i| i.scheme == meridian_symbology::GLOBAL_ID),
            "open_meridian is no longer written (requirement 2)"
        );
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
        assert!(stub.minted && stub.provisional, "{stub:?}");
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
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
        });
        // The bounded ask shortened to 2.5 seconds for the test.
        serve_queries_within(
            &store_bus,
            store.clone(),
            Arc::new(meridian_clock::ManualClock::at(NOW)),
            std::time::Duration::from_millis(2500),
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
            "a resolve waits on the master no longer than its bound: {:?}",
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
        assert!(
            !later.minted && later.instrument_id.starts_with("INS-"),
            "the platform's record, found and not minted: {later:?}"
        );
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
            // The minimum a record needs (contract v24, W3.7).
            stated_description: "Snap One".into(),
            currency: "USD".into(),
            ..Default::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_source_below_the_minimum_is_the_stores_own_miss_and_a_report_of_it_changes_nothing()
    {
        // Contract v25 (W3.2; item 81): answered not found, nothing minted,
        // recorded by the store as a miss by the identifiers asked, counted,
        // and never sent; a plugin's report of it is the same miss.
        let bus = bus();
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        serve_queries(&bus, store.clone(), bus.clock());
        let mut thin = snap();
        thin.stated_description.clear();
        for _ in 0..2 {
            let missed: ResolveIdentifierReply = ask(
                &bus,
                RESOLVE_IDENTIFIER,
                "meridian.v1.ResolveIdentifierRequest",
                thin.clone(),
                "",
            )
            .await
            .unwrap();
            assert_eq!(
                missed.miss_reason,
                meridian_domain::v1::MissReason::NotFound as i32
            );
        }
        assert_eq!(store.count().unwrap(), 0, "nothing minted");
        let held = crate::usage::below_minimum_misses(store.as_ref()).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].asked, 2);
        assert_eq!(
            held[0].identifiers,
            vec![("symbol".into(), "SNAP1".into(), "snaptrade".into())]
        );
        // A report of it (MISS_REASON_NOT_FOUND) is a no-op.
        let reactor = Reactor::new(bus.clone(), store.clone(), bus.clock());
        let reported = reactor
            .react(Delivery {
                envelope: Envelope {
                    meta: None,
                    payload_type: "meridian.v1.MissingInstrumentDetectedEvent".into(),
                    payload: meridian_domain::v1::MissingInstrumentDetectedEvent {
                        identifiers: thin.identifiers.clone(),
                        reason: meridian_domain::v1::MissReason::NotFound as i32,
                        ..Default::default()
                    }
                    .encode_to_vec(),
                },
                sequence: 1,
            })
            .await;
        assert!(matches!(reported, Handled::Listed(false)));
        assert_eq!(
            crate::usage::below_minimum_misses(store.as_ref()).unwrap(),
            held
        );
        // A source meeting the minimum is no below-minimum miss.
        let _: ResolveIdentifierReply = ask(
            &bus,
            RESOLVE_IDENTIFIER,
            "meridian.v1.ResolveIdentifierRequest",
            snap(),
            "",
        )
        .await
        .unwrap();
        assert_eq!(
            crate::usage::below_minimum_misses(store.as_ref())
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_minted_record_is_announced_completed_by_a_person_and_announced_again() {
        let bus = bus();
        let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
        let mut applied = bus.subscribe_every(INSTRUMENT_APPLIED);
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
        let mut replaced_heard = bus.subscribe_every(INSTRUMENT_REPLACED);
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
        // Contract v24 (W3.8): the provisional record resolved to the
        // platform's, which starts from it and carries its LCL- ID under
        // open_meridian_local; never an open_meridian identifier.
        assert!(minted.provisional);
        assert_eq!(
            store
                .replacement_of(&minted.instrument_id)
                .unwrap()
                .as_deref(),
            Some("INS-SNAP")
        );
        let stays = store.by_id("INS-SNAP").unwrap().unwrap();
        assert!(stays
            .held_now()
            .any(|i| i.scheme == crate::one_id::LOCAL_SCHEME && i.value == minted.instrument_id));
        assert!(!stays
            .identifiers
            .iter()
            .any(|i| i.scheme == meridian_symbology::GLOBAL_ID));
        assert!(!stays.local.provisional);
        let gone = store.by_id(&minted.instrument_id).unwrap().unwrap();
        assert_eq!(
            gone.held_now().count(),
            0,
            "its identifiers left it, kept closed"
        );
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
            venue_id: String::new(),
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
        let asked_again = asked.clone();
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
        // By its ID (contract v25, D9), and never both ways at once.
        let by_id: ResolveVenueReply = ask(
            &bus,
            RESOLVE_VENUE,
            "meridian.v1.ResolveVenueRequest",
            ResolveVenueRequest {
                venue_id: "VEN-XNYS".into(),
                ..Default::default()
            },
            "",
        )
        .await
        .unwrap();
        assert_eq!(by_id.venue.unwrap().name, "New York Stock Exchange");
        let both = ask::<ResolveVenueReply>(
            &bus,
            RESOLVE_VENUE,
            "meridian.v1.ResolveVenueRequest",
            ResolveVenueRequest {
                venue_id: "VEN-XNYS".into(),
                ..asked_again.clone()
            },
            "",
        )
        .await;
        assert!(both.is_err_and(|why| why.contains("venue_id")));

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
