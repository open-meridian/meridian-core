//! Starter packs' words and choices (W8.18, W8.19): this deployment's state
//! of a pack, a member's state and terms basis, the members an install
//! ticks from the pack's selection and a person's choices, and Browse's
//! switch.

use meridian_domain::v1::{PackRevision, PluginMetadata};

use super::*;

fn member(listing: &str, selection: PackSelection, group: &str) -> PackMember {
    PackMember {
        listing: listing.into(),
        registry: "community".into(),
        selection: selection as i32,
        group: group.into(),
        minimum_version: "0.1.0".into(),
    }
}

fn crypto() -> PluginPack {
    PluginPack {
        pack: "open-meridian/crypto".into(),
        curator: "Open Meridian".into(),
        revision: Some(PackRevision {
            revision: 1,
            title: "Crypto".into(),
            members: vec![
                member("open-meridian/ny-fed", PackSelection::Preselected, "Data"),
                member("open-meridian/coinbase", PackSelection::Choose, "Venues"),
                member("open-meridian/kraken", PackSelection::Choose, "Venues"),
            ],
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn status(listing: &str, state: PackMemberState) -> PackMemberStatus {
    PackMemberStatus {
        listing: listing.into(),
        state: state as i32,
        ..Default::default()
    }
}

#[test]
fn the_data_members_are_ticked_and_the_venues_chosen_none_preselected() {
    let pack = crypto();
    assert_eq!(
        ticked(&pack, None, &[], &[]).unwrap(),
        vec!["open-meridian/ny-fed"]
    );
    // `--with coinbase`: a bare plugin name reads as its member.
    assert_eq!(
        ticked(&pack, None, &["coinbase".into()], &[]).unwrap(),
        vec!["open-meridian/ny-fed", "open-meridian/coinbase"]
    );
    assert_eq!(
        ticked(
            &pack,
            None,
            &["open-meridian/kraken".into()],
            &["ny-fed".into()]
        )
        .unwrap(),
        vec!["open-meridian/kraken"]
    );
    // A name that is not a member is refused by its path before anything moves.
    let (path, words) = ticked(&pack, None, &["binance".into()], &[]).unwrap_err();
    assert_eq!(path, "with[0]");
    assert!(words.contains("not a member"), "{words}");
    let (path, _) = ticked(
        &pack,
        Some(&["open-meridian/coinbase".into(), "x/y".into()]),
        &[],
        &[],
    )
    .unwrap_err();
    assert_eq!(path, "members[1]");
    assert_eq!(
        ticked(&pack, Some(&["coinbase".into()]), &[], &[]).unwrap(),
        vec!["open-meridian/coinbase"],
        "members whole, as ticked"
    );
}

#[test]
fn a_required_member_cannot_be_left_out() {
    let mut pack = crypto();
    pack.revision.as_mut().unwrap().members[0].selection = PackSelection::Required as i32;
    let (path, _) = ticked(&pack, None, &[], &["ny-fed".into()]).unwrap_err();
    assert_eq!(path, "without[0]");
}

#[test]
fn browse_says_how_much_of_a_pack_is_installed_here() {
    let mut held = PackStatus {
        pack: "open-meridian/crypto".into(),
        latest_revision: 2,
        members: vec![
            status("open-meridian/ny-fed", PackMemberState::Launched),
            status("open-meridian/coinbase", PackMemberState::Installed),
            status("open-meridian/kraken", PackMemberState::NotInstalled),
        ],
        ..Default::default()
    };
    assert_eq!(deployment_state(&held), "Not installed");
    held.installed_revision = 1;
    assert_eq!(deployment_state(&held), "Installed: 2 of 3 members");
    // A member no longer in the pack is not counted against it.
    held.members[2].removed_by_revision = 2;
    assert_eq!(deployment_state(&held), "Installed: 2 of 2 members");
}

#[test]
fn a_members_state_is_said_and_keyed() {
    for (state, said, key) in [
        (
            PackMemberState::NotInstalled,
            "Not installed",
            "not_installed",
        ),
        (PackMemberState::Installing, "Installing", "installing"),
        (PackMemberState::Failed, "Failed", "failed"),
        (PackMemberState::Installed, "Installed", "installed"),
        (PackMemberState::Launched, "Launched", "launched"),
        (PackMemberState::Unavailable, "Unavailable", "unavailable"),
        (
            PackMemberState::NotPermitted,
            "Not permitted here",
            "not_permitted",
        ),
    ] {
        assert_eq!(state_said(state as i32), said);
        assert_eq!(state_key(state as i32), key);
    }
    let json = status_json(&PackStatus {
        pack: "open-meridian/crypto".into(),
        installed_revision: 1,
        latest_revision: 1,
        members: vec![status("open-meridian/coinbase", PackMemberState::Launched)],
        ..Default::default()
    });
    assert_eq!(json["members"][0]["state"], "launched");
    assert_eq!(json["installed_revision"], 1);
}

#[test]
fn a_members_terms_basis_is_licensed_where_any_dataset_is() {
    use meridian_pb::v1::{
        AccessBasis, Catalogue, DatasetDeclaration, DatasetLicence, PluginDeclaration,
    };
    let declaring = |bases: &[AccessBasis]| ListedVersion {
        metadata: Some(PluginMetadata {
            declaration: Some(PluginDeclaration {
                catalogue: Some(Catalogue {
                    datasets: bases
                        .iter()
                        .map(|basis| DatasetDeclaration {
                            key: "rates".into(),
                            licence_default: Some(DatasetLicence {
                                access_basis: *basis as i32,
                                ..Default::default()
                            }),
                            ..Default::default()
                        })
                        .collect(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        access_basis(&declaring(&[AccessBasis::Open, AccessBasis::Licensed])),
        "Licensed"
    );
    assert_eq!(
        access_basis(&declaring(&[AccessBasis::Open, AccessBasis::PublicDomain])),
        "Open"
    );
    assert_eq!(access_basis(&declaring(&[])), "");
    assert_eq!(access_basis(&ListedVersion::default()), "");
}

#[test]
fn browse_switches_between_plugins_and_starter_packs() {
    let drawn = switch("packs");
    assert!(drawn.contains(">Plugins</a>"), "{drawn}");
    assert!(
        drawn
            .contains("data-view=\"packs\" class=\"here\" aria-current=\"page\">Starter packs</a>"),
        "{drawn}"
    );
    assert_eq!(group_key("Data"), "data");
    assert_eq!(group_key("Brokers & venues"), "brokers---venues");
    assert_eq!(groups(members_of(&crypto())), vec!["Data", "Venues"]);
}
