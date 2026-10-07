//! The configuration store's service, over the bus, on the memory store.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessRecords, AccessRecordsRequest, AccountGroup, AccountRecord,
    AccountState, Accounts, AccountsRequest, ClaimCodePurpose, CloseAccountRequest,
    DefineAccessGroupRequest, DefineAccountGroupRequest, DefineAccountRequest,
    DefineUserGroupRequest, DiagnosticBundle, DiagnosticBundleReceipt, ExternalAccountLink,
    GrantPermissionRequest, LinkExternalAccountRequest, Permission, PluginConfiguration,
    PluginConfigurationChangedEvent, PluginConfigurationRequest, PluginReport, PluginSettingValue,
    PluginSettingsRecord, RedeemClaimCodeReply, RedeemClaimCodeRequest, SetPluginSettingsRequest,
    SignInRecord, UserGroup, WithdrawPermissionReply, WithdrawPermissionRequest,
};
use meridian_pb::v1::{AccessLevel, SettingChoice, SettingDeclaration, SettingType};
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
    let bus = Arc::new(Bus::single(
        instance,
        backend.clone(),
        Arc::new(meridian_clock::SystemClock),
    ));
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
            last_reported_at_ns: 0,
        })
        .unwrap();
    // The instrument store, as far as a table's instrument cell needs it:
    // a record for every ID beginning INS-.
    bus.serve(RESOLVE_INSTRUMENT, |envelope| {
        let asked =
            meridian_domain::v1::ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        let found = asked.instrument_id.starts_with("INS-");
        let reply = meridian_domain::v1::ResolveInstrumentReply {
            found,
            instrument: found.then(|| meridian_domain::v1::InstrumentRecord {
                instrument_id: asked.instrument_id.clone(),
                ..Default::default()
            }),
        };
        Ok((
            "meridian.v1.ResolveInstrumentReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
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
            name: name.into(),
            ..Default::default()
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

fn entry(level: AccessLevel) -> AccessEntry {
    AccessEntry {
        plugin_instance_id: "oms-1".into(),
        level: level as i32,
        role: "oms".into(),
    }
}

/// A length-delimited field, written by hand: what a sender built before a
/// field was reserved still writes.
fn length_delimited(number: u32, bytes: &[u8], into: &mut Vec<u8>) {
    prost::encoding::encode_key(number, prost::encoding::WireType::LengthDelimited, into);
    prost::encoding::encode_varint(bytes.len() as u64, into);
    into.extend_from_slice(bytes);
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
            ..Default::default()
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

async fn define(h: &Harness, request: DefineAccountRequest) -> Result<AccountRecord, String> {
    ask(
        h,
        DEFINE_ACCOUNT,
        "meridian.v1.DefineAccountRequest",
        request,
    )
    .await
}

#[tokio::test]
async fn an_account_carries_a_custodian_type_owner_and_note_each_edit_sets_whole() {
    // W6.3: free text and optional; an edit sets all four as given, so an
    // empty one clears it.
    let h = harness("dashboard-1");
    let created = define(
        &h,
        DefineAccountRequest {
            name: "Growth Fund".into(),
            custodian: " Fidelity ".into(),
            account_type: "Roth IRA".into(),
            owner: "Fund I".into(),
            note: "Opened for the 2026 rollover.".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(created.custodian, "Fidelity", "trimmed");
    assert_eq!(created.account_type, "Roth IRA");
    assert_eq!(created.owner, "Fund I");
    assert_eq!(created.note, "Opened for the 2026 rollover.");
    assert_eq!(records(&h).await.accounts, std::slice::from_ref(&created));

    let edited = define(
        &h,
        DefineAccountRequest {
            account_id: created.account_id.clone(),
            name: "Growth Fund".into(),
            custodian: "Fidelity".into(),
            account_type: "Roth IRA".into(),
            owner: "Fund II".into(),
            note: String::new(),
        },
    )
    .await
    .unwrap();
    assert_eq!(edited.owner, "Fund II");
    assert_eq!(edited.note, "", "left empty, cleared");
    assert_eq!(edited.created_at_ns, created.created_at_ns);
    assert_eq!(edited.state, AccountState::Open as i32);

    let cleared = define(
        &h,
        DefineAccountRequest {
            account_id: created.account_id.clone(),
            name: "Growth Fund".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        (
            cleared.custodian.as_str(),
            cleared.account_type.as_str(),
            cleared.owner.as_str(),
            cleared.note.as_str()
        ),
        ("", "", "", "")
    );
    assert_eq!(records(&h).await.accounts, [cleared]);
}

#[tokio::test]
async fn an_account_field_past_its_bound_is_refused_naming_it_and_nothing_changes() {
    // 200 characters each, the note 2,000; characters, not bytes.
    let h = harness("dashboard-1");
    let long_note = define(
        &h,
        DefineAccountRequest {
            name: "Growth".into(),
            custodian: "é".repeat(200),
            note: "n".repeat(2_000),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(long_note.note.chars().count(), 2_000, "kept whole");

    for (request, field) in [
        (
            DefineAccountRequest {
                custodian: "c".repeat(201),
                ..Default::default()
            },
            "an account's custodian is 201 characters",
        ),
        (
            DefineAccountRequest {
                account_type: "t".repeat(201),
                ..Default::default()
            },
            "an account's type is 201 characters",
        ),
        (
            DefineAccountRequest {
                owner: "o".repeat(201),
                ..Default::default()
            },
            "an account's owner is 201 characters",
        ),
        (
            DefineAccountRequest {
                note: "n".repeat(2_001),
                ..Default::default()
            },
            "an account's note is 2001 characters, and at most 2000 are kept",
        ),
    ] {
        let refused = define(
            &h,
            DefineAccountRequest {
                account_id: long_note.account_id.clone(),
                name: "Renamed".into(),
                ..request
            },
        )
        .await
        .unwrap_err();
        assert!(refused.contains(field), "{refused}");
    }
    assert_eq!(
        records(&h).await.accounts,
        [long_note],
        "no refused edit changed it"
    );
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
async fn an_access_entry_names_a_plugin_that_has_reported_and_a_level() {
    let h = harness("dashboard-1");
    assert!(access_group(&h, vec![entry(AccessLevel::Write)])
        .await
        .is_ok());
    assert!(access_group(&h, vec![entry(AccessLevel::Read)])
        .await
        .is_ok());

    let no_level = access_group(&h, vec![entry(AccessLevel::Unspecified)])
        .await
        .unwrap_err();
    assert!(no_level.contains("read, write or admin"), "{no_level}");

    let mut unknown = entry(AccessLevel::Read);
    unknown.plugin_instance_id = "never-reported".into();
    assert!(access_group(&h, vec![unknown])
        .await
        .unwrap_err()
        .contains("has reported"));
}

#[tokio::test]
async fn an_access_entry_naming_a_tag_is_refused_not_widened() {
    // decisions/026: a plugin declares no tags. A sender built before it
    // still writes one, in the field now reserved; decoding would drop it and
    // grant the whole plugin, so the conductor looks for it and refuses.
    let h = harness("dashboard-1");
    let mut tagged = entry(AccessLevel::Read).encode_to_vec();
    length_delimited(2, b"reporting", &mut tagged);
    let mut group = AccessGroup {
        name: "Tagged".into(),
        ..Default::default()
    }
    .encode_to_vec();
    length_delimited(3, &tagged, &mut group);
    let mut request = Vec::new();
    length_delimited(1, &group, &mut request);
    assert_eq!(
        DefineAccessGroupRequest::decode(&request[..])
            .unwrap()
            .access_group
            .unwrap()
            .entries,
        [entry(AccessLevel::Read)],
        "decoded, the tag is gone without a word"
    );

    let refused = h
        .bus
        .call_for(
            DEFINE_ACCESS_GROUP,
            "meridian.v1.DefineAccessGroupRequest",
            request,
            None,
            None,
            ADA,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("no tag") && refused.contains("decisions/026"),
        "{refused}"
    );
    assert!(!records(&h)
        .await
        .access_groups
        .iter()
        .any(|g| g.name == "Tagged"));

    // The same entry without it is an ordinary one.
    let untagged = DefineAccessGroupRequest {
        access_group: Some(AccessGroup {
            name: "Plain".into(),
            entries: vec![entry(AccessLevel::Read)],
            ..Default::default()
        }),
    };
    assert_eq!(
        crate::rules::names_no_tag(&untagged.encode_to_vec()),
        Ok(())
    );
    assert!(
        crate::rules::names_no_tag(&[0x0a, 0x05]).is_err(),
        "truncated"
    );
}

#[tokio::test]
async fn a_permission_names_an_account_group_unless_it_is_to_deployment_admin() {
    let h = harness("dashboard-1");
    let traders = user_group(&h).await;
    let growth = account_group(&h, &[&account(&h, "Growth").await]).await;
    let trading = access_group(&h, vec![entry(AccessLevel::Write)])
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
    let trading = access_group(&h, vec![entry(AccessLevel::Write)])
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
    let trading = access_group(&h, vec![entry(AccessLevel::Read)])
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
            ..Default::default()
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
async fn a_sidecar_is_told_the_accounts_its_links_name_and_when_one_is_renamed() {
    // W4.11: the plugin's links reach it with each account's name, so a
    // rename is a change to its configuration, announced like any other.
    let h = harness("oms-1");
    let linked_account = account(&h, "Growth").await;
    let other = account(&h, "Income").await;
    link_for(
        &h,
        link_request("oms-1", &linked_account.account_id, ""),
        ADA,
    )
    .await
    .unwrap();

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
    let configured = told(&h).await;
    assert_eq!(
        configured.linked_accounts,
        std::slice::from_ref(&linked_account),
        "the account its link names, and no other"
    );

    let mut announced = h.bus.subscribe(PLUGIN_CONFIGURATION_CHANGED);
    let rename = |account_id: &str, name: &str| DefineAccountRequest {
        account_id: account_id.into(),
        name: name.into(),
        ..Default::default()
    };
    let _: AccountRecord = ask(
        &h,
        DEFINE_ACCOUNT,
        "meridian.v1.DefineAccountRequest",
        rename(&other.account_id, "Income Fund"),
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), announced.recv())
            .await
            .is_err(),
        "an account no link names moves no plugin"
    );

    let _: AccountRecord = ask(
        &h,
        DEFINE_ACCOUNT,
        "meridian.v1.DefineAccountRequest",
        rename(&linked_account.account_id, "Growth (joint)"),
    )
    .await
    .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), announced.recv())
        .await
        .expect("announced")
        .unwrap();
    let event = PluginConfigurationChangedEvent::decode(&event.envelope.payload[..]).unwrap();
    assert_eq!(event.plugin_instance_id, "oms-1");
    assert_eq!(told(&h).await.linked_accounts[0].name, "Growth (joint)");
}

fn link_request(plugin: &str, account: &str, new_name: &str) -> LinkExternalAccountRequest {
    LinkExternalAccountRequest {
        plugin_instance_id: plugin.into(),
        external_account_id: "st-1".into(),
        account_id: account.into(),
        new_account_name: new_name.into(),
        ..Default::default()
    }
}

/// A link as a plugin's sidecar sends it: from the plugin, for `by`.
async fn link_for(
    h: &Harness,
    request: LinkExternalAccountRequest,
    by: &str,
) -> Result<ExternalAccountLink, String> {
    let (_, bytes) = h
        .bus
        .call_for(
            LINK_EXTERNAL_ACCOUNT,
            "meridian.v1.LinkExternalAccountRequest",
            request.encode_to_vec(),
            None,
            None,
            by,
        )
        .await
        .map_err(|failed| failed.to_string())?;
    Ok(ExternalAccountLink::decode(&bytes[..]).expect("decodes"))
}

#[tokio::test]
async fn a_link_needs_a_plugin_that_has_reported_and_an_open_account() {
    let ghost = harness("ghost-1");
    let unknown = link_for(&ghost, link_request("ghost-1", "", "A new one"), ADA).await;
    assert!(unknown.unwrap_err().contains("plugin that has reported"));

    let h = harness("oms-1");
    let open = account(&h, "Growth").await;
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
    let closed = link_for(&h, link_request("oms-1", &open.account_id, ""), ADA).await;
    assert!(closed.unwrap_err().contains("closed"));
    let missing = link_for(&h, link_request("oms-1", "ACC-NONE", ""), ADA).await;
    assert!(missing
        .unwrap_err()
        .contains("there is no account ACC-NONE"));
}

#[tokio::test]
async fn a_link_naming_a_new_account_creates_and_links_it_in_one_step() {
    // W6.4: from the plugin's own admin page, for the deployment admin.
    let h = harness("oms-1");
    let mut announced = h.bus.subscribe(PLUGIN_CONFIGURATION_CHANGED);
    let linked = link_for(&h, link_request("oms-1", "", "  Fidelity Brokerage "), ADA)
        .await
        .unwrap();
    assert!(
        !linked.account_id.is_empty(),
        "the reply names the new account"
    );
    let read = records(&h).await;
    let made = read
        .accounts
        .iter()
        .find(|a| a.account_id == linked.account_id)
        .expect("the account exists");
    assert_eq!(made.name, "Fidelity Brokerage");
    assert_eq!(made.state, AccountState::Open as i32);
    assert_eq!(read.links, std::slice::from_ref(&linked));
    assert_eq!(linked.plugin_instance_id, "oms-1");
    assert_eq!(linked.external_account_id, "st-1");
    let event = tokio::time::timeout(Duration::from_secs(2), announced.recv())
        .await
        .expect("announced")
        .unwrap();
    let event = PluginConfigurationChangedEvent::decode(&event.envelope.payload[..]).unwrap();
    assert_eq!(event.plugin_instance_id, "oms-1", "its sidecar is told");

    // The link is the plugin's right to the account (W4.11).
    let configured: PluginConfiguration = ask(
        &h,
        PLUGIN_CONFIGURATION,
        "meridian.v1.PluginConfigurationRequest",
        PluginConfigurationRequest {},
    )
    .await
    .unwrap();
    assert_eq!(
        configured.write_account_ids,
        std::slice::from_ref(&linked.account_id)
    );

    // Neither name removes it, and the account stays: records outlive links.
    let unlinked = link_for(&h, link_request("oms-1", "", ""), ADA)
        .await
        .unwrap();
    assert_eq!(unlinked.account_id, "");
    let read = records(&h).await;
    assert!(read.links.is_empty());
    assert!(read
        .accounts
        .iter()
        .any(|a| a.account_id == linked.account_id));
}

#[tokio::test]
async fn a_new_account_made_by_a_link_carries_what_the_plugin_sent_of_it() {
    // W6.4: the plugin pre-fills the custodian and type from the venue, and
    // the admin may change them; held to W6.3's bounds.
    let h = harness("oms-1");
    let linked = link_for(
        &h,
        LinkExternalAccountRequest {
            new_account_custodian: "Fidelity".into(),
            new_account_type: " Roth IRA ".into(),
            new_account_owner: "Fund I".into(),
            new_account_note: "Linked from SnapTrade.".into(),
            ..link_request("oms-1", "", "Fidelity Brokerage")
        },
        ADA,
    )
    .await
    .unwrap();
    let read = records(&h).await;
    let made = read
        .accounts
        .iter()
        .find(|a| a.account_id == linked.account_id)
        .expect("the account exists");
    assert_eq!(made.custodian, "Fidelity");
    assert_eq!(made.account_type, "Roth IRA");
    assert_eq!(made.owner, "Fund I");
    assert_eq!(made.note, "Linked from SnapTrade.");

    let too_long = link_for(
        &h,
        LinkExternalAccountRequest {
            new_account_note: "n".repeat(2_001),
            ..link_request("oms-1", "", "Another")
        },
        ADA,
    )
    .await;
    assert!(too_long
        .unwrap_err()
        .contains("a new account's note is 2001 characters"));
    assert_eq!(
        records(&h).await.accounts.len(),
        1,
        "refused, so nothing was made or linked"
    );
}

#[tokio::test]
async fn a_new_accounts_fields_are_ignored_when_the_link_names_an_existing_one() {
    // They describe a new account; an existing one is edited only by W6.3.
    let h = harness("oms-1");
    let growth = account(&h, "Growth").await;
    let linked = link_for(
        &h,
        LinkExternalAccountRequest {
            new_account_custodian: "Fidelity".into(),
            new_account_note: "n".repeat(2_001),
            ..link_request("oms-1", &growth.account_id, "")
        },
        ADA,
    )
    .await
    .unwrap();
    assert_eq!(linked.account_id, growth.account_id);
    assert_eq!(records(&h).await.accounts, [growth], "unchanged");
}

#[tokio::test]
async fn a_link_naming_both_an_account_and_a_new_one_is_refused_and_changes_nothing() {
    let h = harness("oms-1");
    let growth = account(&h, "Growth").await;
    let both = link_for(
        &h,
        link_request("oms-1", &growth.account_id, "Another"),
        ADA,
    )
    .await;
    assert!(both.unwrap_err().contains("not both"));
    let blank = link_for(&h, link_request("oms-1", "", "   "), ADA).await;
    assert!(blank
        .unwrap_err()
        .contains("a new account's name is required"));
    let read = records(&h).await;
    assert_eq!(read.accounts.len(), 1, "no account was made");
    assert!(read.links.is_empty());
}

#[tokio::test]
async fn a_link_is_a_deployment_admins_act_on_the_plugins_own_accounts() {
    // Sent for nobody, it is refused: there is nobody to record it as.
    let h = harness("oms-1");
    let growth = account(&h, "Growth").await;
    let nobody = link_for(&h, link_request("oms-1", &growth.account_id, ""), "").await;
    assert!(nobody.unwrap_err().contains("sent for nobody"));
    // A plugin names only its own external accounts, whatever it sends.
    let other = link_for(&h, link_request("snaptrade-1", &growth.account_id, ""), ADA).await;
    assert!(other
        .unwrap_err()
        .contains("only its own external accounts"));
    assert!(records(&h).await.links.is_empty());
}

#[tokio::test]
async fn the_deployments_accounts_are_read_for_a_deployment_admin_with_what_describes_them() {
    let h = harness("oms-1");
    let growth = define(
        &h,
        DefineAccountRequest {
            name: "Growth".into(),
            custodian: "Fidelity".into(),
            account_type: "Roth IRA".into(),
            owner: "Fund I".into(),
            note: "Rollover, 2026.".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let old = account(&h, "Old income").await;
    let _: AccountRecord = ask(
        &h,
        CLOSE_ACCOUNT,
        "meridian.v1.CloseAccountRequest",
        CloseAccountRequest {
            account_id: old.account_id.clone(),
        },
    )
    .await
    .unwrap();
    let read: Accounts = ask(
        &h,
        ACCOUNTS,
        "meridian.v1.AccountsRequest",
        AccountsRequest {},
    )
    .await
    .unwrap();
    let said: Vec<(&str, &str, i32)> = read
        .accounts
        .iter()
        .map(|a| (a.account_id.as_str(), a.name.as_str(), a.state))
        .collect();
    assert_eq!(
        said,
        [
            (
                growth.account_id.as_str(),
                "Growth",
                AccountState::Open as i32
            ),
            (
                old.account_id.as_str(),
                "Old income",
                AccountState::Closed as i32
            ),
        ]
    );
    assert_eq!(
        read.accounts[0], growth,
        "its custodian, type, owner and note with it, so a plugin can tell accounts apart"
    );
    let for_nobody = h
        .bus
        .call(
            ACCOUNTS,
            "meridian.v1.AccountsRequest",
            AccountsRequest {}.encode_to_vec(),
            None,
            None,
        )
        .await;
    assert!(for_nobody.is_err(), "for nobody, nothing is answered");
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
    assert_eq!(
        records.permissions.len(),
        2,
        "deployment admin and All plugins (admin), once"
    );
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
    store
        .install_first_admin(
            &group,
            std::slice::from_ref(&permission),
            &crate::Author::default(),
            0,
        )
        .unwrap();

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
    let sidecar = Bus::single(
        publisher,
        h.backend.clone(),
        Arc::new(meridian_clock::SystemClock),
    );
    let report = PluginReport {
        plugin_instance_id: "oms-1".into(),
        roles: vec!["oms".into()],
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

    // Declared plain later, it is cleared, never shown: a value sealed under
    // one declaration is not another's (W6.11).
    let mut plainer = declared();
    plainer[1].secret = false;
    reported(&h, true, plainer, 2).await;
    let form = records(&h).await.plugin_settings;
    assert!(form[0].secrets_set.is_empty());
    assert!(form[0].values.iter().all(|v| v.name != "api_key"));
    assert!(!contains(&form[0].encode_to_vec(), SECRET));
}

#[tokio::test]
async fn the_plugins_own_sidecar_is_told_its_secret_opened() {
    // The bus named for the plugin, so the configuration query is its
    // sidecar's; the settings held as the dashboard's form stores them (the
    // memory bus serves only its own instance's calls, and a plugin sets
    // none of its settings).
    let h = harness("oms-1");
    reported(&h, true, declared(), 1).await;
    let key = SettingsKey::holding(&[7u8; 32]);
    let form = crate::store::SettingsAuthor {
        by: ADA.into(),
        delegation: String::new(),
    };
    let put = |name: &str, held: Option<Held>, at: i64| {
        h.store
            .put_plugin_settings(
                "oms-1",
                &[crate::store::SettingChange {
                    name: name.into(),
                    held,
                }],
                &form,
                at,
            )
            .unwrap()
    };
    put(
        "api_key",
        Some(Held::Sealed(key.seal("oms-1", "api_key", SECRET).unwrap())),
        1,
    );
    put("poll_minutes", Some(Held::Plain("30".into())), 2);
    let mut configured = told(&h).await.settings;
    configured.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        configured,
        [
            PluginSettingValue {
                name: "api_key".into(),
                value: SECRET.into()
            },
            PluginSettingValue {
                name: "poll_minutes".into(),
                value: "30".into()
            }
        ]
    );
    // Cleared, it is gone.
    put("api_key", None, 3);
    assert_eq!(told(&h).await.settings.len(), 1);
}

#[tokio::test]
async fn a_plugin_sets_none_of_its_settings() {
    // Option A (W6.11, 2026-10-05): published by the plugin's own instance,
    // the update is refused, and nothing is stored.
    let h = harness("oms-1");
    reported(&h, true, declared(), 1).await;
    let refused = set(&h, "oms-1", &[("poll_minutes", "30")], &[])
        .await
        .unwrap_err();
    assert!(refused.contains("sets none of its settings"), "{refused}");
    assert!(h.store.snapshot().unwrap().settings.is_empty());
    assert!(h.store.plugin_setting_changes("oms-1").unwrap().is_empty());
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
            &crate::store::SettingsAuthor {
                by: ADA.into(),
                delegation: String::new(),
            },
            1,
        )
        .unwrap();
    assert!(told(&h).await.settings.is_empty());
    assert_eq!(
        records(&h).await.plugin_settings[0].secrets_set,
        ["api_key"]
    );
}

// ── Each change its own record; a table setting (W6.11, contract v14) ───────

#[tokio::test]
async fn the_form_records_each_change_a_secret_only_as_set() {
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
    assert_eq!(record.updated_by, ADA);
    assert_eq!(records(&h).await.plugin_settings[0].updated_by, ADA);
    set(&h, "oms-1", &[], &["poll_minutes"]).await.unwrap();

    let changes = h.store.plugin_setting_changes("oms-1").unwrap();
    let seen: Vec<_> = changes
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.kind,
                c.value.as_deref(),
                c.secret,
                c.by.as_str(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("api_key", crate::ChangeKind::Set, None, true, ADA),
            (
                "poll_minutes",
                crate::ChangeKind::Set,
                Some("15"),
                false,
                ADA
            ),
            ("poll_minutes", crate::ChangeKind::Cleared, None, false, ADA),
        ]
    );
    assert!(
        !format!("{changes:?}").contains(SECRET),
        "never a secret's value"
    );
}

fn plan_codes() -> SettingDeclaration {
    let column =
        |name: &str, kind: meridian_pb::v1::SettingColumnType| meridian_pb::v1::SettingColumn {
            name: name.into(),
            r#type: kind as i32,
            required: true,
            ..Default::default()
        };
    SettingDeclaration {
        name: "plan_code_links".into(),
        r#type: SettingType::Table as i32,
        columns: vec![
            column(
                "account",
                meridian_pb::v1::SettingColumnType::ExternalAccount,
            ),
            column("code", meridian_pb::v1::SettingColumnType::Text),
            column("instrument", meridian_pb::v1::SettingColumnType::Instrument),
        ],
        ..Default::default()
    }
}

fn rows_of(record: &PluginSettingsRecord) -> Vec<meridian_domain::setting_table::Row> {
    let value = &record
        .values
        .iter()
        .find(|v| v.name == "plan_code_links")
        .expect("held")
        .value;
    meridian_domain::setting_table::parse(value).unwrap()
}

/// oms-1 links these external accounts, as a table's account cell needs.
fn linked(h: &Harness, accounts: &[&str]) {
    for account in accounts {
        h.store
            .put_link(&ExternalAccountLink {
                plugin_instance_id: "oms-1".into(),
                external_account_id: (*account).into(),
                account_id: format!("ACC-{account}"),
            })
            .unwrap();
    }
}

#[tokio::test]
async fn a_table_cell_names_an_account_the_plugin_reported_or_links_and_an_instrument_held() {
    let h = harness("dashboard-1");
    let mut declared = declared();
    declared.push(plan_codes());
    reported(&h, true, declared, 1).await;
    linked(&h, &["st-1"]);

    // Neither reported nor linked, and no record: each cell named, nothing
    // stored, the value never repeated.
    let unheld = r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7"},{"account":"st-9","code":"ABCD","instrument":"NOPE-1"}]"#;
    let refused = set(&h, "oms-1", &[("plan_code_links", unheld)], &[])
        .await
        .unwrap_err();
    assert!(
        refused.contains("plan_code_links[1].account: names no external account"),
        "{refused}"
    );
    assert!(
        refused.contains("plan_code_links[1].instrument: names no instrument record"),
        "{refused}"
    );
    assert!(!refused.contains("plan_code_links[0]"), "{refused}");
    assert!(h.store.snapshot().unwrap().settings.is_empty());

    // Reported by the plugin's custody connection, st-9 reads; its topic
    // names the instance, which only that instance may publish on.
    let custody = Bus::single(
        "oms-1",
        h.backend.clone(),
        Arc::new(meridian_clock::SystemClock),
    );
    custody
        .publish(
            "platform.custody.oms-1.event.external-accounts",
            "meridian.v1.ExternalAccountsEvent",
            meridian_domain::v1::ExternalAccountsEvent {
                accounts: vec![meridian_domain::v1::ExternalAccount {
                    external_account_id: "st-9".into(),
                    ..Default::default()
                }],
            }
            .encode_to_vec(),
            None,
            None,
        )
        .unwrap();
    let held = r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7"},{"account":"st-9","code":"ABCD","instrument":"INS-8"}]"#;
    let mut outcome = Err(String::new());
    for _ in 0..200 {
        outcome = set(&h, "oms-1", &[("plan_code_links", held)], &[]).await;
        if outcome.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(rows_of(&outcome.unwrap()).len(), 2);

    // Another instance's report is not this plugin's.
    let other = r#"[{"account":"st-9","code":"ABCD","instrument":"INS-8"},{"account":"st-5","code":"X","instrument":"INS-8"}]"#;
    let elsewhere = Bus::single(
        "oms-2",
        h.backend.clone(),
        Arc::new(meridian_clock::SystemClock),
    );
    elsewhere
        .publish(
            "platform.custody.oms-2.event.external-accounts",
            "meridian.v1.ExternalAccountsEvent",
            meridian_domain::v1::ExternalAccountsEvent {
                accounts: vec![meridian_domain::v1::ExternalAccount {
                    external_account_id: "st-5".into(),
                    ..Default::default()
                }],
            }
            .encode_to_vec(),
            None,
            None,
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let refused = set(&h, "oms-1", &[("plan_code_links", other)], &[])
        .await
        .unwrap_err();
    assert!(refused.contains("plan_code_links[1].account"), "{refused}");
}

#[tokio::test]
async fn a_table_settings_rows_are_checked_and_each_row_changed_is_stamped() {
    let h = harness("dashboard-1");
    let mut declared = declared();
    declared.push(plan_codes());
    reported(&h, true, declared, 1).await;
    linked(&h, &["st-1", "st-2"]);

    let one = r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7"}]"#;
    let record = set(&h, "oms-1", &[("plan_code_links", one)], &[])
        .await
        .unwrap();
    let rows = rows_of(&record);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].changed_by, ADA);
    assert_eq!(rows[0].changed_at, "2026-09-26T00:00:00.000000Z");

    // A row added beside it: the one standing keeps its stamps. (Here both
    // are stamped at the harness's one moment; the standing row is the
    // same object, so its stamps are its own.)
    let two = r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7"},{"account":"st-2","code":"ABCD","instrument":"INS-8"}]"#;
    let rows = rows_of(
        &set(&h, "oms-1", &[("plan_code_links", two)], &[])
            .await
            .unwrap(),
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].cells["code"], "OQKR");

    // A cell that does not read is named by its path; nothing is stored.
    let bad = r#"[{"account":"st-1","code":"","instrument":"has space"}]"#;
    let refused = set(&h, "oms-1", &[("plan_code_links", bad)], &[])
        .await
        .unwrap_err();
    assert!(refused.contains("plan_code_links[0].code"), "{refused}");
    assert!(
        refused.contains("plan_code_links[0].instrument"),
        "{refused}"
    );
    // Stamps are the conductor's to add.
    let stamped = r#"[{"account":"st-1","code":"X","instrument":"I","changed_by":"me"}]"#;
    let refused = set(&h, "oms-1", &[("plan_code_links", stamped)], &[])
        .await
        .unwrap_err();
    assert!(refused.contains("which the conductor stamps"), "{refused}");
    assert_eq!(
        rows_of(&records(&h).await.plugin_settings[0]).len(),
        2,
        "the refusals stored nothing"
    );
}

#[tokio::test]
async fn an_unchanged_row_keeps_who_changed_it_and_when() {
    let h = harness("dashboard-1");
    let mut declared = declared();
    declared.push(plan_codes());
    reported(&h, true, declared, 1).await;
    linked(&h, &["st-1", "st-2"]);
    // A row Ben stamped earlier, as held.
    let ben = meridian_domain::setting_table::Row {
        cells: [
            ("account", "st-1"),
            ("code", "OQKR"),
            ("instrument", "INS-7"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
        changed_by: "local|ben".into(),
        changed_at: "2026-09-01T00:00:00.000000Z".into(),
    };
    h.store
        .put_plugin_settings(
            "oms-1",
            &[crate::store::SettingChange {
                name: "plan_code_links".into(),
                held: Some(Held::Plain(meridian_domain::setting_table::written(
                    std::slice::from_ref(&ben),
                ))),
            }],
            &crate::store::SettingsAuthor {
                by: "local|ben".into(),
                delegation: String::new(),
            },
            1,
        )
        .unwrap();
    let two = r#"[{"account":"st-2","code":"ABCD","instrument":"INS-8"},{"account":"st-1","code":"OQKR","instrument":"INS-7"}]"#;
    let rows = rows_of(
        &set(&h, "oms-1", &[("plan_code_links", two)], &[])
            .await
            .unwrap(),
    );
    assert_eq!(rows[1], ben, "unchanged, it keeps Ben and his time");
    assert_eq!(rows[0].changed_by, ADA, "the row added is Ada's");
}

// ── A setting re-declared; who last changed them; redaction (W6.11) ─────────

/// Set as `by`, as the dashboard's form does for whoever is signed in.
async fn set_as(
    h: &Harness,
    by: &str,
    values: &[(&str, &str)],
    cleared: &[&str],
) -> Result<PluginSettingsRecord, String> {
    let request = SetPluginSettingsRequest {
        plugin_instance_id: "oms-1".into(),
        values: values
            .iter()
            .map(|(name, value)| PluginSettingValue {
                name: (*name).into(),
                value: (*value).into(),
            })
            .collect(),
        cleared: cleared.iter().map(|name| (*name).into()).collect(),
    };
    let (_, bytes) = h
        .bus
        .call_for(
            SET_PLUGIN_SETTINGS,
            "meridian.v1.SetPluginSettingsRequest",
            request.encode_to_vec(),
            None,
            None,
            by,
        )
        .await
        .map_err(|failed| failed.to_string())?;
    Ok(PluginSettingsRecord::decode(&bytes[..]).unwrap())
}

/// What oms-1's sidecar would be told now.
fn delivered(h: &Harness) -> Vec<PluginSettingValue> {
    configuration(
        &h.store.snapshot().unwrap(),
        "oms-1",
        &SettingsKey::holding(&[7u8; 32]),
    )
    .settings
}

/// A text value forged to read as a table's rows, stamps and all.
const FORGED: &str = r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7","changed_by":"local|mallory","changed_at":"2020-01-01T00:00:00.000000Z"}]"#;

/// What oms-1 declares, plan_code_links being text.
fn plan_codes_as_text() -> Vec<SettingDeclaration> {
    let mut declared = declared();
    declared.push(declaration(
        "plan_code_links",
        SettingType::String,
        false,
        false,
    ));
    declared
}

#[tokio::test]
async fn a_value_held_is_cleared_when_its_setting_is_redeclared_with_another_type() {
    let h = harness("dashboard-1");
    linked(&h, &["st-1"]);
    reported(&h, true, plan_codes_as_text(), 1).await;
    // Text reads anything, the forgery included.
    set_as(&h, "local|mallory", &[("plan_code_links", FORGED)], &[])
        .await
        .unwrap();
    let mut changes = h.bus.subscribe(PLUGIN_CONFIGURATION_CHANGED);

    // A later version declares the name a table.
    let mut declared = declared();
    declared.push(plan_codes());
    reported(&h, true, declared.clone(), 2).await;

    let form = &records(&h).await.plugin_settings[0];
    assert!(form.values.iter().all(|v| v.name != "plan_code_links"));
    assert!(delivered(&h).iter().all(|v| v.name != "plan_code_links"));
    let last = h.store.plugin_setting_changes("oms-1").unwrap();
    let clear = last.last().unwrap();
    assert_eq!(
        (clear.name.as_str(), clear.kind, clear.by.as_str()),
        ("plan_code_links", crate::ChangeKind::Cleared, ""),
        "the clear is its own record, naming no person"
    );
    assert!(clear.note.contains("re-declaration"), "{}", clear.note);
    let event = tokio::time::timeout(Duration::from_secs(2), changes.recv())
        .await
        .expect("announced")
        .expect("open");
    assert_eq!(
        PluginConfigurationChangedEvent::decode(&event.envelope.payload[..])
            .unwrap()
            .plugin_instance_id,
        "oms-1"
    );

    // Declared the same again, nothing more is cleared.
    let count = last.len();
    reported(&h, true, declared, 3).await;
    assert_eq!(
        h.store.plugin_setting_changes("oms-1").unwrap().len(),
        count
    );

    // The next save carries no forged stamp: every row is Ada's.
    let rows = rows_of(
        &set(
            &h,
            "oms-1",
            &[(
                "plan_code_links",
                r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7"}]"#,
            )],
            &[],
        )
        .await
        .unwrap(),
    );
    assert_eq!(rows[0].changed_by, ADA);
}

#[tokio::test]
async fn a_value_held_from_an_earlier_declaration_that_does_not_read_is_withheld() {
    let h = harness("dashboard-1");
    linked(&h, &["st-1"]);
    let mut declared = declared();
    declared.push(plan_codes());
    reported(&h, true, declared, 1).await;
    // Held under the table's declaration, though nothing here wrote it so:
    // as a store from before re-declarations cleared would hold it.
    let author = crate::store::SettingsAuthor {
        by: "local|mallory".into(),
        delegation: String::new(),
    };
    for unread in [
        // Rows without the conductor's stamps.
        r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7"}]"#,
        // A cell the declaration has no column for.
        r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7","secret":"x","changed_by":"a","changed_at":"b"}]"#,
        // Not rows at all.
        "OQKR",
    ] {
        h.store
            .put_plugin_settings(
                "oms-1",
                &[crate::store::SettingChange {
                    name: "plan_code_links".into(),
                    held: Some(Held::Plain(unread.into())),
                }],
                &author,
                1,
            )
            .unwrap();
        assert!(delivered(&h).iter().all(|v| v.name != "plan_code_links"));
        let form = &records(&h).await.plugin_settings[0];
        assert!(form.values.iter().all(|v| v.name != "plan_code_links"));
        assert!(form.secrets_set.is_empty());
    }
    // A save over it stamps every row afresh, lending none the held stamps.
    let rows = rows_of(
        &set(
            &h,
            "oms-1",
            &[(
                "plan_code_links",
                r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7"}]"#,
            )],
            &[],
        )
        .await
        .unwrap(),
    );
    assert_eq!(rows[0].changed_by, ADA);
}

#[tokio::test]
async fn a_sealed_value_redeclared_as_a_table_is_cleared_and_no_table_is_stored_over_one() {
    let h = harness("dashboard-1");
    linked(&h, &["st-1"]);
    let mut secret = declared();
    secret.push(declaration(
        "plan_code_links",
        SettingType::String,
        false,
        true,
    ));
    reported(&h, true, secret, 1).await;
    set(&h, "oms-1", &[("plan_code_links", SECRET)], &[])
        .await
        .unwrap();

    let mut table = declared();
    table.push(plan_codes());
    reported(&h, true, table, 2).await;
    let form = &records(&h).await.plugin_settings[0];
    assert!(form.secrets_set.is_empty(), "not shown as a secret's field");
    assert!(h
        .store
        .snapshot()
        .unwrap()
        .settings
        .iter()
        .all(|s| s.name != "plan_code_links"));

    // Defensively: a sealed value held under the table, as a store from
    // before would hold it, is never delivered as rows, and no table is
    // stored over it until it is cleared.
    let key = SettingsKey::holding(&[7u8; 32]);
    h.store
        .put_plugin_settings(
            "oms-1",
            &[crate::store::SettingChange {
                name: "plan_code_links".into(),
                held: Some(Held::Sealed(
                    key.seal("oms-1", "plan_code_links", SECRET).unwrap(),
                )),
            }],
            &crate::store::SettingsAuthor {
                by: ADA.into(),
                delegation: String::new(),
            },
            3,
        )
        .unwrap();
    assert!(delivered(&h).iter().all(|v| v.name != "plan_code_links"));
    let one = r#"[{"account":"st-1","code":"OQKR","instrument":"INS-7"}]"#;
    let refused = set(&h, "oms-1", &[("plan_code_links", one)], &[])
        .await
        .unwrap_err();
    assert!(refused.contains("holds a sealed value"), "{refused}");
    set(&h, "oms-1", &[], &["plan_code_links"]).await.unwrap();
    assert_eq!(
        rows_of(
            &set(&h, "oms-1", &[("plan_code_links", one)], &[])
                .await
                .unwrap()
        )
        .len(),
        1
    );
    let changes = h.store.plugin_setting_changes("oms-1").unwrap();
    assert!(!format!("{changes:?}").contains(SECRET));
}

#[tokio::test]
async fn who_last_changed_the_settings_is_the_latest_change_a_clear_included() {
    let h = harness("dashboard-1");
    reported(&h, true, declared(), 1).await;
    set(
        &h,
        "oms-1",
        &[("poll_minutes", "15"), ("synthetic", "true")],
        &[],
    )
    .await
    .unwrap();
    let record = set_as(&h, "local|ben", &[], &["poll_minutes"])
        .await
        .unwrap();
    assert_eq!(record.updated_by, "local|ben");
    assert_eq!(records(&h).await.plugin_settings[0].updated_by, "local|ben");
    // Everything cleared, it still says who cleared it, and when.
    let record = set_as(&h, "local|grace", &[], &["synthetic"])
        .await
        .unwrap();
    assert_eq!(record.updated_by, "local|grace");
    assert_eq!(record.updated_at_ns, FixedClock.now_ns());
    assert!(record.values.is_empty());
}

#[tokio::test]
async fn a_setting_redeclared_secret_has_its_earlier_values_redacted() {
    let h = harness("dashboard-1");
    reported(&h, true, declared(), 1).await;
    set(&h, "oms-1", &[("poll_minutes", "15")], &[])
        .await
        .unwrap();
    set_as(&h, "local|ben", &[("poll_minutes", "20")], &[])
        .await
        .unwrap();
    let before = h.store.plugin_setting_changes("oms-1").unwrap();

    let mut secret = declared();
    secret[2].secret = true;
    reported(&h, true, secret.clone(), 2).await;
    let after = h.store.plugin_setting_changes("oms-1").unwrap();
    // Nothing deleted, who and when kept, only the value blanked.
    assert_eq!(
        after.len(),
        before.len() + 2,
        "the clear, then the redaction"
    );
    for (was, is) in before.iter().zip(&after) {
        assert_eq!(
            (&was.name, was.kind, &was.by, was.at_ns),
            (&is.name, is.kind, &is.by, is.at_ns)
        );
        assert_eq!(is.value, None);
    }
    let redaction = after.last().unwrap();
    assert_eq!(redaction.kind, crate::ChangeKind::Redacted);
    assert_eq!(redaction.by, "");
    assert!(
        redaction.note.contains("records 1, 2"),
        "{}",
        redaction.note
    );
    assert!(
        redaction.note.contains("became secret"),
        "{}",
        redaction.note
    );
    // Reported again, nothing more is redacted.
    reported(&h, true, secret, 3).await;
    assert_eq!(
        h.store.plugin_setting_changes("oms-1").unwrap().len(),
        after.len()
    );

    // A value stored sealed redacts what earlier records of it hold, as a
    // store from before would keep a secret's plain value.
    h.store
        .put_plugin_settings(
            "oms-1",
            &[crate::store::SettingChange {
                name: "api_key".into(),
                held: Some(Held::Plain("plain-before".into())),
            }],
            &crate::store::SettingsAuthor {
                by: ADA.into(),
                delegation: String::new(),
            },
            4,
        )
        .unwrap();
    set(&h, "oms-1", &[("api_key", SECRET)], &[]).await.unwrap();
    let changes = h.store.plugin_setting_changes("oms-1").unwrap();
    let last = changes.last().unwrap();
    assert_eq!(last.kind, crate::ChangeKind::Redacted);
    assert!(last.note.contains("sealed"), "{}", last.note);
    assert!(!format!("{changes:?}").contains("plain-before"));
}

// ── A plugin has admins; the built-in groups (2026-09-30) ─────────────────

#[tokio::test]
async fn an_access_group_names_a_plugin_at_admin_and_one_data_level_at_most() {
    let h = harness("dashboard-1");
    assert!(access_group(&h, vec![entry(AccessLevel::Admin)])
        .await
        .is_ok());
    assert!(
        access_group(
            &h,
            vec![entry(AccessLevel::Write), entry(AccessLevel::Admin)]
        )
        .await
        .is_ok(),
        "admin beside a data level"
    );
    let both = access_group(
        &h,
        vec![entry(AccessLevel::Read), entry(AccessLevel::Write)],
    )
    .await
    .unwrap_err();
    assert!(both.contains("both read and write"), "{both}");
    let twice = access_group(
        &h,
        vec![entry(AccessLevel::Admin), entry(AccessLevel::Admin)],
    )
    .await
    .unwrap_err();
    assert!(twice.contains("twice"), "{twice}");
    let none = access_group(&h, vec![entry(AccessLevel::Unspecified)])
        .await
        .unwrap_err();
    assert!(none.contains("read, write or admin"), "{none}");
}

#[tokio::test]
async fn a_permission_granting_only_admin_names_no_account_group() {
    let h = harness("dashboard-1");
    let people = user_group(&h).await;
    let growth = account_group(&h, &[&account(&h, "Growth").await]).await;
    let admins = access_group(&h, vec![entry(AccessLevel::Admin)])
        .await
        .unwrap();
    let refused = grant(
        &h,
        &people.user_group_id,
        &growth.account_group_id,
        &admins.access_group_id,
    )
    .await
    .unwrap_err();
    assert!(refused.contains("names no account group"), "{refused}");
    assert!(
        grant(&h, &people.user_group_id, "", &admins.access_group_id)
            .await
            .is_ok()
    );

    // And the group, granted so, cannot gain a data entry, which would need
    // an account group it has none of.
    let mut widened = admins.clone();
    widened.entries.push(entry(AccessLevel::Read));
    let refused: Result<AccessGroup, String> = ask(
        &h,
        DEFINE_ACCESS_GROUP,
        "meridian.v1.DefineAccessGroupRequest",
        DefineAccessGroupRequest {
            access_group: Some(widened),
        },
    )
    .await;
    assert!(refused.unwrap_err().contains("needs one"));

    // A group with a data entry beside admin still names one.
    let operators = access_group(
        &h,
        vec![entry(AccessLevel::Write), entry(AccessLevel::Admin)],
    )
    .await
    .unwrap();
    assert!(
        grant(&h, &people.user_group_id, "", &operators.access_group_id)
            .await
            .is_err()
    );
    assert!(grant(
        &h,
        &people.user_group_id,
        &growth.account_group_id,
        &operators.access_group_id
    )
    .await
    .is_ok());
}

#[tokio::test]
async fn all_plugins_admin_and_all_accounts_are_built_in() {
    let h = harness("dashboard-1");
    let people = user_group(&h).await;
    let growth = account_group(&h, &[&account(&h, "Growth").await]).await;
    let before = records(&h).await;
    assert!(before
        .access_groups
        .iter()
        .any(|g| g.access_group_id == meridian_access::ALL_PLUGINS_ADMIN && g.built_in));
    assert!(before
        .account_groups
        .iter()
        .any(|g| g.account_group_id == meridian_access::ALL_ACCOUNTS && g.built_in));

    // Neither is edited.
    let edited: Result<AccessGroup, String> = ask(
        &h,
        DEFINE_ACCESS_GROUP,
        "meridian.v1.DefineAccessGroupRequest",
        DefineAccessGroupRequest {
            access_group: Some(AccessGroup {
                access_group_id: meridian_access::ALL_PLUGINS_ADMIN.into(),
                name: "Everything".into(),
                ..Default::default()
            }),
        },
    )
    .await;
    assert!(edited.unwrap_err().contains("built in"));
    let edited: Result<AccountGroup, String> = ask(
        &h,
        DEFINE_ACCOUNT_GROUP,
        "meridian.v1.DefineAccountGroupRequest",
        DefineAccountGroupRequest {
            account_group: Some(AccountGroup {
                account_group_id: meridian_access::ALL_ACCOUNTS.into(),
                name: "Some accounts".into(),
                ..Default::default()
            }),
        },
    )
    .await;
    assert!(edited.unwrap_err().contains("built in"));

    // All plugins (admin) names no account group, and may be withdrawn.
    assert!(grant(
        &h,
        &people.user_group_id,
        &growth.account_group_id,
        meridian_access::ALL_PLUGINS_ADMIN
    )
    .await
    .is_err());
    let linked = grant(
        &h,
        &people.user_group_id,
        "",
        meridian_access::ALL_PLUGINS_ADMIN,
    )
    .await
    .unwrap();
    let withdrawn: WithdrawPermissionReply = ask(
        &h,
        WITHDRAW_PERMISSION,
        "meridian.v1.WithdrawPermissionRequest",
        WithdrawPermissionRequest {
            permission_id: linked.permission_id,
        },
    )
    .await
    .unwrap();
    assert!(withdrawn.withdrawn);

    // All accounts is named like any other.
    let readers = access_group(&h, vec![entry(AccessLevel::Read)])
        .await
        .unwrap();
    assert!(grant(
        &h,
        &people.user_group_id,
        meridian_access::ALL_ACCOUNTS,
        &readers.access_group_id
    )
    .await
    .is_ok());
}

#[tokio::test]
async fn a_claim_code_links_the_first_deployment_admin_to_all_plugins_admin() {
    let h = harness("dashboard-1");
    *h.platform.redeem.lock().unwrap() = true;
    assert!(redeem(&h, ADA).await.redeemed);
    let after = records(&h).await;
    let granted: Vec<&str> = after
        .permissions
        .iter()
        .map(|p| p.access_group_id.as_str())
        .collect();
    assert_eq!(
        granted,
        [DEPLOYMENT_ADMIN, meridian_access::ALL_PLUGINS_ADMIN]
    );
    let ada = meridian_access::person_access(&after, ADA, &[]);
    assert!(ada.deployment_admin && ada.administers("oms-1"));
}

#[tokio::test]
async fn an_account_has_one_external_account_and_two_of_one_connection_are_two_accounts() {
    // W6.4; the product owner, 2026-10-01: "Two custodians are considered two
    // accounts", and "nothing preclude a fund to have two accounts at the
    // same custodian for different purpose".
    let h = harness("oms-1");
    let growth = account(&h, "Growth").await;
    let income = account(&h, "Income").await;
    link_for(&h, link_request("oms-1", &growth.account_id, ""), ADA)
        .await
        .expect("the first");
    link_for(&h, link_request("oms-1", &growth.account_id, ""), ADA)
        .await
        .expect("the same link again");

    let second = LinkExternalAccountRequest {
        external_account_id: "st-2".into(),
        ..link_request("oms-1", &growth.account_id, "")
    };
    let refused = link_for(&h, second.clone(), ADA).await.unwrap_err();
    assert!(
        refused.contains(&format!(
            "{} already has external account st-1 linked (oms-1); an account has one external \
             account: link st-2 to another account, or a new one",
            growth.account_id
        )),
        "{refused}"
    );
    assert_eq!(records(&h).await.links.len(), 1, "nothing was linked");

    // Another external account of the same connection, to another account.
    let admitted = link_for(
        &h,
        LinkExternalAccountRequest {
            account_id: income.account_id.clone(),
            ..second
        },
        ADA,
    )
    .await
    .expect("admitted");
    assert_eq!(admitted.account_id, income.account_id);
    assert_eq!(records(&h).await.links.len(), 2);
}

#[test]
fn accounts_linked_twice_before_the_rule_are_reported_and_kept() {
    let snapshot = crate::store::Snapshot {
        links: vec![
            ExternalAccountLink {
                plugin_instance_id: "snaptrade-1".into(),
                external_account_id: "st-1".into(),
                account_id: "ACC-1".into(),
            },
            ExternalAccountLink {
                plugin_instance_id: "snaptrade-1".into(),
                external_account_id: "st-2".into(),
                account_id: "ACC-1".into(),
            },
            ExternalAccountLink {
                plugin_instance_id: "snaptrade-1".into(),
                external_account_id: "st-3".into(),
                account_id: "ACC-2".into(),
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        crate::rules::accounts_linked_twice(&snapshot),
        vec![(
            "ACC-1".to_string(),
            vec![
                "st-1 (snaptrade-1)".to_string(),
                "st-2 (snaptrade-1)".to_string()
            ]
        )]
    );
}

// ── Access per role (contract v15, decisions/033) ─────────────────────────

fn on_role(plugin: &str, role: &str, level: AccessLevel) -> AccessEntry {
    AccessEntry {
        plugin_instance_id: plugin.into(),
        level: level as i32,
        role: role.into(),
    }
}

/// A plugin holding custody and operations, and one holding none.
fn two_roles_and_none(h: &Harness) {
    h.store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: "ops-1".into(),
            roles: vec!["custody".into(), "operations".into()],
            last_reported_at_ns: 0,
        })
        .unwrap();
    h.store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: "tool-1".into(),
            roles: vec![],
            last_reported_at_ns: 0,
        })
        .unwrap();
}

#[tokio::test]
async fn an_access_entry_names_one_role_its_plugin_holds() {
    let h = harness("dashboard-1");
    two_roles_and_none(&h);
    let per_role = access_group(
        &h,
        vec![
            on_role("ops-1", "operations", AccessLevel::Write),
            on_role("ops-1", "custody", AccessLevel::Read),
            on_role("ops-1", "custody", AccessLevel::Admin),
        ],
    )
    .await
    .unwrap();
    assert_eq!(per_role.entries[0].role, "operations");

    let stranger = access_group(&h, vec![on_role("ops-1", "oms", AccessLevel::Write)])
        .await
        .unwrap_err();
    assert!(
        stranger.contains("not launched with") && stranger.contains("custody, operations"),
        "{stranger}"
    );
    let component = access_group(&h, vec![on_role("ops-1", "street", AccessLevel::Read)])
        .await
        .unwrap_err();
    assert!(component.contains("custody, operations"), "{component}");
    let role_less = access_group(&h, vec![on_role("ops-1", "", AccessLevel::Read)])
        .await
        .unwrap_err();
    assert!(role_less.contains("names no role"), "{role_less}");
    let on_none = access_group(&h, vec![on_role("tool-1", "custody", AccessLevel::Read)])
        .await
        .unwrap_err();
    assert!(on_none.contains("granted as a whole"), "{on_none}");
    assert!(
        access_group(&h, vec![on_role("tool-1", "", AccessLevel::Read)])
            .await
            .is_ok()
    );
    let both = access_group(
        &h,
        vec![
            on_role("ops-1", "custody", AccessLevel::Read),
            on_role("ops-1", "custody", AccessLevel::Write),
        ],
    )
    .await
    .unwrap_err();
    assert!(both.contains("custody on ops-1 is named at both"), "{both}");
    // Read on one role and write on another is not both.
    assert!(access_group(
        &h,
        vec![
            on_role("ops-1", "custody", AccessLevel::Read),
            on_role("ops-1", "operations", AccessLevel::Write),
        ],
    )
    .await
    .is_ok());
}

#[tokio::test]
async fn an_entry_that_no_longer_matches_is_kept_as_written_never_refused_for_being_there() {
    let h = harness("dashboard-1");
    two_roles_and_none(&h);
    let group = access_group(&h, vec![on_role("ops-1", "custody", AccessLevel::Read)])
        .await
        .unwrap();
    // Relaunched without custody.
    h.store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: "ops-1".into(),
            roles: vec!["operations".into()],
            last_reported_at_ns: 1,
        })
        .unwrap();
    let mut edited = group.clone();
    edited.name = "Trading desk".into();
    edited
        .entries
        .push(on_role("ops-1", "operations", AccessLevel::Write));
    let saved: AccessGroup = ask(
        &h,
        DEFINE_ACCESS_GROUP,
        "meridian.v1.DefineAccessGroupRequest",
        DefineAccessGroupRequest {
            access_group: Some(edited),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        saved.entries[0].role, "custody",
        "kept as the admin wrote it"
    );
    // A new entry naming the dropped role is refused.
    let mut again = saved.clone();
    again
        .entries
        .push(on_role("ops-1", "custody", AccessLevel::Admin));
    let refused: Result<AccessGroup, String> = ask(
        &h,
        DEFINE_ACCESS_GROUP,
        "meridian.v1.DefineAccessGroupRequest",
        DefineAccessGroupRequest {
            access_group: Some(again),
        },
    )
    .await;
    assert!(refused.unwrap_err().contains("not launched with"));
}

#[tokio::test]
async fn the_records_carry_each_known_plugins_roles() {
    let h = harness("dashboard-1");
    two_roles_and_none(&h);
    let known = records(&h).await.known_plugins;
    let named: Vec<(String, Vec<String>)> = known
        .into_iter()
        .map(|plugin| (plugin.plugin_instance_id, plugin.roles))
        .collect();
    assert!(named.contains(&("ops-1".into(), vec!["custody".into(), "operations".into()])));
    assert!(named.contains(&("tool-1".into(), vec![])));
    assert!(named.contains(&("oms-1".into(), vec!["oms".into()])));
}

#[tokio::test]
async fn every_grant_change_is_its_own_record_naming_who_and_when() {
    let h = harness("dashboard-1");
    two_roles_and_none(&h);
    let group = access_group(&h, vec![on_role("ops-1", "custody", AccessLevel::Read)])
        .await
        .unwrap();
    let mut changed = group.clone();
    changed.entries[0].level = AccessLevel::Write as i32;
    let _: AccessGroup = ask(
        &h,
        DEFINE_ACCESS_GROUP,
        "meridian.v1.DefineAccessGroupRequest",
        DefineAccessGroupRequest {
            access_group: Some(changed.clone()),
        },
    )
    .await
    .unwrap();
    // Saved again unchanged: nothing to record.
    let _: AccessGroup = ask(
        &h,
        DEFINE_ACCESS_GROUP,
        "meridian.v1.DefineAccessGroupRequest",
        DefineAccessGroupRequest {
            access_group: Some(changed),
        },
    )
    .await
    .unwrap();
    let people = user_group(&h).await;
    let growth = account_group(&h, &[&account(&h, "Growth").await]).await;
    let permission = grant(
        &h,
        &people.user_group_id,
        &growth.account_group_id,
        &group.access_group_id,
    )
    .await
    .unwrap();
    let withdrawn: WithdrawPermissionReply = ask(
        &h,
        WITHDRAW_PERMISSION,
        "meridian.v1.WithdrawPermissionRequest",
        WithdrawPermissionRequest {
            permission_id: permission.permission_id.clone(),
        },
    )
    .await
    .unwrap();
    assert!(withdrawn.withdrawn);

    let records = h.store.access_changes(&group.access_group_id).unwrap();
    let kinds: Vec<_> = records.iter().map(|record| record.kind).collect();
    use crate::AccessChangeKind::*;
    assert_eq!(kinds, [Defined, Changed, Granted, Withdrawn]);
    assert!(records.iter().all(|record| record.by == ADA));
    assert!(records
        .iter()
        .all(|record| record.at_ns == 1_790_380_800_000_000_000));
    assert_eq!(records[0].was, "");
    assert_eq!(records[0].became, "\"Trading\": ops-1 custody read");
    assert_eq!(records[1].was, "\"Trading\": ops-1 custody read");
    assert_eq!(records[1].became, "\"Trading\": ops-1 custody write");
    assert_eq!(records[2].permission_id, permission.permission_id);
    assert!(records[2].became.contains(&growth.account_group_id));
    assert!(records[3].was.contains(&people.user_group_id) && records[3].became.is_empty());
}

/// Contract v16 (W6.25, W4.7, W6.11): a hold a deployment admin sets reaches
/// the sidecar of each instance it covers on its configuration, the records
/// carry it, and a window setting below it is refused naming the setting.
#[tokio::test]
async fn a_hold_reaches_the_sidecars_it_covers_and_holds_a_window_back() {
    let h = harness("dashboard-1");
    h.store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: "custody-1".into(),
            roles: vec!["custody".into()],
            last_reported_at_ns: 0,
        })
        .unwrap();
    h.store
        .record_declared_settings(
            "custody-1",
            &[
                SettingDeclaration {
                    name: "activity_window_days".into(),
                    r#type: SettingType::Integer as i32,
                    ..Default::default()
                },
                SettingDeclaration {
                    name: "activity_past_window".into(),
                    r#type: SettingType::Choice as i32,
                    choices: ["archived", "kept", "deleted"]
                        .iter()
                        .map(|value| SettingChoice {
                            value: value.to_string(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                },
            ],
            0,
        )
        .unwrap();
    let mut changed = h.bus.subscribe(PLUGIN_CONFIGURATION_CHANGED);
    let set: meridian_domain::v1::Hold = ask(
        &h,
        crate::archive::SET_HOLD,
        "meridian.v1.SetHoldRequest",
        meridian_domain::v1::SetHoldRequest {
            role: "custody".into(),
            days: 2190,
            write_once: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(set.updated_by, ADA);
    let told = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let delivery = changed.recv().await.expect("announced");
            let event =
                PluginConfigurationChangedEvent::decode(&delivery.envelope.payload[..]).unwrap();
            if event.plugin_instance_id == "custody-1" {
                return event;
            }
        }
    })
    .await
    .expect("the custody instance was told its configuration changed");
    assert_eq!(told.plugin_instance_id, "custody-1");
    // What its sidecar is told on its configuration.
    let configured = configuration(
        &h.store.snapshot().unwrap(),
        "custody-1",
        &SettingsKey::holding(&[7u8; 32]),
    );
    assert_eq!(configured.hold_days, 2190);
    assert_eq!(records(&h).await.holds, vec![set]);

    let refused = ask::<_, PluginSettingsRecord>(
        &h,
        SET_PLUGIN_SETTINGS,
        "meridian.v1.SetPluginSettingsRequest",
        SetPluginSettingsRequest {
            plugin_instance_id: "custody-1".into(),
            values: vec![PluginSettingValue {
                name: "activity_window_days".into(),
                value: "30".into(),
            }],
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("activity_window_days: 30 is below the hold of 2,190 days"),
        "{refused}"
    );
    // A role that is no edge role is refused, naming it.
    let oms = ask::<_, meridian_domain::v1::Hold>(
        &h,
        crate::archive::SET_HOLD,
        "meridian.v1.SetHoldRequest",
        meridian_domain::v1::SetHoldRequest {
            role: "oms".into(),
            days: 30,
            write_once: false,
        },
    )
    .await
    .unwrap_err();
    assert!(oms.contains("oms is not an edge role"), "{oms}");
}
