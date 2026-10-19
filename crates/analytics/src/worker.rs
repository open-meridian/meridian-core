//! The worker pool (the one-API spec's requirements 25, 30 and 31): every
//! worker of one image runs every node kind, taking units from the queue
//! group its copies share on [`topics::COMPUTE_UNIT`], each answered by one
//! worker. A unit is named by its inputs' hash, so a unit asked again --
//! after a worker died, or a timeout -- is the same unit, answered from the
//! cache or computed again to the same part.
//!
//! What crosses: the inputs document sliced to the unit (the job's accounts
//! but the unit's, its positions alone), its hash, and the unit; back, the
//! unit's part as JSON. A unit whose slice is past what the broker carries
//! in one message is computed by the coordinator itself, the same code to
//! the same part.

use std::sync::Arc;
use std::time::Duration;

use meridian_analytics_engine::case::{Case, CaseError};
use meridian_analytics_engine::graph::{unit_hash, Cache, Executor, Job};
use meridian_analytics_engine::measures::{self, Part, Unit};
use meridian_bus::Bus;
use serde_json::{json, Value};

use crate::topics;

/// What a unit's request is typed as on the component's own topic.
pub const UNIT_TYPE: &str = "application/vnd.open-meridian.analytics-unit+json";
pub const PART_TYPE: &str = "application/vnd.open-meridian.analytics-part+json";

/// The largest unit request sent to a worker: under the broker's one
/// megabyte, with room for the envelope.
pub const MOST_UNIT_BYTES: usize = 900 * 1024;

/// How many times a unit is asked before the coordinator computes it.
const ASKS: usize = 3;

/// The document a unit reads: its account's positions and closed lots
/// alone, the rest whole. The unit's part is the same over this as over the
/// whole document: a unit reads its own account's rows, and the measures'
/// preambles, which read across accounts, ran in the coordinator.
pub fn slice(inputs: &Value, unit: &Unit) -> Value {
    let mut out = inputs.clone();
    if let Some(account) = &unit.account {
        for list in ["positions", "closed_lots"] {
            if let Some(Value::Array(rows)) = out.get_mut(list) {
                rows.retain(|row| row["account_id"] == account.as_str());
            }
        }
    }
    out
}

/// A unit's request.
pub fn request(inputs_hash: &str, sliced: &Value, unit: &Unit) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "inputs_hash": inputs_hash,
        "inputs": sliced,
        "unit": {"measure": unit.measure, "account": unit.account},
    }))
    .expect("a unit's request is JSON")
}

/// Answer a unit's request: its part, from the cache by the unit's name or
/// computed.
pub fn answer(cache: &Cache, payload: &[u8]) -> Result<Part, String> {
    let request: Value = serde_json::from_slice(payload)
        .map_err(|failed| format!("a unit did not read: {failed}"))?;
    let inputs_hash = request["inputs_hash"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let unit = Unit {
        measure: request["unit"]["measure"].as_u64().unwrap_or(0) as usize,
        account: request["unit"]["account"].as_str().map(str::to_string),
    };
    let name = unit_hash(&inputs_hash, &unit);
    if let Some(part) = cache.get(&name) {
        return Ok(part);
    }
    let case = Case::from_json(&request["inputs"]).map_err(|failed| failed.0)?;
    if unit.measure >= case.measures.len() {
        return Err(format!("the job asks no measure {}", unit.measure));
    }
    let part = measures::compute(&case, &unit).map_err(|failed| failed.0)?;
    cache.put(name, part.clone());
    Ok(part)
}

/// Serve units on the pool's queue group.
pub fn serve(bus: &Arc<Bus>, cache: Arc<Cache>) {
    bus.serve(topics::COMPUTE_UNIT, move |envelope| {
        if envelope.payload_type != UNIT_TYPE {
            return Err(format!(
                "expected {UNIT_TYPE}, got {}",
                envelope.payload_type
            ));
        }
        let part = answer(&cache, &envelope.payload)?;
        Ok((
            PART_TYPE.to_string(),
            serde_json::to_vec(&part.to_json()).expect("a part is JSON"),
        ))
    });
}

/// The coordinator's executor: each unit asked of the pool, every unit in
/// flight at once, each asked again on a timeout, and computed here where
/// the pool does not answer or the unit is too large to send.
pub struct Pool {
    pub bus: Arc<Bus>,
    pub handle: tokio::runtime::Handle,
    pub timeout: Duration,
}

impl Pool {
    async fn ask(&self, payload: Vec<u8>) -> Result<Part, String> {
        let mut last = String::new();
        for _ in 0..ASKS {
            match self
                .bus
                .call(
                    topics::COMPUTE_UNIT,
                    UNIT_TYPE,
                    payload.clone(),
                    None,
                    Some(self.timeout),
                )
                .await
            {
                Ok((_, reply)) => {
                    let value: Value = serde_json::from_slice(&reply)
                        .map_err(|failed| format!("a part did not read: {failed}"))?;
                    return Part::from_json(&value).map_err(|failed| failed.0);
                }
                Err(failed) => last = failed.to_string(),
            }
        }
        Err(last)
    }
}

impl Executor for Pool {
    fn run(&self, job: &Job<'_>, units: &[Unit]) -> Vec<Result<Part, CaseError>> {
        let asks = units.iter().map(|unit| {
            let sliced = slice(job.inputs, unit);
            let payload = request(job.inputs_hash, &sliced, unit);
            async move {
                if payload.len() > MOST_UNIT_BYTES {
                    return None;
                }
                match self.ask(payload).await {
                    Ok(part) => Some(part),
                    Err(failed) => {
                        tracing::warn!(%failed, "the worker pool did not answer a unit; computed here");
                        None
                    }
                }
            }
        });
        // Called off the runtime's own threads (a handler's blocking thread,
        // or a job finishing later), where blocking on it is allowed.
        let answered = self.handle.block_on(futures_util::future::join_all(asks));
        answered
            .into_iter()
            .zip(units)
            .map(|(part, unit)| match part {
                Some(part) => Ok(part),
                None => measures::compute(job.case, unit),
            })
            .collect()
    }
}
