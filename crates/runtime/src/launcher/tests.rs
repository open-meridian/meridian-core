use super::*;

const REGISTRY: &str = "localhost:5000";

fn request() -> CreatePluginRequest {
    CreatePluginRequest {
        instance_id: "snaptrade-1".into(),
        image: format!("localhost:5000/plugins/snaptrade@sha256:{}", "a".repeat(64)),
        roles: vec!["custody".into()],
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
            "a role that is not a name",
            CreatePluginRequest {
                roles: vec!["a b".into()],
                ..request()
            },
        ),
    ];
    for (case, asked) in cases {
        assert!(checked(&asked, REGISTRY).is_err(), "{case}");
    }
}

const TEMPLATE: &str = r#"{"metadata":{"name":"r-plugin-__INSTANCE__","labels":{"meridian.dev/instance":"__INSTANCE__","meridian.dev/launched":"true"},"annotations":{"meridian.dev/roles":"__ROLES__"}},"spec":{"template":{"spec":{"hostname":"__INSTANCE__","containers":[{"name":"sidecar","env":[{"name":"MERIDIAN_PLUGIN_ROLES","value":"__ROLES__"}]},{"name":"plugin","image":"__IMAGE__"}]}}}}"#;

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
        "custody"
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

fn shapes(storage: bool) -> Shapes {
    Shapes {
        shards: Some("several".into()),
        plain: "plain".into(),
        live: Some("live".into()),
        storage: storage.then(|| StorageShapes {
            plain: "plain with storage".into(),
            live: Some("live with storage".into()),
            claim: "claim".into(),
            edge_roles: [
                "confirmation",
                "connectivity",
                "custody",
                "data",
                "reporting",
                "servicing",
                "settlement",
            ]
            .map(String::from)
            .to_vec(),
        }),
        archive: None,
    }
}

fn chosen(workload: &'static str, claim: Option<&'static str>) -> Result<Chosen<'static>, String> {
    Ok(Chosen { workload, claim })
}

#[test]
fn a_live_request_is_made_live_on_a_development_deployment_and_nowhere_else() {
    let shapes = shapes(false);
    let ordinary = request();
    let asked_live = CreatePluginRequest {
        live: true,
        ..request()
    };
    assert_eq!(
        template_for(&ordinary, false, &shapes),
        chosen("plain", None)
    );
    assert_eq!(
        template_for(&ordinary, true, &shapes),
        chosen("plain", None)
    );
    assert_eq!(
        template_for(&asked_live, true, &shapes),
        chosen("live", None)
    );
    let refused = template_for(&asked_live, false, &shapes).unwrap_err();
    assert!(
        refused.contains("not installed for development"),
        "{refused}"
    );
    let no_live = Shapes {
        live: None,
        ..shapes.clone()
    };
    let refused = template_for(&asked_live, true, &no_live).unwrap_err();
    assert!(refused.contains("renders no live shape"), "{refused}");
}

#[test]
fn a_plugin_holding_an_edge_role_is_given_its_storage_and_no_other_plugin_is() {
    let shapes = shapes(true);
    let edge = request();
    assert_eq!(
        template_for(&edge, false, &shapes),
        chosen("plain with storage", Some("claim"))
    );
    let live = CreatePluginRequest {
        live: true,
        ..request()
    };
    assert_eq!(
        template_for(&live, true, &shapes),
        chosen("live with storage", Some("claim"))
    );
    for roles in [
        vec![],
        vec!["operations"],
        vec!["oms", "ems"],
        vec!["compliance"],
    ] {
        let inner = CreatePluginRequest {
            roles: roles.iter().map(|r| r.to_string()).collect(),
            ..request()
        };
        assert_eq!(
            template_for(&inner, false, &shapes),
            chosen("plain", None),
            "{roles:?}"
        );
    }
    let both = CreatePluginRequest {
        roles: vec!["operations".into(), "reporting".into()],
        ..request()
    };
    assert_eq!(
        template_for(&both, false, &shapes),
        chosen("plain with storage", Some("claim")),
        "one edge role is enough"
    );
}

#[test]
fn a_chart_giving_no_storage_launches_an_edge_plugin_as_before() {
    assert_eq!(
        template_for(&request(), false, &shapes(false)),
        chosen("plain", None)
    );
}

const CLAIM: &str = r#"{"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"r-storage-__INSTANCE__","labels":{"meridian.dev/instance":"__INSTANCE__","meridian.dev/launched":"true","meridian.dev/plugin":"__PLUGIN__"}},"spec":{"accessModes":["ReadWriteOnce"]}}"#;

#[test]
fn the_claim_is_the_instances_and_names_the_plugin_whose_records_it_holds() {
    let made = claim(CLAIM, &request()).unwrap();
    assert_eq!(made["metadata"]["name"], "r-storage-snaptrade-1");
    assert_eq!(
        made["metadata"]["labels"]["meridian.dev/instance"],
        "snaptrade-1"
    );
    assert_eq!(made["metadata"]["labels"][PLUGIN_LABEL], "snaptrade");
    let unmarked = CLAIM.replace(r#","meridian.dev/launched":"true""#, "");
    assert!(claim(&unmarked, &request()).is_err());
    let unnamed = CLAIM.replace(r#","meridian.dev/plugin":"__PLUGIN__""#, "");
    assert!(claim(&unnamed, &request()).is_err());
    let newer = CLAIM.replace("ReadWriteOnce", "__ACCESS__");
    assert!(claim(&newer, &request())
        .unwrap_err()
        .contains("__ACCESS__"));
}

#[test]
fn kept_storage_is_mounted_again_by_the_same_plugin_and_never_by_another() {
    let kept = claim(CLAIM, &request()).unwrap();
    assert_eq!(reusable(&kept, &request()), Ok(()));
    let another = CreatePluginRequest {
        image: format!("localhost:5000/plugins/elsewhere@sha256:{}", "b".repeat(64)),
        ..request()
    };
    let refused = reusable(&kept, &another).unwrap_err();
    assert!(
        refused.contains("holds the records of snaptrade, not elsewhere"),
        "{refused}"
    );
    let a_newer_version = CreatePluginRequest {
        image: format!("localhost:5000/plugins/snaptrade@sha256:{}", "c".repeat(64)),
        ..request()
    };
    assert_eq!(reusable(&kept, &a_newer_version), Ok(()));
    let not_the_launchers: serde_json::Value = serde_json::json!({
        "metadata": {"name": "r-storage-snaptrade-1", "labels": {"meridian.dev/instance": "snaptrade-1"}}
    });
    let refused = reusable(&not_the_launchers, &request()).unwrap_err();
    assert!(
        refused.contains("was not made by the launcher"),
        "{refused}"
    );
}

const LOCAL_ARCHIVE: &str = r#"{"env":[{"name":"MERIDIAN_ARCHIVE_DIR","value":"/var/lib/meridian/archive"}],"volumeMounts":[{"mountPath":"/var/lib/meridian/archive","name":"archive","subPath":"__INSTANCE__"}],"volumes":[{"name":"archive","persistentVolumeClaim":{"claimName":"check-meridian-runtime-archive"}}]}"#;
const BUCKET_ARCHIVE: &str = r#"{"env":[{"name":"MERIDIAN_ARCHIVE_BUCKET","value":"s3://firm/meridian/__INSTANCE__"}],"serviceAccountName":"archive-writer"}"#;

fn edge() -> Vec<String> {
    vec!["custody".to_string()]
}

fn pod() -> serde_json::Value {
    serde_json::json!({"spec": {"template": {"spec": {
        "containers": [
            {"name": "sidecar", "env": [{"name": "MERIDIAN_PLUGIN_ROLES", "value": "custody"}]},
            {"name": "plugin", "env": [{"name": "MERIDIAN_STORAGE_DIR", "value": "/var/lib/meridian/storage"}],
             "volumeMounts": [{"name": "storage", "mountPath": "/var/lib/meridian/storage"}]}
        ],
        "volumes": [{"name": "storage", "persistentVolumeClaim": {"claimName": "s"}}]
    }}}})
}

fn allowed() -> CreatePluginRequest {
    CreatePluginRequest {
        archive: Some(meridian_domain::v1::PluginArchive {
            instance_id: "snaptrade-1".into(),
            allowed: true,
            ..Default::default()
        }),
        ..request()
    }
}

#[test]
fn an_allowed_archive_is_the_instances_own_directory_in_the_plugins_container_alone() {
    let made = with_archive(pod(), Some(LOCAL_ARCHIVE), &allowed(), &edge()).unwrap();
    let spec = &made["spec"]["template"]["spec"];
    let plugin = &spec["containers"][1];
    assert_eq!(plugin["env"][1]["name"], "MERIDIAN_ARCHIVE_DIR");
    assert_eq!(plugin["volumeMounts"][1]["subPath"], "snaptrade-1");
    assert_eq!(
        spec["volumes"][1]["persistentVolumeClaim"]["claimName"],
        "check-meridian-runtime-archive"
    );
    assert_eq!(
        spec["containers"][0]["env"].as_array().unwrap().len(),
        1,
        "the sidecar is given nothing"
    );
    assert!(spec["serviceAccountName"].is_null());

    let bucket = with_archive(pod(), Some(BUCKET_ARCHIVE), &allowed(), &edge()).unwrap();
    let spec = &bucket["spec"]["template"]["spec"];
    assert_eq!(
        spec["containers"][1]["env"][1]["value"],
        "s3://firm/meridian/snaptrade-1"
    );
    assert_eq!(spec["serviceAccountName"], "archive-writer");
}

#[test]
fn the_archives_bound_is_given_beside_it_and_none_leaves_it_unset() {
    let bound = |most_bytes: u64| CreatePluginRequest {
        archive: Some(meridian_domain::v1::PluginArchive {
            instance_id: "snaptrade-1".into(),
            allowed: true,
            most_bytes,
            ..Default::default()
        }),
        ..request()
    };
    for shape in [LOCAL_ARCHIVE, BUCKET_ARCHIVE] {
        let made = with_archive(pod(), Some(shape), &bound(53_687_091_200), &edge()).unwrap();
        let env = made["spec"]["template"]["spec"]["containers"][1]["env"]
            .as_array()
            .unwrap()
            .clone();
        let given: Vec<_> = env
            .iter()
            .filter(|e| e["name"] == ARCHIVE_MOST_BYTES)
            .collect();
        assert_eq!(given.len(), 1, "{env:?}");
        assert_eq!(
            given[0]["value"], "53687091200",
            "a string, as every env value is"
        );

        let unbounded = with_archive(pod(), Some(shape), &bound(0), &edge()).unwrap();
        let env = &unbounded["spec"]["template"]["spec"]["containers"][1]["env"];
        assert!(
            !env.as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == ARCHIVE_MOST_BYTES),
            "0 is no bound, and the plugin is told none: {env}"
        );
    }
    // The sidecar is told nothing of it.
    let made = with_archive(pod(), Some(LOCAL_ARCHIVE), &bound(1), &edge()).unwrap();
    assert_eq!(
        made["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn no_archive_unless_one_is_allowed_and_the_chart_keeps_one() {
    // None carried, or one withdrawn: as before.
    assert_eq!(
        with_archive(pod(), Some(LOCAL_ARCHIVE), &request(), &edge()).unwrap(),
        pod()
    );
    let withdrawn = CreatePluginRequest {
        archive: Some(meridian_domain::v1::PluginArchive {
            allowed: false,
            ..Default::default()
        }),
        ..request()
    };
    assert_eq!(
        with_archive(pod(), Some(LOCAL_ARCHIVE), &withdrawn, &edge()).unwrap(),
        pod()
    );
    assert!(with_archive(pod(), None, &allowed(), &edge())
        .unwrap_err()
        .contains("keeps none"));
    let inner = CreatePluginRequest {
        roles: vec!["operations".into()],
        ..allowed()
    };
    assert!(with_archive(pod(), Some(LOCAL_ARCHIVE), &inner, &edge())
        .unwrap_err()
        .contains("no edge role"));
    assert!(with_archive(
        pod(),
        Some(r#"{"env":[{"name":"X","value":"__WHO__"}]}"#),
        &allowed(),
        &edge()
    )
    .is_err());
}

/// Contract v30 (W8.3; ruled 2026-11-11): a launch's own share at its
/// sidecar is written over the chart's, field by field; one leaving the
/// sidecar nothing of its own budget is refused naming it.
#[test]
fn a_launchs_own_limits_are_written_over_the_charts_and_a_share_leaving_nothing_is_refused() {
    let shape = || {
        serde_json::json!({
            "metadata": {"labels": {"meridian.dev/launched": "true"}},
            "spec": {"template": {"spec": {"containers": [
                {"name": "sidecar",
                 "resources": {"limits": {"memory": "256Mi"}},
                 "env": [
                    {"name": "MERIDIAN_MEMORY_BUDGET_PERCENT", "value": "80"},
                    {"name": "MERIDIAN_SIDECAR_IN_FLIGHT", "value": "16"},
                    {"name": "MERIDIAN_SIDECAR_REQUESTS_PER_SECOND", "value": "400"},
                    {"name": "MERIDIAN_SIDECAR_ANSWER_MIB", "value": "32"}]},
                {"name": "plugin", "env": []}]}}}
        })
    };
    let said = |made: &serde_json::Value, name: &str| -> Vec<String> {
        made["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["name"] == name)
            .map(|e| e["value"].as_str().unwrap().to_string())
            .collect()
    };
    // None asked: the chart's, untouched.
    let made = with_limits(shape(), &request()).unwrap();
    assert_eq!(said(&made, "MERIDIAN_SIDECAR_IN_FLIGHT"), ["16"]);
    // Two asked, one left at 0: those two over the chart's, the third kept.
    let asked = CreatePluginRequest {
        limits: Some(meridian_domain::v1::SidecarLimits {
            in_flight: 4,
            requests_per_second: 0,
            answer_mib: 8,
            // The lake write share (step 3d; section 6.9).
            write_mib_per_second: 2,
        }),
        ..request()
    };
    let made = with_limits(shape(), &asked).unwrap();
    assert_eq!(said(&made, "MERIDIAN_SIDECAR_IN_FLIGHT"), ["4"]);
    assert_eq!(said(&made, "MERIDIAN_SIDECAR_REQUESTS_PER_SECOND"), ["400"]);
    assert_eq!(said(&made, "MERIDIAN_SIDECAR_ANSWER_MIB"), ["8"]);
    assert_eq!(said(&made, "MERIDIAN_SIDECAR_WRITE_MIB_PER_SECOND"), ["2"]);
    // Answers in flight that leave the sidecar nothing: refused, naming it.
    let greedy = CreatePluginRequest {
        limits: Some(meridian_domain::v1::SidecarLimits {
            answer_mib: 200,
            ..Default::default()
        }),
        ..request()
    };
    let refused = with_limits(shape(), &greedy).unwrap_err();
    assert!(refused.contains("limits.answer_mib 200"), "{refused}");
}

/// The chart's shards shape, as far as the launcher reads it.
fn several_pods() -> serde_json::Value {
    serde_json::json!({"spec": {"replicas": 2,
        "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": 1, "maxUnavailable": 2}},
        "template": {"spec": {
        "serviceAccountName": "check-meridian-runtime-member",
        "containers": [
            {"name": "sidecar", "env": [
                {"name": "MERIDIAN_PLUGIN_SHARDS", "value": "1"},
                {"name": "MERIDIAN_LEASE_RENEW_SECONDS", "value": "2"}
            ]},
            {"name": "plugin", "env": []}
        ]
    }}}})
}

/// Step 3d (the resilience spec's section 7; approved 2026-11-15): a launch
/// with shards or a standby pod is made from the chart's shards shape, a
/// pod per shard and one more for a standby, each sidecar told the count;
/// any other launch is the instance's one pod, as before.
#[test]
fn a_launch_with_shards_or_a_standby_is_a_pod_each_and_its_sidecars_are_told_the_count() {
    let shapes = shapes(false);
    let plain = template_for(&request(), false, &shapes).unwrap();
    // None asked, and one shard with no standby: the plain shape, one pod.
    for (shards, standby) in [(0, false), (1, false)] {
        let asked = CreatePluginRequest {
            shards,
            standby,
            ..request()
        };
        assert!(!several(&asked));
        let chosen = shards_template(
            template_for(&asked, false, &shapes).unwrap(),
            &asked,
            &shapes,
        )
        .unwrap();
        assert_eq!(chosen, plain);
        assert_eq!(
            with_shards(several_pods(), &asked, false).unwrap(),
            several_pods(),
            "untouched"
        );
    }
    let said = |made: &serde_json::Value| -> Vec<String> {
        made["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["name"] == "MERIDIAN_PLUGIN_SHARDS")
            .map(|e| e["value"].as_str().unwrap().to_string())
            .collect()
    };
    for (shards, standby, pods, told) in [
        (2, false, 2, "2"),
        (2, true, 3, "2"),
        (0, true, 2, "1"),
        (1, true, 2, "1"),
        (64, true, 65, "64"),
    ] {
        let asked = CreatePluginRequest {
            shards,
            standby,
            ..request()
        };
        let chosen = shards_template(
            template_for(&asked, false, &shapes).unwrap(),
            &asked,
            &shapes,
        )
        .unwrap();
        assert_eq!(
            chosen,
            Chosen {
                workload: "several",
                claim: None
            }
        );
        let made = with_shards(several_pods(), &asked, false).unwrap();
        assert_eq!(
            made["spec"]["replicas"], pods,
            "{shards} shards, standby {standby}"
        );
        assert_eq!(said(&made), [told], "told once");
        // Rolled a shard at a time: one pod may be away, and one more for
        // a standby pod, which is never ready (review finding 6).
        assert_eq!(
            made["spec"]["strategy"]["rollingUpdate"],
            serde_json::json!({"maxSurge": 1, "maxUnavailable": 1 + u32::from(standby)}),
            "{shards} shards, standby {standby}"
        );
        // The plugin's container is told nothing.
        assert_eq!(
            made["spec"]["template"]["spec"]["containers"][1]["env"],
            serde_json::json!([])
        );
    }
}

/// Review finding 11 (accepted 2026-11-15): shards above the in-flight or
/// answer limit its sidecars are given -- the chart's, or the launch's own
/// -- are refused naming shards: each shard holds at least one of each.
#[test]
fn shards_above_a_limit_its_sidecars_are_given_are_refused_naming_shards() {
    let limited = |name: &str, value: &str| {
        let mut shape = several_pods();
        shape["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name": name, "value": value}));
        shape
    };
    let asked = CreatePluginRequest {
        shards: 3,
        ..request()
    };
    for (name, said) in [
        ("MERIDIAN_SIDECAR_IN_FLIGHT", "sidecar.limits.inFlight"),
        ("MERIDIAN_SIDECAR_ANSWER_MIB", "sidecar.limits.answerMiB"),
    ] {
        let refused = with_shards(limited(name, "2"), &asked, false).unwrap_err();
        assert!(
            refused.starts_with("shards:")
                && refused.contains(said)
                && refused.contains("at most 2"),
            "{refused}"
        );
        with_shards(limited(name, "3"), &asked, false).unwrap();
    }
    // A standby alone is one shard, within any limit.
    let standby = CreatePluginRequest {
        standby: true,
        ..request()
    };
    with_shards(limited("MERIDIAN_SIDECAR_IN_FLIGHT", "1"), &standby, false).unwrap();
}

#[test]
fn shards_are_refused_where_the_instance_cannot_be_several_pods() {
    let asked = CreatePluginRequest {
        shards: 2,
        ..request()
    };
    // A chart that renders no shape for them.
    let none = Shapes {
        shards: None,
        ..shapes(false)
    };
    let refused =
        shards_template(template_for(&asked, false, &none).unwrap(), &asked, &none).unwrap_err();
    assert!(refused.contains("renders no shape"), "{refused}");
    // Storage of its own: a claim is one pod's.
    let stored = shapes(true);
    let chosen = template_for(&asked, false, &stored).unwrap();
    assert!(chosen.claim.is_some(), "the request's plugin keeps storage");
    let refused = shards_template(chosen, &asked, &stored).unwrap_err();
    assert!(refused.contains("storage is one pod's"), "{refused}");
    // Live.
    let live = CreatePluginRequest {
        live: true,
        standby: true,
        ..request()
    };
    let shapes = shapes(false);
    let refused =
        shards_template(template_for(&live, true, &shapes).unwrap(), &live, &shapes).unwrap_err();
    assert!(refused.contains("a live instance is one pod"), "{refused}");
    // More than a declaration may name.
    let many = CreatePluginRequest {
        shards: 65,
        ..request()
    };
    let refused =
        shards_template(template_for(&many, false, &shapes).unwrap(), &many, &shapes).unwrap_err();
    assert!(refused.starts_with("shards:"), "{refused}");
    // An archive's own account: the lease right is the members'.
    let refused = with_shards(several_pods(), &asked, true).unwrap_err();
    assert!(refused.contains("under the members' account"), "{refused}");
    // A template that tells its sidecar no count is not the shards shape.
    let refused = with_shards(pod(), &asked, false).unwrap_err();
    assert!(refused.contains("MERIDIAN_PLUGIN_SHARDS"), "{refused}");
}
