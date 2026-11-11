use super::*;

fn member(component: &str, member: &str, mode: &str, zone: &str) -> ComponentReport {
    ComponentReport {
        component: component.into(),
        member: member.into(),
        mode: mode.into(),
        zone: zone.into(),
        ..Default::default()
    }
}

fn pod(name: &str, component: &str, node: &str) -> Pod {
    Pod {
        name: name.into(),
        component: component.into(),
        instance: String::new(),
        node: node.into(),
    }
}

fn state(lines: &[ResilienceLine], subject: &str) -> (ResilienceState, String, Vec<String>) {
    let line = lines
        .iter()
        .find(|l| l.subject == subject)
        .unwrap_or_else(|| panic!("no line {subject:?}"));
    (
        ResilienceState::try_from(line.state).unwrap(),
        line.reason.clone(),
        line.changes.iter().map(|c| c.change.clone()).collect(),
    )
}

/// The tolerant default on one node, as a desktop runs it: one member each,
/// the brought database alone, the broker alone, no backups.
fn desktop() -> (Values, Seen) {
    let mut members = Vec::new();
    let mut pods = Vec::new();
    for (component, _) in STORES {
        let name = format!("om-{component}-0");
        members.push(member(component, &name, "writer", ""));
        pods.push(pod(&name, component, "node-a"));
    }
    pods.push(pod("om-database-0", "database", "node-a"));
    pods.push(pod("om-broker-0", "broker", "node-a"));
    pods.push(pod(
        "om-analytics-coordinator-x",
        "analytics-coordinator",
        "node-a",
    ));
    pods.push(pod("om-analytics-worker-x", "analytics-worker", "node-a"));
    let values = Values::observed(&members, &pods, 7.0);
    (
        values,
        Seen {
            members,
            pods,
            priorities: Vec::new(),
        },
    )
}

/// The HA configuration on three nodes, every member in one zone.
fn ha_in_one_zone() -> (Values, Seen) {
    let mut members = Vec::new();
    let mut pods = Vec::new();
    for (component, _) in STORES {
        for (i, (mode, node)) in [("writer", "node-a"), ("standby", "node-b")]
            .iter()
            .enumerate()
        {
            let name = format!("om-{component}-{i}");
            members.push(member(component, &name, mode, "zone-a"));
            pods.push(pod(&name, component, node));
        }
    }
    for (i, node) in ["node-a", "node-b", "node-c"].iter().enumerate() {
        pods.push(pod(&format!("om-database-{i}"), "database", node));
        pods.push(pod(&format!("om-broker-{i}"), "broker", node));
    }
    for i in 0..2 {
        pods.push(pod(
            &format!("om-analytics-coordinator-{i}"),
            "analytics-coordinator",
            "node-c",
        ));
        pods.push(pod(
            &format!("om-analytics-worker-{i}"),
            "analytics-worker",
            "node-c",
        ));
    }
    let values = Values::observed(&members, &pods, 7.0);
    (
        values,
        Seen {
            members,
            pods,
            priorities: Vec::new(),
        },
    )
}

/// A tolerant release reads met by restart where the spec's section 10 says
/// its default heals by restart, each naming the change that would meet it;
/// met where it heals without one; not met where only an admin or the HA
/// configuration heals it. Its level is Desktop, and Single node lacks the
/// backups.
#[test]
fn a_tolerant_release_reads_met_by_restart_where_the_spec_says() {
    let (values, seen) = desktop();
    let tested = Tested::compiled();
    let said = profile(&values, &seen, &tested, 1);
    let (pod_state, reason, changes) = state(&said.failures, "a pod");
    assert_eq!(pod_state, ResilienceState::MetByRestart, "{reason}");
    assert!(reason.contains("no standby for"), "{reason}");
    assert!(
        reason.contains("answering again within 2 minutes"),
        "{reason}"
    );
    assert!(
        changes.contains(&"bor.standby: 1".to_string()),
        "{changes:?}"
    );
    for subject in [
        "a torn write",
        "a database failover or restore that lost a committed tail",
    ] {
        assert_eq!(
            state(&said.failures, subject).0,
            ResilienceState::MetByRestart,
            "{subject}"
        );
    }
    for subject in [
        "memory exhausted, or one plugin flooding",
        "the Kubernetes API server unreachable",
        "a firewall or NetworkPolicy change",
    ] {
        let (s, reason, changes) = state(&said.failures, subject);
        assert_eq!(s, ResilienceState::Met, "{subject}: {reason}");
        assert!(changes.is_empty(), "a line met names no change");
    }
    let (node, reason, changes) = state(&said.failures, "a node or hypervisor lost");
    assert_eq!(node, ResilienceState::NotMet);
    assert_eq!(reason, "this deployment runs on one node");
    assert!(changes.contains(&"add a node".to_string()), "{changes:?}");
    let (zone, reason, _) = state(&said.failures, "a zone's node pool");
    assert_eq!(
        (zone, reason.as_str()),
        (ResilienceState::NotMet, "every node in one zone")
    );
    let (tier, reason, changes) = state(
        &said.tiers,
        "the database primary: the book, street and order path",
    );
    assert_eq!(tier, ResilienceState::NotMet);
    assert_eq!(reason, "the database has no synchronous standby");
    assert_eq!(changes, vec!["database.brought.standbys: 2".to_string()]);
    assert_eq!(said.level, ServiceLevel::Desktop as i32);
    assert_eq!(
        said.next_level_changes
            .first()
            .map(|c| (c.change.as_str(), c.cost.as_str())),
        Some((
            "backup.walArchive: on, with a backup schedule",
            "a backup volume and its storage"
        ))
    );
    // Every line not met names its change, and its cost.
    for l in said.failures.iter().chain(&said.tiers) {
        if l.state == ResilienceState::NotMet as i32 {
            assert!(!l.changes.is_empty(), "{} names no change", l.subject);
            assert!(
                l.changes.iter().all(|c| !c.cost.is_empty()),
                "{} names no cost",
                l.subject
            );
        }
    }
}

/// With its backups scheduled and its WAL archived, as the chart states
/// them, the same release is Single node.
#[test]
fn backups_and_the_archive_make_a_desktop_single_node() {
    let (mut values, seen) = desktop();
    values.stated = true;
    values.backup_schedule = true;
    values.wal_archive = true;
    let said = profile(&values, &seen, &Tested::compiled(), 1);
    assert_eq!(said.level, ServiceLevel::SingleNode as i32);
    assert!(said
        .next_level_changes
        .iter()
        .any(|c| c.change == "bor.standby: 1"));
}

/// An ha release with every member in one zone reads the zone class not
/// met, naming the change, and is Multi-node; every other class is met.
#[test]
fn an_ha_release_in_one_zone_reads_the_zone_class_not_met() {
    let (values, seen) = ha_in_one_zone();
    let said = profile(&values, &seen, &Tested::compiled(), 1);
    let (zone, reason, changes) = state(&said.failures, "a zone's node pool");
    assert_eq!(zone, ResilienceState::NotMet);
    assert_eq!(reason, "every node in one zone");
    assert_eq!(
        changes,
        vec!["add nodes in two more zones and set zones".to_string()]
    );
    assert_eq!(
        said.failures
            .iter()
            .find(|l| l.subject == "a zone's node pool")
            .unwrap()
            .changes[0]
            .cost,
        "two nodes or more"
    );
    for l in &said.failures {
        if l.subject != "a zone's node pool" {
            assert_eq!(
                l.state,
                ResilienceState::Met as i32,
                "{}: {}",
                l.subject,
                l.reason
            );
        }
    }
    assert_eq!(said.level, ServiceLevel::MultiNode as i32);
    assert_eq!(
        said.next_level_changes
            .iter()
            .map(|c| c.change.as_str())
            .collect::<Vec<_>>(),
        vec!["add nodes in two more zones and set zones"]
    );
    assert_eq!(
        state(&said.tiers, "a zone: the lake").0,
        ResilienceState::NotMet
    );
    assert_eq!(
        state(&said.tiers, "a component or a node: the lake").0,
        ResilienceState::Met
    );
}

/// The same ha release in three zones, its chart's zones set, is Multi-zone.
#[test]
fn an_ha_release_in_three_zones_is_multi_zone() {
    let (mut values, mut seen) = ha_in_one_zone();
    let zones = ["zone-a", "zone-b", "zone-c"];
    for (i, m) in seen.members.iter_mut().enumerate() {
        m.zone = zones[i % 3].to_string();
    }
    values.zones = zones.iter().map(|z| z.to_string()).collect();
    let said = profile(&values, &seen, &Tested::compiled(), 1);
    assert_eq!(
        state(&said.failures, "a zone's node pool").0,
        ResilienceState::Met
    );
    assert_eq!(said.level, ServiceLevel::MultiZone as i32);
    assert!(said.next_level_changes.is_empty());
}

/// A class is met only where CI tests the configuration (1047): with no
/// green run of the class in ha, the ha release reads it not met, saying
/// so.
#[test]
fn a_class_ci_does_not_test_in_that_configuration_is_not_met() {
    let (values, seen) = ha_in_one_zone();
    let tested = Tested::from_ledger(
        r#"{"entries": [{"class": "bor-writer", "profile": "ha"}, {"class": "lake-writer", "profile": "ha"}]}"#,
    );
    let said = profile(&values, &seen, &tested, 1);
    let (s, reason, _) = state(&said.failures, "a pod");
    assert_eq!(
        s,
        ResilienceState::NotMet,
        "order-plugin is not tested in ha here: {reason}"
    );
    assert!(
        reason.contains("not tested by CI in this configuration"),
        "{reason}"
    );
}

/// The tested configurations are the claims ledger's, never a hand list:
/// the ledger this build carries holds the ha zone class and the tolerant
/// pod classes it was written from.
#[test]
fn the_tested_configurations_are_the_claims_ledgers() {
    let tested = Tested::compiled();
    assert!(tested.all(&["zone"], "ha"));
    assert!(tested.all(&["bor-writer", "lake-writer"], "tolerant"));
    assert!(!tested.all(&["zone"], "tolerant"));
}

/// The chart's values as the release states them are read by their chart
/// paths.
#[test]
fn the_charts_values_read_by_their_paths() {
    let values = Values::from_json(
        r#"{"bor": {"standby": 1, "scrub": {"intervalDays": 7}}, "lake": {"standby": 1},
            "database": {"brought": {"standbys": 2}}, "broker": {"replicas": 3},
            "zones": ["a", "b", "c"], "backup": {"schedule": "0 2 * * *", "walArchive": {"enabled": true}},
            "analytics": {"coordinator": {"replicaCount": 2}, "worker": {"replicaCount": 2}}}"#,
    )
    .unwrap();
    assert!(values.stated && values.backup_schedule && values.wal_archive);
    assert_eq!(values.standby.get("bor"), Some(&1));
    assert_eq!(values.standby.get("street"), Some(&0));
    assert_eq!(
        (
            values.database_standbys,
            values.broker_replicas,
            values.zones.len()
        ),
        (2, 3, 3)
    );
    assert_eq!(values.scrub_days, 7.0);
    assert!(Values::from_json("not json").is_err());
}

/// Never blind per live data type: a second source where its priority
/// names two, standby pods where each source's instance runs two pods.
#[test]
fn never_blind_reads_the_priorities_and_the_pods() {
    let (values, mut seen) = desktop();
    seen.priorities = vec![
        SourcePriority {
            data_type: "meridian.v1.Quote".into(),
            datasets: vec!["kraken-1:ticker".into(), "coinbase-1:ticker".into()],
            ..Default::default()
        },
        SourcePriority {
            data_type: "meridian.v1.Price".into(),
            datasets: vec!["fred-1:daily".into()],
            ..Default::default()
        },
    ];
    for name in ["k-0", "k-1", "c-0", "c-1"] {
        seen.pods.push(Pod {
            name: name.into(),
            component: "sidecar".into(),
            instance: if name.starts_with('k') {
                "kraken-1"
            } else {
                "coinbase-1"
            }
            .into(),
            node: "node-a".into(),
        });
    }
    let said = profile(&values, &seen, &Tested::compiled(), 1);
    let quote = said
        .never_blind
        .iter()
        .find(|l| l.data_type == "meridian.v1.Quote")
        .unwrap();
    assert!(quote.second_source && quote.standby_pods && quote.age_and_source);
    let price = said
        .never_blind
        .iter()
        .find(|l| l.data_type == "meridian.v1.Price")
        .unwrap();
    assert!(!price.second_source && !price.standby_pods && price.age_and_source);
}

/// The Leases name each store's writer by its pod.
#[test]
fn the_leases_name_each_stores_writer() {
    let list: Value = serde_json::json!({"items": [
        {"metadata": {"name": "om-meridian-runtime-bor"}, "spec": {"holderIdentity": "om-bor-1_18a"}},
        {"metadata": {"name": "om-meridian-runtime-lake"}, "spec": {}},
        {"metadata": {"name": "something-else"}, "spec": {"holderIdentity": "x_1"}},
    ]});
    let held = lease_holders(&list);
    assert_eq!(held.get("bor").map(String::as_str), Some("om-bor-1"));
    assert_eq!(held.get("lake").map(String::as_str), Some(""));
    assert_eq!(held.len(), 2);
}

/// The history, newest first and paged, from each store's control changes:
/// a captured store's takeover, the lake's switch and finding, a priority
/// held for never blind and not shown.
#[test]
fn the_history_is_newest_first_and_paged() {
    let history = History::default();
    let took = Record::TookOver(meridian_domain::v1::WriterTookOver {
        component: "street".into(),
        member: "om-street-1".into(),
        epoch: 3,
        took_over_at_ns: 20,
        ..Default::default()
    });
    let rows = serde_json::json!([{"kind": records::TOOK_OVER, "row": records::to_json(&took)}]);
    let switch = SourceSwitched {
        data_type: "meridian.v1.Quote".into(),
        from_dataset: "kraken-1:ticker".into(),
        to_dataset: "coinbase-1:ticker".into(),
        switched_at_ns: 30,
        ..Default::default()
    };
    let finding = Record::Finding(meridian_domain::v1::IntegrityFinding {
        component: "lake".into(),
        found_at_ns: 10,
        ..Default::default()
    });
    let (kind, body) = records::to_bytes(&finding);
    {
        let mut held = history.held.lock().unwrap();
        for change in [
            meridian_changes::Change {
                kind: "rows".into(),
                body: rows.to_string().into_bytes(),
                ..Default::default()
            },
            meridian_changes::Change {
                kind: "meridian.v1.SourceSwitched".into(),
                body: switch.encode_to_vec(),
                ..Default::default()
            },
            meridian_changes::Change {
                kind: kind.into(),
                body,
                ..Default::default()
            },
            meridian_changes::Change {
                kind: "meridian.v1.SourcePriority".into(),
                body: SourcePriority {
                    data_type: "meridian.v1.Quote".into(),
                    ..Default::default()
                }
                .encode_to_vec(),
                ..Default::default()
            },
        ] {
            History::take(&mut held, &change);
        }
        held.items.sort_by_key(|(at, _)| *at);
    }
    let (first, next) = history.page(2, "").unwrap();
    let ats: Vec<i64> = first.iter().map(at_of).collect();
    assert_eq!((ats, next.as_str()), (vec![30, 20], "2"));
    let (rest, next) = history.page(2, "2").unwrap();
    assert_eq!(
        (rest.iter().map(at_of).collect::<Vec<_>>(), next.as_str()),
        (vec![10], "")
    );
    assert!(history.page(2, "x").is_err());
    assert_eq!(history.priorities().len(), 1);
}
