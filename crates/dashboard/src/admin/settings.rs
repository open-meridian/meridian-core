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
//! and with a tag saying when, so without the script every field is shown
//! and the tag says which to skip; a default greyed in its empty field and
//! never stored, or for a required choice, which has no unset option, its
//! option shown chosen and stored when the form is saved; a unit beside a
//! number. A required setting declaring a default is never missing. A
//! setting declared for whoever develops the plugin is shown only on a
//! development deployment, and a form posted anywhere else never changes one.
//!
//! **It fits one screen** (the product owner, 2026-09-30: "compact ... and
//! make the form short enough to display in one page"): each field's label
//! on one line with small tags for required or optional, its default, "set"
//! or "not set", and when it applies; short fields, numbers, on/offs and a
//! choice nothing hangs on, two to a row on a wide screen and one on a
//! phone, and long ones, secrets and text, across; a choice's options on one
//! line; a field's hint one small line without script, and with it a bubble
//! shown while the field is focused or its marker pointed at; a developer's
//! settings under a closed "Developer"; and Save kept in view at the foot.
//! None of it changes what the form posts.
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

/// Whether another setting applies only under this one's value: a choice
/// that decides which fields follow.
fn decides(declaration: &SettingDeclaration, declared: &[SettingDeclaration]) -> bool {
    declared.iter().any(|other| {
        other
            .applies_when
            .as_ref()
            .is_some_and(|condition| condition.setting == declaration.name)
    })
}

/// The settings in the order the form asks for them: first a choice another
/// setting's condition names, since it decides which fields follow; then
/// the rest, as the plugin declared them.
fn in_order(declared: &[SettingDeclaration]) -> Vec<&SettingDeclaration> {
    let (first, rest): (Vec<_>, Vec<_>) = declared.iter().partition(|d| decides(d, declared));
    first.into_iter().chain(rest).collect()
}

/// Whether a field spans the form (the product owner, 2026-09-30: "make the
/// form short enough to display in one page"): a secret, text, or a choice
/// that decides which fields follow does; a number, an on/off or any other
/// choice is short, and sits beside another on a wide screen.
fn wide(declaration: &SettingDeclaration, secret: bool, declared: &[SettingDeclaration]) -> bool {
    if secret {
        return true;
    }
    match kind(declaration) {
        SettingType::Integer | SettingType::Boolean => false,
        SettingType::Choice => decides(declaration, declared),
        _ => true,
    }
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

/// "Only when Key type is Commercial key": when a conditional setting
/// applies, a tag beside its label whether or not the script hides it.
fn condition_tag(declaration: &SettingDeclaration, declared: &[SettingDeclaration]) -> String {
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
        " <span class=\"badge info applies\">Only when {} is {}</span>",
        escape(what),
        escape(&values.join(" or "))
    )
}

/// What the plugin uses while a setting holds nothing, as a tag: "default
/// 300 seconds". A required choice has no unset option, so its default is
/// shown as the option chosen instead.
fn default_tag(declaration: &SettingDeclaration) -> String {
    let default = &declaration.default_value;
    if default.is_empty() || (declaration.required && kind(declaration) == SettingType::Choice) {
        return String::new();
    }
    let (shown, empty) = match kind(declaration) {
        SettingType::Boolean if default == "true" => ("on".to_string(), "Left empty"),
        SettingType::Boolean if default == "false" => ("off".to_string(), "Left empty"),
        SettingType::Choice => (option_label(declaration, default).to_string(), "Not set"),
        _ if declaration.unit.is_empty() => (default.clone(), "Left empty"),
        _ => (format!("{default} {}", declaration.unit), "Left empty"),
    };
    format!(
        " <span class=\"badge\" title=\"{empty}, the plugin uses {shown}.\">default {shown}</span>",
        shown = escape(&shown)
    )
}

/// The control's `id`, for its label.
fn control_id(declaration: &SettingDeclaration) -> String {
    format!("setting-{}", escape(&declaration.name))
}

/// The hint's `id`, which the control and the marker are described by.
fn hint_id(declaration: &SettingDeclaration) -> String {
    format!("setting-{}-about", escape(&declaration.name))
}

/// A field's hint, a line each: what the plugin says of it, what each of a
/// choice's options means, and for a secret, that it is never shown again.
fn about(declaration: &SettingDeclaration, secret: bool) -> String {
    let mut lines = Vec::new();
    if !declaration.description.is_empty() {
        lines.push(declaration.description.clone());
    }
    if !secret && kind(declaration) == SettingType::Choice {
        for choice in &declaration.choices {
            if !choice.description.is_empty() {
                lines.push(format!(
                    "{}: {}",
                    option_label(declaration, &choice.value),
                    choice.description
                ));
            }
        }
    }
    if secret {
        lines.push("A secret: never shown again once set.".to_string());
    }
    lines.join("\n")
}

/// The hint under a field (the product owner, 2026-09-30: compact, not a
/// paragraph under every field): one small line without script; with it,
/// hidden, and shown in the form's one bubble while the field is focused or
/// its marker pointed at, as an account's note is.
fn hint(declaration: &SettingDeclaration, about: &str) -> String {
    if about.is_empty() {
        return String::new();
    }
    format!(
        "<p class=\"hint about\" id=\"{}\">{}</p>",
        hint_id(declaration),
        escape(about)
    )
}

/// ` aria-describedby` for a field's control, when it has a hint.
fn described(declaration: &SettingDeclaration, about: &str) -> String {
    if about.is_empty() {
        String::new()
    } else {
        format!(" aria-describedby=\"{}\"", hint_id(declaration))
    }
}

/// The head of a field, one line: its label and the marker for its hint;
/// then small tags, required or optional, what it holds or falls back to,
/// and when it applies; and its name, small.
fn heading(
    declaration: &SettingDeclaration,
    declared: &[SettingDeclaration],
    labels: bool,
    extra: &str,
    about: &str,
) -> String {
    let called = escape(label(declaration));
    let name = if labels {
        format!(
            "<label class=\"setting-label\" for=\"{}\">{called}</label>",
            control_id(declaration)
        )
    } else {
        format!("<span class=\"setting-label\">{called}</span>")
    };
    let mark = if about.is_empty() {
        String::new()
    } else {
        format!(
            "<button type=\"button\" class=\"note-mark\" aria-label=\"About {called}\" \
             aria-describedby=\"{}\"></button>",
            hint_id(declaration)
        )
    };
    let need = if declaration.required {
        "<span class=\"badge warn\">Required</span>"
    } else {
        "<span class=\"badge\">Optional</span>"
    };
    format!(
        "<span class=\"setting-head\">{name}{mark} {need}{extra}{default}{applies}\
         <span class=\"id\">{}</span></span>",
        escape(&declaration.name),
        default = default_tag(declaration),
        applies = condition_tag(declaration, declared),
    )
}

/// The wrapper every field sits in, carrying what the script reads.
fn holder(declaration: &SettingDeclaration, secret: bool, wide: bool, inner: String) -> String {
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
        "<div class=\"setting {size}\" data-setting=\"{name}\"{secret}{condition}{default}>{inner}</div>",
        size = if wide { "wide" } else { "short" },
        name = escape(&declaration.name),
        secret = if secret { " data-secret=\"true\"" } else { "" },
    )
}

fn secret_field(
    declaration: &SettingDeclaration,
    record: &PluginSettingsRecord,
    declared: &[SettingDeclaration],
    about: &str,
) -> String {
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
        "{head}<span class=\"secret-row\"><input type=\"password\" name=\"{SECRET_FIELD}{name}\" value=\"\" \
         id=\"{id}\" autocomplete=\"new-password\" placeholder=\"{placeholder}\"{described}>{clear}</span>{hint}",
        head = heading(declaration, declared, true, state, about),
        id = control_id(declaration),
        described = described(declaration, about),
        hint = hint(declaration, about),
    )
}

fn choice_field(
    declaration: &SettingDeclaration,
    record: &PluginSettingsRecord,
    declared: &[SettingDeclaration],
    about: &str,
) -> String {
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
    // On one line where they fit; what each means is in the field's hint.
    let radio = |value: &str, label: &str| {
        format!(
            "<label class=\"option\"><input type=\"radio\" name=\"{VALUE_FIELD}{name}\" value=\"{v}\"{c}>\
             <span class=\"option-label\">{l}</span></label>",
            v = escape(value),
            c = if held == value { " checked" } else { "" },
            l = escape(label),
        )
    };
    let mut options = String::new();
    // An optional choice can be left unset; its tag says what the plugin
    // uses then.
    if !declaration.required {
        options.push_str(&radio("", "Not set"));
    }
    for choice in &declaration.choices {
        options.push_str(&radio(
            &choice.value,
            option_label(declaration, &choice.value),
        ));
    }
    format!(
        "<fieldset class=\"choice\"{described}><legend>{head}</legend><div class=\"options\">{options}</div>\
         </fieldset>{hint}",
        described = described(declaration, about),
        head = heading(declaration, declared, false, "", about),
        hint = hint(declaration, about),
    )
}

fn value_field(
    declaration: &SettingDeclaration,
    record: &PluginSettingsRecord,
    declared: &[SettingDeclaration],
    about: &str,
) -> String {
    let name = escape(&declaration.name);
    let value = current(record, &declaration.name).unwrap_or_default();
    let default = &declaration.default_value;
    let placeholder = if default.is_empty() {
        String::new()
    } else {
        format!(" placeholder=\"{}\"", escape(default))
    };
    let labelled = format!(
        " id=\"{}\"{}",
        control_id(declaration),
        described(declaration, about)
    );
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
                "<select name=\"{VALUE_FIELD}{name}\"{labelled}>{}{}{}</select>",
                option("", unset),
                option("true", "On"),
                option("false", "Off")
            )
        }
        SettingType::Integer => format!(
            "<input type=\"number\" step=\"1\" name=\"{VALUE_FIELD}{name}\" value=\"{}\"{placeholder}{labelled}>",
            escape(value)
        ),
        _ => format!(
            "<input type=\"text\" name=\"{VALUE_FIELD}{name}\" value=\"{}\"{placeholder}{labelled}>",
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
    format!(
        "{}{input}{}",
        heading(declaration, declared, true, "", about),
        hint(declaration, about)
    )
}

fn field(
    declaration: &SettingDeclaration,
    record: &PluginSettingsRecord,
    declared: &[SettingDeclaration],
) -> String {
    let secret = is_secret(record, declaration);
    let about = about(declaration, secret);
    let inner = if secret {
        secret_field(declaration, record, declared, &about)
    } else if kind(declaration) == SettingType::Choice {
        choice_field(declaration, record, declared, &about)
    } else {
        value_field(declaration, record, declared, &about)
    };
    holder(
        declaration,
        secret,
        wide(declaration, secret, declared),
        inner,
    )
}

/// The form: every setting the plugin declared that this deployment shows,
/// deciding choices first, in a grid a short field shares with another; a
/// developer's settings under a closed "Developer", opened while one of them
/// is required and missing; and the Save button kept in view.
pub fn form(record: &PluginSettingsRecord, token: &str, development: bool) -> String {
    let declared = &record.declared_settings;
    let (developer, settings): (Vec<_>, Vec<_>) = in_order(declared)
        .into_iter()
        .filter(|declaration| shown(declaration, development))
        .partition(|declaration| declaration.developer);
    if settings.is_empty() && developer.is_empty() {
        return "<p class=\"empty\">This plugin declares no settings.</p>".to_string();
    }
    let fields = |these: &[&SettingDeclaration]| -> String {
        these
            .iter()
            .map(|declaration| field(declaration, record, declared))
            .collect()
    };
    let main = if settings.is_empty() {
        String::new()
    } else {
        format!("<div class=\"fields\">{}</div>", fields(&settings))
    };
    let developer = if developer.is_empty() {
        String::new()
    } else {
        let needed = missing(record, development)
            .iter()
            .any(|declaration| declaration.developer);
        format!(
            "<details class=\"developer\"{open}><summary>Developer \
             <span class=\"summary-note\">{count} for whoever develops the plugin</span></summary>\
             <div class=\"fields\">{fields}</div></details>",
            open = if needed { " open" } else { "" },
            count = if developer.len() == 1 {
                "1 setting".to_string()
            } else {
                format!("{} settings", developer.len())
            },
            fields = fields(&developer),
        )
    };
    format!(
        "<form method=\"post\" action=\"{action}\" class=\"settings\" autocomplete=\"off\" data-settings>{token}\
         {main}{developer}<div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Save settings</button></div>\
         </form><div class=\"note-bubble hints\" id=\"settings-hint-bubble\" aria-hidden=\"true\" hidden></div>\
         <script>{SCRIPT}</script>",
        action = escape(&path(&record.plugin_instance_id)),
    )
}

/// Shows a conditional field only while the setting it names holds one of
/// its values: the radio chosen, what is typed, or else that setting's
/// default. Hidden, its inputs are still posted as they stand, which changes
/// nothing. Without this every field shows, each with its tag.
///
/// And a field's hint in the one bubble, as an account's note is shown
/// (overview's `NOTE_SCRIPT`): while the field is focused, or its marker is
/// pointed at, focused or pressed, placed under the field, or over it when
/// there is no room below. It goes on Escape, on a press elsewhere, when the
/// window resizes and when its field is hidden. The hint reaches the bubble
/// as text, never as markup, and the bubble is hidden from a screen reader,
/// which has the hint as the field's description.
const SCRIPT: &str = r#"(function () {
  var form = document.querySelector("form[data-settings]");
  if (!form) return;
  form.classList.add("js");
  var bubble = document.getElementById("settings-hint-bubble");
  var shown = null;
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
    if (shown && shown.hidden) hide();
  }
  function hide() {
    shown = null;
    if (bubble) bubble.hidden = true;
  }
  function show(holder) {
    if (!bubble || holder === shown) return;
    var about = holder && !holder.hidden && holder.querySelector(".about");
    if (!about) { hide(); return; }
    shown = holder;
    bubble.textContent = about.textContent;
    bubble.hidden = false;
    var edge = 8;
    var gap = 4;
    var box = holder.getBoundingClientRect();
    var width = bubble.offsetWidth;
    var height = bubble.offsetHeight;
    var top = box.bottom + gap;
    if (top + height > window.innerHeight - edge && box.top - height - gap >= edge) top = box.top - height - gap;
    var left = Math.max(edge, Math.min(box.left, window.innerWidth - width - edge));
    bubble.style.top = top + window.scrollY + "px";
    bubble.style.left = left + window.scrollX + "px";
  }
  function focused() {
    var active = document.activeElement;
    var holder = active && form.contains(active) ? active.closest("[data-setting]") : null;
    if (holder) show(holder); else hide();
  }
  form.addEventListener("change", update);
  form.addEventListener("input", update);
  update();
  if (!bubble) return;
  document.body.appendChild(bubble);
  form.addEventListener("focusin", function (event) {
    show(event.target.closest("[data-setting]"));
  });
  form.addEventListener("focusout", function (event) {
    if (!shown || !shown.contains(event.relatedTarget)) hide();
  });
  form.addEventListener("mouseover", function (event) {
    var mark = event.target.closest(".note-mark");
    if (mark) show(mark.closest("[data-setting]"));
  });
  form.addEventListener("mouseout", function (event) {
    var mark = event.target.closest(".note-mark");
    if (mark && !mark.contains(event.relatedTarget)) focused();
  });
  document.addEventListener("click", function (event) {
    var mark = event.target.closest(".note-mark");
    if (mark && form.contains(mark)) { show(mark.closest("[data-setting]")); return; }
    if (!shown || !shown.contains(event.target)) hide();
  });
  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape" && shown) hide();
  });
  window.addEventListener("resize", hide);
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
