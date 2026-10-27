//! An answer as the wire carries it: the engine's answer as a `JobAnswer`
//! (W12.1, W12.4, W12.7), every figure a `meridian.v1.Decimal` at its
//! definition's scale, never a float (decisions/023).

use std::str::FromStr;

use meridian_analytics_engine::answer::{Answer, Figure, Unevaluated};
use meridian_analytics_engine::case::MeasureKey;
use meridian_analytics_engine::exact::Dec;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    AnswerProvenance, BreakdownValue, ComputedAs, ConversionDerivation, ConversionLeg,
    EventGroupInput, Figure as WireFigure, InputPrice, InstrumentInput, JobAnswer, MeasureRef,
    ModelRef, NotEvaluated, NotEvaluatedReason, PartitionSequence, ScenarioKey, ScenarioKind,
    Watermark,
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
            // Contract v28 (W12.8): a scenario point's key.
            scenario_row_key: figure.scenario.scenario_row_key.clone(),
            scenario_version: figure.scenario.scenario_version,
            axis_positions: figure.scenario.axis_positions.clone(),
        }),
        value: decimal(&figure.value)?,
        numerator: decimal(&figure.numerator)?,
        denominator: decimal(&figure.denominator)?,
        currency_instrument_id: figure.currency_instrument_id.clone(),
        computed_as: ComputedAs::from_str_name(figure.computed_as).unwrap_or(ComputedAs::Exact)
            as i32,
        oldest_input_source_ns: figure.oldest_input_source_ns,
        series_key: series_key(&figure.series_key),
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
        age_ns: 0,
        ceiling_ns: 0,
        series_key: series_key(&entry.series_key),
    }
}

/// Contract v28: a series' key as the wire's identifiers.
fn series_key(key: &[(String, String)]) -> Vec<meridian_domain::v1::Identifier> {
    key.iter()
        .map(|(scheme, value)| meridian_domain::v1::Identifier {
            scheme: scheme.clone(),
            value: value.clone(),
            ..Default::default()
        })
        .collect()
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
        // Contract v27 (W12.7; rulings 458, 460 and 462).
        business_date: text(&json["business_date"]),
        input_prices: list(&json["input_prices"])
            .iter()
            .map(|p| InputPrice {
                instrument_id: text(&p["instrument_id"]),
                price: money(&p["price"]),
                dataset: text(&p["dataset"]),
                row_key: text(&p["row_key"]),
                version: integer(&p["version"]).max(0) as u64,
                source_ns: integer(&p["source_ns"]),
            })
            .collect(),
        conversion_legs: list(&json["conversion_legs"])
            .iter()
            .map(|leg| ConversionLeg {
                from_instrument_id: text(&leg["from_instrument_id"]),
                to_instrument_id: text(&leg["to_instrument_id"]),
                rate: wire_decimal(&leg["rate"]),
                derivation: ConversionDerivation::from_str_name(&text(&leg["derivation"]))
                    .map(|d| d as i32)
                    .unwrap_or(0),
                via_instrument_id: text(&leg["via_instrument_id"]),
                dataset: text(&leg["dataset"]),
            })
            .collect(),
    }
}

/// A decimal the provenance carries as text, on the wire; unset for none.
fn wire_decimal(value: &Value) -> Option<meridian_pb::v1::Decimal> {
    value
        .as_str()
        .and_then(|text| Exact::from_str(text).ok())
        .map(Exact::to_wire)
}

/// A price the provenance carries as `{amount, instrument_id}`.
fn money(value: &Value) -> Option<meridian_domain::v1::Money> {
    value.is_object().then(|| meridian_domain::v1::Money {
        amount: wire_decimal(&value["amount"]),
        currency_code: String::new(),
        instrument_id: text(&value["instrument_id"]),
    })
}

/// The `ExecutionCost` rows an `execution_cost` job answered (contract v27,
/// W10.11), as the lake records them: each under its execution's
/// instrument and dated by its trade date, from `executions` (the
/// resolve's, by execution ID); its row key the lake's (`EXE-…:arrival`).
pub fn execution_costs(
    answer: &Answer,
    executions: &std::collections::BTreeMap<String, (String, String)>,
) -> Vec<meridian_domain::v1::ExecutionCost> {
    use meridian_domain::v1::{
        ExecutionCost, LiquidityIndicator, ObservationMeta, Side, SubjectRef, TcaBenchmark,
        TcaUnanswered, TimePrecision,
    };
    let integer = |value: &Value| -> i64 {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            .unwrap_or(0)
    };
    answer
        .execution_costs
        .iter()
        .filter_map(|row| {
            let execution_id = text(&row["execution_id"]);
            let (instrument, trade_date) = executions.get(&execution_id)?;
            Some(ExecutionCost {
                meta: Some(ObservationMeta {
                    subjects: vec![SubjectRef {
                        entity_id: instrument.clone(),
                        ..Default::default()
                    }],
                    business_date: trade_date.clone(),
                    ..Default::default()
                }),
                execution_id,
                order_id: text(&row["order_id"]),
                placement_id: text(&row["placement_id"]),
                account_id: text(&row["account_id"]),
                benchmark: TcaBenchmark::from_str_name(&text(&row["benchmark"]))
                    .map_or(0, |b| b as i32),
                benchmark_price: money(&row["benchmark_price"]),
                benchmark_at_ns: integer(&row["benchmark_at_ns"]),
                benchmark_dataset: text(&row["benchmark_dataset"]),
                benchmark_row_key: text(&row["benchmark_row_key"]),
                fixing_id: String::new(),
                execution_price: money(&row["execution_price"]),
                side: Side::from_str_name(&text(&row["side"])).map_or(0, |s| s as i32),
                notional: money(&row["notional"]),
                cost_bps: wire_decimal(&row["cost_bps"]),
                fees_bps: wire_decimal(&row["fees_bps"]),
                financing_bps: wire_decimal(&row["financing_bps"]),
                liquidity_indicator: LiquidityIndicator::from_str_name(&text(
                    &row["liquidity_indicator"],
                ))
                .map_or(0, |l| l as i32),
                venue_id: text(&row["venue_id"]),
                unanswered: TcaUnanswered::from_str_name(&text(&row["unanswered"]))
                    .map_or(0, |u| u as i32),
                staleness_bound_ns: integer(&row["staleness_bound_ns"]),
                time_precision: TimePrecision::from_str_name(&text(&row["time_precision"]))
                    .map_or(0, |t| t as i32),
                reject_count: integer(&row["reject_count"]).max(0) as u32,
                time_to_fill_ns: integer(&row["time_to_fill_ns"]),
            })
        })
        .collect()
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
        // Whole: the coordinator pages it as it is read (ruling 794).
        next_cursor: String::new(),
    })
}
