//! The trading instructions (W11.15, W11.16; contract v22): active, pause or
//! stop, set at the dashboard by a deployment admin on an account, All
//! accounts, a plugin, a person or a service account, and by a plugin's
//! admin on their plugin or a person, service account or account within it.
//!
//! The book records each with its setter's level, which the dashboard stamps
//! from the setter's claims and nobody enters (approved 2026-10-12): a
//! deployment admin's at that level, anybody else's at a plugin admin's, for
//! a plugin they administer alone. The most restrictive in force governs, an
//! entry is lifted only at the level that set it, and the book refuses and
//! cancels nothing for any: each plugin honours it. A user group is refused
//! as a target in v22 (the product owner, 2026-10-12), its persons not
//! resolved until v27.
//!
//! One page, `/admin/trading-instructions`, for both: a deployment admin sees
//! every instruction in force; a plugin's admin those naming the plugins they
//! administer. One line an instruction, its Change opening the dialog that
//! sets one; nothing stands between a person and a stop. The tools at parity
//! are dashboard__list_trading_instructions and
//! dashboard__set_trading_instruction (the refresh's Q44).

use std::time::Duration;

use meridian_access::Access;
use meridian_domain::v1::{
    AccessRecords, ListTradingInstructionsReply, ListTradingInstructionsRequest,
    SetTradingInstructionRequest, SetterLevel, TradingInstruction, TradingInstructionTarget,
    TradingInstructionValue,
};
use prost::Message;

use crate::html::escape;

pub const SET_TRADING_INSTRUCTION: &str = "platform.book.command.set-trading-instruction";
pub const LIST_TRADING_INSTRUCTIONS: &str = "platform.book.query.list-trading-instructions";
pub const PATH: &str = "/admin/trading-instructions";

const WAIT: Duration = Duration::from_secs(5);

/// The values, by the word the form and the tool use.
pub const VALUES: [(TradingInstructionValue, &str, &str); 3] = [
    (TradingInstructionValue::Active, "active", "Active"),
    (TradingInstructionValue::Pause, "pause", "Pause"),
    (TradingInstructionValue::Stop, "stop", "Stop"),
];

pub fn value_of(word: &str) -> Option<TradingInstructionValue> {
    VALUES
        .iter()
        .find(|(_, said, _)| *said == word)
        .map(|(value, _, _)| *value)
}

pub fn value_word(value: i32) -> &'static str {
    VALUES
        .iter()
        .find(|(held, _, _)| *held as i32 == value)
        .map(|(_, said, _)| *said)
        .unwrap_or("")
}

fn value_said(value: i32) -> &'static str {
    VALUES
        .iter()
        .find(|(held, _, _)| *held as i32 == value)
        .map(|(_, _, said)| *said)
        .unwrap_or("Not said")
}

pub fn level_word(level: i32) -> &'static str {
    match SetterLevel::try_from(level) {
        Ok(SetterLevel::DeploymentAdmin) => "deployment_admin",
        Ok(SetterLevel::PluginAdmin) => "plugin_admin",
        _ => "",
    }
}

fn level_said(level: i32) -> &'static str {
    match SetterLevel::try_from(level) {
        Ok(SetterLevel::DeploymentAdmin) => "Deployment admin",
        Ok(SetterLevel::PluginAdmin) => "Plugin admin",
        _ => "Not said",
    }
}

/// The level a setter sets at, from their access: a deployment admin's, or
/// a plugin admin's on a plugin they administer; otherwise refused, naming
/// why (W11.15: "a target outside the setter's reach").
pub fn level_for(
    access: &Access,
    target: &TradingInstructionTarget,
) -> Result<SetterLevel, String> {
    if access.deployment_admin {
        return Ok(SetterLevel::DeploymentAdmin);
    }
    if target.instance_id.is_empty() || target.all_accounts || !target.user_group_id.is_empty() {
        return Err(
            "a plugin's admin sets an instruction on their plugin, or on a person, service \
             account or account within it, naming the plugin"
                .into(),
        );
    }
    if !access.plugin(&target.instance_id).administers_any_of(&[]) {
        return Err(format!(
            "{} is not a plugin you administer",
            target.instance_id
        ));
    }
    Ok(SetterLevel::PluginAdmin)
}

/// Whether an instruction is the viewer's to see here: every one for a
/// deployment admin, those naming a plugin they administer for its admin.
pub fn visible(access: &Access, instruction: &TradingInstruction) -> bool {
    if access.deployment_admin {
        return true;
    }
    instruction.target.as_ref().is_some_and(|target| {
        !target.instance_id.is_empty() && access.plugin(&target.instance_id).administers_any_of(&[])
    })
}

/// A target in words: "All accounts", "Account ACC-1", "Plugin snaptrade-1",
/// "Person ada@example.com on snaptrade-1".
pub fn target_said(records: &AccessRecords, target: &TradingInstructionTarget) -> String {
    let within = if target.instance_id.is_empty() {
        String::new()
    } else {
        format!(" on {}", target.instance_id)
    };
    if target.all_accounts {
        "All accounts".into()
    } else if !target.account_id.is_empty() {
        let name = records
            .accounts
            .iter()
            .find(|a| a.account_id == target.account_id)
            .map(|a| a.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| target.account_id.clone());
        format!("Account {name}{within}")
    } else if !target.subject.is_empty() {
        format!(
            "Person {}{within}",
            crate::tickets::display_name(records, &target.subject)
        )
    } else if !target.service_account_id.is_empty() {
        format!("Service account {}{within}", target.service_account_id)
    } else if !target.user_group_id.is_empty() {
        format!("User group {}", target.user_group_id)
    } else if !target.instance_id.is_empty() {
        format!("Plugin {}", target.instance_id)
    } else {
        "Nothing named".into()
    }
}

/// The target a form names: its kind -- `all`, `plugin`, `account`,
/// `person`, `service_account` -- the identifier for the last three, and the
/// plugin, alone or beside them.
pub fn target_from(
    kind: &str,
    id: &str,
    instance: &str,
) -> Result<TradingInstructionTarget, String> {
    let id = id.trim().to_string();
    let instance = instance.trim().to_string();
    let named = |what: &str| -> Result<String, String> {
        if id.is_empty() {
            Err(format!("name the {what} the instruction is on"))
        } else {
            Ok(id.clone())
        }
    };
    let target = match kind {
        "all" => {
            if !instance.is_empty() {
                return Err("All accounts names no plugin".into());
            }
            TradingInstructionTarget {
                all_accounts: true,
                ..Default::default()
            }
        }
        "plugin" => {
            if instance.is_empty() {
                return Err("choose the plugin the instruction is on".into());
            }
            TradingInstructionTarget {
                instance_id: instance,
                ..Default::default()
            }
        }
        "account" => TradingInstructionTarget {
            account_id: named("account")?,
            instance_id: instance,
            ..Default::default()
        },
        "person" => TradingInstructionTarget {
            subject: named("person")?,
            instance_id: instance,
            ..Default::default()
        },
        "service_account" => TradingInstructionTarget {
            service_account_id: named("service account")?,
            instance_id: instance,
            ..Default::default()
        },
        other => return Err(format!("{other:?} is not a target an instruction names")),
    };
    Ok(target)
}

/// A form's kind, identifier and plugin for a target, to fill the dialog.
fn form_of(target: &TradingInstructionTarget) -> (&'static str, String) {
    if target.all_accounts {
        ("all", String::new())
    } else if !target.account_id.is_empty() {
        ("account", target.account_id.clone())
    } else if !target.subject.is_empty() {
        ("person", target.subject.clone())
    } else if !target.service_account_id.is_empty() {
        ("service_account", target.service_account_id.clone())
    } else {
        ("plugin", String::new())
    }
}

/// The target as dashboard__set_trading_instruction's argument: the
/// contract's own fields, only those set.
pub fn target_json(target: &TradingInstructionTarget) -> serde_json::Value {
    let mut said = serde_json::Map::new();
    if target.all_accounts {
        said.insert("all_accounts".into(), true.into());
    }
    for (name, value) in [
        ("account_id", &target.account_id),
        ("instance_id", &target.instance_id),
        ("subject", &target.subject),
        ("service_account_id", &target.service_account_id),
        ("user_group_id", &target.user_group_id),
    ] {
        if !value.is_empty() {
            said.insert(name.into(), value.clone().into());
        }
    }
    serde_json::Value::Object(said)
}

/// One instruction as a tool answers it.
pub fn instruction_json(records: &AccessRecords, held: &TradingInstruction) -> serde_json::Value {
    let target = held.target.clone().unwrap_or_default();
    let set_by = held
        .entry
        .as_ref()
        .and_then(|entry| entry.actor.as_ref())
        .map(|actor| crate::mcp::others_words(&actor_named(records, actor)))
        .unwrap_or_default();
    serde_json::json!({
        "target": target_json(&target),
        "target_said": target_said(records, &target),
        "value": value_word(held.value),
        "setter_level": level_word(held.setter_level),
        "reason": crate::mcp::others_words(&held.reason),
        "set_by": set_by,
        "entry_id": held.entry.as_ref().map(|entry| entry.entry_id.clone()).unwrap_or_default(),
        "last_change": held.last_change.as_ref().map(|change| serde_json::json!({
            "partition": change.partition,
            "sequence": change.sequence,
        })),
    })
}

fn actor_named(records: &AccessRecords, actor: &meridian_domain::v1::Actor) -> String {
    use meridian_domain::v1::actor::Kind;
    match actor.kind.as_ref() {
        Some(Kind::Person(person)) => crate::tickets::display_name(records, &person.subject),
        Some(Kind::ServiceAccount(account)) => format!("service account {}", account.subject),
        Some(Kind::System(system)) => format!("instance {}", system.instance_id),
        Some(Kind::Venue(venue)) => format!("venue through {}", venue.instance_id),
        None => String::new(),
    }
}

/// Every instruction in force, read as the dashboard, whatever any scope.
pub async fn read(bus: &meridian_bus::Bus) -> Result<Vec<TradingInstruction>, String> {
    let mut found = Vec::new();
    let mut cursor = String::new();
    loop {
        let (_, bytes) = bus
            .call(
                LIST_TRADING_INSTRUCTIONS,
                "meridian.v1.ListTradingInstructionsRequest",
                ListTradingInstructionsRequest {
                    page_size: 500,
                    cursor: cursor.clone(),
                    ..Default::default()
                }
                .encode_to_vec(),
                None,
                Some(WAIT),
            )
            .await
            .map_err(|failed| failed.to_string())?;
        let page = ListTradingInstructionsReply::decode(&bytes[..])
            .map_err(|failed| format!("the book's answer did not read: {failed}"))?;
        found.extend(page.instructions);
        if page.next_cursor.is_empty() {
            return Ok(found);
        }
        cursor = page.next_cursor;
    }
}

/// The request a setter's form or a tool's arguments make, the level
/// stamped from the setter's access.
pub fn request(
    access: &Access,
    target: TradingInstructionTarget,
    value: TradingInstructionValue,
    reason: &str,
) -> Result<SetTradingInstructionRequest, String> {
    let level = level_for(access, &target)?;
    Ok(SetTradingInstructionRequest {
        target: Some(target),
        value: value as i32,
        reason: reason.to_string(),
        setter_level: level as i32,
    })
}

/// The tool's arguments for a request: what a dialog prepares.
pub fn arguments(request: &SetTradingInstructionRequest) -> serde_json::Value {
    serde_json::json!({
        "target": target_json(&request.target.clone().unwrap_or_default()),
        "value": value_word(request.value),
        "reason": request.reason,
    })
}

/// The page whole: the instructions the viewer may see, one line each, and
/// the dialog that sets one.
pub fn page(
    instructions: &Result<Vec<TradingInstruction>, String>,
    records: &AccessRecords,
    access: &Access,
    token: &str,
    notice: &str,
) -> String {
    let rows: String = match instructions {
        Err(_) => String::new(),
        Ok(held) => held
            .iter()
            .filter(|instruction| visible(access, instruction))
            .map(|instruction| {
                let target = instruction.target.clone().unwrap_or_default();
                let said = target_said(records, &target);
                let (kind, id) = form_of(&target);
                let fill = serde_json::json!({ "fields": {
                    "kind": kind,
                    "id": id,
                    "instance": target.instance_id,
                    "value": value_word(instruction.value),
                    "reason": "",
                } });
                let set_by = instruction
                    .entry
                    .as_ref()
                    .and_then(|entry| entry.actor.as_ref())
                    .map(|actor| actor_named(records, actor))
                    .unwrap_or_default();
                let mine = level_for(access, &target)
                    .is_ok_and(|level| level as i32 == instruction.setter_level);
                format!(
                    "<tr data-id=\"{key}\" data-name=\"{said}\" data-value=\"{value_word}\">\
                     <td title=\"{said}\">{said}</td><td><span class=\"pill\">{value}</span></td>\
                     <td>{level}</td><td class=\"wide\" title=\"{reason}\">{reason}</td>\
                     <td class=\"wide\" title=\"{set_by}\">{set_by}</td><td class=\"actions\">{change}</td></tr>",
                    key = escape(&format!(
                        "{}|{}",
                        level_word(instruction.setter_level),
                        said
                    )),
                    said = escape(&said),
                    value_word = value_word(instruction.value),
                    value = value_said(instruction.value),
                    level = level_said(instruction.setter_level),
                    reason = escape(&instruction.reason),
                    set_by = escape(&set_by),
                    // Lifted only at the level that set it: Change where the
                    // viewer sets at that level.
                    change = if mine {
                        format!(
                            "<button type=\"button\" data-dialog-open=\"instruction\" \
                             data-title=\"Change the instruction on {}\" data-fill=\"{}\">Change</button>",
                            escape(&said),
                            escape(&fill.to_string())
                        )
                    } else {
                        String::new()
                    },
                )
            })
            .collect(),
    };
    let body = match instructions {
        Err(why) => format!(
            "<p class=\"refused\">The book of record is not answering: {}</p>",
            escape(why)
        ),
        Ok(_) if rows.is_empty() => {
            "<p class=\"empty\" data-instructions=\"0\">No instruction is set: \
             every plugin trades as usual.</p>"
                .to_string()
        }
        Ok(_) => super::overview::listing(
            "instructions-table",
            " one-line",
            "instructions",
            "Search by target, instruction or who set it",
            "<th>On</th><th>Instruction</th><th>Level</th><th class=\"wide\">Why</th>\
             <th class=\"wide\">Set by</th><th class=\"actions\"></th>",
            &rows,
        ),
    };
    // The plugins the setter may name: every known one for a deployment
    // admin, those they administer for a plugin's admin.
    let plugins: String = records
        .known_plugins
        .iter()
        .filter(|known| {
            access.deployment_admin
                || access
                    .plugin(&known.plugin_instance_id)
                    .administers_any_of(&[])
        })
        .map(|known| {
            format!(
                "<option value=\"{id}\">{id}</option>",
                id = escape(&known.plugin_instance_id)
            )
        })
        .collect();
    let none = if access.deployment_admin {
        "<option value=\"\">No plugin</option>"
    } else {
        ""
    };
    let kinds: String = [
        ("all", "All accounts", true),
        ("plugin", "A plugin", false),
        ("account", "An account", false),
        ("person", "A person", false),
        ("service_account", "A service account", false),
    ]
    .iter()
    .filter(|(_, _, admin_only)| access.deployment_admin || !admin_only)
    .map(|(value, label, _)| format!("<option value=\"{value}\">{label}</option>"))
    .collect();
    let values: String = VALUES
        .iter()
        .map(|(_, word, label)| format!("<option value=\"{word}\">{label}</option>"))
        .collect();
    let dialog = format!(
        "<dialog id=\"instruction\" aria-labelledby=\"instruction-title\">\
         <form method=\"post\" action=\"{PATH}\">{token}\
         <div class=\"dialog-head\"><h2 id=\"instruction-title\" data-title-new=\"Set an instruction\">Set an instruction</h2></div>\
         <div class=\"dialog-body\">\
         <label>On <select name=\"kind\">{kinds}</select></label>\
         <label>Plugin <select name=\"instance\">{none}{plugins}</select></label>\
         <label>Account, person or service account <input name=\"id\" maxlength=\"200\" \
         placeholder=\"Its ID, or a person's login\"></label>\
         <label>Instruction <select name=\"value\">{values}</select></label>\
         <label>Why <input name=\"reason\" maxlength=\"2000\" required placeholder=\"Kept with the instruction\"></label>\
         <p class=\"hint\">The most restrictive in force governs: stop over pause over active. \
         An instruction is lifted only at the level that set it. The book refuses and cancels \
         nothing for it: each plugin honours it.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" name=\"prepare\" value=\"1\" data-prepare>Prepare</button>\
         <button type=\"submit\" class=\"primary\" data-label-new=\"Set\">Set</button></div></form></dialog>"
    );
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"passed\">{}</p>", escape(notice))
    };
    let add = "<button type=\"button\" class=\"primary\" data-dialog-open=\"instruction\" \
               aria-label=\"Set an instruction\">+ Set</button>";
    format!(
        "<div class=\"admin\"><div class=\"page-head\"><h1>Trading instructions</h1></div>{notice}\
         <section class=\"admin-section\" id=\"instructions\">\
         <div class=\"section-head\"><div><h2>In force</h2><p class=\"hint\">Active, pause or stop, \
         on an account, every account, a plugin, a person or a service account. Each plugin \
         honours them; pause means no new orders, stop no new orders and its working orders \
         cancelled.</p></div>{add}</div>{body}</section>{dialog}</div>\
         <script>{}{}{}</script>",
        super::overview::SCRIPT_START,
        super::overview::SCRIPT_END,
        super::data_sources::PAGER_SCRIPT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{EntryMeta, KnownPluginRoles};

    fn admin() -> Access {
        Access {
            deployment_admin: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_target_is_read_from_the_form_and_its_level_from_the_setter() {
        let all = target_from("all", "", "").unwrap();
        assert!(all.all_accounts);
        assert!(target_from("all", "", "snaptrade-1").is_err());
        assert!(target_from("plugin", "", "").is_err());
        let person = target_from("person", "local|ada", "snaptrade-1").unwrap();
        assert_eq!(
            (person.subject.as_str(), person.instance_id.as_str()),
            ("local|ada", "snaptrade-1")
        );
        assert!(target_from("user_group", "UG-1", "").is_err());
        assert_eq!(
            level_for(&admin(), &all).unwrap(),
            SetterLevel::DeploymentAdmin
        );
        // Nobody else sets All accounts, nor a plugin they do not administer.
        let nobody = Access::default();
        assert!(level_for(&nobody, &all).is_err());
        assert!(level_for(&nobody, &person).is_err());
        let request = request(&admin(), all, TradingInstructionValue::Stop, "halt").unwrap();
        assert_eq!(request.setter_level, SetterLevel::DeploymentAdmin as i32);
        assert_eq!(arguments(&request)["target"]["all_accounts"], true);
        assert_eq!(arguments(&request)["value"], "stop");
    }

    #[test]
    fn the_page_is_one_line_an_instruction_and_says_none_when_none() {
        let records = AccessRecords {
            known_plugins: vec![KnownPluginRoles {
                plugin_instance_id: "snaptrade-1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let empty = page(&Ok(Vec::new()), &records, &admin(), "<t>", "");
        assert!(empty.contains("No instruction is set"));
        assert!(empty.contains("<option value=\"snaptrade-1\">snaptrade-1</option>"));
        assert!(empty.contains("name=\"prepare\""));
        let held = vec![TradingInstruction {
            target: Some(TradingInstructionTarget {
                all_accounts: true,
                ..Default::default()
            }),
            value: TradingInstructionValue::Stop as i32,
            setter_level: SetterLevel::DeploymentAdmin as i32,
            reason: "halt <now>".into(),
            entry: Some(EntryMeta::default()),
            ..Default::default()
        }];
        let drawn = page(&Ok(held.clone()), &records, &admin(), "<t>", "");
        assert!(drawn.contains("data-value=\"stop\""), "{drawn}");
        assert!(drawn.contains("halt &lt;now&gt;"));
        assert!(drawn.contains(">Change</button>"));
        // A plugin's admin sees what names their plugins alone.
        let theirs = page(&Ok(held), &records, &Access::default(), "<t>", "");
        assert!(theirs.contains("No instruction is set"));
        assert!(!theirs.contains("All accounts</option>"));
    }
}
