//! Where the configuration store meets the bus: the `config` domain.
//!
//! Eleven commands and queries from the dashboard, two queries from sidecars,
//! a command and a query from a plugin acting for its admin, two events
//! heard, one announced. Every change is written the same way:
//! read a snapshot, check the rule, write, read again, and announce a change
//! to each plugin whose configuration differs between the two. A sidecar asks
//! again only when something it would be told has changed.
//!
//! # What a sidecar may ask
//!
//! Its own plugin's configuration and access table, and no other. The request
//! names no plugin: the answer is for the instance the envelope says published
//! the question, which the bus stamps and a plugin cannot forge. Secrets
//! travel on that reply and nowhere else, because the broker narrows
//! publishing to an instance and not subscribing (the topic registry says why).
//!
//! # Settings
//!
//! Checked against what the plugin declared at registration, which its
//! sidecar's report carries here (W4.8). A secret is sealed with the
//! deployment's settings key before it is written, and opened only to answer
//! that plugin's sidecar ([`crate::sealing`]). What the dashboard is told of a
//! secret is that it is set; no reply, log line or record here carries its
//! value. Each change is its own record (decisions/031): which setting, set
//! or cleared, the value of one that is not secret, who, through which
//! delegation, and when; the settings record names who made its latest
//! change (`updated_by`). Only the dashboard's form sets a setting: a plugin
//! reads its settings and sets none (W6.11, option A). A table setting's
//! rows are checked cell by cell, and each row added or changed is stamped
//! with `changed_by` and `changed_at` ([`meridian_domain::setting_table`]).
//!
//! # Who asked
//!
//! The dashboard calls on a person's behalf, and the envelope carries them as
//! `acting_for_subject`. A claim redemption needs it to know whom to make the
//! first deployment admin; every other change is logged with it.
//!
//! # A plugin's own external accounts
//!
//! A plugin links its external accounts from one of its pages at `admin`, and
//! reads the deployment's accounts to offer, each acting for the admin of the
//! plugin viewing it (W6.4). Its sidecar admits either only with an assertion
//! whose level is `admin`, a session opened by Manage, a new account only for
//! a deployment admin, and stamps the person; here, a link or a read naming
//! nobody is refused, a plugin links only its own external accounts, and a
//! link naming a new account creates and links it in one step.

use std::collections::BTreeSet;
use std::sync::Arc;

use meridian_bus::{Bus, Envelope};
use meridian_domain::setting_table;
use meridian_domain::v1::{
    AccessRecordsRequest, AccountRecord, AccountState, Accounts, AccountsRequest, ClaimCodePurpose,
    CloseAccountRequest, DefineAccessGroupRequest, DefineAccountGroupRequest, DefineAccountRequest,
    DefineUserGroupRequest, DiagnosticBundle, DiagnosticBundleReceipt, ExternalAccountLink,
    GrantPermissionRequest, LinkExternalAccountRequest, Permission, PluginConfiguration,
    PluginConfigurationChangedEvent, PluginConfigurationRequest, PluginReport, PluginSettingValue,
    PluginSettingsRecord, RedeemClaimCodeReply, RedeemClaimCodeRequest, SetPluginSettingsRequest,
    SignInRecord, UserGroup, WithdrawPermissionReply, WithdrawPermissionRequest,
};
use meridian_pb::v1::PluginAccessRequest;
use prost::Message;

use crate::ids;
use crate::rules;
use crate::sealing::SettingsKey;
use crate::store::{
    Held, KnownPlugin, SettingChange, SettingsAuthor, Snapshot, Store, StoredSetting, Withdrawal,
};
use crate::DEPLOYMENT_ADMIN;

pub const PERSON_SIGNED_IN: &str = "platform.config.event.person-signed-in";
pub const ACCESS_RECORDS: &str = "platform.config.query.access-records";
pub const REDEEM_CLAIM_CODE: &str = "platform.config.command.redeem-claim-code";
pub const DEFINE_ACCOUNT: &str = "platform.config.command.define-account";
pub const CLOSE_ACCOUNT: &str = "platform.config.command.close-account";
pub const LINK_EXTERNAL_ACCOUNT: &str = "platform.config.command.link-external-account";
pub const ACCOUNTS: &str = "platform.config.query.accounts";
pub const DEFINE_USER_GROUP: &str = "platform.config.command.define-user-group";
pub const DEFINE_ACCOUNT_GROUP: &str = "platform.config.command.define-account-group";
pub const DEFINE_ACCESS_GROUP: &str = "platform.config.command.define-access-group";
pub const GRANT_PERMISSION: &str = "platform.config.command.grant-permission";
pub const WITHDRAW_PERMISSION: &str = "platform.config.command.withdraw-permission";
pub const SEND_DIAGNOSTIC_BUNDLE: &str = "platform.config.command.send-diagnostic-bundle";
pub const SET_PLUGIN_SETTINGS: &str = "platform.config.command.set-plugin-settings";
pub const PLUGIN_CONFIGURATION: &str = "platform.config.query.plugin-configuration";
pub const PLUGIN_CONFIGURATION_CHANGED: &str = "platform.config.event.plugin-configuration-changed";
pub const PLUGIN_ACCESS: &str = "platform.config.query.plugin-access";
pub const PLUGIN_REPORT: &str = "platform.deployment.event.plugin-report";

/// Where the time comes from: the deployment's one clock (decisions/024),
/// given by whoever wires this up, so a test does not wait for it.
pub use meridian_clock::Clock;

/// The platform, as far as the configuration store needs it: the two acts a
/// deployment admin can send outward. The conductor implements it with the
/// key it already holds; this crate never sees the key.
///
/// Called from a bus handler, which runs on a blocking thread, so these block.
pub trait Upstream: Send + Sync {
    /// W5.22. Carries the code and its purpose, and nothing about the person.
    ///
    /// The purpose travels because the platform checks it: a code for the
    /// first administrator is not a code for the wizard, and swapping one for
    /// the other is refused there rather than here.
    fn honour_claim_code(&self, code: &str, purpose: i32) -> Result<RedeemClaimCodeReply, String>;

    /// W5.23. Exactly what the deployment admin approved.
    fn submit_diagnostic_bundle(
        &self,
        bundle: &DiagnosticBundle,
    ) -> Result<DiagnosticBundleReceipt, String>;
}

/// The built-in access group of the dashboard's own capabilities, as the
/// store seeds it.
pub fn deployment_admin() -> meridian_domain::v1::AccessGroup {
    meridian_domain::v1::AccessGroup {
        access_group_id: DEPLOYMENT_ADMIN.into(),
        name: "Deployment admin".into(),
        entries: Vec::new(),
        built_in: true,
    }
}

/// The built-in access group granting `admin` on every plugin, as the store
/// seeds it (W6.7).
pub fn all_plugins_admin() -> meridian_domain::v1::AccessGroup {
    meridian_domain::v1::AccessGroup {
        access_group_id: meridian_access::ALL_PLUGINS_ADMIN.into(),
        name: "All plugins (admin)".into(),
        entries: Vec::new(),
        built_in: true,
    }
}

/// The built-in account group holding every account, as the store seeds it
/// (W6.6).
pub fn all_accounts() -> meridian_domain::v1::AccountGroup {
    meridian_domain::v1::AccountGroup {
        account_group_id: meridian_access::ALL_ACCOUNTS.into(),
        name: "All accounts".into(),
        account_ids: Vec::new(),
        built_in: true,
    }
}

/// The permissions that make a user group the deployment's administrators:
/// one to deployment admin, and one to All plugins (admin), as first run and a
/// claim code both write them (W6.2, W7.6).
fn administrators(user_group_id: &str, now: i64) -> Vec<Permission> {
    [DEPLOYMENT_ADMIN, meridian_access::ALL_PLUGINS_ADMIN]
        .into_iter()
        .map(|access_group| Permission {
            permission_id: ids::permission(now),
            user_group_id: user_group_id.to_string(),
            account_group_id: String::new(),
            access_group_id: access_group.into(),
        })
        .collect()
}

/// A plugin's settings as stored: the ones it declared, secrets still sealed.
fn stored_settings<'a>(
    snapshot: &'a Snapshot,
    plugin_instance_id: &'a str,
) -> impl Iterator<Item = &'a StoredSetting> {
    let declared = snapshot
        .declared_settings
        .get(plugin_instance_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    snapshot.settings.iter().filter(move |held| {
        held.plugin_instance_id == plugin_instance_id
            && declared.iter().any(|d| d.name == held.name)
    })
}

/// What a sidecar is told about its plugin, but for its settings: those are
/// [`configuration`]'s, the one place a secret is opened.
fn told(snapshot: &Snapshot, plugin_instance_id: &str) -> PluginConfiguration {
    let scope =
        meridian_access::plugin_scope(&snapshot.records, &snapshot.links, plugin_instance_id);
    let links: Vec<ExternalAccountLink> = snapshot
        .links
        .iter()
        .filter(|link| link.plugin_instance_id == plugin_instance_id)
        .cloned()
        .collect();
    // The accounts those links name, each once, so the sidecar can name them
    // beside the plugin's scope (W4.11); a rename or a close of one is then a
    // change to this plugin's configuration, and announced.
    let linked_accounts = snapshot
        .records
        .accounts
        .iter()
        .filter(|account| {
            links
                .iter()
                .any(|link| link.account_id == account.account_id)
        })
        .cloned()
        .collect();
    PluginConfiguration {
        plugin_instance_id: plugin_instance_id.to_string(),
        settings: Vec::new(),
        links,
        read_account_ids: scope.read.into_iter().collect(),
        write_account_ids: scope.write.into_iter().collect(),
        linked_accounts,
    }
}

/// What a sidecar is told about its plugin, derived from one snapshot:
/// secrets opened, for that sidecar to hand to its plugin and nobody else.
///
/// A secret that does not open with this deployment's key -- the key was
/// lost, or replaced -- is left out and said so, naming the setting: the
/// plugin then reports it missing if it is required, and a deployment admin
/// sets it again.
pub fn configuration(
    snapshot: &Snapshot,
    plugin_instance_id: &str,
    key: &SettingsKey,
) -> PluginConfiguration {
    let mut configuration = told(snapshot, plugin_instance_id);
    configuration.settings = stored_settings(snapshot, plugin_instance_id)
        .filter_map(|held| {
            let value = match &held.held {
                Held::Plain(value) => value.clone(),
                Held::Sealed(sealed) => match key.open(plugin_instance_id, &held.name, sealed) {
                    Ok(value) => value,
                    Err(why) => {
                        tracing::warn!("{why}; the plugin is not given it until it is set again");
                        return None;
                    }
                },
            };
            Some(PluginSettingValue {
                name: held.name.clone(),
                value,
            })
        })
        .collect();
    configuration
}

/// What the dashboard may show of a plugin's settings (W6.11): what it
/// declared, the values that are not secret, and which secrets are set.
///
/// A setting declared secret, or held sealed, is only ever named: whatever
/// the plugin declares later, nothing sealed is shown.
pub fn settings_record(snapshot: &Snapshot, plugin_instance_id: &str) -> PluginSettingsRecord {
    let declared = snapshot
        .declared_settings
        .get(plugin_instance_id)
        .cloned()
        .unwrap_or_default();
    let mut record = PluginSettingsRecord {
        plugin_instance_id: plugin_instance_id.to_string(),
        ..Default::default()
    };
    for held in stored_settings(snapshot, plugin_instance_id) {
        let secret = declared.iter().any(|d| d.name == held.name && d.secret);
        match &held.held {
            Held::Plain(value) if !secret => record.values.push(PluginSettingValue {
                name: held.name.clone(),
                value: value.clone(),
            }),
            _ => record.secrets_set.push(held.name.clone()),
        }
        if held.set_at_ns >= record.updated_at_ns {
            record.updated_at_ns = held.set_at_ns;
            record.updated_by = held.set_by.clone();
        }
    }
    record.declared_settings = declared;
    record
}

/// Every plugin anything could be configured for.
fn plugins_in(snapshot: &Snapshot) -> BTreeSet<String> {
    let mut plugins: BTreeSet<String> = snapshot
        .plugins
        .iter()
        .map(|p| p.plugin_instance_id.clone())
        .collect();
    plugins.extend(snapshot.links.iter().map(|l| l.plugin_instance_id.clone()));
    plugins.extend(
        snapshot
            .settings
            .iter()
            .map(|s| s.plugin_instance_id.clone()),
    );
    for group in &snapshot.records.access_groups {
        plugins.extend(group.entries.iter().map(|e| e.plugin_instance_id.clone()));
    }
    plugins
}

/// The plugins whose configuration differs between two snapshots. Settings
/// are compared as stored, so telling which changed opens no secret.
pub fn changed_plugins(before: &Snapshot, after: &Snapshot) -> Vec<String> {
    let mut all = plugins_in(before);
    all.extend(plugins_in(after));
    let settings = |snapshot: &Snapshot, plugin: &str| -> Vec<(String, Held)> {
        stored_settings(snapshot, plugin)
            .map(|held| (held.name.clone(), held.held.clone()))
            .collect()
    };
    all.into_iter()
        .filter(|plugin| {
            told(before, plugin) != told(after, plugin)
                || settings(before, plugin) != settings(after, plugin)
        })
        .collect()
}

struct Context {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    upstream: Arc<dyn Upstream>,
    key: Arc<SettingsKey>,
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

/// W7.6. Write the administrator the wizard named, once.
///
/// Called by the conductor when it first has a store. Applying the
/// configuration recorded who administers this deployment; writing the
/// permission had to wait, because access records live in this store and this
/// store's database is one of the things the wizard was configuring
/// (decisions/017).
///
/// Idempotent by the same mechanism a claim code uses: the write is refused
/// when a deployment admin already exists, so a restart, a second apply or a
/// claim code redeemed in between all leave one administrator rather than
/// two arguments about who it is.
pub fn install_named_administrator(
    store: &dyn Store,
    clock: &dyn Clock,
    directory_group: &str,
    login: &str,
) -> Result<bool, String> {
    let directory_group = directory_group.trim();
    let login = login.trim();
    if directory_group.is_empty() && login.is_empty() {
        return Ok(false);
    }

    let now = clock.now_ns();
    let group = UserGroup {
        user_group_id: ids::user_group(now),
        name: "Deployment admins".into(),
        directory_groups: match directory_group.is_empty() {
            true => Vec::new(),
            false => vec![directory_group.to_string()],
        },
        logins: match login.is_empty() {
            true => Vec::new(),
            false => vec![login.to_string()],
        },
    };
    let permissions = administrators(&group.user_group_id, now);

    store
        .install_first_admin(&group, &permissions)
        .map_err(|failed| failed.to_string())
}

pub(crate) fn subject(envelope: &Envelope) -> String {
    envelope
        .meta
        .as_ref()
        .map(|meta| meta.acting_for_subject.clone())
        .unwrap_or_default()
}

/// The admin of the plugin its sidecar vouched for, in a session opened by
/// Manage, and stamped; or the refusal: a plugin reaches the deployment's
/// configuration only acting for one (W4.9, W6.4), and what it does there is
/// recorded as theirs.
fn admin_acting(envelope: &Envelope, what: &str) -> Result<String, String> {
    let by = subject(envelope);
    if by.is_empty() {
        return Err(format!(
            "{what} is an admin of the plugin's to ask for, and this is sent for nobody"
        ));
    }
    Ok(by)
}

/// The delegation the person acted through, when they acted through a
/// client (decisions/029); empty otherwise.
fn delegation(envelope: &Envelope) -> String {
    envelope
        .meta
        .as_ref()
        .map(|meta| meta.acting_through_delegation.clone())
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
    answer_on(&Arc::clone(&context.bus), context, topic, types, handle)
}

/// [`answer`], for any context: the one decode-handle-encode for every
/// handler this crate serves.
pub(crate) fn answer_on<C, Req, Rep, F>(
    bus: &Arc<Bus>,
    context: &Arc<C>,
    topic: &'static str,
    types: (&'static str, &'static str),
    handle: F,
) where
    C: Send + Sync + 'static,
    Req: Message + Default,
    Rep: Message,
    F: Fn(&C, Req, &Envelope) -> Result<Rep, String> + Send + Sync + 'static,
{
    let (request_type, reply_type) = types;
    let context = Arc::clone(context);
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
///
/// `key` seals a secret setting before it is written and opens it for its
/// plugin's sidecar; nothing else here holds it.
pub fn serve(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    upstream: Arc<dyn Upstream>,
    key: Arc<SettingsKey>,
) {
    let context = Arc::new(Context {
        bus: Arc::clone(&bus),
        store,
        clock,
        upstream,
        key,
    });

    // Links made before an account held one external account at most are
    // kept, and said, for a deployment admin to separate (W6.4). Off the
    // runtime, since the store blocks on its socket.
    let reading = Arc::clone(&context);
    tokio::task::spawn_blocking(move || {
        if let Ok(snapshot) = reading.snapshot() {
            for (account, links) in rules::accounts_linked_twice(&snapshot) {
                tracing::warn!(
                    account,
                    links = links.join(", "),
                    "an account holds more than one external account, linked before an \
                     account held one; kept, and a new second link is refused"
                );
            }
        }
    });

    answer(
        &context,
        ACCESS_RECORDS,
        (
            "meridian.v1.AccessRecordsRequest",
            "meridian.v1.AccessRecords",
        ),
        |cx, _: AccessRecordsRequest, _| {
            let snapshot = cx.snapshot()?;
            // Each known plugin's settings form (W6.11): what it declared,
            // and which secrets are set, never what they are.
            let plugin_settings = snapshot
                .plugins
                .iter()
                .map(|plugin| settings_record(&snapshot, &plugin.plugin_instance_id))
                .collect();
            let mut records = snapshot.records;
            records.read_at_ns = cx.clock.now_ns();
            records.plugin_settings = plugin_settings;
            // With the links, so the dashboard can tell which of the accounts
            // a connector reports have none (W2.8, W6.4). Only the dashboard
            // asks this, and it may already see every account.
            records.links = snapshot.links;
            Ok(records)
        },
    );

    answer(
        &context,
        SET_PLUGIN_SETTINGS,
        (
            "meridian.v1.SetPluginSettingsRequest",
            "meridian.v1.PluginSettingsRecord",
        ),
        |cx, request: SetPluginSettingsRequest, envelope| {
            let before = cx.snapshot()?;
            let plugin = request.plugin_instance_id.as_str();
            // The dashboard's form alone sets a setting (W6.11, option A):
            // a plugin's sidecar has no grant to publish here, and one that
            // did would be refused.
            if before
                .plugins
                .iter()
                .any(|known| known.plugin_instance_id == publisher(envelope))
            {
                return Err(
                    "a plugin sets none of its settings: an admin of the plugin sets \
                            them in the dashboard's Settings form"
                        .to_string(),
                );
            }
            let by = subject(envelope);
            let now = cx.clock.now_ns();
            // Every value checked, and every secret sealed, before anything
            // is written: a refusal part-way through changes nothing.
            let mut changes = Vec::new();
            for (declaration, value) in rules::plugin_settings(&before, &request)? {
                let name = declaration.name.clone();
                let held = match value {
                    None => None,
                    // A table's rows, each added or changed stamped with who
                    // and when, an unchanged one keeping its own (W6.11).
                    Some(value) if setting_table::is_table(declaration) => {
                        let rows = setting_table::parse(&value)
                            .map_err(|why| format!("setting {name}: {why}"))?;
                        let standing = stored_settings(&before, plugin)
                            .find(|held| held.name == name)
                            .and_then(|held| match &held.held {
                                Held::Plain(text) => setting_table::parse(text).ok(),
                                Held::Sealed(_) => None,
                            })
                            .unwrap_or_default();
                        let cells: Vec<_> = rows.into_iter().map(|row| row.cells).collect();
                        Some(Held::Plain(setting_table::written(
                            &setting_table::stamped(&cells, &standing, &by, now),
                        )))
                    }
                    Some(value) if declaration.secret => Some(Held::Sealed(
                        cx.key.seal(plugin, &name, &value).map_err(|why| {
                            format!("secret setting {name} was not stored, and nothing was: {why}")
                        })?,
                    )),
                    Some(value) => Some(Held::Plain(value)),
                };
                changes.push(SettingChange { name, held });
            }
            if !changes.is_empty() {
                let author = SettingsAuthor {
                    by: by.clone(),
                    delegation: delegation(envelope),
                };
                cx.store
                    .put_plugin_settings(plugin, &changes, &author, now)
                    .map_err(|f| f.to_string())?;
                let named = |set: bool| -> String {
                    changes
                        .iter()
                        .filter(|change| change.held.is_some() == set)
                        .map(|change| change.name.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                };
                // Names, never values.
                tracing::info!(
                    plugin,
                    set = named(true),
                    cleared = named(false),
                    by,
                    "plugin settings changed"
                );
                cx.announce(&before)?;
            }
            Ok(settings_record(&cx.snapshot()?, plugin))
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
            // An edit sets all four as given, so an empty one clears it.
            let described = AccountRecord {
                name: request.name.clone(),
                custodian: request.custodian.trim().to_string(),
                account_type: request.account_type.trim().to_string(),
                owner: request.owner.trim().to_string(),
                note: request.note.trim().to_string(),
                ..AccountRecord::default()
            };
            let account = match before
                .records
                .accounts
                .iter()
                .find(|a| a.account_id == request.account_id)
            {
                Some(existing) => AccountRecord {
                    account_id: existing.account_id.clone(),
                    state: existing.state,
                    created_at_ns: existing.created_at_ns,
                    ..described
                },
                None => AccountRecord {
                    account_id: ids::account(now),
                    state: AccountState::Open as i32,
                    created_at_ns: now,
                    ..described
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
            let by = admin_acting(envelope, "a link")?;
            if publisher(envelope) != request.plugin_instance_id {
                return Err(format!(
                    "a plugin links only its own external accounts, and this names {}",
                    request.plugin_instance_id
                ));
            }
            let before = cx.snapshot()?;
            rules::link(&before, &request)?;
            let mut link = ExternalAccountLink {
                plugin_instance_id: request.plugin_instance_id,
                external_account_id: request.external_account_id,
                account_id: request.account_id,
            };
            if request.new_account_name.is_empty() {
                cx.store.put_link(&link).map_err(|f| f.to_string())?;
            } else {
                // Created and linked in one step, so nothing is left half-done.
                let now = cx.clock.now_ns();
                let account = AccountRecord {
                    account_id: ids::account(now),
                    name: request.new_account_name.trim().to_string(),
                    state: AccountState::Open as i32,
                    created_at_ns: now,
                    custodian: request.new_account_custodian.trim().to_string(),
                    account_type: request.new_account_type.trim().to_string(),
                    owner: request.new_account_owner.trim().to_string(),
                    note: request.new_account_note.trim().to_string(),
                };
                link.account_id = account.account_id.clone();
                cx.store
                    .put_account_and_link(&account, &link)
                    .map_err(|f| f.to_string())?;
                tracing::info!(
                    account = account.account_id,
                    plugin = link.plugin_instance_id,
                    by,
                    "account defined"
                );
            }
            tracing::info!(
                plugin = link.plugin_instance_id,
                external = link.external_account_id,
                account = link.account_id,
                by,
                "external account link set"
            );
            cx.announce(&before)?;
            Ok(link)
        },
    );

    answer(
        &context,
        ACCOUNTS,
        ("meridian.v1.AccountsRequest", "meridian.v1.Accounts"),
        |cx, _: AccountsRequest, envelope| {
            admin_acting(envelope, "the deployment's accounts")?;
            // Each account as the configuration holds it, its custodian,
            // type, owner and note with it: nothing of who may read them,
            // and no holdings, which are not the configuration's.
            Ok(Accounts {
                accounts: cx.snapshot()?.records.accounts,
            })
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
            rules::names_no_tag(&envelope.payload)?;
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
                ..Default::default()
            };
            // A first-run code is the exception, and the only one: it opens
            // the wizard before any directory exists, so nobody can be signed
            // in to redeem it and the deployment has no records to put anybody
            // in. It makes no administrator here; the code it brings back does
            // that later, at a real sign-in (W7.3, W7.7).
            let first_run = request.purpose == ClaimCodePurpose::FirstRun as i32;
            // And a password-reset code (W6.16): whoever holds one cannot sign
            // in, which is why they hold one. It makes nobody anything here;
            // the dashboard sets the password once the platform honours it.
            let reset = request.purpose == ClaimCodePurpose::ResetLocalAdmin as i32;
            if redeemer.is_empty() && !first_run && !reset {
                return Ok(refused("a claim code is redeemed by somebody signed in"));
            }
            if first_run || reset {
                return cx
                    .upstream
                    .honour_claim_code(&request.code, request.purpose);
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

            let answered = cx
                .upstream
                .honour_claim_code(&request.code, request.purpose)?;
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
            let permissions = administrators(&group.user_group_id, now);
            let installed = cx
                .store
                .install_first_admin(&group, &permissions)
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
            Ok(configuration(&cx.snapshot()?, &plugin, &cx.key))
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
                roles: report.roles,
                last_reported_at_ns: report.reported_at_ns,
            };
            // What it declared, only while it is registered: a plugin between
            // registrations declares nothing, and keeps its form meanwhile.
            // And only from its own sidecar, which is on the bus as the
            // plugin's instance: which settings are secret is not something
            // another component may say about a plugin.
            let own = publisher(&delivery.envelope) == plugin.plugin_instance_id;
            let declared = (report.registered && own).then_some(report.declared_settings);
            let store = Arc::clone(&noting.store);
            let keeping = move || {
                store.record_plugin(&plugin)?;
                match declared {
                    Some(declared) => {
                        store.record_declared_settings(&plugin.plugin_instance_id, &declared)
                    }
                    None => Ok(()),
                }
            };
            match tokio::task::spawn_blocking(keeping).await {
                Ok(Ok(())) => {}
                Ok(Err(failed)) => tracing::warn!("a plugin report was not kept: {failed}"),
                Err(failed) => tracing::warn!("a plugin report was not kept: {failed}"),
            }
        }
    });
}
