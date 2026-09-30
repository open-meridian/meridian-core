use std::collections::BTreeSet;

use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessRecords, AccountGroup, AccountRecord, AccountState, Permission,
    SignInRecord, UserGroup,
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
        ..Default::default()
    }
}

fn account_group(id: &str, accounts: &[&str]) -> AccountGroup {
    AccountGroup {
        account_group_id: id.into(),
        name: id.into(),
        account_ids: accounts.iter().map(|a| a.to_string()).collect(),
        built_in: false,
    }
}

fn access_group(id: &str, level: AccessLevel) -> AccessGroup {
    AccessGroup {
        access_group_id: id.into(),
        name: id.into(),
        entries: vec![AccessEntry {
            plugin_instance_id: OMS.into(),
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
        links: vec![],
        plugin_settings: vec![],
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
    let levels = &trader().plugins[OMS].accounts;
    assert_eq!(levels.write, set(&["ACC-GROWTH"]));
    assert_eq!(levels.read, set(&["ACC-GROWTH", "ACC-INCOME"]));
    assert!(
        !levels.write.contains("ACC-INCOME"),
        "read on Income must not become write because write was granted elsewhere"
    );
}

#[test]
fn write_includes_read() {
    let levels = &trader().plugins[OMS].accounts;
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
    let scope = plugin_scope(&records(), &[], OMS);
    assert!(!scope.read.contains("ACC-LONELY"));
    assert!(!scope.write.contains("ACC-LONELY"));
}

#[test]
fn a_closed_account_stays_readable_and_is_never_writable() {
    let mut records = records();
    records.accounts[0].state = AccountState::Closed as i32;
    let access = person_access(&records, "someone", &["trading-desk".into()]);
    let levels = &access.plugins[OMS].accounts;
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
    assert!(!plugin_scope(&records, &[], OMS)
        .read
        .contains("ACC-NOT-YET"));
}

#[test]
fn a_plugins_scope_is_the_union_of_everyone_who_may_use_it() {
    let scope = plugin_scope(&records(), &[], OMS);
    assert_eq!(scope.read, set(&["ACC-GROWTH", "ACC-INCOME"]));
    assert_eq!(scope.write, set(&["ACC-GROWTH"]));
    assert!(plugin_scope(&records(), &[], "another-plugin").is_empty());
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
    assert_eq!(tam.write_account_ids, ["ACC-GROWTH"]);
    assert_eq!(tam.read_account_ids, ["ACC-GROWTH", "ACC-INCOME"]);
    let traders = &table.user_groups[0];
    assert_eq!(traders.write_account_ids, ["ACC-GROWTH"]);
    assert_eq!(traders.read_account_ids, ["ACC-GROWTH", "ACC-INCOME"]);
}

#[test]
fn a_local_account_is_found_by_the_login_first_run_names_it_by() {
    // The two ends of one string: first run names the administrator with
    // `local_login`, and the dashboard signs the account in with it. They
    // were two `format!`s, and a real cluster found them disagreeing.
    assert_eq!(local_login(" Ada "), "local|ada");
    let mut records = records();
    records.user_groups.push(UserGroup {
        user_group_id: "UG-admins".into(),
        name: "Deployment admins".into(),
        directory_groups: vec![],
        logins: vec![local_login("Ada")],
    });
    assert!(user_groups_of(&records, &local_login("ada"), &[])
        .iter()
        .any(|group| group.user_group_id == "UG-admins"));
}

#[test]
fn a_link_is_its_plugins_right_to_the_one_account_it_names() {
    let link = |plugin: &str, account: &str| ExternalAccountLink {
        plugin_instance_id: plugin.into(),
        external_account_id: "FIDELITY:1".into(),
        account_id: account.into(),
    };
    let links = [
        link("snaptrade", "ACC-LONELY"),
        link("snaptrade", "ACC-NOT-YET"),
    ];
    let scope = plugin_scope(&records(), &links, "snaptrade");
    assert_eq!(
        scope.read,
        set(&["ACC-LONELY"]),
        "an account in no group, by its link"
    );
    assert_eq!(
        scope.write,
        set(&["ACC-LONELY"]),
        "and a link to nothing reaches nothing"
    );
    assert!(
        !plugin_scope(&records(), &links, OMS)
            .read
            .contains("ACC-LONELY"),
        "no other plugin's by that link"
    );
}

#[test]
fn a_link_to_a_closed_account_reads_it_and_never_writes_it() {
    let mut records = records();
    for account in &mut records.accounts {
        account.state = AccountState::Closed as i32;
    }
    let links = [ExternalAccountLink {
        plugin_instance_id: "snaptrade".into(),
        external_account_id: "FIDELITY:1".into(),
        account_id: "ACC-LONELY".into(),
    }];
    let scope = plugin_scope(&records, &links, "snaptrade");
    assert!(scope.read.contains("ACC-LONELY") && scope.write.is_empty());
}

#[test]
fn two_entries_for_one_plugin_come_to_the_higher_level() {
    // decisions/026: access to a plugin is read or write, and nothing finer.
    // One access group naming the plugin twice, once at each level, is
    // write on its accounts, as the union of the two entries says.
    let mut records = records();
    records.access_groups.push(AccessGroup {
        access_group_id: "AX-BOTH".into(),
        name: "AX-BOTH".into(),
        entries: vec![
            AccessEntry {
                plugin_instance_id: OMS.into(),
                level: AccessLevel::Read as i32,
            },
            AccessEntry {
                plugin_instance_id: OMS.into(),
                level: AccessLevel::Write as i32,
            },
        ],
        built_in: false,
    });
    records.permissions = vec![permission("P-9", "UG-TRADERS", "AG-INCOME", "AX-BOTH")];
    let held = person_access(&records, "someone", &["trading-desk".into()]).on_plugin(OMS);
    assert_eq!(held.read, set(&["ACC-INCOME"]));
    assert_eq!(held.write, set(&["ACC-INCOME"]));
}

#[test]
fn a_plugin_the_person_holds_nothing_on_is_empty_levels() {
    assert!(trader().on_plugin("another-plugin").is_empty());
    let held = trader().on_plugin(OMS);
    assert_eq!(held.read_account_ids(), ["ACC-GROWTH", "ACC-INCOME"]);
    assert_eq!(held.write_account_ids(), ["ACC-GROWTH"]);
}

// ── Admin, the data level, and the built-in groups (2026-09-30) ──────────

fn entry(plugin: &str, level: AccessLevel) -> AccessEntry {
    AccessEntry {
        plugin_instance_id: plugin.into(),
        level: level as i32,
    }
}

fn with_group(records: &mut AccessRecords, id: &str, entries: Vec<AccessEntry>) {
    records.access_groups.push(AccessGroup {
        access_group_id: id.into(),
        name: id.into(),
        entries,
        built_in: false,
    });
}

#[test]
fn admin_alone_configures_and_reaches_no_account() {
    let mut records = records();
    with_group(
        &mut records,
        "AX-ADMIN",
        vec![entry(OMS, AccessLevel::Admin)],
    );
    records.permissions = vec![permission("P-9", "UG-TRADERS", "", "AX-ADMIN")];
    let held = person_access(&records, "someone", &["trading-desk".into()]).held(OMS);
    assert!(held.admin);
    assert_eq!(held.data, None);
    assert!(held.accounts.is_empty());
    assert_eq!(held.levels(), [AccessLevel::Admin], "Manage alone");
    assert_eq!(held.session(AccessLevel::Admin), Some(Levels::default()));
    assert_eq!(held.session(AccessLevel::Read), None, "no data level held");
    assert!(
        plugin_scope(&records, &[], OMS).is_empty(),
        "admin adds nothing to the plugin's scope"
    );
}

#[test]
fn admin_and_write_give_three_buttons_each_session_cut_to_its_level() {
    let mut records = records();
    with_group(
        &mut records,
        "AX-ADMIN",
        vec![entry(OMS, AccessLevel::Admin)],
    );
    records
        .permissions
        .push(permission("P-9", "UG-TRADERS", "", "AX-ADMIN"));
    let held = person_access(&records, "someone", &["trading-desk".into()]).held(OMS);
    assert_eq!(
        held.levels(),
        [AccessLevel::Admin, AccessLevel::Write, AccessLevel::Read],
        "Manage, Open and View"
    );
    assert_eq!(held.session(AccessLevel::Admin), Some(Levels::default()));
    let open = held.session(AccessLevel::Write).unwrap();
    assert_eq!(open.read, set(&["ACC-GROWTH", "ACC-INCOME"]));
    assert_eq!(
        open.write,
        set(&["ACC-GROWTH"]),
        "acting only on the write set"
    );
    let view = held.session(AccessLevel::Read).unwrap();
    assert_eq!(view.read, set(&["ACC-GROWTH", "ACC-INCOME"]));
    assert!(view.write.is_empty(), "View acts on nothing");
}

#[test]
fn a_reader_holds_view_alone() {
    let mut records = records();
    records.permissions = vec![permission("P-2", "UG-TRADERS", "AG-INCOME", "AX-READ")];
    let held = person_access(&records, "someone", &["trading-desk".into()]).held(OMS);
    assert_eq!(held.levels(), [AccessLevel::Read]);
    assert_eq!(held.session(AccessLevel::Write), None);
    assert_eq!(held.session(AccessLevel::Admin), None);
}

#[test]
fn levels_are_agnostic_of_accounts() {
    // Write on an empty account group still gives Open, acting on nothing.
    let mut records = records();
    records.permissions = vec![permission("P-9", "UG-TRADERS", "AG-EMPTY", "AX-WRITE")];
    let held = person_access(&records, "someone", &["trading-desk".into()]).held(OMS);
    assert_eq!(held.levels(), [AccessLevel::Write, AccessLevel::Read]);
    assert!(held.session(AccessLevel::Write).unwrap().is_empty());
}

#[test]
fn all_plugins_admin_administers_every_plugin_and_reaches_no_account() {
    let mut records = records();
    records
        .permissions
        .push(permission("P-9", "UG-ADMINS", "", ALL_PLUGINS_ADMIN));
    let ada = person_access(&records, ADA, &[]);
    assert!(ada.deployment_admin && ada.all_plugins_admin);
    assert!(ada.administers(OMS));
    assert!(
        ada.administers("launched-tomorrow"),
        "a plugin launched later included"
    );
    assert_eq!(ada.held(OMS).levels(), [AccessLevel::Admin]);
    assert!(ada.on_plugin(OMS).is_empty());
    assert_eq!(
        plugin_scope(&records, &[], OMS),
        plugin_scope(&self::records(), &[], OMS),
        "and nothing to any plugin's scope"
    );
}

#[test]
fn deployment_admin_alone_administers_no_plugin() {
    let ada = person_access(&records(), ADA, &[]);
    assert!(ada.deployment_admin);
    assert!(!ada.administers(OMS));
    assert!(!ada.held(OMS).holds_any());
}

#[test]
fn all_accounts_reaches_every_account_those_in_no_group_included() {
    let mut records = records();
    records.account_groups.push(AccountGroup {
        account_group_id: ALL_ACCOUNTS.into(),
        name: "All accounts".into(),
        account_ids: vec![],
        built_in: true,
    });
    records.permissions = vec![permission("P-9", "UG-TRADERS", ALL_ACCOUNTS, "AX-READ")];
    let held = person_access(&records, "someone", &["trading-desk".into()]).held(OMS);
    assert_eq!(
        held.accounts.read,
        set(&["ACC-GROWTH", "ACC-INCOME", "ACC-LONELY"])
    );
    records
        .accounts
        .push(account("ACC-LATER", AccountState::Open));
    assert!(
        plugin_scope(&records, &[], OMS).read.contains("ACC-LATER"),
        "an account opened later is in it"
    );
}

#[test]
fn a_level_is_named_by_its_button_or_its_name() {
    for level in [AccessLevel::Admin, AccessLevel::Write, AccessLevel::Read] {
        assert_eq!(parse_level(level_name(level)), Some(level));
        assert_eq!(parse_level(button(level)), Some(level));
    }
    assert_eq!(parse_level("owner"), None);
    assert_eq!(parse_level(""), None);
}
