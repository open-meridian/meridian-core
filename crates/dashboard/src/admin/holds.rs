//! The holds on raw records, on the deployment's Settings (W6.25, contract
//! v16; spec/an-edge-plugins-older-records-move-to-the-archive, requirement
//! 8): a deployment admin's, since a firm's recordkeeping rule -- Rule 17a-4
//! among them -- is the deployment's to keep, not a plugin's.
//!
//! One line a hold: the edge role it covers, or every one; the least days a
//! record is kept anywhere, storage or archive; whether it needs records that
//! cannot be altered; who set it, and when, in full -- a phone keeps what it
//! covers, the days and the day it was set, leaves who, write-once and the
//! time to the cells' titles, and opens the dialog from what it covers, where
//! 0 days clears it. Set or changed in a dialog, and
//! cleared from its row, each a command to the conductor, which records it
//! as its own record, refuses a write-once hold where the deployment's
//! archive cannot lock, and tells each sidecar it covers.

use meridian_domain::v1::AccessRecords;
use meridian_domain::{thousands, EDGE_ROLES};

use crate::html::escape;

fn role_said(role: &str) -> String {
    if role.is_empty() {
        "Every edge role".into()
    } else {
        role.to_string()
    }
}

/// The tab's body and its dialog.
pub fn section(records: &AccessRecords, token: &str) -> (String, String) {
    let rows: String = records
        .holds
        .iter()
        .map(|hold| {
            let fill = serde_json::json!({
                "fields": {"role": hold.role, "days": hold.days.to_string()},
                "checked": if hold.write_once { vec!["write_once"] } else { vec![] },
            });
            let at = crate::custody::utc(hold.updated_at_ns);
            let (day, time) = at.split_once(' ').unwrap_or((at.as_str(), ""));
            format!(
                "<tr data-id=\"{role}\" data-name=\"{said}\"><td title=\"{said}\">\
                 <button type=\"button\" class=\"link\" data-dialog-open=\"hold\" data-title=\"Change the hold\" \
                 data-fill=\"{fill}\">{said}</button></td><td>{days} days</td>\
                 <td class=\"wide\">{once}</td><td class=\"wide\" title=\"{by}\">{by}</td>\
                 <td title=\"{when} by {by}\" data-set>{day}<span class=\"dates\"> {time}</span></td>\
                 <td class=\"actions\">\
                 <button type=\"button\" data-dialog-open=\"hold\" data-title=\"Change the hold\" \
                 data-fill=\"{fill}\">Edit</button>\
                 <form method=\"post\" action=\"/admin/holds#holds\" class=\"inline wide\" \
                 data-confirm=\"Clear this hold? A window may then be set shorter, and deletion comes sooner.\">{token}\
                 <input type=\"hidden\" name=\"role\" value=\"{role}\"><input type=\"hidden\" name=\"days\" value=\"0\">\
                 <button type=\"submit\">Clear</button></form></td></tr>",
                role = escape(&hold.role),
                said = escape(&role_said(&hold.role)),
                days = thousands(u64::from(hold.days)),
                once = if hold.write_once { "write-once" } else { "no" },
                by = escape(&crate::tickets::display_name(records, &hold.updated_by)),
                when = escape(&at),
                day = escape(day),
                time = escape(time),
                fill = escape(&fill.to_string()),
            )
        })
        .collect();
    let body = if rows.is_empty() {
        "<p class=\"empty\" data-holds=\"0\">No hold is set: a plugin's admin sets each kind's \
         window as they choose, and nothing stops a deletion past it.</p>"
            .to_string()
    } else {
        format!(
            "<div class=\"scroll\"><table class=\"list one-line holds\" data-holds=\"{n}\"><thead><tr>\
             <th>Covers</th><th>Kept<span class=\"dates\"> at least</span></th><th class=\"wide\">Write-once</th>\
             <th class=\"wide\">Set by</th><th>Set</th><th class=\"actions\"></th></tr></thead>\
             <tbody>{rows}</tbody></table></div>",
            n = records.holds.len()
        )
    };
    let roles: String = std::iter::once(("", "Every edge role".to_string()))
        .chain(EDGE_ROLES.iter().map(|role| (*role, role.to_string())))
        .map(|(value, label)| format!("<option value=\"{value}\">{}</option>", escape(&label)))
        .collect();
    let dialog = format!(
        "<dialog id=\"hold\" aria-labelledby=\"hold-title\"><form method=\"post\" action=\"/admin/holds#holds\">{token}\
         <div class=\"dialog-head\"><h2 id=\"hold-title\" data-title-new=\"Set a hold\">Set a hold</h2></div>\
         <div class=\"dialog-body\">\
         <label>Covers <select name=\"role\">{roles}</select></label>\
         <label>Kept at least, in days <input name=\"days\" type=\"number\" min=\"0\" max=\"36500\" \
         step=\"1\" inputmode=\"numeric\" required></label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"write_once\" value=\"1\"> Records \
         that cannot be altered (object lock, for the hold's length)</label>\
         <p class=\"hint\">The longest hold over a plugin is what holds it. 0 days clears it.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\" data-label-new=\"Set\">Set</button></div></form></dialog>"
    );
    (body, dialog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::Hold;

    #[test]
    fn each_hold_is_one_line_and_none_says_so() {
        let mut records = AccessRecords::default();
        let (empty, dialog) = section(&records, "<t>");
        assert!(empty.contains("No hold is set"));
        assert!(dialog.contains("<option value=\"custody\">custody</option>"));
        assert!(dialog.contains("name=\"write_once\""));
        records.holds = vec![
            Hold {
                role: String::new(),
                days: 400,
                ..Default::default()
            },
            Hold {
                role: "custody".into(),
                days: 2190,
                write_once: true,
                updated_by: "ada@example.com".into(),
                updated_at_ns: 1_791_417_600_000_000_000,
                ..Default::default()
            },
        ];
        let (body, _) = section(&records, "<t>");
        assert!(body.contains("data-holds=\"2\""));
        assert!(
            body.contains(">Every edge role</button></td><td>400 days</td>"),
            "{body}"
        );
        assert!(
            body.contains(
                ">custody</button></td><td>2,190 days</td><td class=\"wide\">write-once</td>"
            ),
            "{body}"
        );
        // When it was set, in full: the day, then the time a phone leaves
        // to the cell's title, never cut off mid-time.
        assert!(
            body.contains(
                "<td title=\"2026-10-08 00:00 UTC by ada@example.com\" data-set>2026-10-08\
                 <span class=\"dates\"> 00:00 UTC</span></td>"
            ),
            "{body}"
        );
        assert!(
            body.contains("class=\"inline wide\""),
            "Clear leaves a phone's row: {body}"
        );
    }
}
