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
    /// Contract v27 (ruling 460): the lake row it came from -- its key and
    /// version -- and its source time, the valid time where none is stated.
    pub row_key: String,
    pub version: i64,
    pub source_ns: i64,
    /// Contract v29 (W10.4; ruling 984): its price basis where not per unit
    /// (`PRICE_BASIS_PERCENT_OF_PAR`, `_YIELD`, `_SPREAD`); empty per unit.
    pub basis: String,
    /// Contract v29: the yield beside it, where the source states one.
    pub quoted_yield: Option<QuotedYield>,
    /// Contract v29: a spread in basis points over its benchmark.
    pub spread_bps: Option<Dec>,
    pub benchmark_instrument_id: String,
}

/// Contract v29 (W10.4): a yield as its source quotes it -- percent, its
/// kind after FIX YieldType (235), and the date a yield to call or put is
/// to.
#[derive(Debug, Clone)]
pub struct QuotedYield {
    pub value: Dec,
    pub kind: String,
    pub to_date: String,
}

/// Contract v29 (W10.4, W10.6): a pool's or a class's factor row.
#[derive(Debug, Clone)]
pub struct FactorRow {
    pub factor: Dec,
    pub factor_month: String,
    pub dataset: String,
    pub row_key: String,
    pub version: i64,
    pub source_ns: i64,
    /// Contract v29 (995): the agency's monthly disclosure values, where
    /// stated: the weighted average coupon (percent a year), remaining
    /// maturity and loan age (months).
    pub wac: Option<Dec>,
    pub wam: Option<u32>,
    pub wala: Option<u32>,
}

/// Contract v29 (W12.2): a par curve's points at a date, as the lake holds
/// them -- Treasury's par yield curve, or H.15's constant maturities on the
/// same record -- or why none was read.
#[derive(Debug, Clone, Default)]
pub struct ParCurve {
    pub subject_id: String,
    pub dataset: String,
    pub date: String,
    /// Each point's tenor label, count, unit and par yield in percent.
    pub points: Vec<(String, i32, String, Dec)>,
    pub source_ns: i64,
    pub missing: String,
    pub missing_reason: String,
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
    /// Contract v27 (ruling 462): its ISO 4217 code, a fiat currency's cash
    /// instrument, whose legs alone are inverted or crossed; empty for a
    /// token.
    pub currency_code: String,
    /// Contract v29 (W1.1, W1.24): a debt record's terms and the fields it
    /// lacks; None for any other record.
    pub debt: Option<crate::bonds::Held>,
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
    /// None where the book records no cost (contract v27, ruling 325).
    pub cost: Option<Dec>,
    pub sleeve: String,
}

/// An execution `execution_cost` measures (contract v27, W12.1, W10.11).
#[derive(Debug, Clone)]
pub struct Execution {
    pub execution_id: String,
    pub order_id: String,
    pub placement_id: String,
    pub account_id: String,
    pub instrument_id: String,
    pub side: String,
    pub price: Dec,
    pub price_instrument_id: String,
    pub quantity: Dec,
    /// Each benchmark's moment, 0 where it has none.
    pub decision_at_ns: i64,
    pub arrival_at_ns: i64,
    pub placement_arrival_at_ns: i64,
    pub executed_at_ns: i64,
    pub fees: Option<Dec>,
    pub financing: Option<Dec>,
    pub liquidity_indicator: String,
    pub venue_id: String,
    pub time_precision: String,
    pub reject_count: i64,
    pub time_to_fill_ns: i64,
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

/// An amount `{amount, instrument_id}`, or None for null or absent.
fn optional_amount(value: Option<&Value>, what: &str) -> Result<Option<Dec>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(held) => Ok(Some(decimal(held.get("amount"), what)?)),
    }
}

fn execution(e: &Value) -> Result<Execution> {
    let price = e.get("price").unwrap_or(&Value::Null);
    Ok(Execution {
        execution_id: text(e, "execution_id"),
        order_id: text(e, "order_id"),
        placement_id: text(e, "placement_id"),
        account_id: text(e, "account_id"),
        instrument_id: text(e, "instrument_id"),
        side: text(e, "side"),
        price: decimal(price.get("amount"), "an execution's price")?,
        price_instrument_id: text(price, "instrument_id"),
        quantity: decimal(e.get("quantity"), "an execution's quantity")?,
        decision_at_ns: integer(e.get("decision_at_ns"), "decision_at_ns")?,
        arrival_at_ns: integer(e.get("arrival_at_ns"), "arrival_at_ns")?,
        placement_arrival_at_ns: integer(
            e.get("placement_arrival_at_ns"),
            "placement_arrival_at_ns",
        )?,
        executed_at_ns: integer(e.get("executed_at_ns"), "executed_at_ns")?,
        fees: optional_amount(e.get("fees"), "an execution's fees")?,
        financing: optional_amount(e.get("financing"), "an execution's financing")?,
        liquidity_indicator: text(e, "liquidity_indicator"),
        venue_id: text(e, "venue_id"),
        time_precision: text(e, "time_precision"),
        reject_count: integer(e.get("reject_count"), "reject_count")?,
        time_to_fill_ns: integer(e.get("time_to_fill_ns"), "time_to_fill_ns")?,
    })
}

/// A market row as the document carries it; a quoted yield's row has no
/// amount (`with_amount` false), its amount 0.
fn market_price(row: &Value, with_amount: bool) -> Result<Price> {
    let valid_at_ns = integer(row.get("valid_at_ns"), "valid_at_ns")?;
    let source_ns = integer(row.get("source_ns"), "source_ns")?;
    let price = Price {
        subject_id: text(row, "subject_id"),
        kind: text(row, "kind"),
        amount: if with_amount {
            decimal(row.get("amount"), "a market row's amount")?
        } else {
            Dec::zero()
        },
        instrument_id: text(row, "instrument_id"),
        dataset: text(row, "dataset"),
        valid_at_ns,
        row_key: text(row, "row_key"),
        version: integer(row.get("version"), "version")?,
        source_ns: if source_ns == 0 {
            valid_at_ns
        } else {
            source_ns
        },
        basis: text(row, "basis"),
        quoted_yield: quoted_yield(row.get("yield"))?,
        spread_bps: match row.get("spread_bps") {
            None | Some(Value::Null) => None,
            some => Some(decimal(some, "a market row's spread_bps")?),
        },
        benchmark_instrument_id: text(row, "benchmark_instrument_id"),
    };
    Ok(price)
}

/// A quoted yield as a row carries it, or None.
fn quoted_yield(value: Option<&Value>) -> Result<Option<QuotedYield>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(y) => Ok(Some(QuotedYield {
            value: decimal(y.get("value"), "a quoted yield's value")?,
            kind: text(y, "kind"),
            to_date: text(y, "to_date"),
        })),
    }
}

/// A whole number of months, where stated.
fn months(value: Option<&Value>, what: &str) -> Result<Option<u32>> {
    match value {
        Some(Value::Null) | None => Ok(None),
        held => u32::try_from(integer(held, what)?)
            .map(Some)
            .map_err(|_| error(format!("{what} is a whole number of months"))),
    }
}

/// Each subject's factor rows, by factor month (the latest version of a
/// month kept).
fn factor_rows(inputs: &Value) -> Result<BTreeMap<String, Vec<FactorRow>>> {
    let mut by: BTreeMap<String, BTreeMap<String, FactorRow>> = BTreeMap::new();
    for f in items(inputs, "factors") {
        let row = FactorRow {
            factor: decimal(f.get("factor"), "a factor")?,
            factor_month: text(f, "factor_month"),
            dataset: text(f, "dataset"),
            row_key: text(f, "row_key"),
            version: integer(f.get("version"), "a factor's version")?,
            source_ns: integer(f.get("source_ns"), "a factor's source_ns")?,
            wac: match f.get("wac") {
                Some(Value::Null) | None => None,
                held => Some(decimal(held, "a factor's wac")?),
            },
            wam: months(f.get("wam"), "a factor's wam")?,
            wala: months(f.get("wala"), "a factor's wala")?,
        };
        let months = by.entry(text(f, "subject_id")).or_default();
        let newer = months
            .get(&row.factor_month)
            .is_none_or(|held| row.version >= held.version);
        if newer {
            months.insert(row.factor_month.clone(), row);
        }
    }
    Ok(by
        .into_iter()
        .map(|(subject, months)| (subject, months.into_values().collect()))
        .collect())
}

/// A par curve as the document carries it.
fn par_curve(c: &Value) -> Result<ParCurve> {
    Ok(ParCurve {
        subject_id: text(c, "subject_id"),
        dataset: text(c, "dataset"),
        date: text(c, "date"),
        points: items(c, "points")
            .iter()
            .map(|p| {
                Ok((
                    text(p, "tenor"),
                    integer(p.get("count"), "a tenor's count")? as i32,
                    text(p, "unit"),
                    decimal(p.get("par_yield"), "a par yield")?,
                ))
            })
            .collect::<Result<_>>()?,
        source_ns: integer(c.get("source_ns"), "a curve's source_ns")?,
        missing: text(c, "missing"),
        missing_reason: text(c, "missing_reason"),
    })
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
    /// By subject, then business date; a later row for the same pair wins.
    pub closes: BTreeMap<String, BTreeMap<String, Dec>>,
    pub outcomes: BTreeMap<String, Outcome>,
    pub groups: BTreeMap<String, Group>,
    pub stale_after: BTreeMap<String, i64>,
    /// Prices a dataset the client is not entitled to would have answered,
    /// by subject and kind, naming the dataset (requirement 6; Q11):
    /// `unentitled`, absent in the reference's cases.
    pub unentitled: BTreeMap<(String, String), String>,
    /// Contract v28 (W12.2; rulings 421 and 619): inputs whose only row in
    /// force is invalid, by subject and kind, naming the dataset:
    /// `quarantined`, absent where none is.
    pub quarantined: BTreeMap<(String, String), String>,
    /// Model ids a deployment admin routed to a `pricing` plugin (W12.6),
    /// each with its implementation, its version and whether it answered:
    /// `routed`, absent where every model id is built in.
    pub routed: BTreeMap<String, Routed>,
    /// What a routed plugin answered, by instrument and the base it valued
    /// it in: `priced`, absent where nothing is routed.
    pub priced: BTreeMap<(String, String), Dec>,
    /// Contract v27 (rulings 458, 459, 462 and 483): the job's business
    /// date and reporting currency, and the deployment's pivot order.
    pub business_date: String,
    pub reporting_currency: String,
    pub pivots: Vec<String>,
    /// Contract v27 (W12.1): the executions `execution_cost` measures, and
    /// its quotes' staleness bound by default.
    pub executions: Vec<Execution>,
    pub tca_staleness_bound_ns: i64,
    /// Contract v28 (W12.1): the chains the job names, resolved, and the
    /// scenario set its points are generated from (W12.8).
    pub chains: Vec<crate::chains::Chain>,
    pub scenario: Option<crate::scenario::Set>,
    /// Contract v29 (W12.1, W12.2): the job's date -- its business date,
    /// else the as-of's date in New York -- which a debt measure is as of;
    /// each pool's and class's factor rows by subject, in factor-month
    /// order; the par curves read, by subject, and the Treasury curve's
    /// subject; and the payment calendars' holidays.
    pub valuation_date: String,
    pub factors: BTreeMap<String, Vec<FactorRow>>,
    pub curves: BTreeMap<String, ParCurve>,
    pub treasury_curve: Option<ParCurve>,
    pub calendars: BTreeMap<String, crate::bonds::dates::Calendar>,
    /// Contract v29 (W10.4): rows in yield with no money price (a
    /// when-issued trade before its auction), by subject; their amount 0.
    pub quoted_yields: BTreeMap<String, Vec<Price>>,
}

/// A model id routed to a `pricing` plugin, for one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub implementation: String,
    pub implementation_version: String,
    /// False where the plugin did not answer the job's batch by its
    /// deadline: every figure through it is then not evaluated, never the
    /// built-in model's in silence (requirement 22).
    pub answered: bool,
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
            let price = market_price(row, true)?;
            market
                .entry((price.subject_id.clone(), price.kind.clone()))
                .or_default()
                .push(price);
        }
        let mut quoted_yields: BTreeMap<String, Vec<Price>> = BTreeMap::new();
        for row in items(inputs, "quoted_yields") {
            let price = market_price(row, false)?;
            quoted_yields
                .entry(price.subject_id.clone())
                .or_default()
                .push(price);
        }
        let history = inputs.get("history").unwrap_or(&Value::Null);
        let mut closes: BTreeMap<String, BTreeMap<String, Dec>> = BTreeMap::new();
        for close in items(history, "closes") {
            closes.entry(text(close, "subject_id")).or_default().insert(
                text(close, "business_date"),
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
                        currency_code: text(i, "currency_code"),
                        debt: match i.get("debt") {
                            None | Some(Value::Null) => None,
                            Some(debt) => Some(crate::bonds::read(debt).map_err(error)?),
                        },
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
                        cost: match c.get("cost") {
                            Some(Value::String(cost)) if !cost.is_empty() => {
                                Some(decimal(c.get("cost"), "a closed lot's cost")?)
                            }
                            _ => None,
                        },
                        sleeve: text(c, "sleeve"),
                    })
                })
                .collect::<Result<_>>()?,
            market,
            quoted_yields,
            dates: items(history, "dates")
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
            unentitled: items(inputs, "unentitled")
                .iter()
                .map(|u| ((text(u, "subject_id"), text(u, "kind")), text(u, "dataset")))
                .collect(),
            quarantined: items(inputs, "quarantined")
                .iter()
                .map(|u| ((text(u, "subject_id"), text(u, "kind")), text(u, "dataset")))
                .collect(),
            routed: items(inputs, "routed")
                .iter()
                .map(|r| {
                    (
                        text(r, "model_id"),
                        Routed {
                            implementation: text(r, "implementation"),
                            implementation_version: text(r, "implementation_version"),
                            answered: r.get("answered").and_then(Value::as_bool).unwrap_or(false),
                        },
                    )
                })
                .collect(),
            priced: items(inputs, "priced")
                .iter()
                .map(|p| {
                    Ok((
                        (
                            text(p, "instrument_id"),
                            text(p, "base_currency_instrument_id"),
                        ),
                        decimal(p.get("value_per_unit"), "a priced unit's value")?,
                    ))
                })
                .collect::<Result<_>>()?,
            business_date: text(job, "business_date"),
            reporting_currency: text(job, "reporting_currency"),
            pivots: items(inputs, "fx_pivot_currencies")
                .iter()
                .map(|p| p.as_str().unwrap_or_default().to_string())
                .collect(),
            executions: items(inputs, "executions")
                .iter()
                .map(execution)
                .collect::<Result<_>>()?,
            tca_staleness_bound_ns: integer(
                inputs.get("tca_staleness_bound_ns"),
                "tca_staleness_bound_ns",
            )?,
            chains: crate::chains::read(inputs)?,
            scenario: crate::scenario::Set::read(inputs)?,
            valuation_date: text(inputs, "valuation_date"),
            factors: factor_rows(inputs)?,
            curves: items(inputs, "par_curves")
                .iter()
                .map(|c| Ok((text(c, "subject_id"), par_curve(c)?)))
                .collect::<Result<_>>()?,
            treasury_curve: match inputs.get("treasury_curve") {
                None | Some(Value::Null) => None,
                Some(c) => Some(par_curve(c)?),
            },
            calendars: items(inputs, "calendars")
                .iter()
                .map(|c| {
                    let holidays = items(c, "holidays")
                        .iter()
                        .map(|d| {
                            meridian_domain::date::Date::parse(d.as_str().unwrap_or_default())
                                .map_err(|why| error(format!("a calendar's holiday: {why}")))
                        })
                        .collect::<Result<_>>()?;
                    Ok((
                        text(c, "calendar_id"),
                        crate::bonds::dates::Calendar { holidays },
                    ))
                })
                .collect::<Result<_>>()?,
        })
    }

    /// The price of `subject` in one instrument (contract v27): an FX rate
    /// is the price of one cash instrument in another, and a currency may be
    /// priced in several, each its own row. The latest valid, as
    /// [`Case::price`] chooses.
    pub fn price_in(&self, subject: &str, kind: &str, instrument_id: &str) -> Option<&Price> {
        let held = self.market.get(&(subject.to_string(), kind.to_string()))?;
        let mut best: Option<&Price> = None;
        for price in held.iter().filter(|p| p.instrument_id == instrument_id) {
            match best {
                Some(b) if (price.valid_at_ns, &price.dataset) <= (b.valid_at_ns, &b.dataset) => {}
                _ => best = Some(price),
            }
        }
        best
    }

    /// The latest price of `subject` of `kind` in each instrument it is
    /// priced in, by the instruments' IDs.
    pub fn quoted_in(&self, subject: &str, kind: &str) -> Vec<&Price> {
        let Some(held) = self.market.get(&(subject.to_string(), kind.to_string())) else {
            return Vec::new();
        };
        let instruments: std::collections::BTreeSet<&str> =
            held.iter().map(|p| p.instrument_id.as_str()).collect();
        instruments
            .into_iter()
            .filter_map(|instrument| self.price_in(subject, kind, instrument))
            .collect()
    }

    /// Whether a cash instrument is a fiat currency's, with an ISO 4217
    /// code: only a fiat leg is inverted or crossed (ruling 462).
    pub fn fiat(&self, instrument_id: &str) -> bool {
        self.instruments
            .get(instrument_id)
            .is_some_and(|record| !record.currency_code.is_empty())
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
