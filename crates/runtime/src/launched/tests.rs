use std::collections::BTreeMap;

use super::*;

fn deployment(instance: Option<&str>, roles: Option<&str>) -> serde_json::Value {
    let mut metadata = serde_json::json!({"labels": {}, "annotations": {}});
    if let Some(instance) = instance {
        metadata["labels"][INSTANCE_LABEL] = instance.into();
    }
    if let Some(roles) = roles {
        metadata["annotations"][ROLES_ANNOTATION] = roles.into();
    }
    serde_json::json!({ "metadata": metadata })
}

#[test]
fn a_launched_plugin_is_its_instance_and_the_roles_it_was_approved_with() {
    let list = serde_json::json!({"items": [
        deployment(Some("snaptrade-2"), Some("custody, reporting")),
        deployment(Some("reference-1"), None),
        deployment(None, Some("custody")),
        deployment(Some("Not.A.Name"), Some("custody")),
        deployment(Some("snaptrade-2"), Some("custody")),
    ]});
    assert_eq!(
        from_deployments(&list),
        vec![
            Launched {
                instance_id: "reference-1".into(),
                roles: vec![]
            },
            Launched {
                instance_id: "snaptrade-2".into(),
                roles: vec!["custody".into(), "reporting".into()]
            },
        ],
        "sorted, one per instance, and nothing that does not name one"
    );
}

fn plugin(instance: &str) -> Launched {
    Launched {
        instance_id: instance.into(),
        roles: vec![],
    }
}

#[test]
fn a_plugin_that_appears_is_given_a_credential_and_one_that_is_held_is_kept() {
    let held = BTreeMap::from([
        (password_key("kept-1"), "Held".to_string()),
        (
            url_key("kept-1"),
            "nats://kept-1:Held@broker:4222".to_string(),
        ),
    ]);
    let minted = reconcile(
        &[plugin("kept-1"), plugin("new-1")],
        &held,
        "broker:4222",
        || "Minted".into(),
    );
    assert_eq!(
        minted.passwords["kept-1"], "Held",
        "reused, not minted again"
    );
    assert_eq!(minted.passwords["new-1"], "Minted");
    assert_eq!(
        minted.changes,
        BTreeMap::from([
            (password_key("new-1"), Some("Minted".into())),
            (
                url_key("new-1"),
                Some("nats://new-1:Minted@broker:4222".into())
            ),
        ]),
        "nothing for the plugin that already had one"
    );
}

#[test]
fn a_plugin_that_has_gone_is_taken_out_whole() {
    let held = BTreeMap::from([
        (password_key("gone-1"), "P".to_string()),
        (url_key("gone-1"), "nats://gone-1:P@broker:4222".to_string()),
        (password_key("stays-1"), "Q".to_string()),
        (
            url_key("stays-1"),
            "nats://stays-1:Q@broker:4222".to_string(),
        ),
    ]);
    let reconciled = reconcile(
        &[plugin("stays-1")],
        &held,
        "broker:4222",
        || unreachable!(),
    );
    assert_eq!(
        reconciled.changes,
        BTreeMap::from([(password_key("gone-1"), None), (url_key("gone-1"), None)])
    );
}

#[test]
fn a_connection_string_that_does_not_match_its_password_is_written_again() {
    // The broker's address moved, or the string was lost: the password stays.
    let held = BTreeMap::from([(password_key("p-1"), "P".to_string())]);
    let reconciled = reconcile(&[plugin("p-1")], &held, "broker:4222", || unreachable!());
    assert_eq!(
        reconciled.changes,
        BTreeMap::from([(url_key("p-1"), Some("nats://p-1:P@broker:4222".into()))])
    );
}

#[test]
fn a_minted_password_starts_with_a_letter_and_is_never_the_same_twice() {
    for _ in 0..200 {
        let password = mint();
        assert_eq!(password.len(), 32);
        assert!(password.as_bytes()[0].is_ascii_alphabetic(), "{password}");
        assert!(password.bytes().all(|b| b.is_ascii_alphanumeric()));
    }
    assert_ne!(mint(), mint());
}

#[test]
fn a_launched_plugin_is_admitted_by_a_hash_and_its_roles_topics() {
    let contract = meridian_sidecar::Contract::embedded();
    let launched = [
        Launched {
            instance_id: "snaptrade-2".into(),
            roles: vec!["custody".into()],
        },
        Launched {
            instance_id: "odd-1".into(),
            roles: vec!["not-a-role".into()],
        },
        Launched {
            instance_id: "custody-test-1".into(),
            roles: vec![],
        },
        Launched {
            instance_id: "waiting-1".into(),
            roles: vec![],
        },
    ];
    let hashes = BTreeMap::from([
        ("snaptrade-2".to_string(), "$2b$12$abc".to_string()),
        ("odd-1".to_string(), "$2b$12$def".to_string()),
        ("custody-test-1".to_string(), "$2b$12$ghi".to_string()),
    ]);
    let chart = [Instance::plugin("custody-test-1", &["custody"])];
    let (written, left_out) = configuration("", contract, &chart, &launched, &hashes).unwrap();

    let line = written
        .lines()
        .find(|line| line.contains("user: snaptrade-2,"))
        .expect("admitted");
    assert!(line.contains("password: \"$2b$12$abc\""), "{line}");
    assert!(
        line.contains("platform.custody.snaptrade-2.event.sync-status"),
        "speaking only as itself: {line}"
    );
    assert!(
        !written.contains("$2b$12$abc\n"),
        "the hash is quoted, not read as a variable"
    );
    assert_eq!(left_out.len(), 3, "{left_out:?}");
    assert!(left_out.iter().any(|why| why.contains("odd-1")));
    assert!(left_out
        .iter()
        .any(|why| why.contains("custody-test-1 is already")));
    assert!(left_out
        .iter()
        .any(|why| why.contains("waiting-1 has no credential")));
    assert_eq!(
        written.matches("user: custody-test-1,").count(),
        1,
        "the chart's instance keeps its own credential"
    );
}
