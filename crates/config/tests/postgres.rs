//! The configuration store, against Postgres.
//!
//! What only the database can show: the migration seeding deployment admin,
//! the check that a permission to it names no account group, the two atomic
//! operations holding under concurrency, and a snapshot reading back exactly
//! what was written. Run by `make test-store`; fails loudly without a database.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use meridian_config::store::{Ending, Held, KnownPlugin, SettingChange, Store, Withdrawal};
use meridian_config::{PostgresStore, SettingsKey, DEPLOYMENT_ADMIN};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessLevel, AccountGroup, AccountRecord, AccountState,
    ExternalAccountLink, Permission, PluginLaunch, PluginLaunchState, PluginMetadata,
    PluginVersion, SignInRecord, UserGroup,
};
use meridian_pb::v1::{SettingChoice, SettingCondition, SettingDeclaration, SettingType};

static COUNTER: AtomicU64 = AtomicU64::new(0);

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
    store.migrate().expect("migrates");
    store.migrate().expect("migrating twice is a no-op");
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
    let admin = &snapshot.records.access_groups[0];
    assert_eq!(admin.access_group_id, DEPLOYMENT_ADMIN);
    assert!(admin.built_in);
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
    };
    store.put_account_group(&accounts).unwrap();
    let access = AccessGroup {
        access_group_id: "AX-1".into(),
        name: "Trading".into(),
        entries: vec![
            AccessEntry {
                plugin_instance_id: "oms-1".into(),
                level: AccessLevel::Write as i32,
            },
            AccessEntry {
                plugin_instance_id: "reporting-1".into(),
                level: AccessLevel::Read as i32,
            },
        ],
        built_in: false,
    };
    store.put_access_group(&access).unwrap();
    let permission = Permission {
        permission_id: "PRM-1".into(),
        user_group_id: "UG-1".into(),
        account_group_id: "AG-1".into(),
        access_group_id: "AX-1".into(),
    };
    store.add_permission(&permission).unwrap();
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
    assert_eq!(snapshot.records.user_groups, [group]);
    assert_eq!(snapshot.records.account_groups, [accounts]);
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
        })
        .unwrap();
    let with_accounts = Permission {
        account_group_id: "AG-1".into(),
        ..admin_permission("PRM-1", "UG-1")
    };
    assert!(
        store.add_permission(&with_accounts).is_err(),
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
                        &admin_permission(&format!("PRM-{n}"), &group.user_group_id),
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
            .add_permission(&admin_permission(&format!("PRM-{n}"), &group.user_group_id))
            .unwrap();
    }
    let threads: Vec<_> = (0..2)
        .map(|n| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || store.withdraw_permission(&format!("PRM-{n}")).unwrap())
        })
        .collect();
    let outcomes: Vec<Withdrawal> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(outcomes.contains(&Withdrawal::Withdrawn), "{outcomes:?}");
    assert!(outcomes.contains(&Withdrawal::LastAdmin), "{outcomes:?}");
    assert_eq!(store.snapshot().unwrap().records.permissions.len(), 1);
    assert_eq!(
        store.withdraw_permission("PRM-missing").unwrap(),
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
    assert!(store.begin_launch(&launch("snaptrade-1")).unwrap());
    assert!(
        !store.begin_launch(&launch("snaptrade-1")).unwrap(),
        "live already"
    );
    assert!(
        store.begin_launch(&launch("snaptrade-2")).unwrap(),
        "another instance"
    );

    let stop = Ending {
        state: PluginLaunchState::Stopped,
        by: "local|ada".into(),
        at_ns: 3,
        failure: String::new(),
    };
    let stopped = store
        .end_launch("snaptrade-1", &stop)
        .unwrap()
        .expect("was live");
    assert_eq!(stopped.state, PluginLaunchState::Stopped as i32);
    assert_eq!(stopped.stopped_at_ns, 3);
    assert!(
        store.end_launch("snaptrade-1", &stop).unwrap().is_none(),
        "none live now"
    );
    assert!(
        store.begin_launch(&launch("snaptrade-1")).unwrap(),
        "free again"
    );

    let launches = store.snapshot().unwrap().catalogue.launches;
    assert_eq!(launches.len(), 3);
    assert_eq!(launches[0], stop.applied_to(&launch("snaptrade-1")));
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
            std::thread::spawn(move || store.begin_launch(&launch("snaptrade-1")).unwrap())
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
    assert!(store.begin_launch(&live).unwrap());
    assert!(store.begin_launch(&launch("snaptrade-2")).unwrap());
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
        .record_declared_settings("snaptrade-1", &declared)
        .unwrap();
    assert_eq!(
        store.snapshot().unwrap().declared_settings["snaptrade-1"],
        declared
    );
    store
        .record_declared_settings("snaptrade-1", &declared[1..])
        .unwrap();
    assert_eq!(
        store.snapshot().unwrap().declared_settings["snaptrade-1"],
        declared[1..]
    );
}

#[test]
fn a_secret_is_at_rest_only_sealed_and_a_change_is_recorded_without_its_value() {
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
            "local|ada",
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
            "local|grace",
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

    // Who, what and when; the table has nowhere to put a value.
    let changes: Vec<(String, String, i16, String, i64)> = dump
        .query(
            "SELECT plugin_instance_id, name, action, changed_by, changed_at_ns
               FROM config_plugin_setting_change ORDER BY change_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        .collect();
    let expected = |name: &str, action: i16, by: &str, at: i64| {
        (
            "snaptrade-1".to_string(),
            name.to_string(),
            action,
            by.to_string(),
            at,
        )
    };
    assert_eq!(
        changes,
        [
            expected("api_key", 1, "local|ada", 5),
            expected("poll_minutes", 1, "local|ada", 5),
            expected("api_key", 2, "local|grace", 9),
        ]
    );
    let columns: Vec<String> = dump
        .query(
            "SELECT column_name::text FROM information_schema.columns
              WHERE table_schema = current_schema()
                AND table_name = 'config_plugin_setting_change'",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        !columns.iter().any(|c| c == "value" || c == "sealed"),
        "{columns:?}"
    );

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
    let (before, retiring): (Vec<_>, Vec<_>) = meridian_config::migrations::MIGRATIONS
        .iter()
        .partition(|m| m.name != "access_is_read_or_write");
    assert_eq!(retiring.len(), 1, "the migration that retires tags");
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
    assert!(store.verify().is_err(), "one migration behind");
    store.migrate().expect("migrates");
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
    let entry = |plugin: &str, level: AccessLevel| AccessEntry {
        plugin_instance_id: plugin.into(),
        level: level as i32,
    };
    assert_eq!(
        entries("AX-TRADING"),
        [
            entry("snaptrade-1", AccessLevel::Write),
            entry("oms-1", AccessLevel::Read)
        ],
        "read on holdings and write on custody is write on the plugin"
    );
    assert_eq!(
        entries("AX-VIEWING"),
        [entry("snaptrade-1", AccessLevel::Read)],
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
