//! The per-source view (ruling 728): an instrument, once picked, as each of
//! the deployment's sources has it, side by side, as a broker's screen shows
//! it -- the source's ticker, exchange and name for it, and its latest quote
//! or price with the source's declared delay; or that the source does not
//! cover it, with the lake's reason; or that it is not entitled.
//!
//! A source is a dataset serving quotes or prices (`meridian.v1.Quote`,
//! `meridian.v1.Price`), as the lake lists it (W10.9). Its line is read from
//! the lake (W10.10) as a plugin entitled to it that the person reads at a
//! data level -- the same bound `dashboard__query_lake` keeps -- so the view
//! shows a person no price their access does not already reach: a source no
//! such plugin is entitled to is "Not entitled". The ticker and the name are
//! what the source's own plugin reported for the record (its identifiers'
//! sources and its offers, W3.1); the exchange is the venue the latest row
//! names. It reads only, and changes no record. The page is
//! `/admin/instruments/sources`, a deployment admin's, with the one search's
//! picker on it (ruling 727); its tool is `dashboard__read_instrument_sources`.

use std::collections::BTreeMap;
use std::time::Duration;

use meridian_access::Access;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    InstrumentRecord, ListDatasetsReply, QueryLakeReply, QueryLakeRequest, ResolveVenueReply,
    ResolveVenueRequest, SourceChoice, SubjectRef, UnansweredReason,
};
use prost::Message;
use serde_json::{json, Value};

use crate::html::escape;

pub const QUERY_LAKE: &str = "platform.lake.query.query-lake";
const QUOTE: &str = "meridian.v1.Quote";
const PRICE: &str = "meridian.v1.Price";
const WAIT: Duration = Duration::from_secs(5);

/// What one source says of the instrument.
#[derive(Debug, Clone, PartialEq)]
pub enum Said {
    /// Its latest quote: bid and ask, each with its currency, and when.
    Quote {
        bid: String,
        ask: String,
        at_ns: i64,
    },
    /// Its latest price, of its kind (`last`, `close`, ...), and when.
    Price {
        price: String,
        kind: String,
        at_ns: i64,
    },
    /// It does not cover it, and why, in the lake's words.
    NotCovered(String),
    /// No plugin the person reads is entitled to the source.
    NotEntitled,
}

/// One source's line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub dataset: String,
    pub vendor: String,
    pub ticker: String,
    pub exchange: String,
    pub name: String,
    pub delay_ns: i64,
    pub said: Said,
}

async fn ask<R: Message + Default>(
    bus: &meridian_bus::Bus,
    topic: &str,
    request_type: &str,
    request: impl Message,
) -> Result<R, String> {
    let (_, bytes) = bus
        .call(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            Some(WAIT),
        )
        .await
        .map_err(|failed| match failed {
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

/// The sources: datasets serving quotes or prices, each with the kind its
/// line reads (quotes where it serves them) and the plugins entitled to it.
fn sources(listed: &ListDatasetsReply) -> Vec<(&meridian_domain::v1::DatasetRef, &'static str)> {
    let mut out: Vec<_> = listed
        .datasets
        .iter()
        .filter_map(|dataset| {
            let types = &dataset.declaration.as_ref()?.data_types;
            if types.iter().any(|t| t == QUOTE) {
                Some((dataset, "quotes"))
            } else if types.iter().any(|t| t == PRICE) {
                Some((dataset, "prices"))
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|a, b| a.0.dataset.cmp(&b.0.dataset));
    out
}

/// The plugin a source is read as: one entitled to it that the person reads
/// at a data level; None where there is none.
fn reader_for<'a>(
    listed: &'a ListDatasetsReply,
    dataset: &str,
    access: &Access,
) -> Option<&'a str> {
    listed
        .entitlements
        .iter()
        .filter(|e| e.dataset == dataset && e.allowed)
        .map(|e| e.instance.as_str())
        .find(|instance| {
            access
                .plugin(instance)
                .roles
                .values()
                .any(|held| held.data.is_some())
        })
}

/// What the source's own plugin reported for the record: its ticker (or
/// symbol) and its name; empty where it reported none.
fn reported(record: &InstrumentRecord, instance: &str) -> (String, String) {
    let ticker = record
        .sources
        .iter()
        .filter(|s| s.instance_id == instance)
        .filter_map(|s| s.identifier.as_ref())
        .chain(
            record
                .offers
                .iter()
                .filter(|o| o.instance_id == instance)
                .filter_map(|o| o.value.as_ref())
                .filter_map(|v| match &v.value {
                    Some(meridian_domain::v1::instrument_value::Value::Identifier(i)) => Some(i),
                    _ => None,
                }),
        )
        .find(|i| i.scheme == "ticker" || i.scheme == "symbol")
        .map(|i| i.value.clone())
        .unwrap_or_default();
    let name = record
        .offers
        .iter()
        .filter(|o| o.instance_id == instance)
        .filter_map(|o| o.value.as_ref())
        .find_map(|v| match &v.value {
            Some(meridian_domain::v1::instrument_value::Value::Description(d)) => Some(d.clone()),
            _ => None,
        })
        .or_else(|| {
            record
                .sources
                .iter()
                .any(|s| {
                    s.instance_id == instance
                        && s.field == meridian_domain::v1::InstrumentField::Description as i32
                })
                .then(|| record.description.clone())
        })
        .unwrap_or_default();
    (ticker, name)
}

fn money(value: Option<&meridian_domain::v1::Money>) -> String {
    value
        .and_then(|m| {
            let amount = Exact::from_wire(m.amount.as_ref()?).ok()?;
            Some(format!("{amount} {}", m.currency_code).trim().to_string())
        })
        .unwrap_or_default()
}

fn reason_words(reason: i32) -> &'static str {
    match UnansweredReason::try_from(reason) {
        Ok(UnansweredReason::NotEntitled) => "not entitled to it",
        Ok(UnansweredReason::NotCovered) => "it does not cover this instrument",
        Ok(UnansweredReason::AskedSource) => "asked its source; nothing recorded yet",
        Ok(UnansweredReason::SourceSilent) => "nothing recorded within its cadence",
        Ok(UnansweredReason::Unresolved) => "the instrument is not one it resolves",
        Ok(UnansweredReason::BeyondHistory) => "beyond the history it reaches",
        Ok(UnansweredReason::NotKept) => "served, not kept",
        _ => "no answer",
    }
}

/// One source's latest row, read as `reader`; and the venue it names.
fn said_of(reply: &QueryLakeReply, kind: &str) -> (Said, String) {
    let latest = |at: i64, venue: &str| (at, venue.to_string());
    if kind == "quotes" {
        if let Some(quote) = reply
            .quotes
            .iter()
            .max_by_key(|q| q.meta.as_ref().map_or(0, |m| m.valid_from_ns))
        {
            let meta = quote.meta.clone().unwrap_or_default();
            let (at_ns, venue) = latest(
                meta.valid_from_ns,
                &meta.source.map(|s| s.venue_id).unwrap_or_default(),
            );
            return (
                Said::Quote {
                    bid: money(quote.bid.as_ref()),
                    ask: money(quote.ask.as_ref()),
                    at_ns,
                },
                venue,
            );
        }
    } else if let Some(price) = reply
        .prices
        .iter()
        .max_by_key(|p| p.meta.as_ref().map_or(0, |m| m.valid_from_ns))
    {
        let meta = price.meta.clone().unwrap_or_default();
        let (at_ns, venue) = latest(
            meta.valid_from_ns,
            &meta.source.map(|s| s.venue_id).unwrap_or_default(),
        );
        return (
            Said::Price {
                price: money(price.price.as_ref()),
                kind: crate::admin::data_sources::kind_word(price.kind).to_string(),
                at_ns,
            },
            venue,
        );
    }
    let why = reply
        .unanswered
        .first()
        .map(|u| reason_words(u.reason))
        .unwrap_or("it holds no row for this instrument");
    (Said::NotCovered(why.to_string()), String::new())
}

/// A venue by its MIC, as a person reads an exchange; its ID where it is
/// not held.
async fn exchange(
    bus: &meridian_bus::Bus,
    venue_id: &str,
    known: &mut BTreeMap<String, String>,
) -> String {
    if venue_id.is_empty() {
        return String::new();
    }
    if let Some(words) = known.get(venue_id) {
        return words.clone();
    }
    let found: Result<ResolveVenueReply, String> = ask(
        bus,
        crate::admin::calendars::RESOLVE_VENUE,
        "meridian.v1.ResolveVenueRequest",
        ResolveVenueRequest {
            venue_id: venue_id.to_string(),
            ..Default::default()
        },
    )
    .await;
    let words = found
        .ok()
        .and_then(|f| f.venue)
        .and_then(|venue| {
            venue
                .identifiers
                .iter()
                .find(|i| i.scheme == "iso10383" && i.valid_until_ns == 0)
                .map(|i| i.value.clone())
                .or_else(|| (!venue.name.is_empty()).then(|| venue.name.clone()))
        })
        .unwrap_or_else(|| venue_id.to_string());
    known.insert(venue_id.to_string(), words.clone());
    words
}

/// Each source's line for a record, side by side.
pub async fn lines(
    bus: &meridian_bus::Bus,
    subject: &str,
    access: &Access,
    record: &InstrumentRecord,
) -> Result<Vec<Line>, String> {
    let listed = crate::admin::data_sources::datasets(bus, subject).await?;
    let mut venues = BTreeMap::new();
    let mut out = Vec::new();
    for (dataset, kind) in sources(&listed) {
        let (ticker, name) = reported(record, &dataset.instance);
        let mut line = Line {
            dataset: dataset.dataset.clone(),
            vendor: dataset.vendor.clone(),
            ticker,
            exchange: String::new(),
            name,
            delay_ns: dataset.declared_delay_ns,
            said: Said::NotEntitled,
        };
        if let Some(reader) = reader_for(&listed, &dataset.dataset, access) {
            let reply: Result<QueryLakeReply, String> = ask(
                bus,
                QUERY_LAKE,
                "meridian.v1.QueryLakeRequest",
                QueryLakeRequest {
                    kind: kind.to_string(),
                    subjects: vec![SubjectRef {
                        entity_id: record.instrument_id.clone(),
                        ..Default::default()
                    }],
                    sources: Some(SourceChoice {
                        named: vec![dataset.dataset.clone()],
                        ..Default::default()
                    }),
                    row_limit: 1,
                    instance_id: reader.to_string(),
                    ..Default::default()
                },
            )
            .await;
            let (said, venue) = match reply {
                Ok(reply) => said_of(&reply, kind),
                Err(why) => (Said::NotCovered(why), String::new()),
            };
            line.exchange = exchange(bus, &venue, &mut venues).await;
            line.said = said;
        }
        out.push(line);
    }
    Ok(out)
}

/// A declared delay in words: real time, or minutes.
pub fn delay_words(delay_ns: i64) -> String {
    if delay_ns <= 0 {
        return "real time".into();
    }
    let minutes = delay_ns / 60_000_000_000;
    if minutes >= 1 {
        format!("{minutes} min delayed")
    } else {
        format!("{} s delayed", delay_ns / 1_000_000_000)
    }
}

/// The lines as the tool answers them.
pub fn lines_json(lines: &[Line]) -> Value {
    Value::Array(
        lines
            .iter()
            .map(|line| {
                let (state, said) = match &line.said {
                    Said::Quote { bid, ask, at_ns } => (
                        "quote",
                        json!({"bid": bid, "ask": ask, "at_ns": at_ns.to_string()}),
                    ),
                    Said::Price { price, kind, at_ns } => (
                        "price",
                        json!({"price": price, "kind": kind, "at_ns": at_ns.to_string()}),
                    ),
                    Said::NotCovered(why) => ("not_covered", json!({"why": why})),
                    Said::NotEntitled => ("not_entitled", json!({})),
                };
                json!({
                    "dataset": line.dataset, "vendor": line.vendor, "ticker": line.ticker,
                    "exchange": line.exchange, "name": line.name,
                    "declared_delay_ns": line.delay_ns.to_string(),
                    "state": state, "said": said,
                })
            })
            .collect(),
    )
}

fn when(at_ns: i64) -> String {
    meridian_domain::instrument_type::utc_minute(at_ns)
}

/// The page's body: the one search's picker, and the record's sources.
pub fn page(
    record: Option<&InstrumentRecord>,
    lines: Option<&Result<Vec<Line>, String>>,
    notice: &str,
) -> String {
    let picker = "<form method=\"get\" action=\"/admin/instruments/sources\" class=\"field-row\" \
         id=\"pick-instrument\"><om-instrument-picker name=\"instrument\" label=\"Instrument\" \
         src=\"/admin/instruments/search?candidates=1\" bring-in=\"/admin/instruments/bring-in\" \
         placeholder=\"Ticker, ISIN, FIGI or name\" style=\"flex: 1 1 16rem\"></om-instrument-picker>\
         <button type=\"submit\" class=\"primary\">Show sources</button></form>";
    let notice = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"refused\">{}</p>", escape(notice))
    };
    let body = match (record, lines) {
        (Some(record), Some(Ok(lines))) => {
            let head = format!(
                "<p class=\"hint\" data-instrument=\"{id}\"><strong>{name}</strong> {class}{currency}</p>",
                id = escape(&record.instrument_id),
                name = escape(&crate::admin::instruments::name_of(record)),
                class = escape(crate::admin::instruments::class_name(record.asset_class)),
                currency = if record.currency.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", escape(&record.currency))
                },
            );
            if lines.is_empty() {
                format!("{head}<p class=\"empty\">No source serves quotes or prices yet.</p>")
            } else {
                let rows: String = lines.iter().map(row).collect();
                format!(
                    "{head}<om-pager><table class=\"list one-line\" id=\"sources-table\">\
                     <caption class=\"visually-hidden\">Each source's view of the instrument</caption>\
                     <thead><tr><th>Source</th><th>Ticker</th><th class=\"wide\">Exchange</th>\
                     <th class=\"wide\">Name</th><th>Latest</th><th class=\"wide\">Delay</th></tr></thead>\
                     <tbody>{rows}</tbody></table></om-pager>"
                )
            }
        }
        (Some(_), Some(Err(why))) => format!(
            "<p class=\"refused\">The sources could not be read: {}</p>",
            escape(why)
        ),
        _ => "<p class=\"hint\">Find an instrument by its ticker, a name, an ISIN or a FIGI; \
              each source's view of it shows here, side by side.</p>"
            .to_string(),
    };
    format!(
        "<div class=\"admin\"><div class=\"page-head\"><div><h1>Sources</h1>\
         <p class=\"hint\">An instrument as each of your sources has it.</p></div></div>\
         {notice}{picker}<section class=\"admin-section\" id=\"sources\">{body}</section></div>"
    )
}

fn row(line: &Line) -> String {
    let source = if line.vendor.is_empty() {
        line.dataset.clone()
    } else {
        format!("{} ({})", line.dataset, line.vendor)
    };
    let (latest, state) = match &line.said {
        Said::Quote { bid, ask, at_ns } => (format!("{bid} / {ask} at {}", when(*at_ns)), "quote"),
        Said::Price { price, kind, at_ns } => {
            (format!("{price} {kind} at {}", when(*at_ns)), "price")
        }
        Said::NotCovered(why) => (format!("Doesn't cover it: {why}"), "not-covered"),
        Said::NotEntitled => ("Not entitled".to_string(), "not-entitled"),
    };
    let dash = |text: &str| {
        if text.is_empty() {
            "—".to_string()
        } else {
            escape(text)
        }
    };
    format!(
        "<tr data-source=\"{dataset}\" data-state=\"{state}\"><td title=\"{source}\">{source}</td>\
         <td title=\"{ticker}\">{ticker}</td><td class=\"wide\" title=\"{exchange}\">{exchange}</td>\
         <td class=\"wide\" title=\"{name}\">{name}</td>\
         <td title=\"{latest}\">{latest}</td><td class=\"wide\" title=\"{delay}\">{delay}</td></tr>",
        dataset = escape(&line.dataset),
        source = escape(&source),
        ticker = dash(&line.ticker),
        exchange = dash(&line.exchange),
        name = dash(&line.name),
        latest = escape(&latest),
        delay = escape(&delay_words(line.delay_ns)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{DatasetEntitlement, DatasetRef, Quote, Unanswered};
    use meridian_pb::v1::DatasetDeclaration;

    fn dataset(id: &str, types: &[&str]) -> DatasetRef {
        DatasetRef {
            dataset: id.into(),
            instance: id.split(':').next().unwrap().into(),
            vendor: "Vendor".into(),
            declaration: Some(DatasetDeclaration {
                data_types: types.iter().map(|t| t.to_string()).collect(),
                ..Default::default()
            }),
            declared_delay_ns: 900_000_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn a_source_is_a_dataset_serving_quotes_or_prices_quotes_first() {
        let listed = ListDatasetsReply {
            datasets: vec![
                dataset("b-1:eod", &[PRICE]),
                dataset("a-1:quotes", &[QUOTE, PRICE]),
                dataset("c-1:rates", &["meridian.v1.Rate"]),
            ],
            ..Default::default()
        };
        let found: Vec<_> = sources(&listed)
            .into_iter()
            .map(|(d, kind)| (d.dataset.as_str(), kind))
            .collect();
        assert_eq!(found, vec![("a-1:quotes", "quotes"), ("b-1:eod", "prices")]);
    }

    #[test]
    fn a_source_no_plugin_the_person_reads_is_entitled_to_is_not_read() {
        let listed = ListDatasetsReply {
            datasets: vec![dataset("a-1:quotes", &[QUOTE])],
            entitlements: vec![DatasetEntitlement {
                dataset: "a-1:quotes".into(),
                instance: "signal-1".into(),
                allowed: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(reader_for(&listed, "a-1:quotes", &Access::default()), None);
    }

    #[test]
    fn the_latest_quote_or_the_lakes_reason_is_what_a_source_says() {
        let quote = |at: i64, bid: &str| Quote {
            meta: Some(meridian_domain::v1::ObservationMeta {
                valid_from_ns: at,
                ..Default::default()
            }),
            bid: Some(meridian_domain::v1::Money {
                amount: Some(bid.parse::<Exact>().unwrap().to_wire()),
                currency_code: "USD".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let reply = QueryLakeReply {
            quotes: vec![quote(1, "1.00"), quote(2, "2.00")],
            ..Default::default()
        };
        match said_of(&reply, "quotes").0 {
            Said::Quote { bid, at_ns, .. } => {
                assert_eq!((bid.as_str(), at_ns), ("2.00 USD", 2));
            }
            other => panic!("{other:?}"),
        }
        let none = QueryLakeReply {
            unanswered: vec![Unanswered {
                reason: UnansweredReason::NotCovered as i32,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            said_of(&none, "quotes").0,
            Said::NotCovered("it does not cover this instrument".into())
        );
        assert_eq!(delay_words(0), "real time");
        assert_eq!(delay_words(900_000_000_000), "15 min delayed");
    }
}
