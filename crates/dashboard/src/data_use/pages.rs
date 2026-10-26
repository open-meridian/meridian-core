//! The professional test's form, the deployment admin's change of a status,
//! the counts' CSV export, and the Data sources page's Use tab (contract
//! v28: W10.1, W10.13).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use meridian_access::person_access;
use meridian_domain::v1::AccessRecords;

use super::{Answering, Counts, Read, Status, STANDARD};
use crate::html::{crumb_here, crumb_link, escape, page_with, Chrome, Report, Viewer};
use crate::session::Session;
use crate::web::App;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/professional-test", get(test_page).post(answer_test))
        .route("/admin/data-sources/professional-status", post(set_status))
        .route("/admin/data-sources/use.csv", get(export))
}

type Fields = HashMap<String, String>;

fn field<'a>(fields: &'a Fields, name: &str) -> &'a str {
    fields.get(name).map(String::as_str).unwrap_or_default()
}

/// Where to go once answered: a path on this dashboard, or home.
fn then_of(fields: &Fields) -> String {
    let then = field(fields, "then");
    if then.starts_with('/') && !then.starts_with("//") && !then.contains('\\') {
        then.to_string()
    } else {
        "/".to_string()
    }
}

/// The platform's one text (the spec's section 2, ruling 379), on the CT
/// Plan's use-based test, the legacy registration questions folded in as
/// examples.
pub const STANDARD_TEXT: &str =
    "You are a non-professional only if all of these are true: you use \
the data for your own personal investing, not for a business, a firm or anyone else's account; \
you are not registered or qualified with a securities, commodities or futures regulator, exchange \
or association, and do not act as if you were (for example as a broker-dealer, an investment \
adviser, a futures commission merchant or a commodity trading adviser); and you do not use it in \
a trade or business, or to manage anyone else's money. Otherwise you are a professional.";

/// A vendor's own test: its words are its terms', which the deployment
/// holds no copy of.
pub const VENDOR_TEXT: &str = "This dataset's vendor defines who counts as a professional in its \
own terms: read them before you answer.";

fn test_words(test: &str) -> String {
    if test == STANDARD {
        "The standard test".to_string()
    } else {
        format!("{test}'s own test")
    }
}

fn person_chrome<'a>(session: &'a Session, admin: bool) -> Chrome<'a> {
    Chrome {
        viewer: Some(Viewer {
            display_name: &session.display_name,
            form_token: &session.form_token,
            admin,
        }),
        crumbs: crumb_link("/", "Home") + &crumb_here("Professional test", None),
        main: "sheet",
        in_admin: false,
        report: Report::Dashboard,
        sidebar: None,
    }
}

/// One test's form: what it asks, and the two answers.
fn form(test: &str, standing: Option<Status>, then: &str, token: &str) -> String {
    let text = if test == STANDARD {
        STANDARD_TEXT
    } else {
        VENDOR_TEXT
    };
    let now = match standing {
        Some(status) => format!(
            "<p class=\"hint\" data-answered=\"{}\">You answered {}. A deployment admin can change it.</p>",
            status.value(),
            status.words().to_lowercase()
        ),
        None => "<p class=\"hint\">Until you answer, you count as a professional.</p>".to_string(),
    };
    let buttons = if standing.is_some() {
        String::new()
    } else {
        "<div class=\"dialog-foot\"><button type=\"submit\" name=\"answer\" value=\"professional\">\
         I am a professional</button><button type=\"submit\" class=\"primary\" name=\"answer\" \
         value=\"non_professional\">I am a non-professional</button></div>"
            .to_string()
    };
    format!(
        "<form method=\"post\" action=\"/professional-test\" class=\"professional-test\" \
         data-test=\"{test}\">{token}<input type=\"hidden\" name=\"test\" value=\"{test}\">\
         <input type=\"hidden\" name=\"then\" value=\"{then}\">\
         <fieldset><legend>{words}</legend><p>{text}</p>{now}</fieldset>{buttons}</form>",
        test = escape(test),
        then = escape(then),
        words = escape(&test_words(test)),
        text = escape(text),
    )
}

/// `GET /professional-test?then=`: the tests the person may be asked, each
/// with what it asks and, unanswered, its two answers; where none is owed,
/// on to `then`.
async fn test_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(asked): Query<Fields>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let then = then_of(&asked);
    let tests = super::tests_for(&records, &session.subject, &session.directory_groups).await;
    let answers = match super::answers().await {
        Ok(answers) => answers,
        Err(why) => return crate::web::refused(&why),
    };
    let token = crate::admin::token_input(&session);
    let owed = tests
        .iter()
        .any(|test| !answers.contains_key(&(session.subject.clone(), test.clone())));
    let forms: String = tests
        .iter()
        .map(|test| {
            form(
                test,
                answers
                    .get(&(session.subject.clone(), test.clone()))
                    .copied(),
                &then,
                &token,
            )
        })
        .collect();
    let body = if tests.is_empty() {
        "<p class=\"empty\">No data you can read asks whether you are a professional.</p>"
            .to_string()
    } else {
        forms
    };
    let lead = if owed {
        "The data a plugin you are opening reads is priced differently for professionals and \
         non-professionals. Answer once, for yourself; an agent acting for you counts as you."
    } else {
        "How you answered for the data you can read."
    };
    let next = if owed {
        String::new()
    } else {
        format!("<p><a href=\"{}\">Continue</a></p>", escape(&then))
    };
    let admin =
        person_access(&records, &session.subject, &session.directory_groups).deployment_admin;
    Html(page_with(
        "Professional test",
        &format!("<h1>Professional or non-professional?</h1><p>{lead}</p>{body}{next}"),
        &person_chrome(&session, admin),
    ))
    .into_response()
}

/// `POST /professional-test`: the person's own answer, its own record.
async fn answer_test(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, false) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = crate::admin::form_token_matches(&session, &fields) {
        return *response;
    }
    let back = format!(
        "/professional-test?then={}",
        crate::documentation::urlencode(&then_of(&fields))
    );
    let test = field(&fields, "test");
    let tests = super::tests_for(&records, &session.subject, &session.directory_groups).await;
    if !tests.contains(test) {
        return crate::admin::not_done("no data you can read asks that test", &back);
    }
    let Some(status) = Status::parse(field(&fields, "answer")) else {
        return crate::admin::not_done("answer professional or non-professional", &back);
    };
    let answers = match super::answers().await {
        Ok(answers) => answers,
        Err(why) => return crate::web::refused(&why),
    };
    if answers.contains_key(&(session.subject.clone(), test.to_string())) {
        return crate::admin::not_done(
            "you have answered already; a deployment admin can change it",
            &back,
        );
    }
    match super::answer(
        Answering {
            subject: &session.subject,
            test,
            status,
            by: &session.subject,
            through_delegation: "",
            client_name: "",
            note: "",
        },
        app.clock.now_ns(),
    )
    .await
    {
        Ok(_) => crate::web::redirect(&back),
        Err(why) => crate::admin::not_done(&why, &back),
    }
}

const USE: &str = "/admin/data-sources#use";

/// `POST /admin/data-sources/professional-status`: a deployment admin
/// changes a person's status, with a note.
async fn set_status(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    if let Err(response) = crate::admin::form_token_matches(&session, &fields) {
        return *response;
    }
    match admin_change(
        &app,
        &records,
        field(&fields, "subject"),
        field(&fields, "test"),
        field(&fields, "answer"),
        field(&fields, "note"),
        (&session.subject, "", ""),
    )
    .await
    {
        Ok(change) => crate::web::redirect(&format!(
            "/admin/data-sources?done={}#use",
            crate::documentation::urlencode(&format!(
                "{} is {}",
                crate::tickets::display_name(&records, &change.subject),
                change.to.words().to_lowercase()
            ))
        )),
        Err((_, why)) => crate::admin::not_done(&why, USE),
    }
}

/// A deployment admin's change of a person's status, as the page and the
/// tool both make it: the person known, the test one their data asks or
/// the standard one, a note. Refused naming the field.
pub(crate) async fn admin_change(
    app: &App,
    records: &AccessRecords,
    subject: &str,
    test: &str,
    answer: &str,
    note: &str,
    by: (&str, &str, &str),
) -> Result<super::store::StatusChange, (&'static str, String)> {
    if subject.is_empty() {
        return Err(("subject", "name the person".into()));
    }
    if crate::service_accounts::is_service_account(subject) {
        return Err((
            "subject",
            "a service account has no professional status".into(),
        ));
    }
    let known = records.people.iter().any(|p| p.subject == subject)
        || records
            .user_groups
            .iter()
            .any(|g| g.logins.iter().any(|login| login == subject));
    if !known {
        return Err((
            "subject",
            format!("no person {subject} is known to this deployment"),
        ));
    }
    let test = if test.is_empty() { STANDARD } else { test };
    let asked = super::asked()
        .read()
        .map(|held| held.values().any(|tests| tests.contains(test)))
        .unwrap_or(false);
    if test != STANDARD && !asked {
        return Err(("test", format!("no dataset's terms ask the test {test}")));
    }
    let Some(status) = Status::parse(answer) else {
        return Err(("answer", "professional or non_professional".into()));
    };
    if note.trim().is_empty() {
        return Err(("note", "say why".into()));
    }
    super::answer(
        Answering {
            subject,
            test,
            status,
            by: by.0,
            through_delegation: by.1,
            client_name: by.2,
            note,
        },
        app.clock.now_ns(),
    )
    .await
    .map_err(|why| ("note", why))
}

/// `GET /admin/data-sources/use.csv?month=`: the month's counts as the
/// report's fields, for the deployment admin to file.
async fn export(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(asked): Query<Fields>,
) -> Response {
    let (session, records) = match crate::admin::gate(&app, &headers, true) {
        Ok(gated) => gated,
        Err(response) => return *response,
    };
    let listed = match crate::admin::data_sources::datasets(&app.bus, &session.subject).await {
        Ok(listed) => listed,
        Err(why) => return crate::web::refused(&format!("the lake did not answer: {why}")),
    };
    let month = Some(field(&asked, "month")).filter(|m| !m.is_empty());
    let read = match super::read(&app, &listed, &records, month).await {
        Ok(read) => read,
        Err(why) => return crate::web::refused(&why),
    };
    let body = super::csv(&read.month, &read.rows, &read.listed, |subject| {
        crate::tickets::display_name(&records, subject)
    });
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"data-use-{}.csv\"", read.month),
            ),
        ],
        body,
    )
        .into_response()
}

// ── The Use tab ────────────────────────────────────────────────────────────

/// The Use tab's body and its dialog: the month and the counting chosen,
/// the export, and a line per dataset or per underlying source.
pub fn tab(
    read: &Result<Read, String>,
    by_source: bool,
    records: &AccessRecords,
    token: &str,
) -> (String, String) {
    let read = match read {
        Ok(read) => read,
        Err(why) => {
            return (
                format!(
                    "<p class=\"refused\">The counts could not be read: {}</p>",
                    escape(why)
                ),
                String::new(),
            )
        }
    };
    let counts: Vec<Counts> = if by_source {
        super::by_source(&read.rows, &read.listed)
    } else {
        super::by_dataset(&read.rows, &read.listed)
    };
    let months: String = read
        .months
        .iter()
        .map(|m| {
            format!(
                "<option value=\"{m}\"{}>{m}</option>",
                if *m == read.month { " selected" } else { "" },
                m = escape(m)
            )
        })
        .collect();
    let choose = format!(
        "<form method=\"get\" action=\"/admin/data-sources#use\" class=\"use-choose\">\
         <label>Month <select name=\"month\" onchange=\"this.form.submit()\">{months}</select></label>\
         <label>Count by <select name=\"by\" onchange=\"this.form.submit()\">\
         <option value=\"dataset\"{d}>Dataset</option><option value=\"source\"{s}>Source</option></select></label>\
         <noscript><button type=\"submit\">Show</button></noscript>\
         <a class=\"button\" href=\"/admin/data-sources/use.csv?month={month}\" download>Export CSV</a></form>",
        d = if by_source { "" } else { " selected" },
        s = if by_source { " selected" } else { "" },
        month = escape(&read.month),
    );
    let lines: String = counts
        .iter()
        .map(|c| {
            let meters = if c.meters.is_empty() {
                "none".to_string()
            } else {
                c.meters.iter().map(|m| m.replace('_', "-")).collect::<Vec<_>>().join(", ")
            };
            let status = if c.split {
                format!("{} / {}", c.professional, c.non_professional)
            } else {
                "not split".to_string()
            };
            format!(
                "<tr data-id=\"{key}\"><td title=\"{key}\">{key}</td>\
                 <td class=\"wide\" title=\"{meters}\">{meters}</td>\
                 <td class=\"num\" data-persons>{persons}</td>\
                 <td class=\"muted\" data-served title=\"Persons served and devices are counted from \
                 what the sidecars serve, which a later release counts\">not counted yet</td>\
                 <td class=\"wide\" data-status title=\"Professional / non-professional; unanswered \
                 counts as professional\">{status}</td>\
                 <td class=\"num\" data-plugins title=\"{plugins_said}\">{plugins}</td>\
                 <td class=\"wide num\" data-service-accounts>{sa}</td>\
                 <td class=\"wide\" title=\"{beside}\">{beside}</td></tr>",
                key = escape(&c.key),
                persons = c.persons,
                plugins = c.plugins_total(),
                plugins_said = escape(&c.plugins_said()),
                sa = c.service_accounts,
                beside = escape(&crate::mcp::others_words(&c.beside)),
            )
        })
        .collect();
    let table = if lines.is_empty() {
        "<p class=\"empty\" data-use=\"0\">No dataset yet, so nothing to count.</p>".to_string()
    } else {
        format!(
            "<om-pager search=\"Search counts\"><table class=\"list one-line use\" data-use=\"{n}\"><thead><tr>\
             <th>{first}</th><th class=\"wide\">Meters</th>\
             <th class=\"num\" title=\"Display persons able to read it\">Persons</th>\
             <th title=\"Display persons served\">Served</th>\
             <th class=\"wide\" title=\"Professional / non-professional\">Pro / non-pro</th>\
             <th class=\"num\" title=\"Non-display plugins entitled, by category\">Plugins</th>\
             <th class=\"wide num\" title=\"Service accounts able to read it, on the non-display meter\">Service accts</th>\
             <th class=\"wide\">{beside}</th></tr></thead><tbody>{lines}</tbody></table></om-pager>",
            n = counts.len(),
            first = if by_source { "Source" } else { "Dataset" },
            beside = if by_source { "Datasets" } else { "Source" },
        )
    };
    let tests: Vec<String> = {
        let mut tests: Vec<String> = vec![STANDARD.to_string()];
        if let Ok(held) = super::asked().read() {
            for test in held.values().flatten() {
                if !tests.contains(test) {
                    tests.push(test.clone());
                }
            }
        }
        tests
    };
    let people: BTreeMap<&str, &str> = records
        .people
        .iter()
        .map(|p| (p.subject.as_str(), p.display_name.as_str()))
        .collect();
    let dialog = format!(
        "<dialog id=\"professional-status\" aria-labelledby=\"professional-status-title\"><form method=\"post\" \
         action=\"/admin/data-sources/professional-status\">{token}\
         <div class=\"dialog-head\"><h2 id=\"professional-status-title\">Change a professional status</h2></div>\
         <div class=\"dialog-body\">\
         <label>Person <input name=\"subject\" list=\"status-people\" required autocomplete=\"off\"></label>\
         <datalist id=\"status-people\">{people}</datalist>\
         <label>Test <select name=\"test\">{tests}</select></label>\
         <label>Status <select name=\"answer\"><option value=\"professional\">Professional</option>\
         <option value=\"non_professional\">Non-professional</option></select></label>\
         <label>Note <input name=\"note\" maxlength=\"2000\" required placeholder=\"Why\"></label>\
         <p class=\"hint\">The person's own answer stands until you change it; each change is kept.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\">Change</button></div></form></dialog>",
        people = people
            .iter()
            .map(|(subject, name)| format!(
                "<option value=\"{}\" label=\"{}\">",
                escape(subject),
                escape(name)
            ))
            .collect::<String>(),
        tests = tests
            .iter()
            .map(|t| format!("<option value=\"{}\">{}</option>", escape(t), escape(&test_words(t))))
            .collect::<String>(),
    );
    (format!("{choose}{table}"), dialog)
}

#[cfg(test)]
pub(super) fn then_for_tests(then: &str) -> String {
    let mut fields = Fields::new();
    fields.insert("then".into(), then.into());
    then_of(&fields)
}
