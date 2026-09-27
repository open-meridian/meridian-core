use super::*;

const REGISTRY: &str = "localhost:5000";

fn request() -> CreatePluginRequest {
    CreatePluginRequest {
        instance_id: "snaptrade-1".into(),
        image: format!("localhost:5000/plugins/snaptrade@sha256:{}", "a".repeat(64)),
        roles: vec!["custody".into()],
        tags: vec!["holdings".into()],
        interface: true,
        ..Default::default()
    }
}

#[test]
fn a_request_for_the_deployments_own_registry_by_digest_is_checked_through() {
    assert_eq!(checked(&request(), REGISTRY), Ok(()));
}

#[test]
fn anything_else_is_refused_before_anything_is_made() {
    let cases: Vec<(&str, CreatePluginRequest)> = vec![
        (
            "another registry",
            CreatePluginRequest {
                image: format!("ghcr.io/x/plugins/snaptrade@sha256:{}", "a".repeat(64)),
                ..request()
            },
        ),
        (
            "a tag",
            CreatePluginRequest {
                image: "localhost:5000/plugins/snaptrade:latest".into(),
                ..request()
            },
        ),
        (
            "a short digest",
            CreatePluginRequest {
                image: "localhost:5000/plugins/snaptrade@sha256:abc".into(),
                ..request()
            },
        ),
        (
            "outside plugins/",
            CreatePluginRequest {
                image: format!("localhost:5000/other/snaptrade@sha256:{}", "a".repeat(64)),
                ..request()
            },
        ),
        (
            "an instance that is not a name",
            CreatePluginRequest {
                instance_id: "Snap\"trade".into(),
                ..request()
            },
        ),
        (
            "a role that could break out",
            CreatePluginRequest {
                roles: vec!["custody\",\"x".into()],
                ..request()
            },
        ),
        (
            "a tag that could break out",
            CreatePluginRequest {
                tags: vec!["a b".into()],
                ..request()
            },
        ),
    ];
    for (case, asked) in cases {
        assert!(checked(&asked, REGISTRY).is_err(), "{case}");
    }
}

const TEMPLATE: &str = r#"{"metadata":{"name":"r-plugin-__INSTANCE__","labels":{"meridian.dev/instance":"__INSTANCE__","meridian.dev/launched":"true"},"annotations":{"meridian.dev/roles":"__ROLES__"}},"spec":{"template":{"spec":{"hostname":"__INSTANCE__","containers":[{"name":"sidecar","env":[{"name":"MERIDIAN_PLUGIN_TAGS","value":"__TAGS__"}]},{"name":"plugin","image":"__IMAGE__"}]}}}}"#;

#[test]
fn the_template_is_filled_in_with_the_checked_request_and_nothing_else() {
    let made = manifest(TEMPLATE, &request()).unwrap();
    assert_eq!(made["metadata"]["name"], "r-plugin-snaptrade-1");
    assert_eq!(
        made["metadata"]["annotations"]["meridian.dev/roles"],
        "custody"
    );
    assert_eq!(made["spec"]["template"]["spec"]["hostname"], "snaptrade-1");
    assert_eq!(
        made["spec"]["template"]["spec"]["containers"][1]["image"],
        request().image
    );
    assert_eq!(
        made["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"],
        "holdings"
    );
}

#[test]
fn a_template_that_does_not_mark_its_workload_as_the_launchers_is_refused() {
    let unmarked = TEMPLATE.replace(r#","meridian.dev/launched":"true""#, "");
    assert!(manifest(&unmarked, &request()).is_err());
}

#[test]
fn a_placeholder_this_launcher_does_not_know_is_refused_rather_than_left_in() {
    let newer = TEMPLATE.replace(r#""name":"plugin""#, r#""name":"__SIDECAR_NAME__""#);
    let refused = manifest(&newer, &request()).unwrap_err();
    assert!(refused.contains("__SIDECAR_NAME__"), "{refused}");
    assert!(
        manifest(TEMPLATE, &request()).is_ok(),
        "a __x in a value is not one"
    );
}

#[test]
fn a_live_request_is_made_live_on_a_development_deployment_and_nowhere_else() {
    let plain = "plain";
    let live = Some("live");
    let ordinary = request();
    let asked_live = CreatePluginRequest {
        live: true,
        ..request()
    };
    assert_eq!(template_for(&ordinary, false, plain, live), Ok("plain"));
    assert_eq!(template_for(&ordinary, true, plain, live), Ok("plain"));
    assert_eq!(template_for(&asked_live, true, plain, live), Ok("live"));
    let refused = template_for(&asked_live, false, plain, live).unwrap_err();
    assert!(
        refused.contains("not installed for development"),
        "{refused}"
    );
    let refused = template_for(&asked_live, true, plain, None).unwrap_err();
    assert!(refused.contains("renders no live shape"), "{refused}");
}
