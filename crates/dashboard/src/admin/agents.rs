//! What agents may do with each act, on two pages (W6.29, contract v20):
//! the deployment's Settings, its **Agents** tab -- the ceiling on each
//! plugin act, core's acts' levels and gates, each also per access group,
//! and the lapse period -- and each plugin's Access tab, widened: one line
//! per act, its role, who may make it by hand (v15's access, read), what
//! their agents may and whether a second pair of eyes is on, set there by
//! an admin of the act's role(s), within the ceiling, shown capped where it
//! caps.
//!
//! And the plugin's **Accounts** tab (W6.30): the accounts a deployment
//! admin granted the instance to link to, granted and withdrawn there,
//! nothing by default.
//!
//! Every page here is one line a row, behind a pager, a dialog per change;
//! nothing is set by a tool (ruled 2026-10-10).

use std::collections::BTreeSet;

use meridian_domain::v1::AccessRecords;

use crate::html::escape;
use crate::prepared::levels::{self, Level, Levels};

/// One act on the pages: its name on the surface, whose it is, its title,
/// the roles it serves, and whether it can be prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActLine {
    pub act: String,
    /// The plugin instance whose act it is; empty for core's.
    pub instance: String,
    pub title: String,
    pub roles: Vec<String>,
    pub preparable: bool,
}

/// Every act the pages set: core's thirteen and the three on a prepared
/// change, then each running plugin's acts.
pub fn acts(app: &crate::web::App) -> Vec<ActLine> {
    let mut lines: Vec<ActLine> = crate::prepared::CORE_PREPARABLE
        .iter()
        .chain(levels::ON_A_PREPARED_CHANGE.iter())
        .filter_map(|name| {
            crate::mcp::core_spec(name).map(|spec| ActLine {
                act: name.to_string(),
                instance: String::new(),
                title: spec.title.to_string(),
                roles: Vec::new(),
                preparable: crate::prepared::CORE_PREPARABLE.contains(name),
            })
        })
        .collect();
    for (instance, report) in app.health.view() {
        for tool in report.declared_tools.iter().filter(|tool| !tool.reads) {
            lines.push(ActLine {
                act: format!("{instance}__{}", tool.name),
                instance: instance.clone(),
                title: tool.title.clone(),
                roles: tool.roles.clone(),
                preparable: !tool.checked_by.is_empty(),
            });
        }
    }
    lines
}

fn level_options(selected: Level, preparable: bool) -> String {
    [Level::MayMake, Level::PrepareOnly, Level::MayNot]
        .into_iter()
        .filter(|level| preparable || *level != Level::PrepareOnly)
        .map(|level| {
            format!(
                "<option value=\"{}\"{}>{}</option>",
                level.value(),
                if level == selected { " selected" } else { "" },
                level.words()
            )
        })
        .collect()
}

fn group_options(records: &AccessRecords) -> String {
    records
        .access_groups
        .iter()
        .filter(|group| !group.built_in)
        .map(|group| {
            format!(
                "<option value=\"group:{}\">Access group {}</option>",
                escape(&group.access_group_id),
                escape(&group.name)
            )
        })
        .collect()
}

/// The stricter levels set for access groups on an act, in words.
fn groups_said(levels: &Levels, act: &str, records: &AccessRecords) -> String {
    let said: Vec<String> = levels
        .rows
        .iter()
        .filter(|row| row.act == act && row.scope.starts_with(levels::GROUP))
        .map(|row| {
            let id = &row.scope[levels::GROUP.len()..];
            let name = records
                .access_groups
                .iter()
                .find(|g| g.access_group_id == id)
                .map(|g| g.name.clone())
                .unwrap_or_else(|| id.to_string());
            format!(
                "{name}: {}{}",
                Level::from_code(row.level).words(),
                if row.second_pair_of_eyes {
                    ", eyes"
                } else {
                    ""
                }
            )
        })
        .collect();
    if said.is_empty() {
        "--".into()
    } else {
        said.join("; ")
    }
}

/// The dialog every row's Edit opens, posting to `action`, its scopes those
/// `scopes` offers.
fn dialog(id: &str, action: &str, scopes: &str, records: &AccessRecords, token: &str) -> String {
    format!(
        "<dialog id=\"{id}\" aria-labelledby=\"{id}-title\"><form method=\"post\" action=\"{action}\">{token}\
         <input type=\"hidden\" name=\"act\" value=\"\">\
         <div class=\"dialog-head\"><h2 id=\"{id}-title\" data-title-new=\"What agents may do\">What agents may do</h2></div>\
         <div class=\"dialog-body\">\
         <label>Where <select name=\"scope\">{scopes}{groups}</select></label>\
         <label>Agents <select name=\"level\">{options}</select></label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"second_pair_of_eyes\" value=\"1\"> \
         A second pair of eyes: nobody makes it alone; it is prepared, and someone other than \
         whoever prepared it makes it</label>\
         <label>Why <input name=\"note\" maxlength=\"2000\"></label>\
         <p class=\"hint\">May make: an agent makes it or prepares it. Prepare only: an agent \
         prepares it and a person makes it. May not: no agent reaches it. An act with no check \
         takes only may make or may not, and no second pair of eyes.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\">Save</button></div></form></dialog>",
        groups = group_options(records),
        options = level_options(Level::MayMake, true),
    )
}

/// The Agents tab of the deployment's Settings: its body and its dialogs.
pub fn deployment_section(
    acts: &[ActLine],
    levels: &Levels,
    lapse_days: u32,
    records: &AccessRecords,
    token: &str,
) -> (String, String) {
    let rows: String = acts
        .iter()
        .map(|line| {
            let core = line.instance.is_empty();
            let scope = if core { levels::ACT } else { levels::CEILING };
            let (level, eyes) = if core {
                levels.own(&line.act)
            } else {
                levels
                    .ceiling(&line.act)
                    .unwrap_or((Level::MayMake, false))
            };
            let fill = serde_json::json!({
                "fields": {"act": line.act, "scope": scope, "level": level.value()},
                "checked": if eyes { vec!["second_pair_of_eyes"] } else { vec![] },
            });
            format!(
                "<tr data-id=\"{act}\" data-name=\"{title}\"><td title=\"{act}\"><span class=\"name\">{title}</span></td>\
                 <td class=\"wide\">{whose}</td><td data-level=\"{value}\">{words}</td><td>{eyes_said}</td>\
                 <td class=\"wide\">{groups}</td><td class=\"actions\"><button type=\"button\" data-dialog-open=\"agents-act\" \
                 data-title=\"{title}\" data-fill=\"{fill}\">Edit</button></td></tr>",
                act = escape(&line.act),
                title = escape(&line.title),
                whose = if core {
                    "Core".to_string()
                } else {
                    format!("{} (ceiling)", escape(&line.instance))
                },
                value = level.value(),
                words = if line.preparable || level != Level::PrepareOnly {
                    level.words()
                } else {
                    Level::MayNot.words()
                },
                eyes_said = if eyes { "On" } else { "Off" },
                groups = escape(&groups_said(levels, &line.act, records)),
                fill = escape(&fill.to_string()),
            )
        })
        .collect();
    let table = super::overview::listing(
        "agents-table",
        " one-line agents",
        "acts",
        "Search by act or plugin",
        "<th>Act</th><th class=\"wide\">Whose</th><th>Agents</th><th>Second pair of eyes</th>\
         <th class=\"wide\">Access groups</th><th class=\"actions\"></th>",
        &rows,
    );
    let lapse = format!(
        "<form method=\"post\" action=\"/admin/agents/lapse#agents\" class=\"inline lapse\">{token}\
         <label>A prepared change lapses after <input name=\"days\" type=\"number\" min=\"1\" max=\"{most}\" \
         value=\"{lapse_days}\" required inputmode=\"numeric\" aria-label=\"Days before a prepared change lapses\"> days</label>\
         <input name=\"note\" maxlength=\"2000\" placeholder=\"Why\" aria-label=\"Why\">\
         <button type=\"submit\">Save</button></form>",
        most = levels::LAPSE_MOST,
    );
    let scopes = "<option value=\"act\">The act (core's)</option>\
                  <option value=\"ceiling\">The ceiling (a plugin's act)</option>";
    (
        format!("{lapse}{table}"),
        dialog("agents-act", "/admin/agents#agents", scopes, records, token),
    )
}

/// One plugin's acts on its Access tab: by hand (who holds its role) and by
/// agent (the level, the ceiling where it caps, the gate), set there by an
/// admin of the act's role(s).
pub fn plugin_section(
    instance: &str,
    acts: &[ActLine],
    levels: &Levels,
    records: &AccessRecords,
    may_set: &dyn Fn(&ActLine) -> bool,
    token: &str,
) -> String {
    let mine: Vec<&ActLine> = acts
        .iter()
        .filter(|line| line.instance == instance)
        .collect();
    if mine.is_empty() {
        return "<h2>What agents may do</h2><p class=\"hint\">This plugin offers no act to agents.</p>"
            .to_string();
    }
    let rows: String = mine
        .iter()
        .map(|line| {
            let (chosen, standing, capped) = levels.capped(&line.act);
            let (_, eyes) = levels.own(&line.act);
            let ceiling_eyes = levels.ceiling(&line.act).is_some_and(|(_, eyes)| eyes);
            let by_hand = by_hand(records, instance, &line.roles);
            let fill = serde_json::json!({
                "fields": {"act": line.act, "scope": "act", "level": chosen.value()},
                "checked": if eyes { vec!["second_pair_of_eyes"] } else { vec![] },
            });
            let edit = if may_set(line) {
                format!(
                    "<button type=\"button\" data-dialog-open=\"plugin-agents\" data-title=\"{}\" data-fill=\"{}\">Edit</button>",
                    escape(&line.title),
                    escape(&fill.to_string())
                )
            } else {
                String::new()
            };
            format!(
                "<tr data-id=\"{act}\" data-name=\"{title}\"><td title=\"{act}\"><span class=\"name\">{title}</span></td>\
                 <td class=\"wide\">{roles}</td><td class=\"wide\" title=\"{hand}\">{hand}</td>\
                 <td data-level=\"{value}\">{words}</td><td>{eyes_said}</td><td class=\"actions\">{edit}</td></tr>",
                act = escape(&line.act),
                title = escape(&line.title),
                roles = escape(&line.roles.join(", ")),
                hand = escape(&by_hand),
                value = standing.value(),
                words = if capped {
                    format!("{} (capped at the ceiling; chosen {})", standing.words(), chosen.words())
                } else if !line.preparable && standing == Level::PrepareOnly {
                    Level::MayNot.words().to_string()
                } else {
                    standing.words().to_string()
                },
                eyes_said = if eyes || ceiling_eyes {
                    if ceiling_eyes { "On (ceiling)" } else { "On" }
                } else {
                    "Off"
                },
            )
        })
        .collect();
    let table = super::overview::listing(
        "plugin-agents-table",
        " one-line agents",
        "acts",
        "Search by act",
        "<th>Act</th><th class=\"wide\">Role</th><th class=\"wide\">By hand</th><th>Agents</th>\
         <th>Second pair of eyes</th><th class=\"actions\"></th>",
        &rows,
    );
    let action = format!("/admin/plugins/{}/agents", escape(instance));
    format!(
        "<h2>What agents may do</h2><p class=\"hint\">By hand is who holds the act's role here; \
         agents is what their agents may do with it, within the deployment's ceiling.</p>{table}{}",
        dialog(
            "plugin-agents",
            &action,
            "<option value=\"act\">This plugin's act</option>",
            records,
            token,
        )
    )
}

/// Who may make an act by hand: the access groups granting a role it
/// serves on the instance (v15, read).
fn by_hand(records: &AccessRecords, instance: &str, roles: &[String]) -> String {
    let named: BTreeSet<String> = records
        .access_groups
        .iter()
        .filter(|group| {
            group.entries.iter().any(|entry| {
                entry.plugin_instance_id == instance
                    && (roles.is_empty() || roles.contains(&entry.role))
            }) || group.access_group_id == meridian_access::ALL_PLUGINS_ADMIN
        })
        .map(|group| group.name.clone())
        .collect();
    if named.is_empty() {
        "nobody".into()
    } else {
        named.into_iter().collect::<Vec<_>>().join(", ")
    }
}

/// The plugin's Accounts tab (W6.30): the accounts granted, one line each,
/// a deployment admin's to grant and withdraw; read by its admins.
pub fn accounts_section(
    instance: &str,
    records: &AccessRecords,
    may_grant: bool,
    token: &str,
) -> String {
    let granted = records
        .linkable_accounts
        .iter()
        .find(|linkable| linkable.plugin_instance_id == instance);
    let ids: Vec<String> = granted.map(|g| g.account_ids.clone()).unwrap_or_default();
    let name_of = |id: &str| {
        records
            .accounts
            .iter()
            .find(|a| a.account_id == id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let linked = |id: &str| {
        records
            .links
            .iter()
            .any(|link| link.plugin_instance_id == instance && link.account_id == id)
    };
    let action = format!("/admin/plugins/{}/accounts", escape(instance));
    let rows: String = ids
        .iter()
        .map(|id| {
            let withdraw = if may_grant {
                format!(
                    "<form method=\"post\" action=\"{action}\" class=\"inline\" \
                     data-confirm=\"Withdraw this account? Its link, if any, is kept.\">{token}\
                     <input type=\"hidden\" name=\"withdraw\" value=\"{id}\"><button type=\"submit\">Withdraw</button></form>",
                    id = escape(id)
                )
            } else {
                String::new()
            };
            format!(
                "<tr data-id=\"{id}\" data-name=\"{name}\"><td><span class=\"name\">{name}</span></td>\
                 <td class=\"wide\"><code>{id}</code></td><td>{linked}</td><td class=\"actions\">{withdraw}</td></tr>",
                id = escape(id),
                name = escape(&name_of(id)),
                linked = if linked(id) { "Linked" } else { "Not linked" },
            )
        })
        .collect();
    let body = if ids.is_empty() {
        "<p class=\"empty\" data-linkable=\"0\">No account is granted: this plugin's admins \
         link its external accounts to none until a deployment admin grants some here.</p>"
            .to_string()
    } else {
        super::overview::listing(
            "linkable-table",
            " one-line linkable",
            "accounts",
            "Search by account",
            "<th>Account</th><th class=\"wide\">ID</th><th>Link</th><th class=\"actions\"></th>",
            &rows,
        )
    };
    let by = granted
        .filter(|g| g.updated_at_ns > 0)
        .map(|g| {
            format!(
                "<p class=\"hint\">Last changed {} by {}.</p>",
                escape(&crate::custody::utc(g.updated_at_ns)),
                escape(&if g.updated_by.is_empty() {
                    "the upgrade to contract v20".to_string()
                } else {
                    crate::tickets::display_name(records, &g.updated_by)
                })
            )
        })
        .unwrap_or_default();
    let grant = if may_grant {
        let options: String = records
            .accounts
            .iter()
            .filter(|a| !ids.contains(&a.account_id) && a.state != 2)
            .map(|a| {
                format!(
                    "<option value=\"{}\">{}</option>",
                    escape(&a.account_id),
                    escape(&a.name)
                )
            })
            .collect();
        format!(
            "<form method=\"post\" action=\"{action}\" class=\"inline grant\">{token}\
             <label>Grant <select name=\"grant\" required><option value=\"\">An account</option>{options}</select></label>\
             <input name=\"note\" maxlength=\"4000\" placeholder=\"Why\" aria-label=\"Why\">\
             <button type=\"submit\" class=\"primary\">Grant</button></form>"
        )
    } else {
        String::new()
    };
    format!(
        "<section class=\"panel padded\" id=\"accounts\"><h2>Accounts it may link to</h2>\
         <p class=\"hint\">A deployment admin grants the accounts this plugin's admins may link \
         its external accounts to. A grant lets them link; it gives nobody anything to read or \
         write.</p>{grant}{body}{by}</section>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prepared::store::LevelRow;

    fn line(act: &str, instance: &str, preparable: bool) -> ActLine {
        ActLine {
            act: act.into(),
            instance: instance.into(),
            title: act.into(),
            roles: vec!["operations".into()],
            preparable,
        }
    }

    #[test]
    fn the_agents_tab_is_one_line_an_act_and_a_capped_level_says_so() {
        let mut levels = Levels::default();
        levels.rows.push(LevelRow {
            act: "ops-1__confirm".into(),
            scope: levels::CEILING.into(),
            level: Level::PrepareOnly as i16,
            second_pair_of_eyes: true,
        });
        let acts = vec![
            line("dashboard__set_hold", "", true),
            line("dashboard__make_prepared", "", false),
            line("ops-1__confirm", "ops-1", true),
        ];
        let records = AccessRecords::default();
        let (body, dialog) = deployment_section(&acts, &levels, 30, &records, "<t>");
        assert!(body.contains("one-line agents"));
        assert!(body.contains("value=\"30\""));
        assert!(
            body.contains("data-level=\"may_not\">May not"),
            "make_prepared starts at may not: {body}"
        );
        assert!(
            body.contains(
                "ops-1 (ceiling)</td><td data-level=\"prepare_only\">Prepare only</td><td>On</td>"
            ),
            "{body}"
        );
        assert!(dialog.contains("name=\"second_pair_of_eyes\""));

        let section = plugin_section("ops-1", &acts, &levels, &records, &|_| true, "<t>");
        assert!(
            section.contains("Prepare only (capped at the ceiling; chosen May make)"),
            "{section}"
        );
        assert!(section.contains("On (ceiling)"));
    }

    #[test]
    fn the_accounts_tab_says_none_is_granted_by_default() {
        let records = AccessRecords::default();
        let said = accounts_section("custody-1", &records, true, "<t>");
        assert!(said.contains("No account is granted"));
        assert!(said.contains("name=\"grant\""));
        let read = accounts_section("custody-1", &records, false, "<t>");
        assert!(!read.contains("name=\"grant\""));
    }
}
