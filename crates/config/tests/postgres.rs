//! The configuration store, against Postgres.
//!
//! What only the database can show: the migration seeding deployment admin,
//! the check that a permission to it names no account group, the two atomic
//! operations holding under concurrency, and a snapshot reading back exactly
//! what was written. Run by `make test-store`; fails loudly without a database.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use meridian_config::store::{KnownPlugin, Store, Withdrawal};
use meridian_config::{PostgresStore, DEPLOYMENT_ADMIN};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessLevel, AccountGroup, AccountRecord, AccountState,
    ExternalAccountLink, Permission, SignInRecord, UserGroup,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn base_url() -> String {
    std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    )
}

/// A migrated store in a schema of this test's own.
fn store(tag: &str) -> PostgresStore {
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
    store
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
                tag: "oms".into(),
                level: AccessLevel::Write as i32,
            },
            AccessEntry {
                plugin_instance_id: "oms-1".into(),
                tag: "reporting".into(),
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
        role: "oms".into(),
        tags: vec!["reporting".into()],
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
