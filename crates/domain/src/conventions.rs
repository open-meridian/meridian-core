//! Amounts' conventions as the runtime reads them (contract v29; ruling 742,
//! with 678 and 685): the day count, a rate's convention, the price basis
//! and a quoted yield, which amounts.proto declares beside `UnitValue` since
//! the instrument store's terms, the book's orders and executions and the
//! lake's rows all use them.
//!
//! What each value is called after FIX and FINOS CDM (508; CDM 7.8.0), as a
//! person reads it, and what the contract refuses: a day count naming no
//! variant on an instrument's terms (678), and a quoted yield with no value,
//! no convention, no kind or a date its kind does not take (W10.4, W11.1).
//! The instrument store's debt terms, the book's debt orders and the lake's
//! fixed-income rows read them here, so each refuses and says them alike.

use crate::exact::Exact;
use crate::v1::{Compounding, DayCount, PriceBasis, QuotedYield, RateConvention, YieldKind};

/// FIX CouponDayCount (1950)'s value for a day count, where FIX names it;
/// `None` for unspecified and for v27's `ACT_ACT` and `THIRTY_360`, which
/// name no variant (678).
pub fn fix_coupon_day_count(day_count: DayCount) -> Option<u32> {
    Some(match day_count {
        DayCount::Unspecified | DayCount::ActAct | DayCount::Thirty360 => return None,
        DayCount::OneOne => 0,
        DayCount::Thirty360BondBasis => 1,
        DayCount::Thirty360Sia => 2,
        DayCount::Thirty360M => 3,
        DayCount::ThirtyE360 => 4,
        DayCount::ThirtyE360Isda => 5,
        DayCount::Act360 => 6,
        DayCount::Act365Fixed => 7,
        DayCount::ActActAfb => 8,
        DayCount::ActActIcma => 9,
        DayCount::ActActIcmaUltimo => 10,
        DayCount::ActActIsda => 11,
        DayCount::Bus252 => 12,
        DayCount::ThirtyEPlus360 => 13,
        DayCount::Act365L => 14,
        DayCount::Nl365 => 15,
        DayCount::Nl360 => 16,
        DayCount::Act364 => 17,
    })
}

/// CDM's `DayCountFractionEnum` value a day count maps to (508; read from
/// FINOS's source 2026-10-26); `None` where CDM has none, so the contract
/// holds more: FIX's SIA and M 30/360s, ICMA ultimo, 30E+/360, NL/365,
/// NL/360 and BUS/252, and the two that name no variant.
pub fn cdm_day_count(day_count: DayCount) -> Option<&'static str> {
    Some(match day_count {
        DayCount::OneOne => "1/1",
        DayCount::Thirty360BondBasis => "30/360",
        DayCount::ThirtyE360 => "30E/360",
        DayCount::ThirtyE360Isda => "30E/360.ISDA",
        DayCount::Act360 => "ACT/360",
        DayCount::Act364 => "ACT/364",
        DayCount::Act365Fixed => "ACT/365.FIXED",
        DayCount::Act365L => "ACT/365L",
        DayCount::ActActAfb => "ACT/ACT.AFB",
        DayCount::ActActIcma => "ACT/ACT.ICMA",
        DayCount::ActActIsda => "ACT/ACT.ISDA",
        _ => return None,
    })
}

/// Whether a day count may stand on an instrument's terms (678): a variant
/// named. Unspecified, and v27's `ACT_ACT` and `THIRTY_360`, which keep
/// their "as stated" meaning for rates, are refused there.
pub fn names_a_variant(day_count: DayCount) -> bool {
    fix_coupon_day_count(day_count).is_some()
}

/// A day count as a person reads it, FIX's name for it (1950).
pub fn day_count_words(day_count: DayCount) -> &'static str {
    match day_count {
        DayCount::Unspecified => "",
        DayCount::Act360 => "Act/360",
        DayCount::Act365Fixed => "Act/365 (fixed)",
        DayCount::ActAct => "Act/Act, as stated",
        DayCount::Thirty360 => "30/360, as stated",
        DayCount::Bus252 => "BUS/252",
        DayCount::OneOne => "1/1",
        DayCount::Thirty360BondBasis => "30/360 (bond basis)",
        DayCount::Thirty360Sia => "30/360 (SIA)",
        DayCount::Thirty360M => "30/360M",
        DayCount::ThirtyE360 => "30E/360",
        DayCount::ThirtyE360Isda => "30E/360 (ISDA)",
        DayCount::ActActAfb => "Act/Act (AFB)",
        DayCount::ActActIcma => "Act/Act (ICMA)",
        DayCount::ActActIcmaUltimo => "Act/Act (ICMA ultimo)",
        DayCount::ActActIsda => "Act/Act (ISDA)",
        DayCount::ThirtyEPlus360 => "30E+/360",
        DayCount::Act365L => "Act/365L",
        DayCount::Nl365 => "NL/365",
        DayCount::Nl360 => "NL/360",
        DayCount::Act364 => "Act/364",
    }
}

/// A day count by its wire number, as a person reads it; empty for one the
/// schema does not define.
pub fn day_count_value_words(value: i32) -> &'static str {
    DayCount::try_from(value).map(day_count_words).unwrap_or("")
}

/// A rate's convention as a person reads it: "30/360 (bond basis),
/// compounded 2 a year".
pub fn rate_convention_words(convention: &RateConvention) -> String {
    let compounding =
        match Compounding::try_from(convention.compounding).unwrap_or(Compounding::Unspecified) {
            Compounding::Unspecified => String::new(),
            Compounding::Simple => "simple".into(),
            Compounding::Compounded => match convention.periods_per_year {
                0 => "compounded".into(),
                1 => "compounded annually".into(),
                2 => "compounded semi-annually".into(),
                4 => "compounded quarterly".into(),
                12 => "compounded monthly".into(),
                n => format!("compounded {n} a year"),
            },
            Compounding::Continuous => "continuous".into(),
            Compounding::Discount => "discount".into(),
        };
    [
        day_count_value_words(convention.day_count).to_string(),
        compounding,
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(", ")
}

/// A price basis as a person reads it, after FIX PriceType (423); before
/// v29 every price was per unit, which unspecified still reads as on a
/// trade, a quote, an order and an execution.
pub fn price_basis_words(basis: PriceBasis) -> &'static str {
    match basis {
        PriceBasis::Unspecified | PriceBasis::PerUnit => "per unit",
        PriceBasis::PercentOfPar => "percent of par, clean",
        PriceBasis::Yield => "yield",
        PriceBasis::Spread => "spread",
    }
}

/// What a quoted yield is to, as a person reads it (FIX YieldType, 235).
pub fn yield_kind_words(kind: YieldKind) -> &'static str {
    match kind {
        YieldKind::Unspecified => "",
        YieldKind::ToMaturity => "to maturity",
        YieldKind::ToCall => "to call",
        YieldKind::ToPut => "to put",
        YieldKind::ToWorst => "to worst",
        YieldKind::Current => "current",
        YieldKind::Discount => "discount",
    }
}

/// A quoted yield as a person reads it: "4.312% to call 2031-06-15
/// (30/360 (bond basis), compounded semi-annually)".
pub fn quoted_yield_words(quoted: &QuotedYield) -> String {
    let value = quoted
        .value
        .as_ref()
        .and_then(|value| value.value.as_ref())
        .and_then(|decimal| Exact::from_wire(decimal).ok())
        .map(|exact| format!("{exact}%"))
        .unwrap_or_default();
    let kind = yield_kind_words(YieldKind::try_from(quoted.kind).unwrap_or(YieldKind::Unspecified));
    let mut out = [value, kind.to_string(), quoted.to_date.clone()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(convention) = quoted.convention.as_ref() {
        let said = rate_convention_words(convention);
        if !said.is_empty() {
            out = format!("{out} ({said})");
        }
    }
    out
}

/// What the contract refuses of a quoted yield at `at` (W10.4, W11.1; the
/// shared dictionary): its value or its convention unset, its kind or its
/// convention's day count unspecified, its convention's compounding
/// unspecified, a date its kind does not take or a date that is not a
/// plain date. Each refusal names its field's path.
pub fn quoted_yield_refused(quoted: &QuotedYield, at: &str) -> Vec<(String, String)> {
    let mut refused = Vec::new();
    let present = quoted
        .value
        .as_ref()
        .and_then(|value| value.value.as_ref())
        .is_some();
    if !present {
        refused.push((
            format!("{at}.value"),
            "a quoted yield states its value, in percent".to_string(),
        ));
    }
    match quoted.convention.as_ref() {
        None => refused.push((
            format!("{at}.convention"),
            "a quoted yield states its convention".to_string(),
        )),
        Some(convention) => {
            if DayCount::try_from(convention.day_count).unwrap_or(DayCount::Unspecified)
                == DayCount::Unspecified
            {
                refused.push((
                    format!("{at}.convention.day_count"),
                    "a quoted yield's convention states its day count".to_string(),
                ));
            }
            if Compounding::try_from(convention.compounding).unwrap_or(Compounding::Unspecified)
                == Compounding::Unspecified
            {
                refused.push((
                    format!("{at}.convention.compounding"),
                    "a quoted yield's convention states its compounding".to_string(),
                ));
            }
        }
    }
    let kind = YieldKind::try_from(quoted.kind).unwrap_or(YieldKind::Unspecified);
    if kind == YieldKind::Unspecified {
        refused.push((
            format!("{at}.kind"),
            "a quoted yield states what it is to: maturity, call, put, worst, current or discount"
                .to_string(),
        ));
    }
    let dated = matches!(
        kind,
        YieldKind::ToCall | YieldKind::ToPut | YieldKind::ToWorst
    );
    if !quoted.to_date.is_empty() {
        if !dated && kind != YieldKind::Unspecified {
            refused.push((
                format!("{at}.to_date"),
                format!(
                    "a yield {} is to no date of its own",
                    yield_kind_words(kind)
                ),
            ));
        } else if !crate::date::is_date(&quoted.to_date) {
            refused.push((
                format!("{at}.to_date"),
                format!("{:?} is not a plain date (YYYY-MM-DD)", quoted.to_date),
            ));
        }
    } else if matches!(kind, YieldKind::ToCall | YieldKind::ToPut) {
        refused.push((
            format!("{at}.to_date"),
            format!(
                "a yield {} states the date it is to",
                yield_kind_words(kind)
            ),
        ));
    }
    refused
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::UnitValue;

    #[test]
    fn every_day_count_naming_a_variant_has_fixs_value_and_the_two_as_stated_none() {
        let mut fix = Vec::new();
        for value in 0..=20 {
            let day_count = DayCount::try_from(value).unwrap();
            match day_count {
                DayCount::Unspecified | DayCount::ActAct | DayCount::Thirty360 => {
                    assert!(!names_a_variant(day_count), "{day_count:?}");
                }
                _ => {
                    assert!(names_a_variant(day_count), "{day_count:?}");
                    fix.push(fix_coupon_day_count(day_count).unwrap());
                    assert!(!day_count_words(day_count).is_empty());
                }
            }
        }
        // FIX 1950's eighteen values, each once.
        fix.sort_unstable();
        assert_eq!(fix, (0..=17).collect::<Vec<_>>());
    }

    #[test]
    fn cdm_has_a_value_where_the_umbrella_says_and_holds_less_elsewhere() {
        // The umbrella task's line (508; CDM 7.8.0).
        assert_eq!(cdm_day_count(DayCount::Thirty360BondBasis), Some("30/360"));
        assert_eq!(cdm_day_count(DayCount::ActActIcma), Some("ACT/ACT.ICMA"));
        for held_more in [
            DayCount::Thirty360Sia,
            DayCount::Thirty360M,
            DayCount::ActActIcmaUltimo,
            DayCount::ThirtyEPlus360,
            DayCount::Nl365,
            DayCount::Nl360,
            DayCount::Bus252,
        ] {
            assert_eq!(cdm_day_count(held_more), None, "{held_more:?}");
        }
    }

    fn quoted(value: Option<&str>, kind: YieldKind, to_date: &str) -> QuotedYield {
        QuotedYield {
            value: value.map(|v| UnitValue {
                value: Some(v.parse::<Exact>().unwrap().to_wire()),
                ..Default::default()
            }),
            convention: Some(RateConvention {
                day_count: DayCount::Thirty360BondBasis as i32,
                compounding: Compounding::Compounded as i32,
                periods_per_year: 2,
            }),
            kind: kind as i32,
            to_date: to_date.into(),
        }
    }

    #[test]
    fn a_quoted_yield_is_said_and_refused_as_the_dictionary_says() {
        let call = quoted(Some("4.312"), YieldKind::ToCall, "2031-06-15");
        assert!(quoted_yield_refused(&call, "limit_yield").is_empty());
        assert_eq!(
            quoted_yield_words(&call),
            "4.312% to call 2031-06-15 (30/360 (bond basis), compounded semi-annually)"
        );
        let refused = |q: &QuotedYield| {
            quoted_yield_refused(q, "yield")
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            refused(&quoted(None, YieldKind::ToMaturity, "")),
            ["yield.value"]
        );
        assert_eq!(
            refused(&quoted(Some("4"), YieldKind::Unspecified, "")),
            ["yield.kind"]
        );
        assert_eq!(
            refused(&quoted(Some("4"), YieldKind::ToMaturity, "2031-06-15")),
            ["yield.to_date"]
        );
        assert_eq!(
            refused(&quoted(Some("4"), YieldKind::ToPut, "")),
            ["yield.to_date"]
        );
        assert_eq!(
            refused(&quoted(Some("4"), YieldKind::ToWorst, "June")),
            ["yield.to_date"]
        );
        assert!(refused(&quoted(Some("4"), YieldKind::ToWorst, "")).is_empty());
        let mut bare = quoted(Some("4"), YieldKind::Current, "");
        bare.convention = Some(RateConvention::default());
        assert_eq!(
            refused(&bare),
            ["yield.convention.day_count", "yield.convention.compounding"]
        );
    }

    #[test]
    fn a_price_basis_unspecified_reads_per_unit() {
        assert_eq!(price_basis_words(PriceBasis::Unspecified), "per unit");
        assert_eq!(
            price_basis_words(PriceBasis::PercentOfPar),
            "percent of par, clean"
        );
    }
}
