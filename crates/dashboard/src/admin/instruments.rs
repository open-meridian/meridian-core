//! The deployment's instrument records, completed by the deployment admin
//! (W3.10 to W3.13, contract v10; ruled 2026-10-02: core owns completing
//! instrument records, on the dashboard, done by the deployment admin).
//!
//! The Instruments page lists the records the book cannot use first -- no
//! asset class or no currency -- each with what it lacks, its identifiers and
//! who reported them, and the values offered for it; the conflicts, for a
//! merge; and how many licensed identifiers the deployment holds, per scheme,
//! against CUSIP Global Services' 500-identifier threshold. A record's own
//! page completes it, value by value, each with its source, asks the platform
//! about it when a person chooses to, and reads its history.
//!
//! Every change goes to the instrument store on the signed-in person's
//! behalf (`Bus::call_for`), so the store records who made it; the page never
//! types a person. It shows records only: no account, no quantity, which the
//! deployment admin does not reach (spec/deployment-dashboard-and-access.md).

use std::time::Duration;

use meridian_domain::instrument_type;
use meridian_domain::v1::{
    instrument_value, AskPlatformForInstrumentReply, AskPlatformForInstrumentRequest, AssetClass,
    CompleteInstrumentsReply, CompleteInstrumentsRequest, Identifier, InstrumentCompletion,
    InstrumentField, InstrumentRecord, InstrumentToComplete, InstrumentType, InstrumentValue,
    InstrumentVersion, LiquidityFeeRegime, ListInstrumentsToCompleteReply,
    ListInstrumentsToCompleteRequest, MergeInstrumentsReply, MergeInstrumentsRequest, MissReason,
    MoneyMarketFund, MoneyMarketFundCategory, MoneyMarketFundInvestors, MoneyMarketFundNav,
    OfferedValue, ReadInstrumentHistoryReply, ReadInstrumentHistoryRequest,
};
use prost::Message;

use crate::html::escape;

pub const COMPLETE_INSTRUMENTS: &str = "platform.reference.command.complete-instruments";
pub const MERGE_INSTRUMENTS: &str = "platform.reference.command.merge-instruments";
pub const LIST_INSTRUMENTS_TO_COMPLETE: &str =
    "platform.reference.query.list-instruments-to-complete";
pub const READ_INSTRUMENT_HISTORY: &str = "platform.reference.query.read-instrument-history";
/// W3.28 (contract v24): the usage report's setting, and the reports sent.
pub const SET_REPORTS_USAGE: &str = "platform.config.command.set-reports-usage";
/// W3.28 (contract v25): the identifier schemes declared as licensed.
pub const SET_LICENSED_SCHEMES: &str = "platform.config.command.set-licensed-schemes";
pub const LIST_USAGE_REPORTS: &str = "platform.config.query.list-usage-reports";
/// W3.26 (contract v24): the deployment admin making the migration's lines.
pub const APPLY_INSTRUMENT_MIGRATION: &str =
    "platform.reference.command.apply-instrument-migration";
pub const ASK_PLATFORM_FOR_INSTRUMENT: &str =
    "platform.reference.query.ask-platform-for-instrument";

/// How long a read waits for the instrument store: one page's worth, and the
/// rest of the dashboard is drawn without it.
const WAIT: Duration = Duration::from_secs(3);

/// How long an ask waits: the conductor's own bound is shorter, so the
/// person hears its answer, unreachable included.
const ASKING: Duration = Duration::from_secs(12);

/// CUSIP Global Services' threshold (ruled 2026-10-02): the count of CUSIPs a
/// deployment holds beyond which its licence terms change.
pub const CUSIP_THRESHOLD: i64 = 500;

/// The asset classes, as the form offers them (W1's closed list).
const CLASSES: [(AssetClass, &str); 7] = [
    (AssetClass::Equity, "Equity"),
    (AssetClass::Debt, "Debt"),
    (AssetClass::Fund, "Fund"),
    (AssetClass::Derivative, "Derivative"),
    (AssetClass::CryptoAsset, "Crypto asset"),
    (AssetClass::EventContract, "Event contract"),
    (AssetClass::Cash, "Cash"),
];

/// The instrument types, as the form offers them (contract v11; the binary
/// event contract from contract v19), each under its asset class: the form
/// offers a type only while its class is chosen.
const TYPES: [(InstrumentType, AssetClass, &str); 2] = [
    (
        InstrumentType::MoneyMarketFund,
        AssetClass::Fund,
        "Money market fund",
    ),
    (
        InstrumentType::BinaryEventContract,
        AssetClass::EventContract,
        "Binary event contract",
    ),
];

/// A moment as a `datetime-local` field holds it, in UTC: `2026-11-03T00:00`.
fn utc_field(ns: i64) -> String {
    if ns <= 0 {
        return String::new();
    }
    let date = meridian_domain::date::Date::of_utc_instant(ns);
    let in_day = ns.rem_euclid(86_400 * 1_000_000_000) / 1_000_000_000;
    format!("{date}T{:02}:{:02}", in_day / 3600, (in_day % 3600) / 60)
}

/// What a `datetime-local` field said, read as UTC; empty is 0, none stated.
fn utc_of_field(text: &str) -> Result<i64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    let bad = || format!("{text:?} is not a date and time (2026-11-03T00:00)");
    let (date, time) = text.split_once('T').ok_or_else(bad)?;
    let date = meridian_domain::date::Date::parse(date).map_err(|_| bad())?;
    let mut parts = time.split(':');
    let hour: i64 = parts.next().and_then(|h| h.parse().ok()).ok_or_else(bad)?;
    let minute: i64 = parts.next().and_then(|m| m.parse().ok()).ok_or_else(bad)?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return Err(bad());
    }
    Ok((date.days_since_epoch() * 86_400 + hour * 3_600 + minute * 60) * 1_000_000_000)
}

fn type_words(value: i32) -> &'static str {
    instrument_type::words(InstrumentType::try_from(value).unwrap_or(InstrumentType::Unspecified))
}

fn class_name(value: i32) -> &'static str {
    CLASSES
        .iter()
        .find(|(class, _)| *class as i32 == value)
        .map(|(_, name)| *name)
        .unwrap_or("")
}

async fn ask<R: Message + Default>(
    bus: &meridian_bus::Bus,
    topic: &str,
    request_type: &str,
    request: impl Message,
    person: Option<&str>,
    wait: Duration,
) -> Result<R, String> {
    let asked = match person {
        Some(subject) => {
            bus.call_for(
                topic,
                request_type,
                request.encode_to_vec(),
                None,
                Some(wait),
                subject,
            )
            .await
        }
        None => {
            bus.call(
                topic,
                request_type,
                request.encode_to_vec(),
                None,
                Some(wait),
            )
            .await
        }
    };
    let (_, bytes) = asked.map_err(|failed| match failed {
        meridian_bus::BusError::HandlerFailed { detail, .. } => {
            match meridian_bus::read_refusal(&detail) {
                Some((_, words)) => words.to_string(),
                None => detail,
            }
        }
        other => other.to_string(),
    })?;
    R::decode(&bytes[..]).map_err(|failed| format!("the answer did not read: {failed}"))
}

/// The records a picker's words find (W3.11, contract v20): by an
/// identifier, a symbol, the description or the ID, complete ones included,
/// at most `limit`.
pub async fn search(
    bus: &meridian_bus::Bus,
    words: &str,
    limit: usize,
) -> Result<ListInstrumentsToCompleteReply, String> {
    ask(
        bus,
        LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            include_complete: true,
            instrument_id: String::new(),
            page_size: limit.clamp(1, 50) as i32,
            cursor: String::new(),
            search: words.chars().take(100).collect(),
            provisional: false,
        },
        None,
        WAIT,
    )
    .await
}

/// A found record as the kit's instrument picker reads one (meridian-ui
/// 0.12.0, om-instrument-picker): the record's fields by their proto names.
pub fn found_json(record: &meridian_domain::v1::InstrumentRecord) -> serde_json::Value {
    let class = meridian_domain::v1::AssetClass::try_from(record.asset_class)
        .map(|class| {
            class
                .as_str_name()
                .trim_start_matches("ASSET_CLASS_")
                .to_lowercase()
        })
        .unwrap_or_default();
    serde_json::json!({
        "instrument_id": record.instrument_id,
        "description": record.description,
        "asset_class": if class == "unspecified" { String::new() } else { class },
        "currency": record.currency,
        "exchange_mic": record.exchange_mic,
        "identifiers": record.identifiers.iter().map(|identifier| serde_json::json!({
            "scheme": identifier.scheme,
            "value": identifier.value,
            "source": identifier.source,
        })).collect::<Vec<_>>(),
    })
}

/// W3.11: the records to complete, or one record (`instrument_id`).
pub async fn list(
    bus: &meridian_bus::Bus,
    instrument_id: &str,
) -> Result<ListInstrumentsToCompleteReply, String> {
    ask(
        bus,
        LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            include_complete: !instrument_id.is_empty(),
            instrument_id: instrument_id.to_string(),
            page_size: 1000,
            cursor: String::new(),
            search: String::new(),
            provisional: false,
        },
        None,
        WAIT,
    )
    .await
}

/// The provisional records (contract v24, W3.11): each with why, complete or
/// not.
pub async fn provisional(
    bus: &meridian_bus::Bus,
) -> Result<ListInstrumentsToCompleteReply, String> {
    ask(
        bus,
        LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            include_complete: true,
            page_size: 1000,
            provisional: true,
            ..Default::default()
        },
        None,
        WAIT,
    )
    .await
}

/// The usage reports the conductor sent (W3.28), newest first.
pub async fn usage_reports(
    bus: &meridian_bus::Bus,
) -> Result<meridian_domain::v1::ListUsageReportsReply, String> {
    ask(
        bus,
        LIST_USAGE_REPORTS,
        "meridian.v1.ListUsageReportsRequest",
        meridian_domain::v1::ListUsageReportsRequest {},
        None,
        WAIT,
    )
    .await
}

/// The migration's counts, as the tools answer them (W3.11, W3.26).
pub fn migration_counts_json(m: &meridian_domain::v1::MigrationCounts) -> serde_json::Value {
    serde_json::json!({
        "already_held": m.already_held,
        "matched": m.matched,
        "minted": m.minted,
        "near_match": m.near_match,
        "awaiting_staff": m.awaiting_staff,
        "kept_local": m.kept_local,
        "unreachable": m.unreachable,
        "held_back": m.held_back,
        "remaining": m.remaining,
    })
}

/// Why a record is provisional, in the page's words (W3.7, W3.11).
pub fn reason_words(reason: i32) -> &'static str {
    use meridian_domain::v1::ProvisionalReason;
    match ProvisionalReason::try_from(reason) {
        Ok(ProvisionalReason::Unreachable) => {
            "the platform not reached; asked again on reconnecting"
        }
        // INCOMPLETE is the incomplete or offline state: a record kept
        // local with nothing on the licence list to look for among them.
        Ok(ProvisionalReason::Incomplete) => {
            "incomplete or offline: its template's fields, or nothing to look for yet"
        }
        Ok(ProvisionalReason::AwaitingStaff) => "with the platform's staff; asked again daily",
        Ok(ProvisionalReason::ClassNeeded) => "class needed: no asset class stated",
        Ok(ProvisionalReason::HeldBack) => "held back at the migration",
        _ => "not said",
    }
}

/// When a record was first recorded, from its earliest source: its age.
pub fn first_recorded_ns(record: &InstrumentRecord) -> i64 {
    record
        .sources
        .iter()
        .map(|source| source.recorded_at_ns)
        .filter(|at| *at > 0)
        .min()
        .unwrap_or(record.record_time_ns)
}

/// An age in a person's words: "3 days", "5 hours", "under an hour".
pub fn age_words(age_ns: i64) -> String {
    let hours = age_ns.max(0) / 3_600_000_000_000;
    match hours {
        0 => "under an hour".into(),
        1..=47 => format!("{hours} hour{}", if hours == 1 { "" } else { "s" }),
        _ => format!("{} days", hours / 24),
    }
}

/// The home page's line for the provisional records (contract v24, W3.11):
/// how many, and the oldest's age. Nothing when none.
pub fn home_provisional(listed: Option<&ListInstrumentsToCompleteReply>, now_ns: i64) -> String {
    let Some(listed) = listed else {
        return String::new();
    };
    let records: Vec<&InstrumentRecord> = listed
        .instruments
        .iter()
        .filter_map(|item| item.instrument.as_ref())
        .filter(|record| record.provisional)
        .collect();
    if records.is_empty() {
        return String::new();
    }
    let oldest = records
        .iter()
        .map(|record| first_recorded_ns(record))
        .min()
        .unwrap_or(now_ns);
    let n = records.len();
    format!(
        "<p class=\"notice\" id=\"provisional-records\" data-provisional=\"{n}\">{n} provisional \
         instrument record{s}, the oldest {age}: local records, not priced, until the platform \
         answers. <a href=\"/admin/instruments#provisional\">See why</a></p>",
        s = if n == 1 { "" } else { "s" },
        age = age_words(now_ns - oldest),
    )
}

/// Whether the deployment holds a record by this ID, asked by the ID rather
/// than read from a list, which is capped (W6.11, a table's instrument cell).
pub async fn record_held(bus: &meridian_bus::Bus, instrument_id: &str) -> Result<bool, String> {
    let reply = list(bus, instrument_id).await?;
    Ok(reply
        .instruments
        .iter()
        .filter_map(|each| each.instrument.as_ref())
        .any(|record| record.instrument_id == instrument_id))
}

/// Every record the deployment holds, complete or not, each as its ID and
/// what a person reads -- its symbol or first identifier, and its
/// description -- for a table setting's instrument column (W6.11, contract
/// v14), sorted by what is read. At most the first thousand.
pub async fn every_record(bus: &meridian_bus::Bus) -> Result<Vec<(String, String)>, String> {
    let reply: ListInstrumentsToCompleteReply = ask(
        bus,
        LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            include_complete: true,
            instrument_id: String::new(),
            page_size: 1000,
            cursor: String::new(),
            search: String::new(),
            provisional: false,
        },
        None,
        WAIT,
    )
    .await?;
    let mut found: Vec<(String, String)> = reply
        .instruments
        .into_iter()
        .filter_map(|each| each.instrument)
        .map(|record| {
            let named = record
                .identifiers
                .iter()
                .find(|i| i.scheme == "symbol")
                .or_else(|| record.identifiers.first())
                .map(|i| i.value.clone())
                .unwrap_or_default();
            let shown = match (named.is_empty(), record.description.is_empty()) {
                (false, false) => format!(
                    "{named} · {} ({})",
                    record.description, record.instrument_id
                ),
                (false, true) => format!("{named} ({})", record.instrument_id),
                (true, false) => format!("{} ({})", record.description, record.instrument_id),
                (true, true) => record.instrument_id.clone(),
            };
            (record.instrument_id, shown)
        })
        .collect();
    found.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(found)
}

/// W3.12.
pub async fn history(
    bus: &meridian_bus::Bus,
    instrument_id: &str,
) -> Result<ReadInstrumentHistoryReply, String> {
    ask(
        bus,
        READ_INSTRUMENT_HISTORY,
        "meridian.v1.ReadInstrumentHistoryRequest",
        ReadInstrumentHistoryRequest {
            instrument_id: instrument_id.to_string(),
            page_size: 200,
            cursor: String::new(),
        },
        None,
        WAIT,
    )
    .await
}

/// W3.10, for the person signed in.
pub async fn complete(
    bus: &meridian_bus::Bus,
    subject: &str,
    request: CompleteInstrumentsRequest,
) -> Result<CompleteInstrumentsReply, String> {
    ask(
        bus,
        COMPLETE_INSTRUMENTS,
        "meridian.v1.CompleteInstrumentsRequest",
        request,
        Some(subject),
        Duration::from_secs(20),
    )
    .await
}

/// W3.13, for the person signed in.
pub async fn merge(
    bus: &meridian_bus::Bus,
    subject: &str,
    request: MergeInstrumentsRequest,
) -> Result<MergeInstrumentsReply, String> {
    ask(
        bus,
        MERGE_INSTRUMENTS,
        "meridian.v1.MergeInstrumentsRequest",
        request,
        Some(subject),
        Duration::from_secs(20),
    )
    .await
}

/// W3.3: ask the platform about a record, through the conductor.
pub async fn ask_platform(
    bus: &meridian_bus::Bus,
    record: &InstrumentRecord,
) -> Result<AskPlatformForInstrumentReply, String> {
    ask(
        bus,
        ASK_PLATFORM_FOR_INSTRUMENT,
        "meridian.v1.AskPlatformForInstrumentRequest",
        AskPlatformForInstrumentRequest {
            instrument_id: record.instrument_id.clone(),
            identifiers: record.identifiers.clone(),
            as_of_ns: 0,
            match_or_mint: None,
        },
        None,
        ASKING,
    )
    .await
}

/// The values a completion form sends: each field the person filled, with
/// the source they gave it. A field left empty is left alone; one equal to
/// what is in force changes nothing at the store.
#[derive(Debug, Default, Clone)]
pub struct Filled {
    pub instrument_id: String,
    pub against_version: String,
    pub asset_class: String,
    pub asset_class_source: String,
    pub currency: String,
    pub currency_source: String,
    pub description: String,
    pub description_source: String,
    /// Contract v11: the type within the asset class, its number as the
    /// wire's; and a money market fund's four attributes, each its number,
    /// stated together, with their source.
    pub instrument_type: String,
    pub instrument_type_source: String,
    pub fund_category: String,
    pub fund_investors: String,
    pub fund_nav: String,
    pub fund_liquidity_fee: String,
    pub fund_source: String,
    /// Contract v19: a binary event contract's payout -- its amount, and its
    /// asset by ISO 4217 code or cash instrument -- when trading closes (a
    /// `datetime-local`, UTC), and their source.
    pub contract_amount: String,
    pub contract_asset: String,
    pub contract_closes: String,
    pub contract_source: String,
    pub identifier_scheme: String,
    pub identifier_value: String,
    pub identifier_namespace: String,
    pub identifier_source: String,
    pub note: String,
    /// What the record held when the page was drawn, as the form carried it
    /// back: a value posted as it was held is no change, and is not sent
    /// again, so a held value whose source the page did not show is never
    /// resubmitted without one (product owner testing, 2026-10-03).
    pub asset_class_held: String,
    pub currency_held: String,
    pub description_held: String,
    pub instrument_type_held: String,
    pub fund_held: String,
    pub contract_held: String,
    /// The platform's ends accepted (contract v19, W3.5): each an ended
    /// identifier as the page offered it, as JSON.
    pub ends: Vec<String>,
}

/// One record's completion from its form, or why the form cannot be sent.
pub fn completion(filled: &Filled) -> Result<CompleteInstrumentsRequest, String> {
    if filled.instrument_id.is_empty() {
        return Err("the form names no record".into());
    }
    let against_version: i64 = filled
        .against_version
        .parse()
        .map_err(|_| "the form names no version of the record".to_string())?;
    let mut values = Vec::new();
    if !filled.asset_class.is_empty() && filled.asset_class != filled.asset_class_held {
        let class: i32 = filled
            .asset_class
            .parse()
            .map_err(|_| format!("{:?} is no asset class", filled.asset_class))?;
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::AssetClass(class)),
            source: filled.asset_class_source.trim().to_string(),
        });
    }
    if !filled.currency.trim().is_empty()
        && !filled
            .currency
            .trim()
            .eq_ignore_ascii_case(filled.currency_held.trim())
    {
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::Currency(
                filled.currency.trim().to_ascii_uppercase(),
            )),
            source: filled.currency_source.trim().to_string(),
        });
    }
    if !filled.description.trim().is_empty()
        && filled.description.trim() != filled.description_held.trim()
    {
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::Description(
                filled.description.trim().to_string(),
            )),
            source: filled.description_source.trim().to_string(),
        });
    }
    // A field gated on a choice is read only with its choice (kit 0.11.0's
    // gate hides and disables it otherwise; without script it posts): a type
    // under the class chosen, a fund's attributes for a money market fund, a
    // contract's for a binary event contract.
    let class_chosen = filled.asset_class.parse::<i32>().ok();
    let type_chosen = filled.instrument_type.parse::<i32>().ok();
    let type_fits = TYPES.iter().any(|(kind, class, _)| {
        Some(*kind as i32) == type_chosen && Some(*class as i32) == class_chosen
    });
    if !filled.instrument_type.is_empty()
        && type_fits
        && filled.instrument_type != filled.instrument_type_held
    {
        let kind: i32 = filled
            .instrument_type
            .parse()
            .map_err(|_| format!("{:?} is no instrument type", filled.instrument_type))?;
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::InstrumentType(kind)),
            source: filled.instrument_type_source.trim().to_string(),
        });
    }
    let attributes = [
        &filled.fund_category,
        &filled.fund_investors,
        &filled.fund_nav,
        &filled.fund_liquidity_fee,
    ];
    let fund_chosen = type_fits && type_chosen == Some(InstrumentType::MoneyMarketFund as i32);
    if fund_chosen && attributes.iter().any(|attribute| !attribute.is_empty()) {
        let number = |text: &str| text.parse::<i32>().unwrap_or(0);
        let fund = MoneyMarketFund {
            category: number(&filled.fund_category),
            investors: number(&filled.fund_investors),
            nav: number(&filled.fund_nav),
            liquidity_fee: number(&filled.fund_liquidity_fee),
        };
        if instrument_type::fund_to_text(&fund) != filled.fund_held {
            if !instrument_type::unstated(&fund).is_empty() {
                return Err(
                    "a money market fund's four attributes are stated together: its category, \
                     investors, net asset value and liquidity fee"
                        .into(),
                );
            }
            if filled.fund_source.trim().is_empty() {
                return Err("say where the money market fund's attributes came from".into());
            }
            values.push(InstrumentValue {
                value: Some(instrument_value::Value::MoneyMarketFund(fund)),
                source: filled.fund_source.trim().to_string(),
            });
        }
    }
    let contract_chosen =
        type_fits && type_chosen == Some(InstrumentType::BinaryEventContract as i32);
    let contract_said = [
        &filled.contract_amount,
        &filled.contract_asset,
        &filled.contract_closes,
    ];
    if contract_chosen && contract_said.iter().any(|said| !said.trim().is_empty()) {
        let amount = filled.contract_amount.trim();
        let amount = amount
            .parse::<meridian_domain::exact::Exact>()
            .map_err(|_| format!("{amount:?} is not an amount the contract pays"))?;
        let asset = filled.contract_asset.trim();
        let (currency_code, instrument_id) = if meridian_domain::money::is_iso4217(asset) {
            (asset.to_string(), String::new())
        } else {
            (String::new(), asset.to_string())
        };
        let contract = meridian_domain::v1::BinaryEventContract {
            payout: Some(meridian_domain::v1::Money {
                amount: Some(amount.to_wire()),
                currency_code,
                instrument_id,
            }),
            closes_at_ns: utc_of_field(&filled.contract_closes)?,
        };
        if instrument_type::contract_to_text(&contract) != filled.contract_held {
            if filled.contract_source.trim().is_empty() {
                return Err("say where the binary event contract's terms came from".into());
            }
            values.push(InstrumentValue {
                value: Some(instrument_value::Value::BinaryEventContract(contract)),
                source: filled.contract_source.trim().to_string(),
            });
        }
    }
    // The platform's end of an identifier, accepted: the window closes on
    // the date it said (contract v19, W3.5), with the offer's source.
    for end in &filled.ends {
        let said: serde_json::Value = serde_json::from_str(end)
            .map_err(|_| "an end offered could not be read; draw the page again".to_string())?;
        let text = |name: &str| said[name].as_str().unwrap_or_default().to_string();
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::Identifier(Identifier {
                scheme: text("scheme"),
                value: text("value"),
                source: text("namespace"),
                valid_from_ns: 0,
                valid_until_ns: said["until"].as_i64().unwrap_or(0),
                trading_currency: String::new(),
            })),
            source: text("source"),
        });
    }
    if !filled.identifier_scheme.trim().is_empty() || !filled.identifier_value.trim().is_empty() {
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::Identifier(Identifier {
                scheme: filled.identifier_scheme.trim().to_ascii_lowercase(),
                value: filled.identifier_value.trim().to_string(),
                source: filled.identifier_namespace.trim().to_string(),
                ..Default::default()
            })),
            source: filled.identifier_source.trim().to_string(),
        });
    }
    if values.is_empty() {
        return Err("nothing was filled in".into());
    }
    Ok(CompleteInstrumentsRequest {
        completions: vec![InstrumentCompletion {
            instrument_id: filled.instrument_id.clone(),
            against_version,
            values,
            note: filled.note.trim().to_string(),
        }],
    })
}

/// "Accept offered values" for the records chosen: for each, every field the
/// book or the list lacks that something offers, the first offer for it, with
/// that offer's source (W3.10, the spec's requirement 8). One command.
pub fn accepting(listed: &[InstrumentToComplete], chosen: &[String]) -> CompleteInstrumentsRequest {
    let mut completions = Vec::new();
    for item in listed {
        let Some(record) = item.instrument.as_ref() else {
            continue;
        };
        if !chosen.contains(&record.instrument_id) {
            continue;
        }
        let mut values = Vec::new();
        for lacking in &item.lacks {
            if let Some(offer) = record
                .offers
                .iter()
                .filter_map(|offered| offered.value.as_ref())
                .find(|value| field_of(value) == *lacking)
            {
                values.push(offer.clone());
            }
        }
        if !values.is_empty() {
            completions.push(InstrumentCompletion {
                instrument_id: record.instrument_id.clone(),
                against_version: record.version,
                values,
                note: String::new(),
            });
        }
    }
    CompleteInstrumentsRequest { completions }
}

fn field_of(value: &InstrumentValue) -> i32 {
    (match value.value {
        Some(instrument_value::Value::AssetClass(_)) => InstrumentField::AssetClass,
        Some(instrument_value::Value::Currency(_)) => InstrumentField::Currency,
        Some(instrument_value::Value::Description(_)) => InstrumentField::Description,
        Some(instrument_value::Value::Identifier(_)) => InstrumentField::Identifier,
        Some(instrument_value::Value::InstrumentType(_)) => InstrumentField::InstrumentType,
        Some(instrument_value::Value::MoneyMarketFund(_)) => InstrumentField::MoneyMarketFund,
        Some(instrument_value::Value::BinaryEventContract(_)) => {
            InstrumentField::BinaryEventContract
        }
        None => InstrumentField::Unspecified,
    }) as i32
}

fn value_words(value: &InstrumentValue) -> String {
    match &value.value {
        Some(instrument_value::Value::AssetClass(class)) => class_name(*class).to_string(),
        Some(instrument_value::Value::Currency(code)) => code.clone(),
        Some(instrument_value::Value::Description(text)) => text.clone(),
        Some(instrument_value::Value::Identifier(id)) => identifier_words(id),
        Some(instrument_value::Value::InstrumentType(kind)) => type_words(*kind).to_string(),
        Some(instrument_value::Value::MoneyMarketFund(fund)) => {
            instrument_type::fund_words(&instrument_type::fund_to_text(fund))
        }
        Some(instrument_value::Value::BinaryEventContract(contract)) => {
            instrument_type::contract_words(&instrument_type::contract_to_text(contract))
        }
        None => String::new(),
    }
}

fn identifier_words(id: &Identifier) -> String {
    if id.source.is_empty() {
        format!("{}: {}", id.scheme, id.value)
    } else {
        format!("{} ({}): {}", id.scheme, id.source, id.value)
    }
}

fn lacks_words(lacks: &[i32]) -> String {
    lacks
        .iter()
        .filter_map(|field| match InstrumentField::try_from(*field).ok()? {
            InstrumentField::AssetClass => Some("asset class"),
            InstrumentField::Currency => Some("currency"),
            InstrumentField::Description => Some("description"),
            InstrumentField::MoneyMarketFund => Some("the money market fund's attributes"),
            InstrumentField::BinaryEventContract => {
                Some("the binary event contract's payout and close")
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// How many records the book cannot use, for the dashboard's home: a
/// first-class number (spec/security-master-and-holdings.md). `None` when the
/// instrument store did not answer.
pub async fn incomplete_for_book(bus: &meridian_bus::Bus) -> Option<i64> {
    ask::<ListInstrumentsToCompleteReply>(
        bus,
        LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            page_size: 1,
            ..Default::default()
        },
        None,
        Duration::from_secs(2),
    )
    .await
    .ok()
    .map(|listed| listed.incomplete_for_book)
}

/// The home page's line for a deployment admin, when records wait.
pub fn home_notice(count: Option<i64>) -> String {
    match count {
        Some(0) | None => String::new(),
        Some(n) => format!(
            "<p class=\"notice warn\" id=\"instruments-waiting\">{n} instrument record{s} the book \
             cannot use yet: {lack}. <a href=\"/admin/instruments\">Complete them</a></p>",
            s = if n == 1 { "" } else { "s" },
            lack = "an asset class or a currency is missing",
        ),
    }
}

/// The Instruments page (W3.11).
pub fn list_page(
    listed: &Result<ListInstrumentsToCompleteReply, String>,
    token: &str,
    notice: &str,
) -> String {
    list_page_with(listed, token, notice, "")
}

/// The Instruments page with its Calendars tab (W3.21, contract v22), drawn
/// by [`super::calendars::tab`].
pub fn list_page_with(
    listed: &Result<ListInstrumentsToCompleteReply, String>,
    token: &str,
    notice: &str,
    calendars: &str,
) -> String {
    list_page_full(listed, None, None, token, notice, calendars, 0)
}

/// The provisional tab (contract v24, W3.11): the migration's and each
/// backfill's counts, and each provisional record with why, its template
/// and what of it it lacks.
fn provisional_tab(
    listed: &ListInstrumentsToCompleteReply,
    token: &str,
    now_ns: i64,
) -> (usize, String) {
    let counts = listed.migration_counts.as_ref().map(|m| {
        format!(
            "<ul class=\"counts tight\" id=\"migration-counts\"><li>Already held: <strong>{}</strong></li>\
             <li>Matched: <strong>{}</strong></li><li>Minted: <strong>{}</strong></li>\
             <li>Near match: <strong>{}</strong></li><li>With the platform's staff: <strong>{}</strong></li>\
             <li>Kept local: <strong>{}</strong></li><li>Not reached: <strong>{}</strong></li>\
             <li>Held back: <strong>{}</strong></li><li>Still to send: <strong>{}</strong></li></ul>",
            m.already_held,
            m.matched,
            m.minted,
            m.near_match,
            m.awaiting_staff,
            m.kept_local,
            m.unreachable,
            m.held_back,
            m.remaining
        )
    });
    let backfill: String = listed
        .backfill_counts
        .iter()
        .map(|b| {
            format!(
                "<li>{}: <strong>{}</strong> linked, <strong>{}</strong> for a person, \
                 <strong>{}</strong> still to send</li>",
                escape(&b.scheme.to_ascii_uppercase()),
                b.matched,
                b.awaiting_person,
                b.remaining
            )
        })
        .collect();
    let rows: Vec<String> = listed
        .instruments
        .iter()
        .filter_map(|item| item.instrument.as_ref().map(|record| (item, record)))
        .filter(|(_, record)| record.provisional)
        .map(|(item, record)| {
            let template = if record.template_id.is_empty() {
                "none yet".to_string()
            } else {
                format!("{} v{}", record.template_id, record.template_version)
            };
            let missing = record.missing_fields.join(", ");
            format!(
                "<tr data-id=\"{id}\" data-reason=\"{reason_n}\"><td><a href=\"/admin/instruments/{id}\">{id}</a></td>\
                 <td class=\"wide\" title=\"{description}\">{description}</td><td title=\"{why}\">{why}</td>\
                 <td class=\"wide\">{template}</td><td class=\"wide\" title=\"{missing}\">{missing}</td>\
                 <td>{age}</td></tr>",
                id = escape(&record.instrument_id),
                description = escape(&record.description),
                reason_n = item.provisional_reason,
                why = escape(reason_words(item.provisional_reason)),
                template = escape(&template),
                missing = escape(if missing.is_empty() { "—" } else { &missing }),
                age = escape(&age_words(now_ns - first_recorded_ns(record))),
            )
        })
        .collect();
    let n = rows.len();
    let table = if rows.is_empty() {
        "<p class=\"empty\" data-provisional=\"0\">None: every record is the platform's.</p>"
            .to_string()
    } else {
        format!(
            "<om-pager search=\"Search provisional records\"><table class=\"list one-line provisional\" \
             id=\"provisional-table\" data-provisional=\"{n}\"><thead><tr><th>Record</th>\
             <th class=\"wide\">Description</th><th>Why</th><th class=\"wide\">Template</th>\
             <th class=\"wide\">Missing</th><th>Age</th></tr></thead><tbody>{}</tbody></table></om-pager>",
            rows.concat()
        )
    };
    let (line_buttons, line_dialogs) = lines_controls(listed, token);
    let body = format!(
        "<section class=\"admin-section\" id=\"provisional\"><div class=\"section-head\"><div>\
         <h2>Provisional records</h2><p class=\"hint\">Local records, not priced, until the \
         platform answers with its record; each resolves on its own, its history reading across \
         the link.</p></div>{line_buttons}</div>{counts}{backfill}{table}{line_dialogs}</section>",
        counts = counts
            .map(|counts| format!("<h3>The upgrade's migration</h3>{counts}"))
            .unwrap_or_default(),
        backfill = if backfill.is_empty() {
            String::new()
        } else {
            format!("<h3>Backfills</h3><ul class=\"counts tight\" id=\"backfill-counts\">{backfill}</ul>")
        },
    );
    (n, body)
}

/// The most lines one change names (W3.26): more exact lines are made as
/// successive changes, in order (approved 2026-10-15).
pub const MOST_LINES: usize = 500;

/// Where the migration's lines are made or prepared.
pub const LINES_PATH: &str = "/admin/instruments/migration";

/// One uncertain line (W3.26): its records, the record the platform
/// answered, and why it is not exact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncertainLine {
    pub instrument_ids: Vec<String>,
    pub answered: String,
    pub why: String,
}

/// The migration's lines still to make, from the provisional records (W3.26;
/// the sections and answers approved 2026-10-15): the exact lines in order,
/// and each uncertain line on its own -- two records answered by one INS- ID
/// (a duplicate line), a record an identifier meeting another names, or a
/// line held back, made later on its own.
pub fn migration_lines(
    listed: &ListInstrumentsToCompleteReply,
) -> (Vec<String>, Vec<UncertainLine>) {
    use meridian_domain::v1::MigrationSection as S;
    use std::collections::{BTreeMap, BTreeSet};
    let conflicted: BTreeSet<&str> = listed
        .conflicts
        .iter()
        .flat_map(|conflict| conflict.instrument_ids.iter().map(String::as_str))
        .collect();
    let mut by_answer: BTreeMap<&str, Vec<(&str, bool)>> = BTreeMap::new();
    // What the platform's answer contradicts of a person's (contract v25):
    // each field with the person's value, which the line shows.
    let mut contradicted: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for item in &listed.instruments {
        let Some(record) = item.instrument.as_ref() else {
            continue;
        };
        let section = S::try_from(item.migration_section).unwrap_or(S::Unspecified);
        let line = matches!(
            section,
            S::AlreadyHeld | S::Matched | S::Minted | S::NearMatch | S::HeldBack
        );
        if !record.provisional || !line || !item.answered_instrument_id.starts_with("INS-") {
            continue;
        }
        by_answer
            .entry(item.answered_instrument_id.as_str())
            .or_default()
            .push((record.instrument_id.as_str(), section == S::HeldBack));
        let fields: Vec<String> = item
            .contradicted_fields
            .iter()
            .map(|field| {
                let (name, value) = match meridian_domain::v1::InstrumentField::try_from(*field) {
                    Ok(meridian_domain::v1::InstrumentField::AssetClass) => (
                        "asset class",
                        meridian_domain::v1::AssetClass::try_from(record.asset_class)
                            .map(|class| {
                                class
                                    .as_str_name()
                                    .trim_start_matches("ASSET_CLASS_")
                                    .to_ascii_lowercase()
                            })
                            .unwrap_or_default(),
                    ),
                    Ok(meridian_domain::v1::InstrumentField::Currency) => {
                        ("currency", record.currency.clone())
                    }
                    Ok(meridian_domain::v1::InstrumentField::Description) => {
                        ("description", record.description.clone())
                    }
                    Ok(meridian_domain::v1::InstrumentField::InstrumentType) => (
                        "type",
                        meridian_domain::v1::InstrumentType::try_from(record.instrument_type)
                            .map(|kind| {
                                kind.as_str_name()
                                    .trim_start_matches("INSTRUMENT_TYPE_")
                                    .to_ascii_lowercase()
                            })
                            .unwrap_or_default(),
                    ),
                    _ => ("a field", String::new()),
                };
                if value.is_empty() {
                    name.to_string()
                } else {
                    format!("{name} {value}")
                }
            })
            .collect();
        if !fields.is_empty() {
            contradicted.insert(record.instrument_id.as_str(), fields);
        }
    }
    let mut exact = Vec::new();
    let mut uncertain = Vec::new();
    for (answered, records) in by_answer {
        if records.len() > 1 {
            uncertain.push(UncertainLine {
                instrument_ids: records.iter().map(|(id, _)| id.to_string()).collect(),
                answered: answered.to_string(),
                why: "a duplicate: two records answered by one".into(),
            });
            continue;
        }
        let (id, held_back) = records[0];
        if held_back {
            uncertain.push(UncertainLine {
                instrument_ids: vec![id.to_string()],
                answered: answered.to_string(),
                why: "held back".into(),
            });
        } else if conflicted.contains(id) {
            uncertain.push(UncertainLine {
                instrument_ids: vec![id.to_string()],
                answered: answered.to_string(),
                why: "an identifier meets another record".into(),
            });
        } else if let Some(fields) = contradicted.get(id) {
            // Contract v25 (W3.26): the platform's answer contradicts what a
            // person set or accepted -- a line of its own, the person's
            // value shown.
            uncertain.push(UncertainLine {
                instrument_ids: vec![id.to_string()],
                answered: answered.to_string(),
                why: format!(
                    "the platform's answer contradicts what a person set: {}",
                    fields.join(", ")
                ),
            });
        } else {
            exact.push(id.to_string());
        }
    }
    exact.sort();
    (exact, uncertain)
}

/// The buttons opening the lines' dialogs, and the dialogs: the exact lines
/// as one change (at most 500 at a time, each line held back by its box),
/// and the uncertain lines, one chosen at a time. Prepared or made, with a
/// note saying why (W3.26, W6.26, W6.27).
fn lines_controls(listed: &ListInstrumentsToCompleteReply, token: &str) -> (String, String) {
    let (exact, uncertain) = migration_lines(listed);
    if exact.is_empty() && uncertain.is_empty() {
        return (String::new(), String::new());
    }
    let answered: std::collections::BTreeMap<&str, &str> = listed
        .instruments
        .iter()
        .filter_map(|item| {
            item.instrument.as_ref().map(|record| {
                (
                    record.instrument_id.as_str(),
                    item.answered_instrument_id.as_str(),
                )
            })
        })
        .collect();
    let batch: Vec<&String> = exact.iter().take(MOST_LINES).collect();
    let foot = |make: &str| {
        format!(
            "<label class=\"why\">Why <input name=\"note\" required maxlength=\"2000\"></label>\
             <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
             <button type=\"submit\" name=\"prepare\" value=\"1\" data-prepare>Prepare</button>{make}</div>"
        )
    };
    let mut buttons = String::from("<div class=\"head-actions\">");
    let mut dialogs = String::new();
    if !batch.is_empty() {
        let rows: String = batch
            .iter()
            .map(|id| {
                format!(
                    "<tr data-line=\"{id}\"><td>{id}<input type=\"hidden\" name=\"line\" value=\"{id}\"></td>\
                     <td>{answer}</td><td><label class=\"inline\"><input type=\"checkbox\" name=\"hold\" \
                     value=\"{id}\"> Hold back</label></td></tr>",
                    id = escape(id),
                    answer = escape(answered.get(id.as_str()).copied().unwrap_or("")),
                )
            })
            .collect();
        let more = if exact.len() > batch.len() {
            format!(
                " The first {} of {}; the rest are the next changes, in order.",
                batch.len(),
                exact.len()
            )
        } else {
            String::new()
        };
        buttons.push_str(&format!(
            "<button type=\"button\" data-dialog-open=\"exact-lines\" data-exact-lines=\"{}\">Exact lines ({})</button>",
            exact.len(),
            exact.len()
        ));
        dialogs.push_str(&format!(
            "<dialog id=\"exact-lines\" aria-labelledby=\"exact-lines-title\">\
             <form method=\"post\" action=\"{LINES_PATH}\">{token}\
             <div class=\"dialog-head\"><h2 id=\"exact-lines-title\">The exact lines</h2></div>\
             <div class=\"dialog-body\"><p class=\"hint\">Made together as one change: each record \
             replaced by the platform's, its history reading across the link. A line held back \
             keeps its local ID, and nothing is minted for it.{more}</p>\
             <om-pager search=\"Search lines\"><table class=\"list one-line\" id=\"exact-lines-table\">\
             <thead><tr><th>Record</th><th>Becomes</th><th></th></tr></thead><tbody>{rows}</tbody>\
             </table></om-pager>{foot}</div></form></dialog>",
            foot = foot("<button type=\"submit\" class=\"primary\">Make the lines</button>"),
        ));
    }
    if !uncertain.is_empty() {
        let rows: String = uncertain
            .iter()
            .enumerate()
            .map(|(n, line)| {
                let ids = line.instrument_ids.join(",");
                format!(
                    "<tr data-uncertain=\"{ids}\"><td><label class=\"inline\"><input type=\"radio\" \
                     name=\"group\" value=\"{ids}\" required{checked}> {shown}</label></td><td>{answer}</td>\
                     <td title=\"{why}\">{why}</td></tr>",
                    ids = escape(&ids),
                    shown = escape(&line.instrument_ids.join(", ")),
                    checked = if n == 0 { " checked" } else { "" },
                    answer = escape(&line.answered),
                    why = escape(&line.why),
                )
            })
            .collect();
        buttons.push_str(&format!(
            "<button type=\"button\" data-dialog-open=\"uncertain-lines\" data-uncertain-lines=\"{}\">Uncertain lines ({})</button>",
            uncertain.len(),
            uncertain.len()
        ));
        dialogs.push_str(&format!(
            "<dialog id=\"uncertain-lines\" aria-labelledby=\"uncertain-lines-title\">\
             <form method=\"post\" action=\"{LINES_PATH}\">{token}\
             <div class=\"dialog-head\"><h2 id=\"uncertain-lines-title\">Uncertain lines</h2></div>\
             <div class=\"dialog-body\"><p class=\"hint\">Each line its own change: choose one, then \
             make it or hold it back. A duplicate's records both become the one record, the book \
             summing them.</p>\
             <om-pager search=\"Search lines\"><table class=\"list one-line\" id=\"uncertain-lines-table\">\
             <thead><tr><th>Records</th><th>Becomes</th><th>Why</th></tr></thead><tbody>{rows}</tbody>\
             </table></om-pager>{foot}</div></form></dialog>",
            foot = foot(
                "<button type=\"submit\" name=\"hold_all\" value=\"1\">Hold back</button>\
                 <button type=\"submit\" class=\"primary\">Make the line</button>"
            ),
        ));
    }
    buttons.push_str("</div>");
    (buttons, dialogs)
}

/// The usage report's setting and the reports sent (contract v24, W3.28).
fn usage_section(
    reports: &Result<meridian_domain::v1::ListUsageReportsReply, String>,
    token: &str,
) -> String {
    let sent = match reports {
        Err(_) => {
            "<p class=\"refused\">The conductor did not say which reports it sent.</p>".to_string()
        }
        Ok(reports) if reports.reports.is_empty() => {
            "<p class=\"empty\" data-usage-reports=\"0\">None sent yet.</p>".to_string()
        }
        Ok(reports) => format!(
            "<ul class=\"counts\" data-usage-reports=\"{}\">{}</ul>",
            reports.reports.len(),
            reports
                .reports
                .iter()
                .take(5)
                .map(|sent| {
                    let report = sent.report.clone().unwrap_or_default();
                    format!(
                        "<li>{} to {}: {} scheme{}, {}</li>",
                        escape(&report.period_start),
                        escape(&report.period_end),
                        report.scheme_counts.len(),
                        if report.scheme_counts.len() == 1 {
                            ""
                        } else {
                            "s"
                        },
                        if sent.received {
                            "received"
                        } else {
                            "not received"
                        }
                    )
                })
                .collect::<String>()
        ),
    };
    // The setting in force, as the reports read answers it (contract v24,
    // ListUsageReportsReply.reports_usage); on where it was not read, the
    // default.
    let on = reports
        .as_ref()
        .map(|reports| reports.reports_usage)
        .unwrap_or(true);
    let (on_selected, off_selected) = if on {
        (" selected", "")
    } else {
        ("", " selected")
    };
    format!(
        "<h3>Report identifier usage</h3><p class=\"hint\">Weekly counts per scheme, on by default: \
         never a value, a holding, a quantity or an account.</p>\
         <form method=\"post\" action=\"{USAGE_PATH}\" class=\"inline-form\" id=\"usage-form\" \
         data-reports-usage=\"{}\">{token}\
         <label>Report identifier usage <select name=\"reports_usage\">\
         <option value=\"on\"{on_selected}>On</option>\
         <option value=\"off\"{off_selected}>Off</option></select></label>\
         <label>Why <input name=\"note\" required maxlength=\"2000\"></label>\
         <button type=\"submit\" name=\"prepare\" value=\"1\" data-prepare>Prepare</button>\
         <button type=\"submit\" class=\"primary\">Set</button></form>{licensed}{sent}",
        if on { "on" } else { "off" },
        licensed = licensed_form(reports, token),
    )
}

/// The schemes the deployment declares it is licensed for (contract v25,
/// W3.28), sent with the weekly report: one box per scheme a deployment holds
/// under licence, and any declared beside them; the conductor holds the
/// choice to the platform's licence list. One line, a note and Set.
fn licensed_form(
    reports: &Result<meridian_domain::v1::ListUsageReportsReply, String>,
    token: &str,
) -> String {
    let declared: Vec<String> = reports
        .as_ref()
        .map(|reports| reports.licensed_schemes.clone())
        .unwrap_or_default();
    let mut offered: Vec<String> = ["cusip", "isin", "sedol"]
        .iter()
        .map(|scheme| scheme.to_string())
        .collect();
    for scheme in &declared {
        if !offered.contains(scheme) {
            offered.push(scheme.clone());
        }
    }
    let boxes: String = offered
        .iter()
        .map(|scheme| {
            format!(
                "<label class=\"check\"><input type=\"checkbox\" name=\"licensed_scheme\" \
                 value=\"{value}\"{checked}> {shown}</label>",
                value = escape(scheme),
                checked = if declared.contains(scheme) {
                    " checked"
                } else {
                    ""
                },
                shown = escape(&scheme.to_ascii_uppercase()),
            )
        })
        .collect();
    format!(
        "<form method=\"post\" action=\"{LICENSED_SCHEMES_PATH}\" class=\"inline-form\" \
         id=\"licensed-schemes-form\" data-licensed-schemes=\"{}\">{token}\
         <span class=\"label\">Licensed for</span>{boxes}\
         <label>Why <input name=\"note\" required maxlength=\"2000\"></label>\
         <button type=\"submit\" name=\"prepare\" value=\"1\" data-prepare>Prepare</button>\
         <button type=\"submit\" class=\"primary\">Set</button></form>",
        escape(&declared.join(",")),
    )
}

/// Where the licensed schemes' form posts (contract v25).
pub const LICENSED_SCHEMES_PATH: &str = "/admin/instruments/licensed-schemes";

/// Where the usage report's form posts.
pub const USAGE_PATH: &str = "/admin/instruments/usage";

/// The Instruments page whole, from contract v24 with its provisional tab
/// and the usage report's setting.
pub fn list_page_full(
    listed: &Result<ListInstrumentsToCompleteReply, String>,
    provisional: Option<&Result<ListInstrumentsToCompleteReply, String>>,
    usage: Option<&Result<meridian_domain::v1::ListUsageReportsReply, String>>,
    token: &str,
    notice: &str,
    calendars: &str,
    now_ns: i64,
) -> String {
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"passed\">{}</p>", escape(notice))
    };
    let listed = match listed {
        Ok(listed) => listed,
        Err(why) => {
            return format!(
                "<div class=\"admin\"><div class=\"page-head\"><h1>Instruments</h1></div>{notice}\
                 <p class=\"refused\">The instrument store is not answering: {}</p></div>",
                escape(why)
            )
        }
    };

    let counts = format!(
        "<p class=\"hint\"><strong>{}</strong> the book cannot use (no asset class or no \
         currency), <strong>{}</strong> lacking something. Each value you set is kept with its \
         source and your name.</p>",
        listed.incomplete_for_book, listed.incomplete
    );
    let licensed: Vec<String> = listed
        .licensed_identifiers
        .iter()
        .map(|count| {
            let over = count.scheme == "cusip" && count.count >= CUSIP_THRESHOLD;
            format!(
                "<li>{}: <strong>{}</strong>{}</li>",
                escape(&count.scheme.to_ascii_uppercase()),
                count.count,
                if count.scheme == "cusip" {
                    if over {
                        format!(
                            " <span class=\"badge warn\">at or past CUSIP Global Services' \
                             {CUSIP_THRESHOLD}-identifier threshold</span>"
                        )
                    } else {
                        format!(" of CUSIP Global Services' {CUSIP_THRESHOLD}-identifier threshold")
                    }
                } else {
                    String::new()
                }
            )
        })
        .collect();
    // From contract v24 the counts alone are reported (W3.28), never the
    // values, unless the deployment admin turns the report off.
    let licensed = format!(
        "<section class=\"admin-section\" id=\"licensed\"><div class=\"section-head\"><div>\
         <h2>Licensed identifiers held</h2><p class=\"hint\">What your plugins reported, kept \
         here under your own licence: never sent anywhere, only how many are counted in the \
         usage report.</p></div></div>{}{}</section>",
        if licensed.is_empty() {
            "<p class=\"empty\">None.</p>".to_string()
        } else {
            format!("<ul class=\"facts\">{}</ul>", licensed.concat())
        },
        usage
            .map(|usage| usage_section(usage, token))
            .unwrap_or_default(),
    );
    let (provisional_n, provisional_tab) = match provisional {
        Some(Ok(provisional)) => provisional_tab(provisional, token, now_ns),
        Some(Err(why)) => (
            0,
            format!(
                "<section class=\"admin-section\" id=\"provisional\"><p class=\"refused\">The \
                 instrument store did not list the provisional records: {}</p></section>",
                escape(why)
            ),
        ),
        None => (0, String::new()),
    };

    let conflict_rows: String = listed
        .conflicts
        .iter()
        .enumerate()
        .map(|(n, conflict)| {
            let ids: Vec<String> = conflict.instrument_ids.clone();
            let records: String = ids
                .iter()
                .map(|id| {
                    format!(
                        "<a href=\"/admin/instruments/{id}\">{id}</a>",
                        id = escape(id)
                    )
                })
                .collect::<Vec<_>>()
                .join(" and ");
            let identifiers = conflict
                .identifiers
                .iter()
                .map(identifier_words)
                .collect::<Vec<_>>()
                .join(", ");
            // Contract v19 (W3.11, the plan's Q35): a resolve met it, flagged
            // for a person; nothing of it is told to the platform.
            let why = if conflict.reason == MissReason::Ambiguous as i32 {
                "<span class=\"badge warn\" data-reason=\"ambiguous\" title=\"A plugin's resolve \
                 met more than one record: flagged for you, never sent to the platform\">a resolve \
                 met both</span>"
            } else {
                "<span class=\"badge\" title=\"A completion, a join or the platform's ID met \
                 another record's identifier\">an identifier held twice</span>"
            };
            format!(
                "<tr data-conflict=\"{n}\"><td title=\"{identifiers}\">{identifiers}</td>\
                 <td>{records}</td><td>{why}</td><td class=\"wide\" title=\"{by}\">{by}</td>\
                 <td class=\"actions\"><button type=\"button\" data-dialog-open=\"merge-{n}\">Merge</button></td></tr>",
                identifiers = escape(&identifiers),
                by = escape(if conflict.reported_by.is_empty() {
                    "—"
                } else {
                    &conflict.reported_by
                }),
            )
        })
        .collect();
    let merge_dialogs: String = listed
        .conflicts
        .iter()
        .enumerate()
        .map(|(n, conflict)| {
            let ids: Vec<String> = conflict.instrument_ids.clone();
            let choices: String = ids
                .iter()
                .enumerate()
                .map(|(at, id)| {
                    format!(
                        "<label class=\"choice\"><input type=\"radio\" name=\"kept_instrument_id\" \
                         value=\"{id}\"{checked}> keep {id}</label>",
                        id = escape(id),
                        checked = if at == 0 { " checked" } else { "" }
                    )
                })
                .collect();
            format!(
                "<dialog id=\"merge-{n}\" aria-labelledby=\"merge-{n}-title\"><form method=\"post\" \
                 action=\"/admin/instruments/merge\">{token}\
                 <div class=\"dialog-head\"><h2 id=\"merge-{n}-title\">Merge {ids_said}</h2></div>\
                 <div class=\"dialog-body\"><input type=\"hidden\" name=\"instrument_ids\" value=\"{ids_list}\">\
                 <p class=\"hint\">The positions held under the one merged move onto the one that \
                 stays; its identifiers' windows on it close now, never deleted.</p>{choices}\
                 <label>Why they are one security <input name=\"note\" required></label></div>\
                 <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
                 <button type=\"submit\" class=\"primary\">Merge</button></div></form></dialog>",
                ids_said = escape(&ids.join(" and ")),
                ids_list = escape(&ids.join(",")),
            )
        })
        .collect();
    let conflicts = if listed.conflicts.is_empty() {
        "<p class=\"empty\" data-conflicts=\"0\">None.</p>".to_string()
    } else {
        format!(
            "<om-pager search=\"Search conflicts\"><table class=\"list one-line conflicts\" \
             data-conflicts=\"{n}\"><thead><tr><th>Identifiers</th><th>Records</th><th>Why</th>\
             <th class=\"wide\">Reported by</th><th class=\"actions\"></th></tr></thead>\
             <tbody>{conflict_rows}</tbody></table></om-pager>{merge_dialogs}",
            n = listed.conflicts.len()
        )
    };

    let rows: String = listed
        .instruments
        .iter()
        .filter_map(|item| item.instrument.as_ref().map(|record| (item, record)))
        .map(|(item, record)| {
            let offered: Vec<String> = record
                .offers
                .iter()
                .filter_map(|offer| offer.value.as_ref())
                .map(|value| format!("{} ({})", value_words(value), value.source))
                .collect();
            let ids = record
                .identifiers
                .iter()
                .filter(|identifier| identifier.valid_until_ns == 0)
                .map(identifier_words)
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "<tr data-id=\"{id}\"><td><input type=\"checkbox\" name=\"instrument_id\" value=\"{id}\" form=\"accept\"{disabled} \
                 aria-label=\"Accept the values offered for {id}\"></td>\
                 <td title=\"{id}: {ids}\"><a href=\"/admin/instruments/{id}\">{id}</a> <span class=\"id\">{ids}</span></td>\
                 <td>{class}</td><td class=\"wide\">{currency}</td><td title=\"{description}\">{description}</td>\
                 <td title=\"{lacks}\">{lacks}{book}</td><td class=\"wide\" title=\"{offered}\">{offered}</td></tr>",
                id = escape(&record.instrument_id),
                disabled = if offered.is_empty() { " disabled" } else { "" },
                ids = escape(&ids),
                class = escape(class_name(record.asset_class)),
                currency = escape(&record.currency),
                description = escape(&record.description),
                lacks = escape(&lacks_words(&item.lacks)),
                book = if item.complete_for_book {
                    ""
                } else {
                    " <span class=\"badge warn\">the book cannot use it</span>"
                },
                offered = escape(&offered.join("; ")),
            )
        })
        .collect();
    let table = if rows.is_empty() {
        "<p class=\"empty\">Every record is complete.</p>".to_string()
    } else {
        format!(
            "<form id=\"accept\" method=\"post\" action=\"/admin/instruments/accept\">{token}\
             <div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Accept offered values \
             for the records ticked</button></div></form>\
             <om-pager search=\"Search instruments\"><table class=\"list one-line\" id=\"instruments-table\">\
             <thead><tr><th></th><th>Record</th><th>Asset class</th>\
             <th class=\"wide\">Currency</th><th>Description</th><th>Lacks</th><th class=\"wide\">Offered</th>\
             </tr></thead><tbody>{rows}</tbody></table></om-pager>"
        )
    };

    format!(
        "<div class=\"admin\"><div class=\"page-head\"><div><h1>Instruments</h1>\
         <p class=\"hint\">The deployment's own instrument records. An opening balance naming one \
         the book cannot use is refused until it is complete.</p></div></div>{notice}\
         <nav class=\"tabs\"><a href=\"#to-complete\" data-tab=\"to-complete\">To complete</a>\
         <a href=\"#conflicts\" data-tab=\"conflicts\">Conflicts ({conflicts_n})</a>\
         {provisional_link}<a href=\"#licensed\" data-tab=\"licensed\">Licensed identifiers</a>{calendars_tab}</nav>\
         <section class=\"admin-section\" id=\"to-complete\"><div class=\"section-head\"><div>\
         <h2>Records to complete</h2>{counts}</div></div>{table}</section>\
         <section class=\"admin-section\" id=\"conflicts\"><div class=\"section-head\"><div>\
         <h2>Conflicts</h2><p class=\"hint\">Identifiers that meet two records. Merge them if \
         they are one security; the positions held under the one merged move onto the one that \
         stays.</p></div></div>{conflicts}</section>{provisional_tab}{licensed}{calendars}</div>\
         <script>{}{}{}</script>",
        super::overview::SCRIPT_START,
        super::overview::SCRIPT_END,
        super::data_sources::PAGER_SCRIPT,
        conflicts_n = listed.conflicts.len(),
        provisional_link = if provisional.is_some() {
            format!("<a href=\"#provisional\" data-tab=\"provisional\">Provisional ({provisional_n})</a>")
        } else {
            String::new()
        },
        calendars_tab = if calendars.is_empty() {
            ""
        } else {
            "<a href=\"#calendars\" data-tab=\"calendars\">Calendars</a>"
        },
    )
}

/// The first offer for a field, where one is held.
fn offer_for(record: &InstrumentRecord, field: InstrumentField) -> Option<&OfferedValue> {
    record
        .offers
        .iter()
        .find(|offer| offer.value.as_ref().map(field_of) == Some(field as i32))
}

/// Where a field's value in force came from, in words.
fn source_words(record: &InstrumentRecord, field: InstrumentField) -> String {
    record
        .sources
        .iter()
        .find(|source| source.field == field as i32)
        .map(|source| {
            let who = if !source.person.is_empty() {
                format!(", set by {}", source.person)
            } else if !source.instance_id.is_empty() {
                format!(", reported by {}", source.instance_id)
            } else {
                String::new()
            };
            format!("{}{}", source.source, who)
        })
        .unwrap_or_default()
}

/// A record's page: its values with their sources, the form completing it
/// pre-filled with what is offered, the platform's answer when asked, and its
/// history (W3.10, W3.12, W3.3).
pub fn record_page(
    item: &InstrumentToComplete,
    versions: &[InstrumentVersion],
    asked: Option<&Result<AskPlatformForInstrumentReply, String>>,
    token: &str,
) -> String {
    let record = item.instrument.clone().unwrap_or_default();
    let id = escape(&record.instrument_id);

    let field_row = |label: &str, field: InstrumentField, value: String| {
        format!(
            "<tr><th>{label}</th><td>{}</td><td class=\"hint\">{}</td></tr>",
            escape(&value),
            escape(&source_words(&record, field))
        )
    };
    let identifiers: String = record
        .identifiers
        .iter()
        .map(|identifier| {
            let source = record
                .sources
                .iter()
                .find(|source| {
                    source.identifier.as_ref().is_some_and(|held| {
                        held.scheme == identifier.scheme
                            && held.value == identifier.value
                            && held.source == identifier.source
                    })
                })
                .map(|source| {
                    if source.instance_id.is_empty() {
                        source.source.clone()
                    } else {
                        format!("reported by {}", source.instance_id)
                    }
                })
                .unwrap_or_default();
            // Its window (contract v19): one that left the record is kept,
            // its window closed, never deleted.
            let window = match (identifier.valid_from_ns, identifier.valid_until_ns) {
                (_, until) if until > 0 => {
                    format!("; left the record {}", instrument_type::utc_minute(until))
                }
                (from, _) if from > 0 => {
                    format!(
                        "; on the record since {}",
                        instrument_type::utc_minute(from)
                    )
                }
                _ => String::new(),
            };
            format!(
                "<tr{left}><th>Identifier</th><td>{}</td><td class=\"hint\">{}{}</td></tr>",
                escape(&identifier_words(identifier)),
                escape(&source),
                escape(&window),
                left = if identifier.valid_until_ns > 0 {
                    " data-left"
                } else {
                    ""
                },
            )
        })
        .collect();
    let values = format!(
        "<table class=\"list\"><tbody>{}{}{}{identifiers}</tbody></table>",
        field_row(
            "Asset class",
            InstrumentField::AssetClass,
            class_name(record.asset_class).to_string()
        ),
        field_row(
            "Currency",
            InstrumentField::Currency,
            record.currency.clone()
        ),
        field_row(
            "Description",
            InstrumentField::Description,
            record.description.clone()
        ),
    ) + &format!(
        "<table class=\"list\"><tbody>{}{}</tbody></table>",
        field_row(
            "Instrument type",
            InstrumentField::InstrumentType,
            type_words(record.instrument_type).to_string()
        ),
        field_row(
            "Money market fund",
            InstrumentField::MoneyMarketFund,
            record
                .money_market_fund
                .as_ref()
                .map(|fund| instrument_type::fund_words(&instrument_type::fund_to_text(fund)))
                .unwrap_or_default()
        ),
    ) + &format!(
        "<table class=\"list\"><tbody>{}</tbody></table>",
        field_row(
            "Binary event contract",
            InstrumentField::BinaryEventContract,
            record
                .binary_event_contract
                .as_ref()
                .map(|c| instrument_type::contract_words(&instrument_type::contract_to_text(c)))
                .unwrap_or_default()
        ),
    );

    // Pre-filled: the value in force and its source; otherwise what is
    // offered, with the offer's source, which a person changing the value
    // must change too.
    let prefill = |field: InstrumentField, held: String| -> (String, String, String) {
        if !held.is_empty() {
            return (held, source_words(&record, field), String::new());
        }
        match offer_for(&record, field).and_then(|offer| offer.value.as_ref()) {
            Some(value) => {
                let text = match &value.value {
                    Some(instrument_value::Value::AssetClass(class)) => class.to_string(),
                    _ => value_words(value),
                };
                (text, value.source.clone(), value.source.clone())
            }
            None => (String::new(), String::new(), String::new()),
        }
    };
    let held_class = if record.asset_class == 0 {
        String::new()
    } else {
        record.asset_class.to_string()
    };
    let (class, class_source, class_offer) = prefill(InstrumentField::AssetClass, held_class);
    let (currency, currency_source, currency_offer) =
        prefill(InstrumentField::Currency, record.currency.clone());
    let (description, description_source, description_offer) =
        prefill(InstrumentField::Description, record.description.clone());
    let held_type = if record.instrument_type == 0 {
        String::new()
    } else {
        record.instrument_type.to_string()
    };
    let (kind, kind_source, kind_offer) = match offer_for(&record, InstrumentField::InstrumentType)
        .and_then(|offer| offer.value.as_ref())
    {
        Some(InstrumentValue {
            value: Some(instrument_value::Value::InstrumentType(offered)),
            source,
        }) if held_type.is_empty() => (offered.to_string(), source.clone(), source.clone()),
        _ => (
            held_type.clone(),
            if held_type.is_empty() {
                String::new()
            } else {
                source_words(&record, InstrumentField::InstrumentType)
            },
            String::new(),
        ),
    };
    let type_options: String = std::iter::once("<option value=\"\">Not set</option>".to_string())
        .chain(TYPES.iter().map(|(value, class, name)| {
            let value = (*value as i32).to_string();
            format!(
                "<option value=\"{value}\" data-om-applies-when=\"asset_class\" \
                 data-om-one-of=\"{class}\"{}>{name}</option>",
                if value == kind { " selected" } else { "" },
                class = *class as i32,
            )
        }))
        .collect();
    // The classes a type is under: the type's fields show for them alone.
    let typed_classes = serde_json::to_string(
        &TYPES
            .iter()
            .map(|(_, class, _)| (*class as i32).to_string())
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();
    let contract = record.binary_event_contract.clone().unwrap_or_default();
    let contract_payout = contract.payout.clone().unwrap_or_default();
    let contract_source = if record.binary_event_contract.is_some() {
        source_words(&record, InstrumentField::BinaryEventContract)
    } else {
        String::new()
    };
    let contract_fields = format!(
        "<label {gate}>Pays, per contract <input name=\"contract_amount\" inputmode=\"decimal\" \
         placeholder=\"1\" value=\"{amount}\"></label>\
         <label {gate}>Paid in: a code, or its record <input name=\"contract_asset\" \
         placeholder=\"USD\" value=\"{asset}\"></label>\
         <label {gate}>Trading closes, UTC <input type=\"datetime-local\" name=\"contract_closes\" \
         value=\"{closes}\"></label>\
         <label {gate}>Where they came from <input name=\"contract_source\" value=\"{source}\"></label>",
        gate = format!(
            "data-om-applies-when=\"instrument_type\" data-om-one-of=\"{}\"",
            InstrumentType::BinaryEventContract as i32
        ),
        amount = escape(
            &contract_payout
                .amount
                .as_ref()
                .and_then(|d| meridian_domain::exact::Exact::from_wire(d).ok())
                .map(|e| e.to_string())
                .unwrap_or_default()
        ),
        asset = escape(if contract_payout.currency_code.is_empty() {
            &contract_payout.instrument_id
        } else {
            &contract_payout.currency_code
        }),
        closes = escape(&utc_field(contract.closes_at_ns)),
        source = escape(&contract_source),
    );
    let fund = record.money_market_fund.unwrap_or_default();
    let fund_source = if record.money_market_fund.is_some() {
        source_words(&record, InstrumentField::MoneyMarketFund)
    } else {
        String::new()
    };
    let select = |name: &str, held: i32, choices: &[(i32, &str)]| {
        let options: String = std::iter::once("<option value=\"\">Not set</option>".to_string())
            .chain(choices.iter().map(|(value, words)| {
                format!(
                    "<option value=\"{value}\"{}>{words}</option>",
                    if *value == held { " selected" } else { "" }
                )
            }))
            .collect();
        format!("<select name=\"{name}\">{options}</select>")
    };
    let fund_fields = format!(
        "<label {class_gate}>Instrument type <select name=\"instrument_type\" data-initial=\"{kind}\">{type_options}</select></label>\
         <label {class_gate}>Where it came from <input name=\"instrument_type_source\" value=\"{kind_source}\"{kind_offer}></label>\
         <label {fund_gate}>Money market fund: category {category}</label>\
         <label {fund_gate}>investors {investors}</label>\
         <label {fund_gate}>net asset value {nav}</label>\
         <label {fund_gate}>liquidity fee {fee}</label>\
         <label {fund_gate}>Where they came from <input name=\"fund_source\" value=\"{fund_source}\"></label>\
         {contract_fields}",
        class_gate = format!(
            "data-om-applies-when=\"asset_class\" data-om-one-of='{}'",
            escape(&typed_classes)
        ),
        fund_gate = format!(
            "data-om-applies-when=\"instrument_type\" data-om-one-of=\"{}\"",
            InstrumentType::MoneyMarketFund as i32
        ),
        kind = escape(&kind),
        kind_source = escape(&kind_source),
        kind_offer = if kind_offer.is_empty() {
            String::new()
        } else {
            format!(" data-offered-source=\"{}\"", escape(&kind_offer))
        },
        category = select(
            "fund_category",
            fund.category,
            &[
                (MoneyMarketFundCategory::Government as i32, "government"),
                (MoneyMarketFundCategory::Prime as i32, "prime"),
                (MoneyMarketFundCategory::TaxExempt as i32, "tax-exempt"),
            ]
        ),
        investors = select(
            "fund_investors",
            fund.investors,
            &[
                (MoneyMarketFundInvestors::Retail as i32, "retail"),
                (MoneyMarketFundInvestors::Institutional as i32, "institutional"),
            ]
        ),
        nav = select(
            "fund_nav",
            fund.nav,
            &[
                (MoneyMarketFundNav::Stable as i32, "stable at 1.00"),
                (MoneyMarketFundNav::Floating as i32, "floating"),
            ]
        ),
        fee = select(
            "fund_liquidity_fee",
            fund.liquidity_fee,
            &[
                (LiquidityFeeRegime::Mandatory as i32, "mandatory"),
                (LiquidityFeeRegime::Discretionary as i32, "discretionary"),
                (LiquidityFeeRegime::None as i32, "none"),
            ]
        ),
        fund_source = escape(&fund_source),
    );
    let options: String = std::iter::once("<option value=\"\">Not set</option>".to_string())
        .chain(CLASSES.iter().map(|(value, name)| {
            let value = (*value as i32).to_string();
            format!(
                "<option value=\"{value}\"{}>{name}</option>",
                if value == class { " selected" } else { "" }
            )
        }))
        .collect();
    let offered_attr = |offer: &str| {
        if offer.is_empty() {
            String::new()
        } else {
            format!(" data-offered-source=\"{}\"", escape(offer))
        }
    };
    let form = format!(
        "<form method=\"post\" action=\"/admin/instruments/complete\" class=\"panel complete\" id=\"complete\">{token}\
         <input type=\"hidden\" name=\"instrument_id\" value=\"{id}\">\
         <input type=\"hidden\" name=\"against_version\" value=\"{version}\">\
         <input type=\"hidden\" name=\"asset_class_held\" value=\"{class_held}\">\
         <input type=\"hidden\" name=\"currency_held\" value=\"{currency_held}\">\
         <input type=\"hidden\" name=\"description_held\" value=\"{description_held}\">\
         <input type=\"hidden\" name=\"instrument_type_held\" value=\"{type_held}\">\
         <input type=\"hidden\" name=\"fund_held\" value=\"{fund_held}\">\
         <input type=\"hidden\" name=\"contract_held\" value=\"{contract_held}\">\
         {ends}<div class=\"fields\">\
         <label>Asset class <select name=\"asset_class\" data-initial=\"{class}\">{options}</select></label>\
         <label>Where it came from <input name=\"asset_class_source\" value=\"{class_source}\"{class_offer}></label>\
         <label>Currency <input name=\"currency\" maxlength=\"3\" pattern=\"[A-Za-z]{{3}}\" placeholder=\"USD\" value=\"{currency}\" data-initial=\"{currency}\"></label>\
         <label>Where it came from <input name=\"currency_source\" value=\"{currency_source}\"{currency_offer}></label>\
         <label>Description <input name=\"description\" value=\"{description}\" data-initial=\"{description}\"></label>\
         <label>Where it came from <input name=\"description_source\" value=\"{description_source}\"{description_offer}></label>\
         {fund_fields}\
         <label class=\"check\"><input type=\"checkbox\" name=\"adding\" value=\"identifier\"> Add an identifier</label>\
         <label {adding}>Scheme <input name=\"identifier_scheme\" placeholder=\"figi\"></label>\
         <label {adding}>Value <input name=\"identifier_value\"></label>\
         <label {adding}>Namespace <input name=\"identifier_namespace\" placeholder=\"none: a global scheme\"></label>\
         <label {adding}>Where it came from <input name=\"identifier_source\"></label>\
         <label>Why, when you change a value already set <input name=\"note\"></label>\
         </div><div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Save</button></div></form>",
        adding = "data-om-applies-when=\"adding\" data-om-one-of=\"identifier\"",
        version = record.version,
        class_held = escape(&if record.asset_class == 0 {
            String::new()
        } else {
            record.asset_class.to_string()
        }),
        currency_held = escape(&record.currency),
        description_held = escape(&record.description),
        type_held = escape(&held_type),
        fund_held = escape(
            &record
                .money_market_fund
                .as_ref()
                .map(instrument_type::fund_to_text)
                .unwrap_or_default()
        ),
        contract_held = escape(
            &record
                .binary_event_contract
                .as_ref()
                .map(instrument_type::contract_to_text)
                .unwrap_or_default()
        ),
        ends = ends_offered(&record),
        class = escape(&class),
        class_source = escape(&class_source),
        class_offer = offered_attr(&class_offer),
        currency = escape(&currency),
        currency_source = escape(&currency_source),
        currency_offer = offered_attr(&currency_offer),
        description = escape(&description),
        description_source = escape(&description_source),
        description_offer = offered_attr(&description_offer),
    );

    let asked_html = match asked {
        None => String::new(),
        Some(Err(why)) => format!(
            "<p class=\"refused\">The platform could not be asked: {}. Completing the record works \
             without it.</p>",
            escape(why)
        ),
        Some(Ok(reply)) if !reply.reachable => format!(
            "<p class=\"notice warn\">The platform could not be reached: {}. Completing the record \
             works without it.</p>",
            escape(&reply.detail)
        ),
        Some(Ok(reply)) if !reply.found => format!(
            "<p class=\"notice\">The platform holds no record by this record's open identifiers. {}</p>",
            escape(&reply.detail)
        ),
        Some(Ok(reply)) => {
            let platform = reply.instrument.clone().unwrap_or_default();
            format!(
                "<p class=\"notice good\">The platform holds {} ({}, {}, {}). Its ID joins this \
                 record as an identifier, and its values are offered below for you to accept; \
                 nothing it says replaces a value you set.</p>",
                escape(&platform.instrument_id),
                escape(class_name(platform.asset_class)),
                escape(&platform.currency),
                escape(&platform.description),
            )
        }
    };

    let history: String = versions
        .iter()
        .map(|version| {
            let changes = version
                .changes
                .iter()
                .map(|change| {
                    let field = match InstrumentField::try_from(change.field) {
                        Ok(InstrumentField::AssetClass) => "asset class".to_string(),
                        Ok(InstrumentField::Currency) => "currency".to_string(),
                        Ok(InstrumentField::Description) => "description".to_string(),
                        Ok(InstrumentField::InstrumentType) => "instrument type".to_string(),
                        Ok(InstrumentField::MoneyMarketFund) => "money market fund".to_string(),
                        Ok(InstrumentField::BinaryEventContract) => {
                            "binary event contract".to_string()
                        }
                        _ => change
                            .identifier
                            .as_ref()
                            .map(|id| id.scheme.clone())
                            .unwrap_or_else(|| "identifier".into()),
                    };
                    if change.before.is_empty() {
                        format!("{field}: {} ({})", change.after, change.source)
                    } else if change.after.is_empty() && change.identifier.is_some() {
                        // An identifier that left the record (contract v19).
                        format!("{field}: {} left ({})", change.before, change.source)
                    } else {
                        format!(
                            "{field}: {} to {} ({})",
                            change.before, change.after, change.source
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            let who = if !version.person.is_empty() {
                version.person.clone()
            } else {
                version.instance_id.clone()
            };
            format!(
                "<tr><td>{}</td><td>{}</td><td title=\"{changes}\">{changes}</td><td class=\"wide\">{}</td>\
                 <td class=\"wide\">{}</td></tr>",
                version.version,
                escape(&version.operation),
                escape(&who),
                escape(&version.note),
                changes = escape(&changes),
            )
        })
        .collect();

    format!(
        "<div class=\"admin\"><div class=\"page-head\"><div><h1>{id}</h1>\
         <p class=\"hint\">Version {version}. {book}</p></div>\
         <form method=\"post\" action=\"/admin/instruments/{id}/ask\">{token}\
         <button type=\"submit\">Ask the platform</button></form></div>{asked_html}\
         <nav class=\"tabs\"><a href=\"#says\" data-tab=\"says\">What it says</a>\
         <a href=\"#complete-it\" data-tab=\"complete-it\">Complete it</a>\
         <a href=\"#history\" data-tab=\"history\">History</a></nav>\
         <section class=\"admin-section\" id=\"says\"><div class=\"section-head\"><div><h2>What it says</h2></div></div>{values}</section>\
         <section class=\"admin-section\" id=\"complete-it\"><div class=\"section-head\"><div><h2>Complete it</h2>\
         <p class=\"hint\">Each value says where it came from. A value offered is filled in with its \
         source; change the value and say where the new one came from.</p></div></div>{form}</section>\
         <section class=\"admin-section\" id=\"history\"><div class=\"section-head\"><div><h2>History</h2></div></div>\
         <om-pager><table class=\"list one-line\"><thead><tr><th>Version</th><th>What</th><th>Changes</th><th class=\"wide\">Who</th>\
         <th class=\"wide\">Note</th></tr></thead><tbody>{history}</tbody></table></om-pager></section></div>\
         <script>{}{}{}{FORM_SCRIPT}</script>",
        super::overview::SCRIPT_START,
        super::overview::SCRIPT_END,
        super::data_sources::PAGER_SCRIPT,
        version = record.version,
        book = if item.complete_for_book {
            "The book can use it."
        } else {
            "The book cannot use it until it has an asset class and a currency."
        },
    )
}

/// The platform's ends of identifiers on the record (contract v19, W3.5):
/// each an offer to close an identifier's window on the date the platform
/// says it left, accepted by ticking it, with the offer's source; a note says
/// why, as for any value already held.
fn ends_offered(record: &InstrumentRecord) -> String {
    let boxes: String = record
        .offers
        .iter()
        .filter_map(|offer| match offer.value.as_ref() {
            Some(InstrumentValue {
                value: Some(instrument_value::Value::Identifier(id)),
                source,
            }) if id.valid_until_ns > 0 => Some((id, source)),
            _ => None,
        })
        .map(|(id, source)| {
            let said = serde_json::json!({
                "scheme": id.scheme,
                "namespace": id.source,
                "value": id.value,
                "until": id.valid_until_ns,
                "source": source,
            });
            format!(
                "<label class=\"check\"><input type=\"checkbox\" name=\"end\" value=\"{value}\"> \
                 {words} left {when} ({source})</label>",
                value = escape(&said.to_string()),
                words = escape(&identifier_words(id)),
                when = escape(&instrument_type::utc_minute(id.valid_until_ns)),
                source = escape(source),
            )
        })
        .collect();
    if boxes.is_empty() {
        return String::new();
    }
    format!(
        "<fieldset class=\"ends\" title=\"Ticked, its window here closes on that date; it is \
         kept, never deleted, so a resolve as of a date before still answers this record.\">\
         <legend>Ended by the platform: tick to accept</legend>{boxes}</fieldset>"
    )
}

/// A value changed from what was offered, with the offer's source still
/// beside it, is not sent: the person says where the new value came from.
const FORM_SCRIPT: &str = r##"(function () {
  var form = document.getElementById("complete");
  if (!form) return;
  form.addEventListener("submit", function (event) {
    var pairs = [["asset_class", "asset_class_source"], ["currency", "currency_source"], ["description", "description_source"], ["instrument_type", "instrument_type_source"]];
    for (var i = 0; i < pairs.length; i++) {
      var value = form.elements[pairs[i][0]];
      var source = form.elements[pairs[i][1]];
      var offered = source.getAttribute("data-offered-source");
      if (offered && value.value !== value.getAttribute("data-initial") && source.value === offered) {
        event.preventDefault();
        source.setCustomValidity("You changed the offered value: say where the new one came from.");
        source.reportValidity();
        source.addEventListener("input", function () { this.setCustomValidity(""); }, { once: true });
        return;
      }
    }
  });
})();"##;

/// What a completion's reply says, for the person: done, or each record's
/// refusal in words.
pub fn outcome(reply: &CompleteInstrumentsReply) -> Result<String, String> {
    let refused: Vec<String> = reply
        .results
        .iter()
        .filter(|result| result.refusal.is_some())
        .map(|result| format!("{}: {}", result.instrument_id, result.detail))
        .collect();
    let done = reply.results.len() - refused.len();
    if refused.is_empty() {
        Ok(format!(
            "{done} record{} completed",
            if done == 1 { "" } else { "s" }
        ))
    } else {
        Err(format!(
            "{done} completed; not done: {}",
            refused.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{InstrumentValueSource, LicensedIdentifierCount};

    fn waiting() -> InstrumentToComplete {
        InstrumentToComplete {
            instrument: Some(InstrumentRecord {
                instrument_id: "LCL-1".into(),
                identifiers: vec![Identifier {
                    scheme: "symbol".into(),
                    value: "SNAP1".into(),
                    source: "snaptrade".into(),
                    ..Default::default()
                }],
                version: 2,
                sources: vec![InstrumentValueSource {
                    field: InstrumentField::Identifier as i32,
                    identifier: Some(Identifier {
                        scheme: "symbol".into(),
                        value: "SNAP1".into(),
                        source: "snaptrade".into(),
                        ..Default::default()
                    }),
                    source: "reported by custody-snaptrade-1".into(),
                    instance_id: "custody-snaptrade-1".into(),
                    ..Default::default()
                }],
                offers: vec![OfferedValue {
                    value: Some(InstrumentValue {
                        value: Some(instrument_value::Value::AssetClass(
                            AssetClass::Equity as i32,
                        )),
                        source: "stated by custody-snaptrade-1".into(),
                    }),
                    instance_id: "custody-snaptrade-1".into(),
                    offered_at_ns: 1,
                }],
                ..Default::default()
            }),
            lacks: vec![
                InstrumentField::AssetClass as i32,
                InstrumentField::Currency as i32,
                InstrumentField::Description as i32,
            ],
            complete_for_book: false,
            complete: false,
            ..Default::default()
        }
    }

    #[test]
    fn a_form_becomes_one_completion_against_its_version_each_value_with_its_source() {
        let request = completion(&Filled {
            instrument_id: "LCL-1".into(),
            against_version: "2".into(),
            asset_class: (AssetClass::Equity as i32).to_string(),
            asset_class_source: "Fidelity statement".into(),
            currency: "usd".into(),
            currency_source: "Fidelity statement".into(),
            ..Default::default()
        })
        .unwrap();
        let one = &request.completions[0];
        assert_eq!(one.against_version, 2);
        assert_eq!(one.values.len(), 2);
        assert_eq!(
            one.values[1].value,
            Some(instrument_value::Value::Currency("USD".into()))
        );
        assert!(one
            .values
            .iter()
            .all(|value| value.source == "Fidelity statement"));
        assert!(completion(&Filled {
            instrument_id: "LCL-1".into(),
            against_version: "2".into(),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn a_held_value_posted_back_as_it_was_is_not_sent_again_and_a_new_one_is() {
        // Product owner testing, 2026-10-03: the page pre-fills a held asset
        // class and description, whose sources it may not show; saving only
        // a new currency sends the currency alone.
        let request = completion(&Filled {
            instrument_id: "LCL-1".into(),
            against_version: "3".into(),
            asset_class: (AssetClass::Equity as i32).to_string(),
            asset_class_held: (AssetClass::Equity as i32).to_string(),
            description: "Apple Inc.".into(),
            description_held: "Apple Inc.".into(),
            currency: "usd".into(),
            currency_source: "Fidelity statement".into(),
            ..Default::default()
        })
        .unwrap();
        let values = &request.completions[0].values;
        assert_eq!(values.len(), 1);
        assert_eq!(
            values[0].value,
            Some(instrument_value::Value::Currency("USD".into()))
        );
        // Nothing new at all is nothing to send.
        assert!(completion(&Filled {
            instrument_id: "LCL-1".into(),
            against_version: "3".into(),
            asset_class: (AssetClass::Equity as i32).to_string(),
            asset_class_held: (AssetClass::Equity as i32).to_string(),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn a_funds_attributes_go_with_their_source_none_fee_included_and_round_trip() {
        let page = record_page(&waiting(), &[], None, "");
        assert!(page.contains("name=\"fund_source\""), "{page}");
        assert!(page.contains("name=\"fund_liquidity_fee\""), "{page}");
        assert!(page.contains(&format!(
            "<option value=\"{}\">none</option>",
            LiquidityFeeRegime::None as i32
        )));
        // Held, the record reads as a sentence: a fund charging no liquidity
        // fee is "no liquidity fee", never "none fee".
        let mut holding = waiting();
        if let Some(record) = holding.instrument.as_mut() {
            record.money_market_fund = Some(MoneyMarketFund {
                category: MoneyMarketFundCategory::Government as i32,
                investors: MoneyMarketFundInvestors::Retail as i32,
                nav: MoneyMarketFundNav::Stable as i32,
                liquidity_fee: LiquidityFeeRegime::None as i32,
            });
        }
        let held_page = record_page(&holding, &[], None, "");
        assert!(
            held_page.contains("government, retail, stable NAV, no liquidity fee"),
            "{held_page}"
        );
        assert!(!held_page.contains("none fee"), "{held_page}");
        // Its class and type held, as the form posts them back: the fund's
        // fields are gated on the type (kit 0.11.0).
        let fund = |source: &str| Filled {
            instrument_id: "LCL-1".into(),
            against_version: "2".into(),
            asset_class: (AssetClass::Fund as i32).to_string(),
            asset_class_held: (AssetClass::Fund as i32).to_string(),
            instrument_type: (InstrumentType::MoneyMarketFund as i32).to_string(),
            instrument_type_held: (InstrumentType::MoneyMarketFund as i32).to_string(),
            fund_category: (MoneyMarketFundCategory::Government as i32).to_string(),
            fund_investors: (MoneyMarketFundInvestors::Retail as i32).to_string(),
            fund_nav: (MoneyMarketFundNav::Stable as i32).to_string(),
            fund_liquidity_fee: (LiquidityFeeRegime::None as i32).to_string(),
            fund_source: source.into(),
            ..Default::default()
        };
        assert!(completion(&fund(""))
            .unwrap_err()
            .contains("where the money market fund's attributes came from"));
        let request = completion(&fund("the fund's prospectus")).unwrap();
        let value = &request.completions[0].values[0];
        assert_eq!(value.source, "the fund's prospectus");
        let Some(instrument_value::Value::MoneyMarketFund(held)) = &value.value else {
            panic!("a fund's attributes");
        };
        assert_eq!(held.liquidity_fee, LiquidityFeeRegime::None as i32);
        // Drawn again holding them, posted back unchanged: nothing sent.
        let mut again = fund("");
        again.fund_held = instrument_type::fund_to_text(held);
        assert!(completion(&again).is_err());
        let mut partial = fund("the prospectus");
        partial.fund_nav = String::new();
        assert!(completion(&partial)
            .unwrap_err()
            .contains("stated together"));
    }

    #[test]
    fn accepting_offers_takes_each_lacking_field_offered_with_the_offers_source() {
        let request = accepting(&[waiting()], &["LCL-1".into()]);
        assert_eq!(request.completions.len(), 1);
        let values = &request.completions[0].values;
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].source, "stated by custody-snaptrade-1");
        assert_eq!(request.completions[0].against_version, 2);
        assert!(accepting(&[waiting()], &[]).completions.is_empty());
    }

    #[test]
    fn the_list_counts_and_never_shows_an_account_or_a_quantity() {
        let listed = Ok(ListInstrumentsToCompleteReply {
            instruments: vec![waiting()],
            incomplete_for_book: 1,
            incomplete: 1,
            licensed_identifiers: vec![LicensedIdentifierCount {
                scheme: "cusip".into(),
                count: 512,
            }],
            ..Default::default()
        });
        let page = list_page(&listed, "", "");
        assert!(page.contains("the book cannot use it"));
        assert!(page.contains("threshold"));
        assert!(page.contains("at or past"));
        assert!(page.contains("href=\"/admin/instruments/LCL-1\""));
        assert!(!page.contains("ACC-"));
        let refused = list_page(&Err("no handler".into()), "", "");
        assert!(refused.contains("not answering"));
    }

    #[test]
    fn a_records_page_prefills_what_is_offered_with_its_source() {
        let page = record_page(&waiting(), &[], None, "");
        assert!(page.contains(&format!(
            "<option value=\"{}\" selected>Equity</option>",
            AssetClass::Equity as i32
        )));
        assert!(page.contains("value=\"stated by custody-snaptrade-1\""));
        assert!(page.contains("data-offered-source=\"stated by custody-snaptrade-1\""));
        assert!(page.contains("name=\"against_version\" value=\"2\""));
        assert!(page.contains("reported by custody-snaptrade-1"));
        assert!(page.contains("cannot use it"));
    }

    #[test]
    fn the_home_says_how_many_wait_and_nothing_when_none_do() {
        assert!(home_notice(Some(3)).contains("3 instrument records"));
        assert!(home_notice(Some(1)).contains("1 instrument record the book"));
        assert!(home_notice(Some(0)).is_empty());
        assert!(home_notice(None).is_empty());
    }

    #[test]
    fn fields_gated_on_a_choice_are_read_only_with_it_and_a_contract_goes_with_its_terms() {
        let page = record_page(&waiting(), &[], None, "");
        assert!(
            page.contains("data-om-applies-when=\"instrument_type\""),
            "{page}"
        );
        assert!(page.contains("name=\"contract_amount\""));
        assert!(page.contains(&format!(
            "data-om-one-of=\"{}\">Binary event contract</option>",
            AssetClass::EventContract as i32
        )));
        let contract = |class: AssetClass, source: &str| Filled {
            instrument_id: "LCL-1".into(),
            against_version: "2".into(),
            asset_class: (class as i32).to_string(),
            instrument_type: (InstrumentType::BinaryEventContract as i32).to_string(),
            instrument_type_source: "stated by kalshi-1".into(),
            asset_class_source: "stated by kalshi-1".into(),
            contract_amount: "1".into(),
            contract_asset: "USD".into(),
            contract_closes: "2026-11-01T00:00".into(),
            contract_source: source.into(),
            ..Default::default()
        };
        let request =
            completion(&contract(AssetClass::EventContract, "the venue's terms")).unwrap();
        let values = &request.completions[0].values;
        let Some(instrument_value::Value::BinaryEventContract(terms)) =
            &values.last().unwrap().value
        else {
            panic!("{values:?}");
        };
        assert_eq!(terms.closes_at_ns, 1_793_491_200_000_000_000);
        assert_eq!(terms.payout.as_ref().unwrap().currency_code, "USD");
        assert!(completion(&contract(AssetClass::EventContract, ""))
            .unwrap_err()
            .contains("terms came from"));
        // Under another class, the type and its terms are not read.
        let other = completion(&contract(AssetClass::Equity, "x")).unwrap();
        assert!(other.completions[0].values.iter().all(|v| !matches!(
            v.value,
            Some(instrument_value::Value::BinaryEventContract(_))
                | Some(instrument_value::Value::InstrumentType(_))
        )));
        assert_eq!(utc_field(1_793_491_200_000_000_000), "2026-11-01T00:00");
    }

    #[test]
    fn an_end_the_platform_offered_is_ticked_and_sent_with_its_source() {
        let mut item = waiting();
        if let Some(record) = item.instrument.as_mut() {
            record.offers.push(OfferedValue {
                value: Some(InstrumentValue {
                    value: Some(instrument_value::Value::Identifier(Identifier {
                        scheme: "symbol".into(),
                        value: "SNAP1".into(),
                        source: "snaptrade".into(),
                        valid_until_ns: 1_793_491_200_000_000_000,
                        ..Default::default()
                    })),
                    source: "the platform, record INS-1 version 2".into(),
                }),
                ..Default::default()
            });
            record.identifiers[0].valid_from_ns = 5;
        }
        let page = record_page(&item, &[], None, "");
        assert!(page.contains("Ended by the platform"), "{page}");
        assert!(page.contains("left 2026-11-01 00:00 UTC"), "{page}");
        let end = serde_json::json!({
            "scheme": "symbol", "namespace": "snaptrade", "value": "SNAP1",
            "until": 1_793_491_200_000_000_000_i64, "source": "the platform, record INS-1 version 2",
        })
        .to_string();
        let request = completion(&Filled {
            instrument_id: "LCL-1".into(),
            against_version: "2".into(),
            note: "delisted".into(),
            ends: vec![end],
            ..Default::default()
        })
        .unwrap();
        let value = &request.completions[0].values[0];
        assert_eq!(value.source, "the platform, record INS-1 version 2");
        let Some(instrument_value::Value::Identifier(id)) = &value.value else {
            panic!();
        };
        assert_eq!(id.valid_until_ns, 1_793_491_200_000_000_000);
    }

    #[test]
    fn the_list_pages_and_searches_and_a_resolves_conflict_says_so() {
        let listed = Ok(ListInstrumentsToCompleteReply {
            instruments: vec![waiting()],
            conflicts: vec![meridian_domain::v1::InstrumentConflict {
                identifiers: vec![Identifier {
                    scheme: "figi".into(),
                    value: "BBG1".into(),
                    ..Default::default()
                }],
                instrument_ids: vec!["LCL-1".into(), "LCL-2".into()],
                reason: MissReason::Ambiguous as i32,
                ..Default::default()
            }],
            ..Default::default()
        });
        let page = list_page(&listed, "", "");
        assert!(
            page.contains("<om-pager search=\"Search instruments\">"),
            "{page}"
        );
        assert!(page.contains("<om-pager search=\"Search conflicts\">"));
        assert!(page.contains("data-reason=\"ambiguous\""));
        assert!(page.contains("data-dialog-open=\"merge-0\""));
        assert!(page.contains("<dialog id=\"merge-0\""));
    }
}

#[cfg(test)]
mod provisional_tests {
    use super::*;
    use meridian_domain::v1::{InstrumentValueSource, MigrationCounts, ProvisionalReason};

    const NOW: i64 = 1_790_380_800_000_000_000;
    const DAY: i64 = 86_400_000_000_000;

    fn listed() -> ListInstrumentsToCompleteReply {
        let record = |id: &str, at: i64| InstrumentRecord {
            instrument_id: id.into(),
            description: "ZZ Top Holdings".into(),
            provisional: true,
            template_id: "TPL-equity".into(),
            template_version: "1".into(),
            missing_fields: vec!["currency".into()],
            sources: vec![InstrumentValueSource {
                recorded_at_ns: at,
                ..Default::default()
            }],
            ..Default::default()
        };
        ListInstrumentsToCompleteReply {
            instruments: vec![
                InstrumentToComplete {
                    instrument: Some(record("LCL-ONE", NOW - 3 * DAY)),
                    provisional_reason: ProvisionalReason::AwaitingStaff as i32,
                    ..Default::default()
                },
                InstrumentToComplete {
                    instrument: Some(record("LCL-TWO", NOW - DAY / 2)),
                    provisional_reason: ProvisionalReason::Unreachable as i32,
                    ..Default::default()
                },
            ],
            migration_counts: Some(MigrationCounts {
                matched: 4,
                kept_local: 2,
                remaining: 1,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn the_provisional_records_are_drawn_with_why_their_template_and_age() {
        let (n, tab) = provisional_tab(&listed(), "", NOW);
        assert_eq!(n, 2);
        assert!(
            tab.contains("with the platform&#39;s staff; asked again daily")
                || tab.contains("with the platform's staff; asked again daily"),
            "{tab}"
        );
        assert!(tab.contains("TPL-equity v1"));
        assert!(tab.contains(">currency<"));
        assert!(tab.contains(">3 days<"));
        assert!(tab.contains("Matched: <strong>4</strong>"));
        assert!(tab.contains("Still to send: <strong>1</strong>"));
        let home = home_provisional(Some(&listed()), NOW);
        assert!(home.contains("data-provisional=\"2\""));
        assert!(home.contains("the oldest 3 days"));
        assert!(home_provisional(Some(&ListInstrumentsToCompleteReply::default()), NOW).is_empty());
        // The page carries its tab and the usage report's form.
        let page = list_page_full(
            &Ok(ListInstrumentsToCompleteReply::default()),
            Some(&Ok(listed())),
            Some(&Ok(Default::default())),
            "<t>",
            "",
            "",
            NOW,
        );
        assert!(page.contains("Provisional (2)"));
        assert!(page.contains("action=\"/admin/instruments/usage\""));
        assert!(page.contains("Report identifier usage"));
        assert!(page.contains("None sent yet."));
    }

    /// W3.26 (contract v24; approved 2026-10-15): the exact lines one
    /// change, a duplicate line and a held-back line each their own.
    #[test]
    fn the_lines_are_exact_or_uncertain_and_drawn_as_two_dialogs() {
        use meridian_domain::v1::{InstrumentConflict, MigrationSection as S};
        let line = |id: &str, section: S, answered: &str| InstrumentToComplete {
            instrument: Some(InstrumentRecord {
                instrument_id: id.into(),
                provisional: true,
                ..Default::default()
            }),
            migration_section: section as i32,
            answered_instrument_id: answered.into(),
            ..Default::default()
        };
        let listed = ListInstrumentsToCompleteReply {
            instruments: vec![
                line("LCL-A", S::Matched, "INS-A"),
                line("LCL-B", S::Minted, "INS-B"),
                line("LCL-USD1", S::AlreadyHeld, "INS-USD"),
                line("LCL-USD2", S::Matched, "INS-USD"),
                line("LCL-GBTC", S::HeldBack, "INS-GBTC"),
                line("LCL-C", S::NearMatch, "INS-C"),
                line("LCL-STAFF", S::AwaitingStaff, ""),
                line("LCL-LOCAL", S::KeptLocal, ""),
            ],
            conflicts: vec![InstrumentConflict {
                instrument_ids: vec!["LCL-C".into(), "LCL-OTHER".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let (exact, uncertain) = migration_lines(&listed);
        assert_eq!(exact, vec!["LCL-A".to_string(), "LCL-B".to_string()]);
        let named: Vec<(Vec<String>, &str)> = uncertain
            .iter()
            .map(|line| (line.instrument_ids.clone(), line.answered.as_str()))
            .collect();
        assert!(named.contains(&(vec!["LCL-USD1".into(), "LCL-USD2".into()], "INS-USD")));
        assert!(named.contains(&(vec!["LCL-GBTC".into()], "INS-GBTC")));
        assert!(named.contains(&(vec!["LCL-C".into()], "INS-C")));
        assert_eq!(
            uncertain.len(),
            3,
            "awaiting staff and kept local are no lines"
        );
        let (buttons, dialogs) = lines_controls(&listed, "");
        assert!(buttons.contains("Exact lines (2)") && buttons.contains("Uncertain lines (3)"));
        assert!(dialogs.contains("name=\"line\" value=\"LCL-A\""));
        assert!(dialogs.contains("name=\"hold\" value=\"LCL-B\""));
        assert!(dialogs.contains("value=\"LCL-USD1,LCL-USD2\""));
        assert!(dialogs.contains("action=\"/admin/instruments/migration\""));
        assert!(dialogs.contains("data-prepare"));
    }

    /// Contract v25 (W3.26): a line whose platform answer contradicts what a
    /// person set is uncertain, its own, the person's value shown.
    #[test]
    fn a_platform_value_contradicting_a_persons_is_an_uncertain_line_of_its_own() {
        use meridian_domain::v1::{InstrumentField, MigrationSection as S};
        let listed = ListInstrumentsToCompleteReply {
            instruments: vec![
                InstrumentToComplete {
                    instrument: Some(InstrumentRecord {
                        instrument_id: "LCL-A".into(),
                        provisional: true,
                        ..Default::default()
                    }),
                    migration_section: S::Matched as i32,
                    answered_instrument_id: "INS-A".into(),
                    ..Default::default()
                },
                InstrumentToComplete {
                    instrument: Some(InstrumentRecord {
                        instrument_id: "LCL-GBP".into(),
                        provisional: true,
                        currency: "GBP".into(),
                        ..Default::default()
                    }),
                    migration_section: S::Matched as i32,
                    answered_instrument_id: "INS-GBP".into(),
                    contradicted_fields: vec![InstrumentField::Currency as i32],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (exact, uncertain) = migration_lines(&listed);
        assert_eq!(exact, vec!["LCL-A".to_string()]);
        assert_eq!(uncertain.len(), 1);
        assert_eq!(uncertain[0].instrument_ids, vec!["LCL-GBP".to_string()]);
        assert!(
            uncertain[0].why.contains("currency GBP"),
            "{}",
            uncertain[0].why
        );
    }

    #[test]
    fn more_than_500_exact_lines_are_successive_changes() {
        use meridian_domain::v1::MigrationSection as S;
        let listed = ListInstrumentsToCompleteReply {
            instruments: (0..501)
                .map(|n| InstrumentToComplete {
                    instrument: Some(InstrumentRecord {
                        instrument_id: format!("LCL-{n:04}"),
                        provisional: true,
                        ..Default::default()
                    }),
                    migration_section: S::Matched as i32,
                    answered_instrument_id: format!("INS-{n:04}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let (_, dialogs) = lines_controls(&listed, "");
        assert_eq!(dialogs.matches("name=\"line\"").count(), 500);
        assert!(dialogs.contains("The first 500 of 501"));
        assert!(
            !dialogs.contains("LCL-0500"),
            "the 501st is the next change"
        );
    }

    #[test]
    fn apply_migration_is_prepare_only_and_close_account_may_not_for_agents() {
        use crate::prepared::levels::{default_for, Level};
        assert_eq!(
            default_for("dashboard__apply_migration"),
            Level::PrepareOnly
        );
        assert_eq!(default_for("dashboard__close_account"), Level::MayNot);
        assert!(crate::prepared::CORE_PREPARABLE.contains(&"dashboard__apply_migration"));
        assert!(crate::prepared::CORE_PREPARABLE.contains(&"dashboard__close_account"));
    }

    /// Contract v25 (W3.28): the licensed schemes' form ticks those declared
    /// and offers the licensed schemes a deployment holds, one line; its act
    /// is preparable and at may not for agents by default.
    #[test]
    fn the_licensed_schemes_form_ticks_those_declared() {
        let reports = Ok(meridian_domain::v1::ListUsageReportsReply {
            reports_usage: true,
            licensed_schemes: vec!["cusip".into(), "wkn".into()],
            ..Default::default()
        });
        let form = licensed_form(&reports, "");
        assert!(form.contains(&format!("action=\"{LICENSED_SCHEMES_PATH}\"")));
        assert!(form.contains("value=\"cusip\" checked"));
        assert!(form.contains("value=\"isin\">"));
        assert!(
            form.contains("value=\"wkn\" checked"),
            "a declared one beyond the three"
        );
        assert!(form.contains("data-licensed-schemes=\"cusip,wkn\""));
        assert!(usage_section(&reports, "").contains("id=\"licensed-schemes-form\""));
        assert!(crate::prepared::CORE_PREPARABLE.contains(&"dashboard__set_licensed_schemes"));
        assert_eq!(
            crate::prepared::levels::default_for("dashboard__set_licensed_schemes"),
            crate::prepared::levels::Level::MayNot
        );
    }
}
