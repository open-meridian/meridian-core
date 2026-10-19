//! A job's inputs, written out: the **inputs document** the answer's hash
//! is taken over (Q7), the same shape as the SDK reference case's `inputs`
//! (`meridian.analytics.reference`), so a pinned case and a job the
//! component resolved from the stores are read alike.
//!
//! Every number is a string in its stated form and every 64-bit integer a
//! string, as protobuf's JSON writes one; only small whole numbers
//! (`model_version`, a measure's `version`) are JSON numbers.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::exact::Dec;

/// Why an inputs document cannot be answered at all: a malformed document,
/// or one naming something it holds no record of. Never a figure's reason,
/// which is a `NotEvaluated`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseError(pub String);

impl std::fmt::Display for CaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CaseError {}

pub type Result<T> = std::result::Result<T, CaseError>;

fn error(words: impl Into<String>) -> CaseError {
    CaseError(words.into())
}

/// A member's text, "" where absent.
fn text(value: &Value, name: &str) -> String {
    match value.get(name) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// A non-empty decimal string.
pub fn decimal(value: Option<&Value>, what: &str) -> Result<Dec> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => {
            Dec::parse(text).map_err(|words| error(format!("{what}: {words}")))
        }
        other => Err(error(format!(
            "{what}: a number in an inputs document is a non-empty string, not {other:?}"
        ))),
    }
}

/// An integer written as a string (or a small number), 0 where absent.
fn integer(value: Option<&Value>, what: &str) -> Result<i64> {
    match value {
        None | Some(Value::Null) => Ok(0),
        Some(Value::String(text)) if text.is_empty() => Ok(0),
        Some(Value::String(text)) => text
            .parse()
            .map_err(|_| error(format!("{what}: {text:?} is not an integer"))),
        Some(Value::Number(number)) => number
            .as_i64()
            .ok_or_else(|| error(format!("{what}: {number} is not an integer"))),
        Some(other) => Err(error(format!("{what}: {other} is not an integer"))),
    }
}

fn items<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

#[derive(Debug, Clone)]
pub struct Price {
    pub subject_id: String,
    pub kind: String,
    pub amount: Dec,
    pub instrument_id: String,
    pub dataset: String,
    pub valid_at_ns: i64,
}

#[derive(Debug, Clone)]
pub struct Instrument {
    pub instrument_id: String,
    pub record_version: String,
    pub template_id: String,
    pub template_version: String,
    pub calculation_version: String,
    pub model_id: String,
    pub model_version: i64,
    pub asset_class: String,
    pub currency_instrument_id: String,
    pub price_kind: String,
    pub payout: Option<(Dec, String)>,
    pub country: String,
}

#[derive(Debug, Clone)]
pub struct Position {
    pub account_id: String,
    pub instrument_id: String,
    pub quantity: Dec,
    pub cost: Option<Dec>,
    pub sleeve: String,
}

#[derive(Debug, Clone)]
pub struct ClosedLot {
    pub account_id: String,
    pub instrument_id: String,
    pub quantity: Dec,
    pub proceeds: Dec,
    pub cost: Dec,
    pub sleeve: String,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub event_group_id: String,
    pub version: String,
    pub members: Vec<String>,
    pub mutually_exclusive: String,
    pub exhaustive: String,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub instrument_id: String,
    pub final_: bool,
    pub settlement_value: Dec,
    pub dataset: String,
}

/// A measure as the answer names it back: definition, version, event group
/// and breakdowns, so two asks of one definition stay apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MeasureKey {
    pub definition: String,
    pub version: i64,
    pub event_group_id: String,
    pub breakdowns: Vec<String>,
}

impl MeasureKey {
    pub fn from_json(value: &Value) -> Result<MeasureKey> {
        Ok(MeasureKey {
            definition: text(value, "definition"),
            version: match value.get("version") {
                None => 1,
                some => integer(some, "a measure's version")?,
            },
            event_group_id: text(value, "event_group_id"),
            breakdowns: items(value, "breakdowns")
                .iter()
                .map(|b| b.as_str().unwrap_or_default().to_string())
                .collect(),
        })
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "definition": self.definition,
            "version": self.version,
            "breakdowns": self.breakdowns,
            "event_group_id": self.event_group_id,
        })
    }
}

#[derive(Debug, Clone)]
pub struct WhatIfTrade {
    pub instrument_id: String,
    pub side: String,
    pub quantity: Dec,
    pub price: Option<(Dec, String)>,
}

#[derive(Debug, Clone)]
pub struct WhatIf {
    pub account_id: String,
    pub trades: Vec<WhatIfTrade>,
}

/// An inputs document, read once.
#[derive(Debug, Clone)]
pub struct Case {
    pub engine_version: String,
    pub account_ids: Vec<String>,
    pub measures: Vec<MeasureKey>,
    pub what_if: Option<WhatIf>,
    pub lake_as_of_ns: i64,
    pub routing_version: String,
    pub book_watermark: Value,
    pub bases: BTreeMap<String, String>,
    pub instruments: BTreeMap<String, Instrument>,
    pub positions: Vec<Position>,
    pub closed_lots: Vec<ClosedLot>,
    /// By subject and kind, in the document's order.
    pub market: BTreeMap<(String, String), Vec<Price>>,
    pub dates: Vec<String>,
    /// By subject and business date; a later row for the same pair wins.
    pub closes: BTreeMap<(String, String), Dec>,
    pub outcomes: BTreeMap<String, Outcome>,
    pub groups: BTreeMap<String, Group>,
    pub stale_after: BTreeMap<String, i64>,
}

impl Case {
    pub fn from_json(inputs: &Value) -> Result<Case> {
        let job = inputs
            .get("job")
            .ok_or_else(|| error("the inputs name no job"))?;
        let what_if = match job.get("what_if") {
            None | Some(Value::Null) => None,
            Some(what_if) => Some(WhatIf {
                account_id: text(what_if, "account_id"),
                trades: items(what_if, "trades")
                    .iter()
                    .map(|trade| {
                        Ok(WhatIfTrade {
                            instrument_id: text(trade, "instrument_id"),
                            side: text(trade, "side"),
                            quantity: decimal(trade.get("quantity"), "a trade's quantity")?,
                            price: match trade.get("price") {
                                None | Some(Value::Null) => None,
                                Some(price) => Some((
                                    decimal(price.get("amount"), "a trade's price")?,
                                    text(price, "instrument_id"),
                                )),
                            },
                        })
                    })
                    .collect::<Result<_>>()?,
            }),
        };
        let mut market: BTreeMap<(String, String), Vec<Price>> = BTreeMap::new();
        for row in items(inputs, "market") {
            let price = Price {
                subject_id: text(row, "subject_id"),
                kind: text(row, "kind"),
                amount: decimal(row.get("amount"), "a market row's amount")?,
                instrument_id: text(row, "instrument_id"),
                dataset: text(row, "dataset"),
                valid_at_ns: integer(row.get("valid_at_ns"), "valid_at_ns")?,
            };
            market
                .entry((price.subject_id.clone(), price.kind.clone()))
                .or_default()
                .push(price);
        }
        let history = inputs.get("history").cloned().unwrap_or(Value::Null);
        let mut closes = BTreeMap::new();
        for close in items(&history, "closes") {
            closes.insert(
                (text(close, "subject_id"), text(close, "business_date")),
                decimal(close.get("amount"), "a close's amount")?,
            );
        }
        let mut stale_after = BTreeMap::new();
        if let Some(Value::Object(limits)) = inputs.get("stale_after_ns") {
            for (dataset, limit) in limits {
                stale_after.insert(dataset.clone(), integer(Some(limit), "stale_after_ns")?);
            }
        }
        Ok(Case {
            engine_version: text(inputs, "engine_version"),
            account_ids: items(job, "account_ids")
                .iter()
                .map(|a| a.as_str().unwrap_or_default().to_string())
                .collect(),
            measures: items(job, "measures")
                .iter()
                .map(MeasureKey::from_json)
                .collect::<Result<_>>()?,
            what_if,
            lake_as_of_ns: integer(inputs.get("lake_as_of_ns"), "lake_as_of_ns")?,
            routing_version: match inputs.get("routing_version") {
                None => "0".to_string(),
                Some(_) => text(inputs, "routing_version"),
            },
            book_watermark: inputs
                .get("book_watermark")
                .cloned()
                .filter(|w| !w.is_null())
                .unwrap_or(Value::Array(Vec::new())),
            bases: items(inputs, "accounts")
                .iter()
                .map(|a| {
                    (
                        text(a, "account_id"),
                        text(a, "base_currency_instrument_id"),
                    )
                })
                .collect(),
            instruments: items(inputs, "instruments")
                .iter()
                .map(|i| {
                    let payout = match i.get("payout") {
                        None | Some(Value::Null) => None,
                        Some(payout) => Some((
                            decimal(payout.get("amount"), "a payout's amount")?,
                            text(payout, "instrument_id"),
                        )),
                    };
                    let record = Instrument {
                        instrument_id: text(i, "instrument_id"),
                        record_version: text(i, "record_version"),
                        template_id: text(i, "template_id"),
                        template_version: text(i, "template_version"),
                        calculation_version: text(i, "calculation_version"),
                        model_id: text(i, "model_id"),
                        model_version: match i.get("model_version") {
                            None => 1,
                            some => integer(some, "model_version")?,
                        },
                        asset_class: text(i, "asset_class"),
                        currency_instrument_id: text(i, "currency_instrument_id"),
                        price_kind: text(i, "price_kind"),
                        payout,
                        country: text(i, "country"),
                    };
                    Ok((record.instrument_id.clone(), record))
                })
                .collect::<Result<_>>()?,
            positions: items(inputs, "positions")
                .iter()
                .map(|p| {
                    Ok(Position {
                        account_id: text(p, "account_id"),
                        instrument_id: text(p, "instrument_id"),
                        quantity: decimal(p.get("quantity"), "a position's quantity")?,
                        cost: match p.get("cost") {
                            Some(Value::String(cost)) if !cost.is_empty() => {
                                Some(decimal(p.get("cost"), "a position's cost")?)
                            }
                            _ => None,
                        },
                        sleeve: text(p, "sleeve"),
                    })
                })
                .collect::<Result<_>>()?,
            closed_lots: items(inputs, "closed_lots")
                .iter()
                .map(|c| {
                    Ok(ClosedLot {
                        account_id: text(c, "account_id"),
                        instrument_id: text(c, "instrument_id"),
                        quantity: decimal(c.get("quantity"), "a closed lot's quantity")?,
                        proceeds: decimal(c.get("proceeds"), "a closed lot's proceeds")?,
                        cost: decimal(c.get("cost"), "a closed lot's cost")?,
                        sleeve: text(c, "sleeve"),
                    })
                })
                .collect::<Result<_>>()?,
            market,
            dates: items(&history, "dates")
                .iter()
                .map(|d| d.as_str().unwrap_or_default().to_string())
                .collect(),
            closes,
            outcomes: items(inputs, "outcomes")
                .iter()
                .map(|o| {
                    Ok((
                        text(o, "instrument_id"),
                        Outcome {
                            instrument_id: text(o, "instrument_id"),
                            final_: o.get("final").and_then(Value::as_bool).unwrap_or(false),
                            settlement_value: decimal(
                                o.get("settlement_value"),
                                "an outcome's settlement value",
                            )?,
                            dataset: text(o, "dataset"),
                        },
                    ))
                })
                .collect::<Result<_>>()?,
            groups: items(inputs, "event_groups")
                .iter()
                .map(|g| {
                    (
                        text(g, "event_group_id"),
                        Group {
                            event_group_id: text(g, "event_group_id"),
                            version: text(g, "version"),
                            members: items(g, "member_instrument_ids")
                                .iter()
                                .map(|m| m.as_str().unwrap_or_default().to_string())
                                .collect(),
                            mutually_exclusive: text(g, "mutually_exclusive"),
                            exhaustive: text(g, "exhaustive"),
                        },
                    )
                })
                .collect(),
            stale_after,
        })
    }

    /// The account's base currency's cash instrument.
    pub fn base(&self, account: &str) -> Result<&str> {
        self.bases
            .get(account)
            .map(String::as_str)
            .ok_or_else(|| error(format!("the inputs hold no base currency for {account}")))
    }

    /// The price of one subject and kind at the as-of: the latest valid
    /// where the document states more than one (by valid time, then
    /// dataset; the first of equals).
    pub fn price(&self, subject: &str, kind: &str) -> Option<&Price> {
        let held = self.market.get(&(subject.to_string(), kind.to_string()))?;
        let mut best: Option<&Price> = None;
        for price in held {
            match best {
                Some(b) if (price.valid_at_ns, &price.dataset) <= (b.valid_at_ns, &b.dataset) => {}
                _ => best = Some(price),
            }
        }
        best
    }

    /// Whether the price is older than its dataset's staleness setting
    /// allows; a dataset not named is never stale.
    pub fn stale(&self, price: &Price) -> bool {
        self.stale_after
            .get(&price.dataset)
            .is_some_and(|limit| self.lake_as_of_ns - price.valid_at_ns > *limit)
    }
}
