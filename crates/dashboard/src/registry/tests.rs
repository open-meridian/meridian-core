//! The Plugins page's words and choices: the version Browse shows, the
//! update offered, a widening, a pull warning, the counts and the labels.

use meridian_domain::v1::{
    ListedVersion, PluginLaunch, PluginListing, PluginMetadata, PluginPull, PluginVersion,
    PullAcknowledgement, PullReasonKind,
};

use super::*;

fn listed(version: &str, registry: &str, roles: &[&str]) -> ListedVersion {
    ListedVersion {
        listing: "acme/fx".into(),
        version: version.into(),
        registry: registry.into(),
        schema_version: "v25".into(),
        metadata: Some(PluginMetadata {
            roles: roles.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn listing(versions: Vec<ListedVersion>) -> PluginListing {
    PluginListing {
        listing: "acme/fx".into(),
        versions,
        ..Default::default()
    }
}

fn installed(version: &str, registry: &str) -> PluginVersion {
    PluginVersion {
        metadata: Some(PluginMetadata {
            name: "acme/fx".into(),
            version: version.into(),
            ..Default::default()
        }),
        source_registry: registry.into(),
        listing: "acme/fx".into(),
        ..Default::default()
    }
}

#[test]
fn browse_shows_the_highest_offered_release_verified_first() {
    let mut pulled = listed("1.3.0", "community", &[]);
    pulled.pull = Some(PluginPull {
        pulled_at_ns: 1,
        ..Default::default()
    });
    let held = listing(vec![
        listed("1.2.0", "community", &[]),
        pulled,
        listed("2.0.0-rc.1", "community", &[]),
    ]);
    assert_eq!(
        latest(&held).unwrap().version,
        "1.2.0",
        "a release over a pre-release, never a pulled one"
    );
    let held = listing(vec![
        listed("1.2.0", "community", &[]),
        listed("1.0.0", "verified", &[]),
    ]);
    assert_eq!(latest(&held).unwrap().registry, "verified");
    let held = listing(vec![listed("2.0.0-rc.1", "community", &[])]);
    assert_eq!(
        latest(&held).unwrap().version,
        "2.0.0-rc.1",
        "a pre-release where none is released"
    );
}

#[test]
fn an_update_is_offered_from_the_source_registry_only_and_never_a_pre_release_over_a_release() {
    let held = vec![listing(vec![
        listed("1.4.0", "community", &[]),
        listed("1.5.0", "community", &[]),
        listed("1.6.0-beta", "community", &[]),
        listed("1.9.0", "verified", &[]),
    ])];
    assert_eq!(
        update_for(&installed("1.4.0", "community"), "1.4.0", &held)
            .unwrap()
            .version,
        "1.5.0"
    );
    assert!(update_for(&installed("1.5.0", "community"), "1.5.0", &held).is_none());
    assert_eq!(
        update_for(&installed("1.0.0", "verified"), "1.0.0", &held)
            .unwrap()
            .version,
        "1.9.0"
    );
    let mut local = installed("1.0.0", "local");
    local.listing.clear();
    assert!(
        update_for(&local, "1.0.0", &held).is_none(),
        "a local version is moved, not updated"
    );
}

#[test]
fn a_widening_and_a_pull_warning_until_kept() {
    let roles = |r: &[&str]| r.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(widening(
        &roles(&["custody"]),
        &roles(&["custody", "operations"])
    ));
    assert!(!widening(
        &roles(&["custody", "operations"]),
        &roles(&["custody"])
    ));

    let pull = PluginPull {
        kind: PullReasonKind::CopyrightNotice as i32,
        pulled_at_ns: 7,
        ..Default::default()
    };
    let mut version = listed("1.4.0", "community", &[]);
    version.pull = Some(pull.clone());
    let held = vec![listing(vec![version])];
    let mut launch = PluginLaunch {
        instance_id: "fx-1".into(),
        name: "acme/fx".into(),
        version: "1.4.0".into(),
        ..Default::default()
    };
    let warned = pull_warning(&launch, &installed("1.4.0", "community"), &held).unwrap();
    assert_eq!(pull_kind(&warned), "a copyright notice");
    launch.pull_acknowledgement = Some(PullAcknowledgement {
        pull: Some(pull),
        ..Default::default()
    });
    assert!(
        pull_warning(&launch, &installed("1.4.0", "community"), &held).is_none(),
        "kept"
    );
}

#[test]
fn counts_below_three_deployments_read_below_three_and_labels_are_the_registries() {
    let mut held = listing(vec![]);
    held.installs = 2;
    assert_eq!(installs(&held), "<3");
    held.counts_shown = true;
    held.installs = 4;
    assert_eq!(installs(&held), "4");
    assert_eq!(registry_label("community"), "Community");
    assert_eq!(registry_label("verified"), "Verified");
    assert_eq!(registry_label(""), "Local");
    assert!(floor_mark(&listed("1.0.0", "community", &[])).0);
    let mut beyond = listed("1.0.0", "community", &[]);
    beyond.schema_version = "v99".into();
    assert!(!floor_mark(&beyond).0);
}
