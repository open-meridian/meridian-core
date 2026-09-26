use super::*;

const TOPICS: &str = "\
topic\tkind\tpublisher\tsubscriber
platform.config.command.apply-first-run-configuration\tcommand\tdashboard\tfirst-run
platform.config.query.enrolment\tquery\tdashboard\tconductor
platform.config.query.plugin-configuration\tquery\tsidecar\tconductor
platform.custody.*.event.sync-status\tevent\tcustody\tdashboard
platform.reference.event.instrument-applied\tevent\tinstrument\tdashboard,custody
platform.street.command.record-holding\tcommand\tcustody\tstreet
platform.street.query.list-positions\tquery\treporting\tstreet
";

const ROLES: &str = "\
name\tkind
custody\trole
reporting\trole
oms\trole
conductor\tcomponent
dashboard\tcomponent
first-run\tcomponent
instrument\tcomponent
sidecar\tcomponent
street\tcomponent
";

fn contract() -> Contract {
    Contract::parse(TOPICS, ROLES).expect("parses")
}

fn roles(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

fn user<'a>(users: &'a [User], name: &str) -> &'a User {
    users.iter().find(|u| u.user == name).expect("is there")
}

#[test]
fn an_instance_may_publish_only_as_itself() {
    let (publish, _) = permissions_for(&contract(), &roles(&["custody"]), "custody-1").unwrap();

    assert!(
        publish.contains(&"platform.custody.custody-1.event.sync-status".to_string()),
        "{publish:?}"
    );
    assert!(
        !publish.contains(&"platform.custody.*.event.sync-status".to_string()),
        "a role-wide credential would let two plugins publish as each other"
    );
}

#[test]
fn but_may_listen_to_every_instance_of_another_role() {
    let users = users(
        &contract(),
        &[Instance::component("dashboard-1", "dashboard")],
    )
    .unwrap();

    // The dashboard hears every connector. Rewriting this to its own
    // identifier would leave it subscribed to a topic nobody publishes, which
    // is the mistake this rule exists because of.
    assert!(
        user(&users, "dashboard-1")
            .subscribe
            .contains(&"platform.custody.*.event.sync-status".to_string()),
        "{users:?}"
    );
}

#[test]
fn a_plugin_holding_several_roles_holds_their_union_under_one_credential() {
    let users = users(
        &contract(),
        &[Instance::plugin("oems-1", &["custody", "reporting"])],
    )
    .unwrap();
    let oems = user(&users, "oems-1");
    assert!(oems
        .publish
        .contains(&"platform.street.command.record-holding".to_string()));
    assert!(oems
        .publish
        .contains(&"platform.street.query.list-positions".to_string()));
    assert!(oems
        .publish
        .contains(&"platform.custody.oems-1.event.sync-status".to_string()));
}

#[test]
fn every_plugin_credential_carries_its_sidecars_own_traffic() {
    // Asking for its configuration is the sidecar's, whatever the plugin's
    // roles -- a plugin with none included.
    let users = users(&contract(), &[Instance::plugin("reference-1", &[])]).unwrap();
    let reference = user(&users, "reference-1");
    assert_eq!(
        reference.publish,
        vec![
            "platform.config.query.plugin-configuration".to_string(),
            INBOX.to_string()
        ]
    );
}

#[test]
fn a_role_no_row_names_is_admitted_with_only_its_sidecars_traffic() {
    let users = users(&contract(), &[Instance::plugin("oms-1", &["oms"])]).unwrap();
    assert!(!user(&users, "oms-1")
        .publish
        .contains(&"platform.street.command.record-holding".to_string()));
}

#[test]
fn a_name_that_is_not_a_role_is_refused_rather_than_admitted() {
    let misspelt =
        users(&contract(), &[Instance::plugin("custody-9", &["custdy"])]).expect_err("not a role");
    assert!(
        misspelt.contains("custody-9") && misspelt.contains("not a role"),
        "{misspelt}"
    );

    let component = users(&contract(), &[Instance::plugin("sneaky-1", &["street"])])
        .expect_err("a component is not a role");
    assert!(component.contains("components"), "{component}");

    let unknown =
        users(&contract(), &[Instance::component("x-1", "ledger")]).expect_err("not a component");
    assert!(unknown.contains("does not have"), "{unknown}");
}

#[test]
fn the_dashboard_is_a_component_with_a_credential_of_its_own() {
    let users = users(
        &contract(),
        &[Instance::component("dashboard-1", "dashboard")],
    )
    .unwrap();
    let dashboard = user(&users, "dashboard-1");
    let runtime = user(&users, "runtime");
    assert!(dashboard
        .publish
        .contains(&"platform.config.query.enrolment".to_string()));
    assert!(
        !runtime
            .publish
            .contains(&"platform.config.query.enrolment".to_string()),
        "the dashboard's rights are not the runtime's"
    );
}

#[test]
fn a_tail_pattern_becomes_the_brokers_own() {
    assert_eq!(subject("**"), ">");
    assert_eq!(subject("platform.street.**"), "platform.street.>");
    assert_eq!(
        subject("platform.street.*.event.x"),
        "platform.street.*.event.x"
    );
}

#[test]
fn first_run_holds_its_own_credential() {
    let users = users(&contract(), &[]).expect("generates");

    assert!(user(&users, "first-run")
        .subscribe
        .contains(&"platform.config.command.apply-first-run-configuration".to_string()));
    assert!(
        !user(&users, "runtime")
            .subscribe
            .contains(&"platform.config.command.apply-first-run-configuration".to_string()),
        "a shared credential would let any component receive what the wizard sealed"
    );
}

#[test]
fn the_runtime_takes_the_contracts_own_columns() {
    let users = users(&contract(), &[]).expect("generates");
    let runtime = user(&users, "runtime");

    assert!(runtime
        .publish
        .contains(&"platform.reference.event.instrument-applied".to_string()));
    assert!(runtime
        .subscribe
        .contains(&"platform.config.query.enrolment".to_string()));
    assert!(runtime
        .subscribe
        .contains(&"platform.street.command.record-holding".to_string()));
}

#[test]
fn every_user_carries_its_inbox() {
    let users = users(
        &contract(),
        &[Instance::plugin("custody-test-1", &["custody"])],
    )
    .expect("generates");

    for user in &users {
        assert!(user.publish.contains(&INBOX.to_string()), "{}", user.user);
        assert!(user.subscribe.contains(&INBOX.to_string()), "{}", user.user);
    }
}

#[test]
fn a_password_is_named_and_never_written() {
    let rendered = rendered(
        "# test\n",
        &users(
            &contract(),
            &[Instance::plugin("custody-test-1", &["custody"])],
        )
        .expect("generates"),
    );

    assert!(
        rendered.contains("password: $MERIDIAN_NATS_CUSTODY_TEST_1"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("password: \""),
        "no password is in the file"
    );
}

#[test]
fn the_compiled_in_contract_generates() {
    // The real contract, as the chart's broker runs it: nothing in it names a
    // participant this code cannot place.
    let users = users(
        Contract::embedded(),
        &[
            Instance::component("dashboard-1", "dashboard"),
            Instance::plugin("custody-test-1", &["custody"]),
            Instance::plugin("reference-1", &[]),
        ],
    )
    .expect("generates");
    assert!(user(&users, "custody-test-1")
        .publish
        .contains(&"platform.street.command.record-statement".to_string()));
}
