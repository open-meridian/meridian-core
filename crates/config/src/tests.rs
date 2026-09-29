//! The configuration store's service, over the bus, on the memory store.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessLevel, AccessRecords, AccessRecordsRequest, AccountGroup,
    AccountRecord, AccountState, ClaimCodePurpose, CloseAccountRequest, DefineAccessGroupRequest,
    DefineAccountGroupRequest, DefineAccountRequest, DefineUserGroupRequest, DiagnosticBundle,
    DiagnosticBundleReceipt, ExternalAccountLink, GrantPermissionRequest,
    LinkExternalAccountRequest, Permission, PluginConfiguration, PluginConfigurationChangedEvent,
    PluginConfigurationRequest, PluginReport, PluginSettingValue, PluginSettingsRecord,
    RedeemClaimCodeReply, RedeemClaimCodeRequest, SetPluginSettingsRequest, SignInRecord,
    UserGroup, WithdrawPermissionReply, WithdrawPermissionRequest,
};
use meridian_pb::v1::{SettingChoice, SettingDeclaration, SettingType};
use prost::Message;

use crate::service::*;
use crate::store::{Held, KnownPlugin, Store};
use crate::{MemoryStore, SettingsKey, DEPLOYMENT_ADMIN};

const ADA: &str = "https://directory.example.org|8812";

struct FixedClock;
impl Clock for FixedClock {
    fn now_ns(&self) -> i64 {
        1_790_380_800_000_000_000
    }
}

/// A platform that answers as told, and remembers what it was asked.
#[derive(Default)]
struct FakePlatform {
    redeem: Mutex<bool>,
    codes: Mutex<Vec<String>>,
    bundles: Mutex<Vec<DiagnosticBundle>>,
}

impl Upstream for FakePlatform {
    fn honour_claim_code(&self, code: &str, _purpose: i32) -> Result<RedeemClaimCodeReply, String> {
        self.codes.lock().unwrap().push(code.to_string());
        let redeemed = *self.redeem.lock().unwrap();
        Ok(RedeemClaimCodeReply {
            redeemed,
            refusal_reason: if redeemed {
                String::new()
            } else {
                "expired".into()
            },
            ..Default::default()
        })
    }

    fn submit_diagnostic_bundle(
        &self,
        bundle: &DiagnosticBundle,
    ) -> Result<DiagnosticBundleReceipt, String> {
        self.bundles.lock().unwrap().push(bundle.clone());
        Ok(DiagnosticBundleReceipt {
            bundle_id: bundle.bundle_id.clone(),
            received_at_ns: 1,
            retain_until_ns: 2,
        })
    }
}

struct Harness {
    bus: Arc<Bus>,
    backend: Arc<MemoryBackend>,
    platform: Arc<FakePlatform>,
    store: Arc<MemoryStore>,
}

/// A bus whose instance is `instance`, so a query "from" a plugin can be made
/// by naming the bus after it.
fn harness(instance: &str) -> Harness {
    harness_keyed(instance, SettingsKey::holding(&[7u8; 32]))
}

fn harness_keyed(instance: &str, key: SettingsKey) -> Harness {
    let backend = Arc::new(MemoryBackend::new());
    let bus = Arc::new(Bus::single(instance, backend.clone()));
    let store = Arc::new(MemoryStore::new());
    let platform = Arc::new(FakePlatform::default());
    serve(
        Arc::clone(&bus),
        store.clone() as Arc<dyn Store>,
        Arc::new(FixedClock),
        platform.clone() as Arc<dyn Upstream>,
        Arc::new(key),
    );
    store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: "oms-1".into(),
            roles: vec!["oms".into()],
            tags: vec!["reporting".into()],
            last_reported_at_ns: 0,
        })
        .unwrap();
    Harness {
        bus,
        backend,
        platform,
        store,
    }
}

async fn ask<Req: Message, Rep: Message + Default>(
    h: &Harness,
    topic: &str,
    request_type: &str,
    request: Req,
) -> Result<Rep, String> {
    let (_, bytes) = h
        .bus
        .call_for(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            None,
            ADA,
        )
        .await
        .map_err(|failed| failed.to_string())?;
    Ok(Rep::decode(&bytes[..]).expect("decodes"))
}

async fn account(h: &Harness, name: &str) -> AccountRecord {
    ask(
        h,
        DEFINE_ACCOUNT,
        "meridian.v1.DefineAccountRequest",
        DefineAccountRequest {
            account_id: String::new(),
            name: name.into(),
        },
    )
    .await
    .unwrap()
}

async fn user_group(h: &Harness) -> UserGroup {
    ask(
        h,
        DEFINE_USER_GROUP,
        "meridian.v1.DefineUserGroupRequest",
        DefineUserGroupRequest {
            user_group: Some(UserGroup {
                name: "Traders".into(),
                directory_groups: vec!["trading-desk".into()],
                ..Default::default()
            }),
        },
    )
    .await
    .unwrap()
}

async fn account_group(h: &Harness, accounts: &[&AccountRecord]) -> AccountGroup {
    ask(
        h,
        DEFINE_ACCOUNT_GROUP,
        "meridian.v1.DefineAccountGroupRequest",
        DefineAccountGroupRequest {
            account_group: Some(AccountGroup {
                name: "Growth".into(),
                account_ids: accounts.iter().map(|a| a.account_id.clone()).collect(),
                ..Default::default()
            }),
        },
    )
    .await
    .unwrap()
}

fn entry(tag: &str, level: AccessLevel) -> AccessEntry {
    AccessEntry {
        plugin_instance_id: "oms-1".into(),
        tag: tag.into(),
        level: level as i32,
    }
}

async fn access_group(h: &Harness, entries: Vec<AccessEntry>) -> Result<AccessGroup, String> {
    ask(
        h,
        DEFINE_ACCESS_GROUP,
        "meridian.v1.DefineAccessGroupRequest",
        DefineAccessGroupRequest {
            access_group: Some(AccessGroup {
                name: "Trading".into(),
                entries,
                ..Default::default()
            }),
        },
    )
    .await
}

async fn grant(
    h: &Harness,
    user: &str,
    accounts: &str,
    access: &str,
) -> Result<Permission, String> {
    ask(
        h,
        GRANT_PERMISSION,
        "meridian.v1.GrantPermissionRequest",
        GrantPermissionRequest {
            user_group_id: user.into(),
            account_group_id: accounts.into(),
            access_group_id: access.into(),
        },
    )
    .await
}

async fn records(h: &Harness) -> AccessRecords {
    ask(
        h,
        ACCESS_RECORDS,
        "meridian.v1.AccessRecordsRequest",
        AccessRecordsRequest {},
    )
    .await
    .unwrap()
}

async fn redeem(h: &Harness, as_whom: &str) -> RedeemClaimCodeReply {
    let (_, bytes) = h
        .bus
        .call_for(
            REDEEM_CLAIM_CODE,
            "meridian.v1.RedeemClaimCodeRequest",
            RedeemClaimCodeRequest {
                code: "7KQ2-MX4P-9RTD".into(),
                ..Default::default()
            }
            .encode_to_vec(),
            None,
            None,
            as_whom,
        )
        .await
        .unwrap();
    RedeemClaimCodeReply::decode(&bytes[..]).unwrap()
}

#[tokio::test]
async fn an_account_is_defined_renamed_and_closed_and_never_deleted() {
    let h = harness("dashboard-1");
    let created = account(&h, "Growth Fund").await;
    assert!(created.account_id.starts_with("ACC-"));
    assert_eq!(created.state, AccountState::Open as i32);

    let renamed: AccountRecord = ask(
        &h,
        DEFINE_ACCOUNT,
        "meridian.v1.DefineAccountRequest",
        DefineAccountRequest {
            account_id: created.account_id.clone(),
            name: "Growth".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(renamed.account_id, created.account_id);
    assert_eq!(renamed.created_at_ns, created.created_at_ns);

    let closed: AccountRecord = ask(
        &h,
        CLOSE_ACCOUNT,
        "meridian.v1.CloseAccountRequest",
        CloseAccountRequest {
            account_id: created.account_id.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(closed.state, AccountState::Closed as i32);
    assert_eq!(records(&h).await.accounts.len(), 1);
}

#[tokio::test]
async fn deployment_admin_is_built_in_and_cannot_be_edited() {
    let h = harness("dashboard-1");
    let refused: Result<AccessGroup, String> = ask(
        &h,
        DEFINE_ACCESS_GROUP,
        "meridian.v1.DefineAccessGroupRequest",
        DefineAccessGroupRequest {
            access_group: Some(AccessGroup {
                access_group_id: DEPLOYMENT_ADMIN.into(),
                name: "Everything".into(),
                ..Default::default()
            }),
        },
    )
    .await;
    assert!(refused.unwrap_err().contains("built in"));
    assert!(records(&h)
        .await
        .access_groups
        .iter()
        .any(|g| g.access_group_id == DEPLOYMENT_ADMIN && g.built_in));
}

#[tokio::test]
async fn an_access_entry_names_a_tag_its_plugin_carries_and_says_which_it_does() {
    let h = harness("dashboard-1");
    assert!(access_group(&h, vec![entry("oms", AccessLevel::Write)])
        .await
        .is_ok());
    assert!(
        access_group(&h, vec![entry("reporting", AccessLevel::Read)])
            .await
            .is_ok()
    );

    let wrong = access_group(&h, vec![entry("custody", AccessLevel::Read)])
        .await
        .unwrap_err();
    assert!(
        wrong.contains("does not carry `custody`") && wrong.contains("oms, reporting"),
        "{wrong}"
    );

    let mut unknown = entry("oms", AccessLevel::Read);
    unknown.plugin_instance_id = "never-reported".into();
    assert!(access_group(&h, vec![unknown])
        .await
        .unwrap_err()
        .contains("has reported"));
}

#[tokio::test]
async fn a_permission_names_an_account_group_unless_it_is_to_deployment_admin() {
    let h = harness("dashboard-1");
    let traders = user_group(&h).await;
    let growth = account_group(&h, &[&account(&h, "Growth").await]).await;
    let trading = access_group(&h, vec![entry("oms", AccessLevel::Write)])
        .await
        .unwrap();

    assert!(
        grant(&h, &traders.user_group_id, "", &trading.access_group_id)
            .await
            .is_err()
    );
    assert!(grant(
        &h,
        &traders.user_group_id,
        &growth.account_group_id,
        DEPLOYMENT_ADMIN
    )
    .await
    .is_err());
    assert!(grant(
        &h,
        &traders.user_group_id,
        &growth.account_group_id,
        &trading.access_group_id
    )
    .await
    .is_ok());
    assert!(grant(
        &h,
        &traders.user_group_id,
        &growth.account_group_id,
        &trading.access_group_id
    )
    .await
    .unwrap_err()
    .contains("already granted"));
}

#[tokio::test]
async fn the_last_permission_to_deployment_admin_cannot_be_withdrawn() {
    let h = harness("dashboard-1");
    *h.platform.redeem.lock().unwrap() = true;
    assert!(redeem(&h, ADA).await.redeemed);
    let first = records(&h).await.permissions[0].permission_id.clone();

    let withdraw = |id: String| {
        let h = &h;
        async move {
            ask::<_, WithdrawPermissionReply>(
                h,
                WITHDRAW_PERMISSION,
                "meridian.v1.WithdrawPermissionRequest",
                WithdrawPermissionRequest { permission_id: id },
            )
            .await
            .unwrap()
        }
    };
    let refused = withdraw(first.clone()).await;
    assert!(!refused.withdrawn);
    assert_eq!(
        refused.refusal_reason,
        "the last permission to deployment admin"
    );

    let admins = records(&h).await.user_groups[0].user_group_id.clone();
    let second = grant(&h, &admins, "", DEPLOYMENT_ADMIN).await;
    assert!(second.is_err(), "the same triple twice is refused");
    let others = user_group(&h).await;
    grant(&h, &others.user_group_id, "", DEPLOYMENT_ADMIN)
        .await
        .unwrap();
    assert!(withdraw(first).await.withdrawn, "not the last one any more");
}

#[tokio::test]
async fn a_claim_code_makes_the_redeemer_the_first_deployment_admin_once() {
    let h = harness("dashboard-1");
    *h.platform.redeem.lock().unwrap() = true;

    let redeemed = redeem(&h, ADA).await;
    assert!(redeemed.redeemed);
    let after = records(&h).await;
    assert_eq!(after.user_groups[0].logins, [ADA]);
    assert_eq!(after.permissions[0].access_group_id, DEPLOYMENT_ADMIN);
    assert!(after.permissions[0].account_group_id.is_empty());

    let again = redeem(&h, "someone-else").await;
    assert!(!again.redeemed);
    assert_eq!(
        again.refusal_reason,
        "this deployment already has a deployment admin"
    );
    assert_eq!(
        h.platform.codes.lock().unwrap().len(),
        1,
        "refused before the platform was asked"
    );
}

#[tokio::test]
async fn a_refused_or_anonymous_claim_installs_nobody() {
    let h = harness("dashboard-1");
    let refused = redeem(&h, ADA).await;
    assert!(!refused.redeemed);
    assert_eq!(refused.refusal_reason, "expired");

    *h.platform.redeem.lock().unwrap() = true;
    let anonymous = redeem(&h, "").await;
    assert!(!anonymous.redeemed);
    assert!(records(&h).await.permissions.is_empty());
}

#[tokio::test]
async fn a_reset_code_is_honoured_with_nobody_signed_in_and_makes_nobody_anything() {
    // W6.16: whoever holds one cannot sign in. The platform's answer goes
    // back; what it resets is the dashboard's to do.
    let h = harness("dashboard-1");
    *h.platform.redeem.lock().unwrap() = true;
    let (_, bytes) = h
        .bus
        .call_for(
            REDEEM_CLAIM_CODE,
            "meridian.v1.RedeemClaimCodeRequest",
            RedeemClaimCodeRequest {
                code: "7KQ2-MX4P-9RTD".into(),
                purpose: ClaimCodePurpose::ResetLocalAdmin as i32,
            }
            .encode_to_vec(),
            None,
            None,
            "",
        )
        .await
        .unwrap();
    assert!(RedeemClaimCodeReply::decode(&bytes[..]).unwrap().redeemed);
    assert!(
        records(&h).await.permissions.is_empty(),
        "nobody was made anything"
    );
}

#[tokio::test]
async fn a_change_is_announced_to_the_plugins_it_moved_and_carries_no_setting() {
    let h = harness("dashboard-1");
    let mut changes = h.bus.subscribe(PLUGIN_CONFIGURATION_CHANGED);
    let traders = user_group(&h).await;
    let growth = account_group(&h, &[&account(&h, "Growth").await]).await;
    let trading = access_group(&h, vec![entry("oms", AccessLevel::Write)])
        .await
        .unwrap();
    grant(
        &h,
        &traders.user_group_id,
        &growth.account_group_id,
        &trading.access_group_id,
    )
    .await
    .unwrap();

    let delivery = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("announced")
        .expect("open");
    let event = PluginConfigurationChangedEvent::decode(&delivery.envelope.payload[..]).unwrap();
    assert_eq!(event.plugin_instance_id, "oms-1");
    assert_eq!(
        delivery.envelope.payload_type,
        "meridian.v1.PluginConfigurationChangedEvent"
    );

    // Defining a user group moves no plugin's configuration, so says nothing.
    user_group(&h).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), changes.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_sidecar_is_told_its_own_plugins_configuration_and_no_other() {
    let h = harness("oms-1");
    let traders = user_group(&h).await;
    let growth_account = account(&h, "Growth").await;
    let growth = account_group(&h, &[&growth_account]).await;
    let trading = access_group(&h, vec![entry("oms", AccessLevel::Read)])
        .await
        .unwrap();
    grant(
        &h,
        &traders.user_group_id,
        &growth.account_group_id,
        &trading.access_group_id,
    )
    .await
    .unwrap();
    let _: ExternalAccountLink = ask(
        &h,
        LINK_EXTERNAL_ACCOUNT,
        "meridian.v1.LinkExternalAccountRequest",
        LinkExternalAccountRequest {
            plugin_instance_id: "oms-1".into(),
            external_account_id: "st-4471".into(),
            account_id: growth_account.account_id.clone(),
        },
    )
    .await
    .unwrap();

    let configured: PluginConfiguration = ask(
        &h,
        PLUGIN_CONFIGURATION,
        "meridian.v1.PluginConfigurationRequest",
        PluginConfigurationRequest {},
    )
    .await
    .unwrap();
    assert_eq!(
        configured.plugin_instance_id, "oms-1",
        "answered for the instance that asked"
    );
    assert_eq!(
        configured.read_account_ids,
        std::slice::from_ref(&growth_account.account_id)
    );
    // Read through the permission; written through the link, which is the
    // plugin's right to the one account it names (W4.11, W6.4).
    assert_eq!(
        configured.write_account_ids,
        std::slice::from_ref(&growth_account.account_id)
    );
    assert_eq!(configured.links.len(), 1);

    // And with the records the dashboard reads, so it can tell which of the
    // accounts a connector reports have no link (W2.8, W6.4).
    let read = records(&h).await;
    assert_eq!(read.links.len(), 1);
    assert_eq!(read.links[0].external_account_id, "st-4471");
    assert_eq!(read.links[0].account_id, growth_account.account_id);
}

#[tokio::test]
async fn a_link_needs_a_plugin_that_has_reported_and_an_open_account() {
    let h = harness("dashboard-1");
    let open = account(&h, "Growth").await;
    let link = |plugin: &str, account: &str| LinkExternalAccountRequest {
        plugin_instance_id: plugin.into(),
        external_account_id: "st-1".into(),
        account_id: account.into(),
    };
    let unknown: Result<ExternalAccountLink, _> = ask(
        &h,
        LINK_EXTERNAL_ACCOUNT,
        "meridian.v1.LinkExternalAccountRequest",
        link("ghost-1", &open.account_id),
    )
    .await;
    assert!(unknown.is_err());

    let _: AccountRecord = ask(
        &h,
        CLOSE_ACCOUNT,
        "meridian.v1.CloseAccountRequest",
        CloseAccountRequest {
            account_id: open.account_id.clone(),
        },
    )
    .await
    .unwrap();
    let closed: Result<ExternalAccountLink, _> = ask(
        &h,
        LINK_EXTERNAL_ACCOUNT,
        "meridian.v1.LinkExternalAccountRequest",
        link("oms-1", &open.account_id),
    )
    .await;
    assert!(closed.unwrap_err().contains("closed"));
}

#[tokio::test]
async fn a_sign_in_is_kept_and_the_latest_one_stands() {
    let h = harness("dashboard-1");
    for (groups, at) in [(vec!["ops"], 5_i64), (vec!["trading-desk"], 9)] {
        h.bus
            .publish(
                PERSON_SIGNED_IN,
                "meridian.v1.SignInRecord",
                SignInRecord {
                    subject: ADA.into(),
                    display_name: "Ada".into(),
                    directory_groups: groups.into_iter().map(String::from).collect(),
                    signed_in_at_ns: at,
                }
                .encode_to_vec(),
                None,
                None,
            )
            .unwrap();
    }
    let seen = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let people = records(&h).await.people;
            if people.first().is_some_and(|p| p.signed_in_at_ns == 9) {
                return people;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recorded");
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].directory_groups,
        ["trading-desk"],
        "never merged across sign-ins"
    );
}

#[tokio::test]
async fn a_bundle_is_forwarded_exactly_as_approved() {
    let h = harness("dashboard-1");
    let bundle = DiagnosticBundle {
        bundle_id: "DB-1".into(),
        assembled_at_ns: 3,
        items: vec![],
        deployment_id: String::new(),
    };
    let receipt: DiagnosticBundleReceipt = ask(
        &h,
        SEND_DIAGNOSTIC_BUNDLE,
        "meridian.v1.DiagnosticBundle",
        bundle.clone(),
    )
    .await
    .unwrap();
    assert_eq!(receipt.bundle_id, "DB-1");
    assert_eq!(
        h.platform.bundles.lock().unwrap().as_slice(),
        std::slice::from_ref(&bundle)
    );
}

// ── W7.6: the administrator the wizard named ────────────────────────────────

#[test]
fn the_administrator_the_wizard_named_gets_the_permission() {
    let store = MemoryStore::new();

    let written = install_named_administrator(&store, &FixedClock, "meridian-admins", "").unwrap();

    assert!(written);
    let records = store.snapshot().unwrap().records;
    let group = records
        .user_groups
        .iter()
        .find(|g| g.directory_groups == vec!["meridian-admins".to_string()])
        .expect("a user group naming the directory group");
    assert!(
        records.permissions.iter().any(|p| {
            p.user_group_id == group.user_group_id && p.access_group_id == DEPLOYMENT_ADMIN
        }),
        "an ordinary permission, audited by reading the same table as every other grant"
    );
}

#[test]
fn a_local_account_is_named_by_its_login_instead() {
    let store = MemoryStore::new();

    assert!(install_named_administrator(&store, &FixedClock, "", "ada").unwrap());

    let records = store.snapshot().unwrap().records;
    assert_eq!(
        records
            .user_groups
            .iter()
            .find(|g| !g.logins.is_empty())
            .map(|g| g.logins.clone()),
        Some(vec!["ada".to_string()])
    );
}

#[test]
fn it_is_written_once_however_often_the_conductor_restarts() {
    // The conductor calls this every start, because it cannot know which one
    // is the first after the wizard. Writing twice would leave a deployment
    // with two groups arguing about who administers it.
    let store = MemoryStore::new();

    assert!(install_named_administrator(&store, &FixedClock, "meridian-admins", "").unwrap());
    assert!(!install_named_administrator(&store, &FixedClock, "meridian-admins", "").unwrap());

    let records = store.snapshot().unwrap().records;
    assert_eq!(records.permissions.len(), 1);
}

#[test]
fn an_administrator_who_arrived_another_way_is_left_alone() {
    // A claim code redeemed before the conductor restarted, say. The store
    // refuses the write when a deployment admin exists, which is what makes
    // this safe to call unconditionally.
    let store = MemoryStore::new();
    let group = UserGroup {
        user_group_id: "ug-existing".into(),
        name: "Deployment admins".into(),
        directory_groups: Vec::new(),
        logins: vec![ADA.into()],
    };
    let permission = Permission {
        permission_id: "perm-existing".into(),
        user_group_id: group.user_group_id.clone(),
        account_group_id: String::new(),
        access_group_id: DEPLOYMENT_ADMIN.into(),
    };
    store.install_first_admin(&group, &permission).unwrap();

    assert!(!install_named_administrator(&store, &FixedClock, "meridian-admins", "").unwrap());
    assert_eq!(store.snapshot().unwrap().records.permissions.len(), 1);
}

#[test]
fn naming_nobody_writes_nothing() {
    let store = MemoryStore::new();

    assert!(!install_named_administrator(&store, &FixedClock, "  ", "").unwrap());

    assert!(store.snapshot().unwrap().records.permissions.is_empty());
}

// ── Plugin settings (W4.8, W6.11) ───────────────────────────────────────────

/// Obviously not a real credential, and long enough to find in bytes.
const SECRET: &str = "sk-test-not-a-real-key-7f3a";

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

fn declaration(name: &str, kind: SettingType, required: bool, secret: bool) -> SettingDeclaration {
    SettingDeclaration {
        name: name.into(),
        r#type: kind as i32,
        required,
        secret,
        description: format!("what {name} is"),
        ..Default::default()
    }
}

/// What oms-1 declares: a choice of key, a required secret, a number and a
/// switch.
fn declared() -> Vec<SettingDeclaration> {
    let choice = |value: &str| SettingChoice {
        value: value.into(),
        label: format!("{value} key"),
        ..Default::default()
    };
    vec![
        SettingDeclaration {
            choices: vec![choice("personal"), choice("commercial")],
            ..declaration("key_type", SettingType::Choice, true, false)
        },
        declaration("api_key", SettingType::String, true, true),
        declaration("poll_minutes", SettingType::Integer, false, false),
        declaration("synthetic", SettingType::Boolean, false, false),
    ]
}

/// oms-1's sidecar reports it, and the conductor has kept the report.
async fn reported(h: &Harness, registered: bool, declared: Vec<SettingDeclaration>, at: i64) {
    reported_by(h, "oms-1", registered, declared, at).await
}

/// A report about oms-1, published on the bus as `publisher`.
async fn reported_by(
    h: &Harness,
    publisher: &str,
    registered: bool,
    declared: Vec<SettingDeclaration>,
    at: i64,
) {
    let sidecar = Bus::single(publisher, h.backend.clone());
    let report = PluginReport {
        plugin_instance_id: "oms-1".into(),
        roles: vec!["oms".into()],
        tags: vec!["reporting".into()],
        registered,
        declared_settings: declared.clone(),
        reported_at_ns: at,
        ..Default::default()
    };
    sidecar
        .publish(
            PLUGIN_REPORT,
            "meridian.v1.PluginReport",
            report.encode_to_vec(),
            None,
            None,
        )
        .unwrap();
    let own = publisher == "oms-1";
    for _ in 0..200 {
        let snapshot = h.store.snapshot().unwrap();
        let heard = snapshot
            .plugins
            .iter()
            .any(|p| p.plugin_instance_id == "oms-1" && p.last_reported_at_ns == at);
        let kept =
            !registered || !own || snapshot.declared_settings.get("oms-1") == Some(&declared);
        if heard && kept {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the report was not kept");
}

async fn set(
    h: &Harness,
    plugin: &str,
    values: &[(&str, &str)],
    cleared: &[&str],
) -> Result<PluginSettingsRecord, String> {
    ask(
        h,
        SET_PLUGIN_SETTINGS,
        "meridian.v1.SetPluginSettingsRequest",
        SetPluginSettingsRequest {
            plugin_instance_id: plugin.into(),
            values: values
                .iter()
                .map(|(name, value)| PluginSettingValue {
                    name: (*name).into(),
                    value: (*value).into(),
                })
                .collect(),
            cleared: cleared.iter().map(|name| (*name).into()).collect(),
        },
    )
    .await
}

async fn told(h: &Harness) -> PluginConfiguration {
    ask(
        h,
        PLUGIN_CONFIGURATION,
        "meridian.v1.PluginConfigurationRequest",
        PluginConfigurationRequest {},
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn what_a_plugin_declares_is_kept_from_its_report_and_between_registrations() {
    let h = harness("dashboard-1");
    reported(&h, true, declared(), 1).await;
    let form = records(&h).await.plugin_settings;
    assert_eq!(form.len(), 1);
    assert_eq!(form[0].plugin_instance_id, "oms-1");
    assert_eq!(form[0].declared_settings, declared());

    // Between registrations its report declares nothing, and its form stays.
    reported(&h, false, Vec::new(), 2).await;
    assert_eq!(
        records(&h).await.plugin_settings[0].declared_settings,
        declared()
    );

    // Registered again with less, the form is what it now declares.
    reported(&h, true, declared()[1..].to_vec(), 3).await;
    assert_eq!(
        records(&h).await.plugin_settings[0].declared_settings,
        declared()[1..]
    );

    // Said by anything but its own sidecar, it changes nothing: which
    // settings are secret is not another component's to say.
    let mut nothing_secret = declared();
    nothing_secret[0].secret = false;
    reported_by(&h, "oms-2", true, nothing_secret, 4).await;
    assert_eq!(
        records(&h).await.plugin_settings[0].declared_settings,
        declared()[1..]
    );
}

#[tokio::test]
async fn a_setting_is_one_the_plugin_declared_in_a_form_its_type_reads() {
    let h = harness("dashboard-1");
    let unknown = set(&h, "ghost-1", &[("api_key", SECRET)], &[]).await;
    assert!(unknown.unwrap_err().contains("has reported"));

    reported(&h, true, declared(), 1).await;
    let refusals = [
        (
            vec![("api_token", "x")],
            vec![],
            "declares no setting named api_token",
        ),
        (vec![("poll_minutes", "soon-ish")], vec![], "whole number"),
        (vec![("synthetic", "maybe-so")], vec![], "true or false"),
        // The fixture's case: a choice not among the declared choices,
        // refused naming the setting and the choices.
        (
            vec![("key_type", "trial")],
            vec![],
            "setting key_type is one of personal, commercial",
        ),
        (vec![("poll_minutes", "")], vec![], "clear it instead"),
        (
            vec![("poll_minutes", "5"), ("poll_minutes", "6")],
            vec![],
            "twice",
        ),
        (vec![("poll_minutes", "5")], vec!["poll_minutes"], "cleared"),
        (
            vec![],
            vec!["api_token"],
            "declares no setting named api_token",
        ),
    ];
    for (values, cleared, said) in refusals {
        // Beside a secret that would have been stored, to show a refusal
        // stores nothing at all.
        let mut values = values;
        values.insert(0, ("api_key", SECRET));
        let refused = set(&h, "oms-1", &values, &cleared).await.unwrap_err();
        assert!(refused.contains(said), "{said}: {refused}");
        for value in [SECRET, "soon-ish", "maybe-so", "trial"] {
            assert!(
                !refused.contains(value),
                "a refusal never repeats a value: {refused}"
            );
        }
    }
    assert!(
        h.store.snapshot().unwrap().settings.is_empty(),
        "nothing was stored"
    );

    // Written as the SDK reads it back.
    let record = set(
        &h,
        "oms-1",
        &[
            ("key_type", "commercial "),
            ("poll_minutes", " 15"),
            ("synthetic", "Yes"),
        ],
        &[],
    )
    .await
    .unwrap();
    let mut values: Vec<(String, String)> = record
        .values
        .into_iter()
        .map(|v| (v.name, v.value))
        .collect();
    values.sort();
    assert_eq!(
        values,
        [
            ("key_type".to_string(), "commercial".to_string()),
            ("poll_minutes".to_string(), "15".to_string()),
            ("synthetic".to_string(), "true".to_string())
        ]
    );
}

#[tokio::test]
async fn a_secret_is_stored_sealed_and_only_ever_named() {
    let h = harness("dashboard-1");
    reported(&h, true, declared(), 1).await;
    let record = set(
        &h,
        "oms-1",
        &[("api_key", SECRET), ("poll_minutes", "15")],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(record.secrets_set, ["api_key"]);
    assert_eq!(record.values.len(), 1);
    assert_eq!(record.values[0].name, "poll_minutes");
    assert_eq!(record.updated_at_ns, FixedClock.now_ns());
    assert!(
        !contains(&record.encode_to_vec(), SECRET),
        "not in the reply"
    );
    assert!(
        !contains(&records(&h).await.encode_to_vec(), SECRET),
        "not in what the dashboard reads"
    );

    // At rest: sealed, and not its own bytes. Who set it, and when, is kept.
    let snapshot = h.store.snapshot().unwrap();
    let held = snapshot
        .settings
        .iter()
        .find(|s| s.name == "api_key")
        .unwrap();
    let Held::Sealed(sealed) = &held.held else {
        panic!("a secret is held sealed: {:?}", held.held)
    };
    assert!(!contains(sealed, SECRET));
    assert_eq!(held.set_by, ADA);
    assert!(!format!("{snapshot:?}").contains(SECRET), "nor in a Debug");

    // Declared plain later, it is still only named: nothing sealed is shown.
    let mut plainer = declared();
    plainer[0].secret = false;
    reported(&h, true, plainer, 2).await;
    let form = records(&h).await.plugin_settings;
    assert_eq!(form[0].secrets_set, ["api_key"]);
    assert!(!contains(&form[0].encode_to_vec(), SECRET));
}

#[tokio::test]
async fn the_plugins_own_sidecar_is_told_its_secret_opened_and_the_change_announced() {
    // The bus named for the plugin, so the configuration query is its sidecar's.
    let h = harness("oms-1");
    reported(&h, true, declared(), 1).await;
    let mut changes = h.bus.subscribe(PLUGIN_CONFIGURATION_CHANGED);
    set(&h, "oms-1", &[("api_key", SECRET)], &[]).await.unwrap();

    let delivery = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("announced")
        .expect("open");
    assert!(
        !contains(&delivery.envelope.payload, SECRET),
        "the announcement carries no setting"
    );
    let event = PluginConfigurationChangedEvent::decode(&delivery.envelope.payload[..]).unwrap();
    assert_eq!(event.plugin_instance_id, "oms-1");

    let configured = told(&h).await;
    assert_eq!(
        configured.settings,
        [PluginSettingValue {
            name: "api_key".into(),
            value: SECRET.into()
        }]
    );

    // Cleared, it is gone from both.
    let record = set(&h, "oms-1", &[], &["api_key"]).await.unwrap();
    assert!(record.secrets_set.is_empty());
    assert!(told(&h).await.settings.is_empty());
}

#[tokio::test]
async fn with_no_settings_key_a_secret_is_refused_and_nothing_else_in_the_request_is_stored() {
    let h = harness_keyed("dashboard-1", SettingsKey::none());
    reported(&h, true, declared(), 1).await;
    let refused = set(
        &h,
        "oms-1",
        &[("poll_minutes", "15"), ("api_key", SECRET)],
        &[],
    )
    .await
    .unwrap_err();
    assert!(refused.contains("api_key"), "{refused}");
    assert!(!refused.contains(SECRET));
    assert!(h.store.snapshot().unwrap().settings.is_empty());

    // What is not secret needs no key.
    set(&h, "oms-1", &[("poll_minutes", "15")], &[])
        .await
        .unwrap();
}

#[tokio::test]
async fn a_secret_that_no_longer_opens_is_withheld_rather_than_sent_sealed() {
    let h = harness("oms-1");
    reported(&h, true, declared(), 1).await;
    // Sealed under another key, as after a key was lost and made again.
    let other = SettingsKey::holding(&[9u8; 32]);
    h.store
        .put_plugin_settings(
            "oms-1",
            &[crate::store::SettingChange {
                name: "api_key".into(),
                held: Some(Held::Sealed(
                    other.seal("oms-1", "api_key", SECRET).unwrap(),
                )),
            }],
            ADA,
            1,
        )
        .unwrap();
    assert!(told(&h).await.settings.is_empty());
    assert_eq!(
        records(&h).await.plugin_settings[0].secrets_set,
        ["api_key"]
    );
}
