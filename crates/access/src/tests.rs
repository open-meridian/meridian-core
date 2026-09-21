use std::collections::BTreeSet;

use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessLevel, AccessRecords, AccountGroup, AccountRecord,
    AccountState, Permission, SignInRecord, UserGroup,
};

use super::*;

const ADA: &str = "https://directory.example.org|8812";
const OMS: &str = "oms-1";

fn account(id: &str, state: AccountState) -> AccountRecord {
    AccountRecord {
        account_id: id.into(),
        name: id.into(),
        state: state as i32,
        created_at_ns: 0,
    }
}

fn account_group(id: &str, accounts: &[&str]) -> AccountGroup {
    AccountGroup {
        account_group_id: id.into(),
        name: id.into(),
        account_ids: accounts.iter().map(|a| a.to_string()).collect(),
    }
}

fn access_group(id: &str, level: AccessLevel) -> AccessGroup {
    AccessGroup {
        access_group_id: id.into(),
        name: id.into(),
        entries: vec![AccessEntry {
            plugin_instance_id: OMS.into(),
            tag: "oms".into(),
            level: level as i32,
        }],
        built_in: false,
    }
}

fn permission(id: &str, user_group: &str, account_group: &str, access_group: &str) -> Permission {
    Permission {
        permission_id: id.into(),
        user_group_id: user_group.into(),
        account_group_id: account_group.into(),
        access_group_id: access_group.into(),
    }
}

/// Traders (by directory group) hold write on Growth and read on Income.
fn records() -> AccessRecords {
    AccessRecords {
        accounts: vec![
            account("ACC-GROWTH", AccountState::Open),
            account("ACC-INCOME", AccountState::Open),
            account("ACC-LONELY", AccountState::Open),
        ],
        user_groups: vec![
            UserGroup {
                user_group_id: "UG-TRADERS".into(),
                name: "Traders".into(),
                directory_groups: vec!["trading-desk".into()],
                logins: vec![],
            },
            UserGroup {
                user_group_id: "UG-ADMINS".into(),
                name: "Admins".into(),
                directory_groups: vec![],
                logins: vec![ADA.into()],
            },
        ],
        account_groups: vec![
            account_group("AG-GROWTH", &["ACC-GROWTH"]),
            account_group("AG-INCOME", &["ACC-INCOME"]),
            account_group("AG-EMPTY", &[]),
        ],
        access_groups: vec![
            access_group("AX-WRITE", AccessLevel::Write),
            access_group("AX-READ", AccessLevel::Read),
        ],
        permissions: vec![
            permission("P-1", "UG-TRADERS", "AG-GROWTH", "AX-WRITE"),
            permission("P-2", "UG-TRADERS", "AG-INCOME", "AX-READ"),
            permission("P-3", "UG-ADMINS", "", DEPLOYMENT_ADMIN),
        ],
        people: vec![],
        read_at_ns: 0,
    }
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

fn trader() -> Access {
    person_access(&records(), "someone", &["trading-desk".into()])
}

#[test]
fn access_is_combined_permission_by_permission_not_dimension_by_dimension() {
    let levels = &trader().plugins[OMS]["oms"];
    assert_eq!(levels.write, set(&["ACC-GROWTH"]));
    assert_eq!(levels.read, set(&["ACC-GROWTH", "ACC-INCOME"]));
    assert!(
        !levels.write.contains("ACC-INCOME"),
        "read on Income must not become write because write was granted elsewhere"
    );
}

#[test]
fn write_includes_read() {
    let levels = &trader().plugins[OMS]["oms"];
    assert!(levels.write.is_subset(&levels.read));
}

#[test]
fn an_empty_account_group_reaches_nothing() {
    let mut records = records();
    records.permissions = vec![permission("P-9", "UG-TRADERS", "AG-EMPTY", "AX-WRITE")];
    let access = person_access(&records, "someone", &["trading-desk".into()]);
    assert!(access.on_plugin(OMS).is_empty());
}

#[test]
fn an_account_in_no_group_is_in_no_plugin_scope() {
    let scope = plugin_scope(&records(), OMS);
    assert!(!scope.read.contains("ACC-LONELY"));
    assert!(!scope.write.contains("ACC-LONELY"));
}

#[test]
fn a_closed_account_stays_readable_and_is_never_writable() {
    let mut records = records();
    records.accounts[0].state = AccountState::Closed as i32;
    let access = person_access(&records, "someone", &["trading-desk".into()]);
    let levels = &access.plugins[OMS]["oms"];
    assert!(levels.read.contains("ACC-GROWTH"));
    assert!(!levels.write.contains("ACC-GROWTH"));
}

#[test]
fn a_login_or_a_directory_group_makes_a_member_and_nothing_else_does() {
    assert!(person_access(&records(), ADA, &[]).deployment_admin);
    assert!(!person_access(&records(), "someone", &["trading-desk".into()]).deployment_admin);
    assert!(person_access(&records(), "stranger", &["marketing".into()])
        .plugins
        .is_empty());
}

#[test]
fn deployment_admin_grants_no_plugin_access_by_itself() {
    assert!(person_access(&records(), ADA, &[]).plugins.is_empty());
}

#[test]
fn an_account_group_naming_an_unknown_account_reaches_only_what_exists() {
    let mut records = records();
    records.account_groups[0]
        .account_ids
        .push("ACC-NOT-YET".into());
    assert!(!plugin_scope(&records, OMS).read.contains("ACC-NOT-YET"));
}

#[test]
fn a_plugins_scope_is_the_union_of_everyone_who_may_use_it() {
    let scope = plugin_scope(&records(), OMS);
    assert_eq!(scope.read, set(&["ACC-GROWTH", "ACC-INCOME"]));
    assert_eq!(scope.write, set(&["ACC-GROWTH"]));
    assert!(plugin_scope(&records(), "another-plugin").is_empty());
}

#[test]
fn the_access_table_lists_groups_holding_access_and_people_who_have_signed_in() {
    let mut records = records();
    records.people = vec![
        SignInRecord {
            subject: "trader-1".into(),
            display_name: "Tam".into(),
            directory_groups: vec!["trading-desk".into()],
            signed_in_at_ns: 7,
        },
        SignInRecord {
            subject: ADA.into(),
            display_name: "Ada".into(),
            directory_groups: vec![],
            signed_in_at_ns: 8,
        },
    ];
    let table = plugin_access_table(&records, OMS);

    let groups: Vec<_> = table
        .user_groups
        .iter()
        .map(|g| g.user_group_id.as_str())
        .collect();
    assert_eq!(
        groups,
        ["UG-TRADERS"],
        "the admins' group holds no access to the plugin"
    );

    assert_eq!(
        table.people.len(),
        1,
        "only people with access to the plugin are listed"
    );
    let tam = &table.people[0];
    assert_eq!(tam.subject, "trader-1");
    assert_eq!(tam.user_group_ids, ["UG-TRADERS"]);
    assert_eq!(tam.last_signed_in_at_ns, 7);
    assert_eq!(tam.access[0].write_account_ids, ["ACC-GROWTH"]);
}
