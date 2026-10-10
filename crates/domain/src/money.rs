//! A Money names its instrument (decisions/023 as amended at contract v18;
//! plans/the-lake-prices-the-book, ruling 1 of 2026-10-09).
//!
//! Every amount is in the cash instrument of its asset, fiat and tokens
//! alike: `Money.instrument_id`, the deployment's own ID. A plugin may name
//! a fiat currency by its ISO 4217 code alone, which core resolves, dated,
//! to that currency's cash instrument through the instrument store (W3.1,
//! by `iso4217`); an asset with no ISO 4217 code -- USDC, USDT, a network's
//! gas token -- is named by its instrument alone, which the plugin resolves
//! as it resolves any instrument. A token's code in `currency_code` is
//! refused, and so is a Money naming neither. What a store keeps and
//! answers names the instrument.
//!
//! This is the part every component reads alike: what a Money names, and
//! the codes a store has resolved. Asking the instrument store is each
//! component's own, over the bus.

use std::collections::BTreeMap;

use crate::v1::Money;

/// The scheme a currency's ISO 4217 code is an identifier under.
pub const ISO4217: &str = "iso4217";

/// Whether `code` has an ISO 4217 code's form: three capital letters. A
/// token's code (`USDC`, `usdt`, `ETH2`) does not, and a fiat currency's
/// does; whether the deployment holds it is the instrument store's to say.
pub fn is_iso4217(code: &str) -> bool {
    code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase())
}

/// What a Money names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asset {
    /// A fiat currency by its ISO 4217 code alone, for core to resolve.
    Code(String),
    /// A cash instrument alone: a token's, or a currency's already resolved.
    Instrument(String),
    /// Both: they must name the same asset.
    Both { code: String, instrument: String },
}

/// What `money` names, or the refusal naming `field`: neither set, or a
/// token's code in `currency_code` (decisions/023 as amended).
pub fn asset_of(field: &str, money: &Money) -> Result<Asset, String> {
    let code = money.currency_code.trim();
    let instrument = money.instrument_id.trim();
    if !code.is_empty() && !is_iso4217(code) {
        return Err(format!(
            "{field}.currency_code {code:?} is not an ISO 4217 code: an asset with none, a \
             token, is named by its cash instrument alone, in instrument_id"
        ));
    }
    match (code.is_empty(), instrument.is_empty()) {
        (true, true) => Err(format!(
            "{field} names no asset: a Money names its cash instrument (instrument_id), or a \
             fiat currency by its ISO 4217 code (currency_code)"
        )),
        (false, true) => Ok(Asset::Code(code.to_string())),
        (true, false) => Ok(Asset::Instrument(instrument.to_string())),
        (false, false) => Ok(Asset::Both {
            code: code.to_string(),
            instrument: instrument.to_string(),
        }),
    }
}

/// The ISO 4217 codes a component has resolved to cash instruments, each
/// with when, so it asks the instrument store once per code. A code maps to
/// one instrument for a deployment's life: the instrument store keeps one
/// record per identifier set, and a merge is followed by its replacement
/// event like any other record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CashInstruments {
    by_code: BTreeMap<String, String>,
}

impl CashInstruments {
    /// The cash instrument a code was resolved to, where it was.
    pub fn instrument(&self, code: &str) -> Option<&str> {
        self.by_code.get(code).map(String::as_str)
    }

    /// The code a cash instrument was resolved from, where one was.
    pub fn code(&self, instrument: &str) -> Option<&str> {
        self.by_code
            .iter()
            .find(|(_, held)| held.as_str() == instrument)
            .map(|(code, _)| code.as_str())
    }

    pub fn insert(&mut self, code: impl Into<String>, instrument: impl Into<String>) {
        self.by_code.insert(code.into(), instrument.into());
    }

    pub fn is_empty(&self) -> bool {
        self.by_code.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_code.iter().map(|(c, i)| (c.as_str(), i.as_str()))
    }

    /// `money` as kept and answered: its instrument filled from its code
    /// where it was resolved, and its code from its instrument where the
    /// instrument is a currency's. Changes nothing a Money already names.
    pub fn fill(&self, money: &mut Money) {
        if money.instrument_id.is_empty() {
            if let Some(instrument) = self.instrument(&money.currency_code) {
                money.instrument_id = instrument.to_string();
            }
        }
        if money.currency_code.is_empty() {
            if let Some(code) = self.code(&money.instrument_id) {
                money.currency_code = code.to_string();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn money(code: &str, instrument: &str) -> Money {
        Money {
            amount: None,
            currency_code: code.into(),
            instrument_id: instrument.into(),
        }
    }

    #[test]
    fn a_money_names_a_code_an_instrument_or_both_and_never_a_tokens_code() {
        assert_eq!(
            asset_of("price", &money("USD", "")).unwrap(),
            Asset::Code("USD".into())
        );
        assert_eq!(
            asset_of("price", &money("", "LCL-USDC")).unwrap(),
            Asset::Instrument("LCL-USDC".into())
        );
        assert!(matches!(
            asset_of("price", &money("USD", "LCL-9")).unwrap(),
            Asset::Both { .. }
        ));
        let token = asset_of("prices[0].price", &money("USDC", "")).unwrap_err();
        assert!(
            token.starts_with("prices[0].price.currency_code"),
            "{token}"
        );
        assert!(asset_of("price", &money("usd", "")).is_err());
        let neither = asset_of("price", &money("", "")).unwrap_err();
        assert!(neither.contains("names no asset"), "{neither}");
    }

    #[test]
    fn a_resolved_code_fills_the_instrument_and_back() {
        let mut held = CashInstruments::default();
        held.insert("USD", "LCL-USD");
        let mut said = money("USD", "");
        held.fill(&mut said);
        assert_eq!(said.instrument_id, "LCL-USD");
        let mut said = money("", "LCL-USD");
        held.fill(&mut said);
        assert_eq!(said.currency_code, "USD");
        let mut token = money("", "LCL-USDC");
        held.fill(&mut token);
        assert_eq!(token.currency_code, "", "a token has no ISO 4217 code");
    }
}
