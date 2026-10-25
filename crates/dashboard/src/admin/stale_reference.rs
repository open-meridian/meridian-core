//! The stale-reference actions the deployment permits, on its Settings
//! (contract v28, W6.38; rulings 515, 517 and 600): a deployment admin's,
//! whether an `ems` plugin's admin may choose to warn, beside refuse, when
//! a placement's reference price is past its ceiling; refuse only by
//! default. Each change its own record naming who and why (decisions/031),
//! kept by the conductor. Under parity
//! `dashboard__read_stale_reference_actions_permitted` and
//! `dashboard__set_stale_reference_actions_permitted`.

use std::time::Duration;

use meridian_domain::v1::{
    ReadStaleReferenceActionsPermittedRequest, StaleReferenceActionsPermittedSetting,
};
use prost::Message;
use serde_json::{json, Value};

use crate::html::escape;

pub const SET_STALE_REFERENCE_ACTIONS_PERMITTED: &str =
    "platform.config.command.set-stale-reference-actions-permitted";
pub const READ_STALE_REFERENCE_ACTIONS_PERMITTED: &str =
    "platform.config.query.read-stale-reference-actions-permitted";

/// The setting's label (W6.38; ruling 517).
pub const LABEL: &str = "Stale reference actions permitted";

/// The setting in force, from the conductor.
pub async fn read(
    bus: &meridian_bus::Bus,
) -> Result<StaleReferenceActionsPermittedSetting, String> {
    let (_, bytes) = bus
        .call(
            READ_STALE_REFERENCE_ACTIONS_PERMITTED,
            "meridian.v1.ReadStaleReferenceActionsPermittedRequest",
            ReadStaleReferenceActionsPermittedRequest {}.encode_to_vec(),
            None,
            Some(Duration::from_secs(3)),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    StaleReferenceActionsPermittedSetting::decode(&bytes[..])
        .map_err(|failed| format!("the conductor's answer did not read: {failed}"))
}

/// The setting as a tool answers it.
pub fn json(setting: &StaleReferenceActionsPermittedSetting) -> Value {
    json!({
        "label": LABEL,
        "warn_permitted": setting.warn_permitted,
        "actions": if setting.warn_permitted { vec!["refuse", "warn"] } else { vec!["refuse"] },
        "default": setting.changed_at_ns == 0,
        "changed_at_ns": setting.changed_at_ns.to_string(),
        "changed_by": setting.changed_by,
    })
}

/// The tab's lines: what is permitted and the last change.
pub fn lines(
    setting: Option<&StaleReferenceActionsPermittedSetting>,
    name_of: impl Fn(&str) -> String,
) -> String {
    let Some(setting) = setting else {
        return "<p class=\"warn\">The stale reference actions could not be read just now.</p>"
            .to_string();
    };
    let permitted = if setting.warn_permitted {
        "Refuse or warn"
    } else {
        "Refuse only"
    };
    let changed = if setting.changed_at_ns == 0 {
        "the default, never changed".to_string()
    } else {
        format!(
            "set {} by {}",
            escape(&crate::custody::utc(setting.changed_at_ns)),
            escape(&name_of(&setting.changed_by))
        )
    };
    format!(
        "<dl class=\"facts\" data-stale-reference=\"{}\"><dt>{LABEL}</dt><dd><strong>{permitted}\
         </strong> <span class=\"faint\">{changed}</span></dd></dl><p class=\"hint\">What an \
         execution plugin's admin may choose when a placement's reference price is past its \
         ceiling. A plugin set to warn while only refuse is permitted refuses.</p>",
        if setting.warn_permitted {
            "warn"
        } else {
            "refuse"
        },
    )
}

/// The dialog's fields: warn permitted or not, and why.
pub fn fields(setting: Option<&StaleReferenceActionsPermittedSetting>) -> String {
    let warn = setting.is_some_and(|setting| setting.warn_permitted);
    format!(
        "<label class=\"check\"><input type=\"checkbox\" name=\"warn_permitted\" value=\"on\"{}> \
         An execution plugin's admin may choose warn</label>\
         <label>Why<textarea name=\"note\" required maxlength=\"2000\" rows=\"2\"></textarea></label>",
        if warn { " checked" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tab_says_refuse_only_by_default_and_who_permitted_warn() {
        let default = StaleReferenceActionsPermittedSetting::default();
        let said = lines(Some(&default), |s| s.to_string());
        assert!(
            said.contains("Refuse only") && said.contains("never changed"),
            "{said}"
        );
        let changed = StaleReferenceActionsPermittedSetting {
            warn_permitted: true,
            changed_at_ns: 1_792_300_000_000_000_000,
            changed_by: "local|ada".into(),
        };
        let said = lines(Some(&changed), |_| "Ada".into());
        assert!(
            said.contains("Refuse or warn") && said.contains("by Ada"),
            "{said}"
        );
        assert!(fields(Some(&changed)).contains("checked"));
        assert_eq!(json(&changed)["actions"][1], "warn");
        assert!(lines(None, |s| s.to_string()).contains("could not be read"));
    }
}
