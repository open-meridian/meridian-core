//! What a plugin learns from the deployment: its settings (W4.7), who may use
//! it (W4.10) and its account scope (W4.11).
//!
//! The settings and the scope are streams: sent at once, and again whenever
//! they change -- on the conductor's announcement, or at the latest when the
//! configuration is read again after 30 seconds. An item is sent only when it
//! differs from the last, so a plugin acts on changes rather than on ticks.
//! The access table is asked for when wanted: it shapes an interface and
//! decides nothing (requirement 22).

use std::pin::Pin;
use std::time::Duration;

use meridian_domain::v1::PluginConfiguration;
use meridian_pb::v1::{
    AccountScopeDelivery, PluginAccessReply, PluginAccessRequest, SettingDeclaration, SettingValue,
    SettingsDelivery,
};
use prost::Message;
use tokio_stream::Stream;
use tonic::Status;

use crate::configuration::{Configuration, REFRESH_NS};
use crate::service::Sidecar;
use crate::typed::{now_ns, refused};

pub const PLUGIN_ACCESS: &str = "platform.config.query.plugin-access";

pub(crate) type Following<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

/// `render` of the configuration, now and on every change to it. Ends with
/// the refusal when the configuration cannot be read any longer, and when the
/// plugin stops listening.
pub(crate) fn following<T>(
    configuration: Configuration,
    render: impl Fn(&PluginConfiguration) -> T + Send + 'static,
) -> Following<T>
where
    T: PartialEq + Clone + Send + 'static,
{
    let (sending, receiving) = tokio::sync::mpsc::channel(4);
    let mut changes = configuration.changes();
    tokio::spawn(async move {
        let mut last: Option<T> = None;
        loop {
            match configuration.current(now_ns()).await {
                Ok(current) => {
                    let item = render(&current);
                    if last.as_ref() != Some(&item) {
                        if sending.send(Ok(item.clone())).await.is_err() {
                            return;
                        }
                        last = Some(item);
                    }
                }
                Err(refusal) => {
                    let _ = sending.send(Err(refusal)).await;
                    return;
                }
            }
            tokio::select! {
                _ = changes.changed() => {}
                _ = tokio::time::sleep(Duration::from_nanos(REFRESH_NS as u64)) => {}
                _ = sending.closed() => return,
            }
        }
    });
    Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiving))
}

/// The values the deployment holds for the settings the plugin declared, and
/// the required ones it holds none for. A value for a setting the plugin did
/// not declare is not the plugin's to see.
pub(crate) fn settings(
    declared: &[SettingDeclaration],
    configuration: &PluginConfiguration,
) -> SettingsDelivery {
    let values: Vec<SettingValue> = declared
        .iter()
        .filter_map(|declaration| {
            configuration
                .settings
                .iter()
                .find(|held| held.name == declaration.name && !held.value.is_empty())
                .map(|held| SettingValue {
                    name: held.name.clone(),
                    value: held.value.clone(),
                })
        })
        .collect();
    let missing_required = declared
        .iter()
        .filter(|declaration| declaration.required)
        .filter(|declaration| !values.iter().any(|value| value.name == declaration.name))
        .map(|declaration| declaration.name.clone())
        .collect();
    SettingsDelivery {
        values,
        missing_required,
    }
}

pub(crate) fn scope(configuration: &PluginConfiguration) -> AccountScopeDelivery {
    AccountScopeDelivery {
        read_account_ids: configuration.read_account_ids.clone(),
        write_account_ids: configuration.write_account_ids.clone(),
    }
}

// Status is tonic's error throughout the operations; boxing it here alone
// would buy nothing.
#[allow(clippy::result_large_err)]
impl Sidecar {
    /// The access table, asked as this plugin.
    pub(crate) async fn access_table(&self) -> Result<PluginAccessReply, Status> {
        let (_, payload) = self
            .bus
            .call(
                PLUGIN_ACCESS,
                "meridian.v1.PluginAccessRequest",
                PluginAccessRequest {}.encode_to_vec(),
                None,
                Some(Duration::from_secs(5)),
            )
            .await
            .map_err(refused)?;
        PluginAccessReply::decode(payload.as_slice()).map_err(|failed| {
            Status::internal(format!("the access table did not decode: {failed}"))
        })
    }

    /// Required settings the plugin declared and the deployment holds no
    /// value for. Nothing, when the plugin is not registered or the
    /// configuration cannot be read: a report is not held up by either.
    pub(crate) async fn missing_settings(&self) -> Vec<String> {
        let Some(registration) = self.registration().filter(|r| !r.departed) else {
            return Vec::new();
        };
        if !registration.settings.iter().any(|s| s.required) {
            return Vec::new();
        }
        match self.configuration(now_ns()).await {
            Ok(configuration) => settings(&registration.settings, &configuration).missing_required,
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests;
