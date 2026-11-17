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
            ..Default::default()
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
    serde_json::json!({"metadata": {"name": "check-meridian-runtime-plugin-snaptrade-1", "labels": {
        "app.kubernetes.io/instance": "check",
        "meridian.dev/component": "sidecar",
        "meridian.dev/instance": "snaptrade-1",
        "meridian.dev/launched": "true"
    }}, "spec": {"replicas": 2,
        "strategy": {"type": "RollingUpdate", "rollingUpdate": {"maxSurge": 1, "maxUnavailable": 2}},
        "template": {"spec": {
        "serviceAccountName": "check-meridian-runtime-shards-snaptrade-1",
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
    // None asked, and one shard with no standby: the plain shape, one pod
    // -- as the launcher takes a request, through [`one_pod_as_shard`]
    // (item 30), its instance's storage not shared.
    for (shards, standby) in [(0, false), (1, false)] {
        let asked = one_pod_as_shard(
            CreatePluginRequest {
                shards,
                standby,
                ..request()
            },
            false,
        );
        assert!(!several(&asked));
        let chosen = shards_template(
            template_for(&asked, false, &shapes).unwrap(),
            &asked,
            &shapes,
        )
        .unwrap();
        assert_eq!(chosen, plain);
        assert_eq!(
            with_shards(several_pods(), &asked).unwrap(),
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
        let made = with_shards(several_pods(), &asked).unwrap();
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
        let refused = with_shards(limited(name, "2"), &asked).unwrap_err();
        assert!(
            refused.starts_with("shards:")
                && refused.contains(said)
                && refused.contains("at most 2"),
            "{refused}"
        );
        with_shards(limited(name, "3"), &asked).unwrap();
    }
    // A standby alone is one shard, within any limit.
    let standby = CreatePluginRequest {
        standby: true,
        ..request()
    };
    with_shards(limited("MERIDIAN_SIDECAR_IN_FLIGHT", "1"), &standby).unwrap();
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
    // Storage of its own, where the chart offers none to share (item 30;
    // the wording approved 2026-11-17).
    let stored = shapes(true);
    let chosen = template_for(&asked, false, &stored).unwrap();
    assert!(chosen.claim.is_some(), "the request's plugin keeps storage");
    let refused = shards_template(chosen, &asked, &stored).unwrap_err();
    assert_eq!(
        refused,
        "snaptrade-1 was asked shards or a standby pod, and its plugin keeps storage: this \
         deployment's chart offers no shared storage (pluginStorage.shared.storageClassName)"
    );
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
    // A shape that runs its pods under any account but the instance's own
    // (item 31): the members', which is the stores' alone.
    let mut members = several_pods();
    members["spec"]["template"]["spec"]["serviceAccountName"] =
        "check-meridian-runtime-member".into();
    let refused = with_shards(members, &asked).unwrap_err();
    assert!(refused.contains("not the instance's own"), "{refused}");
    // A template that tells its sidecar no count is not the shards shape.
    let refused = with_shards(pod(), &asked).unwrap_err();
    assert!(refused.contains("MERIDIAN_PLUGIN_SHARDS"), "{refused}");
}

// ── Item 31: an instance's own account ──────────────────────────────────────

fn sharded(shards: u32, standby: bool) -> CreatePluginRequest {
    CreatePluginRequest {
        shards,
        standby,
        ..request()
    }
}

fn objects(shards: u32) -> ShardsAccount {
    let asked = sharded(shards, true);
    let made = with_shards(several_pods(), &asked).unwrap();
    shards_objects(&made, &asked, "meridian", &serde_json::Map::new()).unwrap()
}

fn names(role: &serde_json::Value) -> Vec<String> {
    role["rules"][0]["resourceNames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_string())
        .collect()
}

/// Item 31 (approved 2026-11-15): an instance of several pods runs under
/// an account of its own, `<fullname>-shards-<instance>`, whose Role's one
/// rule is `get` and `update` on exactly its N shards' Leases, bound to
/// that account alone; it carries no secret, and its Leases are made free.
#[test]
fn an_instance_of_several_pods_is_given_an_account_naming_its_own_shards_leases_alone() {
    const ACCOUNT: &str = "check-meridian-runtime-shards-snaptrade-1";
    for (shards, standby, want) in [
        (2, false, vec!["snaptrade-1-shard-0", "snaptrade-1-shard-1"]),
        (0, true, vec!["snaptrade-1-shard-0"]),
        (
            3,
            true,
            vec![
                "snaptrade-1-shard-0",
                "snaptrade-1-shard-1",
                "snaptrade-1-shard-2",
            ],
        ),
    ] {
        let asked = sharded(shards, standby);
        let made = with_shards(several_pods(), &asked).unwrap();
        assert_eq!(
            made["spec"]["template"]["spec"]["serviceAccountName"],
            ACCOUNT
        );
        let kept = shards_objects(&made, &asked, "meridian", &serde_json::Map::new()).unwrap();
        assert_eq!(
            names(&kept.role),
            want,
            "{shards} shards, standby {standby}"
        );
        assert_eq!(
            kept.role["rules"],
            serde_json::json!([{
                "apiGroups": ["coordination.k8s.io"],
                "resources": ["leases"],
                "verbs": ["get", "update"],
                "resourceNames": want,
            }]),
            "one rule, and no create: RBAC cannot narrow it to a name"
        );
        assert_eq!(kept.role["metadata"]["name"], ACCOUNT);
        assert_eq!(
            kept.binding["subjects"],
            serde_json::json!([{"kind": "ServiceAccount", "name": ACCOUNT, "namespace": "meridian"}])
        );
        assert_eq!(
            kept.binding["roleRef"],
            serde_json::json!({"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": ACCOUNT})
        );
        assert_eq!(kept.account["metadata"]["name"], ACCOUNT);
        assert!(kept.account.get("secrets").is_none());
        assert!(kept.account.get("imagePullSecrets").is_none());
        assert_eq!(kept.account["automountServiceAccountToken"], false);
        let leases: Vec<&str> = kept
            .leases
            .iter()
            .map(|lease| lease["metadata"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(leases, want);
        assert!(kept
            .leases
            .iter()
            .all(|lease| lease["spec"] == serde_json::json!({})));
        // Labelled as the launcher's, for the instance; not as a sidecar.
        for object in [&kept.account, &kept.role, &kept.binding, &kept.leases[0]] {
            let labels = &object["metadata"]["labels"];
            assert_eq!(labels["meridian.dev/launched"], "true");
            assert_eq!(labels["meridian.dev/instance"], "snaptrade-1");
            assert!(labels.get("meridian.dev/component").is_none());
        }
    }
}

/// The API server's objects, in memory, as far as [`Objects`] needs them.
#[derive(Default)]
struct Cluster {
    held: std::sync::Mutex<std::collections::HashMap<(Kind, String), serde_json::Value>>,
    versions: std::sync::atomic::AtomicU64,
}

impl Cluster {
    fn of(&self, kind: Kind, name: &str) -> Option<serde_json::Value> {
        self.held
            .lock()
            .unwrap()
            .get(&(kind, name.to_string()))
            .cloned()
    }
    fn put(&self, kind: Kind, mut object: serde_json::Value) {
        let version = self
            .versions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        object["metadata"]["resourceVersion"] = version.to_string().into();
        let name = object["metadata"]["name"].as_str().unwrap().to_string();
        self.held.lock().unwrap().insert((kind, name), object);
    }
}

impl Objects for Cluster {
    async fn get(&self, kind: Kind, name: &str) -> Result<Option<serde_json::Value>, String> {
        Ok(self.of(kind, name))
    }
    async fn create(&self, kind: Kind, object: &serde_json::Value) -> Result<bool, String> {
        let name = object["metadata"]["name"].as_str().unwrap();
        if self.of(kind, name).is_some() {
            return Ok(false);
        }
        self.put(kind, object.clone());
        Ok(true)
    }
    async fn replace(&self, kind: Kind, object: &serde_json::Value) -> Result<(), String> {
        let name = object["metadata"]["name"].as_str().unwrap();
        let there = self.of(kind, name).ok_or("404")?;
        if there["metadata"]["resourceVersion"] != object["metadata"]["resourceVersion"] {
            return Err("409".into());
        }
        self.put(kind, object.clone());
        Ok(())
    }
    async fn delete(&self, kind: Kind, name: &str) -> Result<bool, String> {
        Ok(self
            .held
            .lock()
            .unwrap()
            .remove(&(kind, name.to_string()))
            .is_some())
    }
}

fn block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

/// Item 31: the Role names exactly the count launched, written again at a
/// launch at another count -- 2 to 3, and 3 to 2 -- whether stopping
/// removed it between or not; every Lease is made once and kept as it is,
/// its holder and transitions with it, and stopping never removes one.
#[test]
fn a_launch_at_another_count_writes_the_role_again_and_keeps_every_lease() {
    const ACCOUNT: &str = "check-meridian-runtime-shards-snaptrade-1";
    block(async {
        let api = Cluster::default();
        make_shards_account(&api, &objects(2)).await.unwrap();
        let role = api.of(Kind::Role, ACCOUNT).unwrap();
        assert_eq!(names(&role), ["snaptrade-1-shard-0", "snaptrade-1-shard-1"]);
        // A take of shard 0, as a sidecar writes it.
        let mut taken = api.of(Kind::Lease, "snaptrade-1-shard-0").unwrap();
        taken["spec"] = serde_json::json!({"holderIdentity": "p_1", "leaseTransitions": 4});
        api.put(Kind::Lease, taken);
        // 2 to 3 in place, as a launch finding them there.
        make_shards_account(&api, &objects(3)).await.unwrap();
        assert_eq!(
            names(&api.of(Kind::Role, ACCOUNT).unwrap()),
            [
                "snaptrade-1-shard-0",
                "snaptrade-1-shard-1",
                "snaptrade-1-shard-2"
            ]
        );
        assert_eq!(
            api.of(Kind::Lease, "snaptrade-1-shard-0").unwrap()["spec"]["leaseTransitions"],
            4,
            "a Lease there is never made again"
        );
        // Stopped: the account, its Role and binding gone; the Leases kept.
        let removed = remove_shards_account(&api, ACCOUNT).await.unwrap();
        assert_eq!(
            removed,
            [Kind::RoleBinding, Kind::Role, Kind::ServiceAccount]
        );
        for shard in 0..3 {
            assert!(api
                .of(Kind::Lease, &format!("snaptrade-1-shard-{shard}"))
                .is_some());
        }
        // 3 to 2, launched again after the stop.
        make_shards_account(&api, &objects(2)).await.unwrap();
        assert_eq!(
            names(&api.of(Kind::Role, ACCOUNT).unwrap()),
            ["snaptrade-1-shard-0", "snaptrade-1-shard-1"]
        );
        assert_eq!(
            api.of(Kind::Lease, "snaptrade-1-shard-0").unwrap()["spec"]["leaseTransitions"],
            4
        );
        assert!(api.of(Kind::Lease, "snaptrade-1-shard-2").is_some());
        // Nothing removed twice, and never what the launcher did not make.
        remove_shards_account(&api, ACCOUNT).await.unwrap();
        assert!(remove_shards_account(&api, ACCOUNT)
            .await
            .unwrap()
            .is_empty());
        let mut theirs = objects(2).role;
        theirs["metadata"]["labels"] = serde_json::json!({});
        api.put(Kind::Role, theirs);
        let refused = make_shards_account(&api, &objects(2)).await.unwrap_err();
        assert!(refused.contains("not made by the launcher"), "{refused}");
        // Its account, made before the Role was refused, is the launcher's.
        assert_eq!(
            remove_shards_account(&api, ACCOUNT).await.unwrap(),
            [Kind::ServiceAccount]
        );
        assert!(api.of(Kind::Role, ACCOUNT).is_some());
    });
}

/// Review c5b L5: stopping removes an instance's account, Role and binding
/// only once its pods have ended -- never while one runs, whose sidecar
/// releases its shard's Lease with that account's token as it stops -- and
/// never while a launch holds the launcher; pods that never end leave the
/// account kept, for a later launch to write over. Its Leases are kept
/// throughout.
#[test]
fn stopping_removes_an_instance_s_account_once_its_pods_have_ended() {
    const ACCOUNT: &str = "check-meridian-runtime-shards-snaptrade-1";
    let paused = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    paused.block_on(async {
        let api = Cluster::default();
        let launching = tokio::sync::Mutex::new(());
        let every = std::time::Duration::from_secs(1);
        make_shards_account(&api, &objects(2)).await.unwrap();
        // Its pods end at the fourth look; until then the account is there.
        let looks = std::sync::atomic::AtomicU32::new(0);
        let ended = || {
            let look = looks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let there = api.of(Kind::ServiceAccount, ACCOUNT).is_some();
            async move {
                assert!(there, "the account was removed before its pods ended");
                Ok(if look >= 3 { Left::Nothing } else { Left::Pods })
            }
        };
        let within = std::time::Duration::from_secs(60);
        let removed =
            remove_shards_account_once_ended(&api, ACCOUNT, &launching, ended, every, within)
                .await
                .unwrap();
        assert_eq!(
            removed,
            Removal::Removed(vec![Kind::RoleBinding, Kind::Role, Kind::ServiceAccount])
        );
        assert_eq!(looks.load(std::sync::atomic::Ordering::SeqCst), 4);
        for shard in 0..2 {
            assert!(api
                .of(Kind::Lease, &format!("snaptrade-1-shard-{shard}"))
                .is_some());
        }
        // Pods that never end: the account kept, once the time is up.
        make_shards_account(&api, &objects(2)).await.unwrap();
        let began = tokio::time::Instant::now();
        let kept = remove_shards_account_once_ended(
            &api,
            ACCOUNT,
            &launching,
            || async { Ok(Left::Pods) },
            every,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(kept, Removal::Stopping);
        assert!(began.elapsed() >= std::time::Duration::from_secs(5));
        assert!(api.of(Kind::ServiceAccount, ACCOUNT).is_some());
        // A launch holding the launcher: nothing asked, nothing removed,
        // until it lets go.
        let held = launching.lock().await;
        let removing = remove_shards_account_once_ended(
            &api,
            ACCOUNT,
            &launching,
            || async { Ok(Left::Nothing) },
            every,
            within,
        );
        tokio::pin!(removing);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(10), &mut removing)
                .await
                .is_err(),
            "removed while a launch held the launcher"
        );
        assert!(api.of(Kind::ServiceAccount, ACCOUNT).is_some());
        drop(held);
        assert!(matches!(removing.await.unwrap(), Removal::Removed(_)));
        assert!(api.of(Kind::ServiceAccount, ACCOUNT).is_none());
    });
}

/// A namespace's Deployments and pods, in memory, as far as [`Workloads`]
/// needs them: a Deployment deleted goes at once, its pods stay until
/// they are let go, as the API server keeps them under a background delete.
#[derive(Default)]
struct Pods {
    deployments: std::sync::Mutex<Vec<String>>,
    pods: std::sync::Mutex<Vec<String>>,
}

fn list(names: &[String]) -> serde_json::Value {
    serde_json::json!({"items": names.iter().map(|name| serde_json::json!({"metadata": {"name": name}})).collect::<Vec<_>>()})
}

impl Workloads for Pods {
    async fn deployments(&self, selector: &str) -> Result<serde_json::Value, String> {
        assert_eq!(
            selector,
            "meridian.dev/launched=true,meridian.dev/instance=snaptrade-1"
        );
        Ok(list(&self.deployments.lock().unwrap()))
    }
    async fn pods(&self, selector: &str) -> Result<serde_json::Value, String> {
        assert_eq!(
            selector,
            "meridian.dev/launched=true,meridian.dev/instance=snaptrade-1"
        );
        Ok(list(&self.pods.lock().unwrap()))
    }
    async fn delete_deployment(&self, name: &str) -> Result<bool, String> {
        let mut held = self.deployments.lock().unwrap();
        let there = held.iter().any(|held| held == name);
        held.retain(|held| held != name);
        Ok(there)
    }
}

/// Review c5b2 H1 (and c5b3 L3): a stop deletes the instance's Deployment
/// at once, so a launch straight after finds none; what is left is its
/// pods, still stopping, until they end -- and a launch made since is
/// what is left once it is made.
#[test]
fn a_stopped_instance_leaves_its_pods_until_they_end_and_no_deployment() {
    block(async {
        let api = Pods::default();
        *api.deployments.lock().unwrap() = vec!["r-plugin-snaptrade-1".into()];
        *api.pods.lock().unwrap() = vec![
            "r-plugin-snaptrade-1-a".into(),
            "r-plugin-snaptrade-1-b".into(),
        ];
        assert_eq!(
            left_of(&api, "snaptrade-1").await.unwrap(),
            Left::Relaunched
        );
        assert_eq!(
            stop_launched(&api, "snaptrade-1").await.unwrap(),
            ["r-plugin-snaptrade-1"]
        );
        assert!(
            api.deployments.lock().unwrap().is_empty(),
            "a launch straight after a stop finds no Deployment of it"
        );
        assert_eq!(left_of(&api, "snaptrade-1").await.unwrap(), Left::Pods);
        api.pods.lock().unwrap().clear();
        assert_eq!(left_of(&api, "snaptrade-1").await.unwrap(), Left::Nothing);
        // Launched again.
        *api.deployments.lock().unwrap() = vec!["r-plugin-snaptrade-1".into()];
        assert_eq!(
            left_of(&api, "snaptrade-1").await.unwrap(),
            Left::Relaunched
        );
    });
}

/// Review c5b3 L2: a launch of the instance made since its stop ends the
/// wait at once, the account kept as the new launch's -- no polling to the
/// end of the wait, and no warning of pods that never ended.
#[test]
fn a_relaunch_ends_the_wait_to_remove_a_stopped_instance_s_account() {
    const ACCOUNT: &str = "check-meridian-runtime-shards-snaptrade-1";
    let paused = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    paused.block_on(async {
        let api = Cluster::default();
        make_shards_account(&api, &objects(2)).await.unwrap();
        let launching = tokio::sync::Mutex::new(());
        let looks = std::sync::atomic::AtomicU32::new(0);
        let began = tokio::time::Instant::now();
        let removal = remove_shards_account_once_ended(
            &api,
            ACCOUNT,
            &launching,
            || {
                looks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(Left::Relaunched) }
            },
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(600),
        )
        .await
        .unwrap();
        assert_eq!(removal, Removal::Relaunched);
        assert_eq!(looks.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(began.elapsed(), std::time::Duration::ZERO);
        assert!(api.of(Kind::ServiceAccount, ACCOUNT).is_some());
    });
}

/// Review c5b L2: the workload-identity annotations the launcher copies
/// are the ones its admission policy admits on an instance's own account.
#[test]
fn the_identity_the_launcher_copies_is_the_identity_its_policy_admits() {
    let chart = include_str!("../../../../deploy/chart/templates/launcher.yaml");
    for key in IDENTITY_ANNOTATIONS {
        assert!(
            chart.contains(&format!("\"{key}\"")),
            "the policy admits no {key}"
        );
    }
}

/// Item 31: a bucket archive's workload identity moves onto the instance's
/// own account -- its workload-identity annotations alone, never a tool's,
/// the cluster's or anyone else's (review c5b L2) -- and the pods stay under
/// that account; one pod is run under the archive's, as before.
#[test]
fn a_bucket_archive_s_identity_moves_onto_an_instance_s_own_account() {
    let archive = serde_json::json!({"metadata": {"name": "archive-writer", "annotations": {
        "eks.amazonaws.com/role-arn": "arn:aws:iam::1:role/archive",
        "eks.amazonaws.com/sts-regional-endpoints": "true",
        "iam.gke.io/gcp-service-account": "archive@p.iam.gserviceaccount.com",
        "azure.workload.identity/client-id": "00000000-0000-0000-0000-000000000001",
        "kubectl.kubernetes.io/last-applied-configuration": "{}",
        "meta.helm.sh/release-name": "check",
        "kubernetes.io/enforce-mountable-secrets": "true",
        "argocd.argoproj.io/tracking-id": "check:/ServiceAccount:meridian/archive-writer",
        "example.com/anything": "else"
    }}});
    let identity = identity_of(&archive);
    assert_eq!(
        identity.keys().collect::<Vec<_>>(),
        [
            "azure.workload.identity/client-id",
            "eks.amazonaws.com/role-arn",
            "eks.amazonaws.com/sts-regional-endpoints",
            "iam.gke.io/gcp-service-account"
        ]
    );
    let asked = CreatePluginRequest {
        shards: 2,
        roles: vec!["data".into()],
        ..allowed()
    };
    assert_eq!(
        archive_account(Some(BUCKET_ARCHIVE), &asked).as_deref(),
        Some("archive-writer")
    );
    assert_eq!(archive_account(Some(LOCAL_ARCHIVE), &asked), None);
    assert_eq!(
        archive_account(Some(BUCKET_ARCHIVE), &sharded(2, false)),
        None
    );
    let made = with_shards(several_pods(), &asked).unwrap();
    let made = with_archive(made, Some(BUCKET_ARCHIVE), &asked, &["data".to_string()]).unwrap();
    assert_eq!(
        made["spec"]["template"]["spec"]["serviceAccountName"],
        "check-meridian-runtime-shards-snaptrade-1"
    );
    let kept = shards_objects(&made, &asked, "meridian", &identity).unwrap();
    assert_eq!(
        kept.account["metadata"]["annotations"],
        serde_json::Value::Object(identity)
    );
    // One pod: the archive's own account, as before.
    let one = with_archive(pod(), Some(BUCKET_ARCHIVE), &allowed(), &edge()).unwrap();
    assert_eq!(
        one["spec"]["template"]["spec"]["serviceAccountName"],
        "archive-writer"
    );
}

/// Item 31: what stopping removes is read from the chart's shards shape.
#[test]
fn the_account_stopping_removes_is_the_shards_shape_s_for_the_instance() {
    let shape = r#"{"spec":{"template":{"spec":{"serviceAccountName":"r-shards-__INSTANCE__","containers":[{"name":"plugin","image":"__IMAGE__"}]}}}}"#;
    assert_eq!(
        shards_account_of(Some(shape), "feed-1").as_deref(),
        Some("r-shards-feed-1")
    );
    assert_eq!(shards_account_of(None, "feed-1"), None);
    assert_eq!(
        shards_account_of(
            Some(r#"{"spec":{"template":{"spec":{"serviceAccountName":"r-member"}}}}"#),
            "feed-1"
        ),
        None
    );
}

// ── Item 30: one shared claim per instance, a folder per shard ─────────────

const COPY: &str = r#"{"apiVersion":"v1","kind":"Pod","metadata":{"name":"check-meridian-runtime-storage-copy-__INSTANCE__","labels":{"meridian.dev/component":"storage-copy","meridian.dev/instance":"__INSTANCE__","meridian.dev/launched":"true"}},"spec":{"restartPolicy":"OnFailure","automountServiceAccountToken":false,"containers":[{"name":"copy","image":"meridian-runtime:check","imagePullPolicy":"IfNotPresent","args":["meridian-launcher","copy-storage","check-meridian-runtime-storage-__INSTANCE__"],"securityContext":{"allowPrivilegeEscalation":false},"volumeMounts":[{"name":"from","mountPath":"/var/lib/meridian/copy/from","readOnly":true},{"name":"to","mountPath":"/var/lib/meridian/copy/to"}]}],"volumes":[{"name":"from","persistentVolumeClaim":{"claimName":"check-meridian-runtime-storage-__INSTANCE__","readOnly":true}},{"name":"to","persistentVolumeClaim":{"claimName":"check-meridian-runtime-shared-storage-__INSTANCE__"}}]}}"#;

fn shared_shapes() -> Shapes {
    let mut shapes = shapes(true);
    let storage = shapes.storage.as_mut().unwrap();
    storage.shards = Some("several with storage".into());
    storage.shared_claim = Some("shared claim".into());
    storage.copy = Some(COPY.into());
    shapes
}

/// Several pods of an instance keeping storage, with their volume.
fn several_with_storage() -> serde_json::Value {
    let mut made = several_pods();
    made["spec"]["template"]["spec"]["volumes"] = serde_json::json!([{
        "name": "storage",
        "persistentVolumeClaim": {"claimName": "check-meridian-runtime-shared-storage-snaptrade-1"}
    }]);
    made
}

/// The instance's own claim, as the launcher made it for `plugin`.
fn own_claim(plugin: &str) -> serde_json::Value {
    serde_json::json!({"metadata": {"name": "check-meridian-runtime-storage-snaptrade-1", "labels": {
        "meridian.dev/launched": "true", "meridian.dev/plugin": plugin,
        "meridian.dev/instance": "snaptrade-1"}}})
}

fn pod_list(pods: &[(&str, &str, &str, &str)]) -> serde_json::Value {
    serde_json::json!({"items": pods.iter().map(|(name, component, phase, claim)| serde_json::json!({
        "metadata": {"name": name, "labels": {"meridian.dev/component": component}},
        "status": {"phase": phase},
        "spec": {"volumes": [{"name": "storage", "persistentVolumeClaim": {"claimName": claim}}]}
    })).collect::<Vec<_>>()})
}

/// Item 30 (approved 2026-11-15): several pods of an instance keeping
/// storage are made from the shape mounting its shared storage, and the
/// claim made is the shared claim; one pod of an instance whose storage is
/// shared since runs as one shard on it (accepted under A, 2026-11-17),
/// and shards 1 asked alone is one pod as ever.
#[test]
fn several_pods_keeping_storage_mount_one_shared_claim() {
    let shapes = shared_shapes();
    let asked = sharded(2, true);
    let chosen = shards_template(
        template_for(&asked, false, &shapes).unwrap(),
        &asked,
        &shapes,
    );
    assert_eq!(
        chosen,
        chosen_of("several with storage", Some("shared claim"))
    );
    // A standby alone shares it too.
    let standby = sharded(0, true);
    let chosen = shards_template(
        template_for(&standby, false, &shapes).unwrap(),
        &standby,
        &shapes,
    );
    assert_eq!(
        chosen,
        chosen_of("several with storage", Some("shared claim"))
    );
    // One pod, its storage shared since: one shard on the shared claim.
    let one = one_pod_as_shard(request(), true);
    assert_eq!((one.shards, one.standby), (1, false));
    assert!(several(&one));
    let chosen = shards_template(template_for(&one, false, &shapes).unwrap(), &one, &shapes);
    assert_eq!(
        chosen,
        chosen_of("several with storage", Some("shared claim"))
    );
    let made = with_shards(several_pods(), &one).unwrap();
    assert_eq!(made["spec"]["replicas"], 1);
    // One pod as ever: none shared, or shards 1 asked alone.
    assert_eq!(one_pod_as_shard(request(), false).shards, 0);
    let asked_one = one_pod_as_shard(sharded(1, false), false);
    assert_eq!(asked_one.shards, 0);
    assert!(!several(&asked_one));
    let chosen = shards_template(
        template_for(&asked_one, false, &shapes).unwrap(),
        &asked_one,
        &shapes,
    );
    assert_eq!(chosen, chosen_of("plain with storage", Some("claim")));
}

fn chosen_of(
    workload: &'static str,
    claim: Option<&'static str>,
) -> Result<Chosen<'static>, String> {
    Ok(Chosen { workload, claim })
}

/// Item 30: before the first launch as several pods, the instance's own
/// claim -- the launcher's, for the same plugin -- is copied into the
/// shared storage's shard-0/ by the one copy pod, and every pod of the
/// instance waits for it in an init container; refused while the
/// instance's earlier pods still mount its own claim (the wording approved
/// 2026-11-17), and for another plugin's claim; none with no claim kept.
/// A copy pod running or done is kept, a failed one made again.
#[test]
fn the_instances_own_claim_is_copied_once_before_its_pods_start() {
    let cluster = Cluster::default();
    let asked = sharded(2, true);
    let none = pod_list(&[]);
    // No claim of its own: no copy, the pods unheld.
    let (made, copied) = block(copy_step(
        &cluster,
        None,
        &none,
        COPY,
        several_with_storage(),
        &asked,
    ))
    .unwrap();
    assert_eq!(copied, None);
    assert!(made["spec"]["template"]["spec"]["initContainers"].is_null());
    assert!(cluster.held.lock().unwrap().is_empty());
    // Its earlier pod still mounts it: refused, nothing made.
    let own = own_claim("snaptrade");
    let stopping = pod_list(&[(
        "check-meridian-runtime-plugin-snaptrade-1-abc",
        "sidecar",
        "Running",
        "check-meridian-runtime-storage-snaptrade-1",
    )]);
    let refused = block(copy_step(
        &cluster,
        Some(&own),
        &stopping,
        COPY,
        several_with_storage(),
        &asked,
    ))
    .unwrap_err();
    assert_eq!(
        refused,
        "snaptrade-1's storage is copied into its shared storage before it runs as several \
         pods, and its earlier pods are still stopping; launch it again once they have"
    );
    assert!(cluster.held.lock().unwrap().is_empty());
    // Another plugin's records are never copied to this one.
    assert!(block(copy_step(
        &cluster,
        Some(&own_claim("elsewhere")),
        &none,
        COPY,
        several_with_storage(),
        &asked,
    ))
    .unwrap_err()
    .contains("holds the records of elsewhere"));
    // Its earlier pods ended (or a copy pod of its own, or one done): the
    // copy pod made, and every pod held until it has copied.
    let ended = pod_list(&[
        (
            "check-meridian-runtime-plugin-snaptrade-1-abc",
            "sidecar",
            "Succeeded",
            "check-meridian-runtime-storage-snaptrade-1",
        ),
        (
            "check-meridian-runtime-storage-copy-snaptrade-1",
            "storage-copy",
            "Running",
            "check-meridian-runtime-storage-snaptrade-1",
        ),
    ]);
    let (made, copied) = block(copy_step(
        &cluster,
        Some(&own),
        &ended,
        COPY,
        several_with_storage(),
        &asked,
    ))
    .unwrap();
    assert_eq!(
        copied.as_deref(),
        Some("check-meridian-runtime-storage-snaptrade-1")
    );
    let pod = cluster
        .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
        .expect("the copy pod is made");
    assert_eq!(
        pod["spec"]["volumes"][1]["persistentVolumeClaim"]["claimName"],
        "check-meridian-runtime-shared-storage-snaptrade-1"
    );
    let gates = made["spec"]["template"]["spec"]["initContainers"]
        .as_array()
        .unwrap();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0]["name"], "storage-copied");
    assert_eq!(
        gates[0]["args"],
        serde_json::json!([
            "meridian-launcher",
            "storage-copied",
            "check-meridian-runtime-storage-snaptrade-1"
        ])
    );
    assert_eq!(
        gates[0]["volumeMounts"],
        serde_json::json!([{"name": "storage", "mountPath": "/var/lib/meridian/storage", "readOnly": true}])
    );
    // Launched again while it copies, or once it has: the pod kept.
    let mut running = pod.clone();
    running["status"]["phase"] = "Running".into();
    cluster.put(Kind::Pod, running);
    let version = cluster
        .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
        .unwrap()["metadata"]["resourceVersion"]
        .clone();
    block(copy_step(
        &cluster,
        Some(&own),
        &none,
        COPY,
        several_with_storage(),
        &asked,
    ))
    .unwrap();
    assert_eq!(
        cluster
            .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
            .unwrap()["metadata"]["resourceVersion"],
        version,
        "kept as it was"
    );
    // A failed copy is made again.
    let mut failed = pod.clone();
    failed["status"]["phase"] = "Failed".into();
    cluster.put(Kind::Pod, failed);
    block(copy_step(
        &cluster,
        Some(&own),
        &none,
        COPY,
        several_with_storage(),
        &asked,
    ))
    .unwrap();
    assert!(cluster
        .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
        .unwrap()["status"]
        .is_null());
    // A shape with no storage to wait on is refused.
    assert!(block(copy_step(
        &cluster,
        Some(&own),
        &none,
        COPY,
        several_pods(),
        &asked
    ))
    .unwrap_err()
    .contains("no volume named storage"));
}
