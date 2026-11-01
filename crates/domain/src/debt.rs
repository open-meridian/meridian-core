//! A debt record's terms (contract v29; sdk-contract/fixed-income-contract;
//! rulings 676 to 682; W1.1, W1.24): a `bond`'s, an `mbs_pool`'s, a `tba`'s,
//! a `cmo_tranche`'s and a `certificate_of_deposit`'s, each a message of its
//! own on the record, after FIX's instrument block.
//!
//! Three things are read and written here, so every component says them
//! alike:
//!
//! - **The JSON** the platform's record answers them in and a tool answers
//!   them in: each field by its proto name, each enum by its value's name
//!   (`DEBT_SECURITY_TYPE_CORP`; empty for none), a `Decimal` as text, so
//!   nothing passes through a float, a repeated group as an array. A name
//!   the schema does not define, or text that is no decimal, is refused
//!   naming its path.
//! - **What a record lacks** against its template (574): a `mint` or
//!   `complete` path the terms leave unfilled, read with the conditions the
//!   dictionary states -- a first coupon date only where the coupon is not
//!   zero, a floater's terms only on a floater, TIPS' index only on TIPS, a
//!   STRIPS component's whole bond only on `TINT` and `TPRN` -- and a day
//!   count naming no variant counted as not stated (678). A `calls` or
//!   `sinking_funds` path is never derived missing from the terms alone:
//!   whether a bond is callable is its source's to say, so the platform's
//!   record names it (the plan's risk 3).
//! - **The CDM line** (508; CDM 7.8.0): the issuer type's
//!   `CollateralEntityTypeEnum` value, as the umbrella task's comparison
//!   states it. Every other term holds more than CDM's `Security`.

use meridian_pb::v1::Decimal;
use serde_json::{json, Map, Value};

use crate::exact::Exact;
use crate::v1::Bond;
use crate::v1::{
    BusinessDayConvention, CertificateOfDeposit, CmoTranche, CouponStep, CouponType, DayCount,
    DebtSecurityType, FederalTaxStatus, FloatingRateTerms, InstrumentType, InterestType,
    IssuerType, MbsPool, MortgageAgency, MunicipalTerms, PrincipalType, RecordDateRule,
    RedemptionKind, RedemptionStyle, RedemptionWindow, RefundingStatus, Seniority,
    SifmaSettlementClass, SinkingFundPayment, Tba,
};

/// The five debt types (W1.1), each under debt.
pub const DEBT_TYPES: [InstrumentType; 5] = [
    InstrumentType::Bond,
    InstrumentType::MbsPool,
    InstrumentType::Tba,
    InstrumentType::CmoTranche,
    InstrumentType::CertificateOfDeposit,
];

/// Whether a type is one of the five debt types.
pub fn is_debt(kind: InstrumentType) -> bool {
    DEBT_TYPES.contains(&kind)
}

/// The record's field each type's terms sit in (`InstrumentRecord.bond`),
/// which is also the JSON's key and the prefix a template path may carry.
pub fn terms_field(kind: InstrumentType) -> Option<&'static str> {
    Some(match kind {
        InstrumentType::Bond => "bond",
        InstrumentType::MbsPool => "mbs_pool",
        InstrumentType::Tba => "tba",
        InstrumentType::CmoTranche => "cmo_tranche",
        InstrumentType::CertificateOfDeposit => "certificate_of_deposit",
        _ => return None,
    })
}

// ── Reading the JSON ───────────────────────────────────────────────────────

/// One object of the JSON, read field by field, each refusal naming its
/// path. A field the object lacks, or `null`, is the field's default.
struct Read<'a> {
    object: &'a Map<String, Value>,
    at: String,
}

impl<'a> Read<'a> {
    fn new(value: &'a Value, at: &str) -> Result<Self, String> {
        value
            .as_object()
            .map(|object| Read {
                object,
                at: at.to_string(),
            })
            .ok_or_else(|| format!("{at} is not an object"))
    }

    fn path(&self, key: &str) -> String {
        format!("{}.{key}", self.at)
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.object.get(key).filter(|value| !value.is_null())
    }

    fn text(&self, key: &str) -> Result<String, String> {
        match self.get(key) {
            None => Ok(String::new()),
            Some(Value::String(text)) => Ok(text.trim().to_string()),
            Some(other) => Err(format!("{} is {other}, not text", self.path(key))),
        }
    }

    fn decimal(&self, key: &str) -> Result<Option<Decimal>, String> {
        let text = match self.get(key) {
            None => return Ok(None),
            Some(Value::String(text)) => text.trim().to_string(),
            Some(Value::Number(number)) => number.to_string(),
            Some(other) => return Err(format!("{} is {other}, not a decimal", self.path(key))),
        };
        if text.is_empty() {
            return Ok(None);
        }
        text.parse::<Exact>()
            .map(|exact| Some(exact.to_wire()))
            .map_err(|_| format!("{} is {text:?}, which is no decimal", self.path(key)))
    }

    fn number(&self, key: &str) -> Result<u32, String> {
        match self.get(key) {
            None => Ok(0),
            Some(Value::Number(number)) => number
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| format!("{} is {number}, not a whole number", self.path(key))),
            Some(Value::String(text)) if text.trim().is_empty() => Ok(0),
            Some(Value::String(text)) => text
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("{} is {text:?}, not a whole number", self.path(key))),
            Some(other) => Err(format!("{} is {other}, not a whole number", self.path(key))),
        }
    }

    fn flag(&self, key: &str) -> Result<bool, String> {
        match self.get(key) {
            None => Ok(false),
            Some(Value::Bool(flag)) => Ok(*flag),
            Some(other) => Err(format!("{} is {other}, not true or false", self.path(key))),
        }
    }

    /// An enum by its value's name; empty, or its `_UNSPECIFIED` name, is
    /// unspecified. A name the schema does not define is refused, never read
    /// as unspecified.
    fn named(&self, key: &str, by_name: impl Fn(&str) -> Option<i32>) -> Result<i32, String> {
        let name = self.text(key)?;
        named_value(&name, &self.path(key), by_name)
    }

    fn names(&self, key: &str, by_name: impl Fn(&str) -> Option<i32>) -> Result<Vec<i32>, String> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(n, item)| match item {
                    Value::String(name) => {
                        named_value(name.trim(), &format!("{}[{n}]", self.path(key)), &by_name)
                    }
                    other => Err(format!("{}[{n}] is {other}, not a name", self.path(key))),
                })
                .collect(),
            Some(other) => Err(format!("{} is {other}, not a list", self.path(key))),
        }
    }

    fn message<T>(
        &self,
        key: &str,
        read: impl Fn(&Value, &str) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        self.get(key)
            .map(|value| read(value, &self.path(key)))
            .transpose()
    }

    fn messages<T>(
        &self,
        key: &str,
        read: impl Fn(&Value, &str) -> Result<T, String>,
    ) -> Result<Vec<T>, String> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(n, item)| read(item, &format!("{}[{n}]", self.path(key))))
                .collect(),
            Some(other) => Err(format!("{} is {other}, not a list", self.path(key))),
        }
    }
}

fn named_value(name: &str, at: &str, by_name: impl Fn(&str) -> Option<i32>) -> Result<i32, String> {
    if name.is_empty() || name.ends_with("_UNSPECIFIED") && by_name(name) == Some(0) {
        return Ok(0);
    }
    by_name(name).ok_or_else(|| format!("{at} is {name:?}, which the schema does not define"))
}

macro_rules! by_name {
    ($enum:ty) => {
        |name: &str| <$enum>::from_str_name(name).map(|value| value as i32)
    };
}

fn floating_from(value: &Value, at: &str) -> Result<FloatingRateTerms, String> {
    let read = Read::new(value, at)?;
    Ok(FloatingRateTerms {
        index_instrument_id: read.text("index_instrument_id")?,
        spread_bps: read.decimal("spread_bps")?,
        multiplier: read.decimal("multiplier")?,
        cap_rate: read.decimal("cap_rate")?,
        floor_rate: read.decimal("floor_rate")?,
        reset_frequency: read.number("reset_frequency")?,
        lookback_days: read.number("lookback_days")?,
        in_arrears: read.flag("in_arrears")?,
    })
}

fn step_from(value: &Value, at: &str) -> Result<CouponStep, String> {
    let read = Read::new(value, at)?;
    Ok(CouponStep {
        from_date: read.text("from_date")?,
        coupon_rate: read.decimal("coupon_rate")?,
    })
}

fn window_from(value: &Value, at: &str) -> Result<RedemptionWindow, String> {
    let read = Read::new(value, at)?;
    Ok(RedemptionWindow {
        kind: read.named("kind", by_name!(RedemptionKind))?,
        from_date: read.text("from_date")?,
        to_date: read.text("to_date")?,
        price: read.decimal("price")?,
        make_whole_spread_bps: read.decimal("make_whole_spread_bps")?,
        make_whole_benchmark_instrument_id: read.text("make_whole_benchmark_instrument_id")?,
        notice_days_min: read.number("notice_days_min")?,
        notice_days_max: read.number("notice_days_max")?,
        style: read.named("style", by_name!(RedemptionStyle))?,
    })
}

fn sinking_fund_from(value: &Value, at: &str) -> Result<SinkingFundPayment, String> {
    let read = Read::new(value, at)?;
    Ok(SinkingFundPayment {
        date: read.text("date")?,
        face_amount: read.decimal("face_amount")?,
        price: read.decimal("price")?,
    })
}

fn municipal_from(value: &Value, at: &str) -> Result<MunicipalTerms, String> {
    let read = Read::new(value, at)?;
    Ok(MunicipalTerms {
        federal_tax_status: read.named("federal_tax_status", by_name!(FederalTaxStatus))?,
        subject_to_amt: read.flag("subject_to_amt")?,
        state_tax_exempt: read.flag("state_tax_exempt")?,
        bank_qualified: read.flag("bank_qualified")?,
        obligor_company_id: read.text("obligor_company_id")?,
        insurer_company_id: read.text("insurer_company_id")?,
        refunding_status: read.named("refunding_status", by_name!(RefundingStatus))?,
        refunded_to_date: read.text("refunded_to_date")?,
    })
}

/// A bond's terms from the JSON at `at`.
pub fn bond_from(value: &Value, at: &str) -> Result<Bond, String> {
    let read = Read::new(value, at)?;
    Ok(Bond {
        security_type: read.named("security_type", by_name!(DebtSecurityType))?,
        issuer_company_id: read.text("issuer_company_id")?,
        issuer_type: read.named("issuer_type", by_name!(IssuerType))?,
        guarantor_company_id: read.text("guarantor_company_id")?,
        governing_law: read.text("governing_law")?,
        country_of_issue: read.text("country_of_issue")?,
        state_or_province_of_issue: read.text("state_or_province_of_issue")?,
        issue_date: read.text("issue_date")?,
        dated_date: read.text("dated_date")?,
        first_coupon_date: read.text("first_coupon_date")?,
        maturity_date: read.text("maturity_date")?,
        coupon_rate: read.decimal("coupon_rate")?,
        coupon_type: read.named("coupon_type", by_name!(CouponType))?,
        coupon_frequency: read.number("coupon_frequency")?,
        day_count: read.named("day_count", by_name!(DayCount))?,
        business_day_convention: read
            .named("business_day_convention", by_name!(BusinessDayConvention))?,
        payment_calendar_id: read.text("payment_calendar_id")?,
        end_of_month: read.flag("end_of_month")?,
        record_date_rule: read.named("record_date_rule", by_name!(RecordDateRule))?,
        record_date_days: read.number("record_date_days")?,
        minimum_denomination: read.decimal("minimum_denomination")?,
        denomination_increment: read.decimal("denomination_increment")?,
        original_issue_amount: read.decimal("original_issue_amount")?,
        seniority: read.named("seniority", by_name!(Seniority))?,
        rule_144a: read.flag("rule_144a")?,
        reg_s: read.flag("reg_s")?,
        floating: read.message("floating", floating_from)?,
        inflation_index_instrument_id: read.text("inflation_index_instrument_id")?,
        base_index_level: read.decimal("base_index_level")?,
        coupon_steps: read.messages("coupon_steps", step_from)?,
        calls: read.messages("calls", window_from)?,
        puts: read.messages("puts", window_from)?,
        sinking_funds: read.messages("sinking_funds", sinking_fund_from)?,
        municipal: read.message("municipal", municipal_from)?,
        stripped_from: read.text("stripped_from")?,
    })
}

/// An agency pool's terms from the JSON at `at`.
pub fn mbs_pool_from(value: &Value, at: &str) -> Result<MbsPool, String> {
    let read = Read::new(value, at)?;
    Ok(MbsPool {
        agency: read.named("agency", by_name!(MortgageAgency))?,
        program: read.text("program")?,
        pool_number: read.text("pool_number")?,
        pass_through_rate: read.decimal("pass_through_rate")?,
        issue_date: read.text("issue_date")?,
        maturity_date: read.text("maturity_date")?,
        original_face: read.decimal("original_face")?,
        original_term_months: read.number("original_term_months")?,
        day_count: read.named("day_count", by_name!(DayCount))?,
        payment_delay_days: read.number("payment_delay_days")?,
        payment_day: read.number("payment_day")?,
        record_date_rule: read.named("record_date_rule", by_name!(RecordDateRule))?,
        payment_calendar_id: read.text("payment_calendar_id")?,
    })
}

/// A TBA's terms from the JSON at `at`.
pub fn tba_from(value: &Value, at: &str) -> Result<Tba, String> {
    let read = Read::new(value, at)?;
    Ok(Tba {
        agency: read.named("agency", by_name!(MortgageAgency))?,
        program: read.text("program")?,
        pass_through_rate: read.decimal("pass_through_rate")?,
        original_term_months: read.number("original_term_months")?,
        settlement_month: read.text("settlement_month")?,
        sifma_class: read.named("sifma_class", by_name!(SifmaSettlementClass))?,
        settlement_date: read.text("settlement_date")?,
    })
}

/// An agency REMIC class's terms from the JSON at `at`.
pub fn cmo_tranche_from(value: &Value, at: &str) -> Result<CmoTranche, String> {
    let read = Read::new(value, at)?;
    Ok(CmoTranche {
        agency: read.named("agency", by_name!(MortgageAgency))?,
        deal: read.text("deal")?,
        class_name: read.text("class_name")?,
        principal_types: read.names("principal_types", by_name!(PrincipalType))?,
        interest_types: read.names("interest_types", by_name!(InterestType))?,
        original_face: read.decimal("original_face")?,
        coupon_rate: read.decimal("coupon_rate")?,
        floating: read.message("floating", floating_from)?,
        issue_date: read.text("issue_date")?,
        maturity_date: read.text("maturity_date")?,
        day_count: read.named("day_count", by_name!(DayCount))?,
        payment_delay_days: read.number("payment_delay_days")?,
        payment_day: read.number("payment_day")?,
        record_date_rule: read.named("record_date_rule", by_name!(RecordDateRule))?,
        record_date_days: read.number("record_date_days")?,
    })
}

/// A brokered CD's terms from the JSON at `at`.
pub fn certificate_of_deposit_from(
    value: &Value,
    at: &str,
) -> Result<CertificateOfDeposit, String> {
    let read = Read::new(value, at)?;
    Ok(CertificateOfDeposit {
        issuer_company_id: read.text("issuer_company_id")?,
        issue_date: read.text("issue_date")?,
        maturity_date: read.text("maturity_date")?,
        coupon_rate: read.decimal("coupon_rate")?,
        coupon_frequency: read.number("coupon_frequency")?,
        day_count: read.named("day_count", by_name!(DayCount))?,
        business_day_convention: read
            .named("business_day_convention", by_name!(BusinessDayConvention))?,
        payment_calendar_id: read.text("payment_calendar_id")?,
        minimum_denomination: read.decimal("minimum_denomination")?,
        deposit_insured: read.flag("deposit_insured")?,
        calls: read.messages("calls", window_from)?,
        survivor_option: read.flag("survivor_option")?,
    })
}

// ── Writing the JSON ───────────────────────────────────────────────────────

fn decimal_json(decimal: Option<&Decimal>) -> Value {
    decimal
        .and_then(|decimal| Exact::from_wire(decimal).ok())
        .map(|exact| Value::from(exact.to_string()))
        .unwrap_or(Value::Null)
}

macro_rules! name_of {
    ($enum:ty, $value:expr) => {
        match <$enum>::try_from($value) {
            Ok(known) if $value != 0 => Value::from(known.as_str_name()),
            _ => Value::from(""),
        }
    };
}

fn floating_json(terms: &FloatingRateTerms) -> Value {
    json!({
        "index_instrument_id": terms.index_instrument_id,
        "spread_bps": decimal_json(terms.spread_bps.as_ref()),
        "multiplier": decimal_json(terms.multiplier.as_ref()),
        "cap_rate": decimal_json(terms.cap_rate.as_ref()),
        "floor_rate": decimal_json(terms.floor_rate.as_ref()),
        "reset_frequency": terms.reset_frequency,
        "lookback_days": terms.lookback_days,
        "in_arrears": terms.in_arrears,
    })
}

fn window_json(window: &RedemptionWindow) -> Value {
    json!({
        "kind": name_of!(RedemptionKind, window.kind),
        "from_date": window.from_date,
        "to_date": window.to_date,
        "price": decimal_json(window.price.as_ref()),
        "make_whole_spread_bps": decimal_json(window.make_whole_spread_bps.as_ref()),
        "make_whole_benchmark_instrument_id": window.make_whole_benchmark_instrument_id,
        "notice_days_min": window.notice_days_min,
        "notice_days_max": window.notice_days_max,
        "style": name_of!(RedemptionStyle, window.style),
    })
}

fn municipal_json(muni: &MunicipalTerms) -> Value {
    json!({
        "federal_tax_status": name_of!(FederalTaxStatus, muni.federal_tax_status),
        "subject_to_amt": muni.subject_to_amt,
        "state_tax_exempt": muni.state_tax_exempt,
        "bank_qualified": muni.bank_qualified,
        "obligor_company_id": muni.obligor_company_id,
        "insurer_company_id": muni.insurer_company_id,
        "refunding_status": name_of!(RefundingStatus, muni.refunding_status),
        "refunded_to_date": muni.refunded_to_date,
    })
}

/// A bond's terms as the JSON carries them: its identity and dates, then
/// its coupon and conventions, then its schedules and groups.
pub fn bond_json(bond: &Bond) -> Value {
    let mut out = Map::new();
    let mut put = |part: Value| {
        if let Value::Object(fields) = part {
            out.extend(fields);
        }
    };
    put(json!({
        "security_type": name_of!(DebtSecurityType, bond.security_type),
        "issuer_company_id": bond.issuer_company_id,
        "issuer_type": name_of!(IssuerType, bond.issuer_type),
        "guarantor_company_id": bond.guarantor_company_id,
        "governing_law": bond.governing_law,
        "country_of_issue": bond.country_of_issue,
        "state_or_province_of_issue": bond.state_or_province_of_issue,
        "issue_date": bond.issue_date,
        "dated_date": bond.dated_date,
        "first_coupon_date": bond.first_coupon_date,
        "maturity_date": bond.maturity_date,
    }));
    put(json!({
        "coupon_rate": decimal_json(bond.coupon_rate.as_ref()),
        "coupon_type": name_of!(CouponType, bond.coupon_type),
        "coupon_frequency": bond.coupon_frequency,
        "day_count": name_of!(DayCount, bond.day_count),
        "business_day_convention": name_of!(BusinessDayConvention, bond.business_day_convention),
        "payment_calendar_id": bond.payment_calendar_id,
        "end_of_month": bond.end_of_month,
        "record_date_rule": name_of!(RecordDateRule, bond.record_date_rule),
        "record_date_days": bond.record_date_days,
        "minimum_denomination": decimal_json(bond.minimum_denomination.as_ref()),
        "denomination_increment": decimal_json(bond.denomination_increment.as_ref()),
        "original_issue_amount": decimal_json(bond.original_issue_amount.as_ref()),
        "seniority": name_of!(Seniority, bond.seniority),
        "rule_144a": bond.rule_144a,
        "reg_s": bond.reg_s,
    }));
    put(json!({
        "floating": bond.floating.as_ref().map(floating_json),
        "inflation_index_instrument_id": bond.inflation_index_instrument_id,
        "base_index_level": decimal_json(bond.base_index_level.as_ref()),
        "coupon_steps": bond.coupon_steps.iter().map(|step| json!({
            "from_date": step.from_date,
            "coupon_rate": decimal_json(step.coupon_rate.as_ref()),
        })).collect::<Vec<_>>(),
        "calls": bond.calls.iter().map(window_json).collect::<Vec<_>>(),
        "puts": bond.puts.iter().map(window_json).collect::<Vec<_>>(),
        "sinking_funds": bond.sinking_funds.iter().map(|fund| json!({
            "date": fund.date,
            "face_amount": decimal_json(fund.face_amount.as_ref()),
            "price": decimal_json(fund.price.as_ref()),
        })).collect::<Vec<_>>(),
        "municipal": bond.municipal.as_ref().map(municipal_json),
        "stripped_from": bond.stripped_from,
    }));
    Value::Object(out)
}

/// An agency pool's terms as the JSON carries them.
pub fn mbs_pool_json(pool: &MbsPool) -> Value {
    json!({
        "agency": name_of!(MortgageAgency, pool.agency),
        "program": pool.program,
        "pool_number": pool.pool_number,
        "pass_through_rate": decimal_json(pool.pass_through_rate.as_ref()),
        "issue_date": pool.issue_date,
        "maturity_date": pool.maturity_date,
        "original_face": decimal_json(pool.original_face.as_ref()),
        "original_term_months": pool.original_term_months,
        "day_count": name_of!(DayCount, pool.day_count),
        "payment_delay_days": pool.payment_delay_days,
        "payment_day": pool.payment_day,
        "record_date_rule": name_of!(RecordDateRule, pool.record_date_rule),
        "payment_calendar_id": pool.payment_calendar_id,
    })
}

/// A TBA's terms as the JSON carries them.
pub fn tba_json(tba: &Tba) -> Value {
    json!({
        "agency": name_of!(MortgageAgency, tba.agency),
        "program": tba.program,
        "pass_through_rate": decimal_json(tba.pass_through_rate.as_ref()),
        "original_term_months": tba.original_term_months,
        "settlement_month": tba.settlement_month,
        "sifma_class": name_of!(SifmaSettlementClass, tba.sifma_class),
        "settlement_date": tba.settlement_date,
    })
}

/// A REMIC class's terms as the JSON carries them.
pub fn cmo_tranche_json(tranche: &CmoTranche) -> Value {
    json!({
        "agency": name_of!(MortgageAgency, tranche.agency),
        "deal": tranche.deal,
        "class_name": tranche.class_name,
        "principal_types": tranche.principal_types.iter()
            .map(|t| name_of!(PrincipalType, *t)).collect::<Vec<_>>(),
        "interest_types": tranche.interest_types.iter()
            .map(|t| name_of!(InterestType, *t)).collect::<Vec<_>>(),
        "original_face": decimal_json(tranche.original_face.as_ref()),
        "coupon_rate": decimal_json(tranche.coupon_rate.as_ref()),
        "floating": tranche.floating.as_ref().map(floating_json),
        "issue_date": tranche.issue_date,
        "maturity_date": tranche.maturity_date,
        "day_count": name_of!(DayCount, tranche.day_count),
        "payment_delay_days": tranche.payment_delay_days,
        "payment_day": tranche.payment_day,
        "record_date_rule": name_of!(RecordDateRule, tranche.record_date_rule),
        "record_date_days": tranche.record_date_days,
    })
}

/// A CD's terms as the JSON carries them.
pub fn certificate_of_deposit_json(cd: &CertificateOfDeposit) -> Value {
    json!({
        "issuer_company_id": cd.issuer_company_id,
        "issue_date": cd.issue_date,
        "maturity_date": cd.maturity_date,
        "coupon_rate": decimal_json(cd.coupon_rate.as_ref()),
        "coupon_frequency": cd.coupon_frequency,
        "day_count": name_of!(DayCount, cd.day_count),
        "business_day_convention": name_of!(BusinessDayConvention, cd.business_day_convention),
        "payment_calendar_id": cd.payment_calendar_id,
        "minimum_denomination": decimal_json(cd.minimum_denomination.as_ref()),
        "deposit_insured": cd.deposit_insured,
        "calls": cd.calls.iter().map(window_json).collect::<Vec<_>>(),
        "survivor_option": cd.survivor_option,
    })
}

// ── What a record lacks (574) ──────────────────────────────────────────────

/// A path as the terms read it: a template's path with the record's field
/// for the type in front (`bond.coupon_rate`) or without it, and a repeated
/// group's or a message's own fields (`calls[0].price`,
/// `floating.index_instrument_id`) read as the group's.
fn bare(kind: InstrumentType, path: &str) -> &str {
    let path = terms_field(kind)
        .and_then(|field| path.strip_prefix(field))
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(path);
    let end = path.find(['.', '[']).unwrap_or(path.len());
    &path[..end]
}

fn needs_days(rule: i32) -> bool {
    matches!(
        RecordDateRule::try_from(rule).unwrap_or(RecordDateRule::Unspecified),
        RecordDateRule::CalendarDaysBeforePayment | RecordDateRule::BusinessDaysBeforePayment
    )
}

fn no_variant(day_count: i32) -> bool {
    !crate::conventions::names_a_variant(
        DayCount::try_from(day_count).unwrap_or(DayCount::Unspecified),
    )
}

/// Whether a bond's terms leave `path` unfilled, as its dictionary entry
/// states it: false for a path the terms do not carry, for a flag (false is
/// a value), and for `calls` and `sinking_funds` (the source's to say).
pub fn bond_path_missing(bond: &Bond, path: &str) -> bool {
    let coupon = CouponType::try_from(bond.coupon_type).unwrap_or(CouponType::Unspecified);
    let security =
        DebtSecurityType::try_from(bond.security_type).unwrap_or(DebtSecurityType::Unspecified);
    let zero = coupon == CouponType::Zero;
    let tips = security == DebtSecurityType::Tips;
    match bare(InstrumentType::Bond, path) {
        "security_type" => security == DebtSecurityType::Unspecified,
        "issuer_company_id" => bond.issuer_company_id.is_empty(),
        "issuer_type" => bond.issuer_type == 0,
        "guarantor_company_id" => bond.guarantor_company_id.is_empty(),
        "governing_law" => bond.governing_law.is_empty(),
        "country_of_issue" => bond.country_of_issue.is_empty(),
        "state_or_province_of_issue" => bond.state_or_province_of_issue.is_empty(),
        "issue_date" => bond.issue_date.is_empty(),
        "dated_date" => bond.dated_date.is_empty(),
        "first_coupon_date" => !zero && bond.first_coupon_date.is_empty(),
        "maturity_date" => bond.maturity_date.is_empty(),
        // A when-issued Treasury's coupon is empty until its auction; a
        // floater's rate is its floating terms'; a zero coupon's is 0.
        "coupon_rate" => {
            !matches!(coupon, CouponType::Zero | CouponType::Floating) && bond.coupon_rate.is_none()
        }
        "coupon_type" => coupon == CouponType::Unspecified,
        "coupon_frequency" => !zero && bond.coupon_frequency == 0,
        "day_count" => no_variant(bond.day_count),
        "business_day_convention" => bond.business_day_convention == 0,
        "payment_calendar_id" => bond.payment_calendar_id.is_empty(),
        "record_date_rule" => !zero && bond.record_date_rule == 0,
        "record_date_days" => needs_days(bond.record_date_rule) && bond.record_date_days == 0,
        "minimum_denomination" => bond.minimum_denomination.is_none(),
        "denomination_increment" => bond.denomination_increment.is_none(),
        "original_issue_amount" => bond.original_issue_amount.is_none(),
        "seniority" => bond.seniority == 0,
        "floating" => coupon == CouponType::Floating && bond.floating.is_none(),
        "inflation_index_instrument_id" => tips && bond.inflation_index_instrument_id.is_empty(),
        "base_index_level" => tips && bond.base_index_level.is_none(),
        "coupon_steps" => coupon == CouponType::Step && bond.coupon_steps.is_empty(),
        "municipal" => {
            matches!(security, DebtSecurityType::Go | DebtSecurityType::Rev)
                && bond.municipal.is_none()
        }
        "stripped_from" => {
            matches!(security, DebtSecurityType::Tint | DebtSecurityType::Tprn)
                && bond.stripped_from.is_empty()
        }
        _ => false,
    }
}

/// Whether an agency pool's terms leave `path` unfilled.
pub fn mbs_pool_path_missing(pool: &MbsPool, path: &str) -> bool {
    match bare(InstrumentType::MbsPool, path) {
        "agency" => pool.agency == 0,
        "program" => pool.program.is_empty(),
        "pool_number" => pool.pool_number.is_empty(),
        "pass_through_rate" => pool.pass_through_rate.is_none(),
        "issue_date" => pool.issue_date.is_empty(),
        "maturity_date" => pool.maturity_date.is_empty(),
        "original_face" => pool.original_face.is_none(),
        "original_term_months" => pool.original_term_months == 0,
        "day_count" => no_variant(pool.day_count),
        "payment_delay_days" => pool.payment_delay_days == 0,
        "payment_day" => pool.payment_day == 0,
        "record_date_rule" => pool.record_date_rule == 0,
        "payment_calendar_id" => pool.payment_calendar_id.is_empty(),
        _ => false,
    }
}

/// Whether a TBA's terms leave `path` unfilled.
pub fn tba_path_missing(tba: &Tba, path: &str) -> bool {
    match bare(InstrumentType::Tba, path) {
        "agency" => tba.agency == 0,
        "program" => tba.program.is_empty(),
        "pass_through_rate" => tba.pass_through_rate.is_none(),
        "original_term_months" => tba.original_term_months == 0,
        "settlement_month" => tba.settlement_month.is_empty(),
        "sifma_class" => tba.sifma_class == 0,
        "settlement_date" => tba.settlement_date.is_empty(),
        _ => false,
    }
}

/// Whether a REMIC class's terms leave `path` unfilled: a coupon rate only
/// on a class that is no floater (a PO's is 0), its floating terms only on a
/// floater or an inverse floater; no delay is a value (a floater's often).
pub fn cmo_tranche_path_missing(tranche: &CmoTranche, path: &str) -> bool {
    let floats = tranche.interest_types.iter().any(|t| {
        matches!(
            InterestType::try_from(*t),
            Ok(InterestType::Floater | InterestType::InverseFloater)
        )
    });
    let principal_only = tranche
        .interest_types
        .iter()
        .any(|t| InterestType::try_from(*t) == Ok(InterestType::Po));
    match bare(InstrumentType::CmoTranche, path) {
        "agency" => tranche.agency == 0,
        "deal" => tranche.deal.is_empty(),
        "class_name" => tranche.class_name.is_empty(),
        "principal_types" => tranche.principal_types.is_empty(),
        "interest_types" => tranche.interest_types.is_empty(),
        "original_face" => tranche.original_face.is_none(),
        "coupon_rate" => !floats && !principal_only && tranche.coupon_rate.is_none(),
        "floating" => floats && tranche.floating.is_none(),
        "issue_date" => tranche.issue_date.is_empty(),
        "maturity_date" => tranche.maturity_date.is_empty(),
        "day_count" => no_variant(tranche.day_count),
        "payment_day" => tranche.payment_day == 0,
        "record_date_rule" => tranche.record_date_rule == 0,
        "record_date_days" => needs_days(tranche.record_date_rule) && tranche.record_date_days == 0,
        _ => false,
    }
}

/// Whether a CD's terms leave `path` unfilled: a CD paying at maturity has
/// no coupon frequency, and no flag is ever missing.
pub fn certificate_of_deposit_path_missing(cd: &CertificateOfDeposit, path: &str) -> bool {
    match bare(InstrumentType::CertificateOfDeposit, path) {
        "issuer_company_id" => cd.issuer_company_id.is_empty(),
        "issue_date" => cd.issue_date.is_empty(),
        "maturity_date" => cd.maturity_date.is_empty(),
        "coupon_rate" => cd.coupon_rate.is_none(),
        "day_count" => no_variant(cd.day_count),
        "business_day_convention" => cd.business_day_convention == 0,
        "payment_calendar_id" => cd.payment_calendar_id.is_empty(),
        "minimum_denomination" => cd.minimum_denomination.is_none(),
        _ => false,
    }
}

// ── The CDM line (508) ─────────────────────────────────────────────────────

/// CDM's `CollateralEntityTypeEnum` value for an issuer type (the umbrella
/// task's comparison): municipal and sub-sovereign both RegionalGovernment,
/// held apart here on purpose (MSRB's and the tax status's reach).
pub fn cdm_issuer_type(issuer_type: IssuerType) -> Option<&'static str> {
    Some(match issuer_type {
        IssuerType::Unspecified => return None,
        IssuerType::Sovereign => "SovereignCentralBank",
        IssuerType::Agency => "QuasiGovernment",
        IssuerType::Supranational => "SupraNational",
        IssuerType::SubSovereign | IssuerType::Municipal => "RegionalGovernment",
        IssuerType::Corporate => "Corporate",
    })
}

/// An enum value's name as a person reads it: its prefix dropped, in lower
/// case, words apart (`ISSUER_TYPE_SUB_SOVEREIGN` is "sub sovereign"); a
/// FIX code (`DEBT_SECURITY_TYPE_TBILL`) keeps its capitals.
pub fn value_words(name: &str, prefix: &str, keep_case: bool) -> String {
    let bare = name.strip_prefix(prefix).unwrap_or(name);
    if bare.is_empty() || bare == "UNSPECIFIED" {
        return String::new();
    }
    if keep_case {
        bare.to_string()
    } else {
        bare.to_ascii_lowercase().replace('_', " ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(text: &str) -> Option<Decimal> {
        Some(text.parse::<Exact>().unwrap().to_wire())
    }

    /// The fixture's corporate (fixtures/reference/define-instrument.yaml,
    /// "v29, a corporate bond"): a make-whole window then a par window.
    fn corporate() -> Value {
        json!({
            "security_type": "DEBT_SECURITY_TYPE_CORP",
            "issuer_company_id": "ENT-01JAXP0000000000000000001",
            "issuer_type": "ISSUER_TYPE_CORPORATE",
            "country_of_issue": "US",
            "issue_date": "2026-02-05",
            "dated_date": "2026-02-05",
            "first_coupon_date": "2026-08-05",
            "maturity_date": "2036-02-05",
            "coupon_rate": "4.875",
            "coupon_type": "COUPON_TYPE_FIXED",
            "coupon_frequency": 2,
            "day_count": "DAY_COUNT_THIRTY_360_BOND_BASIS",
            "business_day_convention": "BUSINESS_DAY_CONVENTION_FOLLOWING",
            "payment_calendar_id": "CAL-US-GOVERNMENT-SECURITIES",
            "record_date_rule": "RECORD_DATE_RULE_CALENDAR_DAYS_BEFORE_PAYMENT",
            "record_date_days": 15,
            "minimum_denomination": "2000",
            "denomination_increment": "1000",
            "seniority": "SENIORITY_SENIOR",
            "calls": [
                {"kind": "REDEMPTION_KIND_MAKE_WHOLE", "from_date": "2026-02-05",
                 "to_date": "2035-11-05", "make_whole_spread_bps": "20",
                 "make_whole_benchmark_instrument_id": "INS-H15-CMT",
                 "notice_days_min": 10, "notice_days_max": 60,
                 "style": "REDEMPTION_STYLE_CONTINUOUS"},
                {"kind": "REDEMPTION_KIND_PAR", "from_date": "2035-11-05",
                 "price": "100", "notice_days_min": 10, "notice_days_max": 60,
                 "style": "REDEMPTION_STYLE_CONTINUOUS"}
            ]
        })
    }

    #[test]
    fn a_bonds_terms_are_read_and_written_as_the_platform_answers_them() {
        let bond = bond_from(&corporate(), "bond").unwrap();
        assert_eq!(bond.security_type, DebtSecurityType::Corp as i32);
        assert_eq!(bond.day_count, DayCount::Thirty360BondBasis as i32);
        assert_eq!(bond.coupon_rate, decimal("4.875"));
        assert_eq!(bond.coupon_frequency, 2);
        assert_eq!(bond.calls.len(), 2);
        assert_eq!(bond.calls[0].make_whole_spread_bps, decimal("20"));
        assert_eq!(bond.calls[1].price, decimal("100"));
        assert!(bond.calls[0].price.is_none());
        // Written back and read again: the same terms, nothing through a float.
        let again = bond_from(&bond_json(&bond), "bond").unwrap();
        assert_eq!(again, bond);
        assert_eq!(bond_json(&bond)["coupon_rate"], "4.875");
        assert_eq!(bond_json(&bond)["guarantor_company_id"], "");
        assert_eq!(bond_json(&bond)["seniority"], "SENIORITY_SENIOR");
    }

    #[test]
    fn a_name_the_schema_does_not_define_or_no_decimal_is_refused_naming_its_path() {
        let mut wrong = corporate();
        wrong["day_count"] = json!("DAY_COUNT_THIRTY_365");
        assert_eq!(
            bond_from(&wrong, "bond").unwrap_err(),
            "bond.day_count is \"DAY_COUNT_THIRTY_365\", which the schema does not define"
        );
        let mut wrong = corporate();
        wrong["calls"][1]["price"] = json!("par");
        assert_eq!(
            bond_from(&wrong, "bond").unwrap_err(),
            "bond.calls[1].price is \"par\", which is no decimal"
        );
        let mut wrong = corporate();
        wrong["coupon_frequency"] = json!(-2);
        assert!(bond_from(&wrong, "bond")
            .unwrap_err()
            .starts_with("bond.coupon_frequency"));
        // Unspecified said either way, and null, read as none.
        let mut unset = corporate();
        unset["seniority"] = json!("SENIORITY_UNSPECIFIED");
        unset["coupon_rate"] = Value::Null;
        let read = bond_from(&unset, "bond").unwrap();
        assert_eq!(read.seniority, 0);
        assert!(read.coupon_rate.is_none());
    }

    #[test]
    fn the_other_four_types_round_trip() {
        let pool = mbs_pool_from(
            &json!({"agency": "MORTGAGE_AGENCY_FNMA", "program": "CL", "pool_number": "MA5123",
                    "pass_through_rate": "5.5", "issue_date": "2023-09-01",
                    "maturity_date": "2053-09-01", "original_face": "1250000000",
                    "original_term_months": 360, "day_count": "DAY_COUNT_THIRTY_360_BOND_BASIS",
                    "payment_delay_days": 55, "payment_day": 25,
                    "record_date_rule": "RECORD_DATE_RULE_LAST_DAY_OF_PRIOR_MONTH"}),
            "mbs_pool",
        )
        .unwrap();
        assert_eq!(mbs_pool_from(&mbs_pool_json(&pool), "p").unwrap(), pool);
        assert!(mbs_pool_path_missing(&pool, "payment_calendar_id"));
        assert!(!mbs_pool_path_missing(&pool, "pool_number"));

        let tba = tba_from(
            &json!({"agency": "MORTGAGE_AGENCY_FNMA", "program": "UMBS", "pass_through_rate": "5.5",
                    "original_term_months": 360, "settlement_month": "202612",
                    "sifma_class": "SIFMA_SETTLEMENT_CLASS_A"}),
            "tba",
        )
        .unwrap();
        assert_eq!(tba_from(&tba_json(&tba), "t").unwrap(), tba);
        assert!(tba_path_missing(&tba, "settlement_date"));

        let tranche = cmo_tranche_from(
            &json!({"agency": "MORTGAGE_AGENCY_FHLMC", "deal": "5300", "class_name": "FA",
                    "principal_types": ["PRINCIPAL_TYPE_PAC"],
                    "interest_types": ["INTEREST_TYPE_FLOATER"],
                    "original_face": "40000000", "issue_date": "2024-03-01",
                    "floating": {"index_instrument_id": "INS-SOFR", "spread_bps": "45",
                                 "cap_rate": "6.5", "floor_rate": "0.45"}}),
            "cmo_tranche",
        )
        .unwrap();
        assert_eq!(
            cmo_tranche_from(&cmo_tranche_json(&tranche), "c").unwrap(),
            tranche
        );
        // A floater's rate is its floating terms', and no delay is a value.
        assert!(!cmo_tranche_path_missing(&tranche, "coupon_rate"));
        assert!(!cmo_tranche_path_missing(&tranche, "floating"));
        assert!(!cmo_tranche_path_missing(&tranche, "payment_delay_days"));
        assert!(cmo_tranche_path_missing(&tranche, "maturity_date"));

        let cd = certificate_of_deposit_from(
            &json!({"issuer_company_id": "ENT-BANK", "issue_date": "2026-10-01",
                    "maturity_date": "2027-10-01", "coupon_rate": "4.1",
                    "day_count": "DAY_COUNT_ACT_365_FIXED", "deposit_insured": true}),
            "certificate_of_deposit",
        )
        .unwrap();
        assert_eq!(
            certificate_of_deposit_from(&certificate_of_deposit_json(&cd), "d").unwrap(),
            cd
        );
        assert!(!certificate_of_deposit_path_missing(
            &cd,
            "coupon_frequency"
        ));
        assert!(certificate_of_deposit_path_missing(
            &cd,
            "payment_calendar_id"
        ));
    }

    #[test]
    fn a_bonds_missing_paths_follow_the_dictionarys_conditions() {
        let bond = bond_from(&corporate(), "bond").unwrap();
        for path in [
            "security_type",
            "coupon_rate",
            "bond.coupon_rate",
            "day_count",
            "calls",
            "calls[0].price",
            "floating",
            "stripped_from",
            "end_of_month",
        ] {
            assert!(!bond_path_missing(&bond, path), "{path}");
        }
        // Optional terms the corporate does not state.
        assert!(bond_path_missing(&bond, "governing_law"));

        // A when-issued Treasury: its coupon empty until the auction.
        let mut when_issued = bond.clone();
        when_issued.security_type = DebtSecurityType::Tnote as i32;
        when_issued.coupon_rate = None;
        assert!(bond_path_missing(&when_issued, "coupon_rate"));

        // A STRIPS principal component, zero coupon, its whole bond missing.
        let strip = Bond {
            security_type: DebtSecurityType::Tprn as i32,
            coupon_type: CouponType::Zero as i32,
            ..Default::default()
        };
        assert!(bond_path_missing(&strip, "stripped_from"));
        for path in [
            "first_coupon_date",
            "coupon_rate",
            "coupon_frequency",
            "record_date_rule",
        ] {
            assert!(!bond_path_missing(&strip, path), "a zero coupon: {path}");
        }

        // A day count naming no variant is not stated (678).
        let mut as_stated = bond.clone();
        as_stated.day_count = DayCount::Thirty360 as i32;
        assert!(bond_path_missing(&as_stated, "day_count"));

        // TIPS, a floater, a step coupon and a municipal each need theirs.
        let tips = Bond {
            security_type: DebtSecurityType::Tips as i32,
            ..bond.clone()
        };
        assert!(bond_path_missing(&tips, "inflation_index_instrument_id"));
        assert!(bond_path_missing(&tips, "base_index_level"));
        let frn = Bond {
            coupon_type: CouponType::Floating as i32,
            ..bond.clone()
        };
        assert!(bond_path_missing(&frn, "floating"));
        assert!(!bond_path_missing(&frn, "coupon_rate"));
        let step = Bond {
            coupon_type: CouponType::Step as i32,
            ..bond.clone()
        };
        assert!(bond_path_missing(&step, "coupon_steps"));
        let muni = Bond {
            security_type: DebtSecurityType::Go as i32,
            ..bond
        };
        assert!(bond_path_missing(&muni, "municipal"));
    }

    #[test]
    fn the_issuer_types_cdm_line_is_the_umbrellas() {
        assert_eq!(
            cdm_issuer_type(IssuerType::Sovereign),
            Some("SovereignCentralBank")
        );
        assert_eq!(cdm_issuer_type(IssuerType::Agency), Some("QuasiGovernment"));
        assert_eq!(
            cdm_issuer_type(IssuerType::Supranational),
            Some("SupraNational")
        );
        assert_eq!(
            cdm_issuer_type(IssuerType::SubSovereign),
            cdm_issuer_type(IssuerType::Municipal)
        );
        assert_eq!(cdm_issuer_type(IssuerType::Corporate), Some("Corporate"));
        assert_eq!(
            value_words("ISSUER_TYPE_SUB_SOVEREIGN", "ISSUER_TYPE_", false),
            "sub sovereign"
        );
        assert_eq!(
            value_words("DEBT_SECURITY_TYPE_TBILL", "DEBT_SECURITY_TYPE_", true),
            "TBILL"
        );
    }
}
