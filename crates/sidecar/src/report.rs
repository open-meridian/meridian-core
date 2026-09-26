//! The sidecar reports its plugin (W4.8).
//!
//! What the sidecar knows of its plugin, on the bus for the conductor and the
//! dashboard: registered or not, healthy or not with the plugin's own reason,
//! when it last said so, the contract version it registered with, and the
//! grants it was refused. The sidecar speaks for the plugin because it is the
//! one that sees refusals and silence, which the plugin cannot report about
//! itself.
//!
//! A plugin missing a required setting it declared is reported unhealthy with
//! that reason, whatever it says of itself (W4.7). Beside the report, the
//! external accounts it sent rows for that nobody has linked, for the
//! dashboard to show a deployment admin (W4.8, W6.4).
//!
//! Sent when the sidecar starts, whenever the plugin registers or leaves or
//! its configuration changes, and every 30 seconds between. The first is what makes a plugin known to the
//! conductor before anybody grants a person access to it: an access group
//! naming a plugin that has never reported is refused, since what it carries
//! is unknown.

use std::sync::Arc;
use std::time::Duration;

use meridian_domain::v1::{PluginReport, UnlinkedExternalAccount, UnlinkedExternalAccountsEvent};
use prost::Message;

use crate::service::Sidecar;

pub const PLUGIN_REPORT: &str = "platform.deployment.event.plugin-report";
pub const UNLINKED_EXTERNAL_ACCOUNTS: &str = "platform.config.event.unlinked-external-accounts";
const EVERY: Duration = Duration::from_secs(30);

impl Sidecar {
    /// Count a refused grant and keep its reason.
    pub(crate) fn note_refusal(&self, reason: &str) {
        let mut refusals = self.refusals.lock().expect("refusal lock poisoned");
        refusals.0 += 1;
        refusals.1 = reason.to_string();
    }

    /// The report, with the plugin unhealthy while a required setting it
    /// declared has no value.
    pub(crate) async fn report_now(&self, now_ns: i64) -> PluginReport {
        let mut report = self.report(now_ns);
        let missing = self.missing_settings().await;
        if report.registered && !missing.is_empty() {
            report.healthy = false;
            report.health_detail = match missing.as_slice() {
                [one] => format!("required setting {one} is not set"),
                many => format!("required settings {} are not set", many.join(", ")),
            };
        }
        report
    }

    /// The external accounts refused for want of a link, if any.
    pub(crate) fn unlinked_now(&self) -> Option<UnlinkedExternalAccountsEvent> {
        let unlinked = self.unlinked.lock().expect("unlinked lock poisoned");
        (!unlinked.is_empty()).then(|| UnlinkedExternalAccountsEvent {
            plugin_instance_id: self.identity.instance_id.clone(),
            accounts: unlinked
                .iter()
                .map(|(external_account_id, seen)| UnlinkedExternalAccount {
                    external_account_id: external_account_id.clone(),
                    refused_rows: seen.refused_rows,
                    first_seen_at_ns: seen.first_seen_at_ns,
                    last_seen_at_ns: seen.last_seen_at_ns,
                })
                .collect(),
        })
    }

    /// What the sidecar would say about its plugin now, from what it has
    /// seen; the settings it asks the configuration for are added by
    /// `report_now`.
    pub fn report(&self, now_ns: i64) -> PluginReport {
        let (refused_grants, last_refusal_reason) =
            self.refusals.lock().expect("refusal lock poisoned").clone();
        let registration = self.registration().filter(|r| !r.departed);
        PluginReport {
            plugin_instance_id: self.identity.instance_id.clone(),
            roles: self.identity.roles.clone(),
            tags: self.identity.tags.clone(),
            registered: registration.is_some(),
            healthy: registration.as_ref().is_some_and(|r| r.healthy),
            health_detail: registration
                .as_ref()
                .map(|r| r.health_detail.clone())
                .unwrap_or_default(),
            last_heartbeat_at_ns: registration
                .as_ref()
                .map(|r| r.last_heartbeat_ns)
                .unwrap_or_default(),
            contract_version: registration
                .as_ref()
                .map(|r| r.contract_version.clone())
                .unwrap_or_default(),
            refused_grants,
            last_refusal_reason,
            reported_at_ns: now_ns,
        }
    }
}

/// For as long as the process runs. A report that cannot be published is
/// logged at debug and the next one tried: the plugin still runs, and the
/// conductor shows an age rather than inferring failure from one silence.
pub async fn report_forever(sidecar: Arc<Sidecar>) {
    if sidecar.identity.instance_id.is_empty() {
        return;
    }
    let mut configuration = sidecar.configuration.changes();
    loop {
        let report = sidecar.report_now(now_ns()).await;
        if let Err(failed) = sidecar.bus.publish(
            PLUGIN_REPORT,
            "meridian.v1.PluginReport",
            report.encode_to_vec(),
            None,
            None,
        ) {
            tracing::debug!(%failed, "the plugin report was not published");
        }
        if let Some(unlinked) = sidecar.unlinked_now() {
            if let Err(failed) = sidecar.bus.publish(
                UNLINKED_EXTERNAL_ACCOUNTS,
                "meridian.v1.UnlinkedExternalAccountsEvent",
                unlinked.encode_to_vec(),
                None,
                None,
            ) {
                tracing::debug!(%failed, "the unlinked external accounts were not published");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(EVERY) => {}
            _ = sidecar.changed.notified() => {}
            _ = configuration.changed() => {}
        }
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
