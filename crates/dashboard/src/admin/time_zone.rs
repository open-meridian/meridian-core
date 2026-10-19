//! The deployment's time zone, on its Settings (contract v26, W6.35; the
//! v26 plan's questions 229 and 230): a deployment admin's, an IANA
//! time-zone ID, `America/New_York` until set; each change its own record
//! naming who and why (decisions/031). The tab says the zone in force, its
//! last change, and the affirmation deadline the launched `operations`
//! instance declares, which a held booking is due by on its trade date in
//! this zone (W11.34). Under parity `dashboard__read_deployment_time_zone`
//! and `dashboard__set_deployment_time_zone`.

use std::time::Duration;

use meridian_domain::v1::{DeploymentTimeZoneSetting, ReadDeploymentTimeZoneRequest};
use prost::Message;
use serde_json::{json, Value};

use crate::html::escape;

/// The zone a deployment holds until one is set (US first).
pub const DEFAULT: &str = "America/New_York";

pub const SET_DEPLOYMENT_TIME_ZONE: &str = "platform.config.command.set-deployment-time-zone";
pub const READ_DEPLOYMENT_TIME_ZONE: &str = "platform.config.query.read-deployment-time-zone";

/// Where the tab's form posts.
pub const PATH: &str = "/admin/time-zone";

/// The setting's label (W6.35).
pub const LABEL: &str = "Deployment time zone";

/// The setting in force, from the conductor.
pub async fn read(bus: &meridian_bus::Bus) -> Result<DeploymentTimeZoneSetting, String> {
    let (_, bytes) = bus
        .call(
            READ_DEPLOYMENT_TIME_ZONE,
            "meridian.v1.ReadDeploymentTimeZoneRequest",
            ReadDeploymentTimeZoneRequest {}.encode_to_vec(),
            None,
            Some(Duration::from_secs(3)),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    DeploymentTimeZoneSetting::decode(&bytes[..])
        .map_err(|failed| format!("the conductor's answer did not read: {failed}"))
}

/// The setting as a tool answers it.
pub fn json(setting: &DeploymentTimeZoneSetting) -> Value {
    json!({
        "deployment_time_zone": setting.deployment_time_zone,
        "label": LABEL,
        "changed_at_ns": setting.changed_at_ns.to_string(),
        "changed_by": setting.changed_by,
        "affirmation_deadline": setting.affirmation_deadline,
        "affirmation_deadline_instance_id": setting.affirmation_deadline_instance_id,
    })
}

/// Minutes after midnight as a clock reads them, `21:00`; 1440 is `24:00`.
pub fn clock(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// The tab's lines: the zone, its last change, and the deadline.
pub fn lines(
    setting: Option<&DeploymentTimeZoneSetting>,
    name_of: impl Fn(&str) -> String,
) -> String {
    let Some(setting) = setting else {
        return "<p class=\"warn\">The deployment's time zone could not be read just now.</p>"
            .to_string();
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
    let deadline = if setting.affirmation_deadline == 0 {
        "No launched operations plugin declares an affirmation deadline: a held booking names \
         no time it is due by."
            .to_string()
    } else {
        format!(
            "A held booking is due by {} on its trade date in this zone, the affirmation \
             deadline <code>{}</code> declares.",
            clock(setting.affirmation_deadline),
            escape(&setting.affirmation_deadline_instance_id)
        )
    };
    format!(
        "<dl class=\"facts\" data-time-zone=\"{zone}\"><dt>{LABEL}</dt><dd><strong>{zone}</strong> \
         <span class=\"faint\">{changed}</span></dd></dl><p class=\"hint\">{deadline}</p>",
        zone = escape(&setting.deployment_time_zone),
    )
}

/// The dialog's fields: the zone, an IANA ID the conductor checks, and why.
pub fn fields(setting: Option<&DeploymentTimeZoneSetting>) -> String {
    let zone = setting
        .map(|s| s.deployment_time_zone.as_str())
        .filter(|zone| !zone.is_empty())
        .unwrap_or(DEFAULT);
    format!(
        "<label>{LABEL}<input name=\"deployment_time_zone\" value=\"{z}\" required maxlength=\"64\" \
         autocomplete=\"off\" spellcheck=\"false\"></label>\
         <p class=\"hint\">An IANA time-zone ID: America/New_York, Europe/London, Asia/Tokyo. It \
         moves when a held booking is due and the times shown here; never a market's own cut, \
         such as FX's 17:00 New York.</p>\
         <label>Why<textarea name=\"note\" required maxlength=\"2000\" rows=\"2\"></textarea></label>",
        z = escape(zone)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tab_says_the_zone_its_change_and_the_deadline() {
        let held = DeploymentTimeZoneSetting {
            deployment_time_zone: "Europe/London".into(),
            changed_at_ns: 1_792_300_000_000_000_000,
            changed_by: "local|ada".into(),
            affirmation_deadline: 21 * 60,
            affirmation_deadline_instance_id: "operations-1".into(),
        };
        let said = lines(Some(&held), |_| "Ada".into());
        assert!(said.contains("<strong>Europe/London</strong>"), "{said}");
        assert!(said.contains("by Ada"), "{said}");
        assert!(
            said.contains("due by 21:00") && said.contains("operations-1"),
            "{said}"
        );
        let default = lines(
            Some(&DeploymentTimeZoneSetting {
                deployment_time_zone: DEFAULT.into(),
                ..Default::default()
            }),
            |s| s.to_string(),
        );
        assert!(default.contains("never changed") && default.contains("names no time"));
        assert!(lines(None, |s| s.to_string()).contains("could not be read"));
        assert_eq!(json(&held)["changed_at_ns"], "1792300000000000000");
        assert_eq!(clock(1440), "24:00");
    }
}
