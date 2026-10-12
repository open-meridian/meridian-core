//! A connectivity plugin's `ems`, the `ems`'s venue account map, and the
//! venue's catalogue (contract v22; W11.20 to W11.22, W4.11, W6.30).
//!
//! Ruled 2026-10-10 (the order-path plan's Q20): a deployment admin makes a
//! connectivity plugin instance available, or withdraws it; the `ems`
//! plugin's admin links an available one to their `ems` -- each to exactly
//! one, an `ems` to many -- and makes it available to the `ems`'s users.
//! Both are access, set at the dashboard's page alone. The `ems` maps its
//! accounts, each one the deployment admin granted its instance (W6.30), to
//! the venue account codes they trade under at its connectivity plugins,
//! acting for the admin viewing its page (W4.9). Each change is its own
//! record (decisions/031): a ConnectivityLinkChange or a
//! VenueAccountMapChange, kept here, which no proto carries.
//!
//! **What a plugin is told** (W4.11), beside its scope: a connectivity
//! plugin, its link, and the `ems`'s mappings on it, which are its read and
//! write scope -- none while it is withdrawn or linked to no `ems`, since it
//! holds no permission-derived scope and no links of its own; an `ems`, the
//! connectivity plugins linked to it and its map. A mapping at a plugin no
//! longer linked to the `ems` that made it stays recorded and reaches
//! nothing, so a plugin relinked elsewhere carries none of its old `ems`'s
//! accounts.
//!
//! **The catalogue** (W11.20): what a connectivity plugin publishes per venue
//! account code, kept latest per plugin and code, each change its own record,
//! and read by an `ems` or an `oms` as itself (ReadVenueCatalogue): a
//! venue's offering, no account's data, and no person's to ask for.

use std::collections::BTreeSet;

use meridian_bus::Envelope;
use meridian_domain::v1::{
    LinkConnectivityRequest, MapVenueAccountRequest, ReadVenueCatalogueReply,
    ReadVenueCatalogueRequest, SetConnectivityAvailableRequest, VenueCatalogue,
};
use meridian_pb::bounds::{
    LINK_CONNECTIVITY_REQUEST_NOTE_LENGTH, SET_CONNECTIVITY_AVAILABLE_REQUEST_NOTE_LENGTH,
};
use meridian_pb::v1::{
    connectivity_user, ConnectivityLink, ConnectivityUser, RefusalReason, VenueAccountMap,
    VenueAccountMapping,
};

use crate::store::{Author, HeldMapping, Snapshot};

pub const SET_CONNECTIVITY_AVAILABLE: &str = "platform.config.command.set-connectivity-available";
pub const LINK_CONNECTIVITY: &str = "platform.config.command.link-connectivity";
pub const MAP_VENUE_ACCOUNT: &str = "platform.config.command.map-venue-account";
pub const VENUE_CATALOGUE: &str = "platform.config.event.venue-catalogue";
pub const READ_VENUE_CATALOGUE: &str = "platform.config.query.venue-catalogue";

/// The role that reaches a venue, and the one it is linked to.
pub const CONNECTIVITY: &str = "connectivity";
pub const EMS: &str = "ems";

/// The note on the upgrade's own record of each connectivity plugin
/// (decisions/031): nothing was available or linked before v22.
pub const AT_THE_UPGRADE: &str = "at the upgrade to contract v22, for no person: no connectivity \
     plugin was available or linked before";

/// The roles an instance holds, as its sidecar last reported them or its
/// launch names them: what a link or a map is held to.
pub fn roles_of(snapshot: &Snapshot, instance: &str) -> Vec<String> {
    let reported = snapshot
        .plugins
        .iter()
        .find(|plugin| plugin.plugin_instance_id == instance)
        .map(|plugin| plugin.roles.clone());
    let launched = || {
        snapshot
            .catalogue
            .launches
            .iter()
            .rev()
            .find(|launch| launch.instance_id == instance)
            .map(|launch| launch.roles.clone())
    };
    meridian_domain::roles_now(&reported.or_else(launched).unwrap_or_default())
}

fn holds(snapshot: &Snapshot, instance: &str, role: &str) -> bool {
    roles_of(snapshot, instance).iter().any(|held| held == role)
}

/// A connectivity plugin's link as it stands: as last recorded, or
/// withdrawn and unlinked where nothing was ever recorded for it.
pub fn link_of(snapshot: &Snapshot, instance: &str) -> ConnectivityLink {
    snapshot
        .connectivity
        .get(instance)
        .cloned()
        .unwrap_or_else(|| ConnectivityLink {
            connectivity_instance_id: instance.to_string(),
            ..Default::default()
        })
}

/// The mappings that reach something: an `ems`'s, at a connectivity plugin
/// linked to that `ems` now.
fn standing(snapshot: &Snapshot) -> impl Iterator<Item = &HeldMapping> {
    snapshot.venue_mappings.iter().filter(|held| {
        snapshot
            .connectivity
            .get(&held.mapping.connectivity_instance_id)
            .is_some_and(|link| link.ems_instance_id == held.ems_instance_id)
    })
}

/// What a plugin is told of its link and map beside its scope (W4.11), and
/// the accounts its connectivity role reaches: a connectivity plugin's link,
/// its `ems`'s mappings on it, and those accounts while it is available and
/// linked; an `ems`'s connectivity plugins and its map.
pub struct Told {
    pub connectivity_link: Option<ConnectivityLink>,
    pub connectivity_links: Vec<ConnectivityLink>,
    pub venue_account_map: Option<VenueAccountMap>,
    pub mapped_accounts: BTreeSet<String>,
}

pub fn told(snapshot: &Snapshot, instance: &str) -> Told {
    let roles = roles_of(snapshot, instance);
    let mut out = Told {
        connectivity_link: None,
        connectivity_links: Vec::new(),
        venue_account_map: None,
        mapped_accounts: BTreeSet::new(),
    };
    if roles.iter().any(|role| role == CONNECTIVITY) {
        let link = link_of(snapshot, instance);
        let mappings: Vec<VenueAccountMapping> = standing(snapshot)
            .filter(|held| held.mapping.connectivity_instance_id == instance)
            .map(|held| held.mapping.clone())
            .collect();
        if link.available && !link.ems_instance_id.is_empty() {
            out.mapped_accounts = mappings
                .iter()
                .map(|mapping| mapping.account_id.clone())
                .collect();
        }
        // The codes its `ems` mapped on it, which the sidecar stamps a venue
        // fact's account by (`venue-account-map`).
        out.venue_account_map = Some(VenueAccountMap {
            ems_instance_id: link.ems_instance_id.clone(),
            mappings,
        });
        out.connectivity_link = Some(link);
    }
    if roles.iter().any(|role| role == EMS) {
        out.connectivity_links = snapshot
            .connectivity
            .values()
            .filter(|link| link.ems_instance_id == instance)
            .cloned()
            .collect();
        let mappings: Vec<VenueAccountMapping> = standing(snapshot)
            .filter(|held| held.ems_instance_id == instance)
            .map(|held| held.mapping.clone())
            .collect();
        // A plugin holding both is told its `ems`'s map, which holds the
        // mappings on itself among the rest.
        out.venue_account_map = Some(VenueAccountMap {
            ems_instance_id: instance.to_string(),
            mappings,
        });
    }
    out
}

fn note_within(note: &str, most: usize) -> Result<(), String> {
    let length = note.chars().count();
    if length > most {
        return Err(format!(
            "note: is {length} characters; at most {most}, and nothing was changed"
        ));
    }
    Ok(())
}

fn person(author: &Author, what: &str) -> Result<(), String> {
    if author.by.is_empty() {
        return Err(format!(
            "{what} is set at the dashboard's page for a person, and this is sent for nobody"
        ));
    }
    Ok(())
}

/// W11.21, a deployment admin's: the link the change makes, or the refusal.
/// The dashboard admits the person; here, the instance is one holding
/// `connectivity` the deployment knows, and the note within its bound.
pub fn availability(
    snapshot: &Snapshot,
    request: &SetConnectivityAvailableRequest,
    author: &Author,
) -> Result<ConnectivityLink, String> {
    person(author, "a connectivity plugin's availability")?;
    let instance = request.connectivity_instance_id.trim();
    if instance.is_empty() {
        return Err("connectivity_instance_id: is required, and nothing was changed".into());
    }
    if !holds(snapshot, instance, CONNECTIVITY) {
        return Err(format!(
            "connectivity_instance_id: {instance} is no instance holding connectivity that \
             this deployment knows, and nothing was changed"
        ));
    }
    note_within(
        &request.note,
        SET_CONNECTIVITY_AVAILABLE_REQUEST_NOTE_LENGTH.most,
    )?;
    let mut link = link_of(snapshot, instance);
    link.available = request.available;
    Ok(link)
}

/// W11.21, the `ems` plugin's admin's: the link the change makes, or the
/// refusal -- an instance not available, one linked to another `ems`
/// (REFUSAL_REASON_CONNECTIVITY_LINKED), an `ems` that is none, a user
/// naming nobody. An empty `ems_instance_id` unlinks it from the `ems` it
/// is linked to, and its users go with the link.
pub fn linked(
    snapshot: &Snapshot,
    request: &LinkConnectivityRequest,
    author: &Author,
) -> Result<ConnectivityLink, String> {
    person(author, "a connectivity plugin's link")?;
    let instance = request.connectivity_instance_id.trim();
    if instance.is_empty() {
        return Err("connectivity_instance_id: is required, and nothing was changed".into());
    }
    if !holds(snapshot, instance, CONNECTIVITY) {
        return Err(format!(
            "connectivity_instance_id: {instance} is no instance holding connectivity that \
             this deployment knows, and nothing was changed"
        ));
    }
    note_within(&request.note, LINK_CONNECTIVITY_REQUEST_NOTE_LENGTH.most)?;
    let mut link = link_of(snapshot, instance);
    let ems = request.ems_instance_id.trim();
    if ems.is_empty() {
        link.ems_instance_id.clear();
        link.users.clear();
        return Ok(link);
    }
    if !holds(snapshot, ems, EMS) {
        return Err(format!(
            "ems_instance_id: {ems} is no instance holding ems that this deployment knows, and \
             nothing was changed"
        ));
    }
    if !link.available {
        return Err(format!(
            "connectivity_instance_id: {instance} is not available: a deployment admin makes a \
             connectivity plugin available before an ems's admin links it, and nothing was \
             changed"
        ));
    }
    if !link.ems_instance_id.is_empty() && link.ems_instance_id != ems {
        return Err(meridian_bus::refusal_naming(
            RefusalReason::ConnectivityLinked as i32,
            &["connectivity_instance_id".to_string()],
            format!(
                "{instance} is linked to {} already, and each connectivity plugin is linked to \
                 exactly one ems: its admin unlinks it first; nothing was changed",
                link.ems_instance_id
            ),
        ));
    }
    let mut users: Vec<ConnectivityUser> = Vec::new();
    for (i, user) in request.users.iter().enumerate() {
        let named = match &user.user {
            Some(connectivity_user::User::Subject(subject)) => subject.trim().to_string(),
            Some(connectivity_user::User::ServiceAccountId(id)) => id.trim().to_string(),
            Some(connectivity_user::User::UserGroupId(id)) => {
                if !snapshot
                    .records
                    .user_groups
                    .iter()
                    .any(|group| &group.user_group_id == id)
                {
                    return Err(format!(
                        "users[{i}].user_group_id: there is no user group {id}, and nothing was \
                         changed"
                    ));
                }
                id.clone()
            }
            None => String::new(),
        };
        if named.is_empty() {
            return Err(format!(
                "users[{i}]: names nobody: a person, a service account or a user group, and \
                 nothing was changed"
            ));
        }
        if !users.contains(user) {
            users.push(user.clone());
        }
    }
    link.ems_instance_id = ems.to_string();
    link.users = users;
    Ok(link)
}

/// W11.22, the `ems`'s for its admin: the mapping the change makes -- the
/// code's account, or none where it is removed -- with the mapping it
/// replaces; or the refusal: a connectivity plugin not linked to this `ems`
/// (REFUSAL_REASON_CONNECTIVITY_NOT_ALLOWED), an account not granted its
/// instance (REFUSAL_REASON_ACCOUNT_NOT_LINKABLE), a code mapped on that
/// plugin to another account already.
pub fn mapped(
    snapshot: &Snapshot,
    request: &MapVenueAccountRequest,
    ems: &str,
    author: &Author,
) -> Result<(Option<VenueAccountMapping>, Option<VenueAccountMapping>), String> {
    if author.by.is_empty() {
        return Err(
            "a venue account mapping is an ems's admin's, and this is sent for nobody".into(),
        );
    }
    let instance = request.connectivity_instance_id.trim();
    let code = request.venue_account_code.trim();
    if instance.is_empty() {
        return Err("connectivity_instance_id: is required, and nothing was changed".into());
    }
    if code.is_empty() {
        return Err("venue_account_code: is required, and nothing was changed".into());
    }
    if link_of(snapshot, instance).ems_instance_id != ems {
        return Err(meridian_bus::refusal_naming(
            RefusalReason::ConnectivityNotAllowed as i32,
            &["connectivity_instance_id".to_string()],
            format!(
                "{instance} is not linked to {ems}: an ems maps accounts only at its own \
                 connectivity plugins; nothing was changed"
            ),
        ));
    }
    let before = snapshot
        .venue_mappings
        .iter()
        .find(|held| {
            held.mapping.connectivity_instance_id == instance
                && held.mapping.venue_account_code == code
        })
        .cloned();
    let account = request.account_id.trim();
    if account.is_empty() {
        return Ok((before.map(|held| held.mapping), None));
    }
    let granted = snapshot
        .linkable
        .get(ems)
        .is_some_and(|linkable| linkable.account_ids.iter().any(|held| held == account));
    if !granted {
        return Err(meridian_bus::refusal_naming(
            RefusalReason::AccountNotLinkable as i32,
            &["account_id".to_string()],
            format!(
                "account {account} is not one a deployment admin granted {ems} to map; nothing \
                 was mapped: ask a deployment admin to grant it on the plugin's Accounts tab"
            ),
        ));
    }
    if let Some(held) = &before {
        // The code at this plugin is the `ems`'s own mapping, or another's
        // made before the plugin was relinked, which reaches nothing and is
        // replaced.
        let theirs = held.ems_instance_id == ems;
        if theirs && held.mapping.account_id != account {
            return Err(format!(
                "already exists: venue_account_code {code} is mapped on {instance} to {} \
                 already; remove that mapping first, and nothing was changed",
                held.mapping.account_id
            ));
        }
    }
    let to = VenueAccountMapping {
        connectivity_instance_id: instance.to_string(),
        venue_account_code: code.to_string(),
        account_id: account.to_string(),
    };
    Ok((before.map(|held| held.mapping), Some(to)))
}

/// A catalogue as published, held to the stamp: its plugin is the
/// publisher's, whatever the plugin wrote, and a code is named.
pub fn catalogue_heard(
    envelope: &Envelope,
    catalogue: Option<VenueCatalogue>,
) -> Option<VenueCatalogue> {
    let publisher = envelope
        .meta
        .as_ref()
        .map(|meta| meta.publisher_instance_id.clone())
        .unwrap_or_default();
    let mut catalogue = catalogue?;
    if publisher.is_empty() || catalogue.venue_account_code.trim().is_empty() {
        return None;
    }
    catalogue.connectivity_instance_id = publisher;
    Some(catalogue)
}

/// W11.20: the latest catalogue for a plugin and code, or an empty one for a
/// code the plugin has not published, never refused.
pub fn read_answer(
    held: Option<VenueCatalogue>,
    request: &ReadVenueCatalogueRequest,
) -> Result<ReadVenueCatalogueReply, String> {
    if request.connectivity_instance_id.trim().is_empty() {
        return Err("connectivity_instance_id: is required".into());
    }
    if request.venue_account_code.trim().is_empty() {
        return Err("venue_account_code: is required".into());
    }
    Ok(ReadVenueCatalogueReply {
        catalogue: Some(held.unwrap_or_else(|| VenueCatalogue {
            connectivity_instance_id: request.connectivity_instance_id.clone(),
            venue_account_code: request.venue_account_code.clone(),
            ..Default::default()
        })),
    })
}

#[cfg(test)]
mod tests;
