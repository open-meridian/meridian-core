//! The plugin's write scope: every account some person may write through it
//! (spec/deployment-dashboard-and-access, requirement 20).
//!
//! Derived by the conductor from permissions, never declared, and read here
//! from the plugin's access table (W4.10) as this instance. A command naming
//! an account outside it is refused, whoever it is sent for: the plugin acting
//! as itself included, since a link says which account a row belongs to and
//! not that anybody may write it through this plugin.
//!
//! Read again when it is 30 seconds old, and used as last read for up to 10
//! minutes when the conductor cannot be reached; past that, refused rather
//! than trusted. Requirement 17's two bounds, as the dashboard keeps them.

use std::collections::BTreeSet;
use std::sync::Arc;

use meridian_pb::v1::{PluginAccessReply, PluginAccessRequest, TagAccess};
use prost::Message;
use tokio::sync::Mutex;
use tonic::Status;

use crate::service::Sidecar;

pub const PLUGIN_ACCESS: &str = "platform.config.query.plugin-access";
const SECOND_NS: i64 = 1_000_000_000;
const REFRESH_NS: i64 = 30 * SECOND_NS;
const CEILING_NS: i64 = 10 * 60 * SECOND_NS;

/// The accounts, and when they were read.
type Read = (BTreeSet<String>, i64);

#[derive(Clone, Default)]
pub struct Scope {
    held: Arc<Mutex<Option<Read>>>,
}

/// Every account a set of tag access lets somebody write.
pub(crate) fn writes<'a>(access: impl IntoIterator<Item = &'a TagAccess>) -> BTreeSet<String> {
    access
        .into_iter()
        .flat_map(|held| held.write_account_ids.iter().cloned())
        .collect()
}

// Status is tonic's error throughout the operations; boxing it here alone
// would buy nothing.
#[allow(clippy::result_large_err)]
impl Sidecar {
    /// The accounts this plugin may write, as of at most 30 seconds ago.
    pub(crate) async fn write_scope(&self, now_ns: i64) -> Result<BTreeSet<String>, Status> {
        let mut held = self.scope.held.lock().await;
        if let Some((accounts, read_at)) = held.as_ref() {
            if now_ns - read_at <= REFRESH_NS {
                return Ok(accounts.clone());
            }
        }
        match self.read_scope().await {
            Ok(accounts) => {
                *held = Some((accounts.clone(), now_ns));
                Ok(accounts)
            }
            Err(failed) => match held.as_ref() {
                Some((accounts, read_at)) if now_ns - read_at <= CEILING_NS => {
                    tracing::warn!("the write scope could not be read again: {failed}");
                    Ok(accounts.clone())
                }
                _ => Err(Status::unavailable(format!(
                    "this plugin's write scope cannot be read: {failed}"
                ))),
            },
        }
    }

    async fn read_scope(&self) -> Result<BTreeSet<String>, String> {
        let (_, payload) = self
            .bus
            .call(
                PLUGIN_ACCESS,
                "meridian.v1.PluginAccessRequest",
                PluginAccessRequest {}.encode_to_vec(),
                None,
                Some(std::time::Duration::from_secs(5)),
            )
            .await
            .map_err(|failed| failed.to_string())?;
        let table = PluginAccessReply::decode(payload.as_slice())
            .map_err(|failed| format!("the access table did not decode: {failed}"))?;
        // Every user group naming the plugin, which is every way anybody
        // reaches it: a person's access here is their groups'.
        Ok(writes(table.user_groups.iter().flat_map(|g| &g.access)))
    }
}
