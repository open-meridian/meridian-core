//! The plugin's configuration, as the conductor last said it (W4.7).
//!
//! One answer carries everything the sidecar needs to know about its plugin
//! from the deployment: the settings' values, the external accounts linked to
//! core accounts (W6.4), and the plugin's account scope -- every account some
//! person may read or write through it, derived by the conductor from
//! permissions and never declared (spec/deployment-dashboard-and-access,
//! requirement 20).
//!
//! Asked as this instance, since the conductor answers for the instance the
//! envelope names. Forgotten when the conductor announces a change to it, and
//! read again when 30 seconds old whatever was announced, since an
//! announcement can be lost; used as last read for up to 10 minutes when the
//! conductor cannot answer, and refused past that. Requirement 17's bounds,
//! as the dashboard keeps them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::v1::{
    PluginConfiguration, PluginConfigurationChangedEvent, PluginConfigurationRequest,
};
use prost::Message;
use tokio::sync::{watch, Mutex};
use tonic::Status;

use crate::service::Sidecar;

pub const PLUGIN_CONFIGURATION: &str = "platform.config.query.plugin-configuration";
pub const PLUGIN_CONFIGURATION_CHANGED: &str = "platform.config.event.plugin-configuration-changed";

const SECOND_NS: i64 = 1_000_000_000;
pub(crate) const REFRESH_NS: i64 = 30 * SECOND_NS;
const CEILING_NS: i64 = 10 * 60 * SECOND_NS;

/// The configuration, and when it was read.
type Read = (PluginConfiguration, i64);

/// Cheap to clone: every clone is the same cache, so a stream holds one.
#[derive(Clone)]
pub struct Configuration {
    bus: Arc<Bus>,
    instance: String,
    held: Arc<Mutex<Option<Read>>>,
    watching: Arc<AtomicBool>,
    /// Bumped whenever the conductor says the configuration changed, so a
    /// stream waiting on it reads again at once.
    changed: Arc<watch::Sender<u64>>,
}

// Status is tonic's error throughout the operations; boxing it here alone
// would buy nothing.
#[allow(clippy::result_large_err)]
impl Configuration {
    pub(crate) fn new(bus: Arc<Bus>, instance: String) -> Configuration {
        Configuration {
            bus,
            instance,
            held: Arc::default(),
            watching: Arc::default(),
            changed: Arc::new(watch::channel(0).0),
        }
    }

    /// Woken on each announced change. Taken before reading, so a change
    /// between the two is not missed.
    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.watch();
        self.changed.subscribe()
    }

    fn watch(&self) {
        // Once, however often it is read again.
        if !self.watching.swap(true, Ordering::SeqCst) {
            self.forget_on_change();
        }
    }

    /// This plugin's configuration, as of at most 30 seconds ago, or since
    /// the conductor last announced a change.
    pub(crate) async fn current(&self, now_ns: i64) -> Result<PluginConfiguration, Status> {
        self.watch();
        let mut held = self.held.lock().await;
        if let Some((configuration, read_at)) = held.as_ref() {
            if now_ns - read_at <= REFRESH_NS {
                return Ok(configuration.clone());
            }
        }
        match self.read().await {
            Ok(configuration) => {
                *held = Some((configuration.clone(), now_ns));
                Ok(configuration)
            }
            Err(failed) => match held.as_ref() {
                Some((configuration, read_at)) if now_ns - read_at <= CEILING_NS => {
                    tracing::warn!("the plugin's configuration could not be read again: {failed}");
                    Ok(configuration.clone())
                }
                _ => Err(failed),
            },
        }
    }

    async fn read(&self) -> Result<PluginConfiguration, Status> {
        let (_, payload) = self
            .bus
            .call(
                PLUGIN_CONFIGURATION,
                "meridian.v1.PluginConfigurationRequest",
                PluginConfigurationRequest {}.encode_to_vec(),
                None,
                Some(Duration::from_secs(5)),
            )
            .await
            .map_err(crate::typed::refused)?;
        let mut configuration =
            PluginConfiguration::decode(payload.as_slice()).map_err(|failed| {
                Status::internal(format!(
                    "the plugin's configuration did not decode: {failed}"
                ))
            })?;
        // Only this plugin's links, whatever else an answer carries.
        configuration
            .links
            .retain(|link| link.plugin_instance_id == self.instance);
        Ok(configuration)
    }

    /// Forget the configuration whenever the conductor says this plugin's
    /// changed, and wake whatever streams it.
    fn forget_on_change(&self) {
        let mut changes = self.bus.subscribe(PLUGIN_CONFIGURATION_CHANGED);
        let configuration = self.clone();
        let instance = self.instance.clone();
        tokio::spawn(async move {
            while let Some(delivery) = changes.recv().await {
                let changed =
                    PluginConfigurationChangedEvent::decode(&delivery.envelope.payload[..]);
                if matches!(changed, Ok(event) if event.plugin_instance_id == instance) {
                    *configuration.held.lock().await = None;
                    configuration.changed.send_modify(|seen| *seen += 1);
                }
            }
        });
    }
}

#[allow(clippy::result_large_err)]
impl Sidecar {
    pub(crate) async fn configuration(&self, now_ns: i64) -> Result<PluginConfiguration, Status> {
        self.configuration.current(now_ns).await
    }

    /// The account an external account is linked to (W6.4), or the refusal
    /// that names what to do: a row for an unlinked account is refused, not
    /// guessed at, and recorded once somebody links it.
    pub(crate) async fn linked_account(&self, external_account_id: &str) -> Result<String, Status> {
        if external_account_id.is_empty() {
            return Err(Status::invalid_argument(
                "external_account_id is required: the account as the rail knows it",
            ));
        }
        let configuration = self.configuration(crate::typed::now_ns()).await?;
        let linked = configuration
            .links
            .iter()
            .find(|link| link.external_account_id == external_account_id)
            .map(|link| link.account_id.clone());
        let mut unlinked = self.unlinked.lock().expect("unlinked lock poisoned");
        match linked {
            Some(account) => {
                unlinked.remove(external_account_id);
                Ok(account)
            }
            None => {
                let now = crate::typed::now_ns();
                let seen = unlinked.entry(external_account_id.to_string()).or_insert(
                    crate::service::Unlinked {
                        first_seen_at_ns: now,
                        ..Default::default()
                    },
                );
                seen.refused_rows += 1;
                seen.last_seen_at_ns = now;
                drop(unlinked);
                self.changed.notify_one();
                Err(Status::failed_precondition(format!(
                    "external account {external_account_id} is not linked to an account; a \
                     deployment admin links it (W6.4), and the next statement records it"
                )))
            }
        }
    }
}
