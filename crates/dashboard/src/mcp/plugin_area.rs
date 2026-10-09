//! Core's plugin-area tools (W6.20, contract v17;
//! plans/cores-plugin-area-is-at-parity-on-the-mcp, accepted 2026-10-09; the
//! MCP spec's requirement 23a and Q14): every page and action core draws for
//! a plugin has a tool at the page's own role and level, each a transport
//! over a row the dashboard already calls, with no rule of its own, the
//! instance an argument (`plugin_instance_id`, or `instance_id` where the
//! row names it so).
//!
//! **The gates are the pages'.** The Summary, the moves and the Settings
//! form are an admin's of any of the plugin's roles; the admin portal's
//! Overview and Access tabs an admin's or a deployment admin's; the
//! archive, the holds, the catalogue, launching and stopping a deployment
//! admin's, through a delegation covering the deployment admin's
//! capabilities. A tool is listed when its gate is held on any instance
//! through the delegation, and checked per instance on every call: a call to
//! one the delegation does not reach is refused naming those it does. For
//! these tools "core's own tools carry no role" gives way to the pages'
//! roles and levels; the call record names the gate it was admitted under
//! (`admin on snaptrade-1:custody`, `deployment admin`).
//!
//! **Two exceptions, and only two.** No tool reads or takes a secret
//! setting's value: an argument naming one is refused by name, saying a
//! person enters it at the Settings form, and a read says only that it is
//! set, by whom and when; clearing one is an act like any other, as the
//! form's Clear. And no tool changes who holds access: DefineUserGroup,
//! DefineAccountGroup, DefineAccessGroup, GrantPermission and
//! WithdrawPermission are no tool's.
//!
//! **One code path, two doors.** The settings tool takes the form's own
//! field grammar as JSON -- `value.<name>`, `table.<name>[<row>].<column>`,
//! `clear.<name>` -- and calls the form's own checks
//! ([`crate::admin::settings_change`]), so a cell refused on the page and an
//! argument refused here are refused by the same code with the same path.
//!
//! Every act carries a note, every one (the plan's Q8), and goes out
//! stamped with the person, the delegation and the client, each change its
//! own record (decisions/031). Answers are the rows' own messages as JSON,
//! by their field names; an enum is its proto name; a time is nanoseconds
//! since the epoch.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use meridian_access::{Access, AccessLevel};
use meridian_domain::setting_table;
use meridian_domain::v1::{
    AccessRecords, AllowArchiveRequest, Hold, LaunchPluginRequest, MoveRecord, PluginArchive,
    PluginCatalogue, PluginCatalogueRequest, PluginLaunch, PluginLaunchState, PluginReport,
    PluginSettingsRecord, PluginVersion, ReadMovesReply, ReadMovesRequest, SetHoldRequest,
    SettingLastChange, StopPluginRequest, WithdrawArchiveRequest,
};
use meridian_pb::v1::{
    MoveOutcome, PluginFigure, SettingColumnType, SettingDeclaration, SettingType, StoredSpan,
    ToolDeclaration,
};
use serde_json::{json, Map, Value};

use super::instruments::{ask, bus_refused, integer, object, only, text, Problems};
use super::{refused, Caller};
use crate::admin::settings::{self, AGAINST_FIELD, CLEAR_FIELD, TABLE_FIELD, VALUE_FIELD};
use crate::admin::{Fields, SettingsRefused};
use crate::web::App;

pub use super::{Area, Spec};

pub const SET_PLUGIN_SETTINGS: &str = "platform.config.command.set-plugin-settings";
pub const SET_HOLD: &str = "platform.config.command.set-hold";

/// What admits a call to one of these tools: the page it mirrors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// The home's plugin entries: any level on any plugin, or the deployment
    /// admin's capabilities.
    Anyone,
    /// The Summary, the moves and the Settings form: admin on any of the
    /// instance's roles (a setting set only by an admin of every role it
    /// serves).
    Admin,
    /// The admin portal's Overview and Access: admin on any of the
    /// instance's roles, or a deployment admin, who reads the Overview's
    /// parts as the portal shows them.
    AdminOrDeployment,
    /// The archive, the holds, the catalogue, launching and stopping: the
    /// deployment admin's capabilities.
    DeploymentAdmin,
}

impl Gate {
    /// Whether a delegation's narrowed access holds this gate on any
    /// instance, so the tool is listed.
    pub fn open_to(self, access: &Access) -> bool {
        match self {
            Gate::Anyone => {
                access.deployment_admin
                    || access.all_plugins_admin
                    || access.plugins.values().any(|held| held.holds_any())
            }
            Gate::Admin => access.administers_any(),
            Gate::AdminOrDeployment => access.deployment_admin || access.administers_any(),
            Gate::DeploymentAdmin => access.deployment_admin,
        }
    }
}

const fn spec(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    reads: bool,
    gate: Gate,
    input_schema: fn() -> Value,
) -> Spec {
    Spec {
        name,
        title,
        description,
        reads,
        open_world: false,
        input_schema,
        area: Area::Plugins(gate),
    }
}

pub static SPECS: &[Spec] = &[
    spec(
        "list_plugins",
        "List the plugins",
        "each plugin instance this person holds any level on through this delegation -- every one for a deployment admin -- with its title, its roles, what the person holds on each role (entries, role and level), and its state as its sidecar last reported it.",
        true,
        Gate::Anyone,
        nothing_schema,
    ),
    spec(
        "read_plugin_summary",
        "Read a plugin's Summary",
        "one instance's Summary, as its admin reads it under Manage: its status (registered, healthy and why not, the version and contract it runs), the figures it reports, its raw records (what storage holds of each kind, the hold over it, what the archive holds and its bound), what it declares and the tools it offered, admitted and refused; and the admin portal's Overview with its connections. A deployment admin who administers none of its roles reads the Overview's parts.",
        true,
        Gate::AdminOrDeployment,
        instance_schema,
    ),
    spec(
        "read_moves",
        "Read a plugin's moves",
        "an edge plugin's moves of its raw records, newest first, a page at a time (next_cursor): each unit archived, restored, returned or deleted, its count and span, the rule or the person, and the delegation and client the person acted through; with what the archive holds of each kind and its archive as allowed.",
        true,
        Gate::Admin,
        moves_schema,
    ),
    spec(
        "read_plugin_settings",
        "Read a plugin's settings",
        "an instance's settings as its Settings form shows this person: each declared setting, the roles it serves, whether they may set it (may_set) and if not why (detail); each value that is not secret, a table's rows with changed_by and changed_at; a secret's being set (secrets_set), by whom and when (changes), never its value; updated_at_ns, which a change names as against_updated_at_ns; and the external accounts a table's column offers.",
        true,
        Gate::Admin,
        instance_schema,
    ),
    spec(
        "set_plugin_settings",
        "Set a plugin's settings",
        "set and clear an instance's settings in the form's own grammar: value.<name> a value, table.<name> a table's rows whole (each row its cells by column), clear.<name> true to clear one, a secret included; against_updated_at_ns as read, refused where the settings changed since; and a note saying why. A secret's value is never taken here, either way: a person enters it at the Settings form. A setting serving several roles is set by an admin of every one. Each cell refused by its path (table.<name>[<row>].<column>).",
        false,
        Gate::Admin,
        set_settings_schema,
    ),
    spec(
        "allow_archive",
        "Allow a plugin an archive",
        "allow an edge plugin instance an archive, or change its bound (most_bytes, 0 for none), with a note: the instance restarts with it.",
        false,
        Gate::DeploymentAdmin,
        allow_archive_schema,
    ),
    spec(
        "withdraw_archive",
        "Withdraw a plugin's archive",
        "withdraw an instance's archive, with a note: the instance restarts without it, and what the archive holds is kept.",
        false,
        Gate::DeploymentAdmin,
        withdraw_archive_schema,
    ),
    spec(
        "read_holds",
        "Read the holds",
        "the deployment's holds on raw records, as its Settings' Holds tab shows them: per edge role, or every one with an empty role, the least days a record is kept, write-once, who set it, when, and the delegation and client they acted through.",
        true,
        Gate::DeploymentAdmin,
        nothing_schema,
    ),
    spec(
        "set_hold",
        "Set a hold",
        "set, change or clear (days 0) the hold on raw records for an edge role, or every one with role empty, with a note; write_once refused where the deployment's archive cannot lock, as on the page.",
        false,
        Gate::DeploymentAdmin,
        set_hold_schema,
    ),
    spec(
        "read_plugin_catalogue",
        "Read the plugin catalogue",
        "what `meridian plugin list` reads: every version uploaded, with its roles and what it declares, and every launch, live or ended, with who launched and stopped it, when, and the delegation and client they acted through.",
        true,
        Gate::DeploymentAdmin,
        nothing_schema,
    ),
    spec(
        "launch_plugin",
        "Launch a plugin",
        "launch a version in the catalogue as an instance, as `meridian plugin launch` does: approved_roles exactly the version's roles, refused otherwise; live only on a development deployment; and a note.",
        false,
        Gate::DeploymentAdmin,
        launch_schema,
    ),
    spec(
        "stop_plugin",
        "Stop a plugin",
        "stop an instance launched from the catalogue, as `meridian plugin stop` does, with a note.",
        false,
        Gate::DeploymentAdmin,
        stop_schema,
    ),
    spec(
        "read_plugin_access",
        "Read who holds access to a plugin",
        "the admin portal's Access tab, read only: the access groups naming the instance, each entry's role and level, the permissions to them, and the user and account groups they join. No tool changes who holds access.",
        true,
        Gate::AdminOrDeployment,
        instance_schema,
    ),
];

/// A tool's description as listed: its own words, then the sentence every
/// one of these tools carries.
pub fn described(spec: &Spec) -> String {
    format!(
        "{} No tool reads or takes a secret setting's value, and no tool changes who holds access; every change carries a note and is its own record, naming the person, the delegation and the client.",
        spec.description
    )
}

// ── Schemas ─────────────────────────────────────────────────────────────

fn nothing_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

fn instance_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"plugin_instance_id": {"type": "string"}},
        "required": ["plugin_instance_id"],
        "additionalProperties": false,
    })
}

fn moves_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "plugin_instance_id": {"type": "string"},
            "cursor": {"type": "string", "description": "next_cursor from the page before"},
        },
        "required": ["plugin_instance_id"],
        "additionalProperties": false,
    })
}

fn note_schema() -> Value {
    json!({"type": "string", "description": "why: required through /mcp", "maxLength": super::instruments::MOST_TEXT})
}

fn set_settings_schema() -> Value {
    let scalar = json!({"type": ["string", "integer", "boolean"]});
    json!({
        "type": "object",
        "properties": {
            "plugin_instance_id": {"type": "string"},
            "value": {"type": "object", "description": "value.<name>: a setting that is not secret, set", "additionalProperties": scalar},
            "table": {"type": "object", "description": "table.<name>: a table's rows, whole", "additionalProperties": {
                "type": "array",
                "items": {"type": "object", "additionalProperties": {"type": "string"}},
            }},
            "clear": {"type": "object", "description": "clear.<name>: true clears the setting, a secret included", "additionalProperties": {"type": "boolean", "const": true}},
            "against_updated_at_ns": {"type": "integer", "description": "updated_at_ns as read"},
            "note": note_schema(),
        },
        "required": ["plugin_instance_id", "note"],
        "additionalProperties": false,
    })
}

fn allow_archive_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "instance_id": {"type": "string"},
            "most_bytes": {"type": "integer", "minimum": 0, "description": "the most bytes it may hold; 0 for no bound"},
            "note": note_schema(),
        },
        "required": ["instance_id", "note"],
        "additionalProperties": false,
    })
}

fn withdraw_archive_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"instance_id": {"type": "string"}, "note": note_schema()},
        "required": ["instance_id", "note"],
        "additionalProperties": false,
    })
}

fn set_hold_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "role": {"type": "string", "description": "an edge role, or empty for every one"},
            "days": {"type": "integer", "minimum": 0, "maximum": 36500},
            "write_once": {"type": "boolean"},
            "note": note_schema(),
        },
        "required": ["days", "note"],
        "additionalProperties": false,
    })
}

fn launch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "version": {"type": "string"},
            "instance_id": {"type": "string"},
            "approved_roles": {"type": "array", "items": {"type": "string"}},
            "live": {"type": "boolean"},
            "note": note_schema(),
        },
        "required": ["name", "version", "instance_id", "approved_roles", "note"],
        "additionalProperties": false,
    })
}

fn stop_schema() -> Value {
    withdraw_archive_schema()
}

// ── Answers as JSON: each row's message by its own field names ──────────

fn enum_or_null(name: Option<&'static str>) -> Value {
    name.map(Value::from).unwrap_or(Value::Null)
}

fn level_json(level: i32) -> Value {
    enum_or_null(AccessLevel::try_from(level).ok().map(|l| l.as_str_name()))
}

fn span_json(span: &StoredSpan) -> Value {
    json!({
        "record_kind": span.record_kind,
        "record_count": span.record_count,
        "first_received_ns": span.first_received_ns,
        "last_received_ns": span.last_received_ns,
        "bytes": span.bytes,
    })
}

fn archive_json(archive: &PluginArchive) -> Value {
    json!({
        "instance_id": archive.instance_id,
        "allowed": archive.allowed,
        "most_bytes": archive.most_bytes,
        "updated_by": archive.updated_by,
        "updated_at_ns": archive.updated_at_ns,
        "acting_through_delegation": archive.acting_through_delegation,
        "client_name": archive.client_name,
    })
}

fn hold_json(hold: &Hold) -> Value {
    json!({
        "role": hold.role,
        "days": hold.days,
        "write_once": hold.write_once,
        "updated_by": hold.updated_by,
        "updated_at_ns": hold.updated_at_ns,
        "acting_through_delegation": hold.acting_through_delegation,
        "client_name": hold.client_name,
    })
}

fn move_json(record: &MoveRecord) -> Value {
    let moved = record.r#move.clone().unwrap_or_default();
    json!({
        "move": {
            "record_kind": moved.record_kind,
            "unit": moved.unit,
            "record_count": moved.record_count,
            "first_received_ns": moved.first_received_ns,
            "last_received_ns": moved.last_received_ns,
            "outcome": enum_or_null(MoveOutcome::try_from(moved.outcome).ok().map(|o| o.as_str_name())),
            "rule": moved.rule,
        },
        "person": record.person,
        "at_ns": record.at_ns,
        "acting_through_delegation": record.acting_through_delegation,
        "client_name": record.client_name,
    })
}

fn moves_json(reply: &ReadMovesReply) -> Value {
    json!({
        "moves": reply.moves.iter().map(move_json).collect::<Vec<_>>(),
        "next_cursor": reply.next_cursor,
        "archived": reply.archived.iter().map(span_json).collect::<Vec<_>>(),
        "archive": reply.archive.as_ref().map(archive_json),
    })
}

fn launch_json(launch: &PluginLaunch) -> Value {
    json!({
        "instance_id": launch.instance_id,
        "name": launch.name,
        "version": launch.version,
        "image_digest": launch.image_digest,
        "roles": launch.roles,
        "launched_by": launch.launched_by,
        "launched_at_ns": launch.launched_at_ns,
        "state": enum_or_null(PluginLaunchState::try_from(launch.state).ok().map(|s| s.as_str_name())),
        "stopped_by": launch.stopped_by,
        "stopped_at_ns": launch.stopped_at_ns,
        "failure": launch.failure,
        "live": launch.live,
        "acting_through_delegation": launch.acting_through_delegation,
        "client_name": launch.client_name,
        "stopped_through_delegation": launch.stopped_through_delegation,
        "stopped_client_name": launch.stopped_client_name,
    })
}

fn version_json(version: &PluginVersion) -> Value {
    let metadata = version.metadata.clone().unwrap_or_default();
    json!({
        "metadata": {
            "name": metadata.name,
            "version": metadata.version,
            "roles": metadata.roles,
            "interface": metadata.interface,
            "sdk_version": metadata.sdk_version,
            "declaration": metadata.declaration.as_ref().map(crate::declaration::to_json),
        },
        "image_digest": version.image_digest,
        "uploaded_by": version.uploaded_by,
        "uploaded_at_ns": version.uploaded_at_ns,
    })
}

fn figure_json(figure: &PluginFigure) -> Value {
    use meridian_pb::v1::plugin_figure::Value as Figure;
    let mut out = json!({
        "label": figure.label,
        "as_of_ns": figure.as_of_ns,
        "state": enum_or_null(meridian_pb::v1::FigureState::try_from(figure.state).ok().map(|s| s.as_str_name())),
        "why": figure.why,
    });
    match &figure.value {
        Some(Figure::Count(count)) => out["count"] = (*count).into(),
        // Exact, as text: never a float.
        Some(Figure::Decimal(decimal)) => {
            out["decimal"] = meridian_domain::exact::Exact::from_wire(decimal)
                .map(|exact| exact.to_string())
                .unwrap_or_else(|_| "out of range".into())
                .into()
        }
        Some(Figure::Text(said)) => out["text"] = said.clone().into(),
        Some(Figure::AtNs(at)) => out["at_ns"] = (*at).into(),
        None => {}
    }
    out
}

fn tool_json(tool: &ToolDeclaration) -> Value {
    json!({
        "name": tool.name,
        "title": tool.title,
        "description": tool.description,
        "levels": tool.levels.iter().map(|l| level_json(*l)).collect::<Vec<_>>(),
        "reads": tool.reads,
        "roles": tool.roles,
    })
}

fn last_change_json(change: &SettingLastChange) -> Value {
    json!({
        "name": change.name,
        "changed_by": change.changed_by,
        "changed_at_ns": change.changed_at_ns,
        "acting_through_delegation": change.acting_through_delegation,
        "client_name": change.client_name,
    })
}

fn declaration_json(declaration: &SettingDeclaration) -> Value {
    let named = |kind: i32| enum_or_null(SettingType::try_from(kind).ok().map(|t| t.as_str_name()));
    let choice = |c: &meridian_pb::v1::SettingChoice| json!({"value": c.value, "label": c.label, "description": c.description});
    json!({
        "name": declaration.name,
        "type": named(declaration.r#type),
        "required": declaration.required,
        "secret": declaration.secret,
        "description": declaration.description,
        "label": declaration.label,
        "default_value": declaration.default_value,
        "unit": declaration.unit,
        "choices": declaration.choices.iter().map(choice).collect::<Vec<_>>(),
        "applies_when": declaration.applies_when.as_ref().map(|c| json!({"setting": c.setting, "one_of": c.one_of})),
        "developer": declaration.developer,
        "columns": declaration.columns.iter().map(|c| json!({
            "name": c.name,
            "label": c.label,
            "type": enum_or_null(SettingColumnType::try_from(c.r#type).ok().map(|t| t.as_str_name())),
            "required": c.required,
            "description": c.description,
            "choices": c.choices.iter().map(choice).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "most_rows": declaration.most_rows,
        "roles": declaration.roles,
    })
}

/// The settings as the form shows this person (W6.11): the record by its
/// fields, a table's value its rows, and each declared setting the form
/// shows with whether they may set it (`may_set`) and, if not, why
/// (`detail`). Never a secret's value: the record holds none.
fn settings_json(
    record: &PluginSettingsRecord,
    held: &meridian_access::PluginHeld,
    plugin_roles: &[String],
    accounts: &[(String, String)],
) -> Value {
    let development = crate::html::is_development();
    let declared: Vec<Value> = record
        .declared_settings
        .iter()
        .filter(|d| development || !d.developer)
        .map(|declaration| {
            let mut shown = declaration_json(declaration);
            match settings::read_only(declaration, held, plugin_roles) {
                None => shown["may_set"] = true.into(),
                Some(why) => {
                    shown["may_set"] = false.into();
                    shown["detail"] = why.into();
                }
            }
            shown
        })
        .collect();
    let values: Vec<Value> = record
        .values
        .iter()
        .map(|held| {
            let table = record
                .declared_settings
                .iter()
                .find(|d| d.name == held.name)
                .is_some_and(setting_table::is_table);
            let value = if table {
                serde_json::from_str::<Value>(&held.value)
                    .unwrap_or_else(|_| held.value.clone().into())
            } else {
                held.value.clone().into()
            };
            json!({"name": held.name, "value": value})
        })
        .collect();
    json!({
        "plugin_instance_id": record.plugin_instance_id,
        "values": values,
        "secrets_set": record.secrets_set,
        "updated_at_ns": record.updated_at_ns,
        "updated_by": record.updated_by,
        "changes": record.changes.iter().map(last_change_json).collect::<Vec<_>>(),
        "declared_settings": declared,
        "accounts": accounts.iter().map(|(id, name)| json!({"external_account_id": id, "name": name})).collect::<Vec<_>>(),
    })
}

// ── Who reaches what ────────────────────────────────────────────────────

/// Every instance the deployment knows: reported, recorded, or configured.
fn known_instances(app: &App, records: &AccessRecords) -> BTreeSet<String> {
    let mut known: BTreeSet<String> = app.health.view().into_keys().collect();
    known.extend(
        records
            .known_plugins
            .iter()
            .map(|p| p.plugin_instance_id.clone()),
    );
    known.extend(
        records
            .plugin_settings
            .iter()
            .map(|r| r.plugin_instance_id.clone()),
    );
    known
}

/// The roles of an instance the person administers through the delegation:
/// empty for none; one empty role for a plugin holding none, administered
/// as a whole.
fn admin_roles(access: &Access, instance: &str) -> Vec<String> {
    access
        .plugin(instance)
        .roles
        .into_iter()
        .filter(|(_, held)| held.admin)
        .map(|(role, _)| role)
        .collect()
}

/// The gate as the call record names it (W6.20): `admin on snaptrade-1:custody`.
fn admin_level(instance: &str, roles: &[String]) -> String {
    let on: Vec<String> = roles
        .iter()
        .map(|role| {
            if role.is_empty() {
                instance.to_string()
            } else {
                format!("{instance}:{role}")
            }
        })
        .collect();
    format!("admin on {}", on.join(" and "))
}

/// The instances a gate reaches for this caller.
fn reached(gate: Gate, access: &Access, known: &BTreeSet<String>) -> Vec<String> {
    let admin = |instance: &String| !admin_roles(access, instance).is_empty();
    let mut all: BTreeSet<String> = known.clone();
    all.extend(access.plugins.keys().cloned());
    all.into_iter()
        .filter(|instance| match gate {
            Gate::Admin => admin(instance),
            Gate::AdminOrDeployment => access.deployment_admin || admin(instance),
            Gate::Anyone => access.deployment_admin || access.plugin(instance).holds_any(),
            Gate::DeploymentAdmin => access.deployment_admin,
        })
        .collect()
}

/// The gate held on one instance, as the record names it; or the refusal
/// naming the instances this delegation does reach, at `path`.
fn gated(
    gate: Gate,
    access: &Access,
    known: &BTreeSet<String>,
    instance: &str,
    path: &str,
) -> Result<String, Value> {
    let roles = admin_roles(access, instance);
    let level = match gate {
        _ if !roles.is_empty() && gate != Gate::DeploymentAdmin => {
            Some(admin_level(instance, &roles))
        }
        Gate::AdminOrDeployment | Gate::DeploymentAdmin | Gate::Anyone
            if access.deployment_admin && known.contains(instance) =>
        {
            Some("deployment admin".to_string())
        }
        _ => None,
    };
    if let Some(level) = level {
        return Ok(level);
    }
    let reaches = reached(gate, access, known);
    let unknown = !known.contains(instance);
    Err(refused(
        "not_listed",
        &if unknown {
            format!(
                "No plugin {instance} in this deployment. Through this delegation the tool reaches: {}.",
                said_list(&reaches)
            )
        } else {
            format!(
                "This delegation does not reach {instance} at this page's level. Through it the tool reaches: {}.",
                said_list(&reaches)
            )
        },
        vec![json!({"path": path})],
    ))
}

fn said_list(items: &[String]) -> String {
    if items.is_empty() {
        "no instance".to_string()
    } else {
        items.join(", ")
    }
}

// ── Calling ─────────────────────────────────────────────────────────────

const WAIT: Duration = Duration::from_secs(5);
const CHANGING: Duration = Duration::from_secs(20);
/// A launch, a stop or an archive's change waits on the launcher's asks.
const RESTARTING: Duration = Duration::from_secs(50);

/// The tool, called: its arguments read, its gate checked on the instance,
/// the row sent, the answer typed; and the gate it was admitted under, which
/// the call's record names.
pub async fn call(app: &App, caller: &Caller, spec: &Spec, arguments: Value) -> (Value, String) {
    let Area::Plugins(gate) = spec.area else {
        return (
            refused("not_listed", "not a plugin-area tool", Vec::new()),
            String::new(),
        );
    };
    let records = match app.records.current(app.clock.now_ns()) {
        Ok(records) => records,
        Err(stale) => {
            return (
                refused("unavailable", &stale.to_string(), Vec::new()),
                String::new(),
            )
        }
    };
    let access = caller.access(&records);
    let known = known_instances(app, &records);
    let at = Context {
        app,
        caller,
        records: &records,
        access: &access,
        known: &known,
        gate,
    };
    match spec.name {
        "list_plugins" => at.list_plugins(&arguments).await,
        "read_plugin_summary" => at.read_plugin_summary(&arguments).await,
        "read_moves" => at.read_moves(&arguments).await,
        "read_plugin_settings" => at.read_plugin_settings(&arguments).await,
        "set_plugin_settings" => at.set_plugin_settings(&arguments).await,
        "allow_archive" => at.allow_archive(&arguments).await,
        "withdraw_archive" => at.withdraw_archive(&arguments).await,
        "read_holds" => at.read_holds(&arguments),
        "set_hold" => at.set_hold(&arguments).await,
        "read_plugin_catalogue" => at.read_plugin_catalogue(&arguments).await,
        "launch_plugin" => at.launch_plugin(&arguments).await,
        "stop_plugin" => at.stop_plugin(&arguments).await,
        "read_plugin_access" => at.read_plugin_access(&arguments),
        other => (
            refused("not_listed", &format!("no tool {other}"), Vec::new()),
            String::new(),
        ),
    }
}

/// What one call works with.
struct Context<'a> {
    app: &'a App,
    caller: &'a Caller,
    records: &'a AccessRecords,
    access: &'a Access,
    known: &'a BTreeSet<String>,
    gate: Gate,
}

/// The gate a deployment admin's tool is admitted under.
const DEPLOYMENT_ADMIN: &str = "deployment admin";

/// Read `fields` of an object, refusing any other by its path.
fn top<'a>(
    arguments: &'a Value,
    known: &[&str],
    problems: &mut Problems,
) -> Option<&'a Map<String, Value>> {
    let top = object(arguments, "", problems)?;
    only(top, known, "", problems);
    Some(top)
}

fn boolean(top: &Map<String, Value>, name: &str, problems: &mut Problems) -> bool {
    match top.get(name) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            problems.add(name, "true or false");
            false
        }
    }
}

/// A scalar as the form would have posted it: text, a whole number, or
/// true or false.
fn posted(value: &Value) -> Option<String> {
    match value {
        Value::String(said) => Some(said.clone()),
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

impl Context<'_> {
    /// One instance named by `name`, its gate checked; or the refusal.
    fn instance(
        &self,
        top: &Map<String, Value>,
        name: &str,
        problems: &mut Problems,
    ) -> Option<(String, String)> {
        let instance = text(top, name, "", true, problems);
        if !problems.is_empty() {
            return None;
        }
        match gated(self.gate, self.access, self.known, &instance, name) {
            Ok(level) => Some((instance, level)),
            Err(refusal) => {
                problems.add(
                    name,
                    refusal["detail"].as_str().unwrap_or_default().to_string(),
                );
                None
            }
        }
    }

    async fn refresh(&self) {
        if let Err(failed) =
            crate::records::refresh(&self.app.bus, &self.app.records, self.app.clock.as_ref()).await
        {
            tracing::warn!("the records could not be re-read after a change: {failed}");
        }
    }

    async fn list_plugins(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        top(arguments, &[], &mut problems);
        let level = if self.access.deployment_admin {
            DEPLOYMENT_ADMIN.to_string()
        } else {
            "any level on a plugin".to_string()
        };
        if !problems.is_empty() {
            return (problems.refusal(), level);
        }
        let reports: BTreeMap<String, PluginReport> = self.app.health.view();
        let plugins: Vec<Value> = reached(Gate::Anyone, self.access, self.known)
            .into_iter()
            .map(|instance| {
                let held = self.access.plugin(&instance);
                let report = reports.get(&instance);
                let roles = report
                    .map(|r| r.roles.clone())
                    .filter(|roles| !roles.is_empty())
                    .or_else(|| self.access.known_roles.get(&instance).cloned())
                    .unwrap_or_default();
                let entries: Vec<Value> = held
                    .roles
                    .iter()
                    // As an access entry names it: admin, and the data
                    // level, each held on the role through the delegation.
                    .flat_map(|(role, on)| {
                        on.admin
                            .then_some(AccessLevel::Admin)
                            .into_iter()
                            .chain(on.data)
                            .map(move |level| json!({"role": role, "level": level.as_str_name()}))
                    })
                    .collect();
                let title = report
                    .and_then(|r| r.declared_interface.as_ref())
                    .map(|i| i.title.clone())
                    .unwrap_or_default();
                let state = crate::health::state(report, self.app.clock.now_ns());
                json!({
                    "plugin_instance_id": instance,
                    "title": title,
                    "roles": roles,
                    "entries": entries,
                    "registered": report.is_some_and(|r| r.registered),
                    "healthy": report.is_some_and(|r| r.healthy),
                    "health_detail": if state.detail.is_empty() { state.word.to_string() } else { format!("{}: {}", state.word, state.detail) },
                    "reported_at_ns": report.map_or(0, |r| r.reported_at_ns),
                })
            })
            .collect();
        (
            json!({"outcome": "unchanged", "data": {"plugins": plugins}}),
            level,
        )
    }

    async fn read_plugin_summary(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(arguments, &["plugin_instance_id"], &mut problems) else {
            return (problems.refusal(), String::new());
        };
        let Some((instance, level)) = self.instance(top, "plugin_instance_id", &mut problems)
        else {
            return (problems.refusal(), String::new());
        };
        let reports = self.app.health.view();
        let report = reports.get(&instance);
        let launch = crate::catalogue::launches(self.app)
            .await
            .into_iter()
            .find(|launch| launch.instance_id == instance);
        let custody = self.app.custody.view();
        let sync: Vec<Value> = custody
            .sync
            .iter()
            .filter(|((held_by, _), _)| *held_by == instance)
            .map(|(_, status)| {
                json!({
                    "external_account_id": status.external_account_id,
                    "account_id": status.account_id,
                    "source": status.source,
                    "state": enum_or_null(meridian_domain::v1::SyncState::try_from(status.state).ok().map(|s| s.as_str_name())),
                    "connection_healthy": status.connection_healthy,
                    "status_detail": status.status_detail,
                    "last_synced_at_ns": status.last_synced_at_ns,
                    "observed_at_ns": status.observed_at_ns,
                })
            })
            .collect();
        let mut data = json!({
            "plugin_instance_id": instance,
            "roles": report.map(|r| r.roles.clone()).unwrap_or_else(|| self.access.known_roles.get(&instance).cloned().unwrap_or_default()),
            "registered": report.is_some_and(|r| r.registered),
            "healthy": report.is_some_and(|r| r.healthy),
            "health_detail": report.map(|r| r.health_detail.clone()).unwrap_or_default(),
            "last_heartbeat_at_ns": report.map_or(0, |r| r.last_heartbeat_at_ns),
            "reported_at_ns": report.map_or(0, |r| r.reported_at_ns),
            "contract_version": report.map(|r| r.contract_version.clone()).unwrap_or_default(),
            "refused_grants": report.map_or(0, |r| r.refused_grants),
            "last_refusal_reason": report.map(|r| r.last_refusal_reason.clone()).unwrap_or_default(),
            "launch": launch.as_ref().map(launch_json),
            "sync": sync,
        });
        // The Summary's own parts, for an admin of any of its roles; a
        // deployment admin who is none reads the Overview's.
        if !admin_roles(self.access, &instance).is_empty() {
            let report_or = report.cloned().unwrap_or_default();
            data["figures"] = report_or
                .figures
                .iter()
                .map(figure_json)
                .collect::<Vec<_>>()
                .into();
            data["declaration"] = report_or
                .declaration
                .as_ref()
                .map(crate::declaration::to_json)
                .unwrap_or(Value::Null);
            data["not_carried_seen"] = report_or
                .not_carried_seen
                .iter()
                .map(|seen| json!({"scheme": seen.scheme, "name": seen.name, "count": seen.count}))
                .collect::<Vec<_>>()
                .into();
            data["declared_tools"] = report_or
                .declared_tools
                .iter()
                .map(tool_json)
                .collect::<Vec<_>>()
                .into();
            data["tool_refusals"] = report_or.tool_refusals.clone().into();
            data["stored"] = report_or
                .stored
                .iter()
                .map(span_json)
                .collect::<Vec<_>>()
                .into();
            if crate::archive::keeps_records(report) {
                data["holds"] = self
                    .records
                    .holds
                    .iter()
                    .filter(|hold| hold.role.is_empty() || report_or.roles.contains(&hold.role))
                    .map(hold_json)
                    .collect::<Vec<_>>()
                    .into();
                match self.moves(&instance, "").await {
                    Ok(reply) => {
                        data["archived"] = reply
                            .archived
                            .iter()
                            .map(span_json)
                            .collect::<Vec<_>>()
                            .into();
                        data["archive"] = reply
                            .archive
                            .as_ref()
                            .map(archive_json)
                            .unwrap_or(Value::Null);
                    }
                    Err(refusal) => return (refusal, level),
                }
            }
        }
        (json!({"outcome": "unchanged", "data": data}), level)
    }

    /// ReadMoves, for the person through the delegation.
    async fn moves(&self, instance: &str, cursor: &str) -> Result<ReadMovesReply, Value> {
        ask(
            self.app,
            self.caller,
            crate::archive::READ_MOVES,
            "meridian.v1.ReadMovesRequest",
            ReadMovesRequest {
                plugin_instance_id: instance.to_string(),
                cursor: cursor.to_string(),
            },
            WAIT,
        )
        .await
        .map_err(|failed| bus_refused(failed, ""))
    }

    async fn read_moves(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(arguments, &["plugin_instance_id", "cursor"], &mut problems) else {
            return (problems.refusal(), String::new());
        };
        let cursor = text(top, "cursor", "", false, &mut problems);
        let Some((instance, level)) = self.instance(top, "plugin_instance_id", &mut problems)
        else {
            return (problems.refusal(), String::new());
        };
        match self.moves(&instance, &cursor).await {
            Ok(reply) => (
                json!({"outcome": "unchanged", "data": moves_json(&reply)}),
                level,
            ),
            Err(refusal) => (refusal, level),
        }
    }

    /// The settings record, and what the person holds on the instance's
    /// roles and the plugin's roles, for the form's own checks.
    fn settings_of(&self, instance: &str) -> Option<(&PluginSettingsRecord, Vec<String>)> {
        let record = self
            .records
            .plugin_settings
            .iter()
            .find(|r| r.plugin_instance_id == instance)?;
        let roles = self
            .access
            .known_roles
            .get(instance)
            .cloned()
            .unwrap_or_default();
        Some((record, roles))
    }

    async fn accounts(&self, record: &PluginSettingsRecord) -> Vec<(String, String)> {
        if !settings::wants(record, SettingColumnType::ExternalAccount) {
            return Vec::new();
        }
        crate::admin::choices(self.app, &record.plugin_instance_id, record, self.records)
            .await
            .external_accounts
    }

    async fn read_plugin_settings(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(arguments, &["plugin_instance_id"], &mut problems) else {
            return (problems.refusal(), String::new());
        };
        let Some((instance, level)) = self.instance(top, "plugin_instance_id", &mut problems)
        else {
            return (problems.refusal(), String::new());
        };
        let Some((record, roles)) = self.settings_of(&instance) else {
            return (no_settings(&instance), level);
        };
        let held = self.access.plugin(&instance);
        let accounts = self.accounts(record).await;
        (
            json!({"outcome": "unchanged", "data": settings_json(record, &held, &roles, &accounts)}),
            level,
        )
    }

    async fn set_plugin_settings(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(
            arguments,
            &[
                "plugin_instance_id",
                "value",
                "table",
                "clear",
                "secret",
                "against_updated_at_ns",
                "note",
            ],
            &mut problems,
        ) else {
            return (problems.refusal(), String::new());
        };
        // A secret's value is never taken here, by name (the ruled
        // exception): a person enters it at the Settings form.
        if let Some(given) = top.get("secret") {
            let names: Vec<String> = given
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            if names.is_empty() {
                problems.add("secret", SECRET_REFUSED);
            }
            for name in names {
                problems.add(&format!("secret.{name}"), SECRET_REFUSED);
            }
        }
        let note = text(top, "note", "", true, &mut problems);
        let against = integer(top, "against_updated_at_ns", "", false, &mut problems);
        let instance = text(top, "plugin_instance_id", "", true, &mut problems);
        if instance.is_empty() {
            return (problems.refusal(), String::new());
        }
        let level = match gated(
            self.gate,
            self.access,
            self.known,
            &instance,
            "plugin_instance_id",
        ) {
            Ok(level) => level,
            Err(refusal) => return (refusal, String::new()),
        };
        let Some((record, _)) = self.settings_of(&instance) else {
            return (no_settings(&instance), level);
        };
        let development = crate::html::is_development();
        let declared = |name: &str| {
            record
                .declared_settings
                .iter()
                .find(|d| d.name == name && (development || !d.developer))
        };
        let secret = |d: &SettingDeclaration| d.secret || record.secrets_set.contains(&d.name);
        let mut fields = Fields::new();
        // Where each setting was named, for a refusal's path.
        let mut named_at: BTreeMap<String, String> = BTreeMap::new();
        if let Some(given) = top.get("value") {
            match given.as_object() {
                None => problems.add("value", "an object of settings by name"),
                Some(values) => {
                    for (name, value) in values {
                        let path = format!("value.{name}");
                        match declared(name) {
                            None => problems.add(&path, format!("no setting {name} is declared, or this deployment does not show it")),
                            Some(d) if secret(d) => problems.add(&path, SECRET_REFUSED),
                            Some(d) if setting_table::is_table(d) => problems.add(&path, format!("{name} is a table: its rows go under table.{name}")),
                            Some(_) => match posted(value) {
                                Some(text) => {
                                    fields.insert(format!("{VALUE_FIELD}{name}"), text);
                                    named_at.insert(name.clone(), path);
                                }
                                None => problems.add(&path, "text, a whole number, or true or false"),
                            },
                        }
                    }
                }
            }
        }
        if let Some(given) = top.get("table") {
            match given.as_object() {
                None => problems.add("table", "an object of tables by name, each a list of rows"),
                Some(tables) => {
                    for (name, rows) in tables {
                        let path = format!("table.{name}");
                        match declared(name) {
                            None => problems.add(&path, format!("no setting {name} is declared, or this deployment does not show it")),
                            Some(d) if secret(d) => problems.add(&path, SECRET_REFUSED),
                            Some(d) if !setting_table::is_table(d) => problems.add(&path, format!("{name} is not a table: its value goes under value.{name}")),
                            Some(_) => {
                                let Some(rows) = rows.as_array() else {
                                    problems.add(&path, "a list of rows, each its cells by column");
                                    continue;
                                };
                                fields.insert(format!("{TABLE_FIELD}{name}"), "1".into());
                                named_at.insert(name.clone(), path.clone());
                                for (n, row) in rows.iter().enumerate() {
                                    let at = format!("{path}[{n}]");
                                    let Some(cells) = row.as_object() else {
                                        problems.add(&at, "a row: its cells by column");
                                        continue;
                                    };
                                    for (column, cell) in cells {
                                        match posted(cell) {
                                            Some(text) => {
                                                fields.insert(format!("{TABLE_FIELD}{name}[{n}].{column}"), text);
                                            }
                                            None => problems.add(&format!("{at}.{column}"), "text"),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(given) = top.get("clear") {
            match given.as_object() {
                None => problems.add("clear", "an object of settings by name, each true"),
                Some(cleared) => {
                    for (name, ticked) in cleared {
                        let path = format!("clear.{name}");
                        if ticked != &Value::Bool(true) {
                            problems.add(&path, "true, to clear it");
                            continue;
                        }
                        match declared(name) {
                            None => problems.add(&path, format!("no setting {name} is declared, or this deployment does not show it")),
                            // The form's Clear: types nothing, reads nothing.
                            Some(d) if secret(d) => {
                                fields.insert(format!("{CLEAR_FIELD}{name}"), "on".into());
                            }
                            Some(d) if setting_table::is_table(d) => {
                                fields.insert(format!("{TABLE_FIELD}{name}"), "1".into());
                            }
                            Some(_) => {
                                fields.insert(format!("{VALUE_FIELD}{name}"), String::new());
                            }
                        }
                        named_at.insert(name.clone(), path);
                    }
                }
            }
        }
        if against != 0 {
            fields.insert(AGAINST_FIELD.into(), against.to_string());
        }
        if !problems.is_empty() {
            return (problems.refusal(), level);
        }
        let path_of = |name: &str| {
            named_at
                .get(name)
                .cloned()
                .unwrap_or_else(|| format!("value.{name}"))
        };
        let request = match crate::admin::settings_change(self.app, record, self.records, self.access, &fields).await {
            Err(SettingsRefused::Cells(cells, said)) => {
                return (
                    refused(
                        "invalid_arguments",
                        &said,
                        cells
                            .iter()
                            .map(|cell| json!({"path": format!("{TABLE_FIELD}{}", cell.path), "message": cell.message}))
                            .collect(),
                    ),
                    level,
                )
            }
            Err(SettingsRefused::NotAdministered(name, said)) => {
                return (refused("not_administered", &said, vec![json!({"path": path_of(&name)})]), level)
            }
            Ok(None) => {
                return (
                    json!({"outcome": "unchanged", "detail": "Nothing named differs from what is set.", "data": settings_json(record, &self.access.plugin(&instance), &self.access.known_roles.get(&instance).cloned().unwrap_or_default(), &[])}),
                    level,
                )
            }
            Ok(Some(request)) => request,
        };
        let request = meridian_domain::v1::SetPluginSettingsRequest {
            note,
            against_updated_at_ns: against,
            ..request
        };
        let named: Vec<String> = request
            .values
            .iter()
            .map(|v| v.name.clone())
            .chain(request.cleared.iter().cloned())
            .collect();
        let answered: Result<PluginSettingsRecord, _> = ask(
            self.app,
            self.caller,
            SET_PLUGIN_SETTINGS,
            "meridian.v1.SetPluginSettingsRequest",
            request,
            CHANGING,
        )
        .await;
        self.refresh().await;
        match answered {
            Ok(record) => {
                let held = self.access.plugin(&instance);
                let roles = self
                    .access
                    .known_roles
                    .get(&instance)
                    .cloned()
                    .unwrap_or_default();
                (
                    json!({"outcome": "made", "data": settings_json(&record, &held, &roles, &[])}),
                    level,
                )
            }
            Err((reason, detail, fields)) if fields.is_empty() => {
                // A sentence of the conductor's naming a setting -- a window
                // below the hold, say -- refused at that setting's path.
                let paths: Vec<Value> = named
                    .iter()
                    .find(|name| detail.contains(name.as_str()))
                    .map(|name| vec![json!({"path": path_of(name)})])
                    .unwrap_or_default();
                (refused(&reason, &detail, paths), level)
            }
            Err(failed) => (bus_refused(failed, ""), level),
        }
    }

    /// One act of a deployment admin's, its arguments read by `read`.
    async fn deployment_act<R: prost::Message + Default>(
        &self,
        topic: &str,
        request_type: &str,
        request: impl prost::Message,
        wait: Duration,
    ) -> Result<R, Value> {
        let answered = ask(self.app, self.caller, topic, request_type, request, wait).await;
        self.refresh().await;
        answered.map_err(|failed| bus_refused(failed, ""))
    }

    async fn allow_archive(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(
            arguments,
            &["instance_id", "most_bytes", "note"],
            &mut problems,
        ) else {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        };
        let note = text(top, "note", "", true, &mut problems);
        let most = integer(top, "most_bytes", "", false, &mut problems);
        if most < 0 {
            problems.add("most_bytes", "0 for no bound, or a number of bytes");
        }
        let Some((instance, level)) = self.instance(top, "instance_id", &mut problems) else {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        };
        if !problems.is_empty() {
            return (problems.refusal(), level);
        }
        match self
            .deployment_act::<PluginArchive>(
                crate::archive::ALLOW_ARCHIVE,
                "meridian.v1.AllowArchiveRequest",
                AllowArchiveRequest {
                    instance_id: instance.clone(),
                    most_bytes: most as u64,
                    note,
                },
                RESTARTING,
            )
            .await
        {
            Ok(archive) => (
                json!({"outcome": "made", "detail": format!("{instance} restarts with its archive."), "data": archive_json(&archive)}),
                level,
            ),
            Err(refusal) => (refusal, level),
        }
    }

    async fn withdraw_archive(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(arguments, &["instance_id", "note"], &mut problems) else {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        };
        let note = text(top, "note", "", true, &mut problems);
        let Some((instance, level)) = self.instance(top, "instance_id", &mut problems) else {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        };
        if !problems.is_empty() {
            return (problems.refusal(), level);
        }
        match self
            .deployment_act::<PluginArchive>(
                crate::archive::WITHDRAW_ARCHIVE,
                "meridian.v1.WithdrawArchiveRequest",
                WithdrawArchiveRequest {
                    instance_id: instance.clone(),
                    note,
                },
                RESTARTING,
            )
            .await
        {
            Ok(archive) if archive.updated_at_ns == 0 => (
                json!({"outcome": "unchanged", "detail": format!("{instance} is allowed no archive."), "data": archive_json(&archive)}),
                level,
            ),
            Ok(archive) => (
                json!({"outcome": "made", "detail": format!("{instance} restarts without its archive; what it holds is kept."), "data": archive_json(&archive)}),
                level,
            ),
            Err(refusal) => (refusal, level),
        }
    }

    fn read_holds(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        top(arguments, &[], &mut problems);
        if !problems.is_empty() {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        }
        (
            json!({"outcome": "unchanged", "data": {"holds": self.records.holds.iter().map(hold_json).collect::<Vec<_>>()}}),
            DEPLOYMENT_ADMIN.into(),
        )
    }

    async fn set_hold(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(
            arguments,
            &["role", "days", "write_once", "note"],
            &mut problems,
        ) else {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        };
        let role = text(top, "role", "", false, &mut problems);
        let days = integer(top, "days", "", true, &mut problems);
        let write_once = boolean(top, "write_once", &mut problems);
        let note = text(top, "note", "", true, &mut problems);
        let Ok(days) = u32::try_from(days) else {
            problems.add("days", "a whole number of days, 0 to clear");
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        };
        if !problems.is_empty() {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        }
        match self
            .deployment_act::<Hold>(
                SET_HOLD,
                "meridian.v1.SetHoldRequest",
                SetHoldRequest {
                    role,
                    days,
                    write_once,
                    note,
                },
                CHANGING,
            )
            .await
        {
            Ok(hold) => (
                json!({"outcome": "made", "data": hold_json(&hold)}),
                DEPLOYMENT_ADMIN.into(),
            ),
            Err(refusal) => (refusal, DEPLOYMENT_ADMIN.into()),
        }
    }

    async fn read_plugin_catalogue(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        top(arguments, &[], &mut problems);
        if !problems.is_empty() {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        }
        let answered: Result<PluginCatalogue, _> = ask(
            self.app,
            self.caller,
            crate::catalogue::PLUGIN_CATALOGUE,
            "meridian.v1.PluginCatalogueRequest",
            PluginCatalogueRequest {},
            WAIT,
        )
        .await;
        match answered {
            Ok(held) => (
                json!({"outcome": "unchanged", "data": {
                    "versions": held.versions.iter().map(version_json).collect::<Vec<_>>(),
                    "launches": held.launches.iter().map(launch_json).collect::<Vec<_>>(),
                }}),
                DEPLOYMENT_ADMIN.into(),
            ),
            Err(failed) => (bus_refused(failed, ""), DEPLOYMENT_ADMIN.into()),
        }
    }

    async fn launch_plugin(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(
            arguments,
            &[
                "name",
                "version",
                "instance_id",
                "approved_roles",
                "live",
                "note",
            ],
            &mut problems,
        ) else {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        };
        let name = text(top, "name", "", true, &mut problems);
        let version = text(top, "version", "", true, &mut problems);
        let instance_id = text(top, "instance_id", "", true, &mut problems);
        let live = boolean(top, "live", &mut problems);
        let note = text(top, "note", "", true, &mut problems);
        // Exactly as the CLI sends them: the conductor refuses any but the
        // version's own.
        let mut approved_roles = Vec::new();
        match top.get("approved_roles") {
            Some(Value::Array(roles)) => {
                for (n, role) in roles.iter().enumerate() {
                    match role.as_str() {
                        Some(role) => approved_roles.push(role.to_string()),
                        None => problems.add(&format!("approved_roles[{n}]"), "a role's name"),
                    }
                }
            }
            _ => problems.add(
                "approved_roles",
                "required: the roles approved, as the version declares them",
            ),
        }
        if !problems.is_empty() {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        }
        match self
            .deployment_act::<PluginLaunch>(
                crate::catalogue::LAUNCH_PLUGIN,
                "meridian.v1.LaunchPluginRequest",
                LaunchPluginRequest {
                    name,
                    version,
                    instance_id,
                    approved_roles,
                    live,
                    note,
                },
                RESTARTING,
            )
            .await
        {
            Ok(launch) => (
                json!({"outcome": "made", "data": launch_json(&launch)}),
                DEPLOYMENT_ADMIN.into(),
            ),
            Err(refusal) => (refusal, DEPLOYMENT_ADMIN.into()),
        }
    }

    async fn stop_plugin(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(arguments, &["instance_id", "note"], &mut problems) else {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        };
        let instance_id = text(top, "instance_id", "", true, &mut problems);
        let note = text(top, "note", "", true, &mut problems);
        if !problems.is_empty() {
            return (problems.refusal(), DEPLOYMENT_ADMIN.into());
        }
        match self
            .deployment_act::<PluginLaunch>(
                crate::catalogue::STOP_PLUGIN,
                "meridian.v1.StopPluginRequest",
                StopPluginRequest { instance_id, note },
                RESTARTING,
            )
            .await
        {
            Ok(launch) => (
                json!({"outcome": "made", "data": launch_json(&launch)}),
                DEPLOYMENT_ADMIN.into(),
            ),
            Err(refusal) => (refusal, DEPLOYMENT_ADMIN.into()),
        }
    }

    /// The Access tab, read only: the records naming the instance, by their
    /// own fields (AccessRecords). Nothing here changes any of it.
    fn read_plugin_access(&self, arguments: &Value) -> (Value, String) {
        let mut problems = Problems::default();
        let Some(top) = top(arguments, &["plugin_instance_id"], &mut problems) else {
            return (problems.refusal(), String::new());
        };
        let Some((instance, level)) = self.instance(top, "plugin_instance_id", &mut problems)
        else {
            return (problems.refusal(), String::new());
        };
        let records = self.records;
        let groups: Vec<Value> = records
            .access_groups
            .iter()
            .filter_map(|group| {
                let every = group.access_group_id == meridian_access::ALL_PLUGINS_ADMIN;
                let entries: Vec<Value> = group
                    .entries
                    .iter()
                    .filter(|e| e.plugin_instance_id == instance)
                    .map(|e| json!({"plugin_instance_id": e.plugin_instance_id, "role": e.role, "level": level_json(e.level)}))
                    .collect();
                (every || !entries.is_empty()).then(|| {
                    json!({"access_group_id": group.access_group_id, "name": group.name, "entries": entries, "built_in": group.built_in})
                })
            })
            .collect();
        let named: BTreeSet<&str> = groups
            .iter()
            .filter_map(|g| g["access_group_id"].as_str())
            .collect();
        let permissions: Vec<&meridian_domain::v1::Permission> = records
            .permissions
            .iter()
            .filter(|p| named.contains(p.access_group_id.as_str()))
            .collect();
        let user_groups: BTreeSet<&str> = permissions
            .iter()
            .map(|p| p.user_group_id.as_str())
            .collect();
        let account_groups: BTreeSet<&str> = permissions
            .iter()
            .map(|p| p.account_group_id.as_str())
            .collect();
        let data = json!({
            "plugin_instance_id": instance,
            "roles": self.access.known_roles.get(&instance).cloned().unwrap_or_default(),
            "access_groups": groups,
            "permissions": permissions.iter().map(|p| json!({
                "permission_id": p.permission_id,
                "user_group_id": p.user_group_id,
                "account_group_id": p.account_group_id,
                "access_group_id": p.access_group_id,
            })).collect::<Vec<_>>(),
            "user_groups": records.user_groups.iter().filter(|g| user_groups.contains(g.user_group_id.as_str()))
                .map(|g| json!({"user_group_id": g.user_group_id, "name": g.name})).collect::<Vec<_>>(),
            "account_groups": records.account_groups.iter().filter(|g| account_groups.contains(g.account_group_id.as_str()))
                .map(|g| json!({"account_group_id": g.account_group_id, "name": g.name})).collect::<Vec<_>>(),
        });
        (json!({"outcome": "unchanged", "data": data}), level)
    }
}

/// Why a secret's value is refused, wherever an argument names one.
pub const SECRET_REFUSED: &str =
    "a secret setting's value is never set or read through /mcp: a person enters it at the plugin's Settings form; clear.<name> clears it";

fn no_settings(instance: &str) -> Value {
    refused(
        "not_listed",
        &format!("no plugin {instance} has reported, so what it needs is not known yet"),
        vec![json!({"path": "plugin_instance_id"})],
    )
}

/// The routes and tabs core draws for a plugin, each against the tool that
/// reaches it or its ruled exception (W6.9, W6.20; the plan's "parity
/// check"). [`tests`] fails the build where a drawn tab or a changing route
/// has neither.
pub const PARITY: &[(&str, &str)] = &[
    // The home's plugin entries, and the admin portal's Plugins tab.
    ("home plugins", "list_plugins"),
    // The area under Manage: its tabs, the Summary's parts.
    ("tab summary", "read_plugin_summary"),
    ("tab settings", "read_plugin_settings set_plugin_settings"),
    ("tab setting-*", "read_plugin_settings set_plugin_settings"),
    ("part status", "read_plugin_summary"),
    ("part records", "read_plugin_summary"),
    ("part moves", "read_moves"),
    ("part declared", "read_plugin_summary"),
    ("part tools", "read_plugin_summary"),
    // The admin portal's view of the instance.
    ("view overview", "read_plugin_summary"),
    ("view settings", "read_plugin_settings set_plugin_settings"),
    ("view setting-*", "read_plugin_settings set_plugin_settings"),
    ("view access", "read_plugin_access"),
    // The routes.
    ("/plugins/{instance}", "read_plugin_summary"),
    ("/plugins/{instance}/settings", "set_plugin_settings"),
    ("/plugins/{instance}/archive", "allow_archive"),
    ("/plugins/{instance}/archive/withdraw", "withdraw_archive"),
    (
        "/plugins/{instance}/enter",
        "exception: the plugin's own pages, already tools of its own",
    ),
    (
        "/admin/plugins/{instance}",
        "read_plugin_summary read_plugin_access",
    ),
    (
        "/admin/plugins/{instance}/settings",
        "read_plugin_settings set_plugin_settings",
    ),
    ("/admin/holds", "read_holds set_hold"),
    (
        "/terminal/plugins",
        "read_plugin_catalogue; exception: upload stays the CLI's (the plan's Q2)",
    ),
    ("/terminal/plugins/launch", "launch_plugin"),
    ("/terminal/plugins/stop", "stop_plugin"),
    (
        "/terminal/plugins/{instance}/dev/{what}",
        "exception: live development, outside the plugin area",
    ),
    (
        "/terminal/plugins/{instance}/open",
        "exception: the plugin's own pages, already tools of its own",
    ),
    (
        "/terminal/plugins/{instance}/page",
        "exception: the plugin's own pages, already tools of its own",
    ),
    // Who holds access: read only, and never changed (the ruling of
    // 2026-10-01).
    (
        "access change",
        "exception: who holds access is never changed through the MCP",
    ),
    (
        "secret value",
        "exception: a secret's value is never read or typed through the MCP",
    ),
];

#[cfg(test)]
mod tests;
