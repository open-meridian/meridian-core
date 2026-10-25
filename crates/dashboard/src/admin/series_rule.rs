//! The deployment's series request rule, on its Settings (contract v28,
//! W6.37; rulings 561 and 589): a deployment admin's, which wants for a
//! listed series not held the instrument store sends to the platform with no
//! person -- an order's, on by default; a standing want's, off by default --
//! and how many a day, 500 by default. Each change its own record naming
//! who and why (decisions/031), kept by the conductor. Under parity
//! `dashboard__read_series_request_rule` and
//! `dashboard__set_series_request_rule`.

use std::time::Duration;

use meridian_domain::v1::{ReadSeriesRequestRuleRequest, SeriesRequestRule};
use prost::Message;
use serde_json::{json, Value};

use crate::html::escape;

pub const SET_SERIES_REQUEST_RULE: &str = "platform.config.command.set-series-request-rule";
pub const READ_SERIES_REQUEST_RULE: &str = "platform.config.query.read-series-request-rule";

/// Where the tab's form posts.
pub const PATH: &str = "/admin/series-request-rule";

/// The setting's label (W6.37).
pub const LABEL: &str = "Series request rule";

/// The most series a day the rule allows.
pub const MOST_A_DAY: u32 = 5_000;

/// The rule in force, from the conductor.
pub async fn read(bus: &meridian_bus::Bus) -> Result<SeriesRequestRule, String> {
    let (_, bytes) = bus
        .call(
            READ_SERIES_REQUEST_RULE,
            "meridian.v1.ReadSeriesRequestRuleRequest",
            ReadSeriesRequestRuleRequest {}.encode_to_vec(),
            None,
            Some(Duration::from_secs(3)),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    SeriesRequestRule::decode(&bytes[..])
        .map_err(|failed| format!("the conductor's answer did not read: {failed}"))
}

/// The rule as a tool answers it.
pub fn json(rule: &SeriesRequestRule) -> Value {
    json!({
        "label": LABEL,
        "orders": rule.orders,
        "standing_wants": rule.standing_wants,
        "daily_limit": rule.daily_limit,
        "default": rule.changed_at_ns == 0,
        "changed_at_ns": rule.changed_at_ns.to_string(),
        "changed_by": rule.changed_by,
    })
}

fn on_off(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// The tab's lines: the three parts of the rule and its last change.
pub fn lines(rule: Option<&SeriesRequestRule>, name_of: impl Fn(&str) -> String) -> String {
    let Some(rule) = rule else {
        return "<p class=\"warn\">The series request rule could not be read just now.</p>"
            .to_string();
    };
    let changed = if rule.changed_at_ns == 0 {
        "the default, never changed".to_string()
    } else {
        format!(
            "set {} by {}",
            escape(&crate::custody::utc(rule.changed_at_ns)),
            escape(&name_of(&rule.changed_by))
        )
    };
    format!(
        "<dl class=\"facts\" data-series-rule=\"{orders}/{standing}/{limit}\">\
         <dt>For an order</dt><dd><strong>{orders}</strong></dd>\
         <dt>For a standing want</dt><dd><strong>{standing}</strong></dd>\
         <dt>A day, at most</dt><dd><strong>{limit}</strong> series \
         <span class=\"faint\">{changed}</span></dd></dl>\
         <p class=\"hint\">A listed option or future this deployment does not hold is asked of \
         the platform only as this allows; a series the rule does not allow is refused, and \
         nothing is sent.</p>",
        orders = on_off(rule.orders),
        standing = on_off(rule.standing_wants),
        limit = rule.daily_limit,
    )
}

/// The dialog's fields: the two wants, the day's limit, and why.
pub fn fields(rule: Option<&SeriesRequestRule>) -> String {
    let (orders, standing, limit) = rule
        .map(|rule| (rule.orders, rule.standing_wants, rule.daily_limit))
        .unwrap_or((true, false, 500));
    let checked = |on: bool| if on { " checked" } else { "" };
    format!(
        "<label class=\"check\"><input type=\"checkbox\" name=\"orders\" value=\"on\"{}> For an \
         order or a proposal</label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"standing_wants\" value=\"on\"{}> \
         For a standing want</label>\
         <label>A day, at most<input name=\"daily_limit\" type=\"number\" min=\"1\" \
         max=\"{MOST_A_DAY}\" value=\"{limit}\" required></label>\
         <label>Why<textarea name=\"note\" required maxlength=\"2000\" rows=\"2\"></textarea></label>",
        checked(orders),
        checked(standing),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tab_says_the_rule_its_default_and_its_change() {
        let default = SeriesRequestRule {
            orders: true,
            standing_wants: false,
            daily_limit: 500,
            ..Default::default()
        };
        let said = lines(Some(&default), |s| s.to_string());
        assert!(said.contains("data-series-rule=\"on/off/500\""), "{said}");
        assert!(said.contains("never changed"), "{said}");
        let changed = SeriesRequestRule {
            standing_wants: true,
            daily_limit: 100,
            changed_at_ns: 1_792_300_000_000_000_000,
            changed_by: "local|ada".into(),
            ..default.clone()
        };
        let said = lines(Some(&changed), |_| "Ada".into());
        assert!(
            said.contains("on/on/100") && said.contains("by Ada"),
            "{said}"
        );
        assert!(lines(None, |s| s.to_string()).contains("could not be read"));
        let form = fields(Some(&changed));
        assert!(
            form.contains("name=\"standing_wants\" value=\"on\" checked"),
            "{form}"
        );
        assert!(form.contains("value=\"100\""), "{form}");
        assert_eq!(json(&changed)["daily_limit"], 100);
        assert_eq!(json(&default)["default"], true);
    }
}
