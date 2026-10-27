//! A job's inputs, resolved on its client's behalf (W12.2; the one-API
//! spec's requirements 3 and 6 to 10; Q6, Q11) into the inputs document the
//! engine answers and the answer's hash is taken over.
//!
//! No new row: the component asks the reads the contract names it on --
//! `ListAccountAttributes` and `ListPositions` of the book,
//! `ResolveInstrument`, `ReadTemplate` and `ListEventGroups` of the
//! instrument store, `ListPrices` and `ListOutcomes` of the lake, and
//! `ListModelRouting` of the conductor; from contract v27 (ruling 510), for
//! an `execution_cost` job, `ListOrders` and `ReadOrderHistory` of the book
//! for the job's accounts -- each carrying the client's
//! instance and the account scope its sidecar stamped on the job, so the
//! book answers within that scope and the lake within that instance's
//! entitlements and its role's kinds. One watermark and one as-of per job
//! (requirements 7, 8): each account's partition read once, the lake at one
//! recorded-time cut-off.
//!
//! Every list in the document is written in one order whatever order the
//! stores answered in, so the same stores give the same document, and the
//! same hash.

use std::collections::{BTreeMap, BTreeSet};

use meridian_analytics_engine::compile::Compiled;
use meridian_bus::{refusal_with_status, RefusalStatus};
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    self as v1, EventGroupFlag, FxPivotCurrenciesSetting, Job, ListAccountAttributesReply,
    ListAccountAttributesRequest, ListEventGroupsReply, ListEventGroupsRequest,
    ListModelRoutingReply, ListModelRoutingRequest, ListOrdersReply, ListOrdersRequest,
    ListOutcomesReply, ListOutcomesRequest, ListPositionsReply, ListPositionsRequest,
    ListPricesReply, ListPricesRequest, Money, OrderHistoryReply, OrderHistoryRequest,
    OutcomeStage, PriceKind, ReadFxPivotCurrenciesRequest, ReadTemplateReply, ReadTemplateRequest,
    ResolveInstrumentReply, ResolveInstrumentRequest, SubjectRef, UnansweredReason,
};
use prost::Message;
use serde_json::{json, Value};

use crate::topics;

/// Asks a store a question for the job: the transport, so a test can stand
/// the stores up on an in-memory bus.
pub trait Ask: Send + Sync {
    fn ask(&self, topic: &str, kind: &str, payload: Vec<u8>) -> Result<(String, Vec<u8>), String>;
}

fn ask<Q: Message, R: Message + Default>(
    asker: &dyn Ask,
    topic: &str,
    kind: &str,
    request: &Q,
) -> Result<R, String> {
    let (reply_type, payload) = asker.ask(topic, kind, request.encode_to_vec())?;
    R::decode(&payload[..]).map_err(|failed| format!("{reply_type} did not read: {failed}"))
}

/// The lake's largest page, and its most subjects per read.
const PAGE: u32 = 500;
const SUBJECTS: usize = 500;
/// The book's and the instrument store's page.
const BOOK_PAGE: i32 = 500;
/// How far back a history read reaches before the as-of: enough business
/// days for a 500-day window and its gaps.
const HISTORY_REACH_NS: i64 = 900 * 86_400 * 1_000_000_000;
/// The kinds a conversion leg is read in, in the order it is taken.
const LEG_KINDS: [PriceKind; 3] = [PriceKind::Close, PriceKind::Mid, PriceKind::Last];
/// The most conversion legs (the first release's Q10).
const MOST_LEGS: usize = 3;

/// A job resolved: its inputs document, and what pricing it through a
/// routed plugin needs.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub inputs: Value,
    /// Model id to the `pricing` instance it is routed to, where any is.
    pub routes: BTreeMap<String, String>,
    /// Contract v27 (W10.11): each execution an `execution_cost` job
    /// measures, by its ID: its instrument and trade date, which its
    /// `ExecutionCost` rows are recorded under.
    pub executions: BTreeMap<String, (String, String)>,
}

/// A refusal of the job, as a bus error carries it.
pub type Refused = String;

fn text_of(decimal: Option<&meridian_pb::v1::Decimal>) -> Option<String> {
    decimal
        .and_then(|d| Exact::from_wire(d).ok())
        .map(|e| e.to_string())
}

fn kind_name(kind: i32) -> String {
    PriceKind::try_from(kind)
        .map(|k| {
            k.as_str_name()
                .trim_start_matches("PRICE_KIND_")
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

fn kind_of(name: &str) -> Option<PriceKind> {
    PriceKind::from_str_name(&format!("PRICE_KIND_{}", name.to_ascii_uppercase()))
}

fn flag_name(flag: i32) -> String {
    EventGroupFlag::try_from(flag)
        .map(|f| f.as_str_name().to_string())
        .unwrap_or_default()
}

fn watermark_json(watermark: &BTreeMap<String, u64>) -> Value {
    Value::Array(
        watermark
            .iter()
            .map(|(partition, sequence)| {
                json!({"partition": partition, "sequence": sequence.to_string()})
            })
            .collect(),
    )
}

/// What one instrument's record and template say, as the document holds it.
#[derive(Debug, Clone)]
struct Record {
    record_version: String,
    template_id: String,
    template_version: String,
    calculation_version: String,
    model_id: String,
    model_version: i64,
    asset_class: String,
    currency_code: String,
    /// Contract v27 (ruling 462): a fiat cash instrument's ISO 4217 code,
    /// from its `iso4217` identifier; empty for a token and every other
    /// instrument.
    iso_code: String,
    price_kind: String,
    payout: Option<(String, String, String)>,
}

/// The pricing input kinds that are a price, as the lake names them.
const PRICE_INPUTS: [&str; 5] = ["close", "nav", "last", "mid", "settlement"];

/// The price kind an instrument is marked at: its package's first pricing
/// input that is a price. A fund's package naming both `close` and `nav`
/// (the first release's Q8: an ETF at the exchange's close, a mutual fund
/// at its NAV) is read by the record: a fund listed on a venue is
/// exchange-traded, at `close`; one listed nowhere, and a money-market
/// fund, at `nav`. No price input named is `close`.
pub fn price_kind_of(
    model_id: &str,
    pricing_inputs: &[String],
    record: &v1::InstrumentRecord,
) -> String {
    if model_id == "cash" {
        return String::new();
    }
    let names = |kind: &str| pricing_inputs.iter().any(|p| p == kind);
    if names("close") && names("nav") {
        let listed = !record.exchange_mic.is_empty() || !record.listing_venue_id.is_empty();
        let money_market = record.money_market_fund.is_some()
            || record.instrument_type == v1::InstrumentType::MoneyMarketFund as i32;
        return if listed && !money_market {
            "close"
        } else {
            "nav"
        }
        .to_string();
    }
    pricing_inputs
        .iter()
        .find(|p| PRICE_INPUTS.contains(&p.as_str()))
        .cloned()
        .unwrap_or_else(|| "close".to_string())
}

/// Resolve a compiled job on its client's behalf.
pub fn resolve(
    asker: &dyn Ask,
    job: &Job,
    compiled: &Compiled,
    engine_version: &str,
    now_ns: i64,
) -> Result<Resolved, Refused> {
    let inputs = job.inputs.clone().unwrap_or_default();
    let as_of = if inputs.lake_as_of_ns > 0 {
        inputs.lake_as_of_ns
    } else {
        now_ns
    };
    let sources = inputs.sources.clone();
    // A job read at now, whose inputs' ages at this hop are measured
    // (W4.16; ruling 606): never one read as of a past time or a date.
    let live = inputs.lake_as_of_ns == 0 && job.business_date.is_empty();
    // Contract v27 (W12.1; ruling 458): a business date -- the book at its
    // end, the lake at its closes -- never one after today's.
    let business_date = job.business_date.clone();
    if !business_date.is_empty() {
        let today = meridian_domain::date::Date::of_utc_instant(now_ns).to_string();
        if business_date > today {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &["business_date".to_string()],
                format!("business_date: {business_date} is after today's, {today}"),
            ));
        }
    }

    // Contract v27 (W6.36; ruling 483): the deployment's pivot order, by
    // cash instrument; never set, USD's and then EUR's, as this job's stores
    // name them (below).
    let pivots: FxPivotCurrenciesSetting = ask(
        asker,
        topics::READ_FX_PIVOT_CURRENCIES,
        "meridian.v1.ReadFxPivotCurrenciesRequest",
        &ReadFxPivotCurrenciesRequest {},
    )
    .unwrap_or_default();

    // The routing table in force (W12.6), named by its version.
    let routing: ListModelRoutingReply = ask(
        asker,
        topics::LIST_MODEL_ROUTING,
        "meridian.v1.ListModelRoutingRequest",
        &ListModelRoutingRequest {},
    )
    .map_err(|failed| format!("the routing table could not be read: {failed}"))?;
    let routes: BTreeMap<String, String> = routing
        .routes
        .iter()
        .filter(|r| !r.implementation.is_empty() && r.implementation != "built-in")
        .map(|r| (r.model_id.clone(), r.implementation.clone()))
        .collect();

    // Currency codes to the cash instruments they name, as the stores say.
    let mut codes: BTreeMap<String, String> = BTreeMap::new();

    // The accounts' base currencies (W9.14).
    let mut base_codes: Vec<(String, String)> = Vec::new();
    for (i, account) in compiled.reads.accounts.iter().enumerate() {
        let reply: ListAccountAttributesReply = ask(
            asker,
            topics::LIST_ACCOUNT_ATTRIBUTES,
            "meridian.v1.ListAccountAttributesRequest",
            &ListAccountAttributesRequest {
                account_id: account.clone(),
                page_size: 1,
                ..Default::default()
            },
        )
        .map_err(|failed| read_refused(&format!("account_ids[{i}]"), account, &failed))?;
        let Some(attributes) = reply
            .attributes
            .into_iter()
            .find(|a| &a.account_id == account)
        else {
            return Err(refusal_with_status(
                RefusalStatus::PermissionDenied,
                &[format!("account_ids[{i}]")],
                format!("{account} is not an account this client may read"),
            ));
        };
        // The base's cash instrument, as the book names it (ruling 316).
        if !attributes.base_cash_instrument_id.is_empty() {
            codes.insert(
                attributes.base_currency_code.clone(),
                attributes.base_cash_instrument_id.clone(),
            );
        }
        base_codes.push((account.clone(), attributes.base_currency_code));
    }

    // The positions, each account's partition at one point (W9.10).
    let mut watermark: BTreeMap<String, u64> = BTreeMap::new();
    let mut positions: Vec<(String, v1::BookPosition)> = Vec::new();
    for (i, account) in compiled.reads.accounts.iter().enumerate() {
        let mut cursor = String::new();
        loop {
            let reply: ListPositionsReply = ask(
                asker,
                topics::LIST_POSITIONS,
                "meridian.v1.ListPositionsRequest",
                &ListPositionsRequest {
                    account_id: account.clone(),
                    at: inputs.book_at.clone(),
                    business_date: business_date.clone(),
                    page_size: BOOK_PAGE,
                    cursor: cursor.clone(),
                    ..Default::default()
                },
            )
            .map_err(|failed| {
                if inputs.book_at.is_some() {
                    refusal_with_status(
                        RefusalStatus::InvalidArgument,
                        &["inputs.book_at".to_string()],
                        format!("inputs.book_at: the book cannot serve it for {account}: {failed}"),
                    )
                } else {
                    read_refused(&format!("account_ids[{i}]"), account, &failed)
                }
            })?;
            for partition in reply.as_of.map(|w| w.partitions).unwrap_or_default() {
                watermark.insert(partition.partition, partition.sequence);
            }
            positions.extend(
                reply
                    .positions
                    .into_iter()
                    .filter(|p| !p.removed)
                    .map(|p| (account.clone(), p)),
            );
            if reply.next_cursor.is_empty() {
                break;
            }
            cursor = reply.next_cursor;
        }
    }

    // Contract v27 (W10.11, W11.17; ruling 510): an `execution_cost` job's
    // executions, read with their orders, placements and the orders'
    // history for the job's accounts.
    let measured = if compiled.reads.executions {
        executions(asker, &compiled.reads.accounts, &business_date)?
    } else {
        Vec::new()
    };

    // Each instrument's record and its template's package (W3.6, W3.24).
    let mut wanted: BTreeSet<String> = positions
        .iter()
        .map(|(_, p)| p.instrument_id.clone())
        // An execution's instrument and the asset its price is in.
        .chain(
            measured
                .iter()
                .flat_map(|e| [e.instrument_id.clone(), e.price_instrument_id.clone()]),
        )
        .chain(compiled.reads.what_if_instruments.iter().cloned())
        // Contract v27: the pivots' and the reporting currency's records,
        // which say whether each is fiat (ruling 462).
        .chain(pivots.fx_pivot_currencies.iter().cloned())
        .chain(Some(job.reporting_currency.clone()).filter(|r| !r.is_empty()))
        .collect();
    let mut groups: BTreeMap<String, v1::EventGroup> = BTreeMap::new();
    let named_groups: Vec<String> = compiled.reads.event_groups.iter().cloned().collect();
    if !named_groups.is_empty() {
        for group in event_groups(asker, Vec::new(), named_groups, as_of)? {
            wanted.extend(group.member_instrument_ids.iter().cloned());
            groups.insert(group.event_group_id.clone(), group);
        }
    }
    let mut records: BTreeMap<String, Record> = BTreeMap::new();
    let mut templates: BTreeMap<(String, String), Option<v1::CalculationPackage>> = BTreeMap::new();
    for id in wanted.iter().filter(|id| !id.starts_with("LCL-")) {
        if let Some(record) = record(asker, id, as_of, &mut templates)? {
            if record.asset_class == "cash" && !record.currency_code.is_empty() {
                codes
                    .entry(record.currency_code.clone())
                    .or_insert(id.clone());
            }
            records.insert(id.clone(), record);
        }
    }
    // The groups the event contracts held are in, for historical VaR's
    // resolution worst case.
    if compiled.reads.history_dates > 0 {
        let events: Vec<String> = records
            .iter()
            .filter(|(_, r)| r.model_id == "binary_event")
            .map(|(id, _)| id.clone())
            .collect();
        if !events.is_empty() {
            for group in event_groups(asker, events, Vec::new(), as_of)? {
                groups.entry(group.event_group_id.clone()).or_insert(group);
            }
        }
    }

    // Money's instruments: where the stores say which cash instrument a
    // currency's code names.
    for (_, position) in &positions {
        for lot in &position.lots {
            if let Some(cost) = lot.terms.as_ref().and_then(|t| t.cost.as_ref()) {
                learn(&mut codes, cost);
            }
        }
    }

    // The market at the as-of (W10.6): each instrument's price of its
    // template's kind, a group member's bid, ask and last, and each
    // conversion leg, leg by leg, at most three deep (Q10).
    let mut market: Vec<Value> = Vec::new();
    let mut unentitled: BTreeMap<(String, String), String> = BTreeMap::new();
    // Contract v28 (W12.2; ruling 619): inputs whose row in force is
    // invalid, by subject and kind, naming the dataset.
    let mut quarantined: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut asked: BTreeSet<(String, i32)> = BTreeSet::new();
    let members: BTreeSet<String> = groups
        .values()
        .flat_map(|g| g.member_instrument_ids.iter().cloned())
        .collect();
    let mut wants: BTreeMap<String, BTreeSet<i32>> = BTreeMap::new();
    for (id, record) in &records {
        if record.model_id != "cash" {
            if let Some(kind) = kind_of(&record.price_kind) {
                wants.entry(id.clone()).or_default().insert(kind as i32);
            }
        }
        if members.contains(id) {
            for kind in [PriceKind::Bid, PriceKind::Ask, PriceKind::Last] {
                wants.entry(id.clone()).or_default().insert(kind as i32);
            }
        }
        if record.model_id == "cash" {
            for kind in LEG_KINDS {
                wants.entry(id.clone()).or_default().insert(kind as i32);
            }
        }
        if let Some((_, currency, _)) = &record.payout {
            for kind in LEG_KINDS {
                wants
                    .entry(currency.clone())
                    .or_default()
                    .insert(kind as i32);
            }
        }
    }
    // Contract v27 (ruling 462): the bases', the pivots' and the reporting
    // currency's own prices too, so a fiat leg with no rate in the lake's
    // direction is inverted or crossed from the rate the other way.
    let mut fiat_ends: BTreeSet<String> = pivots.fx_pivot_currencies.iter().cloned().collect();
    if !job.reporting_currency.is_empty() {
        fiat_ends.insert(job.reporting_currency.clone());
    }
    for (_, code) in &base_codes {
        if let Some(id) = codes.get(code) {
            fiat_ends.insert(id.clone());
        }
    }
    for id in &fiat_ends {
        let kinds = wants.entry(id.clone()).or_default();
        kinds.extend(LEG_KINDS.iter().map(|k| *k as i32));
    }
    let mut depth = 0;
    while !wants.is_empty() && depth <= MOST_LEGS {
        let mut next: BTreeMap<String, BTreeSet<i32>> = BTreeMap::new();
        let mut by_kinds: BTreeMap<Vec<i32>, Vec<String>> = BTreeMap::new();
        for (subject, kinds) in wants {
            let kinds: Vec<i32> = kinds
                .into_iter()
                .filter(|k| asked.insert((subject.clone(), *k)))
                .collect();
            if !kinds.is_empty() {
                by_kinds.entry(kinds).or_default().push(subject);
            }
        }
        for (kinds, subjects) in by_kinds {
            for chunk in subjects.chunks(SUBJECTS) {
                let rows = prices(
                    asker,
                    chunk,
                    &kinds,
                    &sources,
                    as_of,
                    &business_date,
                    &mut unentitled,
                    &mut quarantined,
                )?;
                for price in rows {
                    let Some(row) = market_row(&price) else {
                        continue;
                    };
                    if live {
                        if let Some(meta) = price.meta.as_ref() {
                            age_at_analytics(meta, now_ns);
                        }
                    }
                    if let Some(money) = price.price.as_ref() {
                        learn(&mut codes, money);
                    }
                    let held = row["instrument_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    // The currency it is in, converted next.
                    if !held.is_empty() {
                        let legs = next.entry(held).or_default();
                        legs.extend(LEG_KINDS.iter().map(|k| *k as i32));
                    }
                    market.push(row);
                }
            }
        }
        wants = next;
        depth += 1;
    }

    // Contract v27 (C2, C4): the mid in force at each moment an execution is
    // measured at, in the asset of its price; the engine takes the latest
    // at or before the moment within the staleness bound.
    let mut moments: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for execution in &measured {
        for moment in execution.moments() {
            moments
                .entry(moment)
                .or_default()
                .insert(execution.instrument_id.clone());
        }
    }
    for (moment, subjects) in moments {
        let subjects: Vec<String> = subjects.into_iter().collect();
        for chunk in subjects.chunks(SUBJECTS) {
            for price in mids_at(asker, chunk, &sources, moment, as_of)? {
                if let Some(row) = market_row(&price) {
                    market.push(row);
                }
            }
        }
    }

    // Each base currency's cash instrument's record, which a what-if's cash
    // leg is a position in.
    for (_, code) in &base_codes {
        if let Some(id) = codes.get(code).cloned() {
            if let std::collections::btree_map::Entry::Vacant(slot) = records.entry(id) {
                let id = slot.key().clone();
                if let Some(record) = record(asker, &id, as_of, &mut templates)? {
                    slot.insert(record);
                }
            }
        }
    }

    // Outcomes of the event contracts (W10.6): final only at the venue's
    // final determination (the first release's Q2).
    let mut outcomes: Vec<Value> = Vec::new();
    let events: Vec<String> = records
        .iter()
        .filter(|(_, r)| r.model_id == "binary_event")
        .map(|(id, _)| id.clone())
        .collect();
    for chunk in events.chunks(SUBJECTS) {
        let reply: ListOutcomesReply = ask(
            asker,
            topics::LIST_OUTCOMES,
            "meridian.v1.ListOutcomesRequest",
            &ListOutcomesRequest {
                subjects: chunk
                    .iter()
                    .map(|id| SubjectRef {
                        entity_id: id.clone(),
                        ..Default::default()
                    })
                    .collect(),
                after_watermark: None,
                as_of_ns: as_of,
            },
        )
        .map_err(|failed| format!("the lake's outcomes could not be read: {failed}"))?;
        let mut latest: BTreeMap<String, v1::Outcome> = BTreeMap::new();
        for outcome in reply.outcomes {
            let meta = outcome.meta.clone().unwrap_or_default();
            let Some(subject) = meta.subjects.first().map(|s| s.entity_id.clone()) else {
                continue;
            };
            let newer = latest.get(&subject).is_none_or(|held| {
                let held = held.meta.clone().unwrap_or_default();
                (meta.valid_from_ns, meta.version) > (held.valid_from_ns, held.version)
            });
            if newer {
                latest.insert(subject, outcome);
            }
        }
        for (subject, outcome) in latest {
            let meta = outcome.meta.clone().unwrap_or_default();
            let settlement = outcome
                .settlement_value
                .as_ref()
                .and_then(|m| text_of(m.amount.as_ref()))
                .unwrap_or_else(|| "0".to_string());
            outcomes.push(json!({
                "instrument_id": subject,
                "final": outcome.stage == OutcomeStage::Final as i32,
                "settlement_value": settlement,
                "dataset": meta.source.map(|s| s.dataset).unwrap_or_default(),
            }));
        }
    }

    // The history for historical VaR and ES: each object's closes by
    // business date, the window's dates the union of those seen.
    let (dates, closes) = if compiled.reads.history_dates > 0 {
        let mut subjects: BTreeMap<String, i32> = BTreeMap::new();
        for (id, record) in &records {
            if record.model_id == "quoted_price" {
                if let Some(kind) = kind_of(&record.price_kind) {
                    subjects.insert(id.clone(), kind as i32);
                }
            }
            if record.model_id == "cash" {
                subjects.insert(id.clone(), PriceKind::Close as i32);
            }
        }
        for row in &market {
            if let Some(held) = row["instrument_id"].as_str() {
                subjects
                    .entry(held.to_string())
                    .or_insert(PriceKind::Close as i32);
            }
        }
        history(
            asker,
            &subjects,
            &sources,
            as_of,
            compiled.reads.history_dates,
        )?
    } else {
        (Vec::new(), Vec::new())
    };

    // The document, every list in one order.
    let mut accounts = Vec::new();
    for (i, (account, code)) in base_codes.iter().enumerate() {
        let Some(base) = codes.get(code) else {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &[format!("account_ids[{i}]")],
                format!(
                    "{account}'s base currency {code} names no cash instrument the job's \
                     stores answered"
                ),
            ));
        };
        accounts.push(json!({"account_id": account, "base_currency_instrument_id": base}));
    }
    let mut position_rows: Vec<Value> = Vec::new();
    for (account, position) in &positions {
        let base_code = base_codes
            .iter()
            .find(|(a, _)| a == account)
            .map(|(_, c)| c.clone())
            .unwrap_or_default();
        let base = codes.get(&base_code).cloned().unwrap_or_default();
        position_rows.extend(position_rows_of(account, position, &base, &base_code));
    }
    position_rows.sort_by(|a, b| {
        (
            a["account_id"].as_str(),
            a["instrument_id"].as_str(),
            a["sleeve"].as_str(),
        )
            .cmp(&(
                b["account_id"].as_str(),
                b["instrument_id"].as_str(),
                b["sleeve"].as_str(),
            ))
    });
    let instruments: Vec<Value> = records
        .iter()
        .map(|(id, r)| {
            json!({
                "instrument_id": id,
                "record_version": r.record_version,
                "template_id": r.template_id,
                "template_version": r.template_version,
                "calculation_version": r.calculation_version,
                "model_id": r.model_id,
                "model_version": r.model_version,
                "asset_class": r.asset_class,
                "currency_instrument_id": codes.get(&r.currency_code).cloned().unwrap_or_else(|| r.currency_code.clone()),
                "price_kind": r.price_kind,
                "payout": r.payout.as_ref().map(|(amount, currency, _)| json!({"amount": amount, "instrument_id": currency})),
                "country": "",
                // Contract v27 (ruling 462): fiat alone is inverted or crossed.
                "currency_code": r.iso_code,
            })
        })
        .collect();
    // The pivots in force: the setting's, or never set, USD's cash
    // instrument and then EUR's, as this job's stores named them (W6.36).
    let pivot_ids: Vec<String> = if pivots.fx_pivot_currencies.is_empty() {
        ["USD", "EUR"]
            .iter()
            .filter_map(|code| codes.get(*code).cloned())
            .collect()
    } else {
        pivots.fx_pivot_currencies.clone()
    };
    market.sort_by_key(|row| {
        (
            row["subject_id"].as_str().unwrap_or_default().to_string(),
            row["kind"].as_str().unwrap_or_default().to_string(),
            row["dataset"].as_str().unwrap_or_default().to_string(),
            row["valid_at_ns"].as_str().unwrap_or_default().to_string(),
            row["amount"].as_str().unwrap_or_default().to_string(),
        )
    });
    market.dedup();
    let event_groups: Vec<Value> = groups
        .values()
        .map(|g| {
            json!({
                "event_group_id": g.event_group_id,
                "version": g.version.to_string(),
                "member_instrument_ids": g.member_instrument_ids,
                "mutually_exclusive": flag_name(g.mutually_exclusive),
                "exhaustive": flag_name(g.exhaustive),
            })
        })
        .collect();
    let mut document = json!({
        "fx_pivot_currencies": pivot_ids,
        "engine_version": engine_version,
        "job": job_json(job, compiled),
        "book_watermark": watermark_json(&watermark),
        "lake_as_of_ns": as_of.to_string(),
        "routing_version": routing.version.to_string(),
        "accounts": accounts,
        "positions": position_rows,
        // Contract v27 (rulings 320 and 396): each closed lot's relief with
        // its proceeds, for pnl version 2's realized; none where no measure
        // asks.
        "closed_lots": if compiled.definitions.iter().any(|d| d.definition == "pnl" && d.version >= 2) {
            closed_lots(&positions, &codes, &base_codes)
        } else {
            Vec::new()
        },
        "instruments": instruments,
        "market": market,
        "history": {"dates": dates, "closes": closes},
        "outcomes": outcomes,
        "event_groups": event_groups,
        "stale_after_ns": {},
    });
    if compiled.reads.executions {
        document["executions"] = Value::Array(measured.iter().map(Measured::to_json).collect());
    }
    // Contract v28 (W12.1, W12.2, W12.8): the chains, resolved, and the
    // scenario set; written only where the job reads them.
    if compiled.reads.chains {
        let held: BTreeSet<String> = positions
            .iter()
            .map(|(_, p)| p.instrument_id.clone())
            .collect();
        let rate = crate::chains::rate(asker, compiled, as_of, &sources);
        let mut placing = crate::chains::Placing::default();
        let mut chains = Vec::new();
        for chain in crate::chains::refs(asker, job, compiled, &held, as_of) {
            chains.push(crate::chains::chain(
                asker,
                &chain,
                &sources,
                as_of,
                Some(&rate),
                &mut placing,
                live,
            )?);
        }
        document["chains"] = Value::Array(chains);
    }
    if let Some(set) = crate::chains::scenario(asker, job)? {
        document["scenario"] = set;
    }
    if !quarantined.is_empty() {
        document["quarantined"] = Value::Array(
            quarantined
                .iter()
                .map(|((subject, kind), dataset)| {
                    json!({"subject_id": subject, "kind": kind, "dataset": dataset})
                })
                .collect(),
        );
    }
    if !unentitled.is_empty() {
        document["unentitled"] = Value::Array(
            unentitled
                .iter()
                .map(|((subject, kind), dataset)| {
                    json!({"subject_id": subject, "kind": kind, "dataset": dataset})
                })
                .collect(),
        );
    }
    Ok(Resolved {
        inputs: document,
        routes,
        executions: measured
            .iter()
            .map(|e| {
                (
                    e.execution_id.clone(),
                    (e.instrument_id.clone(), e.trade_date.clone()),
                )
            })
            .collect(),
    })
}

/// One execution an `execution_cost` job measures (contract v27, W10.11;
/// C2, C7, C8), as the book answers it: the order's proposal (decision) and
/// release to the desk (arrival) from its history, the placement's
/// `routed_at_ns` (placement arrival) and the fill's trade time.
#[derive(Debug, Clone, PartialEq)]
struct Measured {
    execution_id: String,
    order_id: String,
    placement_id: String,
    account_id: String,
    instrument_id: String,
    side: String,
    price: String,
    price_instrument_id: String,
    quantity: String,
    trade_date: String,
    decision_at_ns: i64,
    arrival_at_ns: i64,
    placement_arrival_at_ns: i64,
    executed_at_ns: i64,
    fees: Option<String>,
    liquidity_indicator: String,
    venue_id: String,
    time_precision: String,
    reject_count: i64,
    time_to_fill_ns: i64,
}

impl Measured {
    /// The moments it is measured at, each not 0.
    fn moments(&self) -> Vec<i64> {
        [
            self.decision_at_ns,
            self.arrival_at_ns,
            self.placement_arrival_at_ns,
            self.executed_at_ns,
        ]
        .into_iter()
        .filter(|moment| *moment > 0)
        .collect()
    }

    /// As the engine's inputs document holds it (the reference cases'
    /// shape).
    fn to_json(&self) -> Value {
        json!({
            "execution_id": self.execution_id,
            "order_id": self.order_id,
            "placement_id": self.placement_id,
            "account_id": self.account_id,
            "instrument_id": self.instrument_id,
            "side": self.side,
            "price": {"amount": self.price, "instrument_id": self.price_instrument_id},
            "quantity": self.quantity,
            "decision_at_ns": self.decision_at_ns.to_string(),
            "arrival_at_ns": self.arrival_at_ns.to_string(),
            "placement_arrival_at_ns": self.placement_arrival_at_ns.to_string(),
            "executed_at_ns": self.executed_at_ns.to_string(),
            "fees": self.fees.as_ref().map(|amount| json!({
                "amount": amount, "instrument_id": self.price_instrument_id,
            })),
            "financing": Value::Null,
            "liquidity_indicator": self.liquidity_indicator,
            "venue_id": self.venue_id,
            "time_precision": self.time_precision,
            "reject_count": self.reject_count.to_string(),
            "time_to_fill_ns": self.time_to_fill_ns.to_string(),
        })
    }
}

/// The name of an enum value, empty for unspecified.
fn named(name: &str) -> String {
    if name.ends_with("_UNSPECIFIED") {
        String::new()
    } else {
        name.to_string()
    }
}

/// Each account's executions an `execution_cost` job measures (contract
/// v27, W10.11; ruling 510): every page of `ListOrders` for the account --
/// its orders, their placements and executions -- and each measured order's
/// history for its proposal and its release. A fill matched to an order is
/// measured; one unmatched, answered don't know or superseded is not. With a
/// business date, the executions traded on it alone.
fn executions(
    asker: &dyn Ask,
    accounts: &[String],
    business_date: &str,
) -> Result<Vec<Measured>, Refused> {
    let mut out = Vec::new();
    for (i, account) in accounts.iter().enumerate() {
        let mut orders: BTreeMap<String, v1::Order> = BTreeMap::new();
        let mut placements: BTreeMap<String, v1::Placement> = BTreeMap::new();
        let mut fills: BTreeMap<String, v1::Execution> = BTreeMap::new();
        let mut cursor = String::new();
        loop {
            let reply: ListOrdersReply = ask(
                asker,
                topics::LIST_ORDERS,
                "meridian.v1.ListOrdersRequest",
                &ListOrdersRequest {
                    account_id: account.clone(),
                    page_size: BOOK_PAGE,
                    cursor: cursor.clone(),
                    ..Default::default()
                },
            )
            .map_err(|failed| read_refused(&format!("account_ids[{i}]"), account, &failed))?;
            for order in reply.orders {
                for placement in &order.placements {
                    placements.insert(placement.placement_id.clone(), placement.clone());
                }
                for fill in &order.executions {
                    fills.insert(fill.execution_id.clone(), fill.clone());
                }
                orders.insert(order.order_id.clone(), order);
            }
            for placement in reply.placements {
                placements.insert(placement.placement_id.clone(), placement);
            }
            for fill in reply.executions {
                fills.insert(fill.execution_id.clone(), fill);
            }
            if reply.next_cursor.is_empty() {
                break;
            }
            cursor = reply.next_cursor;
        }
        let superseded: BTreeSet<String> = fills
            .values()
            .filter(|f| !f.supersedes.is_empty())
            .map(|f| f.supersedes.clone())
            .collect();
        let kept: Vec<&v1::Execution> = fills
            .values()
            .filter(|f| {
                f.account_id == *account
                    && !f.order_id.is_empty()
                    && orders.contains_key(&f.order_id)
                    && !superseded.contains(&f.execution_id)
                    && matches!(
                        v1::ExecutionState::try_from(f.state),
                        Ok(v1::ExecutionState::Matched | v1::ExecutionState::Unspecified)
                    )
                    && (business_date.is_empty() || f.trade_date == business_date)
            })
            .collect();
        // Each measured order's proposal and release, from its history.
        let mut moments: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for order_id in kept
            .iter()
            .map(|f| f.order_id.clone())
            .collect::<BTreeSet<_>>()
        {
            let mut proposed = 0;
            let mut released = 0;
            let mut cursor = String::new();
            loop {
                let reply: OrderHistoryReply = ask(
                    asker,
                    topics::ORDER_HISTORY,
                    "meridian.v1.OrderHistoryRequest",
                    &OrderHistoryRequest {
                        account_id: account.clone(),
                        order_id: order_id.clone(),
                        page_size: BOOK_PAGE,
                        cursor: cursor.clone(),
                    },
                )
                .map_err(|failed| read_refused(&format!("account_ids[{i}]"), account, &failed))?;
                for entry in reply.entries {
                    let at = if entry.event_time_ns > 0 {
                        entry.event_time_ns
                    } else {
                        entry.received_at_ns
                    };
                    match entry.kind.as_str() {
                        "order-proposed" if proposed == 0 => proposed = at,
                        "order-released" if released == 0 => released = at,
                        _ => {}
                    }
                }
                if reply.next_cursor.is_empty() {
                    break;
                }
                cursor = reply.next_cursor;
            }
            moments.insert(order_id, (proposed, released));
        }
        for fill in kept {
            let Some(order) = orders.get(&fill.order_id) else {
                continue;
            };
            let Some(price) = fill.price.as_ref() else {
                continue;
            };
            let (Some(amount), Some(quantity)) = (
                text_of(price.amount.as_ref()),
                text_of(fill.quantity.as_ref()),
            ) else {
                continue;
            };
            let placement = placements.get(&fill.placement_id);
            let (decision, arrival) = moments.get(&fill.order_id).copied().unwrap_or_default();
            // A manual placement's agreed time, else its routing (C8).
            let (placed_at, precision) = match placement {
                Some(p) if p.agreed_at_ns > 0 => (p.agreed_at_ns, p.agreed_precision),
                Some(p) => (p.routed_at_ns, p.routed_precision),
                None => (0, 0),
            };
            // Fees in the asset of the price, summed (C3); none stated, none.
            let mut fees: Option<Exact> = None;
            for fee in &fill.fees {
                if let Some(money) = fee.amount.as_ref() {
                    let same = money.instrument_id == price.instrument_id
                        || (!money.currency_code.is_empty()
                            && money.currency_code == price.currency_code);
                    if let (true, Some(value)) = (
                        same,
                        money.amount.as_ref().and_then(|d| Exact::from_wire(d).ok()),
                    ) {
                        fees = Some(match fees {
                            None => value,
                            Some(held) => held.checked_add(value).map_err(|_| {
                                format!("{}'s fees are out of range", fill.execution_id)
                            })?,
                        });
                    }
                }
            }
            // The order's placements the venue rejected: last-look
            // rejections, never a cost (C7).
            let reject_count = order
                .placements
                .iter()
                .filter(|p| p.rejection_reason == v1::RejectionReason::VenueRejected as i32)
                .count() as i64;
            out.push(Measured {
                execution_id: fill.execution_id.clone(),
                order_id: fill.order_id.clone(),
                placement_id: fill.placement_id.clone(),
                account_id: account.clone(),
                instrument_id: fill.instrument_id.clone(),
                side: named(
                    v1::Side::try_from(fill.side)
                        .unwrap_or(v1::Side::Unspecified)
                        .as_str_name(),
                ),
                price: amount,
                price_instrument_id: price.instrument_id.clone(),
                quantity,
                trade_date: fill.trade_date.clone(),
                decision_at_ns: decision,
                arrival_at_ns: arrival,
                placement_arrival_at_ns: placed_at,
                executed_at_ns: fill.trade_time_ns,
                fees: fees.map(|f| f.to_string()),
                liquidity_indicator: named(
                    v1::LiquidityIndicator::try_from(fill.liquidity_indicator)
                        .unwrap_or(v1::LiquidityIndicator::Unspecified)
                        .as_str_name(),
                ),
                venue_id: fill.venue_id.clone(),
                time_precision: named(
                    v1::TimePrecision::try_from(precision)
                        .unwrap_or(v1::TimePrecision::Unspecified)
                        .as_str_name(),
                ),
                reject_count,
                time_to_fill_ns: if placed_at > 0 && fill.trade_time_ns >= placed_at {
                    fill.trade_time_ns - placed_at
                } else {
                    0
                },
            });
        }
    }
    out.sort_by(|a, b| (&a.account_id, &a.execution_id).cmp(&(&b.account_id, &b.execution_id)));
    Ok(out)
}

/// The mids of these subjects in force at a moment, as of the job's
/// recorded-time cut (contract v27, C4), every page.
fn mids_at(
    asker: &dyn Ask,
    subjects: &[String],
    sources: &Option<v1::SourceChoice>,
    moment: i64,
    as_of: i64,
) -> Result<Vec<v1::Price>, Refused> {
    let mut out = Vec::new();
    let mut cursor = String::new();
    loop {
        let reply: ListPricesReply = ask(
            asker,
            topics::LIST_PRICES,
            "meridian.v1.ListPricesRequest",
            &ListPricesRequest {
                subjects: subjects
                    .iter()
                    .map(|s| SubjectRef {
                        entity_id: s.clone(),
                        ..Default::default()
                    })
                    .collect(),
                kinds: vec![PriceKind::Mid as i32],
                sources: sources.clone(),
                at_ns: moment,
                as_of_ns: as_of,
                page_size: PAGE,
                cursor: cursor.clone(),
                ..Default::default()
            },
        )
        .map_err(|failed| format!("the lake's mids could not be read: {failed}"))?;
        out.extend(reply.prices);
        if reply.next_cursor.is_empty() {
            break;
        }
        cursor = reply.next_cursor;
    }
    Ok(out)
}

/// Where a store's money says which cash instrument a currency's code
/// names, remembered.
fn learn(codes: &mut BTreeMap<String, String>, money: &Money) {
    if !money.currency_code.is_empty() && !money.instrument_id.is_empty() {
        codes
            .entry(money.currency_code.clone())
            .or_insert(money.instrument_id.clone());
    }
}

/// A store's refusal of a read for an account: outside the client's scope,
/// or not served.
fn read_refused(field: &str, account: &str, failed: &str) -> Refused {
    if failed.to_ascii_lowercase().contains("scope") || failed.contains("permission") {
        return refusal_with_status(
            RefusalStatus::PermissionDenied,
            &[field.to_string()],
            format!("{account} is not an account this client may read"),
        );
    }
    format!("{field}: {account} could not be read: {failed}")
}

/// One instrument's record and its template's package, where the store
/// holds it.
fn record(
    asker: &dyn Ask,
    id: &str,
    as_of: i64,
    templates: &mut BTreeMap<(String, String), Option<v1::CalculationPackage>>,
) -> Result<Option<Record>, Refused> {
    let reply: ResolveInstrumentReply = ask(
        asker,
        topics::RESOLVE_INSTRUMENT,
        "meridian.v1.ResolveInstrumentRequest",
        &ResolveInstrumentRequest {
            instrument_id: id.to_string(),
            as_of_ns: as_of,
        },
    )
    .map_err(|failed| format!("the instrument store did not resolve {id}: {failed}"))?;
    let Some(record) = reply.instrument.filter(|_| reply.found) else {
        return Err(format!("the instrument store holds no record of {id}"));
    };
    let key = (record.template_id.clone(), record.template_version.clone());
    if !key.0.is_empty() && !templates.contains_key(&key) {
        let read: Result<ReadTemplateReply, String> = ask(
            asker,
            topics::READ_TEMPLATE,
            "meridian.v1.ReadTemplateRequest",
            &ReadTemplateRequest {
                template_id: key.0.clone(),
                template_version: key.1.clone(),
            },
        );
        let package = read
            .ok()
            .and_then(|r| r.template)
            .and_then(|t| t.calculation_package);
        templates.insert(key.clone(), package);
    }
    let package = templates.get(&key).cloned().flatten();
    let class = v1::AssetClass::try_from(record.asset_class).unwrap_or(v1::AssetClass::Unspecified);
    let asset_class = meridian_domain::asset_class::lower(class);
    // A template version published before v26 binds no model: its class's
    // (the first release's §1 table).
    let (model_id, model_version, pricing_inputs, calculation_version) = match &package {
        Some(p) if !p.model_id.is_empty() => (
            p.model_id.clone(),
            p.model_version as i64,
            p.pricing_inputs.clone(),
            p.calculation_version.clone(),
        ),
        _ => {
            let model = match asset_class.as_str() {
                "cash" => "cash",
                "event_contract" => "binary_event",
                _ => "quoted_price",
            };
            let kind = match (asset_class.as_str(), &record.money_market_fund) {
                ("cash", _) => vec![],
                ("event_contract", _) => vec!["last".to_string()],
                ("fund", Some(_)) => vec!["nav".to_string()],
                _ => vec!["close".to_string()],
            };
            (model.to_string(), 1, kind, String::new())
        }
    };
    let price_kind = price_kind_of(&model_id, &pricing_inputs, &record);
    let iso_code = if asset_class == "cash" {
        record
            .identifiers
            .iter()
            .find(|i| i.scheme == "iso4217")
            .map(|i| i.value.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let payout = record
        .binary_event_contract
        .as_ref()
        .and_then(|c| c.payout.as_ref())
        .and_then(|m| {
            text_of(m.amount.as_ref())
                .map(|amount| (amount, m.instrument_id.clone(), m.currency_code.clone()))
        });
    Ok(Some(Record {
        record_version: record.version.to_string(),
        template_id: record.template_id,
        template_version: record.template_version,
        calculation_version,
        model_id,
        model_version,
        asset_class,
        currency_code: record.currency,
        iso_code,
        price_kind,
        payout,
    }))
}

fn event_groups(
    asker: &dyn Ask,
    instrument_ids: Vec<String>,
    event_group_ids: Vec<String>,
    as_of: i64,
) -> Result<Vec<v1::EventGroup>, Refused> {
    let mut out = Vec::new();
    let mut cursor = String::new();
    loop {
        let reply: ListEventGroupsReply = ask(
            asker,
            topics::LIST_EVENT_GROUPS,
            "meridian.v1.ListEventGroupsRequest",
            &ListEventGroupsRequest {
                instrument_ids: instrument_ids.clone(),
                event_group_ids: event_group_ids.clone(),
                as_of_ns: as_of,
                after_sequence: 0,
                page_size: BOOK_PAGE,
                cursor: cursor.clone(),
            },
        )
        .map_err(|failed| format!("the event groups could not be read: {failed}"))?;
        out.extend(reply.event_groups);
        if reply.next_cursor.is_empty() {
            break;
        }
        cursor = reply.next_cursor;
    }
    Ok(out)
}

/// Prices of these subjects and kinds at the as-of, every page; what a
/// dataset would have answered and the client is not entitled to, noted.
///
/// Contract v28 (W12.2; rulings 421 and 619): a job's read leaves invalid
/// rows out as every default read does. Where a subject and kind found no
/// row and a dataset was source silent for the subject, that dataset is read
/// again by name with `include_quarantined`: where the row in force there
/// is invalid, the input is quarantined, noted naming the dataset. A row
/// answered invalid -- to a job naming its datasets with
/// `include_quarantined` -- is never used, and is noted the same way.
#[allow(clippy::too_many_arguments)]
fn prices(
    asker: &dyn Ask,
    subjects: &[String],
    kinds: &[i32],
    sources: &Option<v1::SourceChoice>,
    as_of: i64,
    business_date: &str,
    unentitled: &mut BTreeMap<(String, String), String>,
    quarantined: &mut BTreeMap<(String, String), String>,
) -> Result<Vec<v1::Price>, Refused> {
    let read = |subjects: &[String],
                sources: &Option<v1::SourceChoice>|
     -> Result<Vec<ListPricesReply>, Refused> {
        let mut replies = Vec::new();
        let mut cursor = String::new();
        loop {
            let reply: ListPricesReply = ask(
                asker,
                topics::LIST_PRICES,
                "meridian.v1.ListPricesRequest",
                &ListPricesRequest {
                    subjects: subjects
                        .iter()
                        .map(|s| SubjectRef {
                            entity_id: s.clone(),
                            ..Default::default()
                        })
                        .collect(),
                    kinds: kinds.to_vec(),
                    sources: sources.clone(),
                    // A business date's closes (ruling 458), else the latest in
                    // force at the as-of.
                    at_ns: if business_date.is_empty() { as_of } else { 0 },
                    business_date: business_date.to_string(),
                    as_of_ns: as_of,
                    page_size: PAGE,
                    cursor: cursor.clone(),
                    ..Default::default()
                },
            )
            .map_err(|failed| format!("the lake's prices could not be read: {failed}"))?;
            let next = reply.next_cursor.clone();
            replies.push(reply);
            if next.is_empty() {
                break;
            }
            cursor = next;
        }
        Ok(replies)
    };
    let mut out = Vec::new();
    let mut silent: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for reply in read(subjects, sources)? {
        for missing in &reply.unanswered {
            let Some(subject) = &missing.subject else {
                continue;
            };
            if missing.reason == UnansweredReason::NotEntitled as i32 {
                for kind in kinds {
                    unentitled
                        .entry((subject.entity_id.clone(), kind_name(*kind)))
                        .or_insert(missing.dataset.clone());
                }
            }
            if missing.reason == UnansweredReason::SourceSilent as i32
                && !missing.dataset.is_empty()
            {
                silent
                    .entry(missing.dataset.clone())
                    .or_default()
                    .insert(subject.entity_id.clone());
            }
        }
        out.extend(reply.prices);
    }
    let (out, mut answered) = usable(out, quarantined);
    for (dataset, held) in silent {
        let wanted: Vec<String> = held
            .into_iter()
            .filter(|s| kinds.iter().any(|k| !answered.contains(&(s.clone(), *k))))
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let named = Some(v1::SourceChoice {
            named: vec![dataset.clone()],
            include_quarantined: true,
            ..Default::default()
        });
        for reply in read(&wanted, &named)? {
            for price in reply.prices {
                let Some(subject) = price
                    .meta
                    .as_ref()
                    .and_then(|m| m.subjects.first())
                    .map(|s| s.entity_id.clone())
                else {
                    continue;
                };
                if answered.contains(&(subject.clone(), price.kind)) || !quarantine(&price) {
                    continue;
                }
                quarantined
                    .entry((subject.clone(), kind_name(price.kind)))
                    .or_insert(dataset.clone());
                answered.insert((subject, price.kind));
            }
        }
    }
    Ok(out)
}

/// Whether a row's effective state is invalid (contract v28, W10.4).
fn quarantine(price: &v1::Price) -> bool {
    price
        .meta
        .as_ref()
        .is_some_and(|m| m.quality == v1::QualityState::Invalid as i32)
}

/// The rows a job may use -- every one not invalid -- and the subjects and
/// kinds they answer; an invalid one noted quarantined, naming its dataset.
fn usable(
    rows: Vec<v1::Price>,
    quarantined: &mut BTreeMap<(String, String), String>,
) -> (Vec<v1::Price>, BTreeSet<(String, i32)>) {
    let mut kept = Vec::new();
    let mut invalid = Vec::new();
    for price in rows {
        if quarantine(&price) {
            invalid.push(price);
        } else {
            kept.push(price);
        }
    }
    let answered: BTreeSet<(String, i32)> = kept
        .iter()
        .filter_map(|p| Some((p.meta.as_ref()?.subjects.first()?.entity_id.clone(), p.kind)))
        .collect();
    for price in invalid {
        let Some(meta) = price.meta.as_ref() else {
            continue;
        };
        let Some(subject) = meta.subjects.first() else {
            continue;
        };
        if answered.contains(&(subject.entity_id.clone(), price.kind)) {
            continue;
        }
        quarantined
            .entry((subject.entity_id.clone(), kind_name(price.kind)))
            .or_insert(
                meta.source
                    .as_ref()
                    .map(|s| s.dataset.clone())
                    .unwrap_or_default(),
            );
    }
    (kept, answered)
}

/// An input row's age at the `analytics` hop (W4.16; the latency spec's
/// "The hops"; ruling 606): now less its source time, recorded for the
/// component's next report to the monitor.
pub(crate) fn age_at_analytics(meta: &v1::ObservationMeta, now_ns: i64) {
    let dataset = meta
        .source
        .as_ref()
        .map(|s| s.dataset.as_str())
        .unwrap_or_default();
    meridian_domain::latency::recorder().record(
        dataset,
        v1::AgeHop::Analytics,
        now_ns - meridian_domain::latency::source_ns(meta),
    );
}

fn market_row(price: &v1::Price) -> Option<Value> {
    let meta = price.meta.as_ref()?;
    let subject = meta.subjects.first()?.entity_id.clone();
    let money = price.price.as_ref()?;
    let amount = text_of(money.amount.as_ref())?;
    // Contract v27 (ruling 460): its source time, the event time where the
    // source states one, else the earliest it states, else its valid time.
    let event = meta
        .source_times
        .iter()
        .find(|t| t.kind == v1::SourceTimeKind::Event as i32)
        .map(|t| t.at_ns);
    let earliest = meta.source_times.iter().map(|t| t.at_ns).min();
    let source_ns = event.or(earliest).unwrap_or(meta.valid_from_ns);
    Some(json!({
        "subject_id": subject,
        "kind": kind_name(price.kind),
        "amount": amount,
        "instrument_id": money.instrument_id,
        "dataset": meta.source.as_ref().map(|s| s.dataset.clone()).unwrap_or_default(),
        "valid_at_ns": meta.valid_from_ns.to_string(),
        "row_key": meta.row_key,
        "version": meta.version.to_string(),
        "source_ns": source_ns.to_string(),
    }))
}

/// The closes of each subject by business date over the window's reach,
/// and the dates, the union of those seen, the last `wanted` of them.
fn history(
    asker: &dyn Ask,
    subjects: &BTreeMap<String, i32>,
    sources: &Option<v1::SourceChoice>,
    as_of: i64,
    wanted: usize,
) -> Result<(Vec<String>, Vec<Value>), Refused> {
    let mut by: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    let mut by_kind: BTreeMap<i32, Vec<String>> = BTreeMap::new();
    for (subject, kind) in subjects {
        by_kind.entry(*kind).or_default().push(subject.clone());
    }
    for (kind, list) in by_kind {
        for chunk in list.chunks(SUBJECTS) {
            let mut cursor = String::new();
            loop {
                let reply: ListPricesReply = ask(
                    asker,
                    topics::LIST_PRICES,
                    "meridian.v1.ListPricesRequest",
                    &ListPricesRequest {
                        subjects: chunk
                            .iter()
                            .map(|s| SubjectRef {
                                entity_id: s.clone(),
                                ..Default::default()
                            })
                            .collect(),
                        kinds: vec![kind],
                        sources: sources.clone(),
                        valid_from_ns: as_of - HISTORY_REACH_NS,
                        valid_until_ns: as_of,
                        as_of_ns: as_of,
                        page_size: PAGE,
                        cursor: cursor.clone(),
                        ..Default::default()
                    },
                )
                .map_err(|failed| format!("the lake's history could not be read: {failed}"))?;
                for price in &reply.prices {
                    let Some(meta) = price.meta.as_ref() else {
                        continue;
                    };
                    let (Some(subject), Some(money)) =
                        (meta.subjects.first(), price.price.as_ref())
                    else {
                        continue;
                    };
                    if meta.business_date.is_empty() {
                        continue;
                    }
                    if let Some(amount) = text_of(money.amount.as_ref()) {
                        by.insert(
                            (subject.entity_id.clone(), meta.business_date.clone()),
                            (amount, money.instrument_id.clone()),
                        );
                    }
                }
                if reply.next_cursor.is_empty() {
                    break;
                }
                cursor = reply.next_cursor;
            }
        }
    }
    let all: BTreeSet<String> = by.keys().map(|(_, date)| date.clone()).collect();
    let dates: Vec<String> = all.into_iter().collect();
    let dates: Vec<String> = dates[dates.len().saturating_sub(wanted)..].to_vec();
    let first = dates.first().cloned().unwrap_or_default();
    let closes = by
        .into_iter()
        .filter(|((_, date), _)| *date >= first)
        .map(|((subject, date), (amount, instrument))| {
            json!({"subject_id": subject, "amount": amount, "instrument_id": instrument, "business_date": date})
        })
        .collect();
    Ok((dates, closes))
}

/// A position as the document's rows: one per sleeve its open lots are in,
/// each with its lots' cost in the base currency, or none where a lot
/// states no cost in it.
fn position_rows_of(
    account: &str,
    position: &v1::BookPosition,
    base: &str,
    base_code: &str,
) -> Vec<Value> {
    let short = position.side == v1::HoldingSide::Short as i32;
    let signed = |text: String| -> String {
        if short && !text.starts_with('-') && text.chars().any(|c| c.is_ascii_digit() && c != '0') {
            format!("-{text}")
        } else {
            text
        }
    };
    let quantity = position
        .trade_date_quantity
        .as_ref()
        .and_then(|d| Exact::from_wire(d).ok());
    let Some(quantity) = quantity else {
        return Vec::new();
    };
    let lots_total = position
        .lots
        .iter()
        .filter_map(|l| {
            l.open_quantity
                .as_ref()
                .and_then(|d| Exact::from_wire(d).ok())
        })
        .try_fold(Exact::ZERO, |sum, q| sum.checked_add(q).ok());
    let by_sleeve = !position.lots.is_empty() && lots_total == Some(quantity);
    let cost_in_base = |lot: &v1::Lot| -> Option<Exact> {
        let cost = lot.terms.as_ref()?.cost.as_ref()?;
        let in_base = cost.instrument_id == base
            || (cost.instrument_id.is_empty() && cost.currency_code == base_code);
        if !in_base {
            return None;
        }
        cost.amount.as_ref().and_then(|d| Exact::from_wire(d).ok())
    };
    if !by_sleeve {
        let cost = if position.lots.is_empty() {
            None
        } else {
            position
                .lots
                .iter()
                .map(cost_in_base)
                .try_fold(Exact::ZERO, |sum, c| {
                    c.and_then(|c| sum.checked_add(c).ok())
                })
        };
        return vec![json!({
            "account_id": account,
            "instrument_id": position.instrument_id,
            "quantity": signed(quantity.to_string()),
            "cost": cost.map(|c| signed(c.to_string())).unwrap_or_default(),
            "sleeve": "",
        })];
    }
    let mut sleeves: BTreeMap<String, (Exact, Option<Exact>)> = BTreeMap::new();
    for lot in &position.lots {
        let open = lot
            .open_quantity
            .as_ref()
            .and_then(|d| Exact::from_wire(d).ok())
            .unwrap_or(Exact::ZERO);
        let held = sleeves
            .entry(lot.sleeve.clone())
            .or_insert((Exact::ZERO, Some(Exact::ZERO)));
        held.0 = held.0.checked_add(open).unwrap_or(held.0);
        held.1 = match (held.1, cost_in_base(lot)) {
            (Some(sum), Some(cost)) => sum.checked_add(cost).ok(),
            _ => None,
        };
    }
    sleeves
        .into_iter()
        .map(|(sleeve, (quantity, cost))| {
            json!({
                "account_id": account,
                "instrument_id": position.instrument_id,
                "quantity": signed(quantity.to_string()),
                "cost": cost.map(|c| signed(c.to_string())).unwrap_or_default(),
                "sleeve": sleeve,
            })
        })
        .collect()
}

/// The job as the document holds it: the reference case's shape.
fn job_json(job: &Job, compiled: &Compiled) -> Value {
    let what_if = job.what_if.as_ref().map(|w| {
        json!({
            "account_id": w.account_id,
            "trades": w.trades.iter().map(|t| json!({
                "instrument_id": t.instrument_id,
                "side": v1::Side::try_from(t.side).map(|s| s.as_str_name()).unwrap_or_default(),
                "quantity": text_of(t.quantity.as_ref()).unwrap_or_default(),
                "price": t.price.as_ref().map(|p| json!({
                    "amount": text_of(p.amount.as_ref()).unwrap_or_default(),
                    "instrument_id": p.instrument_id,
                })),
            })).collect::<Vec<_>>(),
        })
    });
    let mut out = json!({
        "account_ids": job.account_ids,
        "measures": compiled.measures.iter().map(|m| m.to_json()).collect::<Vec<_>>(),
        "what_if": what_if,
    });
    // Contract v27 (rulings 458 and 459), written where the job names them.
    if !job.business_date.is_empty() {
        out["business_date"] = json!(job.business_date);
    }
    if !job.reporting_currency.is_empty() {
        out["reporting_currency"] = json!(job.reporting_currency);
    }
    out
}

/// Each relief of a lot the book closed, as pnl version 2 reads it (W9.10,
/// contract v27; rulings 320 and 396): the quantity relieved, what it was
/// sold for and its cost -- the lot's cost in the base for that share of it
/// -- each in the account's base; a relief with no proceeds is no sale and
/// is left out; a lot with no cost in the base answers none, which the
/// engine answers COST_MISSING (ruling 325).
fn closed_lots(
    positions: &[(String, v1::BookPosition)],
    codes: &BTreeMap<String, String>,
    base_codes: &[(String, String)],
) -> Vec<Value> {
    let mut out = Vec::new();
    for (account, position) in positions {
        let base_code = base_codes
            .iter()
            .find(|(a, _)| a == account)
            .map(|(_, c)| c.clone())
            .unwrap_or_default();
        let base = codes.get(&base_code).cloned().unwrap_or_default();
        let in_base = |money: &Money| {
            money.instrument_id == base
                || (money.instrument_id.is_empty() && money.currency_code == base_code)
        };
        for lot in &position.lots {
            let original = lot
                .original_quantity
                .as_ref()
                .and_then(|d| Exact::from_wire(d).ok());
            let cost = lot
                .terms
                .as_ref()
                .and_then(|t| t.cost.as_ref())
                .filter(|c| in_base(c))
                .and_then(|c| c.amount.as_ref())
                .and_then(|d| Exact::from_wire(d).ok());
            for relief in &lot.reliefs {
                let Some(proceeds) = relief.proceeds.as_ref().filter(|p| in_base(p)) else {
                    continue;
                };
                let (Some(quantity), Some(amount)) = (
                    relief
                        .quantity
                        .as_ref()
                        .and_then(|d| Exact::from_wire(d).ok()),
                    proceeds
                        .amount
                        .as_ref()
                        .and_then(|d| Exact::from_wire(d).ok()),
                ) else {
                    continue;
                };
                // The lot's cost for the quantity relieved, exact where it
                // divides, else rounded at 18 places.
                let share = match (cost, original) {
                    (Some(cost), Some(original)) if !original.is_zero() => {
                        cost_share(cost, quantity, original)
                    }
                    _ => None,
                };
                out.push(json!({
                    "account_id": account,
                    "instrument_id": position.instrument_id,
                    "quantity": quantity.to_string(),
                    "proceeds": amount.to_string(),
                    "cost": share.map(|c| c.to_string()).unwrap_or_default(),
                    "sleeve": lot.sleeve,
                }));
            }
        }
    }
    out.sort_by(|a, b| {
        (a["account_id"].as_str(), a["instrument_id"].as_str())
            .cmp(&(b["account_id"].as_str(), b["instrument_id"].as_str()))
    });
    out
}

/// cost × quantity ÷ original, as text the engine reads exactly: by the
/// engine's own exact arithmetic, rounded half to even to 18 places.
fn cost_share(cost: Exact, quantity: Exact, original: Exact) -> Option<String> {
    use meridian_analytics_engine::exact::Dec;
    let read = |e: Exact| Dec::parse(&e.to_string()).ok();
    let (cost, quantity, original) = (read(cost)?, read(quantity)?, read(original)?);
    if original.is_zero() {
        return None;
    }
    let share = cost
        .mul(&quantity)
        .to_ratio()
        .div(&original.to_ratio())
        .round_half_even(18);
    Some(share.text())
}
