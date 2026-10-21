//! An instrument's type within its asset class, and a money market fund's
//! attributes, read from and written as text (contract v11).
//!
//! `meridian.v1.InstrumentType` grows one type at a time as a workflow needs
//! it, each under one asset class, in the contract's own words (ISO 10962 a
//! reference, not the vocabulary). The money market fund is the first: a fund,
//! held as a fund and never as cash, with four attributes from SEC rule 2a-7
//! (the product owner, 2026-10-02). The instrument store keeps a type as its
//! enum's name and a fund's attributes as their four names in order, and the
//! dashboard reads and writes the same text.
//!
//! The binary event contract is the second (contract v19; plans/the-lake-
//! prices-the-book, Q38): under event_contract, a contract paying a fixed
//! amount if an event happens, with its payout (a Money naming its cash
//! instrument) and when trading in it closes. Which outcome it pays on stays
//! in its description, the venue's words. The store keeps its attributes as
//! one line of text, read and written here.

use crate::exact::Exact;
use crate::v1::{
    AssetClass, BinaryEventContract, EventContractBound, InstrumentType, LiquidityFeeRegime, Money,
    MoneyMarketFund, MoneyMarketFundCategory, MoneyMarketFundInvestors, MoneyMarketFundNav,
};

/// The asset class a type is under; `None` for no type.
pub fn class_of(kind: InstrumentType) -> Option<AssetClass> {
    match kind {
        InstrumentType::Unspecified => None,
        InstrumentType::MoneyMarketFund => Some(AssetClass::Fund),
        InstrumentType::BinaryEventContract => Some(AssetClass::EventContract),
        // Contract v27, the rates slice: what the lake observes and nobody
        // holds.
        InstrumentType::InterestRate | InstrumentType::RateCurve | InstrumentType::Index => {
            Some(AssetClass::Referential)
        }
    }
}

/// A type as the store holds it: the enum's name, or empty for none.
pub fn name(kind: InstrumentType) -> &'static str {
    match kind {
        InstrumentType::Unspecified => "",
        kind => kind.as_str_name(),
    }
}

/// The type `text` names: the enum's name, or its name without the prefix in
/// lower case (`money_market_fund`). Empty is no type; anything else `None`.
pub fn read(text: &str) -> Option<InstrumentType> {
    let text = text.trim();
    if text.is_empty() {
        return Some(InstrumentType::Unspecified);
    }
    InstrumentType::from_str_name(text).or_else(|| {
        let upper = text.to_ascii_uppercase();
        InstrumentType::from_str_name(&format!("INSTRUMENT_TYPE_{upper}"))
    })
}

/// A type as a person reads it: `money market fund`.
pub fn words(kind: InstrumentType) -> &'static str {
    match kind {
        InstrumentType::Unspecified => "",
        InstrumentType::MoneyMarketFund => "money market fund",
        InstrumentType::BinaryEventContract => "binary event contract",
        InstrumentType::InterestRate => "interest rate",
        InstrumentType::RateCurve => "rate curve",
        InstrumentType::Index => "index",
    }
}

/// Every type the list holds, in its order: what a form offers.
pub const TYPES: [InstrumentType; 2] = [
    InstrumentType::MoneyMarketFund,
    InstrumentType::BinaryEventContract,
];

/// A type by its name without the prefix, in lower case
/// (`binary_event_contract`): how a refusal lists the types.
pub fn short(kind: InstrumentType) -> String {
    kind.as_str_name()
        .strip_prefix("INSTRUMENT_TYPE_")
        .unwrap_or(kind.as_str_name())
        .to_ascii_lowercase()
}

/// A fund's attributes as the store holds them: their four enum names, in
/// order, separated by a space.
pub fn fund_to_text(fund: &MoneyMarketFund) -> String {
    [
        MoneyMarketFundCategory::try_from(fund.category)
            .unwrap_or(MoneyMarketFundCategory::Unspecified)
            .as_str_name(),
        MoneyMarketFundInvestors::try_from(fund.investors)
            .unwrap_or(MoneyMarketFundInvestors::Unspecified)
            .as_str_name(),
        MoneyMarketFundNav::try_from(fund.nav)
            .unwrap_or(MoneyMarketFundNav::Unspecified)
            .as_str_name(),
        LiquidityFeeRegime::try_from(fund.liquidity_fee)
            .unwrap_or(LiquidityFeeRegime::Unspecified)
            .as_str_name(),
    ]
    .join(" ")
}

/// The attributes `text` holds, or `None` for empty or text this did not write.
pub fn fund_from_text(text: &str) -> Option<MoneyMarketFund> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [category, investors, nav, fee] = parts.as_slice() else {
        return None;
    };
    Some(MoneyMarketFund {
        category: MoneyMarketFundCategory::from_str_name(category)? as i32,
        investors: MoneyMarketFundInvestors::from_str_name(investors)? as i32,
        nav: MoneyMarketFundNav::from_str_name(nav)? as i32,
        liquidity_fee: LiquidityFeeRegime::from_str_name(fee)? as i32,
    })
}

/// Each attribute of `fund` not stated, by its field name: the attributes are
/// stated together, and a person completes all four.
pub fn unstated(fund: &MoneyMarketFund) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !matches!(
        MoneyMarketFundCategory::try_from(fund.category),
        Ok(value) if value != MoneyMarketFundCategory::Unspecified
    ) {
        missing.push("category");
    }
    if !matches!(
        MoneyMarketFundInvestors::try_from(fund.investors),
        Ok(value) if value != MoneyMarketFundInvestors::Unspecified
    ) {
        missing.push("investors");
    }
    if !matches!(
        MoneyMarketFundNav::try_from(fund.nav),
        Ok(value) if value != MoneyMarketFundNav::Unspecified
    ) {
        missing.push("nav");
    }
    if !matches!(
        LiquidityFeeRegime::try_from(fund.liquidity_fee),
        Ok(value) if value != LiquidityFeeRegime::Unspecified
    ) {
        missing.push("liquidity_fee");
    }
    missing
}

/// A fund's attributes as a person reads them: `government, retail, stable
/// NAV, discretionary liquidity fee`, and `no liquidity fee` for a fund that
/// charges none, which read as "none fee" until 2026-10-03.
pub fn fund_words(text: &str) -> String {
    let Some(fund) = fund_from_text(text) else {
        return text.to_string();
    };
    let lower = |name: &str, prefix: &str| {
        name.strip_prefix(prefix)
            .unwrap_or(name)
            .to_ascii_lowercase()
            .replace('_', "-")
    };
    let fee = match LiquidityFeeRegime::try_from(fund.liquidity_fee).unwrap_or_default() {
        LiquidityFeeRegime::None => "no liquidity fee".to_string(),
        regime => format!(
            "{} liquidity fee",
            lower(regime.as_str_name(), "LIQUIDITY_FEE_REGIME_")
        ),
    };
    format!(
        "{}, {}, {} NAV, {fee}",
        lower(
            MoneyMarketFundCategory::try_from(fund.category)
                .unwrap_or_default()
                .as_str_name(),
            "MONEY_MARKET_FUND_CATEGORY_"
        ),
        lower(
            MoneyMarketFundInvestors::try_from(fund.investors)
                .unwrap_or_default()
                .as_str_name(),
            "MONEY_MARKET_FUND_INVESTORS_"
        ),
        lower(
            MoneyMarketFundNav::try_from(fund.nav)
                .unwrap_or_default()
                .as_str_name(),
            "MONEY_MARKET_FUND_NAV_"
        ),
    )
}

// ── A binary event contract's attributes (contract v19) ──────────────────

/// A binary event contract's attributes as the store holds them: the
/// payout's amount, its ISO 4217 code and its cash instrument (either may be
/// empty, not both), and when trading closes in nanoseconds, separated by
/// `|`. A range market's bounds (contract v26, W1.24) follow as three more
/// fields -- the floor, the cap and their unit, a bound written as its value
/// with `=` before it when inclusive, empty for an open tail -- and only when
/// one is set, so a contract with none keeps v19's four.
pub fn contract_to_text(contract: &BinaryEventContract) -> String {
    let payout = contract.payout.clone().unwrap_or_default();
    let amount = payout
        .amount
        .as_ref()
        .and_then(|d| Exact::from_wire(d).ok())
        .map(|e| e.to_string())
        .unwrap_or_default();
    let mut text = format!(
        "{amount}|{}|{}|{}",
        payout.currency_code.trim(),
        payout.instrument_id.trim(),
        contract.closes_at_ns
    );
    if contract.floor.is_some() || contract.cap.is_some() {
        text.push_str(&format!(
            "|{}|{}|{}",
            bound_to_text(contract.floor.as_ref()),
            bound_to_text(contract.cap.as_ref()),
            contract.bound_unit.trim()
        ));
    }
    text
}

fn bound_to_text(bound: Option<&EventContractBound>) -> String {
    let Some(bound) = bound else {
        return String::new();
    };
    let value = bound
        .value
        .as_ref()
        .and_then(|d| Exact::from_wire(d).ok())
        .map(|e| e.to_string())
        .unwrap_or_default();
    if bound.inclusive {
        format!("={value}")
    } else {
        value
    }
}

fn bound_from_text(text: &str) -> Option<Option<EventContractBound>> {
    if text.is_empty() {
        return Some(None);
    }
    let (inclusive, value) = match text.strip_prefix('=') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let value: Exact = value.parse().ok()?;
    Some(Some(EventContractBound {
        value: Some(value.to_wire()),
        inclusive,
    }))
}

/// The attributes `text` holds, or `None` for empty or text this did not
/// write.
pub fn contract_from_text(text: &str) -> Option<BinaryEventContract> {
    let parts: Vec<&str> = text.split('|').collect();
    let (amount, code, instrument, closes, floor, cap, unit) = match parts.as_slice() {
        [amount, code, instrument, closes] => (amount, code, instrument, closes, None, None, ""),
        [amount, code, instrument, closes, floor, cap, unit] => (
            amount,
            code,
            instrument,
            closes,
            bound_from_text(floor)?,
            bound_from_text(cap)?,
            *unit,
        ),
        _ => return None,
    };
    let amount: Exact = amount.parse().ok()?;
    Some(BinaryEventContract {
        payout: Some(Money {
            amount: Some(amount.to_wire()),
            currency_code: code.to_string(),
            instrument_id: instrument.to_string(),
        }),
        closes_at_ns: closes.parse().ok()?,
        floor,
        cap,
        bound_unit: unit.to_string(),
    })
}

/// What is wrong with a binary event contract's attributes, each by its
/// field and in words: a payout naming no asset, naming a token's code, or
/// not more than zero; a close before the epoch. Empty when they stand.
pub fn contract_refused(contract: &BinaryEventContract) -> Vec<(&'static str, String)> {
    let mut wrong = Vec::new();
    match contract.payout.as_ref() {
        None => wrong.push((
            "payout",
            "a binary event contract states what it pays, a Money naming its asset".to_string(),
        )),
        Some(payout) => {
            if let Err(said) = crate::money::asset_of("payout", payout) {
                wrong.push(("payout", said));
            }
            match payout.amount.as_ref().map(Exact::from_wire) {
                Some(Ok(amount)) if amount > Exact::ZERO => {}
                Some(Ok(_)) => wrong.push((
                    "payout.amount",
                    "a binary event contract pays more than zero".to_string(),
                )),
                Some(Err(out)) => wrong.push(("payout.amount", format!("the payout {out}"))),
                None => wrong.push((
                    "payout.amount",
                    "a binary event contract states how much it pays".to_string(),
                )),
            }
        }
    }
    if contract.closes_at_ns < 0 {
        wrong.push((
            "closes_at_ns",
            "when trading closes is a time after 1970, or 0 where the venue states none"
                .to_string(),
        ));
    }
    let mut bounds = Vec::new();
    for (field, bound) in [
        ("floor", contract.floor.as_ref()),
        ("cap", contract.cap.as_ref()),
    ] {
        let Some(bound) = bound else { continue };
        match bound.value.as_ref().map(Exact::from_wire) {
            Some(Ok(value)) => bounds.push(value),
            Some(Err(out)) => wrong.push((
                if field == "floor" {
                    "floor.value"
                } else {
                    "cap.value"
                },
                format!("the bound {out}"),
            )),
            None => wrong.push((
                if field == "floor" {
                    "floor.value"
                } else {
                    "cap.value"
                },
                "a range market's bound states its value".to_string(),
            )),
        }
    }
    if let [floor, cap] = bounds.as_slice() {
        if floor > cap {
            wrong.push((
                "cap",
                "a range market's cap is not below its floor".to_string(),
            ));
        }
    }
    let bounded = contract.floor.is_some() || contract.cap.is_some();
    if bounded && contract.bound_unit.trim().is_empty() {
        wrong.push((
            "bound_unit",
            "a range market's bounds name their unit".to_string(),
        ));
    }
    if !bounded && !contract.bound_unit.trim().is_empty() {
        wrong.push((
            "bound_unit",
            "a unit names the bounds' unit, and this contract has none".to_string(),
        ));
    }
    if contract.bound_unit.contains('|') {
        wrong.push(("bound_unit", "a unit is one word, without `|`".to_string()));
    }
    wrong
}

/// A binary event contract's attributes as a person reads them: `pays 1
/// USD, closes 2026-11-03 00:00 UTC`.
pub fn contract_words(text: &str) -> String {
    let Some(contract) = contract_from_text(text) else {
        return text.to_string();
    };
    let payout = contract.payout.unwrap_or_default();
    let amount = payout
        .amount
        .as_ref()
        .and_then(|d| Exact::from_wire(d).ok())
        .map(|e| e.to_string())
        .unwrap_or_default();
    let asset = match (payout.currency_code.as_str(), payout.instrument_id.as_str()) {
        ("", instrument) => instrument.to_string(),
        (code, "") => code.to_string(),
        (code, instrument) => format!("{code} ({instrument})"),
    };
    let mut line = format!("pays {amount} {asset}");
    let unit = contract.bound_unit.as_str();
    let said = |bound: &EventContractBound| {
        bound
            .value
            .as_ref()
            .and_then(|d| Exact::from_wire(d).ok())
            .map(|e| format!("{e} {unit}"))
            .unwrap_or_default()
    };
    match (contract.floor.as_ref(), contract.cap.as_ref()) {
        (Some(floor), Some(cap)) => line.push_str(&format!(
            ", between {} ({}) and {} ({})",
            said(floor),
            if floor.inclusive {
                "inclusive"
            } else {
                "exclusive"
            },
            said(cap),
            if cap.inclusive {
                "inclusive"
            } else {
                "exclusive"
            }
        )),
        (Some(floor), None) => line.push_str(&format!(
            ", {} {}",
            if floor.inclusive {
                "at or above"
            } else {
                "above"
            },
            said(floor)
        )),
        (None, Some(cap)) => line.push_str(&format!(
            ", {} {}",
            if cap.inclusive {
                "at or below"
            } else {
                "below"
            },
            said(cap)
        )),
        (None, None) => {}
    }
    if contract.closes_at_ns > 0 {
        line.push_str(&format!(", closes {}", utc_minute(contract.closes_at_ns)));
    }
    line
}

/// A moment as `2026-11-03 00:00 UTC`.
pub fn utc_minute(ns: i64) -> String {
    let date = crate::date::Date::of_utc_instant(ns);
    let in_day = ns.rem_euclid(86_400 * 1_000_000_000) / 1_000_000_000;
    format!(
        "{date} {:02}:{:02} UTC",
        in_day / 3600,
        (in_day % 3600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_event_contract_is_an_event_contract_and_its_attributes_round_trip() {
        assert_eq!(
            class_of(InstrumentType::BinaryEventContract),
            Some(AssetClass::EventContract)
        );
        assert_eq!(
            read("binary_event_contract"),
            Some(InstrumentType::BinaryEventContract)
        );
        assert_eq!(
            short(InstrumentType::BinaryEventContract),
            "binary_event_contract"
        );
        let contract = BinaryEventContract {
            payout: Some(Money {
                amount: Some(Exact::new(1, 0).unwrap().to_wire()),
                currency_code: "USD".into(),
                instrument_id: String::new(),
            }),
            closes_at_ns: 1_793_664_000_000_000_000,
            ..Default::default()
        };
        assert!(contract_refused(&contract).is_empty());
        let text = contract_to_text(&contract);
        assert_eq!(text, "1|USD||1793664000000000000");
        assert_eq!(contract_from_text(&text), Some(contract.clone()));
        assert_eq!(
            contract_words(&text),
            "pays 1 USD, closes 2026-11-03 00:00 UTC"
        );
        let nothing = BinaryEventContract {
            payout: Some(Money {
                amount: Some(Exact::ZERO.to_wire()),
                currency_code: String::new(),
                instrument_id: String::new(),
            }),
            closes_at_ns: 0,
            ..Default::default()
        };
        let fields: Vec<&str> = contract_refused(&nothing).iter().map(|(f, _)| *f).collect();
        assert_eq!(fields, vec!["payout", "payout.amount"]);
        assert_eq!(contract_from_text(""), None);
    }

    #[test]
    fn a_range_markets_bounds_round_trip_and_are_checked() {
        let bound = |value: i128, inclusive: bool| EventContractBound {
            value: Some(Exact::new(value, 0).unwrap().to_wire()),
            inclusive,
        };
        let range = BinaryEventContract {
            payout: Some(Money {
                amount: Some(Exact::new(1, 0).unwrap().to_wire()),
                currency_code: "USD".into(),
                instrument_id: String::new(),
            }),
            closes_at_ns: 0,
            floor: Some(bound(70, true)),
            cap: Some(bound(74, false)),
            bound_unit: "degrees_f".into(),
        };
        assert!(contract_refused(&range).is_empty());
        let text = contract_to_text(&range);
        assert_eq!(text, "1|USD||0|=70|74|degrees_f");
        assert_eq!(contract_from_text(&text), Some(range.clone()));
        assert_eq!(
            contract_words(&text),
            "pays 1 USD, between 70 degrees_f (inclusive) and 74 degrees_f (exclusive)"
        );
        let tail = BinaryEventContract {
            cap: None,
            ..range.clone()
        };
        let text = contract_to_text(&tail);
        assert_eq!(text, "1|USD||0|=70||degrees_f");
        assert_eq!(contract_from_text(&text), Some(tail.clone()));
        assert_eq!(
            contract_words(&text),
            "pays 1 USD, at or above 70 degrees_f"
        );
        let upside_down = BinaryEventContract {
            floor: Some(bound(80, false)),
            ..range.clone()
        };
        let fields: Vec<&str> = contract_refused(&upside_down)
            .iter()
            .map(|(f, _)| *f)
            .collect();
        assert_eq!(fields, vec!["cap"]);
        let unitless = BinaryEventContract {
            bound_unit: String::new(),
            ..range
        };
        let fields: Vec<&str> = contract_refused(&unitless)
            .iter()
            .map(|(f, _)| *f)
            .collect();
        assert_eq!(fields, vec!["bound_unit"]);
    }

    #[test]
    fn a_money_market_fund_is_a_fund_and_reads_both_ways() {
        assert_eq!(
            class_of(InstrumentType::MoneyMarketFund),
            Some(AssetClass::Fund)
        );
        assert_eq!(class_of(InstrumentType::Unspecified), None);
        assert_eq!(
            read("money_market_fund"),
            Some(InstrumentType::MoneyMarketFund)
        );
        assert_eq!(
            read(name(InstrumentType::MoneyMarketFund)),
            Some(InstrumentType::MoneyMarketFund)
        );
        assert_eq!(read(""), Some(InstrumentType::Unspecified));
        assert_eq!(read("etf"), None);
    }

    #[test]
    fn a_funds_attributes_round_trip_as_text_and_say_what_is_missing() {
        let fund = MoneyMarketFund {
            category: MoneyMarketFundCategory::Government as i32,
            investors: MoneyMarketFundInvestors::Retail as i32,
            nav: MoneyMarketFundNav::Stable as i32,
            liquidity_fee: LiquidityFeeRegime::Discretionary as i32,
        };
        let text = fund_to_text(&fund);
        assert_eq!(fund_from_text(&text), Some(fund));
        assert!(unstated(&fund).is_empty());
        assert_eq!(
            fund_words(&text),
            "government, retail, stable NAV, discretionary liquidity fee"
        );
        for (regime, said) in [
            (LiquidityFeeRegime::None, "no liquidity fee"),
            (LiquidityFeeRegime::Mandatory, "mandatory liquidity fee"),
        ] {
            let text = fund_to_text(&MoneyMarketFund {
                liquidity_fee: regime as i32,
                ..fund
            });
            assert_eq!(
                fund_words(&text),
                format!("government, retail, stable NAV, {said}")
            );
        }
        let partial = MoneyMarketFund {
            category: MoneyMarketFundCategory::Prime as i32,
            ..Default::default()
        };
        assert_eq!(
            unstated(&partial),
            vec!["investors", "nav", "liquidity_fee"]
        );
        assert_eq!(fund_from_text(""), None);
    }
}
