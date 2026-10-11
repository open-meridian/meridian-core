//! A plugin's tools on the deployment's MCP surface (W4.1, W4.8, W4.9,
//! W6.20; contract v12; spec/a-deployment-serves-its-mcp, requirements 10
//! and 13).
//!
//! **At registration** each declared tool is checked, and one that fails is
//! refused by name without refusing the plugin: its name malformed, repeated
//! or too long for this instance's surface name; its path not a path on the
//! plugin's host, or under `/.meridian`; its method not one a route takes;
//! its levels none or outside the three; its input schema not a JSON object
//! schema; its title, description or schemas past their bounds; or past the
//! 200th. The report carries the tools admitted and a sentence for each
//! refused, which the plugin's Summary shows.
//!
//! **At the front door** a request whose claims name a tool is admitted only
//! at that tool's declared method and path, so a tool's claim -- which the
//! SDK serves without a browser's form token -- can never be used to reach
//! another route. Every bound is the data dictionary's.

use meridian_pb::bounds::{
    Length, REGISTER_REQUEST_TOOLS_COUNT, TOOL_DECLARATION_DESCRIPTION_LENGTH,
    TOOL_DECLARATION_INPUT_SCHEMA_LENGTH, TOOL_DECLARATION_NAME_LENGTH,
    TOOL_DECLARATION_OUTPUT_SCHEMA_LENGTH, TOOL_DECLARATION_PATH_LENGTH,
    TOOL_DECLARATION_TITLE_LENGTH,
};
use meridian_pb::v1::{AccessLevel, ToolDeclaration};

/// The longest a tool's name on the surface may be, `{instance}__{name}`:
/// what the clients people use accept.
pub const MOST_SURFACE_NAME: usize = 64;

/// The methods a tool's route may take.
pub const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

fn within(text: &str, bound: Length) -> bool {
    bound.admits(text.chars().count())
}

/// Why one tool is refused, or None when it stands.
fn refused(tool: &ToolDeclaration, instance: &str) -> Option<String> {
    let name = &tool.name;
    let named = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !named || !within(name, TOOL_DECLARATION_NAME_LENGTH) {
        return Some(format!(
            "the tool {name:?} is refused: a tool's name is {} to {} lower-case letters, digits, _ and -, beginning with a letter or digit",
            TOOL_DECLARATION_NAME_LENGTH.least, TOOL_DECLARATION_NAME_LENGTH.most
        ));
    }
    let surface = instance.len() + 2 + name.len();
    if surface > MOST_SURFACE_NAME {
        return Some(format!(
            "the tool {name} is refused: {instance}__{name} is {surface} characters, and a tool's name on the surface is at most {MOST_SURFACE_NAME}"
        ));
    }
    if !within(&tool.title, TOOL_DECLARATION_TITLE_LENGTH) {
        return Some(format!(
            "the tool {name} is refused: its title is {} to {} characters",
            TOOL_DECLARATION_TITLE_LENGTH.least, TOOL_DECLARATION_TITLE_LENGTH.most
        ));
    }
    if !within(&tool.description, TOOL_DECLARATION_DESCRIPTION_LENGTH) {
        return Some(format!(
            "the tool {name} is refused: its description is {} to {} characters",
            TOOL_DECLARATION_DESCRIPTION_LENGTH.least, TOOL_DECLARATION_DESCRIPTION_LENGTH.most
        ));
    }
    if !METHODS.contains(&tool.method.as_str()) {
        return Some(format!(
            "the tool {name} is refused: {:?} is not a method a route takes ({})",
            tool.method,
            METHODS.join(", ")
        ));
    }
    let path = &tool.path;
    let first = path
        .get(1..)
        .unwrap_or_default()
        .split(['/', '?'])
        .next()
        .unwrap_or_default();
    if !within(path, TOOL_DECLARATION_PATH_LENGTH)
        || !path.starts_with('/')
        || path.starts_with("//")
        || first == ".meridian"
        || path.contains('?')
        || path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | '#' | ' '))
    {
        return Some(format!(
            "the tool {name} is refused: {path:?} is not a path on the plugin's host (one leading /, no query, not under /.meridian)"
        ));
    }
    let served = |level: &i32| {
        matches!(
            AccessLevel::try_from(*level),
            Ok(AccessLevel::Admin | AccessLevel::Write | AccessLevel::Read)
        )
    };
    if tool.levels.is_empty() || !tool.levels.iter().all(served) {
        return Some(format!(
            "the tool {name} is refused: it serves no level, or one outside admin, write and read"
        ));
    }
    if !within(&tool.input_schema, TOOL_DECLARATION_INPUT_SCHEMA_LENGTH)
        || !within(&tool.output_schema, TOOL_DECLARATION_OUTPUT_SCHEMA_LENGTH)
    {
        return Some(format!(
            "the tool {name} is refused: a schema is at most {} characters",
            TOOL_DECLARATION_INPUT_SCHEMA_LENGTH.most
        ));
    }
    let object = |text: &str| {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .is_some_and(|schema| schema.get("type").and_then(|t| t.as_str()) == Some("object"))
    };
    if !object(&tool.input_schema) {
        return Some(format!(
            "the tool {name} is refused: its input schema is not a JSON Schema of an object"
        ));
    }
    if !tool.output_schema.is_empty() && !object(&tool.output_schema) {
        return Some(format!(
            "the tool {name} is refused: its output schema is not a JSON Schema of an object"
        ));
    }
    None
}

/// The tools a plugin declared, split into those admitted and a sentence
/// for each refused, in its order. Each admitted tool's roles are those it
/// serves, filled where it names none and may (contract v15,
/// [`crate::roles`]); one naming a role the plugin was not launched with, or
/// none on a plugin holding several from v15, is refused by name.
pub fn checked(
    declared: &[ToolDeclaration],
    instance: &str,
    launched: &[String],
    built_at: u32,
) -> (Vec<ToolDeclaration>, Vec<String>) {
    let mut admitted: Vec<ToolDeclaration> = Vec::new();
    let mut refusals = Vec::new();
    for (index, tool) in declared.iter().enumerate() {
        if index >= REGISTER_REQUEST_TOOLS_COUNT.most {
            refusals.push(format!(
                "the tool {:?} is refused: a plugin declares at most {} tools",
                tool.name, REGISTER_REQUEST_TOOLS_COUNT.most
            ));
            continue;
        }
        if let Some(why) = refused(tool, instance) {
            refusals.push(why);
            continue;
        }
        let roles = match crate::roles::served(&tool.roles, launched, built_at) {
            Ok(roles) => roles,
            Err(why) => {
                refusals.push(format!("the tool {} is refused: it {why}", tool.name));
                continue;
            }
        };
        if admitted.iter().any(|held| held.name == tool.name) {
            refusals.push(format!(
                "the tool {} is refused: it is declared twice",
                tool.name
            ));
            continue;
        }
        if admitted
            .iter()
            .any(|held| held.method == tool.method && held.path == tool.path)
        {
            refusals.push(format!(
                "the tool {} is refused: another tool is already at {} {}",
                tool.name, tool.method, tool.path
            ));
            continue;
        }
        admitted.push(ToolDeclaration {
            roles,
            ..tool.clone()
        });
    }
    (admitted, refusals)
}

/// The most input paths an act's `accounts` names (contract v20).
pub const MOST_ACCOUNT_PATHS: usize = 8;

/// Whether `path` -- the dictionary's grammar, `account_id` or
/// `positions[].account_id` -- names a property the input schema has.
pub fn schema_has(input_schema: &str, path: &str) -> bool {
    let Ok(mut schema) = serde_json::from_str::<serde_json::Value>(input_schema) else {
        return false;
    };
    for part in path.split('.') {
        let (name, each) = match part.strip_suffix("[]") {
            Some(name) => (name, true),
            None => (part, false),
        };
        if name.is_empty() {
            return false;
        }
        let Some(next) = schema
            .get("properties")
            .and_then(|properties| properties.get(name))
            .cloned()
        else {
            return false;
        };
        schema = if each {
            match next.get("items") {
                Some(items) => items.clone(),
                None => return false,
            }
        } else {
            next
        };
    }
    true
}

/// Contract v20 (W4.1, W6.26): what makes each admitted act preparable,
/// held to its rules -- `checked_by` one of this plugin's read tools and set
/// only on an act; `accounts` at most eight paths its input schema has;
/// `made_on` one of the pages the plugin declares. A tool failing one is
/// refused by name, the plugin admitted, as every tool's refusal is.
pub fn preparable(
    admitted: Vec<ToolDeclaration>,
    pages: &[String],
) -> (Vec<ToolDeclaration>, Vec<String>) {
    let reads: Vec<String> = admitted
        .iter()
        .filter(|tool| tool.reads)
        .map(|tool| tool.name.clone())
        .collect();
    let mut kept = Vec::new();
    let mut refusals = Vec::new();
    for tool in admitted {
        let name = &tool.name;
        let refusal = if !tool.checked_by.is_empty() && tool.reads {
            Some(format!(
                "the tool {name} is refused: checked_by names a check tool, and {name} is a read, which changes nothing to check"
            ))
        } else if !tool.checked_by.is_empty() && !reads.contains(&tool.checked_by) {
            Some(format!(
                "the tool {name} is refused: checked_by names {:?}, which is no read tool of this plugin",
                tool.checked_by
            ))
        } else if tool.accounts.len() > MOST_ACCOUNT_PATHS {
            Some(format!(
                "the tool {name} is refused: accounts names {} paths; at most {MOST_ACCOUNT_PATHS}",
                tool.accounts.len()
            ))
        } else if let Some(stranger) = tool
            .accounts
            .iter()
            .find(|path| !schema_has(&tool.input_schema, path))
        {
            Some(format!(
                "the tool {name} is refused: accounts names {stranger:?}, which its input schema does not have"
            ))
        } else if !tool.made_on.is_empty() && !pages.contains(&tool.made_on) {
            Some(format!(
                "the tool {name} is refused: made_on names {:?}, which is none of the pages this plugin declares",
                tool.made_on
            ))
        } else {
            None
        };
        match refusal {
            Some(refusal) => refusals.push(refusal),
            None => kept.push(tool),
        }
    }
    (kept, refusals)
}

/// Whether a request naming the tool `name` may pass at `method` and
/// `path` (its query aside): only at that tool's route (W4.9); or why not.
pub fn admits(
    tools: &[ToolDeclaration],
    name: &str,
    method: &str,
    path: &str,
) -> Result<(), String> {
    let asked = path.split('?').next().unwrap_or(path);
    match tools.iter().find(|tool| tool.name == name) {
        None => Err(format!(
            "the assertion names the tool {name}, which this plugin does not offer"
        )),
        Some(tool) if tool.method.eq_ignore_ascii_case(method) && tool.path == asked => Ok(()),
        Some(tool) => Err(format!(
            "the assertion names the tool {name}, which is served at {} {}, not {method} {asked}",
            tool.method, tool.path
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, method: &str, path: &str) -> ToolDeclaration {
        ToolDeclaration {
            name: name.into(),
            title: "A tool".into(),
            description: "Does a thing.".into(),
            method: method.into(),
            path: path.into(),
            levels: vec![AccessLevel::Write as i32],
            reads: false,
            input_schema: r#"{"type":"object"}"#.into(),
            output_schema: String::new(),
            roles: vec![],
            checked_by: String::new(),
            accounts: vec![],
            made_on: String::new(),
        }
    }

    #[test]
    fn an_acts_check_tool_accounts_and_page_are_held_to_their_rules() {
        let mut check = tool("check_draft", "POST", "/check");
        check.reads = true;
        check.input_schema = r#"{"type":"object","properties":{"account_id":{"type":"string"},"positions":{"type":"array","items":{"type":"object","properties":{"account_id":{"type":"string"}}}}}}"#.into();
        let act = |checked_by: &str, accounts: &[&str], made_on: &str| ToolDeclaration {
            checked_by: checked_by.into(),
            accounts: accounts.iter().map(|path| path.to_string()).collect(),
            made_on: made_on.into(),
            input_schema: check.input_schema.clone(),
            ..tool("confirm", "POST", "/confirm")
        };
        let pages = vec!["/opening".to_string()];
        let good = act(
            "check_draft",
            &["account_id", "positions[].account_id"],
            "/opening",
        );
        let (kept, refused) = preparable(vec![check.clone(), good], &pages);
        assert_eq!(kept.len(), 2, "{refused:?}");
        for (bad, says) in [
            (act("nothing", &[], ""), "no read tool"),
            (
                act("check_draft", &["owner_id"], ""),
                "input schema does not have",
            ),
            (
                act("check_draft", &["positions.account_id"], ""),
                "input schema does not have",
            ),
            (act("check_draft", &[], "/elsewhere"), "none of the pages"),
        ] {
            let (kept, refused) = preparable(vec![check.clone(), bad], &pages);
            assert_eq!(kept.len(), 1);
            assert!(refused[0].contains(says), "{refused:?}");
        }
        let mut read = check.clone();
        read.checked_by = "check_draft".into();
        let (_, refused) = preparable(vec![check.clone(), read], &pages);
        assert!(refused[0].contains("is a read"), "{refused:?}");
    }

    #[test]
    fn a_tool_that_cannot_be_checked_is_refused_by_name_and_the_rest_stand() {
        let declared = vec![
            tool("confirm_opening_balance", "POST", "/opening/confirm"),
            tool("Sync-Now", "POST", "/sync"),
            tool("peek", "GET", "/.meridian/enter"),
            tool("confirm_opening_balance", "POST", "/elsewhere"),
            tool("bad_method", "TRACE", "/x"),
            ToolDeclaration {
                levels: vec![],
                ..tool("no_level", "POST", "/y")
            },
            ToolDeclaration {
                input_schema: "[]".into(),
                ..tool("no_object", "POST", "/z")
            },
            ToolDeclaration {
                description: "x".repeat(1025),
                ..tool("wordy", "POST", "/w")
            },
        ];
        let (admitted, refused) = checked(&declared, "operations-1", &[], 14);
        assert_eq!(
            admitted.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["confirm_opening_balance"]
        );
        assert_eq!(refused.len(), 7, "{refused:?}");
        assert!(refused[0].contains("Sync-Now"));
        assert!(refused[1].contains("/.meridian"));
        assert!(refused[2].contains("declared twice"));
    }

    #[test]
    fn a_name_too_long_for_its_instance_is_refused_and_past_200_too() {
        let long = "a".repeat(60);
        let (_, refused) = checked(&[tool(&long, "POST", "/a")], "operations-sample-1", &[], 14);
        assert!(refused[0].contains("at most 64"), "{refused:?}");
        let many: Vec<_> = (0..201)
            .map(|n| tool(&format!("t{n}"), "POST", &format!("/t{n}")))
            .collect();
        let (admitted, refused) = checked(&many, "ops", &[], 14);
        assert_eq!((admitted.len(), refused.len()), (200, 1));
    }

    #[test]
    fn a_tools_roles_are_filled_or_it_is_refused_by_name() {
        let both = vec!["custody".to_string(), "operations".to_string()];
        let declared = vec![
            ToolDeclaration {
                roles: vec!["custody".into()],
                ..tool("record_statement", "POST", "/statement")
            },
            tool("role_less", "POST", "/opening"),
            ToolDeclaration {
                roles: vec!["oms".into()],
                ..tool("stranger", "POST", "/order")
            },
        ];
        let (admitted, refused) = checked(&declared, "ops-1", &both, 15);
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].roles, ["custody"]);
        assert!(refused[0].contains("role_less") && refused[0].contains("names no role"));
        assert!(refused[1].contains("stranger") && refused[1].contains("oms"));
        let (older, _) = checked(&declared[1..2], "ops-1", &both, 14);
        assert_eq!(older[0].roles, both, "built before v15: every role");
        let (one, _) = checked(&declared[1..2], "ops-1", &both[..1], 15);
        assert_eq!(one[0].roles, ["custody"], "one role fills it");
    }

    #[test]
    fn a_tools_claim_passes_only_at_its_route() {
        let tools = vec![tool("confirm", "POST", "/opening/confirm")];
        assert!(admits(&tools, "confirm", "POST", "/opening/confirm").is_ok());
        assert!(admits(&tools, "confirm", "POST", "/opening/confirm?x=1").is_ok());
        assert!(admits(&tools, "confirm", "POST", "/opening/other").is_err());
        assert!(admits(&tools, "confirm", "GET", "/opening/confirm").is_err());
        assert!(admits(&tools, "other", "POST", "/opening/confirm").is_err());
    }
}
