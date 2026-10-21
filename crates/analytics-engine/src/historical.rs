//! Historical VaR and expected shortfall, version 1 (Q15; the first
//! release's Q6, Q7 and Q9): full revaluation over the history's last
//! `window_days` relative returns of every market object a position reads.
//!
//! Each day's scenario value is quantity × Π(object now × close on the day ÷
//! close the day before), its P&L that less the base's. The P&Ls are sorted,
//! worst first; k = max(1, ⌊N × (1 − confidence)⌋); VaR is minus the k-th,
//! ES minus the mean of the first k. Binary event contracts are left out,
//! and beside the figure a second, keyed `SCENARIO_KIND_OUTCOME`, gives
//! their exact resolution worst case.
//!
//! **Exact, and cheap.** A return divides, so the reference keeps every
//! P&L as an exact rational and rounds only the figure. Taking 500 days ×
//! every position in rationals costs far more than the figure needs: only
//! the k worst days enter it. So every day is first taken in floating point
//! with a bound on its error, every day that could be among the k worst
//! (its lower bound at most the k-th smallest upper bound) is then computed
//! exactly, and the figure is taken from those, rounded once. A day outside
//! that set cannot be among the k worst, so the figure is the reference's
//! digit for digit; and the first pass sizes its workspace before it loops,
//! so an iteration never allocates (requirement 31).

use std::collections::BTreeMap;

use crate::answer::*;
use crate::case::{Case, CaseError, MeasureKey, Result};
use crate::definitions::Definition;
use crate::exact::big::Big;
use crate::exact::{Dec, Ratio};
use crate::measures::{breakdown_of, payoffs, positions, scenarios, Part};
use crate::model;

/// A close carried forward at most this many days running.
pub const MOST_CARRIED: usize = 5;

pub fn window(definition: &Definition) -> usize {
    definition
        .parameter("window_days")
        .and_then(|w| w.parse().ok())
        .unwrap_or(0)
}

/// The closes of `subject` over `dates`, a missing one carried forward at
/// most [`MOST_CARRIED`] days running; None past that, or missing on the
/// first date.
fn series(case: &Case, subject: &str, dates: &[String]) -> Option<Vec<Dec>> {
    let mut out: Vec<Dec> = Vec::with_capacity(dates.len());
    let mut carried = 0;
    let held = case.closes.get(subject);
    for day in dates {
        match held.and_then(|closes| closes.get(day)) {
            Some(close) => {
                carried = 0;
                out.push(close.clone());
            }
            None => {
                if out.is_empty() || carried == MOST_CARRIED {
                    return None;
                }
                carried += 1;
                let last = out.last().cloned().expect("not empty");
                out.push(last);
            }
        }
    }
    Some(out)
}

/// One subject's closes over the window, exact and as day-on-day ratios in
/// floating point.
#[derive(Debug, Clone)]
pub struct Series {
    pub closes: Vec<Dec>,
    pub ratios: Vec<f64>,
}

/// One position's part in a bucket: its value now, and the series it moves
/// with.
#[derive(Debug, Clone)]
pub struct Holding {
    pub now: Dec,
    pub now_f: f64,
    /// Each series it moves with, and its exponent: 1, or -1 for an
    /// inverted conversion leg, which moves inversely (contract v27).
    pub series: Vec<(usize, i32)>,
}

/// A bucket's holdings over a shared table of series: what one figure is
/// computed from.
#[derive(Debug, Clone, Default)]
pub struct Bucket {
    pub holdings: Vec<Holding>,
}

/// The workspace the first pass needs, sized once.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub approximate: Vec<f64>,
    pub bound: Vec<f64>,
    pub upper: Vec<f64>,
}

impl Workspace {
    pub fn new(days: usize) -> Workspace {
        Workspace {
            approximate: vec![0.0; days],
            bound: vec![0.0; days],
            upper: vec![0.0; days],
        }
    }
}

/// Every day's P&L in floating point and a bound on its error: the inner
/// loop, which never allocates. False where a value is not finite, and the
/// caller then takes every day exactly.
pub fn approximate(bucket: &Bucket, table: &[Series], days: usize, work: &mut Workspace) -> bool {
    let most = bucket
        .holdings
        .iter()
        .map(|h| h.series.len())
        .max()
        .unwrap_or(0);
    let terms = bucket.holdings.len();
    // Each term: the value now and each close read (relative error 2ε each:
    // the integer, then the scale), a ratio per series (ε) and a product
    // per series (ε), a subtraction; then a sum over the terms. Four times
    // that, for the bound's own rounding and to spare: a wider bound only
    // computes more days exactly, never a different figure.
    let relative = (5 * most + 8 + terms) as f64 * f64::EPSILON * 4.0;
    for day in 0..days {
        let mut sum = 0.0f64;
        let mut magnitude = 0.0f64;
        for holding in &bucket.holdings {
            let mut moved = holding.now_f;
            for &(s, exponent) in &holding.series {
                if exponent < 0 {
                    moved /= table[s].ratios[day];
                } else {
                    moved *= table[s].ratios[day];
                }
            }
            sum += moved - holding.now_f;
            magnitude += moved.abs() + holding.now_f.abs();
        }
        if !sum.is_finite() || !magnitude.is_finite() {
            return false;
        }
        work.approximate[day] = sum;
        work.bound[day] = magnitude * relative + f64::MIN_POSITIVE;
    }
    true
}

/// One day's P&L, exact.
pub fn exact_day(bucket: &Bucket, table: &[Series], day: usize) -> Ratio {
    let mut total = Ratio::zero();
    for holding in &bucket.holdings {
        let now = holding.now.to_ratio();
        let mut moved = now.clone();
        for &(s, exponent) in &holding.series {
            let after = table[s].closes[day + 1].to_ratio();
            let before = table[s].closes[day].to_ratio();
            moved = if exponent < 0 {
                moved.mul(&before).div(&after)
            } else {
                moved.mul(&after).div(&before)
            };
        }
        total = total.add(&moved.sub(&now));
    }
    total
}

/// The k worst days' P&Ls, exact and sorted worst first: the days the first
/// pass could not rule out, each computed exactly.
pub fn worst_days(
    bucket: &Bucket,
    table: &[Series],
    days: usize,
    k: usize,
    work: &mut Workspace,
) -> Vec<Ratio> {
    let candidates: Vec<usize> = if approximate(bucket, table, days, work) {
        for day in 0..days {
            work.upper[day] = work.approximate[day] + work.bound[day];
        }
        let (_, kth, _) = work.upper[..days].select_nth_unstable_by(k - 1, f64::total_cmp);
        let ceiling = *kth;
        (0..days)
            .filter(|&day| work.approximate[day] - work.bound[day] <= ceiling)
            .collect()
    } else {
        (0..days).collect()
    };
    let mut exact: Vec<Ratio> = candidates
        .into_iter()
        .map(|day| exact_day(bucket, table, day))
        .collect();
    exact.sort();
    exact.truncate(k);
    exact
}

/// k = max(1, ⌊N × (1 − confidence)⌋), exactly.
pub fn order_statistic(days: usize, definition: &Definition) -> Result<usize> {
    let confidence =
        Dec::parse(definition.parameter("confidence").unwrap_or("0")).map_err(CaseError)?;
    let tail = Dec::from_int(1)
        .sub(&confidence)
        .mul(&Dec::from_int(days as i128));
    // Floor of the decimal: its integer divided by 10^scale, rounded down.
    let whole = tail
        .integer()
        .to_big()
        .divmod_floor(&Big::pow10(tail.scale()))
        .0
        .to_i128()
        .unwrap_or(0);
    Ok((whole.max(1)) as usize)
}

/// The account's event contracts' exact resolution worst case: each mutually
/// exclusive group's least payoff, and each other contract's worse of paying
/// and not; None where one cannot be valued.
fn worst_resolution(
    case: &Case,
    held: &[crate::case::Position],
    base: &str,
    reads: &mut Reads,
) -> Result<Option<Dec>> {
    let events: Vec<&crate::case::Position> = held
        .iter()
        .filter(|p| {
            case.instruments
                .get(&p.instrument_id)
                .is_some_and(|r| r.model_id == "binary_event")
        })
        .collect();
    if events.is_empty() {
        return Ok(Some(Dec::zero()));
    }
    let mut worst = Dec::zero();
    let mut done: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for group in case.groups.values() {
        if group.mutually_exclusive != "EVENT_GROUP_FLAG_TRUE" {
            continue;
        }
        let inside: Vec<crate::case::Position> = events
            .iter()
            .filter(|p| {
                group.members.contains(&p.instrument_id) && !done.contains(&p.instrument_id)
            })
            .map(|p| (*p).clone())
            .collect();
        if inside.is_empty() {
            continue;
        }
        let Ok(payoffs) = payoffs(case, group, &inside, base, reads)? else {
            return Ok(None);
        };
        reads.groups.insert(group.event_group_id.clone());
        let least = payoffs
            .into_iter()
            .map(|(_, total)| total)
            .min()
            .unwrap_or_else(Dec::zero);
        worst = worst.add(&least);
        done.extend(inside.into_iter().map(|p| p.instrument_id));
    }
    for position in events {
        if done.contains(&position.instrument_id) {
            continue;
        }
        let record = &case.instruments[&position.instrument_id];
        let (Some((payout, currency)), Some(cost)) = (&record.payout, &position.cost) else {
            return Ok(None);
        };
        let Ok(legs) = model::conversion(case, currency, base) else {
            return Ok(None);
        };
        let paid = position.quantity.mul(payout).mul(&model::product(&legs));
        let floor = if paid.signum() < 0 { paid } else { Dec::zero() };
        worst = worst.add(&floor.sub(cost));
    }
    Ok(Some(worst))
}

/// What one account's figures under one scenario are computed from: the
/// series table, the buckets by breakdown, and the positions held.
pub struct Gathered {
    pub table: Vec<Series>,
    pub buckets: BTreeMap<Vec<(String, String)>, Bucket>,
    pub held: Vec<crate::case::Position>,
}

/// Gather an account's holdings under a scenario into buckets, recording
/// what it could not evaluate (under the base) and what it read.
pub fn gather(
    case: &Case,
    key: &MeasureKey,
    definition: &Definition,
    account: &str,
    scenario: &'static str,
    part: &mut Part,
) -> Result<Gathered> {
    let window = window(definition);
    let names = &key.breakdowns;
    let dates = &case.dates[case.dates.len() - (window + 1)..];
    let base = case.base(account)?.to_string();
    let held = positions(case, &mut part.reads, account, scenario)?;
    let mut table: Vec<Series> = Vec::new();
    let mut by_subject: BTreeMap<String, Option<usize>> = BTreeMap::new();
    let mut buckets: BTreeMap<Vec<(String, String)>, Bucket> = BTreeMap::new();
    for position in &held {
        if let Some(record) = case
            .instruments
            .get(&position.instrument_id)
            .filter(|r| r.model_id == "binary_event")
        {
            part.reads
                .instruments
                .insert(position.instrument_id.clone());
            part.reads
                .models
                .insert((record.model_id.clone(), record.model_version));
            continue;
        }
        let valued = match model::value(case, &mut part.reads, &position.instrument_id, &base)? {
            Ok(valued) => valued,
            Err((why, detail)) => {
                if scenario == BASE {
                    part.not_evaluated.push(Unevaluated {
                        account_id: account.to_string(),
                        instrument_id: position.instrument_id.clone(),
                        measure: key.clone(),
                        reason: why,
                        detail,
                    });
                }
                continue;
            }
        };
        let group = match breakdown_of(case, position, names) {
            Ok(group) => group,
            Err(missing) => {
                if scenario == BASE {
                    part.not_evaluated.push(Unevaluated {
                        account_id: account.to_string(),
                        instrument_id: position.instrument_id.clone(),
                        measure: key.clone(),
                        reason: SCHEME_NOT_HELD,
                        detail: missing,
                    });
                }
                continue;
            }
        };
        let mut moves = Vec::with_capacity(valued.factors.len());
        let mut short = false;
        for (object, exponent) in &valued.factors {
            let index = by_subject
                .entry(object.subject_id.clone())
                .or_insert_with(|| {
                    let closes = series(case, &object.subject_id, dates)?;
                    // Every close, the last too: an inverted leg divides by
                    // it (contract v27).
                    if closes.iter().any(Dec::is_zero) {
                        return None;
                    }
                    let floats: Vec<f64> = closes.iter().map(Dec::to_f64).collect();
                    let ratios = floats.windows(2).map(|w| w[1] / w[0]).collect();
                    table.push(Series { closes, ratios });
                    Some(table.len() - 1)
                });
            match index {
                Some(index) => moves.push((*index, *exponent)),
                None => {
                    short = true;
                    break;
                }
            }
        }
        if short {
            if scenario == BASE {
                part.not_evaluated.push(Unevaluated {
                    account_id: account.to_string(),
                    instrument_id: position.instrument_id.clone(),
                    measure: key.clone(),
                    reason: HISTORY_INSUFFICIENT,
                    detail: String::new(),
                });
            }
            continue;
        }
        model::read(&mut part.reads, &valued);
        let now = valued.per_unit.mul(&position.quantity);
        let now_f = now.to_f64();
        buckets.entry(group).or_default().holdings.push(Holding {
            now,
            now_f,
            series: moves,
        });
    }
    if buckets.is_empty() && names.is_empty() {
        buckets.insert(Vec::new(), Bucket::default());
    }
    Ok(Gathered {
        table,
        buckets,
        held,
    })
}

/// One account's historical VaR or ES, by full revaluation.
pub fn compute(
    case: &Case,
    key: &MeasureKey,
    definition: &Definition,
    account: &str,
    part: &mut Part,
) -> Result<()> {
    let window = window(definition);
    let k = order_statistic(window, definition)?;
    let base = case.base(account)?.to_string();
    let Some((rate, currency)) = crate::measures::reporting(case, key, account, part)? else {
        return Ok(());
    };
    for scenario in scenarios(case, account) {
        let Gathered {
            table,
            buckets,
            held,
        } = gather(case, key, definition, account, scenario, part)?;
        let mut work = Workspace::new(window);
        for (group, bucket) in &buckets {
            let worst = worst_days(bucket, &table, window, k, &mut work);
            let figure = if definition.definition == "historical_var" {
                worst[k - 1].negated()
            } else {
                let mut sum = Ratio::zero();
                for day in &worst {
                    sum = sum.add(day);
                }
                sum.negated().div_small(k as u32)
            };
            part.figures.push(Figure {
                measure: key.clone(),
                account_id: account.to_string(),
                breakdown: group.clone(),
                scenario: Scenario::of(scenario),
                value: Some(figure.mul(&rate.to_ratio()).round_half_even(definition.scale)),
                numerator: None,
                denominator: None,
                currency_instrument_id: currency.clone(),
            });
        }
        let worst = worst_resolution(case, &held, &base, &mut part.reads)?;
        let events = held.iter().any(|p| {
            case.instruments
                .get(&p.instrument_id)
                .is_some_and(|r| r.model_id == "binary_event")
        });
        if let (true, Some(worst), true, true) =
            (events, worst, key.breakdowns.is_empty(), scenario == BASE)
        {
            part.figures.push(Figure {
                measure: key.clone(),
                account_id: account.to_string(),
                breakdown: Vec::new(),
                scenario: Scenario::of(OUTCOME),
                value: Some(worst.negated().mul(&rate).round_half_even(definition.scale)),
                numerator: None,
                denominator: None,
                currency_instrument_id: currency.clone(),
            });
        }
    }
    Ok(())
}
