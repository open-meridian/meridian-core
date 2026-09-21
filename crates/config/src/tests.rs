//! The configuration store's service, over the bus, on the memory store.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessLevel, AccessRecords, AccessRecordsRequest, AccountGroup,
    AccountRecord, AccountState, CloseAccountRequest, DefineAccessGroupRequest,
    DefineAccountGroupRequest, DefineAccountRequest, DefineUserGroupRequest, DiagnosticBundle,
    DiagnosticBundleReceipt, ExternalAccountLink, GrantPermissionRequest,
    LinkExternalAccountRequest, Permission, PluginConfiguration, PluginConfigurationChangedEvent,
    PluginConfigurationRequest, RedeemClaimCodeReply, RedeemClaimCodeRequest, SignInRecord,
    UserGroup, WithdrawPermissionReply, WithdrawPermissionRequest,
};
use prost::Message;

use crate::service::*;
use crate::store::{KnownPlugin, Store};
use crate::{MemoryStore, DEPLOYMENT_ADMIN};

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
    fn honour_claim_code(&self, code: &str) -> Result<RedeemClaimCodeReply, String> {
        self.codes.lock().unwrap().push(code.to_string());
        let redeemed = *self.redeem.lock().unwrap();
        Ok(RedeemClaimCodeReply {
            redeemed,
            refusal_reason: if redeemed {
                String::new()
            } else {
                "expired".into()
            },
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
    platform: Arc<FakePlatform>,
}

/// A bus whose instance is `instance`, so a query "from" a plugin can be made
/// by naming the bus after it.
fn harness(instance: &str) -> Harness {
    let bus = Arc::new(Bus::single(instance, Arc::new(MemoryBackend::new())));
    let store = Arc::new(MemoryStore::new());
    let platform = Arc::new(FakePlatform::default());
    serve(
        Arc::clone(&bus),
        store.clone() as Arc<dyn Store>,
        Arc::new(FixedClock),
        platform.clone() as Arc<dyn Upstream>,
    );
    store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: "oms-1".into(),
            role: "oms".into(),
            tags: vec!["reporting".into()],
            last_reported_at_ns: 0,
        })
        .unwrap();
    Harness { bus, platform }
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
    assert!(configured.write_account_ids.is_empty());
    assert_eq!(configured.links.len(), 1);
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
