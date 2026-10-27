//! Where the configuration store meets the bus: the `config` domain.
//!
//! Eleven commands and queries from the dashboard, two queries from sidecars,
//! a command and a query from a plugin acting for its admin, three events
//! heard, a query asked of the instrument store, one announced. And from
//! contract v16 an edge plugin's raw records ([`crate::archive`]): a hold set
//! and the moves read, from the dashboard, and a move reported by a sidecar as
//! itself; the hold over an instance is told to its sidecar on its
//! configuration, and a window setting below it refused. Every change
//! is written the same way:
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
//! with `changed_by` and `changed_at` ([`meridian_domain::setting_table`]);
//! an external account cell names one the plugin reported or links, and an
//! instrument cell a record the instrument store holds, asked by its ID.
//!
//! A setting re-declared with another type, or as secret or not, has its
//! value cleared as the declaration is kept, the clear its own record naming
//! the re-declaration and no person; one declared secret has the values its
//! earlier records hold redacted (the product owner, 2026-10-05). And a value
//! held is re-read against its declaration as it stands before it is
//! delivered, shown, or its rows' stamps kept: one that does not read is
//! withheld.
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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

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
use meridian_domain::v1::{
    ExternalAccountsEvent, PluginLaunchState, ResolveInstrumentReply, ResolveInstrumentRequest,
};
use meridian_pb::v1::{PluginAccessRequest, SettingColumnType, SettingDeclaration};
use prost::Message;

use crate::ids;
use crate::rules;
use crate::sealing::SettingsKey;
use crate::store::{
    note_refused, Author, Held, KnownPlugin, SettingChange, SettingsAuthor, Snapshot, Store,
    StoredSetting, Withdrawal,
};
use crate::DEPLOYMENT_ADMIN;

pub const PERSON_SIGNED_IN: &str = "platform.config.event.person-signed-in";
pub const ACCESS_RECORDS: &str = "platform.config.query.access-records";
pub const REDEEM_CLAIM_CODE: &str = "platform.config.command.redeem-claim-code";
pub const DEFINE_ACCOUNT: &str = "platform.config.command.define-account";
pub const CLOSE_ACCOUNT: &str = "platform.config.command.close-account";
/// W3.28 (contract v24): the usage statistics' setting, and the reports sent.
pub const SET_REPORTS_USAGE: &str = "platform.config.command.set-reports-usage";
/// W6.35 (contract v26): the deployment's time zone, set and read.
pub const SET_DEPLOYMENT_TIME_ZONE: &str = "platform.config.command.set-deployment-time-zone";
pub const READ_DEPLOYMENT_TIME_ZONE: &str = "platform.config.query.read-deployment-time-zone";
// Contract v27 (W6.36; ruling 483): the deployment's FX pivot currencies.
pub const SET_FX_PIVOT_CURRENCIES: &str = "platform.config.command.set-fx-pivot-currencies";
pub const READ_FX_PIVOT_CURRENCIES: &str = "platform.config.query.read-fx-pivot-currencies";
/// Contract v28 (W6.37; rulings 561 and 589): the series request rule.
pub const SET_SERIES_REQUEST_RULE: &str = "platform.config.command.set-series-request-rule";
pub const READ_SERIES_REQUEST_RULE: &str = "platform.config.query.read-series-request-rule";
/// Contract v28 (W6.38; rulings 515, 517, 600): the stale-reference actions
/// the deployment permits.
pub const SET_STALE_REFERENCE_ACTIONS_PERMITTED: &str =
    "platform.config.command.set-stale-reference-actions-permitted";
pub const READ_STALE_REFERENCE_ACTIONS_PERMITTED: &str =
    "platform.config.query.read-stale-reference-actions-permitted";
/// The most pivots a deployment orders (W6.36).
pub const MOST_PIVOTS: usize = 4;
/// W12.6 (contract v26; ruling 285): the analytics routing table, held here.
pub const SET_MODEL_ROUTING: &str = "platform.analytics.command.set-model-routing";
pub const LIST_MODEL_ROUTING: &str = "platform.analytics.query.list-model-routing";
/// v26's model ids, in the order the table lists them (the first release's
/// §1), and the implementation every one has.
pub const MODEL_IDS: [&str; 3] = ["cash", "quoted_price", "binary_event"];
pub const BUILT_IN: &str = "built-in";
/// The deployment's time zone with nothing set (the v26 plan's question
/// 229: US first).
pub const DEFAULT_TIME_ZONE: &str = "America/New_York";
/// W3.28 (contract v25): the identifier schemes declared as licensed.
pub const SET_LICENSED_SCHEMES: &str = "platform.config.command.set-licensed-schemes";

/// A declaration of the licensed schemes held to its rules (W3.28, contract
/// v25): each a scheme family the licence list names (`listed`; the list the
/// deployment sends by where none is kept), named once, at most
/// [`MOST_LICENSED_SCHEMES`]; refused naming the scheme by its path. In the
/// order given, lower-cased and trimmed.
pub fn licensed_schemes_held(
    asked: &[String],
    listed: Option<&[String]>,
) -> Result<Vec<String>, String> {
    if asked.len() > MOST_LICENSED_SCHEMES {
        return Err(meridian_bus::refusal_with_status(
            meridian_bus::RefusalStatus::InvalidArgument,
            &["licensed_schemes".to_string()],
            format!("at most {MOST_LICENSED_SCHEMES} schemes"),
        ));
    }
    let mut held: Vec<String> = Vec::new();
    for (index, scheme) in asked.iter().enumerate() {
        let scheme = scheme.trim().to_ascii_lowercase();
        let path = format!("licensed_schemes[{index}]");
        let named = listed.is_none_or(|listed| listed.iter().any(|on| on == &scheme));
        if scheme.is_empty() || !named {
            return Err(meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::InvalidArgument,
                &[path],
                format!(
                    "{scheme:?} is not a scheme the licence list names{}",
                    listed
                        .map(|listed| format!(" ({})", listed.join(", ")))
                        .unwrap_or_default()
                ),
            ));
        }
        if held.contains(&scheme) {
            return Err(meridian_bus::refusal_with_status(
                meridian_bus::RefusalStatus::InvalidArgument,
                &[path],
                format!("{scheme} is named twice"),
            ));
        }
        held.push(scheme);
    }
    Ok(held)
}
pub const LIST_USAGE_REPORTS: &str = "platform.config.query.list-usage-reports";
pub const LINK_EXTERNAL_ACCOUNT: &str = "platform.config.command.link-external-account";
pub const ACCOUNTS: &str = "platform.config.query.accounts";
pub const SET_LINKABLE_ACCOUNTS: &str = "platform.config.command.set-linkable-accounts";
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
/// W2.8: the external accounts each custody connection reaches, which a
/// table's external account cell is checked against (W6.11).
pub const EXTERNAL_ACCOUNTS: &str = "platform.custody.*.event.external-accounts";
/// W3.6: a record by its ID, which a table's instrument cell is checked
/// against (W6.11).
pub const RESOLVE_INSTRUMENT: &str = "platform.reference.query.resolve-instrument";

/// How long a settings update waits for the instrument store to say whether
/// a record exists.
const INSTRUMENT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

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

    /// The scheme families the platform's licence list names (W3.25), which
    /// a declaration of the licensed schemes is held to (contract v25,
    /// W3.28): the conductor's latest copy, or before one is read, the list
    /// the deployment sends by. None where the conductor keeps none.
    fn licence_schemes(&self) -> Option<Vec<String>> {
        None
    }
}

/// The most schemes a declaration of the licensed schemes names, as the
/// usage report carries at most.
pub const MOST_LICENSED_SCHEMES: usize = 64;

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
    declared_and_stored(snapshot, plugin_instance_id).map(|(_, held)| held)
}

/// Each setting a plugin declared and has a value held for, with its
/// declaration as it stands.
fn declared_and_stored<'a>(
    snapshot: &'a Snapshot,
    plugin_instance_id: &'a str,
) -> impl Iterator<Item = (&'a SettingDeclaration, &'a StoredSetting)> {
    let declared = snapshot
        .declared_settings
        .get(plugin_instance_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    snapshot.settings.iter().filter_map(move |held| {
        if held.plugin_instance_id != plugin_instance_id {
            return None;
        }
        declared
            .iter()
            .find(|d| d.name == held.name)
            .map(|declaration| (declaration, held))
    })
}

/// A value held, if it reads under its declaration as it stands
/// ([`rules::held_reads`]); one that does not is said, naming the setting
/// and never the value, and withheld.
fn held_reading<'a>(
    declaration: &SettingDeclaration,
    held: &'a StoredSetting,
    plugin_instance_id: &str,
) -> Option<&'a Held> {
    match rules::held_reads(declaration, &held.held) {
        Ok(()) => Some(&held.held),
        Err(why) => {
            tracing::warn!(
                plugin = plugin_instance_id,
                "{why}; withheld until it is set again"
            );
            None
        }
    }
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
    let (hold_days, hold_write_once) = crate::archive::hold_over(snapshot, plugin_instance_id);
    // A connectivity plugin's link and the accounts its `ems` mapped on it,
    // which its connectivity role reaches beside whatever its other roles do;
    // an `ems`'s connectivity plugins and its map (W4.11, W11.21, W11.22,
    // contract v22). An account mapped and since closed is read, not written.
    let connectivity = crate::connectivity::told(snapshot, plugin_instance_id);
    let mut read = scope.read;
    let mut write = scope.write;
    for account in &connectivity.mapped_accounts {
        let Some(record) = snapshot
            .records
            .accounts
            .iter()
            .find(|held| &held.account_id == account)
        else {
            continue;
        };
        read.insert(account.clone());
        if record.state != AccountState::Closed as i32 {
            write.insert(account.clone());
        }
    }
    PluginConfiguration {
        plugin_instance_id: plugin_instance_id.to_string(),
        settings: Vec::new(),
        links,
        read_account_ids: read.into_iter().collect(),
        write_account_ids: write.into_iter().collect(),
        linked_accounts,
        // The hold over the instance (W6.25, contract v16): the longest of
        // its edge roles' and the one for every role. Its sidecar refuses a
        // deletion inside it (W4.13); the plugin is never told it.
        hold_days,
        hold_write_once,
        connectivity_link: connectivity.connectivity_link,
        connectivity_links: connectivity.connectivity_links,
        venue_account_map: connectivity.venue_account_map,
        declaration: launched_declaration(snapshot, plugin_instance_id),
        launched: snapshot
            .catalogue
            .launches
            .iter()
            .any(|launch| launch.instance_id == plugin_instance_id),
    }
}

/// The declaration the instance's launched version was uploaded with (W4.1,
/// contract v25), which its sidecar holds the plugin's registration to;
/// none for an instance not launched from the catalogue, or a version
/// uploaded with none.
/// W6.35 (contract v26): the deployment's time zone in force, and beside
/// it the earliest affirmation deadline a launched `operations` instance
/// declares (W8.1, W11.34; the v26 plan's question 230), with the instance.
pub fn time_zone_setting(
    snapshot: &Snapshot,
    held: Option<crate::store::TimeZoneChange>,
) -> meridian_domain::v1::DeploymentTimeZoneSetting {
    let held = held.unwrap_or_default();
    let mut instances: Vec<&str> = snapshot
        .catalogue
        .launches
        .iter()
        .filter(|launch| launch.state == PluginLaunchState::Launched as i32)
        .map(|launch| launch.instance_id.as_str())
        .collect();
    instances.sort_unstable();
    instances.dedup();
    let earliest = instances
        .into_iter()
        .filter(|instance| {
            crate::connectivity::roles_of(snapshot, instance)
                .iter()
                .any(|role| role == "operations")
        })
        .filter_map(|instance| {
            launched_declaration(snapshot, instance)
                .map(|declaration| (declaration.affirmation_deadline, instance))
        })
        .filter(|(deadline, _)| (1..=1440).contains(deadline))
        .min();
    meridian_domain::v1::DeploymentTimeZoneSetting {
        deployment_time_zone: if held.zone.is_empty() {
            DEFAULT_TIME_ZONE.to_string()
        } else {
            held.zone
        },
        changed_at_ns: held.changed_at_ns,
        changed_by: held.changed_by,
        affirmation_deadline: earliest.map(|(deadline, _)| deadline).unwrap_or(0),
        affirmation_deadline_instance_id: earliest
            .map(|(_, instance)| instance.to_string())
            .unwrap_or_default(),
    }
}

/// W12.6 (contract v26): the routing table in force, each model id at its
/// latest change or `built-in`, and its version, one more than its changes.
pub fn model_routing(
    changes: &[crate::store::ModelRoutingChange],
) -> meridian_domain::v1::ListModelRoutingReply {
    meridian_domain::v1::ListModelRoutingReply {
        routes: MODEL_IDS
            .iter()
            .map(
                |model| match changes.iter().rev().find(|c| c.model_id == *model) {
                    Some(change) => meridian_domain::v1::ModelRouting {
                        model_id: change.model_id.clone(),
                        implementation: change.implementation.clone(),
                        changed_at_ns: change.changed_at_ns,
                        changed_by: change.changed_by.clone(),
                        reason: change.note.clone(),
                    },
                    None => meridian_domain::v1::ModelRouting {
                        model_id: model.to_string(),
                        implementation: BUILT_IN.to_string(),
                        ..Default::default()
                    },
                },
            )
            .collect(),
        version: changes.len() as i64 + 1,
    }
}

/// Whether `instance` is a `pricing` instance the deployment runs: launched
/// from the catalogue holding the role, or reporting it.
fn runs_pricing(snapshot: &Snapshot, instance: &str) -> bool {
    let holds = crate::connectivity::roles_of(snapshot, instance)
        .iter()
        .any(|role| role == "pricing");
    let launched = snapshot
        .catalogue
        .launches
        .iter()
        .rev()
        .find(|launch| launch.instance_id == instance)
        .is_some_and(|launch| launch.state == PluginLaunchState::Launched as i32);
    let reported = snapshot
        .plugins
        .iter()
        .any(|plugin| plugin.plugin_instance_id == instance);
    holds && (launched || reported)
}

/// The pivots in force, as the read answers them (W6.36): the latest
/// change's, or never set, none, with no change -- read as USD's cash
/// instrument and then EUR's, which each reader names by the codes its own
/// stores answer (the conductor resolves no code).
pub fn fx_pivot_setting(
    held: Option<crate::store::FxPivotChange>,
) -> meridian_domain::v1::FxPivotCurrenciesSetting {
    let held = held.unwrap_or_default();
    meridian_domain::v1::FxPivotCurrenciesSetting {
        fx_pivot_currencies: held.fx_pivot_currencies,
        changed_at_ns: held.changed_at_ns,
        changed_by: held.changed_by,
    }
}

/// The series request rule in force (W6.37; 589): the latest change's, or
/// never set, the default -- orders on, standing wants off, 500 a day --
/// with no change.
pub fn series_request_rule(
    held: Option<crate::store::DeploymentSettingChange>,
) -> meridian_domain::v1::SeriesRequestRule {
    match held {
        Some(change) => {
            let rule = meridian_domain::v1::SeriesRequestRule::decode(&change.value[..])
                .unwrap_or_else(|_| default_series_request_rule());
            meridian_domain::v1::SeriesRequestRule {
                changed_at_ns: change.changed_at_ns,
                changed_by: change.changed_by,
                ..rule
            }
        }
        None => default_series_request_rule(),
    }
}

/// The stale-reference actions permitted in force (W6.38): the latest
/// change's, or never set, refuse only, with no change.
pub fn stale_reference_actions_permitted(
    held: Option<crate::store::DeploymentSettingChange>,
) -> meridian_domain::v1::StaleReferenceActionsPermittedSetting {
    match held {
        Some(change) => meridian_domain::v1::StaleReferenceActionsPermittedSetting {
            warn_permitted: meridian_domain::v1::StaleReferenceActionsPermittedSetting::decode(
                &change.value[..],
            )
            .map(|setting| setting.warn_permitted)
            .unwrap_or(false),
            changed_at_ns: change.changed_at_ns,
            changed_by: change.changed_by,
        },
        None => Default::default(),
    }
}

fn default_series_request_rule() -> meridian_domain::v1::SeriesRequestRule {
    meridian_domain::v1::SeriesRequestRule {
        orders: true,
        standing_wants: false,
        daily_limit: 500,
        changed_at_ns: 0,
        changed_by: String::new(),
    }
}

/// W7.5 (contract v26): the zone the wizard was told, recorded once as the
/// first change, naming the administrator the wizard was told (`by`; the
/// first run itself where none), where nothing set one before. Says
/// whether it recorded it.
pub fn record_first_run_time_zone(
    store: &dyn crate::store::Store,
    zone: &str,
    by: &str,
    at_ns: i64,
) -> Result<bool, String> {
    let zone = zone.trim();
    if zone.is_empty()
        || store
            .deployment_time_zone()
            .map_err(|f| f.to_string())?
            .is_some()
    {
        return Ok(false);
    }
    if !meridian_domain::zones::is_zone(zone) {
        return Err(format!("{zone:?} is not an IANA time zone"));
    }
    store
        .set_deployment_time_zone(
            zone,
            &Author {
                // The person the wizard was told administers the deployment
                // (W7.5), else the first run itself.
                by: if by.trim().is_empty() {
                    "first-run".into()
                } else {
                    by.trim().to_string()
                },
                note: "set at the deployment's first run (W7.5)".into(),
                ..Default::default()
            },
            at_ns,
        )
        .map_err(|f| f.to_string())?;
    Ok(true)
}

fn launched_declaration(
    snapshot: &Snapshot,
    plugin_instance_id: &str,
) -> Option<meridian_pb::v1::PluginDeclaration> {
    // The instance's live launch, else its latest: the catalogue keeps every
    // launch in history, oldest first, and an instance launched again from a
    // newer version must be held to that version's declaration, never its
    // first launch's (found on the dev deployment, 2026-10-17: every v25
    // plugin refused for its 0.1.0's missing declaration).
    let launches = || {
        snapshot
            .catalogue
            .launches
            .iter()
            .filter(|launch| launch.instance_id == plugin_instance_id)
    };
    let launch = launches()
        .rev()
        .find(|launch| launch.state == PluginLaunchState::Launched as i32)
        .or_else(|| launches().next_back())?;
    snapshot
        .catalogue
        .versions
        .iter()
        .filter_map(|version| version.metadata.as_ref())
        .find(|metadata| metadata.name == launch.name && metadata.version == launch.version)?
        .declaration
        .clone()
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
    configuration.settings = declared_and_stored(snapshot, plugin_instance_id)
        .filter_map(|(declaration, held)| {
            let value = match held_reading(declaration, held, plugin_instance_id)? {
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
/// declared, the values that are not secret, and which secrets are set; and
/// who made the latest change, and when, from the change records, a clear
/// included.
///
/// A setting declared secret, or held sealed, is only ever named: whatever
/// the plugin declares later, nothing sealed is shown. A value that is not
/// secret is shown only if it reads under its declaration as it stands.
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
    for (declaration, held) in declared_and_stored(snapshot, plugin_instance_id) {
        match &held.held {
            Held::Plain(value) if !declaration.secret => {
                if held_reading(declaration, held, plugin_instance_id).is_some() {
                    record.values.push(PluginSettingValue {
                        name: held.name.clone(),
                        value: value.clone(),
                    });
                }
            }
            _ => record.secrets_set.push(held.name.clone()),
        }
    }
    if let Some(last) = snapshot.settings_changed.get(plugin_instance_id) {
        record.updated_at_ns = last.at_ns;
        record.updated_by = last.by.clone();
    }
    // Each setting's latest change, a secret's included, never its value
    // (contract v17).
    record.changes = snapshot
        .setting_changes
        .get(plugin_instance_id)
        .cloned()
        .unwrap_or_default();
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
    /// The external accounts each custody instance last reported (W2.8),
    /// as heard since this started: in memory, since a connector reports its
    /// whole list on each read, as the dashboard keeps it.
    reported: Mutex<BTreeMap<String, BTreeSet<String>>>,
    /// The deployment's archive, as its install named it (W7.1, W8.7).
    grant: crate::archive::ArchiveGrant,
    /// The kinds of raw record each plugin last declared (W4.1, W4.8).
    kinds: crate::archive::Kinds,
    /// The instances heard registered since this started: the first report
    /// of each publishes the data configuration, so its sidecar hears its
    /// entitlements (contract v18).
    registered: Mutex<BTreeSet<String>>,
}

impl Context {
    fn snapshot(&self) -> Result<Snapshot, String> {
        self.store.snapshot().map_err(|failed| failed.to_string())
    }

    /// What the instrument store says a record became (contract v24, W3.8):
    /// its own ID where it was not replaced; `None` where it holds none.
    fn instrument_became(&self, instrument_id: &str) -> Result<Option<String>, String> {
        let handle = tokio::runtime::Handle::try_current().map_err(|failed| failed.to_string())?;
        let (payload_type, payload) = handle
            .block_on(
                self.bus.call(
                    RESOLVE_INSTRUMENT,
                    "meridian.v1.ResolveInstrumentRequest",
                    ResolveInstrumentRequest {
                        instrument_id: instrument_id.to_string(),
                        as_of_ns: self.clock.now_ns(),
                    }
                    .encode_to_vec(),
                    None,
                    Some(INSTRUMENT_WAIT),
                ),
            )
            .map_err(|failed| failed.to_string())?;
        if payload_type != "meridian.v1.ResolveInstrumentReply" {
            return Err(format!("the instrument store answered {payload_type}"));
        }
        let reply =
            ResolveInstrumentReply::decode(&payload[..]).map_err(|failed| failed.to_string())?;
        Ok(reply
            .instrument
            .filter(|_| reply.found)
            .map(|record| record.instrument_id))
    }

    /// Re-point every instrument cell of a plugin's settings tables naming a
    /// replaced record (contract v24, the one-ID spec's requirement 10;
    /// W3.26): each table changed is a recorded settings change, its "who"
    /// the resolution rule naming the replacement, its rows stamped so; the
    /// page and the plugin then show one ID. Answers how many tables.
    pub(crate) fn repoint_settings(&self) -> Result<usize, String> {
        let before = self.snapshot()?;
        let now = self.clock.now_ns();
        let mut became: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut changed_tables = 0;
        let plugins: Vec<String> = before.declared_settings.keys().cloned().collect();
        for plugin in plugins {
            let mut changes = Vec::new();
            let mut said = Vec::new();
            for (declaration, held) in declared_and_stored(&before, &plugin) {
                if !setting_table::is_table(declaration) {
                    continue;
                }
                let columns: Vec<&str> = declaration
                    .columns
                    .iter()
                    .filter(|column| {
                        SettingColumnType::try_from(column.r#type)
                            == Ok(SettingColumnType::Instrument)
                    })
                    .map(|column| column.name.as_str())
                    .collect();
                if columns.is_empty() {
                    continue;
                }
                let Some(Held::Plain(text)) = held_reading(declaration, held, &plugin) else {
                    continue;
                };
                let Ok(rows) = setting_table::parse(text) else {
                    continue;
                };
                let mut cells: Vec<_> = rows.iter().map(|row| row.cells.clone()).collect();
                let mut moved = Vec::new();
                for row in cells.iter_mut() {
                    for column in &columns {
                        let Some(value) = row.get(*column).map(|v| v.trim().to_string()) else {
                            continue;
                        };
                        if value.is_empty() {
                            continue;
                        }
                        let now_is = match became.get(&value) {
                            Some(known) => known.clone(),
                            None => {
                                let known = self.instrument_became(&value)?;
                                became.insert(value.clone(), known.clone());
                                known
                            }
                        };
                        if let Some(now_is) = now_is.filter(|now_is| *now_is != value) {
                            row.insert(column.to_string(), now_is.clone());
                            moved.push(format!("{value} became {now_is}"));
                        }
                    }
                }
                if moved.is_empty() {
                    continue;
                }
                let by = format!("the resolution rule: {}", moved.join(", "));
                changes.push(SettingChange {
                    name: declaration.name.clone(),
                    held: Some(Held::Plain(setting_table::written(
                        &setting_table::stamped(&cells, &rows, &by, now),
                    ))),
                    hint: String::new(),
                });
                said.extend(moved);
            }
            if changes.is_empty() {
                continue;
            }
            let author = SettingsAuthor {
                by: "the resolution rule".into(),
                delegation: String::new(),
                client: String::new(),
                note: format!(
                    "re-pointed by core, the record each named having been replaced \
                     (contract v24, W3.8): {}",
                    said.join("; ")
                ),
                prepared_call_id: String::new(),
            };
            self.store
                .put_plugin_settings(&plugin, &changes, &author, now)
                .map_err(|failed| failed.to_string())?;
            changed_tables += changes.len();
            tracing::info!(
                plugin,
                tables = changes.len(),
                "settings cells re-pointed to the records they became"
            );
        }
        if changed_tables > 0 {
            self.announce(&before)?;
        }
        Ok(changed_tables)
    }

    /// A record by its ID, as the instrument store answers it from a
    /// handler's blocking thread; `None` where it holds none.
    fn instrument_record(
        &self,
        instrument_id: &str,
    ) -> Result<Option<meridian_domain::v1::InstrumentRecord>, String> {
        let handle = tokio::runtime::Handle::try_current().map_err(|failed| failed.to_string())?;
        let (payload_type, payload) = handle
            .block_on(
                self.bus.call(
                    RESOLVE_INSTRUMENT,
                    "meridian.v1.ResolveInstrumentRequest",
                    ResolveInstrumentRequest {
                        instrument_id: instrument_id.to_string(),
                        as_of_ns: self.clock.now_ns(),
                    }
                    .encode_to_vec(),
                    None,
                    Some(INSTRUMENT_WAIT),
                ),
            )
            .map_err(|failed| failed.to_string())?;
        if payload_type != "meridian.v1.ResolveInstrumentReply" {
            return Err(format!("the instrument store answered {payload_type}"));
        }
        let reply =
            ResolveInstrumentReply::decode(&payload[..]).map_err(|failed| failed.to_string())?;
        Ok(reply.instrument.filter(|_| reply.found))
    }

    /// Whether the instrument store holds a record by this ID, asked from a
    /// handler's blocking thread; or why it could not be told.
    fn instrument_held(&self, instrument_id: &str) -> Result<bool, String> {
        let unavailable = |why: String| {
            format!("the instrument store did not say whether it holds the record, and nothing was stored: {why}")
        };
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|failed| unavailable(failed.to_string()))?;
        let (payload_type, payload) = handle
            .block_on(
                self.bus.call(
                    RESOLVE_INSTRUMENT,
                    "meridian.v1.ResolveInstrumentRequest",
                    ResolveInstrumentRequest {
                        instrument_id: instrument_id.to_string(),
                        as_of_ns: self.clock.now_ns(),
                    }
                    .encode_to_vec(),
                    None,
                    Some(INSTRUMENT_WAIT),
                ),
            )
            .map_err(|failed| unavailable(failed.to_string()))?;
        if payload_type != "meridian.v1.ResolveInstrumentReply" {
            return Err(unavailable(format!("it answered {payload_type}")));
        }
        let reply = ResolveInstrumentReply::decode(&payload[..])
            .map_err(|failed| unavailable(failed.to_string()))?;
        Ok(reply.found && reply.instrument.is_some())
    }

    /// Every cell of a table update that names what this deployment does not
    /// hold, by its path as given (W6.11): an external account the plugin
    /// has not reported and does not link, an instrument the instrument
    /// store holds no record of. The conductor is the authority on both; the
    /// dashboard checks the same to say it on the form first.
    fn references_refused(
        &self,
        snapshot: &Snapshot,
        request: &SetPluginSettingsRequest,
    ) -> Result<(), String> {
        let plugin = request.plugin_instance_id.as_str();
        let Some(declared) = snapshot.declared_settings.get(plugin) else {
            return Ok(());
        };
        let reported: BTreeSet<String> = self
            .reported
            .lock()
            .map(|heard| heard.get(plugin).cloned().unwrap_or_default())
            .unwrap_or_default();
        let linked = |id: &str| {
            snapshot
                .links
                .iter()
                .any(|l| l.plugin_instance_id == plugin && l.external_account_id == id)
        };
        let mut instruments: BTreeMap<String, bool> = BTreeMap::new();
        let mut problems = Vec::new();
        for given in &request.values {
            let Some(declaration) = declared
                .iter()
                .find(|d| d.name == given.name && setting_table::is_table(d))
            else {
                continue;
            };
            let rows = setting_table::parse(&given.value)
                .map_err(|why| format!("setting {}: {why}", given.name))?;
            for (n, row) in rows.iter().enumerate() {
                for column in &declaration.columns {
                    let typed = row
                        .cells
                        .get(&column.name)
                        .map(|v| v.trim())
                        .unwrap_or_default();
                    if typed.is_empty() {
                        continue;
                    }
                    let path = format!("{}[{n}].{}", given.name, column.name);
                    match SettingColumnType::try_from(column.r#type) {
                        Ok(SettingColumnType::ExternalAccount)
                            if !reported.contains(typed) && !linked(typed) =>
                        {
                            problems.push(format!(
                                "{path}: names no external account this plugin reported or links"
                            ));
                        }
                        Ok(SettingColumnType::Instrument) => {
                            let held = match instruments.get(typed) {
                                Some(held) => *held,
                                None => {
                                    let held = self.instrument_held(typed)?;
                                    instruments.insert(typed.to_string(), held);
                                    held
                                }
                            };
                            if !held {
                                problems.push(format!(
                                    "{path}: names no instrument record this deployment holds"
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the settings name what this deployment does not hold, and nothing was stored: {}",
                problems.join("; ")
            ))
        }
    }

    /// The deployment's data configuration, published whole (W10.1).
    fn publish_data(&self) -> Result<(), String> {
        let event = crate::data::configuration(&self.snapshot()?, self.clock.now_ns());
        self.bus
            .publish(
                crate::data::ENTITLEMENTS_CHANGED,
                "meridian.v1.EntitlementsChangedEvent",
                event.encode_to_vec(),
                None,
                None,
            )
            .map(|_| ())
            .map_err(|failed| failed.to_string())
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

    // Granted by the first run that named them, no person signed in.
    let author = Author::default();
    store
        .install_first_admin(&group, &permissions, &author, now)
        .map_err(|failed| failed.to_string())
}

/// Who made a change the envelope carries: the person the dashboard stamped,
/// and the delegation they acted through where they used one, with its
/// client's name (contract v17).
pub(crate) fn author(envelope: &Envelope) -> Author {
    Author {
        by: subject(envelope),
        delegation: delegation(envelope),
        client: client(envelope),
        note: String::new(),
        prepared_call_id: prepared_call_id(envelope),
    }
}

/// The prepared change the person made the change from, as the dashboard
/// stamped it beside them (W4.9, W6.27, contract v20); empty otherwise, and
/// never without a person.
pub(crate) fn prepared_call_id(envelope: &Envelope) -> String {
    envelope
        .meta
        .as_ref()
        .filter(|meta| !meta.acting_for_subject.is_empty())
        .map(|meta| meta.prepared_call_id.clone())
        .unwrap_or_default()
}

/// The client's name the dashboard stamped beside the delegation (W4.9,
/// contract v10); empty without one.
pub(crate) fn client(envelope: &Envelope) -> String {
    envelope
        .meta
        .as_ref()
        .filter(|meta| !meta.acting_through_delegation.is_empty())
        .map(|meta| meta.acting_through_client.clone())
        .unwrap_or_default()
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
pub(crate) fn delegation(envelope: &Envelope) -> String {
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
/// plugin's sidecar; nothing else here holds it. A deployment with no
/// archive ([`serve_with`] names one).
pub fn serve(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    upstream: Arc<dyn Upstream>,
    key: Arc<SettingsKey>,
) {
    serve_with(
        bus,
        store,
        clock,
        upstream,
        key,
        crate::archive::ArchiveGrant::default(),
    )
}

/// One sweep of settings cells naming a replaced record (requirement 10),
/// for a test to drive without waiting for the hourly one.
/// The dashboard's AccessRecords as the store answers them from `snapshot`
/// (decisions/015): what the conductor serves on the bus, and what the
/// upgrade preflight measures against the broker's 1 MiB payload limit.
pub fn access_records_of(snapshot: Snapshot, now_ns: i64) -> meridian_domain::v1::AccessRecords {
    // Each connectivity plugin's link and each ems's map (W11.21,
    // W11.22, contract v22), for the dashboard's pages and its read
    // tool.
    let connectivity_links = snapshot.connectivity.values().cloned().collect();
    let venue_account_maps = crate::connectivity::maps(&snapshot);
    // Each known plugin's settings form (W6.11): what it declared,
    // and which secrets are set, never what they are.
    let plugin_settings = snapshot
        .plugins
        .iter()
        .map(|plugin| settings_record(&snapshot, &plugin.plugin_instance_id))
        .collect();
    let mut records = snapshot.records;
    records.read_at_ns = now_ns;
    records.plugin_settings = plugin_settings;
    // With the links, so the dashboard can tell which of the accounts
    // a connector reports have none (W2.8, W6.4). Only the dashboard
    // asks this, and it may already see every account.
    records.links = snapshot.links;
    // The holds standing, for the deployment's Settings and each
    // edge plugin's Summary (W6.25, contract v16).
    records.holds = snapshot.holds;
    // Each instance's linkable accounts (W6.30, contract v20).
    records.linkable_accounts = snapshot.linkable.into_values().collect();
    records.connectivity_links = connectivity_links;
    records.venue_account_maps = venue_account_maps;
    records
}

#[cfg(test)]
pub(crate) fn repoint_now(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    upstream: Arc<dyn Upstream>,
    key: Arc<SettingsKey>,
) -> Result<usize, String> {
    Context {
        bus,
        store,
        clock,
        upstream,
        key,
        reported: Mutex::new(BTreeMap::new()),
        grant: crate::archive::ArchiveGrant::default(),
        kinds: Mutex::new(BTreeMap::new()),
        registered: Mutex::new(BTreeSet::new()),
    }
    .repoint_settings()
}

/// [`serve`], for a deployment whose install named where archives are kept
/// (W7.1, W8.7, contract v16): what a write-once hold is refused against.
pub fn serve_with(
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn Clock>,
    upstream: Arc<dyn Upstream>,
    key: Arc<SettingsKey>,
    grant: crate::archive::ArchiveGrant,
) {
    let context = Arc::new(Context {
        bus: Arc::clone(&bus),
        store,
        clock,
        upstream,
        key,
        reported: Mutex::new(BTreeMap::new()),
        grant,
        kinds: Mutex::new(BTreeMap::new()),
        registered: Mutex::new(BTreeSet::new()),
    });

    // Links made before an account held one external account at most are
    // kept, and said, for a deployment admin to separate (W6.4). Off the
    // runtime, since the store blocks on its socket.
    // Settings cells naming a replaced record, re-pointed (contract v24,
    // requirement 10): swept soon after a start and then hourly, since the
    // conductor hears no replacement itself.
    let sweeping = Arc::clone(&context);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        loop {
            let cx = Arc::clone(&sweeping);
            match tokio::task::spawn_blocking(move || cx.repoint_settings()).await {
                Ok(Ok(_)) => {}
                Ok(Err(failed)) => {
                    tracing::info!(%failed, "settings were not re-pointed this time")
                }
                Err(failed) => tracing::warn!(%failed, "the settings sweep failed"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });

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
            Ok(access_records_of(snapshot, cx.clock.now_ns()))
        },
    );

    answer(
        &context,
        crate::archive::SET_HOLD,
        ("meridian.v1.SetHoldRequest", "meridian.v1.Hold"),
        |cx, request: meridian_domain::v1::SetHoldRequest, envelope| {
            let before = cx.snapshot()?;
            let hold = crate::archive::set_hold(
                cx.store.as_ref(),
                cx.grant,
                &request,
                &author(envelope),
                cx.clock.now_ns(),
            )?;
            // Each sidecar the hold covers reads hold_days afresh.
            cx.announce(&before)?;
            Ok(hold)
        },
    );

    answer(
        &context,
        crate::data::SET_DATASET_LICENCE,
        (
            "meridian.v1.SetDatasetLicenceRequest",
            "meridian.v1.DatasetLicence",
        ),
        |cx, request: meridian_domain::v1::SetDatasetLicenceRequest, envelope| {
            let licence = crate::data::set_licence(
                cx.store.as_ref(),
                &cx.snapshot()?,
                &request,
                &author(envelope),
                cx.clock.now_ns(),
            )?;
            cx.publish_data()?;
            Ok(licence)
        },
    );

    answer(
        &context,
        crate::data::SET_DATASET_ENTITLEMENT,
        (
            "meridian.v1.SetDatasetEntitlementRequest",
            "meridian.v1.DatasetEntitlement",
        ),
        |cx, request: meridian_domain::v1::SetDatasetEntitlementRequest, envelope| {
            let entitlement = crate::data::set_entitlement(
                cx.store.as_ref(),
                &cx.snapshot()?,
                &request,
                &author(envelope),
                cx.clock.now_ns(),
            )?;
            cx.publish_data()?;
            Ok(entitlement)
        },
    );

    // The data configuration, on the conductor's start and every minute
    // besides (W10.1): whoever started since hears it whole.
    let publishing = Arc::clone(&context);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(crate::data::REPUBLISH_EVERY);
        loop {
            every.tick().await;
            let cx = Arc::clone(&publishing);
            match tokio::task::spawn_blocking(move || cx.publish_data()).await {
                Ok(Ok(())) => {}
                Ok(Err(failed)) => {
                    tracing::warn!("the data configuration was not published: {failed}")
                }
                Err(failed) => {
                    tracing::warn!("the data configuration was not published: {failed}")
                }
            }
        }
    });

    answer(
        &context,
        crate::archive::RECORD_MOVE,
        (
            "meridian.v1.RecordMoveRequest",
            "meridian.v1.RecordMoveReply",
        ),
        |cx, request: meridian_pb::v1::RecordMoveRequest, envelope| {
            crate::archive::record_move(
                cx.store.as_ref(),
                &cx.snapshot()?,
                &publisher(envelope),
                request,
                &author(envelope),
                cx.clock.now_ns(),
            )
        },
    );

    answer(
        &context,
        crate::archive::READ_MOVES,
        ("meridian.v1.ReadMovesRequest", "meridian.v1.ReadMovesReply"),
        |cx, request: meridian_domain::v1::ReadMovesRequest, _| {
            crate::archive::read_moves(cx.store.as_ref(), &cx.snapshot()?, &request)
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
            // A change against a record changed since it was read is refused,
            // naming the field (contract v17): a table replaced whole from an
            // old read would drop a person's rows. Every change carries the
            // guard (contract v18, W6.11; v17's security review, I1): only
            // the dashboard sends this command, so 0 reads as "against a
            // record never saved", refused once anything has been saved.
            let standing = settings_record(&before, plugin).updated_at_ns;
            if standing != request.against_updated_at_ns {
                let said = if request.against_updated_at_ns == 0 {
                    format!(
                        "the settings of {plugin} were saved at {standing}, and this change \
                         is against a record never saved; read them again, and nothing was \
                         changed"
                    )
                } else {
                    format!(
                        "the settings of {plugin} changed since they were read (now \
                         {standing}, read at {}); read them again, and nothing was changed",
                        request.against_updated_at_ns
                    )
                };
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::RecordChanged as i32,
                    &["against_updated_at_ns".to_string()],
                    said,
                ));
            }
            if let Some(refusal) = note_refused(&request.note) {
                return Err(refusal);
            }
            // Every value checked, and every secret sealed, before anything
            // is written: a refusal part-way through changes nothing.
            let checked = rules::plugin_settings(&before, &request)?;
            cx.references_refused(&before, &request)?;
            // A window never below the hold, and archived only where an
            // archive is allowed and the kind archivable (W6.11, contract
            // v16), naming the setting.
            let kinds = cx
                .kinds
                .lock()
                .ok()
                .and_then(|kinds| kinds.get(plugin).cloned());
            if let Some(refusal) =
                crate::archive::windows_refused(&before, &request, kinds.as_deref())
            {
                return Err(refusal);
            }
            let mut changes = Vec::new();
            for (declaration, value) in checked {
                let name = declaration.name.clone();
                // Never a value that is not secret over one held sealed: the
                // form shows a sealed value as a secret's field, and what is
                // typed there is stored in plain text and recorded. Cleared
                // first, it may be set.
                let sealed = stored_settings(&before, plugin)
                    .any(|held| held.name == name && matches!(held.held, Held::Sealed(_)));
                if value.is_some() && sealed && !declaration.secret {
                    return Err(format!(
                        "setting {name} holds a sealed value and is no longer declared secret; \
                         clear it, then set it"
                    ));
                }
                // A secret's last four, for one long enough, taken before it
                // is sealed and kept apart from it (W6.11, contract v20).
                let hint = match &value {
                    Some(value) if declaration.secret => crate::store::hint_of(value),
                    _ => String::new(),
                };
                let held = match value {
                    None => None,
                    // A table's rows, each added or changed stamped with who
                    // and when, an unchanged one keeping its own (W6.11).
                    Some(value) if setting_table::is_table(declaration) => {
                        let rows = setting_table::parse(&value)
                            .map_err(|why| format!("setting {name}: {why}"))?;
                        // The stamps of the rows held, only where what is
                        // held reads as this table: a value from an earlier
                        // declaration lends no row its stamps.
                        let standing = declared_and_stored(&before, plugin)
                            .find(|(_, held)| held.name == name)
                            .and_then(|(declared, held)| held_reading(declared, held, plugin))
                            .and_then(|held| match held {
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
                changes.push(SettingChange { name, held, hint });
            }
            if !changes.is_empty() {
                let author = SettingsAuthor {
                    note: request.note.clone(),
                    ..author(envelope)
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
            // Why, on each grant's withdrawal; through a delegation required
            // (approved 2026-10-15).
            let note = request.note.trim();
            if let Some(refused) = crate::store::note_refused(note) {
                return Err(refused);
            }
            if note.is_empty() && !delegation(envelope).is_empty() {
                return Err(meridian_bus::refusal(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    "through a delegation closing an account says why",
                ));
            }
            // Never left dangling (approved 2026-10-14): an account still
            // linked, granted or mapped is refused, naming each. "Unlink all
            // and close" (contract v24, W6.3) withdraws the grants in the
            // same step; a link or an ems's mapping is undone at its
            // plugin's own page, so while one stands nothing is withdrawn
            // or closed.
            let attached = rules::attached(&before, &request.account_id);
            let standing: Vec<&rules::Attached> = attached
                .iter()
                .filter(|item| {
                    !(request.unlink_attached && matches!(item, rules::Attached::Grant { .. }))
                })
                .collect();
            if !standing.is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::Unspecified as i32,
                    &["account_id".to_string()],
                    format!(
                        "account {} is still attached, and nothing was {}: {}",
                        request.account_id,
                        if request.unlink_attached {
                            "withdrawn or closed"
                        } else {
                            "closed"
                        },
                        standing
                            .iter()
                            .map(|item| item.words())
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                ));
            }
            let mut account = before
                .records
                .accounts
                .iter()
                .find(|a| a.account_id == request.account_id)
                .cloned()
                .expect("the rule checked it exists");
            account.state = AccountState::Closed as i32;
            if attached.is_empty() {
                cx.store.put_account(&account).map_err(|f| f.to_string())?;
            } else {
                let withdrawn_from = cx
                    .store
                    .close_account_withdrawing_grants(
                        &account,
                        &Author {
                            note: if note.is_empty() {
                                "withdrawn with the account's closure: Unlink all and close (W6.3)"
                                    .into()
                            } else {
                                format!(
                                    "withdrawn with the account's closure: Unlink all and close \
                                     (W6.3): {note}"
                                )
                            },
                            ..author(envelope)
                        },
                        cx.clock.now_ns(),
                    )
                    .map_err(|f| f.to_string())?;
                tracing::info!(
                    account = account.account_id,
                    withdrawn = withdrawn_from.len(),
                    by = subject(envelope),
                    "grants withdrawn with the account's closure"
                );
            }
            tracing::info!(
                account = account.account_id,
                by = subject(envelope),
                "account closed"
            );
            cx.announce(&before)?;
            Ok(account)
        },
    );

    // W3.28 (contract v24): "Report identifier usage", the deployment
    // admin's, on by default, each change its own record.
    answer(
        &context,
        SET_REPORTS_USAGE,
        (
            "meridian.v1.SetReportsUsageRequest",
            "meridian.v1.ReportsUsageSetting",
        ),
        |cx, request: meridian_domain::v1::SetReportsUsageRequest, envelope| {
            let by = subject(envelope);
            if by.is_empty() {
                return Err(
                    "the usage report's setting is a deployment admin's, and this is sent for nobody"
                        .into(),
                );
            }
            if request.reason.trim().is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    &["reason".to_string()],
                    "reason: a change to the usage report's setting says why",
                ));
            }
            if request.reason.chars().count() > 4_000 {
                return Err("reason: at most 4,000 characters".into());
            }
            if cx.store.reports_usage().map_err(|f| f.to_string())? != request.reports_usage {
                cx.store
                    .set_reports_usage(
                        request.reports_usage,
                        &Author {
                            note: request.reason.trim().to_string(),
                            ..author(envelope)
                        },
                        cx.clock.now_ns(),
                    )
                    .map_err(|f| f.to_string())?;
                tracing::info!(
                    on = request.reports_usage,
                    by,
                    "the usage report's setting changed"
                );
            }
            Ok(meridian_domain::v1::ReportsUsageSetting {
                reports_usage: cx.store.reports_usage().map_err(|f| f.to_string())?,
                licensed_schemes: cx.store.licensed_schemes().map_err(|f| f.to_string())?,
            })
        },
    );

    // W12.6 (contract v26; ruling 285): the analytics routing table, a
    // deployment admin's, each change its own record; read by the dashboard
    // and by the analytics component at every job.
    answer(
        &context,
        SET_MODEL_ROUTING,
        (
            "meridian.v1.SetModelRoutingRequest",
            "meridian.v1.ListModelRoutingReply",
        ),
        |cx, request: meridian_domain::v1::SetModelRoutingRequest, envelope| {
            let by = subject(envelope);
            if by.is_empty() {
                return Err(meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::PermissionDenied,
                    &[],
                    "routing a model id is a deployment admin's, and this is sent for nobody",
                ));
            }
            let model = request.model_id.trim();
            if !MODEL_IDS.contains(&model) {
                return Err(meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::InvalidArgument,
                    &["model_id".to_string()],
                    format!(
                        "model_id: {model:?} is not a model id v26 serves ({})",
                        MODEL_IDS.join(", ")
                    ),
                ));
            }
            if request.reason.trim().is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    &["reason".to_string()],
                    "reason: a change to the routing table says why",
                ));
            }
            if request.reason.chars().count() > 4_000 {
                return Err("reason: at most 4,000 characters".into());
            }
            let implementation = request.implementation.trim();
            if implementation != BUILT_IN && !runs_pricing(&cx.snapshot()?, implementation) {
                return Err(format!(
                    "failed_precondition: [fields=implementation] implementation: \
                     {implementation:?} is not a pricing instance this deployment runs; \
                     route to built-in or to a launched pricing instance"
                ));
            }
            // Contract v27 (W12.3, W12.6; ruling 324): a pricing instance
            // whose launched version declares its models must list this
            // one; a version built before v27 declares none and is read as
            // before.
            if implementation != BUILT_IN {
                let declared = launched_declaration(&cx.snapshot()?, implementation)
                    .map(|declaration| declaration.pricing_models.clone())
                    .unwrap_or_default();
                if !declared.is_empty() && !declared.iter().any(|m| m.model_id == model) {
                    let listed: Vec<String> = declared
                        .iter()
                        .map(|m| format!("{} v{}", m.model_id, m.model_version))
                        .collect();
                    return Err(format!(
                        "failed_precondition: [fields=implementation] implementation: \
                         {implementation}'s launched version declares the models {}, not \
                         {model}; route {model} to built-in or to an instance declaring it",
                        listed.join(", ")
                    ));
                }
            }
            let changes = cx
                .store
                .model_routing_changes()
                .map_err(|f| f.to_string())?;
            let now = model_routing(&changes);
            let held = now
                .routes
                .iter()
                .find(|r| r.model_id == model)
                .map(|r| r.implementation.clone())
                .unwrap_or_default();
            if held != implementation {
                cx.store
                    .set_model_routing(
                        model,
                        implementation,
                        &Author {
                            note: request.reason.trim().to_string(),
                            ..author(envelope)
                        },
                        cx.clock.now_ns(),
                    )
                    .map_err(|f| f.to_string())?;
                tracing::info!(model, implementation, by, "a model id was routed");
            }
            Ok(model_routing(
                &cx.store
                    .model_routing_changes()
                    .map_err(|f| f.to_string())?,
            ))
        },
    );
    answer(
        &context,
        LIST_MODEL_ROUTING,
        (
            "meridian.v1.ListModelRoutingRequest",
            "meridian.v1.ListModelRoutingReply",
        ),
        |cx, _request: meridian_domain::v1::ListModelRoutingRequest, _| {
            Ok(model_routing(
                &cx.store
                    .model_routing_changes()
                    .map_err(|f| f.to_string())?,
            ))
        },
    );

    // W6.35 (contract v26): the deployment's time zone, a deployment admin's,
    // each change its own record; and its read, by the book and the
    // dashboard.
    answer(
        &context,
        SET_DEPLOYMENT_TIME_ZONE,
        (
            "meridian.v1.SetDeploymentTimeZoneRequest",
            "meridian.v1.DeploymentTimeZoneSetting",
        ),
        |cx, request: meridian_domain::v1::SetDeploymentTimeZoneRequest, envelope| {
            let by = subject(envelope);
            if by.is_empty() {
                return Err(
                    "the deployment's time zone is a deployment admin's, and this is sent for nobody"
                        .into(),
                );
            }
            let zone = request.deployment_time_zone.trim();
            if !meridian_domain::zones::is_zone(zone) {
                return Err(meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::InvalidArgument,
                    &["deployment_time_zone".to_string()],
                    format!(
                        "deployment_time_zone: {zone:?} is not an IANA time zone \
                         (America/New_York, Europe/London, Asia/Tokyo)"
                    ),
                ));
            }
            if request.reason.trim().is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    &["reason".to_string()],
                    "reason: a change to the deployment's time zone says why",
                ));
            }
            if request.reason.chars().count() > 4_000 {
                return Err("reason: at most 4,000 characters".into());
            }
            let held = cx.store.deployment_time_zone().map_err(|f| f.to_string())?;
            let now = held
                .as_ref()
                .map(|held| held.zone.as_str())
                .unwrap_or(DEFAULT_TIME_ZONE);
            if now != zone || held.is_none() {
                cx.store
                    .set_deployment_time_zone(
                        zone,
                        &Author {
                            note: request.reason.trim().to_string(),
                            ..author(envelope)
                        },
                        cx.clock.now_ns(),
                    )
                    .map_err(|f| f.to_string())?;
                tracing::info!(zone, by, "the deployment's time zone changed");
            }
            Ok(time_zone_setting(
                &cx.snapshot()?,
                cx.store.deployment_time_zone().map_err(|f| f.to_string())?,
            ))
        },
    );
    answer(
        &context,
        READ_DEPLOYMENT_TIME_ZONE,
        (
            "meridian.v1.ReadDeploymentTimeZoneRequest",
            "meridian.v1.DeploymentTimeZoneSetting",
        ),
        |cx, _request: meridian_domain::v1::ReadDeploymentTimeZoneRequest, _| {
            Ok(time_zone_setting(
                &cx.snapshot()?,
                cx.store.deployment_time_zone().map_err(|f| f.to_string())?,
            ))
        },
    );

    // W6.36 (contract v27; rulings 462 and 483): the deployment's FX pivot
    // currencies, a deployment admin's, each change its own record; read by
    // the analytics component as it converts a job's legs, and by the
    // dashboard.
    answer(
        &context,
        SET_FX_PIVOT_CURRENCIES,
        (
            "meridian.v1.SetFxPivotCurrenciesRequest",
            "meridian.v1.FxPivotCurrenciesSetting",
        ),
        |cx, request: meridian_domain::v1::SetFxPivotCurrenciesRequest, envelope| {
            let by = subject(envelope);
            if by.is_empty() {
                return Err(
                    "the deployment's FX pivot currencies are a deployment admin's, and this is \
                     sent for nobody"
                        .into(),
                );
            }
            let invalid = |field: String, words: String| {
                meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::InvalidArgument,
                    &[field],
                    words,
                )
            };
            let pivots: Vec<String> = request
                .fx_pivot_currencies
                .iter()
                .map(|p| p.trim().to_string())
                .collect();
            if pivots.is_empty() || pivots.len() > MOST_PIVOTS {
                return Err(invalid(
                    "fx_pivot_currencies".into(),
                    format!(
                        "fx_pivot_currencies names {}; one to {MOST_PIVOTS} fiat cash instruments",
                        pivots.len()
                    ),
                ));
            }
            for (n, pivot) in pivots.iter().enumerate() {
                let field = format!("fx_pivot_currencies[{n}]");
                if pivots[..n].contains(pivot) {
                    return Err(invalid(field, format!("{pivot} is named twice")));
                }
                let record = cx.instrument_record(pivot)?;
                let fiat = record.as_ref().is_some_and(|r| {
                    r.asset_class == meridian_domain::v1::AssetClass::Cash as i32
                        && r.identifiers.iter().any(|i| i.scheme == "iso4217")
                });
                if !fiat {
                    return Err(invalid(
                        field,
                        format!(
                            "{pivot} is no fiat currency's cash instrument: a pivot is never a \
                             crypto asset or a stablecoin (ruling 462)"
                        ),
                    ));
                }
            }
            if request.reason.trim().is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    &["reason".to_string()],
                    "reason: a change to the FX pivot currencies says why",
                ));
            }
            if request.reason.chars().count() > 4_000 {
                return Err("reason: at most 4,000 characters".into());
            }
            let held = cx.store.fx_pivot_currencies().map_err(|f| f.to_string())?;
            if held.as_ref().map(|h| &h.fx_pivot_currencies) != Some(&pivots) {
                cx.store
                    .set_fx_pivot_currencies(
                        &pivots,
                        &Author {
                            note: request.reason.trim().to_string(),
                            ..author(envelope)
                        },
                        cx.clock.now_ns(),
                    )
                    .map_err(|f| f.to_string())?;
                tracing::info!(
                    pivots = pivots.join(","),
                    by,
                    "the FX pivot currencies changed"
                );
            }
            Ok(fx_pivot_setting(
                cx.store.fx_pivot_currencies().map_err(|f| f.to_string())?,
            ))
        },
    );
    answer(
        &context,
        READ_FX_PIVOT_CURRENCIES,
        (
            "meridian.v1.ReadFxPivotCurrenciesRequest",
            "meridian.v1.FxPivotCurrenciesSetting",
        ),
        |cx, _request: meridian_domain::v1::ReadFxPivotCurrenciesRequest, _| {
            Ok(fx_pivot_setting(
                cx.store.fx_pivot_currencies().map_err(|f| f.to_string())?,
            ))
        },
    );

    // W6.37 (contract v28; rulings 561 and 589): the series request rule, a
    // deployment admin's, each change its own record; read by the
    // instrument store before it asks the platform for a series, and by the
    // dashboard.
    answer(
        &context,
        SET_SERIES_REQUEST_RULE,
        (
            "meridian.v1.SetSeriesRequestRuleRequest",
            "meridian.v1.SeriesRequestRule",
        ),
        |cx, request: meridian_domain::v1::SetSeriesRequestRuleRequest, envelope| {
            let by = subject(envelope);
            if by.is_empty() {
                return Err(
                    "the series request rule is a deployment admin's, and this is sent for nobody"
                        .into(),
                );
            }
            let rule = request.rule.unwrap_or_default();
            if !(1..=5_000).contains(&rule.daily_limit) {
                return Err(meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::InvalidArgument,
                    &["rule.daily_limit".to_string()],
                    format!(
                        "rule.daily_limit is {}; 1 to 5,000 series a day",
                        rule.daily_limit
                    ),
                ));
            }
            if request.reason.trim().is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    &["reason".to_string()],
                    "reason: a change to the series request rule says why",
                ));
            }
            if request.reason.chars().count() > 4_000 {
                return Err("reason: at most 4,000 characters".into());
            }
            let value = meridian_domain::v1::SeriesRequestRule {
                orders: rule.orders,
                standing_wants: rule.standing_wants,
                daily_limit: rule.daily_limit,
                changed_at_ns: 0,
                changed_by: String::new(),
            };
            let held = series_request_rule(
                cx.store
                    .setting(crate::store::SERIES_REQUEST_RULE)
                    .map_err(|f| f.to_string())?,
            );
            let unchanged = (held.orders, held.standing_wants, held.daily_limit)
                == (value.orders, value.standing_wants, value.daily_limit)
                && held.changed_at_ns != 0;
            if !unchanged {
                cx.store
                    .set_setting(
                        crate::store::SERIES_REQUEST_RULE,
                        &value.encode_to_vec(),
                        &Author {
                            note: request.reason.trim().to_string(),
                            ..author(envelope)
                        },
                        cx.clock.now_ns(),
                    )
                    .map_err(|f| f.to_string())?;
                tracing::info!(
                    orders = value.orders,
                    standing_wants = value.standing_wants,
                    daily_limit = value.daily_limit,
                    by,
                    "the series request rule changed"
                );
            }
            Ok(series_request_rule(
                cx.store
                    .setting(crate::store::SERIES_REQUEST_RULE)
                    .map_err(|f| f.to_string())?,
            ))
        },
    );
    answer(
        &context,
        READ_SERIES_REQUEST_RULE,
        (
            "meridian.v1.ReadSeriesRequestRuleRequest",
            "meridian.v1.SeriesRequestRule",
        ),
        |cx, _request: meridian_domain::v1::ReadSeriesRequestRuleRequest, _| {
            Ok(series_request_rule(
                cx.store
                    .setting(crate::store::SERIES_REQUEST_RULE)
                    .map_err(|f| f.to_string())?,
            ))
        },
    );

    // W6.38 (contract v28; rulings 515, 517, 600): whether an ems plugin's
    // admin may choose warn beside refuse on a stale reference price, a
    // deployment admin's, each change its own record; read by an ems at its
    // own role and by the dashboard.
    answer(
        &context,
        SET_STALE_REFERENCE_ACTIONS_PERMITTED,
        (
            "meridian.v1.SetStaleReferenceActionsPermittedRequest",
            "meridian.v1.StaleReferenceActionsPermittedSetting",
        ),
        |cx, request: meridian_domain::v1::SetStaleReferenceActionsPermittedRequest, envelope| {
            let by = subject(envelope);
            if by.is_empty() {
                return Err(
                    "the stale-reference actions permitted are a deployment admin's, and this is \
                     sent for nobody"
                        .into(),
                );
            }
            if request.reason.trim().is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    &["reason".to_string()],
                    "reason: a change to the stale-reference actions permitted says why",
                ));
            }
            if request.reason.chars().count() > 4_000 {
                return Err("reason: at most 4,000 characters".into());
            }
            let held = stale_reference_actions_permitted(
                cx.store
                    .setting(crate::store::STALE_REFERENCE_ACTIONS_PERMITTED)
                    .map_err(|f| f.to_string())?,
            );
            if held.changed_at_ns == 0 || held.warn_permitted != request.warn_permitted {
                cx.store
                    .set_setting(
                        crate::store::STALE_REFERENCE_ACTIONS_PERMITTED,
                        &meridian_domain::v1::StaleReferenceActionsPermittedSetting {
                            warn_permitted: request.warn_permitted,
                            ..Default::default()
                        }
                        .encode_to_vec(),
                        &Author {
                            note: request.reason.trim().to_string(),
                            ..author(envelope)
                        },
                        cx.clock.now_ns(),
                    )
                    .map_err(|f| f.to_string())?;
                tracing::info!(
                    warn_permitted = request.warn_permitted,
                    by,
                    "the stale-reference actions permitted changed"
                );
            }
            Ok(stale_reference_actions_permitted(
                cx.store
                    .setting(crate::store::STALE_REFERENCE_ACTIONS_PERMITTED)
                    .map_err(|f| f.to_string())?,
            ))
        },
    );
    answer(
        &context,
        READ_STALE_REFERENCE_ACTIONS_PERMITTED,
        (
            "meridian.v1.ReadStaleReferenceActionsPermittedRequest",
            "meridian.v1.StaleReferenceActionsPermittedSetting",
        ),
        |cx, _request: meridian_domain::v1::ReadStaleReferenceActionsPermittedRequest, _| {
            Ok(stale_reference_actions_permitted(
                cx.store
                    .setting(crate::store::STALE_REFERENCE_ACTIONS_PERMITTED)
                    .map_err(|f| f.to_string())?,
            ))
        },
    );

    // W3.28 (contract v25): the identifier schemes the deployment declares
    // it is licensed for, the deployment admin's, the whole list each time,
    // each a scheme the licence list names, each change its own record.
    answer(
        &context,
        SET_LICENSED_SCHEMES,
        (
            "meridian.v1.SetLicensedSchemesRequest",
            "meridian.v1.ReportsUsageSetting",
        ),
        |cx, request: meridian_domain::v1::SetLicensedSchemesRequest, envelope| {
            let by = subject(envelope);
            if by.is_empty() {
                return Err(
                    "the licensed schemes are a deployment admin's, and this is sent for nobody"
                        .into(),
                );
            }
            if request.reason.trim().is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    &["reason".to_string()],
                    "reason: a change to the licensed schemes says why",
                ));
            }
            if request.reason.chars().count() > 4_000 {
                return Err("reason: at most 4,000 characters".into());
            }
            let listed = cx.upstream.licence_schemes();
            let schemes = licensed_schemes_held(&request.licensed_schemes, listed.as_deref())?;
            if cx.store.licensed_schemes().map_err(|f| f.to_string())? != schemes {
                cx.store
                    .set_licensed_schemes(
                        &schemes,
                        &Author {
                            note: request.reason.trim().to_string(),
                            ..author(envelope)
                        },
                        cx.clock.now_ns(),
                    )
                    .map_err(|f| f.to_string())?;
                tracing::info!(
                    schemes = schemes.join(","),
                    by,
                    "the licensed schemes changed"
                );
            }
            Ok(meridian_domain::v1::ReportsUsageSetting {
                reports_usage: cx.store.reports_usage().map_err(|f| f.to_string())?,
                licensed_schemes: schemes,
            })
        },
    );

    answer(
        &context,
        LIST_USAGE_REPORTS,
        (
            "meridian.v1.ListUsageReportsRequest",
            "meridian.v1.ListUsageReportsReply",
        ),
        |cx, _request: meridian_domain::v1::ListUsageReportsRequest, _| {
            Ok(meridian_domain::v1::ListUsageReportsReply {
                reports: cx.store.usage_reports(52).map_err(|f| f.to_string())?,
                // The setting in force, so the page says whether the report
                // is on (W3.28, contract v24), and the schemes declared
                // (contract v25).
                reports_usage: cx.store.reports_usage().map_err(|f| f.to_string())?,
                licensed_schemes: cx.store.licensed_schemes().map_err(|f| f.to_string())?,
            })
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
            // Expected refusals carry their category (contract v25), as the
            // rules' do: a link sent for nobody, or naming another plugin.
            let by = admin_acting(envelope, "a link").map_err(|words| {
                meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::PermissionDenied,
                    &[],
                    words,
                )
            })?;
            if publisher(envelope) != request.plugin_instance_id {
                return Err(meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::PermissionDenied,
                    &["plugin_instance_id".to_string()],
                    format!(
                        "a plugin links only its own external accounts, and this names {}",
                        request.plugin_instance_id
                    ),
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
                // A new account named by a deployment admin through a link is
                // granted the instance with it, its own record (W6.30).
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
                cx.store
                    .set_linkable_accounts(
                        &link.plugin_instance_id,
                        std::slice::from_ref(&account.account_id),
                        &[],
                        &Author {
                            note: "granted with the new account a deployment admin named in a link"
                                .into(),
                            ..author(envelope)
                        },
                        now,
                    )
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
            // and no holdings, which are not the configuration's. From
            // contract v20 only those a deployment admin granted the asking
            // instance (W6.30), nothing by default.
            let snapshot = cx.snapshot()?;
            let instance = publisher(envelope);
            let granted = snapshot
                .linkable
                .get(&instance)
                .map(|linkable| linkable.account_ids.clone())
                .unwrap_or_default();
            Ok(Accounts {
                accounts: snapshot
                    .records
                    .accounts
                    .into_iter()
                    .filter(|account| granted.contains(&account.account_id))
                    .collect(),
            })
        },
    );

    answer(
        &context,
        SET_LINKABLE_ACCOUNTS,
        (
            "meridian.v1.SetLinkableAccountsRequest",
            "meridian.v1.LinkableAccounts",
        ),
        |cx, request: meridian_domain::v1::SetLinkableAccountsRequest, envelope| {
            let by = subject(envelope);
            if by.is_empty() {
                return Err(
                    "a grant of linkable accounts is a deployment admin's, and this is sent for nobody"
                        .into(),
                );
            }
            let before = cx.snapshot()?;
            rules::linkable(&before, &request, !delegation(envelope).is_empty())?;
            let linkable = cx
                .store
                .set_linkable_accounts(
                    &request.plugin_instance_id,
                    &request.grant_account_ids,
                    &request.withdraw_account_ids,
                    &Author {
                        note: request.note.clone(),
                        ..author(envelope)
                    },
                    cx.clock.now_ns(),
                )
                .map_err(|f| f.to_string())?;
            tracing::info!(
                plugin = request.plugin_instance_id,
                granted = request.grant_account_ids.len(),
                withdrawn = request.withdraw_account_ids.len(),
                by,
                "linkable accounts changed"
            );
            cx.announce(&before)?;
            Ok(linkable)
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
                .put_access_group(&group, &author(envelope), cx.clock.now_ns())
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
                .add_permission(&permission, &author(envelope), cx.clock.now_ns())
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
            // Contract v28 (W6.7, ruling 549): why, required, on the
            // withdrawal's own record (decisions/031).
            let note = request.note.trim();
            if note.is_empty() {
                return Err(meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::ReasonRequired as i32,
                    &["note".to_string()],
                    "note: a withdrawal of a permission says why",
                ));
            }
            if note.chars().count() > 2_000 {
                return Err(meridian_bus::refusal_with_status(
                    meridian_bus::RefusalStatus::InvalidArgument,
                    &["note".to_string()],
                    "note: at most 2,000 characters".to_string(),
                ));
            }
            let before = cx.snapshot()?;
            let outcome = cx
                .store
                .withdraw_permission(
                    &request.permission_id,
                    &Author {
                        note: note.to_string(),
                        ..author(envelope)
                    },
                    cx.clock.now_ns(),
                )
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
            // Refused here, before the platform is asked, so a code is not
            // spent on a deployment that cannot use it. A permission held
            // only by a directory group nobody signed in with -- the
            // wizard's, misspelt -- holds for nobody, so the claim goes on
            // (design/a-misspelt-administrators-group, ruled 2026-10-10).
            if meridian_access::deployment_admin_held(&cx.snapshot()?.records) {
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
                .install_claimed_admin(&group, &permissions, &author(envelope), now)
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

    // W11.21: a connectivity plugin made available or withdrawn, for a
    // deployment admin at the page; linked to an `ems`, or unlinked, and
    // made available to its users, for the `ems`'s admin at the page. Each
    // change its own record; each plugin whose configuration it changes is
    // told (contract v22).
    answer(
        &context,
        crate::connectivity::SET_CONNECTIVITY_AVAILABLE,
        (
            "meridian.v1.SetConnectivityAvailableRequest",
            "meridian.v1.ConnectivityLink",
        ),
        |cx, request: meridian_domain::v1::SetConnectivityAvailableRequest, envelope| {
            let before = cx.snapshot()?;
            let author = Author {
                note: request.note.clone(),
                ..author(envelope)
            };
            let link = crate::connectivity::availability(&before, &request, &author)?;
            let link = cx
                .store
                .set_connectivity_link(&link, &author, cx.clock.now_ns())
                .map_err(|f| f.to_string())?;
            tracing::info!(
                connectivity = link.connectivity_instance_id,
                available = link.available,
                by = author.by,
                "connectivity availability set"
            );
            cx.announce(&before)?;
            Ok(link)
        },
    );

    answer(
        &context,
        crate::connectivity::LINK_CONNECTIVITY,
        (
            "meridian.v1.LinkConnectivityRequest",
            "meridian.v1.ConnectivityLink",
        ),
        |cx, request: meridian_domain::v1::LinkConnectivityRequest, envelope| {
            let before = cx.snapshot()?;
            let author = Author {
                note: request.note.clone(),
                ..author(envelope)
            };
            let link = crate::connectivity::linked(&before, &request, &author)?;
            let link = cx
                .store
                .set_connectivity_link(&link, &author, cx.clock.now_ns())
                .map_err(|f| f.to_string())?;
            tracing::info!(
                connectivity = link.connectivity_instance_id,
                ems = link.ems_instance_id,
                users = link.users.len(),
                by = author.by,
                "connectivity link set"
            );
            cx.announce(&before)?;
            Ok(link)
        },
    );

    // W11.22: an `ems`'s venue account mapping, sent by the `ems` for its
    // admin and stamped with its instance by its sidecar; answered by the
    // `ems`'s map as it now stands.
    answer(
        &context,
        crate::connectivity::MAP_VENUE_ACCOUNT,
        (
            "meridian.v1.MapVenueAccountRequest",
            "meridian.v1.VenueAccountMap",
        ),
        |cx, request: meridian_domain::v1::MapVenueAccountRequest, envelope| {
            let ems = publisher(envelope);
            let before = cx.snapshot()?;
            let author = author(envelope);
            let (from, to) = crate::connectivity::mapped(&before, &request, &ems, &author)?;
            cx.store
                .map_venue_account(&ems, from.as_ref(), to.as_ref(), &author, cx.clock.now_ns())
                .map_err(|f| f.to_string())?;
            tracing::info!(
                ems,
                connectivity = request.connectivity_instance_id,
                code = request.venue_account_code,
                account = request.account_id,
                by = author.by,
                "venue account mapping set"
            );
            cx.announce(&before)?;
            Ok(crate::connectivity::told(&cx.snapshot()?, &ems)
                .venue_account_map
                .unwrap_or_else(|| meridian_pb::v1::VenueAccountMap {
                    ems_instance_id: ems.clone(),
                    mappings: Vec::new(),
                }))
        },
    );

    // W11.20: the latest catalogue for a plugin and code, read by an `ems`
    // or an `oms` as itself.
    answer(
        &context,
        crate::connectivity::READ_VENUE_CATALOGUE,
        (
            "meridian.v1.ReadVenueCatalogueRequest",
            "meridian.v1.ReadVenueCatalogueReply",
        ),
        |cx, request: meridian_domain::v1::ReadVenueCatalogueRequest, _| {
            let held = cx
                .store
                .venue_catalogue(
                    request.connectivity_instance_id.trim(),
                    request.venue_account_code.trim(),
                )
                .map_err(|f| f.to_string())?;
            crate::connectivity::read_answer(held, &request)
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
    let mut sign_ins = bus.subscribe_shared(PERSON_SIGNED_IN, "sign-ins");
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

    // The external accounts each custody instance reports, by the instance
    // its topic names, which the broker lets only that instance publish.
    let mut accounts = bus.subscribe_shared(EXTERNAL_ACCOUNTS, "external-accounts");
    let hearing = Arc::clone(&context);
    tokio::spawn(async move {
        while let Some(delivery) = accounts.recv().await {
            let topic = delivery
                .envelope
                .meta
                .as_ref()
                .map(|meta| meta.topic.clone())
                .unwrap_or_default();
            let instance = match topic.split('.').collect::<Vec<_>>()[..] {
                ["platform", "custody", instance, ..] if !instance.is_empty() => {
                    instance.to_string()
                }
                _ => continue,
            };
            match ExternalAccountsEvent::decode(&delivery.envelope.payload[..]) {
                Ok(event) => {
                    if let Ok(mut heard) = hearing.reported.lock() {
                        heard.insert(
                            instance,
                            event
                                .accounts
                                .into_iter()
                                .map(|account| account.external_account_id)
                                .collect(),
                        );
                    }
                }
                Err(failed) => {
                    tracing::warn!(instance, "an account report did not decode: {failed}")
                }
            }
        }
    });

    // W11.20: each venue catalogue a connectivity plugin publishes, kept
    // latest per plugin and code, a change its own record.
    let mut catalogues =
        bus.subscribe_shared(crate::connectivity::VENUE_CATALOGUE, "venue-catalogues");
    let cataloguing = Arc::clone(&context);
    tokio::spawn(async move {
        while let Some(delivery) = catalogues.recv().await {
            let event = match meridian_domain::v1::VenueCatalogueEvent::decode(
                &delivery.envelope.payload[..],
            ) {
                Ok(event) => event,
                Err(failed) => {
                    tracing::warn!("a venue catalogue did not decode: {failed}");
                    continue;
                }
            };
            let Some(catalogue) =
                crate::connectivity::catalogue_heard(&delivery.envelope, event.catalogue)
            else {
                tracing::warn!("a venue catalogue named no code, or no publisher; not kept");
                continue;
            };
            let store = Arc::clone(&cataloguing.store);
            let now = cataloguing.clock.now_ns();
            match tokio::task::spawn_blocking(move || store.keep_venue_catalogue(&catalogue, now))
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(failed)) => tracing::warn!("a venue catalogue was not kept: {failed}"),
                Err(failed) => tracing::warn!("a venue catalogue was not kept: {failed}"),
            }
        }
    });

    let mut reports = bus.subscribe_every(PLUGIN_REPORT);
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
            // The kinds of raw record it declared (contract v16), which a
            // past-the-window setting is checked against.
            if report.registered && own {
                let kinds = report
                    .declaration
                    .as_ref()
                    .and_then(|declaration| declaration.storage.as_ref())
                    .map(|storage| storage.record_kinds.clone())
                    .unwrap_or_default();
                if let Ok(mut held) = noting.kinds.lock() {
                    held.insert(plugin.plugin_instance_id.clone(), kinds);
                }
            }
            // Its catalogue (contract v18), kept whole; a change, or its
            // first registration since this started, publishes the data
            // configuration.
            let catalogue = (report.registered && own).then(|| {
                report
                    .declaration
                    .as_ref()
                    .and_then(|declaration| declaration.catalogue.clone())
                    .unwrap_or_default()
            });
            let first = report.registered
                && own
                && noting
                    .registered
                    .lock()
                    .map(|mut heard| heard.insert(plugin.plugin_instance_id.clone()))
                    .unwrap_or(false);
            let declared = (report.registered && own).then_some(report.declared_settings);
            let store = Arc::clone(&noting.store);
            let instance = plugin.plugin_instance_id.clone();
            // A value held for a setting re-declared with another type, or
            // as secret or not, is cleared as the declaration is kept, the
            // clear its own record naming the re-declaration (W6.11).
            let now = noting.clock.now_ns();
            let catalogued = Arc::clone(&noting);
            let keeping = move || -> Result<Vec<String>, crate::StoreError> {
                store.record_plugin(&plugin)?;
                let changed = match &catalogue {
                    Some(catalogue) => {
                        store.record_catalogue(&plugin.plugin_instance_id, catalogue, now)?
                    }
                    None => false,
                };
                if changed || first {
                    if let Err(failed) = catalogued.publish_data() {
                        tracing::warn!("the data configuration was not published: {failed}");
                    }
                }
                match declared {
                    Some(declared) => {
                        store.record_declared_settings(&plugin.plugin_instance_id, &declared, now)
                    }
                    None => Ok(Vec::new()),
                }
            };
            match tokio::task::spawn_blocking(keeping).await {
                Ok(Ok(cleared)) if cleared.is_empty() => {}
                Ok(Ok(cleared)) => {
                    tracing::info!(
                        plugin = instance,
                        cleared = cleared.join(","),
                        "plugin settings cleared: re-declared with another type, or secret or not"
                    );
                    let event = PluginConfigurationChangedEvent {
                        plugin_instance_id: instance,
                        changed_at_ns: noting.clock.now_ns(),
                    };
                    if let Err(failed) = noting.bus.publish(
                        PLUGIN_CONFIGURATION_CHANGED,
                        "meridian.v1.PluginConfigurationChangedEvent",
                        event.encode_to_vec(),
                        None,
                        None,
                    ) {
                        tracing::warn!("a settings clear was not announced: {failed}");
                    }
                }
                Ok(Err(failed)) => tracing::warn!("a plugin report was not kept: {failed}"),
                Err(failed) => tracing::warn!("a plugin report was not kept: {failed}"),
            }
        }
    });
}

/// The dashboard's access records' size as people grow (H1, ruling 586):
/// the largest bus answer core builds from records, against the broker's
/// 1 MiB. Each person signed in, in a user group of their own holding one
/// permission, and an account of their own.
#[cfg(test)]
mod capacity_tests {
    use meridian_domain::v1::{AccountRecord, Permission, SignInRecord, UserGroup};

    use super::*;

    fn records_for(people: usize) -> usize {
        let mut snapshot = Snapshot::default();
        for p in 0..people {
            let subject = format!("https://login.example.com/|person-{p:05}@example.com");
            snapshot.records.people.push(SignInRecord {
                subject: subject.clone(),
                display_name: format!("Person Number {p:05}"),
                directory_groups: vec!["traders".into(), "london-desk".into()],
                signed_in_at_ns: 1_792_800_000_000_000_000,
            });
            snapshot.records.user_groups.push(UserGroup {
                user_group_id: format!("UG-{p:026}"),
                name: format!("Person {p:05}'s group"),
                directory_groups: vec![],
                logins: vec![subject],
            });
            snapshot.records.permissions.push(Permission {
                permission_id: format!("P-{p:026}"),
                user_group_id: format!("UG-{p:026}"),
                account_group_id: "AcG-ALL".into(),
                access_group_id: "AG-TRADERS".into(),
            });
            snapshot.records.accounts.push(AccountRecord {
                account_id: format!("ACC-{p:026}"),
                name: format!("Account {p:05}"),
                state: 1,
                created_at_ns: 1_792_800_000_000_000_000,
                ..Default::default()
            });
        }
        access_records_of(snapshot, 1).encoded_len()
    }

    /// Bytes per person, and how many people fit in three quarters of the
    /// broker's 1 MiB -- where the upgrade preflight starts to warn.
    fn per_person() -> (f64, usize) {
        let base = records_for(100);
        let more = records_for(1_100);
        let per = (more - base) as f64 / 1_000.0;
        let room = (0.75 * 1_048_576.0 - base as f64) / per;
        (per, 100 + room as usize)
    }

    /// The gate's guard: a person costs no more than it did when the
    /// capacity was stated, with room.
    #[test]
    fn a_person_costs_the_access_records_what_it_did() {
        let (per, people) = per_person();
        eprintln!("bench: access records {per:.0} bytes a person; {people} people to the preflight's warning");
        assert!(per <= 500.0, "{per:.0} bytes a person");
    }

    #[test]
    #[ignore = "the capacity bench: make bench"]
    fn capacity_access_records_per_person() {
        let (per, people) = per_person();
        println!("CAPACITY access_records bytes_per_person={per:.0} people_to_warning={people}");
    }
}

#[cfg(test)]
mod declaration_tests {
    use meridian_domain::v1::{PluginLaunch, PluginMetadata, PluginVersion};
    use meridian_pb::v1::PluginDeclaration;

    use super::*;

    /// W4.1 (contract v25): a sidecar is told the declaration of the version
    /// its instance was launched from, and no other version's.
    #[test]
    fn a_sidecar_is_told_its_launched_versions_declaration() {
        let version = |version: &str, secret: &str| PluginVersion {
            metadata: Some(PluginMetadata {
                name: "snaptrade".into(),
                version: version.into(),
                declaration: Some(PluginDeclaration {
                    secret_settings: vec![secret.into()],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut snapshot = Snapshot::default();
        snapshot.catalogue.versions = vec![version("0.1.0", "old"), version("0.2.0", "new")];
        snapshot.catalogue.launches = vec![PluginLaunch {
            instance_id: "snaptrade-1".into(),
            name: "snaptrade".into(),
            version: "0.2.0".into(),
            ..Default::default()
        }];
        let launched = told(&snapshot, "snaptrade-1");
        assert!(launched.launched);
        assert_eq!(launched.declaration.unwrap().secret_settings, ["new"]);
        let harnessed = told(&snapshot, "other-1");
        assert!(!harnessed.launched && harnessed.declaration.is_none());
    }
}
