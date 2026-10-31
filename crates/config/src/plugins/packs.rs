//! Starter packs in the deployment (W8.18, W8.19; contract v27's wire, built
//! in v28; spec/starter-packs, requirements 16 to 27): Browse's pack view,
//! a pack's install as a job, its page -- the setup checklist -- "Update
//! all", and "Remove pack".
//!
//! A pack is a catalogue list, never a merged plugin (336): installing one
//! runs the per-plugin install once for each member ticked, each member its
//! own install record naming the pack, and leaves one `pack_installed`
//! record naming the revision, the members ticked and each one's outcome.
//! After that the members live on their own. The deployment's pack record
//! is the run of its changes since its last removal, each its own record
//! (decisions/031); removing it touches no member.
//!
//! A member tracks its latest listed release at or above its minimum
//! (requirement 8, ruling 400), resolved here against the listings the
//! conductor already holds, a withdrawn or pulled release skipped.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use meridian_bus::{refusal_with_status, Envelope, RefusalStatus};
use meridian_domain::semver::{is_prerelease, semver_cmp};
use meridian_domain::v1::{
    InstallPackRequest, ListRegistryPacksReply, ListRegistryPacksRequest, ListedVersion,
    PackInstall, PackMember, PackMemberState, PackMemberStatus, PackSelection, PackStatus,
    PluginLaunchState, PluginListing, PluginPack, ReadPackRequest, RemovePackRequest,
    UpdatePackRequest, UpdatePluginRequest,
};

use super::Plugins;
use crate::registries::{floor_refused, offered};
use crate::service::author;
use crate::store::{note_refused, HeldCatalogue, PackChange, PackChangeKind, PackJob, Snapshot};

/// W8.18: Browse's pack view.
pub const REGISTRY_PACKS: &str = "platform.config.query.registry-packs";
/// W8.18: a pack's ticked members installed, one by one, as a job.
pub const INSTALL_PACK: &str = "platform.config.command.install-pack";
/// W8.19: a pack's page, the setup checklist.
pub const READ_PACK: &str = "platform.config.query.read-pack";
/// W8.19: "Update all".
pub const UPDATE_PACK: &str = "platform.config.command.update-pack";
/// W8.19: "Remove pack".
pub const REMOVE_PACK: &str = "platform.config.command.remove-pack";

/// What a member installed but not launched reads as its next step: its
/// launch, from the checklist (405).
pub const LAUNCH_TO_CONNECT: &str = "Launch to connect";

/// What a widening member reads on "Update all" (requirement 20).
pub const NEEDS_APPROVAL: &str = "Needs approval";

/// The packs whose install is under way, each member's state so far: a job
/// runs member by member (407), and the pack's page reads it until its
/// `pack_installed` record is written. Each is also kept in the store
/// ([`PackJob`]) at its start and after each member, so a conductor that
/// restarts mid-job carries it on (616).
#[derive(Default)]
pub(super) struct Jobs(Mutex<BTreeMap<String, Vec<PackMemberStatus>>>);

impl Jobs {
    fn get(&self, pack: &str) -> Option<Vec<PackMemberStatus>> {
        self.0
            .lock()
            .expect("jobs lock poisoned")
            .get(pack)
            .cloned()
    }

    /// Begin a pack's job, unless one is under way.
    fn begin(&self, pack: &str, members: Vec<PackMemberStatus>) -> bool {
        let mut held = self.0.lock().expect("jobs lock poisoned");
        if held.contains_key(pack) {
            return false;
        }
        held.insert(pack.to_string(), members);
        true
    }

    fn set(&self, pack: &str, outcome: &PackMemberStatus) {
        if let Some(members) = self.0.lock().expect("jobs lock poisoned").get_mut(pack) {
            if let Some(slot) = members.iter_mut().find(|m| m.listing == outcome.listing) {
                *slot = outcome.clone();
            }
        }
    }

    fn end(&self, pack: &str) {
        self.0.lock().expect("jobs lock poisoned").remove(pack);
    }
}

/// The deployment's record of a pack: the run of its changes since its last
/// removal, none when it was never installed or was removed since.
#[derive(Debug, Clone)]
pub struct Kept {
    /// The revision installed, and the pack as the catalogue held it then.
    pub revision: u64,
    pub held: PluginPack,
    /// Each member's latest outcome, by listing.
    pub outcomes: BTreeMap<String, PackMemberStatus>,
}

/// Fold a pack's changes, oldest first, into its record as it stands.
pub fn kept(changes: &[PackChange]) -> Option<Kept> {
    let mut kept: Option<Kept> = None;
    for change in changes {
        match change.kind {
            PackChangeKind::Removed => kept = None,
            PackChangeKind::Installed => {
                let mut outcomes = kept.take().map(|k| k.outcomes).unwrap_or_default();
                for member in &change.members {
                    outcomes.insert(member.listing.clone(), member.clone());
                }
                kept = Some(Kept {
                    revision: change.revision,
                    held: change.held.clone(),
                    outcomes,
                });
            }
            PackChangeKind::Updated => {
                if let Some(kept) = kept.as_mut() {
                    for member in &change.members {
                        kept.outcomes.insert(member.listing.clone(), member.clone());
                    }
                }
            }
        }
    }
    kept
}

fn members_of(pack: &PluginPack) -> &[PackMember] {
    pack.revision
        .as_ref()
        .map(|revision| revision.members.as_slice())
        .unwrap_or_default()
}

fn revision_of(pack: &PluginPack) -> u64 {
    pack.revision
        .as_ref()
        .map_or(0, |revision| revision.revision)
}

fn newer(a: &str, b: &str) -> bool {
    semver_cmp(a, b) == Some(std::cmp::Ordering::Greater)
}

fn highest<'a>(versions: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    versions.fold(None, |best, version| match best {
        Some(held) if !newer(version, held) => Some(held),
        _ => Some(version),
    })
}

/// A member's version as resolved (requirement 8): the highest offered
/// release in `registry` at or above its minimum, a pre-release only where
/// no release is; or, where none is offered, why in words (requirement 10):
/// none listed, withdrawn by its vendor, or pulled.
pub fn resolve(
    listings: &[PluginListing],
    member: &PackMember,
    registry: &str,
) -> Result<ListedVersion, String> {
    let floor = &member.minimum_version;
    let none = || {
        if floor.is_empty() {
            "none listed".to_string()
        } else {
            format!("none listed at or above {floor}")
        }
    };
    let listing = listings
        .iter()
        .find(|held| held.listing == member.listing)
        .ok_or_else(none)?;
    let above: Vec<&ListedVersion> = listing
        .versions
        .iter()
        .filter(|v| v.registry == registry)
        .filter(|v| {
            floor.is_empty()
                || semver_cmp(&v.version, floor).is_some_and(|o| o != std::cmp::Ordering::Less)
        })
        .collect();
    let live: Vec<&ListedVersion> = above.iter().copied().filter(|v| offered(v)).collect();
    let releases: Vec<&ListedVersion> = live
        .iter()
        .copied()
        .filter(|v| !is_prerelease(&v.version))
        .collect();
    if let Some(best) = pick(&releases).or_else(|| pick(&live)) {
        return Ok(best.clone());
    }
    if above.is_empty() {
        Err(none())
    } else if above.iter().all(|v| v.withdrawn_at_ns != 0) {
        Err("withdrawn by its vendor".into())
    } else {
        Err("pulled".into())
    }
}

/// The highest of `from` by SemVer 2.0 precedence.
fn pick<'a>(from: &[&'a ListedVersion]) -> Option<&'a ListedVersion> {
    from.iter()
        .copied()
        .fold(None::<&ListedVersion>, |best, v| match best {
            Some(held) if !newer(&v.version, &held.version) => Some(held),
            _ => Some(v),
        })
}

/// A member's roles as a launch approves them: a verified version's its
/// review's, a community one's its declaration's.
fn roles_of(version: &ListedVersion) -> Vec<String> {
    if version.registry == "verified" && !version.approved_roles.is_empty() {
        version.approved_roles.clone()
    } else {
        version
            .metadata
            .as_ref()
            .map(|m| m.roles.clone())
            .unwrap_or_default()
    }
}

/// A failed member's reason (requirement 24): its kind -- resolution
/// refused, verification failed, floor not met, timeout -- then the words.
fn failure(why: &str) -> String {
    const VERIFIED: [&str; 9] = [
        "the digest",
        "the listing statement",
        "the provenance",
        "the image's metadata",
        "the signing key",
        "the signer",
        "the declaration",
        "the roles",
        "the documentation",
    ];
    let kind = if why.starts_with("the floor") {
        "floor not met"
    } else if why.contains("timed out") || why.contains("timeout") {
        "timeout"
    } else if VERIFIED.iter().any(|check| why.starts_with(check)) {
        "verification failed"
    } else {
        "resolution refused"
    };
    format!("{kind}: {why}")
}

fn failed(member: &PackMember, latest: &str, why: String) -> PackMemberStatus {
    PackMemberStatus {
        listing: member.listing.clone(),
        group: member.group.clone(),
        latest_version: latest.to_string(),
        state: PackMemberState::Failed as i32,
        reason: why,
        ..Default::default()
    }
}

/// One member's state in this deployment (requirements 19 and 23 to 26),
/// from the catalogue's copy, the plugin catalogue, the pack's record and a
/// job under way.
fn member_status(
    member: &PackMember,
    listings: &[PluginListing],
    snapshot: &Snapshot,
    outcome: Option<&PackMemberStatus>,
    job: Option<&PackMemberStatus>,
    taken: &dyn Fn(&str) -> Option<String>,
) -> PackMemberStatus {
    let resolved = resolve(listings, member, &member.registry);
    let latest = resolved
        .as_ref()
        .map(|v| v.version.clone())
        .unwrap_or_default();
    let installed: Vec<&str> = snapshot
        .catalogue
        .versions
        .iter()
        .filter(|v| v.listing == member.listing && v.retired_at_ns == 0)
        .filter_map(|v| v.metadata.as_ref().map(|m| m.version.as_str()))
        .collect();
    let launches: Vec<_> = snapshot
        .catalogue
        .launches
        .iter()
        .filter(|l| l.state == PluginLaunchState::Launched as i32 && l.name == member.listing)
        .collect();
    let mut status = PackMemberStatus {
        listing: member.listing.clone(),
        group: member.group.clone(),
        latest_version: latest.clone(),
        instance_ids: launches.iter().map(|l| l.instance_id.clone()).collect(),
        ..Default::default()
    };
    let running = highest(launches.iter().map(|l| l.version.as_str()));
    let at = running.or_else(|| highest(installed.iter().copied()));
    if let Some(job) = job.filter(|job| job.state == PackMemberState::Installing as i32) {
        status.state = job.state;
        status.installed_version = at.unwrap_or_default().to_string();
        return status;
    }
    if let Some(at) = at {
        status.installed_version = at.to_string();
        status.state = if launches.is_empty() {
            status.setup = LAUNCH_TO_CONNECT.into();
            PackMemberState::Installed as i32
        } else {
            PackMemberState::Launched as i32
        };
        // Requirement 25: below the version resolved, left for "Update all".
        if !latest.is_empty() && newer(&latest, at) {
            status.reason = format!("Installed {at}; {latest} offered");
            // Requirement 20, remembered (616): "Update all" held it back
            // as widening, and it still runs below the version offered, so
            // it reads "Needs approval" until its own approval moves it --
            // after the pack's page is read again, or core restarts.
            if !launches.is_empty() && outcome.is_some_and(|o| o.reason == NEEDS_APPROVAL) {
                status.reason = NEEDS_APPROVAL.into();
            }
        }
        return status;
    }
    if let Some(outcome) = outcome.filter(|o| o.state == PackMemberState::Failed as i32) {
        status.state = outcome.state;
        status.reason = outcome.reason.clone();
        return status;
    }
    match resolved {
        Err(why) => {
            status.state = PackMemberState::Unavailable as i32;
            status.reason = why;
        }
        // Requirement 26: its floor not met, or a registry not taken here.
        Ok(version) => match floor_refused(&version).or_else(|| taken(&version.registry)) {
            Some(why) => {
                status.state = PackMemberState::NotPermitted as i32;
                status.reason = why;
            }
            None => status.state = PackMemberState::NotInstalled as i32,
        },
    }
    status
}

impl Plugins {
    fn held_catalogue(&self) -> Result<HeldCatalogue, String> {
        self.registries()?
            .held()?
            .ok_or_else(|| "the catalogue has not been pulled yet; pull it first".to_string())
    }

    fn kept_pack(&self, pack: &str) -> Result<Option<Kept>, String> {
        Ok(kept(
            &self
                .store
                .pack_changes(pack)
                .map_err(|failed| failed.to_string())?,
        ))
    }

    /// A pack's status in this deployment (requirement 19): every member of
    /// the catalogue's revision in the curator's order, then any the
    /// installed revision had and the latest dropped.
    fn pack_status(&self, pack: &str) -> Result<PackStatus, String> {
        let held = self.held_catalogue()?;
        let listings = self.registries()?.listings_of(&held);
        let catalogued = held.packs.iter().find(|p| p.pack == pack).cloned();
        let kept = self.kept_pack(pack)?;
        let Some(latest) = catalogued
            .clone()
            .or_else(|| kept.as_ref().map(|k| k.held.clone()))
        else {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &["pack".to_string()],
                format!("no starter pack {pack} is in the catalogue this deployment holds"),
            ));
        };
        let snapshot = self.snapshot()?;
        let hold = self.registries()?;
        let taken = |registry: &str| hold.taken_refused(registry);
        let job = self.packs.get(pack).unwrap_or_default();
        let latest_revision = revision_of(&latest);
        let installed_members: &[PackMember] = kept.as_ref().map_or(&[], |k| members_of(&k.held));
        let status_of = |member: &PackMember| {
            let mut status = member_status(
                member,
                &listings,
                &snapshot,
                kept.as_ref().and_then(|k| k.outcomes.get(&member.listing)),
                job.iter().find(|j| j.listing == member.listing),
                &taken,
            );
            if kept.is_some()
                && !installed_members
                    .iter()
                    .any(|held| held.listing == member.listing)
            {
                status.added_by_revision = latest_revision;
            }
            status
        };
        let mut members: Vec<PackMemberStatus> =
            members_of(&latest).iter().map(status_of).collect();
        for gone in installed_members.iter().filter(|held| {
            !members_of(&latest)
                .iter()
                .any(|m| m.listing == held.listing)
        }) {
            let mut status = status_of(gone);
            status.removed_by_revision = latest_revision;
            members.push(status);
        }
        Ok(PackStatus {
            pack: pack.to_string(),
            installed_revision: kept.as_ref().map_or(0, |k| k.revision),
            latest_revision,
            withdrawn: catalogued.is_none_or(|p| p.withdrawn_at_ns != 0),
            members,
            // Contract v29 (616): the pack's title as its curator states it,
            // the latest revision's, catalogued or kept.
            title: latest
                .revision
                .as_ref()
                .map(|r| r.title.clone())
                .unwrap_or_default(),
        })
    }

    /// ListRegistryPacks (W8.18): Browse's pack view, from the copy held,
    /// pulled first where asked; a withdrawn pack is not listed.
    pub(super) fn list_registry_packs(
        &self,
        request: ListRegistryPacksRequest,
    ) -> Result<ListRegistryPacksReply, String> {
        let hold = self.registries()?;
        let held = if request.pull_now {
            self.block_on(hold.pull())?
        } else {
            hold.held()?.unwrap_or_default()
        };
        let words: Vec<String> = request
            .search
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let packs: Vec<PluginPack> = held
            .packs
            .iter()
            .filter(|pack| pack.withdrawn_at_ns == 0)
            .filter(|pack| request.pack.is_empty() || pack.pack == request.pack)
            .filter(|pack| {
                let revision = pack.revision.clone().unwrap_or_default();
                let haystack = format!(
                    "{} {} {} {}",
                    pack.pack,
                    revision.title,
                    revision.description,
                    revision.asset_classes.join(" ")
                )
                .to_lowercase();
                words.iter().all(|word| haystack.contains(word))
            })
            .cloned()
            .collect();
        let size = usize::try_from(request.page_size)
            .ok()
            .filter(|size| *size > 0)
            .unwrap_or(50)
            .min(200);
        let start: usize = request.cursor.parse().unwrap_or(0);
        let next = start + size;
        Ok(ListRegistryPacksReply {
            next_cursor: if next < packs.len() {
                next.to_string()
            } else {
                String::new()
            },
            packs: packs.into_iter().skip(start).take(size).collect(),
            catalogue_pulled_at_ns: held.pulled_at_ns,
        })
    }

    /// ReadPack (W8.19): the pack's page, the setup checklist; for a pack
    /// not installed, its members as the install sheet shows them.
    pub(super) fn read_pack(&self, request: ReadPackRequest) -> Result<PackStatus, String> {
        self.pack_status(&request.pack)
    }

    /// InstallPack (W8.18; requirements 18 and 24 to 26): checked whole
    /// before anything moves, then a job, member by member, continuing past
    /// a failure with nothing rolled back; answered at once with each
    /// ticked member's state, those to install "installing".
    pub(super) fn install_pack(
        self: &Arc<Self>,
        request: InstallPackRequest,
        envelope: &Envelope,
    ) -> Result<PackInstall, String> {
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        let held = self.held_catalogue()?;
        let Some(pack) = held
            .packs
            .iter()
            .find(|p| p.pack == request.pack && p.withdrawn_at_ns == 0)
            .cloned()
        else {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &["pack".to_string()],
                format!(
                    "no starter pack {} is offered in the catalogue this deployment holds",
                    request.pack
                ),
            ));
        };
        let revision = revision_of(&pack);
        if request.revision != revision {
            return Err(format!(
                "revision: {} is at revision {revision} in the catalogue, and {} was read; read \
                 the pack again",
                request.pack, request.revision
            ));
        }
        let members = members_of(&pack);
        for (n, asked) in request.members.iter().enumerate() {
            if !members.iter().any(|m| &m.listing == asked) {
                return Err(refusal_with_status(
                    RefusalStatus::InvalidArgument,
                    &[format!("members[{n}]")],
                    format!("{asked} is not a member of {}", request.pack),
                ));
            }
        }
        if let Some(required) = members.iter().find(|m| {
            m.selection == PackSelection::Required as i32 && !request.members.contains(&m.listing)
        }) {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &["members".to_string()],
                format!(
                    "{} is required by {} and is installed with it",
                    required.listing, request.pack
                ),
            ));
        }
        let hold = self.registries()?;
        let listings = hold.listings_of(&held);
        let taken = |registry: &str| hold.taken_refused(registry);
        let snapshot = self.snapshot()?;
        let kept = self.kept_pack(&request.pack)?;
        let ticked: Vec<PackMember> = members
            .iter()
            .filter(|m| request.members.contains(&m.listing))
            .cloned()
            .collect();
        let mut queued = Vec::new();
        let statuses: Vec<PackMemberStatus> = ticked
            .iter()
            .map(|member| {
                let status = member_status(
                    member,
                    &listings,
                    &snapshot,
                    kept.as_ref().and_then(|k| k.outcomes.get(&member.listing)),
                    None,
                    &taken,
                );
                // Requirement 25: a member installed at any version is left
                // alone; the rest are installed by the job.
                if status.state == PackMemberState::Installed as i32
                    || status.state == PackMemberState::Launched as i32
                {
                    status
                } else {
                    queued.push(member.clone());
                    PackMemberStatus {
                        state: PackMemberState::Installing as i32,
                        reason: String::new(),
                        ..status
                    }
                }
            })
            .collect();
        let answer = PackInstall {
            pack: request.pack.clone(),
            revision,
            members: statuses.clone(),
        };
        if !self.packs.begin(&request.pack, statuses) {
            return Err(format!(
                "an install of {} is under way; follow it on the pack's page",
                request.pack
            ));
        }
        let began_at_ns = self.clock.now_ns();
        self.keep_job(&pack, &request.note, envelope, began_at_ns);
        let plugins = Arc::clone(self);
        let envelope = envelope.clone();
        let note = request.note.clone();
        let job = move || plugins.run_pack_install(&pack, &queued, &note, &envelope, began_at_ns);
        match tokio::runtime::Handle::try_current() {
            // A job of its own (407): the answer does not wait for it.
            Ok(runtime) => {
                runtime.spawn_blocking(job);
            }
            Err(_) => job(),
        }
        Ok(answer)
    }

    /// The pack job: each queued member installed by InstallPlugin's steps
    /// in the curator's order, then the one `pack_installed` record.
    fn run_pack_install(
        &self,
        pack: &PluginPack,
        queued: &[PackMember],
        note: &str,
        envelope: &Envelope,
        began_at_ns: i64,
    ) {
        for member in queued {
            let outcome = self.install_member(pack, member, note, envelope);
            self.packs.set(&pack.pack, &outcome);
            self.keep_job(pack, note, envelope, began_at_ns);
        }
        let members = self.packs.get(&pack.pack).unwrap_or_default();
        let change = PackChange {
            kind: PackChangeKind::Installed,
            pack: pack.pack.clone(),
            revision: revision_of(pack),
            held: pack.clone(),
            members,
            by: crate::store::Author {
                note: note.to_string(),
                ..author(envelope)
            },
            at_ns: self.clock.now_ns(),
        };
        let recorded = self.store.record_pack_change(&change);
        // The job is over once its record is written (or failed to be): it
        // ends here, before the kept job is cleared, so a job no longer kept
        // is never still under way -- a new install is not refused for it.
        self.packs.end(&pack.pack);
        match recorded {
            Ok(()) => {
                tracing::info!(
                    pack = change.pack,
                    revision = change.revision,
                    by = change.by.by,
                    failed = change
                        .members
                        .iter()
                        .filter(|m| m.state == PackMemberState::Failed as i32)
                        .count(),
                    "a starter pack installed"
                );
                // Only once the record is written: a job whose record was
                // not is carried on, and recorded, at the next start.
                if let Err(failed) = self.store.end_pack_job(&change.pack) {
                    tracing::warn!(pack = change.pack, %failed, "a finished pack job was not cleared");
                }
            }
            Err(failed) => tracing::error!(
                pack = change.pack,
                %failed,
                "a starter pack's install was not recorded; its members' installs were"
            ),
        }
    }

    /// The job as it stands, kept in the store (616); a failure to keep it
    /// is logged and the job goes on, as it would have before.
    fn keep_job(&self, pack: &PluginPack, note: &str, envelope: &Envelope, began_at_ns: i64) {
        let Some(members) = self.packs.get(&pack.pack) else {
            return;
        };
        let job = PackJob {
            pack: pack.pack.clone(),
            held: pack.clone(),
            members,
            envelope: envelope.clone(),
            note: note.to_string(),
            began_at_ns,
        };
        if let Err(failed) = self.store.put_pack_job(&job) {
            tracing::warn!(pack = job.pack, %failed, "a pack job's progress was not kept");
        }
    }

    /// At start (616): each pack install a restart interrupted, carried on
    /// from the members still installing, in the curator's order, by the
    /// person and delegation that asked, then recorded once as
    /// `pack_installed`. A member whose install completed before the
    /// restart reads as installed, never installed twice.
    pub(super) fn resume_pack_jobs(self: &Arc<Self>) {
        // The store blocks, as Postgres's client does: read and run off the
        // runtime, as every other store call here is.
        let plugins = Arc::clone(self);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || plugins.resume_now());
            }
            Err(_) => plugins.resume_now(),
        }
    }

    /// [`Plugins::resume_pack_jobs`], on a thread that may block: each job
    /// carried on in turn.
    fn resume_now(&self) {
        let jobs = match self.store.pack_jobs() {
            Ok(jobs) => jobs,
            Err(failed) => {
                tracing::error!(%failed, "the pack jobs under way could not be read");
                return;
            }
        };
        for job in jobs {
            let queued: Vec<PackMember> = members_of(&job.held)
                .iter()
                .filter(|member| {
                    job.members.iter().any(|m| {
                        m.listing == member.listing && m.state == PackMemberState::Installing as i32
                    })
                })
                .cloned()
                .collect();
            if !self.packs.begin(&job.pack, job.members.clone()) {
                continue;
            }
            tracing::info!(
                pack = job.pack,
                members = queued.len(),
                "a starter pack's install carried on after a restart"
            );
            self.run_pack_install(
                &job.held,
                &queued,
                &job.note,
                &job.envelope,
                job.began_at_ns,
            );
        }
    }

    /// One member's install within a pack's job: resolved again, its floor
    /// checked, then InstallPlugin's steps with the pack named.
    fn install_member(
        &self,
        pack: &PluginPack,
        member: &PackMember,
        note: &str,
        envelope: &Envelope,
    ) -> PackMemberStatus {
        let listings = match self
            .held_catalogue()
            .and_then(|held| Ok(self.registries()?.listings_of(&held)))
        {
            Ok(listings) => listings,
            Err(why) => return failed(member, "", failure(&why)),
        };
        let version = match resolve(&listings, member, &member.registry) {
            Ok(version) => version,
            Err(why) => return failed(member, "", format!("resolution refused: {why}")),
        };
        if let Some(why) = floor_refused(&version) {
            return failed(member, &version.version, failure(&why));
        }
        let installed = PackMemberStatus {
            listing: member.listing.clone(),
            group: member.group.clone(),
            installed_version: version.version.clone(),
            latest_version: version.version.clone(),
            state: PackMemberState::Installed as i32,
            setup: LAUNCH_TO_CONNECT.into(),
            ..Default::default()
        };
        // A job carried on after a restart (616): the member's install may
        // have completed before it, and is not done twice.
        let held_already = self.snapshot().is_ok_and(|snapshot| {
            snapshot.catalogue.versions.iter().any(|v| {
                v.listing == member.listing
                    && v.retired_at_ns == 0
                    && v.metadata
                        .as_ref()
                        .is_some_and(|m| m.version == version.version)
            })
        });
        if held_already {
            return installed;
        }
        match self.install_version(
            &member.listing,
            &version.version,
            &version.registry,
            note,
            envelope,
            &pack.pack,
        ) {
            Ok(_) => installed,
            Err(why) => {
                tracing::warn!(
                    pack = pack.pack,
                    listing = member.listing,
                    %why,
                    "a starter pack's member was not installed"
                );
                failed(member, &version.version, failure(&why))
            }
        }
    }

    /// UpdatePack (W8.19; requirement 20): each launched member with a
    /// newer offered version in its source registry moved by UpdatePlugin,
    /// its own record, a widening one skipped as "Needs approval"; an
    /// installed, unlaunched member given the newer version; a member added
    /// since never installed (408); then one `pack_updated` record.
    pub(super) fn update_pack(
        &self,
        request: UpdatePackRequest,
        envelope: &Envelope,
    ) -> Result<PackStatus, String> {
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        let Some(kept) = self.kept_pack(&request.pack)? else {
            return Err(format!(
                "{} is not installed here; there is nothing to update",
                request.pack
            ));
        };
        if self.packs.get(&request.pack).is_some() {
            return Err(format!(
                "an install of {} is under way; update once it ends",
                request.pack
            ));
        }
        let held = self.held_catalogue()?;
        let listings = self.registries()?.listings_of(&held);
        let latest = held
            .packs
            .iter()
            .find(|p| p.pack == request.pack)
            .cloned()
            .unwrap_or_else(|| kept.held.clone());
        let before = self.pack_status(&request.pack)?;
        let snapshot = self.snapshot()?;
        let definition = |listing: &str| {
            members_of(&latest)
                .iter()
                .chain(members_of(&kept.held))
                .find(|m| m.listing == listing)
                .cloned()
        };
        let mut outcomes: BTreeMap<String, PackMemberStatus> = BTreeMap::new();
        for status in &before.members {
            let Some(member) = definition(&status.listing) else {
                continue;
            };
            let outcome = if status.state == PackMemberState::Launched as i32 {
                self.update_launched(
                    &member,
                    status,
                    &listings,
                    &snapshot,
                    &request.note,
                    envelope,
                )
            } else if status.state == PackMemberState::Installed as i32 {
                self.update_installed(
                    &member,
                    status,
                    &listings,
                    &request.pack,
                    &request.note,
                    envelope,
                )
            } else {
                None
            };
            if let Some(outcome) = outcome {
                outcomes.insert(outcome.listing.clone(), outcome);
            }
        }
        let mut after = self.pack_status(&request.pack)?;
        for member in &mut after.members {
            if let Some(outcome) = outcomes.get(&member.listing) {
                if outcome.state == PackMemberState::Failed as i32
                    || outcome.reason == NEEDS_APPROVAL
                {
                    member.state = outcome.state;
                    member.reason = outcome.reason.clone();
                }
            }
        }
        let change = PackChange {
            kind: PackChangeKind::Updated,
            pack: request.pack.clone(),
            revision: kept.revision,
            held: latest,
            members: after.members.clone(),
            by: crate::store::Author {
                note: request.note.clone(),
                ..author(envelope)
            },
            at_ns: self.clock.now_ns(),
        };
        self.store
            .record_pack_change(&change)
            .map_err(|failed| failed.to_string())?;
        tracing::info!(
            pack = change.pack,
            by = change.by.by,
            "a starter pack updated"
        );
        Ok(after)
    }

    /// A launched member's instances, each moved to the newest version its
    /// source registry offers at or above the member's minimum.
    fn update_launched(
        &self,
        member: &PackMember,
        status: &PackMemberStatus,
        listings: &[PluginListing],
        snapshot: &Snapshot,
        note: &str,
        envelope: &Envelope,
    ) -> Option<PackMemberStatus> {
        let mut outcome: Option<PackMemberStatus> = None;
        for instance in &status.instance_ids {
            let Some(launch) = snapshot.catalogue.launches.iter().find(|l| {
                &l.instance_id == instance && l.state == PluginLaunchState::Launched as i32
            }) else {
                continue;
            };
            let source = snapshot
                .catalogue
                .versions
                .iter()
                .find(|v| {
                    v.listing == member.listing
                        && v.metadata
                            .as_ref()
                            .is_some_and(|m| m.version == launch.version)
                })
                .map(|v| v.source_registry.clone())
                .unwrap_or_else(|| member.registry.clone());
            let Ok(target) = resolve(listings, member, &source) else {
                continue;
            };
            if !newer(&target.version, &launch.version)
                || (is_prerelease(&target.version) && !is_prerelease(&launch.version))
            {
                continue;
            }
            let roles = roles_of(&target);
            let widening = roles.iter().any(|role| !launch.roles.contains(role));
            let said = if widening {
                PackMemberStatus {
                    state: PackMemberState::Launched as i32,
                    reason: NEEDS_APPROVAL.into(),
                    ..status.clone()
                }
            } else {
                match self.update(
                    UpdatePluginRequest {
                        instance_id: instance.clone(),
                        listing: member.listing.clone(),
                        version: target.version.clone(),
                        approved_roles: roles,
                        note: note.to_string(),
                    },
                    envelope,
                ) {
                    Ok(moved) => PackMemberStatus {
                        state: PackMemberState::Launched as i32,
                        installed_version: moved.version,
                        reason: String::new(),
                        ..status.clone()
                    },
                    Err(why) => PackMemberStatus {
                        state: PackMemberState::Failed as i32,
                        reason: failure(&why),
                        ..status.clone()
                    },
                }
            };
            // A failure or a held widening on any instance is what the
            // member reads.
            let keep = outcome.as_ref().is_some_and(|held| {
                held.state == PackMemberState::Failed as i32 || held.reason == NEEDS_APPROVAL
            });
            if !keep {
                outcome = Some(said);
            }
        }
        outcome
    }

    /// An installed, unlaunched member given the newer version.
    fn update_installed(
        &self,
        member: &PackMember,
        status: &PackMemberStatus,
        listings: &[PluginListing],
        pack: &str,
        note: &str,
        envelope: &Envelope,
    ) -> Option<PackMemberStatus> {
        let target = resolve(listings, member, &member.registry).ok()?;
        if !newer(&target.version, &status.installed_version) {
            return None;
        }
        Some(
            match self.install_version(
                &member.listing,
                &target.version,
                &target.registry,
                note,
                envelope,
                pack,
            ) {
                Ok(_) => PackMemberStatus {
                    installed_version: target.version.clone(),
                    reason: String::new(),
                    ..status.clone()
                },
                Err(why) => PackMemberStatus {
                    state: PackMemberState::Failed as i32,
                    reason: failure(&why),
                    ..status.clone()
                },
            },
        )
    }

    /// RemovePack (W8.19; requirement 22): only the deployment's pack
    /// record, `pack_removed` with its note; nothing stopped or
    /// uninstalled. Answered with the members as they stay.
    pub(super) fn remove_pack(
        &self,
        request: RemovePackRequest,
        envelope: &Envelope,
    ) -> Result<PackStatus, String> {
        if request.note.trim().is_empty() {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &["note".to_string()],
                "note: say why the pack's record is removed",
            ));
        }
        if let Some(refusal) = note_refused(&request.note) {
            return Err(refusal);
        }
        let Some(kept) = self.kept_pack(&request.pack)? else {
            return Err(format!(
                "{} is not installed here; there is no record to remove",
                request.pack
            ));
        };
        if self.packs.get(&request.pack).is_some() {
            return Err(format!(
                "an install of {} is under way; remove it once it ends",
                request.pack
            ));
        }
        let status = self.pack_status(&request.pack)?;
        let change = PackChange {
            kind: PackChangeKind::Removed,
            pack: request.pack.clone(),
            revision: kept.revision,
            held: kept.held,
            members: status.members.clone(),
            by: crate::store::Author {
                note: request.note.clone(),
                ..author(envelope)
            },
            at_ns: self.clock.now_ns(),
        };
        self.store
            .record_pack_change(&change)
            .map_err(|failed| failed.to_string())?;
        tracing::info!(
            pack = change.pack,
            by = change.by.by,
            "a starter pack's record removed"
        );
        Ok(status)
    }
}

#[cfg(test)]
mod tests;
