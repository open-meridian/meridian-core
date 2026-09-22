use super::*;
use serde_json::json;

const MANIFEST: &str = "\
topic\tkind\tpublisher\tsubscriber
platform.config.command.apply-first-run-configuration\tcommand\tadmin\tfirst-run
platform.config.query.enrolment\tquery\tadmin\tconductor
platform.reference.event.instrument-applied\tevent\tinstrument\tadmin,custody
platform.street.command.record-holding\tcommand\tcustody\tstreet
";

fn grants() -> serde_json::Value {
    json!({"roles": {
        "custody": {
            "publish": [
                "platform.custody.*.event.sync-status",
                "platform.street.command.record-holding",
            ],
            "subscribe": ["platform.reference.event.instrument-applied"],
        },
        "admin": {
            "publish": ["platform.config.command.apply-first-run-configuration"],
            "subscribe": ["platform.custody.*.event.sync-status"],
        },
    }})
}

fn instance(id: &str, role: &str) -> Instance {
    Instance {
        instance_id: id.into(),
        role: role.into(),
        tags: vec![],
    }
}

#[test]
fn an_instance_may_publish_only_as_itself() {
    let (publish, _) = permissions_for(&grants(), "custody", &[], "custody-1");

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
    let (_, subscribe) = permissions_for(&grants(), "admin", &[], "dashboard-1");

    // The dashboard hears every connector. Rewriting this to its own
    // identifier would leave it subscribed to a topic nobody publishes, which
    // is the mistake this rule exists because of.
    assert!(
        subscribe.contains(&"platform.custody.*.event.sync-status".to_string()),
        "{subscribe:?}"
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
    let users = users(MANIFEST, &grants(), &[]).expect("generates");
    let first_run = users
        .iter()
        .find(|u| u.user == "first-run")
        .expect("is there");
    let runtime = users
        .iter()
        .find(|u| u.user == "runtime")
        .expect("is there");

    assert!(first_run
        .subscribe
        .contains(&"platform.config.command.apply-first-run-configuration".to_string()));
    assert!(
        !runtime
            .subscribe
            .contains(&"platform.config.command.apply-first-run-configuration".to_string()),
        "a shared credential would let any component receive what the wizard sealed"
    );
}

#[test]
fn the_runtime_takes_the_registrys_own_columns() {
    let users = users(MANIFEST, &grants(), &[]).expect("generates");
    let runtime = users
        .iter()
        .find(|u| u.user == "runtime")
        .expect("is there");

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
fn an_instance_with_no_grants_is_refused_rather_than_admitted() {
    let refused = users(MANIFEST, &grants(), &[instance("oms-1", "oms")])
        .expect_err("a role the grant table does not define");

    assert!(refused.contains("has no bus"), "{refused}");
}

#[test]
fn every_user_carries_its_inbox() {
    let users = users(MANIFEST, &grants(), &[instance("custody-1", "custody")]).expect("generates");

    for user in &users {
        assert!(user.publish.contains(&INBOX.to_string()), "{}", user.user);
        assert!(user.subscribe.contains(&INBOX.to_string()), "{}", user.user);
    }
}

#[test]
fn a_password_is_named_and_never_written() {
    let rendered = rendered(
        "# test\n",
        &users(MANIFEST, &grants(), &[instance("custody-1", "custody")]).expect("generates"),
    );

    assert!(
        rendered.contains("password: $MERIDIAN_NATS_CUSTODY_1"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("password: \""),
        "no password is in the file"
    );
}
