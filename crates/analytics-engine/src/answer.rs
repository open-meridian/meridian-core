//! A job's answer: its figures, what was not evaluated and why, and its
//! provenance (the one-API spec's requirements 4, 5 and 9; W12.1, W12.7).
//!
//! Two forms: the answer's JSON, as the SDK reference case's `answer` holds
//! it (`meridian.analytics.answer_json`), which the identity test compares
//! byte for byte; and the wire's `JobAnswer`.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::case::MeasureKey;
use crate::exact::Dec;

pub const BASE: &str = "SCENARIO_KIND_BASE";
pub const WHAT_IF: &str = "SCENARIO_KIND_WHAT_IF";
pub const HISTORICAL_DAY: &str = "SCENARIO_KIND_HISTORICAL_DAY";
pub const OUTCOME: &str = "SCENARIO_KIND_OUTCOME";
pub const NONE_OF_THE_NAMED: &str = "SCENARIO_KIND_NONE_OF_THE_NAMED";
pub const EXACT: &str = "COMPUTED_AS_EXACT";

pub const PRICE_MISSING: &str = "NOT_EVALUATED_REASON_PRICE_MISSING";
pub const PRICE_STALE: &str = "NOT_EVALUATED_REASON_PRICE_STALE";
pub const CONVERSION_MISSING: &str = "NOT_EVALUATED_REASON_CONVERSION_MISSING";
pub const NOT_ENTITLED: &str = "NOT_EVALUATED_REASON_NOT_ENTITLED";
pub const LOCAL_RECORD: &str = "NOT_EVALUATED_REASON_LOCAL_RECORD";
pub const IMPLEMENTATION_UNAVAILABLE: &str = "NOT_EVALUATED_REASON_IMPLEMENTATION_UNAVAILABLE";
pub const SCHEME_NOT_HELD: &str = "NOT_EVALUATED_REASON_SCHEME_NOT_HELD";
pub const GROUP_NOT_EXHAUSTIVE: &str = "NOT_EVALUATED_REASON_GROUP_NOT_EXHAUSTIVE";
pub const HISTORY_INSUFFICIENT: &str = "NOT_EVALUATED_REASON_HISTORY_INSUFFICIENT";

/// The order scenarios are answered in within one breakdown.
pub fn scenario_order(kind: &str) -> u8 {
    match kind {
        BASE => 0,
        WHAT_IF => 1,
        HISTORICAL_DAY => 2,
        OUTCOME => 3,
        NONE_OF_THE_NAMED => 4,
        _ => 9,
    }
}

/// Which scenario a figure is under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Scenario {
    pub kind: &'static str,
    pub business_date: String,
    pub outcome_instrument_id: String,
}

impl Scenario {
    pub fn of(kind: &'static str) -> Scenario {
        Scenario {
            kind,
            business_date: String::new(),
            outcome_instrument_id: String::new(),
        }
    }

    pub fn outcome(member: &str) -> Scenario {
        Scenario {
            kind: OUTCOME,
            business_date: String::new(),
            outcome_instrument_id: member.to_string(),
        }
    }
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
}

impl Figure {
    pub fn to_json(&self) -> Value {
        let text = |held: &Option<Dec>| match held {
            Some(value) => Value::String(value.text()),
            None => Value::Null,
        };
        json!({
            "measure": self.measure.to_json(),
            "account_id": self.account_id,
            "breakdown": self.breakdown.iter()
                .map(|(b, v)| json!({"breakdown": b, "value": v}))
                .collect::<Vec<_>>(),
            "scenario_key": {
                "kind": self.scenario.kind,
                "business_date": self.scenario.business_date,
                "outcome_instrument_id": self.scenario.outcome_instrument_id,
            },
            "currency_instrument_id": self.currency_instrument_id,
            "computed_as": EXACT,
            "value": text(&self.value),
            "numerator": text(&self.numerator),
            "denominator": text(&self.denominator),
        })
    }

    /// The order figures are answered in within one measure: by account,
    /// breakdown and scenario.
    pub fn order_key(&self) -> (&str, &[(String, String)], u8, &Scenario) {
        (
            &self.account_id,
            &self.breakdown,
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
}

impl Unevaluated {
    pub fn to_json(&self) -> Value {
        json!({
            "account_id": self.account_id,
            "instrument_id": self.instrument_id,
            "measure": self.measure.to_json(),
            "reason": self.reason,
            "detail": self.detail,
        })
    }

    pub fn order_key(&self) -> (&str, &str, &str, &str) {
        (
            &self.account_id,
            &self.instrument_id,
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
}

impl Reads {
    pub fn absorb(&mut self, other: Reads) {
        self.datasets.extend(other.datasets);
        self.instruments.extend(other.instruments);
        self.models.extend(other.models);
        self.groups.extend(other.groups);
    }
}

/// A job's answer.
#[derive(Debug, Clone)]
pub struct Answer {
    pub figures: Vec<Figure>,
    pub not_evaluated: Vec<Unevaluated>,
    pub provenance: Value,
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
        json!({
            "figures": self.figures.iter().map(Figure::to_json).collect::<Vec<_>>(),
            "not_evaluated": self.not_evaluated.iter().map(Unevaluated::to_json).collect::<Vec<_>>(),
            "provenance": self.provenance,
        })
    }
}
