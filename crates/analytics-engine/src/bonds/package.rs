//! The debt templates' calculation packages' test cases, as the engine
//! answers them (contract v29; W1.24, W12.3; ruling 984: the platform's test
//! cases set the inputs' names, `bond_cash_flows`, `factor` and
//! `quoted_price`).
//!
//! Every implementation routed for a model must equal each case exactly
//! (the platform's packages, published with the five templates). The
//! inputs a case names say which computation it is:
//!
//! - `high_yield`: a Treasury note's or bond's price and accrued interest
//!   per 100 at its auction's high yield on its settlement date, by 31 CFR
//!   356 Appendix B, each rounded half up to six places;
//! - `high_discount_rate`: a bill's price per 100 from its discount rate;
//! - `accrual_start_date`: accrued interest per 100 on the day count from
//!   that date to settlement (MSRB Rule G-33's 30/360; a CD's actual/365);
//! - `factor` and `prior_factor`: a pool's or a class's month, its interest
//!   on the face outstanding before the month's payment for one month of the
//!   day count and its principal the fall in face, each rounded once to
//!   cents;
//! - `close` and `basis`: a TBA's market value from its close in its price
//!   basis (984), rounded once to cents.

use meridian_domain::date::Date;
use serde_json::{json, Map, Value};

use super::dates::{add_months, Calendar, Roll};
use super::daycount::{ratio, DayCount};
use super::pricing::{bill_price_from_discount, Grid, Method, Priceable};
use super::schedule::{accrued, flows_to, hundred, Shape};
use crate::exact::{Dec, Ratio};

fn text<'a>(inputs: &'a Value, name: &str) -> Result<&'a str, String> {
    inputs
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name}: a case states it as text"))
}

fn number(inputs: &Value, name: &str) -> Result<Dec, String> {
    Dec::parse(text(inputs, name)?).map_err(|why| format!("{name}: {why}"))
}

fn day(inputs: &Value, name: &str) -> Result<Date, String> {
    Date::parse(text(inputs, name)?).map_err(|why| format!("{name}: {why}"))
}

/// A case's answer from its inputs, each figure a decimal's text.
pub fn answer(inputs: &Value) -> Result<Value, String> {
    let has = |name: &str| inputs.get(name).is_some();
    let mut out = Map::new();
    if has("high_yield") {
        let shape = Shape {
            accrues_from: day(inputs, "dated_date")?,
            first_coupon: Some(day(inputs, "first_coupon_date")?),
            maturity: day(inputs, "maturity_date")?,
            frequency: text(inputs, "coupon_frequency")?
                .parse()
                .map_err(|_| "coupon_frequency: a whole number".to_string())?,
            day_count: DayCount::named(text(inputs, "day_count")?)?,
            // A Treasury maturing on a month's last day pays on each
            // month's last day.
            end_of_month: super::dates::is_month_end(day(inputs, "maturity_date")?),
            rate: number(inputs, "coupon_rate")?.to_ratio(),
            steps: Vec::new(),
            calendar: Calendar::default(),
            roll: Roll::Unadjusted,
        };
        let schedule = shape.build();
        let settle = day(inputs, "settlement_date")?;
        let accrued = accrued(&shape, &schedule, settle);
        let priceable = Priceable {
            method: Method::Treasury,
            grid: Grid::of(&schedule),
            settle,
            flows: flows_to(&shape, &schedule, settle, shape.maturity, &hundred()),
            accrued: accrued.clone(),
        };
        let dirty = priceable
            .treasury_dirty_exact(&number(inputs, "high_yield")?.to_ratio())
            .ok_or("the flows are off Treasury's half-year grid")?
            .round_half_up(6);
        let accrued = accrued.round_half_up(6);
        out.insert("accrued_per100".into(), json!(accrued.text()));
        out.insert("price_per100".into(), json!(dirty.sub(&accrued).text()));
    } else if has("high_discount_rate") {
        let settle = day(inputs, "settlement_date")?;
        let maturity = day(inputs, "maturity_date")?;
        let price = bill_price_from_discount(
            &number(inputs, "high_discount_rate")?.to_ratio(),
            super::dates::days(settle, maturity),
        );
        out.insert("price_per100".into(), json!(price.round_half_up(6).text()));
    } else if has("accrual_start_date") {
        let start = day(inputs, "accrual_start_date")?;
        let settle = day(inputs, "settlement_date")?;
        let count = DayCount::named(text(inputs, "day_count")?)?;
        let rate = number(inputs, "coupon_rate")?.to_ratio();
        let accrued = rate.mul(&count.fraction(start, settle, None, None));
        out.insert(
            "accrued_per100".into(),
            json!(accrued.round_half_up(6).text()),
        );
    } else if has("factor") && has("prior_factor") {
        let (interest, principal) = month(inputs)?;
        out.insert("interest".into(), json!(interest.text()));
        out.insert("principal".into(), json!(principal.text()));
    } else if has("close") && has("basis") {
        let close = number(inputs, "close")?;
        let per_unit = per_unit_in_basis(&close, text(inputs, "basis")?)?;
        let value = per_unit
            .mul(&number(inputs, "quantity")?)
            .mul(&number(inputs, "fx")?);
        out.insert(
            "market_value".into(),
            json!(value.round_half_even(2).text()),
        );
    } else {
        return Err("a case names none of the inputs the engine computes from".to_string());
    }
    Ok(Value::Object(out))
}

/// A pool's or a class's month (`bond_cash_flows` with `factor`): interest
/// on the face outstanding before the month's payment, for one month on the
/// day count, and principal the fall in face; each rounded once to cents.
pub fn month(inputs: &Value) -> Result<(Dec, Dec), String> {
    let face = number(inputs, "original_face")?;
    let factor = number(inputs, "factor")?;
    let prior = number(inputs, "prior_factor")?;
    let rate = match inputs.get("pass_through_rate") {
        Some(_) => number(inputs, "pass_through_rate")?,
        None => number(inputs, "coupon_rate")?,
    };
    let count = DayCount::named(text(inputs, "day_count")?)?;
    Ok(month_of(&face, &prior, &factor, &rate, count))
}

/// One month's interest and principal on `face` of original face.
pub fn month_of(face: &Dec, prior: &Dec, factor: &Dec, rate: &Dec, count: DayCount) -> (Dec, Dec) {
    let start = Date::from_ymd(2001, 1, 1).expect("a date");
    let one_month = if count.is_thirty() {
        ratio(30, 360)
    } else {
        count.fraction(start, add_months(start, 1, false), None, None)
    };
    let interest = face
        .mul(prior)
        .to_ratio()
        .mul(&rate.to_ratio())
        .div_small(100)
        .mul(&one_month)
        .round_half_even(2);
    let principal = face.mul(&prior.sub(factor)).round_half_even(2);
    (interest, principal)
}

/// A close's value per unit of face in its price basis (984): percent of
/// par is times 0.01 (the working agreement's ruling at stage B), per unit
/// is itself; a yield or a spread is no price, and refused naming it.
pub fn per_unit_in_basis(close: &Dec, basis: &str) -> Result<Dec, String> {
    match basis {
        "PRICE_BASIS_PERCENT_OF_PAR" => Ok(close
            .to_ratio()
            .div_small(100)
            .round_half_even(close.scale() + 2)),
        "" | "PRICE_BASIS_UNSPECIFIED" | "PRICE_BASIS_PER_UNIT" => Ok(close.clone()),
        other => Err(format!(
            "the price is quoted as {other}, no price per unit: a yield or a spread is \
             converted only through the bond's own solver, which no TBA has without a \
             prepayment model (v29)"
        )),
    }
}

/// A ratio's half-up rounding, for callers outside the package.
pub fn per_100(value: &Ratio) -> Dec {
    value.round_half_up(6)
}
