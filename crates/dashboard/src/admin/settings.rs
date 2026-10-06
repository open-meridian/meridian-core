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
//! make the form short enough to display in one page"; 2026-10-04: "target
//! our display to one page without scrolling (use pagination or tab
//! instead)"): each field's label on one line with small tags for required
//! or optional, its default, "set" or "not set", and when it applies; short
//! fields, numbers, on/offs and a choice nothing hangs on, two to a row on a
//! wide screen and one on a phone, and long ones, secrets and text, across; a
//! choice's options on one line; a field's hint one small line without
//! script, and with it a bubble shown while the field is focused or its
//! marker pointed at; the settings in groups, each a tab of the one form --
//! Required, Optional and, on a development deployment, Developer, a group
//! past six fields going on in another -- and Save kept in view at the foot.
//! None of it changes what the form posts.
//!
//! **A secret is write-only here.** The record names which secrets are set
//! and never holds a value, so there is nothing to show: a secret's field is
//! always empty, says "set" or "not set", and replaces the value when typed
//! into. A setting held sealed is treated as secret whatever the plugin
//! declares of it now. The fields' names carry the setting's name and nothing
//! else, and nothing here logs a field.
//!
//! **A table setting** (contract v14; W4.8, W6.11) is an editable typed table
//! on a page of its own, a tab beside Settings titled with its label (the
//! product owner, 2026-10-05: "two tabs - plan code links vs cash links"):
//! the kit's `om-entry-grid`, which the dashboard loads, one line a row,
//! paged to the screen, rows added to the table's most, a typed input per
//! column -- an external account chosen from those the plugin reported, an
//! instrument from the deployment's records, held by its ID and never a
//! symbol -- each cell checked in the browser as typed, and again here before
//! anything is sent ([`table_problems`], [`meridian_domain::setting_table`]), a
//! refusal naming each cell. Without script it is a plain table of inputs,
//! the rows held and a few blank. The rows' `changed_by` and `changed_at` are
//! the conductor's to stamp.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use meridian_domain::setting_table::{self, Cells, Problem};
use meridian_domain::v1::{PluginSettingValue, PluginSettingsRecord, SetPluginSettingsRequest};
use meridian_pb::v1::{SettingColumn, SettingColumnType, SettingDeclaration, SettingType};

use crate::html::escape;

/// A secret's field, which replaces it when typed into.
pub const SECRET_FIELD: &str = "secret.";
/// A setting that is not secret, shown as it stands.
pub const VALUE_FIELD: &str = "value.";
/// Ticked, clears a secret that is set.
pub const CLEAR_FIELD: &str = "clear.";
/// A table setting's cells: `table.<setting>[<row>].<column>`.
pub const TABLE_FIELD: &str = "table.";
/// Blank rows the plain table offers below those held, for more.
const BLANK_ROWS: usize = 3;

/// What a table setting's typed columns offer (W6.11): the external accounts
/// the plugin reported, and the deployment's instrument records, each as its
/// identifier and what a person reads. Empty where the form has no such
/// column; `unread` says why the instruments could not be listed.
#[derive(Debug, Clone, Default)]
pub struct Choices {
    pub external_accounts: Vec<(String, String)>,
    pub instruments: Vec<(String, String)>,
    pub unread: String,
}

impl Choices {
    fn of(&self, kind: SettingColumnType) -> Option<&[(String, String)]> {
        match kind {
            SettingColumnType::ExternalAccount => Some(&self.external_accounts),
            SettingColumnType::Instrument => Some(&self.instruments),
            _ => None,
        }
    }
}

/// Whether any setting a record declares is a table with a column of `kind`.
pub fn wants(record: &PluginSettingsRecord, kind: SettingColumnType) -> bool {
    record.declared_settings.iter().any(|declaration| {
        setting_table::is_table(declaration)
            && declaration
                .columns
                .iter()
                .any(|column| column.r#type == kind as i32)
    })
}

fn column_kind(column: &SettingColumn) -> SettingColumnType {
    SettingColumnType::try_from(column.r#type).unwrap_or(SettingColumnType::Unspecified)
}

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
        "<span class=\"badge warn need\">Required</span>"
    } else {
        "<span class=\"badge need\">Optional</span>"
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
    // A table is on a page of its own (`table_form`) unless it is held
    // sealed, and then it is a secret like any other.
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

/// The table settings this deployment shows, each on a page of its own
/// (the product owner, 2026-10-05: "two tabs - plan code links vs cash
/// links"): a tab beside Settings titled with its label, holding its entry
/// grid alone. A table held sealed is a secret, and stays on the form.
pub fn tables(record: &PluginSettingsRecord, development: bool) -> Vec<&SettingDeclaration> {
    record
        .declared_settings
        .iter()
        .filter(|declaration| {
            setting_table::is_table(declaration)
                && !is_secret(record, declaration)
                && shown(declaration, development)
        })
        .collect()
}

/// What a table setting's page is called in a tab's query: its name, after
/// a prefix no tab of the dashboard's own has.
pub fn table_key(declaration: &SettingDeclaration) -> String {
    format!("setting-{}", declaration.name)
}

/// The table setting a tab's query names, of those this deployment shows.
pub fn table_named<'a>(
    record: &'a PluginSettingsRecord,
    key: &str,
    development: bool,
) -> Option<&'a SettingDeclaration> {
    tables(record, development)
        .into_iter()
        .find(|declaration| table_key(declaration) == key)
}

/// The most fields a group's tab holds, so a tab fits one screen on a phone;
/// a group with more is two tabs.
const MOST_A_TAB: usize = 6;

/// One tab of the form: its fragment, what it is called, and its settings.
struct Group<'a> {
    id: String,
    title: String,
    settings: Vec<&'a SettingDeclaration>,
}

/// The form's settings in groups, each a tab (every page fits one screen,
/// the product owner, 2026-10-04): what must be filled in, what has a
/// default or may be left, and a developer's, each as the plugin declared
/// them, deciding choices first; a group past [`MOST_A_TAB`] fields
/// continues on another tab.
fn groups<'a>(settings: &[&'a SettingDeclaration]) -> Vec<Group<'a>> {
    let of = |want: fn(&SettingDeclaration) -> bool| -> Vec<&'a SettingDeclaration> {
        settings.iter().copied().filter(|d| want(d)).collect()
    };
    let mut out = Vec::new();
    for (key, title, these) in [
        ("required", "Required", of(|d| !d.developer && d.required)),
        ("optional", "Optional", of(|d| !d.developer && !d.required)),
        ("developer", "Developer", of(|d| d.developer)),
    ] {
        for (n, chunk) in these.chunks(MOST_A_TAB).enumerate() {
            let (id, title) = if n == 0 {
                (format!("settings-{key}"), title.to_string())
            } else {
                (
                    format!("settings-{key}-{}", n + 1),
                    format!("{title} {}", n + 1),
                )
            };
            out.push(Group {
                id,
                title,
                settings: chunk.to_vec(),
            });
        }
    }
    out
}

/// The form: every setting the plugin declared that this deployment shows
/// but its tables, which have pages of their own ([`table_form`]): the
/// tests' form.
#[cfg(test)]
pub fn form(record: &PluginSettingsRecord, token: &str, development: bool) -> String {
    form_with(
        record,
        token,
        development,
        &path(&record.plugin_instance_id),
    )
}

/// The form of a plugin's settings but its tables, fitting one screen: its
/// groups as tabs ([`groups`]), each a fragment of the page, so each opens
/// by its address; within one, fields in a grid a short field shares with
/// another; and the Save button kept in view. One form, so Save posts every
/// group as it stands, a hidden one too, and what it posts is what the form
/// always posted. Without script every group shows, one under another, each
/// under its title. A tab says how many of its required settings are
/// missing, so none is left behind a tab unseen.
pub fn form_with(
    record: &PluginSettingsRecord,
    token: &str,
    development: bool,
    action: &str,
) -> String {
    let declared = &record.declared_settings;
    let tables: Vec<&str> = tables(record, development)
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect();
    let settings: Vec<&SettingDeclaration> = in_order(declared)
        .into_iter()
        .filter(|declaration| shown(declaration, development))
        .filter(|declaration| !tables.contains(&declaration.name.as_str()))
        .collect();
    if settings.is_empty() {
        return if tables.is_empty() {
            "<p class=\"empty\">This plugin declares no settings.</p>".to_string()
        } else {
            "<p class=\"empty\">This plugin declares no settings but its tables, each on a tab \
             of its own.</p>"
                .to_string()
        };
    }
    let missing: Vec<&str> = missing(record, development)
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect();
    let groups = groups(&settings);
    let nav = if groups.len() < 2 {
        String::new()
    } else {
        let links: String = groups
            .iter()
            .map(|group| {
                let needed = group
                    .settings
                    .iter()
                    .filter(|declaration| missing.contains(&declaration.name.as_str()))
                    .count();
                let needed = if needed == 0 {
                    String::new()
                } else {
                    format!(" <span class=\"badge warn\">{needed} missing</span>")
                };
                format!(
                    "<a href=\"#{id}\">{title}{needed}</a>",
                    id = group.id,
                    title = escape(&group.title),
                )
            })
            .collect();
        format!(
            "<nav class=\"tabs setting-groups\" data-sections aria-label=\"The settings by group\">{links}</nav>"
        )
    };
    let sections: String = groups
        .iter()
        .map(|group| {
            let fields: String = group
                .settings
                .iter()
                .map(|declaration| field(declaration, record, declared))
                .collect();
            format!(
                "<section class=\"setting-group\" id=\"{id}\" aria-label=\"{title}\">\
                 <h3 class=\"group-title\">{title}</h3><div class=\"fields\">{fields}</div></section>",
                id = group.id,
                title = escape(&group.title),
            )
        })
        .collect();
    format!(
        "<form method=\"post\" action=\"{action}\" class=\"settings\" autocomplete=\"off\" data-settings>{token}\
         {nav}{sections}<div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Save settings</button></div>\
         </form><div class=\"note-bubble hints\" id=\"settings-hint-bubble\" aria-hidden=\"true\" hidden></div>\
         <script>{SCRIPT}</script>",
        action = escape(action),
    )
}

/// A table setting's page: its entry grid alone, paged to the screen, one
/// line a row ([`table_field`]), and Save; posted to `action` with the
/// session's token as the form is, and checked as the form's tables are.
/// What it posts names this table alone, so every other setting is left as
/// it stands.
///
/// `named` is what a person reads for the subject a row's stamp names.
pub fn table_form(
    record: &PluginSettingsRecord,
    declaration: &SettingDeclaration,
    token: &str,
    action: &str,
    choices: &Choices,
    named: &dyn Fn(&str) -> String,
) -> String {
    let about = about(declaration, false);
    format!(
        "<form method=\"post\" action=\"{action}\" class=\"table-setting\" autocomplete=\"off\" \
         data-table-setting>{token}{grid}<div class=\"form-foot\"><button type=\"submit\" class=\"primary\">\
         Save</button></div></form>",
        action = escape(action),
        grid = table_field(declaration, record, &about, choices, named),
    )
}

/// Shows one group of the form at a time, the one the address names or else
/// the first, its tab marked; without this every group shows, under its
/// title.
///
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
  var nav = form.querySelector("nav[data-sections]");
  function group() {
    if (!nav) return;
    var tabs = [].slice.call(nav.querySelectorAll("a[href^='#']"));
    var ids = tabs.map(function (a) { return a.getAttribute("href").slice(1); });
    var asked = decodeURIComponent(location.hash.slice(1));
    var want = ids.indexOf(asked) >= 0 ? asked : ids[0];
    tabs.forEach(function (a) {
      var on = a.getAttribute("href").slice(1) === want;
      a.classList.toggle("on", on);
      if (on) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current");
    });
    ids.forEach(function (id) {
      var section = document.getElementById(id);
      if (section) section.hidden = id !== want;
    });
    hide();
  }
  window.addEventListener("hashchange", group);
  group();
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
/// The first setting a change names that serves a role the person does not
/// administer, as the refusal saying so (W6.11, contract v15): a setting is
/// set only by one holding admin on every role it serves. None when every
/// one is theirs to set.
pub fn not_administered(
    record: &PluginSettingsRecord,
    request: &SetPluginSettingsRequest,
    held: &meridian_access::PluginHeld,
    plugin_roles: &[String],
) -> Option<String> {
    let named = request
        .values
        .iter()
        .map(|value| value.name.as_str())
        .chain(request.cleared.iter().map(String::as_str));
    for name in named {
        let Some(declaration) = record.declared_settings.iter().find(|d| d.name == name) else {
            continue;
        };
        // A declaration naming no role -- from a sidecar before v15 -- serves
        // every role the plugin holds, or the plugin as a whole.
        let serves = if declaration.roles.is_empty() {
            plugin_roles
        } else {
            declaration.roles.as_slice()
        };
        if held.administers_every(serves) {
            continue;
        }
        let not: Vec<&str> = serves
            .iter()
            .filter(|role| !held.administers(role))
            .map(String::as_str)
            .collect();
        return Some(format!(
            "{} serves {}, and you do not administer {}: a setting serving several roles is set \
             by an admin of every one",
            label(declaration),
            serves.join(" and "),
            not.join(" or ")
        ));
    }
    None
}

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
        if setting_table::is_table(declaration) && !is_secret(record, declaration) {
            let Some(rows) = posted_rows(fields, name) else {
                continue;
            };
            // Checked before this is called (`table_problems`); what does not
            // read here is sent as typed, for the conductor to refuse.
            let rows = setting_table::checked(declaration, &rows).unwrap_or(rows);
            let held: Vec<Cells> = current(record, name)
                .and_then(|text| setting_table::parse(text).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|row| row.cells)
                .collect();
            if rows == held {
                continue;
            }
            if rows.is_empty() {
                if current(record, name).is_some() {
                    request.cleared.push(name.clone());
                }
            } else {
                request.values.push(PluginSettingValue {
                    name: name.clone(),
                    value: setting_table::cells_written(&rows),
                });
            }
            continue;
        }
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

// ── A table setting (contract v14; W4.8, W6.11) ─────────────────────────────

/// The rows a form posted for a table setting, in their order, each wholly
/// blank row kept so a problem's path names the row as the form numbered it;
/// None where the form did not post the table at all.
fn posted_rows(fields: &HashMap<String, String>, name: &str) -> Option<Vec<Cells>> {
    let prefix = format!("{TABLE_FIELD}{name}[");
    let mut rows: BTreeMap<usize, Cells> = BTreeMap::new();
    for (field, value) in fields {
        let Some(rest) = field.strip_prefix(&prefix) else {
            continue;
        };
        let Some((index, column)) = rest.split_once("].") else {
            continue;
        };
        let Ok(index) = index.parse::<usize>() else {
            continue;
        };
        if index > setting_table::MOST_ROWS + BLANK_ROWS {
            continue;
        }
        rows.entry(index)
            .or_default()
            .insert(column.to_string(), value.clone());
    }
    let present = fields.contains_key(&format!("{TABLE_FIELD}{name}"));
    if rows.is_empty() && !present {
        return None;
    }
    let last = rows.keys().next_back().copied().map_or(0, |n| n + 1);
    Some(
        (0..last)
            .map(|n| rows.remove(&n).unwrap_or_default())
            .collect(),
    )
}

/// Every instrument the form's tables name, typed into an instrument column,
/// each once: what is checked by its ID before anything is sent.
pub fn posted_instruments(
    record: &PluginSettingsRecord,
    fields: &HashMap<String, String>,
) -> BTreeSet<String> {
    let mut named = BTreeSet::new();
    for declaration in &record.declared_settings {
        if !setting_table::is_table(declaration) {
            continue;
        }
        let Some(rows) = posted_rows(fields, &declaration.name) else {
            continue;
        };
        for row in &rows {
            for column in &declaration.columns {
                if column_kind(column) != SettingColumnType::Instrument {
                    continue;
                }
                let typed = row.get(&column.name).map(|v| v.trim()).unwrap_or_default();
                if !typed.is_empty() {
                    named.insert(typed.to_string());
                }
            }
        }
    }
    named
}

/// Every cell of every table the form posted that does not read, by its
/// path: as the conductor checks it, and an external account one the plugin
/// reported, an instrument one the deployment holds. Nothing is sent while
/// any is said.
pub fn table_problems(
    record: &PluginSettingsRecord,
    fields: &HashMap<String, String>,
    choices: &Choices,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    for declaration in &record.declared_settings {
        if !setting_table::is_table(declaration) {
            continue;
        }
        let Some(rows) = posted_rows(fields, &declaration.name) else {
            continue;
        };
        if let Err(found) = setting_table::checked(declaration, &rows) {
            problems.extend(found);
        }
        for (n, row) in rows.iter().enumerate() {
            for column in &declaration.columns {
                let Some(offered) = choices.of(column_kind(column)) else {
                    continue;
                };
                let typed = row.get(&column.name).map(|v| v.trim()).unwrap_or_default();
                if typed.is_empty() || offered.iter().any(|(id, _)| id == typed) {
                    continue;
                }
                problems.push(Problem {
                    path: format!("{}[{n}].{}", declaration.name, column.name),
                    message: match column_kind(column) {
                        SettingColumnType::Instrument if !choices.unread.is_empty() => {
                            format!("the instruments could not be read: {}", choices.unread)
                        }
                        SettingColumnType::Instrument => {
                            "names no instrument record this deployment holds".to_string()
                        }
                        _ => "names no external account this plugin reported".to_string(),
                    },
                });
            }
        }
    }
    problems
}

/// A refusal of table cells, as a person reads it: each by its row and
/// column's label.
pub fn said(record: &PluginSettingsRecord, problems: &[Problem]) -> String {
    let named = |path: &str| -> String {
        for declaration in &record.declared_settings {
            let Some(rest) = path.strip_prefix(&format!("{}[", declaration.name)) else {
                continue;
            };
            if let Some((n, column)) = rest.split_once("].") {
                let heading = declaration
                    .columns
                    .iter()
                    .find(|c| c.name == column)
                    .map(setting_table::label)
                    .unwrap_or(column);
                let row = n.parse::<usize>().map_or(0, |n| n + 1);
                return format!("{}, row {row}, {heading}", label(declaration));
            }
        }
        path.to_string()
    };
    problems
        .iter()
        .map(|problem| {
            format!(
                "{}: {} ({})",
                named(&problem.path),
                problem.message,
                problem.path
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn grid_column(column: &SettingColumn, choices: &Choices) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("key".into(), column.name.clone().into());
    out.insert("label".into(), setting_table::label(column).into());
    if column.required {
        out.insert("required".into(), true.into());
    }
    if !column.description.is_empty() {
        out.insert("hint".into(), column.description.clone().into());
    }
    let options = |pairs: Vec<(String, String)>| -> serde_json::Value {
        pairs
            .into_iter()
            .map(|(value, label)| serde_json::json!({"value": value, "label": label}))
            .collect::<Vec<_>>()
            .into()
    };
    match column_kind(column) {
        SettingColumnType::Integer => {
            out.insert("type".into(), "decimal".into());
            out.insert("places".into(), 0.into());
        }
        SettingColumnType::Decimal => {
            out.insert("type".into(), "decimal".into());
            out.insert("places".into(), 18.into());
        }
        SettingColumnType::Date => {
            out.insert("type".into(), "date".into());
        }
        SettingColumnType::Choice => {
            out.insert("type".into(), "choice".into());
            out.insert(
                "options".into(),
                options(
                    column
                        .choices
                        .iter()
                        .map(|c| {
                            let shown = if c.label.is_empty() {
                                &c.value
                            } else {
                                &c.label
                            };
                            (c.value.clone(), shown.clone())
                        })
                        .collect(),
                ),
            );
        }
        kind @ (SettingColumnType::ExternalAccount | SettingColumnType::Instrument) => {
            out.insert("type".into(), "choice".into());
            out.insert(
                "options".into(),
                options(choices.of(kind).unwrap_or_default().to_vec()),
            );
        }
        SettingColumnType::Text | SettingColumnType::Unspecified => {
            out.insert("max_length".into(), setting_table::MOST_TEXT.into());
        }
    }
    serde_json::Value::Object(out)
}

fn cell_input(
    name: &str,
    n: usize,
    column: &SettingColumn,
    value: &str,
    choices: &Choices,
) -> String {
    let field = escape(&format!("{TABLE_FIELD}{name}[{n}].{}", column.name));
    let aria = escape(&format!("{}, row {}", setting_table::label(column), n + 1));
    let select = |pairs: Vec<(String, String)>| -> String {
        let mut known = pairs.iter().any(|(id, _)| id == value);
        let mut out =
            format!(r#"<select name="{field}" aria-label="{aria}"><option value=""></option>"#);
        for (id, shown) in &pairs {
            out.push_str(&format!(
                r#"<option value="{}"{}>{}</option>"#,
                escape(id),
                if id == value { " selected" } else { "" },
                escape(shown)
            ));
        }
        if !value.is_empty() && !known {
            // Held, and no longer offered: kept, and said.
            out.push_str(&format!(
                r#"<option value="{}" selected>{} (not offered now)</option>"#,
                escape(value),
                escape(value)
            ));
            known = true;
        }
        let _ = known;
        out.push_str("</select>");
        out
    };
    match column_kind(column) {
        SettingColumnType::Choice => select(
            column
                .choices
                .iter()
                .map(|c| {
                    (
                        c.value.clone(),
                        if c.label.is_empty() {
                            c.value.clone()
                        } else {
                            c.label.clone()
                        },
                    )
                })
                .collect(),
        ),
        kind @ (SettingColumnType::ExternalAccount | SettingColumnType::Instrument) => {
            select(choices.of(kind).unwrap_or_default().to_vec())
        }
        kind => format!(
            r#"<input name="{field}" value="{}" aria-label="{aria}" autocomplete="off"{}>"#,
            escape(value),
            match kind {
                SettingColumnType::Integer | SettingColumnType::Decimal =>
                    r#" inputmode="decimal""#,
                SettingColumnType::Date => r#" placeholder="YYYY-MM-DD""#,
                _ => "",
            }
        ),
    }
}

/// A table setting: its head on one line -- its label, how many rows of its
/// most it holds, and who changed the latest row when -- what it is for on
/// one line under it, cut short with the whole on hover; then the kit's entry
/// grid over a plain table of inputs, the rows held and a few blank ones, one
/// line each, within the table's most. The grid adds rows to the most, pages
/// them to the screen and checks each cell as it is typed; without it, the
/// plain table posts the same names.
fn table_field(
    declaration: &SettingDeclaration,
    record: &PluginSettingsRecord,
    about: &str,
    choices: &Choices,
    named: &dyn Fn(&str) -> String,
) -> String {
    let name = &declaration.name;
    let held = current(record, name)
        .and_then(|text| setting_table::parse(text).ok())
        .unwrap_or_default();
    let most = setting_table::most_rows(declaration);
    let grid = serde_json::json!({
        "columns": declaration.columns.iter().map(|c| grid_column(c, choices)).collect::<Vec<_>>(),
        "rows": held.iter().map(|row| row.cells.clone()).collect::<Vec<_>>(),
    });
    let blank = BLANK_ROWS.min(most.saturating_sub(held.len()));
    let mut body = String::new();
    for n in 0..held.len() + blank {
        let cells = held.get(n).map(|row| &row.cells);
        body.push_str("<tr>");
        for column in &declaration.columns {
            let value = cells
                .and_then(|cells| cells.get(&column.name))
                .map(String::as_str)
                .unwrap_or_default();
            body.push_str(&format!(
                "<td>{}</td>",
                cell_input(name, n, column, value, choices)
            ));
        }
        body.push_str("</tr>");
    }
    let heads: String = declaration
        .columns
        .iter()
        .map(|c| format!("<th>{}</th>", escape(setting_table::label(c))))
        .collect();
    let caption = escape(label(declaration));
    let unread = if choices.unread.is_empty() {
        String::new()
    } else {
        format!(
            r#"<p class="hint bad-ink">The instruments could not be read: {}</p>"#,
            escape(&choices.unread)
        )
    };
    let count = format!(
        "<span class=\"badge\" data-rows>{} of at most {most} {}</span>",
        held.len(),
        if most == 1 { "row" } else { "rows" }
    );
    // Who changed the latest row, and when: the conductor's stamps.
    let who = held
        .iter()
        .max_by(|a, b| a.changed_at.cmp(&b.changed_at))
        .map(|row| {
            let said = format!(
                "The latest changed by {} at {}",
                named(&row.changed_by),
                row.changed_at
            );
            format!(
                "<span class=\"changed\" title=\"{said}\" data-last-changed>{said}</span>",
                said = escape(&said)
            )
        })
        .unwrap_or_default();
    let about = if about.is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"hint about-line\" title=\"{about}\">{about}</p>",
            about = escape(about)
        )
    };
    // The grid's JSON escapes `<`, `>` and `&` so no cell closes the script.
    let json = grid
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    format!(
        "<div class=\"table-head\"><h2>{caption}</h2>{count}{who}</div>{about}{unread}\
         <input type=\"hidden\" name=\"{TABLE_FIELD}{field}\" value=\"1\">\
         <om-entry-grid name=\"{TABLE_FIELD}{field}\" caption=\"{caption}\" max-rows=\"{most}\">\
         <script type=\"application/json\">{json}</script>\
         <div class=\"table-wrap\"><table class=\"one-line\"><caption>{caption}</caption>\
         <thead><tr>{heads}</tr></thead><tbody>{body}</tbody></table></div></om-entry-grid>",
        field = escape(name),
    )
}
