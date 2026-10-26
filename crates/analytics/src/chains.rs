//! A job's chains and scenario set, resolved on its client's behalf into
//! the inputs document (contract v28; W12.1, W12.2, W12.8): for each chain
//! the underlying's record and price at the as-of, the declared dividends,
//! every listed series in its window (`ListSeries`) with its quote, its open
//! interest (`ListStatistics`) and its settlement instant; and the scenario
//! set, inline or saved (`ListScenarios`).
//!
//! **A series' settlement instant** (ruling 570 (i)): AM at its session's
//! open and PM at 16:00, in the zone of the trading-sessions calendar that
//! names the row's venue -- the quote's venue, else its dataset's -- and,
//! for a row with no venue, the dataset's declared `calendar_id`; where
//! neither holds sessions, a PM series settles at 16:00 in the dataset's
//! declared day time zone, and a series with no zone at all is answered not
//! evaluated, naming why, never at a guessed hour.
//!
//! Nothing is minted: a series not held is named by its key (ruling 561).

use std::collections::{BTreeMap, BTreeSet};

use meridian_analytics_engine::compile::Compiled;
use meridian_analytics_engine::scenario;
use meridian_bus::{refusal_with_status, RefusalStatus};
use meridian_domain::date::Date;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    self as v1, ChainRef, CorporateActionKind, DatasetRef, Identifier, InstrumentType, Job,
    ListCorporateActionsReply, ListCorporateActionsRequest, ListQuotesReply, ListQuotesRequest,
    ListScenariosReply, ListScenariosRequest, ListSeriesReply, ListSeriesRequest,
    ListStatisticsReply, ListStatisticsRequest, PriceKind, QualityState, ReadCalendarReply,
    ReadCalendarRequest, ResolveInstrumentReply, ResolveInstrumentRequest, StatisticKind,
    SubjectRef,
};
use meridian_domain::zone::Zone;
use prost::Message;
use serde_json::{json, Value};

use crate::resolve::{Ask, Refused};
use crate::topics;

const PAGE: u32 = 500;
const DAY_NS: i64 = 86_400_000_000_000;

fn ask<Q: Message, R: Message + Default>(
    asker: &dyn Ask,
    topic: &str,
    kind: &str,
    request: &Q,
) -> Result<R, String> {
    let (reply_type, payload) = asker.ask(topic, kind, request.encode_to_vec())?;
    R::decode(&payload[..]).map_err(|failed| format!("{reply_type} did not read: {failed}"))
}

fn subject(id: &str) -> Vec<SubjectRef> {
    vec![SubjectRef {
        entity_id: id.to_string(),
        ..Default::default()
    }]
}

fn text_of(decimal: Option<&meridian_pb::v1::Decimal>) -> Option<String> {
    decimal
        .and_then(|d| Exact::from_wire(d).ok())
        .map(|e| e.to_string())
}

fn key_of(key: &[Identifier]) -> Vec<(String, String)> {
    key.iter()
        .map(|i| (i.scheme.clone(), i.value.clone()))
        .collect()
}

fn key_json(key: &[(String, String)]) -> Value {
    Value::Array(
        key.iter()
            .map(|(s, v)| json!({"scheme": s, "value": v}))
            .collect(),
    )
}

fn resolve_instrument(asker: &dyn Ask, id: &str, as_of: i64) -> Option<v1::InstrumentRecord> {
    let reply: ResolveInstrumentReply = ask(
        asker,
        topics::RESOLVE_INSTRUMENT,
        "meridian.v1.ResolveInstrumentRequest",
        &ResolveInstrumentRequest {
            instrument_id: id.to_string(),
            as_of_ns: as_of,
        },
    )
    .ok()?;
    reply.instrument.filter(|_| reply.found)
}

/// The chains a job reads: those it names, and -- for the scenario grid --
/// each underlying of an option an account holds, over its held expiries.
pub fn refs(
    asker: &dyn Ask,
    job: &Job,
    compiled: &Compiled,
    held: &BTreeSet<String>,
    as_of: i64,
) -> Vec<ChainRef> {
    let mut out = job.chains.clone();
    if !compiled.reads.held_chains {
        return out;
    }
    let mut wanted: BTreeMap<String, (String, String)> = BTreeMap::new();
    for id in held {
        let Some(record) = resolve_instrument(asker, id, as_of) else {
            continue;
        };
        let Some(option) = record.listed_option else {
            continue;
        };
        let window = wanted
            .entry(option.underlying_instrument_id.clone())
            .or_insert((
                option.expiration_date.clone(),
                option.expiration_date.clone(),
            ));
        if option.expiration_date < window.0 {
            window.0 = option.expiration_date.clone();
        }
        if option.expiration_date > window.1 {
            window.1 = option.expiration_date.clone();
        }
    }
    for (underlying, (from, to)) in wanted {
        if out.iter().any(|c| c.underlying_instrument_id == underlying) {
            continue;
        }
        out.push(ChainRef {
            underlying_instrument_id: underlying,
            expiration_from: from,
            expiration_to: to,
            series_keys: Vec::new(),
        });
    }
    out
}

/// A field of a listed series' terms, by its path.
fn field<'a>(series: &'a v1::ListedSeries, path: &str) -> &'a str {
    series
        .fields
        .iter()
        .find(|f| f.path == path)
        .map(|f| f.value.as_str())
        .unwrap_or_default()
}

/// A decimal amount from a term's text: `230.00 USD` reads 230.00.
fn amount(text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    first.parse::<Exact>().ok().map(|e| e.to_string())
}

/// The sessions a row's venue or dataset names, and their zone.
struct Sessions {
    zone: Option<Zone>,
    zone_name: String,
    sessions: Option<v1::TradingSessions>,
    why: String,
}

fn sessions(
    asker: &dyn Ask,
    venue: &str,
    declaration: Option<&meridian_pb::v1::DatasetDeclaration>,
    as_of: i64,
    cache: &mut BTreeMap<String, Option<v1::TradingSessions>>,
) -> Sessions {
    let mut read = |calendar: &str| -> Option<v1::TradingSessions> {
        if calendar.is_empty() {
            return None;
        }
        cache
            .entry(calendar.to_string())
            .or_insert_with(|| {
                let reply: ReadCalendarReply = ask(
                    asker,
                    topics::READ_CALENDAR,
                    "meridian.v1.ReadCalendarRequest",
                    &ReadCalendarRequest {
                        calendar_id: calendar.to_string(),
                        as_of_ns: as_of,
                        ..Default::default()
                    },
                )
                .ok()?;
                reply.trading_sessions.filter(|_| reply.found)
            })
            .clone()
    };
    let venue = if venue.is_empty() {
        declaration.map(|d| d.venue_id.as_str()).unwrap_or_default()
    } else {
        venue
    };
    let found = read(venue).or_else(|| {
        read(
            declaration
                .map(|d| d.calendar_id.as_str())
                .unwrap_or_default(),
        )
    });
    let zone_name = found
        .as_ref()
        .map(|s| s.time_zone.clone())
        .filter(|z| !z.is_empty())
        .or_else(|| {
            declaration
                .map(|d| d.day_time_zone.clone())
                .filter(|z| !z.is_empty())
        })
        .unwrap_or_default();
    let zone = if zone_name.is_empty() {
        None
    } else {
        Zone::load(&zone_name).ok()
    };
    let why = if zone.is_none() {
        format!(
            "no trading sessions for venue {:?} or its dataset's calendar, and no day time zone",
            venue
        )
    } else {
        String::new()
    };
    Sessions {
        zone,
        zone_name,
        sessions: found,
        why,
    }
}

/// A series' settlement instant: AM at the session's open, PM at 16:00 in
/// the venue's zone (W12.1).
fn settles_at(expiry: &str, am: bool, held: &Sessions) -> Result<i64, String> {
    let date = Date::parse(expiry).map_err(|_| format!("expiration_date {expiry:?}"))?;
    let zone = held.zone.as_ref().ok_or_else(|| held.why.clone())?;
    let minute = if am {
        let weekday = meridian_domain::sessions::weekday(date);
        held.sessions
            .as_ref()
            .and_then(|s| {
                s.sessions
                    .iter()
                    .filter(|x| !x.extended && x.weekdays.contains(&weekday))
                    .map(|x| x.start_minute)
                    .min()
            })
            .ok_or_else(|| {
                format!(
                    "an AM series needs its session's open; none held for {} in {}",
                    expiry, held.zone_name
                )
            })?
    } else {
        16 * 60
    };
    Ok(zone.instant_of(date, i64::from(minute) * 60))
}

fn source_ns(meta: &v1::ObservationMeta) -> i64 {
    meta.source_times
        .iter()
        .find(|t| t.kind == v1::SourceTimeKind::Event as i32)
        .map(|t| t.at_ns)
        .or_else(|| meta.source_times.iter().map(|t| t.at_ns).min())
        .unwrap_or(meta.valid_from_ns)
}

/// One chain, resolved into the document's form.
#[allow(clippy::too_many_lines)]
pub fn chain(
    asker: &dyn Ask,
    chain: &ChainRef,
    sources: &Option<v1::SourceChoice>,
    as_of: i64,
    rate: Option<&Value>,
    calendars: &mut BTreeMap<String, Option<v1::TradingSessions>>,
    live: bool,
) -> Result<Value, Refused> {
    let underlying = chain.underlying_instrument_id.as_str();
    let record = resolve_instrument(asker, underlying, as_of);
    let kind = match record
        .as_ref()
        .map(|r| InstrumentType::try_from(r.instrument_type).unwrap_or(InstrumentType::Unspecified))
    {
        Some(InstrumentType::Future) => "future",
        Some(InstrumentType::Index) => "index",
        _ => "equity",
    };
    // The listed series in the window (W10.6), paged.
    let mut series: Vec<v1::ListedSeries> = Vec::new();
    let mut declarations: BTreeMap<String, meridian_pb::v1::DatasetDeclaration> = BTreeMap::new();
    let learn = |refs: &[DatasetRef],
                 held: &mut BTreeMap<String, meridian_pb::v1::DatasetDeclaration>| {
        for d in refs {
            if let Some(declaration) = &d.declaration {
                held.entry(d.dataset.clone()).or_insert(declaration.clone());
            }
        }
    };
    let mut cursor = String::new();
    loop {
        let reply: ListSeriesReply = ask(
            asker,
            topics::LIST_SERIES,
            "meridian.v1.ListSeriesRequest",
            &ListSeriesRequest {
                subjects: subject(underlying),
                expiration_from: chain.expiration_from.clone(),
                expiration_to: chain.expiration_to.clone(),
                series_keys: chain.series_keys.clone(),
                sources: sources.clone(),
                as_of_ns: as_of,
                page_size: PAGE,
                cursor: cursor.clone(),
                ..Default::default()
            },
        )
        .map_err(|failed| {
            format!("the lake's series of {underlying} could not be read: {failed}")
        })?;
        learn(&reply.datasets, &mut declarations);
        series.extend(reply.series);
        if reply.next_cursor.is_empty() {
            break;
        }
        cursor = reply.next_cursor;
    }
    // Each series' latest quote at the as-of, by its key. Contract v28
    // (W12.2; rulings 421 and 619): an invalid quote is never used; where a
    // series found none and a dataset was source silent for the underlying,
    // that dataset is read again by name with `include_quarantined`, and a
    // series whose quote in force there is invalid is quarantined, naming it.
    let mut quotes: BTreeMap<Vec<(String, String)>, v1::Quote> = BTreeMap::new();
    let mut invalid: BTreeMap<Vec<(String, String)>, (i64, v1::Source)> = BTreeMap::new();
    let mut silent: BTreeSet<String> = BTreeSet::new();
    let mut quotes_refused = String::new();
    let mut read_quotes = |sources: &Option<v1::SourceChoice>,
                           quotes: &mut BTreeMap<Vec<(String, String)>, v1::Quote>,
                           invalid: &mut BTreeMap<Vec<(String, String)>, (i64, v1::Source)>,
                           silent: &mut BTreeSet<String>|
     -> Result<(), String> {
        let mut cursor = String::new();
        loop {
            let reply: ListQuotesReply = ask(
                asker,
                topics::LIST_QUOTES,
                "meridian.v1.ListQuotesRequest",
                &ListQuotesRequest {
                    subjects: subject(underlying),
                    sources: sources.clone(),
                    at_ns: as_of,
                    as_of_ns: as_of,
                    page_size: PAGE,
                    cursor: cursor.clone(),
                    series_keys: chain.series_keys.clone(),
                    all_series: chain.series_keys.is_empty(),
                    ..Default::default()
                },
            )?;
            learn(&reply.datasets, &mut declarations);
            for missing in &reply.unanswered {
                if missing.reason == v1::UnansweredReason::SourceSilent as i32
                    && !missing.dataset.is_empty()
                {
                    silent.insert(missing.dataset.clone());
                }
            }
            for quote in reply.quotes {
                let Some(meta) = &quote.meta else { continue };
                let key = key_of(&meta.series_key);
                if meta.quality == QualityState::Invalid as i32 {
                    let newer = invalid
                        .get(&key)
                        .is_none_or(|(at, _)| meta.valid_from_ns >= *at);
                    if newer {
                        invalid.insert(
                            key,
                            (meta.valid_from_ns, meta.source.clone().unwrap_or_default()),
                        );
                    }
                    continue;
                }
                if live {
                    // Read at now (as_of is now): its age at this hop (606).
                    crate::resolve::age_at_analytics(meta, as_of);
                }
                let newer = quotes
                    .get(&key)
                    .and_then(|q| q.meta.as_ref())
                    .is_none_or(|held| meta.valid_from_ns >= held.valid_from_ns);
                if newer {
                    quotes.insert(key, quote);
                }
            }
            if reply.next_cursor.is_empty() {
                return Ok(());
            }
            cursor = reply.next_cursor;
        }
    };
    if let Err(failed) = read_quotes(sources, &mut quotes, &mut invalid, &mut silent) {
        quotes_refused = format!("the chain's quotes could not be read: {failed}");
    }
    if quotes_refused.is_empty() {
        let unquoted = series
            .iter()
            .filter_map(|listed| listed.meta.as_ref())
            .any(|meta| !quotes.contains_key(&key_of(&meta.series_key)));
        for dataset in silent.clone() {
            if !unquoted {
                break;
            }
            let named = Some(v1::SourceChoice {
                named: vec![dataset],
                include_quarantined: true,
                ..Default::default()
            });
            let mut ignored = BTreeSet::new();
            let mut found = BTreeMap::new();
            // A quarantine read that fails leaves the series unquoted, as
            // before: never a refusal of the chain.
            if read_quotes(&named, &mut found, &mut invalid, &mut ignored).is_ok() {
                // A valid quote in force there is not the default's to
                // answer (another dataset stood first): leave it unused.
                for key in found.keys() {
                    if let Some((at, _)) = invalid.get(key) {
                        let valid_at = found[key].meta.as_ref().map_or(0, |m| m.valid_from_ns);
                        if valid_at >= *at {
                            invalid.remove(key);
                        }
                    }
                }
            }
        }
    }
    // Open interest per series on the latest trading date and the one
    // before, and the underlying's volume, from the lake's statistics.
    let today = Date::of_utc_instant(as_of);
    let from = Date::of_utc_instant(as_of - 14 * DAY_NS).to_string();
    let mut interest: BTreeMap<Vec<(String, String)>, BTreeMap<String, String>> = BTreeMap::new();
    let mut volume: BTreeMap<String, String> = BTreeMap::new();
    let mut stats_datasets: BTreeSet<String> = BTreeSet::new();
    let mut cursor = String::new();
    loop {
        let read: Result<ListStatisticsReply, String> = ask(
            asker,
            topics::LIST_STATISTICS,
            "meridian.v1.ListStatisticsRequest",
            &ListStatisticsRequest {
                subjects: subject(underlying),
                kinds: vec![
                    StatisticKind::OpenInterest as i32,
                    StatisticKind::Volume as i32,
                ],
                from_date: from.clone(),
                to_date: today.to_string(),
                series_keys: chain.series_keys.clone(),
                all_series: chain.series_keys.is_empty(),
                sources: sources.clone(),
                as_of_ns: as_of,
                page_size: PAGE,
                cursor: cursor.clone(),
                ..Default::default()
            },
        );
        let Ok(reply) = read else { break };
        for statistic in reply.statistics {
            let Some(meta) = &statistic.meta else {
                continue;
            };
            let Some(value) = statistic
                .value
                .as_ref()
                .and_then(|v| text_of(v.value.as_ref()))
            else {
                continue;
            };
            if let Some(source) = &meta.source {
                stats_datasets.insert(source.dataset.clone());
            }
            let key = key_of(&meta.series_key);
            match (StatisticKind::try_from(statistic.kind), key.is_empty()) {
                (Ok(StatisticKind::OpenInterest), false) => {
                    interest
                        .entry(key)
                        .or_default()
                        .insert(meta.business_date.clone(), value);
                }
                (Ok(StatisticKind::Volume), true) => {
                    volume.insert(meta.business_date.clone(), value);
                }
                _ => {}
            }
        }
        if reply.next_cursor.is_empty() {
            break;
        }
        cursor = reply.next_cursor;
    }
    // The underlying's price at the as-of: its latest mid, last or close.
    let price_reply: Result<v1::ListPricesReply, String> = ask(
        asker,
        topics::LIST_PRICES,
        "meridian.v1.ListPricesRequest",
        &v1::ListPricesRequest {
            subjects: subject(underlying),
            kinds: vec![
                PriceKind::Mid as i32,
                PriceKind::Last as i32,
                PriceKind::Close as i32,
            ],
            sources: sources.clone(),
            at_ns: as_of,
            as_of_ns: as_of,
            page_size: PAGE,
            ..Default::default()
        },
    );
    let spot = price_reply.ok().and_then(|reply| {
        reply
            .prices
            .into_iter()
            .filter_map(|p| {
                let meta = p.meta.clone()?;
                let money = p.price.clone()?;
                Some((meta, money, p.kind))
            })
            // The latest; at one instant a mid before a last before a close.
            .max_by_key(|(meta, _, kind)| {
                let rank = match PriceKind::try_from(*kind) {
                    Ok(PriceKind::Mid) => 3,
                    Ok(PriceKind::Last) => 2,
                    _ => 1,
                };
                (meta.valid_from_ns, rank)
            })
    });
    // Declared cash dividends going ex after the as-of (W10.4, the filings
    // slice); none where the lake holds none.
    let mut dividends: Vec<Value> = Vec::new();
    let mut dividends_dataset = String::new();
    let actions: Result<ListCorporateActionsReply, String> = ask(
        asker,
        topics::LIST_CORPORATE_ACTIONS,
        "meridian.v1.ListCorporateActionsRequest",
        &ListCorporateActionsRequest {
            subjects: subject(underlying),
            from_date: today.to_string(),
            kinds: vec![CorporateActionKind::CashDividend as i32],
            sources: sources.clone(),
            as_of_ns: as_of,
            page_size: PAGE,
            ..Default::default()
        },
    );
    if let Ok(reply) = actions {
        for action in reply.actions {
            let (Ok(ex), Some(per)) = (
                Date::parse(&action.ex_date),
                action
                    .amount_per_share
                    .as_ref()
                    .and_then(|m| text_of(m.amount.as_ref())),
            ) else {
                continue;
            };
            if let Some(source) = action.meta.as_ref().and_then(|m| m.source.as_ref()) {
                dividends_dataset = source.dataset.clone();
            }
            dividends.push(json!({
                "ex_date": action.ex_date,
                // Ex at the open: midnight UTC of its date is the bound the
                // tree needs, a day's precision.
                "ex_ns": (ex.days_since_epoch() * DAY_NS).to_string(),
                "amount": per,
            }));
        }
    }
    let mut datasets: BTreeSet<String> = declarations.keys().cloned().collect();
    datasets.extend(stats_datasets);
    let mut today_local: Option<String> = None;
    let mut packages: BTreeMap<(String, String), Option<v1::CalculationPackage>> = BTreeMap::new();
    let mut rows = Vec::new();
    for listed in &series {
        let Some(meta) = &listed.meta else { continue };
        let key = key_of(&meta.series_key);
        let dataset = meta
            .source
            .as_ref()
            .map(|s| s.dataset.clone())
            .unwrap_or_default();
        let quote = quotes.get(&key);
        let quote_meta = quote.and_then(|q| q.meta.as_ref());
        // Its quarantined quote's source, where its only quote in force is
        // invalid: its venue still finds its sessions (W12.2; ruling 619).
        let quarantined_source = invalid
            .get(&key)
            .filter(|_| quote.is_none())
            .map(|(_, source)| source);
        let venue = quote_meta
            .and_then(|m| m.source.as_ref())
            .or(quarantined_source)
            .map(|s| s.venue_id.clone())
            .filter(|v| !v.is_empty())
            .or_else(|| meta.source.as_ref().map(|s| s.venue_id.clone()))
            .unwrap_or_default();
        let quote_dataset = quote_meta
            .and_then(|m| m.source.as_ref())
            .or(quarantined_source)
            .map(|s| s.dataset.clone())
            .unwrap_or_else(|| dataset.clone());
        datasets.insert(quote_dataset.clone());
        if !dataset.is_empty() {
            datasets.insert(dataset.clone());
        }
        let held = sessions(
            asker,
            &venue,
            declarations
                .get(&quote_dataset)
                .or(declarations.get(&dataset)),
            as_of,
            calendars,
        );
        if today_local.is_none() {
            today_local = held
                .zone
                .as_ref()
                .and_then(|z| z.local(as_of))
                .map(|(date, _)| date.to_string());
        }
        let expiry = field(listed, "expiration_date").to_string();
        let am = field(listed, "settlement_time").ends_with("AM");
        let (settles, unsessioned) = match settles_at(&expiry, am, &held) {
            Ok(at) => (at, String::new()),
            Err(why) => (0, why),
        };
        let european = field(listed, "exercise_style").ends_with("EUROPEAN");
        // The model its template version's package binds for its terms
        // (ruling 576; W12.2): never a table of the engine's own.
        let (model, unbound) = match bound_model(
            asker,
            &listed.template_id,
            &listed.template_version,
            field(listed, "exercise_style"),
            record.as_ref().map_or(0, |r| r.asset_class),
            &mut packages,
        ) {
            Ok(model) => (model, String::new()),
            Err(why) => (String::new(), why),
        };
        let Some(strike) = amount(field(listed, "strike_price")) else {
            continue;
        };
        let bid = quote
            .and_then(|q| q.bid.as_ref())
            .and_then(|m| text_of(m.amount.as_ref()));
        let ask_price = quote
            .and_then(|q| q.ask.as_ref())
            .and_then(|m| text_of(m.amount.as_ref()));
        // Its only quote in force invalid (W12.2; ruling 619).
        let quarantined = quarantined_source
            .map(|s| s.dataset.clone())
            .unwrap_or_default();
        let oi = interest.get(&key);
        let mut dates: Vec<&String> = oi.map(|o| o.keys().collect()).unwrap_or_default();
        dates.sort();
        let latest = dates
            .last()
            .and_then(|d| oi.and_then(|o| o.get(*d)))
            .cloned();
        let prior = dates
            .len()
            .checked_sub(2)
            .and_then(|i| oi.and_then(|o| o.get(dates[i])))
            .cloned();
        rows.push(json!({
            "series_key": key_json(&key),
            "instrument_id": listed.instrument_id,
            "right": if field(listed, "put_or_call").ends_with("PUT") { "put" } else { "call" },
            "strike": strike,
            "expiration_date": expiry,
            "settles_at_ns": settles.to_string(),
            "unsessioned": if quotes_refused.is_empty() || quote.is_some() { unsessioned } else { quotes_refused.clone() },
            "exercise_style": if european { "european" } else { "american" },
            "model_id": model,
            "unbound": unbound,
            "multiplier": amount(field(listed, "contract_multiplier")).unwrap_or_else(|| "100".into()),
            "bid": bid,
            "ask": ask_price,
            "quote_dataset": quote_dataset,
            "quote_source_ns": quote_meta.map(source_ns).unwrap_or(0).to_string(),
            "quarantined": quarantined,
            "open_interest": latest,
            "open_interest_prior": prior,
        }));
    }
    rows.sort_by_key(|r| r["series_key"].to_string());
    let mut latest_volume: Vec<(&String, &String)> = volume.iter().collect();
    latest_volume.sort();
    let spot_json = spot.as_ref().map(|(meta, money, _)| {
        json!({
            "amount": text_of(money.amount.as_ref()).unwrap_or_default(),
            "dataset": meta.source.as_ref().map(|s| s.dataset.clone()).unwrap_or_default(),
            "row_key": meta.row_key,
            "version": meta.version.to_string(),
            "source_ns": source_ns(meta).to_string(),
        })
    });
    let currency = spot
        .as_ref()
        .map(|(_, money, _)| money.instrument_id.clone())
        .unwrap_or_default();
    Ok(json!({
        "underlying_instrument_id": underlying,
        "currency_instrument_id": currency,
        "spot": spot_json,
        "rate": rate.cloned(),
        "dividends": dividends,
        "dividends_dataset": dividends_dataset,
        "underlying_kind": kind,
        "today": today_local.unwrap_or_else(|| today.to_string()),
        "series": rows,
        "datasets": datasets.into_iter().collect::<Vec<_>>(),
        "underlying_volume": latest_volume.last().map(|(_, v)| (*v).clone()),
    }))
}

/// The scenario set a job runs: inline under `inline`, or saved, read from
/// the lake by its row key and version in the dataset named (W12.8).
pub fn scenario(asker: &dyn Ask, job: &Job) -> Result<Option<Value>, Refused> {
    if let Some(set) = &job.scenario_set {
        return Ok(Some(scenario::set_json("inline", 0, set)));
    }
    let Some(saved) = &job.scenario_ref else {
        return Ok(None);
    };
    let reply: ListScenariosReply = ask(
        asker,
        topics::LIST_SCENARIOS,
        "meridian.v1.ListScenariosRequest",
        &ListScenariosRequest {
            row_keys: vec![saved.row_key.clone()],
            version: saved.version,
        },
    )
    .map_err(|failed| format!("scenario_ref: the lake's scenarios could not be read: {failed}"))?;
    let found = reply.scenarios.into_iter().find(|s| {
        s.meta.as_ref().is_some_and(|m| {
            m.row_key == saved.row_key
                && m.source
                    .as_ref()
                    .is_some_and(|src| src.dataset == saved.dataset)
                && (saved.version == 0 || m.version == saved.version)
        })
    });
    let Some(found) = found else {
        return Err(refusal_with_status(
            RefusalStatus::InvalidArgument,
            &["scenario_ref.row_key".to_string()],
            format!(
                "scenario_ref: {} holds no scenario {:?} at version {}",
                saved.dataset, saved.row_key, saved.version
            ),
        ));
    };
    let version = found.meta.as_ref().map(|m| m.version).unwrap_or(0);
    let set = match (&found.scenario_set, &found.scenario) {
        (Some(set), _) => set.clone(),
        (None, Some(one)) => scenario::single(one),
        (None, None) => {
            return Err(refusal_with_status(
                RefusalStatus::InvalidArgument,
                &["scenario_ref.row_key".to_string()],
                "scenario_ref: the saved row carries no scenario".to_string(),
            ))
        }
    };
    if let Some((field, words)) = scenario::wire_refused("scenario_ref", &set) {
        return Err(refusal_with_status(
            RefusalStatus::InvalidArgument,
            &[field],
            words,
        ));
    }
    Ok(Some(scenario::set_json(&saved.row_key, version, &set)))
}

/// The model a series' template version's calculation package binds for
/// its exercise style and its underlying's asset class (ruling 576; W1.24,
/// W12.2): the most specific binding that matches -- a style and a class
/// named over either left unspecified -- or, for a package with no
/// bindings, the package's own model. Why not, where none binds.
fn bound_model(
    asker: &dyn Ask,
    template_id: &str,
    template_version: &str,
    exercise_style: &str,
    underlying_asset_class: i32,
    packages: &mut BTreeMap<(String, String), Option<v1::CalculationPackage>>,
) -> Result<String, String> {
    let package = packages
        .entry((template_id.to_string(), template_version.to_string()))
        .or_insert_with(|| {
            let reply: Result<v1::ReadTemplateReply, String> = ask(
                asker,
                topics::READ_TEMPLATE,
                "meridian.v1.ReadTemplateRequest",
                &v1::ReadTemplateRequest {
                    template_id: template_id.to_string(),
                    template_version: template_version.to_string(),
                },
            );
            reply
                .ok()
                .and_then(|r| r.template)
                .and_then(|t| t.calculation_package)
        })
        .as_ref()
        .ok_or_else(|| {
            format!("template {template_id} version {template_version}: no calculation package")
        })?;
    if package.model_bindings.is_empty() {
        return Ok(package.model_id.clone());
    }
    let style = v1::ExerciseStyle::from_str_name(exercise_style).map_or(0, |s| s as i32);
    package
        .model_bindings
        .iter()
        .filter(|b| b.exercise_style == 0 || b.exercise_style == style)
        .filter(|b| b.underlying_asset_class == 0 || b.underlying_asset_class == underlying_asset_class)
        .max_by_key(|b| {
            u8::from(b.exercise_style != 0) + u8::from(b.underlying_asset_class != 0)
        })
        .map(|b| b.model_id.clone())
        .ok_or_else(|| {
            format!(
                "template {template_id} version {template_version} binds no model for {exercise_style}"
            )
        })
}

/// The curves a `risk_free_rate` value names (ruling 608; 611's names):
/// `ust_cmt`, Treasury's par yield curve -- its constant-maturity rates --
/// found by its administrator's symbol as the platform seeds the record
/// (W12.2, W3.1; the fourth amendment).
const CURVES: &[(&str, &str, &str)] = &[(
    "ust_cmt",
    "Daily Treasury Par Yield Curve Rates",
    "ustreasury",
)];

/// The job's risk-free curve at the as-of (W12.2; rulings 608 and 611): the
/// one its definitions' `risk_free_rate` names, resolved read-only by its
/// symbol and read through the deployment's source priority for rates, its
/// par yields -- bond-equivalent, compounded twice a year -- as continuous
/// rates by time in years, for the engine to read at each expiry by its
/// `rate_interpolation`. Where it cannot be read, why, and every option of
/// the job is not evaluated naming it.
pub fn rate(
    asker: &dyn Ask,
    compiled: &Compiled,
    as_of: i64,
    sources: &Option<v1::SourceChoice>,
) -> Value {
    let named = compiled
        .definitions
        .iter()
        .find_map(|d| d.parameter("risk_free_rate"))
        .unwrap_or("ust_cmt");
    let missing = |why: &str| json!({"missing": format!("the risk-free curve {named}: {why}")});
    let Some(&(_, symbol, source)) = CURVES.iter().find(|(name, _, _)| *name == named) else {
        return missing("no curve of that name");
    };
    let resolved: Result<v1::ResolveIdentifierReply, String> = ask(
        asker,
        topics::RESOLVE_IDENTIFIER,
        "meridian.v1.ResolveIdentifierRequest",
        &v1::ResolveIdentifierRequest {
            identifiers: vec![Identifier {
                scheme: "symbol".into(),
                value: symbol.into(),
                source: source.into(),
                ..Default::default()
            }],
            as_of_ns: as_of,
            ..Default::default()
        },
    );
    let curve = match resolved {
        Ok(reply) if reply.found && !reply.instrument_id.is_empty() => reply.instrument_id,
        Ok(_) => return missing("the deployment holds no record of it"),
        Err(failed) => return missing(&format!("not resolved: {failed}")),
    };
    let mut points: Vec<Value> = Vec::new();
    let mut dataset = String::new();
    let mut unanswered = String::new();
    let mut cursor = String::new();
    loop {
        let reply: v1::ListRatesReply = match ask(
            asker,
            topics::LIST_RATES,
            "meridian.v1.ListRatesRequest",
            &v1::ListRatesRequest {
                subjects: subject(&curve),
                kinds: vec![v1::RateKind::ParYield as i32],
                sources: sources.clone(),
                at_ns: as_of,
                as_of_ns: as_of,
                page_size: PAGE,
                cursor: cursor.clone(),
                ..Default::default()
            },
        ) {
            Ok(reply) => reply,
            Err(failed) => return missing(&format!("its rates could not be read: {failed}")),
        };
        if let Some(first) = reply.unanswered.first() {
            unanswered = first.dataset.clone();
        }
        for r in &reply.rates {
            let Some(percent) = r
                .value
                .as_ref()
                .and_then(|v| text_of(v.value.as_ref()))
                .and_then(|v| v.parse::<f64>().ok())
            else {
                continue;
            };
            let days = scenario::tenor_days(&r.tenor.unwrap_or_default());
            if days <= 0.0 {
                continue;
            }
            if dataset.is_empty() {
                dataset = r
                    .meta
                    .as_ref()
                    .and_then(|m| m.source.as_ref())
                    .map(|s| s.dataset.clone())
                    .unwrap_or_default();
            }
            points.push(json!({
                "years": format!("{:.9}", days / 365.0),
                "rate": format!("{:.12}", 2.0 * (1.0 + percent / 200.0).ln()),
                "tenor_days": format!("{days}"),
                "par_yield": format!("{percent}"),
            }));
        }
        if reply.next_cursor.is_empty() {
            break;
        }
        cursor = reply.next_cursor;
    }
    if points.is_empty() {
        return missing(&if unanswered.is_empty() {
            "no par yields at the as-of".to_string()
        } else {
            format!("no par yields at the as-of in {unanswered}")
        });
    }
    let interpolation = compiled
        .definitions
        .iter()
        .find_map(|d| d.parameter("rate_interpolation"))
        .unwrap_or("cubic_spline");
    json!({
        "subject_id": curve,
        "dataset": dataset,
        "interpolation": interpolation,
        "points": points,
    })
}
