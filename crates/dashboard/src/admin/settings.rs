//! A plugin instance's settings form (W6.11), built from what the plugin
//! declared at registration and the record the conductor keeps of it.
//!
//! Placed in the admin view for the plugin instance
//! (kernel/a-plugins-admin-view) beside its health and who has access, so it
//! renders from a record and a token and nothing else.
//!
//! **It says what to fill in** (the product owner, 2026-09-28): each field
//! under its label, marked required or optional; a choice as radio buttons,
//! first where it decides which fields follow; a field that applies only
//! under another setting's value shown only while it does, by a small script,
//! and with a note saying when, so without the script every field is shown
//! and the note says which to skip; a default greyed in its empty field and
//! never stored, or for a required choice, which has no unset option, its
//! option shown chosen and stored when the form is saved; a unit beside a
//! number. A required setting declaring a default is never missing. A
//! setting declared for whoever develops the plugin is shown only on a
//! development deployment, and a form posted anywhere else never changes one.
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

/// Where a plugin instance's settings are posted. The instance goes in the
/// path, so it is escaped where it is written into a page.
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

fn kind(declaration: &SettingDeclaration) -> SettingType {
    SettingType::try_from(declaration.r#type).unwrap_or(SettingType::String)
}

/// What the form calls a setting: its label, or its name when it has none.
pub fn label(declaration: &SettingDeclaration) -> &str {
    if declaration.label.is_empty() {
        &declaration.name
    } else {
        &declaration.label
    }
}

/// Whether the form shows a setting at all: a developer's only on a
/// development deployment.
fn shown(declaration: &SettingDeclaration, development: bool) -> bool {
    development || !declaration.developer
}

/// The value a setting holds, or while it holds none, its declared default.
/// A secret's is never known here, and is never what a condition names.
fn effective<'a>(
    record: &'a PluginSettingsRecord,
    declared: &'a [SettingDeclaration],
    name: &str,
) -> Option<&'a str> {
    current(record, name)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            declared
                .iter()
                .find(|declaration| declaration.name == name)
                .map(|declaration| declaration.default_value.as_str())
                .filter(|default| !default.is_empty())
        })
}

/// Whether a setting applies as the record stands: always, unless it names
/// another setting it applies under, and then only while that one holds one
/// of the values named (W4.8). The sidecar tells its plugin the same.
pub fn applies(record: &PluginSettingsRecord, declaration: &SettingDeclaration) -> bool {
    let Some(condition) = &declaration.applies_when else {
        return true;
    };
    effective(record, &record.declared_settings, &condition.setting)
        .is_some_and(|value| condition.one_of.iter().any(|one| one == value))
}

/// The required settings, of those that apply, that hold no value and
/// declare no default: what a deployment admin still has to fill in. A
/// default is the value the plugin uses while none is set (W6.11), so a
/// required setting declaring one is never missing. The Plugins tab and the
/// admin view say what a plugin needs from this alone.
pub fn missing(record: &PluginSettingsRecord, development: bool) -> Vec<&SettingDeclaration> {
    record
        .declared_settings
        .iter()
        .filter(|declaration| declaration.required && shown(declaration, development))
        .filter(|declaration| declaration.default_value.is_empty())
        .filter(|declaration| applies(record, declaration))
        .filter(|declaration| {
            !record.secrets_set.contains(&declaration.name)
                && current(record, &declaration.name).is_none_or(str::is_empty)
        })
        .collect()
}

/// The settings in the order the form asks for them: first a choice another
/// setting's condition names, since it decides which fields follow; then
/// the rest, as the plugin declared them.
fn in_order(declared: &[SettingDeclaration]) -> Vec<&SettingDeclaration> {
    let decides = |declaration: &SettingDeclaration| {
        declared.iter().any(|other| {
            other
                .applies_when
                .as_ref()
                .is_some_and(|condition| condition.setting == declaration.name)
        })
    };
    let (first, rest): (Vec<_>, Vec<_>) = declared.iter().partition(|d| decides(d));
    first.into_iter().chain(rest).collect()
}

/// A choice's option by its value, as the form shows it.
fn option_label<'a>(declaration: &'a SettingDeclaration, value: &'a str) -> &'a str {
    declaration
        .choices
        .iter()
        .find(|choice| choice.value == value)
        .map(|choice| {
            if choice.label.is_empty() {
                choice.value.as_str()
            } else {
                choice.label.as_str()
            }
        })
        .unwrap_or(value)
}

/// "Only when Key is Commercial key.": when a conditional setting applies,
/// said beside it whether or not the script hides it.
fn condition_note(declaration: &SettingDeclaration, declared: &[SettingDeclaration]) -> String {
    let Some(condition) = &declaration.applies_when else {
        return String::new();
    };
    let named = declared
        .iter()
        .find(|other| other.name == condition.setting);
    let what = named.map(label).unwrap_or(&condition.setting);
    let values: Vec<String> = condition
        .one_of
        .iter()
        .map(|value| match named {
            Some(named) => option_label(named, value).to_string(),
            None => value.clone(),
        })
        .collect();
    format!(
        "<p class=\"hint applies\">Only when {} is {}.</p>",
        escape(what),
        escape(&values.join(" or "))
    )
}

/// The head of a field: its label, required or optional, and its name small.
fn heading(declaration: &SettingDeclaration, extra: &str) -> String {
    let need = if declaration.required {
        "<span class=\"badge warn\">Required</span>"
    } else {
        "<span class=\"badge\">Optional</span>"
    };
    let developer = if declaration.developer {
        " <span class=\"badge info\">Developer</span>"
    } else {
        ""
    };
    format!(
        "<span class=\"setting-head\"><span class=\"setting-label\">{}</span> {need}{developer}{extra}\
         <span class=\"id\">{}</span></span>",
        escape(label(declaration)),
        escape(&declaration.name)
    )
}

fn about(declaration: &SettingDeclaration) -> String {
    if declaration.description.is_empty() {
        String::new()
    } else {
        format!("<p class=\"hint\">{}</p>", escape(&declaration.description))
    }
}

/// The wrapper every field sits in, carrying what the script reads.
fn holder(
    declaration: &SettingDeclaration,
    secret: bool,
    declared: &[SettingDeclaration],
    inner: String,
) -> String {
    let condition = declaration
        .applies_when
        .as_ref()
        .map(|condition| {
            format!(
                " data-applies-setting=\"{}\" data-applies-one-of=\"{}\"",
                escape(&condition.setting),
                escape(&serde_json::to_string(&condition.one_of).unwrap_or_default())
            )
        })
        .unwrap_or_default();
    let default = if declaration.default_value.is_empty() {
        String::new()
    } else {
        format!(" data-default=\"{}\"", escape(&declaration.default_value))
    };
    format!(
        "<div class=\"setting\" data-setting=\"{name}\"{secret}{condition}{default}>{inner}{note}{about}</div>",
        name = escape(&declaration.name),
        secret = if secret { " data-secret=\"true\"" } else { "" },
        note = condition_note(declaration, declared),
        about = about(declaration),
    )
}

fn secret_field(declaration: &SettingDeclaration, record: &PluginSettingsRecord) -> String {
    let name = escape(&declaration.name);
    let set = record.secrets_set.contains(&declaration.name);
    let (state, placeholder, clear) = if set {
        (
            " <span class=\"badge good\">set</span>",
            "Type a new value to replace it",
            format!(
                "<label class=\"check\"><input type=\"checkbox\" name=\"{CLEAR_FIELD}{name}\"> Clear it</label>"
            ),
        )
    } else {
        (
            " <span class=\"badge\">not set</span>",
            "Not set",
            String::new(),
        )
    };
    // Never a value: the record holds none, and a password field keeps
    // what was typed off the screen and out of the browser's history.
    format!(
        "<label class=\"field\">{head}\
         <input type=\"password\" name=\"{SECRET_FIELD}{name}\" value=\"\" \
         autocomplete=\"new-password\" placeholder=\"{placeholder}\"></label>{clear}\
         <p class=\"hint\">A secret: never shown again once set.</p>",
        head = heading(declaration, state),
    )
}

fn choice_field(declaration: &SettingDeclaration, record: &PluginSettingsRecord) -> String {
    let name = escape(&declaration.name);
    // A required choice has no unset option, so while nothing is saved the
    // one its plugin uses, its default, is shown chosen. It is saved only
    // when the form is.
    let held = current(record, &declaration.name)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            declaration
                .required
                .then_some(declaration.default_value.as_str())
        })
        .unwrap_or_default();
    let radio = |value: &str, label: &str, description: &str| {
        format!(
            "<label class=\"option\"><input type=\"radio\" name=\"{VALUE_FIELD}{name}\" value=\"{v}\"{c}>\
             <span><span class=\"option-label\">{l}</span>{d}</span></label>",
            v = escape(value),
            c = if held == value { " checked" } else { "" },
            l = escape(label),
            d = if description.is_empty() {
                String::new()
            } else {
                format!("<span class=\"hint\">{}</span>", escape(description))
            },
        )
    };
    let mut options = String::new();
    // An optional choice can be left unset, and says what the plugin does then.
    if !declaration.required {
        let then = if declaration.default_value.is_empty() {
            String::new()
        } else {
            format!(
                "The plugin uses {}.",
                option_label(declaration, &declaration.default_value)
            )
        };
        options.push_str(&radio("", "Not set", &then));
    }
    for choice in &declaration.choices {
        options.push_str(&radio(
            &choice.value,
            if choice.label.is_empty() {
                &choice.value
            } else {
                &choice.label
            },
            &choice.description,
        ));
    }
    format!(
        "<fieldset class=\"choice\"><legend>{}</legend><div class=\"options\">{options}</div></fieldset>",
        heading(declaration, "")
    )
}

fn value_field(declaration: &SettingDeclaration, record: &PluginSettingsRecord) -> String {
    let name = escape(&declaration.name);
    let value = current(record, &declaration.name).unwrap_or_default();
    let default = &declaration.default_value;
    let placeholder = if default.is_empty() {
        String::new()
    } else {
        format!(" placeholder=\"{}\"", escape(default))
    };
    let input = match kind(declaration) {
        SettingType::Boolean => {
            let option = |option: &str, label: &str| {
                format!(
                    "<option value=\"{option}\"{}>{label}</option>",
                    if value == option { " selected" } else { "" }
                )
            };
            let unset = match default.as_str() {
                "true" => "Not set (on)",
                "false" => "Not set (off)",
                _ => "Not set",
            };
            format!(
                "<select name=\"{VALUE_FIELD}{name}\">{}{}{}</select>",
                option("", unset),
                option("true", "On"),
                option("false", "Off")
            )
        }
        SettingType::Integer => format!(
            "<input type=\"number\" step=\"1\" name=\"{VALUE_FIELD}{name}\" value=\"{}\"{placeholder}>",
            escape(value)
        ),
        _ => format!(
            "<input type=\"text\" name=\"{VALUE_FIELD}{name}\" value=\"{}\"{placeholder}>",
            escape(value)
        ),
    };
    let input = if declaration.unit.is_empty() {
        input
    } else {
        format!(
            "<span class=\"with-unit\">{input}<span class=\"unit\">{}</span></span>",
            escape(&declaration.unit)
        )
    };
    let fallback = if default.is_empty() {
        String::new()
    } else {
        let shown = match kind(declaration) {
            SettingType::Boolean if default == "true" => "on".to_string(),
            SettingType::Boolean if default == "false" => "off".to_string(),
            _ if declaration.unit.is_empty() => default.clone(),
            _ => format!("{default} {}", declaration.unit),
        };
        format!(
            "<p class=\"hint\">Left empty, the plugin uses {}.</p>",
            escape(&shown)
        )
    };
    format!(
        "<label class=\"field\">{}{input}</label>{fallback}",
        heading(declaration, "")
    )
}

fn field(
    declaration: &SettingDeclaration,
    record: &PluginSettingsRecord,
    declared: &[SettingDeclaration],
) -> String {
    let secret = is_secret(record, declaration);
    let inner = if secret {
        secret_field(declaration, record)
    } else if kind(declaration) == SettingType::Choice {
        choice_field(declaration, record)
    } else {
        value_field(declaration, record)
    };
    holder(declaration, secret, declared, inner)
}

/// The form: every setting the plugin declared that this deployment shows,
/// deciding choices first.
pub fn form(record: &PluginSettingsRecord, token: &str, development: bool) -> String {
    let declared = &record.declared_settings;
    let fields: String = in_order(declared)
        .into_iter()
        .filter(|declaration| shown(declaration, development))
        .map(|declaration| field(declaration, record, declared))
        .collect();
    if fields.is_empty() {
        return "<p class=\"empty\">This plugin declares no settings.</p>".to_string();
    }
    format!(
        "<form method=\"post\" action=\"{action}\" class=\"settings\" autocomplete=\"off\" data-settings>{token}\
         {fields}<div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Save settings</button></div>\
         </form><script>{SCRIPT}</script>",
        action = escape(&path(&record.plugin_instance_id)),
    )
}

/// Shows a conditional field only while the setting it names holds one of
/// its values: the radio chosen, what is typed, or else that setting's
/// default. Hidden, its inputs are still posted as they stand, which changes
/// nothing. Without this every field shows, each with its note.
const SCRIPT: &str = r#"(function () {
  var form = document.querySelector("form[data-settings]");
  if (!form) return;
  function value(name) {
    var field = form.elements["value." + name];
    if (field && field.value) return field.value;
    var holder = form.querySelector('[data-setting="' + name + '"]');
    return holder ? holder.getAttribute("data-default") || "" : "";
  }
  function update() {
    form.querySelectorAll("[data-applies-setting]").forEach(function (holder) {
      var oneOf = JSON.parse(holder.getAttribute("data-applies-one-of") || "[]");
      holder.hidden = oneOf.indexOf(value(holder.getAttribute("data-applies-setting"))) < 0;
    });
  }
  form.addEventListener("change", update);
  form.addEventListener("input", update);
  update();
})();"#;

/// The change a submitted form asks for, or `None` when it asks for none.
///
/// A secret left empty is left as it is; one typed into is replaced; one
/// ticked to clear, and not typed into, is cleared. A setting that is not
/// secret is sent when it differs from what the record holds, and cleared
/// when emptied; one the form did not post is left as it is. Only settings
/// the plugin declared, and this deployment shows, are read from the form.
pub fn request(
    record: &PluginSettingsRecord,
    fields: &HashMap<String, String>,
    development: bool,
) -> Option<SetPluginSettingsRequest> {
    let given = |prefix: &str, name: &str| {
        fields
            .get(&format!("{prefix}{name}"))
            .map(|value| value.trim())
    };
    let mut request = SetPluginSettingsRequest {
        plugin_instance_id: record.plugin_instance_id.clone(),
        ..Default::default()
    };
    for declaration in &record.declared_settings {
        if !shown(declaration, development) {
            continue;
        }
        let name = &declaration.name;
        if is_secret(record, declaration) {
            let typed = given(SECRET_FIELD, name).unwrap_or_default();
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
        let Some(typed) = given(VALUE_FIELD, name) else {
            continue;
        };
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
