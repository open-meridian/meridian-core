//! Core's own tools, the Instruments page's first (W6.20, requirement 17;
//! spec/a-deployment-serves-its-mcp, "Core's tools"): each a transport over
//! a matrix row the dashboard already calls (W3.10 to W3.13, W3.3's ask),
//! with no rule of its own, for a delegation covering the deployment admin's
//! capabilities. Records only, never an account or a quantity, as the page.
//!
//! Arguments are the row's request message's own field names in JSON, so a
//! refusal's path is the row's, prefixed by where it sits in the tool's
//! input: `completions[0].values[1].source`. An argument outside them is
//! refused by name; an enum is its proto name (`ASSET_CLASS_EQUITY`). What
//! the dashboard sends carries the person, the delegation and its client
//! (W4.9's stamp, requirement 18), and a completion or a merge through
//! `/mcp` carries a note, every one (Q10).

use std::time::Duration;

use meridian_bus::Stamp;
use meridian_domain::v1::{
    instrument_value, AskPlatformForInstrumentReply, AskPlatformForInstrumentRequest, AssetClass,
    CompleteInstrumentsReply, CompleteInstrumentsRequest, Identifier, InstrumentCompletion,
    InstrumentField, InstrumentRecord, InstrumentToComplete, InstrumentType, InstrumentValue,
    InstrumentVersion, LiquidityFeeRegime, ListInstrumentsToCompleteReply,
    ListInstrumentsToCompleteRequest, MergeInstrumentsReply, MergeInstrumentsRequest,
    MoneyMarketFund, MoneyMarketFundCategory, MoneyMarketFundInvestors, MoneyMarketFundNav,
    ReadInstrumentHistoryReply, ReadInstrumentHistoryRequest,
};
use meridian_pb::v1::RefusalReason;
use prost::Message;
use serde_json::{json, Map, Value};

use super::{refused, Caller};
use crate::admin::instruments as page;
use crate::web::App;

/// The most completions one call takes, as the row does.
pub const MOST_COMPLETIONS: usize = 500;
/// The longest a note or a source may be.
pub(super) const MOST_TEXT: usize = 2000;

pub use super::{Area, Spec};

pub static SPECS: &[Spec] = &[
    Spec {
        name: "list_instruments_to_complete",
        title: "List the records to complete",
        description: "the deployment's instrument records the book cannot use first (no asset class or no currency), then those lacking a description, a symbol, a money market fund's attributes or a binary event contract's payout and close; each with what it lacks, its identifiers with their windows (one that left the record kept, valid_until_ns set) and who reported them, the values offered, and its version; the conflicts for a merge, each with its reason (MISS_REASON_AMBIGUOUS where a plugin's resolve met several records); the counts and the licensed identifiers per scheme; from contract v24 each record's provisional state, template and missing fields, provisional ones alone with `provisional` and each with its reason, and the migration's and each backfill's counts. Paged by cursor.",
        reads: true,
        open_world: false,
        area: Area::Instruments,
        input_schema: list_schema,
    },
    Spec {
        name: "read_instrument",
        title: "Read an instrument record",
        description: "one record by its ID: each value with its source, the person and the client it was set through, the values offered, what it lacks, and its version, which a completion names.",
        reads: true,
        open_world: false,
        area: Area::Instruments,
        input_schema: one_schema,
    },
    Spec {
        name: "read_instrument_history",
        title: "Read a record's history",
        description: "a record's versions, newest first: each change's field, before and after, its source, the person or reporting instance, the delegation and client, and the note.",
        reads: true,
        open_world: false,
        area: Area::Instruments,
        input_schema: history_schema,
    },
    Spec {
        name: "complete_instruments",
        title: "Complete instrument records",
        description: "set values on up to 500 records, each against the version read: an asset class, an instrument type within it, a money market fund's attributes, a currency, a description, or an identifier added; each value with its source in words (a value without one takes the completion's source), and a note on every completion saying why. Each record's own outcome, in order.",
        reads: false,
        open_world: false,
        area: Area::Instruments,
        input_schema: complete_schema,
    },
    Spec {
        name: "accept_offered_values",
        title: "Accept offered values",
        description: "accept, for each record named against the version read, every value offered for a field it lacks, each value carrying its offer's source, with a note saying why. Each record's own outcome, in order.",
        reads: false,
        open_world: false,
        area: Area::Instruments,
        input_schema: accept_schema,
    },
    Spec {
        name: "merge_instruments",
        title: "Merge two records",
        description: "merge a record into another that is the same security, each named at the version read, with a note; the merged record's identifiers join the one kept, and where both hold a value the kept one's stands unless take_from_merged names the field.",
        reads: false,
        open_world: false,
        area: Area::Instruments,
        input_schema: merge_schema,
    },
    Spec {
        name: "ask_platform_for_instrument",
        title: "Ask the platform about a record",
        description: "ask the platform's open sources about one record by its open identifiers: its values come back as offers on the record, never in force until accepted; or the platform could not be reached.",
        reads: true,
        open_world: true,
        area: Area::Instruments,
        input_schema: one_schema,
    },
    // The Calendars tab's reads (W3.21, contract v22; the calendars spec's
    // Q18): read-only, at the deployment admin's level; nothing sets a
    // calendar in a deployment.
    Spec {
        name: "list_calendars",
        title: "List the calendars held",
        description: "the settlement calendars the deployment holds, one line each at its latest version (its name, kind, subject, version, coverage's last date and when it was last refreshed), and the date rules held (kind, subject, version); filtered by kind and searched by text on the name and the subject's codes (an ISO 4217 code, a venue's MIC or name, a settlement system's name or country), as the Calendars tab is; paged by cursor. A calendar's subject is its currency's cash instrument by its platform INS- ID, the venues that follow it by their VEN- IDs, or its settlement system. Read-only: a calendar is the platform's, pulled as a date read needs it.",
        reads: true,
        open_world: false,
        area: Area::Instruments,
        input_schema: calendars_schema,
    },
    Spec {
        name: "read_calendar",
        title: "Read a calendar",
        description: "one calendar's days over a range, from_date to to_date inclusive (a year at a time, as the tab shows it), as of a recorded time or its latest version: its weekends, each closed weekday and weekend business day with its name and source, its coverage and its sources with the time each was read. A calendar not held is answered found false and asked of the platform.",
        reads: true,
        open_world: false,
        area: Area::Instruments,
        input_schema: calendar_schema,
    },
    // Contract v24 (W3.28, W6.20): the usage statistics' setting.
    Spec {
        name: "set_reports_usage",
        title: "Set the usage report",
        description: "turn \"Report identifier usage\" on or off, with a note saying why (contract v24, W3.28): while on, as it is by default, the conductor sends the platform the deployment's usage statistics weekly -- per identifier scheme, how many records carry it, resolve only through it and missed with it alone -- counts only, never a value, a holding, a quantity or an account. Each change its own record. Answers the setting in force.",
        reads: false,
        open_world: false,
        area: Area::Instruments,
        input_schema: reports_usage_schema,
    },
    // Contract v25 (W3.28, W6.20): the licensed schemes declared,
    // preparable, agents' level "may not" by default.
    Spec {
        name: "set_licensed_schemes",
        title: "Set the licensed schemes",
        description: "declare the identifier schemes the deployment is licensed for, the whole list replacing the last, with a note saying why (contract v25, W3.28): each a scheme the platform's licence list names, refused by its path otherwise; the weekly usage report carries them. Each change its own record. Answers the setting in force with the schemes declared.",
        reads: false,
        open_world: false,
        area: Area::Instruments,
        input_schema: licensed_schemes_schema,
    },
    // Contract v24 (W3.26, W6.20; the names approved 2026-10-15): making the
    // migration's lines, preparable, agents' level "prepare only" by
    // default.
    Spec {
        name: "apply_migration",
        title: "Make the migration's lines",
        description: "make the upgrade's migration lines (contract v24, W3.26), with a note saying why: each LCL- record named in instrument_ids is replaced by the INS- record the platform answered its line with (list_instruments_to_complete with provisional true names it, answered_instrument_id, and its migration_section); each in held_back_ids keeps its LCL- ID, provisional and held back, nothing minted for it, until a later change makes it. The exact lines are one change, each uncertain line its own: two records answered by one INS- ID are a duplicate line, made together. Made together or refused whole, naming each line that is none the platform answered (awaiting its staff, kept local, unreachable) by its path; a line already made is unchanged. At most 500 in each list: more exact lines are successive changes, in order. Answers the migration's counts after.",
        reads: false,
        open_world: false,
        area: Area::Instruments,
        input_schema: apply_migration_schema,
    },
];

fn apply_migration_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "instrument_ids": {"type": "array", "maxItems": 500, "items": {"type": "string", "pattern": "^LCL-"}, "description": "the LCL- records whose lines are made, each replaced by the INS- record its line answered"},
            "held_back_ids": {"type": "array", "maxItems": 500, "items": {"type": "string", "pattern": "^LCL-"}, "description": "the LCL- records whose lines are held back, each keeping its LCL- ID, provisional, nothing minted"},
            "note": {"type": "string", "minLength": 1, "maxLength": 2000, "description": "why, kept on each record's version"},
        },
        "required": ["note"],
        "additionalProperties": false,
    })
}

fn reports_usage_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "reports_usage": {"type": "boolean"},
            "note": {"type": "string", "minLength": 1, "maxLength": MOST_TEXT, "description": "why, kept with the change's record"},
        },
        "required": ["reports_usage", "note"],
        "additionalProperties": false,
    })
}

fn licensed_schemes_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "licensed_schemes": {"type": "array", "items": {"type": "string"}, "maxItems": 64},
            "note": {"type": "string", "minLength": 1, "maxLength": MOST_TEXT, "description": "why, kept with the change's record"},
        },
        "required": ["licensed_schemes", "note"],
        "additionalProperties": false,
    })
}

fn calendars_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cursor": {"type": "string", "description": "next_cursor from the page before"},
            "page_size": {"type": "integer", "minimum": 1, "maximum": 200},
            "kind": enum_names(&[
                "CALENDAR_KIND_CURRENCY",
                "CALENDAR_KIND_TRADING",
                "CALENDAR_KIND_SECURITIES_SETTLEMENT",
            ]),
            "query": {"type": "string", "description": "a text matched, ignoring case, on the name and the subject's codes; absent or empty matches every calendar"},
        },
        "additionalProperties": false,
    })
}

fn calendar_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "calendar_id": {"type": "string"},
            "from_date": {"type": "string", "description": "YYYY-MM-DD"},
            "to_date": {"type": "string", "description": "YYYY-MM-DD, on or after from_date"},
            "as_of_ns": {"type": "integer", "description": "a recorded time; 0 or absent for the latest"},
        },
        "required": ["calendar_id", "from_date", "to_date"],
        "additionalProperties": false,
    })
}

// ── Schemas ─────────────────────────────────────────────────────────────

fn enum_names(names: &[&str]) -> Value {
    json!({"type": "string", "enum": names})
}

fn classes() -> Vec<&'static str> {
    (1..=7)
        .filter_map(|n| AssetClass::try_from(n).ok().map(|c| c.as_str_name()))
        .collect()
}

fn list_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cursor": {"type": "string", "description": "next_cursor from the page before"},
            "page_size": {"type": "integer", "minimum": 1, "maximum": 500},
            "include_complete": {"type": "boolean", "description": "complete records as well"},
            "provisional": {"type": "boolean", "description": "the provisional records alone, complete or not, each with why it is provisional (contract v24)"},
        },
        "additionalProperties": false,
    })
}

fn one_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"instrument_id": {"type": "string"}},
        "required": ["instrument_id"],
        "additionalProperties": false,
    })
}

fn history_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "instrument_id": {"type": "string"},
            "cursor": {"type": "string"},
            "page_size": {"type": "integer", "minimum": 1, "maximum": 200},
        },
        "required": ["instrument_id"],
        "additionalProperties": false,
    })
}

fn value_schema() -> Value {
    json!({
        "type": "object",
        "description": "exactly one value, with its source",
        "properties": {
            "asset_class": enum_names(&classes()),
            "instrument_type": enum_names(&[
                InstrumentType::MoneyMarketFund.as_str_name(),
                InstrumentType::BinaryEventContract.as_str_name(),
            ]),
            "binary_event_contract": {
                "type": "object",
                "description": "a binary event contract's terms: what one contract pays if the event happens, and when trading closes (0 for none stated)",
                "properties": {
                    "payout": {
                        "type": "object",
                        "properties": {
                            "amount": {"type": "string", "description": "a decimal, more than zero"},
                            "currency_code": {"type": "string", "pattern": "^[A-Z]{3}$"},
                            "instrument_id": {"type": "string", "description": "its cash record, for an asset with no ISO 4217 code"},
                        },
                        "required": ["amount"],
                        "additionalProperties": false,
                    },
                    "closes_at_ns": {"type": "integer", "minimum": 0},
                },
                "required": ["payout"],
                "additionalProperties": false,
            },
            "money_market_fund": {
                "type": "object",
                "properties": {
                    "category": enum_names(&["MONEY_MARKET_FUND_CATEGORY_GOVERNMENT", "MONEY_MARKET_FUND_CATEGORY_PRIME", "MONEY_MARKET_FUND_CATEGORY_TAX_EXEMPT"]),
                    "investors": enum_names(&["MONEY_MARKET_FUND_INVESTORS_RETAIL", "MONEY_MARKET_FUND_INVESTORS_INSTITUTIONAL"]),
                    "nav": enum_names(&["MONEY_MARKET_FUND_NAV_STABLE", "MONEY_MARKET_FUND_NAV_FLOATING"]),
                    "liquidity_fee": enum_names(&["LIQUIDITY_FEE_REGIME_MANDATORY", "LIQUIDITY_FEE_REGIME_DISCRETIONARY", "LIQUIDITY_FEE_REGIME_NONE"]),
                },
                "required": ["category", "investors", "nav", "liquidity_fee"],
                "additionalProperties": false,
            },
            "currency": {"type": "string", "pattern": "^[A-Z]{3}$"},
            "description": {"type": "string"},
            "identifier": {
                "type": "object",
                "properties": {"scheme": {"type": "string"}, "value": {"type": "string"}, "source": {"type": "string", "description": "the namespace of a source-scoped symbol"}, "valid_until_ns": {"type": "integer", "minimum": 0, "description": "an end the platform offered, accepted: the identifier's window on the record closes then, never deleted"}},
                "required": ["scheme", "value"],
                "additionalProperties": false,
            },
            "source": {"type": "string", "description": "where the value came from, in words"},
        },
        "additionalProperties": false,
    })
}

fn complete_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "completions": {
                "type": "array",
                "minItems": 1,
                "maxItems": MOST_COMPLETIONS,
                "items": {
                    "type": "object",
                    "properties": {
                        "instrument_id": {"type": "string"},
                        "against_version": {"type": "integer", "description": "the record's version as read"},
                        "values": {"type": "array", "items": value_schema()},
                        "source": {"type": "string", "description": "the source of each value given without its own"},
                        "note": {"type": "string", "description": "why: required through /mcp"},
                    },
                    "required": ["instrument_id", "against_version", "values", "note"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["completions"],
        "additionalProperties": false,
    })
}

fn accept_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "records": {
                "type": "array",
                "minItems": 1,
                "maxItems": MOST_COMPLETIONS,
                "items": {
                    "type": "object",
                    "properties": {
                        "instrument_id": {"type": "string"},
                        "against_version": {"type": "integer"},
                    },
                    "required": ["instrument_id", "against_version"],
                    "additionalProperties": false,
                },
            },
            "note": {"type": "string", "description": "why: required through /mcp"},
        },
        "required": ["records", "note"],
        "additionalProperties": false,
    })
}

fn merge_schema() -> Value {
    let fields = enum_names(&[
        "INSTRUMENT_FIELD_ASSET_CLASS",
        "INSTRUMENT_FIELD_CURRENCY",
        "INSTRUMENT_FIELD_DESCRIPTION",
        "INSTRUMENT_FIELD_INSTRUMENT_TYPE",
        "INSTRUMENT_FIELD_MONEY_MARKET_FUND",
    ]);
    json!({
        "type": "object",
        "properties": {
            "kept_instrument_id": {"type": "string"},
            "kept_version": {"type": "integer"},
            "merged_instrument_id": {"type": "string"},
            "merged_version": {"type": "integer"},
            "take_from_merged": {"type": "array", "items": fields},
            "note": {"type": "string"},
        },
        "required": ["kept_instrument_id", "kept_version", "merged_instrument_id", "merged_version", "note"],
        "additionalProperties": false,
    })
}

// ── Reading arguments, refusing by path ─────────────────────────────────

/// Each field a call's arguments could not give, by its path.
#[derive(Default)]
pub(super) struct Problems(Vec<Value>);

impl Problems {
    pub(super) fn add(&mut self, path: &str, message: impl Into<String>) {
        self.0
            .push(json!({"path": path, "message": message.into()}));
    }
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// The refusal naming each field, at most [`MOST_PROBLEMS`] of them,
    /// and how many more (contract v18; v17's security review, Nit 4): a
    /// megabyte of unknown keys is not a megabyte of refusal.
    pub(super) fn refusal(mut self) -> Value {
        let more = self.0.len().saturating_sub(MOST_PROBLEMS);
        self.0.truncate(MOST_PROBLEMS);
        let mut detail = self
            .0
            .iter()
            .map(|p| {
                format!(
                    "{}: {}",
                    p["path"].as_str().unwrap_or_default(),
                    p["message"].as_str().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        if more > 0 {
            detail.push_str(&format!("; and {more} more not listed"));
        }
        refused("invalid_arguments", &detail, self.0)
    }
}

/// The most fields one refusal names.
pub(super) const MOST_PROBLEMS: usize = 50;

pub(super) fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

/// Refuse every key of `object` not in `known`, by its path.
pub(super) fn only(
    object: &Map<String, Value>,
    known: &[&str],
    path: &str,
    problems: &mut Problems,
) {
    for key in object.keys() {
        if !known.contains(&key.as_str()) {
            problems.add(
                &join(path, key),
                format!("{key:?} is not an input this takes"),
            );
        }
    }
}

pub(super) fn text(
    object: &Map<String, Value>,
    name: &str,
    path: &str,
    required: bool,
    problems: &mut Problems,
) -> String {
    match object.get(name) {
        None | Some(Value::Null) => {
            if required {
                problems.add(&join(path, name), "required");
            }
            String::new()
        }
        Some(Value::String(said)) if said.chars().count() > MOST_TEXT => {
            problems.add(
                &join(path, name),
                format!("longer than {MOST_TEXT} characters"),
            );
            String::new()
        }
        Some(Value::String(said)) => {
            if required && said.trim().is_empty() {
                problems.add(&join(path, name), "required, and empty");
            }
            said.trim().to_string()
        }
        Some(_) => {
            problems.add(&join(path, name), "this is text");
            String::new()
        }
    }
}

pub(super) fn integer(
    object: &Map<String, Value>,
    name: &str,
    path: &str,
    required: bool,
    problems: &mut Problems,
) -> i64 {
    match object.get(name) {
        None | Some(Value::Null) => {
            if required {
                problems.add(&join(path, name), "required");
            }
            0
        }
        Some(Value::Number(n)) if n.is_i64() => n.as_i64().unwrap_or_default(),
        Some(_) => {
            problems.add(&join(path, name), "a whole number, a JSON integer");
            0
        }
    }
}

/// The largest integer a JSON number carries exactly (2^53): an
/// optimistic lock's nanosecond stamp is far above it, so it travels as a
/// decimal string (found 2026-10-14, the build plan's O3).
pub(super) const EXACT_IN_JSON: i64 = 1 << 53;

/// A `*_ns` optimistic-lock field (contract v24's string fix, O3): a decimal
/// string; an integer only where it is at most 2^53, otherwise refused
/// naming the field, since a JSON number above it has lost its digits.
pub(super) fn stamp_ns(
    object: &Map<String, Value>,
    name: &str,
    path: &str,
    required: bool,
    problems: &mut Problems,
) -> i64 {
    let at = join(path, name);
    match object.get(name) {
        None | Some(Value::Null) => {
            if required {
                problems.add(&at, "required");
            }
            0
        }
        Some(Value::String(text)) => match text.trim().parse::<i64>() {
            Ok(stamp) if stamp >= 0 && text.trim().bytes().all(|b| b.is_ascii_digit()) => stamp,
            _ => {
                problems.add(&at, "a decimal string of nanoseconds, as read");
                0
            }
        },
        Some(Value::Number(n)) => match n.as_i64() {
            Some(stamp) if (0..=EXACT_IN_JSON).contains(&stamp) => stamp,
            _ => {
                problems.add(
                    &at,
                    "a decimal string: a JSON number above 2^53 loses its last digits, so send \
                     the stamp as read, as a string",
                );
                0
            }
        },
        Some(_) => {
            problems.add(&at, "a decimal string of nanoseconds, as read");
            0
        }
    }
}

/// A `*_ns` optimistic lock as a tool answers it: a decimal string (O3).
pub fn stamp_said(stamp: i64) -> Value {
    Value::String(stamp.to_string())
}

fn enum_of<T>(
    said: &Value,
    path: &str,
    parse: fn(&str) -> Option<T>,
    problems: &mut Problems,
) -> Option<T> {
    match said.as_str().and_then(parse) {
        Some(value) => Some(value),
        None => {
            problems.add(path, format!("{said} is not one of the values this takes"));
            None
        }
    }
}

pub(super) fn object<'a>(
    said: &'a Value,
    path: &str,
    problems: &mut Problems,
) -> Option<&'a Map<String, Value>> {
    match said.as_object() {
        Some(object) => Some(object),
        None => {
            problems.add(path, "an object");
            None
        }
    }
}

/// One value of a completion, exactly one of the kinds, with its source.
fn value(
    said: &Value,
    path: &str,
    default_source: &str,
    problems: &mut Problems,
) -> Option<InstrumentValue> {
    let object = object(said, path, problems)?;
    const KINDS: [&str; 7] = [
        "asset_class",
        "instrument_type",
        "money_market_fund",
        "binary_event_contract",
        "currency",
        "description",
        "identifier",
    ];
    let mut known: Vec<&str> = KINDS.to_vec();
    known.push("source");
    only(object, &known, path, problems);
    let given: Vec<&str> = KINDS
        .iter()
        .copied()
        .filter(|kind| object.contains_key(*kind))
        .collect();
    if given.len() != 1 {
        problems.add(path, "exactly one of asset_class, instrument_type, money_market_fund, binary_event_contract, currency, description and identifier");
        return None;
    }
    let source = match object.get("source") {
        Some(Value::String(source)) if !source.trim().is_empty() => source.trim().to_string(),
        Some(Value::String(_)) | None | Some(Value::Null) => default_source.to_string(),
        Some(_) => {
            problems.add(&join(path, "source"), "this is text");
            String::new()
        }
    };
    let kind = given[0];
    let at = join(path, kind);
    let held = &object[kind];
    let inner = match kind {
        "asset_class" => instrument_value::Value::AssetClass(enum_of(
            held,
            &at,
            AssetClass::from_str_name,
            problems,
        )? as i32),
        "instrument_type" => instrument_value::Value::InstrumentType(enum_of(
            held,
            &at,
            InstrumentType::from_str_name,
            problems,
        )? as i32),
        "money_market_fund" => {
            let fund = object_fields(held, &at, problems)?;
            only(
                fund,
                &["category", "investors", "nav", "liquidity_fee"],
                &at,
                problems,
            );
            let part = |name: &str| fund.get(name).cloned().unwrap_or(Value::Null);
            instrument_value::Value::MoneyMarketFund(MoneyMarketFund {
                category: enum_of(
                    &part("category"),
                    &join(&at, "category"),
                    MoneyMarketFundCategory::from_str_name,
                    problems,
                )
                .map_or(0, |v| v as i32),
                investors: enum_of(
                    &part("investors"),
                    &join(&at, "investors"),
                    MoneyMarketFundInvestors::from_str_name,
                    problems,
                )
                .map_or(0, |v| v as i32),
                nav: enum_of(
                    &part("nav"),
                    &join(&at, "nav"),
                    MoneyMarketFundNav::from_str_name,
                    problems,
                )
                .map_or(0, |v| v as i32),
                liquidity_fee: enum_of(
                    &part("liquidity_fee"),
                    &join(&at, "liquidity_fee"),
                    LiquidityFeeRegime::from_str_name,
                    problems,
                )
                .map_or(0, |v| v as i32),
            })
        }
        "binary_event_contract" => {
            let terms = object_fields(held, &at, problems)?;
            only(
                terms,
                &["payout", "closes_at_ns", "floor", "cap", "bound_unit"],
                &at,
                problems,
            );
            let payout_at = join(&at, "payout");
            let payout = match terms.get("payout") {
                Some(said) => object_fields(said, &payout_at, problems)?,
                None => {
                    problems.add(&payout_at, "required");
                    return None;
                }
            };
            only(
                payout,
                &["amount", "currency_code", "instrument_id"],
                &payout_at,
                problems,
            );
            let amount_said = text(payout, "amount", &payout_at, true, problems);
            let amount = match amount_said.parse::<meridian_domain::exact::Exact>() {
                Ok(amount) => Some(amount.to_wire()),
                Err(_) => {
                    if !amount_said.is_empty() {
                        problems.add(&join(&payout_at, "amount"), "a decimal, as text");
                    }
                    None
                }
            };
            instrument_value::Value::BinaryEventContract(meridian_domain::v1::BinaryEventContract {
                payout: Some(meridian_domain::v1::Money {
                    amount,
                    currency_code: text(payout, "currency_code", &payout_at, false, problems),
                    instrument_id: text(payout, "instrument_id", &payout_at, false, problems),
                }),
                closes_at_ns: integer(terms, "closes_at_ns", &at, false, problems),
                // A range market's bounds (contract v26, W1.24), each a
                // value as text and whether it is inclusive; either may be
                // absent, for an open tail.
                floor: bound_of(terms, "floor", &at, problems),
                cap: bound_of(terms, "cap", &at, problems),
                bound_unit: text(terms, "bound_unit", &at, false, problems),
            })
        }
        "currency" => match held.as_str() {
            Some(code) => instrument_value::Value::Currency(code.trim().to_string()),
            None => {
                problems.add(&at, "this is text, an ISO 4217 code");
                return None;
            }
        },
        "description" => match held.as_str() {
            Some(said) if said.chars().count() <= MOST_TEXT => {
                instrument_value::Value::Description(said.trim().to_string())
            }
            _ => {
                problems.add(&at, format!("text of at most {MOST_TEXT} characters"));
                return None;
            }
        },
        _ => {
            let id = object_fields(held, &at, problems)?;
            only(
                id,
                &["scheme", "value", "source", "valid_until_ns"],
                &at,
                problems,
            );
            instrument_value::Value::Identifier(Identifier {
                scheme: text(id, "scheme", &at, true, problems).to_ascii_lowercase(),
                value: text(id, "value", &at, true, problems),
                source: text(id, "source", &at, false, problems),
                valid_from_ns: 0,
                valid_until_ns: integer(id, "valid_until_ns", &at, false, problems),
                trading_currency: String::new(),
            })
        }
    };
    Some(InstrumentValue {
        value: Some(inner),
        source,
    })
}

/// A range market's bound as a tool's input spells it (contract v26):
/// `{"value": "70", "inclusive": true}`, or `None` where it is absent.
fn bound_of(
    terms: &Map<String, Value>,
    name: &str,
    path: &str,
    problems: &mut Problems,
) -> Option<meridian_domain::v1::EventContractBound> {
    let at = join(path, name);
    let said = terms.get(name).filter(|said| !said.is_null())?;
    let bound = object_fields(said, &at, problems)?;
    only(bound, &["value", "inclusive"], &at, problems);
    let value_said = text(bound, "value", &at, true, problems);
    let value = match value_said.parse::<meridian_domain::exact::Exact>() {
        Ok(value) => Some(value.to_wire()),
        Err(_) => {
            if !value_said.is_empty() {
                problems.add(&join(&at, "value"), "a decimal, as text");
            }
            None
        }
    };
    let inclusive = match bound.get("inclusive") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(inclusive)) => *inclusive,
        Some(_) => {
            problems.add(&join(&at, "inclusive"), "true or false");
            false
        }
    };
    Some(meridian_domain::v1::EventContractBound { value, inclusive })
}

fn object_fields<'a>(
    said: &'a Value,
    path: &str,
    problems: &mut Problems,
) -> Option<&'a Map<String, Value>> {
    object(said, path, problems)
}

fn completions(arguments: &Value) -> Result<CompleteInstrumentsRequest, Value> {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return Err(problems.refusal());
    };
    only(top, &["completions"], "", &mut problems);
    let listed = match top.get("completions") {
        Some(Value::Array(listed)) if !listed.is_empty() && listed.len() <= MOST_COMPLETIONS => {
            listed.clone()
        }
        Some(Value::Array(listed)) if listed.is_empty() => {
            problems.add("completions", "at least one completion");
            Vec::new()
        }
        Some(Value::Array(_)) => {
            problems.add(
                "completions",
                format!("at most {MOST_COMPLETIONS} completions"),
            );
            Vec::new()
        }
        _ => {
            problems.add("completions", "required: a list of completions");
            Vec::new()
        }
    };
    let mut out = Vec::new();
    for (n, said) in listed.iter().enumerate() {
        let path = format!("completions[{n}]");
        let Some(completion) = object(said, &path, &mut problems) else {
            continue;
        };
        only(
            completion,
            &[
                "instrument_id",
                "against_version",
                "values",
                "source",
                "note",
            ],
            &path,
            &mut problems,
        );
        let instrument_id = text(completion, "instrument_id", &path, true, &mut problems);
        let against_version = integer(completion, "against_version", &path, true, &mut problems);
        let source = text(completion, "source", &path, false, &mut problems);
        // Q10: through /mcp every completion carries a note.
        let note = text(completion, "note", &path, true, &mut problems);
        let mut values = Vec::new();
        match completion.get("values") {
            Some(Value::Array(given)) if !given.is_empty() => {
                for (m, each) in given.iter().enumerate() {
                    let at = format!("{path}.values[{m}]");
                    if let Some(read) = value(each, &at, &source, &mut problems) {
                        if read.source.is_empty() {
                            problems.add(&join(&at, "source"), "required: a value carries its source, or the completion one for all");
                        }
                        values.push(read);
                    }
                }
            }
            _ => problems.add(&join(&path, "values"), "required: at least one value"),
        }
        out.push(InstrumentCompletion {
            instrument_id,
            against_version,
            values,
            note,
        });
    }
    if problems.is_empty() {
        Ok(CompleteInstrumentsRequest { completions: out })
    } else {
        Err(problems.refusal())
    }
}

// ── Answers as JSON ─────────────────────────────────────────────────────

fn enum_name<T: TryFrom<i32>>(value: i32, name: fn(&T) -> &'static str) -> Value {
    match T::try_from(value) {
        Ok(known) if value != 0 => Value::from(name(&known)),
        _ => Value::Null,
    }
}

/// An identifier, with its window on the record where it has one (contract
/// v19): valid_until_ns set on one that left it, kept, never deleted.
fn identifier_json(id: &Identifier) -> Value {
    let mut out = json!({"scheme": id.scheme, "value": id.value, "source": id.source});
    if id.valid_from_ns != 0 {
        out["valid_from_ns"] = id.valid_from_ns.into();
    }
    if id.valid_until_ns != 0 {
        out["valid_until_ns"] = id.valid_until_ns.into();
    }
    out
}

fn value_json(value: &InstrumentValue) -> Value {
    let mut out = match &value.value {
        Some(instrument_value::Value::AssetClass(c)) => {
            json!({"asset_class": enum_name::<AssetClass>(*c, |v| v.as_str_name())})
        }
        Some(instrument_value::Value::Currency(c)) => json!({"currency": c}),
        Some(instrument_value::Value::Description(d)) => json!({"description": d}),
        Some(instrument_value::Value::Identifier(id)) => json!({"identifier": identifier_json(id)}),
        Some(instrument_value::Value::InstrumentType(t)) => {
            json!({"instrument_type": enum_name::<InstrumentType>(*t, |v| v.as_str_name())})
        }
        Some(instrument_value::Value::MoneyMarketFund(f)) => {
            json!({"money_market_fund": fund_json(f)})
        }
        Some(instrument_value::Value::BinaryEventContract(c)) => {
            json!({"binary_event_contract": contract_json(c)})
        }
        None => json!({}),
    };
    out["source"] = value.source.clone().into();
    out
}

fn fund_json(fund: &MoneyMarketFund) -> Value {
    json!({
        "category": enum_name::<MoneyMarketFundCategory>(fund.category, |v| v.as_str_name()),
        "investors": enum_name::<MoneyMarketFundInvestors>(fund.investors, |v| v.as_str_name()),
        "nav": enum_name::<MoneyMarketFundNav>(fund.nav, |v| v.as_str_name()),
        "liquidity_fee": enum_name::<LiquidityFeeRegime>(fund.liquidity_fee, |v| v.as_str_name()),
    })
}

/// A binary event contract's attributes as a tool answers them (contract
/// v19): the payout's amount as text, its asset, and when trading closes.
fn contract_json(contract: &meridian_domain::v1::BinaryEventContract) -> Value {
    let payout = contract.payout.clone().unwrap_or_default();
    json!({
        "payout": {
            "amount": payout
                .amount
                .as_ref()
                .and_then(|d| meridian_domain::exact::Exact::from_wire(d).ok())
                .map(|e| e.to_string())
                .unwrap_or_default(),
            "currency_code": payout.currency_code,
            "instrument_id": payout.instrument_id,
        },
        "closes_at_ns": contract.closes_at_ns,
    })
}

fn field_name(field: i32) -> Value {
    enum_name::<InstrumentField>(field, |v| v.as_str_name())
}

/// A record as a tool answers it: records only, no account, no quantity.
pub fn record_json(record: &InstrumentRecord) -> Value {
    json!({
        "instrument_id": record.instrument_id,
        "version": record.version,
        "asset_class": enum_name::<AssetClass>(record.asset_class, |v| v.as_str_name()),
        "instrument_type": enum_name::<InstrumentType>(record.instrument_type, |v| v.as_str_name()),
        "money_market_fund": record.money_market_fund.as_ref().map(fund_json),
        "binary_event_contract": record.binary_event_contract.as_ref().map(contract_json),
        "currency": record.currency,
        "description": record.description,
        "identifiers": record.identifiers.iter().map(identifier_json).collect::<Vec<_>>(),
        "sources": record.sources.iter().map(|s| json!({
            "field": field_name(s.field),
            "identifier": s.identifier.as_ref().map(identifier_json),
            "source": s.source,
            "person": s.person,
            "acting_through_delegation": s.acting_through_delegation,
            "client_name": s.client_name,
            "instance_id": s.instance_id,
            "recorded_at_ns": s.recorded_at_ns,
            "note": s.note,
        })).collect::<Vec<_>>(),
        "offers": record.offers.iter().map(|o| json!({
            "value": o.value.as_ref().map(value_json),
            "instance_id": o.instance_id,
        })).collect::<Vec<_>>(),
        // Contract v24 (W3.7): provisional until resolved, its template and
        // what of it the record lacks.
        "provisional": record.provisional,
        "template_id": record.template_id,
        "template_version": record.template_version,
        "missing_fields": record.missing_fields,
    })
}

fn to_complete_json(item: &InstrumentToComplete) -> Value {
    let mut out = item
        .instrument
        .as_ref()
        .map(record_json)
        .unwrap_or_else(|| json!({}));
    out["lacks"] = item
        .lacks
        .iter()
        .map(|f| field_name(*f))
        .collect::<Vec<_>>()
        .into();
    out["complete_for_book"] = item.complete_for_book.into();
    out["complete"] = item.complete.into();
    if item.provisional_reason != 0 {
        out["provisional_reason"] =
            enum_name::<meridian_domain::v1::ProvisionalReason>(item.provisional_reason, |v| {
                v.as_str_name()
            });
    }
    // The migration's line (contract v24, W3.26): its section and the record
    // the platform answered it with.
    if item.migration_section != 0 {
        out["migration_section"] =
            enum_name::<meridian_domain::v1::MigrationSection>(item.migration_section, |v| {
                v.as_str_name()
            });
    }
    if !item.answered_instrument_id.is_empty() {
        out["answered_instrument_id"] = item.answered_instrument_id.clone().into();
    }
    // Contract v25 (W3.26): what the platform's answer contradicts of a
    // person's, an uncertain line of its own.
    if !item.contradicted_fields.is_empty() {
        out["contradicted_fields"] = item
            .contradicted_fields
            .iter()
            .map(|field| {
                enum_name::<meridian_domain::v1::InstrumentField>(*field, |v| v.as_str_name())
            })
            .collect::<Vec<_>>()
            .into();
    }
    out
}

fn version_json(version: &InstrumentVersion) -> Value {
    json!({
        "version": version.version,
        "operation": version.operation,
        "changes": version.changes.iter().map(|c| json!({
            "field": field_name(c.field),
            "identifier": c.identifier.as_ref().map(identifier_json),
            "before": c.before,
            "after": c.after,
            "source": c.source,
        })).collect::<Vec<_>>(),
        "person": version.person,
        "acting_through_delegation": version.acting_through_delegation,
        "client_name": version.client_name,
        "instance_id": version.instance_id,
        "note": version.note,
        "record_time_ns": version.record_time_ns,
        "merged_instrument_id": version.merged_instrument_id,
    })
}

// ── Calling ─────────────────────────────────────────────────────────────

/// One row on the bus, for the person through the delegation, stamped
/// beside them (requirement 18).
pub(super) async fn ask<R: Message + Default>(
    app: &App,
    caller: &Caller,
    topic: &str,
    request_type: &str,
    request: impl Message,
    wait: Duration,
) -> Result<R, (String, String, Vec<String>)> {
    let acting = super::Acting::now();
    // A check runs every step but the act's own row (contract v20).
    if acting.checking && super::is_act_row(topic) {
        return Err((
            super::CHECKED.into(),
            "every check passed; nothing was sent, as this was a check".into(),
            Vec::new(),
        ));
    }
    let stamp = Stamp {
        acting_for_subject: caller.subject.clone(),
        acting_through_delegation: caller.delegation_id.clone(),
        acting_through_client: caller.client_name.clone(),
        prepared_call_id: acting.prepared_call_id,
        account_scope: None,
        ..meridian_bus::Stamp::default()
    };
    let answered = app
        .bus
        .call_stamped(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            Some(wait),
            &stamp,
        )
        .await;
    let (_, bytes) = answered.map_err(|failed| match failed {
        // A store's refusal with a status and no code (contract v22): a
        // command it reads as malformed, naming each field, or one outside
        // the scope it was sent with.
        meridian_bus::BusError::HandlerFailed { detail, .. }
            if meridian_bus::read_refusal_status(&detail).is_some() =>
        {
            let (status, fields, words) =
                meridian_bus::read_refusal_status(&detail).expect("matched above");
            (
                match status {
                    meridian_bus::RefusalStatus::InvalidArgument => "invalid_arguments",
                    meridian_bus::RefusalStatus::PermissionDenied => "not_permitted",
                }
                .into(),
                words.to_string(),
                fields,
            )
        }
        meridian_bus::BusError::HandlerFailed { detail, .. } => {
            match meridian_bus::read_refusal(&detail) {
                Some((reason, words)) => (
                    RefusalReason::try_from(reason)
                        .map(|r| r.as_str_name().to_string())
                        .unwrap_or_else(|_| "refused".into()),
                    words.to_string(),
                    meridian_bus::refusal_fields(&detail),
                ),
                None => ("refused".into(), detail, Vec::new()),
            }
        }
        other => ("unavailable".into(), other.to_string(), Vec::new()),
    })?;
    R::decode(&bytes[..]).map_err(|failed| {
        (
            "unavailable".into(),
            format!("the answer did not read: {failed}"),
            Vec::new(),
        )
    })
}

pub(super) fn bus_refused(
    (reason, detail, fields): (String, String, Vec<String>),
    prefix: &str,
) -> Value {
    refused(
        &reason,
        &detail,
        fields
            .into_iter()
            .map(|path| json!({"path": join(prefix, &path)}))
            .collect(),
    )
}

/// The tool, called: its arguments read, the row sent, the answer typed.
pub async fn call(app: &App, caller: &Caller, spec: &Spec, arguments: Value) -> Value {
    match spec.name {
        "list_instruments_to_complete" => list(app, caller, &arguments).await,
        "read_instrument" => read(app, caller, &arguments).await,
        "read_instrument_history" => history(app, caller, &arguments).await,
        "complete_instruments" => match completions(&arguments) {
            Ok(request) => complete(app, caller, request).await,
            Err(refusal) => refusal,
        },
        "accept_offered_values" => accept(app, caller, &arguments).await,
        "merge_instruments" => merge(app, caller, &arguments).await,
        "ask_platform_for_instrument" => ask_platform(app, caller, &arguments).await,
        "list_calendars" => list_calendars(app, caller, &arguments).await,
        "read_calendar" => read_calendar(app, caller, &arguments).await,
        "set_reports_usage" => set_reports_usage(app, caller, &arguments).await,
        "set_licensed_schemes" => set_licensed_schemes(app, caller, &arguments).await,
        "apply_migration" => apply_migration(app, caller, &arguments).await,
        other => refused("not_listed", &format!("no tool {other}"), Vec::new()),
    }
}

const WAIT: Duration = Duration::from_secs(5);
const CHANGING: Duration = Duration::from_secs(20);

async fn list_one(
    app: &App,
    caller: &Caller,
    instrument_id: &str,
) -> Result<Option<InstrumentToComplete>, Value> {
    let reply: ListInstrumentsToCompleteReply = ask(
        app,
        caller,
        page::LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            include_complete: true,
            instrument_id: instrument_id.to_string(),
            page_size: 1,
            cursor: String::new(),
            search: String::new(),
            provisional: false,
        },
        WAIT,
    )
    .await
    .map_err(|failed| bus_refused(failed, ""))?;
    Ok(reply.instruments.into_iter().find(|item| {
        item.instrument
            .as_ref()
            .is_some_and(|r| r.instrument_id == instrument_id)
    }))
}

async fn list(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(
        top,
        &["cursor", "page_size", "include_complete", "provisional"],
        "",
        &mut problems,
    );
    let cursor = text(top, "cursor", "", false, &mut problems);
    let page_size = integer(top, "page_size", "", false, &mut problems);
    if !(0..=500).contains(&page_size) {
        problems.add("page_size", "from 1 to 500");
    }
    let include_complete = match top.get("include_complete") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            problems.add("include_complete", "true or false");
            false
        }
    };
    let provisional = match top.get("provisional") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            problems.add("provisional", "true or false");
            false
        }
    };
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<ListInstrumentsToCompleteReply, _> = ask(
        app,
        caller,
        page::LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            search: Default::default(),
            include_complete,
            instrument_id: String::new(),
            page_size: if page_size == 0 {
                100
            } else {
                page_size as i32
            },
            cursor,
            provisional,
        },
        WAIT,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => json!({
            "outcome": "unchanged",
            "data": {
                "records": reply.instruments.iter().map(to_complete_json).collect::<Vec<_>>(),
                "next_cursor": reply.next_cursor,
                "conflicts": reply.conflicts.iter().map(|c| json!({
                    "identifiers": c.identifiers.iter().map(identifier_json).collect::<Vec<_>>(),
                    "instrument_ids": c.instrument_ids,
                    "reported_by": c.reported_by,
                    "reason": enum_name::<meridian_domain::v1::MissReason>(c.reason, |v| v.as_str_name()),
                })).collect::<Vec<_>>(),
                "incomplete_for_book": reply.incomplete_for_book,
                "incomplete": reply.incomplete,
                "licensed_identifiers": reply.licensed_identifiers.iter().map(|l| json!({"scheme": l.scheme, "count": l.count})).collect::<Vec<_>>(),
                // Contract v24 (W3.11): the migration's and each backfill's
                // counts, counts only.
                "migration_counts": reply.migration_counts.as_ref().map(|m| json!({
                    "already_held": m.already_held,
                    "matched": m.matched,
                    "minted": m.minted,
                    "near_match": m.near_match,
                    "awaiting_staff": m.awaiting_staff,
                    "kept_local": m.kept_local,
                    "unreachable": m.unreachable,
                    "held_back": m.held_back,
                    "remaining": m.remaining,
                })),
                "backfill_counts": reply.backfill_counts.iter().map(|b| json!({
                    "scheme": b.scheme,
                    "matched": b.matched,
                    "awaiting_person": b.awaiting_person,
                    "remaining": b.remaining,
                })).collect::<Vec<_>>(),
            },
        }),
    }
}

fn one_id(arguments: &Value) -> Result<String, Value> {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return Err(problems.refusal());
    };
    only(top, &["instrument_id"], "", &mut problems);
    let id = text(top, "instrument_id", "", true, &mut problems);
    if problems.is_empty() {
        Ok(id)
    } else {
        Err(problems.refusal())
    }
}

async fn read(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let id = match one_id(arguments) {
        Ok(id) => id,
        Err(refusal) => return refusal,
    };
    match list_one(app, caller, &id).await {
        Err(refusal) => refusal,
        Ok(None) => refused(
            "REFUSAL_REASON_UNKNOWN_INSTRUMENT",
            &format!("no record {id} in this deployment"),
            vec![json!({"path": "instrument_id"})],
        ),
        Ok(Some(item)) => json!({"outcome": "unchanged", "data": to_complete_json(&item)}),
    }
}

async fn history(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(
        top,
        &["instrument_id", "cursor", "page_size"],
        "",
        &mut problems,
    );
    let instrument_id = text(top, "instrument_id", "", true, &mut problems);
    let cursor = text(top, "cursor", "", false, &mut problems);
    let page_size = integer(top, "page_size", "", false, &mut problems);
    if !(0..=200).contains(&page_size) {
        problems.add("page_size", "from 1 to 200");
    }
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<ReadInstrumentHistoryReply, _> = ask(
        app,
        caller,
        page::READ_INSTRUMENT_HISTORY,
        "meridian.v1.ReadInstrumentHistoryRequest",
        ReadInstrumentHistoryRequest {
            instrument_id,
            page_size: if page_size == 0 { 50 } else { page_size as i32 },
            cursor,
        },
        WAIT,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => json!({
            "outcome": "unchanged",
            "data": {
                "versions": reply.versions.iter().map(version_json).collect::<Vec<_>>(),
                "next_cursor": reply.next_cursor,
            },
        }),
    }
}

/// W3.28 (contract v24): the usage report's setting, sent to the conductor.
async fn set_reports_usage(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(top, &["reports_usage", "note"], "", &mut problems);
    let on = match top.get("reports_usage") {
        Some(Value::Bool(on)) => *on,
        None | Some(Value::Null) => {
            problems.add("reports_usage", "required");
            false
        }
        Some(_) => {
            problems.add("reports_usage", "true or false");
            false
        }
    };
    let reason = text(top, "note", "", true, &mut problems);
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<meridian_domain::v1::ReportsUsageSetting, _> = ask(
        app,
        caller,
        page::SET_REPORTS_USAGE,
        "meridian.v1.SetReportsUsageRequest",
        meridian_domain::v1::SetReportsUsageRequest {
            reports_usage: on,
            reason,
        },
        WAIT,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => json!({"outcome": "made", "data": {"reports_usage": reply.reports_usage}}),
    }
}

/// W3.28 (contract v25): the licensed schemes declared, sent to the
/// conductor, which holds them to the licence list.
async fn set_licensed_schemes(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(top, &["licensed_schemes", "note"], "", &mut problems);
    let mut schemes = Vec::new();
    match top.get("licensed_schemes") {
        Some(Value::Array(items)) => {
            if items.len() > 64 {
                problems.add("licensed_schemes", "at most 64");
            }
            for (n, item) in items.iter().enumerate() {
                match item.as_str() {
                    Some(scheme) => schemes.push(scheme.to_string()),
                    None => problems.add(&format!("licensed_schemes[{n}]"), "a scheme's name"),
                }
            }
        }
        None | Some(Value::Null) => problems.add("licensed_schemes", "required: the whole list"),
        Some(_) => problems.add("licensed_schemes", "a list of schemes"),
    }
    let reason = text(top, "note", "", true, &mut problems);
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<meridian_domain::v1::ReportsUsageSetting, _> = ask(
        app,
        caller,
        page::SET_LICENSED_SCHEMES,
        "meridian.v1.SetLicensedSchemesRequest",
        meridian_domain::v1::SetLicensedSchemesRequest {
            licensed_schemes: schemes,
            reason,
        },
        WAIT,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => json!({"outcome": "made", "data": {
            "reports_usage": reply.reports_usage,
            "licensed_schemes": reply.licensed_schemes,
        }}),
    }
}

/// The ids a list names, each by its path where it is not text.
fn ids(top: &serde_json::Map<String, Value>, name: &str, problems: &mut Problems) -> Vec<String> {
    match top.get(name) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            if items.len() > 500 {
                problems.add(name, "at most 500");
            }
            items
                .iter()
                .enumerate()
                .filter_map(|(n, item)| match item.as_str() {
                    Some(id) if !id.trim().is_empty() => Some(id.trim().to_string()),
                    _ => {
                        problems.add(&format!("{name}[{n}]"), "a record's ID");
                        None
                    }
                })
                .collect()
        }
        Some(_) => {
            problems.add(name, "a list of records' IDs");
            Vec::new()
        }
    }
}

/// W3.26 (contract v24): the migration's lines made, a deployment admin's.
async fn apply_migration(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(
        top,
        &["instrument_ids", "held_back_ids", "note"],
        "",
        &mut problems,
    );
    let request = meridian_domain::v1::ApplyInstrumentMigrationRequest {
        instrument_ids: ids(top, "instrument_ids", &mut problems),
        held_back_ids: ids(top, "held_back_ids", &mut problems),
        note: text(top, "note", "", true, &mut problems),
    };
    if request.instrument_ids.is_empty() && request.held_back_ids.is_empty() {
        problems.add("instrument_ids", "a line to make or hold back");
    }
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<meridian_domain::v1::MigrationCounts, _> = ask(
        app,
        caller,
        page::APPLY_INSTRUMENT_MIGRATION,
        "meridian.v1.ApplyInstrumentMigrationRequest",
        request,
        CHANGING,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(counts) => {
            json!({"outcome": "made", "data": {"migration_counts": page::migration_counts_json(&counts)}})
        }
    }
}

/// W3.21: the calendars and rules held, a page at a time.
async fn list_calendars(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(
        top,
        &["cursor", "page_size", "kind", "query"],
        "",
        &mut problems,
    );
    let cursor = text(top, "cursor", "", false, &mut problems);
    let page_size = integer(top, "page_size", "", false, &mut problems);
    if !(0..=200).contains(&page_size) {
        problems.add("page_size", "from 1 to 200");
    }
    let kind = match top.get("kind") {
        None | Some(Value::Null) => 0,
        Some(said) => match enum_of(
            said,
            "kind",
            meridian_domain::v1::CalendarKind::from_str_name,
            &mut problems,
        ) {
            Some(meridian_domain::v1::CalendarKind::Unspecified) => {
                problems.add("kind", "names a kind; leave it out for every kind");
                0
            }
            Some(kind) => kind as i32,
            None => 0,
        },
    };
    let query = text(top, "query", "", false, &mut problems);
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<meridian_domain::v1::ListCalendarsReply, _> = ask(
        app,
        caller,
        crate::admin::calendars::LIST_CALENDARS,
        "meridian.v1.ListCalendarsRequest",
        meridian_domain::v1::ListCalendarsRequest {
            page_size: page_size as i32,
            cursor,
            kind,
            query,
        },
        WAIT,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => json!({
            "outcome": "unchanged",
            "data": {
                "calendars": reply.calendars.iter().map(|calendar| json!({
                    "calendar_id": calendar.calendar_id,
                    "name": calendar.name,
                    "kind": meridian_domain::v1::CalendarKind::try_from(calendar.kind)
                        .map(|kind| kind.as_str_name()).unwrap_or_default(),
                    "version": calendar.version,
                    "coverage_to_date": calendar.coverage_to_date,
                    "refreshed_at_ns": calendar.refreshed_at_ns,
                    "instrument_id": calendar.instrument_id,
                    "venue_ids": calendar.venue_ids,
                    "settlement_system": calendar.settlement_system.as_ref().map(|system| json!({
                        "name": system.name,
                        "country_code": system.country_code,
                    })),
                })).collect::<Vec<_>>(),
                "rules": reply.rules.iter().map(|rule| json!({
                    "kind": meridian_pb::v1::DateRuleKind::try_from(rule.kind)
                        .map(|kind| kind.as_str_name()).unwrap_or_default(),
                    "subject": rule.subject,
                    "version": rule.version,
                    "refreshed_at_ns": rule.refreshed_at_ns,
                })).collect::<Vec<_>>(),
                "next_cursor": reply.next_cursor,
            },
        }),
    }
}

/// W3.19: one calendar's days over a range.
async fn read_calendar(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(
        top,
        &["calendar_id", "from_date", "to_date", "as_of_ns"],
        "",
        &mut problems,
    );
    let calendar_id = text(top, "calendar_id", "", true, &mut problems);
    let from_date = text(top, "from_date", "", true, &mut problems);
    let to_date = text(top, "to_date", "", true, &mut problems);
    let as_of_ns = integer(top, "as_of_ns", "", false, &mut problems);
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<meridian_domain::v1::ReadCalendarReply, _> = ask(
        app,
        caller,
        crate::admin::calendars::READ_CALENDAR,
        "meridian.v1.ReadCalendarRequest",
        meridian_domain::v1::ReadCalendarRequest {
            calendar_id,
            from_date,
            to_date,
            as_of_ns,
        },
        WAIT,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => json!({
            "outcome": "unchanged",
            "data": {
                "found": reply.found,
                "calendar_id": reply.calendar_id,
                "version": reply.version,
                "weekends": reply.weekends.iter().map(|weekend| json!({
                    "weekdays": weekend.weekdays.iter().map(|day| {
                        meridian_domain::v1::Weekday::try_from(*day)
                            .map(|day| day.as_str_name()).unwrap_or_default()
                    }).collect::<Vec<_>>(),
                    "valid_from_date": weekend.valid_from_date,
                })).collect::<Vec<_>>(),
                "days": reply.days.iter().map(|day| json!({
                    "date": day.date,
                    "kind": meridian_domain::v1::CalendarDayKind::try_from(day.kind)
                        .map(|kind| kind.as_str_name()).unwrap_or_default(),
                    "name": day.name,
                    "source": day.source,
                })).collect::<Vec<_>>(),
                "coverage": reply.coverage.as_ref().map(|coverage| json!({
                    "from_date": coverage.from_date,
                    "to_date": coverage.to_date,
                })),
                "sources": reply.sources.iter().map(|source| json!({
                    "source_id": source.source_id,
                    "name": source.name,
                    "address": source.address,
                    "read_at_ns": source.read_at_ns,
                })).collect::<Vec<_>>(),
            },
        }),
    }
}

/// W3.10, each record's own outcome in order: made at a new version,
/// unchanged where nothing it said differed, or refused naming its fields
/// by their path in the tool's input.
async fn complete(app: &App, caller: &Caller, request: CompleteInstrumentsRequest) -> Value {
    let asked: Vec<(String, i64)> = request
        .completions
        .iter()
        .map(|c| (c.instrument_id.clone(), c.against_version))
        .collect();
    let reply: Result<CompleteInstrumentsReply, _> = ask(
        app,
        caller,
        page::COMPLETE_INSTRUMENTS,
        "meridian.v1.CompleteInstrumentsRequest",
        request,
        CHANGING,
    )
    .await;
    let reply = match reply {
        Ok(reply) => reply,
        Err(failed) => return bus_refused(failed, ""),
    };
    results(&asked, &reply, "completions")
}

fn results(asked: &[(String, i64)], reply: &CompleteInstrumentsReply, under: &str) -> Value {
    let mut made = 0;
    let mut refusals = 0;
    let rows: Vec<Value> = reply
        .results
        .iter()
        .enumerate()
        .map(|(n, result)| {
            let path = format!("{under}[{n}]");
            if let Some(refusal) = &result.refusal {
                refusals += 1;
                return json!({
                    "instrument_id": result.instrument_id,
                    "outcome": "refused",
                    "reason": RefusalReason::try_from(refusal.reason).map(|r| r.as_str_name()).unwrap_or("refused"),
                    "fields": refusal.fields.iter().map(|f| json!({"path": join(&path, f)})).collect::<Vec<_>>(),
                    "detail": result.detail,
                });
            }
            let version = result.instrument.as_ref().map(|r| r.version).unwrap_or_default();
            let before = asked.get(n).map(|(_, v)| *v).unwrap_or_default();
            let outcome = if version > before { "made" } else { "unchanged" };
            if outcome == "made" {
                made += 1;
            }
            json!({
                "instrument_id": result.instrument_id,
                "outcome": outcome,
                "version": version,
                "record": result.instrument.as_ref().map(record_json),
            })
        })
        .collect();
    let outcome = if refusals == rows.len() && !rows.is_empty() {
        "refused"
    } else if made > 0 {
        "made"
    } else {
        "unchanged"
    };
    let mut out = json!({"outcome": outcome, "results": rows});
    if outcome == "refused" {
        out["reason"] = "every_record_refused".into();
        out["detail"] = "Every record was refused; each says why.".into();
    }
    out
}

async fn accept(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(top, &["records", "note"], "", &mut problems);
    let note = text(top, "note", "", true, &mut problems);
    let mut wanted = Vec::new();
    match top.get("records") {
        Some(Value::Array(records)) if !records.is_empty() && records.len() <= MOST_COMPLETIONS => {
            for (n, said) in records.iter().enumerate() {
                let path = format!("records[{n}]");
                if let Some(record) = object(said, &path, &mut problems) {
                    only(
                        record,
                        &["instrument_id", "against_version"],
                        &path,
                        &mut problems,
                    );
                    wanted.push((
                        text(record, "instrument_id", &path, true, &mut problems),
                        integer(record, "against_version", &path, true, &mut problems),
                    ));
                }
            }
        }
        _ => problems.add(
            "records",
            format!("required: from 1 to {MOST_COMPLETIONS} records"),
        ),
    }
    if !problems.is_empty() {
        return problems.refusal();
    }
    let mut listed = Vec::new();
    for (n, (id, _)) in wanted.iter().enumerate() {
        match list_one(app, caller, id).await {
            Err(refusal) => return refusal,
            Ok(None) => problems.add(
                &format!("records[{n}].instrument_id"),
                format!("no record {id} in this deployment"),
            ),
            Ok(Some(item)) => listed.push(item),
        }
    }
    if !problems.is_empty() {
        return problems.refusal();
    }
    let chosen: Vec<String> = wanted.iter().map(|(id, _)| id.clone()).collect();
    let mut request = page::accepting(&listed, &chosen);
    // Against the version the caller read, not the one just listed: a record
    // changed since is refused as the page refuses it.
    for completion in &mut request.completions {
        if let Some((_, version)) = wanted
            .iter()
            .find(|(id, _)| *id == completion.instrument_id)
        {
            completion.against_version = *version;
        }
        completion.note = note.clone();
    }
    if request.completions.is_empty() {
        return json!({"outcome": "unchanged", "results": [], "detail": "Nothing offered for what those records lack."});
    }
    let asked: Vec<(String, i64)> = request
        .completions
        .iter()
        .map(|c| (c.instrument_id.clone(), c.against_version))
        .collect();
    let reply: Result<CompleteInstrumentsReply, _> = ask(
        app,
        caller,
        page::COMPLETE_INSTRUMENTS,
        "meridian.v1.CompleteInstrumentsRequest",
        request,
        CHANGING,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => results(&asked, &reply, "records"),
    }
}

async fn merge(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(
        top,
        &[
            "kept_instrument_id",
            "kept_version",
            "merged_instrument_id",
            "merged_version",
            "take_from_merged",
            "note",
        ],
        "",
        &mut problems,
    );
    let mut take = Vec::new();
    if let Some(given) = top.get("take_from_merged") {
        match given.as_array() {
            Some(fields) => {
                for (n, field) in fields.iter().enumerate() {
                    if let Some(read) = enum_of(
                        field,
                        &format!("take_from_merged[{n}]"),
                        InstrumentField::from_str_name,
                        &mut problems,
                    ) {
                        take.push(read as i32);
                    }
                }
            }
            None => problems.add("take_from_merged", "a list of fields"),
        }
    }
    let request = MergeInstrumentsRequest {
        kept_instrument_id: text(top, "kept_instrument_id", "", true, &mut problems),
        kept_version: integer(top, "kept_version", "", true, &mut problems),
        merged_instrument_id: text(top, "merged_instrument_id", "", true, &mut problems),
        merged_version: integer(top, "merged_version", "", true, &mut problems),
        take_from_merged: take,
        // Q10: through /mcp a merge carries a note, as the row asks anyway.
        note: text(top, "note", "", true, &mut problems),
    };
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<MergeInstrumentsReply, _> = ask(
        app,
        caller,
        page::MERGE_INSTRUMENTS,
        "meridian.v1.MergeInstrumentsRequest",
        request,
        CHANGING,
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => {
            json!({"outcome": "made", "data": {"record": reply.instrument.as_ref().map(record_json)}})
        }
    }
}

async fn ask_platform(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let id = match one_id(arguments) {
        Ok(id) => id,
        Err(refusal) => return refusal,
    };
    let record = match list_one(app, caller, &id).await {
        Err(refusal) => return refusal,
        Ok(None) => {
            return refused(
                "REFUSAL_REASON_UNKNOWN_INSTRUMENT",
                &format!("no record {id} in this deployment"),
                vec![json!({"path": "instrument_id"})],
            )
        }
        Ok(Some(item)) => item.instrument.unwrap_or_default(),
    };
    let reply: Result<AskPlatformForInstrumentReply, _> = ask(
        app,
        caller,
        page::ASK_PLATFORM_FOR_INSTRUMENT,
        "meridian.v1.AskPlatformForInstrumentRequest",
        AskPlatformForInstrumentRequest {
            instrument_id: record.instrument_id.clone(),
            identifiers: record.identifiers.clone(),
            as_of_ns: 0,
            match_or_mint: None,
        },
        Duration::from_secs(12),
    )
    .await;
    match reply {
        Err(failed) => bus_refused(failed, ""),
        Ok(reply) => json!({
            "outcome": "unchanged",
            "data": {
                "reachable": reply.reachable,
                "found": reply.found,
                "offered": reply.instrument.as_ref().map(record_json),
                "detail": reply.detail,
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_completion_reads_from_the_rows_own_names_each_value_with_its_source() {
        let request = completions(&json!({"completions": [{
            "instrument_id": "LCL-1", "against_version": 1,
            "values": [{"asset_class": "ASSET_CLASS_EQUITY"}, {"currency": "USD", "source": "ISO 4217"}],
            "source": "Fidelity statement", "note": "From the statement.",
        }]}))
        .expect("reads");
        let completion = &request.completions[0];
        assert_eq!(completion.against_version, 1);
        assert_eq!(completion.values[0].source, "Fidelity statement");
        assert_eq!(completion.values[1].source, "ISO 4217");
        assert_eq!(completion.note, "From the statement.");
    }

    #[test]
    fn a_completion_without_a_note_or_with_an_unknown_argument_is_refused_by_path() {
        let refusal = completions(&json!({"completions": [{
            "instrument_id": "LCL-1", "against_version": 1,
            "values": [{"asset_class": "EQUITY", "source": "s", "price": "1"}],
            "colour": "red",
        }]}))
        .expect_err("refused");
        let paths: Vec<&str> = refusal["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"completions[0].colour"), "{paths:?}");
        assert!(paths.contains(&"completions[0].note"), "{paths:?}");
        assert!(
            paths.contains(&"completions[0].values[0].price"),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"completions[0].values[0].asset_class"),
            "{paths:?}"
        );
        assert_eq!(refusal["outcome"], "refused");
    }

    #[test]
    fn a_value_without_a_source_where_the_completion_gives_none_is_refused() {
        let refusal = completions(&json!({"completions": [{
            "instrument_id": "LCL-1", "against_version": 1,
            "values": [{"currency": "USD"}], "note": "n",
        }]}))
        .expect_err("refused");
        assert_eq!(
            refusal["fields"][0]["path"],
            "completions[0].values[0].source"
        );
    }

    #[test]
    fn a_reply_says_each_records_outcome_in_order() {
        let reply = CompleteInstrumentsReply {
            results: vec![
                meridian_domain::v1::InstrumentCompletionResult {
                    instrument_id: "A".into(),
                    instrument: Some(InstrumentRecord {
                        instrument_id: "A".into(),
                        version: 2,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                meridian_domain::v1::InstrumentCompletionResult {
                    instrument_id: "B".into(),
                    refusal: Some(meridian_pb::v1::Refusal {
                        reason: RefusalReason::RecordChanged as i32,
                        fields: vec!["against_version".into()],
                    }),
                    detail: "changed".into(),
                    ..Default::default()
                },
            ],
        };
        let answer = results(&[("A".into(), 1), ("B".into(), 1)], &reply, "completions");
        assert_eq!(answer["outcome"], "made");
        assert_eq!(answer["results"][0]["outcome"], "made");
        assert_eq!(answer["results"][1]["outcome"], "refused");
        assert_eq!(
            answer["results"][1]["fields"][0]["path"],
            "completions[1].against_version"
        );
    }

    #[test]
    fn a_binary_event_contract_and_an_accepted_end_are_read_by_the_tool() {
        let request = completions(&json!({"completions": [{
            "instrument_id": "LCL-1",
            "against_version": 1,
            "note": "the venue's terms",
            "values": [
                {"instrument_type": "INSTRUMENT_TYPE_BINARY_EVENT_CONTRACT", "source": "stated by kalshi-1"},
                {"binary_event_contract": {"payout": {"amount": "1", "currency_code": "USD"}, "closes_at_ns": 5}, "source": "the venue's terms"},
                {"identifier": {"scheme": "symbol", "value": "ZZ", "source": "kalshi", "valid_until_ns": 7}, "source": "the platform"},
            ],
        }]}))
        .unwrap();
        let values = &request.completions[0].values;
        let Some(instrument_value::Value::BinaryEventContract(terms)) = &values[1].value else {
            panic!("{values:?}");
        };
        assert_eq!(terms.closes_at_ns, 5);
        assert_eq!(terms.payout.as_ref().unwrap().currency_code, "USD");
        let Some(instrument_value::Value::Identifier(id)) = &values[2].value else {
            panic!();
        };
        assert_eq!(id.valid_until_ns, 7);
        let refusal = completions(&json!({"completions": [{
            "instrument_id": "LCL-1", "against_version": 1, "note": "n",
            "values": [{"binary_event_contract": {"payout": {"amount": "one"}}, "source": "s"}],
        }]}))
        .unwrap_err();
        assert!(refusal.to_string().contains("payout.amount"), "{refusal}");
        let record = record_json(&InstrumentRecord {
            identifiers: vec![Identifier {
                scheme: "symbol".into(),
                value: "ZZ".into(),
                valid_until_ns: 7,
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(record["identifiers"][0]["valid_until_ns"], 7);
    }
}
