//! The configuration store, against Postgres.
//!
//! What only the database can show: the migration seeding deployment admin,
//! the check that a permission to it names no account group, the two atomic
//! operations holding under concurrency, and a snapshot reading back exactly
//! what was written. Run by `make test-store`; fails loudly without a database.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use meridian_config::store::{
    ChangeKind, Ending, Held, KnownPlugin, SettingChange, SettingsAuthor, Store, Withdrawal,
};
use meridian_config::{PostgresStore, SettingsKey, DEPLOYMENT_ADMIN};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccountGroup, AccountRecord, AccountState, ExternalAccountLink,
    Permission, PluginLaunch, PluginLaunchState, PluginMetadata, PluginVersion, SignInRecord,
    UserGroup,
};
use meridian_pb::v1::{
    AccessLevel, SettingChoice, SettingCondition, SettingDeclaration, SettingType,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A suffix no earlier run used: a process's id repeats in a fresh
/// container, and the test database keeps every schema a run made.
fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

/// A migrated store in a schema of this test's own.
fn store(tag: &str) -> PostgresStore {
    store_at(tag).0
}

/// The same, and the URL that reaches its schema, to read its tables as a
/// database dump would.
fn store_at(tag: &str) -> (PostgresStore, String) {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("config_{tag}_{nanos}_{seq}");
    postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .expect("could not create a schema");
    let url = format!("{}?options=-c%20search_path%3D{name}", base_url());
    let store = PostgresStore::connect(&url, 4).expect("connects");
    assert!(
        store.verify().is_err(),
        "an empty schema is refused before migrating"
    );
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migrates");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migrating twice is a no-op");
    store.verify().expect("recognised after migrating");
    (store, url)
}

fn user_group(id: &str, login: &str) -> UserGroup {
    UserGroup {
        user_group_id: id.into(),
        name: id.into(),
        directory_groups: vec!["desk".into()],
        logins: vec![login.into()],
    }
}

fn admin_permission(id: &str, group: &str) -> Permission {
    Permission {
        permission_id: id.into(),
        user_group_id: group.into(),
        account_group_id: String::new(),
        access_group_id: DEPLOYMENT_ADMIN.into(),
    }
}

#[test]
fn a_fresh_store_has_deployment_admin_and_nobody_holding_it() {
    let store = store("fresh");
    let snapshot = store.snapshot().unwrap();
    let built_in = |id: &str| {
        snapshot
            .records
            .access_groups
            .iter()
            .find(|g| g.access_group_id == id)
            .is_some_and(|g| g.built_in)
    };
    assert!(built_in(DEPLOYMENT_ADMIN));
    assert!(built_in(meridian_access::ALL_PLUGINS_ADMIN));
    assert!(
        snapshot
            .records
            .account_groups
            .iter()
            .any(|g| g.account_group_id == meridian_access::ALL_ACCOUNTS
                && g.built_in
                && g.account_ids.is_empty()),
        "All accounts, listing none of its own"
    );
    assert!(snapshot.records.permissions.is_empty());
}

#[test]
fn what_is_written_is_what_a_snapshot_reads_back() {
    let store = store("roundtrip");
    let account = AccountRecord {
        account_id: "ACC-1".into(),
        name: "Growth".into(),
        state: AccountState::Open as i32,
        created_at_ns: 7,
        ..AccountRecord::default()
    };
    store.put_account(&account).unwrap();
    store
        .put_account(&AccountRecord {
            name: "Growth Fund".into(),
            ..account.clone()
        })
        .unwrap();
    let group = user_group("UG-1", "ada");
    store.put_user_group(&group).unwrap();
    let accounts = AccountGroup {
        account_group_id: "AG-1".into(),
        name: "Growth".into(),
        account_ids: vec!["ACC-1".into()],
        built_in: false,
    };
    store.put_account_group(&accounts).unwrap();
    let access = AccessGroup {
        access_group_id: "AX-1".into(),
        name: "Trading".into(),
        entries: vec![
            AccessEntry {
                plugin_instance_id: "oms-1".into(),
                level: AccessLevel::Write as i32,
                role: String::new(),
            },
            AccessEntry {
                plugin_instance_id: "reporting-1".into(),
                level: AccessLevel::Read as i32,
                role: String::new(),
            },
        ],
        built_in: false,
    };
    store
        .put_access_group(&access, &meridian_config::Author::default(), 0)
        .unwrap();
    let permission = Permission {
        permission_id: "PRM-1".into(),
        user_group_id: "UG-1".into(),
        account_group_id: "AG-1".into(),
        access_group_id: "AX-1".into(),
    };
    store
        .add_permission(&permission, &meridian_config::Author::default(), 0)
        .unwrap();
    let link = ExternalAccountLink {
        plugin_instance_id: "oms-1".into(),
        external_account_id: "st-1".into(),
        account_id: "ACC-1".into(),
    };
    store.put_link(&link).unwrap();
    let plugin = KnownPlugin {
        plugin_instance_id: "oms-1".into(),
        roles: vec!["oms".into()],
        last_reported_at_ns: 3,
    };
    store.record_plugin(&plugin).unwrap();

    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.records.accounts[0].name, "Growth Fund");
    assert_eq!(snapshot.records.accounts[0].created_at_ns, 7);
    assert_eq!(
        snapshot.records.accounts[0].custodian, "",
        "none of the four given, none held"
    );
    assert_eq!(snapshot.records.user_groups, [group]);
    assert_eq!(
        snapshot.records.account_groups,
        [accounts, meridian_config::all_accounts()],
        "beside All accounts, built in"
    );
    assert!(
        snapshot.records.access_groups.contains(&access),
        "entries in order"
    );
    assert_eq!(snapshot.records.permissions, [permission]);
    assert_eq!(snapshot.links, std::slice::from_ref(&link));
    assert_eq!(snapshot.plugins, [plugin]);

    store
        .put_link(&ExternalAccountLink {
            account_id: String::new(),
            ..link
        })
        .unwrap();
    assert!(
        store.snapshot().unwrap().links.is_empty(),
        "an empty account unlinks"
    );
}

#[test]
fn a_new_account_and_its_link_are_written_together_or_not_at_all() {
    // W6.4: a link naming a new account creates it and links it in one step.
    let store = store("account_and_link");
    let account = AccountRecord {
        account_id: "ACC-NEW".into(),
        name: "Fidelity Brokerage".into(),
        state: AccountState::Open as i32,
        created_at_ns: 9,
        custodian: "Fidelity".into(),
        account_type: "Roth IRA".into(),
        ..AccountRecord::default()
    };
    let link = ExternalAccountLink {
        plugin_instance_id: "snaptrade-1".into(),
        external_account_id: "st-acct-4471".into(),
        account_id: "ACC-NEW".into(),
    };
    store.put_account_and_link(&account, &link).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.records.accounts, std::slice::from_ref(&account));
    assert_eq!(snapshot.links, std::slice::from_ref(&link));

    // The same account again fails on the account, and the link it would
    // have moved is left as it was: neither is written.
    let other = ExternalAccountLink {
        external_account_id: "st-acct-9".into(),
        ..link.clone()
    };
    assert!(store.put_account_and_link(&account, &other).is_err());
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.records.accounts.len(), 1);
    assert_eq!(snapshot.links, [link]);
}

#[test]
fn the_table_refuses_a_permission_shaped_wrong_whoever_writes_it() {
    let store = store("shape");
    store.put_user_group(&user_group("UG-1", "ada")).unwrap();
    store
        .put_account_group(&AccountGroup {
            account_group_id: "AG-1".into(),
            name: "g".into(),
            account_ids: vec![],
            built_in: false,
        })
        .unwrap();
    let with_accounts = Permission {
        account_group_id: "AG-1".into(),
        ..admin_permission("PRM-1", "UG-1")
    };
    assert!(
        store
            .add_permission(&with_accounts, &meridian_config::Author::default(), 0)
            .is_err(),
        "deployment admin names no account group"
    );
}

#[test]
fn the_latest_sign_in_stands_even_if_an_older_one_arrives_later() {
    let store = store("signin");
    let at = |when: i64, group: &str| SignInRecord {
        subject: "ada".into(),
        display_name: "Ada".into(),
        directory_groups: vec![group.into()],
        signed_in_at_ns: when,
    };
    store.record_sign_in(&at(9, "trading-desk")).unwrap();
    store.record_sign_in(&at(5, "ops")).unwrap();
    let people = store.snapshot().unwrap().records.people;
    assert_eq!(people.len(), 1);
    assert_eq!(people[0].directory_groups, ["trading-desk"]);
}

#[test]
fn only_one_of_two_concurrent_redemptions_installs_an_admin() {
    let store = Arc::new(store("race"));
    let threads: Vec<_> = (0..2)
        .map(|n| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let group = user_group(&format!("UG-{n}"), &format!("person-{n}"));
                store
                    .install_first_admin(
                        &group,
                        &[admin_permission(&format!("PRM-{n}"), &group.user_group_id)],
                        &meridian_config::Author::default(),
                        0,
                    )
                    .unwrap()
            })
        })
        .collect();
    let installed: Vec<bool> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        installed.iter().filter(|won| **won).count(),
        1,
        "{installed:?}"
    );
    assert_eq!(store.snapshot().unwrap().records.permissions.len(), 1);
}

#[test]
fn two_concurrent_withdrawals_cannot_leave_no_admin() {
    let store = Arc::new(store("lastadmin"));
    for n in 0..2 {
        let group = user_group(&format!("UG-{n}"), &format!("person-{n}"));
        store.put_user_group(&group).unwrap();
        store
            .add_permission(
                &admin_permission(&format!("PRM-{n}"), &group.user_group_id),
                &meridian_config::Author::default(),
                0,
            )
            .unwrap();
    }
    let threads: Vec<_> = (0..2)
        .map(|n| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .withdraw_permission(
                        &format!("PRM-{n}"),
                        &meridian_config::Author::default(),
                        0,
                    )
                    .unwrap()
            })
        })
        .collect();
    let outcomes: Vec<Withdrawal> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(outcomes.contains(&Withdrawal::Withdrawn), "{outcomes:?}");
    assert!(outcomes.contains(&Withdrawal::LastAdmin), "{outcomes:?}");
    assert_eq!(store.snapshot().unwrap().records.permissions.len(), 1);
    assert_eq!(
        store
            .withdraw_permission("PRM-missing", &meridian_config::Author::default(), 0)
            .unwrap(),
        Withdrawal::Unknown
    );
}

// ── The plugin catalogue (W8) ───────────────────────────────────────────────

fn version(name: &str, version: &str) -> PluginVersion {
    PluginVersion {
        metadata: Some(PluginMetadata {
            name: name.into(),
            version: version.into(),
            roles: vec!["custody".into()],
            interface: true,
            sdk_version: "0.2.0".into(),
            // Kept with the version and read back whole (contract v11).
            declaration: Some(meridian_pb::v1::PluginDeclaration {
                secret_settings: vec!["consumer_key".into()],
                not_carried: vec![],
                storage: Some(meridian_pb::v1::StorageDeclaration {
                    retention_days: 2555,
                    ..Default::default()
                }),
                catalogue: None,
            }),
        }),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        uploaded_by: "local|ada".into(),
        uploaded_at_ns: 1,
    }
}

fn launch(instance: &str) -> PluginLaunch {
    PluginLaunch {
        instance_id: instance.into(),
        name: "snaptrade".into(),
        version: "0.1.0".into(),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        roles: vec!["custody".into()],
        launched_by: "local|ada".into(),
        launched_at_ns: 2,
        state: PluginLaunchState::Launched as i32,
        ..Default::default()
    }
}

#[test]
fn a_version_is_recorded_once_and_read_back_whole() {
    let store = store("versions");
    assert!(store
        .record_plugin_version(&version("snaptrade", "0.1.0"))
        .unwrap());
    let mut changed = version("snaptrade", "0.1.0");
    changed.image_digest = format!("sha256:{}", "b".repeat(64));
    assert!(
        !store.record_plugin_version(&changed).unwrap(),
        "the same name and version again is refused, not an update"
    );
    let catalogue = store.snapshot().unwrap().catalogue;
    assert_eq!(catalogue.versions, vec![version("snaptrade", "0.1.0")]);
}

#[test]
fn one_live_launch_per_instance_and_it_ends_once() {
    let store = store("launches");
    store
        .record_plugin_version(&version("snaptrade", "0.1.0"))
        .unwrap();
    assert!(store.begin_launch(&launch("snaptrade-1"), "").unwrap());
    assert!(
        !store.begin_launch(&launch("snaptrade-1"), "").unwrap(),
        "live already"
    );
    assert!(
        store.begin_launch(&launch("snaptrade-2"), "").unwrap(),
        "another instance"
    );

    let stop = Ending {
        state: PluginLaunchState::Stopped,
        by: "local|ada".into(),
        delegation: "DLG-9".into(),
        client: "meridian on ada-laptop".into(),
        note: "Retiring this connection.".into(),
        at_ns: 3,
        failure: String::new(),
    };
    let stopped = store
        .end_launch("snaptrade-1", &stop)
        .unwrap()
        .expect("was live");
    assert_eq!(stopped.state, PluginLaunchState::Stopped as i32);
    assert_eq!(stopped.stopped_at_ns, 3);
    // The stop's note is kept and read with the launch (contract v17).
    assert_eq!(stopped.stopped_note, "Retiring this connection.");
    let read = store.snapshot().unwrap().catalogue.launches;
    let ended = read
        .iter()
        .find(|l| l.instance_id == "snaptrade-1")
        .expect("recorded");
    assert_eq!(ended.stopped_note, "Retiring this connection.");
    assert!(
        store.end_launch("snaptrade-1", &stop).unwrap().is_none(),
        "none live now"
    );
    assert!(
        store.begin_launch(&launch("snaptrade-1"), "").unwrap(),
        "free again"
    );

    let launches = store.snapshot().unwrap().catalogue.launches;
    assert_eq!(launches.len(), 3);
    assert_eq!(launches[0], stop.applied_to(&launch("snaptrade-1")));
    // The stop names its delegation and client, and keeps its note
    // (contract v17).
    assert_eq!(launches[0].stopped_through_delegation, "DLG-9");
    assert_eq!(launches[0].stopped_client_name, "meridian on ada-laptop");
    let notes = store.launch_notes("snaptrade-1").unwrap();
    assert!(notes
        .iter()
        .any(|n| n.act == meridian_config::LaunchAct::Stopped
            && n.note == "Retiring this connection."
            && !n.gap));
}

/// Contract v17 (W8.3, W8.4): a launch names the delegation and client it
/// was made through and keeps its note; and every launch and stop recorded
/// before migration 15 gets one gap record each, at the moment it ran,
/// never an invented delegation.
#[test]
fn a_launch_keeps_its_stamp_and_note_and_those_before_v17_a_gap_record() {
    let (store, url) = store_at("launch_gaps");
    store
        .record_plugin_version(&version("snaptrade", "0.1.0"))
        .unwrap();
    let through = PluginLaunch {
        acting_through_delegation: "DLG-1".into(),
        client_name: "Claude".into(),
        ..launch("snaptrade-1")
    };
    assert!(store
        .begin_launch(&through, "Bringing SnapTrade in.")
        .unwrap());
    let read = store.snapshot().unwrap().catalogue.launches;
    assert_eq!(read[0].acting_through_delegation, "DLG-1");
    assert_eq!(read[0].client_name, "Claude");
    // The launch's note is read with it (contract v17).
    assert_eq!(read[0].note, "Bringing SnapTrade in.");
    let notes = store.launch_notes("snaptrade-1").unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].note, "Bringing SnapTrade in.");
    assert_eq!(notes[0].act, meridian_config::LaunchAct::Launched);

    // As a launch and a stop made before v17 stand: re-run migration 15's
    // gap records over a launch row naming no delegation, and see one gap
    // per act, at the migration's moment, and once only.
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .execute(
            "INSERT INTO config_plugin_launch (instance_id, name, version, image_digest, roles,
                     launched_by, launched_at_ns, state, stopped_by, stopped_at_ns)
             VALUES ('old-1', 'snaptrade', '0.1.0', $1, '{custody}', 'local|ada', 5, 2,
                     'local|ada', 6)",
            &[&format!("sha256:{}", "a".repeat(64))],
        )
        .unwrap();
    let gaps = |client: &mut postgres::Client| -> i64 {
        client
            .query_one(
                "SELECT count(*) FROM config_plugin_launch_gap WHERE instance_id = 'old-1'",
                &[],
            )
            .unwrap()
            .get(0)
    };
    assert_eq!(gaps(&mut client), 0);
    for at in [77, 78] {
        let mut tx = client.transaction().unwrap();
        meridian_config::migrations::launches_not_known(&mut tx, at).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        gaps(&mut client),
        2,
        "once per launch and act, however often run"
    );
    let notes = store.launch_notes("old-1").unwrap();
    let gapped: Vec<_> = notes.iter().filter(|n| n.gap).collect();
    assert_eq!(gapped.len(), 2);
    assert!(gapped
        .iter()
        .all(|n| n.at_ns == 77 && n.note == meridian_config::LAUNCH_THROUGH_NOT_KNOWN));
    let old = store
        .snapshot()
        .unwrap()
        .catalogue
        .launches
        .into_iter()
        .find(|l| l.instance_id == "old-1")
        .unwrap();
    assert!(old.acting_through_delegation.is_empty() && old.stopped_client_name.is_empty());
}

#[test]
fn two_launches_of_one_instance_at_once_admit_one() {
    let store = Arc::new(store("race"));
    store
        .record_plugin_version(&version("snaptrade", "0.1.0"))
        .unwrap();
    let racing: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || store.begin_launch(&launch("snaptrade-1"), "").unwrap())
        })
        .collect();
    let admitted = racing
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(admitted, 1);
}

#[test]
fn a_live_launch_is_read_back_as_live() {
    let store = store("live");
    assert!(store
        .record_plugin_version(&version("snaptrade", "0.1.0"))
        .unwrap());
    let live = PluginLaunch {
        live: true,
        ..launch("snaptrade-1")
    };
    assert!(store.begin_launch(&live, "").unwrap());
    assert!(store.begin_launch(&launch("snaptrade-2"), "").unwrap());
    let launches = store.snapshot().unwrap().catalogue.launches;
    let read = |instance: &str| {
        launches
            .iter()
            .find(|l| l.instance_id == instance)
            .unwrap()
            .live
    };
    assert!(read("snaptrade-1"));
    assert!(!read("snaptrade-2"));
}

/// Obviously not a real credential, and long enough to find in bytes.
const SECRET: &str = "sk-test-not-a-real-key-7f3a";

fn declaration(name: &str, kind: SettingType, secret: bool) -> SettingDeclaration {
    SettingDeclaration {
        name: name.into(),
        r#type: kind as i32,
        required: secret,
        secret,
        description: format!("what {name} is"),
        ..Default::default()
    }
}

#[test]
fn what_a_plugin_declared_is_replaced_whole_and_read_back_in_order() {
    let store = store("declared");
    store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: "snaptrade-1".into(),
            roles: vec!["custody".into()],
            last_reported_at_ns: 1,
        })
        .unwrap();
    // Every part the form is built from survives the store: the label, the
    // choices, the condition, the default and unit, and the developer's mark.
    let declared = vec![
        SettingDeclaration {
            label: "Key".into(),
            choices: vec![
                SettingChoice {
                    value: "personal".into(),
                    label: "Personal key".into(),
                    description: "Belongs to one user.".into(),
                },
                SettingChoice {
                    value: "commercial".into(),
                    label: "Commercial key".into(),
                    ..Default::default()
                },
            ],
            ..declaration("key_type", SettingType::Choice, true)
        },
        SettingDeclaration {
            applies_when: Some(SettingCondition {
                setting: "key_type".into(),
                one_of: vec!["commercial".into()],
            }),
            ..declaration("api_key", SettingType::String, true)
        },
        SettingDeclaration {
            default_value: "15".into(),
            unit: "minutes".into(),
            ..declaration("poll_minutes", SettingType::Integer, false)
        },
        SettingDeclaration {
            developer: true,
            ..declaration("synthetic", SettingType::Boolean, false)
        },
    ];
    store
        .record_declared_settings("snaptrade-1", &declared, 1)
        .unwrap();
    assert_eq!(
        store.snapshot().unwrap().declared_settings["snaptrade-1"],
        declared
    );
    store
        .record_declared_settings("snaptrade-1", &declared[1..], 2)
        .unwrap();
    assert_eq!(
        store.snapshot().unwrap().declared_settings["snaptrade-1"],
        declared[1..]
    );
}

#[test]
fn a_secret_is_at_rest_only_sealed_and_a_change_is_recorded_without_a_secrets_value() {
    let (store, url) = store_at("settings");
    let key = SettingsKey::holding(&[7u8; 32]);
    let sealed = key.seal("snaptrade-1", "api_key", SECRET).unwrap();
    store
        .put_plugin_settings(
            "snaptrade-1",
            &[
                SettingChange {
                    name: "api_key".into(),
                    held: Some(Held::Sealed(sealed.clone())),
                },
                SettingChange {
                    name: "poll_minutes".into(),
                    held: Some(Held::Plain("15".into())),
                },
            ],
            &author("local|ada"),
            5,
        )
        .unwrap();

    let snapshot = store.snapshot().unwrap();
    let held: Vec<(&str, &Held, &str, i64)> = snapshot
        .settings
        .iter()
        .map(|s| (s.name.as_str(), &s.held, s.set_by.as_str(), s.set_at_ns))
        .collect();
    assert_eq!(
        held,
        [
            ("api_key", &Held::Sealed(sealed.clone()), "local|ada", 5),
            ("poll_minutes", &Held::Plain("15".into()), "local|ada", 5),
        ]
    );
    assert_eq!(
        key.open("snaptrade-1", "api_key", &sealed).unwrap(),
        SECRET,
        "and opens with the key"
    );

    // What a dump of the database holds: every row of both tables as text.
    let mut dump = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let text: String = dump
        .query_one(
            "SELECT coalesce(string_agg(s::text, '|'), '')
               FROM (SELECT * FROM config_plugin_setting) s",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(
        text.contains("poll_minutes"),
        "the dump read the table: {text}"
    );
    assert!(!text.contains(SECRET), "no secret at rest");
    let hex: String = SECRET.bytes().map(|b| format!("{b:02x}")).collect();
    assert!(!text.contains(&hex), "not even as bytea");
    let value: Option<String> = dump
        .query_one(
            "SELECT value FROM config_plugin_setting WHERE name = 'api_key'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(value, None, "a secret is never written to the value column");

    store
        .put_plugin_settings(
            "snaptrade-1",
            &[SettingChange {
                name: "api_key".into(),
                held: None,
            }],
            &SettingsAuthor {
                by: "local|grace".into(),
                delegation: "DLG-7".into(),
                client: "Claude".into(),
                note: "The key leaked; cleared it.".into(),
            },
            9,
        )
        .unwrap();
    let names: Vec<String> = store
        .snapshot()
        .unwrap()
        .settings
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["poll_minutes"], "cleared");

    // Each change its own record: what, to what (never a secret's value),
    // who, through which delegation, and when.
    let changes: Vec<_> = store
        .plugin_setting_changes("snaptrade-1")
        .unwrap()
        .into_iter()
        .map(|c| {
            (
                c.name,
                c.kind,
                c.value,
                c.secret,
                c.by,
                c.delegation,
                c.at_ns,
            )
        })
        .collect();
    let expected = |name: &str,
                    kind: ChangeKind,
                    value: Option<&str>,
                    secret: bool,
                    by: &str,
                    delegation: &str,
                    at: i64| {
        (
            name.to_string(),
            kind,
            value.map(str::to_string),
            secret,
            by.to_string(),
            delegation.to_string(),
            at,
        )
    };
    assert_eq!(
        changes,
        [
            expected("api_key", ChangeKind::Set, None, true, "local|ada", "", 5),
            expected(
                "poll_minutes",
                ChangeKind::Set,
                Some("15"),
                false,
                "local|ada",
                "",
                5
            ),
            expected(
                "api_key",
                ChangeKind::Cleared,
                None,
                false,
                "local|grace",
                "DLG-7",
                9
            ),
        ]
    );
    let text: String = dump
        .query_one(
            "SELECT coalesce(string_agg(c::text, '|'), '')
               FROM (SELECT * FROM config_plugin_setting_change) c",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(!text.contains(SECRET), "no secret in the change records");
    // The table refuses a secret's value, whoever writes it.
    let secret_value = dump.execute(
        "INSERT INTO config_plugin_setting_change
                (plugin_instance_id, name, action, changed_by, changed_at_ns, value, secret)
         VALUES ('snaptrade-1', 'api_key', 1, 'someone', 1, 'x', true)",
        &[],
    );
    assert!(secret_value.is_err());

    // And the table itself refuses a row that is neither, or both.
    let both = dump.execute(
        "INSERT INTO config_plugin_setting (plugin_instance_id, name, value, sealed, set_by, set_at_ns)
         VALUES ('snaptrade-1', 'x', 'a', '\\x00', 'someone', 1)",
        &[],
    );
    assert!(both.is_err());
    let neither = dump.execute(
        "INSERT INTO config_plugin_setting (plugin_instance_id, name, set_by, set_at_ns)
         VALUES ('snaptrade-1', 'y', 'someone', 1)",
        &[],
    );
    assert!(neither.is_err());
}

#[test]
fn a_database_behind_this_binary_is_waited_for_and_one_ahead_is_refused() {
    // A starting conductor waits out a schema the migration Job has not
    // reached, and refuses one a newer release migrated: waiting never fixes
    // that, and this binary's queries may already be wrong against it. The
    // two have to be told apart by what the store returns, not by its words.
    let (store, url) = store_at("ahead");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connects");

    client
        .execute(
            "DELETE FROM config_schema_migration WHERE version = $1",
            &[&meridian_config::migrations::latest()],
        )
        .expect("could not pretend to be behind");
    let behind = store.verify().expect_err("a schema behind must not verify");
    assert!(
        matches!(behind, meridian_config::store::StoreError::Unavailable(_)),
        "a schema behind this binary is one the migration reaches: {behind:?}"
    );

    client
        .execute(
            "INSERT INTO config_schema_migration (version, name, applied_at_ns)
             VALUES (999, 'later', 1), ($1, 'restored', 1)",
            &[&meridian_config::migrations::latest()],
        )
        .expect("could not pretend to be ahead");
    let ahead = store.verify().expect_err("a newer schema must not verify");
    let said = ahead.to_string();
    assert!(
        said.contains("999") && said.contains(&meridian_config::migrations::latest().to_string()),
        "the refusal has to name both versions: {said}"
    );
    assert!(
        matches!(ahead, meridian_config::store::StoreError::SchemaAhead(_)),
        "a schema ahead of this binary is not one waiting fixes: {ahead:?}"
    );
}

#[test]
fn access_entries_naming_tags_become_one_per_plugin_at_the_highest_level() {
    // decisions/026: access to a plugin is read or write, and a plugin
    // declares no tags. A deployment migrated before it holds entries naming
    // one plugin through several tags; the migration leaves one entry per
    // plugin, at the highest level any of its tags had, in the order each
    // plugin first appeared. `read` on one tag and `write` on another is
    // `write`.
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("config_tagged_{nanos}_{seq}");
    postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .expect("could not create a schema");
    let url = format!("{}?options=-c%20search_path%3D{name}", base_url());
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connects");

    // The store as the release before this one left it: every migration up
    // to the one that retires tags, recorded as that release recorded them.
    let before: Vec<_> = meridian_config::migrations::MIGRATIONS
        .iter()
        .take_while(|m| m.name != "access_is_read_or_write")
        .collect();
    assert!(
        before.len() < meridian_config::migrations::MIGRATIONS.len(),
        "the migration that retires tags"
    );
    client
        .batch_execute(meridian_config::migrations::HISTORY)
        .unwrap();
    for migration in before {
        let mut tx = client.transaction().unwrap();
        tx.batch_execute(migration.sql).unwrap();
        meridian_config::migrations::record(&mut tx, migration, 1).unwrap();
        tx.commit().unwrap();
    }
    client
        .batch_execute(
            "INSERT INTO config_access_group (access_group_id, name) VALUES
                 ('AX-TRADING', 'Trading'), ('AX-VIEWING', 'Viewing');
             INSERT INTO config_access_entry
                    (access_group_id, position, plugin_instance_id, tag, level) VALUES
                 ('AX-TRADING', 0, 'snaptrade-1', 'holdings',  1),
                 ('AX-TRADING', 1, 'oms-1',       'oms',       1),
                 ('AX-TRADING', 2, 'snaptrade-1', 'custody',   2),
                 ('AX-TRADING', 3, 'oms-1',       'reporting', 1),
                 ('AX-VIEWING', 0, 'snaptrade-1', 'custody',   1),
                 ('AX-VIEWING', 1, 'snaptrade-1', 'holdings',  1);
             INSERT INTO config_known_plugin (plugin_instance_id, roles, tags, last_reported_at_ns)
                 VALUES ('snaptrade-1', '{custody}', '{holdings}', 1);",
        )
        .unwrap();

    let store = PostgresStore::connect(&url, 2).expect("connects");
    assert!(store.verify().is_err(), "migrations behind");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migrates");
    store.verify().expect("recognised after migrating");

    let snapshot = store.snapshot().unwrap();
    let entries = |id: &str| {
        snapshot
            .records
            .access_groups
            .iter()
            .find(|g| g.access_group_id == id)
            .unwrap()
            .entries
            .clone()
    };
    // And, migrated on to contract v15, each entry names its plugin's one
    // role where the deployment knows it: SnapTrade reported custody, and
    // nothing is known of oms-1, which is left naming none.
    let entry = |plugin: &str, role: &str, level: AccessLevel| AccessEntry {
        plugin_instance_id: plugin.into(),
        level: level as i32,
        role: role.into(),
    };
    assert_eq!(
        entries("AX-TRADING"),
        [
            entry("snaptrade-1", "custody", AccessLevel::Write),
            entry("oms-1", "", AccessLevel::Read)
        ],
        "read on holdings and write on custody is write on the plugin"
    );
    assert_eq!(
        entries("AX-VIEWING"),
        [entry("snaptrade-1", "custody", AccessLevel::Read)],
        "read on both tags stays read"
    );
    assert_eq!(snapshot.plugins[0].roles, ["custody"]);

    let tag_columns: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_schema = current_schema() AND column_name IN ('tag', 'tags')",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(tag_columns, 0, "no table keeps a tag");
}

#[test]
fn an_accounts_custodian_type_owner_and_note_are_written_edited_and_cleared() {
    // W6.3: free text, optional, and set whole by each edit, so an empty one
    // clears it; an empty one is held as NULL, as every account's was before
    // the four existed.
    let (store, url) = store_at("attributes");
    let described = AccountRecord {
        account_id: "ACC-1".into(),
        name: "Growth".into(),
        state: AccountState::Open as i32,
        created_at_ns: 7,
        custodian: "Fidelity".into(),
        account_type: "Roth IRA".into(),
        owner: "Fund I".into(),
        note: "n".repeat(2_000),
    };
    store.put_account(&described).unwrap();
    assert_eq!(
        store.snapshot().unwrap().records.accounts,
        std::slice::from_ref(&described),
        "all four, and a note of 2,000 characters, kept whole"
    );

    let edited = AccountRecord {
        owner: "Fund II".into(),
        note: "Moved to Fund II.".into(),
        ..described.clone()
    };
    store.put_account(&edited).unwrap();
    assert_eq!(
        store.snapshot().unwrap().records.accounts,
        std::slice::from_ref(&edited)
    );

    let cleared = AccountRecord {
        custodian: String::new(),
        account_type: String::new(),
        owner: String::new(),
        note: String::new(),
        ..edited
    };
    store.put_account(&cleared).unwrap();
    assert_eq!(
        store.snapshot().unwrap().records.accounts,
        std::slice::from_ref(&cleared)
    );
    let mut dump = postgres::Client::connect(&url, postgres::NoTls).expect("connects");
    let nulls: i64 = dump
        .query_one(
            "SELECT count(*) FROM config_account
              WHERE custodian IS NULL AND account_type IS NULL AND owner IS NULL AND note IS NULL",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(nulls, 1, "a cleared one is NULL, not ''");

    // The column's own bound, for whatever writes past the conductor's rules.
    let too_long = dump.execute(
        "UPDATE config_account SET note = repeat('n', 2001) WHERE account_id = 'ACC-1'",
        &[],
    );
    assert!(too_long.is_err(), "a note past 2,000 characters is refused");
}

#[test]
fn an_account_from_before_its_attributes_reads_back_with_none() {
    // Migration 8 adds the columns nullable and with no default, so an
    // account written by the release before reads as it did.
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("config_before_attributes_{nanos}_{seq}");
    postgres::Client::connect(&base_url(), postgres::NoTls)
        .expect("could not reach the test database")
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .expect("could not create a schema");
    let url = format!("{}?options=-c%20search_path%3D{name}", base_url());
    let mut client = postgres::Client::connect(&url, postgres::NoTls).expect("connects");

    let before: Vec<_> = meridian_config::migrations::MIGRATIONS
        .iter()
        .take_while(|m| m.name != "account_attributes")
        .collect();
    assert!(
        before.len() < meridian_config::migrations::MIGRATIONS.len(),
        "the migration that adds them"
    );
    client
        .batch_execute(meridian_config::migrations::HISTORY)
        .unwrap();
    for migration in before {
        let mut tx = client.transaction().unwrap();
        tx.batch_execute(migration.sql).unwrap();
        meridian_config::migrations::record(&mut tx, migration, 1).unwrap();
        tx.commit().unwrap();
    }
    client
        .batch_execute(
            "INSERT INTO config_account (account_id, name, state, created_at_ns)
                 VALUES ('ACC-OLD', 'Old income', 1, 5);",
        )
        .unwrap();

    let store = PostgresStore::connect(&url, 2).expect("connects");
    assert!(store.verify().is_err(), "one migration behind");
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migrates");
    store.verify().expect("recognised after migrating");

    assert_eq!(
        store.snapshot().unwrap().records.accounts,
        [AccountRecord {
            account_id: "ACC-OLD".into(),
            name: "Old income".into(),
            state: AccountState::Open as i32,
            created_at_ns: 5,
            ..AccountRecord::default()
        }]
    );
}

/// sdk-contract/a-plugin-has-admins: a deployment set up before admin was a
/// level had its deployment admins administer every plugin by being one.
/// Upgrading links their user groups to All plugins (admin), as first run
/// and a claim code now do, so nothing is taken away; and a permission to
/// either built-in access group still names no account group.
#[test]
fn upgrading_links_the_deployment_admins_to_all_plugins_admin() {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("config_upgrade_{nanos}_{seq}");
    let mut admin = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    admin
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .unwrap();
    let url = format!("{}?options=-c%20search_path%3D{name}", base_url());

    // The release before: migrations 1 to 8, and a deployment admin.
    let mut before = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    before
        .batch_execute(meridian_config::migrations::HISTORY)
        .unwrap();
    for migration in meridian_config::migrations::MIGRATIONS
        .iter()
        .filter(|m| m.version <= 8)
    {
        let mut tx = before.transaction().unwrap();
        tx.batch_execute(migration.sql).unwrap();
        meridian_config::migrations::record(&mut tx, migration, 1).unwrap();
        tx.commit().unwrap();
    }
    before
        .batch_execute(
            "INSERT INTO config_user_group (user_group_id, name, logins) VALUES ('UG-1', 'Admins', '{ada}');
             INSERT INTO config_permission (permission_id, user_group_id, account_group_id, access_group_id)
             VALUES ('PRM-1', 'UG-1', NULL, 'deployment-admin');",
        )
        .unwrap();

    let store = PostgresStore::connect(&url, 2).unwrap();
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("migrates to this release");
    let records = store.snapshot().unwrap().records;
    let linked: Vec<(&str, &str)> = records
        .permissions
        .iter()
        .map(|p| (p.user_group_id.as_str(), p.access_group_id.as_str()))
        .collect();
    assert!(
        linked.contains(&("UG-1", meridian_access::ALL_PLUGINS_ADMIN)),
        "{linked:?}"
    );
    let ada = meridian_access::person_access(&records, "ada", &[]);
    assert!(ada.deployment_admin && ada.administers("any-plugin"));

    // Its level column takes admin now, and either built-in access group
    // still names no account group, whoever writes it.
    store
        .put_account_group(&AccountGroup {
            account_group_id: "AG-1".into(),
            name: "g".into(),
            account_ids: vec![],
            built_in: false,
        })
        .unwrap();
    let naming = Permission {
        permission_id: "PRM-2".into(),
        user_group_id: "UG-1".into(),
        account_group_id: "AG-1".into(),
        access_group_id: meridian_access::ALL_PLUGINS_ADMIN.into(),
    };
    assert!(store
        .add_permission(&naming, &meridian_config::Author::default(), 0)
        .is_err());
    store
        .put_access_group(
            &AccessGroup {
                access_group_id: "AX-ADMIN".into(),
                name: "Admins of oms".into(),
                entries: vec![AccessEntry {
                    plugin_instance_id: "oms-1".into(),
                    level: AccessLevel::Admin as i32,
                    role: String::new(),
                }],
                built_in: false,
            },
            &meridian_config::Author::default(),
            0,
        )
        .expect("an admin entry is kept");
    let held = store.snapshot().unwrap().records.access_groups;
    assert!(held.iter().any(
        |g| g.access_group_id == "AX-ADMIN" && g.entries[0].level == AccessLevel::Admin as i32
    ));
}

fn author(by: &str) -> SettingsAuthor {
    SettingsAuthor {
        by: by.into(),
        ..Default::default()
    }
}

struct At(i64);
impl meridian_clock::Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

#[test]
fn changes_from_before_each_was_its_own_record_are_backfilled_where_known_and_a_gap_said() {
    // A store at migration 10, as a deployment before contract v14 has it:
    // two changes to poll_minutes and a secret set, recorded without values.
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let name = format!("config_backfill_{seq}_{}", unique());
    let mut admin = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    admin
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .unwrap();
    let url = format!("{}?options=-c%20search_path%3D{name}", base_url());
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.batch_execute(meridian_config::migrations::HISTORY)
        .unwrap();
    for migration in meridian_config::migrations::MIGRATIONS
        .iter()
        .filter(|m| m.version <= 10)
    {
        db.batch_execute(migration.sql).unwrap();
        db.execute(
            "INSERT INTO config_schema_migration (version, name, applied_at_ns) VALUES ($1, $2, 1)",
            &[&migration.version, &migration.name],
        )
        .unwrap();
    }
    db.batch_execute(
        "INSERT INTO config_plugin_setting (plugin_instance_id, name, value, sealed, set_by, set_at_ns)
         VALUES ('snaptrade-1', 'poll_minutes', '30', NULL, 'local|grace', 20),
                ('snaptrade-1', 'api_key', NULL, '\\x0102', 'local|ada', 10);
         INSERT INTO config_plugin_setting_change (plugin_instance_id, name, action, changed_by, changed_at_ns)
         VALUES ('snaptrade-1', 'api_key', 1, 'local|ada', 10),
                ('snaptrade-1', 'poll_minutes', 1, 'local|ada', 10),
                ('snaptrade-1', 'poll_minutes', 1, 'local|grace', 20);",
    )
    .unwrap();

    let store = PostgresStore::connect(&url, 2).unwrap();
    store.migrate(&At(1_000)).unwrap();
    store
        .migrate(&At(2_000))
        .expect("migrating twice is a no-op");

    let changes = store.plugin_setting_changes("snaptrade-1").unwrap();
    let seen: Vec<_> = changes
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.kind,
                c.value.as_deref(),
                c.secret,
                c.by.as_str(),
                c.at_ns,
                c.backfilled,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            // The secret's latest: known secret, its value never.
            (
                "api_key",
                ChangeKind::Set,
                None,
                true,
                "local|ada",
                10,
                true
            ),
            // An earlier change: its value is not known, so it stays unknown.
            (
                "poll_minutes",
                ChangeKind::Set,
                None,
                false,
                "local|ada",
                10,
                false
            ),
            // The latest, the one the setting says: its value as held.
            (
                "poll_minutes",
                ChangeKind::Set,
                Some("30"),
                false,
                "local|grace",
                20,
                true
            ),
            // One gap per setting, at the migration's own time, once.
            (
                "api_key",
                ChangeKind::NotKnownBefore,
                None,
                false,
                "",
                1_000,
                false
            ),
            (
                "poll_minutes",
                ChangeKind::NotKnownBefore,
                None,
                false,
                "",
                1_000,
                false
            ),
        ]
    );
    assert!(
        changes[2].note.contains("backfilled by migration 11"),
        "{}",
        changes[2].note
    );
    assert!(
        changes[3].note.starts_with("not known before"),
        "{}",
        changes[3].note
    );

    // A change since records it whole, and the gap is not said again.
    store
        .put_plugin_settings(
            "snaptrade-1",
            &[SettingChange {
                name: "poll_minutes".into(),
                held: Some(Held::Plain("45".into())),
            }],
            &author("local|ada"),
            3_000,
        )
        .unwrap();
    let after = store.plugin_setting_changes("snaptrade-1").unwrap();
    assert_eq!(after.len(), 6);
    assert_eq!(after[5].value.as_deref(), Some("45"));
}

#[test]
fn a_redeclared_settings_value_is_cleared_and_a_secrets_earlier_values_redacted() {
    let (store, url) = store_at("redeclared");
    store
        .record_plugin(&KnownPlugin {
            plugin_instance_id: "snaptrade-1".into(),
            roles: vec!["custody".into()],
            last_reported_at_ns: 1,
        })
        .unwrap();
    let declared = vec![
        declaration("note", SettingType::String, false),
        declaration("poll_minutes", SettingType::Integer, false),
        declaration("synthetic", SettingType::Boolean, false),
    ];
    assert!(store
        .record_declared_settings("snaptrade-1", &declared, 1)
        .unwrap()
        .is_empty());
    let plain = |name: &str, value: &str| SettingChange {
        name: name.into(),
        held: Some(Held::Plain(value.into())),
    };
    store
        .put_plugin_settings(
            "snaptrade-1",
            &[
                plain("note", "[]"),
                plain("poll_minutes", "15"),
                plain("synthetic", "true"),
            ],
            &author("local|ada"),
            5,
        )
        .unwrap();
    store
        .put_plugin_settings(
            "snaptrade-1",
            &[plain("poll_minutes", "20")],
            &author("local|ben"),
            6,
        )
        .unwrap();

    // Declared the same again: nothing is cleared or redacted.
    assert!(store
        .record_declared_settings("snaptrade-1", &declared, 7)
        .unwrap()
        .is_empty());

    // note becomes a table, poll_minutes a secret; synthetic stays.
    let mut table = declaration("note", SettingType::Table, false);
    table.columns = vec![meridian_pb::v1::SettingColumn {
        name: "code".into(),
        r#type: meridian_pb::v1::SettingColumnType::Text as i32,
        ..Default::default()
    }];
    let again = vec![
        table,
        declaration("poll_minutes", SettingType::Integer, true),
        declaration("synthetic", SettingType::Boolean, false),
    ];
    let cleared = store
        .record_declared_settings("snaptrade-1", &again, 8)
        .unwrap();
    assert_eq!(cleared, ["note", "poll_minutes"]);
    let snapshot = store.snapshot().unwrap();
    let held: Vec<&str> = snapshot.settings.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(held, ["synthetic"]);
    // The latest change is the re-declaration's clear: no person, at 8.
    assert_eq!(
        snapshot.settings_changed["snaptrade-1"],
        meridian_config::LastChange {
            by: String::new(),
            at_ns: 8
        }
    );

    let changes = store.plugin_setting_changes("snaptrade-1").unwrap();
    let seen: Vec<_> = changes
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.kind,
                c.value.as_deref(),
                c.by.as_str(),
                c.at_ns,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("note", ChangeKind::Set, Some("[]"), "local|ada", 5),
            // Redacted: the value blanked, who and when kept.
            ("poll_minutes", ChangeKind::Set, None, "local|ada", 5),
            ("synthetic", ChangeKind::Set, Some("true"), "local|ada", 5),
            ("poll_minutes", ChangeKind::Set, None, "local|ben", 6),
            ("note", ChangeKind::Cleared, None, "", 8),
            ("poll_minutes", ChangeKind::Cleared, None, "", 8),
            ("poll_minutes", ChangeKind::Redacted, None, "", 8),
        ]
    );
    assert!(
        changes[4].note.contains("re-declaration"),
        "{}",
        changes[4].note
    );
    assert!(changes[4].note.contains("table"), "{}", changes[4].note);
    assert!(changes[6].secret);
    assert!(
        changes[6].note.contains("became secret"),
        "{}",
        changes[6].note
    );
    // The records it redacted, by id: the two that held a value.
    let mut dump = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let ids: Vec<i64> = dump
        .query(
            "SELECT change_id FROM config_plugin_setting_change
              WHERE name = 'poll_minutes' AND action = 1 ORDER BY change_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert!(
        changes[6]
            .note
            .contains(&format!("records {}, {}", ids[0], ids[1])),
        "{}",
        changes[6].note
    );

    // A secret set later redacts nothing more, and is recorded as set.
    let key = SettingsKey::holding(&[7u8; 32]);
    store
        .put_plugin_settings(
            "snaptrade-1",
            &[SettingChange {
                name: "poll_minutes".into(),
                held: Some(Held::Sealed(
                    key.seal("snaptrade-1", "poll_minutes", "25").unwrap(),
                )),
            }],
            &author("local|ada"),
            9,
        )
        .unwrap();
    assert_eq!(
        store.plugin_setting_changes("snaptrade-1").unwrap().len(),
        8
    );
}

#[test]
fn migration_12_redacts_what_a_secret_settings_records_hold() {
    // A store at migration 11 whose api_key became secret before redaction
    // was ruled: its records, a backfilled one among them, hold its values.
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let name = format!("config_redact_{seq}_{}", unique());
    let mut admin = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    admin
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .unwrap();
    let url = format!("{}?options=-c%20search_path%3D{name}", base_url());
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.batch_execute(meridian_config::migrations::HISTORY)
        .unwrap();
    for migration in meridian_config::migrations::MIGRATIONS
        .iter()
        .filter(|m| m.version <= 11)
    {
        db.batch_execute(migration.sql).unwrap();
        db.execute(
            "INSERT INTO config_schema_migration (version, name, applied_at_ns) VALUES ($1, $2, 1)",
            &[&migration.version, &migration.name],
        )
        .unwrap();
    }
    db.batch_execute(
        "INSERT INTO config_known_plugin (plugin_instance_id, roles, last_reported_at_ns)
         VALUES ('snaptrade-1', '{custody}', 1);
         INSERT INTO config_plugin_setting_declaration
                (plugin_instance_id, position, name, type, required, secret)
         VALUES ('snaptrade-1', 0, 'api_key', 1, true, true),
                ('snaptrade-1', 1, 'poll_minutes', 2, false, false);
         INSERT INTO config_plugin_setting (plugin_instance_id, name, value, sealed, set_by, set_at_ns)
         VALUES ('snaptrade-1', 'api_key', 'plain-before', NULL, 'local|ada', 10),
                ('snaptrade-1', 'poll_minutes', '30', NULL, 'local|grace', 20);
         INSERT INTO config_plugin_setting_change
                (plugin_instance_id, name, action, changed_by, changed_at_ns, value, backfilled)
         VALUES ('snaptrade-1', 'api_key', 1, 'local|ada', 10, 'plain-before', true),
                ('snaptrade-1', 'poll_minutes', 1, 'local|grace', 20, '30', true);",
    )
    .unwrap();

    let store = PostgresStore::connect(&url, 2).unwrap();
    store.migrate(&At(1_000)).unwrap();
    store
        .migrate(&At(2_000))
        .expect("migrating twice is a no-op");

    let changes = store.plugin_setting_changes("snaptrade-1").unwrap();
    let seen: Vec<_> = changes
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.kind,
                c.value.as_deref(),
                c.by.as_str(),
                c.at_ns,
                c.backfilled,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("api_key", ChangeKind::Set, None, "local|ada", 10, true),
            (
                "poll_minutes",
                ChangeKind::Set,
                Some("30"),
                "local|grace",
                20,
                true
            ),
            ("api_key", ChangeKind::Redacted, None, "", 1_000, false),
        ]
    );
    assert!(
        changes[2].note.contains("migration 12"),
        "{}",
        changes[2].note
    );
    let text: String = db
        .query_one(
            "SELECT coalesce(string_agg(c::text, '|'), '')
               FROM (SELECT * FROM config_plugin_setting_change) c",
            &[],
        )
        .unwrap()
        .get(0);
    assert!(!text.contains("plain-before"), "redacted: {text}");
}

#[test]
fn upgrading_to_v15_rewrites_each_entry_to_name_its_plugins_one_role_and_records_it() {
    // A store at migration 12, as a deployment at contract v14 has it: entries
    // naming plugins and levels alone, on a custody plugin, an operations
    // plugin, a plugin holding no role, a plugin holding two, and one no
    // sidecar has reported but a launch names.
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let name = format!("config_per_role_{seq}_{}", unique());
    let mut admin = postgres::Client::connect(&base_url(), postgres::NoTls).unwrap();
    admin
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .unwrap();
    let url = format!("{}?options=-c%20search_path%3D{name}", base_url());
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.batch_execute(meridian_config::migrations::HISTORY)
        .unwrap();
    for migration in meridian_config::migrations::MIGRATIONS
        .iter()
        .filter(|m| m.version <= 12)
    {
        db.batch_execute(migration.sql).unwrap();
        db.execute(
            "INSERT INTO config_schema_migration (version, name, applied_at_ns) VALUES ($1, $2, 1)",
            &[&migration.version, &migration.name],
        )
        .unwrap();
    }
    db.batch_execute(
        "INSERT INTO config_known_plugin (plugin_instance_id, roles, last_reported_at_ns)
         VALUES ('snaptrade-1', '{custody}', 1), ('ops-1', '{operations}', 1),
                ('tool-1', '{}', 1), ('multi-1', '{custody,operations}', 1);
         INSERT INTO config_plugin_version (name, version, roles, interface, sdk_version,
                image_digest, uploaded_by, uploaded_at_ns)
         VALUES ('later', '1.0.0', '{oms}', false, '0.19.0', 'sha256:00', 'local|ada', 1);
         INSERT INTO config_plugin_launch (instance_id, name, version, image_digest, roles,
                launched_by, launched_at_ns, state)
         VALUES ('later-1', 'later', '1.0.0', 'sha256:00', '{oms}', 'local|ada', 1, 1);
         INSERT INTO config_account (account_id, name, state, created_at_ns)
         VALUES ('ACC-1', 'Growth', 1, 1), ('ACC-2', 'Income', 1, 1);
         INSERT INTO config_user_group (user_group_id, name, logins)
         VALUES ('UG-1', 'Traders', '{local|tam}');
         INSERT INTO config_account_group (account_group_id, name, account_ids)
         VALUES ('AG-1', 'Growth', '{ACC-1}');
         INSERT INTO config_access_group (access_group_id, name) VALUES ('AX-1', 'Trading'),
                ('AX-2', 'Nothing to rewrite');
         INSERT INTO config_access_entry (access_group_id, position, plugin_instance_id, level)
         VALUES ('AX-1', 0, 'snaptrade-1', 1), ('AX-1', 1, 'snaptrade-1', 3),
                ('AX-1', 2, 'ops-1', 2), ('AX-1', 3, 'tool-1', 2),
                ('AX-1', 4, 'multi-1', 1), ('AX-1', 5, 'later-1', 2),
                ('AX-2', 0, 'tool-1', 1);
         INSERT INTO config_permission (permission_id, user_group_id, account_group_id, access_group_id)
         VALUES ('P-1', 'UG-1', 'AG-1', 'AX-1');
         INSERT INTO config_sign_in (subject, display_name, signed_in_at_ns)
         VALUES ('local|tam', 'Tam', 1);",
    )
    .unwrap();

    let store = PostgresStore::connect(&url, 2).unwrap();
    store.migrate(&At(5_000)).unwrap();
    store
        .migrate(&At(6_000))
        .expect("migrating twice is a no-op");

    let snapshot = store.snapshot().unwrap();
    let trading = snapshot
        .records
        .access_groups
        .iter()
        .find(|group| group.access_group_id == "AX-1")
        .unwrap();
    let named: Vec<(&str, &str)> = trading
        .entries
        .iter()
        .map(|entry| (entry.plugin_instance_id.as_str(), entry.role.as_str()))
        .collect();
    assert_eq!(
        named,
        [
            ("snaptrade-1", "custody"),
            ("snaptrade-1", "custody"),
            ("ops-1", "operations"),
            ("tool-1", ""),
            ("multi-1", ""),
            ("later-1", "oms"),
        ],
        "each plugin's one role; none for a role-less plugin; a two-role plugin's left"
    );

    // Each group has its gap; the one rewritten its record, by no person.
    let trading_changes = store.access_changes("AX-1").unwrap();
    let kinds: Vec<_> = trading_changes.iter().map(|change| change.kind).collect();
    use meridian_config::AccessChangeKind::{NotKnownBefore, Rewritten};
    assert_eq!(kinds, [NotKnownBefore, Rewritten]);
    assert!(trading_changes
        .iter()
        .all(|change| change.at_ns == 5_000 && change.by.is_empty()));
    assert!(trading_changes[1].was.contains("snaptrade-1 read"));
    assert!(trading_changes[1]
        .became
        .contains("snaptrade-1 custody read"));
    assert!(trading_changes[1].note.contains("contract v15"));
    assert_eq!(
        store
            .access_changes("AX-2")
            .unwrap()
            .iter()
            .map(|change| change.kind)
            .collect::<Vec<_>>(),
        [NotKnownBefore],
        "nothing to rewrite, so only its gap"
    );
    for built_in in [DEPLOYMENT_ADMIN, meridian_access::ALL_PLUGINS_ADMIN] {
        assert_eq!(store.access_changes(built_in).unwrap().len(), 1);
    }

    // Run again, it changes nothing.
    let mut tx = db.transaction().unwrap();
    let again =
        meridian_config::migrations::rewrite_entries_to_name_their_role(&mut tx, 7_000).unwrap();
    tx.commit().unwrap();
    assert!(again.is_empty(), "idempotent: {again:?}");
    assert_eq!(store.access_changes("AX-1").unwrap().len(), 2);

    // Nobody's access moved: the access table and every person's level and
    // accounts are what reading the entries per plugin, as v14 did, gives.
    let mut as_v14 = snapshot.records.clone();
    as_v14.known_plugins.clear();
    for group in &mut as_v14.access_groups {
        for entry in &mut group.entries {
            entry.role.clear();
        }
    }
    for plugin in ["snaptrade-1", "ops-1", "tool-1", "later-1"] {
        assert_eq!(
            meridian_access::plugin_access_table(&snapshot.records, plugin)
                .people
                .iter()
                .map(|person| (
                    person.read_account_ids.clone(),
                    person.write_account_ids.clone()
                ))
                .collect::<Vec<_>>(),
            meridian_access::plugin_access_table(&as_v14, plugin)
                .people
                .iter()
                .map(|person| (
                    person.read_account_ids.clone(),
                    person.write_account_ids.clone()
                ))
                .collect::<Vec<_>>(),
            "{plugin}'s table"
        );
        let now = meridian_access::person_access(&snapshot.records, "local|tam", &[]).held(plugin);
        let then = meridian_access::person_access(&as_v14, "local|tam", &[]).held(plugin);
        assert_eq!(now, then, "Tam on {plugin}");
    }
    // Only the two-role plugin's entry, which the rewrite could not name,
    // holds nothing now, and is flagged.
    assert!(!meridian_access::entry_holds(
        &snapshot.records,
        "multi-1",
        ""
    ));
}

/// Contract v16 (W6.25, W8.7, W4.13): each hold, archive and move its own
/// record, what stands each one's latest, a retry recorded once, and the
/// archive's spans each unit's latest move, as the in-memory store has them.
#[test]
fn holds_archives_and_moves_are_each_their_own_record_and_read_back() {
    use meridian_domain::v1::{Hold, MoveRecord, PluginArchive};
    use meridian_pb::v1::{MoveOutcome, RecordMoveRequest};

    let (store, url) = store_at("archive");
    let hold = |role: &str, days: u32, at: i64| Hold {
        role: role.into(),
        days,
        write_once: false,
        updated_by: "ada@example.com".into(),
        updated_at_ns: at,
        acting_through_delegation: String::new(),
        client_name: String::new(),
        note: String::new(),
    };
    store.set_hold(&hold("custody", 2190, 1), "").unwrap();
    let through = Hold {
        acting_through_delegation: "DLG-1".into(),
        client_name: "Claude".into(),
        note: "The records rule.".into(),
        ..hold("", 400, 2)
    };
    store.set_hold(&through, "The records rule.").unwrap();
    store.set_hold(&hold("custody", 3650, 3), "").unwrap();
    store.set_hold(&hold("", 0, 4), "").unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.holds, vec![hold("custody", 3650, 3)]);
    // Each change its own record, with its delegation, client and note
    // (contract v17).
    let changes = store.hold_changes().unwrap();
    assert_eq!(changes.len(), 4);
    assert_eq!(
        changes[1],
        (through.clone(), "The records rule.".to_string())
    );
    // The latest change's note is read with the hold (contract v17).
    store
        .set_hold(
            &Hold {
                note: "Six years.".into(),
                ..hold("custody", 2190, 5)
            },
            "Six years.",
        )
        .unwrap();
    assert_eq!(store.snapshot().unwrap().holds[0].note, "Six years.");

    let archive = PluginArchive {
        instance_id: "snaptrade-1".into(),
        allowed: true,
        most_bytes: 53_687_091_200,
        updated_by: "ada@example.com".into(),
        updated_at_ns: 5,
        acting_through_delegation: "DLG-1".into(),
        client_name: "Claude".into(),
        note: "Archive past seven years.".into(),
    };
    store
        .put_archive(&archive, "Archive past seven years.")
        .unwrap();
    assert_eq!(
        store.archive_changes("snaptrade-1").unwrap(),
        vec![(archive.clone(), "Archive past seven years.".to_string())]
    );
    let withdrawn = PluginArchive {
        allowed: false,
        updated_at_ns: 6,
        acting_through_delegation: String::new(),
        client_name: String::new(),
        note: String::new(),
        ..archive.clone()
    };
    store.put_archive(&withdrawn, "").unwrap();
    assert_eq!(store.snapshot().unwrap().archives, vec![withdrawn]);

    let moved = |unit: &str, outcome: MoveOutcome, person: &str, at: i64| MoveRecord {
        r#move: Some(RecordMoveRequest {
            record_kind: "activity".into(),
            unit: unit.into(),
            record_count: 214,
            first_received_ns: 1_551_398_400_000_000_000,
            last_received_ns: 1_554_076_799_000_000_000,
            outcome: outcome as i32,
            rule: if person.is_empty() {
                "activity_window_days 2555".into()
            } else {
                String::new()
            },
        }),
        person: person.into(),
        at_ns: at,
        acting_through_delegation: if person == "ben@example.com" {
            "DLG-2".into()
        } else {
            String::new()
        },
        client_name: if person == "ben@example.com" {
            "Claude".into()
        } else {
            String::new()
        },
    };
    assert!(store
        .record_move("snaptrade-1", &moved("u1", MoveOutcome::Archived, "", 10))
        .unwrap());
    assert!(
        !store
            .record_move("snaptrade-1", &moved("u1", MoveOutcome::Archived, "", 11))
            .unwrap(),
        "a retry is recorded once"
    );
    assert!(store
        .record_move(
            "snaptrade-1",
            &moved("u1", MoveOutcome::Restored, "ben@example.com", 12)
        )
        .unwrap());
    assert!(store
        .record_move("snaptrade-1", &moved("u2", MoveOutcome::Archived, "", 13))
        .unwrap());
    assert!(store
        .record_move(
            "snaptrade-1",
            &moved("u2", MoveOutcome::Deleted, "ada@example.com", 14)
        )
        .unwrap());
    assert!(store
        .record_move("other-1", &moved("u9", MoveOutcome::Archived, "", 15))
        .unwrap());

    let latest = store
        .latest_move("snaptrade-1", "activity", "u1")
        .unwrap()
        .unwrap();
    assert_eq!(
        latest,
        moved("u1", MoveOutcome::Restored, "ben@example.com", 12)
    );
    let page = store.moves("snaptrade-1", 0, 2).unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].record.at_ns, 14);
    assert_eq!(page[1].record.at_ns, 13);
    let older = store.moves("snaptrade-1", page[1].move_id, 10).unwrap();
    assert_eq!(
        older.iter().map(|m| m.record.at_ns).collect::<Vec<_>>(),
        [12, 10]
    );
    assert_eq!(older[0].record.acting_through_delegation, "DLG-2");
    assert_eq!(older[0].record.client_name, "Claude");
    let archived = store.archived("snaptrade-1").unwrap();
    assert_eq!(archived.len(), 1);
    assert_eq!(
        archived[0].record_count, 214,
        "u2 was deleted; u1 is in the archive"
    );

    // Nothing is ever updated: five hold records, two archive records.
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let mut count = |table: &str| -> i64 {
        client
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .unwrap()
            .get(0)
    };
    assert_eq!(count("config_hold_change"), 5);
    assert_eq!(count("config_archive_change"), 2);
    assert_eq!(count("config_record_move"), 5);
}

#[test]
fn a_catalogue_licences_and_entitlements_are_kept_each_change_its_own_record() {
    use meridian_domain::v1::DatasetEntitlement;
    use meridian_pb::v1::{Catalogue, DatasetDeclaration, DatasetLicence};

    let store = store("data");
    let catalogue = Catalogue {
        datasets: vec![DatasetDeclaration {
            key: "daily".into(),
            vendor: "Coinbase".into(),
            data_types: vec!["meridian.v1.Price".into()],
            modes: vec![1],
            ..Default::default()
        }],
    };
    assert!(store.record_catalogue("coinbase-1", &catalogue, 1).unwrap());
    assert!(
        !store.record_catalogue("coinbase-1", &catalogue, 2).unwrap(),
        "unchanged"
    );
    let licence = |days: u32, at: i64| DatasetLicence {
        dataset: "coinbase-1:daily".into(),
        kept: true,
        retention_days: days,
        updated_by: "local|ada".into(),
        updated_at_ns: at,
        note: "terms".into(),
        ..Default::default()
    };
    store.set_dataset_licence(&licence(30, 3)).unwrap();
    store.set_dataset_licence(&licence(60, 4)).unwrap();
    let entitlement = |allowed: bool, at: i64| DatasetEntitlement {
        dataset: "coinbase-1:daily".into(),
        instance: "reporting-1".into(),
        allowed,
        updated_by: "local|ada".into(),
        updated_at_ns: at,
        ..Default::default()
    };
    store
        .set_dataset_entitlement(&entitlement(true, 5))
        .unwrap();
    store
        .set_dataset_entitlement(&entitlement(false, 6))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.catalogues.get("coinbase-1"), Some(&catalogue));
    assert_eq!(snapshot.licences, vec![licence(60, 4)]);
    assert_eq!(snapshot.entitlements, vec![entitlement(false, 6)]);
    let (licences, entitlements) = store.dataset_changes("coinbase-1:daily").unwrap();
    assert_eq!((licences.len(), entitlements.len()), (2, 2));
}
