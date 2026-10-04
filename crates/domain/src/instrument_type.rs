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

use crate::v1::{
    AssetClass, InstrumentType, LiquidityFeeRegime, MoneyMarketFund, MoneyMarketFundCategory,
    MoneyMarketFundInvestors, MoneyMarketFundNav,
};

/// The asset class a type is under; `None` for no type.
pub fn class_of(kind: InstrumentType) -> Option<AssetClass> {
    match kind {
        InstrumentType::Unspecified => None,
        InstrumentType::MoneyMarketFund => Some(AssetClass::Fund),
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
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
