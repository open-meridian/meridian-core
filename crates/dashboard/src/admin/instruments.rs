//! The deployment's instrument records, completed by the deployment admin
//! (W3.10 to W3.13, contract v10; ruled 2026-10-02: core owns completing
//! instrument records, on the dashboard, done by the deployment admin).
//!
//! The Instruments page lists the records the book cannot use first -- no
//! asset class or no currency -- each with what it lacks, its identifiers and
//! who reported them, and the values offered for it; the conflicts, for a
//! merge; and how many licensed identifiers the deployment holds, per scheme,
//! against CUSIP Global Services' 500-identifier threshold. A record's own
//! page completes it, value by value, each with its source, asks the platform
//! about it when a person chooses to, and reads its history.
//!
//! Every change goes to the instrument store on the signed-in person's
//! behalf (`Bus::call_for`), so the store records who made it; the page never
//! types a person. It shows records only: no account, no quantity, which the
//! deployment admin does not reach (spec/deployment-dashboard-and-access.md).

use std::time::Duration;

use meridian_domain::v1::{
    instrument_value, AskPlatformForInstrumentReply, AskPlatformForInstrumentRequest, AssetClass,
    CompleteInstrumentsReply, CompleteInstrumentsRequest, Identifier, InstrumentCompletion,
    InstrumentField, InstrumentRecord, InstrumentToComplete, InstrumentValue, InstrumentVersion,
    ListInstrumentsToCompleteReply, ListInstrumentsToCompleteRequest, MergeInstrumentsReply,
    MergeInstrumentsRequest, OfferedValue, ReadInstrumentHistoryReply,
    ReadInstrumentHistoryRequest,
};
use prost::Message;

use crate::html::escape;

pub const COMPLETE_INSTRUMENTS: &str = "platform.reference.command.complete-instruments";
pub const MERGE_INSTRUMENTS: &str = "platform.reference.command.merge-instruments";
pub const LIST_INSTRUMENTS_TO_COMPLETE: &str =
    "platform.reference.query.list-instruments-to-complete";
pub const READ_INSTRUMENT_HISTORY: &str = "platform.reference.query.read-instrument-history";
pub const ASK_PLATFORM_FOR_INSTRUMENT: &str =
    "platform.reference.query.ask-platform-for-instrument";

/// How long a read waits for the instrument store: one page's worth, and the
/// rest of the dashboard is drawn without it.
const WAIT: Duration = Duration::from_secs(3);

/// How long an ask waits: the conductor's own bound is shorter, so the
/// person hears its answer, unreachable included.
const ASKING: Duration = Duration::from_secs(12);

/// CUSIP Global Services' threshold (ruled 2026-10-02): the count of CUSIPs a
/// deployment holds beyond which its licence terms change.
pub const CUSIP_THRESHOLD: i64 = 500;

/// The asset classes, as the form offers them (W1's closed list).
const CLASSES: [(AssetClass, &str); 7] = [
    (AssetClass::Equity, "Equity"),
    (AssetClass::Debt, "Debt"),
    (AssetClass::Fund, "Fund"),
    (AssetClass::Derivative, "Derivative"),
    (AssetClass::CryptoAsset, "Crypto asset"),
    (AssetClass::EventContract, "Event contract"),
    (AssetClass::Cash, "Cash"),
];

fn class_name(value: i32) -> &'static str {
    CLASSES
        .iter()
        .find(|(class, _)| *class as i32 == value)
        .map(|(_, name)| *name)
        .unwrap_or("")
}

async fn ask<R: Message + Default>(
    bus: &meridian_bus::Bus,
    topic: &str,
    request_type: &str,
    request: impl Message,
    person: Option<&str>,
    wait: Duration,
) -> Result<R, String> {
    let asked = match person {
        Some(subject) => {
            bus.call_for(
                topic,
                request_type,
                request.encode_to_vec(),
                None,
                Some(wait),
                subject,
            )
            .await
        }
        None => {
            bus.call(
                topic,
                request_type,
                request.encode_to_vec(),
                None,
                Some(wait),
            )
            .await
        }
    };
    let (_, bytes) = asked.map_err(|failed| match failed {
        meridian_bus::BusError::HandlerFailed { detail, .. } => {
            match meridian_bus::read_refusal(&detail) {
                Some((_, words)) => words.to_string(),
                None => detail,
            }
        }
        other => other.to_string(),
    })?;
    R::decode(&bytes[..]).map_err(|failed| format!("the answer did not read: {failed}"))
}

/// W3.11: the records to complete, or one record (`instrument_id`).
pub async fn list(
    bus: &meridian_bus::Bus,
    instrument_id: &str,
) -> Result<ListInstrumentsToCompleteReply, String> {
    ask(
        bus,
        LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            include_complete: !instrument_id.is_empty(),
            instrument_id: instrument_id.to_string(),
            page_size: 1000,
            cursor: String::new(),
        },
        None,
        WAIT,
    )
    .await
}

/// W3.12.
pub async fn history(
    bus: &meridian_bus::Bus,
    instrument_id: &str,
) -> Result<ReadInstrumentHistoryReply, String> {
    ask(
        bus,
        READ_INSTRUMENT_HISTORY,
        "meridian.v1.ReadInstrumentHistoryRequest",
        ReadInstrumentHistoryRequest {
            instrument_id: instrument_id.to_string(),
            page_size: 200,
            cursor: String::new(),
        },
        None,
        WAIT,
    )
    .await
}

/// W3.10, for the person signed in.
pub async fn complete(
    bus: &meridian_bus::Bus,
    subject: &str,
    request: CompleteInstrumentsRequest,
) -> Result<CompleteInstrumentsReply, String> {
    ask(
        bus,
        COMPLETE_INSTRUMENTS,
        "meridian.v1.CompleteInstrumentsRequest",
        request,
        Some(subject),
        Duration::from_secs(20),
    )
    .await
}

/// W3.13, for the person signed in.
pub async fn merge(
    bus: &meridian_bus::Bus,
    subject: &str,
    request: MergeInstrumentsRequest,
) -> Result<MergeInstrumentsReply, String> {
    ask(
        bus,
        MERGE_INSTRUMENTS,
        "meridian.v1.MergeInstrumentsRequest",
        request,
        Some(subject),
        Duration::from_secs(20),
    )
    .await
}

/// W3.3: ask the platform about a record, through the conductor.
pub async fn ask_platform(
    bus: &meridian_bus::Bus,
    record: &InstrumentRecord,
) -> Result<AskPlatformForInstrumentReply, String> {
    ask(
        bus,
        ASK_PLATFORM_FOR_INSTRUMENT,
        "meridian.v1.AskPlatformForInstrumentRequest",
        AskPlatformForInstrumentRequest {
            instrument_id: record.instrument_id.clone(),
            identifiers: record.identifiers.clone(),
            as_of_ns: 0,
        },
        None,
        ASKING,
    )
    .await
}

/// The values a completion form sends: each field the person filled, with
/// the source they gave it. A field left empty is left alone; one equal to
/// what is in force changes nothing at the store.
#[derive(Debug, Default, Clone)]
pub struct Filled {
    pub instrument_id: String,
    pub against_version: String,
    pub asset_class: String,
    pub asset_class_source: String,
    pub currency: String,
    pub currency_source: String,
    pub description: String,
    pub description_source: String,
    pub identifier_scheme: String,
    pub identifier_value: String,
    pub identifier_namespace: String,
    pub identifier_source: String,
    pub note: String,
}

/// One record's completion from its form, or why the form cannot be sent.
pub fn completion(filled: &Filled) -> Result<CompleteInstrumentsRequest, String> {
    if filled.instrument_id.is_empty() {
        return Err("the form names no record".into());
    }
    let against_version: i64 = filled
        .against_version
        .parse()
        .map_err(|_| "the form names no version of the record".to_string())?;
    let mut values = Vec::new();
    if !filled.asset_class.is_empty() {
        let class: i32 = filled
            .asset_class
            .parse()
            .map_err(|_| format!("{:?} is no asset class", filled.asset_class))?;
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::AssetClass(class)),
            source: filled.asset_class_source.trim().to_string(),
        });
    }
    if !filled.currency.trim().is_empty() {
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::Currency(
                filled.currency.trim().to_ascii_uppercase(),
            )),
            source: filled.currency_source.trim().to_string(),
        });
    }
    if !filled.description.trim().is_empty() {
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::Description(
                filled.description.trim().to_string(),
            )),
            source: filled.description_source.trim().to_string(),
        });
    }
    if !filled.identifier_scheme.trim().is_empty() || !filled.identifier_value.trim().is_empty() {
        values.push(InstrumentValue {
            value: Some(instrument_value::Value::Identifier(Identifier {
                scheme: filled.identifier_scheme.trim().to_ascii_lowercase(),
                value: filled.identifier_value.trim().to_string(),
                source: filled.identifier_namespace.trim().to_string(),
            })),
            source: filled.identifier_source.trim().to_string(),
        });
    }
    if values.is_empty() {
        return Err("nothing was filled in".into());
    }
    Ok(CompleteInstrumentsRequest {
        completions: vec![InstrumentCompletion {
            instrument_id: filled.instrument_id.clone(),
            against_version,
            values,
            note: filled.note.trim().to_string(),
        }],
    })
}

/// "Accept offered values" for the records chosen: for each, every field the
/// book or the list lacks that something offers, the first offer for it, with
/// that offer's source (W3.10, the spec's requirement 8). One command.
pub fn accepting(listed: &[InstrumentToComplete], chosen: &[String]) -> CompleteInstrumentsRequest {
    let mut completions = Vec::new();
    for item in listed {
        let Some(record) = item.instrument.as_ref() else {
            continue;
        };
        if !chosen.contains(&record.instrument_id) {
            continue;
        }
        let mut values = Vec::new();
        for lacking in &item.lacks {
            if let Some(offer) = record
                .offers
                .iter()
                .filter_map(|offered| offered.value.as_ref())
                .find(|value| field_of(value) == *lacking)
            {
                values.push(offer.clone());
            }
        }
        if !values.is_empty() {
            completions.push(InstrumentCompletion {
                instrument_id: record.instrument_id.clone(),
                against_version: record.version,
                values,
                note: String::new(),
            });
        }
    }
    CompleteInstrumentsRequest { completions }
}

fn field_of(value: &InstrumentValue) -> i32 {
    (match value.value {
        Some(instrument_value::Value::AssetClass(_)) => InstrumentField::AssetClass,
        Some(instrument_value::Value::Currency(_)) => InstrumentField::Currency,
        Some(instrument_value::Value::Description(_)) => InstrumentField::Description,
        Some(instrument_value::Value::Identifier(_)) => InstrumentField::Identifier,
        None => InstrumentField::Unspecified,
    }) as i32
}

fn value_words(value: &InstrumentValue) -> String {
    match &value.value {
        Some(instrument_value::Value::AssetClass(class)) => class_name(*class).to_string(),
        Some(instrument_value::Value::Currency(code)) => code.clone(),
        Some(instrument_value::Value::Description(text)) => text.clone(),
        Some(instrument_value::Value::Identifier(id)) => identifier_words(id),
        None => String::new(),
    }
}

fn identifier_words(id: &Identifier) -> String {
    if id.source.is_empty() {
        format!("{}: {}", id.scheme, id.value)
    } else {
        format!("{} ({}): {}", id.scheme, id.source, id.value)
    }
}

fn lacks_words(lacks: &[i32]) -> String {
    lacks
        .iter()
        .filter_map(|field| match InstrumentField::try_from(*field).ok()? {
            InstrumentField::AssetClass => Some("asset class"),
            InstrumentField::Currency => Some("currency"),
            InstrumentField::Description => Some("description"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// How many records the book cannot use, for the dashboard's home: a
/// first-class number (spec/security-master-and-holdings.md). `None` when the
/// instrument store did not answer.
pub async fn incomplete_for_book(bus: &meridian_bus::Bus) -> Option<i64> {
    ask::<ListInstrumentsToCompleteReply>(
        bus,
        LIST_INSTRUMENTS_TO_COMPLETE,
        "meridian.v1.ListInstrumentsToCompleteRequest",
        ListInstrumentsToCompleteRequest {
            page_size: 1,
            ..Default::default()
        },
        None,
        Duration::from_secs(2),
    )
    .await
    .ok()
    .map(|listed| listed.incomplete_for_book)
}

/// The home page's line for a deployment admin, when records wait.
pub fn home_notice(count: Option<i64>) -> String {
    match count {
        Some(0) | None => String::new(),
        Some(n) => format!(
            "<p class=\"notice warn\" id=\"instruments-waiting\">{n} instrument record{s} the book \
             cannot use yet: {lack}. <a href=\"/admin/instruments\">Complete them</a></p>",
            s = if n == 1 { "" } else { "s" },
            lack = "an asset class or a currency is missing",
        ),
    }
}

/// The Instruments page (W3.11).
pub fn list_page(
    listed: &Result<ListInstrumentsToCompleteReply, String>,
    token: &str,
    notice: &str,
) -> String {
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"passed\">{}</p>", escape(notice))
    };
    let listed = match listed {
        Ok(listed) => listed,
        Err(why) => {
            return format!(
                "<div class=\"admin\"><div class=\"page-head\"><h1>Instruments</h1></div>{notice}\
                 <p class=\"refused\">The instrument store is not answering: {}</p></div>",
                escape(why)
            )
        }
    };

    let mut counts = format!(
        "<p class=\"hint\"><strong>{}</strong> the book cannot use (no asset class or no \
         currency), <strong>{}</strong> lacking something. Each value you set is kept with its \
         source and your name.</p>",
        listed.incomplete_for_book, listed.incomplete
    );
    let licensed: Vec<String> = listed
        .licensed_identifiers
        .iter()
        .map(|count| {
            let over = count.scheme == "cusip" && count.count >= CUSIP_THRESHOLD;
            format!(
                "<li>{}: <strong>{}</strong>{}</li>",
                escape(&count.scheme.to_ascii_uppercase()),
                count.count,
                if count.scheme == "cusip" {
                    if over {
                        format!(
                            " <span class=\"badge warn\">at or past CUSIP Global Services' \
                             {CUSIP_THRESHOLD}-identifier threshold</span>"
                        )
                    } else {
                        format!(" of CUSIP Global Services' {CUSIP_THRESHOLD}-identifier threshold")
                    }
                } else {
                    String::new()
                }
            )
        })
        .collect();
    counts.push_str(&format!(
        "<section class=\"admin-section\" id=\"licensed\"><div class=\"section-head\"><div>\
         <h2>Licensed identifiers held</h2><p class=\"hint\">What your plugins reported, kept \
         here under your own licence and never sent anywhere.</p></div></div>{}</section>",
        if licensed.is_empty() {
            "<p class=\"empty\">None.</p>".to_string()
        } else {
            format!("<ul class=\"facts\">{}</ul>", licensed.concat())
        }
    ));

    let conflicts = if listed.conflicts.is_empty() {
        "<p class=\"empty\">None.</p>".to_string()
    } else {
        listed
            .conflicts
            .iter()
            .map(|conflict| {
                let ids: Vec<String> = conflict.instrument_ids.clone();
                let choices: String = ids
                    .iter()
                    .enumerate()
                    .map(|(at, id)| {
                        format!(
                            "<label class=\"choice\"><input type=\"radio\" name=\"kept_instrument_id\" \
                             value=\"{id}\"{checked}> keep <a href=\"/admin/instruments/{id}\">{id}</a></label>",
                            id = escape(id),
                            checked = if at == 0 { " checked" } else { "" }
                        )
                    })
                    .collect();
                format!(
                    "<form class=\"panel\" method=\"post\" action=\"/admin/instruments/merge\">{token}\
                     <p><strong>{}</strong> meet{} {}{}</p>\
                     <input type=\"hidden\" name=\"instrument_ids\" value=\"{}\">{choices}\
                     <label>Why they are one security <input name=\"note\" required></label>\
                     <div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Merge</button></div></form>",
                    escape(
                        &conflict
                            .identifiers
                            .iter()
                            .map(identifier_words)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    if conflict.identifiers.len() == 1 { "s" } else { "" },
                    escape(&ids.join(" and ")),
                    if conflict.reported_by.is_empty() {
                        String::new()
                    } else {
                        format!(", reported by {}", escape(&conflict.reported_by))
                    },
                    escape(&ids.join(",")),
                )
            })
            .collect()
    };

    let rows: String = listed
        .instruments
        .iter()
        .filter_map(|item| item.instrument.as_ref().map(|record| (item, record)))
        .map(|(item, record)| {
            let offered: Vec<String> = record
                .offers
                .iter()
                .filter_map(|offer| offer.value.as_ref())
                .map(|value| format!("{} ({})", value_words(value), value.source))
                .collect();
            format!(
                "<tr><td><input type=\"checkbox\" name=\"instrument_id\" value=\"{id}\" form=\"accept\"{disabled} \
                 aria-label=\"Accept the values offered for {id}\"></td>\
                 <td><a href=\"/admin/instruments/{id}\">{id}</a><span class=\"id\">{ids}</span></td>\
                 <td>{class}</td><td>{currency}</td><td>{description}</td><td>{lacks}{book}</td><td>{offered}</td></tr>",
                id = escape(&record.instrument_id),
                disabled = if offered.is_empty() { " disabled" } else { "" },
                ids = escape(
                    &record
                        .identifiers
                        .iter()
                        .map(identifier_words)
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
                class = escape(class_name(record.asset_class)),
                currency = escape(&record.currency),
                description = escape(&record.description),
                lacks = escape(&lacks_words(&item.lacks)),
                book = if item.complete_for_book {
                    ""
                } else {
                    " <span class=\"badge warn\">the book cannot use it</span>"
                },
                offered = escape(&offered.join("; ")),
            )
        })
        .collect();
    let table = if rows.is_empty() {
        "<p class=\"empty\">Every record is complete.</p>".to_string()
    } else {
        format!(
            "<form id=\"accept\" method=\"post\" action=\"/admin/instruments/accept\">{token}\
             <div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Accept offered values \
             for the records ticked</button></div></form>\
             <table class=\"list\" id=\"instruments-table\"><thead><tr><th></th><th>Record</th><th>Asset class</th>\
             <th>Currency</th><th>Description</th><th>Lacks</th><th>Offered</th></tr></thead><tbody>{rows}</tbody></table>"
        )
    };

    format!(
        "<div class=\"admin\"><div class=\"page-head\"><div><h1>Instruments</h1>\
         <p class=\"hint\">The deployment's own instrument records. An opening balance naming one \
         the book cannot use is refused until it is complete.</p></div></div>{notice}{counts}\
         <section class=\"admin-section\" id=\"to-complete\"><div class=\"section-head\"><div>\
         <h2>Records to complete</h2></div></div>{table}</section>\
         <section class=\"admin-section\" id=\"conflicts\"><div class=\"section-head\"><div>\
         <h2>Conflicts</h2><p class=\"hint\">Identifiers that meet two records. Merge them if \
         they are one security; the positions held under the one merged move onto the one that \
         stays.</p></div></div>{conflicts}</section></div>"
    )
}

/// The first offer for a field, where one is held.
fn offer_for(record: &InstrumentRecord, field: InstrumentField) -> Option<&OfferedValue> {
    record
        .offers
        .iter()
        .find(|offer| offer.value.as_ref().map(field_of) == Some(field as i32))
}

/// Where a field's value in force came from, in words.
fn source_words(record: &InstrumentRecord, field: InstrumentField) -> String {
    record
        .sources
        .iter()
        .find(|source| source.field == field as i32)
        .map(|source| {
            let who = if !source.person.is_empty() {
                format!(", set by {}", source.person)
            } else if !source.instance_id.is_empty() {
                format!(", reported by {}", source.instance_id)
            } else {
                String::new()
            };
            format!("{}{}", source.source, who)
        })
        .unwrap_or_default()
}

/// A record's page: its values with their sources, the form completing it
/// pre-filled with what is offered, the platform's answer when asked, and its
/// history (W3.10, W3.12, W3.3).
pub fn record_page(
    item: &InstrumentToComplete,
    versions: &[InstrumentVersion],
    asked: Option<&Result<AskPlatformForInstrumentReply, String>>,
    token: &str,
) -> String {
    let record = item.instrument.clone().unwrap_or_default();
    let id = escape(&record.instrument_id);

    let field_row = |label: &str, field: InstrumentField, value: String| {
        format!(
            "<tr><th>{label}</th><td>{}</td><td class=\"hint\">{}</td></tr>",
            escape(&value),
            escape(&source_words(&record, field))
        )
    };
    let identifiers: String = record
        .identifiers
        .iter()
        .map(|identifier| {
            let source = record
                .sources
                .iter()
                .find(|source| source.identifier.as_ref() == Some(identifier))
                .map(|source| {
                    if source.instance_id.is_empty() {
                        source.source.clone()
                    } else {
                        format!("reported by {}", source.instance_id)
                    }
                })
                .unwrap_or_default();
            format!(
                "<tr><th>Identifier</th><td>{}</td><td class=\"hint\">{}</td></tr>",
                escape(&identifier_words(identifier)),
                escape(&source)
            )
        })
        .collect();
    let values = format!(
        "<table class=\"list\"><tbody>{}{}{}{identifiers}</tbody></table>",
        field_row(
            "Asset class",
            InstrumentField::AssetClass,
            class_name(record.asset_class).to_string()
        ),
        field_row(
            "Currency",
            InstrumentField::Currency,
            record.currency.clone()
        ),
        field_row(
            "Description",
            InstrumentField::Description,
            record.description.clone()
        ),
    );

    // Pre-filled: the value in force and its source; otherwise what is
    // offered, with the offer's source, which a person changing the value
    // must change too.
    let prefill = |field: InstrumentField, held: String| -> (String, String, String) {
        if !held.is_empty() {
            return (held, source_words(&record, field), String::new());
        }
        match offer_for(&record, field).and_then(|offer| offer.value.as_ref()) {
            Some(value) => {
                let text = match &value.value {
                    Some(instrument_value::Value::AssetClass(class)) => class.to_string(),
                    _ => value_words(value),
                };
                (text, value.source.clone(), value.source.clone())
            }
            None => (String::new(), String::new(), String::new()),
        }
    };
    let held_class = if record.asset_class == 0 {
        String::new()
    } else {
        record.asset_class.to_string()
    };
    let (class, class_source, class_offer) = prefill(InstrumentField::AssetClass, held_class);
    let (currency, currency_source, currency_offer) =
        prefill(InstrumentField::Currency, record.currency.clone());
    let (description, description_source, description_offer) =
        prefill(InstrumentField::Description, record.description.clone());
    let options: String = std::iter::once("<option value=\"\">Not set</option>".to_string())
        .chain(CLASSES.iter().map(|(value, name)| {
            let value = (*value as i32).to_string();
            format!(
                "<option value=\"{value}\"{}>{name}</option>",
                if value == class { " selected" } else { "" }
            )
        }))
        .collect();
    let offered_attr = |offer: &str| {
        if offer.is_empty() {
            String::new()
        } else {
            format!(" data-offered-source=\"{}\"", escape(offer))
        }
    };
    let form = format!(
        "<form method=\"post\" action=\"/admin/instruments/complete\" class=\"panel\" id=\"complete\">{token}\
         <input type=\"hidden\" name=\"instrument_id\" value=\"{id}\">\
         <input type=\"hidden\" name=\"against_version\" value=\"{version}\">\
         <div class=\"fields\">\
         <label>Asset class <select name=\"asset_class\" data-initial=\"{class}\">{options}</select></label>\
         <label>Where it came from <input name=\"asset_class_source\" value=\"{class_source}\"{class_offer}></label>\
         <label>Currency <input name=\"currency\" maxlength=\"3\" pattern=\"[A-Za-z]{{3}}\" placeholder=\"USD\" value=\"{currency}\" data-initial=\"{currency}\"></label>\
         <label>Where it came from <input name=\"currency_source\" value=\"{currency_source}\"{currency_offer}></label>\
         <label>Description <input name=\"description\" value=\"{description}\" data-initial=\"{description}\"></label>\
         <label>Where it came from <input name=\"description_source\" value=\"{description_source}\"{description_offer}></label>\
         <label>Add an identifier: scheme <input name=\"identifier_scheme\" placeholder=\"figi\"></label>\
         <label>value <input name=\"identifier_value\"></label>\
         <label>namespace, for a source's own symbol <input name=\"identifier_namespace\" placeholder=\"empty for a global scheme\"></label>\
         <label>Where it came from <input name=\"identifier_source\"></label>\
         <label>Why, when you change a value already set <input name=\"note\"></label>\
         </div><div class=\"form-foot\"><button type=\"submit\" class=\"primary\">Save</button></div></form>",
        version = record.version,
        class = escape(&class),
        class_source = escape(&class_source),
        class_offer = offered_attr(&class_offer),
        currency = escape(&currency),
        currency_source = escape(&currency_source),
        currency_offer = offered_attr(&currency_offer),
        description = escape(&description),
        description_source = escape(&description_source),
        description_offer = offered_attr(&description_offer),
    );

    let asked_html = match asked {
        None => String::new(),
        Some(Err(why)) => format!(
            "<p class=\"refused\">The platform could not be asked: {}. Completing the record works \
             without it.</p>",
            escape(why)
        ),
        Some(Ok(reply)) if !reply.reachable => format!(
            "<p class=\"notice warn\">The platform could not be reached: {}. Completing the record \
             works without it.</p>",
            escape(&reply.detail)
        ),
        Some(Ok(reply)) if !reply.found => format!(
            "<p class=\"notice\">The platform holds no record by this record's open identifiers. {}</p>",
            escape(&reply.detail)
        ),
        Some(Ok(reply)) => {
            let platform = reply.instrument.clone().unwrap_or_default();
            format!(
                "<p class=\"notice good\">The platform holds {} ({}, {}, {}). Its ID joins this \
                 record as an identifier, and its values are offered below for you to accept; \
                 nothing it says replaces a value you set.</p>",
                escape(&platform.instrument_id),
                escape(class_name(platform.asset_class)),
                escape(&platform.currency),
                escape(&platform.description),
            )
        }
    };

    let history: String = versions
        .iter()
        .map(|version| {
            let changes = version
                .changes
                .iter()
                .map(|change| {
                    let field = match InstrumentField::try_from(change.field) {
                        Ok(InstrumentField::AssetClass) => "asset class".to_string(),
                        Ok(InstrumentField::Currency) => "currency".to_string(),
                        Ok(InstrumentField::Description) => "description".to_string(),
                        _ => change
                            .identifier
                            .as_ref()
                            .map(|id| id.scheme.clone())
                            .unwrap_or_else(|| "identifier".into()),
                    };
                    if change.before.is_empty() {
                        format!("{field}: {} ({})", change.after, change.source)
                    } else {
                        format!(
                            "{field}: {} to {} ({})",
                            change.before, change.after, change.source
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            let who = if !version.person.is_empty() {
                version.person.clone()
            } else {
                version.instance_id.clone()
            };
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                version.version,
                escape(&version.operation),
                escape(&changes),
                escape(&who),
                escape(&version.note),
            )
        })
        .collect();

    format!(
        "<div class=\"admin\"><div class=\"page-head\"><div><h1>{id}</h1>\
         <p class=\"hint\">Version {version}. {book}</p></div>\
         <form method=\"post\" action=\"/admin/instruments/{id}/ask\">{token}\
         <button type=\"submit\">Ask the platform</button></form></div>{asked_html}\
         <section class=\"admin-section\"><div class=\"section-head\"><div><h2>What it says</h2></div></div>{values}</section>\
         <section class=\"admin-section\"><div class=\"section-head\"><div><h2>Complete it</h2>\
         <p class=\"hint\">Each value says where it came from. A value offered is filled in with its \
         source; change the value and say where the new one came from.</p></div></div>{form}</section>\
         <section class=\"admin-section\"><div class=\"section-head\"><div><h2>History</h2></div></div>\
         <table class=\"list\"><thead><tr><th>Version</th><th>What</th><th>Changes</th><th>Who</th>\
         <th>Note</th></tr></thead><tbody>{history}</tbody></table></section></div>\
         <script>{FORM_SCRIPT}</script>",
        version = record.version,
        book = if item.complete_for_book {
            "The book can use it."
        } else {
            "The book cannot use it until it has an asset class and a currency."
        },
    )
}

/// A value changed from what was offered, with the offer's source still
/// beside it, is not sent: the person says where the new value came from.
const FORM_SCRIPT: &str = r##"(function () {
  var form = document.getElementById("complete");
  if (!form) return;
  form.addEventListener("submit", function (event) {
    var pairs = [["asset_class", "asset_class_source"], ["currency", "currency_source"], ["description", "description_source"]];
    for (var i = 0; i < pairs.length; i++) {
      var value = form.elements[pairs[i][0]];
      var source = form.elements[pairs[i][1]];
      var offered = source.getAttribute("data-offered-source");
      if (offered && value.value !== value.getAttribute("data-initial") && source.value === offered) {
        event.preventDefault();
        source.setCustomValidity("You changed the offered value: say where the new one came from.");
        source.reportValidity();
        source.addEventListener("input", function () { this.setCustomValidity(""); }, { once: true });
        return;
      }
    }
  });
})();"##;

/// What a completion's reply says, for the person: done, or each record's
/// refusal in words.
pub fn outcome(reply: &CompleteInstrumentsReply) -> Result<String, String> {
    let refused: Vec<String> = reply
        .results
        .iter()
        .filter(|result| result.refusal.is_some())
        .map(|result| format!("{}: {}", result.instrument_id, result.detail))
        .collect();
    let done = reply.results.len() - refused.len();
    if refused.is_empty() {
        Ok(format!(
            "{done} record{} completed",
            if done == 1 { "" } else { "s" }
        ))
    } else {
        Err(format!(
            "{done} completed; not done: {}",
            refused.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{InstrumentValueSource, LicensedIdentifierCount};

    fn waiting() -> InstrumentToComplete {
        InstrumentToComplete {
            instrument: Some(InstrumentRecord {
                instrument_id: "LCL-1".into(),
                identifiers: vec![Identifier {
                    scheme: "symbol".into(),
                    value: "SNAP1".into(),
                    source: "snaptrade".into(),
                }],
                version: 2,
                sources: vec![InstrumentValueSource {
                    field: InstrumentField::Identifier as i32,
                    identifier: Some(Identifier {
                        scheme: "symbol".into(),
                        value: "SNAP1".into(),
                        source: "snaptrade".into(),
                    }),
                    source: "reported by custody-snaptrade-1".into(),
                    instance_id: "custody-snaptrade-1".into(),
                    ..Default::default()
                }],
                offers: vec![OfferedValue {
                    value: Some(InstrumentValue {
                        value: Some(instrument_value::Value::AssetClass(
                            AssetClass::Equity as i32,
                        )),
                        source: "stated by custody-snaptrade-1".into(),
                    }),
                    instance_id: "custody-snaptrade-1".into(),
                    offered_at_ns: 1,
                }],
                ..Default::default()
            }),
            lacks: vec![
                InstrumentField::AssetClass as i32,
                InstrumentField::Currency as i32,
                InstrumentField::Description as i32,
            ],
            complete_for_book: false,
            complete: false,
        }
    }

    #[test]
    fn a_form_becomes_one_completion_against_its_version_each_value_with_its_source() {
        let request = completion(&Filled {
            instrument_id: "LCL-1".into(),
            against_version: "2".into(),
            asset_class: (AssetClass::Equity as i32).to_string(),
            asset_class_source: "Fidelity statement".into(),
            currency: "usd".into(),
            currency_source: "Fidelity statement".into(),
            ..Default::default()
        })
        .unwrap();
        let one = &request.completions[0];
        assert_eq!(one.against_version, 2);
        assert_eq!(one.values.len(), 2);
        assert_eq!(
            one.values[1].value,
            Some(instrument_value::Value::Currency("USD".into()))
        );
        assert!(one
            .values
            .iter()
            .all(|value| value.source == "Fidelity statement"));
        assert!(completion(&Filled {
            instrument_id: "LCL-1".into(),
            against_version: "2".into(),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn accepting_offers_takes_each_lacking_field_offered_with_the_offers_source() {
        let request = accepting(&[waiting()], &["LCL-1".into()]);
        assert_eq!(request.completions.len(), 1);
        let values = &request.completions[0].values;
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].source, "stated by custody-snaptrade-1");
        assert_eq!(request.completions[0].against_version, 2);
        assert!(accepting(&[waiting()], &[]).completions.is_empty());
    }

    #[test]
    fn the_list_counts_and_never_shows_an_account_or_a_quantity() {
        let listed = Ok(ListInstrumentsToCompleteReply {
            instruments: vec![waiting()],
            incomplete_for_book: 1,
            incomplete: 1,
            licensed_identifiers: vec![LicensedIdentifierCount {
                scheme: "cusip".into(),
                count: 512,
            }],
            ..Default::default()
        });
        let page = list_page(&listed, "", "");
        assert!(page.contains("the book cannot use it"));
        assert!(page.contains("threshold"));
        assert!(page.contains("at or past"));
        assert!(page.contains("href=\"/admin/instruments/LCL-1\""));
        assert!(!page.contains("ACC-"));
        let refused = list_page(&Err("no handler".into()), "", "");
        assert!(refused.contains("not answering"));
    }

    #[test]
    fn a_records_page_prefills_what_is_offered_with_its_source() {
        let page = record_page(&waiting(), &[], None, "");
        assert!(page.contains(&format!(
            "<option value=\"{}\" selected>Equity</option>",
            AssetClass::Equity as i32
        )));
        assert!(page.contains("value=\"stated by custody-snaptrade-1\""));
        assert!(page.contains("data-offered-source=\"stated by custody-snaptrade-1\""));
        assert!(page.contains("name=\"against_version\" value=\"2\""));
        assert!(page.contains("reported by custody-snaptrade-1"));
        assert!(page.contains("cannot use it"));
    }

    #[test]
    fn the_home_says_how_many_wait_and_nothing_when_none_do() {
        assert!(home_notice(Some(3)).contains("3 instrument records"));
        assert!(home_notice(Some(1)).contains("1 instrument record the book"));
        assert!(home_notice(Some(0)).is_empty());
        assert!(home_notice(None).is_empty());
    }
}
