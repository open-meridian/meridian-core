//! The analytics engine: v26's models and measures over a job's inputs,
//! exact, with the answer's provenance and hash
//! (spec/the-analytics-engine-answers-behind-one-api; spec/the-analytics-
//! engines-first-release; W12).
//!
//! Our own Rust, under core's licence (ruled 2026-10-12). Pure: nothing here
//! reads a store, the bus or a clock. The component (`meridian-analytics`)
//! resolves a job's inputs on its client's behalf into an **inputs
//! document** ([`case`]), and this crate answers it:
//!
//! - [`compile`] checks a job's measures against the definitions served
//!   and names what it must read -- the typed builders' messages, compiled;
//! - [`graph`] splits the answer into units, each a pure node named by the
//!   hash of its inputs and the engine's version, so a unit computed twice,
//!   or on another worker, is the same unit;
//! - [`measures`] and [`historical`] compute each unit; [`model`] values an
//!   instrument by its template's model, built in;
//! - [`exact`] is the arithmetic: 256-bit intermediates with a checked
//!   128-bit fast path, unbounded past that, every figure rounded once, half
//!   to even (ENG Q11);
//! - [`canonical`] is RFC 8785's canonical JSON and the answer's hash (Q7).
//!
//! The SDK's reference computations (`meridian.analytics.reference`) are
//! what this must equal, figure for figure and hash for hash (requirement
//! 36): the pinned cases are vendored beside this crate and held to it by
//! its tests.

pub mod answer;
pub mod canonical;
pub mod case;
pub mod chains;
pub mod compile;
pub mod definitions;
pub mod exact;
pub mod graph;
pub mod historical;
pub mod measures;
pub mod model;
pub mod options;
pub mod pricing;
pub mod scenario;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod chains_tests;
#[cfg(test)]
mod dev_chain_tests;
#[cfg(test)]
mod tests;

pub use answer::Answer;
pub use case::{Case, CaseError};
pub use graph::{Executor, Sequential, Threads};

use serde_json::Value;

/// The answer to an inputs document, its units run by `executor`.
pub fn compute_with(inputs: &Value, executor: &dyn Executor) -> Result<Answer, CaseError> {
    let hash = canonical::answer_hash(inputs).map_err(|failed| CaseError(failed.to_string()))?;
    let case = Case::from_json(inputs)?;
    graph::answer(&case, inputs, &hash, executor)
}

/// The answer to an inputs document, its units run here, one after another.
pub fn compute(inputs: &Value) -> Result<Answer, CaseError> {
    compute_with(inputs, &Sequential)
}
