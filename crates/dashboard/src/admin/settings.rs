//! A plugin instance's settings form (W6.11), built from what the plugin
//! declared at registration and the record the conductor keeps of it.
//!
//! A page of its own for now, under the administration pages; the admin view
//! for a plugin instance (kernel/a-plugins-admin-view) places the same form
//! beside the plugin's accounts and access, so it renders from a record and
//! a token and nothing else.
//!
//! **A secret is write-only here.** The record names which secrets are set
//! and never holds a value, so there is nothing to show: a secret's field is
//! always empty, says "set" or "not set", and replaces the value when typed
//! into. A setting held sealed is treated as secret whatever the plugin
//! declares of it now. The fields' names carry the setting's name and nothing
//! else, and nothing here logs a field.

use std::collections::HashMap;

use meridian_domain::v1::{PluginSettingValue, PluginSettingsRecord, SetPluginSettingsRequest};
use meridian_pb::v1::{SettingDeclaration, SettingType};

use crate::html::escape;

/// A secret's field, which replaces it when typed into.
pub const SECRET_FIELD: &str = "secret.";
/// A setting that is not secret, shown as it stands.
pub const VALUE_FIELD: &str = "value.";
/// Ticked, clears a secret that is set.
pub const CLEAR_FIELD: &str = "clear.";

/// Where a plugin instance's settings are. The instance goes in the path, so
/// it is escaped where it is written into a page.
pub fn path(instance: &str) -> String {
    format!("/admin/plugins/{instance}/settings")
}

fn is_secret(record: &PluginSettingsRecord, declaration: &SettingDeclaration) -> bool {
    declaration.secret || record.secrets_set.contains(&declaration.name)
}

fn current<'a>(record: &'a PluginSettingsRecord, name: &str) -> Option<&'a str> {
    record
        .values
        .iter()
        .find(|held| held.name == name)
        .map(|held| held.value.as_str())
}

fn kind(declaration: &SettingDeclaration) -> &'static str {
    match SettingType::try_from(declaration.r#type) {
        Ok(SettingType::Integer) => "whole number",
        Ok(SettingType::Boolean) => "on or off",
        _ => "text",
    }
}

fn field(declaration: &SettingDeclaration, record: &PluginSettingsRecord) -> String {
    let name = escape(&declaration.name);
    let required = if declaration.required {
        " <span class=\"pill warn\">required</span>"
    } else {
        ""
    };
    let about = if declaration.description.is_empty() {
        String::new()
    } else {
        format!("<p class=\"hint\">{}</p>", escape(&declaration.description))
    };
    if is_secret(record, declaration) {
        let set = record.secrets_set.contains(&declaration.name);
        let (state, placeholder, clear) = if set {
            (
                "<span class=\"pill good\">set</span>",
                "Type a new value to replace it",
                format!(
                    "<label><input type=\"checkbox\" name=\"{CLEAR_FIELD}{name}\">Clear it</label>"
                ),
            )
        } else {
            (
                "<span class=\"pill\">not set</span>",
                "Not set",
                String::new(),
            )
        };
        // Never a value: the record holds none, and a password field keeps
        // what was typed off the screen and out of the browser's history.
        return format!(
            "<div class=\"setting\" data-setting=\"{name}\" data-secret=\"true\">\
             <label>{name}{required} {state} <span class=\"id\">secret, {kind}</span>\
             <input type=\"password\" name=\"{SECRET_FIELD}{name}\" value=\"\" \
             autocomplete=\"new-password\" placeholder=\"{placeholder}\"></label>{clear}{about}</div>",
            kind = kind(declaration),
        );
    }
    let value = current(record, &declaration.name).unwrap_or_default();
    let input = match SettingType::try_from(declaration.r#type) {
        Ok(SettingType::Boolean) => {
            let option = |option: &str, label: &str| {
                format!(
                    "<option value=\"{option}\"{}>{label}</option>",
                    if value == option { " selected" } else { "" }
                )
            };
            format!(
                "<select name=\"{VALUE_FIELD}{name}\">{}{}{}</select>",
                option("", "Not set"),
                option("true", "On"),
                option("false", "Off")
            )
        }
        Ok(SettingType::Integer) => format!(
            "<input type=\"number\" step=\"1\" name=\"{VALUE_FIELD}{name}\" value=\"{}\">",
            escape(value)
        ),
        _ => format!(
            "<input type=\"text\" name=\"{VALUE_FIELD}{name}\" value=\"{}\">",
            escape(value)
        ),
    };
    format!(
        "<div class=\"setting\" data-setting=\"{name}\">\
         <label>{name}{required} <span class=\"id\">{kind}</span>{input}</label>{about}</div>",
        kind = kind(declaration),
    )
}

/// The form: every setting the plugin declared, in its order.
pub fn render(record: &PluginSettingsRecord, token: &str, notice: &str) -> String {
    let instance = escape(&record.plugin_instance_id);
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"passed\">{}</p>", escape(notice))
    };
    let body = if record.declared_settings.is_empty() {
        "<p class=\"empty\">This plugin declares no settings.</p>".to_string()
    } else {
        let fields: String = record
            .declared_settings
            .iter()
            .map(|declaration| field(declaration, record))
            .collect();
        format!(
            "<form method=\"post\" action=\"{action}\" class=\"settings\" autocomplete=\"off\">{token}\
             {fields}<button type=\"submit\" class=\"primary\">Save</button></form>",
            action = escape(&path(&record.plugin_instance_id)),
        )
    };
    format!(
        "<div class=\"page-head\"><h1>Settings for {instance}</h1><a href=\"/admin\">Administer</a></div>\
         <p class=\"hint\">What the plugin declared it needs. A secret is never shown again once \
         set: type a new value to replace it.</p>{notice}{body}"
    )
}

/// The change a submitted form asks for, or `None` when it asks for none.
///
/// A secret left empty is left as it is; one typed into is replaced; one
/// ticked to clear, and not typed into, is cleared. A setting that is not
/// secret is sent when it differs from what the record holds, and cleared
/// when emptied. Only settings the plugin declared are read from the form.
pub fn request(
    record: &PluginSettingsRecord,
    fields: &HashMap<String, String>,
) -> Option<SetPluginSettingsRequest> {
    let given = |prefix: &str, name: &str| {
        fields
            .get(&format!("{prefix}{name}"))
            .map(|value| value.trim())
            .unwrap_or_default()
    };
    let mut request = SetPluginSettingsRequest {
        plugin_instance_id: record.plugin_instance_id.clone(),
        ..Default::default()
    };
    for declaration in &record.declared_settings {
        let name = &declaration.name;
        if is_secret(record, declaration) {
            let typed = given(SECRET_FIELD, name);
            if !typed.is_empty() {
                request.values.push(PluginSettingValue {
                    name: name.clone(),
                    value: typed.to_string(),
                });
            } else if fields.contains_key(&format!("{CLEAR_FIELD}{name}"))
                && record.secrets_set.contains(name)
            {
                request.cleared.push(name.clone());
            }
            continue;
        }
        let typed = given(VALUE_FIELD, name);
        let held = current(record, name).unwrap_or_default();
        if typed == held {
            continue;
        }
        if typed.is_empty() {
            request.cleared.push(name.clone());
        } else {
            request.values.push(PluginSettingValue {
                name: name.clone(),
                value: typed.to_string(),
            });
        }
    }
    (!request.values.is_empty() || !request.cleared.is_empty()).then_some(request)
}
