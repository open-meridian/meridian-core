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

/// A version installed from a registry, as the conductor names its image:
/// plugins/{listing} in the deployment's own registry, by digest (W8.10).
fn listed(listing: &str) -> CreatePluginRequest {
    let version = meridian_domain::v1::PluginVersion {
        listing: listing.into(),
        source_registry: "community".into(),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        ..Default::default()
    };
    CreatePluginRequest {
        instance_id: "ecb".into(),
        image: format!(
            "{REGISTRY}/{}@{}",
            meridian_config::plugins::repository_of(&version),
            version.image_digest
        ),
        roles: vec!["data".into()],
        ..Default::default()
    }
}

/// What Kubernetes takes as a label's value: at most 63 of letters, digits,
/// `-`, `_` and `.`, a letter or digit at each end.
fn is_label_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    let edge = |b: &u8| b.is_ascii_alphanumeric();
    bytes.len() <= 63
        && bytes.first().is_none_or(edge)
        && bytes.last().is_none_or(edge)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[test]
fn a_plugin_installed_from_a_registry_launches_as_the_instance_chosen() {
    let asked = listed("open-meridian/ecb");
    assert_eq!(
        asked.image,
        format!(
            "localhost:5000/plugins/open-meridian/ecb@sha256:{}",
            "a".repeat(64)
        )
    );
    assert_eq!(checked(&asked, REGISTRY), Ok(()));
    let made = manifest(TEMPLATE, &asked).unwrap();
    assert_eq!(made["metadata"]["name"], "r-plugin-ecb");
    assert_eq!(
        made["spec"]["template"]["spec"]["containers"][1]["image"],
        asked.image
    );
    // A listing's parts may start with a digit, as the catalogue takes them.
    assert_eq!(checked(&listed("0x/1inch"), REGISTRY), Ok(()));
}

#[test]
fn the_instance_stays_a_bare_name_and_the_image_stays_under_plugins() {
    let digest = "a".repeat(64);
    let cases: Vec<(&str, CreatePluginRequest)> = vec![
        (
            "an instance named as a listing",
            CreatePluginRequest {
                instance_id: "open-meridian/ecb".into(),
                ..listed("open-meridian/ecb")
            },
        ),
        (
            "a path deeper than organisation/plugin",
            CreatePluginRequest {
                image: format!("localhost:5000/plugins/a/b/c@sha256:{digest}"),
                ..request()
            },
        ),
        (
            "a path that climbs out of plugins/",
            CreatePluginRequest {
                image: format!("localhost:5000/plugins/../ecb@sha256:{digest}"),
                ..request()
            },
        ),
        (
            "an organisation with no plugin",
            CreatePluginRequest {
                image: format!("localhost:5000/plugins/open-meridian/@sha256:{digest}"),
                ..request()
            },
        ),
        (
            "a plugin with no organisation",
            CreatePluginRequest {
                image: format!("localhost:5000/plugins//ecb@sha256:{digest}"),
                ..request()
            },
        ),
        (
            "a listing that is not lower case",
            CreatePluginRequest {
                image: format!("localhost:5000/plugins/Open-Meridian/ecb@sha256:{digest}"),
                ..request()
            },
        ),
        (
            "a listing that could break out of the template",
            CreatePluginRequest {
                image: format!("localhost:5000/plugins/open-meridian/e\"cb@sha256:{digest}"),
                ..request()
            },
        ),
    ];
    for (case, asked) in cases {
        assert!(checked(&asked, REGISTRY).is_err(), "{case}");
    }
}

#[test]
fn a_registry_plugins_storage_is_labelled_with_its_listing_as_a_label_takes_it() {
    let asked = listed("open-meridian/ecb");
    let made = claim(CLAIM, &asked).unwrap();
    assert_eq!(made["metadata"]["name"], "r-storage-ecb");
    let label = made["metadata"]["labels"][PLUGIN_LABEL].as_str().unwrap();
    assert!(is_label_value(label), "{label}");
    assert_eq!(label, "open-meridian.ecb");
    // Kept, it is mounted again by the same listing, at a newer version too.
    assert_eq!(reusable(&made, &asked), Ok(()));
    let newer = CreatePluginRequest {
        image: asked.image.replace(&"a".repeat(64), &"c".repeat(64)),
        ..asked.clone()
    };
    assert_eq!(reusable(&made, &newer), Ok(()));
    // And never by another organisation's plugin of that name, nor by a
    // local plugin of that name, nor theirs by it.
    let anothers = listed("acme/ecb");
    let refused = reusable(&made, &anothers).unwrap_err();
    assert!(
        refused.contains("holds the records of open-meridian/ecb, not acme/ecb"),
        "{refused}"
    );
    let local = CreatePluginRequest {
        image: format!("localhost:5000/plugins/ecb@sha256:{}", "a".repeat(64)),
        ..asked.clone()
    };
    let refused = reusable(&made, &local).unwrap_err();
    assert!(
        refused.contains("holds the records of open-meridian/ecb, not ecb"),
        "{refused}"
    );
    let locals = claim(CLAIM, &local).unwrap();
    assert_eq!(locals["metadata"]["labels"][PLUGIN_LABEL], "ecb");
    let refused = reusable(&locals, &asked).unwrap_err();
    assert!(
        refused.contains("holds the records of ecb, not open-meridian/ecb"),
        "{refused}"
    );
}

#[test]
fn a_listing_longer_than_a_label_holds_is_refused_its_storage_and_said_why() {
    // Each part of a listing is at most 39, so the two may pass a label's 63.
    let long = format!("{}/{}", "o".repeat(39), "p".repeat(39));
    let asked = listed(&long);
    assert_eq!(checked(&asked, REGISTRY), Ok(()));
    let refused = claim(CLAIM, &asked).unwrap_err();
    assert!(refused.contains("63"), "{refused}");
    assert!(refused.contains(&long), "{refused}");
    // Refused where its template is chosen, before anything is made; and
    // launched where it keeps no storage.
    let refused = template_for(&asked, false, &shapes(true)).unwrap_err();
    assert!(refused.contains("63"), "{refused}");
    assert_eq!(
        template_for(&asked, false, &shapes(false)),
        chosen("plain", None)
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
        // The plugin's container is told the same count, and nothing else
        // (PO item 73, 2026-11-17).
        assert_eq!(
            made["spec"]["template"]["spec"]["containers"][1]["env"],
            serde_json::json!([{"name": "MERIDIAN_PLUGIN_SHARDS", "value": told}])
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
/// A delete removes the object at once, unless `on_a_lost_node`: then a pod
/// is kept, Terminating, as a real API server keeps one whose kubelet never
/// confirms it stopped (review c5c86 M4).
#[derive(Default)]
struct Cluster {
    held: std::sync::Mutex<std::collections::HashMap<(Kind, String), serde_json::Value>>,
    versions: std::sync::atomic::AtomicU64,
    on_a_lost_node: bool,
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
        let mut held = self.held.lock().unwrap();
        let key = (kind, name.to_string());
        if self.on_a_lost_node && kind == Kind::Pod {
            return Ok(match held.get_mut(&key) {
                Some(pod) => {
                    if pod["metadata"]["deletionTimestamp"].is_null() {
                        pod["metadata"]["deletionTimestamp"] = "2026-11-17T12:00:00Z".into();
                    }
                    true
                }
                None => false,
            });
        }
        Ok(held.remove(&key).is_some())
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

/// The chart's copy pod as the ha profile renders it (templates/_helpers.tpl,
/// storageCopy); chart-check holds it equal (tools/chart_shards.py
/// copy-fixture, review c5c86 M4).
const COPY: &str = r#"{"apiVersion":"v1","kind":"Pod","metadata":{"labels":{"app.kubernetes.io/instance":"check","app.kubernetes.io/managed-by":"Helm","app.kubernetes.io/name":"meridian-runtime","app.kubernetes.io/version":"0.1.0","meridian.dev/component":"storage-copy","meridian.dev/instance":"__INSTANCE__","meridian.dev/launched":"true"},"name":"check-meridian-runtime-storage-copy-__INSTANCE__"},"spec":{"automountServiceAccountToken":false,"containers":[{"command":["meridian-storage","copy","check-meridian-runtime-storage-__INSTANCE__"],"image":"__IMAGE__","imagePullPolicy":"IfNotPresent","name":"copy","resources":{"limits":{"memory":"256Mi"},"requests":{"cpu":"50m","memory":"64Mi"}},"securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"readOnlyRootFilesystem":true,"runAsNonRoot":true},"volumeMounts":[{"mountPath":"/var/lib/meridian/copy/from","name":"from","readOnly":true},{"mountPath":"/var/lib/meridian/copy/to","name":"to"}]}],"restartPolicy":"Never","securityContext":{"fsGroup":65532,"fsGroupChangePolicy":"OnRootMismatch","seccompProfile":{"type":"RuntimeDefault"}},"volumes":[{"name":"from","persistentVolumeClaim":{"claimName":"check-meridian-runtime-storage-__INSTANCE__","readOnly":true}},{"name":"to","persistentVolumeClaim":{"claimName":"check-meridian-runtime-shared-storage-__INSTANCE__"}}]}}"#;

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

/// The moment the copy tests launch at: 2026-11-17T12:00:00Z.
const NOW: i64 = 1_794_916_800_000_000_000;

/// The instance's copy pod as the API server holds it: made at `made`, in
/// `phase`, with `status` beside it.
fn copy_pod_held(made: &str, phase: &str, status: serde_json::Value) -> serde_json::Value {
    let mut pod = copy_pod(COPY, &sharded(2, true)).unwrap();
    pod["metadata"]["creationTimestamp"] = made.into();
    pod["status"] = status;
    pod["status"]["phase"] = phase.into();
    pod
}

/// The instance's own claim, as the launcher made it for `plugin`.
fn own_claim(plugin: &str) -> serde_json::Value {
    serde_json::json!({"metadata": {"name": "check-meridian-runtime-storage-snaptrade-1", "labels": {
        "meridian.dev/launched": "true", "meridian.dev/plugin": plugin,
        "meridian.dev/instance": "snaptrade-1"}}})
}

/// The instance's shared claim as the API server holds it, made at
/// `since`, ReadWriteMany.
fn shared_claim(since: &str) -> serde_json::Value {
    serde_json::json!({"metadata": {"name": "check-meridian-runtime-shared-storage-snaptrade-1",
        "creationTimestamp": since, "labels": {"meridian.dev/launched": "true",
        "meridian.dev/plugin": "snaptrade", "meridian.dev/instance": "snaptrade-1"}},
        "spec": {"accessModes": ["ReadWriteMany"]}})
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
    let shared = shared_claim("2026-11-17T10:00:00Z");
    let none = pod_list(&[]);
    // No claim of its own: no copy, the pods unheld.
    let (made, copied) = block(copy_step(
        &cluster,
        None,
        &none,
        COPY,
        several_with_storage(),
        &asked,
        &shared,
        false,
        NOW,
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
        &shared,
        false,
        NOW,
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
        &shared,
        false,
        NOW,
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
        &shared,
        true,
        NOW,
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
    // Item 86 (PO, 2026-11-17): the copy and the wait are the SDK's, run in
    // the instance's own plugin image as its own user; the command's name is
    // the PO's item 88.
    assert_eq!(pod["spec"]["containers"][0]["image"], asked.image.as_str());
    assert_eq!(gates[0]["image"], asked.image.as_str());
    assert_eq!(
        gates[0]["command"],
        serde_json::json!([
            "meridian-storage",
            "copied",
            "check-meridian-runtime-storage-snaptrade-1"
        ])
    );
    assert!(gates[0]["args"].is_null());
    // The gate is confined as the chart's copy container is (review c5c86
    // M4: COPY is the chart's render, which chart-check holds it to).
    let box_ = &pod["spec"]["containers"][0]["securityContext"];
    assert_eq!(gates[0]["securityContext"], *box_);
    assert_eq!(box_["runAsNonRoot"], true);
    assert_eq!(box_["readOnlyRootFilesystem"], true);
    assert_eq!(box_["capabilities"]["drop"], serde_json::json!(["ALL"]));
    assert_eq!(
        gates[0]["volumeMounts"],
        serde_json::json!([{"name": "storage", "mountPath": "/var/lib/meridian/storage", "readOnly": true}])
    );
    // Launched again while it copies, or once it has: the pod kept.
    let mut running = pod.clone();
    running["status"]["phase"] = "Running".into();
    running["metadata"]["creationTimestamp"] = "2026-11-17T10:00:05Z".into();
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
        &shared,
        false,
        NOW,
    ))
    .unwrap();
    assert_eq!(
        cluster
            .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
            .unwrap()["metadata"]["resourceVersion"],
        version,
        "kept as it was"
    );
    // Once it has copied into the shared claim there: kept, done, and this
    // launch copies none, so the copy's record is the launch's that copied
    // (review c5c L6).
    let mut done = pod.clone();
    done["status"]["phase"] = "Succeeded".into();
    done["status"]["containerStatuses"] =
        serde_json::json!([{"state": {"terminated": {"exitCode": 0, "message": "copied"}}}]);
    done["metadata"]["creationTimestamp"] = "2026-11-17T10:00:05Z".into();
    cluster.put(Kind::Pod, done);
    let version = cluster
        .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
        .unwrap()["metadata"]["resourceVersion"]
        .clone();
    let (made, copied) = block(copy_step(
        &cluster,
        Some(&own),
        &none,
        COPY,
        several_with_storage(),
        &asked,
        &shared,
        false,
        NOW,
    ))
    .unwrap();
    assert_eq!(copied, None);
    assert_eq!(
        made["spec"]["template"]["spec"]["initContainers"][0]["name"], "storage-copied",
        "its pods still wait on the marker, there at once"
    );
    assert_eq!(
        cluster
            .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
            .unwrap()["metadata"]["resourceVersion"],
        version,
        "kept as it was"
    );
    // Done into a shared claim since removed and made again: stale, so not
    // known (review c5c89 M5) -- made again to read the marker, and this
    // launch reports no copy of its own.
    let (_, copied) = block(copy_step(
        &cluster,
        Some(&own),
        &none,
        COPY,
        several_with_storage(),
        &asked,
        &shared_claim("2026-11-17T11:00:00Z"),
        false,
        NOW,
    ))
    .unwrap();
    assert_eq!(copied, None);
    let remade = cluster
        .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
        .unwrap();
    assert!(remade["status"].is_null());
    assert!(remade["metadata"]["annotations"]["meridian.dev/storage-unmarked"].is_null());
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
        &shared,
        false,
        NOW,
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
        &asked,
        &shared,
        false,
        NOW,
    ))
    .unwrap_err()
    .contains("no volume named storage"));
}

const OWN: &str = r#"{"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"check-meridian-runtime-storage-__INSTANCE__","labels":{"meridian.dev/instance":"__INSTANCE__","meridian.dev/launched":"true","meridian.dev/plugin":"__PLUGIN__"}},"spec":{"accessModes":["ReadWriteOnce"]}}"#;
const SHARED: &str = r#"{"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"check-meridian-runtime-shared-storage-__INSTANCE__","labels":{"meridian.dev/instance":"__INSTANCE__","meridian.dev/launched":"true","meridian.dev/plugin":"__PLUGIN__"}},"spec":{"accessModes":["ReadWriteMany"],"storageClassName":"files"}}"#;

/// Review c5c L8, L4 and L5: the launcher's storage and copy, as it runs
/// them for a launch. One pod: its own claim made, nothing copied. Several,
/// no own claim kept: the shared claim made, nothing copied. Several with
/// its own claim kept: the shared claim and the copy pod made, that claim
/// said. A refusal of the copy comes before any claim is made, so the
/// instance is left as it was (L4); a shared claim kept that was made for
/// one node is refused where the chart now asks it shared across nodes
/// (L5), nothing made.
#[test]
fn a_launch_s_storage_and_copy_run_in_order() {
    let mut shapes = shared_shapes();
    shapes.storage.as_mut().unwrap().claim = OWN.into();
    let none = pod_list(&[]);
    let own_name = "check-meridian-runtime-storage-snaptrade-1";
    let shared_name = "check-meridian-runtime-shared-storage-snaptrade-1";
    // One pod: its own claim, no copy.
    let cluster = Cluster::default();
    let (made, copied) = block(storage_and_copy(
        &cluster,
        &shapes,
        OWN,
        &none,
        several_with_storage(),
        &request(),
        NOW,
    ))
    .unwrap();
    assert_eq!(copied, None);
    assert!(made["spec"]["template"]["spec"]["initContainers"].is_null());
    assert!(cluster.of(Kind::PersistentVolumeClaim, own_name).is_some());
    assert!(cluster
        .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
        .is_none());
    // Several, no own claim: the shared claim, no copy.
    let asked = sharded(2, true);
    let cluster = Cluster::default();
    let (_, copied) = block(storage_and_copy(
        &cluster,
        &shapes,
        SHARED,
        &none,
        several_with_storage(),
        &asked,
        NOW,
    ))
    .unwrap();
    assert_eq!(copied, None);
    assert!(cluster
        .of(Kind::PersistentVolumeClaim, shared_name)
        .is_some());
    assert!(cluster.of(Kind::PersistentVolumeClaim, own_name).is_none());
    // Several, its own claim kept, an earlier pod still on it: refused,
    // and its storage not shared yet (L4).
    let cluster = Cluster::default();
    cluster.put(Kind::PersistentVolumeClaim, claim(OWN, &asked).unwrap());
    let stopping = pod_list(&[(
        "check-meridian-runtime-plugin-snaptrade-1-abc",
        "sidecar",
        "Running",
        own_name,
    )]);
    let refused = block(storage_and_copy(
        &cluster,
        &shapes,
        SHARED,
        &stopping,
        several_with_storage(),
        &asked,
        NOW,
    ))
    .unwrap_err();
    assert_eq!(refused, still_stopping_said("snaptrade-1"));
    assert!(
        cluster
            .of(Kind::PersistentVolumeClaim, shared_name)
            .is_none(),
        "no shared claim made by a refused launch"
    );
    // Once they have ended: the shared claim and the copy pod made.
    let (made, copied) = block(storage_and_copy(
        &cluster,
        &shapes,
        SHARED,
        &none,
        several_with_storage(),
        &asked,
        NOW,
    ))
    .unwrap();
    assert_eq!(copied.as_deref(), Some(own_name));
    assert_eq!(
        made["spec"]["template"]["spec"]["initContainers"][0]["name"],
        "storage-copied"
    );
    assert!(cluster
        .of(Kind::PersistentVolumeClaim, shared_name)
        .is_some());
    assert!(cluster
        .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
        .is_some());
    // A shared claim kept from one node, ReadWriteOnce, where the chart now
    // names a class: refused, the copy pod not made (L5).
    let cluster = Cluster::default();
    cluster.put(Kind::PersistentVolumeClaim, claim(OWN, &asked).unwrap());
    let mut one_node = claim(SHARED, &asked).unwrap();
    one_node["spec"] = serde_json::json!({"accessModes": ["ReadWriteOnce"]});
    cluster.put(Kind::PersistentVolumeClaim, one_node);
    let refused = block(storage_and_copy(
        &cluster,
        &shapes,
        SHARED,
        &none,
        several_with_storage(),
        &asked,
        NOW,
    ))
    .unwrap_err();
    assert_eq!(
        refused,
        not_shareable_said("snaptrade-1", shared_name, "ReadWriteOnce", "ReadWriteMany")
    );
    assert!(cluster
        .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
        .is_none());
    // A shared claim kept ReadWriteMany where the chart now asks
    // ReadWriteOnce (one node, no class named) mounts as it is: kept, not
    // refused (review c5c2fold L4; item 81 is the other way round).
    let cluster = Cluster::default();
    cluster.put(Kind::PersistentVolumeClaim, claim(OWN, &asked).unwrap());
    cluster.put(Kind::PersistentVolumeClaim, claim(SHARED, &asked).unwrap());
    let one_node = SHARED.replace(
        "\"ReadWriteMany\"],\"storageClassName\":\"files\"",
        "\"ReadWriteOnce\"]",
    );
    assert!(one_node.contains("ReadWriteOnce"));
    let (_, copied) = block(storage_and_copy(
        &cluster,
        &shapes,
        &one_node,
        &none,
        several_with_storage(),
        &asked,
        NOW,
    ))
    .unwrap();
    // Kept, so its copy pod is made to read the marker, which an earlier
    // copy may have written: no copy reported (item 89, review c5c86 L3).
    assert_eq!(copied, None);
    assert_eq!(
        cluster
            .of(Kind::PersistentVolumeClaim, shared_name)
            .unwrap()["spec"]["accessModes"],
        serde_json::json!(["ReadWriteMany"])
    );
}

/// PO items 80 and 83 (2026-11-17; review c5c L9 and L4): with its own
/// claim kept, an instance of several pods is refused where the chart
/// renders no copy pod, nothing made; and an instance whose storage is
/// shared since is refused even one pod where the chart renders no shape
/// mounting shared storage, rather than run on its own claim without its
/// shards' records. Where the chart renders it, its storage is shared.
#[test]
fn shared_storage_is_never_left_behind_by_a_chart_without_its_shapes() {
    let asked = sharded(2, true);
    let own_name = "check-meridian-runtime-storage-snaptrade-1";
    let mut no_copy = shared_shapes();
    no_copy.storage.as_mut().unwrap().claim = OWN.into();
    no_copy.storage.as_mut().unwrap().copy = None;
    let cluster = Cluster::default();
    cluster.put(Kind::PersistentVolumeClaim, claim(OWN, &asked).unwrap());
    let refused = block(storage_and_copy(
        &cluster,
        &no_copy,
        SHARED,
        &pod_list(&[]),
        several_with_storage(),
        &asked,
        NOW,
    ))
    .unwrap_err();
    assert_eq!(refused, no_copy_pod_said("snaptrade-1", own_name));
    assert_eq!(
        refused,
        "snaptrade-1 was asked shards or a standby pod, and its own storage \
         check-meridian-runtime-storage-snaptrade-1 is kept: this deployment's chart renders no \
         pod to copy it into its shared storage (pluginStorage.shared), so it is not launched \
         as several"
    );
    assert!(cluster
        .of(
            Kind::PersistentVolumeClaim,
            "check-meridian-runtime-shared-storage-snaptrade-1"
        )
        .is_none());
    // Its storage shared since: shared, where the chart renders it ...
    let mut full = shared_shapes();
    full.storage.as_mut().unwrap().claim = OWN.into();
    full.storage.as_mut().unwrap().shared_claim = Some(SHARED.into());
    let one = request();
    let cluster = Cluster::default();
    assert_eq!(
        block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
        Ok(false)
    );
    cluster.put(Kind::PersistentVolumeClaim, claim(SHARED, &one).unwrap());
    assert_eq!(
        block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
        Ok(true)
    );
    // ... and refused where it renders none.
    let mut plain = shapes(true);
    plain.storage.as_mut().unwrap().claim = OWN.into();
    let refused = block(shared_kept(&cluster, &plain, &one, &pod_list(&[]), NOW)).unwrap_err();
    assert_eq!(
        refused,
        "snaptrade-1's storage is shared and kept, and this deployment's chart renders no pod \
         that mounts shared storage (pluginStorage.shared), so it is not launched"
    );
    // Never shared: launched on its own claim as ever.
    assert_eq!(
        block(shared_kept(
            &Cluster::default(),
            &plain,
            &one,
            &pod_list(&[]),
            NOW
        )),
        Ok(false)
    );
}

/// Review c5c2fold M1: a copy pod Pending past the copy's wait
/// (unschedulable, say) is made again at the next launch, as the wait's
/// message says; one Pending within the wait, or Running however long, is
/// kept. One on a lost node -- Unknown, or being removed -- stays
/// Terminating on a real API server, so is not made again (review c5c86
/// M2, M4): the launch is refused, saying so.
#[test]
fn a_copy_pod_that_never_ends_is_made_again_after_the_wait() {
    let asked = sharded(2, true);
    let own = own_claim("snaptrade");
    let shared = shared_claim("2026-11-17T10:00:00Z");
    let name = "check-meridian-runtime-storage-copy-snaptrade-1";
    let unschedulable = serde_json::json!({"conditions": [{"type": "PodScheduled",
        "status": "False", "reason": "Unschedulable"}]});
    let step = |cluster: &Cluster| {
        block(copy_step(
            cluster,
            Some(&own),
            &pod_list(&[]),
            COPY,
            several_with_storage(),
            &asked,
            &shared,
            false,
            NOW,
        ))
    };
    // Kept: Pending ten minutes, within the wait; Running an hour, a copy
    // of much storage.
    for (made, phase) in [
        ("2026-11-17T11:50:00Z", "Pending"),
        ("2026-11-17T11:00:00Z", "Running"),
    ] {
        let cluster = Cluster::default();
        cluster.put(Kind::Pod, copy_pod_held(made, phase, unschedulable.clone()));
        let (_, copied) = step(&cluster).unwrap();
        // Reported by the launch that made it (item 89, review c5c86 L3).
        assert_eq!(copied, None);
        assert_eq!(
            cluster.of(Kind::Pod, name).unwrap()["status"]["phase"],
            phase,
            "{phase} since {made}: kept"
        );
    }
    // Made again: Pending an hour, past the wait, on no node, which the
    // API server removes at once.
    let cluster = Cluster::default();
    cluster.put(
        Kind::Pod,
        copy_pod_held("2026-11-17T11:00:00Z", "Pending", unschedulable.clone()),
    );
    let (_, copied) = step(&cluster).unwrap();
    // Made again to read the marker: not known to be the first to write it.
    assert_eq!(copied, None);
    assert!(
        cluster.of(Kind::Pod, name).unwrap()["status"].is_null(),
        "Pending past the wait: made again"
    );
    // On a lost node -- Unknown, or being removed -- the API server keeps
    // it Terminating, so it is not made again: the launch is refused,
    // saying so (review c5c86 M2, M4). What becomes of it is held for the
    // product owner's ruling.
    let mut removing = copy_pod_held("2026-11-17T11:55:00Z", "Running", serde_json::json!({}));
    removing["metadata"]["deletionTimestamp"] = "2026-11-17T11:58:00Z".into();
    for (case, held) in [
        (
            "Unknown",
            copy_pod_held("2026-11-17T11:55:00Z", "Unknown", serde_json::json!({})),
        ),
        ("being removed", removing),
    ] {
        let cluster = Cluster {
            on_a_lost_node: true,
            ..Cluster::default()
        };
        cluster.put(Kind::Pod, held);
        let said = step(&cluster).unwrap_err();
        assert!(said.contains("still being removed"), "{case}: {said}");
        let kept = cluster.of(Kind::Pod, name).unwrap();
        assert!(!kept["status"].is_null(), "{case}: not made again");
        assert!(!kept["metadata"]["deletionTimestamp"].is_null(), "{case}");
    }
}

/// Review c5c2fold M2: a copy that never succeeds leaves the instance as it
/// was. Where its own claim is kept and its copy pod failed saying
/// `unmarked`, its storage is not shared yet: one pod runs on its own claim,
/// ungated, and no copy pod is made for it. Shared where the copy pod ended
/// `copied`, runs or waits within the wait; and where no own claim is kept.
/// What is not known -- one made unmarked and Pending past the wait among
/// it (review c5c89 M2, M5) -- is read
/// ([`item_89_the_copy_pod_says_how_its_copy_ended`]).
#[test]
fn a_copy_that_never_succeeds_leaves_the_instance_on_its_own_claim() {
    let mut full = shared_shapes();
    full.storage.as_mut().unwrap().claim = OWN.into();
    full.storage.as_mut().unwrap().shared_claim = Some(SHARED.into());
    let one = request();
    let name = "check-meridian-runtime-storage-copy-snaptrade-1";
    let own_name = "check-meridian-runtime-storage-snaptrade-1";
    let with = |pod: Option<serde_json::Value>| {
        let cluster = Cluster::default();
        cluster.put(Kind::PersistentVolumeClaim, claim(OWN, &one).unwrap());
        let mut shared = claim(SHARED, &one).unwrap();
        shared["metadata"]["creationTimestamp"] = "2026-11-17T10:00:00Z".into();
        cluster.put(Kind::PersistentVolumeClaim, shared);
        if let Some(pod) = pod {
            cluster.put(Kind::Pod, pod);
        }
        cluster
    };
    let none = serde_json::json!({});
    for (case, pod, shared) in [
        (
            "failed unmarked",
            Some(copy_pod_said("2026-11-17T10:00:01Z", "Failed", "unmarked")),
            false,
        ),
        (
            "copied",
            Some(copy_pod_said("2026-11-17T10:00:01Z", "Succeeded", "copied")),
            true,
        ),
        (
            "copying",
            Some(copy_pod_held(
                "2026-11-17T11:00:00Z",
                "Running",
                none.clone(),
            )),
            true,
        ),
        (
            "waiting within the wait",
            Some(copy_pod_held(
                "2026-11-17T11:55:00Z",
                "Pending",
                none.clone(),
            )),
            true,
        ),
    ] {
        let cluster = with(pod);
        assert_eq!(
            block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
            Ok(shared),
            "{case}"
        );
    }
    // Failed unmarked: one pod on its own claim, ungated, the failed pod
    // left as it was.
    let cluster = with(Some(copy_pod_said(
        "2026-11-17T10:00:01Z",
        "Failed",
        "unmarked",
    )));
    let version = cluster.of(Kind::Pod, name).unwrap()["metadata"]["resourceVersion"].clone();
    let shared = block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)).unwrap();
    let asked = one_pod_as_shard(one.clone(), shared);
    assert!(!several(&asked));
    let (made, copied) = block(storage_and_copy(
        &cluster,
        &full,
        OWN,
        &pod_list(&[]),
        several_with_storage(),
        &asked,
        NOW,
    ))
    .unwrap();
    assert_eq!(copied, None);
    assert!(made["spec"]["template"]["spec"]["initContainers"].is_null());
    assert_eq!(
        cluster.of(Kind::Pod, name).unwrap()["metadata"]["resourceVersion"],
        version,
        "no copy pod made for one pod on its own claim"
    );
    assert!(cluster.of(Kind::PersistentVolumeClaim, own_name).is_some());
    // Made unmarked and Pending past the wait: not known (review c5c89 M2,
    // M5), so made again to read the marker -- the old one gone first --
    // and the launch refused meanwhile.
    let cluster = with(Some(unmarked_pending()));
    assert_eq!(
        block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
        Err(reading_copy_said("snaptrade-1"))
    );
    assert!(cluster.of(Kind::Pod, name).unwrap()["status"].is_null());
    // No own claim kept: shared, whatever its copy pod.
    let cluster = Cluster::default();
    cluster.put(Kind::PersistentVolumeClaim, claim(SHARED, &one).unwrap());
    cluster.put(
        Kind::Pod,
        copy_pod_held("2026-11-17T10:00:01Z", "Failed", none),
    );
    assert_eq!(
        block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
        Ok(true)
    );
}

/// PO item 73 (2026-11-17): the plugin's container is told its instance's
/// shard count as its sidecar is, the value the launch asked written over
/// the chart's 1; one pod's is left as the chart renders it.
#[test]
fn the_plugin_is_told_its_instance_s_shard_count_as_its_sidecar_is() {
    let mut shape = several_pods();
    shape["spec"]["template"]["spec"]["containers"][1]["env"] =
        serde_json::json!([{"name": "MERIDIAN_PLUGIN_SHARDS", "value": "1"}]);
    let made = with_shards(shape.clone(), &sharded(3, true)).unwrap();
    let told = |made: &serde_json::Value, container: usize| -> Vec<serde_json::Value> {
        made["spec"]["template"]["spec"]["containers"][container]["env"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["name"] == "MERIDIAN_PLUGIN_SHARDS")
            .map(|e| e["value"].clone())
            .collect()
    };
    assert_eq!(told(&made, 0), vec![serde_json::json!("3")]);
    assert_eq!(told(&made, 1), vec![serde_json::json!("3")]);
    // A standby alone: 1, as its sidecar.
    let made = with_shards(shape.clone(), &sharded(0, true)).unwrap();
    assert_eq!(told(&made, 1), vec![serde_json::json!("1")]);
    // One pod: untouched.
    assert_eq!(
        with_shards(shape.clone(), &sharded(0, false)).unwrap(),
        shape
    );
}

/// A copy pod made where no marker was, never run, Pending past the wait.
fn unmarked_pending() -> serde_json::Value {
    let mut pod = copy_pod_held("2026-11-17T11:00:00Z", "Pending", serde_json::json!({}));
    pod["metadata"]["annotations"]["meridian.dev/storage-unmarked"] = "true".into();
    pod
}

/// A copy pod held as the API server holds it, ended saying `said` (PO
/// item 89, revised: its termination message).
fn copy_pod_said(made: &str, phase: &str, said: &str) -> serde_json::Value {
    copy_pod_held(
        made,
        phase,
        serde_json::json!({"containerStatuses": [{"name": "copy",
            "state": {"terminated": {"exitCode": i32::from(phase != "Succeeded"), "message": said}}}]}),
    )
}

/// PO item 89 (revised, 2026-11-17; review c5c86 M2, M3, L3): the launcher
/// reads how the copy ended from what the copy pod said, never from its
/// absence or its phase alone. Failed `unmarked` alone (review c5c89 M5)
/// leaves one pod on its own claim;
/// `copied` or `copied before` shares it, kept on the shared claim; anything
/// else is not known, so the copy pod is made again to read the marker and
/// the launch refused meanwhile. A launch reports a copy only where its copy
/// pod is the first to write the marker. A copy pod on a lost node is named,
/// with its node.
#[test]
fn item_89_the_copy_pod_says_how_its_copy_ended() {
    let mut full = shared_shapes();
    full.storage.as_mut().unwrap().claim = OWN.into();
    full.storage.as_mut().unwrap().shared_claim = Some(SHARED.into());
    let one = request();
    let name = "check-meridian-runtime-storage-copy-snaptrade-1";
    let own_name = "check-meridian-runtime-storage-snaptrade-1";
    let shared_name = "check-meridian-runtime-shared-storage-snaptrade-1";
    let with = |pod: Option<serde_json::Value>, lost: bool| {
        let cluster = Cluster {
            on_a_lost_node: lost,
            ..Cluster::default()
        };
        cluster.put(Kind::PersistentVolumeClaim, claim(OWN, &one).unwrap());
        let mut shared = claim(SHARED, &one).unwrap();
        shared["metadata"]["creationTimestamp"] = "2026-11-17T10:00:00Z".into();
        cluster.put(Kind::PersistentVolumeClaim, shared);
        if let Some(pod) = pod {
            cluster.put(Kind::Pod, pod);
        }
        cluster
    };
    let annotated = |cluster: &Cluster| {
        cluster
            .of(Kind::PersistentVolumeClaim, shared_name)
            .unwrap()["metadata"]["annotations"]["meridian.dev/storage-copied"]
            .clone()
    };
    // One pod. Copied, either way: shared, and kept on the shared claim.
    for said in ["copied", "copied before"] {
        let cluster = with(
            Some(copy_pod_said("2026-11-17T10:00:01Z", "Succeeded", said)),
            false,
        );
        assert_eq!(
            block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
            Ok(true),
            "{said}"
        );
        assert_eq!(
            annotated(&cluster),
            own_name,
            "{said}: kept on the shared claim"
        );
        // ... so it holds once the copy pod is gone.
        block(cluster.delete(Kind::Pod, name)).unwrap();
        assert_eq!(
            block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
            Ok(true),
            "{said}, gone"
        );
        assert!(
            cluster.of(Kind::Pod, name).is_none(),
            "{said}: no copy made again"
        );
    }
    // Unmarked: one pod on its own claim (item 83 kept: no marker, so no
    // shard ever started on the shared claim).
    let cluster = with(
        Some(copy_pod_said("2026-11-17T10:00:01Z", "Failed", "unmarked")),
        false,
    );
    assert_eq!(
        block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
        Ok(false)
    );
    assert!(annotated(&cluster).is_null());
    // A pod made where no marker was, that never ran or failed without a
    // word: not known, whatever its annotation (review c5c89 M5).
    for (phase, made, said) in [
        ("Pending", "2026-11-17T11:00:00Z", ""),
        ("Failed", "2026-11-17T10:00:01Z", ""),
    ] {
        let mut pod = copy_pod_said(made, phase, said);
        pod["metadata"]["annotations"]["meridian.dev/storage-unmarked"] = "true".into();
        let cluster = with(Some(pod), false);
        assert_eq!(
            block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)),
            Err(reading_copy_said("snaptrade-1")),
            "{phase}, made unmarked"
        );
    }
    // Not known -- none there, failed after `marking` or without a word,
    // succeeded without one, never ran -- so read: the copy pod made again,
    // the launch refused meanwhile.
    for (case, pod) in [
        ("not there", None),
        (
            "failed marking",
            Some(copy_pod_said("2026-11-17T10:00:01Z", "Failed", "marking")),
        ),
        (
            "failed, no word",
            Some(copy_pod_said("2026-11-17T10:00:01Z", "Failed", "")),
        ),
        (
            "succeeded, no word",
            Some(copy_pod_said("2026-11-17T10:00:01Z", "Succeeded", "")),
        ),
        (
            "never ran",
            Some(copy_pod_said("2026-11-17T11:00:00Z", "Pending", "")),
        ),
    ] {
        let cluster = with(pod, false);
        let said = block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)).unwrap_err();
        assert_eq!(
            said, "reading snaptrade-1's copy; launch again once it ends",
            "{case}"
        );
        let made = cluster.of(Kind::Pod, name).expect("made again");
        assert!(made["status"].is_null(), "{case}: made again");
        assert!(
            made["metadata"]["annotations"]["meridian.dev/storage-unmarked"].is_null(),
            "{case}"
        );
    }
    // On a lost node: named, with its node, and kept.
    let mut lost = copy_pod_said("2026-11-17T10:00:01Z", "Running", "");
    lost["metadata"]["deletionTimestamp"] = "2026-11-17T11:58:00Z".into();
    lost["spec"]["nodeName"] = "node-b".into();
    let cluster = with(Some(lost), true);
    let said = block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)).unwrap_err();
    assert!(said.contains(name) && said.contains("node-b"), "{said}");
    assert!(!cluster.of(Kind::Pod, name).unwrap()["status"].is_null());
    // Several pods (L3): a copy reported only where this launch's copy pod
    // is the first to write the marker -- the shared claim made now, or an
    // unmarked copy there -- and that pod made knowing it.
    let asked = sharded(2, true);
    let several = |cluster: &Cluster| {
        block(storage_and_copy(
            cluster,
            &full,
            SHARED,
            &pod_list(&[]),
            several_with_storage(),
            &asked,
            NOW,
        ))
        .map(|(_, copied)| copied)
    };
    let fresh = Cluster::default();
    fresh.put(Kind::PersistentVolumeClaim, claim(OWN, &asked).unwrap());
    assert_eq!(
        several(&fresh),
        Ok(Some(own_name.to_string())),
        "the shared claim made now"
    );
    assert_eq!(
        fresh.of(Kind::Pod, name).unwrap()["metadata"]["annotations"]
            ["meridian.dev/storage-unmarked"],
        "true"
    );
    let cluster = with(
        Some(copy_pod_said("2026-11-17T10:00:01Z", "Failed", "unmarked")),
        false,
    );
    assert_eq!(
        several(&cluster),
        Ok(Some(own_name.to_string())),
        "unmarked there"
    );
    for (case, pod) in [
        ("not there", None),
        (
            "failed marking",
            Some(copy_pod_said("2026-11-17T10:00:01Z", "Failed", "marking")),
        ),
        (
            "copied",
            Some(copy_pod_said("2026-11-17T10:00:01Z", "Succeeded", "copied")),
        ),
        (
            "copied before",
            Some(copy_pod_said(
                "2026-11-17T10:00:01Z",
                "Succeeded",
                "copied before",
            )),
        ),
    ] {
        let cluster = with(pod, false);
        assert_eq!(several(&cluster), Ok(None), "{case}: no copy reported");
    }
}

/// Review c5c89 M5: the table read exactly as PO item 89 says. Only Failed
/// saying `unmarked` is not shared. A pod that failed saying `marking`,
/// `copied` or nothing, one Pending past its wait, and one older than the
/// shared claim are not known, whatever the `storage-unmarked` annotation
/// says: it records what was true as the pod was made, and never overrides
/// what the pod said.
#[test]
fn review_c5c89_m5_only_failed_unmarked_is_not_shared() {
    let shared = shared_claim("2026-11-17T10:00:00Z");
    let marked = |mut pod: serde_json::Value| {
        pod["metadata"]["annotations"]["meridian.dev/storage-unmarked"] = "true".into();
        pod
    };
    assert_eq!(
        copy_pod_now(
            Some(&copy_pod_said("2026-11-17T10:00:01Z", "Failed", "unmarked")),
            &shared,
            NOW
        ),
        CopyPod::Unmarked
    );
    let read_otherwise = [
        (
            "made unmarked, failed saying copied",
            marked(copy_pod_said("2026-11-17T10:00:01Z", "Failed", "copied")),
        ),
        (
            "made unmarked, failed saying copied before",
            marked(copy_pod_said(
                "2026-11-17T10:00:01Z",
                "Failed",
                "copied before",
            )),
        ),
        (
            "made unmarked, failed without a word",
            marked(copy_pod_said("2026-11-17T10:00:01Z", "Failed", "")),
        ),
        ("made unmarked, Pending past the wait", unmarked_pending()),
        (
            "older than the shared claim, ended copied",
            copy_pod_said("2026-11-17T09:00:00Z", "Succeeded", "copied"),
        ),
        (
            "older than the shared claim, ended unmarked",
            copy_pod_said("2026-11-17T09:00:00Z", "Failed", "unmarked"),
        ),
    ]
    .into_iter()
    .filter(|(_, pod)| copy_pod_now(Some(pod), &shared, NOW) != CopyPod::Unknown)
    .map(|(case, pod)| format!("{case}: {:?}", copy_pod_now(Some(&pod), &shared, NOW)))
    .collect::<Vec<_>>();
    assert!(
        read_otherwise.is_empty(),
        "read otherwise than not known: {read_otherwise:?}"
    );
}

/// Review c5c89 M2: a copy pod removed is never answered "not shared" while
/// it is still there. One Pending past its wait on a node, which a real API
/// server keeps Terminating while its kubelet stops it, refuses the launch.
#[test]
fn review_c5c89_m2_a_removed_copy_pod_is_gone_before_not_shared() {
    let mut full = shared_shapes();
    full.storage.as_mut().unwrap().claim = OWN.into();
    full.storage.as_mut().unwrap().shared_claim = Some(SHARED.into());
    let one = request();
    let cluster = Cluster {
        on_a_lost_node: true,
        ..Cluster::default()
    };
    cluster.put(Kind::PersistentVolumeClaim, claim(OWN, &one).unwrap());
    let mut shared = claim(SHARED, &one).unwrap();
    shared["metadata"]["creationTimestamp"] = "2026-11-17T10:00:00Z".into();
    cluster.put(Kind::PersistentVolumeClaim, shared);
    let mut pod = unmarked_pending();
    pod["spec"]["nodeName"] = "node-b".into();
    cluster.put(Kind::Pod, pod);
    let said = block(shared_kept(&cluster, &full, &one, &pod_list(&[]), NOW)).unwrap_err();
    assert!(
        said.contains("still being removed") || said.starts_with("reading "),
        "{said}"
    );
}

/// Review c5c89 M3 (item 69 kept): a one-pod launch that reads its copy is
/// refused while the instance's earlier pods still mount its own claim,
/// and no copy pod is made under them.
#[test]
fn review_c5c89_m3_no_copy_pod_while_earlier_pods_stop() {
    let mut full = shared_shapes();
    full.storage.as_mut().unwrap().claim = OWN.into();
    full.storage.as_mut().unwrap().shared_claim = Some(SHARED.into());
    let one = request();
    let own_name = "check-meridian-runtime-storage-snaptrade-1";
    let cluster = Cluster::default();
    cluster.put(Kind::PersistentVolumeClaim, claim(OWN, &one).unwrap());
    let mut shared = claim(SHARED, &one).unwrap();
    shared["metadata"]["creationTimestamp"] = "2026-11-17T10:00:00Z".into();
    cluster.put(Kind::PersistentVolumeClaim, shared);
    let pods = pod_list(&[("snaptrade-1-abc", "sidecar", "Running", own_name)]);
    let said = block(shared_kept(&cluster, &full, &one, &pods, NOW)).unwrap_err();
    assert_eq!(said, still_stopping_said("snaptrade-1"));
    assert!(
        cluster
            .of(Kind::Pod, "check-meridian-runtime-storage-copy-snaptrade-1")
            .is_none(),
        "no copy pod made under a pod still mounting the own claim"
    );
}

/// PO item 91: the launcher tells the conductor a launch ran on its
/// instance's shared storage -- several pods keeping storage, or one pod as
/// one shard of storage shared since -- and never one on its own claim or
/// keeping none.
#[test]
fn item_91_a_launch_on_the_shared_storage_says_so() {
    let one = request();
    assert!(runs_shared(Some("shared"), &sharded(2, true)));
    assert!(runs_shared(
        Some("shared"),
        &one_pod_as_shard(one.clone(), true)
    ));
    assert!(!runs_shared(
        Some("own"),
        &one_pod_as_shard(one.clone(), false)
    ));
    assert!(!runs_shared(None, &sharded(2, true)));
}
