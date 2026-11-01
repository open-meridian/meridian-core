//! v29's curve and bond solvers (contract v29; sdk-contract/fixed-income-
//! contract; plans/the-v29-build, Track D; ruling 688; ENG1 §3 "v29:
//! bonds"; W12.1 to W12.3).
//!
//! Our own Rust under core's licence; QuantLib is the oracle in the tests
//! alone, never here and never in the image (565). The models:
//!
//! - [`schedule`] and [`daycount`], `bond_cash_flows` v1: a debt record's
//!   schedule, coupons, accrual and redemptions from its terms and the
//!   settlement calendars, exact; a pool's and a class's month from its
//!   `Factor` rows ([`package`] holds the template's cases);
//! - [`pricing`], `bond_yield_solver` v1: price to yield and back, by the
//!   instrument's day count and frequency -- Treasury's Appendix B, a bill's
//!   discount, and the street's (MSRB G-33's for a municipal);
//! - [`curve`], `monotone_convex` v1: Hagan and West's interpolation over
//!   the par yields, bootstrapped to par, zero and discount curves;
//! - [`mbs`]: historical SMM and CPR from the factors, and a WAL and yield
//!   at a stated CPR (static, no prepayment model).
//!
//! [`measures`] answers the eleven measure definitions over them.

pub mod curve;
pub mod dates;
pub mod daycount;
pub mod mbs;
pub mod measures;
pub mod package;
pub mod pricing;
pub mod schedule;

#[cfg(test)]
mod oracle;
#[cfg(test)]
mod tests;

use meridian_domain::date::Date;
use meridian_domain::debt;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    Bond, CertificateOfDeposit, CmoTranche, CouponType, DebtSecurityType, IssuerType, MbsPool,
    RedemptionKind, Tba,
};
use serde_json::Value;

use crate::exact::{Dec, Ratio};
use dates::{Calendar, Roll};
use daycount::DayCount;
use pricing::Method;
use schedule::Shape;

/// The model ids v29 adds (W12.3; the names approved 2026-10-26).
pub const BOND_CASH_FLOWS: &str = "bond_cash_flows";
pub const BOND_YIELD_SOLVER: &str = "bond_yield_solver";
pub const MONOTONE_CONVEX: &str = "monotone_convex";

/// A debt record's terms, by its type (W1.1). A bond's terms are larger
/// than the others'; one is read per record, so none is boxed.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Debt {
    Bond(Bond),
    Pool(MbsPool),
    Tba(Tba),
    Tranche(CmoTranche),
    Cd(CertificateOfDeposit),
}

/// A debt record as the inputs document carries it: its terms and the
/// fields it lacks (574, W1.24).
#[derive(Debug, Clone)]
pub struct Held {
    pub debt: Debt,
    pub missing: Vec<String>,
}

/// Why a measure is not evaluated for a record: its reason and detail.
pub type Why = (&'static str, String);

/// A debt record from its inputs document's `debt` member: `type`, `terms`
/// as the instrument store writes them (`meridian_domain::debt`), and
/// `missing_fields`.
pub fn read(value: &Value) -> Result<Held, String> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let terms = value.get("terms").cloned().unwrap_or(Value::Null);
    let debt = match kind {
        "bond" => Debt::Bond(debt::bond_from(&terms, "debt.terms")?),
        "mbs_pool" => Debt::Pool(debt::mbs_pool_from(&terms, "debt.terms")?),
        "tba" => Debt::Tba(debt::tba_from(&terms, "debt.terms")?),
        "cmo_tranche" => Debt::Tranche(debt::cmo_tranche_from(&terms, "debt.terms")?),
        "certificate_of_deposit" => {
            Debt::Cd(debt::certificate_of_deposit_from(&terms, "debt.terms")?)
        }
        other => return Err(format!("debt.type: {other:?} is no debt type")),
    };
    let missing = value
        .get("missing_fields")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(Held { debt, missing })
}

fn date(text: &str, field: &str) -> Result<Date, Why> {
    Date::parse(text).map_err(|_| (crate::answer::TERMS_INCOMPLETE, field.to_string()))
}

fn optional_date(text: &str) -> Option<Date> {
    Date::parse(text).ok()
}

/// A wire decimal, read: `decimal(field.as_ref().map(Exact::from_wire))`.
fn decimal<E>(value: Option<Result<Exact, E>>) -> Option<Dec> {
    Dec::parse(&value?.ok()?.to_string()).ok()
}

/// A redemption a bond may be called or put on: its date, its price per
/// 100, and whether it is a make-whole window.
#[derive(Debug, Clone)]
pub struct Redemption {
    pub from: Date,
    pub to: Option<Date>,
    pub price: Option<Ratio>,
    pub make_whole: bool,
    pub notice_days_min: u32,
    pub spread_bps: Option<Dec>,
    pub benchmark: String,
}

fn windows(rows: &[meridian_domain::v1::RedemptionWindow]) -> Vec<Redemption> {
    rows.iter()
        .filter_map(|row| {
            Some(Redemption {
                from: optional_date(&row.from_date)?,
                to: optional_date(&row.to_date),
                price: decimal(row.price.as_ref().map(Exact::from_wire)).map(|p| p.to_ratio()),
                make_whole: row.kind == RedemptionKind::MakeWhole as i32,
                notice_days_min: row.notice_days_min,
                spread_bps: decimal(row.make_whole_spread_bps.as_ref().map(Exact::from_wire)),
                benchmark: row.make_whole_benchmark_instrument_id.clone(),
            })
        })
        .collect()
}

impl Held {
    /// The fields it lacks that a measure needs (574): any, since a figure
    /// is never computed from a guess -- a callable bond minted without its
    /// calls not evaluated, naming `calls`, never a yield to maturity in
    /// its place (W12.1).
    pub fn incomplete(&self) -> Option<Why> {
        self.lacking(true)
    }

    /// The fields it lacks that a measure reads: accrual and a mark read
    /// the coupon's terms alone; a yield, risk or spread its redemptions
    /// (to worst) and its settlement's calendar too. A field no
    /// computation reads -- a denomination, a record date, a municipal's
    /// tax status -- never holds a figure back. A path counts by its last
    /// name (`calls`, `bond.calls`).
    pub fn lacking(&self, redemptions: bool) -> Option<Why> {
        const ACCRUAL: [&str; 14] = [
            "dated_date",
            "issue_date",
            "first_coupon_date",
            "maturity_date",
            "coupon_rate",
            "coupon_type",
            "coupon_frequency",
            "day_count",
            "end_of_month",
            "coupon_steps",
            "floating",
            "pass_through_rate",
            "original_term_months",
            "original_face",
        ];
        const REDEMPTION: [&str; 5] = [
            "calls",
            "puts",
            "sinking_funds",
            "payment_calendar_id",
            "business_day_convention",
        ];
        let named: Vec<&str> = self
            .missing
            .iter()
            .map(String::as_str)
            .filter(|path| {
                let last = path.rsplit('.').next().unwrap_or(path);
                ACCRUAL.contains(&last) || (redemptions && REDEMPTION.contains(&last))
            })
            .collect();
        (!named.is_empty()).then(|| (crate::answer::TERMS_INCOMPLETE, named.join(", ")))
    }

    /// Why its close alone does not value it (`bond_cash_flows`' market
    /// value, a clean price times face): a TIPS, whose principal accretes by
    /// the CPI-U index ratio, which v29 does not read.
    pub fn valuation_gap(&self) -> Option<Why> {
        match &self.debt {
            Debt::Bond(bond) if bond.security_type == DebtSecurityType::Tips as i32 => Some((
                crate::answer::PRICE_MISSING,
                "the CPI-U index ratio a TIPS's principal accretes by".to_string(),
            )),
            _ => None,
        }
    }

    /// The bond's or CD's schedule's shape, or why it has none in v29.
    pub fn shape(&self, calendar: &Calendar) -> Result<Shape, Why> {
        self.shape_for(calendar, true)
    }

    /// The shape, its terms complete for a measure reading its redemptions
    /// or not ([`Held::lacking`]).
    pub fn shape_for(&self, calendar: &Calendar, redemptions: bool) -> Result<Shape, Why> {
        if let Some(why) = self.lacking(redemptions) {
            return Err(why);
        }
        match &self.debt {
            Debt::Bond(bond) => {
                let coupon =
                    CouponType::try_from(bond.coupon_type).unwrap_or(CouponType::Unspecified);
                if coupon == CouponType::Floating || bond.floating.is_some() {
                    return Err((
                        crate::answer::PRICE_MISSING,
                        "a floater's index projection: its coupons are not projected in v29"
                            .to_string(),
                    ));
                }
                if bond.security_type == DebtSecurityType::Tips as i32 {
                    return Err((
                        crate::answer::PRICE_MISSING,
                        "the CPI-U index ratio a TIPS's principal accretes by".to_string(),
                    ));
                }
                let maturity = date(&bond.maturity_date, "maturity_date")?;
                let issue = optional_date(&bond.issue_date);
                let accrues_from = optional_date(&bond.dated_date)
                    .or(issue)
                    .ok_or((crate::answer::TERMS_INCOMPLETE, "dated_date".to_string()))?;
                let zero = coupon == CouponType::Zero
                    || matches!(
                        DebtSecurityType::try_from(bond.security_type),
                        Ok(DebtSecurityType::Tbill
                            | DebtSecurityType::Tint
                            | DebtSecurityType::Tprn
                            | DebtSecurityType::Fadn)
                    );
                let day_count = if zero && bond.day_count == 0 {
                    DayCount::ActActIcma
                } else {
                    DayCount::named(name_of_day_count(bond.day_count))
                        .map_err(|why| (crate::answer::TERMS_INCOMPLETE, why))?
                };
                let rate = if zero {
                    Ratio::zero()
                } else {
                    decimal(bond.coupon_rate.as_ref().map(Exact::from_wire))
                        .ok_or((crate::answer::TERMS_INCOMPLETE, "coupon_rate".to_string()))?
                        .to_ratio()
                };
                Ok(Shape {
                    accrues_from,
                    first_coupon: optional_date(&bond.first_coupon_date),
                    maturity,
                    frequency: if zero { 0 } else { bond.coupon_frequency },
                    day_count,
                    // A Treasury maturing on a month's last day pays on each
                    // month's last day, whatever the source said.
                    end_of_month: bond.end_of_month
                        || (self.method() == Method::Treasury && dates::is_month_end(maturity)),
                    rate,
                    steps: bond
                        .coupon_steps
                        .iter()
                        .filter_map(|s| {
                            Some((
                                optional_date(&s.from_date)?,
                                decimal(s.coupon_rate.as_ref().map(Exact::from_wire))?.to_ratio(),
                            ))
                        })
                        .collect(),
                    calendar: calendar.clone(),
                    roll: Roll::named(name_of_roll(bond.business_day_convention)),
                })
            }
            Debt::Cd(cd) => {
                let maturity = date(&cd.maturity_date, "maturity_date")?;
                let accrues_from = date(&cd.issue_date, "issue_date")?;
                let day_count = DayCount::named(name_of_day_count(cd.day_count))
                    .map_err(|why| (crate::answer::TERMS_INCOMPLETE, why))?;
                Ok(Shape {
                    accrues_from,
                    first_coupon: None,
                    maturity,
                    frequency: cd.coupon_frequency,
                    day_count,
                    end_of_month: false,
                    rate: decimal(cd.coupon_rate.as_ref().map(Exact::from_wire))
                        .ok_or((crate::answer::TERMS_INCOMPLETE, "coupon_rate".to_string()))?
                        .to_ratio(),
                    steps: Vec::new(),
                    calendar: calendar.clone(),
                    roll: Roll::named(name_of_roll(cd.business_day_convention)),
                })
            }
            _ => Err((
                crate::answer::TERMS_INCOMPLETE,
                "a pool's, a TBA's or a class's flows are its factors'".to_string(),
            )),
        }
    }

    /// How its price and yield are related.
    pub fn method(&self) -> Method {
        match &self.debt {
            Debt::Bond(bond) => match DebtSecurityType::try_from(bond.security_type) {
                Ok(DebtSecurityType::Tbill) => Method::Bill,
                Ok(
                    DebtSecurityType::Tnote
                    | DebtSecurityType::Tbond
                    | DebtSecurityType::Tint
                    | DebtSecurityType::Tprn,
                ) => Method::Treasury,
                _ => Method::Street,
            },
            _ => Method::Street,
        }
    }

    /// Whether a municipal: MSRB's rules (G-15's yield to worst, G-33's
    /// rounding).
    pub fn municipal(&self) -> bool {
        matches!(&self.debt, Debt::Bond(bond) if bond.issuer_type == IssuerType::Municipal as i32)
    }

    /// The calls and puts a yield to call, to put and to worst read.
    pub fn calls(&self) -> Vec<Redemption> {
        match &self.debt {
            Debt::Bond(bond) => windows(&bond.calls),
            Debt::Cd(cd) => windows(&cd.calls),
            _ => Vec::new(),
        }
    }

    pub fn puts(&self) -> Vec<Redemption> {
        match &self.debt {
            Debt::Bond(bond) => windows(&bond.puts),
            _ => Vec::new(),
        }
    }

    /// The payment calendar its terms name; empty where none is stated.
    pub fn calendar_id(&self) -> &str {
        match &self.debt {
            Debt::Bond(b) => &b.payment_calendar_id,
            Debt::Cd(c) => &c.payment_calendar_id,
            Debt::Pool(p) => &p.payment_calendar_id,
            _ => "",
        }
    }

    /// The face a lot is held in is original face (680): a pool's and a
    /// class's current face is it times the factor.
    pub fn factored(&self) -> bool {
        matches!(&self.debt, Debt::Pool(_) | Debt::Tranche(_))
    }

    /// Its annual coupon in percent, where fixed.
    pub fn coupon_percent(&self) -> Option<Dec> {
        match &self.debt {
            Debt::Bond(b) => decimal(b.coupon_rate.as_ref().map(Exact::from_wire)),
            Debt::Cd(c) => decimal(c.coupon_rate.as_ref().map(Exact::from_wire)),
            Debt::Pool(p) => decimal(p.pass_through_rate.as_ref().map(Exact::from_wire)),
            Debt::Tba(t) => decimal(t.pass_through_rate.as_ref().map(Exact::from_wire)),
            Debt::Tranche(t) => decimal(t.coupon_rate.as_ref().map(Exact::from_wire)),
        }
    }
}

/// The day count's value name, as the terms' JSON writes it.
pub fn name_of_day_count(value: i32) -> &'static str {
    meridian_domain::v1::DayCount::try_from(value)
        .ok()
        .filter(|_| value != 0)
        .map(|d| d.as_str_name())
        .unwrap_or("")
}

fn name_of_roll(value: i32) -> &'static str {
    meridian_domain::v1::BusinessDayConvention::try_from(value)
        .map(|r| r.as_str_name())
        .unwrap_or("")
}
