//! The data terms' tools (contract v28: W10.1, W10.13; names approved
//! 2026-10-24, ruling 596): `read_data_use` and `export_data_use` at the
//! deployment admin's level, the Data sources page's Use tab and its
//! export; and `answer_professional_test`, a person's own act through
//! their delegation, which a deployment admin may also use to change
//! another person's status with a note, as the Use tab's dialog does.
//!
//! The counts are [`crate::data_use`]'s, read as the page reads them: who
//! could use each dataset in the month, from the records and the
//! entitlements; the persons served and the devices are v30's and answered
//! as not counted yet, never as a number.

use serde_json::{json, Map, Value};

use super::instruments::{ask, bus_refused, object, only, text, Problems};
use super::{refused, Area, Caller, Spec};
use crate::data_use::{self, Answering, Status, STANDARD};
use crate::web::App;
use meridian_domain::v1::{ListDatasetsReply, ListDatasetsRequest};

const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

pub static SPECS: &[Spec] = &[
    Spec {
        name: "read_data_use",
        title: "Read the data use",
        description: "who could use each dataset in a calendar month (`month`, YYYY-MM, this \
month by default), against its meters, counted from the records and the entitlements: the \
display persons able to read it, split professional and non-professional where its terms split \
them (unanswered counts as professional); the plugin instances entitled to it by non-display \
category; the service accounts able to read it, on the non-display meter. `by` `source` counts \
per underlying source instead, each once however many of its datasets it could use. Persons \
served and devices are not counted yet (null). Never a price; never sent to the platform. \
Answers `unchanged`, with the months kept.",
        reads: true,
        open_world: false,
        input_schema: read_schema,
        area: Area::DataUse,
    },
    Spec {
        name: "export_data_use",
        title: "Export the data use",
        description: "a month's data use as CSV (`month`, YYYY-MM, this month by default), one \
line per dataset and who could use it, in the report's fields: month, dataset, underlying source, \
meter, kind, who, name, classification, activated, deactivated, quantity and timeliness. The \
deployment admin files it with the vendor; the deployment never sends it. Answers `unchanged`.",
        reads: true,
        open_world: false,
        input_schema: export_schema,
        area: Area::DataUse,
    },
    Spec {
        name: "answer_professional_test",
        title: "Answer the professional test",
        description: "answer, once, whether the person is a professional or a non-professional \
for data whose terms price them apart: `test` `standard` (the default) or the dataset whose \
vendor's own test applies, and `answer` `professional` or `non_professional`, with a note. The \
standard test: a non-professional uses the data only for their own personal investing, not for a \
business, a firm or anyone else's account; is not registered or qualified with a securities, \
commodities or futures regulator, exchange or association, nor acts as if they were (a \
broker-dealer, an investment adviser, a futures commission merchant or a commodity trading \
adviser, for example); and does not use it in a trade or business or to manage anyone else's \
money. Anyone else is a professional, and unanswered counts as professional. An agent answers \
only what its person says. A deployment admin may name another person's `subject` to change \
their status. Each answer is its own record. Answers the record.",
        reads: false,
        open_world: false,
        input_schema: answer_schema,
        area: Area::ProfessionalTest,
    },
];

fn month_property() -> Value {
    json!({"type": "string", "pattern": "^[0-9]{4}-[0-9]{2}$",
           "description": "the calendar month, YYYY-MM, in UTC; this month when omitted"})
}

fn read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "month": month_property(),
            "by": {"type": "string", "enum": ["dataset", "source"],
                   "description": "count per dataset (the default) or per underlying source"},
        },
        "additionalProperties": false,
    })
}

fn export_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"month": month_property()},
        "additionalProperties": false,
    })
}

fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "test": {"type": "string", "description": "`standard`, or the dataset whose vendor's own test applies"},
            "answer": {"type": "string", "enum": ["professional", "non_professional"]},
            "subject": {"type": "string", "description": "a deployment admin's change of another person's status: their subject"},
            "note": {"type": "string", "maxLength": 2000, "description": "why"},
        },
        "required": ["answer", "note"],
        "additionalProperties": false,
    })
}

/// The tool, called.
pub async fn call(app: &App, caller: &Caller, spec: &Spec, arguments: Value) -> Value {
    match spec.name {
        "read_data_use" => read(app, caller, &arguments, false).await,
        "export_data_use" => read(app, caller, &arguments, true).await,
        "answer_professional_test" => answer(app, caller, &arguments).await,
        other => refused("not_listed", &format!("no tool {other}"), Vec::new()),
    }
}

fn month_of(top: &Map<String, Value>, problems: &mut Problems) -> Option<String> {
    let month = text(top, "month", "", false, problems);
    if month.is_empty() {
        return None;
    }
    if !data_use::is_month(&month) {
        problems.add("month", "a calendar month, YYYY-MM");
    }
    Some(month)
}

async fn read(app: &App, caller: &Caller, arguments: &Value, export: bool) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(
        top,
        if export { &["month"] } else { &["month", "by"] },
        "",
        &mut problems,
    );
    let month = month_of(top, &mut problems);
    let by = if export {
        String::new()
    } else {
        text(top, "by", "", false, &mut problems)
    };
    if !matches!(by.as_str(), "" | "dataset" | "source") {
        problems.add("by", "dataset or source");
    }
    if !problems.is_empty() {
        return problems.refusal();
    }
    let records = match app.records.current(app.clock.now_ns()) {
        Ok(records) => records,
        Err(stale) => return refused("unavailable", &stale.to_string(), Vec::new()),
    };
    if !caller.access(&records).deployment_admin {
        return refused(
            "not_permitted",
            "the data use is a deployment admin's to read",
            Vec::new(),
        );
    }
    let listed: Result<ListDatasetsReply, _> = ask(
        app,
        caller,
        crate::admin::data_sources::LIST_DATASETS,
        "meridian.v1.ListDatasetsRequest",
        ListDatasetsRequest {},
        WAIT,
    )
    .await;
    let listed = match listed {
        Ok(listed) => listed,
        Err(failed) => return bus_refused(failed, ""),
    };
    let read = match data_use::read(app, &listed, &records, month.as_deref()).await {
        Ok(read) => read,
        Err(why) => return refused("unavailable", &why, Vec::new()),
    };
    if export {
        let csv = data_use::csv(&read.month, &read.rows, &read.listed, |subject| {
            crate::tickets::display_name(&records, subject)
        });
        return json!({"outcome": "unchanged", "data": {
            "month": read.month,
            "lines": read.rows.len(),
            "csv": csv,
        }});
    }
    let by_source = by == "source";
    let counts = if by_source {
        data_use::by_source(&read.rows, &read.listed)
    } else {
        data_use::by_dataset(&read.rows, &read.listed)
    };
    let mut counted: Vec<Value> = counts
        .iter()
        .map(|c| data_use::counts_json(c, by_source))
        .collect();
    // A vendor's name and a source's are a plugin's words.
    for entry in &mut counted {
        super::others_json(entry);
    }
    let mut data = json!({
        "month": read.month,
        "months": read.months,
        "by": if by_source { "source" } else { "dataset" },
    });
    data[if by_source { "sources" } else { "datasets" }] = Value::Array(counted);
    json!({"outcome": "unchanged", "data": data})
}

async fn answer(app: &App, caller: &Caller, arguments: &Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(arguments, "", &mut problems) else {
        return problems.refusal();
    };
    only(
        top,
        &["test", "answer", "subject", "note"],
        "",
        &mut problems,
    );
    let test = text(top, "test", "", false, &mut problems);
    let said = text(top, "answer", "", true, &mut problems);
    let subject = text(top, "subject", "", false, &mut problems);
    let note = text(top, "note", "", true, &mut problems);
    if !said.is_empty() && Status::parse(&said).is_none() {
        problems.add("answer", "professional or non_professional");
    }
    if note.trim().is_empty() && !problems.is_empty() {
        // Already named as required.
    } else if note.trim().is_empty() {
        problems.add("note", "say why");
    }
    if !problems.is_empty() {
        return problems.refusal();
    }
    let records = match app.records.current(app.clock.now_ns()) {
        Ok(records) => records,
        Err(stale) => return refused("unavailable", &stale.to_string(), Vec::new()),
    };
    if !caller.owner_subject.is_empty()
        || crate::service_accounts::is_service_account(&caller.subject)
    {
        return refused(
            "not_permitted",
            "a service account has no professional status",
            Vec::new(),
        );
    }
    let test = if test.is_empty() {
        STANDARD.to_string()
    } else {
        test
    };
    let by = (
        caller.subject.as_str(),
        caller.delegation_id.as_str(),
        caller.client_name.as_str(),
    );
    let change = if !subject.is_empty() && subject != caller.subject {
        if !caller.access(&records).deployment_admin {
            return refused(
                "not_permitted",
                "changing another person's status is a deployment admin's",
                vec![json!({"path": "subject", "message": "your own, or none"})],
            );
        }
        match crate::data_use::pages::admin_change(app, &records, &subject, &test, &said, &note, by)
            .await
        {
            Ok(change) => change,
            Err((path, why)) => {
                return refused(
                    "invalid_arguments",
                    &format!("{path}: {why}"),
                    vec![json!({"path": path, "message": why})],
                )
            }
        }
    } else {
        let tests = data_use::tests_for(&records, &caller.subject, &caller.directory_groups).await;
        if !tests.contains(&test) {
            return refused(
                "invalid_arguments",
                &format!("test: no data you can read asks the test {test}"),
                vec![json!({"path": "test", "message": "a test the data you can read asks"})],
            );
        }
        match data_use::answers().await {
            Ok(answers) if answers.contains_key(&(caller.subject.clone(), test.clone())) => {
                return refused(
                    "already_answered",
                    "you have answered this test already; a deployment admin can change it",
                    vec![json!({"path": "test", "message": "answered already"})],
                )
            }
            Ok(_) => {}
            Err(why) => return refused("unavailable", &why, Vec::new()),
        }
        let Some(status) = Status::parse(&said) else {
            return refused(
                "invalid_arguments",
                "answer: professional or non_professional",
                Vec::new(),
            );
        };
        match data_use::answer(
            Answering {
                subject: &caller.subject,
                test: &test,
                status,
                by: by.0,
                through_delegation: by.1,
                client_name: by.2,
                note: &note,
            },
            app.clock.now_ns(),
        )
        .await
        {
            Ok(change) => change,
            Err(why) => return refused("invalid_arguments", &format!("note: {why}"), Vec::new()),
        }
    };
    json!({"outcome": "made", "data": {"record": {
        "kind": "professional_status_set",
        "subject": change.subject,
        "test": change.test,
        "from": change.from.map(Status::value),
        "to": change.to.value(),
        "by": change.by,
        "through_delegation": change.through_delegation,
        "client_name": change.client_name,
        "at_ns": change.at_ns.to_string(),
        "note": change.note,
    }}})
}

/// The routes and tab the Use tab and the test draw, each against the tool
/// that reaches it (W6.20). [`tests`] fails the build where one has none.
pub const PARITY: &[(&str, &str)] = &[
    ("tab use", "read_data_use export_data_use"),
    ("/admin/data-sources/use.csv", "export_data_use"),
    (
        "/admin/data-sources/professional-status",
        "answer_professional_test",
    ),
    ("/professional-test", "answer_professional_test"),
];

#[cfg(test)]
mod tests;
