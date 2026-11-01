//! A job's answer: its figures, what was not evaluated and why, and its
//! provenance (the one-API spec's requirements 4, 5 and 9; W12.1, W12.7).
//!
//! Two forms: the answer's JSON, as the SDK reference case's `answer` holds
//! it (`meridian.analytics.answer_json`), which the identity test compares
//! byte for byte; and the wire's `JobAnswer`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::case::MeasureKey;
use crate::exact::Dec;

pub const BASE: &str = "SCENARIO_KIND_BASE";
pub const WHAT_IF: &str = "SCENARIO_KIND_WHAT_IF";
pub const HISTORICAL_DAY: &str = "SCENARIO_KIND_HISTORICAL_DAY";
pub const OUTCOME: &str = "SCENARIO_KIND_OUTCOME";
pub const NONE_OF_THE_NAMED: &str = "SCENARIO_KIND_NONE_OF_THE_NAMED";
pub const EXACT: &str = "COMPUTED_AS_EXACT";
/// Contract v28 (W12.1): a model's floating-point value, crossing at its
/// definition's scale, rounded half to even.
pub const MODEL: &str = "COMPUTED_AS_MODEL";
/// Contract v28 (W12.8): one point of a scenario or a scenario set.
pub const SCENARIO_POINT: &str = "SCENARIO_KIND_SCENARIO_POINT";

pub const PRICE_MISSING: &str = "NOT_EVALUATED_REASON_PRICE_MISSING";
pub const PRICE_STALE: &str = "NOT_EVALUATED_REASON_PRICE_STALE";
pub const CONVERSION_MISSING: &str = "NOT_EVALUATED_REASON_CONVERSION_MISSING";
pub const NOT_ENTITLED: &str = "NOT_EVALUATED_REASON_NOT_ENTITLED";
pub const LOCAL_RECORD: &str = "NOT_EVALUATED_REASON_LOCAL_RECORD";
pub const IMPLEMENTATION_UNAVAILABLE: &str = "NOT_EVALUATED_REASON_IMPLEMENTATION_UNAVAILABLE";
pub const SCHEME_NOT_HELD: &str = "NOT_EVALUATED_REASON_SCHEME_NOT_HELD";
pub const GROUP_NOT_EXHAUSTIVE: &str = "NOT_EVALUATED_REASON_GROUP_NOT_EXHAUSTIVE";
pub const HISTORY_INSUFFICIENT: &str = "NOT_EVALUATED_REASON_HISTORY_INSUFFICIENT";
/// Contract v27 (ruling 325): a lot's cost the book does not record.
pub const COST_MISSING: &str = "NOT_EVALUATED_REASON_COST_MISSING";
/// Contract v28 (W12.1): an option quote below its intrinsic value.
pub const BELOW_INTRINSIC: &str = "NOT_EVALUATED_REASON_BELOW_INTRINSIC";
/// Contract v28 (W12.1): a surface fit failing a no-arbitrage check or not
/// converging.
pub const FIT_FAILED: &str = "NOT_EVALUATED_REASON_FIT_FAILED";
/// Contract v28 (W12.2; ruling 421): the only input in force is invalid.
pub const INPUT_QUARANTINED: &str = "NOT_EVALUATED_REASON_INPUT_QUARANTINED";
/// Contract v29 (W12.1): a debt record minted without the terms a measure
/// needs, the fields named.
pub const TERMS_INCOMPLETE: &str = "NOT_EVALUATED_REASON_TERMS_INCOMPLETE";

/// How a conversion leg's rate was found (contract v27, ruling 462).
pub const DIRECT: &str = "CONVERSION_DERIVATION_DIRECT";
pub const INVERTED: &str = "CONVERSION_DERIVATION_INVERTED";
pub const CROSSED: &str = "CONVERSION_DERIVATION_CROSSED";

/// The order scenarios are answered in within one breakdown.
pub fn scenario_order(kind: &str) -> u8 {
    match kind {
        BASE => 0,
        WHAT_IF => 1,
        HISTORICAL_DAY => 2,
        OUTCOME => 3,
        NONE_OF_THE_NAMED => 4,
        SCENARIO_POINT => 5,
        _ => 9,
    }
}

/// Which scenario a figure is under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Scenario {
    pub kind: &'static str,
    pub business_date: String,
    pub outcome_instrument_id: String,
    /// Contract v28 (W12.8): a scenario point's saved row key, or `inline`;
    /// its version; and its position on each axis.
    pub scenario_row_key: String,
    pub scenario_version: u64,
    pub axis_positions: Vec<u32>,
}

impl Default for Scenario {
    fn default() -> Scenario {
        Scenario::of(BASE)
    }
}

impl Scenario {
    pub fn of(kind: &'static str) -> Scenario {
        Scenario {
            kind,
            business_date: String::new(),
            outcome_instrument_id: String::new(),
            scenario_row_key: String::new(),
            scenario_version: 0,
            axis_positions: Vec::new(),
        }
    }

    pub fn outcome(member: &str) -> Scenario {
        Scenario {
            outcome_instrument_id: member.to_string(),
            ..Scenario::of(OUTCOME)
        }
    }

    /// Contract v28: one point of a scenario set, by its axis positions.
    pub fn point(row_key: &str, version: u64, axis_positions: Vec<u32>) -> Scenario {
        Scenario {
            scenario_row_key: row_key.to_string(),
            scenario_version: version,
            axis_positions,
            ..Scenario::of(SCENARIO_POINT)
        }
    }

    fn to_json(&self) -> Value {
        let mut out = json!({
            "kind": self.kind,
            "business_date": self.business_date,
            "outcome_instrument_id": self.outcome_instrument_id,
        });
        // Contract v28: written only on a scenario point, so every earlier
        // answer's JSON, and its hash, is unchanged.
        if self.kind == SCENARIO_POINT {
            out["scenario_row_key"] = json!(self.scenario_row_key);
            out["scenario_version"] = json!(self.scenario_version.to_string());
            out["axis_positions"] = json!(self.axis_positions);
        }
        out
    }
}

/// Contract v28: a series' key, as `Identifier`s (scheme, value).
pub type SeriesKey = Vec<(String, String)>;

fn series_json(key: &SeriesKey) -> Value {
    Value::Array(
        key.iter()
            .map(|(scheme, value)| json!({"scheme": scheme, "value": value}))
            .collect(),
    )
}

/// One figure.
#[derive(Debug, Clone)]
pub struct Figure {
    pub measure: MeasureKey,
    pub account_id: String,
    pub breakdown: Vec<(String, String)>,
    pub scenario: Scenario,
    pub value: Option<Dec>,
    pub numerator: Option<Dec>,
    pub denominator: Option<Dec>,
    pub currency_instrument_id: String,
    /// Contract v28: `COMPUTED_AS_MODEL` for a model's figure, else exact.
    pub computed_as: &'static str,
    /// Contract v28: a series not held, named by its key.
    pub series_key: SeriesKey,
    /// Contract v27 (ruling 374): its oldest input's source time; 0 for a
    /// figure that read no market data, and where the measure does not say.
    pub oldest_input_source_ns: i64,
}

impl Default for Figure {
    fn default() -> Figure {
        Figure {
            measure: MeasureKey {
                definition: String::new(),
                version: 1,
                event_group_id: String::new(),
                breakdowns: Vec::new(),
            },
            account_id: String::new(),
            breakdown: Vec::new(),
            scenario: Scenario::default(),
            value: None,
            numerator: None,
            denominator: None,
            currency_instrument_id: String::new(),
            computed_as: EXACT,
            series_key: Vec::new(),
            oldest_input_source_ns: 0,
        }
    }
}

impl Figure {
    pub fn to_json(&self) -> Value {
        let text = |held: &Option<Dec>| match held {
            Some(value) => Value::String(value.text()),
            None => Value::Null,
        };
        let mut out = json!({
            "measure": self.measure.to_json(),
            "account_id": self.account_id,
            "breakdown": self.breakdown.iter()
                .map(|(b, v)| json!({"breakdown": b, "value": v}))
                .collect::<Vec<_>>(),
            "scenario_key": self.scenario.to_json(),
            "currency_instrument_id": self.currency_instrument_id,
            "computed_as": self.computed_as,
            "value": text(&self.value),
            "numerator": text(&self.numerator),
            "denominator": text(&self.denominator),
        });
        // Contract v28: written only where held, so every earlier answer's
        // JSON is unchanged.
        if !self.series_key.is_empty() {
            out["series_key"] = series_json(&self.series_key);
        }
        if self.oldest_input_source_ns != 0 {
            out["oldest_input_source_ns"] = json!(self.oldest_input_source_ns.to_string());
        }
        out
    }

    /// The order figures are answered in within one measure: by account,
    /// breakdown and scenario.
    pub fn order_key(&self) -> (&str, &[(String, String)], &SeriesKey, u8, &Scenario) {
        (
            &self.account_id,
            &self.breakdown,
            &self.series_key,
            scenario_order(self.scenario.kind),
            &self.scenario,
        )
    }
}

/// A position or a measure not evaluated, and why (requirement 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unevaluated {
    pub account_id: String,
    pub instrument_id: String,
    pub measure: MeasureKey,
    pub reason: &'static str,
    pub detail: String,
    /// Contract v28: a series not held, named by its key.
    pub series_key: SeriesKey,
}

impl Unevaluated {
    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "account_id": self.account_id,
            "instrument_id": self.instrument_id,
            "measure": self.measure.to_json(),
            "reason": self.reason,
            "detail": self.detail,
        });
        if !self.series_key.is_empty() {
            out["series_key"] = series_json(&self.series_key);
        }
        out
    }

    pub fn order_key(&self) -> (&str, &str, &SeriesKey, &str, &str) {
        (
            &self.account_id,
            &self.instrument_id,
            &self.series_key,
            self.reason,
            &self.detail,
        )
    }
}

/// What an answer read, for its provenance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reads {
    pub datasets: BTreeSet<String>,
    pub instruments: BTreeSet<String>,
    pub models: BTreeSet<(String, i64)>,
    pub groups: BTreeSet<String>,
    /// Contract v27 (ruling 460): each position's own price, by its
    /// instrument, as the provenance names it.
    pub input_prices: BTreeMap<String, Value>,
    /// Contract v27 (ruling 462): each conversion leg, by (from, to,
    /// derivation's rank, via), as the provenance names it.
    pub legs: BTreeMap<(String, String, u8, String), Value>,
}

impl Reads {
    pub fn absorb(&mut self, other: Reads) {
        self.datasets.extend(other.datasets);
        self.instruments.extend(other.instruments);
        self.models.extend(other.models);
        self.groups.extend(other.groups);
        for (key, price) in other.input_prices {
            self.input_prices.entry(key).or_insert(price);
        }
        for (key, leg) in other.legs {
            self.legs.entry(key).or_insert(leg);
        }
    }
}

/// A job's answer.
#[derive(Debug, Clone)]
pub struct Answer {
    pub figures: Vec<Figure>,
    pub not_evaluated: Vec<Unevaluated>,
    pub provenance: Value,
    /// Contract v27 (W10.11): the `ExecutionCost` rows an `execution_cost`
    /// job records, in the answer's JSON only where the job asks for it.
    pub execution_costs: Vec<Value>,
    pub asks_execution_cost: bool,
}

impl Answer {
    pub fn answer_hash(&self) -> &str {
        self.provenance
            .get("answer_hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    /// The answer as a reference case's `answer` holds it.
    pub fn to_json(&self) -> Value {
        let mut out = json!({
            "figures": self.figures.iter().map(Figure::to_json).collect::<Vec<_>>(),
            "not_evaluated": self.not_evaluated.iter().map(Unevaluated::to_json).collect::<Vec<_>>(),
            "provenance": self.provenance,
        });
        if self.asks_execution_cost {
            out["execution_costs"] = Value::Array(self.execution_costs.clone());
        }
        out
    }
}
