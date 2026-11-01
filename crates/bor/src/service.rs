//! Where the book meets the bus.
//!
//! Seven commands in, four queries in, one event heard, one query asked, four
//! events out. An `operations` plugin's commands -- an opening balance
//! (W9.1), a break (W9.4), the figures (W9.5), a break's handling (W9.6), its
//! resolution and its close as cleared (W9.7) -- and the dashboard's
//! attribute (W9.13) arrive as
//! commands; five roles' reads (W9.10 to W9.12, W9.14) as queries; a
//! merged record's replacement (W3.8) as an event, which the book follows
//! (W9.9), also asking the instrument store what each instrument it holds has
//! become (W3.6), and asking it the record of each instrument a command names:
//! its version (Q31), and its asset class and currency, which the book
//! requires (W9.1, contract v10). Every record an entry changes leaves as an event,
//! whole, after the entry commits (W9.8).
//!
//! # Who caused it, and whose read it is
//!
//! Read off the envelope, as the street store reads it: the instance that
//! sent a command, which its sidecar stamped as the publisher; the person it
//! was sent for, whom the sidecar vouched for; its chain and its own
//! identifier. A query's envelope says whose read it is: a plugin's, its scope
//! marked as applying, answered only within it; or a core component's,
//! answered for every account (W4.11).
//!
//! # A refusal's code
//!
//! A refusal a plugin must tell apart carries its code on the bus beside its
//! words ([`StoreError::on_the_bus`]), which the sidecar puts in
//! `meridian-refusal-bin` beside `ABORTED` (open point 13).

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, Delivery, Envelope};
use meridian_domain::v1::{
    AccountAttributeChangedEvent, AccountAttributeReply, AccountFiguresRecordedEvent,
    BookEntryReply, BreakChangedEvent, CloseBreaksAsClearedRequest, HandleBreakRequest,
    InstrumentReplacedEvent, ListAccountAttributesReply, ListAccountAttributesRequest,
    ListAccountFiguresReply, ListAccountFiguresRequest, ListBreaksReply, ListBreaksRequest,
    ListPositionsReply, ListPositionsRequest, PositionChangedEvent, RecordAccountFiguresRequest,
    RecordBreakRequest, RecordEncumbrancesRequest, RecordOpeningBalanceRequest,
    ResolveBreakRequest, ResolveInstrumentReply, ResolveInstrumentRequest,
    SetAccountAttributeRequest,
};
use meridian_pb::bounds;
use prost::Message;

use crate::decide::{self, Context, Made, RecordSays};
use crate::store::{
    mark_of, page_limit, watermark_of, Acted, AttributesRead, BreaksRead, Decided, FiguresRead,
    PositionsRead, Scope, Store, StoreError,
};

pub use meridian_clock::Clock;

pub const RECORD_OPENING_BALANCE: &str = "platform.book.command.record-opening-balance";
pub const RECORD_BREAK: &str = "platform.book.command.record-break";
pub const RECORD_ACCOUNT_FIGURES: &str = "platform.book.command.record-account-figures";
pub const RECORD_ENCUMBRANCES: &str = "platform.book.command.record-encumbrances";
pub const HANDLE_BREAK: &str = "platform.book.command.handle-break";
pub const RESOLVE_BREAK: &str = "platform.book.command.resolve-break";
pub const CLOSE_BREAKS_AS_CLEARED: &str = "platform.book.command.close-breaks-as-cleared";
pub const SET_ACCOUNT_ATTRIBUTE: &str = "platform.book.command.set-account-attribute";

pub const POSITION_CHANGED: &str = "platform.book.event.position-changed";
pub const BREAK_CHANGED: &str = "platform.book.event.break-changed";
pub const ACCOUNT_FIGURES_RECORDED: &str = "platform.book.event.account-figures-recorded";
pub const ACCOUNT_ATTRIBUTE_CHANGED: &str = "platform.book.event.account-attribute-changed";
/// Contract v28 (W9.16 to W9.19): a listed option's or a future's lifecycle.
pub const RECORD_LIFECYCLE_EVENT: &str = "platform.book.command.record-lifecycle-event";
pub const PROPOSE_LIFECYCLE_EVENTS: &str = "platform.book.command.propose-lifecycle-events";
pub const RELEASE_LIFECYCLE_EVENT: &str = "platform.book.command.release-lifecycle-event";
pub const LIST_LIFECYCLE_EVENTS: &str = "platform.book.query.list-lifecycle-events";
pub const LIFECYCLE_EVENT_RECORDED: &str = "platform.book.event.lifecycle-event-recorded";

pub const LIST_POSITIONS: &str = "platform.book.query.list-positions";
pub const LIST_BREAKS: &str = "platform.book.query.list-breaks";
pub const LIST_ACCOUNT_FIGURES: &str = "platform.book.query.list-account-figures";
pub const LIST_ACCOUNT_ATTRIBUTES: &str = "platform.book.query.list-account-attributes";

/// W3.8, heard (W9.9).
pub const INSTRUMENT_REPLACED: &str = "platform.reference.event.instrument-replaced";

/// W3.6, asked: an instrument's record, for its version (Q31), its asset class
/// and currency (W9.1), and what a merged record has become.
pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";

/// Contract v29 (W10.6, W11.11; ruling 986, amended at core stage C):
/// asked, a pool's or a tranche's factors, for a fill stating none.
pub const LIST_FACTORS: &str = "platform.lake.query.list-factors";

/// What the lake answers for a pool's or a tranche's fill stating no
/// factor (W11.11; rulings 986, 991, 992, amended at core stages C and E).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FactorRead {
    /// The factor published for the settlement month -- the latest version
    /// of the row whose factor month is the one before the month the fill
    /// settles in -- or, while that is unpublished, the latest row published
    /// before it, provisional: its row's awaited month says which.
    Taken(meridian_pb::v1::Decimal, crate::orders::FactorRow),
    /// No published row answers that month or any before it.
    NonePublished,
    /// The lake did not answer in time, or refused the read.
    Unavailable,
}

/// The month a fill settles in, as a factor month (`YYYYMM`): its
/// settlement date's, its computed one's, or its trade date's.
pub(crate) fn settlement_month(dates: &[&str]) -> Option<String> {
    let date = dates.iter().find(|d| !d.is_empty())?;
    let parsed = meridian_domain::date::Date::parse(date).ok()?;
    let text = parsed.to_string();
    Some(format!("{}{}", &text[0..4], &text[5..7]))
}

/// The factor month before `month` (`YYYYMM`): the agency's month whose
/// factor a fill settling in `month` trades at (W10.4: the month's factor,
/// published early in the next, prices what settles in it).
pub(crate) fn month_before(month: &str) -> String {
    let (year, number) = (
        month
            .get(..4)
            .and_then(|y| y.parse::<i32>().ok())
            .unwrap_or(0),
        month
            .get(4..6)
            .and_then(|m| m.parse::<i32>().ok())
            .unwrap_or(1),
    );
    let (year, number) = if number <= 1 {
        (year - 1, 12)
    } else {
        (year, number - 1)
    };
    format!("{year:04}{number:02}")
}

/// Every `Factor` row the lake holds for a pool or a tranche, each version
/// as answered; `None` where the lake did not answer. The book reads as the
/// deployment's own component, every dataset, the default source.
pub(crate) fn factor_rows(bus: &Bus, instrument: &str) -> Option<Vec<meridian_domain::v1::Factor>> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let mut rows = Vec::new();
    let mut cursor = String::new();
    loop {
        let asked = handle.block_on(
            bus.call(
                LIST_FACTORS,
                "meridian.v1.ListFactorsRequest",
                meridian_domain::v1::ListFactorsRequest {
                    subjects: vec![meridian_domain::v1::SubjectRef {
                        entity_id: instrument.to_string(),
                        ..Default::default()
                    }],
                    page_size: 500,
                    cursor: cursor.clone(),
                    ..Default::default()
                }
                .encode_to_vec(),
                None,
                Some(VERSION_WAIT),
            ),
        );
        let (payload_type, payload) = asked.ok()?;
        if payload_type != "meridian.v1.ListFactorsReply" {
            return None;
        }
        let reply = meridian_domain::v1::ListFactorsReply::decode(&payload[..]).ok()?;
        rows.extend(reply.factors);
        if reply.next_cursor.is_empty() {
            break;
        }
        cursor = reply.next_cursor;
    }
    Some(rows)
}

/// Of the rows held, the factor for a fill settling in `month` (rulings 991
/// and 992): the latest version of the month before's row, final; else the
/// latest row published before it, its awaited month that month before.
pub(crate) fn factor_for(rows: &[meridian_domain::v1::Factor], month: &str) -> FactorRead {
    let wanted = month_before(month);
    let mut best: Option<(&str, u64, &meridian_domain::v1::Factor)> = None;
    for row in rows {
        if row.factor.is_none() || row.factor_month.as_str() > wanted.as_str() {
            continue;
        }
        let version = row.meta.as_ref().map_or(0, |m| m.version);
        let later = best.is_none_or(|(held, held_version, _)| {
            (row.factor_month.as_str(), version) > (held, held_version)
        });
        if later {
            best = Some((row.factor_month.as_str(), version, row));
        }
    }
    match best {
        Some((held, version, row)) => FactorRead::Taken(
            row.factor.expect("kept only with its factor"),
            crate::orders::FactorRow {
                month: held.to_string(),
                version,
                payment_date: row.payment_date.clone(),
                awaited: if held < wanted.as_str() {
                    wanted
                } else {
                    String::new()
                },
            },
        ),
        None => FactorRead::NonePublished,
    }
}

/// The factor for a pool's or a tranche's fill settling in `month`, as the
/// lake answers it now (W11.11).
pub(crate) fn latest_factor(bus: &Bus, instrument: &str, month: &str) -> FactorRead {
    match factor_rows(bus, instrument) {
        Some(rows) => factor_for(&rows, month),
        None => FactorRead::Unavailable,
    }
}

/// How long a command waits for the instrument store's records. Short: a
/// store that does not answer in it is a command refused to be tried again
/// (contract v10), never one admitted unchecked.
const VERSION_WAIT: Duration = Duration::from_secs(2);

/// How often the instruments held are asked about again: the recovery path
/// for a merge this process did not hear, so slow and certain, as the street
/// store's is.
pub const SWEEP_EVERY: Duration = Duration::from_secs(15 * 60);

/// Whose read a query is (W4.11).
pub fn scope_of(envelope: &Envelope) -> Scope {
    match envelope.meta.as_ref() {
        Some(meta) if meta.account_scope_applies => {
            Scope::Within(meta.account_scope.iter().cloned().collect())
        }
        _ => Scope::Everything,
    }
}

/// What a command knows beside its body, from its envelope.
pub(crate) fn context_of(envelope: &Envelope, clock: &dyn Clock) -> Context {
    let meta = envelope.meta.clone().unwrap_or_default();
    let now = clock.now_ns();
    Context {
        // The write scope a command naming its record by ID alone carries
        // (contract v22, W4.9), and a placement's link verdict (W11.8).
        account_scope: meta
            .account_scope_applies
            .then(|| meta.account_scope.iter().cloned().collect()),
        connectivity_allowed: meta.connectivity_allowed,
        connectivity_demo: meta.connectivity_demo,
        message_id: meta.message_id,
        instance_id: meta.publisher_instance_id,
        acting_for: meta.acting_for_subject,
        acting_through_delegation: meta.acting_through_delegation,
        acting_through_client: meta.acting_through_client,
        prepared_call_id: meta.prepared_call_id,
        acting_for_kind: meta.acting_for_kind,
        owner_subject: meta.owner_subject,
        algorithm_id: meta.algorithm_id,
        correlation_id: meta.correlation_id,
        event_time_ns: meta.published_at_ns,
        received_at_ns: now,
        committed_at_ns: now,
        reference_versions: Default::default(),
        records: Default::default(),
        reference_unavailable: false,
        control_sequence: 0,
        account_marks: Default::default(),
        members: Default::default(),
        local_record_orders: None,
        agent_orders_demo_only: None,
        affirmation: crate::deadline::current(),
        factor_taken: None,
        factor_row: Default::default(),
        factors_unavailable: false,
    }
}

fn expect(arrived: &str, wanted: &str) -> Result<(), String> {
    if arrived == wanted {
        return Ok(());
    }
    Err(format!("expected {wanted}, got {arrived}"))
}

pub(crate) fn decode<M: Message + Default>(envelope: &Envelope, wanted: &str) -> Result<M, String> {
    expect(&envelope.payload_type, wanted)?;
    M::decode(&envelope.payload[..]).map_err(|failed| format!("undecodable {wanted}: {failed}"))
}

/// What the instrument store says of each instrument a command names: its
/// reference version (Q31), and what its record says the book requires (W9.1,
/// contract v10); or that it did not answer in time.
#[derive(Default)]
pub(crate) struct References {
    pub(crate) versions: std::collections::BTreeMap<String, i64>,
    pub(crate) records: std::collections::BTreeMap<String, RecordSays>,
    pub(crate) unavailable: bool,
}

/// What a record says that the book requires: an asset class and a currency
/// in force, and whether the class is cash.
fn record_says(record: &meridian_domain::v1::InstrumentRecord) -> RecordSays {
    let class = meridian_domain::v1::AssetClass::try_from(record.asset_class)
        .unwrap_or(meridian_domain::v1::AssetClass::Unspecified);
    RecordSays {
        asset_class: class != meridian_domain::v1::AssetClass::Unspecified,
        currency: !record.currency.trim().is_empty(),
        cash: class == meridian_domain::v1::AssetClass::Cash,
        code: record.currency.trim().to_string(),
        provisional: record.provisional,
        referential: class == meridian_domain::v1::AssetClass::Referential,
        instrument_type: record.instrument_type,
        expired: record.lifecycle_state
            == meridian_domain::v1::InstrumentLifecycleState::Expired as i32,
        // Contract v28 (559): the series' premium multiplier.
        multiplier: record
            .listed_option
            .as_ref()
            .and_then(|o| o.contract_multiplier.as_ref())
            .or_else(|| {
                record
                    .future
                    .as_ref()
                    .and_then(|f| f.contract_multiplier.as_ref())
            })
            .and_then(|m| meridian_domain::exact::Exact::from_wire(m).ok())
            .filter(|m| !m.is_zero() && !m.is_negative()),
        debt: debt_says(record),
    }
}

/// Contract v29: what a debt record's terms say that the book checks (W9.16,
/// W11.1); None for a record that is no debt record.
fn debt_says(record: &meridian_domain::v1::InstrumentRecord) -> Option<crate::decide::DebtSays> {
    let exact = |d: Option<&meridian_pb::v1::Decimal>| {
        d.and_then(|d| meridian_domain::exact::Exact::from_wire(d).ok())
    };
    if let Some(bond) = &record.bond {
        return Some(crate::decide::DebtSays {
            minimum_denomination: exact(bond.minimum_denomination.as_ref()),
            denomination_increment: exact(bond.denomination_increment.as_ref()),
            stripped_from: bond.stripped_from.clone(),
            ..Default::default()
        });
    }
    if let Some(pool) = &record.mbs_pool {
        return Some(crate::decide::DebtSays {
            agency: pool.agency,
            program: pool.program.clone(),
            pass_through_rate: exact(pool.pass_through_rate.as_ref()),
            ..Default::default()
        });
    }
    if let Some(tba) = &record.tba {
        return Some(crate::decide::DebtSays {
            agency: tba.agency,
            program: tba.program.clone(),
            pass_through_rate: exact(tba.pass_through_rate.as_ref()),
            ..Default::default()
        });
    }
    if let Some(tranche) = &record.cmo_tranche {
        return Some(crate::decide::DebtSays {
            agency: tranche.agency,
            ..Default::default()
        });
    }
    record
        .certificate_of_deposit
        .as_ref()
        .map(|cd| crate::decide::DebtSays {
            minimum_denomination: exact(cd.minimum_denomination.as_ref()),
            ..Default::default()
        })
}

/// Each instrument's record from the instrument store. A record the store
/// does not hold is absent, and lacks everything; a store that does not
/// answer in time is `unavailable`, and the command is refused to be tried
/// again.
pub(crate) fn references(bus: &Bus, instruments: &[String], now_ns: i64) -> References {
    let mut found = References::default();
    if instruments.is_empty() {
        return found;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        found.unavailable = true;
        return found;
    };
    for instrument in instruments {
        let asked = handle.block_on(
            bus.call(
                RESOLVE_INSTRUMENT,
                "meridian.v1.ResolveInstrumentRequest",
                ResolveInstrumentRequest {
                    instrument_id: instrument.clone(),
                    as_of_ns: now_ns,
                }
                .encode_to_vec(),
                None,
                Some(VERSION_WAIT),
            ),
        );
        let Ok((payload_type, payload)) = asked else {
            // The instrument store away is away for all of them.
            found.unavailable = true;
            break;
        };
        if payload_type != "meridian.v1.ResolveInstrumentReply" {
            found.unavailable = true;
            break;
        }
        match ResolveInstrumentReply::decode(&payload[..]) {
            Ok(reply) => {
                if let Some(record) = reply.instrument.filter(|_| reply.found) {
                    if record.version > 0 {
                        found.versions.insert(instrument.clone(), record.version);
                    }
                    found
                        .records
                        .insert(instrument.clone(), record_says(&record));
                }
            }
            Err(_) => {
                found.unavailable = true;
                break;
            }
        }
    }
    found
}

/// A command's answer for the book's entries.
fn entry_reply(made: &Made) -> BookEntryReply {
    // Each amount naming its cash instrument (contract v18).
    let mut reply = BookEntryReply {
        entry: Some(made.entry.meta.clone()),
        journal: Some(made.changes.first.clone()),
        positions: made
            .changes
            .positions
            .iter()
            .map(|(record, _)| record.clone())
            .collect(),
        breaks: made.changes.breaks.clone(),
        figures: made.changes.figures.clone(),
        attributes: made.changes.attributes.clone(),
    };
    meridian_domain::money::fill_known(&mut reply);
    reply
}

/// What a reply kept with an entry is prefixed by where the entry records a
/// refusal (contract v22, W11.8): the command, and any duplicate of it, is
/// answered with the refusal after the entry commits.
pub(crate) const REFUSED: &[u8] = b"\0refused\0";

pub(crate) fn decided(made: Made, reply: Vec<u8>) -> Decided {
    let reply = match &made.refusal {
        Some(refusal) => [REFUSED, refusal.as_bytes()].concat(),
        None => reply,
    };
    Decided {
        entry: made.entry,
        book: made.book,
        changes: made.changes,
        reply,
    }
}

/// Publish every record an entry changed, whole, in the order it numbered
/// them, each in the command's chain and caused by it.
pub fn announce(bus: &Bus, decided: &Decided, correlation: Option<&str>, causation: Option<&str>) {
    let entry = &decided.entry;
    let say = |topic: &str, payload_type: &str, payload: Vec<u8>| {
        if let Err(failed) = bus.publish(topic, payload_type, payload, correlation, causation) {
            // Committed is committed: a reader catches up from the store
            // after a gap (W4.3), so a publish lost here is not a lost change.
            tracing::warn!(topic, %failed, "a change of the book was committed and not published");
        }
    };
    // The order path's records first (W11.13): W9's PositionChanged follows
    // for each position a fill moved.
    crate::order_service::announce(&say, decided);
    for (record, previous) in &decided.changes.positions {
        say(POSITION_CHANGED, "meridian.v1.PositionChangedEvent", {
            let mut event = PositionChangedEvent {
                position: Some(record.clone()),
                previous_trade_date_quantity: Some(previous.to_wire()),
                entry: Some(entry.meta.clone()),
                journal: record.last_change.clone(),
                cause: Some(entry.cause.clone()),
            };
            meridian_domain::money::fill_known(&mut event);
            event.encode_to_vec()
        });
    }
    for record in &decided.changes.breaks {
        say(BREAK_CHANGED, "meridian.v1.BreakChangedEvent", {
            let mut event = BreakChangedEvent {
                break_record: Some(record.clone()),
                entry: Some(entry.meta.clone()),
                journal: record.last_change.clone(),
                cause: Some(entry.cause.clone()),
            };
            meridian_domain::money::fill_known(&mut event);
            event.encode_to_vec()
        });
    }
    for record in &decided.changes.figures {
        say(
            ACCOUNT_FIGURES_RECORDED,
            "meridian.v1.AccountFiguresRecordedEvent",
            {
                let mut event = AccountFiguresRecordedEvent {
                    figures: Some(record.clone()),
                    entry: Some(entry.meta.clone()),
                    journal: record.last_change.clone(),
                    cause: Some(entry.cause.clone()),
                };
                meridian_domain::money::fill_known(&mut event);
                event.encode_to_vec()
            },
        );
    }
    // Contract v28 (W9.19): each lifecycle event the entry changed, after
    // it commits, with its journal and cause.
    for record in &decided.changes.lifecycle_events {
        say(
            LIFECYCLE_EVENT_RECORDED,
            "meridian.v1.LifecycleEventRecordedEvent",
            {
                let mut event = meridian_domain::v1::LifecycleEventRecordedEvent {
                    event: Some(record.clone()),
                    journal: record.last_change.clone(),
                    cause: Some(entry.cause.clone()),
                };
                meridian_domain::money::fill_known(&mut event);
                event.encode_to_vec()
            },
        );
    }
    if let Some(record) = &decided.changes.attributes {
        say(
            ACCOUNT_ATTRIBUTE_CHANGED,
            "meridian.v1.AccountAttributeChangedEvent",
            AccountAttributeChangedEvent {
                attributes: Some(record.clone()),
                entry: Some(entry.meta.clone()),
                journal: record.last_change.clone(),
                cause: Some(entry.cause.clone()),
            }
            .encode_to_vec(),
        );
    }
}

/// A command's decision and its encoded answer.
pub(crate) type Deciding<'a> =
    dyn Fn(&crate::book::Book, u64, &Context) -> Result<(Made, Vec<u8>), StoreError> + 'a;

/// One command: decided under the account's lock, committed, announced, and
/// answered; or answered as the first was.
#[allow(clippy::too_many_arguments)]
fn command(
    bus: &Bus,
    store: &dyn Store,
    envelope: &Envelope,
    ctx: Context,
    account_id: &str,
    idempotency_key: &str,
    reply_type: &str,
    decide: &Deciding<'_>,
) -> Result<(String, Vec<u8>), String> {
    if account_id.is_empty() {
        return Err("the command names no account".into());
    }
    command_store(
        bus,
        store,
        envelope,
        ctx,
        account_id,
        idempotency_key,
        reply_type,
        decide,
    )
    .map_err(|failed| failed.on_the_bus())
}

/// [`command`], its refusal kept typed: what the order path reads a fact
/// to keep unplaced from (W11.13). Every entry records the control
/// partition's sequence in force (decisions/024's note).
#[allow(clippy::too_many_arguments)]
pub(crate) fn command_store(
    bus: &Bus,
    store: &dyn Store,
    envelope: &Envelope,
    mut ctx: Context,
    account_id: &str,
    idempotency_key: &str,
    reply_type: &str,
    decide: &Deciding<'_>,
) -> Result<(String, Vec<u8>), StoreError> {
    ctx.control_sequence = store
        .heads()?
        .get(crate::store::CONTROL_PARTITION)
        .copied()
        .unwrap_or(0);
    let acted = store.act(
        account_id,
        &ctx.message_id,
        idempotency_key,
        &envelope.payload,
        &mut |book, head| {
            let (made, reply) = decide(book, head, &ctx)?;
            Ok(decided(made, reply))
        },
    );
    let acted = match acted {
        // A venue's fact already applied: answered as it stands.
        Err(StoreError::Repeated(reply)) => return Ok((reply_type.to_string(), reply)),
        other => other?,
    };
    let reply = match acted {
        Acted::Duplicate(reply) => reply,
        Acted::Committed(decided) => {
            let meta = envelope.meta.as_ref();
            announce(
                bus,
                &decided,
                meta.map(|meta| meta.correlation_id.as_str()),
                meta.map(|meta| meta.message_id.as_str()),
            );
            decided.reply
        }
    };
    if let Some(refusal) = reply.strip_prefix(REFUSED) {
        return Err(StoreError::Recorded(
            String::from_utf8_lossy(refusal).into_owned(),
        ));
    }
    Ok((reply_type.to_string(), reply))
}

/// W9.16 to W9.19 (contract v28): a lifecycle event reported, proposed,
/// released and read.
fn serve_lifecycle(bus: &Arc<Bus>, store: &Arc<dyn Store>, clock: &Arc<dyn Clock>) {
    use meridian_domain::v1::{
        LifecycleEvent, ListLifecycleEventsReply, ListLifecycleEventsRequest,
        ProposeLifecycleEventsReply, ProposeLifecycleEventsRequest, RecordLifecycleEventReply,
        RecordLifecycleEventRequest, ReleaseLifecycleEventReply, ReleaseLifecycleEventRequest,
    };
    fn event_of(made: &Made, booked: bool) -> LifecycleEvent {
        made.changes
            .lifecycle_events
            .iter()
            .find(|event| {
                !booked || event.state == meridian_domain::v1::LifecycleEventState::Booked as i32
            })
            .cloned()
            .unwrap_or_default()
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_LIFECYCLE_EVENT, move |envelope| {
            let request: RecordLifecycleEventRequest =
                decode(&envelope, "meridian.v1.RecordLifecycleEventRequest")?;
            let event = request.event.clone().unwrap_or_default();
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let mut ctx = context_of(&envelope, clock.as_ref());
            let mut named = vec![event.instrument_id.clone()];
            named.extend(event.deliveries.iter().map(|d| d.instrument_id.clone()));
            let found = references(&bus2, &named, ctx.received_at_ns);
            // Contract v29: cash a debt record's event derives -- a
            // maturity's, a paydown's -- is in its record's currency.
            let codes: Vec<String> = found
                .records
                .values()
                .filter(|said| said.debt.is_some() && !said.code.is_empty())
                .map(|said| said.code.clone())
                .collect();
            crate::cash::ensure_blocking(&bus2, &store, &codes, clock.now_ns());
            ctx.reference_versions = found.versions;
            ctx.records = found.records;
            ctx.reference_unavailable = found.unavailable;
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &event.account_id,
                &request.idempotency_key,
                "meridian.v1.RecordLifecycleEventReply",
                &|book, head, ctx| {
                    let made = crate::lifecycle::record(book, head, ctx, &request)?;
                    let reply = RecordLifecycleEventReply {
                        event: Some(event_of(&made, true)),
                    }
                    .encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(PROPOSE_LIFECYCLE_EVENTS, move |envelope| {
            let request: ProposeLifecycleEventsRequest =
                decode(&envelope, "meridian.v1.ProposeLifecycleEventsRequest")?;
            if !bounds::PROPOSE_LIFECYCLE_EVENTS_REQUEST_EVENTS_COUNT.admits(request.events.len()) {
                return Err(format!("events names {}; 1 to 500", request.events.len()));
            }
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let mut ctx = context_of(&envelope, clock.as_ref());
            let named: Vec<String> = request
                .events
                .iter()
                .map(|event| event.instrument_id.clone())
                .collect();
            let found = references(&bus2, &named, ctx.received_at_ns);
            // Contract v29: cash a debt record's event derives -- a
            // maturity's, a paydown's -- is in its record's currency.
            let codes: Vec<String> = found
                .records
                .values()
                .filter(|said| said.debt.is_some() && !said.code.is_empty())
                .map(|said| said.code.clone())
                .collect();
            crate::cash::ensure_blocking(&bus2, &store, &codes, clock.now_ns());
            ctx.reference_versions = found.versions;
            ctx.records = found.records;
            ctx.reference_unavailable = found.unavailable;
            // Each account's proposals one entry, in the order first named.
            let mut accounts: Vec<String> = Vec::new();
            for event in &request.events {
                if event.account_id.is_empty() {
                    return Err("events[].account_id: each proposal names its account".into());
                }
                if !accounts.contains(&event.account_id) {
                    accounts.push(event.account_id.clone());
                }
            }
            let mut proposed: Vec<(usize, LifecycleEvent)> = Vec::new();
            for account in &accounts {
                let mine: Vec<(usize, LifecycleEvent)> = request
                    .events
                    .iter()
                    .enumerate()
                    .filter(|(_, event)| &event.account_id == account)
                    .map(|(i, event)| (i, event.clone()))
                    .collect();
                let positions: Vec<usize> = mine.iter().map(|(i, _)| *i).collect();
                let (_, bytes) = command(
                    &bus2,
                    store.as_ref(),
                    &envelope,
                    ctx.clone(),
                    account,
                    "",
                    "meridian.v1.ProposeLifecycleEventsReply",
                    &|book, head, ctx| {
                        let made = crate::lifecycle::propose(book, head, ctx, &mine)?;
                        let reply = ProposeLifecycleEventsReply {
                            events: made.changes.lifecycle_events.clone(),
                        }
                        .encode_to_vec();
                        Ok((made, reply))
                    },
                )?;
                let answered = ProposeLifecycleEventsReply::decode(&bytes[..])
                    .map_err(|failed| failed.to_string())?;
                proposed.extend(positions.into_iter().zip(answered.events));
            }
            proposed.sort_by_key(|(i, _)| *i);
            Ok((
                "meridian.v1.ProposeLifecycleEventsReply".to_string(),
                ProposeLifecycleEventsReply {
                    events: proposed.into_iter().map(|(_, event)| event).collect(),
                }
                .encode_to_vec(),
            ))
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RELEASE_LIFECYCLE_EVENT, move |envelope| {
            let request: ReleaseLifecycleEventRequest =
                decode(&envelope, "meridian.v1.ReleaseLifecycleEventRequest")?;
            let mut ctx = context_of(&envelope, clock.as_ref());
            // The account holding the proposal: among the sender's scope, or
            // every account for a core component.
            let accounts: Vec<String> = match &ctx.account_scope {
                Some(scope) => scope.iter().cloned().collect(),
                None => store.accounts().map_err(|failed| failed.on_the_bus())?,
            };
            let mut holder = None;
            for account in accounts {
                let book = store.book(&account).map_err(|failed| failed.on_the_bus())?;
                if let Some(event) = book.lifecycle_events.get(&request.event_id) {
                    holder = Some((account, event.clone()));
                    break;
                }
            }
            let Some((account, proposal)) = holder else {
                return Err(meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::InvalidArgument,
                    &["event_id".to_string()],
                    format!(
                        "{:?} names no proposal in the accounts in the sender's scope",
                        request.event_id
                    ),
                ));
            };
            let mut named = vec![proposal.instrument_id.clone()];
            named.extend(proposal.deliveries.iter().map(|d| d.instrument_id.clone()));
            let found = references(&bus2, &named, ctx.received_at_ns);
            // Contract v29: cash a debt record's event derives -- a
            // maturity's, a paydown's -- is in its record's currency.
            let codes: Vec<String> = found
                .records
                .values()
                .filter(|said| said.debt.is_some() && !said.code.is_empty())
                .map(|said| said.code.clone())
                .collect();
            crate::cash::ensure_blocking(&bus2, &store, &codes, clock.now_ns());
            ctx.reference_versions = found.versions;
            ctx.records = found.records;
            ctx.reference_unavailable = found.unavailable;
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &account,
                "",
                "meridian.v1.ReleaseLifecycleEventReply",
                &|book, head, ctx| {
                    let made = crate::lifecycle::release(book, head, ctx, &request)?;
                    let reply = ReleaseLifecycleEventReply {
                        event: Some(event_of(&made, true)),
                    }
                    .encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let reading = store.clone();
        bus.serve(LIST_LIFECYCLE_EVENTS, move |envelope| {
            let request: ListLifecycleEventsRequest =
                decode(&envelope, "meridian.v1.ListLifecycleEventsRequest")?;
            let scope = scope_of(&envelope);
            if !request.account_id.is_empty() && !scope.holds(&request.account_id) {
                return Err(format!(
                    "{} is outside the reader's scope",
                    request.account_id
                ));
            }
            // Contract v29 (ruling 740; W9.19): no account names every
            // account in the reader's scope, as ListPositions does, each
            // event naming its account; in account order, each account's
            // in its book's sequence.
            let accounts = if request.account_id.is_empty() {
                reading
                    .accounts()
                    .map_err(|failed| failed.on_the_bus())?
                    .into_iter()
                    .filter(|account| scope.holds(account))
                    .collect::<Vec<_>>()
            } else {
                vec![request.account_id.clone()]
            };
            let since_mark = crate::store::mark_of(request.since.as_ref());
            let mut all = Vec::new();
            let mut partitions = Vec::new();
            for account in &accounts {
                let book = reading
                    .book(account)
                    .map_err(|failed| failed.on_the_bus())?;
                let since = since_mark
                    .as_ref()
                    .and_then(|mark| mark.get(&book.partition).copied())
                    .unwrap_or(0);
                all.extend(crate::lifecycle::listed(
                    &book,
                    &request.instrument_id,
                    &request.kinds,
                    &request.from_date,
                    &request.to_date,
                    since,
                ));
                if !partitions.contains(&book.partition) {
                    partitions.push(book.partition.clone());
                }
            }
            let limit = crate::store::page_limit(
                request.page_size as i32,
                bounds::LIST_LIFECYCLE_EVENTS_REQUEST_PAGE_SIZE_RANGE,
            );
            let offset: usize = request.cursor.parse().unwrap_or(0);
            let page: Vec<LifecycleEvent> = all.iter().skip(offset).take(limit).cloned().collect();
            let next = offset + page.len();
            let heads = reading.heads().map_err(|failed| failed.on_the_bus())?;
            let mut as_of = crate::store::Mark::new();
            for partition in &partitions {
                if let Some(head) = heads.get(partition) {
                    as_of.insert(partition.clone(), *head);
                }
            }
            let mut reply = ListLifecycleEventsReply {
                events: page,
                as_of: Some(crate::store::watermark_of(&as_of)),
                next_cursor: if next < all.len() {
                    next.to_string()
                } else {
                    String::new()
                },
            };
            meridian_domain::money::fill_known(&mut reply);
            Ok((
                "meridian.v1.ListLifecycleEventsReply".to_string(),
                reply.encode_to_vec(),
            ))
        });
    }
}

/// Register every handler the book serves.
pub fn serve(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_OPENING_BALANCE, move |envelope| {
            let request: RecordOpeningBalanceRequest =
                decode(&envelope, "meridian.v1.RecordOpeningBalanceRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let mut ctx = context_of(&envelope, clock.as_ref());
            let named = decide::instruments_named(&[], &request.positions);
            let found = references(&bus2, &named, ctx.received_at_ns);
            ctx.reference_versions = found.versions;
            ctx.records = found.records;
            ctx.reference_unavailable = found.unavailable;
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.BookEntryReply",
                &|book, head, ctx| {
                    let made = decide::opening_balance(book, head, ctx, &request)?;
                    let reply = entry_reply(&made).encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_BREAK, move |envelope| {
            let request: RecordBreakRequest = decode(&envelope, "meridian.v1.RecordBreakRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let ctx = context_of(&envelope, clock.as_ref());
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.BookEntryReply",
                &|book, head, ctx| {
                    let made = decide::record_break(book, head, ctx, &request)?;
                    let reply = entry_reply(&made).encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_ACCOUNT_FIGURES, move |envelope| {
            let request: RecordAccountFiguresRequest =
                decode(&envelope, "meridian.v1.RecordAccountFiguresRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let ctx = context_of(&envelope, clock.as_ref());
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.BookEntryReply",
                &|book, head, ctx| {
                    let made = decide::account_figures(book, head, ctx, &request)?;
                    let reply = entry_reply(&made).encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_ENCUMBRANCES, move |envelope| {
            let request: RecordEncumbrancesRequest =
                decode(&envelope, "meridian.v1.RecordEncumbrancesRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.BookEntryReply",
                &|book, head, ctx| {
                    let made = decide::encumbrances(book, head, ctx, &request)?;
                    let reply = entry_reply(&made).encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    serve_lifecycle(&bus, &store, &clock);
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(HANDLE_BREAK, move |envelope| {
            let request: HandleBreakRequest = decode(&envelope, "meridian.v1.HandleBreakRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.BookEntryReply",
                &|book, head, ctx| {
                    let made = decide::handle_break(book, head, ctx, &request)?;
                    let reply = entry_reply(&made).encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RESOLVE_BREAK, move |envelope| {
            let request: ResolveBreakRequest =
                decode(&envelope, "meridian.v1.ResolveBreakRequest")?;
            crate::cash::ensure_blocking(
                &bus2,
                &store,
                &meridian_domain::money::codes(&request),
                clock.now_ns(),
            );
            let mut ctx = context_of(&envelope, clock.as_ref());
            if let Some(meridian_domain::v1::resolve_break_request::Resolution::Adjustment(
                adjustment,
            )) = &request.resolution
            {
                let named = decide::instruments_named(&adjustment.lines, &[]);
                let found = references(&bus2, &named, ctx.received_at_ns);
                ctx.reference_versions = found.versions;
                ctx.records = found.records;
                ctx.reference_unavailable = found.unavailable;
            }
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.BookEntryReply",
                &|book, head, ctx| {
                    let made = decide::resolve_break(book, head, ctx, &request)?;
                    let reply = entry_reply(&made).encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(CLOSE_BREAKS_AS_CLEARED, move |envelope| {
            let request: CloseBreaksAsClearedRequest =
                decode(&envelope, "meridian.v1.CloseBreaksAsClearedRequest")?;
            let ctx = context_of(&envelope, clock.as_ref());
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                &request.idempotency_key,
                "meridian.v1.BookEntryReply",
                &|book, head, ctx| {
                    let made = decide::close_as_cleared(book, head, ctx, &request)?;
                    let reply = entry_reply(&made).encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(SET_ACCOUNT_ATTRIBUTE, move |envelope| {
            let request: SetAccountAttributeRequest =
                decode(&envelope, "meridian.v1.SetAccountAttributeRequest")?;
            // A base currency's code resolved to its cash instrument before
            // the change is decided, so the record names both (ruling 316).
            if let Some(
                meridian_domain::v1::set_account_attribute_request::Attribute::BaseCurrencyCode(
                    code,
                ),
            ) = &request.attribute
            {
                crate::cash::ensure_blocking(
                    &bus2,
                    &store,
                    std::slice::from_ref(code),
                    clock.now_ns(),
                );
            }
            let ctx = context_of(&envelope, clock.as_ref());
            command(
                &bus2,
                store.as_ref(),
                &envelope,
                ctx,
                &request.account_id,
                "",
                "meridian.v1.AccountAttributeReply",
                &|book, head, ctx| {
                    let made = decide::set_attribute(book, head, ctx, &request)?;
                    let reply = AccountAttributeReply {
                        attributes: made.changes.attributes.clone(),
                        entry: Some(made.entry.meta.clone()),
                        journal: Some(made.changes.first.clone()),
                    }
                    .encode_to_vec();
                    Ok((made, reply))
                },
            )
        });
    }

    let reading = store.clone();
    bus.serve(LIST_POSITIONS, move |envelope| {
        let request: ListPositionsRequest = decode(&envelope, "meridian.v1.ListPositionsRequest")?;
        if request.since.is_some() && (!request.business_date.is_empty() || request.at.is_some()) {
            return Err(StoreError::Invalid(
                "a read by business date or at a watermark takes no `since`".into(),
            )
            .on_the_bus());
        }
        if !request.business_date.is_empty() && !crate::dates::is_date(&request.business_date) {
            return Err(format!(
                "business_date is {:?}; a business date is YYYY-MM-DD",
                request.business_date
            ));
        }
        let page = reading
            .positions(&PositionsRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                since: mark_of(request.since.as_ref()),
                business_date: request.business_date.clone(),
                at: mark_of(request.at.as_ref()),
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_POSITIONS_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok(("meridian.v1.ListPositionsReply".to_string(), {
            let mut reply = ListPositionsReply {
                positions: page.records,
                next_cursor: page.next_cursor,
                as_of: Some(watermark_of(&page.as_of)),
            };
            meridian_domain::money::fill_known(&mut reply);
            reply.encode_to_vec()
        }))
    });

    let reading = store.clone();
    bus.serve(LIST_BREAKS, move |envelope| {
        let request: ListBreaksRequest = decode(&envelope, "meridian.v1.ListBreaksRequest")?;
        let states = request
            .states
            .iter()
            .map(|state| {
                meridian_domain::v1::BreakState::try_from(*state)
                    .map_err(|_| format!("states names {state}, which is no break state"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let page = reading
            .breaks(&BreaksRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                states,
                since: mark_of(request.since.as_ref()),
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_BREAKS_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok(("meridian.v1.ListBreaksReply".to_string(), {
            let mut reply = ListBreaksReply {
                breaks: page.records,
                next_cursor: page.next_cursor,
                as_of: Some(watermark_of(&page.as_of)),
            };
            meridian_domain::money::fill_known(&mut reply);
            reply.encode_to_vec()
        }))
    });

    let reading = store.clone();
    bus.serve(LIST_ACCOUNT_FIGURES, move |envelope| {
        let request: ListAccountFiguresRequest =
            decode(&envelope, "meridian.v1.ListAccountFiguresRequest")?;
        if request.since.is_some() && request.at.is_some() {
            return Err("a read at a watermark takes no `since`".into());
        }
        let page = reading
            .figures(&FiguresRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                agreement: request.agreement.clone(),
                from_date: request.from_date.clone(),
                to_date: request.to_date.clone(),
                since: mark_of(request.since.as_ref()),
                at: mark_of(request.at.as_ref()),
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_ACCOUNT_FIGURES_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok(("meridian.v1.ListAccountFiguresReply".to_string(), {
            let mut reply = ListAccountFiguresReply {
                figures: page.records,
                next_cursor: page.next_cursor,
                as_of: Some(watermark_of(&page.as_of)),
            };
            meridian_domain::money::fill_known(&mut reply);
            reply.encode_to_vec()
        }))
    });

    crate::order_service::serve(bus.clone(), store.clone(), clock.clone());
    // Contract v24: booking, the alert and the escape hatch (W11.26 to
    // W11.37).
    crate::booking_service::serve(bus.clone(), store.clone(), clock.clone());

    let reading = store;
    bus.serve(LIST_ACCOUNT_ATTRIBUTES, move |envelope| {
        let request: ListAccountAttributesRequest =
            decode(&envelope, "meridian.v1.ListAccountAttributesRequest")?;
        let page = reading
            .attributes(&AttributesRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                since: mark_of(request.since.as_ref()),
                limit: page_limit(
                    request.page_size,
                    bounds::LIST_ACCOUNT_ATTRIBUTES_REQUEST_PAGE_SIZE_RANGE,
                ),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok((
            "meridian.v1.ListAccountAttributesReply".to_string(),
            ListAccountAttributesReply {
                // An account whose base was set before contract v26, or
                // before its code was resolved, answered with its cash
                // instrument once the book holds the resolution (ruling 316).
                attributes: page
                    .records
                    .into_iter()
                    .map(|mut attributes| {
                        if attributes.base_cash_instrument_id.is_empty() {
                            attributes.base_cash_instrument_id =
                                meridian_domain::money::known(&attributes.base_currency_code)
                                    .unwrap_or_default();
                        }
                        attributes
                    })
                    .collect(),
                next_cursor: page.next_cursor,
                as_of: Some(watermark_of(&page.as_of)),
            }
            .encode_to_vec(),
        ))
    });
}

/// Subscribe to replacements and hand back the loop that follows them
/// (W9.9). Subscribed before it returns: at-most-once delivery drops what
/// arrives before a subscriber exists.
pub fn follow_replacements(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
) -> impl std::future::Future<Output = ()> {
    let mut replaced = bus.subscribe_shared(INSTRUMENT_REPLACED, "follow-replacements");
    async move {
        while let Some(delivery) = replaced.recv().await {
            match follow_one(&bus, &store, delivery).await {
                Ok(moved) => tracing::debug!(moved, "followed a replacement"),
                Err(why) => tracing::warn!(why, "could not follow a replacement"),
            }
        }
    }
}

/// One replacement, heard. Says how many accounts it moved.
pub async fn follow_one(
    bus: &Arc<Bus>,
    store: &Arc<dyn Store>,
    delivery: Delivery,
) -> Result<usize, String> {
    let envelope = delivery.envelope;
    let event: InstrumentReplacedEvent = decode(&envelope, "meridian.v1.InstrumentReplacedEvent")?;
    let instrument = event
        .instrument
        .map(|record| record.instrument_id)
        .unwrap_or_default();
    // Every same-instrument check compares the record each ID became
    // (contract v24, W11's invariant).
    crate::became::record(&event.replaced_instrument_id, &instrument);
    let meta = envelope.meta.clone().unwrap_or_default();
    move_accounts(
        bus,
        store,
        &event.replaced_instrument_id,
        &instrument,
        resolved_by(event.cause),
        &meta.correlation_id,
        &meta.message_id,
    )
    .await
}

/// Ask the instrument store what every instrument the book holds has become,
/// and follow each one replaced, as the event would have.
pub async fn sweep_replacements(bus: &Arc<Bus>, store: &Arc<dyn Store>) -> Result<usize, String> {
    let listing = Arc::clone(store);
    // What the book holds, and from contract v24 what its open orders name:
    // an open order's later fill compares through the replacement.
    let placeholders = tokio::task::spawn_blocking(move || {
        let mut named = listing.instruments_held()?;
        for instrument in listing.live_order_instruments()? {
            if !named.contains(&instrument) {
                named.push(instrument);
            }
        }
        crate::store::Result::Ok(named)
    })
    .await
    .map_err(|failed| format!("the sweep task failed: {failed}"))?
    .map_err(|failed| failed.to_string())?;
    let mut moved = 0;
    for placeholder in placeholders {
        let (payload_type, payload) = bus
            .call(
                RESOLVE_INSTRUMENT,
                "meridian.v1.ResolveInstrumentRequest",
                ResolveInstrumentRequest {
                    instrument_id: placeholder.clone(),
                    as_of_ns: bus.clock().now_ns(),
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
        let Some(record) = reply.instrument.filter(|_| reply.found) else {
            continue;
        };
        if record.instrument_id == placeholder || record.instrument_id.is_empty() {
            continue;
        }
        crate::became::record(&placeholder, &record.instrument_id);
        // A resolution carries the provisional record's ID under
        // open_meridian_local (contract v24, W3.8); anything else a merge.
        let resolved = record.identifiers.iter().any(|identifier| {
            identifier.scheme == "open_meridian_local" && identifier.value == placeholder
        });
        moved += move_accounts(
            bus,
            store,
            &placeholder,
            &record.instrument_id,
            resolved,
            "",
            "",
        )
        .await?;
    }
    Ok(moved)
}

/// Sweep now, and then every `every`, for as long as the process runs.
pub async fn sweep_forever(bus: Arc<Bus>, store: Arc<dyn Store>, every: Duration) {
    loop {
        match sweep_replacements(&bus, &store).await {
            Ok(moved) => tracing::debug!(moved, "swept the instruments held for merges"),
            Err(why) => tracing::warn!(why, "could not sweep the instruments held for merges"),
        }
        tokio::time::sleep(every).await;
    }
}

/// W9.9's one path, for the event and the sweep: each account holding the
/// replaced record moved in its own entry, in its own partition.
/// Whether a replacement is a provisional record resolved (contract v24,
/// `REPLACEMENT_CAUSE_RESOLVED`), journalled `instrument-resolved`.
fn resolved_by(cause: i32) -> bool {
    cause == meridian_domain::v1::ReplacementCause::Resolved as i32
}

async fn move_accounts(
    bus: &Arc<Bus>,
    store: &Arc<dyn Store>,
    placeholder: &str,
    instrument: &str,
    resolved: bool,
    correlation: &str,
    causation: &str,
) -> Result<usize, String> {
    if placeholder.is_empty() || instrument.is_empty() || placeholder == instrument {
        return Ok(0);
    }
    let (placeholder, instrument) = (placeholder.to_string(), instrument.to_string());
    let (correlation, causation) = (correlation.to_string(), causation.to_string());
    let moving = Arc::clone(store);
    let announcing = Arc::clone(bus);
    tokio::task::spawn_blocking(move || {
        let accounts = moving
            .accounts_holding(&placeholder)
            .map_err(|failed| failed.to_string())?;
        let mut moved = 0;
        for account in accounts {
            let now = announcing.clock().now_ns();
            let ctx = Context {
                instance_id: announcing.instance_id().to_string(),
                correlation_id: correlation.clone(),
                event_time_ns: now,
                received_at_ns: now,
                committed_at_ns: now,
                ..Default::default()
            };
            let acted = moving.act(&account, "", "", &[], &mut |book, head| {
                match decide::follow_replacement(
                    book,
                    head,
                    &ctx,
                    &placeholder,
                    &instrument,
                    resolved,
                )? {
                    Some(made) => {
                        let reply = entry_reply(&made).encode_to_vec();
                        Ok(decided(made, reply))
                    }
                    None => Err(StoreError::Invalid(NOTHING_HELD.into())),
                }
            });
            match acted {
                Ok(Acted::Committed(decided)) => {
                    announce(
                        &announcing,
                        &decided,
                        (!correlation.is_empty()).then_some(correlation.as_str()),
                        (!causation.is_empty()).then_some(causation.as_str()),
                    );
                    moved += 1;
                }
                Ok(Acted::Duplicate(_)) => {}
                Err(StoreError::Invalid(said)) if said == NOTHING_HELD => {}
                Err(failed) => return Err(failed.to_string()),
            }
        }
        Ok(moved)
    })
    .await
    .map_err(|failed| format!("the move task failed: {failed}"))?
}

/// An account found holding a replaced record that no longer does, by the time
/// its lock is held: nothing to move, and nothing wrong.
const NOTHING_HELD: &str = "the account holds nothing under the replaced record";
