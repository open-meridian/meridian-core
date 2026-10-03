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
const MOST_TEXT: usize = 2000;

/// One of core's tools.
#[derive(Debug)]
pub struct Spec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub reads: bool,
    pub open_world: bool,
    pub input_schema: fn() -> Value,
}

pub static SPECS: &[Spec] = &[
    Spec {
        name: "list_instruments_to_complete",
        title: "List the records to complete",
        description: "the deployment's instrument records the book cannot use first (no asset class or no currency), then those lacking a description, a symbol or a money market fund's attributes; each with what it lacks, its identifiers and who reported them, the values offered, and its version; the conflicts for a merge; the counts and the licensed identifiers per scheme. Paged by cursor.",
        reads: true,
        open_world: false,
        input_schema: list_schema,
    },
    Spec {
        name: "read_instrument",
        title: "Read an instrument record",
        description: "one record by its ID: each value with its source, the person and the client it was set through, the values offered, what it lacks, and its version, which a completion names.",
        reads: true,
        open_world: false,
        input_schema: one_schema,
    },
    Spec {
        name: "read_instrument_history",
        title: "Read a record's history",
        description: "a record's versions, newest first: each change's field, before and after, its source, the person or reporting instance, the delegation and client, and the note.",
        reads: true,
        open_world: false,
        input_schema: history_schema,
    },
    Spec {
        name: "complete_instruments",
        title: "Complete instrument records",
        description: "set values on up to 500 records, each against the version read: an asset class, an instrument type within it, a money market fund's attributes, a currency, a description, or an identifier added; each value with its source in words (a value without one takes the completion's source), and a note on every completion saying why. Each record's own outcome, in order.",
        reads: false,
        open_world: false,
        input_schema: complete_schema,
    },
    Spec {
        name: "accept_offered_values",
        title: "Accept offered values",
        description: "accept, for each record named against the version read, every value offered for a field it lacks, each value carrying its offer's source, with a note saying why. Each record's own outcome, in order.",
        reads: false,
        open_world: false,
        input_schema: accept_schema,
    },
    Spec {
        name: "merge_instruments",
        title: "Merge two records",
        description: "merge a record into another that is the same security, each named at the version read, with a note; the merged record's identifiers join the one kept, and where both hold a value the kept one's stands unless take_from_merged names the field.",
        reads: false,
        open_world: false,
        input_schema: merge_schema,
    },
    Spec {
        name: "ask_platform_for_instrument",
        title: "Ask the platform about a record",
        description: "ask the platform's open sources about one record by its open identifiers: its values come back as offers on the record, never in force until accepted; or the platform could not be reached.",
        reads: true,
        open_world: true,
        input_schema: one_schema,
    },
];

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
            "instrument_type": enum_names(&[InstrumentType::MoneyMarketFund.as_str_name()]),
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
                "properties": {"scheme": {"type": "string"}, "value": {"type": "string"}, "source": {"type": "string", "description": "the namespace of a source-scoped symbol"}},
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
struct Problems(Vec<Value>);

impl Problems {
    fn add(&mut self, path: &str, message: impl Into<String>) {
        self.0
            .push(json!({"path": path, "message": message.into()}));
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    fn refusal(self) -> Value {
        let detail = self
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
        refused("invalid_arguments", &detail, self.0)
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

/// Refuse every key of `object` not in `known`, by its path.
fn only(object: &Map<String, Value>, known: &[&str], path: &str, problems: &mut Problems) {
    for key in object.keys() {
        if !known.contains(&key.as_str()) {
            problems.add(
                &join(path, key),
                format!("{key:?} is not an input this takes"),
            );
        }
    }
}

fn text(
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

fn integer(
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

fn object<'a>(
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
    const KINDS: [&str; 6] = [
        "asset_class",
        "instrument_type",
        "money_market_fund",
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
        problems.add(path, "exactly one of asset_class, instrument_type, money_market_fund, currency, description and identifier");
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
            only(id, &["scheme", "value", "source"], &at, problems);
            instrument_value::Value::Identifier(Identifier {
                scheme: text(id, "scheme", &at, true, problems).to_ascii_lowercase(),
                value: text(id, "value", &at, true, problems),
                source: text(id, "source", &at, false, problems),
            })
        }
    };
    Some(InstrumentValue {
        value: Some(inner),
        source,
    })
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

fn identifier_json(id: &Identifier) -> Value {
    json!({"scheme": id.scheme, "value": id.value, "source": id.source})
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
async fn ask<R: Message + Default>(
    app: &App,
    caller: &Caller,
    topic: &str,
    request_type: &str,
    request: impl Message,
    wait: Duration,
) -> Result<R, (String, String, Vec<String>)> {
    let stamp = Stamp {
        acting_for_subject: caller.subject.clone(),
        acting_through_delegation: caller.delegation_id.clone(),
        acting_through_client: caller.client_name.clone(),
        account_scope: None,
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

fn bus_refused((reason, detail, fields): (String, String, Vec<String>), prefix: &str) -> Value {
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
        &["cursor", "page_size", "include_complete"],
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
    if !problems.is_empty() {
        return problems.refusal();
    }
    let reply: Result<ListInstrumentsToCompleteReply, _> = ask(
        app,
        caller,
        page::LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            include_complete,
            instrument_id: String::new(),
            page_size: if page_size == 0 {
                100
            } else {
                page_size as i32
            },
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
                "records": reply.instruments.iter().map(to_complete_json).collect::<Vec<_>>(),
                "next_cursor": reply.next_cursor,
                "conflicts": reply.conflicts.iter().map(|c| json!({
                    "identifiers": c.identifiers.iter().map(identifier_json).collect::<Vec<_>>(),
                    "instrument_ids": c.instrument_ids,
                    "reported_by": c.reported_by,
                })).collect::<Vec<_>>(),
                "incomplete_for_book": reply.incomplete_for_book,
                "incomplete": reply.incomplete,
                "licensed_identifiers": reply.licensed_identifiers.iter().map(|l| json!({"scheme": l.scheme, "count": l.count})).collect::<Vec<_>>(),
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
}
