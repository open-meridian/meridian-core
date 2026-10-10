//! The Data sources page (W10.1, W10.2, contract v18; plans/the-lake-prices-
//! the-book, Q29): a deployment admin's, beside Instruments on Settings.
//!
//! Three tabs, each a table of one-line rows on the kit's pager, so the page
//! fits one screen at a desk and on a phone (the one-screen rule):
//!
//! - **Datasets**: each dataset a launched `dgm`'s catalogue declares, with
//!   who originated and carried it, what it serves, the licence enforced --
//!   the deployment's, or the catalogue's default until one is set -- the
//!   plugins entitled to it, and the lake's counts of values left
//!   unconverted and of identifiers and venues its instance reported
//!   missing. A dataset whose terms are one person's (`personal_use`) is
//!   flagged where more than one person holds read on a plugin entitled to
//!   it (Q16): derived here from the records, with no field of its own.
//! - **Entitlements**: each plugin instance entitled to a dataset, all
//!   fields or some, withdrawn from its row.
//! - **Priority**: the ordered datasets a default read takes for each data
//!   type and kind, replaced whole against the priority as it was read (the
//!   stale guard), with who set it and when.
//!
//! The listings are the lake's (`ListDatasets`, `ListSourcePriorities`),
//! read as core reads them, whole; a licence and an entitlement are the
//! conductor's, a priority the lake's. The page shows no price: prices are
//! the reading roles'. A licence records what the admin entered; nothing
//! here says a deployment meets a vendor's terms. Every act here has its
//! tool ([`crate::mcp::data_sources`]), and a tool's answer is what this
//! page shows the same person (Q30): both read [`rows`].

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use meridian_access::person_access;
use meridian_bus::BusError;
use meridian_domain::v1::{
    AccessRecords, DatasetEntitlement, DatasetRef, ListDatasetsReply, ListDatasetsRequest,
    ListSourcePrioritiesReply, ListSourcePrioritiesRequest, PriceKind, SourcePriority,
};
use meridian_domain::{lake, thousands};
use meridian_pb::v1::DatasetLicence;
use prost::Message;

use crate::html::escape;

pub const LIST_DATASETS: &str = "platform.lake.query.list-datasets";
pub const LIST_SOURCE_PRIORITIES: &str = "platform.lake.query.list-source-priorities";
pub const SET_SOURCE_PRIORITY: &str = "platform.lake.command.set-source-priority";
pub const SET_DATASET_LICENCE: &str = "platform.config.command.set-dataset-licence";
pub const SET_DATASET_ENTITLEMENT: &str = "platform.config.command.set-dataset-entitlement";

const WAIT: Duration = Duration::from_secs(5);

/// The page's tabs, in the order an admin reaches for them.
pub const TABS: [(&str, &str); 3] = [
    ("datasets", "Datasets"),
    ("entitlements", "Entitlements"),
    ("priority", "Priority"),
];

async fn read<R: Message + Default>(
    bus: &meridian_bus::Bus,
    subject: &str,
    topic: &str,
    request_type: &str,
    request: impl Message,
) -> Result<R, String> {
    let (_, bytes) = bus
        .call_for(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            Some(WAIT),
            subject,
        )
        .await
        .map_err(|failed| match failed {
            BusError::HandlerFailed { detail, .. } => detail,
            other => other.to_string(),
        })?;
    R::decode(&bytes[..]).map_err(|failed| format!("an undecodable reply: {failed}"))
}

/// Every dataset with its licence and entitlements, as the lake answers
/// core.
pub async fn datasets(bus: &meridian_bus::Bus, subject: &str) -> Result<ListDatasetsReply, String> {
    read(
        bus,
        subject,
        LIST_DATASETS,
        "meridian.v1.ListDatasetsRequest",
        ListDatasetsRequest {},
    )
    .await
}

/// Every priority, with who set each and when.
pub async fn priorities(
    bus: &meridian_bus::Bus,
    subject: &str,
) -> Result<ListSourcePrioritiesReply, String> {
    read(
        bus,
        subject,
        LIST_SOURCE_PRIORITIES,
        "meridian.v1.ListSourcePrioritiesRequest",
        ListSourcePrioritiesRequest {},
    )
    .await
}

/// One dataset as the page and `dashboard__list_datasets` both draw it.
#[derive(Debug, Clone)]
pub struct Row {
    pub dataset: DatasetRef,
    /// The licence enforced.
    pub licence: DatasetLicence,
    /// Whether the deployment set it, rather than the catalogue's default.
    pub licence_set: bool,
    /// The instances entitled to it, allowed now.
    pub entitled: Vec<DatasetEntitlement>,
    /// Who holds read on a plugin entitled to it, by subject, when its terms
    /// are one person's and more than one person does (Q16); empty
    /// otherwise.
    pub one_person_readers: Vec<String>,
}

impl Row {
    /// The one-person warning, in words, or none.
    pub fn warning(&self, records: &AccessRecords) -> Option<String> {
        if self.one_person_readers.is_empty() {
            return None;
        }
        let names: Vec<String> = self
            .one_person_readers
            .iter()
            .map(|subject| crate::tickets::display_name(records, subject))
            .collect();
        Some(format!(
            "its terms are one person's, and {} people hold read on a plugin entitled to it: {}",
            names.len(),
            names.join(", ")
        ))
    }
}

/// Who holds read on `instance`, by subject, as the records say now: each
/// person who signed in, and each login a user group names, signed in yet
/// or not, whose data level on it is read or write, whatever accounts it
/// reaches. A directory group's members are known only once each has
/// signed in.
fn readers_of(records: &AccessRecords, instance: &str) -> BTreeSet<String> {
    let signed_in = records
        .people
        .iter()
        .map(|person| (person.subject.clone(), person.directory_groups.clone()));
    let named = records
        .user_groups
        .iter()
        .flat_map(|group| group.logins.iter().map(|login| (login.clone(), Vec::new())));
    let mut candidates: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (subject, groups) in named.chain(signed_in) {
        candidates.insert(subject, groups);
    }
    candidates
        .into_iter()
        .filter(|(subject, groups)| {
            person_access(records, subject, groups)
                .plugins
                .get(instance)
                .is_some_and(|held| held.union().data.is_some())
        })
        .map(|(subject, _)| subject)
        .collect()
}

/// Each dataset the lake listed, joined to its licence, its entitlements
/// and the one-person warning.
pub fn rows(reply: &ListDatasetsReply, records: &AccessRecords) -> Vec<Row> {
    let mut rows: Vec<Row> = reply
        .datasets
        .iter()
        .map(|dataset| {
            let set = reply.licences.iter().find(|l| l.dataset == dataset.dataset);
            let licence =
                lake::effective_licence(&dataset.dataset, set, dataset.declaration.as_ref());
            let entitled: Vec<DatasetEntitlement> = reply
                .entitlements
                .iter()
                .filter(|e| e.dataset == dataset.dataset && e.allowed)
                .cloned()
                .collect();
            let one_person_readers = if licence.personal_use {
                let readers: BTreeSet<String> = entitled
                    .iter()
                    .flat_map(|e| readers_of(records, &e.instance))
                    .collect();
                if readers.len() > 1 {
                    readers.into_iter().collect()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            Row {
                dataset: dataset.clone(),
                licence,
                licence_set: set.is_some(),
                entitled,
                one_person_readers,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.dataset.dataset.cmp(&b.dataset.dataset));
    rows
}

/// A data type in a word: Price, Bar.
pub fn type_word(data_type: &str) -> &str {
    data_type.strip_prefix("meridian.v1.").unwrap_or(data_type)
}

/// A price kind in its word, as the tools take it: `close`, `last`, `nav`,
/// `settlement`; empty for none.
pub fn kind_word(kind: i32) -> &'static str {
    match PriceKind::try_from(kind) {
        Ok(PriceKind::Close) => "close",
        Ok(PriceKind::Last) => "last",
        Ok(PriceKind::Nav) => "nav",
        Ok(PriceKind::Settlement) => "settlement",
        _ => "",
    }
}

/// A price kind from its word, or none.
pub fn kind_of(word: &str) -> Option<PriceKind> {
    match word {
        "close" => Some(PriceKind::Close),
        "last" => Some(PriceKind::Last),
        "nav" => Some(PriceKind::Nav),
        "settlement" => Some(PriceKind::Settlement),
        _ => None,
    }
}

/// The licence in a few words: kept and for how long, or served not kept.
fn licence_said(licence: &DatasetLicence) -> String {
    if !licence.kept {
        return "served, not kept".into();
    }
    if licence.retention_days == 0 {
        "kept".into()
    } else {
        format!("kept {} days", thousands(u64::from(licence.retention_days)))
    }
}

fn when(ns: i64) -> (String, String) {
    let at = crate::custody::utc(ns);
    let (day, time) = at.split_once(' ').unwrap_or((at.as_str(), ""));
    (day.to_string(), time.to_string())
}

fn datasets_tab(rows: &[Row], records: &AccessRecords, token: &str) -> (String, String) {
    let lines: String = rows
        .iter()
        .map(|row| {
            let d = &row.dataset;
            let declaration = d.declaration.clone().unwrap_or_default();
            let licence = &row.licence;
            let fill = serde_json::json!({
                "fields": {
                    "dataset": d.dataset,
                    "retention_days": licence.retention_days.to_string(),
                    "default_fields": licence.default_fields.join(", "),
                    "note": "",
                },
                "checked": {
                    "kept": if licence.kept { vec!["1"] } else { vec![] },
                    "derived_use": if licence.derived_use { vec!["1"] } else { vec![] },
                    "display": if licence.display { vec!["1"] } else { vec![] },
                    "personal_use": if licence.personal_use { vec!["1"] } else { vec![] },
                },
            });
            let entitle = serde_json::json!({"fields": {"dataset": d.dataset, "instance": "", "fields": "", "note": ""},
                                             "checked": {"allowed": ["1"]}});
            let vendor = if d.vendor.is_empty() {
                declaration.vendor.clone()
            } else {
                d.vendor.clone()
            };
            let aggregator = if d.aggregator.is_empty() {
                declaration.aggregator.clone()
            } else {
                d.aggregator.clone()
            };
            let carried = if aggregator.is_empty() {
                vendor.clone()
            } else {
                format!("{vendor} via {aggregator}")
            };
            let serves: Vec<&str> = lake::types_served(&declaration)
                .into_iter()
                .map(type_word)
                .collect();
            let modes: Vec<&str> = declaration
                .modes
                .iter()
                .map(|m| crate::declaration::mode_word(*m))
                .collect();
            let entitled: Vec<&str> = row.entitled.iter().map(|e| e.instance.as_str()).collect();
            let warning = row.warning(records);
            let badge = match &warning {
                Some(said) => format!(
                    " <span class=\"badge warn\" title=\"{}\" data-one-person>one person's</span>",
                    escape(said)
                ),
                None => String::new(),
            };
            let source = if row.licence_set {
                format!(
                    "set by {}",
                    crate::tickets::display_name(records, &licence.updated_by)
                )
            } else {
                "the catalogue's default".into()
            };
            format!(
                "<tr data-id=\"{id}\"><td title=\"{id}\"><button type=\"button\" class=\"link\" \
                 data-dialog-open=\"licence\" data-title=\"Licence {id}\" data-fill=\"{fill}\">{id}</button></td>\
                 <td class=\"wide\" title=\"{carried}\">{carried}</td>\
                 <td class=\"wide\" title=\"{serves}; {modes}\">{serves}</td>\
                 <td title=\"{licence_said}, {source}\">{licence_said}{badge}</td>\
                 <td class=\"wide\" title=\"{entitled_all}\" data-entitled=\"{n}\">{n}</td>\
                 <td class=\"wide num\" data-unconverted>{unconverted}</td>\
                 <td class=\"wide num\" data-misses>{misses}</td>\
                 <td class=\"actions\"><button type=\"button\" class=\"wide\" data-dialog-open=\"licence\" \
                 data-title=\"Licence {id}\" data-fill=\"{fill}\">Licence</button> \
                 <button type=\"button\" data-dialog-open=\"entitle\" data-title=\"Entitle a plugin to {id}\" \
                 data-fill=\"{entitle}\">Entitle</button></td></tr>",
                id = escape(&d.dataset),
                fill = escape(&fill.to_string()),
                entitle = escape(&entitle.to_string()),
                carried = escape(&crate::mcp::others_words(&carried)),
                serves = escape(&serves.join(", ")),
                modes = escape(&modes.join(", ")),
                licence_said = escape(&licence_said(licence)),
                source = escape(&source),
                entitled_all = if entitled.is_empty() {
                    "no plugin".to_string()
                } else {
                    escape(&entitled.join(", "))
                },
                n = entitled.len(),
                unconverted = thousands(d.unconverted_count),
                misses = thousands(d.miss_count),
            )
        })
        .collect();
    let body = if lines.is_empty() {
        "<p class=\"empty\" data-datasets=\"0\">No dataset yet: a launched plugin holding \
         <code>dgm</code> declares its datasets in its catalogue, and each is listed here.</p>"
            .to_string()
    } else {
        format!(
            "<om-pager><table class=\"list one-line datasets\" data-datasets=\"{n}\"><thead><tr>\
             <th>Dataset</th><th class=\"wide\">Vendor</th><th class=\"wide\">Serves</th>\
             <th>Licence</th><th class=\"wide count\" title=\"Plugins entitled\">Entitled</th>\
             <th class=\"wide num\" title=\"Values left unconverted\">Unconverted</th>\
             <th class=\"wide num\" title=\"Identifiers and venues reported missing\">Misses</th>\
             <th class=\"actions\"></th></tr></thead><tbody>{lines}</tbody></table></om-pager>",
            n = rows.len()
        )
    };
    let dialogs = format!(
        "<dialog id=\"licence\" aria-labelledby=\"licence-title\"><form method=\"post\" \
         action=\"/admin/data-sources/licence\">{token}\
         <div class=\"dialog-head\"><h2 id=\"licence-title\" data-title-new=\"Licence a dataset\">Licence a dataset</h2></div>\
         <div class=\"dialog-body\"><input type=\"hidden\" name=\"dataset\">\
         <label class=\"check\"><input type=\"checkbox\" name=\"kept\" value=\"1\"> The lake may keep its rows \
         (otherwise served, not kept)</label>\
         <label>Kept for, in days <input name=\"retention_days\" type=\"number\" min=\"0\" max=\"36500\" \
         step=\"1\" inputmode=\"numeric\" required></label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"derived_use\" value=\"1\"> Derived data may be made</label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"display\" value=\"1\"> It may be shown</label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"personal_use\" value=\"1\"> Its terms are one person's</label>\
         <label>Fields readable by default <input name=\"default_fields\" \
         placeholder=\"Every field; or meridian.v1.Bar.vwap, ...\"></label>\
         <label>Note <input name=\"note\" maxlength=\"2000\" placeholder=\"Why\"></label>\
         <p class=\"hint\">0 days keeps rows with no limit set. This records what you enter; it \
         does not say the deployment meets the vendor's terms.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\" data-label-new=\"Save\">Save</button></div></form></dialog>\
         <dialog id=\"entitle\" aria-labelledby=\"entitle-title\"><form method=\"post\" \
         action=\"/admin/data-sources/entitlement\">{token}\
         <div class=\"dialog-head\"><h2 id=\"entitle-title\" data-title-new=\"Entitle a plugin\">Entitle a plugin</h2></div>\
         <div class=\"dialog-body\"><input type=\"hidden\" name=\"dataset\">\
         <label>Plugin instance <input name=\"instance\" list=\"instances\" required autocomplete=\"off\"></label>\
         <datalist id=\"instances\">{instances}</datalist>\
         <label class=\"check\"><input type=\"checkbox\" name=\"allowed\" value=\"1\"> It may read the dataset</label>\
         <label>Fields <input name=\"fields\" placeholder=\"Every field; or meridian.v1.Price.price, ...\"></label>\
         <label>Note <input name=\"note\" maxlength=\"2000\" placeholder=\"Why\"></label></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\" data-label-new=\"Save\">Save</button></div></form></dialog>",
        instances = records
            .known_plugins
            .iter()
            .map(|p| format!("<option value=\"{}\">", escape(&p.plugin_instance_id)))
            .collect::<String>(),
    );
    (body, dialogs)
}

fn entitlements_tab(rows: &[Row], records: &AccessRecords, token: &str) -> String {
    let lines: String = rows
        .iter()
        .flat_map(|row| row.entitled.iter())
        .map(|e| {
            let (day, time) = when(e.updated_at_ns);
            let fields = if e.fields.is_empty() {
                "every field".to_string()
            } else {
                e.fields.join(", ")
            };
            let by = crate::tickets::display_name(records, &e.updated_by);
            let fill = serde_json::json!({
                "fields": {"dataset": e.dataset, "instance": e.instance, "fields": e.fields.join(", "), "note": ""},
                "checked": {"allowed": ["1"]},
            });
            format!(
                "<tr data-id=\"{dataset} {instance}\"><td title=\"{dataset}\">{dataset}</td>\
                 <td title=\"{instance}\">{instance}</td><td class=\"wide\" title=\"{fields}\">{fields}</td>\
                 <td class=\"wide\" title=\"{note}\">{by}</td><td title=\"{day} {time} by {by}\">{day}\
                 <span class=\"dates\"> {time}</span></td>\
                 <td class=\"actions\"><button type=\"button\" class=\"wide\" data-dialog-open=\"entitle\" \
                 data-title=\"Change the entitlement\" data-fill=\"{fill}\">Edit</button> \
                 <form method=\"post\" action=\"/admin/data-sources/entitlement#entitlements\" class=\"inline\" \
                 data-confirm=\"Withdraw {instance}'s entitlement to {dataset}? Its deliveries stop.\">{token}\
                 <input type=\"hidden\" name=\"dataset\" value=\"{dataset}\">\
                 <input type=\"hidden\" name=\"instance\" value=\"{instance}\">\
                 <button type=\"submit\">Withdraw</button></form></td></tr>",
                dataset = escape(&e.dataset),
                instance = escape(&e.instance),
                fields = escape(&fields),
                note = escape(&crate::mcp::others_words(&e.note)),
                by = escape(&by),
                day = escape(&day),
                time = escape(&time),
                fill = escape(&fill.to_string()),
            )
        })
        .collect();
    if lines.is_empty() {
        return "<p class=\"empty\" data-entitlements=\"0\">No plugin is entitled to a dataset: \
                Entitle, on a dataset's row, lets a plugin read it.</p>"
            .to_string();
    }
    format!(
        "<om-pager><table class=\"list one-line entitlements\" data-entitlements=\"{n}\"><thead><tr>\
         <th>Dataset</th><th>Plugin</th><th class=\"wide\">Fields</th><th class=\"wide\">Set by</th>\
         <th>Set</th><th class=\"actions\"></th></tr></thead><tbody>{lines}</tbody></table></om-pager>",
        n = rows.iter().map(|r| r.entitled.len()).sum::<usize>()
    )
}

fn priority_tab(
    priorities: &[SourcePriority],
    rows: &[Row],
    records: &AccessRecords,
    token: &str,
) -> (String, String) {
    let lines: String = priorities
        .iter()
        .map(|p| {
            let kind = kind_word(p.kind);
            let what = if kind.is_empty() {
                type_word(&p.data_type).to_string()
            } else {
                format!("{}, {kind}", type_word(&p.data_type))
            };
            let (day, time) = when(p.updated_at_ns);
            let by = crate::tickets::display_name(records, &p.updated_by);
            let fill = serde_json::json!({
                "fields": {
                    "data_type": p.data_type,
                    "kind": kind,
                    "datasets": p.datasets.join("\n"),
                    "against_updated_at_ns": p.updated_at_ns.to_string(),
                    "note": "",
                },
            });
            format!(
                "<tr data-id=\"{what}\"><td title=\"{what}\"><button type=\"button\" class=\"link\" \
                 data-dialog-open=\"priority-dialog\" data-title=\"Change the priority\" data-fill=\"{fill}\">{what}</button></td>\
                 <td title=\"{order}\">{order}</td><td class=\"wide\" title=\"{note}\">{by}</td>\
                 <td title=\"{day} {time} by {by}\">{day}<span class=\"dates\"> {time}</span></td>\
                 <td class=\"actions\"><button type=\"button\" data-dialog-open=\"priority-dialog\" \
                 data-title=\"Change the priority\" data-fill=\"{fill}\">Edit</button></td></tr>",
                what = escape(&what),
                order = escape(&p.datasets.join(" then ")),
                note = escape(&crate::mcp::others_words(&p.note)),
                by = escape(&by),
                day = escape(&day),
                time = escape(&time),
                fill = escape(&fill.to_string()),
            )
        })
        .collect();
    let body = if lines.is_empty() {
        "<p class=\"empty\" data-priorities=\"0\">No priority is set: a default read takes the \
         datasets in the order the lake lists them.</p>"
            .to_string()
    } else {
        format!(
            "<om-pager><table class=\"list one-line priorities\" data-priorities=\"{n}\"><thead><tr>\
             <th>For</th><th>Datasets, first to last</th><th class=\"wide\">Set by</th><th>Set</th>\
             <th class=\"actions\"></th></tr></thead><tbody>{lines}</tbody></table></om-pager>",
            n = priorities.len()
        )
    };
    let ids: Vec<String> = rows.iter().map(|r| r.dataset.dataset.clone()).collect();
    let known = if ids.is_empty() {
        String::new()
    } else {
        format!(": {}", escape(&ids.join(", ")))
    };
    let dialog = format!(
        "<dialog id=\"priority-dialog\" aria-labelledby=\"priority-dialog-title\"><form method=\"post\" \
         action=\"/admin/data-sources/priority#priority\">{token}\
         <div class=\"dialog-head\"><h2 id=\"priority-dialog-title\" data-title-new=\"Set a priority\">Set a priority</h2></div>\
         <div class=\"dialog-body\"><input type=\"hidden\" name=\"against_updated_at_ns\" value=\"0\">\
         <label>Data type <select name=\"data_type\"><option value=\"{price}\">Price</option>\
         <option value=\"{bar}\">Bar</option></select></label>\
         <label>Kind, for prices <select name=\"kind\"><option value=\"\">None (bars)</option>\
         <option value=\"close\">Close</option><option value=\"last\">Last</option>\
         <option value=\"nav\">NAV</option><option value=\"settlement\">Settlement</option></select></label>\
         <label>Datasets, first to last, one a line <textarea name=\"datasets\" rows=\"3\" required></textarea></label>\
         <label>Note <input name=\"note\" maxlength=\"2000\" placeholder=\"Why\"></label>\
         <p class=\"hint\">At most 16, each declared for the data type{known}. Replaced whole; \
         refused if it changed since you read it.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" class=\"primary\" data-label-new=\"Set\">Set</button></div></form></dialog>",
        price = lake::PRICE,
        bar = lake::BAR,
    );
    (body, dialog)
}

/// A tab shown measures its pager again, as on a resize: a pager measured
/// while its tab was hidden pages nothing.
const PAGER_SCRIPT: &str = r#"
(function () {
  function measure() { window.setTimeout(function () { window.dispatchEvent(new Event("resize")); }, 0); }
  window.addEventListener("hashchange", measure);
  document.addEventListener("click", function (event) {
    if (event.target.closest("nav.tabs a")) measure();
  });
  measure();
})();"#;

fn section(id: &str, title: &str, about: &str, action: &str, body: &str) -> String {
    format!(
        "<section class=\"admin-section\" id=\"{id}\">\
         <div class=\"section-head\"><div><h2>{title}</h2><p class=\"hint\">{about}</p></div>{action}</div>\
         {body}</section>"
    )
}

/// The page whole: its tabs, their tables and dialogs, and the script that
/// shows one tab at a time. `unread` says what could not be read, in place
/// of a table.
pub fn page(
    listed: &Result<ListDatasetsReply, String>,
    priorities: &Result<ListSourcePrioritiesReply, String>,
    records: &AccessRecords,
    token: &str,
    notice: &str,
) -> String {
    let unread = |why: &str| {
        format!(
            "<p class=\"refused\">The lake did not answer: {}</p>",
            escape(why)
        )
    };
    let rows = match listed {
        Ok(reply) => rows(reply, records),
        Err(_) => Vec::new(),
    };
    let (datasets_body, datasets_dialogs, entitlements_body) = match listed {
        Ok(_) => {
            let (body, dialogs) = datasets_tab(&rows, records, token);
            (body, dialogs, entitlements_tab(&rows, records, token))
        }
        Err(why) => (unread(why), String::new(), unread(why)),
    };
    let (priority_body, priority_dialog) = match priorities {
        Ok(reply) => priority_tab(&reply.priorities, &rows, records, token),
        Err(why) => (unread(why), String::new()),
    };
    let tabs: String = TABS
        .iter()
        .map(|(id, title)| format!("<a href=\"#{id}\" data-tab=\"{id}\">{title}</a>"))
        .collect();
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"passed\">{}</p>", escape(notice))
    };
    let add = "<button type=\"button\" class=\"primary\" data-dialog-open=\"priority-dialog\" \
               aria-label=\"Add a priority\">+ Add</button>";
    format!(
        "<div class=\"admin\"><div class=\"page-head\"><h1>Data sources</h1></div>{notice}\
         <nav class=\"tabs\">{tabs}</nav>{}{}{}{datasets_dialogs}{priority_dialog}</div>\
         <script>{}{}{PAGER_SCRIPT}</script>",
        section(
            "datasets",
            "Datasets",
            "What each launched plugin's catalogue serves the lake, on the terms enforced: the \
             deployment's licence, or the catalogue's default until one is set. Prices are read \
             by the plugins entitled to them, never shown here.",
            "",
            &datasets_body
        ),
        section(
            "entitlements",
            "Entitlements",
            "The plugins that may read each dataset, all its fields or some.",
            "",
            &entitlements_body
        ),
        section(
            "priority",
            "Priority",
            "The datasets a default read takes for a data type and kind, first to last, failing \
             over when one is silent.",
            add,
            &priority_body
        ),
        super::overview::SCRIPT_START,
        super::overview::SCRIPT_END,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_pb::v1::{DatasetDeclaration, ObservationMode};

    fn listed(personal_use: bool) -> ListDatasetsReply {
        ListDatasetsReply {
            datasets: vec![DatasetRef {
                dataset: "coinbase-1:daily".into(),
                instance: "coinbase-1".into(),
                vendor: "Coinbase".into(),
                declaration: Some(DatasetDeclaration {
                    key: "daily".into(),
                    vendor: "Coinbase".into(),
                    data_types: vec![lake::PRICE.into(), lake::BAR.into()],
                    modes: vec![ObservationMode::Pull as i32],
                    licence_default: Some(DatasetLicence {
                        kept: true,
                        personal_use,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                unconverted_count: 2,
                miss_count: 1,
                ..Default::default()
            }],
            licences: vec![],
            entitlements: vec![DatasetEntitlement {
                dataset: "coinbase-1:daily".into(),
                instance: crate::tickets::tests::OPS.into(),
                allowed: true,
                updated_by: "ada@example.com".into(),
                updated_at_ns: 1_791_417_600_000_000_000,
                ..Default::default()
            }],
        }
    }

    /// Two people holding read on the entitled plugin (Ada writes, Ben
    /// reads, on ops-1), and one administering it alone.
    fn two_readers() -> AccessRecords {
        crate::tickets::tests::records()
    }

    #[test]
    fn each_dataset_is_one_line_with_its_counts_and_the_default_licence() {
        let records = AccessRecords::default();
        let reply = listed(false);
        let body = page(
            &Ok(reply),
            &Ok(ListSourcePrioritiesReply::default()),
            &records,
            "<t>",
            "",
        );
        assert!(body.contains("data-datasets=\"1\""), "{body}");
        assert!(body.contains("<td class=\"wide num\" data-unconverted>2</td>"));
        assert!(body.contains("<td class=\"wide num\" data-misses>1</td>"));
        assert!(
            body.contains("the catalogue&#39;s default")
                || body.contains("the catalogue's default")
        );
        assert!(body.contains("data-entitled=\"1\""));
        assert!(body.contains("data-entitlements=\"1\""));
        assert!(body.contains("No priority is set"));
        assert!(!body.contains("data-one-person"));
        for (id, _) in TABS {
            assert!(body.contains(&format!("id=\"{id}\"")), "{id}");
        }
    }

    #[test]
    fn one_person_s_terms_read_by_two_people_are_flagged() {
        let records = two_readers();
        let rows_one = rows(&listed(true), &records);
        assert_eq!(
            rows_one[0].one_person_readers,
            vec![
                crate::tickets::tests::ADA.to_string(),
                crate::tickets::tests::BEN.to_string()
            ]
        );
        assert!(rows_one[0].warning(&records).unwrap().contains("2 people"));
        let body = page(
            &Ok(listed(true)),
            &Ok(ListSourcePrioritiesReply::default()),
            &records,
            "<t>",
            "",
        );
        assert!(body.contains("data-one-person"), "{body}");
        // Not one person's terms: no warning however many read.
        assert!(rows(&listed(false), &records)[0]
            .one_person_readers
            .is_empty());
    }

    #[test]
    fn a_priority_s_edit_carries_what_it_was_read_at() {
        let priorities = ListSourcePrioritiesReply {
            priorities: vec![SourcePriority {
                data_type: lake::PRICE.into(),
                kind: PriceKind::Close as i32,
                datasets: vec!["a-1:daily".into(), "b-1:daily".into()],
                updated_at_ns: 42,
                ..Default::default()
            }],
        };
        let body = page(
            &Ok(ListDatasetsReply::default()),
            &Ok(priorities),
            &AccessRecords::default(),
            "<t>",
            "",
        );
        assert!(body.contains("Price, close"));
        assert!(body.contains("a-1:daily then b-1:daily"));
        assert!(
            body.contains("against_updated_at_ns&quot;:&quot;42"),
            "{body}"
        );
        assert!(body.contains("No dataset yet"));
    }

    #[test]
    fn a_lake_not_answering_says_so_in_place_of_the_tables() {
        let body = page(
            &Err("timed out".into()),
            &Err("timed out".into()),
            &AccessRecords::default(),
            "<t>",
            "",
        );
        assert!(body.contains("The lake did not answer: timed out"));
    }
}
