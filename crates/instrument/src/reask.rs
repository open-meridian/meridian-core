//! Asking the platform again, and the upgrade's migration (contract v24;
//! W3.4, W3.25, W3.26).
//!
//! **Asked again** (W3.4): a provisional record whose ask did not reach the
//! platform is asked again once it can be reached -- tried every few
//! minutes, which is how a reconnect is noticed -- and one waiting for the
//! platform's staff is asked daily, each a new request with a new key. A
//! record with nothing sendable is asked once the licence list names a
//! scheme it carries: the backfill (W3.25). Asked in batches of the list's
//! size at its pace, never faster.
//!
//! **The migration** (W3.26): at the upgrade every `LCL-` record kept before
//! v24 is sent as a matching request, in the same batches, and the answer
//! recorded on it -- its section, and the `INS-` record answered -- each its
//! own version (decisions/031). The record is marked provisional, its
//! reason said, and stays as it is until the deployment admin makes its
//! line: the run is a view of the store, so it resumes where it stopped.
//! The admin makes the lines with `ApplyInstrumentMigration` (W3.26, the
//! names approved 2026-10-15): the exact lines as one prepared change, each
//! uncertain line its own; the run's counts are the Instruments page's.

use std::collections::HashMap;

use meridian_domain::v1::{
    BackfillCount, EscalateInstrumentRequest, MigrationCounts, ProvisionalReason,
};

use crate::one_id::{self, Applied};
use crate::resolve::PlatformSaid;
use crate::store::{Instrument, Result, Store, Written};

/// How the store reaches the platform: the conductor's ask (W3.4), or a
/// test's stand-in.
pub trait Ask: Send + Sync {
    fn ask(&self, request: EscalateInstrumentRequest) -> Answer;
}

/// What the platform said, with which answer a record was: a match, a mint
/// from its source, or a near-match minted and flagged (W3.4).
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub said: PlatformSaid,
    pub minted: bool,
    pub near_match: bool,
}

impl From<PlatformSaid> for Answer {
    fn from(said: PlatformSaid) -> Self {
        Answer {
            said,
            minted: false,
            near_match: false,
        }
    }
}

/// The store's own marker that the migration ran to its end.
pub const MIGRATED: &str = "migration_v24_done_at_ns";

/// An unreachable record is tried again no sooner than this.
pub const UNREACHED_AGAIN_NS: i64 = 5 * 60 * 1_000_000_000;
/// One waiting for the platform's staff is asked again daily.
pub const STAFF_AGAIN_NS: i64 = 86_400 * 1_000_000_000;

/// The migration's sections, as a record's line keeps them (W3.26).
pub mod section {
    pub const ALREADY_HELD: &str = "already_held";
    pub const MATCHED: &str = "matched";
    pub const MINTED: &str = "minted";
    pub const NEAR_MATCH: &str = "near_match";
    pub const AWAITING_STAFF: &str = "awaiting_staff";
    pub const KEPT_LOCAL: &str = "kept_local";
    pub const UNREACHABLE: &str = "unreachable";
    pub const HELD_BACK: &str = "held_back";
}

/// A line's section as the wire names it (MigrationSection, contract v24).
pub fn section_to_wire(kept: &str) -> meridian_domain::v1::MigrationSection {
    use meridian_domain::v1::MigrationSection as S;
    match kept {
        section::ALREADY_HELD => S::AlreadyHeld,
        section::MATCHED => S::Matched,
        section::MINTED => S::Minted,
        section::NEAR_MATCH => S::NearMatch,
        section::AWAITING_STAFF => S::AwaitingStaff,
        section::KEPT_LOCAL => S::KeptLocal,
        section::UNREACHABLE => S::Unreachable,
        section::HELD_BACK => S::HeldBack,
        _ => S::Unspecified,
    }
}

/// Whether a record is the migration's, its line not yet made: never
/// resolved without the deployment admin.
pub fn awaits_its_line(record: &Instrument) -> bool {
    !record.local.migration.is_empty()
}

/// One pass of asking again: at most `batch` records, each due, each
/// answered kept. Stops at the first not reached -- the platform is away --
/// so a pass never hammers it. `asked` holds when each was last asked in
/// this process.
pub fn ask_again(
    store: &dyn Store,
    ask: &dyn Ask,
    writing: &std::sync::Mutex<()>,
    asked: &mut HashMap<String, i64>,
    batch: usize,
    now_ns: i64,
) -> Result<Vec<Applied>> {
    let list = one_id::licence(store);
    let list_at: i64 = store
        .state(one_id::LICENCE_KEPT_AT)?
        .and_then(|text| text.parse().ok())
        .unwrap_or(0);
    let mut applied = Vec::new();
    let mut sent = 0;
    for record in store.all()? {
        if sent >= batch {
            break;
        }
        if !record.local.provisional
            || awaits_its_line(&record)
            || record.local.reason == ProvisionalReason::HeldBack as i32
            || store.replacement_of(&record.instrument_id)?.is_some()
        {
            continue;
        }
        let Some(request) = one_id::escalation_for(&record, &list, now_ns) else {
            continue;
        };
        let last = asked
            .get(&record.instrument_id)
            .copied()
            .unwrap_or(record.local.asked_at_ns);
        let due = last < list_at
            || last == 0
            || match ProvisionalReason::try_from(record.local.reason) {
                Ok(ProvisionalReason::Unreachable) => now_ns - last >= UNREACHED_AGAIN_NS,
                Ok(ProvisionalReason::AwaitingStaff) => now_ns - last >= STAFF_AGAIN_NS,
                _ => false,
            };
        if !due {
            continue;
        }
        sent += 1;
        let said = ask.ask(request).said;
        asked.insert(record.instrument_id.clone(), now_ns);
        if said == PlatformSaid::Unreachable {
            break;
        }
        let _held = writing
            .lock()
            .map_err(|_| crate::store::StoreError::Unavailable("the write lock".into()))?;
        let kept = one_id::apply_answer(store, &record.instrument_id, &said, now_ns)?;
        if kept.replaced.is_some() || !kept.changed.is_empty() {
            applied.push(kept);
        }
    }
    Ok(applied)
}

/// One pass of the migration: at most `batch` records kept before v24 sent
/// and answered, each line recorded on its record. Answers how many were
/// sent; zero once none remain, the run marked done.
pub fn migrate(
    store: &dyn Store,
    ask: &dyn Ask,
    writing: &std::sync::Mutex<()>,
    batch: usize,
    now_ns: i64,
) -> Result<(usize, Vec<Instrument>)> {
    let list = one_id::licence(store);
    let catalogue = one_id::catalogue(store);
    let mut changed = Vec::new();
    let mut sent = 0;
    let mut stop = false;
    let mut away = 0;
    // Those never sent first, then those not reached before, so a record
    // the platform cannot be reached about starves none of the others.
    let mut queue = Vec::new();
    for record in store.all()? {
        if remaining(store, &record)? {
            queue.push(record);
        }
    }
    queue.sort_by_key(|record| record.local.migration == section::UNREACHABLE);
    for record in queue {
        if sent >= batch || stop {
            break;
        }
        let mut line = record.clone();
        // Already carrying the platform's ID: a currency pulled since v20,
        // a record a person asked about.
        let carried = record
            .held_now()
            .find(|identifier| {
                identifier.scheme == meridian_symbology::GLOBAL_ID
                    && identifier.value.starts_with("INS-")
            })
            .map(|identifier| identifier.value.clone());
        let (section, answer, reason) = if let Some(ins) = carried {
            (section::ALREADY_HELD, ins, ProvisionalReason::Unspecified)
        } else {
            match one_id::escalation_for(&record, &list, now_ns) {
                None => (
                    section::KEPT_LOCAL,
                    String::new(),
                    ProvisionalReason::Unspecified,
                ),
                Some(request) => {
                    sent += 1;
                    let answer = ask.ask(request);
                    match answer.said {
                        PlatformSaid::Record(master) => (
                            if answer.near_match {
                                section::NEAR_MATCH
                            } else if answer.minted {
                                section::MINTED
                            } else {
                                section::MATCHED
                            },
                            master.instrument_id.clone(),
                            ProvisionalReason::Unspecified,
                        ),
                        PlatformSaid::Pending => (
                            section::AWAITING_STAFF,
                            String::new(),
                            ProvisionalReason::AwaitingStaff,
                        ),
                        PlatformSaid::Unreachable | PlatformSaid::NotAsked => {
                            // Its line says so; sent again by a later pass,
                            // and this batch stops: the platform is away.
                            stop = true;
                            (
                                section::UNREACHABLE,
                                String::new(),
                                ProvisionalReason::Unreachable,
                            )
                        }
                    }
                }
            }
        };
        let class = crate::record::asset_class_value(record.value(crate::store::Field::AssetClass));
        let kind = crate::record::type_value(record.value(crate::store::Field::InstrumentType));
        let template = one_id::pick(
            &catalogue,
            meridian_domain::v1::AssetClass::try_from(class)
                .unwrap_or(meridian_domain::v1::AssetClass::Unspecified),
            meridian_domain::v1::InstrumentType::try_from(kind)
                .unwrap_or(meridian_domain::v1::InstrumentType::Unspecified),
        );
        let mut local = one_id::provisional_state(&record, template.as_ref(), reason, "", now_ns);
        // Answered but not yet made (ruled 2026-10-15; W3.11, W3.26): the
        // record stays provisional, INCOMPLETE, until a change makes it.
        if answered(section) {
            local.reason = ProvisionalReason::Incomplete as i32;
        }
        local.migration = section.to_string();
        local.migration_answer = answer;
        line.local = local;
        line.version = record.version + 1;
        line.record_time_ns = now_ns;
        let entry = crate::store::Version {
            acting_through_delegation: String::new(),
            client_name: String::new(),
            prepared_call_id: String::new(),
            instrument_id: record.instrument_id.clone(),
            version: line.version,
            operation: "migrate".into(),
            changes: Vec::new(),
            person: String::new(),
            instance_id: String::new(),
            note: format!(
                "the upgrade to contract v24 sent this record to the platform (W3.26): {}{}; \
                 provisional until the deployment admin makes its line",
                section.replace('_', " "),
                if line.local.migration_answer.is_empty() {
                    String::new()
                } else {
                    format!(", answered {}", line.local.migration_answer)
                }
            ),
            merged_instrument_id: String::new(),
            record_time_ns: now_ns,
        };
        let _held = writing
            .lock()
            .map_err(|_| crate::store::StoreError::Unavailable("the write lock".into()))?;
        if line.local.migration == section::UNREACHABLE {
            away += 1;
            if record.local.migration == section::UNREACHABLE {
                // Its line says so already: no new version for each try.
                continue;
            }
        }
        if store.write(line.clone(), record.version, entry)? == Written::Stored {
            changed.push(line);
        }
    }
    let sent_and_answered = sent - away;
    if sent_and_answered == 0 && away == 0 && store.state(MIGRATED)?.is_none() {
        let left = store
            .all()?
            .iter()
            .map(|record| remaining(store, record))
            .collect::<Result<Vec<bool>>>()?
            .into_iter()
            .filter(|left| *left)
            .count();
        if left == 0 {
            store.set_state(MIGRATED, &now_ns.to_string())?;
        }
    }
    Ok((sent, changed))
}

/// A record the migration has still to send: an `LCL-` record kept before
/// v24 -- not provisional, never given a line -- and not replaced.
fn remaining(store: &dyn Store, record: &Instrument) -> Result<bool> {
    let unsent = !record.local.provisional && record.local.migration.is_empty();
    let unreached = record.local.migration == section::UNREACHABLE;
    Ok(record.instrument_id.starts_with(crate::ids::LOCAL_PREFIX)
        && (unsent || unreached)
        && store.replacement_of(&record.instrument_id)?.is_none())
}

/// The migration's counts (W3.11, W3.26), from the records alone.
pub fn migration_counts(store: &dyn Store, live: &[Instrument]) -> Result<Option<MigrationCounts>> {
    let mut counts = MigrationCounts::default();
    let mut any = false;
    for record in live {
        if remaining(store, record)? && record.local.migration.is_empty() {
            counts.remaining += 1;
            any = true;
            continue;
        }
        let slot = match record.local.migration.as_str() {
            section::ALREADY_HELD => &mut counts.already_held,
            section::MATCHED => &mut counts.matched,
            section::MINTED => &mut counts.minted,
            section::NEAR_MATCH => &mut counts.near_match,
            section::AWAITING_STAFF => &mut counts.awaiting_staff,
            section::KEPT_LOCAL => &mut counts.kept_local,
            section::UNREACHABLE => &mut counts.unreachable,
            section::HELD_BACK => &mut counts.held_back,
            _ => continue,
        };
        *slot += 1;
        any = true;
    }
    Ok(any.then_some(counts))
}

/// Each backfill's counts (W3.11, W3.25): per scheme the licence list
/// names beyond the open ones every deployment starts with, the records
/// carrying it resolved, and those still to send.
pub fn backfill_counts(store: &dyn Store, live: &[Instrument]) -> Vec<BackfillCount> {
    let list = one_id::licence(store);
    list.schemes
        .iter()
        .filter(|scheme| !one_id::OPEN_FAMILIES.contains(&scheme.as_str()))
        .map(|scheme| {
            let carrying = |record: &&Instrument| {
                record
                    .held_now()
                    .any(|identifier| one_id::family(&identifier.scheme) == scheme.as_str())
            };
            BackfillCount {
                scheme: scheme.clone(),
                matched: live
                    .iter()
                    .filter(carrying)
                    .filter(|record| {
                        record.instrument_id.starts_with("INS-")
                            && record
                                .identifiers
                                .iter()
                                .any(|identifier| identifier.scheme == one_id::LOCAL_SCHEME)
                    })
                    .count() as i64,
                awaiting_person: live
                    .iter()
                    .filter(carrying)
                    .filter(|record| awaits_its_line(record))
                    .count() as i64,
                remaining: live
                    .iter()
                    .filter(carrying)
                    .filter(|record| record.local.provisional && !awaits_its_line(record))
                    .count() as i64,
            }
        })
        .collect()
}

/// A line the platform answered with an `INS-` record: matched, minted, a
/// near-match, or already held (W3.26).
pub fn answered(section: &str) -> bool {
    matches!(
        section,
        section::ALREADY_HELD | section::MATCHED | section::MINTED | section::NEAR_MATCH
    )
}

/// The most lines either list of one change names.
pub const MOST_LINES: usize = 500;

/// The longest a change's note may be, as a note on an instrument record.
pub const MOST_NOTE: usize = 2_000;

/// Why a change to the migration's lines was refused: the person missing,
/// a list past its bound, or a line that is none the platform answered,
/// each named by its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinesRefused {
    ActorRequired,
    /// Through a delegation a change says why (approved 2026-10-15).
    NoteRequired,
    /// A field past its bound: a list of lines, or the note.
    TooMany(String),
    NotALine {
        path: String,
        words: String,
    },
}

/// What making the lines did: each replacement to announce with the records
/// at their new versions, the records held back, and the run's counts after.
#[derive(Debug, Clone, Default)]
pub struct LinesMade {
    pub applied: Vec<Applied>,
    pub held_back: Vec<Instrument>,
}

/// W3.26: make the migration's lines a deployment admin names -- each in
/// `instrument_ids` replaced by the `INS-` record its line answered (W3.8,
/// `REPLACEMENT_CAUSE_RESOLVED`), each in `held_back_ids` kept under its
/// `LCL-` ID, provisional and HELD_BACK, nothing minted -- together or not
/// at all: every line is checked before any is made. A line already made is
/// unchanged. Called under the store's write lock.
pub fn apply_migration(
    store: &dyn Store,
    request: &meridian_domain::v1::ApplyInstrumentMigrationRequest,
    by: &crate::complete::Actor,
    now_ns: i64,
) -> Result<std::result::Result<LinesMade, LinesRefused>> {
    if by.person.is_empty() {
        return Ok(Err(LinesRefused::ActorRequired));
    }
    for (field, ids) in [
        ("instrument_ids", &request.instrument_ids),
        ("held_back_ids", &request.held_back_ids),
    ] {
        if ids.len() > MOST_LINES {
            return Ok(Err(LinesRefused::TooMany(field.to_string())));
        }
    }
    let note = request.note.trim();
    if note.chars().count() > MOST_NOTE {
        return Ok(Err(LinesRefused::TooMany("note".into())));
    }
    if note.is_empty() && !by.delegation.is_empty() {
        return Ok(Err(LinesRefused::NoteRequired));
    }
    let said = if note.is_empty() {
        String::new()
    } else {
        format!(": {note}")
    };
    // Checked whole first: the lines are made together or not at all.
    let mut to_make = Vec::new();
    let mut to_hold = Vec::new();
    for (index, id) in request.instrument_ids.iter().enumerate() {
        let path = format!("instrument_ids[{index}]");
        if request.instrument_ids[..index].contains(id) {
            return Ok(Err(LinesRefused::NotALine {
                path,
                words: format!("{id} is named twice"),
            }));
        }
        if store.replacement_of(id)?.is_some() {
            continue; // made already: unchanged, as a replacement recorded is
        }
        let record = line_of(store, id, &path)?;
        match record {
            Ok(record) => to_make.push(record),
            Err(refused) => return Ok(Err(refused)),
        }
    }
    for (index, id) in request.held_back_ids.iter().enumerate() {
        let path = format!("held_back_ids[{index}]");
        if request.instrument_ids.contains(id) || request.held_back_ids[..index].contains(id) {
            return Ok(Err(LinesRefused::NotALine {
                path,
                words: format!("{id} is named twice: a line is made or held back"),
            }));
        }
        if store.replacement_of(id)?.is_some() {
            return Ok(Err(LinesRefused::NotALine {
                path,
                words: format!("{id}'s line is made already, and a made line is not held back"),
            }));
        }
        match line_of(store, id, &path)? {
            Ok(record) => to_hold.push(record),
            Err(refused) => return Ok(Err(refused)),
        }
    }

    let mut made = LinesMade::default();
    for record in to_hold {
        if record.local.migration == section::HELD_BACK {
            continue;
        }
        let mut held = record.clone();
        held.local.migration = section::HELD_BACK.to_string();
        held.local.reason = ProvisionalReason::HeldBack as i32;
        held.version = record.version + 1;
        held.record_time_ns = now_ns;
        let entry = crate::store::Version {
            acting_through_delegation: by.delegation.clone(),
            client_name: by.client.clone(),
            prepared_call_id: by.prepared_call_id.clone(),
            instrument_id: record.instrument_id.clone(),
            version: held.version,
            operation: "hold-back".into(),
            changes: Vec::new(),
            person: by.person.clone(),
            instance_id: String::new(),
            note: format!(
                "the deployment admin held back this record's line (W3.26): it keeps its \
                 LCL- ID, provisional, and nothing is minted for it; answered {}{said}",
                record.local.migration_answer
            ),
            merged_instrument_id: String::new(),
            record_time_ns: now_ns,
        };
        if store.write(held.clone(), record.version, entry)? == Written::Stored {
            made.held_back.push(held);
        }
    }
    for record in to_make {
        let master = meridian_domain::v1::InstrumentRecord {
            instrument_id: record.local.migration_answer.clone(),
            ..Default::default()
        };
        let Some(resolved) = one_id::resolve_to_as(
            store,
            &record.instrument_id,
            &master,
            meridian_domain::v1::ReplacementCause::Resolved,
            by,
            note,
            now_ns,
        )?
        else {
            continue;
        };
        let mut changed = vec![resolved.stays.clone()];
        if let Some(gone) = store.by_id(&record.instrument_id)? {
            changed.push(gone);
        }
        made.applied.push(Applied {
            replaced: Some(resolved.event),
            changed,
        });
    }
    Ok(Ok(made))
}

/// The record a line names, refused by its path unless it is an `LCL-`
/// record the platform answered with an `INS-` record (or one held back
/// from such a line).
fn line_of(
    store: &dyn Store,
    id: &str,
    path: &str,
) -> Result<std::result::Result<Instrument, LinesRefused>> {
    let Some(record) = store.by_id(id)? else {
        return Ok(Err(LinesRefused::NotALine {
            path: path.to_string(),
            words: format!("the deployment holds no record {id}"),
        }));
    };
    let section = record.local.migration.as_str();
    let made_from = answered(section) || section == section::HELD_BACK;
    if !id.starts_with(crate::ids::LOCAL_PREFIX)
        || !made_from
        || !record.local.migration_answer.starts_with("INS-")
    {
        let what = match section {
            "" => "is no line of the migration".to_string(),
            other => format!(
                "is {}, no line the platform answered",
                other.replace('_', " ")
            ),
        };
        return Ok(Err(LinesRefused::NotALine {
            path: path.to_string(),
            words: format!("{id} {what}; nothing changed"),
        }));
    }
    Ok(Ok(record))
}
