//! Where the book meets the bus.
//!
//! Seven commands in, four queries in, one event heard, one query asked, four
//! events out. An `operations` plugin's commands -- an opening balance
//! (W9.1), a break (W9.4), the figures (W9.5), a break's handling (W9.6), its
//! resolution and its close as cleared (W9.7) -- and the dashboard's
//! attribute (W9.13) arrive as
//! commands; five roles' reads (W9.10 to W9.12, W9.14) as queries; a
//! placeholder's replacement (W3.8) as an event, which the book follows
//! (W9.9), also asking the instrument store about what it still holds under
//! one (W3.6), and asking it the reference version of each instrument a
//! command names (Q31). Every record an entry changes leaves as an event,
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
use prost::Message;

use crate::decide::{self, Context, Made};
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

pub const LIST_POSITIONS: &str = "platform.book.query.list-positions";
pub const LIST_BREAKS: &str = "platform.book.query.list-breaks";
pub const LIST_ACCOUNT_FIGURES: &str = "platform.book.query.list-account-figures";
pub const LIST_ACCOUNT_ATTRIBUTES: &str = "platform.book.query.list-account-attributes";

/// W3.8, heard (W9.9).
pub const INSTRUMENT_REPLACED: &str = "platform.reference.event.instrument-replaced";

/// W3.6, asked: an instrument's record, for its version (Q31) and for what a
/// placeholder has become.
pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";

/// How long a command waits for the instrument store's versions. Short: a
/// version not had is recorded as none, never waited for (Q31).
const VERSION_WAIT: Duration = Duration::from_secs(2);

/// How often the placeholders still held are asked about again: the
/// recovery path for a replacement this process did not hear, so slow and
/// certain, as the street store's is.
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
fn context_of(envelope: &Envelope, clock: &dyn Clock) -> Context {
    let meta = envelope.meta.clone().unwrap_or_default();
    let now = clock.now_ns();
    Context {
        message_id: meta.message_id,
        instance_id: meta.publisher_instance_id,
        acting_for: meta.acting_for_subject,
        correlation_id: meta.correlation_id,
        event_time_ns: meta.published_at_ns,
        received_at_ns: now,
        committed_at_ns: now,
        reference_versions: Default::default(),
        control_sequence: 0,
    }
}

fn expect(arrived: &str, wanted: &str) -> Result<(), String> {
    if arrived == wanted {
        return Ok(());
    }
    Err(format!("expected {wanted}, got {arrived}"))
}

fn decode<M: Message + Default>(envelope: &Envelope, wanted: &str) -> Result<M, String> {
    expect(&envelope.payload_type, wanted)?;
    M::decode(&envelope.payload[..]).map_err(|failed| format!("undecodable {wanted}: {failed}"))
}

/// The reference version of each instrument, from the instrument store,
/// where it answers in time; none where it does not (Q31).
fn versions(
    bus: &Bus,
    instruments: &[String],
    now_ns: i64,
) -> std::collections::BTreeMap<String, i64> {
    let mut found = std::collections::BTreeMap::new();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
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
            break;
        };
        if payload_type != "meridian.v1.ResolveInstrumentReply" {
            continue;
        }
        if let Ok(reply) = ResolveInstrumentReply::decode(&payload[..]) {
            if let Some(record) = reply.instrument.filter(|_| reply.found) {
                if record.version > 0 {
                    found.insert(instrument.clone(), record.version);
                }
            }
        }
    }
    found
}

/// A command's answer for the book's entries.
fn entry_reply(made: &Made) -> BookEntryReply {
    BookEntryReply {
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
    }
}

fn decided(made: Made, reply: Vec<u8>) -> Decided {
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
    for (record, previous) in &decided.changes.positions {
        say(
            POSITION_CHANGED,
            "meridian.v1.PositionChangedEvent",
            PositionChangedEvent {
                position: Some(record.clone()),
                previous_trade_date_quantity: Some(previous.to_wire()),
                entry: Some(entry.meta.clone()),
                journal: record.last_change.clone(),
                cause: Some(entry.cause.clone()),
            }
            .encode_to_vec(),
        );
    }
    for record in &decided.changes.breaks {
        say(
            BREAK_CHANGED,
            "meridian.v1.BreakChangedEvent",
            BreakChangedEvent {
                break_record: Some(record.clone()),
                entry: Some(entry.meta.clone()),
                journal: record.last_change.clone(),
                cause: Some(entry.cause.clone()),
            }
            .encode_to_vec(),
        );
    }
    for record in &decided.changes.figures {
        say(
            ACCOUNT_FIGURES_RECORDED,
            "meridian.v1.AccountFiguresRecordedEvent",
            AccountFiguresRecordedEvent {
                figures: Some(record.clone()),
                entry: Some(entry.meta.clone()),
                journal: record.last_change.clone(),
                cause: Some(entry.cause.clone()),
            }
            .encode_to_vec(),
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
type Deciding<'a> =
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
    let acted = store
        .act(
            account_id,
            &ctx.message_id,
            idempotency_key,
            &envelope.payload,
            &mut |book, head| {
                let (made, reply) = decide(book, head, &ctx)?;
                Ok(decided(made, reply))
            },
        )
        .map_err(|failed| failed.on_the_bus())?;
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
    Ok((reply_type.to_string(), reply))
}

/// Register every handler the book serves.
pub fn serve(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn Clock>) {
    {
        let (bus2, store, clock) = (bus.clone(), store.clone(), clock.clone());
        bus.serve(RECORD_OPENING_BALANCE, move |envelope| {
            let request: RecordOpeningBalanceRequest =
                decode(&envelope, "meridian.v1.RecordOpeningBalanceRequest")?;
            let mut ctx = context_of(&envelope, clock.as_ref());
            let named = decide::instruments_named(&[], &request.positions);
            ctx.reference_versions = versions(&bus2, &named, ctx.received_at_ns);
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
            let mut ctx = context_of(&envelope, clock.as_ref());
            if let Some(meridian_domain::v1::resolve_break_request::Resolution::Adjustment(
                adjustment,
            )) = &request.resolution
            {
                let named = decide::instruments_named(&adjustment.lines, &[]);
                ctx.reference_versions = versions(&bus2, &named, ctx.received_at_ns);
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
                limit: page_limit(request.page_size),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok((
            "meridian.v1.ListPositionsReply".to_string(),
            ListPositionsReply {
                positions: page.records,
                next_cursor: page.next_cursor,
                as_of: Some(watermark_of(&page.as_of)),
            }
            .encode_to_vec(),
        ))
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
                limit: page_limit(request.page_size),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok((
            "meridian.v1.ListBreaksReply".to_string(),
            ListBreaksReply {
                breaks: page.records,
                next_cursor: page.next_cursor,
                as_of: Some(watermark_of(&page.as_of)),
            }
            .encode_to_vec(),
        ))
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
                limit: page_limit(request.page_size),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok((
            "meridian.v1.ListAccountFiguresReply".to_string(),
            ListAccountFiguresReply {
                figures: page.records,
                next_cursor: page.next_cursor,
                as_of: Some(watermark_of(&page.as_of)),
            }
            .encode_to_vec(),
        ))
    });

    let reading = store;
    bus.serve(LIST_ACCOUNT_ATTRIBUTES, move |envelope| {
        let request: ListAccountAttributesRequest =
            decode(&envelope, "meridian.v1.ListAccountAttributesRequest")?;
        let page = reading
            .attributes(&AttributesRead {
                scope: scope_of(&envelope),
                account_id: request.account_id.clone(),
                since: mark_of(request.since.as_ref()),
                limit: page_limit(request.page_size),
                cursor: request.cursor.clone(),
            })
            .map_err(|failed| failed.on_the_bus())?;
        Ok((
            "meridian.v1.ListAccountAttributesReply".to_string(),
            ListAccountAttributesReply {
                attributes: page.records,
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
    let mut replaced = bus.subscribe(INSTRUMENT_REPLACED);
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
    let meta = envelope.meta.clone().unwrap_or_default();
    move_accounts(
        bus,
        store,
        &event.replaced_instrument_id,
        &instrument,
        &meta.correlation_id,
        &meta.message_id,
    )
    .await
}

/// Ask the instrument store what every placeholder the book still holds has
/// become, and follow each one replaced, as the event would have.
pub async fn sweep_placeholders(bus: &Arc<Bus>, store: &Arc<dyn Store>) -> Result<usize, String> {
    let listing = Arc::clone(store);
    let placeholders = tokio::task::spawn_blocking(move || listing.placeholder_instruments())
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
        moved += move_accounts(bus, store, &placeholder, &record.instrument_id, "", "").await?;
    }
    Ok(moved)
}

/// Sweep now, and then every `every`, for as long as the process runs.
pub async fn sweep_forever(bus: Arc<Bus>, store: Arc<dyn Store>, every: Duration) {
    loop {
        match sweep_placeholders(&bus, &store).await {
            Ok(moved) => tracing::debug!(moved, "swept the placeholders still held"),
            Err(why) => tracing::warn!(why, "could not sweep the placeholders still held"),
        }
        tokio::time::sleep(every).await;
    }
}

/// W9.9's one path, for the event and the sweep: each account holding the
/// placeholder moved in its own entry, in its own partition.
async fn move_accounts(
    bus: &Arc<Bus>,
    store: &Arc<dyn Store>,
    placeholder: &str,
    instrument: &str,
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
                match decide::follow_replacement(book, head, &ctx, &placeholder, &instrument)? {
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

/// An account found holding a placeholder that no longer does, by the time
/// its lock is held: nothing to move, and nothing wrong.
const NOTHING_HELD: &str = "the account holds nothing under the placeholder";
