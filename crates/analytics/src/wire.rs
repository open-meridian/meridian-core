//! An answer as the wire carries it: the engine's answer as a `JobAnswer`
//! (W12.1, W12.4, W12.7), every figure a `meridian.v1.Decimal` at its
//! definition's scale, never a float (decisions/023).

use std::str::FromStr;

use meridian_analytics_engine::answer::{Answer, Figure, Unevaluated};
use meridian_analytics_engine::case::MeasureKey;
use meridian_analytics_engine::exact::Dec;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    AnswerProvenance, BreakdownValue, ComputedAs, EventGroupInput, Figure as WireFigure,
    InstrumentInput, JobAnswer, MeasureRef, ModelRef, NotEvaluated, NotEvaluatedReason,
    PartitionSequence, ScenarioKey, ScenarioKind, Watermark,
};
use serde_json::Value;

fn decimal(value: &Option<Dec>) -> Result<Option<meridian_pb::v1::Decimal>, String> {
    value
        .as_ref()
        .map(|d| {
            Exact::from_str(&d.text())
                .map(Exact::to_wire)
                .map_err(|failed| format!("a figure {} {failed}", d.text()))
        })
        .transpose()
}

pub fn measure(key: &MeasureKey) -> MeasureRef {
    MeasureRef {
        definition: key.definition.clone(),
        version: key.version as i32,
        breakdowns: key.breakdowns.clone(),
        event_group_id: key.event_group_id.clone(),
    }
}

fn figure(figure: &Figure) -> Result<WireFigure, String> {
    Ok(WireFigure {
        measure: Some(measure(&figure.measure)),
        account_id: figure.account_id.clone(),
        breakdown: figure
            .breakdown
            .iter()
            .map(|(breakdown, value)| BreakdownValue {
                breakdown: breakdown.clone(),
                value: value.clone(),
            })
            .collect(),
        scenario_key: Some(ScenarioKey {
            kind: ScenarioKind::from_str_name(figure.scenario.kind)
                .map(|k| k as i32)
                .unwrap_or(0),
            business_date: figure.scenario.business_date.clone(),
            outcome_instrument_id: figure.scenario.outcome_instrument_id.clone(),
        }),
        value: decimal(&figure.value)?,
        numerator: decimal(&figure.numerator)?,
        denominator: decimal(&figure.denominator)?,
        currency_instrument_id: figure.currency_instrument_id.clone(),
        computed_as: ComputedAs::Exact as i32,
    })
}

fn unevaluated(entry: &Unevaluated) -> NotEvaluated {
    NotEvaluated {
        account_id: entry.account_id.clone(),
        instrument_id: entry.instrument_id.clone(),
        measure: Some(measure(&entry.measure)),
        reason: NotEvaluatedReason::from_str_name(entry.reason)
            .map(|r| r as i32)
            .unwrap_or(0),
        detail: entry.detail.clone(),
    }
}

fn integer(value: &Value) -> i64 {
    match value {
        Value::String(text) => text.parse().unwrap_or(0),
        Value::Number(number) => number.as_i64().unwrap_or(0),
        _ => 0,
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// The provenance as the wire carries it.
pub fn provenance(json: &Value) -> AnswerProvenance {
    AnswerProvenance {
        book_watermark: Some(Watermark {
            partitions: list(&json["book_watermark"])
                .iter()
                .map(|p| PartitionSequence {
                    partition: text(&p["partition"]),
                    sequence: integer(&p["sequence"]).max(0) as u64,
                })
                .collect(),
        }),
        lake_as_of_ns: integer(&json["lake_as_of_ns"]),
        datasets: list(&json["datasets"]).iter().map(text).collect(),
        instruments: list(&json["instruments"])
            .iter()
            .map(|i| InstrumentInput {
                instrument_id: text(&i["instrument_id"]),
                record_version: integer(&i["record_version"]),
                template_id: text(&i["template_id"]),
                template_version: text(&i["template_version"]),
                calculation_version: text(&i["calculation_version"]),
            })
            .collect(),
        models: list(&json["models"])
            .iter()
            .map(|m| ModelRef {
                model_id: text(&m["model_id"]),
                model_version: integer(&m["model_version"]) as i32,
                implementation: text(&m["implementation"]),
                implementation_version: text(&m["implementation_version"]),
            })
            .collect(),
        measures: list(&json["measures"])
            .iter()
            .map(|m| MeasureRef {
                definition: text(&m["definition"]),
                version: integer(&m["version"]) as i32,
                breakdowns: Vec::new(),
                event_group_id: String::new(),
            })
            .collect(),
        event_groups: list(&json["event_groups"])
            .iter()
            .map(|g| EventGroupInput {
                event_group_id: text(&g["event_group_id"]),
                version: integer(&g["version"]),
            })
            .collect(),
        routing_version: integer(&json["routing_version"]),
        engine_version: text(&json["engine_version"]),
        answer_hash: text(&json["answer_hash"]),
    }
}

/// The engine's answer as a `JobAnswer`.
pub fn job_answer(job_id: &str, answer: &Answer) -> Result<JobAnswer, String> {
    Ok(JobAnswer {
        job_id: job_id.to_string(),
        figures: answer
            .figures
            .iter()
            .map(figure)
            .collect::<Result<_, _>>()?,
        not_evaluated: answer.not_evaluated.iter().map(unevaluated).collect(),
        provenance: Some(provenance(&answer.provenance)),
    })
}
