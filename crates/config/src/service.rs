//! Where the configuration store meets the bus: the `config` domain.
//!
//! Eleven commands and queries from the dashboard, two queries from sidecars,
//! two events heard, one announced. Every change is written the same way:
//! read a snapshot, check the rule, write, read again, and announce a change
//! to each plugin whose configuration differs between the two. A sidecar asks
//! again only when something it would be told has changed.
//!
//! # What a sidecar may ask
//!
//! Its own plugin's configuration and access table, and no other. The request
//! names no plugin: the answer is for the instance the envelope says published
//! the question, which the bus stamps and a plugin cannot forge. Secrets will
//! travel on that reply and nowhere else, because the broker narrows
//! publishing to an instance and not subscribing (the topic registry says why).
//!
//! # Who asked
//!
//! The dashboard calls on a person's behalf, and the envelope carries them as
//! `acting_for_subject`. A claim redemption needs it to know whom to make the
//! first deployment admin; every other change is logged with it.

use std::collections::BTreeSet;
use std::sync::Arc;

use meridian_bus::{Bus, Envelope};
use meridian_domain::v1::{
    AccessRecordsRequest, AccountRecord, AccountState, CloseAccountRequest,
    DefineAccessGroupRequest, DefineAccountGroupRequest, DefineAccountRequest,
    DefineUserGroupRequest, DiagnosticBundle, DiagnosticBundleReceipt, ExternalAccountLink,
    GrantPermissionRequest, LinkExternalAccountRequest, Permission, PluginConfiguration,
    PluginConfigurationChangedEvent, PluginConfigurationRequest, PluginReport,
    RedeemClaimCodeReply, RedeemClaimCodeRequest, SignInRecord, UserGroup, WithdrawPermissionReply,
    WithdrawPermissionRequest,
};
use meridian_pb::v1::PluginAccessRequest;
use prost::Message;

use crate::ids;
use crate::rules;
use crate::store::{KnownPlugin, Snapshot, Store, Withdrawal};
use crate::DEPLOYMENT_ADMIN;

pub const PERSON_SIGNED_IN: &str = "platform.config.event.person-signed-in";
pub const ACCESS_RECORDS: &str = "platform.config.query.access-records";
pub const REDEEM_CLAIM_CODE: &str = "platform.config.command.redeem-claim-code";
pub const DEFINE_ACCOUNT: &str = "platform.config.command.define-account";
pub const CLOSE_ACCOUNT: &str = "platform.config.command.close-account";
pub const LINK_EXTERNAL_ACCOUNT: &str = "platform.config.command.link-external-account";
pub const DEFINE_USER_GROUP: &str = "platform.config.command.define-user-group";
pub const DEFINE_ACCOUNT_GROUP: &str = "platform.config.command.define-account-group";
pub const DEFINE_ACCESS_GROUP: &str = "platform.config.command.define-access-group";
pub const GRANT_PERMISSION: &str = "platform.config.command.grant-permission";
pub const WITHDRAW_PERMISSION: &str = "platform.config.command.withdraw-permission";
pub const SEND_DIAGNOSTIC_BUNDLE: &str = "platform.config.command.send-diagnostic-bundle";
pub const PLUGIN_CONFIGURATION: &str = "platform.config.query.plugin-configuration";
pub const PLUGIN_CONFIGURATION_CHANGED: &str = "platform.config.event.plugin-configuration-changed";
pub const PLUGIN_ACCESS: &str = "platform.config.query.plugin-access";
pub const PLUGIN_REPORT: &str = "platform.deployment.event.plugin-report";

/// Where the time comes from, so a test does not wait for it.
pub trait Clock: Send + Sync {
    fn now_ns(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos() as i64)
            .unwrap_or_default()
    }
}

/// The platform, as far as the configuration store needs it: the two acts a
/// deployment admin can send outward. The conductor implements it with the
/// key it already holds; this crate never sees the key.
///
/// Called from a bus handler, which runs on a blocking thread, so these block.
pub trait Upstream: Send + Sync {
    /// W5.22. Carries the code and nothing about the person.
    fn honour_claim_code(&self, code: &str) -> Result<RedeemClaimCodeReply, String>;

    /// W5.23. Exactly what the deployment admin approved.
    fn submit_diagnostic_bundle(
        &self,
        bundle: &DiagnosticBundle,
    ) -> Result<DiagnosticBundleReceipt, String>;
}

/// The built-in access group, as the store seeds it.
pub fn deployment_admin() -> meridian_domain::v1::AccessGroup {
    meridian_domain::v1::AccessGroup {
        access_group_id: DEPLOYMENT_ADMIN.into(),
        name: "Deployment admin".into(),
        entries: Vec::new(),
        built_in: true,
    }
}

/// What a sidecar is told about its plugin, derived from one snapshot.
///
/// Settings are empty until the settings slice gives them somewhere to come
/// from (kernel/dashboard-health-settings-and-bundle).
pub fn configuration(snapshot: &Snapshot, plugin_instance_id: &str) -> PluginConfiguration {
    let scope = meridian_access::plugin_scope(&snapshot.records, plugin_instance_id);
    PluginConfiguration {
        plugin_instance_id: plugin_instance_id.to_string(),
        settings: Vec::new(),
        links: snapshot
            .links
            .iter()
            .filter(|link| link.plugin_instance_id == plugin_instance_id)
            .cloned()
            .collect(),
        read_account_ids: scope.read.into_iter().collect(),
        write_account_ids: scope.write.into_iter().collect(),
    }
}

/// Every plugin anything could be configured for.
fn plugins_in(snapshot: &Snapshot) -> BTreeSet<String> {
    let mut plugins: BTreeSet<String> = snapshot
        .plugins
        .iter()
        .map(|p| p.plugin_instance_id.clone())
        .collect();
    plugins.extend(snapshot.links.iter().map(|l| l.plugin_instance_id.clone()));
    for group in &snapshot.records.access_groups {
        plugins.extend(group.entries.iter().map(|e| e.plugin_instance_id.clone()));
    }
    plugins
}

/// The plugins whose configuration differs between two snapshots.
pub fn changed_plugins(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let mut all = plugins_in(before);
    all.extend(plugins_in(after));
    all.into_iter()
        .filter(|plugin| configuration(before, plugin) != configuration(after, plugin))
        .collect()
}

struct Context {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    upstream: Arc<dyn Upstream>,
}

impl Context {
    fn snapshot(&self) -> Result<Snapshot, String> {
        self.store.snapshot().map_err(|failed| failed.to_string())
    }

    /// Announce each plugin whose configuration a change moved. Carries no
    /// setting, so every sidecar may hear it.
    fn announce(&self, before: &Snapshot) -> Result<(), String> {
        let after = self.snapshot()?;
        for plugin in changed_plugins(before, &after) {
            let event = PluginConfigurationChangedEvent {
                plugin_instance_id: plugin,
                changed_at_ns: self.clock.now_ns(),
            };
            self.bus
                .publish(
                    PLUGIN_CONFIGURATION_CHANGED,
                    "meridian.v1.PluginConfigurationChangedEvent",
                    event.encode_to_vec(),
                    None,
                    None,
                )
                .map_err(|failed| failed.to_string())?;
        }
        Ok(())
    }
}

fn subject(envelope: &Envelope) -> String {
    envelope
        .meta
        .as_ref()
        .map(|meta| meta.acting_for_subject.clone())
        .unwrap_or_default()
}

fn publisher(envelope: &Envelope) -> String {
    envelope
        .meta
        .as_ref()
        .map(|meta| meta.publisher_instance_id.clone())
        .unwrap_or_default()
}

/// Register a handler that decodes one request type and encodes one reply.
fn answer<Req, Rep, F>(
    context: &Arc<Context>,
    topic: &'static str,
    types: (&'static str, &'static str),
    handle: F,
) where
    Req: Message + Default,
    Rep: Message,
    F: Fn(&Context, Req, &Envelope) -> Result<Rep, String> + Send + Sync + 'static,
{
    let (request_type, reply_type) = types;
    let context = Arc::clone(context);
    let bus = Arc::clone(&context.bus);
    bus.serve(topic, move |envelope| {
        if envelope.payload_type != request_type {
            return Err(format!(
                "{topic} expects {request_type}, and this is {}",
                envelope.payload_type
            ));
        }
        let request = Req::decode(&envelope.payload[..])
            .map_err(|failed| format!("undecodable {request_type}: {failed}"))?;
        let reply = handle(&context, request, &envelope)?;
        Ok((reply_type.to_string(), reply.encode_to_vec()))
    });
}

/// Register every handler the configuration store serves, and start listening
/// for the two events it keeps. Call inside a runtime.
pub fn serve(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    upstream: Arc<dyn Upstream>,
) {
    let context = Arc::new(Context {
        bus: Arc::clone(&bus),
        store,
        clock,
        upstream,
    });

    answer(
        &context,
        ACCESS_RECORDS,
        (
            "meridian.v1.AccessRecordsRequest",
            "meridian.v1.AccessRecords",
        ),
        |cx, _: AccessRecordsRequest, _| {
            let mut records = cx.snapshot()?.records;
            records.read_at_ns = cx.clock.now_ns();
            Ok(records)
        },
    );

    answer(
        &context,
        DEFINE_ACCOUNT,
        (
            "meridian.v1.DefineAccountRequest",
            "meridian.v1.AccountRecord",
        ),
        |cx, request: DefineAccountRequest, envelope| {
            let before = cx.snapshot()?;
            rules::define_account(&before, &request)?;
            let now = cx.clock.now_ns();
            let account = match before
                .records
                .accounts
                .iter()
                .find(|a| a.account_id == request.account_id)
            {
                Some(existing) => AccountRecord {
                    name: request.name.clone(),
                    ..existing.clone()
                },
                None => AccountRecord {
                    account_id: ids::account(now),
                    name: request.name.clone(),
                    state: AccountState::Open as i32,
                    created_at_ns: now,
                },
            };
            cx.store.put_account(&account).map_err(|f| f.to_string())?;
            tracing::info!(
                account = account.account_id,
                by = subject(envelope),
                "account defined"
            );
            cx.announce(&before)?;
            Ok(account)
        },
    );

    answer(
        &context,
        CLOSE_ACCOUNT,
        (
            "meridian.v1.CloseAccountRequest",
            "meridian.v1.AccountRecord",
        ),
        |cx, request: CloseAccountRequest, envelope| {
            let before = cx.snapshot()?;
            rules::close_account(&before, &request.account_id)?;
            let mut account = before
                .records
                .accounts
                .iter()
                .find(|a| a.account_id == request.account_id)
                .cloned()
                .expect("the rule checked it exists");
            account.state = AccountState::Closed as i32;
            cx.store.put_account(&account).map_err(|f| f.to_string())?;
            tracing::info!(
                account = account.account_id,
                by = subject(envelope),
                "account closed"
            );
            cx.announce(&before)?;
            Ok(account)
        },
    );

    answer(
        &context,
        LINK_EXTERNAL_ACCOUNT,
        (
            "meridian.v1.LinkExternalAccountRequest",
            "meridian.v1.ExternalAccountLink",
        ),
        |cx, request: LinkExternalAccountRequest, envelope| {
            let before = cx.snapshot()?;
            rules::link(&before, &request)?;
            let link = ExternalAccountLink {
                plugin_instance_id: request.plugin_instance_id,
                external_account_id: request.external_account_id,
                account_id: request.account_id,
            };
            cx.store.put_link(&link).map_err(|f| f.to_string())?;
            tracing::info!(
                plugin = link.plugin_instance_id,
                external = link.external_account_id,
                account = link.account_id,
                by = subject(envelope),
                "external account link set"
            );
            cx.announce(&before)?;
            Ok(link)
        },
    );

    answer(
        &context,
        DEFINE_USER_GROUP,
        (
            "meridian.v1.DefineUserGroupRequest",
            "meridian.v1.UserGroup",
        ),
        |cx, request: DefineUserGroupRequest, envelope| {
            let before = cx.snapshot()?;
            let mut group = request.user_group.unwrap_or_default();
            rules::user_group(&before, &group)?;
            if group.user_group_id.is_empty() {
                group.user_group_id = ids::user_group(cx.clock.now_ns());
            }
            cx.store.put_user_group(&group).map_err(|f| f.to_string())?;
            tracing::info!(
                user_group = group.user_group_id,
                by = subject(envelope),
                "user group defined"
            );
            cx.announce(&before)?;
            Ok(group)
        },
    );

    answer(
        &context,
        DEFINE_ACCOUNT_GROUP,
        (
            "meridian.v1.DefineAccountGroupRequest",
            "meridian.v1.AccountGroup",
        ),
        |cx, request: DefineAccountGroupRequest, envelope| {
            let before = cx.snapshot()?;
            let mut group = request.account_group.unwrap_or_default();
            rules::account_group(&before, &group)?;
            if group.account_group_id.is_empty() {
                group.account_group_id = ids::account_group(cx.clock.now_ns());
            }
            cx.store
                .put_account_group(&group)
                .map_err(|f| f.to_string())?;
            tracing::info!(
                account_group = group.account_group_id,
                by = subject(envelope),
                "account group defined"
            );
            cx.announce(&before)?;
            Ok(group)
        },
    );

    answer(
        &context,
        DEFINE_ACCESS_GROUP,
        (
            "meridian.v1.DefineAccessGroupRequest",
            "meridian.v1.AccessGroup",
        ),
        |cx, request: DefineAccessGroupRequest, envelope| {
            let before = cx.snapshot()?;
            let mut group = request.access_group.unwrap_or_default();
            rules::access_group(&before, &group)?;
            if group.access_group_id.is_empty() {
                group.access_group_id = ids::access_group(cx.clock.now_ns());
            }
            cx.store
                .put_access_group(&group)
                .map_err(|f| f.to_string())?;
            tracing::info!(
                access_group = group.access_group_id,
                by = subject(envelope),
                "access group defined"
            );
            cx.announce(&before)?;
            Ok(group)
        },
    );

    answer(
        &context,
        GRANT_PERMISSION,
        (
            "meridian.v1.GrantPermissionRequest",
            "meridian.v1.Permission",
        ),
        |cx, request: GrantPermissionRequest, envelope| {
            let before = cx.snapshot()?;
            rules::grant(&before, &request)?;
            let permission = Permission {
                permission_id: ids::permission(cx.clock.now_ns()),
                user_group_id: request.user_group_id,
                account_group_id: request.account_group_id,
                access_group_id: request.access_group_id,
            };
            cx.store
                .add_permission(&permission)
                .map_err(|f| f.to_string())?;
            tracing::info!(
                permission = permission.permission_id,
                by = subject(envelope),
                "permission granted"
            );
            cx.announce(&before)?;
            Ok(permission)
        },
    );

    answer(
        &context,
        WITHDRAW_PERMISSION,
        (
            "meridian.v1.WithdrawPermissionRequest",
            "meridian.v1.WithdrawPermissionReply",
        ),
        |cx, request: WithdrawPermissionRequest, envelope| {
            let before = cx.snapshot()?;
            let outcome = cx
                .store
                .withdraw_permission(&request.permission_id)
                .map_err(|f| f.to_string())?;
            let reply = match outcome {
                Withdrawal::Withdrawn => {
                    tracing::info!(
                        permission = request.permission_id,
                        by = subject(envelope),
                        "permission withdrawn"
                    );
                    cx.announce(&before)?;
                    WithdrawPermissionReply {
                        withdrawn: true,
                        refusal_reason: String::new(),
                    }
                }
                Withdrawal::Unknown => WithdrawPermissionReply {
                    withdrawn: false,
                    refusal_reason: format!("there is no permission {}", request.permission_id),
                },
                Withdrawal::LastAdmin => WithdrawPermissionReply {
                    withdrawn: false,
                    refusal_reason: "the last permission to deployment admin".into(),
                },
            };
            Ok(reply)
        },
    );

    answer(
        &context,
        REDEEM_CLAIM_CODE,
        (
            "meridian.v1.RedeemClaimCodeRequest",
            "meridian.v1.RedeemClaimCodeReply",
        ),
        |cx, request: RedeemClaimCodeRequest, envelope| {
            let redeemer = subject(envelope);
            let refused = |reason: &str| RedeemClaimCodeReply {
                redeemed: false,
                refusal_reason: reason.to_string(),
            };
            if redeemer.is_empty() {
                return Ok(refused("a claim code is redeemed by somebody signed in"));
            }
            let has_admin = |snapshot: &Snapshot| {
                snapshot
                    .records
                    .permissions
                    .iter()
                    .any(|p| p.access_group_id == DEPLOYMENT_ADMIN)
            };
            // Refused here, before the platform is asked, so a code is not
            // spent on a deployment that cannot use it.
            if has_admin(&cx.snapshot()?) {
                return Ok(refused("this deployment already has a deployment admin"));
            }

            let answered = cx.upstream.honour_claim_code(&request.code)?;
            if !answered.redeemed {
                return Ok(answered);
            }

            let now = cx.clock.now_ns();
            let group = UserGroup {
                user_group_id: ids::user_group(now),
                name: "Deployment admins".into(),
                directory_groups: Vec::new(),
                logins: vec![redeemer.clone()],
            };
            let permission = Permission {
                permission_id: ids::permission(now),
                user_group_id: group.user_group_id.clone(),
                account_group_id: String::new(),
                access_group_id: DEPLOYMENT_ADMIN.into(),
            };
            let installed = cx
                .store
                .install_first_admin(&group, &permission)
                .map_err(|f| f.to_string())?;
            if !installed {
                // Another redemption won between the check and now. The code
                // is spent at the platform either way; saying so is better
                // than pretending this one worked.
                return Ok(refused("this deployment already has a deployment admin"));
            }
            tracing::info!(
                by = redeemer,
                "the first deployment admin redeemed a claim code"
            );
            Ok(answered)
        },
    );

    answer(
        &context,
        SEND_DIAGNOSTIC_BUNDLE,
        (
            "meridian.v1.DiagnosticBundle",
            "meridian.v1.DiagnosticBundleReceipt",
        ),
        |cx, bundle: DiagnosticBundle, envelope| {
            // Forwarded exactly as approved. Nothing is added here, including
            // who sent it, which the platform has no need to know.
            let receipt = cx.upstream.submit_diagnostic_bundle(&bundle)?;
            tracing::info!(
                bundle = bundle.bundle_id,
                by = subject(envelope),
                "diagnostic bundle sent"
            );
            Ok(receipt)
        },
    );

    answer(
        &context,
        PLUGIN_CONFIGURATION,
        (
            "meridian.v1.PluginConfigurationRequest",
            "meridian.v1.PluginConfiguration",
        ),
        |cx, _: PluginConfigurationRequest, envelope| {
            let plugin = publisher(envelope);
            Ok(configuration(&cx.snapshot()?, &plugin))
        },
    );

    answer(
        &context,
        PLUGIN_ACCESS,
        (
            "meridian.v1.PluginAccessRequest",
            "meridian.v1.PluginAccessReply",
        ),
        |cx, _: PluginAccessRequest, envelope| {
            let plugin = publisher(envelope);
            Ok(meridian_access::plugin_access_table(
                &cx.snapshot()?.records,
                &plugin,
            ))
        },
    );

    // Subscribed before this returns, for the reason at-most-once delivery
    // makes unforgiving: what arrives before a subscriber exists is dropped.
    //
    // The store is synchronous, and the Postgres one drives its own runtime
    // underneath, so each write goes to the blocking pool as the handlers'
    // do. Called on this task instead, the first sign-in panicked the
    // conductor with "Cannot start a runtime from within a runtime", and the
    // panic aborted it.
    let mut sign_ins = bus.subscribe(PERSON_SIGNED_IN);
    let keeping = Arc::clone(&context);
    tokio::spawn(async move {
        while let Some(delivery) = sign_ins.recv().await {
            match SignInRecord::decode(&delivery.envelope.payload[..]) {
                Ok(record) => {
                    let store = Arc::clone(&keeping.store);
                    match tokio::task::spawn_blocking(move || store.record_sign_in(&record)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(failed)) => tracing::warn!("a sign-in was not recorded: {failed}"),
                        Err(failed) => tracing::warn!("a sign-in was not recorded: {failed}"),
                    }
                }
                Err(failed) => tracing::warn!("a sign-in record did not decode: {failed}"),
            }
        }
    });

    let mut reports = bus.subscribe(PLUGIN_REPORT);
    let noting = Arc::clone(&context);
    tokio::spawn(async move {
        while let Some(delivery) = reports.recv().await {
            let Ok(report) = PluginReport::decode(&delivery.envelope.payload[..]) else {
                tracing::warn!("a plugin report did not decode");
                continue;
            };
            let plugin = KnownPlugin {
                plugin_instance_id: report.plugin_instance_id,
                role: report.role,
                tags: report.tags,
                last_reported_at_ns: report.reported_at_ns,
            };
            let store = Arc::clone(&noting.store);
            match tokio::task::spawn_blocking(move || store.record_plugin(&plugin)).await {
                Ok(Ok(())) => {}
                Ok(Err(failed)) => tracing::warn!("a plugin report was not kept: {failed}"),
                Err(failed) => tracing::warn!("a plugin report was not kept: {failed}"),
            }
        }
    });
}
