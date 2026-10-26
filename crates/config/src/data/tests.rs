use meridian_domain::v1::{
    PluginLaunch, PluginLaunchState, SetDatasetEntitlementRequest, SetDatasetLicenceRequest,
};
use meridian_pb::v1::{Catalogue, DatasetDeclaration, DatasetLicence, ObservationMode};

use super::*;
use crate::store::{Author, KnownPlugin, Store};
use crate::MemoryStore;

const NOW: i64 = 1_791_417_600_000_000_000;

fn ada() -> Author {
    Author {
        prepared_call_id: Default::default(),
        by: "local|ada".into(),
        delegation: "DLG-1".into(),
        client: "Claude".into(),
        note: String::new(),
    }
}

fn daily() -> Catalogue {
    Catalogue {
        datasets: vec![DatasetDeclaration {
            key: "daily".into(),
            vendor: "Coinbase".into(),
            data_types: vec!["meridian.v1.Price".into(), "meridian.v1.Bar".into()],
            modes: vec![ObservationMode::Pull as i32],
            cadence: 86_400,
            day_time_zone: "Etc/UTC".into(),
            ..Default::default()
        }],
    }
}

/// coinbase-1 reported with its catalogue, and reporting-1 beside it.
fn store() -> MemoryStore {
    let store = MemoryStore::new();
    for (instance, roles) in [("coinbase-1", "data"), ("reporting-1", "reporting")] {
        store
            .record_plugin(&KnownPlugin {
                plugin_instance_id: instance.into(),
                roles: vec![roles.into()],
                last_reported_at_ns: NOW,
            })
            .unwrap();
    }
    assert!(store.record_catalogue("coinbase-1", &daily(), NOW).unwrap());
    assert!(!store.record_catalogue("coinbase-1", &daily(), NOW).unwrap());
    store
}

#[test]
fn a_reported_catalogues_datasets_are_launched_and_a_stopped_ones_are_not() {
    let store = store();
    let snapshot = store.snapshot().unwrap();
    let launched = datasets(&snapshot);
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].dataset, "coinbase-1:daily");
    assert_eq!(launched[0].vendor, "Coinbase");
    // Launched from the catalogue once and stopped: its datasets leave.
    store
        .begin_launch(
            &PluginLaunch {
                instance_id: "coinbase-1".into(),
                name: "coinbase".into(),
                version: "0.1.0".into(),
                state: PluginLaunchState::Launched as i32,
                ..Default::default()
            },
            "",
        )
        .unwrap();
    store
        .end_launch(
            "coinbase-1",
            &crate::store::Ending::failed(NOW, "gone".into()),
        )
        .unwrap();
    assert!(datasets(&store.snapshot().unwrap()).is_empty());
}

#[test]
fn a_licence_is_set_on_a_launched_dataset_within_its_bounds_and_kept_as_its_own_record() {
    let store = store();
    let snapshot = store.snapshot().unwrap();
    let ask = |dataset: &str, licence: DatasetLicence| SetDatasetLicenceRequest {
        dataset: dataset.into(),
        licence: Some(licence),
        note: "The vendor's personal terms.".into(),
    };
    let unknown = set_licence(
        &store,
        &snapshot,
        &ask("kraken-1:daily", DatasetLicence::default()),
        &ada(),
        NOW,
    )
    .unwrap_err();
    assert!(unknown.starts_with("dataset:"), "{unknown}");
    let long = set_licence(
        &store,
        &snapshot,
        &ask(
            "coinbase-1:daily",
            DatasetLicence {
                retention_days: 36_501,
                ..Default::default()
            },
        ),
        &ada(),
        NOW,
    )
    .unwrap_err();
    assert!(long.contains("retention_days"), "{long}");
    let field = set_licence(
        &store,
        &snapshot,
        &ask(
            "coinbase-1:daily",
            DatasetLicence {
                default_fields: vec!["meridian.v1.Trade.price".into()],
                ..Default::default()
            },
        ),
        &ada(),
        NOW,
    )
    .unwrap_err();
    assert!(field.contains("default_fields[0]"), "{field}");
    let nobody = set_licence(
        &store,
        &snapshot,
        &ask("coinbase-1:daily", DatasetLicence::default()),
        &Author::default(),
        NOW,
    )
    .unwrap_err();
    assert!(nobody.contains("deployment admin"), "{nobody}");

    let kept = set_licence(
        &store,
        &snapshot,
        &ask(
            "coinbase-1:daily",
            DatasetLicence {
                kept: true,
                retention_days: 3650,
                personal_use: true,
                dataset: "something-else".into(),
                updated_by: "a plugin's word".into(),
                ..Default::default()
            },
        ),
        &ada(),
        NOW,
    )
    .unwrap();
    assert_eq!(
        kept.dataset, "coinbase-1:daily",
        "the request's, whatever the terms say"
    );
    assert_eq!(kept.updated_by, "local|ada");
    assert_eq!(kept.client_name, "Claude");
    assert_eq!(kept.note, "The vendor's personal terms.");
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.licences, vec![kept.clone()]);
    let (licences, _) = store.dataset_changes("coinbase-1:daily").unwrap();
    assert_eq!(licences.len(), 1);
    let event = configuration(&snapshot, NOW);
    assert_eq!(event.licences, vec![kept]);
    assert_eq!(event.datasets.len(), 1);
}

#[test]
fn an_entitlement_names_a_running_instance_and_entries_of_the_datasets_types() {
    let store = store();
    let snapshot = store.snapshot().unwrap();
    let ask = |instance: &str, allowed: bool, fields: &[&str]| SetDatasetEntitlementRequest {
        dataset: "coinbase-1:daily".into(),
        instance: instance.into(),
        allowed,
        fields: fields.iter().map(|f| f.to_string()).collect(),
        note: "The valuation reads it.".into(),
    };
    let stranger =
        set_entitlement(&store, &snapshot, &ask("nobody-1", true, &[]), &ada(), NOW).unwrap_err();
    assert!(stranger.starts_with("instance:"), "{stranger}");
    let wrong = set_entitlement(
        &store,
        &snapshot,
        &ask("reporting-1", true, &["meridian.v1.Bar.vwapp"]),
        &ada(),
        NOW,
    )
    .unwrap_err();
    assert!(wrong.starts_with("fields[0]:"), "{wrong}");
    let some = set_entitlement(
        &store,
        &snapshot,
        &ask(
            "reporting-1",
            true,
            &["meridian.v1.Bar.open", "meridian.v1.Bar.close"],
        ),
        &ada(),
        NOW,
    )
    .unwrap();
    assert_eq!(some.fields.len(), 2);
    let withdrawn = set_entitlement(
        &store,
        &snapshot,
        &ask("reporting-1", false, &["meridian.v1.Bar.open"]),
        &ada(),
        NOW + 1,
    )
    .unwrap();
    assert!(!withdrawn.allowed && withdrawn.fields.is_empty());
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.entitlements, vec![withdrawn]);
    let (_, changes) = store.dataset_changes("coinbase-1:daily").unwrap();
    assert_eq!(changes.len(), 2, "each change its own record");
}

#[test]
fn a_datasets_stale_ceiling_is_its_data_plugin_admins_dataset_thresholds() {
    // Contract v27 (W10.9; ruling 417): `stale_after` in whole seconds, by
    // the dataset's key; a row that does not read passed over.
    let store = store();
    let mut snapshot = store.snapshot().unwrap();
    snapshot.settings.push(crate::store::StoredSetting {
        plugin_instance_id: "coinbase-1".into(),
        name: DATASET_THRESHOLDS.into(),
        held: crate::store::Held::Plain(
            r#"[{"dataset":"daily","asset_class":"crypto","stale_after":"10"},
                {"dataset":"ticks","stale_after":"soon"}]"#
                .into(),
        ),
        set_by: "local|ada".into(),
        set_at_ns: NOW,
    });
    let launched = datasets(&snapshot);
    assert_eq!(launched[0].stale_after_ns, 10_000_000_000);
    assert_eq!(
        stale_after(&snapshot, "coinbase-1").len(),
        1,
        "the row that does not read is passed over"
    );
    assert!(stale_after(&snapshot, "kraken-1").is_empty());
}

#[test]
fn a_persons_licence_keeps_the_declared_terms_it_does_not_state() {
    // Contract v28 (W10.1; 491, 597).
    let declared = DatasetLicence {
        attribution_required: true,
        attribution_text: "Data from a vendor".into(),
        copyright_class: 2,
        redistribution: 1,
        no_more_restrictive_terms: true,
        not_affiliated_notice: "Not affiliated".into(),
        terms_version: "2026-10".into(),
        eligibility: meridian_domain::lake::eligibility_of(&["personal"], "", "", "").unwrap(),
        ..Default::default()
    };
    let stated = DatasetLicence {
        kept: true,
        retention_days: 30,
        ..Default::default()
    };
    let kept = super::kept_declared(stated, Some(&declared));
    assert!(kept.kept && kept.retention_days == 30);
    assert!(kept.attribution_required && kept.no_more_restrictive_terms);
    assert_eq!(kept.attribution_text, "Data from a vendor");
    assert_eq!((kept.copyright_class, kept.redistribution), (2, 1));
    assert_eq!(kept.not_affiliated_notice, "Not affiliated");
    assert_eq!(kept.terms_version, "2026-10");
    assert_eq!(kept.eligibility, declared.eligibility);
    // A stated eligibility stands over the declared one.
    let commercial =
        meridian_domain::lake::eligibility_of(&["commercial"], "", "1000000", "usd").unwrap();
    let stated = DatasetLicence {
        eligibility: commercial.clone(),
        ..Default::default()
    };
    assert_eq!(
        super::kept_declared(stated, Some(&declared)).eligibility,
        commercial
    );
    assert_eq!(commercial.unwrap().buyer_revenue_currency_code, "USD");
    assert!(meridian_domain::lake::eligibility_of(&[], "5", "", "").is_err());
    assert!(meridian_domain::lake::eligibility_of(&["corporate"], "", "", "").is_err());
    assert_eq!(
        meridian_domain::lake::eligibility_of(&[], "", "", "").unwrap(),
        None
    );
}
