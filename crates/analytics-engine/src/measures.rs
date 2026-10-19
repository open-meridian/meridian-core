//! v26's measure definitions, version 1, computed over an inputs document
//! (the one-API spec's requirement 15; the first release's §1 and Q3 to
//! Q6; rulings 301 to 303). Each value is a sum over unrounded terms,
//! rounded once, half to even, to the definition's scale.
//!
//! A measure is computed in units any worker can take (requirement 30): its
//! preamble -- the checks that answer for the whole measure -- and then one
//! unit per account, or for the implied distribution one for the group.
//! Each unit is a pure function of the document and its own name, and the
//! answer is the units' parts in the job's order, each measure's sorted, so
//! one worker and four give the same answer (requirement 25).

use std::collections::BTreeMap;

use crate::answer::*;
use crate::case::{Case, CaseError, Group, MeasureKey, Position, Result};
use crate::definitions::{self, Definition, BY, COMPONENT};
use crate::exact::Dec;
use crate::historical;
use crate::model::{self, conversion, product, Valued};

/// A whole-number scale for a what-if trade's cost, as the reference rounds
/// it.
const COST_SCALE: u32 = 18;

/// What one unit, or a measure's preamble, answered.
#[derive(Debug, Clone, Default)]
pub struct Part {
    pub figures: Vec<Figure>,
    pub not_evaluated: Vec<Unevaluated>,
    pub reads: Reads,
}

/// A unit of work: one measure of the job, over one account or (for the
/// implied distribution) its group.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Unit {
    /// The measure's place in the job.
    pub measure: usize,
    /// The account; None for a figure across accounts.
    pub account: Option<String>,
}

/// A measure's preamble and its units.
#[derive(Debug, Clone)]
pub struct Planned {
    pub preamble: Part,
    pub units: Vec<Unit>,
}

pub fn lookup(key: &MeasureKey) -> Result<&'static Definition> {
    i32::try_from(key.version)
        .ok()
        .and_then(|version| definitions::definition(&key.definition, version))
        .ok_or_else(|| {
            CaseError(format!(
                "the component serves no definition {} v{}",
                key.definition, key.version
            ))
        })
}

fn group<'a>(case: &'a Case, key: &MeasureKey, reads: &mut Reads) -> Result<&'a Group> {
    let group = case.groups.get(&key.event_group_id).ok_or_else(|| {
        CaseError(format!(
            "{} names event group {:?}, and the inputs hold none",
            key.definition, key.event_group_id
        ))
    })?;
    reads.groups.insert(group.event_group_id.clone());
    for member in &group.members {
        if case.instruments.contains_key(member) {
            reads.instruments.insert(member.clone());
        }
    }
    Ok(group)
}

/// A measure's preamble, and the units it splits into.
pub fn plan(case: &Case, index: usize) -> Result<Planned> {
    let key = &case.measures[index];
    let definition = lookup(key)?;
    let mut preamble = Part::default();
    let per_account = || {
        case.account_ids
            .iter()
            .map(|account| Unit {
                measure: index,
                account: Some(account.clone()),
            })
            .collect::<Vec<_>>()
    };
    let unknown = |allowed: &[&str]| -> Vec<Unevaluated> {
        key.breakdowns
            .iter()
            .filter(|name| !allowed.contains(&name.as_str()))
            .map(|name| Unevaluated {
                account_id: String::new(),
                instrument_id: String::new(),
                measure: key.clone(),
                reason: SCHEME_NOT_HELD,
                detail: name.clone(),
            })
            .collect()
    };
    let units = match definition.definition {
        "market_value" | "exposure" | "pnl" => {
            preamble.not_evaluated = unknown(&BY);
            if preamble.not_evaluated.is_empty() {
                per_account()
            } else {
                Vec::new()
            }
        }
        "concentration" => per_account(),
        "historical_var" | "expected_shortfall" => {
            preamble.not_evaluated = unknown(definition.breakdowns);
            let window = historical::window(definition);
            if !preamble.not_evaluated.is_empty() {
                Vec::new()
            } else if case.dates.len() < window + 1 {
                preamble.not_evaluated.push(Unevaluated {
                    account_id: String::new(),
                    instrument_id: String::new(),
                    measure: key.clone(),
                    reason: HISTORY_INSUFFICIENT,
                    detail: format!("{} business dates", case.dates.len()),
                });
                Vec::new()
            } else {
                per_account()
            }
        }
        "resolution_payoff" => {
            group(case, key, &mut preamble.reads)?;
            per_account()
        }
        "implied_distribution" => vec![Unit {
            measure: index,
            account: None,
        }],
        other => return Err(CaseError(format!("no computation for {other}"))),
    };
    Ok(Planned { preamble, units })
}

/// One unit's part of the answer.
pub fn compute(case: &Case, unit: &Unit) -> Result<Part> {
    let key = &case.measures[unit.measure];
    let definition = lookup(key)?;
    let mut part = Part::default();
    let account = unit.account.as_deref().unwrap_or_default();
    match definition.definition {
        "market_value" | "exposure" | "pnl" => linear(case, key, definition, account, &mut part)?,
        "concentration" => concentration(case, key, definition, account, &mut part)?,
        "historical_var" | "expected_shortfall" => {
            historical::compute(case, key, definition, account, &mut part)?
        }
        "resolution_payoff" => resolution_payoff(case, key, definition, account, &mut part)?,
        "implied_distribution" => implied_distribution(case, key, definition, &mut part)?,
        other => return Err(CaseError(format!("no computation for {other}"))),
    }
    Ok(part)
}

/// The scenarios an account is answered under: the base, and the what-if
/// where the job's trades are its.
pub fn scenarios(case: &Case, account: &str) -> Vec<&'static str> {
    match &case.what_if {
        Some(what_if) if what_if.account_id == account => vec![BASE, WHAT_IF],
        _ => vec![BASE],
    }
}

/// The account's positions under a scenario: under the what-if, with its
/// trades added (buy +, sell -), each trade's cost its quantity times its
/// assumed price converted, or the as-of value per unit, and beside each
/// its cash leg, the base currency paid or received (ruling 301).
pub fn positions(
    case: &Case,
    reads: &mut Reads,
    account: &str,
    scenario: &str,
) -> Result<Vec<Position>> {
    let mut held: Vec<Position> = case
        .positions
        .iter()
        .filter(|p| p.account_id == account)
        .cloned()
        .collect();
    let Some(what_if) = case.what_if.as_ref().filter(|_| scenario == WHAT_IF) else {
        return Ok(held);
    };
    let base = case.base(account)?.to_string();
    for trade in &what_if.trades {
        let quantity = if trade.side.starts_with("SIDE_SELL") {
            trade.quantity.negated()
        } else {
            trade.quantity.clone()
        };
        let per = match &trade.price {
            Some((amount, currency)) => match conversion(case, currency, &base) {
                Ok(legs) => amount.mul(&product(&legs)),
                Err(_) => Dec::zero(),
            },
            None => match model::value(case, reads, &trade.instrument_id, &base)? {
                Ok(valued) => valued.per_unit,
                Err(_) => Dec::zero(),
            },
        };
        let cost = per.mul(&quantity).round_half_even(COST_SCALE);
        held.push(Position {
            account_id: account.to_string(),
            instrument_id: trade.instrument_id.clone(),
            quantity,
            cost: Some(cost.clone()),
            sleeve: String::new(),
        });
        held.push(Position {
            account_id: account.to_string(),
            instrument_id: base.clone(),
            quantity: cost.negated(),
            cost: Some(cost.negated()),
            sleeve: String::new(),
        });
    }
    Ok(held)
}

/// A position's place in each breakdown named, or the first it lacks.
pub fn breakdown_of(
    case: &Case,
    position: &Position,
    names: &[String],
) -> std::result::Result<Vec<(String, String)>, String> {
    let record = case.instruments.get(&position.instrument_id);
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let held = match name.as_str() {
            "account" => position.account_id.clone(),
            "asset_class" => record.map(|r| r.asset_class.clone()).unwrap_or_default(),
            "currency" => record
                .map(|r| r.currency_instrument_id.clone())
                .unwrap_or_default(),
            "instrument" => position.instrument_id.clone(),
            "sleeve" => position.sleeve.clone(),
            "country" => record.map(|r| r.country.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        if held.is_empty() {
            return Err(name.clone());
        }
        out.push((name.clone(), held));
    }
    Ok(out)
}

fn unevaluated(
    account: &str,
    instrument: &str,
    key: &MeasureKey,
    reason: &'static str,
    detail: String,
) -> Unevaluated {
    Unevaluated {
        account_id: account.to_string(),
        instrument_id: instrument.to_string(),
        measure: key.clone(),
        reason,
        detail,
    }
}

type Sums = BTreeMap<Vec<(String, String)>, BTreeMap<&'static str, Dec>>;

fn add(sums: &mut Sums, group: Vec<(String, String)>, component: &'static str, worth: Dec) {
    let held = sums.entry(group).or_default();
    let total = match held.get(component) {
        Some(sum) => sum.add(&worth),
        None => worth,
    };
    held.insert(component, total);
}

/// market_value, exposure and pnl: a sum per account and breakdown; for
/// exposure and pnl one per component (ruling 301): exposure's gross and
/// net, pnl's unrealized and realized since the account opened.
fn linear(
    case: &Case,
    key: &MeasureKey,
    definition: &Definition,
    account: &str,
    part: &mut Part,
) -> Result<()> {
    let names = &key.breakdowns;
    let components: &[&'static str] = match definition.definition {
        "exposure" => &["gross", "net"],
        "pnl" => &["unrealized", "realized"],
        _ => &[""],
    };
    let base = case.base(account)?.to_string();
    for scenario in scenarios(case, account) {
        let mut sums = Sums::new();
        for position in positions(case, &mut part.reads, account, scenario)? {
            let valued: Valued =
                match model::value(case, &mut part.reads, &position.instrument_id, &base)? {
                    Ok(valued) => valued,
                    Err((why, detail)) => {
                        if scenario == BASE {
                            part.not_evaluated.push(unevaluated(
                                account,
                                &position.instrument_id,
                                key,
                                why,
                                detail,
                            ));
                        }
                        continue;
                    }
                };
            model::read(&mut part.reads, &valued);
            let group = match breakdown_of(case, &position, names) {
                Ok(group) => group,
                Err(missing) => {
                    if scenario == BASE {
                        part.not_evaluated.push(unevaluated(
                            account,
                            &position.instrument_id,
                            key,
                            SCHEME_NOT_HELD,
                            missing,
                        ));
                    }
                    continue;
                }
            };
            let worth = valued.per_unit.mul(&position.quantity);
            match definition.definition {
                "exposure" => {
                    add(&mut sums, group.clone(), "gross", worth.abs());
                    add(&mut sums, group, "net", worth);
                }
                "pnl" => {
                    let cost = position.cost.as_ref().ok_or_else(|| {
                        CaseError(format!(
                            "pnl asks {}'s lots' cost, and the inputs state none",
                            position.instrument_id
                        ))
                    })?;
                    add(&mut sums, group, "unrealized", worth.sub(cost));
                }
                _ => add(&mut sums, group, "", worth),
            }
        }
        if definition.definition == "pnl" {
            for lot in case.closed_lots.iter().filter(|l| l.account_id == account) {
                let as_position = Position {
                    account_id: account.to_string(),
                    instrument_id: lot.instrument_id.clone(),
                    quantity: lot.quantity.clone(),
                    cost: None,
                    sleeve: lot.sleeve.clone(),
                };
                match breakdown_of(case, &as_position, names) {
                    Ok(group) => add(&mut sums, group, "realized", lot.proceeds.sub(&lot.cost)),
                    Err(missing) => {
                        if scenario == BASE {
                            part.not_evaluated.push(unevaluated(
                                account,
                                &lot.instrument_id,
                                key,
                                SCHEME_NOT_HELD,
                                missing,
                            ));
                        }
                    }
                }
            }
        }
        if sums.is_empty() && names.is_empty() {
            sums.insert(Vec::new(), BTreeMap::new());
        }
        for (group, held) in sums {
            for component in components {
                let mut breakdown = group.clone();
                if !component.is_empty() {
                    breakdown.push((COMPONENT.to_string(), component.to_string()));
                }
                let value = held.get(component).cloned().unwrap_or_else(Dec::zero);
                part.figures.push(Figure {
                    measure: key.clone(),
                    account_id: account.to_string(),
                    breakdown,
                    scenario: Scenario::of(scenario),
                    value: Some(value.round_half_even(definition.scale)),
                    numerator: None,
                    denominator: None,
                    currency_instrument_id: base.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Each instrument's weight in its account: its market value over the
/// account's market value, each rounded to the scale.
fn concentration(
    case: &Case,
    key: &MeasureKey,
    definition: &Definition,
    account: &str,
    part: &mut Part,
) -> Result<()> {
    let base = case.base(account)?.to_string();
    for scenario in scenarios(case, account) {
        let mut by: BTreeMap<String, Dec> = BTreeMap::new();
        for position in positions(case, &mut part.reads, account, scenario)? {
            let valued = match model::value(case, &mut part.reads, &position.instrument_id, &base)?
            {
                Ok(valued) => valued,
                Err((why, detail)) => {
                    if scenario == BASE {
                        part.not_evaluated.push(unevaluated(
                            account,
                            &position.instrument_id,
                            key,
                            why,
                            detail,
                        ));
                    }
                    continue;
                }
            };
            model::read(&mut part.reads, &valued);
            let worth = valued.per_unit.mul(&position.quantity);
            let total = match by.get(&position.instrument_id) {
                Some(sum) => sum.add(&worth),
                None => worth,
            };
            by.insert(position.instrument_id.clone(), total);
        }
        let mut whole = Dec::zero();
        for worth in by.values() {
            whole = whole.add(worth);
        }
        let total = whole.round_half_even(definition.scale);
        for (instrument, worth) in by {
            part.figures.push(Figure {
                measure: key.clone(),
                account_id: account.to_string(),
                breakdown: vec![("instrument".to_string(), instrument)],
                scenario: Scenario::of(scenario),
                value: None,
                numerator: Some(worth.round_half_even(definition.scale)),
                denominator: Some(total.clone()),
                currency_instrument_id: String::new(),
            });
        }
    }
    Ok(())
}

/// A member's price for the distribution: the mid where both sides stand
/// and the spread is at most the threshold, else the last trade.
fn member_price<'a>(
    case: &'a Case,
    member: &str,
    threshold: &Dec,
) -> Option<(Dec, String, Vec<&'a crate::case::Price>)> {
    let bid = case.price(member, "bid");
    let ask = case.price(member, "ask");
    let last = case.price(member, "last");
    if let (Some(bid), Some(ask)) = (bid, ask) {
        if ask.amount.sub(&bid.amount) <= *threshold {
            let mid = bid.amount.add(&ask.amount).half();
            return Some((mid, bid.instrument_id.clone(), vec![bid, ask]));
        }
    }
    last.map(|last| (last.amount.clone(), last.instrument_id.clone(), vec![last]))
}

/// Each member's price over the sum of the members' prices, beside the sum;
/// a group not exhaustive answers the raw prices and is
/// `GROUP_NOT_EXHAUSTIVE` (the first release's Q3 to Q5).
fn implied_distribution(
    case: &Case,
    key: &MeasureKey,
    definition: &Definition,
    part: &mut Part,
) -> Result<()> {
    let group = group(case, key, &mut part.reads)?;
    let threshold =
        Dec::parse(definition.parameter("spread_threshold").unwrap_or("0")).map_err(CaseError)?;
    let mut prices: Vec<(String, Dec)> = Vec::new();
    let mut cash = std::collections::BTreeSet::new();
    let mut missing = false;
    for member in &group.members {
        let Some((price, held, read)) = member_price(case, member, &threshold) else {
            part.not_evaluated
                .push(unevaluated("", member, key, PRICE_MISSING, String::new()));
            missing = true;
            continue;
        };
        for each in read {
            if !each.dataset.is_empty() {
                part.reads.datasets.insert(each.dataset.clone());
            }
        }
        cash.insert(held);
        prices.push((member.clone(), price));
    }
    if missing {
        return Ok(());
    }
    if cash.len() > 1 {
        part.not_evaluated.push(unevaluated(
            "",
            "",
            key,
            CONVERSION_MISSING,
            "the members' prices are in different cash".to_string(),
        ));
        return Ok(());
    }
    let currency = cash.into_iter().next().unwrap_or_default();
    let mut total = Dec::zero();
    for (_, price) in &prices {
        total = total.add(price);
    }
    let scale = definition.scale;
    part.figures.push(Figure {
        measure: key.clone(),
        account_id: String::new(),
        breakdown: Vec::new(),
        scenario: Scenario::of(BASE),
        value: Some(total.round_half_even(scale)),
        numerator: None,
        denominator: None,
        currency_instrument_id: currency.clone(),
    });
    let exhaustive = group.exhaustive == "EVENT_GROUP_FLAG_TRUE";
    if !exhaustive {
        part.not_evaluated.push(unevaluated(
            "",
            "",
            key,
            GROUP_NOT_EXHAUSTIVE,
            String::new(),
        ));
    }
    for (member, price) in prices {
        part.figures.push(if exhaustive {
            Figure {
                measure: key.clone(),
                account_id: String::new(),
                breakdown: Vec::new(),
                scenario: Scenario::outcome(&member),
                value: None,
                numerator: Some(price.round_half_even(scale)),
                denominator: Some(total.round_half_even(scale)),
                currency_instrument_id: String::new(),
            }
        } else {
            Figure {
                measure: key.clone(),
                account_id: String::new(),
                breakdown: Vec::new(),
                scenario: Scenario::outcome(&member),
                value: Some(price.round_half_even(scale)),
                numerator: None,
                denominator: None,
                currency_instrument_id: currency.clone(),
            }
        });
    }
    Ok(())
}

/// Each resolution scenario's payoff over the positions in the group's
/// members, or each position's reason it cannot be.
#[allow(clippy::type_complexity)]
pub fn payoffs(
    case: &Case,
    group: &Group,
    positions: &[Position],
    base: &str,
    reads: &mut Reads,
) -> Result<std::result::Result<Vec<(Scenario, Dec)>, Vec<(String, &'static str, String)>>> {
    let mut terms: Vec<(&Position, Dec)> = Vec::new();
    let mut failed = Vec::new();
    for position in positions
        .iter()
        .filter(|p| group.members.contains(&p.instrument_id))
    {
        let record = case.instruments.get(&position.instrument_id);
        let record = match record {
            Some(record) if !position.instrument_id.starts_with("LCL-") => record,
            _ => {
                failed.push((
                    position.instrument_id.clone(),
                    LOCAL_RECORD,
                    "local record: not priced".to_string(),
                ));
                continue;
            }
        };
        let (payout, currency) = record.payout.as_ref().ok_or_else(|| {
            CaseError(format!(
                "{} is in a group and states no payout",
                position.instrument_id
            ))
        })?;
        let legs = match conversion(case, currency, base) {
            Ok(legs) => legs,
            Err((why, _)) => {
                failed.push((position.instrument_id.clone(), why, String::new()));
                continue;
            }
        };
        for leg in &legs {
            if !leg.dataset.is_empty() {
                reads.datasets.insert(leg.dataset.clone());
            }
        }
        if position.cost.is_none() {
            return Err(CaseError(format!(
                "{}'s payoff needs its lots' cost",
                position.instrument_id
            )));
        }
        terms.push((position, payout.mul(&product(&legs))));
    }
    if !failed.is_empty() {
        return Ok(Err(failed));
    }
    let mut scenarios: Vec<Scenario> = group.members.iter().map(|m| Scenario::outcome(m)).collect();
    if group.mutually_exclusive == "EVENT_GROUP_FLAG_TRUE"
        && group.exhaustive != "EVENT_GROUP_FLAG_TRUE"
    {
        scenarios.push(Scenario::of(NONE_OF_THE_NAMED));
    }
    let mut out = Vec::with_capacity(scenarios.len());
    for scenario in scenarios {
        let mut total = Dec::zero();
        for (position, paid) in &terms {
            let pays = scenario.kind == OUTCOME
                && scenario.outcome_instrument_id == position.instrument_id;
            let cost = position.cost.clone().unwrap_or_else(Dec::zero);
            let received = if pays {
                position.quantity.mul(paid)
            } else {
                Dec::zero()
            };
            total = total.add(&received.sub(&cost));
        }
        out.push((scenario, total));
    }
    Ok(Ok(out))
}

/// Each account's P&L if each member resolves YES (and none of the named,
/// for a group mutually exclusive and not exhaustive): exact, base alone.
fn resolution_payoff(
    case: &Case,
    key: &MeasureKey,
    definition: &Definition,
    account: &str,
    part: &mut Part,
) -> Result<()> {
    let group = group(case, key, &mut part.reads)?;
    let base = case.base(account)?.to_string();
    let held: Vec<Position> = case
        .positions
        .iter()
        .filter(|p| p.account_id == account)
        .cloned()
        .collect();
    if !held
        .iter()
        .any(|p| group.members.contains(&p.instrument_id))
    {
        return Ok(());
    }
    match payoffs(case, group, &held, &base, &mut part.reads)? {
        Err(failed) => {
            for (instrument, why, detail) in failed {
                part.not_evaluated
                    .push(unevaluated(account, &instrument, key, why, detail));
            }
        }
        Ok(payoffs) => {
            for (scenario, total) in payoffs {
                part.figures.push(Figure {
                    measure: key.clone(),
                    account_id: account.to_string(),
                    breakdown: Vec::new(),
                    scenario,
                    value: Some(total.round_half_even(definition.scale)),
                    numerator: None,
                    denominator: None,
                    currency_instrument_id: base.clone(),
                });
            }
        }
    }
    Ok(())
}
