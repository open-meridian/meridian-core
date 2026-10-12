//! A person's own settings for a plugin (W6.31, contract v20): the values of
//! the settings a plugin declares `per_person`, kept under the person the
//! assertion names, each change its own record (`OwnSettingChange`), and
//! handed back in their claims (`own_settings`) under Open and View.
//!
//! The sidecar admits SetOwnSettings only for a person under Open with
//! `write` on a role the setting serves; here each value is checked against
//! its declaration as the conductor checks a setting -- a table's rows by
//! its columns, at most 500 -- and refused by path where it does not read.
//! Nobody else's are read or set: no other person, no deployment admin and
//! no tool of anybody else's.

use std::sync::Arc;

use meridian_domain::setting_table;
use meridian_domain::v1::Envelope;
use meridian_pb::v1::{
    SetOwnSettingsReply, SetOwnSettingsRequest, SettingDeclaration, SettingValue,
};
use prost::Message;

use super::store::OwnChange;
use crate::web::App;

/// ReadPreparedCall on the bus (W4.14).
pub const PREPARED_CALL: &str = "platform.config.query.prepared-call";
/// PluginSetsOwnSettings on the bus (W6.31).
pub const SET_OWN_SETTINGS: &str = "platform.config.command.set-own-settings";

/// The most of a person's own values handed back in the claims.
pub const MOST_BYTES: usize = 32 * 1024;

/// What the plugin declared per_person, by the records the conductor keeps.
pub fn declared(app: &App, instance: &str) -> Vec<SettingDeclaration> {
    app.records
        .current(app.clock.now_ns())
        .ok()
        .and_then(|records| {
            records
                .plugin_settings
                .iter()
                .find(|record| record.plugin_instance_id == instance)
                .map(|record| record.declared_settings.clone())
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|declaration| declaration.per_person)
        .collect()
}

/// One value checked against its declaration, as it will be kept; or the
/// refusal naming it by path.
pub fn checked_value(
    declaration: &SettingDeclaration,
    at: usize,
    value: &str,
) -> Result<String, String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    if setting_table::is_table(declaration) {
        let rows =
            setting_table::parse(value).map_err(|why| format!("values[{at}].value: {why}"))?;
        if rows.len() > setting_table::most_rows(declaration) {
            return Err(format!(
                "values[{at}].value: {} rows; {} keeps at most {}",
                rows.len(),
                declaration.name,
                setting_table::most_rows(declaration)
            ));
        }
        let cells: Vec<_> = rows.into_iter().map(|row| row.cells).collect();
        let kept = setting_table::checked(declaration, &cells).map_err(|problems| {
            problems
                .iter()
                .map(|problem| format!("{}: {}", problem.path, problem.message))
                .collect::<Vec<_>>()
                .join("; ")
        })?;
        return Ok(setting_table::cells_written(&kept));
    }
    Ok(value.to_string())
}

/// SetOwnSettings, for the person the sidecar stamped on the envelope.
pub fn set(app: &App, envelope: &Envelope) -> Result<SetOwnSettingsReply, String> {
    let meta = envelope.meta.clone().unwrap_or_default();
    let instance = meta.publisher_instance_id;
    if !crate::plugins::is_instance(&instance) {
        return Err("permission_denied: this is answered for a plugin instance's sidecar".into());
    }
    if meta.acting_for_subject.is_empty() {
        return Err(
            "permission_denied: a person's own settings are never set by a plugin as itself".into(),
        );
    }
    // A service account administers nothing and has no settings of its own
    // (contract v21): refused, as the sidecar refused it first.
    if meta.acting_for_kind == meridian_pb::v1::SubjectKind::ServiceAccount as i32 {
        return Err(
            "permission_denied: a service account administers nothing, and keeps no settings of              its own"
                .into(),
        );
    }
    let asked = SetOwnSettingsRequest::decode(&envelope.payload[..])
        .map_err(|failed| format!("invalid_argument: the change did not read: {failed}"))?;
    let declared = declared(app, &instance);
    let now = app.clock.now_ns();
    let mut changes = Vec::new();
    for (at, value) in asked.values.iter().enumerate() {
        let Some(declaration) = declared.iter().find(|d| d.name == value.name) else {
            return Err(format!(
                "invalid_argument: values[{at}].name: {:?} is no setting {instance} declares per_person",
                value.name
            ));
        };
        let kept = checked_value(declaration, at, &value.value)
            .map_err(|why| format!("invalid_argument: {why}"))?;
        changes.push(OwnChange {
            instance: instance.clone(),
            subject: meta.acting_for_subject.clone(),
            name: value.name.clone(),
            value: kept,
            delegation: meta.acting_through_delegation.clone(),
            client: if meta.acting_through_delegation.is_empty() {
                String::new()
            } else {
                meta.acting_through_client.clone()
            },
            at_ns: now,
        });
    }
    let store = app.prepared.store();
    let standing = store.own(&instance, &meta.acting_for_subject)?;
    let mut after = standing.clone();
    for change in &changes {
        if change.value.is_empty() {
            after.remove(&change.name);
        } else {
            after.insert(change.name.clone(), change.value.clone());
        }
    }
    let bytes: usize = after
        .iter()
        .map(|(name, value)| name.len() + value.len())
        .sum();
    if bytes > MOST_BYTES {
        return Err(format!(
            "invalid_argument: values: a person's own values come to {bytes} bytes; at most {MOST_BYTES}"
        ));
    }
    // A value set to what it is changes nothing and leaves no record.
    let changes: Vec<OwnChange> = changes
        .into_iter()
        .filter(|change| {
            standing.get(&change.name).map(String::as_str).unwrap_or("") != change.value
        })
        .collect();
    store.set_own(&changes)?;
    tracing::info!(
        instance,
        by = meta.acting_for_subject,
        changed = changes.len(),
        "a person's own settings saved"
    );
    Ok(SetOwnSettingsReply {
        values: after
            .into_iter()
            .map(|(name, value)| SettingValue { name, value })
            .collect(),
    })
}

/// The person's own values for the claims of their request to `instance`:
/// only what the plugin still declares per_person, within the bound.
pub async fn for_claims(app: &App, instance: &str, subject: &str) -> Vec<SettingValue> {
    let declared: Vec<String> = declared(app, instance)
        .into_iter()
        .map(|declaration| declaration.name)
        .collect();
    if declared.is_empty() {
        return Vec::new();
    }
    let (instance, subject) = (instance.to_string(), subject.to_string());
    let held = app
        .prepared
        .blocking(move |store| store.own(&instance, &subject))
        .await
        .unwrap_or_default();
    let mut bytes = 0;
    held.into_iter()
        .filter(|(name, _)| declared.contains(name))
        .take_while(|(name, value)| {
            bytes += name.len() + value.len();
            bytes <= MOST_BYTES
        })
        .map(|(name, value)| SettingValue { name, value })
        .collect()
}

/// Answer PluginSetsOwnSettings on the bus, from now on.
pub fn serve(app: Arc<App>) {
    let setting = Arc::clone(&app);
    app.bus.serve(SET_OWN_SETTINGS, move |envelope| {
        let reply = set(&setting, &envelope)?;
        Ok((
            "meridian.v1.SetOwnSettingsReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}
