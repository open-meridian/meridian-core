//! The pricing seam (W12.3; the one-API spec's requirements 18 to 23): a
//! batch of instruments sharing a model, priced at once, by column.
//!
//! The built-in models price a batch laid out by column ([`Columns`]): each
//! instrument's own price (or settlement value) and its conversion legs,
//! flattened, so a batch sizes its output before it loops and an iteration
//! never allocates (requirement 31). A `pricing` plugin is handed the same
//! batch as market objects ([`unit_inputs`]) and must answer what
//! [`crate::model::value_per_unit`] answers.

use crate::case::{Case, Price};
use crate::exact::Dec;
use crate::model;

/// A batch by column: for instrument `i`, `amounts[i]` times the legs
/// `legs[starts[i]..starts[i + 1]]`.
#[derive(Debug, Clone, Default)]
pub struct Columns {
    pub instruments: Vec<String>,
    pub amounts: Vec<Dec>,
    pub legs: Vec<Dec>,
    pub starts: Vec<usize>,
}

impl Columns {
    pub fn push(&mut self, instrument: &str, amount: Dec, legs: &[Dec]) {
        if self.starts.is_empty() {
            self.starts.push(0);
        }
        self.instruments.push(instrument.to_string());
        self.amounts.push(amount);
        self.legs.extend_from_slice(legs);
        self.starts.push(self.legs.len());
    }

    pub fn len(&self) -> usize {
        self.amounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.amounts.is_empty()
    }

    /// Each instrument's value per unit, into `out`, sized by the caller.
    pub fn price(&self, out: &mut [Dec]) {
        for (i, slot) in out.iter_mut().enumerate().take(self.amounts.len()) {
            let mut value = self.amounts[i].clone();
            for leg in &self.legs[self.starts[i]..self.starts[i + 1]] {
                value = value.mul(leg);
            }
            *slot = value;
        }
    }
}

/// The market objects a model declares for one instrument, valued in
/// `base`: what a `pricing` plugin is handed for its unit (requirement 20),
/// as (subject, kind, amount, instrument) -- its own price (or, for a
/// `binary_event` after a final determination, its outcome), then each
/// conversion leg. None where the built-in model would not value it.
pub fn unit_inputs(case: &Case, instrument_id: &str, base: &str) -> Option<Vec<Price>> {
    let record = case.instruments.get(instrument_id)?;
    let mut out = Vec::new();
    let held = match record.model_id.as_str() {
        "cash" => instrument_id.to_string(),
        "binary_event" if case.outcomes.get(instrument_id).is_some_and(|o| o.final_) => {
            let outcome = &case.outcomes[instrument_id];
            let (_, currency) = record.payout.as_ref()?;
            out.push(Price {
                subject_id: instrument_id.to_string(),
                kind: "outcome".to_string(),
                amount: outcome.settlement_value.clone(),
                instrument_id: currency.clone(),
                dataset: outcome.dataset.clone(),
                valid_at_ns: 0,
                row_key: String::new(),
                version: 0,
                source_ns: 0,
            });
            currency.clone()
        }
        _ => {
            let price = case.price(instrument_id, &record.price_kind)?;
            out.push(price.clone());
            price.instrument_id.clone()
        }
    };
    // Each conversion leg as one market object: the lake's own rate, or
    // from contract v27 a derived one (inverted or crossed), the currency
    // in hand's price in the next at the leg's rate.
    out.extend(model::conversion(case, &held, base).ok()?.into_iter().map(|leg| {
        match (leg.derivation, leg.factors.first()) {
            (crate::answer::DIRECT, Some((price, _))) => price.clone(),
            _ => Price {
                subject_id: leg.from_id,
                kind: "derived".to_string(),
                amount: leg.rate,
                instrument_id: leg.to_id,
                dataset: leg.dataset,
                valid_at_ns: 0,
                row_key: String::new(),
                version: 0,
                source_ns: 0,
            },
        }
    }));
    Some(out)
}
