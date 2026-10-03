//! What each act of W9 is, decided against the account as its journal stands.
//!
//! Every function here takes the account replayed from its journal and the
//! head of its partition, and answers with the entry -- numbered, its lines
//! with every lot identifier minted -- and the account after it, or with the
//! refusal. Nothing here writes anything: the store appends what is decided
//! here, under the partition's lock, in the transaction that read the
//! journal (W9.8).
//!
//! **Who answers for an act** (W9.1, W9.6, W9.7, W9.13): a justified act --
//! an opening balance, handling, a resolution, an attribute -- is a person's,
//! refused without one (`REFUSAL_REASON_ACTOR_REQUIRED`) or without its reason
//! (`REFUSAL_REASON_REASON_REQUIRED`). A finding -- a break, the figures -- may
//! be the plugin's own, its actor the instance. Every resolution is a
//! person's until service accounts exist (open point 3).

use std::collections::{BTreeMap, BTreeSet};

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    actor, basis_adjustment, break_cause, record_break_request, resolve_break_request,
    set_account_attribute_request, AccountAttributes, AccountFigures, Actor, BasisAdjustment,
    Break, BreakCategory, BreakCauseCategory, BreakResolution, BreakState, ChangeCause,
    CloseBreaksAsClearedRequest, Encumbrance, EncumbranceKind, EntryMeta, FigureKey,
    HandleBreakRequest, HoldingSide, LotReliefMethod, LotSource, MovementLine, OpeningBalance,
    OpeningPosition, PersonActor, PositionKey, RecordAccountFiguresRequest, RecordBreakRequest,
    RecordEncumbrancesRequest, RecordOpeningBalanceRequest, ReferenceVersion, ResolveBreakRequest,
    SetAccountAttributeRequest, SettlementBucket, SystemActor,
};
use meridian_pb::v1::RefusalReason;

use crate::book::{agreement_key, side_named, Book, Changes, Key};
use crate::dates;
use crate::ids;
use crate::journal::{Body, Entry};
use crate::numbers;
use crate::store::{Result, StoreError};

/// What an act knows beside its command: who sent it, for whom, when, and
/// the reference version of each instrument it names (Q31).
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub message_id: String,
    pub instance_id: String,
    /// The person the sidecar vouched for and stamped (W4.9); empty when the
    /// plugin acted as itself.
    pub acting_for: String,
    /// The delegation and the client that person acted through, as the
    /// sidecar stamped them beside the person (W4.9, contract v10); empty for
    /// a person at the dashboard and for a plugin acting as itself.
    pub acting_through_delegation: String,
    pub acting_through_client: String,
    pub correlation_id: String,
    pub event_time_ns: i64,
    pub received_at_ns: i64,
    pub committed_at_ns: i64,
    /// The instrument store's version of each instrument the command names,
    /// where it has one.
    pub reference_versions: BTreeMap<String, i64>,
    /// What each instrument's record says, where the instrument store holds
    /// one (W9.1, contract v10). Absent for a record the store does not hold,
    /// which lacks everything.
    pub records: BTreeMap<String, RecordSays>,
    /// The instrument store did not answer in time: the command cannot be
    /// checked, and is refused to be tried again (contract v10).
    pub reference_unavailable: bool,
    /// The control partition's sequence in force (decisions/024's note).
    pub control_sequence: u64,
}

/// What an instrument's record says that the book requires (W9.1, contract
/// v10): an asset class and a currency in force, and whether its class is
/// cash, which takes away the lots requirement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecordSays {
    pub asset_class: bool,
    pub currency: bool,
    pub cash: bool,
}

impl Context {
    fn actor(&self) -> Actor {
        Actor {
            kind: Some(if self.acting_for.is_empty() {
                actor::Kind::System(SystemActor {
                    instance_id: self.instance_id.clone(),
                })
            } else {
                actor::Kind::Person(PersonActor {
                    subject: self.acting_for.clone(),
                    delegation_id: self.acting_through_delegation.clone(),
                    client_name: if self.acting_through_delegation.is_empty() {
                        String::new()
                    } else {
                        self.acting_through_client.clone()
                    },
                })
            }),
        }
    }

    /// Refused to be tried again when the instrument store did not answer:
    /// a command the book cannot check is never admitted unchecked (contract
    /// v10; the completion spec's Q7).
    fn references_answered(&self) -> Result<()> {
        if self.reference_unavailable {
            return Err(StoreError::refused(
                RefusalReason::ReferenceUnavailable,
                "the instrument store did not answer in time, so the instruments this names \
                 could not be checked; nothing was recorded, and the same command may be sent \
                 again",
            ));
        }
        Ok(())
    }

    /// Each instrument field the book requires that `instrument`'s record
    /// lacks, by its path under `field` (W9.1, contract v10).
    fn instrument_missing(&self, field: &str, instrument: &str) -> Vec<String> {
        let says = self.records.get(instrument).copied().unwrap_or_default();
        let mut missing = Vec::new();
        if !says.asset_class {
            missing.push(format!("{field}.instrument.asset_class"));
        }
        if !says.currency {
            missing.push(format!("{field}.instrument.currency"));
        }
        missing
    }

    fn cause(&self) -> ChangeCause {
        ChangeCause {
            instance_id: self.instance_id.clone(),
            acting_for_subject: self.acting_for.clone(),
            correlation_id: self.correlation_id.clone(),
            causation_id: self.message_id.clone(),
            committed_at_ns: self.committed_at_ns,
        }
    }

    fn mint(&self, prefix: &str) -> String {
        ids::mint(prefix, self.received_at_ns)
    }

    fn today(&self) -> String {
        dates::date_of(self.received_at_ns)
    }

    /// A justified act: a person, and a reason.
    fn justified(&self, reason: &str, what: &str) -> Result<()> {
        if self.acting_for.is_empty() {
            return Err(StoreError::refused(
                RefusalReason::ActorRequired,
                format!("{what} is a person's act, and this was sent for no one"),
            ));
        }
        if reason.trim().is_empty() {
            return Err(StoreError::refused(
                RefusalReason::ReasonRequired,
                format!("{what} is a justified act, and this carries no reason"),
            ));
        }
        Ok(())
    }
}

/// An act decided: its entry, numbered, the account after it and what it
/// changed.
pub struct Made {
    pub entry: Entry,
    pub book: Book,
    pub changes: Changes,
}

/// The parts of an entry an act chooses.
struct Draft {
    entry_id: String,
    kind: &'static str,
    effective_date: String,
    reason: String,
    break_ids: Vec<String>,
    idempotency_key: String,
    actor: Actor,
    body: Body,
}

/// Number and replay a draft onto a copy of the account, and check what must
/// hold after it.
fn made(book: &Book, head: u64, ctx: &Context, draft: Draft) -> Result<Made> {
    let named: BTreeSet<&str> = draft
        .body
        .lines
        .iter()
        .map(|line| line.instrument_id.as_str())
        .chain(draft.body.breaks.iter().filter_map(break_instrument))
        .collect();
    let meta = EntryMeta {
        entry_id: draft.entry_id,
        kind: draft.kind.to_string(),
        actor: Some(draft.actor),
        event_time_ns: ctx.event_time_ns,
        received_at_ns: ctx.received_at_ns,
        effective_date: draft.effective_date,
        control_sequence: ctx.control_sequence,
        reference_versions: ctx
            .reference_versions
            .iter()
            .filter(|(instrument, _)| named.contains(instrument.as_str()))
            .map(|(instrument_id, version)| ReferenceVersion {
                instrument_id: instrument_id.clone(),
                version: *version,
            })
            .collect(),
        reason: draft.reason,
        break_ids: draft.break_ids,
        idempotency_key: draft.idempotency_key.clone(),
    };
    let mut entry = Entry {
        entry_id: meta.entry_id.clone(),
        account_id: book.account_id.clone(),
        partition: book.partition.clone(),
        first_sequence: head + 1,
        last_sequence: 0,
        message_id: ctx.message_id.clone(),
        idempotency_key: draft.idempotency_key,
        meta,
        cause: ctx.cause(),
        body: draft.body,
    };
    let mut after = book.clone();
    let changes = after.apply(&entry, false)?;
    after.check(&entry.body)?;
    entry.last_sequence = changes.last_sequence;
    Ok(Made {
        entry,
        book: after,
        changes,
    })
}

fn break_instrument(record: &Break) -> Option<&str> {
    match record.subject.as_ref()? {
        meridian_domain::v1::r#break::Subject::Position(key) => Some(key.instrument_id.as_str()),
        meridian_domain::v1::r#break::Subject::Figure(key) => {
            (!key.instrument_id.is_empty()).then_some(key.instrument_id.as_str())
        }
    }
}

fn date(field: &str, text: &str) -> Result<()> {
    if dates::is_date(text) {
        return Ok(());
    }
    Err(StoreError::Invalid(format!(
        "{field} is {text:?}; a business date is an ISO 8601 date, YYYY-MM-DD"
    )))
}

fn side(field: &str, value: i32) -> Result<HoldingSide> {
    match HoldingSide::try_from(value) {
        Ok(HoldingSide::Long) => Ok(HoldingSide::Long),
        Ok(HoldingSide::Short) => Ok(HoldingSide::Short),
        _ => Err(StoreError::Invalid(format!(
            "{field} says neither long nor short; a position says which side it is on"
        ))),
    }
}

fn admits(side: HoldingSide, quantity: Exact) -> bool {
    match side {
        HoldingSide::Short => !quantity.negated().is_negative(),
        _ => !quantity.is_negative(),
    }
}

fn opening_required(book: &Book) -> Result<(String, String)> {
    book.opening.clone().ok_or_else(|| {
        StoreError::refused(
            RefusalReason::NoOpeningBalance,
            format!(
                "account {} has no opening balance; it enters the book once, with one (W9.1)",
                book.account_id
            ),
        )
    })
}

// ── W9.1: the opening balance ───────────────────────────────────────────────

/// One bucket of an opening position, as a line is made of it.
struct Bucket {
    bucket: SettlementBucket,
    value_date: String,
    state: Option<meridian_domain::v1::PendingState>,
    quantity: Exact,
}

pub fn opening_balance(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordOpeningBalanceRequest,
) -> Result<Made> {
    ctx.justified(&request.reason, "an opening balance")?;
    if let Some((standing, as_of)) = &book.opening {
        return Err(StoreError::refused(
            RefusalReason::OpeningBalanceRecorded,
            format!(
                "account {} has an opening balance standing, {standing} as of {as_of}; it is \
                 corrected by an adjustment, or by its reversal and a new one naming it",
                book.account_id
            ),
        ));
    }
    match (
        book.reversed_openings.last(),
        request.replaces_entry_id.as_str(),
    ) {
        (None, "") => {}
        (None, named) => {
            return Err(StoreError::Invalid(format!(
                "this replaces {named}, and account {} has no reversed opening balance",
                book.account_id
            )))
        }
        (Some(reversed), named) if named != reversed => {
            return Err(StoreError::Invalid(format!(
                "account {}'s opening balance {reversed} was reversed; a new one names it as \
                 the one it replaces",
                book.account_id
            )))
        }
        _ => {}
    }
    ctx.references_answered()?;
    let missing = opening_missing(ctx, request);
    if !missing.is_empty() {
        return Err(StoreError::incomplete("the opening balance", missing));
    }
    date("as_of_date", &request.as_of_date)?;

    let mut lines = Vec::new();
    let mut seen: BTreeSet<Key> = BTreeSet::new();
    for (index, position) in request.positions.iter().enumerate() {
        let field = format!("positions[{index}]");
        if position.instrument_id.is_empty() {
            return Err(StoreError::Invalid(format!("{field} names no instrument")));
        }
        let held = side(&format!("{field}.side"), position.side)?;
        if !seen.insert((position.instrument_id.clone(), position.side)) {
            return Err(StoreError::Invalid(format!(
                "{field} names {} {} twice",
                side_named(position.side),
                position.instrument_id
            )));
        }
        lines.extend(opening_lines(ctx, &field, held, position)?);
    }

    let entry_id = ctx.mint(ids::ENTRY);
    let opening = OpeningBalance {
        entry_id: entry_id.clone(),
        as_of_date: request.as_of_date.clone(),
        sources: request.sources.clone(),
        recorded_by: Some(ctx.actor()),
        journal: None,
        reason: request.reason.clone(),
    };
    let mut attributes = book
        .attributes
        .clone()
        .unwrap_or_else(|| AccountAttributes {
            account_id: book.account_id.clone(),
            ..Default::default()
        });
    attributes.opening_balance = Some(opening.clone());
    let draft = Draft {
        entry_id,
        kind: "opening-balance",
        effective_date: request.as_of_date.clone(),
        reason: request.reason.clone(),
        break_ids: Vec::new(),
        idempotency_key: request.idempotency_key.clone(),
        actor: ctx.actor(),
        body: Body {
            lines,
            sources: request.sources.clone(),
            opening: Some(opening),
            attributes: Some(attributes),
            ..Default::default()
        },
    };
    made(book, head, ctx, draft)
}

/// Every field an opening balance leaves out that the book requires (W9.1,
/// contract v9; the product owner's required list of 2026-10-02), by its
/// path in the command: of the balance, its date and a named source; of each
/// position, its instrument with its record's asset class and currency
/// (contract v10), side and quantity, its settled quantity, each pending
/// quantity with its value date, the whole quantity settled or pending on a
/// date, and its lots -- but on cash -- each with its quantity, cost and
/// acquisition date. A value present but malformed is not missing: it is
/// refused with its words when the lines are made.
fn opening_missing(ctx: &Context, request: &RecordOpeningBalanceRequest) -> Vec<String> {
    let mut missing = Vec::new();
    if request.as_of_date.trim().is_empty() {
        missing.push("as_of_date".to_string());
    }
    if request.sources.is_empty() {
        missing.push("sources".to_string());
    } else if !request.positions.is_empty() {
        for (index, source) in request.sources.iter().enumerate() {
            if source.name.trim().is_empty() {
                missing.push(format!("sources[{index}].name"));
            }
        }
    }
    for (index, position) in request.positions.iter().enumerate() {
        let field = format!("positions[{index}]");
        if position.instrument_id.is_empty() {
            missing.push(format!("{field}.instrument_id"));
        } else {
            missing.extend(ctx.instrument_missing(&field, &position.instrument_id));
        }
        if position.side == HoldingSide::Unspecified as i32 {
            missing.push(format!("{field}.side"));
        }
        if position.trade_date_quantity.is_none() {
            missing.push(format!("{field}.trade_date_quantity"));
        }
        if position.settled_quantity.is_none() {
            missing.push(format!("{field}.settled_quantity"));
        }
        for (at, pending) in position.pending.iter().enumerate() {
            if pending.quantity.is_none() {
                missing.push(format!("{field}.pending[{at}].quantity"));
            }
            if pending.value_date.trim().is_empty() {
                missing.push(format!("{field}.pending[{at}].value_date"));
            }
        }
        // Settled and pending on a date account for the whole quantity; the
        // rest, neither, is pending whose quantity and date nobody has given.
        let rest = || -> Option<Exact> {
            let trade = Exact::from_wire(position.trade_date_quantity.as_ref()?).ok()?;
            let settled = Exact::from_wire(position.settled_quantity.as_ref()?).ok()?;
            let mut rest = trade.checked_add(settled.negated()).ok()?;
            for pending in &position.pending {
                let quantity = Exact::from_wire(pending.quantity.as_ref()?).ok()?;
                rest = rest.checked_add(quantity.negated()).ok()?;
            }
            Some(rest)
        };
        if rest().is_some_and(|rest| !rest.is_zero()) {
            missing.push(format!("{field}.pending"));
        }
        // Whether lots apply is the record's to say: cash has none. A record
        // with no class is refused naming its class above, and asked for its
        // lots once it says.
        let says = ctx.records.get(&position.instrument_id).copied();
        if position.lots.is_empty() && says.is_some_and(|says| says.asset_class && !says.cash) {
            missing.push(format!("{field}.lots"));
        }
        for (at, lot) in position.lots.iter().enumerate() {
            let lot_field = format!("{field}.lots[{at}]");
            if lot.quantity.is_none() {
                missing.push(format!("{lot_field}.quantity"));
            }
            missing.extend(terms_missing(&lot_field, lot.terms.as_ref(), ".terms"));
        }
    }
    missing
}

/// What a lot the book opens leaves out of its terms (contract v9): its cost
/// and its acquisition date. A lot of unknown cost is no longer admitted.
/// `within` is the path of the terms inside `field`: `.terms` for an opening
/// lot, `.opens_lot` for an adjustment's line.
fn terms_missing(
    field: &str,
    terms: Option<&meridian_domain::v1::LotTerms>,
    within: &str,
) -> Vec<String> {
    let mut missing = Vec::new();
    if terms.and_then(|terms| terms.cost.as_ref()).is_none() {
        missing.push(format!("{field}{within}.cost"));
    }
    if terms.is_none_or(|terms| terms.acquired_date.trim().is_empty()) {
        missing.push(format!("{field}{within}.acquired_date"));
    }
    missing
}

/// An opening position as lines opening each bucket and lot from zero (W9.2):
/// the settled quantity to settled; each pending settlement on its value
/// date. The two account for the whole quantity, which [`opening_missing`]
/// has checked (contract v9): the not-stated bucket and pending with no date,
/// which v8 opened for the rest, are no longer admitted.
fn opening_lines(
    ctx: &Context,
    field: &str,
    side: HoldingSide,
    position: &OpeningPosition,
) -> Result<Vec<MovementLine>> {
    let trade = numbers::required(
        &format!("{field}.trade_date_quantity"),
        position.trade_date_quantity.as_ref(),
    )?;
    if !admits(side, trade) {
        return Err(StoreError::Invalid(format!(
            "{field} is on the {} side with a trade-date quantity of {trade}; it is signed to \
             match its side",
            side_named(side as i32)
        )));
    }
    let settled = numbers::optional(
        &format!("{field}.settled_quantity"),
        position.settled_quantity.as_ref(),
    )?;
    let mut buckets = Vec::new();
    let mut rest = trade;
    if let Some(settled) = settled {
        buckets.push(Bucket {
            bucket: SettlementBucket::Settled,
            value_date: String::new(),
            state: None,
            quantity: settled,
        });
        rest = numbers::add(
            &format!("{field}.settled_quantity"),
            rest,
            settled.negated(),
        )?;
    }
    for (index, pending) in position.pending.iter().enumerate() {
        let quantity = numbers::required(
            &format!("{field}.pending[{index}].quantity"),
            pending.quantity.as_ref(),
        )?;
        if !pending.value_date.is_empty() {
            date(
                &format!("{field}.pending[{index}].value_date"),
                &pending.value_date,
            )?;
        }
        buckets.push(Bucket {
            bucket: SettlementBucket::Pending,
            value_date: pending.value_date.clone(),
            state: pending.state.clone(),
            quantity,
        });
        rest = numbers::add(&format!("{field}.pending"), rest, quantity.negated())?;
    }
    if !rest.is_zero() {
        return Err(StoreError::incomplete(
            "the opening balance",
            vec![format!("{field}.pending")],
        ));
    }
    buckets.retain(|bucket| !bucket.quantity.is_zero());

    let mut lots = Vec::new();
    let mut total = Exact::ZERO;
    for (index, lot) in position.lots.iter().enumerate() {
        let quantity = numbers::required(
            &format!("{field}.lots[{index}].quantity"),
            lot.quantity.as_ref(),
        )?;
        total = numbers::add(&format!("{field}.lots"), total, quantity)?;
        let mut terms = lot.terms.clone().unwrap_or_default();
        if terms.source == LotSource::Unspecified as i32 {
            terms.source = LotSource::OpeningBalance as i32;
        }
        lots.push((ctx.mint(ids::LOT), terms, quantity));
    }
    if !lots.is_empty() && total != trade {
        return Err(StoreError::refused(
            RefusalReason::LotsUnbalanced,
            format!(
                "{field}'s lots sum to {total} and its trade-date quantity is {trade}; the rest \
                 of its lots, each with its cost and acquisition date, is supplied before it \
                 is sent"
            ),
        ));
    }

    let line = |bucket: &Bucket, quantity: Exact| MovementLine {
        instrument_id: position.instrument_id.clone(),
        side: side as i32,
        bucket: bucket.bucket as i32,
        value_date: bucket.value_date.clone(),
        quantity: numbers::wire(quantity),
        pending_state: bucket.state.clone(),
        ..Default::default()
    };
    if lots.is_empty() {
        return Ok(buckets
            .iter()
            .map(|bucket| line(bucket, bucket.quantity))
            .collect());
    }

    // Every bucket's quantity across the lots, so the buckets sum to the
    // position and the lots each to theirs. Walked together where every
    // quantity is on one side; otherwise the first lot takes every bucket and
    // passes each other lot its own.
    let mut out: Vec<MovementLine> = Vec::new();
    let mut opened: BTreeSet<String> = BTreeSet::new();
    let mut push = |out: &mut Vec<MovementLine>, bucket: &Bucket, lot: usize, quantity: Exact| {
        let (lot_id, terms, _) = &lots[lot];
        let mut made = line(bucket, quantity);
        made.lot_id = lot_id.clone();
        if opened.insert(lot_id.clone()) {
            made.opens_lot = Some(terms.clone());
        }
        out.push(made);
    };
    let one_side = buckets.iter().all(|bucket| admits(side, bucket.quantity))
        && lots.iter().all(|(_, _, quantity)| admits(side, *quantity));
    if one_side {
        let size = |value: Exact| {
            if value.is_negative() {
                value.negated()
            } else {
                value
            }
        };
        let signed = |value: Exact| {
            if side == HoldingSide::Short {
                value.negated()
            } else {
                value
            }
        };
        let (mut b, mut l) = (0, 0);
        let mut left_b = buckets.first().map(|bucket| size(bucket.quantity));
        let mut left_l = lots.first().map(|(_, _, quantity)| size(*quantity));
        while let (Some(bucket_left), Some(lot_left)) = (left_b, left_l) {
            let take = bucket_left.min(lot_left);
            if !take.is_zero() {
                push(&mut out, &buckets[b], l, signed(take));
            }
            let bucket_rest = numbers::add(field, bucket_left, take.negated())?;
            let lot_rest = numbers::add(field, lot_left, take.negated())?;
            if bucket_rest.is_zero() {
                b += 1;
                left_b = buckets.get(b).map(|bucket| size(bucket.quantity));
            } else {
                left_b = Some(bucket_rest);
            }
            if lot_rest.is_zero() {
                l += 1;
                left_l = lots.get(l).map(|(_, _, quantity)| size(*quantity));
            } else {
                left_l = Some(lot_rest);
            }
        }
        // A lot of zero quantity is opened all the same.
        for index in l..lots.len() {
            if let Some(first) = buckets.first() {
                push(&mut out, first, index, Exact::ZERO);
            }
        }
    } else if let Some(first) = buckets.first() {
        for bucket in &buckets {
            push(&mut out, bucket, 0, bucket.quantity);
        }
        for (index, (_, _, quantity)) in lots.iter().enumerate().skip(1) {
            push(&mut out, first, index, *quantity);
            push(&mut out, first, 0, quantity.negated());
        }
    }
    Ok(out)
}

// ── W9.4: a break ───────────────────────────────────────────────────────────

pub fn record_break(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordBreakRequest,
) -> Result<Made> {
    opening_required(book)?;
    date("business_date", &request.business_date)?;
    let subject = request
        .subject
        .clone()
        .ok_or_else(|| StoreError::Invalid("a break names a position or a figure".into()))?;
    let subject = match subject {
        record_break_request::Subject::Position(key) => {
            check_position_key(&key)?;
            meridian_domain::v1::r#break::Subject::Position(key)
        }
        record_break_request::Subject::Figure(key) => {
            check_figure_key(&key)?;
            meridian_domain::v1::r#break::Subject::Figure(key)
        }
    };
    if request.category == BreakCategory::Unspecified as i32
        || BreakCategory::try_from(request.category).is_err()
    {
        return Err(StoreError::Invalid("a break names its category".into()));
    }
    if request.differences.is_empty() {
        return Err(StoreError::Invalid(
            "a break records each differing field with both values".into(),
        ));
    }
    for (index, cause) in request.candidate_causes.iter().enumerate() {
        check_cause(&format!("candidate_causes[{index}]"), cause)?;
    }

    let record = if request.break_id.is_empty() {
        Break {
            break_id: ctx.mint(ids::BREAK),
            account_id: book.account_id.clone(),
            subject: Some(subject),
            category: request.category,
            differences: request.differences.clone(),
            book_watermark: request.book_watermark.clone(),
            street: request.street.clone(),
            first_seen_date: request.business_date.clone(),
            last_seen_date: request.business_date.clone(),
            state: BreakState::Open as i32,
            candidate_causes: request.candidate_causes.clone(),
            recorded_by: Some(ctx.actor()),
            ..Default::default()
        }
    } else {
        let held = open_break(book, &request.break_id)?;
        if held.subject.as_ref() != Some(&subject) || held.category != request.category {
            return Err(StoreError::Invalid(format!(
                "break {} is of another subject or category; a difference of its own is a \
                 break of its own",
                request.break_id
            )));
        }
        Break {
            differences: request.differences.clone(),
            book_watermark: request.book_watermark.clone(),
            street: request.street.clone(),
            last_seen_date: request.business_date.clone(),
            candidate_causes: request.candidate_causes.clone(),
            ..held.clone()
        }
    };
    let break_id = record.break_id.clone();
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "break-recorded",
            effective_date: request.business_date.clone(),
            reason: String::new(),
            break_ids: vec![break_id],
            idempotency_key: request.idempotency_key.clone(),
            actor: ctx.actor(),
            body: Body {
                breaks: vec![record],
                ..Default::default()
            },
        },
    )
}

fn check_position_key(key: &PositionKey) -> Result<()> {
    if key.instrument_id.is_empty() {
        return Err(StoreError::Invalid(
            "a break's position names no instrument".into(),
        ));
    }
    side("position.side", key.side).map(|_| ())
}

fn check_figure_key(key: &FigureKey) -> Result<()> {
    if agreement_key(key.agreement.as_ref()).is_empty() || key.figure.is_empty() {
        return Err(StoreError::Invalid(
            "a figure break names its margin agreement and the figure".into(),
        ));
    }
    Ok(())
}

fn check_cause(field: &str, cause: &meridian_domain::v1::BreakCause) -> Result<()> {
    if cause.category == BreakCauseCategory::Unspecified as i32
        || BreakCauseCategory::try_from(cause.category).is_err()
    {
        return Err(StoreError::Invalid(format!(
            "{field} names no cause category"
        )));
    }
    if let Some(break_cause::Item::PendingSettlement(pending)) = &cause.item {
        side(&format!("{field}.pending_settlement.side"), pending.side)?;
    }
    Ok(())
}

fn held_break<'a>(book: &'a Book, break_id: &str) -> Result<&'a Break> {
    book.breaks.get(break_id).ok_or_else(|| {
        StoreError::Invalid(format!(
            "account {} holds no break {break_id}",
            book.account_id
        ))
    })
}

fn open_break<'a>(book: &'a Book, break_id: &str) -> Result<&'a Break> {
    let held = held_break(book, break_id)?;
    if held.state != BreakState::Open as i32 {
        return Err(StoreError::refused(
            RefusalReason::BreakState,
            format!(
                "break {break_id} is {}; only an open break is updated, handled, resolved or \
                 closed",
                state_named(held.state)
            ),
        ));
    }
    Ok(held)
}

fn state_named(state: i32) -> &'static str {
    match BreakState::try_from(state) {
        Ok(BreakState::Open) => "open",
        Ok(BreakState::Resolved) => "resolved",
        Ok(BreakState::Closed) => "closed",
        _ => "unspecified",
    }
}

// ── W9.5: the figures ───────────────────────────────────────────────────────

pub fn account_figures(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordAccountFiguresRequest,
) -> Result<Made> {
    opening_required(book)?;
    date("business_date", &request.business_date)?;
    if request.agreements.is_empty() {
        return Err(StoreError::Invalid(
            "a record of the figures names at least one margin agreement".into(),
        ));
    }
    let mut keys = BTreeSet::new();
    let mut figures = Vec::new();
    for (index, agreement) in request.agreements.iter().enumerate() {
        let key = agreement_key(agreement.agreement.as_ref());
        if key.is_empty() {
            return Err(StoreError::Invalid(format!(
                "agreements[{index}] names no margin agreement"
            )));
        }
        if !keys.insert(key) {
            return Err(StoreError::Invalid(format!(
                "agreements[{index}] names an agreement already in this record"
            )));
        }
        let segment = match agreement
            .agreement
            .as_ref()
            .and_then(|held| held.agreement.as_ref())
        {
            Some(meridian_domain::v1::margin_agreement_ref::Agreement::StatementSegment(held)) => {
                held.segment.clone()
            }
            None => String::new(),
        };
        let said = agreement
            .figures
            .as_ref()
            .map(|figures| figures.segment.clone())
            .unwrap_or_default();
        if said != segment {
            return Err(StoreError::Invalid(format!(
                "agreements[{index}]'s figures are for segment {said:?} and its agreement is \
                 segment {segment:?}; a set is recorded under its own agreement"
            )));
        }
        if !segment.is_empty() && !agreement.position_values.is_empty() {
            return Err(StoreError::Invalid(format!(
                "agreements[{index}] carries the custodian's values per position under segment \
                 {segment:?}; they ride on the set with no segment"
            )));
        }
        figures.push(AccountFigures {
            account_id: book.account_id.clone(),
            business_date: request.business_date.clone(),
            agreement: agreement.agreement.clone(),
            figures: agreement.figures.clone(),
            position_values: agreement.position_values.clone(),
            source: request.source.clone(),
            last_change: None,
        });
    }
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "figures-recorded",
            effective_date: request.business_date.clone(),
            reason: String::new(),
            break_ids: Vec::new(),
            idempotency_key: request.idempotency_key.clone(),
            actor: ctx.actor(),
            body: Body {
                figures,
                ..Default::default()
            },
        },
    )
}

// ── W9.15: a position's encumbrances ────────────────────────────────────────

/// The kinds the book holds (W9.15): the street's PENDING, REHYPOTHECATED and
/// BORROWED are not encumbrances of the book's.
fn book_kind(kind: i32) -> bool {
    matches!(
        EncumbranceKind::try_from(kind),
        Ok(EncumbranceKind::Pledged
            | EncumbranceKind::Posted
            | EncumbranceKind::OnLoan
            | EncumbranceKind::Blocked
            | EncumbranceKind::Restricted
            | EncumbranceKind::InTransit
            | EncumbranceKind::Other)
    )
}

/// What makes two encumbrances the same one, for its first date.
fn encumbrance_key(held: &Encumbrance) -> (i32, String, String, String) {
    (
        held.kind,
        held.pledgee.clone(),
        held.held_at.clone(),
        agreement_key(held.agreement.as_ref()),
    )
}

pub fn encumbrances(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &RecordEncumbrancesRequest,
) -> Result<Made> {
    opening_required(book)?;
    date("business_date", &request.business_date)?;
    if request.positions.is_empty() {
        return Err(StoreError::Invalid(
            "a record of encumbrances names at least one position".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut recorded = Vec::new();
    for (index, named) in request.positions.iter().enumerate() {
        let key: Key = (named.instrument_id.clone(), named.side);
        if !seen.insert(key.clone()) {
            return Err(StoreError::Invalid(format!(
                "positions[{index}] names {} again; a position's encumbrances are one set",
                named.instrument_id
            )));
        }
        let Some(position) = book.positions.get(&key).filter(|held| !held.removed) else {
            return Err(StoreError::Invalid(format!(
                "positions[{index}]: the book holds no {} {} in {}; that difference is a \
                 break of its own (W9.4)",
                side_named(named.side),
                named.instrument_id,
                book.account_id
            )));
        };
        let mut set = Vec::new();
        for (at, held) in named.encumbrances.iter().enumerate() {
            let path = format!("positions[{index}].encumbrances[{at}]");
            if held.kind == EncumbranceKind::Unspecified as i32 {
                return Err(StoreError::Invalid(format!(
                    "{path}.kind is unspecified; an encumbrance says which it is"
                )));
            }
            if !book_kind(held.kind) {
                return Err(StoreError::Invalid(format!(
                    "{path}.kind is the street's alone: pending stays in the settlement \
                     buckets, a right of use reduces nothing, and a borrow is a later slice"
                )));
            }
            if held.kind == EncumbranceKind::Other as i32 && held.source_code.is_empty() {
                return Err(StoreError::Invalid(format!(
                    "{path}.source_code is required for OTHER: the source's own code, verbatim"
                )));
            }
            numbers::required(&format!("{path}.quantity"), held.quantity.as_ref())?;
            let since = position
                .encumbrances
                .iter()
                .find(|before| encumbrance_key(before) == encumbrance_key(held))
                .map(|before| before.since_date.clone())
                .unwrap_or_else(|| request.business_date.clone());
            set.push(Encumbrance {
                source: request.source.clone(),
                since_date: since,
                set_by: None,
                ..held.clone()
            });
        }
        recorded.push((key.0, key.1, set));
    }
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "encumbrances-recorded",
            effective_date: request.business_date.clone(),
            reason: String::new(),
            break_ids: Vec::new(),
            idempotency_key: request.idempotency_key.clone(),
            actor: ctx.actor(),
            body: Body {
                encumbrances: recorded,
                ..Default::default()
            },
        },
    )
}

// ── W9.6: cause and handling ────────────────────────────────────────────────

pub fn handle_break(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &HandleBreakRequest,
) -> Result<Made> {
    ctx.justified(&request.reason, "setting a break's cause or handling")?;
    let held = open_break(book, &request.break_id)?;
    if request.confirmed_cause.is_none() && request.handling.is_none() {
        return Err(StoreError::Invalid(
            "handling a break sets its confirmed cause, its handling or both; this sets neither"
                .into(),
        ));
    }
    if let Some(cause) = &request.confirmed_cause {
        check_cause("confirmed_cause", cause)?;
    }
    if let Some(handling) = &request.handling {
        if !handling.due_date.is_empty() {
            date("handling.due_date", &handling.due_date)?;
        }
    }
    let mut record = held.clone();
    if request.confirmed_cause.is_some() {
        record.confirmed_cause = request.confirmed_cause.clone();
    }
    if request.handling.is_some() {
        record.handling = request.handling.clone();
    }
    let effective_date = record.last_seen_date.clone();
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "break-handled",
            effective_date,
            reason: request.reason.clone(),
            break_ids: vec![request.break_id.clone()],
            idempotency_key: request.idempotency_key.clone(),
            actor: ctx.actor(),
            body: Body {
                breaks: vec![record],
                ..Default::default()
            },
        },
    )
}

// ── W9.7: resolution and closing ────────────────────────────────────────────

pub fn resolve_break(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &ResolveBreakRequest,
) -> Result<Made> {
    ctx.justified(&request.reason, "resolving or closing a break")?;
    if request.break_ids.is_empty() {
        return Err(StoreError::Invalid(
            "a resolution names the breaks it resolves".into(),
        ));
    }
    let mut breaks = Vec::new();
    let mut named = BTreeSet::new();
    for break_id in &request.break_ids {
        if !named.insert(break_id.clone()) {
            return Err(StoreError::Invalid(format!(
                "break {break_id} is named twice"
            )));
        }
        breaks.push(open_break(book, break_id)?.clone());
    }
    let latest_seen = breaks
        .iter()
        .map(|held| held.last_seen_date.clone())
        .max()
        .unwrap_or_default();
    let resolution = request.resolution.clone().ok_or_else(|| {
        StoreError::Invalid(
            "a resolution says how: an adjustment, a reversal, the entries or an explanation"
                .into(),
        )
    })?;
    let resolution_of =
        |entries: Vec<meridian_domain::v1::JournalRef>, explanation: String| BreakResolution {
            entries,
            explanation,
            actor: Some(ctx.actor()),
            reason: request.reason.clone(),
            cleared_at: None,
        };

    let (kind, effective_date, mut body, state) = match resolution {
        resolve_break_request::Resolution::Adjustment(adjustment) => {
            let (_, as_of) = opening_required(book)?;
            date("adjustment.effective_date", &adjustment.effective_date)?;
            if adjustment.effective_date <= as_of {
                return Err(StoreError::refused(
                    RefusalReason::BeforeOpeningBalance,
                    format!(
                        "the adjustment is effective {} and account {}'s opening balance stands \
                         for everything up to {as_of}; an entry is effective after it",
                        adjustment.effective_date, book.account_id
                    ),
                ));
            }
            let body = adjustment_body(book, ctx, &adjustment)?;
            for held in breaks.iter_mut() {
                held.resolution = Some(resolution_of(Vec::new(), String::new()));
            }
            (
                "adjustment",
                adjustment.effective_date.clone(),
                body,
                BreakState::Resolved,
            )
        }
        resolve_break_request::Resolution::Reversal(reversal) => {
            let (body, effective_date) = reversal_body(book, &reversal.entry_id)?;
            for held in breaks.iter_mut() {
                held.resolution = Some(resolution_of(Vec::new(), String::new()));
            }
            ("reversal", effective_date, body, BreakState::Resolved)
        }
        resolve_break_request::Resolution::Entries(entries) => {
            if entries.entry_ids.is_empty() {
                return Err(StoreError::Invalid(
                    "a resolution by entries names at least one".into(),
                ));
            }
            let mut refs = Vec::new();
            for entry_id in &entries.entry_ids {
                let held = book.entry(entry_id).ok_or_else(|| {
                    StoreError::Invalid(format!(
                        "account {} holds no entry {entry_id}",
                        book.account_id
                    ))
                })?;
                refs.push(held.first.clone());
            }
            for held in breaks.iter_mut() {
                held.resolution = Some(resolution_of(refs.clone(), String::new()));
            }
            (
                "break-resolved",
                latest_seen.clone(),
                Body::default(),
                BreakState::Resolved,
            )
        }
        resolve_break_request::Resolution::Explanation(explanation) => {
            if explanation.trim().is_empty() {
                return Err(StoreError::refused(
                    RefusalReason::ReasonRequired,
                    "closing a break records its explanation, and this carries none",
                ));
            }
            for held in breaks.iter_mut() {
                held.resolution = Some(resolution_of(Vec::new(), explanation.clone()));
            }
            (
                "break-closed",
                latest_seen.clone(),
                Body::default(),
                BreakState::Closed,
            )
        }
    };
    let resolved_by_this = matches!(kind, "adjustment" | "reversal");
    for held in breaks.iter_mut() {
        held.state = state as i32;
    }
    if resolved_by_this {
        body.resolved_by_this = request.break_ids.clone();
    }
    body.breaks = breaks;
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind,
            effective_date,
            reason: request.reason.clone(),
            break_ids: request.break_ids.clone(),
            idempotency_key: request.idempotency_key.clone(),
            actor: ctx.actor(),
            body,
        },
    )
}

/// W9.7: close breaks as cleared, citing the statement where the difference
/// was gone; nothing moves (Q2 of the sample operations plugin). A person's
/// until service accounts exist.
pub fn close_as_cleared(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &CloseBreaksAsClearedRequest,
) -> Result<Made> {
    ctx.justified(&request.reason, "closing a break as cleared")?;
    if request.break_ids.is_empty() {
        return Err(StoreError::Invalid(
            "closing as cleared names the breaks it closes".into(),
        ));
    }
    let cleared_at = request
        .cleared_at
        .clone()
        .filter(|street| !street.statement_id.is_empty())
        .ok_or_else(|| {
            StoreError::Invalid(
                "closing a break as cleared cites the statement where it cleared".into(),
            )
        })?;
    let mut breaks = Vec::new();
    let mut named = BTreeSet::new();
    for break_id in &request.break_ids {
        if !named.insert(break_id.clone()) {
            return Err(StoreError::Invalid(format!(
                "break {break_id} is named twice"
            )));
        }
        let mut held = open_break(book, break_id)?.clone();
        held.state = BreakState::Closed as i32;
        held.resolution = Some(BreakResolution {
            entries: Vec::new(),
            explanation: String::new(),
            actor: Some(ctx.actor()),
            reason: request.reason.clone(),
            cleared_at: Some(cleared_at.clone()),
        });
        breaks.push(held);
    }
    let effective_date = if dates::is_date(&cleared_at.as_of_date) {
        cleared_at.as_of_date.clone()
    } else {
        breaks
            .iter()
            .map(|held| held.last_seen_date.clone())
            .max()
            .unwrap_or_default()
    };
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "break-closed",
            effective_date,
            reason: request.reason.clone(),
            break_ids: request.break_ids.clone(),
            idempotency_key: request.idempotency_key.clone(),
            actor: ctx.actor(),
            body: Body {
                breaks,
                ..Default::default()
            },
        },
    )
}

/// An adjustment's lines and basis adjustments, its new lots' identifiers
/// minted (Q28).
fn adjustment_body(
    book: &Book,
    ctx: &Context,
    adjustment: &meridian_domain::v1::Adjustment,
) -> Result<Body> {
    if adjustment.lines.is_empty() && adjustment.basis_adjustments.is_empty() {
        return Err(StoreError::Invalid(
            "an adjustment carries movement lines, basis adjustments or both".into(),
        ));
    }
    ctx.references_answered()?;
    let mut missing: Vec<String> = Vec::new();
    for (index, line) in adjustment.lines.iter().enumerate() {
        let field = format!("adjustment.lines[{index}]");
        if !line.instrument_id.is_empty() {
            missing.extend(ctx.instrument_missing(&field, &line.instrument_id));
        }
        if line.opens_lot.is_some() {
            missing.extend(terms_missing(&field, line.opens_lot.as_ref(), ".opens_lot"));
        }
    }
    if !missing.is_empty() {
        return Err(StoreError::incomplete("the adjustment", missing));
    }
    let mut lines = Vec::new();
    for (index, line) in adjustment.lines.iter().enumerate() {
        let field = format!("adjustment.lines[{index}]");
        if line.instrument_id.is_empty() {
            return Err(StoreError::Invalid(format!("{field} names no instrument")));
        }
        side(&format!("{field}.side"), line.side)?;
        numbers::required(&format!("{field}.quantity"), line.quantity.as_ref())?;
        match SettlementBucket::try_from(line.bucket) {
            Ok(SettlementBucket::Settled) => {}
            // A pending line may name no value date: the street carries none,
            // and a proposal matching the custodian's split cannot invent one.
            Ok(SettlementBucket::Pending) if line.value_date.is_empty() => {}
            Ok(SettlementBucket::Pending) => {
                date(&format!("{field}.value_date"), &line.value_date)?
            }
            Ok(SettlementBucket::NotStated) => {
                return Err(StoreError::Invalid(format!(
                    "{field} is not stated, which the book no longer admits (contract v9)"
                )))
            }
            _ => {
                return Err(StoreError::Invalid(format!(
                    "{field} names no settlement bucket"
                )))
            }
        }
        let mut made = line.clone();
        if let Some(terms) = made.opens_lot.as_mut() {
            if !line.lot_id.is_empty() {
                return Err(StoreError::Invalid(format!(
                    "{field} opens a lot and names lot {}; a lot it opens is minted by the book",
                    line.lot_id
                )));
            }
            if terms.source == LotSource::Unspecified as i32 {
                terms.source = LotSource::Adjustment as i32;
            }
            made.lot_id = ctx.mint(ids::LOT);
        } else if !line.lot_id.is_empty() {
            let key = (line.instrument_id.clone(), line.side);
            let holds = book
                .positions
                .get(&key)
                .is_some_and(|position| position.lots.contains_key(&line.lot_id));
            if !holds {
                return Err(StoreError::Invalid(format!(
                    "{field} names lot {}, which {} {} does not hold",
                    line.lot_id,
                    side_named(line.side),
                    line.instrument_id
                )));
            }
        }
        lines.push(made);
    }
    let mut prior = Vec::new();
    for (index, basis) in adjustment.basis_adjustments.iter().enumerate() {
        let field = format!("adjustment.basis_adjustments[{index}]");
        let Some(key) = book.lot_owner(&basis.lot_id) else {
            return Err(StoreError::Invalid(format!(
                "{field} names lot {}, which account {} does not hold",
                basis.lot_id, book.account_id
            )));
        };
        let lot = &book.positions[key].lots[&basis.lot_id].lot;
        let cost = lot.terms.as_ref().and_then(|terms| terms.cost.as_ref());
        match (&basis.cost, cost) {
            (Some(basis_adjustment::Cost::CostChange(change)), Some(cost)) => {
                numbers::add_money(&format!("{field}.cost_change"), cost, change)?;
            }
            (Some(basis_adjustment::Cost::CostChange(_)), None) => {
                return Err(StoreError::Invalid(format!(
                    "{field}: lot {}'s cost is unknown; state it (stated_cost) rather than \
                     change it",
                    basis.lot_id
                )))
            }
            (Some(basis_adjustment::Cost::StatedCost(_)), Some(_)) => {
                return Err(StoreError::Invalid(format!(
                    "{field}: lot {}'s cost is known; a known cost is changed by an amount \
                     (cost_change), not stated again",
                    basis.lot_id
                )))
            }
            (Some(basis_adjustment::Cost::StatedCost(stated)), None) => {
                numbers::amount(&format!("{field}.stated_cost"), stated)?;
            }
            (None, _) if basis.holding_period_start.is_empty() => {
                return Err(StoreError::Invalid(format!("{field} changes nothing")))
            }
            _ => {}
        }
        if !basis.holding_period_start.is_empty() {
            date(
                &format!("{field}.holding_period_start"),
                &basis.holding_period_start,
            )?;
        }
        prior.push(
            lot.terms
                .as_ref()
                .map(|terms| terms.holding_period_start.clone())
                .unwrap_or_default(),
        );
    }
    Ok(Body {
        lines,
        basis_adjustments: adjustment.basis_adjustments.clone(),
        prior_holding_period_starts: prior,
        event_reference: adjustment.event_reference.clone(),
        ..Default::default()
    })
}

/// A reversal: the reversed entry's lines negated, at its effective date
/// (W9.7); of an opening balance, only while no later entry moving the
/// account's positions stands (W9.1, open point 5).
fn reversal_body(book: &Book, entry_id: &str) -> Result<(Body, String)> {
    let reversed = book.entry(entry_id).ok_or_else(|| {
        StoreError::Invalid(format!(
            "account {} holds no entry {entry_id}",
            book.account_id
        ))
    })?;
    if reversed.reversed_by.is_some() {
        return Err(StoreError::Invalid(format!(
            "entry {entry_id} is reversed already"
        )));
    }
    if !reversed.body.moves_positions() {
        return Err(StoreError::Invalid(format!(
            "entry {entry_id} is a {}, which moves no position; a break's record is corrected \
             by its own act",
            reversed.kind
        )));
    }
    let opening = book
        .opening
        .as_ref()
        .is_some_and(|(standing, _)| standing == entry_id);
    if opening {
        let index = book
            .entries
            .iter()
            .position(|held| held.entry_id == entry_id)
            .expect("found above");
        let later = book.entries[index + 1..].iter().find(|held| {
            held.body.moves_positions()
                && held.reversed_by.is_none()
                && held.body.reverses.is_empty()
        });
        if let Some(later) = later {
            return Err(StoreError::refused(
                RefusalReason::LaterEntriesStand,
                format!(
                    "entry {} moves account {}'s positions after its opening balance; reverse \
                     it first, or a new opening balance would count it twice",
                    later.entry_id, book.account_id
                ),
            ));
        }
    }
    let lines = reversed
        .body
        .lines
        .iter()
        .map(|line| {
            let mut negated = line.clone();
            negated.opens_lot = None;
            negated.quantity = line.quantity.as_ref().map(|quantity| {
                Exact::from_wire(quantity)
                    .map(|value| value.negated().to_wire())
                    .unwrap_or(*quantity)
            });
            negated
        })
        .collect();
    let mut restored_unknown_costs = Vec::new();
    let basis_adjustments = reversed
        .body
        .basis_adjustments
        .iter()
        .zip(reversed.body.prior_holding_period_starts.iter())
        .map(|(adjustment, prior)| BasisAdjustment {
            lot_id: adjustment.lot_id.clone(),
            cost: match &adjustment.cost {
                Some(basis_adjustment::Cost::CostChange(change)) => {
                    let mut negated = change.clone();
                    negated.amount = change.amount.as_ref().map(|amount| {
                        Exact::from_wire(amount)
                            .map(|value| value.negated().to_wire())
                            .unwrap_or(*amount)
                    });
                    Some(basis_adjustment::Cost::CostChange(negated))
                }
                Some(basis_adjustment::Cost::StatedCost(_)) => {
                    restored_unknown_costs.push(adjustment.lot_id.clone());
                    None
                }
                None => None,
            },
            holding_period_start: prior.clone(),
        })
        .collect();
    let mut body = Body {
        lines,
        basis_adjustments,
        restored_unknown_costs,
        reverses: entry_id.to_string(),
        clears_opening: opening,
        ..Default::default()
    };
    if opening {
        let mut attributes = book
            .attributes
            .clone()
            .unwrap_or_else(|| AccountAttributes {
                account_id: book.account_id.clone(),
                ..Default::default()
            });
        attributes.opening_balance = None;
        body.attributes = Some(attributes);
    }
    Ok((body, reversed.effective_date.clone()))
}

// ── W9.13: an attribute ─────────────────────────────────────────────────────

pub fn set_attribute(
    book: &Book,
    head: u64,
    ctx: &Context,
    request: &SetAccountAttributeRequest,
) -> Result<Made> {
    ctx.justified(&request.reason, "setting an account's attribute")?;
    let mut attributes = book
        .attributes
        .clone()
        .unwrap_or_else(|| AccountAttributes {
            account_id: book.account_id.clone(),
            ..Default::default()
        });
    match request.attribute.clone() {
        Some(set_account_attribute_request::Attribute::BaseCurrencyCode(code)) => {
            if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_uppercase()) {
                return Err(StoreError::Invalid(format!(
                    "base_currency_code is {code:?}; an ISO 4217 code is three capital letters"
                )));
            }
            attributes.base_currency_code = code;
        }
        Some(set_account_attribute_request::Attribute::LotReliefDefault(method)) => {
            if method == LotReliefMethod::Unspecified as i32
                || LotReliefMethod::try_from(method).is_err()
            {
                return Err(StoreError::Invalid(
                    "lot_relief_default names no method of relieving lots".into(),
                ));
            }
            attributes.lot_relief_default = method;
        }
        None => return Err(StoreError::Invalid("the request sets no attribute".into())),
    }
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: "attribute-set",
            effective_date: ctx.today(),
            reason: request.reason.clone(),
            break_ids: Vec::new(),
            idempotency_key: String::new(),
            actor: ctx.actor(),
            body: Body {
                attributes: Some(attributes),
                ..Default::default()
            },
        },
    )
}

// ── W9.9: a merged record followed ──────────────────────────────────────────

/// The kind of entry that moves what a merged record held (W9.9, contract
/// v10). v8 and v9 journalled `placeholder-moved`, which stands as recorded.
pub const INSTRUMENT_MERGED: &str = "instrument-merged";

/// Move what the account holds under `placeholder`, a record merged into
/// another (or before v10 a placeholder replaced by its INS- ID), onto
/// `instrument`: lines closing each bucket and lot under the one and opening
/// the same under the other, the lots keeping their identifiers, costs and
/// dates; the replaced record's positions kept as tombstones; the open breaks
/// naming it brought onto the record that stays. The book's own act: no person, no
/// instance, nothing to refuse. `None` when the account holds nothing under
/// it.
pub fn follow_replacement(
    book: &Book,
    head: u64,
    ctx: &Context,
    placeholder: &str,
    instrument: &str,
) -> Result<Option<Made>> {
    let mut lines = Vec::new();
    let mut tombstones = Vec::new();
    let mut encumbrances = Vec::new();
    for ((instrument_id, side), position) in &book.positions {
        if instrument_id != placeholder || position.removed {
            continue;
        }
        tombstones.push((instrument_id.clone(), *side));
        // Its encumbrances, as recorded, follow it onto the instrument.
        if !position.encumbrances.is_empty() {
            encumbrances.push((instrument.to_string(), *side, position.encumbrances.clone()));
        }
        let mut buckets: Vec<(SettlementBucket, String, Option<_>, Exact)> = Vec::new();
        if !position.settled.is_zero() {
            buckets.push((
                SettlementBucket::Settled,
                String::new(),
                None,
                position.settled,
            ));
        }
        for (value_date, (quantity, state)) in &position.pending {
            buckets.push((
                SettlementBucket::Pending,
                value_date.clone(),
                state.clone(),
                *quantity,
            ));
        }
        if !position.not_stated.is_zero() {
            buckets.push((
                SettlementBucket::NotStated,
                String::new(),
                None,
                position.not_stated,
            ));
        }
        let line = |to: &str,
                    bucket: &(
            SettlementBucket,
            String,
            Option<meridian_domain::v1::PendingState>,
            Exact,
        ),
                    quantity: Exact| {
            MovementLine {
                instrument_id: to.to_string(),
                side: *side,
                bucket: bucket.0 as i32,
                value_date: bucket.1.clone(),
                quantity: numbers::wire(quantity),
                pending_state: bucket.2.clone(),
                ..Default::default()
            }
        };
        for bucket in &buckets {
            lines.push(line(placeholder, bucket, bucket.3.negated()));
            lines.push(line(instrument, bucket, bucket.3));
        }
        // Each open lot carried across whole: closed under the placeholder,
        // opened under the instrument with its identifier and record.
        let carrier = buckets.first().cloned().unwrap_or((
            SettlementBucket::Settled,
            String::new(),
            None,
            Exact::ZERO,
        ));
        for state in position.lots.values() {
            let open = numbers::required("lots.open_quantity", state.lot.open_quantity.as_ref())?;
            if open.is_zero() {
                continue;
            }
            // Opened under the instrument first, so the record it carries is
            // the lot's before the placeholder's closes it.
            let mut into = line(instrument, &carrier, open);
            into.lot_id = state.lot.lot_id.clone();
            into.opens_lot = Some(state.lot.terms.clone().unwrap_or_default());
            let mut out = line(placeholder, &carrier, open.negated());
            out.lot_id = state.lot.lot_id.clone();
            // The lot lines move no bucket: each pair nets to zero there.
            let forth = line(instrument, &carrier, open.negated());
            let back = line(placeholder, &carrier, open);
            lines.extend([into, out, forth, back]);
        }
    }
    let mut breaks = Vec::new();
    for held in book.breaks.values() {
        if held.state != BreakState::Open as i32 {
            continue;
        }
        let mut moved = held.clone();
        let named = match moved.subject.as_mut() {
            Some(meridian_domain::v1::r#break::Subject::Position(key))
                if key.instrument_id == placeholder =>
            {
                key.instrument_id = instrument.to_string();
                true
            }
            Some(meridian_domain::v1::r#break::Subject::Figure(key))
                if key.instrument_id == placeholder =>
            {
                key.instrument_id = instrument.to_string();
                true
            }
            _ => false,
        };
        if named {
            breaks.push(moved);
        }
    }
    if lines.is_empty() && tombstones.is_empty() && breaks.is_empty() {
        return Ok(None);
    }
    let break_ids = breaks.iter().map(|held| held.break_id.clone()).collect();
    made(
        book,
        head,
        ctx,
        Draft {
            entry_id: ctx.mint(ids::ENTRY),
            kind: INSTRUMENT_MERGED,
            effective_date: ctx.today(),
            reason: String::new(),
            break_ids,
            idempotency_key: String::new(),
            actor: Actor {
                kind: Some(actor::Kind::System(SystemActor {
                    instance_id: String::new(),
                })),
            },
            body: Body {
                lines,
                tombstones,
                breaks,
                encumbrances,
                ..Default::default()
            },
        },
    )
    .map(Some)
}

/// The instruments a command names, whose reference versions an entry
/// records (Q31).
pub fn instruments_named(lines: &[MovementLine], positions: &[OpeningPosition]) -> Vec<String> {
    let mut named: BTreeSet<String> = lines
        .iter()
        .map(|line| line.instrument_id.clone())
        .collect();
    named.extend(
        positions
            .iter()
            .map(|position| position.instrument_id.clone()),
    );
    named.into_iter().filter(|id| !id.is_empty()).collect()
}
