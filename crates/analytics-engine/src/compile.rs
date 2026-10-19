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
    if job.account_ids.is_empty() {
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
                refused(
                    format!("measures[{i}].version"),
                    format!(
                        "the component serves {} at version 1, not {}",
                        measure.definition, measure.version
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
        if matches!(
            definition.definition,
            "historical_var" | "expected_shortfall"
        ) {
            reads.history_dates = reads.history_dates.max(historical::window(definition) + 1);
        }
        measures.push(key(measure));
        served.push(definition);
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
}
