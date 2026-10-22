//! The deployment's FX pivot currencies, on its Settings (contract v27,
//! W6.36; rulings 462 and 483): a deployment admin's, one to four fiat cash
//! instruments tried in order when the analytics component crosses a fiat
//! leg the lake holds no direct rate for; USD's and then EUR's until set.
//! Each change its own record naming who and why (decisions/031), kept by
//! the conductor. The tab names each pivot by its ISO 4217 code; the form
//! takes the codes and sends their cash instruments. Under parity
//! `dashboard__read_fx_pivot_currencies` and
//! `dashboard__set_fx_pivot_currencies`.

use std::collections::BTreeMap;
use std::time::Duration;

use meridian_domain::v1::{FxPivotCurrenciesSetting, ReadFxPivotCurrenciesRequest};
use prost::Message;
use serde_json::{json, Value};

use crate::html::escape;

pub const SET_FX_PIVOT_CURRENCIES: &str = "platform.config.command.set-fx-pivot-currencies";
pub const READ_FX_PIVOT_CURRENCIES: &str = "platform.config.query.read-fx-pivot-currencies";

/// Where the tab's form posts.
pub const PATH: &str = "/admin/fx-pivot-currencies";

/// The setting's label (W6.36; ruling 483).
pub const LABEL: &str = "FX pivot currencies";

/// The pivots never set, by their codes, in order.
pub const DEFAULT_CODES: [&str; 2] = ["USD", "EUR"];

/// The most pivots a deployment orders.
pub const MOST: usize = 4;

/// The setting in force, from the conductor.
pub async fn read(bus: &meridian_bus::Bus) -> Result<FxPivotCurrenciesSetting, String> {
    let (_, bytes) = bus
        .call(
            READ_FX_PIVOT_CURRENCIES,
            "meridian.v1.ReadFxPivotCurrenciesRequest",
            ReadFxPivotCurrenciesRequest {}.encode_to_vec(),
            None,
            Some(Duration::from_secs(3)),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    FxPivotCurrenciesSetting::decode(&bytes[..])
        .map_err(|failed| format!("the conductor's answer did not read: {failed}"))
}

/// A fiat currency's ISO 4217 code, where the record is a cash instrument
/// carrying one.
pub fn code_of(record: &meridian_domain::v1::InstrumentRecord) -> Option<String> {
    (record.asset_class == meridian_domain::v1::AssetClass::Cash as i32)
        .then(|| {
            record
                .identifiers
                .iter()
                .find(|i| i.scheme == "iso4217")
                .map(|i| i.value.clone())
        })
        .flatten()
}

/// Each pivot's code, by its cash instrument, as the instrument store
/// answers them; an instrument it does not answer keeps its ID.
pub async fn codes(bus: &meridian_bus::Bus, ids: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for id in ids {
        if let Ok(reply) = crate::admin::instruments::list(bus, id).await {
            if let Some(code) = reply
                .instruments
                .iter()
                .filter_map(|held| held.instrument.as_ref())
                .find(|record| &record.instrument_id == id)
                .and_then(code_of)
            {
                out.insert(id.clone(), code);
            }
        }
    }
    out
}

/// The fiat cash instrument a code names, as the instrument store's search
/// finds it: a cash record whose iso4217 identifier is the code.
pub async fn instrument_of(bus: &meridian_bus::Bus, code: &str) -> Option<String> {
    let code = code.trim().to_ascii_uppercase();
    let found = crate::admin::instruments::search(bus, &code, 50)
        .await
        .ok()?;
    found
        .instruments
        .iter()
        .filter_map(|held| held.instrument.as_ref())
        .find(|record| code_of(record).as_deref() == Some(code.as_str()))
        .map(|record| record.instrument_id.clone())
}

/// The setting as a tool answers it: the pivots by their cash instruments
/// and codes, the default named where never set.
pub fn json(setting: &FxPivotCurrenciesSetting, codes: &BTreeMap<String, String>) -> Value {
    json!({
        "label": LABEL,
        "fx_pivot_currencies": setting.fx_pivot_currencies.iter().map(|id| json!({
            "instrument_id": id,
            "currency_code": codes.get(id).cloned().unwrap_or_default(),
        })).collect::<Vec<_>>(),
        "default": setting.fx_pivot_currencies.is_empty(),
        "default_codes": DEFAULT_CODES,
        "changed_at_ns": setting.changed_at_ns.to_string(),
        "changed_by": setting.changed_by,
    })
}

/// The pivots in words: `USD, then EUR`.
fn ordered(names: &[String]) -> String {
    names.join(", then ")
}

/// The tab's lines: the pivots in order and their last change.
pub fn lines(
    setting: Option<&FxPivotCurrenciesSetting>,
    codes: &BTreeMap<String, String>,
    name_of: impl Fn(&str) -> String,
) -> String {
    let Some(setting) = setting else {
        return "<p class=\"warn\">The FX pivot currencies could not be read just now.</p>"
            .to_string();
    };
    // Never set (ruling 512): read as empty, the readers falling back to
    // USD's and then EUR's, and said so.
    let (pivots, changed) = if setting.fx_pivot_currencies.is_empty() {
        (
            format!("{} (default)", DEFAULT_CODES.join(", ")),
            "never changed".to_string(),
        )
    } else {
        (
            ordered(
                &setting
                    .fx_pivot_currencies
                    .iter()
                    .map(|id| codes.get(id).cloned().unwrap_or_else(|| id.clone()))
                    .collect::<Vec<_>>(),
            ),
            format!(
                "set {} by {}",
                escape(&crate::custody::utc(setting.changed_at_ns)),
                escape(&name_of(&setting.changed_by))
            ),
        )
    };
    format!(
        "<dl class=\"facts\" data-fx-pivots=\"{p}\"><dt>{LABEL}</dt><dd><strong>{p}</strong> \
         <span class=\"faint\">{changed}</span></dd></dl><p class=\"hint\">Where the lake holds \
         no rate between two fiat currencies in either direction, a valuation crosses through \
         the first of these both reach. A crypto asset or a stablecoin is never crossed.</p>",
        p = escape(&pivots),
    )
}

/// The dialog's fields: up to four ISO 4217 codes in order, the first
/// required, and why.
pub fn fields(
    setting: Option<&FxPivotCurrenciesSetting>,
    codes: &BTreeMap<String, String>,
) -> String {
    let held: Vec<String> = match setting {
        Some(setting) if !setting.fx_pivot_currencies.is_empty() => setting
            .fx_pivot_currencies
            .iter()
            .map(|id| codes.get(id).cloned().unwrap_or_default())
            .collect(),
        _ => DEFAULT_CODES.iter().map(|c| c.to_string()).collect(),
    };
    let mut out = String::from("<div class=\"row-inline\">");
    for n in 0..MOST {
        let value = held.get(n).cloned().unwrap_or_default();
        out.push_str(&format!(
            "<label>{}<input name=\"pivot_{n}\" value=\"{}\" maxlength=\"3\" size=\"3\" \
             pattern=\"[A-Za-z]{{3}}\" autocomplete=\"off\" spellcheck=\"false\"{}></label>",
            if n == 0 {
                "First".to_string()
            } else {
                format!("Then ({})", n + 1)
            },
            escape(&value),
            if n == 0 { " required" } else { "" }
        ));
    }
    out.push_str(
        "</div><p class=\"hint\">ISO 4217 codes, tried in order: USD, EUR, GBP. Fiat currencies \
         only, none twice.</p>\
         <label>Why<textarea name=\"note\" required maxlength=\"2000\" rows=\"2\"></textarea></label>",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tab_says_the_pivots_in_order_and_their_change() {
        let held = FxPivotCurrenciesSetting {
            fx_pivot_currencies: vec!["INS-EUR".into(), "INS-USD".into()],
            changed_at_ns: 1_792_300_000_000_000_000,
            changed_by: "local|ada".into(),
        };
        let codes: BTreeMap<String, String> = [
            ("INS-EUR".to_string(), "EUR".to_string()),
            ("INS-USD".to_string(), "USD".to_string()),
        ]
        .into();
        let said = lines(Some(&held), &codes, |_| "Ada".into());
        assert!(said.contains("<strong>EUR, then USD</strong>"), "{said}");
        assert!(said.contains("by Ada"), "{said}");
        let default = lines(
            Some(&FxPivotCurrenciesSetting::default()),
            &BTreeMap::new(),
            |s| s.to_string(),
        );
        assert!(
            default.contains("<strong>USD, EUR (default)</strong>")
                && default.contains("never changed"),
            "{default}"
        );
        assert!(lines(None, &codes, |s| s.to_string()).contains("could not be read"));
        let form = fields(Some(&held), &codes);
        assert!(form.contains("name=\"pivot_0\" value=\"EUR\""), "{form}");
        assert!(form.contains("name=\"pivot_3\" value=\"\""), "{form}");
        let answered = json(&held, &codes);
        assert_eq!(answered["fx_pivot_currencies"][0]["currency_code"], "EUR");
        assert_eq!(answered["default"], false);
    }
}
