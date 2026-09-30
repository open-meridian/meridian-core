//! An asset class read from text.
//!
//! `meridian.v1.AssetClass` is a closed list the product owner ruled
//! (sdk-contract/asset-class-is-an-enum, 2026-09-28 and 2026-09-30). Before it,
//! the class was free text, so `EQUITY`, `Equity` and `equities` were three
//! classes and nothing refused a typo. Text still carries a class in two places
//! the runtime reads: the platform's JSON, which spells it as the enum's name
//! (`ASSET_CLASS_EQUITY`) as it spells a lifecycle state, and the instrument
//! store's column. [`read`] is the strict reading of both; [`legacy`] also
//! reads what the free text held, for the store's migration and for a platform
//! that has not yet migrated its own.

use crate::v1::AssetClass;

/// The class `text` names: the enum's name, or its name without the prefix in
/// lower case as the product owner spells it (`equity`, `crypto_asset`).
/// Empty is no class. Anything else is `None`, and never a guess.
pub fn read(text: &str) -> Option<AssetClass> {
    let text = text.trim();
    if text.is_empty() {
        return Some(AssetClass::Unspecified);
    }
    AssetClass::from_str_name(text).or_else(|| {
        let upper = text.to_ascii_uppercase();
        (text == upper.to_ascii_lowercase())
            .then(|| AssetClass::from_str_name(&format!("ASSET_CLASS_{upper}")))
            .flatten()
    })
}

/// The class a value of the old free-text field meant, where it plainly meant
/// one: any spelling [`read`] takes, a class's name in any case or plural, and
/// the kinds of instrument the ruling itself names under a class (ETFs and
/// mutual funds are funds, options and futures derivatives, bonds debt), and
/// `CRYPTO`, which the platform's agent tools offered as an example. `None` for
/// anything else, for a person to decide.
///
/// Mapping a kind of instrument to its class loses the kind: the instrument
/// type is not yet carried (its list is still open).
pub fn legacy(text: &str) -> Option<AssetClass> {
    if let Some(class) = read(text) {
        return Some(class);
    }
    let normal: String = text
        .trim()
        .to_ascii_uppercase()
        .split([' ', '-', '_'])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    Some(match normal.as_str() {
        "EQUITY" | "EQUITIES" => AssetClass::Equity,
        "DEBT" | "BOND" | "BONDS" => AssetClass::Debt,
        "FUND" | "FUNDS" | "ETF" | "ETFS" | "MUTUAL_FUND" | "MUTUAL_FUNDS" => AssetClass::Fund,
        "DERIVATIVE" | "DERIVATIVES" | "OPTION" | "OPTIONS" | "FUTURE" | "FUTURES" => {
            AssetClass::Derivative
        }
        "CRYPTO" | "CRYPTO_ASSET" | "CRYPTO_ASSETS" => AssetClass::CryptoAsset,
        "EVENT_CONTRACT" | "EVENT_CONTRACTS" => AssetClass::EventContract,
        "CASH" => AssetClass::Cash,
        _ => return None,
    })
}

/// The enum's name, or empty for no class: what the platform's JSON and the
/// instrument store's column hold.
pub fn name(class: AssetClass) -> &'static str {
    match class {
        AssetClass::Unspecified => "",
        class => class.as_str_name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_enums_name_and_the_ruled_spelling_are_read() {
        assert_eq!(read("ASSET_CLASS_EQUITY"), Some(AssetClass::Equity));
        assert_eq!(read("equity"), Some(AssetClass::Equity));
        assert_eq!(read("crypto_asset"), Some(AssetClass::CryptoAsset));
        assert_eq!(read("event_contract"), Some(AssetClass::EventContract));
        assert_eq!(read(""), Some(AssetClass::Unspecified));
    }

    #[test]
    fn anything_else_is_not_a_class() {
        for text in [
            "EQUITY",
            "Equity",
            "equities",
            "etf",
            "ASSET_CLASS_ETF",
            "stock",
        ] {
            assert_eq!(read(text), None, "{text}");
        }
    }

    #[test]
    fn the_free_text_maps_where_it_plainly_meant_a_class() {
        assert_eq!(legacy("EQUITY"), Some(AssetClass::Equity));
        assert_eq!(legacy("Equities"), Some(AssetClass::Equity));
        assert_eq!(legacy("ETF"), Some(AssetClass::Fund));
        assert_eq!(legacy("mutual fund"), Some(AssetClass::Fund));
        assert_eq!(legacy("OPTION"), Some(AssetClass::Derivative));
        assert_eq!(legacy("CRYPTO"), Some(AssetClass::CryptoAsset));
        assert_eq!(legacy("ASSET_CLASS_CASH"), Some(AssetClass::Cash));
    }

    #[test]
    fn what_it_did_not_plainly_mean_is_left_for_a_person() {
        for text in ["stock", "REIT", "commodity", "warrant", "unknown"] {
            assert_eq!(legacy(text), None, "{text}");
        }
    }

    #[test]
    fn no_class_is_written_as_nothing() {
        assert_eq!(name(AssetClass::Unspecified), "");
        assert_eq!(name(AssetClass::Fund), "ASSET_CLASS_FUND");
    }
}
