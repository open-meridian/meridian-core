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

/// The data plugin admin's stale ceilings (contract v27, W10.9; rulings 368
/// and 417): the table setting `dataset_thresholds`, read by its fixed name,
/// each row's `dataset` the dataset's key in the instance's catalogue and its
/// `stale_after` whole seconds; by key, in nanoseconds. A row that does not
/// read is passed over, never guessed at.
pub const DATASET_THRESHOLDS: &str = "dataset_thresholds";

pub fn stale_after(snapshot: &Snapshot, instance: &str) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    let Some(held) = snapshot
        .settings
        .iter()
        .find(|s| s.plugin_instance_id == instance && s.name == DATASET_THRESHOLDS)
    else {
        return out;
    };
    let crate::store::Held::Plain(value) = &held.held else {
        return out;
    };
    for row in meridian_domain::setting_table::parse(value).unwrap_or_default() {
        let key = row.cells.get("dataset").map(|k| k.trim().to_string());
        let seconds = row
            .cells
            .get("stale_after")
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|s| *s > 0);
        if let (Some(key), Some(seconds)) = (key, seconds) {
            if let Some(ns) = seconds.checked_mul(1_000_000_000) {
                out.insert(key, ns);
            }
        }
    }
    out
}

/// Every launched dataset, by its ID, with its catalogue entry.
pub fn datasets(snapshot: &Snapshot) -> Vec<DatasetRef> {
    let mut out = Vec::new();
    for (instance, catalogue) in catalogues(snapshot) {
        let ceilings = stale_after(snapshot, &instance);
        for declared in catalogue.datasets {
            let stale_after_ns = ceilings.get(&declared.key).copied().unwrap_or(0);
            out.push(DatasetRef {
                dataset: lake::dataset_id(&instance, &declared.key),
                instance: instance.clone(),
                vendor: declared.vendor.clone(),
                aggregator: declared.aggregator.clone(),
                declared_delay_ns: declared.declared_delay_ns,
                declaration: Some(declared),
                unconverted_count: 0,
                miss_count: 0,
                closed_day_count: 0,
                outside_coverage_count: 0,
                // Contract v27 (W10.9): the declared delay is the
                // catalogue's, the stale ceiling the data plugin admin's
                // `dataset_thresholds`; liveness the lake's.
                stale_after_ns,
                liveness: Vec::new(),
                // Contract v29 (619 (2)): the quality counts are the lake's,
                // as the other counts are.
                valid_count: 0,
                suspicious_count: 0,
                invalid_count: 0,
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
    // Contract v28 (W10.1; sdk-contract/a-deployment-counts-its-licensed-
    // data-use-contract): a person's licence keeps the declared terms -- a
    // part the request leaves unstated is the catalogue's declared value,
    // never cleared by an edit of another part.
    let terms = kept_declared(terms, declaration.licence_default.as_ref());
    let mut licence = DatasetLicence {
        dataset: request.dataset.clone(),
        updated_by: author.by.clone(),
        updated_at_ns: now_ns,
        acting_through_delegation: author.delegation.clone(),
        client_name: author.client.clone(),
        note: request.note.clone(),
        prepared_call_id: author.prepared_call_id.clone(),
        ..terms
    };
    // Ruling 479: what is kept carries both the terms' answer and the
    // deprecated boolean, whichever the writer stated.
    lake::derived_use_both(&mut licence);
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

/// A licence as a person states it, its unstated parts the declared terms'
/// (contract v28, W10.1): the terms version, access basis, timeliness and
/// delay, the underlying source, the meters and the display, non-display
/// and enterprise terms, the derived-use, redistribution and outside
/// computation answers, the prohibited uses, the six attribution terms,
/// the report's cadence and day, and the eligibility (491, 597).
pub fn kept_declared(
    mut terms: DatasetLicence,
    declared: Option<&DatasetLicence>,
) -> DatasetLicence {
    let Some(declared) = declared else {
        return terms;
    };
    let text = |held: &mut String, from: &String| {
        if held.is_empty() {
            *held = from.clone();
        }
    };
    let number = |held: &mut i32, from: i32| {
        if *held == 0 {
            *held = from;
        }
    };
    text(&mut terms.terms_version, &declared.terms_version);
    number(&mut terms.access_basis, declared.access_basis);
    number(&mut terms.timeliness, declared.timeliness);
    if terms.delay_minutes == 0 {
        terms.delay_minutes = declared.delay_minutes;
    }
    text(&mut terms.delay_label, &declared.delay_label);
    text(&mut terms.underlying_source, &declared.underlying_source);
    if terms.meters.is_empty() {
        terms.meters = declared.meters.clone();
    }
    if terms.display_terms.is_none() {
        terms.display_terms = declared.display_terms;
    }
    if terms.non_display.is_none() {
        terms.non_display = declared.non_display.clone();
    }
    if terms.enterprise.is_none() {
        terms.enterprise = declared.enterprise.clone();
    }
    number(&mut terms.derived_use_terms, declared.derived_use_terms);
    if terms.derived_multi_instrument_free.is_none() {
        terms.derived_multi_instrument_free = declared.derived_multi_instrument_free;
    }
    terms.derived_creation_is_non_display |= declared.derived_creation_is_non_display;
    number(&mut terms.redistribution, declared.redistribution);
    number(&mut terms.outside_computation, declared.outside_computation);
    if terms.prohibited_uses.is_empty() {
        terms.prohibited_uses = declared.prohibited_uses.clone();
    }
    terms.attribution_required |= declared.attribution_required;
    text(&mut terms.attribution_text, &declared.attribution_text);
    number(&mut terms.copyright_class, declared.copyright_class);
    terms.no_more_restrictive_terms |= declared.no_more_restrictive_terms;
    text(
        &mut terms.not_affiliated_notice,
        &declared.not_affiliated_notice,
    );
    number(&mut terms.report_cadence, declared.report_cadence);
    if terms.report_due_day == 0 {
        terms.report_due_day = declared.report_due_day;
    }
    if terms.eligibility.is_none() {
        terms.eligibility = declared.eligibility.clone();
    }
    terms
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
        prepared_call_id: author.prepared_call_id.clone(),
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
