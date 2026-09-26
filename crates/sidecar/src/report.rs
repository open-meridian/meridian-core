//! The sidecar reports its plugin (W4.8).
//!
//! What the sidecar knows of its plugin, on the bus for the conductor and the
//! dashboard: registered or not, healthy or not with the plugin's own reason,
//! when it last said so, the contract version it registered with, and the
//! grants it was refused. The sidecar speaks for the plugin because it is the
//! one that sees refusals and silence, which the plugin cannot report about
//! itself.
//!
//! Sent when the sidecar starts, whenever the plugin registers or leaves, and
//! every 30 seconds between. The first is what makes a plugin known to the
//! conductor before anybody grants a person access to it: an access group
//! naming a plugin that has never reported is refused, since what it carries
//! is unknown.

use std::sync::Arc;
use std::time::Duration;

use meridian_domain::v1::PluginReport;
use prost::Message;

use crate::service::Sidecar;

pub const PLUGIN_REPORT: &str = "platform.deployment.event.plugin-report";
const EVERY: Duration = Duration::from_secs(30);

impl Sidecar {
    /// Count a refused grant and keep its reason.
    pub(crate) fn note_refusal(&self, reason: &str) {
        let mut refusals = self.refusals.lock().expect("refusal lock poisoned");
        refusals.0 += 1;
        refusals.1 = reason.to_string();
    }

    /// What the sidecar would say about its plugin now.
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
    loop {
        let report = sidecar.report(now_ns());
        if let Err(failed) = sidecar.bus.publish(
            PLUGIN_REPORT,
            "meridian.v1.PluginReport",
            report.encode_to_vec(),
            None,
            None,
        ) {
            tracing::debug!(%failed, "the plugin report was not published");
        }
        tokio::select! {
            _ = tokio::time::sleep(EVERY) => {}
            _ = sidecar.changed.notified() => {}
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
