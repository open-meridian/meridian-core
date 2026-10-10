//! The deployment's data configuration (contract v18, W10.1; spec/the-lake,
//! requirements 12 to 14): each launched dataset with its catalogue entry,
//! each dataset's licence and each entitlement to it.
//!
//! A deployment admin licenses a dataset -- confirming or replacing the terms
//! its catalogue declares -- and entitles a plugin instance to it, every field
//! or some, or withdraws the entitlement; each change is its own record, its
//! note kept with it (decisions/031). The conductor keeps both beside the
//! plugins' configuration and publishes the configuration whole as
//! EntitlementsChanged: on every change, on its start, when a catalogue
//! changes, and every minute besides, so a lake, a sidecar or a launcher that
//! started since hears it. None of it reaches the platform.
//!
//! **A dataset is launched** when its instance's catalogue is known -- from
//! its sidecar's report, or from the version a live launch runs -- and the
//! instance runs: a live launch, or, for an instance never launched from the
//! catalogue (the chart's, a harness's), one whose sidecar has reported. A
//! stopped instance's datasets leave the configuration; their licences and
//! entitlements are kept for when it runs again.

use std::collections::BTreeMap;

use meridian_domain::lake;
use meridian_domain::v1::{
    DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, PluginLaunchState,
    SetDatasetEntitlementRequest, SetDatasetLicenceRequest,
};
use meridian_pb::bounds::SET_DATASET_ENTITLEMENT_REQUEST_FIELDS_COUNT;
use meridian_pb::v1::{Catalogue, DatasetDeclaration, DatasetLicence};

use crate::store::{note_refused, Author, Snapshot, Store};

pub const SET_DATASET_LICENCE: &str = "platform.config.command.set-dataset-licence";
pub const SET_DATASET_ENTITLEMENT: &str = "platform.config.command.set-dataset-entitlement";
pub const ENTITLEMENTS_CHANGED: &str = "platform.config.event.entitlements-changed";

/// How often the configuration is published whole besides on a change: the
/// recovery path for whoever started since.
pub const REPUBLISH_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Each running instance's catalogue: as its sidecar reported it, or as the
/// version its live launch runs declares it.
pub fn catalogues(snapshot: &Snapshot) -> BTreeMap<String, Catalogue> {
    let launches = &snapshot.catalogue.launches;
    let live = |instance: &str| {
        launches
            .iter()
            .any(|l| l.instance_id == instance && l.state == PluginLaunchState::Launched as i32)
    };
    let never_launched = |instance: &str| launches.iter().all(|l| l.instance_id != instance);
    let mut running: BTreeMap<String, Catalogue> = BTreeMap::new();
    for launch in launches
        .iter()
        .filter(|l| l.state == PluginLaunchState::Launched as i32)
    {
        let declared = snapshot
            .catalogue
            .versions
            .iter()
            .find(|v| {
                v.metadata
                    .as_ref()
                    .is_some_and(|m| m.name == launch.name && m.version == launch.version)
            })
            .and_then(|v| v.metadata.as_ref())
            .and_then(|m| m.declaration.as_ref())
            .and_then(|d| d.catalogue.clone());
        if let Some(catalogue) = declared {
            running.insert(launch.instance_id.clone(), catalogue);
        }
    }
    for (instance, catalogue) in &snapshot.catalogues {
        let reported = snapshot
            .plugins
            .iter()
            .any(|p| &p.plugin_instance_id == instance);
        if live(instance) || (reported && never_launched(instance)) {
            running.insert(instance.clone(), catalogue.clone());
        }
    }
    running.retain(|_, catalogue| !catalogue.datasets.is_empty());
    running
}

/// Every launched dataset, by its ID, with its catalogue entry.
pub fn datasets(snapshot: &Snapshot) -> Vec<DatasetRef> {
    let mut out = Vec::new();
    for (instance, catalogue) in catalogues(snapshot) {
        for declared in catalogue.datasets {
            out.push(DatasetRef {
                dataset: lake::dataset_id(&instance, &declared.key),
                instance: instance.clone(),
                vendor: declared.vendor.clone(),
                aggregator: declared.aggregator.clone(),
                declaration: Some(declared),
                unconverted_count: 0,
                miss_count: 0,
            });
        }
    }
    out
}

/// The configuration whole, as EntitlementsChanged carries it.
pub fn configuration(snapshot: &Snapshot, now_ns: i64) -> EntitlementsChangedEvent {
    EntitlementsChangedEvent {
        datasets: datasets(snapshot),
        licences: snapshot.licences.clone(),
        entitlements: snapshot.entitlements.clone(),
        changed_at_ns: now_ns,
    }
}

fn declared<'a>(launched: &'a [DatasetRef], dataset: &str) -> Option<&'a DatasetDeclaration> {
    launched
        .iter()
        .find(|d| d.dataset == dataset)
        .and_then(|d| d.declaration.as_ref())
}

fn person(author: &Author, what: &str) -> Result<(), String> {
    if author.by.is_empty() {
        return Err(format!(
            "{what} is a deployment admin's to set, and this is sent for nobody"
        ));
    }
    Ok(())
}

/// A dataset licensed (W10.1): its terms held to their bounds and to the
/// dataset's data types, recorded as its own record, and answered as kept.
pub fn set_licence(
    store: &dyn Store,
    snapshot: &Snapshot,
    request: &SetDatasetLicenceRequest,
    author: &Author,
    now_ns: i64,
) -> Result<DatasetLicence, String> {
    person(author, "a dataset's licence")?;
    let launched = datasets(snapshot);
    let Some(declaration) = declared(&launched, &request.dataset) else {
        return Err(format!(
            "dataset: {:?} is no dataset a launched instance's catalogue declares, and nothing \
             was changed",
            request.dataset
        ));
    };
    let terms = request.licence.clone().unwrap_or_default();
    if let Some(refused) = lake::licence_refused(&terms, Some(declaration), "licence") {
        return Err(format!("{refused}, and nothing was changed"));
    }
    if let Some(refused) = note_refused(&request.note) {
        return Err(refused);
    }
    let licence = DatasetLicence {
        dataset: request.dataset.clone(),
        updated_by: author.by.clone(),
        updated_at_ns: now_ns,
        acting_through_delegation: author.delegation.clone(),
        client_name: author.client.clone(),
        note: request.note.clone(),
        ..terms
    };
    store
        .set_dataset_licence(&licence)
        .map_err(|failed| failed.to_string())?;
    tracing::info!(
        dataset = licence.dataset,
        kept = licence.kept,
        retention_days = licence.retention_days,
        by = licence.updated_by,
        "a dataset licensed"
    );
    Ok(licence)
}

/// An instance entitled to a dataset, or its entitlement withdrawn (W10.1).
pub fn set_entitlement(
    store: &dyn Store,
    snapshot: &Snapshot,
    request: &SetDatasetEntitlementRequest,
    author: &Author,
    now_ns: i64,
) -> Result<DatasetEntitlement, String> {
    person(author, "an entitlement")?;
    let launched = datasets(snapshot);
    let Some(declaration) = declared(&launched, &request.dataset) else {
        return Err(format!(
            "dataset: {:?} is no dataset a launched instance's catalogue declares, and nothing \
             was changed",
            request.dataset
        ));
    };
    let running = snapshot
        .plugins
        .iter()
        .any(|p| p.plugin_instance_id == request.instance)
        || snapshot.catalogue.launches.iter().any(|l| {
            l.instance_id == request.instance && l.state == PluginLaunchState::Launched as i32
        });
    if !running {
        return Err(format!(
            "instance: {:?} is no instance launched in this deployment, and nothing was changed",
            request.instance
        ));
    }
    if !SET_DATASET_ENTITLEMENT_REQUEST_FIELDS_COUNT.admits(request.fields.len()) {
        return Err(format!(
            "fields names {}; at most {}, and nothing was changed",
            request.fields.len(),
            SET_DATASET_ENTITLEMENT_REQUEST_FIELDS_COUNT.most
        ));
    }
    for (i, field) in request.fields.iter().enumerate() {
        if !lake::is_field_of(declaration, field) {
            return Err(format!(
                "fields[{i}]: {field:?} is not an entry of the dataset's data types, and nothing \
                 was changed"
            ));
        }
    }
    if let Some(refused) = note_refused(&request.note) {
        return Err(refused);
    }
    let entitlement = DatasetEntitlement {
        dataset: request.dataset.clone(),
        instance: request.instance.clone(),
        allowed: request.allowed,
        fields: if request.allowed {
            request.fields.clone()
        } else {
            Vec::new()
        },
        updated_by: author.by.clone(),
        updated_at_ns: now_ns,
        acting_through_delegation: author.delegation.clone(),
        client_name: author.client.clone(),
        note: request.note.clone(),
    };
    store
        .set_dataset_entitlement(&entitlement)
        .map_err(|failed| failed.to_string())?;
    tracing::info!(
        dataset = entitlement.dataset,
        instance = entitlement.instance,
        allowed = entitlement.allowed,
        by = entitlement.updated_by,
        "an entitlement set"
    );
    Ok(entitlement)
}

/// Publish the configuration whole, from a handler's thread: after a launch
/// or a stop moves which datasets run.
pub fn publish(bus: &meridian_bus::Bus, store: &dyn Store, now_ns: i64) {
    let published = store
        .snapshot()
        .map_err(|failed| failed.to_string())
        .and_then(|snapshot| {
            use prost::Message;
            bus.publish(
                ENTITLEMENTS_CHANGED,
                "meridian.v1.EntitlementsChangedEvent",
                configuration(&snapshot, now_ns).encode_to_vec(),
                None,
                None,
            )
            .map_err(|failed| failed.to_string())
        });
    if let Err(failed) = published {
        tracing::warn!("the data configuration was not published: {failed}");
    }
}

#[cfg(test)]
mod tests;
