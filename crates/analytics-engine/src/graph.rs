//! The graph of pure nodes (the one-API spec's requirements 24, 25 and 30).
//!
//! A job's answer is its inputs document (the leaves, named by the answer's
//! hash), each measure's preamble, and its units: each unit a pure function
//! of the document, named by the hash of the document's hash, the unit and
//! the engine's code version ([`unit_hash`]), so a unit's part may be cached
//! by that name and reused by any job asking the same, and computing it
//! twice, or on another worker, changes nothing. The root joins the parts in
//! the job's order, each measure's sorted, and adds the provenance.
//!
//! Who runs the units is an [`Executor`]: here, one after another
//! ([`Sequential`]) or across threads ([`Threads`]); in the component, the
//! worker pool on the bus's queue group.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::answer::*;
use crate::canonical;
use crate::case::{Case, CaseError, MeasureKey, Result};
use crate::definitions::BUILT_IN;
use crate::exact::Dec;
use crate::measures::{self, Part, Unit};

/// The engine's code version, which every node's name covers.
pub const CODE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A unit's name: SHA-256 over its inputs' hash, the unit and the code's
/// version.
pub fn unit_hash(inputs_hash: &str, unit: &Unit) -> String {
    let named = json!({
        "inputs": inputs_hash,
        "measure": unit.measure,
        "account": unit.account,
        "code": CODE_VERSION,
    });
    canonical::sha256_name(
        canonical::canonical(&named)
            .expect("a unit's name is canonical")
            .as_bytes(),
    )
}

/// What an executor is handed: the document, read and as written, and its
/// hash.
pub struct Job<'a> {
    pub case: &'a Case,
    pub inputs: &'a Value,
    pub inputs_hash: &'a str,
}

/// Runs units, each to its part, in the order given.
pub trait Executor: Send + Sync {
    fn run(&self, job: &Job<'_>, units: &[Unit]) -> Vec<Result<Part>>;
}

/// Every unit here, one after another.
pub struct Sequential;

impl Executor for Sequential {
    fn run(&self, job: &Job<'_>, units: &[Unit]) -> Vec<Result<Part>> {
        units
            .iter()
            .map(|u| measures::compute(job.case, u))
            .collect()
    }
}

/// The units shared among `n` threads, each taking the next unit left: the
/// same answer as one, by requirement 25.
pub struct Threads(pub usize);

impl Executor for Threads {
    fn run(&self, job: &Job<'_>, units: &[Unit]) -> Vec<Result<Part>> {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let done: Mutex<Vec<Option<Result<Part>>>> = Mutex::new(vec![None; units.len()]);
        std::thread::scope(|scope| {
            for _ in 0..self.0.max(1) {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if i >= units.len() {
                        break;
                    }
                    let part = measures::compute(job.case, &units[i]);
                    done.lock().expect("not poisoned")[i] = Some(part);
                });
            }
        });
        done.into_inner()
            .expect("not poisoned")
            .into_iter()
            .map(|part| part.expect("every unit ran"))
            .collect()
    }
}

/// Parts already computed, by their unit's name: losable, bounded, the
/// oldest leaving first (Q12).
pub struct Cache {
    held: Mutex<(BTreeMap<String, Part>, VecDeque<String>)>,
    most: usize,
}

impl Cache {
    pub fn new(most: usize) -> Cache {
        Cache {
            held: Mutex::new((BTreeMap::new(), VecDeque::new())),
            most,
        }
    }

    pub fn get(&self, name: &str) -> Option<Part> {
        self.held.lock().expect("not poisoned").0.get(name).cloned()
    }

    pub fn put(&self, name: String, part: Part) {
        let mut held = self.held.lock().expect("not poisoned");
        if held.0.contains_key(&name) {
            return;
        }
        held.0.insert(name.clone(), part);
        held.1.push_back(name);
        while held.1.len() > self.most {
            if let Some(oldest) = held.1.pop_front() {
                held.0.remove(&oldest);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.held.lock().expect("not poisoned").0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The answer: every measure's preamble and units, joined in the job's
/// order, each measure sorted, and the provenance.
pub fn answer(case: &Case, inputs: &Value, hash: &str, executor: &dyn Executor) -> Result<Answer> {
    let mut planned = Vec::with_capacity(case.measures.len());
    let mut units = Vec::new();
    for index in 0..case.measures.len() {
        let plan = measures::plan(case, index)?;
        units.extend(plan.units.iter().cloned());
        planned.push(plan);
    }
    let job = Job {
        case,
        inputs,
        inputs_hash: hash,
    };
    let mut parts = executor.run(&job, &units).into_iter();
    let mut figures = Vec::new();
    let mut not_evaluated = Vec::new();
    let mut execution_costs = Vec::new();
    let mut reads = Reads::default();
    for plan in planned {
        let (figures_before, unevaluated_before) = (figures.len(), not_evaluated.len());
        let mut take = |part: Part| {
            figures.extend(part.figures);
            not_evaluated.extend(part.not_evaluated);
            execution_costs.extend(part.execution_costs);
            reads.absorb(part.reads);
        };
        take(plan.preamble);
        for _ in &plan.units {
            take(parts.next().expect("a part per unit")?);
        }
        not_evaluated[unevaluated_before..].sort_by(|a, b| a.order_key().cmp(&b.order_key()));
        figures[figures_before..].sort_by(|a, b| a.order_key().cmp(&b.order_key()));
    }
    let provenance = provenance(case, &reads, hash);
    // Contract v27 (W10.11): the rows by execution, then benchmark.
    execution_costs.sort_by_key(|row| {
        (
            row["execution_id"].as_str().unwrap_or_default().to_string(),
            measures::benchmark_order(row["benchmark"].as_str().unwrap_or_default()),
        )
    });
    Ok(Answer {
        figures,
        not_evaluated,
        provenance,
        execution_costs,
        asks_execution_cost: case
            .measures
            .iter()
            .any(|key| key.definition == "execution_cost"),
    })
}

fn provenance(case: &Case, reads: &Reads, hash: &str) -> Value {
    let mut measures_asked: Vec<Value> = Vec::new();
    for key in &case.measures {
        let held = json!({"definition": key.definition, "version": key.version});
        if !measures_asked.contains(&held) {
            measures_asked.push(held);
        }
    }
    json!({
        "book_watermark": case.book_watermark,
        // Contract v27 (W12.7; rulings 458, 460 and 462).
        "business_date": case.business_date,
        "input_prices": reads.input_prices.values().collect::<Vec<_>>(),
        "conversion_legs": reads.legs.values().collect::<Vec<_>>(),
        "lake_as_of_ns": case.lake_as_of_ns.to_string(),
        "datasets": reads.datasets.iter().collect::<Vec<_>>(),
        "instruments": reads.instruments.iter()
            .filter_map(|id| case.instruments.get(id))
            .map(|i| json!({
                "instrument_id": i.instrument_id,
                "record_version": i.record_version,
                "template_id": i.template_id,
                "template_version": i.template_version,
                "calculation_version": i.calculation_version,
            }))
            .collect::<Vec<_>>(),
        "models": reads.models.iter()
            .map(|(model, version)| match case.routed.get(model) {
                Some(routed) => json!({
                    "model_id": model,
                    "model_version": version,
                    "implementation": routed.implementation,
                    "implementation_version": routed.implementation_version,
                }),
                None => json!({
                    "model_id": model,
                    "model_version": version,
                    "implementation": BUILT_IN,
                    "implementation_version": case.engine_version,
                }),
            })
            .collect::<Vec<_>>(),
        "measures": measures_asked,
        "event_groups": reads.groups.iter()
            .map(|g| json!({"event_group_id": g, "version": case.groups[g].version}))
            .collect::<Vec<_>>(),
        "routing_version": case.routing_version,
        "engine_version": case.engine_version,
        "answer_hash": hash,
    })
}

// ── A part over the wire, between the coordinator and a worker ──────────────

/// The enum names a part may carry, as `&'static str`.
const NAMES: [&str; 22] = [
    BASE,
    WHAT_IF,
    HISTORICAL_DAY,
    OUTCOME,
    NONE_OF_THE_NAMED,
    PRICE_MISSING,
    PRICE_STALE,
    CONVERSION_MISSING,
    NOT_ENTITLED,
    LOCAL_RECORD,
    IMPLEMENTATION_UNAVAILABLE,
    SCHEME_NOT_HELD,
    GROUP_NOT_EXHAUSTIVE,
    HISTORY_INSUFFICIENT,
    EXACT,
    COST_MISSING,
    // Contract v28.
    MODEL,
    SCENARIO_POINT,
    BELOW_INTRINSIC,
    FIT_FAILED,
    INPUT_QUARANTINED,
    // Contract v29.
    TERMS_INCOMPLETE,
];

fn series_key(value: &Value) -> Vec<(String, String)> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| {
                    (
                        i["scheme"].as_str().unwrap_or_default().to_string(),
                        i["value"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn name(value: &Value) -> Result<&'static str> {
    let text = value.as_str().unwrap_or_default();
    NAMES
        .iter()
        .find(|n| **n == text)
        .copied()
        .ok_or_else(|| CaseError(format!("{text:?} is not a name a part carries")))
}

fn decimal(value: &Value) -> Result<Option<Dec>> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Dec::parse(text).map(Some).map_err(CaseError),
        other => Err(CaseError(format!("{other} is not a decimal's text"))),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| i.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

impl Part {
    /// The part as JSON, every decimal at its full exactness: a worker's
    /// figures are already rounded, and the coordinator only joins them.
    pub fn to_json(&self) -> Value {
        json!({
            "figures": self.figures.iter().map(Figure::to_json).collect::<Vec<_>>(),
            "not_evaluated": self.not_evaluated.iter().map(Unevaluated::to_json).collect::<Vec<_>>(),
            "reads": {
                "datasets": self.reads.datasets,
                "instruments": self.reads.instruments,
                "models": self.reads.models.iter().map(|(m, v)| json!([m, v])).collect::<Vec<_>>(),
                "groups": self.reads.groups,
                "input_prices": self.reads.input_prices.values().collect::<Vec<_>>(),
                "legs": self.reads.legs.values().collect::<Vec<_>>(),
            },
            "execution_costs": self.execution_costs,
        })
    }

    pub fn from_json(value: &Value) -> Result<Part> {
        let measure = |v: &Value| MeasureKey::from_json(v.get("measure").unwrap_or(&Value::Null));
        let figures = value
            .get("figures")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|f| {
                let scenario = f.get("scenario_key").cloned().unwrap_or(Value::Null);
                Ok(Figure {
                    measure: measure(f)?,
                    account_id: f["account_id"].as_str().unwrap_or_default().to_string(),
                    breakdown: f["breakdown"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or(&[])
                        .iter()
                        .map(|b| {
                            (
                                b["breakdown"].as_str().unwrap_or_default().to_string(),
                                b["value"].as_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect(),
                    scenario: Scenario {
                        kind: name(&scenario["kind"])?,
                        business_date: scenario["business_date"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        outcome_instrument_id: scenario["outcome_instrument_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        scenario_row_key: scenario["scenario_row_key"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        scenario_version: scenario["scenario_version"]
                            .as_str()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0),
                        axis_positions: scenario["axis_positions"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|p| p.as_u64())
                                    .map(|p| p as u32)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    },
                    computed_as: name(&f["computed_as"]).unwrap_or(EXACT),
                    series_key: series_key(&f["series_key"]),
                    oldest_input_source_ns: f["oldest_input_source_ns"]
                        .as_str()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    value: decimal(&f["value"])?,
                    numerator: decimal(&f["numerator"])?,
                    denominator: decimal(&f["denominator"])?,
                    currency_instrument_id: f["currency_instrument_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect::<Result<_>>()?;
        let not_evaluated = value
            .get("not_evaluated")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|u| {
                Ok(Unevaluated {
                    account_id: u["account_id"].as_str().unwrap_or_default().to_string(),
                    instrument_id: u["instrument_id"].as_str().unwrap_or_default().to_string(),
                    measure: measure(u)?,
                    reason: name(&u["reason"])?,
                    detail: u["detail"].as_str().unwrap_or_default().to_string(),
                    series_key: series_key(&u["series_key"]),
                })
            })
            .collect::<Result<_>>()?;
        let read = &value["reads"];
        Ok(Part {
            figures,
            not_evaluated,
            reads: Reads {
                datasets: strings(&read["datasets"]).into_iter().collect(),
                instruments: strings(&read["instruments"]).into_iter().collect(),
                models: read["models"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .map(|m| {
                        (
                            m[0].as_str().unwrap_or_default().to_string(),
                            m[1].as_i64().unwrap_or(1),
                        )
                    })
                    .collect(),
                groups: strings(&read["groups"]).into_iter().collect(),
                input_prices: read["input_prices"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .map(|p| {
                        (
                            p["instrument_id"].as_str().unwrap_or_default().to_string(),
                            p.clone(),
                        )
                    })
                    .collect(),
                legs: read["legs"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .map(|leg| {
                        let text = |name: &str| leg[name].as_str().unwrap_or_default().to_string();
                        (
                            (
                                text("from_instrument_id"),
                                text("to_instrument_id"),
                                crate::model::derivation_rank(&text("derivation")),
                                text("via_instrument_id"),
                            ),
                            leg.clone(),
                        )
                    })
                    .collect(),
            },
            execution_costs: value["execution_costs"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        })
    }
}
