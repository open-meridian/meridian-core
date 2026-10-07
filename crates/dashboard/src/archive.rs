//! An edge plugin's raw records on its Summary, and its archive allowed on
//! its Manage page (W6.9, W8.7, contract v16;
//! spec/an-edge-plugins-older-records-move-to-the-archive, requirements 3
//! and 10).
//!
//! **The panel.** For each kind of raw record the version declares, one
//! line: its label, its window -- the plugin admin's setting, or the
//! version's default -- and whether the hold over the instance overrides it,
//! how many records its storage holds and from when to when, as the plugin's
//! heartbeat last said (W4.5), and how many the archive holds and from when
//! to when, summed by the conductor from the moves (ReadMoves), and the
//! bytes it uses of the archive, as the plugin's heartbeat said
//! (`StoredSpan.bytes`, named 2026-10-07), with the bytes of every kind
//! together in the table's foot. Then its moves, newest first, one line
//! each, paged by the kit's om-pager: when, what became of the unit, the
//! kind, the unit, its count and span, and the rule or the person. Where no
//! archive is allowed it says so, and that records past their window are
//! kept; where one was and is withdrawn, that, and what it still holds;
//! where one is, its bound and how much of it is used, or that it is full.
//! Nothing of a record's content is ever here: counts, times, sizes, the
//! plugin's own keys.
//!
//! **Allowing an archive** is a deployment admin's (W8.7: it grants
//! resources, possibly unbounded): a dialog on the panel with the bound, or
//! none, and Withdraw beside it, each a command to the conductor, which
//! records it and restarts the instance through CreatePlugin. A plugin's
//! admin who is not a deployment admin sees the panel and no button.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use meridian_bus::BusError;
use meridian_domain::v1::{
    AccessRecords, AllowArchiveRequest, PluginArchive, PluginReport, ReadMovesReply,
    ReadMovesRequest, WithdrawArchiveRequest,
};
use meridian_domain::{thousands, EDGE_ROLES};
use meridian_pb::v1::{MoveOutcome, RawRecordKind, StoredSpan};
use prost::Message;

use crate::admin::{form_token_matches, gate, Fields};
use crate::custody::utc;
use crate::html::escape;
use crate::web::App;

pub const READ_MOVES: &str = "platform.config.query.moves";
pub const ALLOW_ARCHIVE: &str = "platform.config.command.allow-archive";
pub const WITHDRAW_ARCHIVE: &str = "platform.config.command.withdraw-archive";

const GIB: u64 = 1024 * 1024 * 1024;

/// The kinds of raw record a report's declaration names; none from a plugin
/// declaring none.
pub fn kinds(report: Option<&PluginReport>) -> &[RawRecordKind] {
    report
        .and_then(|report| report.declaration.as_ref())
        .and_then(|declaration| declaration.storage.as_ref())
        .map(|storage| storage.record_kinds.as_slice())
        .unwrap_or_default()
}

/// Whether a plugin keeps raw records the panel is drawn for: one declaring
/// storage, at the edge.
pub fn keeps_records(report: Option<&PluginReport>) -> bool {
    report.is_some_and(|report| {
        report
            .declaration
            .as_ref()
            .is_some_and(|declaration| declaration.storage.is_some())
            && report
                .roles
                .iter()
                .any(|role| EDGE_ROLES.contains(&role.as_str()))
    })
}

/// The instance's moves and archive, as the conductor answers them, from
/// `cursor` (empty for the newest); or why they could not be read.
pub async fn read(app: &App, instance: &str, cursor: &str) -> Result<ReadMovesReply, String> {
    let (_, bytes) = app
        .bus
        .call(
            READ_MOVES,
            "meridian.v1.ReadMovesRequest",
            ReadMovesRequest {
                plugin_instance_id: instance.to_string(),
                cursor: cursor.to_string(),
            }
            .encode_to_vec(),
            None,
            Some(Duration::from_secs(5)),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    ReadMovesReply::decode(&bytes[..]).map_err(|failed| failed.to_string())
}

/// The day a time falls on.
fn day(ns: i64) -> String {
    utc(ns).chars().take(10).collect()
}

/// "48,210, 2019-04-01 to 2026-09-26", or "none".
fn span_said(span: Option<&StoredSpan>) -> String {
    match span {
        Some(span) if span.record_count > 0 => {
            let first = day(span.first_received_ns);
            let last = day(span.last_received_ns);
            if first == last {
                format!("{}, {first}", thousands(span.record_count))
            } else {
                format!("{}, {first} to {last}", thousands(span.record_count))
            }
        }
        _ => "none".into(),
    }
}

/// A span in a cell: its count, then its dates, which a phone leaves to the
/// cell's title (the one-screen rule: fewer columns and words on one line,
/// the rest a tap away).
fn span_cell(span: Option<&StoredSpan>) -> String {
    let said = span_said(span);
    match said.split_once(", ") {
        Some((count, dates)) => format!(
            "{}<span class=\"dates\">, {}</span>",
            escape(count),
            escape(dates)
        ),
        None => escape(&said),
    }
}

/// A moment in a cell: its day, then its time, which a phone leaves to the
/// cell's title.
fn when_cell(at_ns: i64) -> String {
    let said = utc(at_ns);
    match said.split_once(' ') {
        Some((day, time)) => format!(
            "{}<span class=\"dates\"> {}</span>",
            escape(day),
            escape(time)
        ),
        None => escape(&said),
    }
}

/// A size as a person reads it: whole bytes below a KiB, else to a tenth of
/// the largest binary unit it reaches, rounded down so a size never reads
/// as more than it is; integers throughout.
pub fn bytes_said(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return if bytes == 1 {
            "1 byte".into()
        } else {
            format!("{} bytes", thousands(bytes))
        };
    }
    let mut unit = 1024u64;
    let mut named = 0;
    while named + 1 < UNITS.len() && bytes / unit >= 1024 {
        unit *= 1024;
        named += 1;
    }
    let tenths = u128::from(bytes) * 10 / u128::from(unit);
    let (whole, tenth) = (tenths / 10, tenths % 10);
    if tenth == 0 {
        format!("{} {}", thousands(whole as u64), UNITS[named])
    } else {
        format!("{}.{tenth} {}", thousands(whole as u64), UNITS[named])
    }
}

/// What a kind uses of the archive, in a cell: its size, or none.
fn used_said(bytes: u64) -> String {
    if bytes == 0 {
        "none".into()
    } else {
        bytes_said(bytes)
    }
}

/// How much of a bound is used, as the state line says it: "1.2 GiB of at
/// most 50 GiB used", or that it is full; with no bound, the size alone.
pub fn used_of(used: u64, most_bytes: u64) -> String {
    if most_bytes == 0 {
        format!("no bound, {} used", bytes_said(used))
    } else if used >= most_bytes {
        format!(
            "full: {} of at most {} used",
            bytes_said(used),
            bytes_said(most_bytes)
        )
    } else {
        format!(
            "{} of at most {} used",
            bytes_said(used),
            bytes_said(most_bytes)
        )
    }
}

/// A bound as a person reads it.
pub fn bound_said(most_bytes: u64) -> String {
    if most_bytes == 0 {
        "no bound".into()
    } else if most_bytes.is_multiple_of(GIB) {
        format!("at most {} GiB", thousands(most_bytes / GIB))
    } else {
        format!("at most {} bytes", thousands(most_bytes))
    }
}

fn outcome_said(outcome: i32) -> &'static str {
    match MoveOutcome::try_from(outcome) {
        Ok(MoveOutcome::Archived) => "Archived",
        Ok(MoveOutcome::Restored) => "Restored",
        Ok(MoveOutcome::Returned) => "Returned",
        Ok(MoveOutcome::Deleted) => "Deleted",
        _ => "Moved",
    }
}

/// The hold over an instance as the records carry the holds (W6.25): the
/// longest of its edge roles' and the one for every role.
pub fn hold_over(records: &AccessRecords, roles: &[String]) -> u32 {
    if !roles.iter().any(|role| EDGE_ROLES.contains(&role.as_str())) {
        return 0;
    }
    records
        .holds
        .iter()
        .filter(|hold| hold.role.is_empty() || roles.contains(&hold.role))
        .map(|hold| hold.days)
        .max()
        .unwrap_or(0)
}

/// What the panel is drawn from.
pub struct Panel<'a> {
    pub instance: &'a str,
    pub report: Option<&'a PluginReport>,
    pub records: &'a AccessRecords,
    pub moves: &'a Result<ReadMovesReply, String>,
    /// A deployment admin, who alone allows or withdraws its archive.
    pub deployment_admin: bool,
    pub token: &'a str,
    /// The page of moves asked for: empty for the newest.
    pub cursor: &'a str,
}

/// A kind's window as its admin set it, or the version's default.
fn window_of(records: &AccessRecords, instance: &str, kind: &RawRecordKind) -> (u32, bool) {
    let name = format!("{}_window_days", kind.name);
    records
        .plugin_settings
        .iter()
        .find(|record| record.plugin_instance_id == instance)
        .and_then(|record| record.values.iter().find(|value| value.name == name))
        .and_then(|value| value.value.trim().parse::<u32>().ok())
        .map_or((kind.window_days, false), |days| (days, true))
}

/// The panel -- each kind's line, and the archive's state with Allow and
/// Withdraw -- and its moves, apart, so the Summary draws each as a part of
/// its own; or nothing for a plugin keeping no raw records.
pub fn sections(panel: &Panel) -> (String, String) {
    if !keeps_records(panel.report) {
        return (String::new(), String::new());
    }
    let report = panel.report.expect("keeps_records holds one");
    let hold = hold_over(panel.records, &report.roles);
    let (archived, archive, moves, next, unread) = match panel.moves {
        Ok(reply) => (
            reply.archived.as_slice(),
            reply.archive.as_ref(),
            reply.moves.as_slice(),
            reply.next_cursor.as_str(),
            None,
        ),
        Err(why) => (&[][..], None, &[][..], "", Some(why.as_str())),
    };
    let allowed = archive.filter(|archive| archive.allowed);
    // What each kind uses of the archive, as the plugin last said.
    let used = |kind: &str| {
        report
            .stored
            .iter()
            .find(|s| s.record_kind == kind)
            .map_or(0, |s| s.bytes)
    };
    let used_in_all: u64 = report.stored.iter().map(|s| s.bytes).sum();
    // An archive withdrawn: one recorded as no longer allowed, or records
    // the archive still holds with none allowed now.
    let withdrawn = allowed.is_none()
        && (archive.is_some_and(|archive| !archive.allowed)
            || archived.iter().any(|span| span.record_count > 0)
            || used_in_all > 0);
    let state = match (allowed, unread) {
        (_, Some(_)) => "<span class=\"archive-state\" data-archive=\"unread\">The archive could not be read just now.</span>".to_string(),
        (Some(archive), None) => format!(
            "<span class=\"archive-state\" data-archive=\"{full}\" title=\"Allowed by {by}, {at}\">\
             Archive allowed: {used}.</span>",
            full = if archive.most_bytes > 0 && used_in_all >= archive.most_bytes {
                "full"
            } else {
                "allowed"
            },
            used = escape(&used_of(used_in_all, archive.most_bytes)),
            by = escape(&crate::tickets::display_name(panel.records, &archive.updated_by)),
            at = escape(&utc(archive.updated_at_ns)),
        ),
        (None, None) if withdrawn => format!(
            "<span class=\"archive-state\" data-archive=\"withdrawn\">Archive withdrawn: what it \
             holds is kept{held}, and records past their window are kept in storage.</span>",
            held = if used_in_all > 0 {
                format!(", {}", escape(&bytes_said(used_in_all)))
            } else {
                String::new()
            },
        ),
        (None, None) => "<span class=\"archive-state\" data-archive=\"none\">No archive allowed: \
             records past their window are kept.</span>"
            .to_string(),
    };
    let actions = if panel.deployment_admin {
        allow_controls(panel.instance, allowed, panel.token)
    } else {
        String::new()
    };
    let kinds = kinds(panel.report);
    let mut overridden = Vec::new();
    let lines: String = if kinds.is_empty() {
        format!(
            "<tr data-kind=\"\"><td>Its raw records</td><td class=\"wide\">{} days</td><td>as it keeps them</td><td data-archived>{}</td><td class=\"num\">none</td></tr>",
            report
                .declaration
                .as_ref()
                .and_then(|d| d.storage.as_ref())
                .map(|s| thousands(u64::from(s.retention_days)))
                .unwrap_or_default(),
            escape(&span_said(None)),
        )
    } else {
        kinds
            .iter()
            .map(|kind| {
                let (window, set) = window_of(panel.records, panel.instance, kind);
                let held = window < hold;
                if held {
                    overridden.push(format!("{}_window_days", kind.name));
                }
                let window = format!(
                    "{} days{}{}",
                    thousands(u64::from(window)),
                    if set { "" } else { " (default)" },
                    if held { ", held longer" } else { "" }
                );
                let stored = report.stored.iter().find(|s| s.record_kind == kind.name);
                let archived = archived.iter().find(|s| s.record_kind == kind.name);
                let bytes = used(&kind.name);
                format!(
                    "<tr data-kind=\"{name}\"><td title=\"{name}\">{label}</td>\
                     <td class=\"wide\" title=\"{window}\">{window}</td>\
                     <td data-stored title=\"{stored_said}\">{stored}</td>\
                     <td data-archived title=\"{archived_said}\">{archived}</td>\
                     <td class=\"num\" data-used=\"{bytes}\" title=\"{bytes_exact} bytes\">{used}</td></tr>",
                    name = escape(&kind.name),
                    label = escape(&kind.label),
                    window = escape(&window),
                    stored_said = escape(&span_said(stored)),
                    stored = span_cell(stored),
                    archived_said = escape(&span_said(archived)),
                    archived = span_cell(archived),
                    bytes_exact = thousands(bytes),
                    used = escape(&used_said(bytes)),
                )
            })
            .collect()
    };
    let held = if hold == 0 {
        String::new()
    } else if overridden.is_empty() {
        format!(
            "<p class=\"hint\" data-hold>A hold of {} days is over it: nothing younger is deleted.</p>",
            thousands(u64::from(hold))
        )
    } else {
        format!(
            "<p class=\"hint\" data-hold>A hold of {} days is over it, longer than {}: nothing \
             younger is deleted.</p>",
            thousands(u64::from(hold)),
            escape(&overridden.join(", "))
        )
    };
    // Every kind together, against the bound where there is one; nothing
    // to add up where no archive is allowed and none holds anything.
    let total = if allowed.is_none() && used_in_all == 0 {
        String::new()
    } else {
        let against = allowed
            .filter(|archive| archive.most_bytes > 0)
            .map(|archive| format!(" of {}", bytes_said(archive.most_bytes)))
            .unwrap_or_default();
        format!(
            "<tfoot><tr data-used-in-all=\"{used_in_all}\"><td>In all</td><td class=\"wide\"></td>\
             <td></td><td data-archived></td><td class=\"num\" title=\"{exact} bytes\">{said}{against}</td></tr></tfoot>",
            exact = thousands(used_in_all),
            said = escape(&used_said(used_in_all)),
            against = escape(&against),
        )
    };
    let rows: String = moves
        .iter()
        .map(|record| {
            let moved = record.r#move.clone().unwrap_or_default();
            let who = if record.person.is_empty() {
                moved.rule.clone()
            } else {
                crate::tickets::display_name(panel.records, &record.person)
            };
            format!(
                "<tr data-move=\"{outcome}\"><td title=\"{at}\">{when}</td><td>{outcome}</td>\
                 <td class=\"wide\">{kind}</td><td title=\"{unit}\">{unit}</td>\
                 <td class=\"wide\" title=\"{span}\">{span}</td><td class=\"wide\" title=\"{who}\">{who}</td></tr>",
                at = escape(&utc(record.at_ns)),
                when = when_cell(record.at_ns),
                outcome = outcome_said(moved.outcome),
                kind = escape(&moved.record_kind),
                unit = escape(&moved.unit),
                span = escape(&span_said(Some(&StoredSpan {
                    record_kind: moved.record_kind.clone(),
                    record_count: moved.record_count,
                    first_received_ns: moved.first_received_ns,
                    last_received_ns: moved.last_received_ns,
                    bytes: 0,
                }))),
                who = escape(&who),
            )
        })
        .collect();
    // The pages beyond the conductor's, on the moves' own head line.
    let older = if next.is_empty() {
        String::new()
    } else {
        format!(
            "<a class=\"older\" href=\"?level=admin&amp;tab=summary&amp;moves={}#part-moves\">Older moves</a>",
            escape(next)
        )
    };
    let newer = if panel.cursor.is_empty() {
        String::new()
    } else {
        "<a class=\"newer\" href=\"?level=admin&amp;tab=summary#part-moves\">Newest moves</a>"
            .into()
    };
    let listed = if rows.is_empty() {
        "<p class=\"empty\" data-moves=\"0\">Nothing has moved yet.</p>".to_string()
    } else {
        format!(
            "<om-pager><table class=\"list one-line moves\" data-moves=\"{n}\"><thead><tr><th>When</th>\
             <th>What</th><th class=\"wide\">Kind</th><th>Unit</th><th class=\"wide\">Records</th>\
             <th class=\"wide\">By</th></tr></thead>\
             <tbody>{rows}</tbody></table></om-pager>",
            n = moves.len(),
        )
    };
    (
        format!(
            "<section class=\"panel padded\" id=\"records\"><div class=\"row records-head\"><h2>Raw records</h2>\
             {state}{actions}</div>\
             <table class=\"list one-line kinds\"><thead><tr><th>Kind</th><th class=\"wide\">Window</th>\
             <th title=\"What its storage holds\">Stored</th><th data-archived title=\"What the archive holds\">Archived</th>\
             <th class=\"num\" title=\"The bytes it uses of the archive, which the bound is counted against\">Archive size</th></tr></thead>\
             <tbody>{lines}</tbody>{total}</table>{held}</section>"
        ),
        format!(
            "<section class=\"panel padded\" id=\"moves\"><div class=\"row moves-head\"><h2>Moves</h2>\
             {newer}{older}</div>{listed}</section>"
        ),
    )
}

/// Allow (or change) and Withdraw, for a deployment admin: a dialog holding
/// the bound.
fn allow_controls(instance: &str, allowed: Option<&PluginArchive>, token: &str) -> String {
    let instance = escape(instance);
    let bound = allowed
        .filter(|archive| archive.most_bytes > 0 && archive.most_bytes.is_multiple_of(GIB))
        .map(|archive| (archive.most_bytes / GIB).to_string())
        .unwrap_or_default();
    let (open, title, submit) = if allowed.is_some() {
        ("Change bound", "Change the archive's bound", "Change")
    } else {
        ("Allow archive", "Allow an archive", "Allow")
    };
    let withdraw = if allowed.is_some() {
        format!(
            "<form method=\"post\" action=\"/plugins/{instance}/archive/withdraw\" class=\"inline\">{token}\
             <button type=\"submit\" data-withdraw-archive>Withdraw</button></form>"
        )
    } else {
        String::new()
    };
    format!(
        "<span class=\"records-actions\"><button type=\"button\" class=\"primary\" \
         data-dialog-open=\"allow-archive\" data-allow-archive>{open}</button>{withdraw}</span>\
         <dialog id=\"allow-archive\" aria-labelledby=\"allow-archive-title\">\
         <form method=\"post\" action=\"/plugins/{instance}/archive\">{token}\
         <div class=\"dialog-head\"><h2 id=\"allow-archive-title\">{title}</h2></div>\
         <div class=\"dialog-body\"><p class=\"hint\">{instance} is restarted with it, and moves its \
         records past their window there where its admin chooses archived. What it holds is kept \
         if the archive is withdrawn.</p>\
         <label>Bound, in GiB <input name=\"most_gib\" type=\"number\" min=\"1\" step=\"1\" \
         inputmode=\"numeric\" value=\"{bound}\" placeholder=\"No bound\"></label>\
         <p class=\"hint\">Empty for no bound: it may then grow without limit.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\">{submit}</button></div></form></dialog>\
         <script>{DIALOG}</script>"
    )
}

/// Opens the dialog its button names, and closes it on Cancel or a press
/// outside it, as the deployment's Settings does its own.
const DIALOG: &str = r##"(function () {
  var dialog = document.getElementById("allow-archive");
  if (!dialog) return;
  document.addEventListener("click", function (event) {
    if (event.target.closest("[data-allow-archive]")) {
      dialog.showModal();
      var first = dialog.querySelector("input:not([type=hidden])");
      if (first) first.focus();
      return;
    }
    if (event.target.closest("#allow-archive [data-dialog-close]") || event.target === dialog) dialog.close();
  });
})();"##;

/// Back to the plugin's Summary under Manage, or the conductor's sentence.
fn back(instance: &str, outcome: Result<(), String>) -> Response {
    let summary = format!("/plugins/{instance}?level=admin&tab=summary");
    match outcome {
        Ok(()) => (
            StatusCode::SEE_OTHER,
            [(
                axum::http::header::LOCATION,
                format!("{summary}&saved=1#part-records"),
            )],
        )
            .into_response(),
        Err(sentence) => crate::admin::not_done(&sentence, &summary),
    }
}

/// One archive command, for the deployment admin signed in.
async fn command(
    app: &App,
    subject: &str,
    topic: &str,
    request_type: &str,
    request: impl Message,
) -> Result<(), String> {
    let answered = app
        .bus
        .call_for(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            // A restart waits on the launcher's two asks.
            Some(Duration::from_secs(50)),
            subject,
        )
        .await;
    let (_, bytes) = answered.map_err(|failed| match failed {
        BusError::HandlerFailed { detail, .. } => detail,
        other => other.to_string(),
    })?;
    PluginArchive::decode(&bytes[..]).map_err(|failed| failed.to_string())?;
    Ok(())
}

/// The bound as posted: whole GiB, or empty for none.
pub fn bound_posted(fields: &Fields) -> Result<u64, String> {
    let given = fields.get("most_gib").map(|v| v.trim()).unwrap_or_default();
    if given.is_empty() {
        return Ok(0);
    }
    given
        .parse::<u64>()
        .ok()
        .filter(|gib| *gib > 0)
        .and_then(|gib| gib.checked_mul(GIB))
        .ok_or_else(|| format!("the bound {given:?} is not a whole number of GiB"))
}

/// `POST /plugins/{instance}/archive`: a deployment admin allows an archive,
/// or changes its bound (W8.7).
pub async fn allow(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let outcome = match bound_posted(&fields) {
        Ok(most_bytes) => {
            command(
                &app,
                &session.subject,
                ALLOW_ARCHIVE,
                "meridian.v1.AllowArchiveRequest",
                AllowArchiveRequest {
                    instance_id: instance.clone(),
                    most_bytes,
                },
            )
            .await
        }
        Err(why) => Err(why),
    };
    back(&instance, outcome)
}

/// `POST /plugins/{instance}/archive/withdraw` (W8.7).
pub async fn withdraw(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(instance): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, _) = match gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = form_token_matches(&session, &fields) {
        return *response;
    }
    let outcome = command(
        &app,
        &session.subject,
        WITHDRAW_ARCHIVE,
        "meridian.v1.WithdrawArchiveRequest",
        WithdrawArchiveRequest {
            instance_id: instance.clone(),
        },
    )
    .await;
    back(&instance, outcome)
}

#[cfg(test)]
mod tests;
