//! A plugin's page and a prepared change (W6.26, W6.27; the plan's Q15,
//! Q16 and Q20): the two places the dashboard -- which mints the assertion
//! for every request to a plugin -- reads a request before passing it on.
//!
//! **A person prepares on the page.** The kit's Prepare button posts the
//! page's own form with the reserved field `prepare` (its value the
//! person's reason, or the form's `note`). The dashboard finds the act whose
//! route the form posts to, parses the form into the act's typed record by
//! its input schema -- one record from the form and the tool -- runs the
//! act's check with the person's claims, and keeps the prepared change; the
//! act's route is never called.
//!
//! **Knowing it was made.** A page opened from a prepared change carries its
//! ID in the claims; the person's request to the act's own route from it is
//! passed on whole, and on the plugin's answer the prepared change settles:
//! `made` where the record matches what was prepared, `changed` where the
//! person altered it. A refusal settles nothing.

use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use meridian_pb::v1::ToolDeclaration;
use serde_json::{Map, Value};

use super::store::Kept;
use crate::html::escape;
use crate::mcp::Caller;
use crate::plugins::Plugins;
use crate::web::App;

/// The reserved field (W6.26): a plugin's own field of that name is refused
/// by its SDK.
pub const PREPARE_FIELD: &str = "prepare";

/// The most of a form read before it is passed on.
const MOST_FORM: usize = 1 << 20;

fn is_form(request: &Request) -> bool {
    request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.starts_with("application/x-www-form-urlencoded"))
}

fn is_json(request: &Request) -> bool {
    request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|kind| kind.starts_with("application/json"))
}

/// One step of a form field's path: a name, or a place in a list.
enum Step {
    Name(String),
    At(usize),
}

/// `positions[0].lots[1].cost` as its steps.
fn steps(path: &str) -> Vec<Step> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let mut rest = part;
        let name_end = rest.find('[').unwrap_or(rest.len());
        if name_end > 0 {
            out.push(Step::Name(rest[..name_end].to_string()));
        }
        rest = &rest[name_end..];
        while let Some(open) = rest.strip_prefix('[') {
            let Some(close) = open.find(']') else { break };
            if let Ok(at) = open[..close].parse() {
                out.push(Step::At(at));
            }
            rest = &open[close + 1..];
        }
    }
    out
}

fn place(into: &mut Value, steps: &[Step], value: Value) {
    let Some((first, rest)) = steps.split_first() else {
        *into = value;
        return;
    };
    match first {
        Step::Name(name) => {
            if !into.is_object() {
                *into = Value::Object(Map::new());
            }
            let next = into
                .as_object_mut()
                .expect("an object")
                .entry(name.clone())
                .or_insert(Value::Null);
            place(next, rest, value);
        }
        Step::At(at) => {
            if !into.is_array() {
                *into = Value::Array(Vec::new());
            }
            let list = into.as_array_mut().expect("a list");
            while list.len() <= *at {
                list.push(Value::Null);
            }
            place(&mut list[*at], rest, value);
        }
    }
}

/// The schema of the value at `steps` in an input schema.
fn schema_at<'a>(schema: &'a Value, steps: &[Step]) -> Option<&'a Value> {
    let mut at = schema;
    for step in steps {
        at = match step {
            Step::Name(name) => at.get("properties")?.get(name)?,
            Step::At(_) => at.get("items")?,
        };
    }
    Some(at)
}

/// A form's text, typed as the schema says its field is.
fn typed(text: &str, schema: Option<&Value>) -> Value {
    let kind = schema
        .and_then(|schema| schema.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("string");
    match kind {
        "integer" => text
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(text.to_string())),
        "number" => serde_json::from_str::<serde_json::Number>(text.trim())
            .map(Value::Number)
            .unwrap_or_else(|_| Value::String(text.to_string())),
        "boolean" => Value::Bool(matches!(text, "true" | "on" | "1" | "yes")),
        "array" | "object" => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
        }
        _ => Value::String(text.to_string()),
    }
}

/// A form as the act's typed record, by its input schema: the form's token
/// and the reserved field left out.
pub fn record_of_form(pairs: &[(String, String)], input_schema: &str) -> Value {
    let schema: Value = serde_json::from_str(input_schema).unwrap_or(Value::Null);
    let mut record = Value::Object(Map::new());
    for (name, text) in pairs {
        if name == PREPARE_FIELD || name == "csrf" || name == "form_token" {
            continue;
        }
        let path = steps(name);
        if path.is_empty() {
            continue;
        }
        let known = schema_at(&schema, &path);
        if known.is_none() && schema.get("properties").is_some() {
            continue;
        }
        place(&mut record, &path, typed(text, known));
    }
    record
}

/// The act whose route a request is: an act of the instance's at this
/// method and path.
fn act_at(app: &App, instance: &str, method: &Method, path: &str) -> Option<ToolDeclaration> {
    let path = path.split('?').next().unwrap_or(path);
    app.health.view().get(instance).and_then(|report| {
        report
            .declared_tools
            .iter()
            .find(|tool| {
                !tool.reads
                    && tool.method.eq_ignore_ascii_case(method.as_str())
                    && tool.path == path
            })
            .cloned()
    })
}

/// A form's fields, as `application/x-www-form-urlencoded` writes them.
pub fn pairs_of(bytes: &[u8]) -> Vec<(String, String)> {
    let mut url = reqwest::Url::parse("http://form.invalid/").expect("a fixed address");
    url.set_query(Some(&String::from_utf8_lossy(bytes)));
    url.query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

fn rebuilt(parts: axum::http::request::Parts, bytes: axum::body::Bytes) -> Request {
    Request::from_parts(parts, Body::from(bytes))
}

fn answer_page(status: StatusCode, title: &str, said: &str, link: &str) -> Response {
    (
        status,
        Html(crate::html::page(
            title,
            &format!(
                "<h1>{}</h1><p class=\"{}\">{}</p>{link}<p><a href=\"javascript:history.back()\">Back</a></p>",
                escape(title),
                if status.is_success() { "passed" } else { "refused" },
                escape(said)
            ),
        )),
    )
        .into_response()
}

/// W6.26, Q20: a form posted with the reserved field is prepared, never
/// passed on; any other request comes back to be passed on as it was.
pub async fn prepare_on_page(
    app: &App,
    caller: &Caller,
    instance: &str,
    request: Request,
) -> Result<Request, Response> {
    if request.method() != Method::POST || !is_form(&request) {
        return Ok(request);
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, MOST_FORM).await else {
        return Err(answer_page(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Not passed on",
            "the form is larger than a page's form may be",
            "",
        ));
    };
    let pairs: Vec<(String, String)> = pairs_of(&bytes);
    let Some(reason) = pairs
        .iter()
        .find(|(name, _)| name == PREPARE_FIELD)
        .map(|(_, value)| value.clone())
    else {
        return Ok(rebuilt(parts, bytes));
    };
    let reason = if reason.trim().is_empty() || reason == "1" {
        pairs
            .iter()
            .find(|(name, _)| name == "note")
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    } else {
        reason
    };
    let Some(act) = act_at(app, instance, &parts.method, parts.uri.path()) else {
        return Err(answer_page(
            StatusCode::BAD_REQUEST,
            "Not prepared",
            "this form posts to no act of the plugin's that can be prepared",
            "",
        ));
    };
    let record = record_of_form(&pairs, &act.input_schema);
    let tool = format!("{instance}__{}", act.name);
    match super::prepare(app, caller, &tool, record, &reason).await {
        Ok(kept) => Err(answer_page(
            StatusCode::OK,
            "Prepared",
            &format!(
                "{} is prepared and waits for someone who may make it; nothing changed.",
                kept.prepared_call_id
            ),
            &format!(
                "<p><a href=\"{}/prepared/{}\" target=\"_top\">The prepared change</a></p>",
                escape(app.public_url.trim_end_matches('/')),
                escape(&kept.prepared_call_id)
            ),
        )),
        Err(refusal) => Err(answer_page(
            StatusCode::BAD_REQUEST,
            "Not prepared",
            refusal
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("the check refused it"),
            "",
        )),
    }
}

/// W6.27: the request passed on, and where it is the person's request to
/// the act's own route, the prepared change settled on the answer.
#[allow(clippy::too_many_arguments)]
pub async fn forward_settling(
    app: &App,
    plugins: &Plugins,
    instance: &str,
    request: Request,
    assertion: String,
    caller: &Caller,
    kept: &Kept,
    key: &str,
) -> Response {
    let Some(act) = act_at(app, instance, request.method(), request.uri().path()) else {
        return crate::plugins::forward(plugins, instance, request, assertion).await;
    };
    if format!("{instance}__{}", act.name) != kept.tool_name {
        return crate::plugins::forward(plugins, instance, request, assertion).await;
    }
    let json = is_json(&request);
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, MOST_FORM).await else {
        return answer_page(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Not passed on",
            "the request is larger than a page's form may be",
            "",
        );
    };
    let record = if json {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    } else {
        let pairs: Vec<(String, String)> = pairs_of(&bytes);
        record_of_form(&pairs, &act.input_schema)
    };
    let answered =
        crate::plugins::forward(plugins, instance, rebuilt(parts, bytes), assertion).await;
    let status = answered.status();
    if status.is_success() || status.is_redirection() {
        super::settle_from_page(app, caller, &kept.prepared_call_id, &record).await;
        plugins.forget_prepared(key);
    }
    answered
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_form_is_the_acts_typed_record_by_its_schema() {
        let schema = r#"{"type":"object","properties":{
            "account_id":{"type":"string"},
            "days":{"type":"integer"},
            "live":{"type":"boolean"},
            "positions":{"type":"array","items":{"type":"object","properties":{
                "quantity":{"type":"string"},"lots":{"type":"array","items":{"type":"object","properties":{"cost":{"type":"string"}}}}}}}}}"#;
        let pairs: Vec<(String, String)> = [
            ("csrf", "x"),
            ("prepare", "why"),
            ("account_id", "ACC-1"),
            ("days", "30"),
            ("live", "on"),
            ("positions[0].quantity", "1.5"),
            ("positions[0].lots[1].cost", "12.00"),
            ("stranger", "dropped"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        assert_eq!(
            record_of_form(&pairs, schema),
            json!({"account_id": "ACC-1", "days": 30, "live": true,
                   "positions": [{"quantity": "1.5", "lots": [null, {"cost": "12.00"}]}]})
        );
    }
}
