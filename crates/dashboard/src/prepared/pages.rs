//! A prepared change on the person's pages (W6.27, W6.28): the Inbox's
//! "Prepared changes" -- those waiting for them, one line each, behind a
//! pager -- and one prepared change, whole: the act, its arguments as
//! prepared, the check's answer, who prepared it and through which client,
//! why (withheld while suspect), its state and its history; with Make (or
//! the page it is made on), Discard, Share and Withdraw where they may.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use meridian_pb::v1::PreparedCallState;
use serde_json::Value;

use super::store::{Kept, StepKind};
use super::{Act, Prepared};
use crate::html::{crumb_here, crumb_link, escape, page_with, Chrome, Report, Viewer};
use crate::session::Session;
use crate::web::App;

type Fields = HashMap<String, String>;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/inbox/prepared", get(waiting_page))
        .route("/prepared/{id}", get(prepared_page))
        .route("/prepared/{id}/make", post(make_posted))
        .route("/prepared/{id}/discard", post(discard_posted))
        .route("/prepared/{id}/share", post(share_posted))
        .route("/prepared/{id}/withdraw", post(withdraw_posted))
}

fn chrome<'a>(session: &'a Session, admin: bool, crumbs: String) -> Chrome<'a> {
    Chrome {
        viewer: Some(Viewer {
            display_name: &session.display_name,
            form_token: &session.form_token,
            admin,
        }),
        crumbs,
        main: "page",
        in_admin: false,
        report: Report::Dashboard,
    }
}

fn token_input(session: &Session) -> String {
    format!(
        "<input type=\"hidden\" name=\"form_token\" value=\"{}\">",
        escape(&session.form_token)
    )
}

fn said(status: StatusCode, title: &str, sentence: &str) -> Response {
    (
        status,
        Html(crate::html::page(
            title,
            &format!(
                "<h1>{}</h1><p class=\"refused\">{}</p><p><a href=\"/inbox/prepared\">Prepared changes</a></p>",
                escape(title),
                escape(sentence)
            ),
        )),
    )
        .into_response()
}

/// The Inbox's two parts: its notices and the prepared changes waiting.
pub fn inbox_tabs(here: &str, waiting: usize) -> String {
    let tab = |href: &str, key: &str, title: String| {
        format!(
            "<a href=\"{href}\" data-tab=\"{key}\"{}>{title}</a>",
            if key == here {
                " class=\"here\" aria-current=\"page\""
            } else {
                ""
            }
        )
    };
    format!(
        "<nav class=\"tabs view-tabs\" aria-label=\"The inbox\">{}{}</nav>",
        tab("/inbox", "notices", "Notices".into()),
        tab(
            "/inbox/prepared",
            "prepared",
            if waiting == 0 {
                "Prepared changes".into()
            } else {
                format!("Prepared changes <span class=\"badge accent\">{waiting}</span>")
            }
        ),
    )
}

/// Who prepared it, as a line says it: the person, and through the client.
pub fn prepared_by(kept: &Kept) -> String {
    let name = if kept.preparer_name.is_empty() {
        &kept.preparer_subject
    } else {
        &kept.preparer_name
    };
    if kept.client_name.is_empty() {
        name.to_string()
    } else {
        format!("{name} through {}", kept.client_name)
    }
}

/// The reason as shown: withheld while it is held as suspect.
pub fn reason_shown(kept: &Kept) -> String {
    if kept.reason_suspect {
        "[withheld: held as suspect until a person releases it]".into()
    } else {
        kept.reason.clone()
    }
}

/// A band on a page opened from a prepared change: "prepared by" and why,
/// the whole of it one click away (W6.27).
pub fn band(kept: &Kept) -> String {
    format!(
        "<div class=\"notice\" role=\"status\" data-prepared=\"{id}\">Prepared by {by}: \
         <q>{reason}</q> <a href=\"/prepared/{id}\">The whole of it</a></div>",
        id = escape(&kept.prepared_call_id),
        by = escape(&prepared_by(kept)),
        reason = escape(&reason_shown(kept)),
    )
}

async fn waiting_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let admin = super::access_of(&records, &session.subject).deployment_admin
        || meridian_access::person_access(&records, &session.subject, &session.directory_groups)
            .deployment_admin;
    let waiting = super::waiting_for(&app, &session.subject).await;
    let rows: String = waiting
        .iter()
        .map(|kept| {
            format!(
                "<tr data-prepared=\"{id}\"><td title=\"{tool}\"><a href=\"/prepared/{id}\">{title}</a></td>\
                 <td class=\"wide\" title=\"{by}\">{by}</td><td title=\"{reason}\">{reason}</td>\
                 <td>{state}</td><td class=\"wide\">{when}</td></tr>",
                id = escape(&kept.prepared_call_id),
                tool = escape(&kept.tool_name),
                title = escape(&super::act_title(&app, kept)),
                by = escape(&prepared_by(kept)),
                reason = escape(&reason_shown(kept)),
                state = super::state_words(kept.state),
                when = crate::custody::utc(kept.prepared_at_ns),
            )
        })
        .collect();
    let list = if waiting.is_empty() {
        "<div class=\"panel empty-state\"><strong>Nothing waiting</strong><p>A change prepared \
         for you to make -- by your agent, or by someone else for everyone who may make it -- is \
         listed here.</p></div>"
            .to_string()
    } else {
        format!(
            "<om-pager search=\"Search the prepared changes\"><table class=\"list one-line prepared\">\
             <thead><tr><th>Act</th><th class=\"wide\">Prepared by</th><th>Why</th><th>State</th>\
             <th class=\"wide\">Prepared</th></tr></thead><tbody>{rows}</tbody></table></om-pager>"
        )
    };
    let body = format!(
        "<div class=\"inbox\"><div class=\"page-head\"><div><h1>Inbox</h1>\
         <p>Changes prepared for you to make, change first or discard; nothing changes until a \
         person makes one.</p></div></div>{tabs}{list}</div>",
        tabs = inbox_tabs("prepared", waiting.len()),
    );
    Html(page_with(
        "Prepared changes",
        &body,
        &chrome(
            &session,
            admin,
            crumb_link("/", "Home")
                + &crumb_link("/inbox", "Inbox")
                + &crumb_here("Prepared changes", None),
        ),
    ))
    .into_response()
}

/// Where the person makes it, when a page draws the act (W6.27).
fn made_on_link(act: &Act, kept: &Kept) -> Option<String> {
    match act {
        Act::Plugin { instance, .. } => act.made_on().map(|path| {
            format!(
                "/plugins/{}?level=write&prepared={}&path={}",
                instance,
                kept.prepared_call_id,
                crate::web::path_segment(path)
            )
        }),
        Act::Core(spec) if spec.name == "set_plugin_settings" => {
            super::instance_named(kept).map(|instance| {
                format!(
                    "/admin/plugins/{instance}?tab=settings&prepared={}",
                    kept.prepared_call_id
                )
            })
        }
        Act::Core(_) => None,
    }
}

fn arguments_facts(kept: &Kept) -> String {
    let arguments: Value = serde_json::from_str(&kept.arguments_json).unwrap_or(Value::Null);
    let rows: String = arguments
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(name, value)| {
                    let said = match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    format!(
                        "<dt><code>{}</code></dt><dd>{said}</dd>",
                        escape(name),
                        said = escape(&said)
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    format!("<dl class=\"facts\" aria-label=\"Its arguments\">{rows}</dl>")
}

async fn prepared_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let access =
        meridian_access::person_access(&records, &session.subject, &session.directory_groups);
    let Ok(Some(kept)) = app.prepared.get(&id).await else {
        return said(
            StatusCode::NOT_FOUND,
            "Not found",
            &format!("No prepared change {id} is shown to you."),
        );
    };
    let act = super::resolve_kept(&app, &kept, &records).await;
    let eyes = match &act {
        Some(act) => app.prepared.eyes(&app, act, &kept).await,
        None => false,
    };
    let shown = act
        .as_ref()
        .map(|act| Prepared::shown_to(&app, &records, act, &kept, eyes))
        .unwrap_or_default()
        .contains(&session.subject)
        || kept.preparer_subject == session.subject;
    if !shown {
        return said(
            StatusCode::NOT_FOUND,
            "Not found",
            &format!("No prepared change {id} is shown to you."),
        );
    }
    let steps = app.prepared.steps(&id).await.unwrap_or_default();
    let pending = super::store::PENDING.contains(&kept.state);
    let may_make = act
        .as_ref()
        .map(|act| Prepared::may_make(&app, &records, act, &kept, eyes, &session.subject))
        .unwrap_or(Err("its act is no longer offered".into()));
    let token = token_input(&session);
    // One form, one "Why", a button for each step the person may take; each
    // posts to its own route (formaction), so the note goes with the one
    // pressed.
    let button = |path: &str, label: &str, primary: bool, confirm: &str| {
        format!(
            "<button type=\"submit\" formaction=\"/prepared/{id}/{path}\"{primary}{confirm}>{label}</button>",
            id = escape(&id),
            primary = if primary { " class=\"primary\"" } else { "" },
            confirm = if confirm.is_empty() {
                String::new()
            } else {
                format!(" data-confirm=\"{}\"", escape(confirm))
            },
        )
    };
    let mut lead = String::new();
    let mut buttons = Vec::new();
    if pending {
        match (&may_make, &act) {
            (Ok(()), Some(act)) => {
                if let Some(href) = made_on_link(act, &kept) {
                    lead.push_str(&format!(
                        "<a class=\"button primary\" href=\"{}\" data-made-on>Open on its page</a>",
                        escape(&href)
                    ));
                }
                buttons.push(button("make", "Make", lead.is_empty(), ""));
            }
            (Err(why), _) => lead.push_str(&format!(
                "<p class=\"hint\" data-may-not-make>{}</p>",
                escape(why)
            )),
            _ => {}
        }
        buttons.push(button(
            "discard",
            "Discard",
            false,
            "Discard it? Nothing is made of it.",
        ));
        if kept.preparer_subject == session.subject {
            if kept.state == PreparedCallState::Prepared as i32 {
                buttons.push(button("share", "Share", false, ""));
            }
            if kept.through_delegation.is_empty() {
                buttons.push(button("withdraw", "Withdraw", false, ""));
            }
        }
    }
    let actions = if buttons.is_empty() {
        lead
    } else {
        format!(
            "{lead}<form method=\"post\" action=\"/prepared/{id}\" class=\"prepared-act\">{token}\
             <input name=\"note\" maxlength=\"2000\" placeholder=\"Why (optional)\" aria-label=\"Why\">{buttons}</form>",
            id = escape(&id),
            buttons = buttons.concat(),
        )
    };
    let history: String = steps
        .iter()
        .map(|step| {
            format!(
                "<tr><td>{what}</td><td>{who}</td><td>{when}</td><td title=\"{note}\">{note}</td></tr>",
                what = step.kind.said(),
                who = escape(&if step.by_subject.is_empty() {
                    "the dashboard".to_string()
                } else if step.client_name.is_empty() {
                    crate::tickets::display_name(&records, &step.by_subject)
                } else {
                    format!(
                        "{} through {}",
                        crate::tickets::display_name(&records, &step.by_subject),
                        step.client_name
                    )
                }),
                when = crate::custody::utc(step.at_ns),
                note = escape(&step.note),
            )
        })
        .collect();
    let body = format!(
        "<div class=\"prepared-change\"><div class=\"page-head\"><div><h1>{title}</h1>\
         <p><code>{id}</code> &middot; {state} &middot; <code title=\"Its tool\">{tool}</code>{eyes_said}</p></div></div>\
         <div class=\"notice\" role=\"status\">Prepared by {by}: <q>{reason}</q></div>\
         <div class=\"actions\" data-actions>{actions}</div>\
         <div class=\"stack\"><section class=\"panel padded\"><h2>As prepared</h2>{facts}\
         <p class=\"hint\">The check: {checked}</p></section>\
         <section class=\"panel padded\"><h2>Its history</h2><table class=\"list one-line\"><thead><tr>\
         <th>Step</th><th>By</th><th>When</th><th>Note</th></tr></thead><tbody>{history}</tbody></table></section></div>\
         </div>\
         <script>document.addEventListener(\"submit\",function(e){{var b=e.submitter;if(b&&b.hasAttribute(\"data-confirm\")&&!window.confirm(b.getAttribute(\"data-confirm\")))e.preventDefault();}});</script>",
        tool = escape(&kept.tool_name),
        title = escape(act.as_ref().map_or(kept.tool_name.as_str(), |act| act.title())),
        id = escape(&id),
        state = super::state_words(kept.state),
        eyes_said = if eyes {
            " &middot; a second pair of eyes: made by someone other than whoever prepared it"
        } else {
            ""
        },
        by = escape(&prepared_by(&kept)),
        reason = escape(&reason_shown(&kept)),
        facts = if super::contents_withheld(&app, &records, &kept, &session.subject) {
            format!(
                "<p class=\"hint\" data-withheld>{}</p>",
                escape(super::WITHHELD_CONTENTS)
            )
        } else {
            arguments_facts(&kept)
        },
        checked = escape(&kept.checked),
        actions = actions,
    );
    let _ = StepKind::Prepared;
    Html(page_with(
        &format!("Prepared change {id}"),
        &body,
        &chrome(
            &session,
            access.deployment_admin,
            crumb_link("/", "Home")
                + &crumb_link("/inbox/prepared", "Prepared changes")
                + &crumb_here(&id, None),
        ),
    ))
    .into_response()
}

fn person_of(session: &Session) -> crate::mcp::Caller {
    super::person_caller(
        &session.subject,
        &session.display_name,
        &session.directory_groups,
    )
}

fn checked_form(app: &App, headers: &HeaderMap, fields: &Fields) -> Result<Session, Box<Response>> {
    let (session, _) = crate::admin::gate(app, headers, false)?;
    crate::admin::form_token_matches(&session, fields)?;
    Ok(session)
}

fn note(fields: &Fields) -> String {
    fields
        .get("note")
        .map(|n| n.trim().to_string())
        .unwrap_or_default()
}

fn back_to(id: &str, outcome: Result<(), String>) -> Response {
    match outcome {
        Ok(()) => (
            StatusCode::SEE_OTHER,
            [(axum::http::header::LOCATION, format!("/prepared/{id}"))],
        )
            .into_response(),
        Err(why) => said(StatusCode::BAD_REQUEST, "Not done", &why),
    }
}

fn refusal_words(refusal: &Value) -> String {
    refusal
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("refused")
        .to_string()
}

async fn make_posted(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    let session = match checked_form(&app, &headers, &fields) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let outcome = super::make(&app, &person_of(&session), &id, None, &note(&fields))
        .await
        .map(|_| ())
        .map_err(|refusal| refusal_words(&refusal));
    back_to(&id, outcome)
}

async fn decided(
    app: Arc<App>,
    headers: HeaderMap,
    id: String,
    fields: Fields,
    kind: StepKind,
) -> Response {
    let session = match checked_form(&app, &headers, &fields) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let outcome = super::decide(&app, &person_of(&session), &id, kind, &note(&fields))
        .await
        .map(|_| ())
        .map_err(|refusal| refusal_words(&refusal));
    back_to(&id, outcome)
}

async fn discard_posted(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    decided(app, headers, id, fields, StepKind::Discarded).await
}

async fn share_posted(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    decided(app, headers, id, fields, StepKind::Shared).await
}

async fn withdraw_posted(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Response {
    decided(app, headers, id, fields, StepKind::Withdrawn).await
}
