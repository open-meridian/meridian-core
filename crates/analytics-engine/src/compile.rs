//! The typed builders' messages, compiled (the one-API spec's requirements
//! 11 to 14; Q1): a `Job` as the SDK's builders and a page write it,
//! checked against the definitions the component serves, and the reads it
//! needs named, before anything is read.
//!
//! A job is refused here, naming the input, when it asks what the component
//! cannot answer at all (W12.1): no account, a definition or version not
//! served, an event measure naming no group, a what-if over an account the
//! job is not over, a trade with no instrument or quantity. A breakdown the
//! deployment holds no scheme for is not refused: it is answered not
//! evaluated, never guessed (requirement 17).

use std::collections::BTreeSet;

use meridian_domain::v1::{Job, MeasureRef};

use crate::case::MeasureKey;
use crate::definitions::{self, Definition};
use crate::historical;

/// A refusal: the input's path in the job (`measures[1].version`) and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub field: String,
    pub words: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.words)
    }
}

/// What a job reads, named before it reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reads {
    /// The accounts, in the job's order.
    pub accounts: Vec<String>,
    /// The event groups the event measures name.
    pub event_groups: BTreeSet<String>,
    /// Whether a measure needs the closed lots (pnl's realized).
    pub closed_lots: bool,
    /// The history's business dates a measure needs (window + 1), 0 for none.
    pub history_dates: usize,
    /// Instruments a what-if adds, beyond the positions held.
    pub what_if_instruments: BTreeSet<String>,
    /// Contract v27 (W12.1): whether a measure reads the accounts'
    /// executions and their quotes (`execution_cost`).
    pub executions: bool,
    /// Contract v28 (W12.1): whether a measure reads a chain, and whether
    /// the scenario grid reads the accounts' options' chains.
    pub chains: bool,
    pub held_chains: bool,
    /// Contract v29 (W12.1, W12.2): whether a measure reads the debt
    /// records' terms and the job's date, the factors' history, and the
    /// Treasury curve (and the make-whole benchmarks' curves).
    pub debt: bool,
    pub factors: bool,
    pub treasury_curve: bool,
    pub benchmark_curves: bool,
}

/// A job, compiled: its measures as the answer names them back, and its
/// reads.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub measures: Vec<MeasureKey>,
    pub definitions: Vec<&'static Definition>,
    pub reads: Reads,
}

fn refused(field: impl Into<String>, words: impl Into<String>) -> Refused {
    Refused {
        field: field.into(),
        words: words.into(),
    }
}

fn key(measure: &MeasureRef) -> MeasureKey {
    MeasureKey {
        definition: measure.definition.clone(),
        version: measure.version as i64,
        event_group_id: measure.event_group_id.clone(),
        breakdowns: measure.breakdowns.clone(),
    }
}

/// Compile a job, or refuse it naming the input.
pub fn compile(job: &Job) -> Result<Compiled, Refused> {
    // Contract v28 (W12.1): a job asking only chain measures may name no
    // account; any other measure is over one account or more.
    // Contract v29: nor a job asking only the Treasury curve.
    let chains_only = !job.measures.is_empty()
        && job.measures.iter().all(|m| {
            crate::chains::is_chain_measure(&m.definition) || m.definition == "treasury_curve"
        });
    if job.account_ids.is_empty() && !chains_only {
        return Err(refused("account_ids", "a job is over one account or more"));
    }
    for (i, account) in job.account_ids.iter().enumerate() {
        if account.is_empty() {
            return Err(refused(format!("account_ids[{i}]"), "an account is named"));
        }
    }
    if job.measures.is_empty() {
        return Err(refused("measures", "a job asks one measure or more"));
    }
    // Contract v27 (W12.1; ruling 458): a business date, never beside a
    // watermark or an as-of; the component refuses one after today's.
    if !job.business_date.is_empty() {
        if meridian_domain::date::Date::parse(&job.business_date).is_err() {
            return Err(refused("business_date", "a date is YYYY-MM-DD"));
        }
        let inputs = job.inputs.clone().unwrap_or_default();
        if inputs.book_at.is_some() || inputs.lake_as_of_ns != 0 {
            return Err(refused(
                "business_date",
                "a job reads at a business date or at a watermark and an as-of, never both",
            ));
        }
    }
    let mut reads = Reads {
        accounts: job.account_ids.clone(),
        ..Reads::default()
    };
    let mut measures = Vec::with_capacity(job.measures.len());
    let mut served = Vec::with_capacity(job.measures.len());
    for (i, measure) in job.measures.iter().enumerate() {
        let Some(definition) = definitions::definition(&measure.definition, measure.version) else {
            let named = definitions::DEFINITIONS
                .iter()
                .any(|d| d.definition == measure.definition);
            return Err(if named {
                let versions: Vec<String> = definitions::DEFINITIONS
                    .iter()
                    .filter(|d| d.definition == measure.definition)
                    .map(|d| d.version.to_string())
                    .collect();
                refused(
                    format!("measures[{i}].version"),
                    format!(
                        "the component serves {} at version {}, not {}",
                        measure.definition,
                        versions.join(" or "),
                        measure.version
                    ),
                )
            } else {
                refused(
                    format!("measures[{i}].definition"),
                    format!(
                        "the component serves no definition {:?}",
                        measure.definition
                    ),
                )
            });
        };
        if definition.is_event_measure() {
            if measure.event_group_id.is_empty() {
                return Err(refused(
                    format!("measures[{i}].event_group_id"),
                    format!(
                        "{} is over an event group, and names none",
                        measure.definition
                    ),
                ));
            }
            reads.event_groups.insert(measure.event_group_id.clone());
        } else if !measure.event_group_id.is_empty() {
            return Err(refused(
                format!("measures[{i}].event_group_id"),
                format!("{} is not over an event group", measure.definition),
            ));
        }
        if definition.definition == "pnl" {
            reads.closed_lots = true;
        }
        if definition.definition == "execution_cost" {
            reads.executions = true;
        }
        // Contract v28 (W12.1, W12.8).
        if crate::chains::is_chain_measure(definition.definition) {
            if job.chains.is_empty() {
                return Err(refused(
                    "chains",
                    format!(
                        "{} is over a chain, and the job names none",
                        measure.definition
                    ),
                ));
            }
            reads.chains = true;
        }
        if definition.definition == "scenario_grid" {
            if job.scenario_set.is_none() && job.scenario_ref.is_none() {
                return Err(refused(
                    "scenario_set",
                    "scenario_grid is over a scenario set: inline, or saved by scenario_ref",
                ));
            }
            reads.chains = true;
            reads.held_chains = true;
        }
        // Contract v29 (W12.1, W12.2).
        if crate::bonds::measures::is_position_measure(definition.definition)
            || definition.definition == "treasury_curve"
        {
            reads.debt = true;
        }
        if crate::bonds::measures::reads_factors(definition.definition) {
            reads.factors = true;
        }
        if crate::bonds::measures::reads_curve(definition.definition) {
            reads.treasury_curve = true;
        }
        if matches!(
            definition.definition,
            "make_whole_amount" | "bond_spread" | "bond_price"
        ) {
            reads.benchmark_curves = true;
        }
        if matches!(
            definition.definition,
            "historical_var" | "expected_shortfall"
        ) {
            reads.history_dates = reads.history_dates.max(historical::window(definition) + 1);
        }
        measures.push(key(measure));
        served.push(definition);
    }
    // Contract v28 (W12.1): each chain names its underlying, and its window
    // is two plain dates in order.
    for (i, chain) in job.chains.iter().enumerate() {
        if chain.underlying_instrument_id.trim().is_empty() {
            return Err(refused(
                format!("chains[{i}].underlying_instrument_id"),
                "a chain names its underlying",
            ));
        }
        for (name, date) in [
            ("expiration_from", &chain.expiration_from),
            ("expiration_to", &chain.expiration_to),
        ] {
            if !date.is_empty() && meridian_domain::date::Date::parse(date).is_err() {
                return Err(refused(
                    format!("chains[{i}].{name}"),
                    "a date is YYYY-MM-DD",
                ));
            }
        }
        if !chain.expiration_from.is_empty()
            && !chain.expiration_to.is_empty()
            && chain.expiration_to < chain.expiration_from
        {
            return Err(refused(
                format!("chains[{i}].expiration_to"),
                "a window ends on or after it starts",
            ));
        }
        for (k, key) in chain.series_keys.iter().enumerate() {
            if key.scheme.is_empty() || key.value.is_empty() {
                return Err(refused(
                    format!("chains[{i}].series_keys[{k}]"),
                    "a series key states its scheme and value",
                ));
            }
        }
    }
    // Contract v28 (W12.8; rulings 581 to 583): an inline set, or a saved
    // one by its key, never both.
    if let Some(set) = &job.scenario_set {
        if job.scenario_ref.is_some() {
            return Err(refused("scenario_ref", "refused beside scenario_set"));
        }
        if let Some((field, words)) = crate::scenario::wire_refused("scenario_set", set) {
            return Err(refused(field, words));
        }
    }
    if let Some(saved) = &job.scenario_ref {
        if saved.row_key.trim().is_empty() {
            return Err(refused(
                "scenario_ref.row_key",
                "a saved scenario is named by its row key",
            ));
        }
        if saved.dataset.trim().is_empty() {
            return Err(refused(
                "scenario_ref.dataset",
                "a saved scenario names its dataset",
            ));
        }
    }
    if let Some(what_if) = &job.what_if {
        if !job.account_ids.contains(&what_if.account_id) {
            return Err(refused(
                "what_if.account_id",
                "a what-if adds trades to one of the job's accounts",
            ));
        }
        for (i, trade) in what_if.trades.iter().enumerate() {
            if trade.instrument_id.is_empty() {
                return Err(refused(
                    format!("what_if.trades[{i}].instrument_id"),
                    "a trade names its instrument",
                ));
            }
            if trade.quantity.is_none() {
                return Err(refused(
                    format!("what_if.trades[{i}].quantity"),
                    "a trade states its quantity",
                ));
            }
            if !(1..=4).contains(&trade.side) {
                return Err(refused(
                    format!("what_if.trades[{i}].side"),
                    "a trade is a buy or a sell (short or not)",
                ));
            }
            reads
                .what_if_instruments
                .insert(trade.instrument_id.clone());
        }
    }
    Ok(Compiled {
        measures,
        definitions: served,
        reads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{WhatIf, WhatIfTrade};

    fn measure(definition: &str, version: i32, group: &str) -> MeasureRef {
        MeasureRef {
            definition: definition.into(),
            version,
            breakdowns: vec!["sector".into()],
            event_group_id: group.into(),
        }
    }

    #[test]
    fn a_job_compiles_to_its_reads_or_is_refused_naming_the_input() {
        let job = Job {
            account_ids: vec!["ACC-1".into()],
            inputs: None,
            measures: vec![
                measure("pnl", 1, ""),
                measure("historical_var", 1, ""),
                measure("implied_distribution", 1, "EVG-1"),
            ],
            what_if: None,
            ..Default::default()
        };
        let compiled = compile(&job).unwrap();
        assert!(compiled.reads.closed_lots);
        assert_eq!(compiled.reads.history_dates, 501);
        assert!(compiled.reads.event_groups.contains("EVG-1"));
        // A breakdown with no scheme compiles: it is answered not evaluated.
        assert_eq!(compiled.measures[0].breakdowns, vec!["sector".to_string()]);

        let mut bad = job.clone();
        bad.measures[1].version = 2;
        assert_eq!(compile(&bad).unwrap_err().field, "measures[1].version");
        let mut bad = job.clone();
        bad.measures[0].definition = "sleeve_attribution".into();
        assert_eq!(compile(&bad).unwrap_err().field, "measures[0].definition");
        let mut bad = job.clone();
        bad.measures[2].event_group_id.clear();
        assert_eq!(
            compile(&bad).unwrap_err().field,
            "measures[2].event_group_id"
        );
        let mut bad = job.clone();
        bad.what_if = Some(WhatIf {
            account_id: "ACC-9".into(),
            trades: vec![WhatIfTrade::default()],
        });
        assert_eq!(compile(&bad).unwrap_err().field, "what_if.account_id");
        let mut bad = job.clone();
        bad.account_ids.clear();
        assert_eq!(compile(&bad).unwrap_err().field, "account_ids");
    }

    #[test]
    fn a_chain_job_names_its_chains_and_a_grid_its_scenario_set() {
        use meridian_domain::v1::{ChainRef, Scenario, ScenarioAxis, ScenarioRef, ScenarioSet};
        let chain = ChainRef {
            underlying_instrument_id: "INS-AAPL".into(),
            expiration_from: "2026-11-20".into(),
            expiration_to: "2026-11-20".into(),
            series_keys: Vec::new(),
        };
        let job = Job {
            chains: vec![chain.clone()],
            measures: vec![
                measure("implied_volatility", 1, ""),
                measure("greeks", 1, ""),
            ],
            ..Default::default()
        };
        let compiled = compile(&job).unwrap();
        assert!(compiled.reads.chains && compiled.reads.accounts.is_empty());
        let mut bad = job.clone();
        bad.chains.clear();
        assert_eq!(compile(&bad).unwrap_err().field, "chains");
        let mut bad = job.clone();
        bad.chains[0].expiration_to = "2026-11-19".into();
        assert_eq!(compile(&bad).unwrap_err().field, "chains[0].expiration_to");

        let mut grid = Job {
            account_ids: vec!["ACC-1".into()],
            measures: vec![measure("scenario_grid", 1, "")],
            ..Default::default()
        };
        assert_eq!(compile(&grid).unwrap_err().field, "scenario_set");
        grid.scenario_set = Some(ScenarioSet {
            name: "grid".into(),
            axes: vec![ScenarioAxis {
                name: "spot".into(),
                steps: vec![Scenario {
                    name: "base".into(),
                    shocks: Vec::new(),
                }],
            }],
        });
        assert!(compile(&grid).unwrap().reads.held_chains);
        grid.scenario_ref = Some(ScenarioRef {
            row_key: "grid:1".into(),
            dataset: "sample-options-1:scenarios".into(),
            version: 1,
        });
        assert_eq!(compile(&grid).unwrap_err().field, "scenario_ref");
        grid.scenario_ref = None;
        grid.scenario_set.as_mut().unwrap().axes[0].steps[0]
            .name
            .clear();
        assert_eq!(
            compile(&grid).unwrap_err().field,
            "scenario_set.axes[0].steps[0].name"
        );
    }
}
