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

use serde_json::{json, Value};

use crate::answer::{
    Reads, CONVERSION_MISSING, CROSSED, DIRECT, IMPLEMENTATION_UNAVAILABLE, INVERTED, LOCAL_RECORD,
    NOT_ENTITLED, PRICE_MISSING, PRICE_STALE,
};
use crate::case::{Case, CaseError, Price, Result};
use crate::exact::big::Big;
use crate::exact::{Dec, Ratio};

/// Conversion legs' kinds, in the order a leg is taken.
pub const CONVERSION_KINDS: [&str; 3] = ["close", "mid", "last"];
/// The longest conversion chain, a cross counting as its two sub-legs.
pub const MOST_LEGS: usize = 3;
/// The places an inverted or crossed rate, and a quoted price's value per
/// unit, are rounded to, half to even (rulings 453, 462 and 486).
pub const RATE_SCALE: u32 = 18;

/// One conversion leg (contract v27, ruling 462): from one cash instrument
/// to another at a rate -- the lake's own, its inverse rounded half to even
/// to 18 places, or a cross through a pivot, its two sub-legs' rates
/// multiplied and rounded so -- with the lake's prices it moves with, each
/// with its exponent (1, or -1 inverted).
#[derive(Debug, Clone)]
pub struct Leg {
    pub from_id: String,
    pub to_id: String,
    pub rate: Dec,
    pub derivation: &'static str,
    pub via: String,
    pub dataset: String,
    pub factors: Vec<(Price, i32)>,
    pub subs: Vec<Leg>,
    pub kind_rank: usize,
}

impl Leg {
    fn direct(price: &Price, rank: usize) -> Leg {
        Leg {
            from_id: price.subject_id.clone(),
            to_id: price.instrument_id.clone(),
            rate: price.amount.clone(),
            derivation: DIRECT,
            via: String::new(),
            dataset: price.dataset.clone(),
            factors: vec![(price.clone(), 1)],
            subs: Vec::new(),
            kind_rank: rank,
        }
    }

    /// As the provenance names it (W12.7).
    pub fn to_json(&self) -> Value {
        json!({
            "from_instrument_id": self.from_id,
            "to_instrument_id": self.to_id,
            "rate": self.rate.text(),
            "derivation": self.derivation,
            "via_instrument_id": self.via,
            "dataset": self.dataset,
        })
    }
}

/// A derivation's place in a tie (direct, then inverted, then crossed).
pub fn derivation_rank(derivation: &str) -> u8 {
    match derivation {
        DIRECT => 0,
        INVERTED => 1,
        _ => 2,
    }
}

/// A position's value per unit in its base, the market objects it read
/// with how each moves it (its exponent), its own price where its model
/// read one, and its conversion legs.
#[derive(Debug, Clone)]
pub struct Valued {
    pub per_unit: Dec,
    pub factors: Vec<(Price, i32)>,
    pub own: Option<Price>,
    pub legs: Vec<Leg>,
}

fn valued(per_unit: Dec, legs: Vec<Leg>, own: Option<Price>) -> Valued {
    let mut factors: Vec<(Price, i32)> = Vec::new();
    if let Some(own) = &own {
        factors.push((own.clone(), 1));
    }
    for leg in &legs {
        factors.extend(leg.factors.iter().cloned());
    }
    Valued {
        per_unit,
        factors,
        own,
        legs,
    }
}

/// Why a value per unit could not be given: the reason and its detail.
pub type Why = (&'static str, String);

/// The product of the legs' rates.
pub fn product(legs: &[Leg]) -> Dec {
    let mut out = Dec::from_int(1);
    for leg in legs {
        out = out.mul(&leg.rate);
    }
    out
}

/// A chain's length against [`MOST_LEGS`]: a cross counts as its two.
fn counted(legs: &[Leg]) -> usize {
    legs.iter()
        .map(|leg| if leg.derivation == CROSSED { 2 } else { 1 })
        .sum()
}

/// 1 / x, rounded half to even to 18 places.
fn inverse(x: &Dec) -> Dec {
    Ratio::new(Big::pow10(x.scale()), x.integer().to_big()).round_half_even(RATE_SCALE)
}

/// The lake's own rate from `at` to `to`, by kind in order.
fn direct(case: &Case, at: &str, to: &str) -> Option<Leg> {
    CONVERSION_KINDS
        .iter()
        .enumerate()
        .find_map(|(rank, kind)| case.price_in(at, kind, to).map(|p| Leg::direct(p, rank)))
}

/// The lake's rate the other way (`to` priced in `at`), inverted (ruling
/// 462).
fn inverted(case: &Case, at: &str, to: &str) -> Option<Leg> {
    CONVERSION_KINDS
        .iter()
        .enumerate()
        .find_map(|(rank, kind)| {
            let price = case.price_in(to, kind, at)?;
            if price.amount.is_zero() {
                return None;
            }
            Some(Leg {
                from_id: at.to_string(),
                to_id: to.to_string(),
                rate: inverse(&price.amount),
                derivation: INVERTED,
                via: String::new(),
                dataset: price.dataset.clone(),
                factors: vec![(price.clone(), -1)],
                subs: Vec::new(),
                kind_rank: rank,
            })
        })
}

/// A fiat leg with no direct rate in the lake's direction: inverted, else
/// crossed through the first pivot both sub-legs reach, each direct or
/// inverted, its rate their product rounded half to even to 18 places
/// (rulings 462 and 483). None for a token's leg.
fn derived(case: &Case, at: &str, to: &str) -> Option<Leg> {
    if !(case.fiat(at) && case.fiat(to)) {
        return None;
    }
    if let Some(leg) = inverted(case, at, to) {
        return Some(leg);
    }
    let either =
        |from: &str, into: &str| direct(case, from, into).or_else(|| inverted(case, from, into));
    for pivot in &case.pivots {
        if pivot == at || pivot == to || !case.fiat(pivot) {
            continue;
        }
        let (Some(first), Some(second)) = (either(at, pivot), either(pivot, to)) else {
            continue;
        };
        let rate = first.rate.mul(&second.rate).round_half_even(RATE_SCALE);
        let mut factors = first.factors.clone();
        factors.extend(second.factors.iter().cloned());
        let kind_rank = first.kind_rank.max(second.kind_rank);
        return Some(Leg {
            from_id: at.to_string(),
            to_id: to.to_string(),
            rate,
            derivation: CROSSED,
            via: pivot.clone(),
            dataset: String::new(),
            factors,
            subs: vec![first, second],
            kind_rank,
        });
    }
    None
}

type Chain = (usize, Vec<u8>, Vec<usize>, Vec<String>, Vec<Leg>);

fn keep(chains: &mut Vec<Chain>, legs: &[Leg]) {
    chains.push((
        counted(legs),
        legs.iter()
            .map(|leg| derivation_rank(leg.derivation))
            .collect(),
        legs.iter().map(|leg| leg.kind_rank).collect(),
        legs.iter().map(|leg| leg.to_id.clone()).collect(),
        legs.to_vec(),
    ));
}

/// The legs from `held` to `base`, or why there are none: the shortest
/// chain of the lake's own rates, at most [`MOST_LEGS`] (a cross counting as
/// its two sub-legs), its last leg from a fiat currency to a fiat base
/// inverted or crossed where the lake has no direct rate (contract v27,
/// ruling 462); a token's leg only ever the lake's own. Ties go by each
/// leg's derivation, then the lowest kinds, then the instrument IDs' order.
/// A stale price under any leg is `PRICE_STALE`.
pub fn conversion(case: &Case, held: &str, base: &str) -> std::result::Result<Vec<Leg>, Why> {
    if held == base {
        return Ok(Vec::new());
    }
    let mut chains: Vec<Chain> = Vec::new();
    let mut seen = BTreeSet::from([held.to_string()]);
    let mut legs = Vec::new();
    walk(case, held, base, &mut legs, &mut seen, &mut chains);
    if chains.is_empty() {
        return Err((CONVERSION_MISSING, String::new()));
    }
    chains.sort_by(|a, b| (a.0, &a.1, &a.2, &a.3).cmp(&(b.0, &b.1, &b.2, &b.3)));
    let (_, _, _, _, legs) = chains.swap_remove(0);
    if legs
        .iter()
        .any(|leg| leg.factors.iter().any(|(price, _)| case.stale(price)))
    {
        return Err((PRICE_STALE, String::new()));
    }
    Ok(legs)
}

fn walk(
    case: &Case,
    at: &str,
    base: &str,
    legs: &mut Vec<Leg>,
    seen: &mut BTreeSet<String>,
    chains: &mut Vec<Chain>,
) {
    if at == base {
        keep(chains, legs);
        return;
    }
    if legs.len() == MOST_LEGS {
        return;
    }
    if direct(case, at, base).is_none() {
        if let Some(leg) = derived(case, at, base) {
            legs.push(leg);
            if counted(legs) <= MOST_LEGS {
                keep(chains, legs);
            }
            legs.pop();
        }
    }
    for (rank, kind) in CONVERSION_KINDS.iter().enumerate() {
        for price in case.quoted_in(at, kind) {
            if seen.contains(&price.instrument_id) {
                continue;
            }
            let next = price.instrument_id.clone();
            seen.insert(next.clone());
            legs.push(Leg::direct(price, rank));
            walk(case, &next, base, legs, seen, chains);
            legs.pop();
            seen.remove(&next);
        }
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
            ..valued
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
            return Ok(conversion(case, instrument_id, base)
                .map(|legs| valued(product(&legs), legs, None)));
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
                return Ok(conversion(case, currency, base).map(|legs| {
                    valued(outcome.settlement_value.mul(&product(&legs)), legs, None)
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
        // Contract v27 (rulings 453 and 486): a quoted price's value per
        // unit rounded half to even to 18 places; its market value that
        // times the quantity, exact.
        let per_unit = price
            .amount
            .mul(&product(&legs))
            .round_half_even(RATE_SCALE);
        valued(per_unit, legs, Some(price.clone()))
    }))
}

/// What a used value read, for the provenance: its datasets, its own price
/// (ruling 460) and its conversion legs (ruling 462).
pub fn read(reads: &mut Reads, valued: &Valued) {
    for (price, _) in &valued.factors {
        if !price.dataset.is_empty() {
            reads.datasets.insert(price.dataset.clone());
        }
    }
    if let Some(own) = &valued.own {
        reads
            .input_prices
            .entry(own.subject_id.clone())
            .or_insert_with(|| {
                json!({
                    "instrument_id": own.subject_id,
                    "price": {"amount": own.amount.text(), "instrument_id": own.instrument_id},
                    "dataset": own.dataset,
                    "row_key": own.row_key,
                    "version": own.version.to_string(),
                    "source_ns": own.source_ns.to_string(),
                })
            });
    }
    read_legs(reads, &valued.legs);
}

/// Conversion legs read, each named once in the provenance, a cross with
/// its two sub-legs.
pub fn read_legs(reads: &mut Reads, legs: &[Leg]) {
    for leg in legs {
        for (price, _) in &leg.factors {
            if !price.dataset.is_empty() {
                reads.datasets.insert(price.dataset.clone());
            }
        }
        reads
            .legs
            .entry((
                leg.from_id.clone(),
                leg.to_id.clone(),
                derivation_rank(leg.derivation),
                leg.via.clone(),
            ))
            .or_insert_with(|| leg.to_json());
        read_legs(reads, &leg.subs);
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
    if !crate::definitions::MODELS[..3].contains(&model_id) {
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
            // Contract v27 (rulings 453 and 486): a quoted price's rounded
            // half to even to 18 places where it runs past them.
            if model_id == "quoted_price" && value.scale() > RATE_SCALE {
                value = value.round_half_even(RATE_SCALE);
            }
            return Ok((value, held));
        };
        visited.insert(subject.clone());
        value = value.mul(amount);
        held = currency.clone();
    }
}
