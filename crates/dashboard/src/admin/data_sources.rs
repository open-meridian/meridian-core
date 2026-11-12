//! The Data sources page (W10.1, W10.2, contract v18; plans/the-lake-prices-
//! the-book, Q29): a deployment admin's, beside Instruments on Settings.
//!
//! Three tabs, each a table of one-line rows on the kit's pager, so the page
//! fits one screen at a desk and on a phone (the one-screen rule):
//!
//! - **Datasets**: each dataset a launched data plugin's catalogue declares, with
//!   who originated and carried it and, from contract 28 as amended
//!   2026-10-31 (rulings 896 and 906), the source behind the vendor or whose
//!   aggregate it serves and the class of source its prices come from, what
//!   it serves, the licence enforced --
//!   the deployment's, or until one is set the dataset kept and read with
//!   each use right restrictive (ruling 1030; step 3c's question 6) -- the
//!   plugins entitled to it, and the lake's counts of values left
//!   unconverted and of identifiers and venues its instance reported
//!   missing, and from contract v23 of its daily values dated on a closed
//!   day of its calendar, those outside the calendar's coverage beside
//!   them, apart (W10.9). A dataset whose terms are one person's (`personal_use`) is
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
    AccessRecords, DatasetEntitlement, DatasetGap, DatasetRef, GapCause, GapState,
    ListDatasetGapsReply, ListDatasetGapsRequest, ListDatasetsReply, ListDatasetsRequest,
    ListSourcePrioritiesReply, ListSourcePrioritiesRequest, ListSourceSwitchesReply,
    ListSourceSwitchesRequest, PriceKind, SourcePriority, SourceSwitchReason, SourceSwitched,
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
/// Contract v30 (W10.6, W10.14): the source switches and the gaps, the lake's.
pub const LIST_SOURCE_SWITCHES: &str = "platform.lake.query.list-source-switches";
pub const LIST_DATASET_GAPS: &str = "platform.lake.query.list-dataset-gaps";

/// The words a default read standing on a backup is shown with (the
/// resilience spec's section 8).
pub const SWITCHED: &str = "switched to backup source";

const WAIT: Duration = Duration::from_secs(5);

/// The page's tabs, in the order an admin reaches for them.
pub const TABS: [(&str, &str); 6] = [
    ("datasets", "Datasets"),
    ("entitlements", "Entitlements"),
    ("priority", "Priority"),
    // Contract v28 (W10.9): the monitor's data age and session latency.
    ("age", "Age"),
    // Contract v28 (W10.13; 754): who could use each dataset this month,
    // against its meters ([`crate::data_use`]).
    ("use", "Use"),
    // Contract v30 (W10.14; the resilience spec's section 4): a line per
    // gap not recovered, the unattributed one's dataset not yet known.
    ("gaps", "Gaps"),
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

/// A page of the lake's gaps, each in its latest state, newest first
/// (W10.14): of `dataset` where named, in `state` where not 0.
pub async fn gaps(
    bus: &meridian_bus::Bus,
    subject: &str,
    dataset: &str,
    state: i32,
    page_size: u32,
    cursor: &str,
) -> Result<ListDatasetGapsReply, String> {
    read(
        bus,
        subject,
        LIST_DATASET_GAPS,
        "meridian.v1.ListDatasetGapsRequest",
        ListDatasetGapsRequest {
            dataset: dataset.to_string(),
            state,
            page_size,
            cursor: cursor.to_string(),
        },
    )
    .await
}

/// A page of the source switches, newest first (W10.6): of `data_type`
/// and of `dataset` (from or to) where named.
pub async fn switches(
    bus: &meridian_bus::Bus,
    subject: &str,
    data_type: &str,
    dataset: &str,
    page_size: u32,
    cursor: &str,
) -> Result<ListSourceSwitchesReply, String> {
    read(
        bus,
        subject,
        LIST_SOURCE_SWITCHES,
        "meridian.v1.ListSourceSwitchesRequest",
        ListSourceSwitchesRequest {
            data_type: data_type.to_string(),
            dataset: dataset.to_string(),
            page_size,
            cursor: cursor.to_string(),
        },
    )
    .await
}

/// The most pages of a lake listing the page reads, the lake's largest each.
const PAGES_MOST: usize = 4;

/// Every gap not recovered, newest first: what Data sources draws a line
/// for, the unattributed one among them.
pub async fn open_gaps(bus: &meridian_bus::Bus, subject: &str) -> Result<Vec<DatasetGap>, String> {
    let mut open = Vec::new();
    let mut cursor = String::new();
    for _ in 0..PAGES_MOST {
        let page = gaps(bus, subject, "", 0, 500, &cursor).await?;
        open.extend(
            page.gaps
                .into_iter()
                .filter(|gap| gap.state != GapState::Recovered as i32),
        );
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    Ok(open)
}

/// Each default read standing on a backup now: the latest switch of each
/// data type and kind, where it was not a switch back (the resilience
/// spec's section 8).
pub async fn standing(
    bus: &meridian_bus::Bus,
    subject: &str,
) -> Result<Vec<SourceSwitched>, String> {
    let mut read_all = Vec::new();
    let mut cursor = String::new();
    for _ in 0..PAGES_MOST {
        let page = switches(bus, subject, "", "", 500, &cursor).await?;
        read_all.extend(page.switches);
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    Ok(standing_of(&read_all))
}

/// [`standing`] from the switches, newest first.
pub fn standing_of(newest_first: &[SourceSwitched]) -> Vec<SourceSwitched> {
    let mut seen = BTreeSet::new();
    newest_first
        .iter()
        .filter(|s| seen.insert((s.data_type.clone(), s.kind)))
        .filter(|s| s.reason != SourceSwitchReason::Restored as i32)
        .cloned()
        .collect()
}

/// A switch's reason as a person reads it.
pub fn reason_word(reason: i32) -> &'static str {
    match SourceSwitchReason::try_from(reason).unwrap_or(SourceSwitchReason::Unspecified) {
        SourceSwitchReason::Silent => "silent",
        SourceSwitchReason::Stalled => "stalled",
        SourceSwitchReason::Quarantined => "quarantined",
        SourceSwitchReason::Restored => "restored",
        SourceSwitchReason::Unspecified => "not said",
    }
}

/// A gap's state, and its cause, as a person reads them.
pub fn gap_state_word(state: i32) -> &'static str {
    match GapState::try_from(state).unwrap_or(GapState::Unspecified) {
        GapState::Refetching => "refetching",
        GapState::Recovered => "recovered",
        GapState::Unrecovered => "unrecovered",
        GapState::Unspecified => "not said",
    }
}

pub fn gap_cause_word(cause: i32) -> &'static str {
    match GapCause::try_from(cause).unwrap_or(GapCause::Unspecified) {
        GapCause::WriteAheadBound => "write-ahead bound",
        GapCause::Unspecified => "not said",
    }
}

/// The words for a gap whose dataset is not yet known (proto3's unset).
pub const NOT_YET_KNOWN: &str = "dataset not yet known";

/// The Gaps tab: a line per gap not recovered, newest first.
fn gaps_tab(open: &[DatasetGap]) -> String {
    if open.is_empty() {
        return "<p class=\"empty\" data-gaps=\"0\">No gap: every row the lake acknowledged is \
                recorded.</p>"
            .to_string();
    }
    let lines: String = open
        .iter()
        .map(|gap| {
            let dataset = if gap.dataset.is_empty() {
                NOT_YET_KNOWN.to_string()
            } else {
                gap.dataset.clone()
            };
            let (from_day, from_time) = when(gap.valid_from_ns);
            let (until_day, until_time) = when(gap.valid_until_ns);
            let (day, time) = when(gap.recorded_at_ns);
            let range = if gap.dataset.is_empty() {
                "not yet known".to_string()
            } else {
                format!("{from_day} {from_time} to {until_day} {until_time}")
            };
            format!(
                "<tr data-gap=\"{id}\"><td title=\"{dataset}\">{dataset}</td>\
                 <td title=\"{state}\"><span class=\"badge warn\">{state}</span></td><td class=\"wide\" title=\"{range}\">{range}</td>\
                 <td class=\"wide\" title=\"{cause}\">{cause}</td><td title=\"{day} {time}, {id}\">{day}<span class=\"dates\"> {time}</span></td></tr>",
                id = escape(&gap.gap_id),
                dataset = escape(&dataset),
                state = gap_state_word(gap.state),
                range = escape(&range),
                cause = gap_cause_word(gap.cause),
                day = escape(&day),
                time = escape(&time),
            )
        })
        .collect();
    format!(
        "<om-pager search=\"Search gaps\"><table class=\"list one-line gaps\" data-gaps=\"{n}\"><thead><tr>\
         <th>Dataset</th><th>State</th><th class=\"wide\">Valid</th><th class=\"wide\">Cause</th>\
         <th>Found</th></tr></thead><tbody>{lines}</tbody></table></om-pager>",
        n = open.len()
    )
}

/// One dataset as the page and `dashboard__list_datasets` both draw it.
#[derive(Debug, Clone)]
pub struct Row {
    pub dataset: DatasetRef,
    /// The licence enforced.
    pub licence: DatasetLicence,
    /// Whether the deployment set it, rather than the restrictive reading
    /// of a dataset not licensed.
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
pub(crate) fn readers_of(records: &AccessRecords, instance: &str) -> BTreeSet<String> {
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

/// A dataset's class of source, as the page says it beside its prices
/// (contract 28, amended 2026-10-31, ruling 906); none where its catalogue
/// states none.
fn class_said(class: i32) -> Option<&'static str> {
    match meridian_pb::v1::PriceSourceClass::try_from(class) {
        Ok(meridian_pb::v1::PriceSourceClass::Traded) => Some("traded"),
        Ok(meridian_pb::v1::PriceSourceClass::FirmQuote) => Some("firm quote"),
        Ok(meridian_pb::v1::PriceSourceClass::Composite) => Some("composite"),
        Ok(meridian_pb::v1::PriceSourceClass::Evaluated) => Some("evaluated"),
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
                    "derived_use": if meridian_domain::lake::derived_use_bool(licence) { vec!["1"] } else { vec![] },
                    "display": if licence.display { vec!["1"] } else { vec![] },
                    "personal_use": if licence.personal_use { vec!["1"] } else { vec![] },
                    // Contract v28 (491, 597): the eligibility in force.
                    "eligibility_uses": licence.eligibility.as_ref().map(|e| e.uses.iter().map(|u| match meridian_pb::v1::DataUse::try_from(*u) {
                        Ok(meridian_pb::v1::DataUse::Personal) => "personal",
                        _ => "commercial",
                    }).collect::<Vec<_>>()).unwrap_or_default(),
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
            // Contract 28, amended 2026-10-31 (W10.1; ruling 896): the
            // source behind the vendor, or whose aggregate it serves, from
            // the licence enforced, which keeps the declared one.
            let carried = if licence.underlying_source.is_empty() {
                carried
            } else {
                format!("{carried} \u{b7} {}", licence.underlying_source)
            };
            // Ruling 906: the class of source its prices come from, said
            // beside the prices it qualifies.
            let class = class_said(declaration.price_source_class);
            let serves: Vec<String> = lake::types_served(&declaration)
                .into_iter()
                .map(|data_type| match class {
                    Some(class) if data_type == lake::PRICE => {
                        format!("{} ({class})", type_word(data_type))
                    }
                    _ => type_word(data_type).to_string(),
                })
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
            // Contract v29 (W10.9; ruling 619 (2)): the rows in force by their
            // effective state, the flagged ones in the cell.
            let quality = format!(
                "{} \u{b7} {}",
                thousands(d.suspicious_count),
                thousands(d.invalid_count)
            );
            let quality_said = format!(
                "{quality}: {} valid, {} suspicious, {} invalid (left out of default reads)",
                thousands(d.valid_count),
                thousands(d.suspicious_count),
                thousands(d.invalid_count)
            );
            let source = if row.licence_set {
                format!(
                    "set by {}",
                    crate::tickets::display_name(records, &licence.updated_by)
                )
            } else {
                // Step 3c's question 6 (ruling 1030): kept and read as
                // before, each use right read restrictively.
                "not licensed: kept and read, each use right restrictive".into()
            };
            format!(
                "<tr data-id=\"{id}\"><td title=\"{id}\"><button type=\"button\" class=\"link\" \
                 data-dialog-open=\"licence\" data-title=\"Licence {id}\" data-fill=\"{fill}\">{id}</button></td>\
                 <td class=\"wide\" title=\"{carried_said}\">{carried}</td>\
                 <td class=\"wide\" title=\"{serves}; {modes}\">{serves}</td>\
                 <td title=\"{licence_said}, {source}\">{licence_said}{badge}</td>\
                 <td class=\"wide\" title=\"{entitled_all}\" data-entitled=\"{n}\">{n}</td>\
                 <td class=\"wide num\" data-unconverted>{unconverted}</td>\
                 <td class=\"wide num\" data-misses>{misses}</td>\
                 <td class=\"wide num\" data-closed-days title=\"{closed_said}\">{closed}</td>\
                 <td class=\"wide num\" data-quality title=\"{quality_said}\">{quality}</td>\
                 <td class=\"actions\"><button type=\"button\" data-dialog-open=\"entitle\" data-title=\"Entitle a plugin to {id}\" \
                 data-fill=\"{entitle}\">Entitle</button></td></tr>",
                id = escape(&d.dataset),
                fill = escape(&fill.to_string()),
                entitle = escape(&entitle.to_string()),
                carried = escape(&crate::mcp::others_words(&carried)),
                // Contract v30 (W10.1; ruling 1032): the vendor's terms and
                // pricing in its words, read whole in the cell's title.
                carried_said = escape(&if declaration.terms_and_pricing.is_empty() {
                    crate::mcp::others_words(&carried)
                } else {
                    format!(
                        "{}; terms and pricing: {}",
                        crate::mcp::others_words(&carried),
                        crate::mcp::others_words(&declaration.terms_and_pricing)
                    )
                }),
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
                closed = closed_cell(d.closed_day_count, d.outside_coverage_count),
                closed_said = escape(&format!(
                    "{}: {} dated on a closed day of its calendar, {} outside its coverage",
                    closed_cell(d.closed_day_count, d.outside_coverage_count),
                    d.closed_day_count,
                    d.outside_coverage_count
                )),
                quality = quality,
                quality_said = escape(&quality_said),
            )
        })
        .collect();
    let body = if lines.is_empty() {
        "<p class=\"empty\" data-datasets=\"0\">No dataset yet: a launched plugin holding \
         <code>data</code> declares its datasets in its catalogue, and each is listed here.</p>"
            .to_string()
    } else {
        format!(
            "<om-pager search=\"Search datasets\"><table class=\"list one-line datasets\" data-datasets=\"{n}\"><thead><tr>\
             <th>Dataset</th><th class=\"wide\" title=\"Vendor, and the source behind it\">Vendor</th>\
             <th class=\"wide\" title=\"What it serves, its prices' class of source beside them\">Serves</th>\
             <th class=\"licence\" title=\"The dataset's name opens its licence\">Licence</th><th class=\"wide count\" title=\"Plugins entitled\">Entitled</th>\
             <th class=\"wide num\" title=\"Values left unconverted\">Unconverted</th>\
             <th class=\"wide num\" title=\"Identifiers and venues reported missing\">Misses</th>\
             <th class=\"wide num\" title=\"Daily values dated on a closed day of its calendar, and outside its coverage\">Closed days</th>\
             <th class=\"wide num\" title=\"Rows in force flagged suspicious, and invalid; valid in each cell's hover\">Flagged</th>\
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
         <label data-om-applies-when=\"kept\" data-om-one-of=\"1\">Kept for, in days \
         <input name=\"retention_days\" type=\"number\" min=\"0\" max=\"36500\" \
         step=\"1\" inputmode=\"numeric\" required></label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"derived_use\" value=\"1\"> Derived data may be made</label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"display\" value=\"1\"> It may be shown</label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"personal_use\" value=\"1\"> Its terms are one person's</label>\
         <fieldset class=\"eligibility\"><legend>Who may take it, as its terms state</legend>\
         <label class=\"check\"><input type=\"checkbox\" name=\"eligibility_uses\" value=\"personal\"> Personal use</label>\
         <label class=\"check\"><input type=\"checkbox\" name=\"eligibility_uses\" value=\"commercial\"> Commercial use</label>\
         <div class=\"threshold\"><label>Revenue below <input name=\"buyer_revenue_below\" inputmode=\"decimal\" \
         pattern=\"[0-9]+(\\.[0-9]+)?\" placeholder=\"None\"></label>\
         <label>or at least <input name=\"buyer_revenue_at_least\" inputmode=\"decimal\" \
         pattern=\"[0-9]+(\\.[0-9]+)?\" placeholder=\"None\"></label>\
         <label>Currency <input name=\"buyer_revenue_currency_code\" maxlength=\"3\" pattern=\"[A-Za-z]{{3}}\" \
         placeholder=\"USD\"></label></div></fieldset>\
         <label>Fields readable by default <input name=\"default_fields\" \
         placeholder=\"Every field; or meridian.v1.Bar.vwap, ...\"></label>\
         <label>Note <input name=\"note\" maxlength=\"2000\" placeholder=\"Why\"></label>\
         <p class=\"hint\">0 days keeps rows with no limit set. This records what you enter; it \
         does not say the deployment meets the vendor's terms.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" name=\"prepare\" value=\"1\" data-prepare>Prepare</button><button type=\"submit\" class=\"primary\" data-label-new=\"Save\">Save</button></div></form></dialog>\
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
         <button type=\"submit\" name=\"prepare\" value=\"1\" data-prepare>Prepare</button><button type=\"submit\" class=\"primary\" data-label-new=\"Save\">Save</button></div></form></dialog>",
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
        "<om-pager search=\"Search entitlements\"><table class=\"list one-line entitlements\" data-entitlements=\"{n}\"><thead><tr>\
         <th>Dataset</th><th>Plugin</th><th class=\"wide\">Fields</th><th class=\"wide\">Set by</th>\
         <th>Set</th><th class=\"actions\"></th></tr></thead><tbody>{lines}</tbody></table></om-pager>",
        n = rows.iter().map(|r| r.entitled.len()).sum::<usize>()
    )
}

fn priority_tab(
    priorities: &[SourcePriority],
    standing: &[SourceSwitched],
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
            // Its default read standing on a backup (section 8).
            let switched = standing
                .iter()
                .find(|s| s.data_type == p.data_type && s.kind == p.kind)
                .map_or(String::new(), |s| {
                    format!(
                        "<span class=\"badge warn\" data-switched title=\"{} to {}, {}\">{SWITCHED}</span> ",
                        escape(&s.from_dataset),
                        escape(&s.to_dataset),
                        reason_word(s.reason)
                    )
                });
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
                 <td title=\"{whole}\">{switched}{order}</td><td class=\"wide\" title=\"{note}\">{by}</td>\
                 <td title=\"{day} {time} by {by}\">{day}<span class=\"dates\"> {time}</span></td>\
                 <td class=\"actions\"><button type=\"button\" data-dialog-open=\"priority-dialog\" \
                 data-title=\"Change the priority\" data-fill=\"{fill}\">Edit</button></td></tr>",
                what = escape(&what),
                order = escape(&p.datasets.join(" then ")),
                // The cell's whole text in its title, the badge's words with
                // it, since an ellipsised cell is read whole from its title.
                whole = escape(&if switched.is_empty() {
                    p.datasets.join(" then ")
                } else {
                    format!("{SWITCHED} {}", p.datasets.join(" then "))
                }),
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
            "<om-pager search=\"Search priorities\"><table class=\"list one-line priorities\" data-priorities=\"{n}\"><thead><tr>\
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
         <option value=\"{bar}\">Bar</option><option value=\"{trade}\">Trade</option>\
         <option value=\"{quote}\">Quote</option><option value=\"{rate}\">Rate</option></select></label>\
         <label data-om-applies-when=\"data_type\" data-om-one-of=\"{price}\">Kind, for prices \
         <select name=\"kind\" required><option value=\"close\">Close</option><option value=\"last\">Last</option>\
         <option value=\"nav\">NAV</option><option value=\"settlement\">Settlement</option></select></label>\
         <label>Datasets, first to last, one a line <textarea name=\"datasets\" rows=\"3\" required></textarea></label>\
         <label>Note <input name=\"note\" maxlength=\"2000\" placeholder=\"Why\"></label>\
         <p class=\"hint\">At most 16, each declared for the data type{known}. Replaced whole; \
         refused if it changed since you read it.</p></div>\
         <div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
         <button type=\"submit\" name=\"prepare\" value=\"1\" data-prepare>Prepare</button><button type=\"submit\" class=\"primary\" data-label-new=\"Set\">Set</button></div></form></dialog>",
        price = lake::PRICE,
        bar = lake::BAR,
        trade = lake::TRADE,
        quote = lake::QUOTE,
        rate = lake::RATE,
    );
    (body, dialog)
}

/// A tab shown measures its pager again, as on a resize: a pager measured
/// while its tab was hidden pages nothing.
pub(super) const PAGER_SCRIPT: &str = r#"
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
#[allow(clippy::too_many_arguments)]
pub fn page(
    listed: &Result<ListDatasetsReply, String>,
    priorities: &Result<ListSourcePrioritiesReply, String>,
    open_gaps: &Result<Vec<DatasetGap>, String>,
    standing: &Result<Vec<SourceSwitched>, String>,
    records: &AccessRecords,
    token: &str,
    notice: &str,
    ages: &[meridian_domain::latency::Read],
    used: &(String, String),
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
    let standing: &[SourceSwitched] = standing.as_deref().unwrap_or_default();
    let (priority_body, priority_dialog) = match priorities {
        Ok(reply) => priority_tab(&reply.priorities, standing, &rows, records, token),
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
    // Each default read standing on a backup, one line (section 8).
    let notice = if standing.is_empty() {
        notice
    } else {
        let said: Vec<String> = standing
            .iter()
            .map(|s| {
                let kind = kind_word(s.kind);
                let what = if kind.is_empty() {
                    type_word(&s.data_type).to_string()
                } else {
                    format!("{}, {kind}", type_word(&s.data_type))
                };
                format!(
                    "<span title=\"{} to {}, {}\">{}</span>",
                    escape(&s.from_dataset),
                    escape(&s.to_dataset),
                    reason_word(s.reason),
                    escape(&what)
                )
            })
            .collect();
        format!(
            "{notice}<p class=\"refused one-line\" data-switched-line>{}: {SWITCHED}</p>",
            said.join(", ")
        )
    };
    let add = "<button type=\"button\" class=\"primary\" data-dialog-open=\"priority-dialog\" \
               aria-label=\"Add a priority\">+ Add</button>";
    format!(
        "<div class=\"admin\"><div class=\"page-head\"><h1>Data sources</h1></div>{notice}\
         <nav class=\"tabs\">{tabs}</nav>{}{}{}{}{}{}{datasets_dialogs}{priority_dialog}{}</div>\
         <script>{}{}{PAGER_SCRIPT}</script>",
        section(
            "datasets",
            "Datasets",
            "What each launched plugin's catalogue serves the lake, on the terms enforced: the \
             deployment's licence, or until one is set the dataset kept and read with each use \
             right restrictive. Prices are read \
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
        section(
            "age",
            "Age",
            "Over the last minute, how old each dataset's rows are at each hop, from the source's \
             time, and each plugin's sessions' round trip, order acknowledgement and fill delay, \
             as the plugins measure them.",
            "",
            &crate::latency::tab(ages)
        ),
        section(
            "use",
            "Use",
            "Who could use each dataset in the month, from the records and entitlements: \
             persons, plugins by category, service accounts. Yours to file; never sent.",
            "<button type=\"button\" data-dialog-open=\"professional-status\">Change a status</button>",
            &used.0
        ),
        section(
            "gaps",
            "Gaps",
            "Each gap the lake's write-ahead stream left that is not recovered: rows inside it are \
             not invented, and a read over it returns what was recorded.",
            "",
            &match open_gaps {
                Ok(open) => gaps_tab(open),
                Err(why) => unread(why),
            }
        ),
        used.1,
        super::overview::SCRIPT_START,
        super::overview::SCRIPT_END,
    )
}

/// A dataset's closed-day count as its cell says it, those outside the
/// calendar's coverage beside it, apart (W10.9).
fn closed_cell(closed: u64, outside: u64) -> String {
    if outside == 0 {
        thousands(closed)
    } else {
        format!("{} ({} outside)", thousands(closed), thousands(outside))
    }
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
                    ..Default::default()
                }),
                unconverted_count: 2,
                miss_count: 1,
                closed_day_count: 3,
                outside_coverage_count: 1,
                ..Default::default()
            }],
            // The deployment's licence (contract v30 retired a catalogue's
            // licence_default, ruling 1032).
            licences: vec![DatasetLicence {
                dataset: "coinbase-1:daily".into(),
                kept: true,
                personal_use,
                ..Default::default()
            }],
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
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &records,
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
        );
        assert!(body.contains("data-datasets=\"1\""), "{body}");
        assert!(body.contains("<td class=\"wide num\" data-unconverted>2</td>"));
        assert!(body.contains("data-closed-days"));
        assert!(body.contains(
            " data-closed-days title=\"3 (1 outside): 3 dated on a closed day of its calendar, \
             1 outside its coverage\">3 (1 outside)</td>"
        ));
        assert!(body.contains("<td class=\"wide num\" data-misses>1</td>"));
        assert!(body.contains("each use right restrictive"));
        assert!(!body.contains("not licensed:"));
        assert!(body.contains("data-entitled=\"1\""));
        assert!(body.contains("data-entitlements=\"1\""));
        assert!(body.contains("No priority is set"));
        assert!(!body.contains("data-one-person"));
        for (id, _) in TABS {
            assert!(body.contains(&format!("id=\"{id}\"")), "{id}");
        }
    }

    /// Step 3c's question 6 (ruling 1030): a dataset no deployment admin
    /// has licensed is said so, kept and read with each use right
    /// restrictive, and its licence's form opens kept with derived data not
    /// ticked.
    #[test]
    fn a_dataset_not_licensed_is_kept_and_read_each_use_right_restrictive() {
        let mut reply = listed(false);
        reply.licences.clear();
        let body = page(
            &Ok(reply),
            &Ok(ListSourcePrioritiesReply::default()),
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &AccessRecords::default(),
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
        );
        assert!(body.contains("not licensed: kept and read, each use right restrictive"));
        assert!(body.contains("&quot;derived_use&quot;:[]"), "{body}");
        assert!(body.contains("&quot;kept&quot;:[&quot;1&quot;]"), "{body}");
    }

    /// Contract v30 (W10.1; ruling 1032): a dataset's terms and pricing, as
    /// its upload declares them, read whole in its vendor's cell's title.
    #[test]
    fn a_datasets_terms_and_pricing_are_read_in_its_vendors_title() {
        let mut reply = listed(false);
        reply.datasets[0]
            .declaration
            .as_mut()
            .unwrap()
            .terms_and_pricing = "Free; no redistribution.".into();
        let body = page(
            &Ok(reply),
            &Ok(ListSourcePrioritiesReply::default()),
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &AccessRecords::default(),
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
        );
        assert!(
            body.contains("; terms and pricing: Free; no redistribution.\">"),
            "{body}"
        );
    }

    /// Contract 28, amended 2026-10-31 (rulings 896, 906): the class of
    /// source beside the prices it qualifies, and whose aggregate beside the
    /// vendor, each read whole in its cell's title.
    #[test]
    fn a_dataset_shows_its_class_of_source_and_whose_aggregate() {
        let mut reply = listed(false);
        let declaration = reply.datasets[0].declaration.as_mut().unwrap();
        declaration.price_source_class = meridian_pb::v1::PriceSourceClass::Composite as i32;
        reply.licences[0].underlying_source = "Pyth's aggregate of its publishers".into();
        let body = page(
            &Ok(reply),
            &Ok(ListSourcePrioritiesReply::default()),
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &AccessRecords::default(),
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
        );
        assert!(body.contains(">Price (composite), Bar</td>"), "{body}");
        assert!(
            body.contains("Coinbase \u{b7} Pyth&#39;s aggregate of its publishers")
                || body.contains("Coinbase \u{b7} Pyth's aggregate of its publishers"),
            "{body}"
        );
        // Not stated, as every catalogue before the amendment.
        let body = page(
            &Ok(listed(false)),
            &Ok(ListSourcePrioritiesReply::default()),
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &AccessRecords::default(),
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
        );
        assert!(body.contains(">Price, Bar</td>"), "{body}");
        assert!(!body.contains("Coinbase \u{b7}"), "{body}");
    }

    /// Contract v30 (W10.6, W10.14): a line per gap not recovered, the
    /// unattributed one's dataset not yet known; and a default read standing
    /// on a backup "switched to backup source", on its priority and above
    /// the tabs, a switch back standing on nothing.
    #[test]
    fn gaps_and_a_read_standing_on_a_backup_are_shown() {
        let quote = |reason: SourceSwitchReason, at: i64| SourceSwitched {
            data_type: lake::QUOTE.into(),
            from_dataset: "kraken-1:ticker".into(),
            to_dataset: "coinbase-1:ticker".into(),
            reason: reason as i32,
            switched_at_ns: at,
            ..Default::default()
        };
        // Newest first: stalled now, after a switch back.
        let standing = standing_of(&[
            quote(SourceSwitchReason::Stalled, 3),
            quote(SourceSwitchReason::Restored, 2),
        ]);
        assert_eq!(standing.len(), 1);
        assert!(standing_of(&[
            quote(SourceSwitchReason::Restored, 4),
            quote(SourceSwitchReason::Stalled, 3)
        ])
        .is_empty());
        let gaps = vec![
            DatasetGap {
                gap_id: "GAP-02".into(),
                dataset: "drive-data:drive".into(),
                state: GapState::Unrecovered as i32,
                cause: GapCause::WriteAheadBound as i32,
                found_in_gap_id: "GAP-01".into(),
                ..Default::default()
            },
            DatasetGap {
                gap_id: "GAP-01".into(),
                state: GapState::Unrecovered as i32,
                cause: GapCause::WriteAheadBound as i32,
                ..Default::default()
            },
        ];
        let priorities = ListSourcePrioritiesReply {
            priorities: vec![SourcePriority {
                data_type: lake::QUOTE.into(),
                datasets: vec!["kraken-1:ticker".into(), "coinbase-1:ticker".into()],
                ..Default::default()
            }],
        };
        let body = page(
            &Ok(listed(false)),
            &Ok(priorities),
            &Ok(gaps),
            &Ok(standing),
            &AccessRecords::default(),
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
        );
        assert!(body.contains("data-gaps=\"2\""), "{body}");
        assert!(body.contains(&format!(">{NOT_YET_KNOWN}</td>")));
        assert!(body.contains(">drive-data:drive</td>"));
        assert!(body.contains("write-ahead bound"));
        assert!(
            body.contains("data-switched title=") && body.contains(&format!(": {SWITCHED}</p>")),
            "on its priority and above the tabs"
        );
        assert!(body.contains("data-switched-line"));
        let none = page(
            &Ok(listed(false)),
            &Ok(ListSourcePrioritiesReply::default()),
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &AccessRecords::default(),
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
        );
        assert!(none.contains("data-gaps=\"0\"") && !none.contains(SWITCHED));
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
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &records,
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
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
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &AccessRecords::default(),
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
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
            &Ok(Vec::new()),
            &Ok(Vec::new()),
            &AccessRecords::default(),
            "<t>",
            "",
            &[],
            &(String::new(), String::new()),
        );
        assert!(body.contains("The lake did not answer: timed out"));
    }
}
