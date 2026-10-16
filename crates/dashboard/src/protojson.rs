//! What a version's declaration says of its settings, interface and tools,
//! as the upload carries them (W8.1, contract v25): protobuf's JSON of the
//! messages registration sends (`SettingDeclaration`, `InterfaceDeclaration`,
//! `ToolDeclaration`), as the SDK writes it -- each field by its proto name,
//! a field at its default left out, each enum by its value's name
//! (`SETTING_TYPE_CHOICE`, `ACCESS_LEVEL_ADMIN`) -- read field by field, a
//! field the message does not have refused by its path, so what the version
//! declared is exactly what is kept. And written back in the same shape, for
//! the catalogue's listing and the tools.

use meridian_pb::v1::{
    AccessLevel, InterfaceDeclaration, PageDeclaration, SettingChoice, SettingColumn,
    SettingColumnType, SettingCondition, SettingDeclaration, SettingType, ToolDeclaration,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct ChoiceJson {
    value: String,
    label: String,
    description: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct ConditionJson {
    setting: String,
    one_of: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct ColumnJson {
    name: String,
    label: String,
    #[serde(rename = "type")]
    kind: String,
    required: bool,
    description: String,
    choices: Vec<ChoiceJson>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct SettingJson {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    required: bool,
    secret: bool,
    description: String,
    label: String,
    default_value: String,
    unit: String,
    choices: Vec<ChoiceJson>,
    applies_when: Option<ConditionJson>,
    developer: bool,
    columns: Vec<ColumnJson>,
    most_rows: i32,
    roles: Vec<String>,
    per_person: bool,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct PageJson {
    path: String,
    title: String,
    levels: Vec<String>,
    roles: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct InterfaceJson {
    loopback_port: u32,
    title: String,
    pages: Vec<PageJson>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct ToolJson {
    name: String,
    title: String,
    description: String,
    method: String,
    path: String,
    levels: Vec<String>,
    reads: bool,
    input_schema: String,
    output_schema: String,
    roles: Vec<String>,
    checked_by: String,
    accounts: Vec<String>,
    made_on: String,
}

fn read<T: for<'de> Deserialize<'de>>(value: &Value, at: &str) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|failed| format!("{at}: {failed}"))
}

fn level(name: &str, at: &str) -> Result<i32, String> {
    AccessLevel::from_str_name(name)
        .filter(|level| *level != AccessLevel::Unspecified)
        .map(|level| level as i32)
        .ok_or_else(|| {
            format!("{at} {name:?} is not an access level (ACCESS_LEVEL_READ, _WRITE or _ADMIN)")
        })
}

fn levels(names: &[String], at: &str) -> Result<Vec<i32>, String> {
    names
        .iter()
        .enumerate()
        .map(|(n, name)| level(name, &format!("{at}[{n}]")))
        .collect()
}

fn choices(held: Vec<ChoiceJson>) -> Vec<SettingChoice> {
    held.into_iter()
        .map(|choice| SettingChoice {
            value: choice.value,
            label: choice.label,
            description: choice.description,
        })
        .collect()
}

/// A setting as the upload carries it, or the refusal naming its path.
pub fn setting_from(value: &Value, at: &str) -> Result<SettingDeclaration, String> {
    let held: SettingJson = read(value, at)?;
    let kind = if held.kind.is_empty() {
        SettingType::Unspecified
    } else {
        SettingType::from_str_name(&held.kind)
            .ok_or_else(|| format!("{at}.type {:?} is not a setting's type", held.kind))?
    };
    let mut columns = Vec::new();
    for (n, column) in held.columns.into_iter().enumerate() {
        let column_type = if column.kind.is_empty() {
            SettingColumnType::Unspecified
        } else {
            SettingColumnType::from_str_name(&column.kind).ok_or_else(|| {
                format!(
                    "{at}.columns[{n}].type {:?} is not a column's type",
                    column.kind
                )
            })?
        };
        columns.push(SettingColumn {
            name: column.name,
            label: column.label,
            r#type: column_type as i32,
            required: column.required,
            description: column.description,
            choices: choices(column.choices),
        });
    }
    Ok(SettingDeclaration {
        name: held.name,
        r#type: kind as i32,
        required: held.required,
        secret: held.secret,
        description: held.description,
        label: held.label,
        default_value: held.default_value,
        unit: held.unit,
        choices: choices(held.choices),
        applies_when: held.applies_when.map(|condition| SettingCondition {
            setting: condition.setting,
            one_of: condition.one_of,
        }),
        developer: held.developer,
        columns,
        most_rows: held.most_rows,
        roles: held.roles,
        per_person: held.per_person,
    })
}

/// The interface as the upload carries it.
pub fn interface_from(value: &Value, at: &str) -> Result<InterfaceDeclaration, String> {
    let held: InterfaceJson = read(value, at)?;
    let mut pages = Vec::new();
    for (n, page) in held.pages.into_iter().enumerate() {
        pages.push(PageDeclaration {
            path: page.path,
            title: page.title,
            levels: levels(&page.levels, &format!("{at}.pages[{n}].levels"))?,
            roles: page.roles,
        });
    }
    Ok(InterfaceDeclaration {
        loopback_port: held.loopback_port,
        title: held.title,
        pages,
    })
}

/// A tool as the upload carries it.
pub fn tool_from(value: &Value, at: &str) -> Result<ToolDeclaration, String> {
    let held: ToolJson = read(value, at)?;
    Ok(ToolDeclaration {
        name: held.name,
        title: held.title,
        description: held.description,
        method: held.method,
        path: held.path,
        levels: levels(&held.levels, &format!("{at}.levels"))?,
        reads: held.reads,
        input_schema: held.input_schema,
        output_schema: held.output_schema,
        roles: held.roles,
        checked_by: held.checked_by,
        accounts: held.accounts,
        made_on: held.made_on,
    })
}

// ── Written back, as protobuf's JSON ───────────────────────────────────────

/// A field kept unless it is its default, as protobuf's JSON leaves it out.
fn put(out: &mut Map<String, Value>, name: &str, value: Value) {
    let default = match &value {
        Value::String(text) => text.is_empty(),
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_i64() == Some(0) || number.as_u64() == Some(0),
        Value::Array(items) => items.is_empty(),
        Value::Null => true,
        Value::Object(_) => false,
    };
    if !default {
        out.insert(name.to_string(), value);
    }
}

fn level_names(levels: &[i32]) -> Value {
    levels
        .iter()
        .map(|level| {
            AccessLevel::try_from(*level)
                .map(|level| level.as_str_name())
                .unwrap_or("ACCESS_LEVEL_UNSPECIFIED")
        })
        .collect::<Vec<_>>()
        .into()
}

fn choices_json(held: &[SettingChoice]) -> Value {
    held.iter()
        .map(|choice| {
            let mut out = Map::new();
            put(&mut out, "value", json!(choice.value));
            put(&mut out, "label", json!(choice.label));
            put(&mut out, "description", json!(choice.description));
            Value::Object(out)
        })
        .collect::<Vec<_>>()
        .into()
}

pub fn setting_json(setting: &SettingDeclaration) -> Value {
    let mut out = Map::new();
    put(&mut out, "name", json!(setting.name));
    if setting.r#type != 0 {
        out.insert(
            "type".into(),
            json!(SettingType::try_from(setting.r#type)
                .map(|kind| kind.as_str_name())
                .unwrap_or_default()),
        );
    }
    put(&mut out, "required", json!(setting.required));
    put(&mut out, "secret", json!(setting.secret));
    put(&mut out, "description", json!(setting.description));
    put(&mut out, "label", json!(setting.label));
    put(&mut out, "default_value", json!(setting.default_value));
    put(&mut out, "unit", json!(setting.unit));
    put(&mut out, "choices", choices_json(&setting.choices));
    if let Some(condition) = &setting.applies_when {
        let mut when = Map::new();
        put(&mut when, "setting", json!(condition.setting));
        put(&mut when, "one_of", json!(condition.one_of));
        out.insert("applies_when".into(), Value::Object(when));
    }
    put(&mut out, "developer", json!(setting.developer));
    put(
        &mut out,
        "columns",
        setting
            .columns
            .iter()
            .map(|column| {
                let mut held = Map::new();
                put(&mut held, "name", json!(column.name));
                put(&mut held, "label", json!(column.label));
                if column.r#type != 0 {
                    held.insert(
                        "type".into(),
                        json!(SettingColumnType::try_from(column.r#type)
                            .map(|kind| kind.as_str_name())
                            .unwrap_or_default()),
                    );
                }
                put(&mut held, "required", json!(column.required));
                put(&mut held, "description", json!(column.description));
                put(&mut held, "choices", choices_json(&column.choices));
                Value::Object(held)
            })
            .collect::<Vec<_>>()
            .into(),
    );
    put(&mut out, "most_rows", json!(setting.most_rows));
    put(&mut out, "roles", json!(setting.roles));
    put(&mut out, "per_person", json!(setting.per_person));
    Value::Object(out)
}

pub fn interface_json(interface: &InterfaceDeclaration) -> Value {
    let mut out = Map::new();
    put(&mut out, "loopback_port", json!(interface.loopback_port));
    put(&mut out, "title", json!(interface.title));
    put(
        &mut out,
        "pages",
        interface
            .pages
            .iter()
            .map(|page| {
                let mut held = Map::new();
                put(&mut held, "path", json!(page.path));
                put(&mut held, "title", json!(page.title));
                put(&mut held, "levels", level_names(&page.levels));
                put(&mut held, "roles", json!(page.roles));
                Value::Object(held)
            })
            .collect::<Vec<_>>()
            .into(),
    );
    Value::Object(out)
}

pub fn tool_json(tool: &ToolDeclaration) -> Value {
    let mut out = Map::new();
    put(&mut out, "name", json!(tool.name));
    put(&mut out, "title", json!(tool.title));
    put(&mut out, "description", json!(tool.description));
    put(&mut out, "method", json!(tool.method));
    put(&mut out, "path", json!(tool.path));
    put(&mut out, "levels", level_names(&tool.levels));
    put(&mut out, "reads", json!(tool.reads));
    put(&mut out, "input_schema", json!(tool.input_schema));
    put(&mut out, "output_schema", json!(tool.output_schema));
    put(&mut out, "roles", json!(tool.roles));
    put(&mut out, "checked_by", json!(tool.checked_by));
    put(&mut out, "accounts", json!(tool.accounts));
    put(&mut out, "made_on", json!(tool.made_on));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sdks_json_reads_back_whole_and_writes_the_same() {
        // As the SDK's json_format.MessageToDict writes them (e341c5d):
        // proto field names, enums by name, defaults left out.
        let setting = json!({
            "name": "accounts", "type": "SETTING_TYPE_TABLE", "required": true,
            "label": "Accounts", "description": "Which accounts",
            "columns": [{"name": "account", "label": "Account", "type": "SETTING_COLUMN_TYPE_EXTERNAL_ACCOUNT",
                         "required": true},
                        {"name": "kind", "type": "SETTING_COLUMN_TYPE_CHOICE",
                         "choices": [{"value": "cash", "label": "Cash"}]}],
            "most_rows": 50, "roles": ["custody"],
            "applies_when": {"setting": "mode", "one_of": ["live"]}
        });
        let read = setting_from(&setting, "settings[0]").unwrap();
        assert_eq!(read.r#type, SettingType::Table as i32);
        assert_eq!(read.columns[1].choices[0].label, "Cash");
        assert_eq!(setting_json(&read), setting);
        let interface = json!({"title": "Snaptrade", "loopback_port": 8080,
            "pages": [{"path": "/", "title": "Accounts",
                       "levels": ["ACCESS_LEVEL_ADMIN", "ACCESS_LEVEL_WRITE"], "roles": ["custody"]}]});
        let read = interface_from(&interface, "interface").unwrap();
        assert_eq!(read.pages[0].levels.len(), 2);
        assert_eq!(interface_json(&read), interface);
        let tool = json!({"name": "link_account", "title": "Link", "method": "POST",
            "path": "/link", "levels": ["ACCESS_LEVEL_ADMIN"], "input_schema": "{}",
            "roles": ["custody"], "checked_by": "check_link_account"});
        let read = tool_from(&tool, "tools[0]").unwrap();
        assert!(!read.reads);
        assert_eq!(tool_json(&read), tool);
    }

    #[test]
    fn a_field_the_message_lacks_or_an_enum_it_lacks_is_refused_by_its_path() {
        let refused =
            setting_from(&json!({"name": "a", "colour": "red"}), "settings[2]").unwrap_err();
        assert!(
            refused.starts_with("settings[2]") && refused.contains("colour"),
            "{refused}"
        );
        let refused =
            setting_from(&json!({"name": "a", "type": "TEXT"}), "settings[0]").unwrap_err();
        assert!(refused.contains("settings[0].type"), "{refused}");
        let refused =
            tool_from(&json!({"name": "a", "levels": ["ADMIN"]}), "tools[1]").unwrap_err();
        assert!(refused.contains("tools[1].levels[0]"), "{refused}");
    }
}
