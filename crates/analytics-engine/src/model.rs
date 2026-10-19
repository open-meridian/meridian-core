//! v26's three built-in models (the first release's §1; Q1, Q2, Q10): each
//! gives a value per unit in the account's base currency, or says why not.
//!
//! - `cash`: exactly 1 for the base currency; any other cash instrument (a
//!   stablecoin's included, never at par by assumption) at its own
//!   conversion.
//! - `quoted_price`: the price of the template's kind times its conversion.
//! - `binary_event`: before the venue's final determination, its price
//!   times its conversion; after it, the settlement value per contract the
//!   Outcome states times the conversion of the payout's currency. A
//!   proposed or disputed outcome keeps the price.
//! - A conversion is leg by leg, each leg a market price of the currency in
//!   hand in another instrument, of kind `close`, then `mid`, then `last`:
//!   the shortest chain to the base, at most three legs, ties to the lowest
//!   kinds and then to the instruments' IDs. No rate is inverted. No chain is
//!   `CONVERSION_MISSING`; a stale leg `PRICE_STALE`.
//! - An `LCL-` record is never priced: `LOCAL_RECORD`, never zero.

use std::collections::BTreeSet;

use crate::answer::{
    Reads, CONVERSION_MISSING, IMPLEMENTATION_UNAVAILABLE, LOCAL_RECORD, NOT_ENTITLED,
    PRICE_MISSING, PRICE_STALE,
};
use crate::case::{Case, CaseError, Price, Result};
use crate::exact::Dec;

/// Conversion legs' kinds, in the order a leg is taken.
pub const CONVERSION_KINDS: [&str; 3] = ["close", "mid", "last"];
/// The longest conversion chain.
pub const MOST_LEGS: usize = 3;

/// A position's value per unit in its base, and the market objects it read
/// (its price, then its conversion legs), which a historical scenario moves.
#[derive(Debug, Clone)]
pub struct Valued {
    pub per_unit: Dec,
    pub objects: Vec<Price>,
}

/// Why a value per unit could not be given: the reason and its detail.
pub type Why = (&'static str, String);

/// The product of the legs' amounts.
pub fn product(legs: &[Price]) -> Dec {
    let mut out = Dec::from_int(1);
    for leg in legs {
        out = out.mul(&leg.amount);
    }
    out
}

/// The legs from `held` to `base`, or why there are none.
pub fn conversion(case: &Case, held: &str, base: &str) -> std::result::Result<Vec<Price>, Why> {
    if held == base {
        return Ok(Vec::new());
    }
    let mut chains: Vec<(Vec<usize>, Vec<String>, Vec<Price>)> = Vec::new();
    let mut seen = BTreeSet::from([held.to_string()]);
    let mut legs = Vec::new();
    walk(case, held, base, &mut legs, &mut seen, &mut chains);
    if chains.is_empty() {
        return Err((CONVERSION_MISSING, String::new()));
    }
    chains.sort_by(|a, b| (a.2.len(), &a.0, &a.1).cmp(&(b.2.len(), &b.0, &b.1)));
    let (_, _, legs) = chains.swap_remove(0);
    if legs.iter().any(|leg| case.stale(leg)) {
        return Err((PRICE_STALE, String::new()));
    }
    Ok(legs)
}

fn walk(
    case: &Case,
    at: &str,
    base: &str,
    legs: &mut Vec<Price>,
    seen: &mut BTreeSet<String>,
    chains: &mut Vec<(Vec<usize>, Vec<String>, Vec<Price>)>,
) {
    if at == base {
        let ranks = legs
            .iter()
            .map(|leg| {
                CONVERSION_KINDS
                    .iter()
                    .position(|k| *k == leg.kind)
                    .unwrap_or(9)
            })
            .collect();
        let ids = legs.iter().map(|leg| leg.instrument_id.clone()).collect();
        chains.push((ranks, ids, legs.clone()));
        return;
    }
    if legs.len() == MOST_LEGS {
        return;
    }
    for kind in CONVERSION_KINDS {
        let Some(leg) = case.price(at, kind) else {
            continue;
        };
        if seen.contains(&leg.instrument_id) {
            continue;
        }
        let next = leg.instrument_id.clone();
        seen.insert(next.clone());
        legs.push(leg.clone());
        walk(case, &next, base, legs, seen, chains);
        legs.pop();
        seen.remove(&next);
    }
}

/// A value per unit in `base`, or why not. Records what it read in `reads`
/// as the reference does: the record and its model at once, a final
/// outcome's dataset at once; the price's and legs' datasets only when the
/// caller uses the value ([`read`]).
pub fn value(
    case: &Case,
    reads: &mut Reads,
    instrument_id: &str,
    base: &str,
) -> Result<std::result::Result<Valued, Why>> {
    let valued = built_in(case, reads, instrument_id, base)?;
    let Some(routed) = case
        .instruments
        .get(instrument_id)
        .and_then(|record| case.routed.get(&record.model_id))
    else {
        return Ok(valued);
    };
    // Routed to a plugin (W12.3): its value per unit, the market objects
    // its model declared kept for the historical scenarios; where it did
    // not answer, not evaluated, naming it -- never the built-in model's in
    // silence (requirement 22).
    let unavailable = || {
        Ok(Err((
            IMPLEMENTATION_UNAVAILABLE,
            routed.implementation.clone(),
        )))
    };
    if !routed.answered {
        return unavailable();
    }
    match (
        valued,
        case.priced
            .get(&(instrument_id.to_string(), base.to_string())),
    ) {
        (Ok(valued), Some(per_unit)) => Ok(Ok(Valued {
            per_unit: per_unit.clone(),
            objects: valued.objects,
        })),
        (Err(why), _) => Ok(Err(why)),
        (Ok(_), None) => unavailable(),
    }
}

/// The built-in model's value per unit (requirement 19).
fn built_in(
    case: &Case,
    reads: &mut Reads,
    instrument_id: &str,
    base: &str,
) -> Result<std::result::Result<Valued, Why>> {
    if instrument_id.starts_with("LCL-") {
        return Ok(Err((LOCAL_RECORD, "local record: not priced".to_string())));
    }
    let record = case.instruments.get(instrument_id).ok_or_else(|| {
        CaseError(format!(
            "the inputs name {instrument_id} and hold no record of it"
        ))
    })?;
    reads.instruments.insert(instrument_id.to_string());
    reads
        .models
        .insert((record.model_id.clone(), record.model_version));
    match record.model_id.as_str() {
        "cash" => {
            return Ok(conversion(case, instrument_id, base).map(|legs| Valued {
                per_unit: product(&legs),
                objects: legs,
            }));
        }
        "binary_event" => {
            if let Some(outcome) = case.outcomes.get(instrument_id).filter(|o| o.final_) {
                if !outcome.dataset.is_empty() {
                    reads.datasets.insert(outcome.dataset.clone());
                }
                let (_, currency) = record.payout.as_ref().ok_or_else(|| {
                    CaseError(format!(
                        "{instrument_id} is a binary event contract with no payout"
                    ))
                })?;
                return Ok(conversion(case, currency, base).map(|legs| Valued {
                    per_unit: outcome.settlement_value.mul(&product(&legs)),
                    objects: legs,
                }));
            }
        }
        "quoted_price" => {}
        other => {
            return Err(CaseError(format!(
                "{instrument_id}'s model {other:?} is not one of v26's"
            )))
        }
    }
    let Some(price) = case.price(instrument_id, &record.price_kind) else {
        if let Some(dataset) = case
            .unentitled
            .get(&(instrument_id.to_string(), record.price_kind.clone()))
        {
            return Ok(Err((NOT_ENTITLED, dataset.clone())));
        }
        return Ok(Err((PRICE_MISSING, record.price_kind.clone())));
    };
    if case.stale(price) {
        return Ok(Err((PRICE_STALE, price.dataset.clone())));
    }
    Ok(conversion(case, &price.instrument_id, base).map(|legs| {
        let per_unit = price.amount.mul(&product(&legs));
        let mut objects = Vec::with_capacity(legs.len() + 1);
        objects.push(price.clone());
        objects.extend(legs);
        Valued { per_unit, objects }
    }))
}

/// The datasets a used value read.
pub fn read(reads: &mut Reads, valued: &Valued) {
    for object in &valued.objects {
        if !object.dataset.is_empty() {
            reads.datasets.insert(object.dataset.clone());
        }
    }
}

/// A built-in model's value per unit for one pricing unit (W12.3), from
/// exactly the market objects a batch carries: the instrument's own price
/// (for `binary_event`, its `outcome` where one is carried), times each
/// conversion leg the inputs hold from the currency in hand, until none
/// leads further. The value and the instrument it is in. The SDK
/// reference's `value_per_unit`, which the sample `pricing` plugin equals.
pub fn value_per_unit(
    model_id: &str,
    instrument_id: &str,
    inputs: &[(String, String, Dec, String)],
) -> std::result::Result<(Dec, String), String> {
    if !crate::definitions::MODELS.contains(&model_id) {
        return Err(format!("{model_id:?} is not one of v26's models"));
    }
    let (mut value, mut held) = if model_id == "cash" {
        (Dec::from_int(1), instrument_id.to_string())
    } else {
        let own = |outcome: bool| {
            inputs.iter().find(|(subject, kind, _, _)| {
                subject == instrument_id && (kind == "outcome") == outcome
            })
        };
        let chosen = if model_id == "binary_event" {
            own(true).or_else(|| own(false))
        } else {
            own(false)
        };
        let Some((_, _, amount, currency)) = chosen else {
            return Err(format!(
                "no price of {instrument_id} among the batch's inputs"
            ));
        };
        (amount.clone(), currency.clone())
    };
    let mut visited = BTreeSet::new();
    loop {
        let leg = inputs.iter().find(|(subject, _, _, _)| {
            (model_id == "cash" || subject != instrument_id)
                && *subject == held
                && !visited.contains(subject)
        });
        let Some((subject, _, amount, currency)) = leg else {
            return Ok((value, held));
        };
        visited.insert(subject.clone());
        value = value.mul(amount);
        held = currency.clone();
    }
}
