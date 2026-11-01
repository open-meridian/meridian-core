//! v29's measures over debt positions and the Treasury curve (contract
//! v29; W12.1; ruling 688): `accrued_interest`, `bond_yield`, `bond_price`,
//! `bond_risk`, `key_rate_durations`, `bond_spread`, `treasury_curve`,
//! `historical_prepayment`, `wal_at_cpr` and `make_whole_amount`.
//!
//! Each position measure is one unit per account over its debt positions
//! (a position under any other record is no bond, and has no figure); each
//! figure names its instrument and its own breakdown. A position under a
//! local record is never priced; a record minted without the terms a
//! measure needs is not evaluated naming them (574), never answered from a
//! guess; a price missing, stale, unentitled or quarantined is answered as
//! every price is (W12.2).
//!
//! The job's date (`valuation_date`) is what accrued interest, the curve
//! and a make-whole redemption are as of; a price's yield, spread and risk
//! settle the business day after it on the bond's payment calendar (T+1,
//! SEC Rule 15c6-1 for corporates, agencies and municipals, the market's
//! for Treasuries).

use meridian_domain::date::Date;

use super::curve::{maturity_of, Curve, ParPoint};
use super::dates::{days, plus_days, Calendar};
use super::daycount::DayCount;
use super::pricing::{g33_yield, Grid, Method, Priceable};
use super::schedule::{accrued, flows_to, hundred, Schedule, Shape};
use super::{Held, Why};
use crate::answer::*;
use crate::case::{Case, CaseError, FactorRow, MeasureKey, ParCurve, Price, Result};
use crate::chains::exact;
use crate::definitions::{Definition, COMPONENT};
use crate::exact::{Dec, Ratio};
use crate::measures::Part;
use crate::model::{self, conversion, own_price, product};

/// Whether a definition is one of v29's debt position measures.
pub fn is_position_measure(definition: &str) -> bool {
    matches!(
        definition,
        "accrued_interest"
            | "bond_yield"
            | "bond_price"
            | "bond_risk"
            | "key_rate_durations"
            | "bond_spread"
            | "historical_prepayment"
            | "wal_at_cpr"
            | "make_whole_amount"
    )
}

/// Whether a definition reads the Treasury curve.
pub fn reads_curve(definition: &str) -> bool {
    matches!(
        definition,
        "treasury_curve" | "key_rate_durations" | "bond_spread"
    )
}

/// Whether a definition reads factor history.
pub fn reads_factors(definition: &str) -> bool {
    matches!(definition, "historical_prepayment" | "wal_at_cpr")
}

/// The job's date.
pub fn valuation_date(case: &Case) -> Result<Date> {
    Date::parse(&case.valuation_date).map_err(|_| {
        CaseError(format!(
            "a debt measure is as of the job's date, and the inputs state {:?}",
            case.valuation_date
        ))
    })
}

/// A pool's or a class's factor in force at the job's date: the latest row
/// whose factor month is the date's or before (680, W10.6).
pub fn factor_at(case: &Case, instrument_id: &str) -> std::result::Result<FactorRow, Why> {
    let month = case
        .valuation_date
        .get(..7)
        .unwrap_or_default()
        .replace('-', "");
    case.factors
        .get(instrument_id)
        .and_then(|rows| {
            rows.iter()
                .filter(|r| month.is_empty() || r.factor_month <= month)
                .max_by(|a, b| a.factor_month.cmp(&b.factor_month))
        })
        .cloned()
        .ok_or((PRICE_MISSING, "factor".to_string()))
}

fn calendar<'a>(case: &'a Case, held: &Held) -> std::borrow::Cow<'a, Calendar> {
    match case.calendars.get(held.calendar_id()) {
        Some(calendar) => std::borrow::Cow::Borrowed(calendar),
        None => std::borrow::Cow::Owned(Calendar::default()),
    }
}

/// A close as a clean price per 100 of face: percent of par as it is, a
/// price per unit of face times 100; a yield or a spread is none.
fn clean_of(price: &Price) -> std::result::Result<Dec, Why> {
    match price.basis.as_str() {
        "PRICE_BASIS_PERCENT_OF_PAR" => Ok(price.amount.clone()),
        "" | "PRICE_BASIS_PER_UNIT" => Ok(price.amount.mul(&Dec::from_int(100))),
        other => Err((
            PRICE_MISSING,
            format!("a clean price: the close is quoted as {other}"),
        )),
    }
}

/// What a figure is told: its measure, account and scale.
struct Out<'a> {
    key: &'a MeasureKey,
    account: &'a str,
    scale: u32,
    part: &'a mut Part,
}

impl Out<'_> {
    fn exact(
        &mut self,
        instrument: &str,
        by: Vec<(&str, String)>,
        value: Dec,
        currency: &str,
        source_ns: i64,
    ) {
        let mut breakdown = vec![("instrument".to_string(), instrument.to_string())];
        breakdown.extend(by.into_iter().map(|(b, v)| (b.to_string(), v)));
        self.part.figures.push(Figure {
            measure: self.key.clone(),
            account_id: self.account.to_string(),
            breakdown,
            value: Some(value.round_half_even(self.scale)),
            currency_instrument_id: currency.to_string(),
            oldest_input_source_ns: source_ns,
            ..Default::default()
        });
    }

    fn model(
        &mut self,
        instrument: &str,
        by: Vec<(&str, String)>,
        value: f64,
        currency: &str,
        source_ns: i64,
    ) {
        match exact(value, self.scale) {
            Some(value) => {
                self.exact(instrument, by, value, currency, source_ns);
                if let Some(last) = self.part.figures.last_mut() {
                    last.computed_as = MODEL;
                }
            }
            None => self.not(instrument, FIT_FAILED, "not a finite number".to_string()),
        }
    }

    fn not(&mut self, instrument: &str, reason: &'static str, detail: String) {
        self.part.not_evaluated.push(Unevaluated {
            account_id: self.account.to_string(),
            instrument_id: instrument.to_string(),
            measure: self.key.clone(),
            reason,
            detail,
            series_key: Vec::new(),
        });
    }
}

/// A bond's schedule priced from a date: its shape, schedule and the
/// settlement its price is for.
struct Bond<'a> {
    held: &'a Held,
    shape: Shape,
    schedule: Schedule,
    settle: Date,
}

impl Bond<'_> {
    fn accrued(&self, on: Date) -> Ratio {
        accrued(&self.shape, &self.schedule, on)
    }

    /// The flows to a redemption on `on` at `price` per 100, discounted by
    /// its method.
    fn to(&self, on: Date, price: &Ratio) -> Priceable {
        // A Treasury's redemption before maturity (an old callable bond's
        // call) is off Appendix B's half-year grid: the street's formula.
        let method = match self.held.method() {
            Method::Treasury if on != self.schedule.maturity => Method::Street,
            other => other,
        };
        Priceable {
            method,
            grid: Grid::of(&self.schedule),
            settle: self.settle,
            flows: flows_to(&self.shape, &self.schedule, self.settle, on, price),
            accrued: self.accrued(self.settle),
        }
    }

    /// Every date the bond may be redeemed on and its price, with the
    /// kind of yield to it: maturity at 100, and each call window but a
    /// make-whole one at the first date on or after both its start and the
    /// settlement plus its least notice (W12.1; MSRB G-15's worst).
    fn redemptions(&self, puts: bool) -> Vec<(&'static str, Date, Ratio)> {
        let mut out = vec![("to_maturity", self.schedule.maturity, hundred())];
        let windows = if puts {
            self.held.puts()
        } else {
            self.held.calls()
        };
        for window in windows.iter().filter(|w| !w.make_whole) {
            let earliest = plus_days(self.settle, i64::from(window.notice_days_min.max(1)));
            let on = if window.from > earliest {
                window.from
            } else {
                earliest
            };
            if on >= self.schedule.maturity || window.to.is_some_and(|to| on > to) {
                continue;
            }
            out.push((
                if puts { "to_put" } else { "to_call" },
                on,
                window.price.clone().unwrap_or_else(hundred),
            ));
        }
        out
    }
}

/// The context one position is answered in.
struct Position<'a> {
    id: &'a str,
    face: Dec,
    held: &'a Held,
    record: &'a crate::case::Instrument,
}

fn bond<'a>(case: &Case, held: &'a Held) -> std::result::Result<Bond<'a>, Why> {
    let date = Date::parse(&case.valuation_date)
        .map_err(|_| (PRICE_MISSING, "the job's date".to_string()))?;
    let calendar = calendar(case, held);
    let shape = held.shape(&calendar)?;
    let schedule = shape.build();
    let settle = calendar.business_days_after(date, 1);
    if settle >= schedule.maturity {
        return Err((PRICE_MISSING, format!("matured on {}", schedule.maturity)));
    }
    Ok(Bond {
        held,
        shape,
        schedule,
        settle,
    })
}

/// The yield (a fraction) the dirty price per 100 is at, to each
/// redemption, and the worst of them; `None` where a price prices to none.
fn yields(b: &Bond<'_>, dirty: f64) -> Vec<(&'static str, Date, f64)> {
    let mut out = Vec::new();
    for (kind, on, price) in b.redemptions(false) {
        if let Some(y) = b.to(on, &price).yield_of(dirty) {
            out.push((kind, on, y));
        }
    }
    out
}

/// The worst: the least yield over maturity and the calls.
fn worst(found: &[(&'static str, Date, f64)]) -> Option<(Date, f64)> {
    found
        .iter()
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(_, on, y)| (*on, *y))
}

/// The yield in percent as a figure's decimal: MSRB G-33's rounding for a
/// municipal.
fn yield_figure(held: &Held, fraction: f64, scale: u32) -> Option<Dec> {
    let percent = exact(fraction * 100.0, 12)?;
    Some(if held.municipal() {
        g33_yield(&percent)
    } else {
        percent.round_half_even(scale)
    })
}

/// One unit: a position measure over an account's debt positions.
pub fn compute(
    case: &Case,
    key: &MeasureKey,
    definition: &Definition,
    account: &str,
) -> Result<Part> {
    let mut part = Part::default();
    let base = case.base(account)?.to_string();
    let positions: Vec<crate::case::Position> = case
        .positions
        .iter()
        .filter(|p| p.account_id == account)
        .cloned()
        .collect();
    let mut out = Out {
        key,
        account,
        scale: definition.scale,
        part: &mut part,
    };
    // The curve, fitted once for the unit where the measure reads it.
    let curve = if reads_curve(definition.definition) {
        Some(fit(case, out.part))
    } else {
        None
    };
    for p in &positions {
        if p.instrument_id.starts_with("LCL-") {
            // A local record is never priced; only a debt one is asked of.
            continue;
        }
        let Some(record) = case.instruments.get(&p.instrument_id) else {
            continue;
        };
        let Some(held) = record.debt.as_ref() else {
            continue;
        };
        // A pool's, a TBA's and a class's yield, risk and spread wait for a
        // prepayment model (688); their measures are the factors' and the
        // yield at a stated CPR.
        let bond_like = matches!(held.debt, super::Debt::Bond(_) | super::Debt::Cd(_));
        if !bond_like
            && !matches!(
                definition.definition,
                "accrued_interest" | "historical_prepayment" | "wal_at_cpr"
            )
        {
            continue;
        }
        out.part.reads.instruments.insert(p.instrument_id.clone());
        out.part
            .reads
            .models
            .insert((record.model_id.clone(), record.model_version));
        let position = Position {
            id: &p.instrument_id,
            face: p.quantity.clone(),
            held,
            record,
        };
        let done = match definition.definition {
            "accrued_interest" => accrued_interest(case, &position, &base, &mut out),
            "bond_yield" => bond_yield(case, &position, &mut out),
            "bond_price" => bond_price(case, &position, &mut out),
            "bond_risk" => bond_risk(case, &position, &base, &mut out),
            "key_rate_durations" | "bond_spread" => match curve.as_ref().expect("fitted") {
                Ok(curve) => {
                    if definition.definition == "bond_spread" {
                        bond_spread(case, &position, curve, &mut out)
                    } else {
                        key_rate_durations(case, &position, curve, &mut out)
                    }
                }
                Err(why) => Err(why.clone()),
            },
            "historical_prepayment" => historical_prepayment(case, &position, &mut out),
            "wal_at_cpr" => wal_at_cpr(case, &position, definition, &mut out),
            "make_whole_amount" => make_whole_amount(case, &position, &base, &mut out),
            other => return Err(CaseError(format!("no computation for {other}"))),
        };
        if let Err((reason, detail)) = done {
            out.not(&p.instrument_id, reason, detail);
        }
    }
    Ok(part)
}

/// The position's close and its source time, read into the provenance.
fn close<'a>(
    case: &'a Case,
    position: &Position<'_>,
    out: &mut Out<'_>,
) -> std::result::Result<&'a Price, Why> {
    let price = own_price(case, position.id, &position.record.price_kind)?;
    if !price.dataset.is_empty() {
        out.part.reads.datasets.insert(price.dataset.clone());
    }
    out.part
        .reads
        .input_prices
        .entry(price.subject_id.clone())
        .or_insert_with(|| {
            serde_json::json!({
                "instrument_id": price.subject_id,
                "price": {"amount": price.amount.text(), "instrument_id": price.instrument_id},
                "dataset": price.dataset,
                "row_key": price.row_key,
                "version": price.version.to_string(),
                "source_ns": price.source_ns.to_string(),
            })
        });
    Ok(price)
}

/// The rate from the price's currency to the account's base, its legs read.
fn rate_to_base(
    case: &Case,
    from: &str,
    base: &str,
    out: &mut Out<'_>,
) -> std::result::Result<Dec, Why> {
    let legs = conversion(case, from, base)?;
    model::read_legs(&mut out.part.reads, &legs);
    Ok(product(&legs))
}

/// The face outstanding: original face times the factor for a pool or a
/// class (680), else face.
fn outstanding(
    case: &Case,
    position: &Position<'_>,
    out: &mut Out<'_>,
) -> std::result::Result<Dec, Why> {
    if !position.held.factored() {
        return Ok(position.face.clone());
    }
    let row = factor_at(case, position.id)?;
    if !row.dataset.is_empty() {
        out.part.reads.datasets.insert(row.dataset.clone());
    }
    Ok(position.face.mul(&row.factor))
}

/// Accrued interest per 100 as of the job's date (684: computed, never a
/// stored balance): a bond's or a CD's on its schedule; a pool's or a
/// class's at its rate on 30/360 from the month's first day (the agencies'
/// convention); none on a TBA before it settles.
fn accrued_per_100(case: &Case, held: &Held, on: Date) -> std::result::Result<Ratio, Why> {
    match &held.debt {
        super::Debt::Pool(_) | super::Debt::Tranche(_) => {
            if let Some(why) = held.lacking(false) {
                return Err(why);
            }
            let rate = held
                .coupon_percent()
                .ok_or((TERMS_INCOMPLETE, "coupon_rate".to_string()))?;
            let first = Date::from_ymd(on.year(), on.month(), 1).unwrap_or(on);
            Ok(rate
                .to_ratio()
                .mul(&DayCount::Thirty360BondBasis.fraction(first, on, None, None)))
        }
        super::Debt::Tba(_) => Ok(Ratio::zero()),
        _ => {
            let calendar = calendar(case, held);
            let shape = held.shape_for(&calendar, false)?;
            let schedule = shape.build();
            Ok(accrued(&shape, &schedule, on))
        }
    }
}

fn accrued_interest(
    case: &Case,
    position: &Position<'_>,
    base: &str,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    let on = Date::parse(&case.valuation_date)
        .map_err(|_| (PRICE_MISSING, "the job's date".to_string()))?;
    if let Some(why) = position.held.valuation_gap() {
        return Err(why);
    }
    // Accrued from the terms and the date alone (684); the clean and dirty
    // values need the close as well, and are not evaluated without it.
    let per_100 = accrued_per_100(case, position.held, on)?;
    let face = outstanding(case, position, out)?;
    let currency = position.record.currency_instrument_id.clone();
    let rate = rate_to_base(case, &currency, base, out)?;
    let scale = out.scale;
    let hundredth = |per: &Ratio| {
        per.mul(&face.to_ratio())
            .div_small(100)
            .mul(&rate.to_ratio())
    };
    let accrued_value = hundredth(&per_100).round_half_even(scale);
    let priced =
        close(case, position, out).and_then(|price| Ok((clean_of(price)?, price.source_ns)));
    match priced {
        Ok((clean, src)) => {
            let clean_value = hundredth(&clean.to_ratio()).round_half_even(scale);
            let dirty = clean_value.add(&accrued_value);
            out.exact(
                position.id,
                vec![(COMPONENT, "clean".into())],
                clean_value,
                base,
                src,
            );
            out.exact(
                position.id,
                vec![(COMPONENT, "accrued".into())],
                accrued_value,
                base,
                0,
            );
            out.exact(
                position.id,
                vec![(COMPONENT, "dirty".into())],
                dirty,
                base,
                src,
            );
        }
        Err((reason, detail)) => {
            out.exact(
                position.id,
                vec![(COMPONENT, "accrued".into())],
                accrued_value,
                base,
                0,
            );
            out.not(position.id, reason, format!("clean and dirty: {detail}"));
        }
    }
    Ok(())
}

fn bond_yield(
    case: &Case,
    position: &Position<'_>,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    let b = bond(case, position.held)?;
    let price = close(case, position, out)?;
    let clean = clean_of(price)?;
    let src = price.source_ns;
    out.part
        .reads
        .models
        .insert((super::BOND_YIELD_SOLVER.to_string(), 1));
    let dirty = clean.to_ratio().add(&b.accrued(b.settle)).to_f64();
    let found = yields(&b, dirty);
    if found.is_empty() {
        return Err((
            PRICE_MISSING,
            format!("no yield prices {} per 100", clean.text()),
        ));
    }
    let scale = out.scale;
    for (kind, on, y) in &found {
        if *kind == "to_call" {
            continue;
        }
        if let Some(value) = yield_figure(position.held, *y, scale) {
            out.exact(
                position.id,
                vec![("yield_kind", (*kind).into()), ("to_date", on.to_string())],
                value,
                "",
                src,
            );
            mark_model(out);
        }
    }
    // To call: to the first call date on or after settlement.
    if let Some((_, on, y)) = found
        .iter()
        .filter(|(kind, _, _)| *kind == "to_call")
        .min_by_key(|(_, on, _)| *on)
    {
        if let Some(value) = yield_figure(position.held, *y, scale) {
            out.exact(
                position.id,
                vec![
                    ("yield_kind", "to_call".into()),
                    ("to_date", on.to_string()),
                ],
                value,
                "",
                src,
            );
            mark_model(out);
        }
    }
    for (_, on, price) in b.redemptions(true).into_iter().skip(1) {
        if let Some(y) = b.to(on, &price).yield_of(dirty) {
            if let Some(value) = yield_figure(position.held, y, scale) {
                out.exact(
                    position.id,
                    vec![("yield_kind", "to_put".into()), ("to_date", on.to_string())],
                    value,
                    "",
                    src,
                );
                mark_model(out);
            }
            break;
        }
    }
    if let Some((on, y)) = worst(&found) {
        if let Some(value) = yield_figure(position.held, y, scale) {
            out.exact(
                position.id,
                vec![
                    ("yield_kind", "to_worst".into()),
                    ("to_date", on.to_string()),
                ],
                value,
                "",
                src,
            );
            mark_model(out);
        }
    }
    // Current: the annual coupon over the clean price.
    if let Some(coupon) = position.held.coupon_percent().filter(|c| c.signum() > 0) {
        if clean.signum() > 0 {
            let current = coupon.to_ratio().mul(&hundred()).div(&clean.to_ratio());
            out.exact(
                position.id,
                vec![("yield_kind", "current".into()), ("to_date", String::new())],
                current.round_half_even(scale),
                "",
                src,
            );
        }
    }
    Ok(())
}

fn mark_model(out: &mut Out<'_>) {
    if let Some(last) = out.part.figures.last_mut() {
        last.computed_as = MODEL;
    }
}

/// The yield (a fraction) a position is quoted at, and the date it is to:
/// its row's quoted yield, a spread over its benchmark's yield, or the yield
/// its clean close is at.
fn quoted(
    case: &Case,
    position: &Position<'_>,
    b: &Bond<'_>,
    out: &mut Out<'_>,
) -> std::result::Result<(f64, Date, i64), Why> {
    let rows: Vec<&Price> = case
        .quoted_yields
        .get(position.id)
        .map(|rows| rows.iter().collect())
        .unwrap_or_default();
    let latest = rows.iter().max_by_key(|p| p.valid_at_ns).copied();
    let price = match latest {
        Some(row) => row,
        None => close(case, position, out)?,
    };
    if !price.dataset.is_empty() {
        out.part.reads.datasets.insert(price.dataset.clone());
    }
    if let Some(q) = &price.quoted_yield {
        let on = Date::parse(&q.to_date).unwrap_or(b.schedule.maturity);
        return Ok((q.value.to_f64() / 100.0, on, price.source_ns));
    }
    if price.basis == "PRICE_BASIS_SPREAD" {
        let spread = price
            .spread_bps
            .as_ref()
            .ok_or((PRICE_MISSING, "spread_bps".to_string()))?;
        let benchmark = benchmark_yield(
            case,
            &price.benchmark_instrument_id,
            b.schedule.maturity,
            out,
        )?;
        return Ok((
            benchmark + spread.to_f64() / 10_000.0,
            b.schedule.maturity,
            price.source_ns,
        ));
    }
    Err((
        PRICE_MISSING,
        "a yield or a spread to price from".to_string(),
    ))
}

fn bond_price(
    case: &Case,
    position: &Position<'_>,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    let b = bond(case, position.held)?;
    let scale = out.scale;
    // A close in percent of par is its own clean price.
    if let Ok(price) = close(case, position, out) {
        if let Ok(clean) = clean_of(price) {
            let dirty = clean
                .to_ratio()
                .add(&b.accrued(b.settle))
                .round_half_even(scale);
            out.exact(
                position.id,
                vec![(COMPONENT, "clean".into())],
                clean,
                "",
                price.source_ns,
            );
            out.exact(
                position.id,
                vec![(COMPONENT, "dirty".into())],
                dirty,
                "",
                price.source_ns,
            );
            return Ok(());
        }
    }
    let (y, on, src) = quoted(case, position, &b, out)?;
    out.part
        .reads
        .models
        .insert((super::BOND_YIELD_SOLVER.to_string(), 1));
    let redemption = b
        .redemptions(false)
        .into_iter()
        .find(|(_, date, _)| *date == on)
        .map(|(_, _, price)| price)
        .unwrap_or_else(hundred);
    let priceable = b.to(on, &redemption);
    let accrued = b.accrued(b.settle);
    // Treasury's price from a yield is exact (Appendix B).
    let percent = Dec::parse(&format!("{:.12}", y * 100.0)).map_err(|w| (PRICE_MISSING, w))?;
    if priceable.method == Method::Treasury {
        if let Some(dirty) = priceable.treasury_dirty_exact(&percent.to_ratio()) {
            let dirty = dirty.round_half_up(6);
            let accrued = accrued.round_half_up(6);
            out.exact(
                position.id,
                vec![(COMPONENT, "clean".into())],
                dirty.sub(&accrued),
                "",
                src,
            );
            out.exact(
                position.id,
                vec![(COMPONENT, "dirty".into())],
                dirty,
                "",
                src,
            );
            return Ok(());
        }
    }
    let dirty = priceable.dirty_at(y);
    out.model(
        position.id,
        vec![(COMPONENT, "clean".into())],
        dirty - accrued.to_f64(),
        "",
        src,
    );
    out.model(
        position.id,
        vec![(COMPONENT, "dirty".into())],
        dirty,
        "",
        src,
    );
    Ok(())
}

/// The yield (a fraction) of a named benchmark at `maturity`: a bond's
/// from its close, or a curve's par yield interpolated there.
fn benchmark_yield(
    case: &Case,
    benchmark: &str,
    maturity: Date,
    out: &mut Out<'_>,
) -> std::result::Result<f64, Why> {
    if benchmark.is_empty() {
        return Err((PRICE_MISSING, "the benchmark: none named".to_string()));
    }
    if let Some(curve) = case.curves.get(benchmark) {
        let date = Date::parse(&case.valuation_date)
            .map_err(|_| (PRICE_MISSING, "the job's date".to_string()))?;
        if !curve.dataset.is_empty() {
            out.part.reads.datasets.insert(curve.dataset.clone());
        }
        let fitted = fitted(curve, date)?;
        return Ok(fitted.par(maturity) / 100.0);
    }
    let record = case
        .instruments
        .get(benchmark)
        .ok_or((PRICE_MISSING, format!("the benchmark {benchmark}")))?;
    let held = record
        .debt
        .as_ref()
        .ok_or((PRICE_MISSING, format!("the benchmark {benchmark}: no bond")))?;
    let b = bond(case, held)?;
    let price = own_price(case, benchmark, &record.price_kind)?;
    if !price.dataset.is_empty() {
        out.part.reads.datasets.insert(price.dataset.clone());
    }
    let dirty = clean_of(price)?
        .to_ratio()
        .add(&b.accrued(b.settle))
        .to_f64();
    b.to(b.schedule.maturity, &hundred())
        .yield_of(dirty)
        .ok_or((PRICE_MISSING, format!("the benchmark {benchmark}'s yield")))
}

/// The yield to worst (a fraction), its date and redemption price, and
/// the dirty price per 100 the close is at.
fn at_worst(
    case: &Case,
    position: &Position<'_>,
    b: &Bond<'_>,
    out: &mut Out<'_>,
) -> std::result::Result<(f64, Date, Ratio, f64, i64), Why> {
    let price = close(case, position, out)?;
    let clean = clean_of(price)?;
    out.part
        .reads
        .models
        .insert((super::BOND_YIELD_SOLVER.to_string(), 1));
    let dirty = clean.to_ratio().add(&b.accrued(b.settle)).to_f64();
    let found = yields(b, dirty);
    let (on, y) = worst(&found).ok_or((
        PRICE_MISSING,
        format!("no yield prices {} per 100", clean.text()),
    ))?;
    let redemption = b
        .redemptions(false)
        .into_iter()
        .find(|(_, date, _)| *date == on)
        .map(|(_, _, p)| p)
        .unwrap_or_else(hundred);
    Ok((y, on, redemption, dirty, price.source_ns))
}

fn bond_risk(
    case: &Case,
    position: &Position<'_>,
    base: &str,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    let b = bond(case, position.held)?;
    let (y, on, redemption, dirty, src) = at_worst(case, position, &b, out)?;
    let priceable = b.to(on, &redemption);
    let (macaulay, modified, convexity) = priceable.risk(y);
    let dv01 = modified * dirty / 10_000.0;
    out.model(
        position.id,
        vec![(COMPONENT, "macaulay_duration".into())],
        macaulay,
        "",
        src,
    );
    out.model(
        position.id,
        vec![(COMPONENT, "modified_duration".into())],
        modified,
        "",
        src,
    );
    out.model(
        position.id,
        vec![(COMPONENT, "convexity".into())],
        convexity,
        "",
        src,
    );
    out.model(
        position.id,
        vec![(COMPONENT, "dv01_per_100".into())],
        dv01,
        "",
        src,
    );
    let price = close(case, position, out)?;
    let rate = rate_to_base(case, &price.instrument_id, base, out)?;
    let face = outstanding(case, position, out)?;
    out.model(
        position.id,
        vec![(COMPONENT, "dv01".into())],
        dv01 * face.to_f64() / 100.0 * rate.to_f64(),
        base,
        src,
    );
    Ok(())
}

/// A par curve's points fitted at the job's date.
fn fitted(curve: &ParCurve, date: Date) -> std::result::Result<Curve, Why> {
    if !curve.missing.is_empty() {
        let reason = if curve.missing_reason == "not_entitled" {
            NOT_ENTITLED
        } else {
            PRICE_MISSING
        };
        return Err((reason, curve.missing.clone()));
    }
    let points: Vec<ParPoint> = curve
        .points
        .iter()
        .filter_map(|(label, count, unit, par)| {
            Some(ParPoint {
                tenor: label.clone(),
                maturity: maturity_of(date, *count, unit)?,
                par: par.to_f64(),
            })
        })
        .collect();
    if points.is_empty() {
        return Err((
            PRICE_MISSING,
            format!("{}: no par yields at the as-of", curve.subject_id),
        ));
    }
    Curve::fit(date, points).ok_or((
        FIT_FAILED,
        format!("{}: the par yields did not fit", curve.subject_id),
    ))
}

/// The Treasury curve at the job's date, its read named.
fn fit(case: &Case, part: &mut Part) -> std::result::Result<Curve, Why> {
    let curve = case.treasury_curve.as_ref().ok_or((
        PRICE_MISSING,
        "the Treasury par curve: none read".to_string(),
    ))?;
    let date = Date::parse(&case.valuation_date)
        .map_err(|_| (PRICE_MISSING, "the job's date".to_string()))?;
    if !curve.dataset.is_empty() {
        part.reads.datasets.insert(curve.dataset.clone());
    }
    part.reads
        .models
        .insert((super::MONOTONE_CONVEX.to_string(), 1));
    fitted(curve, date)
}

/// The dirty price per 100 at settlement of flows discounted on the curve
/// plus a spread `s` (a fraction), semi-annual on actual/365 fixed.
pub fn on_curve(curve: &Curve, settle: Date, flows: &[super::schedule::Flow], s: f64) -> f64 {
    let discount = |on: Date| {
        let t = super::curve::years(curve.date, on);
        if t <= 0.0 {
            return 1.0;
        }
        let z = 2.0 * ((curve.zero(on) / 2.0).exp() - 1.0);
        (1.0 + (z + s) / 2.0).powf(-2.0 * t)
    };
    let at = discount(settle);
    flows
        .iter()
        .map(|flow| flow.amount.to_f64() * discount(flow.date) / at)
        .sum()
}

/// The spread (a fraction) over the curve the flows' dirty price is at.
pub fn z_spread(
    curve: &Curve,
    settle: Date,
    flows: &[super::schedule::Flow],
    dirty: f64,
) -> Option<f64> {
    let (mut lo, mut hi) = (-0.2, 1.0);
    let g = |s: f64| on_curve(curve, settle, flows, s) - dirty;
    if g(lo).signum() == g(hi).signum() {
        return None;
    }
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if g(mid) > 0.0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(0.5 * (lo + hi))
}

fn bond_spread(
    case: &Case,
    position: &Position<'_>,
    curve: &Curve,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    let b = bond(case, position.held)?;
    let (y, on, redemption, dirty, src) = at_worst(case, position, &b, out)?;
    let flows = b.to(on, &redemption).flows;
    out.model(
        position.id,
        vec![("spread_kind", "g_spread".into())],
        (y * 100.0 - curve.par(on)) * 100.0,
        "",
        src,
    );
    match z_spread(curve, b.settle, &flows, dirty) {
        Some(z) => out.model(
            position.id,
            vec![("spread_kind", "z_spread".into())],
            z * 10_000.0,
            "",
            src,
        ),
        None => out.not(
            position.id,
            FIT_FAILED,
            "z_spread: no spread prices the close".to_string(),
        ),
    }
    let price = close(case, position, out)?;
    if price.basis == "PRICE_BASIS_SPREAD" || !price.benchmark_instrument_id.is_empty() {
        match benchmark_yield(
            case,
            &price.benchmark_instrument_id,
            b.schedule.maturity,
            out,
        ) {
            Ok(benchmark) => out.model(
                position.id,
                vec![("spread_kind", "benchmark".into())],
                (y - benchmark) * 10_000.0,
                "",
                src,
            ),
            Err((reason, detail)) => out.not(position.id, reason, format!("benchmark: {detail}")),
        }
    }
    Ok(())
}

fn key_rate_durations(
    case: &Case,
    position: &Position<'_>,
    curve: &Curve,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    let b = bond(case, position.held)?;
    let (_, on, redemption, dirty, src) = at_worst(case, position, &b, out)?;
    let flows = b.to(on, &redemption).flows;
    let z = z_spread(curve, b.settle, &flows, dirty).ok_or((
        FIT_FAILED,
        "z_spread: no spread prices the close".to_string(),
    ))?;
    let base = on_curve(curve, b.settle, &flows, z);
    for (i, point) in curve.points.iter().enumerate() {
        let Some(bumped) = curve.bumped(i, 1.0) else {
            out.not(
                position.id,
                FIT_FAILED,
                format!("{}: the bumped curve did not fit", point.tenor),
            );
            continue;
        };
        let moved = on_curve(&bumped, b.settle, &flows, z);
        out.model(
            position.id,
            vec![("tenor", point.tenor.clone())],
            (base - moved) / (base * 0.0001),
            "",
            src,
        );
    }
    Ok(())
}

/// Months between two YYYYMM months, `to` after `from`.
fn months_between(from: &str, to: &str) -> Option<i32> {
    let read = |m: &str| -> Option<i32> {
        if m.len() != 6 {
            return None;
        }
        Some(m[..4].parse::<i32>().ok()? * 12 + m[4..].parse::<i32>().ok()? - 1)
    };
    Some(read(to)? - read(from)?)
}

fn month_of(date: Date) -> String {
    format!("{:04}{:02}", date.year(), date.month())
}

fn month_back(month: &str, n: i32) -> String {
    let index = month[..4].parse::<i32>().unwrap_or(0) * 12
        + month[4..].parse::<i32>().unwrap_or(1)
        - 1
        - n;
    format!("{:04}{:02}", index.div_euclid(12), index.rem_euclid(12) + 1)
}

/// A pool's or a class's issue and maturity, term in months and rate (a
/// fraction).
fn amortizing(held: &Held) -> std::result::Result<(Date, Date, u32, f64), Why> {
    if let Some(why) = held.incomplete() {
        return Err(why);
    }
    let (issue, maturity, term) = match &held.debt {
        super::Debt::Pool(pool) => (
            pool.issue_date.as_str(),
            pool.maturity_date.as_str(),
            pool.original_term_months,
        ),
        super::Debt::Tranche(class) => (class.issue_date.as_str(), class.maturity_date.as_str(), 0),
        _ => return Err((TERMS_INCOMPLETE, "a pool's or a class's terms".to_string())),
    };
    let issue = Date::parse(issue).map_err(|_| (TERMS_INCOMPLETE, "issue_date".to_string()))?;
    let maturity =
        Date::parse(maturity).map_err(|_| (TERMS_INCOMPLETE, "maturity_date".to_string()))?;
    let term = if term > 0 {
        term
    } else {
        months_between(&month_of(issue), &month_of(maturity))
            .unwrap_or(0)
            .max(1) as u32
    };
    let rate = held
        .coupon_percent()
        .ok_or((TERMS_INCOMPLETE, "coupon_rate".to_string()))?
        .to_f64()
        / 100.0;
    Ok((issue, maturity, term, rate))
}

fn historical_prepayment(
    case: &Case,
    position: &Position<'_>,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    if !position.held.factored() {
        return Ok(());
    }
    let (issue, _, term, rate) = amortizing(position.held)?;
    let latest = factor_at(case, position.id)?;
    let rows = case.factors.get(position.id).cloned().unwrap_or_default();
    for row in &rows {
        if !row.dataset.is_empty() {
            out.part.reads.datasets.insert(row.dataset.clone());
        }
    }
    let at = |month: &str| rows.iter().find(|r| r.factor_month == month);
    let issued = month_of(issue);
    let src = latest.source_ns;
    let mut windows: Vec<(String, Option<(f64, i32)>)> = Vec::new();
    for n in [1, 3, 6, 12] {
        let from = month_back(&latest.factor_month, n);
        let start = if from == issued {
            Some(1.0)
        } else if from < issued {
            None
        } else {
            at(&from).map(|r| r.factor.to_f64())
        };
        windows.push((format!("{n}m"), start.map(|s| (s, n))));
    }
    let life = months_between(&issued, &latest.factor_month).filter(|n| *n > 0);
    windows.push(("life".to_string(), life.map(|n| (1.0, n))));
    let end = latest.factor.to_f64();
    for (window, start) in windows {
        let Some((start, n)) = start else {
            out.not(
                position.id,
                HISTORY_INSUFFICIENT,
                format!(
                    "{window}: the factor {} months before {} is not held",
                    window.trim_end_matches('m'),
                    latest.factor_month
                ),
            );
            continue;
        };
        let age = months_between(&issued, &latest.factor_month).unwrap_or(0) - n;
        match super::mbs::smm_cpr(start, end, n as u32, age.max(0) as u32, term, rate) {
            Some((smm, cpr)) => {
                out.model(
                    position.id,
                    vec![("window", window.clone()), (COMPONENT, "smm".into())],
                    smm * 100.0,
                    "",
                    src,
                );
                out.model(
                    position.id,
                    vec![("window", window.clone()), (COMPONENT, "cpr".into())],
                    cpr * 100.0,
                    "",
                    src,
                );
            }
            None => out.not(
                position.id,
                HISTORY_INSUFFICIENT,
                format!("{window}: no scheduled balance"),
            ),
        }
    }
    Ok(())
}

fn wal_at_cpr(
    case: &Case,
    position: &Position<'_>,
    definition: &Definition,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    if !position.held.factored() {
        return Ok(());
    }
    let (_, maturity, _, rate) = amortizing(position.held)?;
    let latest = factor_at(case, position.id)?;
    if !latest.dataset.is_empty() {
        out.part.reads.datasets.insert(latest.dataset.clone());
    }
    let cpr: f64 = definition
        .parameter("cpr")
        .and_then(|c| c.parse().ok())
        .unwrap_or(6.0)
        / 100.0;
    let left = months_between(&latest.factor_month, &month_of(maturity))
        .unwrap_or(0)
        .max(0) as u32;
    let projection = super::mbs::project(rate, left, cpr);
    let src = latest.source_ns;
    out.model(
        position.id,
        vec![(COMPONENT, "wal".into())],
        projection.wal(),
        "",
        src,
    );
    let date = Date::parse(&case.valuation_date)
        .map_err(|_| (PRICE_MISSING, "the job's date".to_string()))?;
    let price = close(case, position, out)?;
    let clean = clean_of(price)?.to_f64() / 100.0;
    let accrued = accrued_per_100(case, position.held, date)?.to_f64() / 100.0;
    let delay = match &position.held.debt {
        super::Debt::Pool(pool) => pool.payment_delay_days,
        super::Debt::Tranche(class) => class.payment_delay_days,
        _ => 0,
    };
    match projection.yield_of(clean + accrued, delay) {
        Some((_, bey)) => out.model(
            position.id,
            vec![(COMPONENT, "yield".into())],
            bey * 100.0,
            "",
            price.source_ns,
        ),
        None => out.not(
            position.id,
            PRICE_MISSING,
            format!("no yield prices {clean} per 1 of face"),
        ),
    }
    Ok(())
}

/// A par curve's rate in percent at `on`, straight-line between the
/// tenors either side by actual days, flat past the ends (a prospectus's
/// "Treasury Rate", interpolated to the par call date).
fn interpolated(curve: &ParCurve, date: Date, on: Date) -> Option<f64> {
    let mut points: Vec<(i64, f64)> = curve
        .points
        .iter()
        .filter_map(|(_, count, unit, par)| {
            Some((days(date, maturity_of(date, *count, unit)?), par.to_f64()))
        })
        .collect();
    points.sort_by_key(|p| p.0);
    let target = days(date, on);
    let first = *points.first()?;
    let last = *points.last()?;
    if target <= first.0 {
        return Some(first.1);
    }
    if target >= last.0 {
        return Some(last.1);
    }
    let pair = points
        .windows(2)
        .find(|w| w[0].0 <= target && target <= w[1].0)?;
    let (a, b) = (pair[0], pair[1]);
    Some(a.1 + (b.1 - a.1) * (target - a.0) as f64 / (b.0 - a.0) as f64)
}

fn make_whole_amount(
    case: &Case,
    position: &Position<'_>,
    base: &str,
    out: &mut Out<'_>,
) -> std::result::Result<(), Why> {
    let date = Date::parse(&case.valuation_date)
        .map_err(|_| (PRICE_MISSING, "the job's date".to_string()))?;
    let calls = position.held.calls();
    let Some(window) = calls
        .iter()
        .find(|w| w.make_whole && w.from <= date && w.to.is_none_or(|to| date <= to))
    else {
        // No make-whole window in force: no figure.
        return Ok(());
    };
    let calendar = calendar(case, position.held);
    let shape = position.held.shape(&calendar)?;
    let schedule = shape.build();
    // The par call date: the first par window's opening after it, else
    // maturity (R2).
    let par_call = calls
        .iter()
        .filter(|w| !w.make_whole && w.from > date)
        .map(|w| w.from)
        .min()
        .unwrap_or(schedule.maturity);
    let curve = case.curves.get(&window.benchmark).ok_or((
        PRICE_MISSING,
        format!("the make-whole benchmark {}", window.benchmark),
    ))?;
    if !curve.missing.is_empty() {
        return Err((PRICE_MISSING, curve.missing.clone()));
    }
    if !curve.dataset.is_empty() {
        out.part.reads.datasets.insert(curve.dataset.clone());
    }
    let treasury = interpolated(curve, date, par_call).ok_or((
        PRICE_MISSING,
        format!("{}: no constant maturities at the as-of", window.benchmark),
    ))?;
    let rate =
        (treasury + window.spread_bps.as_ref().map(Dec::to_f64).unwrap_or(0.0) / 100.0) / 100.0;
    let flows = flows_to(&shape, &schedule, date, par_call, &hundred());
    let pv: f64 = flows
        .iter()
        .map(|flow| {
            let periods = DayCount::Thirty360BondBasis.days(date, flow.date, None) as f64 / 180.0;
            flow.amount.to_f64() / (1.0 + rate / 2.0).powf(periods)
        })
        .sum();
    let accrued = accrued(&shape, &schedule, date).to_f64();
    let per_100 = (pv - accrued).max(100.0);
    let src = curve.source_ns;
    out.model(
        position.id,
        vec![(COMPONENT, "price".into())],
        per_100,
        "",
        src,
    );
    let currency = position.record.currency_instrument_id.clone();
    let fx = rate_to_base(case, &currency, base, out)?;
    out.model(
        position.id,
        vec![(COMPONENT, "amount".into())],
        per_100 * position.face.to_f64() / 100.0 * fx.to_f64(),
        base,
        src,
    );
    Ok(())
}

/// `treasury_curve`: the fitted curve's par, zero and discount at each of
/// its tenors, at the job's date.
pub fn treasury_curve(case: &Case, key: &MeasureKey, definition: &Definition) -> Result<Part> {
    let mut part = Part::default();
    let curve = fit(case, &mut part);
    let src = case
        .treasury_curve
        .as_ref()
        .map(|c| c.source_ns)
        .unwrap_or(0);
    let mut out = Out {
        key,
        account: "",
        scale: definition.scale,
        part: &mut part,
    };
    match curve {
        Err((reason, detail)) => out.not("", reason, detail),
        Ok(curve) => {
            for point in &curve.points {
                let mut figure = |kind: &str, value: f64| {
                    if let Some(v) = exact(value, definition.scale) {
                        out.part.figures.push(Figure {
                            measure: key.clone(),
                            breakdown: vec![
                                ("curve".to_string(), kind.to_string()),
                                ("tenor".to_string(), point.tenor.clone()),
                            ],
                            value: Some(v),
                            computed_as: MODEL,
                            oldest_input_source_ns: src,
                            ..Default::default()
                        });
                    }
                };
                figure("par", curve.par(point.maturity));
                figure("zero", curve.zero(point.maturity) * 100.0);
                figure("discount", curve.discount(point.maturity));
            }
        }
    }
    Ok(part)
}
