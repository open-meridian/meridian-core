//! Starter packs through the bus, against the stand-in platform (W8.18,
//! W8.19; spec/starter-packs, "Verification", core's cases): Browse's
//! view, an install with one member chosen and one failing while the rest
//! install, a member already installed left alone, "Update all" holding a
//! widening member and moving the rest, a revision's added and removed
//! members, "Remove pack" leaving every member running, a withdrawn pack.

use std::sync::Arc;

use meridian_domain::v1::{
    InstallPackRequest, ListRegistryPacksReply, ListRegistryPacksRequest, PackInstall,
    PackMemberState, PackStatus, ReadPackRequest, RemovePackRequest, UpdatePackRequest,
};
use serde_json::json;

use super::*;
use crate::plugins::registry::tests::{
    ask, catalogue, harness, harness_with, launch, list_version_1_5, pull, Harness, ADA,
};
use crate::registries::tests::LISTING;

const PACK: &str = "open-meridian/brokerage";
const ABSENT: &str = "open-meridian/absent";
const NOTE: &str = "The brokerage desk.";

/// The stand-in's catalogue carries a pack: SnapTrade to choose, and a data
/// member no listing offers, preselected.
fn publish(h: &Harness, revision: u64, members: serde_json::Value) {
    h.platform.catalogue.lock().unwrap()["packs"] = json!([{
        "pack": PACK, "curator": "Open Meridian", "sequence": 8, "withdrawn_at_ns": 0,
        "revision": {"revision": revision, "title": "Brokerage", "description": "Accounts.",
                     "asset_classes": ["equity"], "members": members, "note": "Reviewed.",
                     "published_at_ns": 1},
    }]);
}

fn members_v1() -> serde_json::Value {
    json!([
        {"listing": ABSENT, "registry": "community", "selection": "PACK_SELECTION_PRESELECTED",
         "group": "Data", "minimum_version": "0.1.0"},
        {"listing": LISTING, "registry": "community", "selection": "PACK_SELECTION_CHOOSE",
         "group": "Brokers", "minimum_version": "1.4.0"},
    ])
}

async fn read(h: &Harness) -> PackStatus {
    ask(
        h,
        READ_PACK,
        "meridian.v1.ReadPackRequest",
        ReadPackRequest { pack: PACK.into() },
    )
    .await
    .unwrap()
}

fn member<'a>(status: &'a PackStatus, listing: &str) -> &'a meridian_domain::v1::PackMemberStatus {
    status
        .members
        .iter()
        .find(|m| m.listing == listing)
        .unwrap_or_else(|| panic!("{listing} is not in {status:?}"))
}

async fn install(h: &Harness, revision: u64, members: &[&str]) -> Result<PackInstall, String> {
    ask(
        h,
        INSTALL_PACK,
        "meridian.v1.InstallPackRequest",
        InstallPackRequest {
            pack: PACK.into(),
            revision,
            members: members.iter().map(|m| m.to_string()).collect(),
            note: NOTE.into(),
        },
    )
    .await
}

/// The pack read until no member is installing: the job runs on its own.
async fn settled(h: &Harness) -> PackStatus {
    for _ in 0..200 {
        let status = read(h).await;
        if !status
            .members
            .iter()
            .any(|m| m.state == PackMemberState::Installing as i32)
        {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the pack's job never ended");
}

async fn changes(h: &Harness) -> Vec<PackChange> {
    let store = Arc::clone(&h.store);
    tokio::task::spawn_blocking(move || store.pack_changes(PACK))
        .await
        .unwrap()
        .unwrap()
}

async fn installed(h: &Harness) -> PackStatus {
    publish(h, 1, members_v1());
    pull(h).await;
    install(h, 1, &[ABSENT, LISTING]).await.unwrap();
    settled(h).await
}

#[tokio::test(flavor = "multi_thread")]
async fn browse_lists_a_pack_and_its_members_resolve_against_the_listings_held() {
    let h = harness();
    publish(&h, 1, members_v1());
    pull(&h).await;
    let listed: ListRegistryPacksReply = ask(
        &h,
        REGISTRY_PACKS,
        "meridian.v1.ListRegistryPacksRequest",
        ListRegistryPacksRequest {
            search: "brokerage".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(listed.packs.len(), 1);
    assert_eq!(listed.packs[0].revision.as_ref().unwrap().members.len(), 2);
    // Not installed: each member as the sheet shows it.
    let status = read(&h).await;
    assert_eq!(status.installed_revision, 0);
    assert_eq!(status.latest_revision, 1);
    let snaptrade = member(&status, LISTING);
    assert_eq!(snaptrade.state, PackMemberState::NotInstalled as i32);
    assert_eq!(snaptrade.latest_version, "1.4.0");
    let absent = member(&status, ABSENT);
    assert_eq!(absent.state, PackMemberState::Unavailable as i32);
    assert!(absent.reason.contains("none listed"), "{}", absent.reason);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pack_installs_member_by_member_past_a_failure_each_install_naming_it() {
    let h = harness();
    publish(&h, 1, members_v1());
    pull(&h).await;
    // Checked whole before anything moves.
    let stale = install(&h, 2, &[LISTING]).await.unwrap_err();
    assert!(stale.contains("revision"), "{stale}");
    let stranger = install(&h, 1, &[LISTING, "open-meridian/stranger"])
        .await
        .unwrap_err();
    assert!(stranger.contains("members[1]"), "{stranger}");
    assert!(catalogue(&h).await.versions.is_empty());
    // Answered at once, the job running on.
    let answer = install(&h, 1, &[ABSENT, LISTING]).await.unwrap();
    assert_eq!(answer.revision, 1);
    assert!(answer
        .members
        .iter()
        .all(|m| m.state == PackMemberState::Installing as i32));
    let status = settled(&h).await;
    assert_eq!(status.installed_revision, 1);
    let snaptrade = member(&status, LISTING);
    assert_eq!(snaptrade.state, PackMemberState::Installed as i32);
    assert_eq!(snaptrade.installed_version, "1.4.0");
    assert_eq!(snaptrade.setup, LAUNCH_TO_CONNECT);
    let absent = member(&status, ABSENT);
    assert_eq!(absent.state, PackMemberState::Failed as i32);
    assert!(
        absent.reason.starts_with("resolution refused"),
        "{}",
        absent.reason
    );
    // Install launches nothing.
    assert!(catalogue(&h).await.launches.is_empty());
    // The member's install record names the pack; one pack_installed.
    let store = Arc::clone(&h.store);
    let named = tokio::task::spawn_blocking(move || store.install_pack(LISTING, "1.4.0"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(named, PACK);
    let held = changes(&h).await;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].kind, PackChangeKind::Installed);
    assert_eq!(
        (held[0].by.by.as_str(), held[0].by.note.as_str()),
        (ADA, NOTE)
    );
    assert_eq!(held[0].revision, 1);
    assert_eq!(held[0].members.len(), 2);
    // Retry installs the failed one alone, and the one installed is left.
    let retried = install(&h, 1, &[ABSENT]).await.unwrap();
    assert_eq!(retried.members.len(), 1);
    settled(&h).await;
    assert_eq!(changes(&h).await.len(), 2);
    assert_eq!(catalogue(&h).await.versions.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pack_job_a_restart_interrupted_is_carried_on_and_recorded_once() {
    // 616: the job is kept in the store at its start and after each
    // member; a conductor starting over that store carries it on.
    let before = harness();
    publish(&before, 1, members_v1());
    pull(&before).await;
    let listed: ListRegistryPacksReply = ask(
        &before,
        REGISTRY_PACKS,
        "meridian.v1.ListRegistryPacksRequest",
        ListRegistryPacksRequest::default(),
    )
    .await
    .unwrap();
    let job = PackJob {
        pack: PACK.into(),
        held: listed.packs[0].clone(),
        members: vec![meridian_domain::v1::PackMemberStatus {
            listing: LISTING.into(),
            state: PackMemberState::Installing as i32,
            ..Default::default()
        }],
        envelope: Envelope {
            meta: Some(meridian_pb::v1::MessageMeta {
                acting_for_subject: ADA.into(),
                ..Default::default()
            }),
            ..Default::default()
        },
        note: NOTE.into(),
        began_at_ns: 1,
    };
    before.store.put_pack_job(&job).unwrap();

    let after = harness_with(Arc::clone(&before.platform), Arc::clone(&before.store));
    let status = settled(&after).await;
    assert_eq!(status.installed_revision, 1);
    assert_eq!(member(&status, LISTING).state, PackMemberState::Installed as i32);
    let held = changes(&after).await;
    assert_eq!(held.len(), 1, "one pack_installed");
    assert_eq!(held[0].kind, PackChangeKind::Installed);
    assert_eq!((held[0].by.by.as_str(), held[0].by.note.as_str()), (ADA, NOTE));
    for _ in 0..100 {
        if after.store.pack_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(after.store.pack_jobs().unwrap().is_empty(), "the job is cleared");
    assert_eq!(catalogue(&after).await.versions.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pack_job_is_kept_while_it_runs_and_cleared_once_recorded() {
    let h = harness();
    installed(&h).await;
    for _ in 0..100 {
        if h.store.pack_jobs().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(h.store.pack_jobs().unwrap().is_empty());
    assert_eq!(changes(&h).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_member_installed_already_is_left_alone_and_shown_below_the_version_offered() {
    let h = harness();
    let status = installed(&h).await;
    assert_eq!(
        member(&status, LISTING).state,
        PackMemberState::Installed as i32
    );
    list_version_1_5(&h, &["custody"]);
    publish(&h, 1, members_v1());
    pull(&h).await;
    install(&h, 1, &[LISTING]).await.unwrap();
    let status = settled(&h).await;
    let snaptrade = member(&status, LISTING);
    assert_eq!(snaptrade.installed_version, "1.4.0");
    assert_eq!(snaptrade.reason, "Installed 1.4.0; 1.5.0 offered");
    assert_eq!(
        catalogue(&h).await.versions.len(),
        1,
        "never moved by a pack"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn update_all_holds_a_widening_member_for_its_own_approval() {
    let h = harness();
    installed(&h).await;
    launch(&h, LISTING, "1.4.0", &["custody"]).await.unwrap();
    assert_eq!(
        member(&read(&h).await, LISTING).state,
        PackMemberState::Launched as i32
    );
    list_version_1_5(&h, &["custody", "operations"]);
    publish(&h, 1, members_v1());
    pull(&h).await;
    let updated: PackStatus = ask(
        &h,
        UPDATE_PACK,
        "meridian.v1.UpdatePackRequest",
        UpdatePackRequest {
            pack: PACK.into(),
            note: "Monthly update.".into(),
        },
    )
    .await
    .unwrap();
    let snaptrade = member(&updated, LISTING);
    assert_eq!(snaptrade.reason, NEEDS_APPROVAL);
    assert_eq!(snaptrade.installed_version, "1.4.0");
    let held = catalogue(&h).await;
    assert!(held
        .launches
        .iter()
        .any(|l| l.version == "1.4.0" && l.state == PluginLaunchState::Launched as i32));
    let recorded = changes(&h).await;
    assert_eq!(recorded.last().unwrap().kind, PackChangeKind::Updated);
    // Remembered (616): read again, from the pack_updated record, it still
    // needs approval while it runs below the version offered.
    let again = read(&h).await;
    let again = member(&again, LISTING);
    assert_eq!(again.reason, NEEDS_APPROVAL);
    assert_eq!(again.state, PackMemberState::Launched as i32);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_all_moves_each_launched_member_by_its_own_update() {
    let h = harness();
    installed(&h).await;
    launch(&h, LISTING, "1.4.0", &["custody"]).await.unwrap();
    list_version_1_5(&h, &["custody"]);
    publish(&h, 1, members_v1());
    pull(&h).await;
    let updated: PackStatus = ask(
        &h,
        UPDATE_PACK,
        "meridian.v1.UpdatePackRequest",
        UpdatePackRequest {
            pack: PACK.into(),
            note: "Monthly update.".into(),
        },
    )
    .await
    .unwrap();
    let snaptrade = member(&updated, LISTING);
    assert_eq!(snaptrade.state, PackMemberState::Launched as i32);
    assert_eq!(snaptrade.installed_version, "1.5.0");
    let held = catalogue(&h).await;
    assert!(held
        .launches
        .iter()
        .any(|l| l.version == "1.5.0" && l.state == PluginLaunchState::Launched as i32));
    // The member added since is never installed by Update all (408).
    assert_eq!(
        member(&updated, ABSENT).state,
        PackMemberState::Failed as i32
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revision_shows_its_added_and_removed_members_and_remove_pack_touches_none() {
    let h = harness();
    installed(&h).await;
    launch(&h, LISTING, "1.4.0", &["custody"]).await.unwrap();
    // Revision 2 drops the data member and adds another.
    publish(
        &h,
        2,
        json!([
            {"listing": LISTING, "registry": "community", "selection": "PACK_SELECTION_CHOOSE",
             "group": "Brokers", "minimum_version": "1.4.0"},
            {"listing": "open-meridian/later", "registry": "community",
             "selection": "PACK_SELECTION_PRESELECTED", "group": "Data", "minimum_version": "0.1.0"},
        ]),
    );
    pull(&h).await;
    let status = read(&h).await;
    assert_eq!((status.installed_revision, status.latest_revision), (1, 2));
    assert_eq!(member(&status, "open-meridian/later").added_by_revision, 2);
    assert_eq!(member(&status, ABSENT).removed_by_revision, 2);
    assert_eq!(member(&status, LISTING).added_by_revision, 0);
    // "Remove pack" needs a note, and removes only the record.
    let refused: Result<PackStatus, String> = ask(
        &h,
        REMOVE_PACK,
        "meridian.v1.RemovePackRequest",
        RemovePackRequest {
            pack: PACK.into(),
            note: String::new(),
        },
    )
    .await;
    assert!(refused.unwrap_err().contains("note"));
    let removed: PackStatus = ask(
        &h,
        REMOVE_PACK,
        "meridian.v1.RemovePackRequest",
        RemovePackRequest {
            pack: PACK.into(),
            note: "Desk moving to a custom set.".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(removed.installed_revision, 1);
    assert_eq!(
        member(&removed, LISTING).state,
        PackMemberState::Launched as i32
    );
    let after = read(&h).await;
    assert_eq!(after.installed_revision, 0);
    assert_eq!(
        member(&after, LISTING).state,
        PackMemberState::Launched as i32,
        "nothing stopped"
    );
    assert_eq!(catalogue(&h).await.versions.len(), 1, "nothing uninstalled");
    let recorded = changes(&h).await;
    assert_eq!(recorded.last().unwrap().kind, PackChangeKind::Removed);
    assert_eq!(
        recorded.last().unwrap().by.note,
        "Desk moving to a custom set."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_pack_leaves_browse_and_reads_withdrawn() {
    let h = harness();
    installed(&h).await;
    h.platform.catalogue.lock().unwrap()["packs"][0]["withdrawn_at_ns"] = json!(5);
    pull(&h).await;
    let listed: ListRegistryPacksReply = ask(
        &h,
        REGISTRY_PACKS,
        "meridian.v1.ListRegistryPacksRequest",
        ListRegistryPacksRequest::default(),
    )
    .await
    .unwrap();
    assert!(listed.packs.is_empty());
    let status = read(&h).await;
    assert!(status.withdrawn);
    assert_eq!(status.installed_revision, 1);
    assert!(install(&h, 1, &[LISTING]).await.is_err());
}

#[test]
fn a_member_resolves_to_its_highest_offered_release_at_or_above_its_minimum() {
    let version = |v: &str, withdrawn: i64| ListedVersion {
        listing: LISTING.into(),
        version: v.into(),
        registry: "community".into(),
        withdrawn_at_ns: withdrawn,
        ..Default::default()
    };
    let listings = vec![PluginListing {
        listing: LISTING.into(),
        versions: vec![
            version("0.3.0", 0),
            version("0.5.1", 0),
            version("0.6.0-rc.1", 0),
            version("0.7.0", 9),
        ],
        ..Default::default()
    }];
    let member = |floor: &str| PackMember {
        listing: LISTING.into(),
        registry: "community".into(),
        minimum_version: floor.into(),
        ..Default::default()
    };
    assert_eq!(
        resolve(&listings, &member("0.3.0"), "community")
            .unwrap()
            .version,
        "0.5.1"
    );
    assert_eq!(
        resolve(&listings, &member("0.6.0-rc.1"), "community")
            .unwrap()
            .version,
        "0.6.0-rc.1",
        "a pre-release only where no release is"
    );
    assert_eq!(
        resolve(&listings, &member("0.7.0"), "community").unwrap_err(),
        "withdrawn by its vendor"
    );
    assert!(resolve(&listings, &member("0.3.0"), "verified").is_err());
    assert_eq!(
        failure("the provenance: gone"),
        "verification failed: the provenance: gone"
    );
    assert_eq!(failure("the floor: v99"), "floor not met: the floor: v99");
}
