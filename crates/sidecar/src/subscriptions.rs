//! Subscriptions: a targeted read kept open on the one stream (W4.3;
//! contract v28, amended 2026-10-30; spec/one-data-access-model, step 2;
//! rulings 820, 834 and 881 to 891).
//!
//! Beside the one `Receive`, a plugin calls `Subscribe` with a selector --
//! one row and the read's own narrowing: subjects, series by key or every
//! series in an expiration window, and fields -- named by a
//! `subscription_id` it makes, as FIX's MDReqID (262) is the requester's;
//! `Unsubscribe` drops one. From the reply on, every delivery matching an
//! open selector is queued on the stream, naming each subscription it
//! matched in `DeliveryMeta.subscription_ids`: this sidecar is the one
//! matcher, so the SDK never matches again.
//!
//! **The window.** A chain's window is held to the series the lake's
//! listing holds expiring in it, read once when the subscription opens
//! (ListSeries, as the plugin), and grows as series are listed: every
//! `SeriesRecorded` this sidecar hears adds its series to each open window
//! it falls in, whether or not the plugin hears that row itself.
//!
//! **Entitlements, once.** A delivery's fields are the plugin's entitled
//! fields of its dataset, as the conductor last published them -- decided
//! again only on EntitlementsChanged -- intersected with the selector's
//! `fields`; the envelope always. Where a delivery matches several
//! selectors, it carries the union of what each may read.
//!
//! **Tied to the stream.** A subscription is opened on the plugin's newest
//! open stream, is delivered on it alone, and lives as long as it: when the
//! plugin stops listening there, its subscriptions are dropped, and the SDK
//! opens them again on the stream it has, reading each snapshot again. A
//! stream opened later leaves the earlier one's subscriptions where they
//! are. Subscribing with no stream open is refused.
//!
//! **Upstream.** Each lake row's selector keeps its want alive at the lake
//! through `RenewWants`, one message per lease for every open selector, and
//! one at once for a selector newly opened (W10.7; ruling 885).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_domain::date::Date;
use meridian_domain::v1::{
    Identifier, ListSeriesReply, ListSeriesRequest, ListedSeries, RenewWantsRequest, RenewedWant,
    SeriesRecordedEvent, SubjectRef,
};
use meridian_pb::plugin::v1 as plugin;
use prost::Message;
use tonic::{Response, Status};

use crate::receive::Row;
use crate::service::Sidecar;

/// The most subscriptions open per plugin (SidecarSubscribe.limit).
pub(crate) const MOST_OPEN: usize =
    meridian_pb::bounds::SIDECAR_SUBSCRIBE_LIMIT_RANGE.most as usize;

/// A sidecar's lease on the wants it renews (ruling 885).
pub(crate) const LEASE: Duration = Duration::from_secs(30);

/// The most wants one renewal names (RenewWantsRequest.wants).
const WANTS_AT_ONCE: usize = 500;

pub(crate) const RENEW_WANTS: &str = "platform.lake.command.renew-wants";
const LIST_SERIES: &str = "platform.lake.query.list-series";

/// The template paths a listed series' expiry is read from, in order: an
/// option's expiration date, a future's last trade date, else its final
/// settlement date -- as the lake's listing reads one (ruling 856).
const EXPIRY_PATHS: [&str; 3] = [
    "expiration_date",
    "last_trade_date",
    "final_settlement_date",
];

/// A series key as a subscription holds it: its scheme and value, and for
/// a ticker its venue's MIC, in capitals, as the reads match one.
pub(crate) fn series_named(identifier: &Identifier) -> String {
    if identifier.scheme == meridian_domain::lake::TICKER_SCHEME {
        format!(
            "{}={}@{}",
            identifier.scheme,
            identifier.value,
            identifier.source.to_ascii_uppercase()
        )
    } else {
        format!("{}={}", identifier.scheme, identifier.value)
    }
}

/// A row's series key, as [`series_named`] names each of its identifiers.
fn row_series(key: &[Identifier]) -> BTreeSet<String> {
    key.iter().map(series_named).collect()
}

/// A listed series' expiry, as its terms state it.
pub(crate) fn expiry(series: &ListedSeries) -> Option<Date> {
    EXPIRY_PATHS.iter().find_map(|path| {
        series
            .fields
            .iter()
            .find(|field| field.path == *path)
            .and_then(|field| Date::parse(field.value.trim()).ok())
    })
}

/// The data type a lake row's message carries, by its message's full name,
/// as a want names it: only the rows a want keeps current.
fn wanted_type(row: &Row) -> Option<&'static str> {
    Some(match row.payload_type {
        "meridian.v1.QuotesRecordedEvent" => meridian_domain::lake::QUOTE,
        "meridian.v1.TradesRecordedEvent" => meridian_domain::lake::TRADE,
        "meridian.v1.PricesRecordedEvent" => meridian_domain::lake::PRICE,
        "meridian.v1.BarsRecordedEvent" => meridian_domain::lake::BAR,
        "meridian.v1.StatisticsRecordedEvent" => meridian_domain::lake::STATISTIC,
        "meridian.v1.RatesRecordedEvent" => meridian_domain::lake::RATE,
        // Contract v29 (W10.7): a pool's or a class's factor, wanted
        // standing for a record held.
        "meridian.v1.FactorsRecordedEvent" => meridian_domain::lake::FACTOR,
        _ => return None,
    })
}

/// The data type whose dictionary entries a lake row's `fields` names.
fn fields_type(row: &Row) -> Option<&'static str> {
    wanted_type(row).or(match row.payload_type {
        "meridian.v1.SeriesRecordedEvent" => Some(meridian_domain::lake::LISTED_SERIES),
        "meridian.v1.OutcomesRecordedEvent" => Some(meridian_domain::lake::OUTCOME),
        "meridian.v1.FilingsRecordedEvent" => Some(meridian_domain::lake::FILING),
        "meridian.v1.CorporateActionsRecordedEvent" => {
            Some(meridian_domain::lake::CORPORATE_ACTION)
        }
        _ => None,
    })
}

/// Whether a row is the lake's, heard on each entitled dataset's subject.
fn is_lake(row: &Row) -> bool {
    row.topic.contains(crate::lake::DATASET)
}

/// One open subscription.
#[derive(Debug, Clone)]
struct Open {
    /// The request as asked, its id aside: the same id asked again with an
    /// equal selector is answered as the first.
    asked: plugin::SubscribeRequest,
    row: &'static str,
    lake: bool,
    wanted: Option<&'static str>,
    subjects: BTreeSet<String>,
    series: BTreeSet<String>,
    all_series: bool,
    window: Option<(Option<Date>, Option<Date>)>,
    /// Within a window: the series the listing holds expiring in it.
    listed: BTreeSet<String>,
    fields: BTreeSet<String>,
    /// The stream it was opened on, and is delivered on.
    generation: u64,
}

impl Open {
    fn in_window(&self, date: &Date) -> bool {
        match &self.window {
            None => true,
            Some((from, to)) => {
                from.as_ref().is_none_or(|from| date >= from)
                    && to.as_ref().is_none_or(|to| date <= to)
            }
        }
    }

    /// Whether a lake row about `subjects`, keyed by `series` (empty for
    /// the subject's own row), is this selector's.
    fn holds(&self, subjects: &[String], series: &BTreeSet<String>) -> bool {
        if !subjects.iter().any(|s| self.subjects.contains(s)) {
            return false;
        }
        if series.is_empty() {
            // A subject's own row is heard unless the selector names only
            // series by key.
            return self.series.is_empty() || self.all_series;
        }
        if self.all_series {
            return self.window.is_none() || series.iter().any(|s| self.listed.contains(s));
        }
        series.iter().any(|s| self.series.contains(s))
    }

    /// The fields this selector reads of a dataset the plugin may read
    /// `entitled` of: an empty set for every field.
    fn mask(&self, entitled: &BTreeSet<String>, meta: Option<&str>) -> BTreeSet<String> {
        if self.fields.is_empty() {
            return entitled.clone();
        }
        let mut mask: BTreeSet<String> = if entitled.is_empty() {
            self.fields.clone()
        } else {
            self.fields.intersection(entitled).cloned().collect()
        };
        if let Some(meta) = meta {
            mask.insert(meta.to_string());
        }
        mask
    }
}

/// What the plugin's open stream lets a subscription start: a forwarder
/// for a row the stream is not yet hearing.
pub(crate) type StartRow = Arc<dyn Fn(&'static Row) + Send + Sync>;

struct Stream {
    /// The rows forwarded on it now, chosen or subscribed.
    forwarded: BTreeSet<&'static str>,
    start: StartRow,
}

#[derive(Default)]
struct Inner {
    /// The plugin's open streams, by generation; the newest is the one a
    /// subscription opens on.
    streams: BTreeMap<u64, Stream>,
    open: BTreeMap<String, Open>,
    generations: u64,
}

impl Inner {
    fn newest(&mut self) -> Option<(u64, &mut Stream)> {
        self.streams
            .iter_mut()
            .next_back()
            .map(|(generation, stream)| (*generation, stream))
    }
}

/// The plugin's open subscriptions, and the stream they deliver on.
#[derive(Default)]
pub(crate) struct Registry {
    inner: Mutex<Inner>,
}

/// What a delivery matched: by the stream's own rows and subjects, and the
/// subscriptions, each with the fields it reads.
#[derive(Debug, Default)]
pub(crate) struct Matched {
    pub ids: Vec<String>,
    /// The union of the matched subscriptions' masks; `None` where one of
    /// them reads every field the plugin may.
    pub mask: Option<BTreeSet<String>>,
}

impl Registry {
    /// A stream opened: the rows it forwards, and how to start another.
    /// Its generation names it.
    pub(crate) fn opened(&self, chosen: BTreeSet<&'static str>, start: StartRow) -> u64 {
        let mut inner = self.inner.lock().expect("subscriptions poisoned");
        inner.generations += 1;
        let generation = inner.generations;
        inner.streams.insert(
            generation,
            Stream {
                forwarded: chosen,
                start,
            },
        );
        generation
    }

    /// How a stream starts a row it does not yet forward, given once its
    /// forwarders' context exists.
    pub(crate) fn starting(&self, generation: u64, start: StartRow) {
        if let Some(stream) = self
            .inner
            .lock()
            .expect("subscriptions poisoned")
            .streams
            .get_mut(&generation)
        {
            stream.start = start;
        }
    }

    /// The stream of `generation` closed: its subscriptions with it.
    pub(crate) fn closed(&self, generation: u64) {
        let mut inner = self.inner.lock().expect("subscriptions poisoned");
        inner.streams.remove(&generation);
        inner.open.retain(|_, open| open.generation != generation);
    }

    fn is_open(&self, generation: u64) -> bool {
        self.inner
            .lock()
            .expect("subscriptions poisoned")
            .streams
            .contains_key(&generation)
    }

    pub(crate) fn open_count(&self) -> usize {
        self.inner
            .lock()
            .expect("subscriptions poisoned")
            .open
            .len()
    }

    /// A lake row heard: the subscriptions it matches, and their mask. A
    /// listed series heard first widens each open window it falls in.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn matching_lake(
        &self,
        generation: u64,
        row: &str,
        subjects: &[String],
        series_key: &[Identifier],
        payload_type: &str,
        payload: &[u8],
        entitled: &BTreeSet<String>,
        meta: Option<&str>,
    ) -> Matched {
        let series = row_series(series_key);
        let mut inner = self.inner.lock().expect("subscriptions poisoned");
        if payload_type == "meridian.v1.SeriesRecordedEvent" {
            if let Some(listed) = SeriesRecordedEvent::decode(payload)
                .ok()
                .and_then(|event| event.series)
            {
                if let Some(date) = expiry(&listed) {
                    for open in inner.open.values_mut() {
                        if open.lake
                            && open.all_series
                            && open.window.is_some()
                            && open.in_window(&date)
                            && subjects.iter().any(|s| open.subjects.contains(s))
                        {
                            open.listed.extend(series.iter().cloned());
                        }
                    }
                }
            }
        }
        let mut matched = Matched::default();
        let mut mask: Option<BTreeSet<String>> = Some(BTreeSet::new());
        for (id, open) in &inner.open {
            if open.generation != generation
                || open.row != row
                || !open.lake
                || !open.holds(subjects, &series)
            {
                continue;
            }
            matched.ids.push(id.clone());
            let theirs = open.mask(entitled, meta);
            mask = match (mask, theirs.is_empty()) {
                (_, true) | (None, _) => None,
                (Some(mut held), false) => {
                    held.extend(theirs);
                    Some(held)
                }
            };
        }
        if !matched.ids.is_empty() {
            matched.mask = mask;
        }
        matched
    }

    /// A row naming an account, or none, heard: the subscriptions it
    /// matches, each its row's and naming that account or every one.
    pub(crate) fn matching_book(
        &self,
        generation: u64,
        row: &str,
        account: Option<&str>,
    ) -> Vec<String> {
        let inner = self.inner.lock().expect("subscriptions poisoned");
        inner
            .open
            .iter()
            .filter(|(_, open)| {
                open.generation == generation
                    && open.row == row
                    && !open.lake
                    && (open.subjects.is_empty()
                        || account.is_some_and(|account| open.subjects.contains(account)))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The wants the open lake selectors of a stream hold, as a renewal
    /// names them.
    pub(crate) fn wants(&self, generation: u64) -> Vec<RenewedWant> {
        let inner = self.inner.lock().expect("subscriptions poisoned");
        let mut wants: Vec<RenewedWant> = inner
            .open
            .values()
            .filter(|open| open.generation == generation)
            .filter_map(want_of)
            .collect();
        wants.sort_by_key(|want| want.encode_to_vec());
        wants.dedup();
        wants
    }
}

fn want_of(open: &Open) -> Option<RenewedWant> {
    let data_type = open.wanted?;
    if open.subjects.is_empty() {
        return None;
    }
    Some(RenewedWant {
        data_type: data_type.to_string(),
        subjects: open
            .subjects
            .iter()
            .map(|s| SubjectRef {
                entity_id: s.clone(),
                ..Default::default()
            })
            .collect(),
        series_keys: open
            .asked
            .series_keys
            .iter()
            .map(domain_identifier)
            .collect(),
        all_series: open.all_series,
        expiration_from: open.asked.expiration_from.clone(),
        expiration_to: open.asked.expiration_to.clone(),
    })
}

fn domain_identifier(identifier: &plugin::Identifier) -> Identifier {
    Identifier::decode(&identifier.encode_to_vec()[..]).unwrap_or_default()
}

fn refused(sidecar: &Sidecar, status: Status) -> Status {
    sidecar.note_refusal(status.message());
    status
}

impl Sidecar {
    /// W4.3: a targeted read kept open (contract v28, amended 2026-10-30).
    pub(crate) async fn subscribe_typed(
        &self,
        request: plugin::SubscribeRequest,
        rows: &'static [Row],
    ) -> Result<Response<plugin::SubscribeReply>, Status> {
        let registration = self.admitted()?;
        let id = request.subscription_id.clone();
        if !meridian_pb::bounds::SUBSCRIBE_REQUEST_SUBSCRIPTION_ID_LENGTH.admits(id.chars().count())
        {
            return Err(refused(
                self,
                Status::invalid_argument("subscription_id is 1 to 64 characters"),
            ));
        }
        let Some(row) = rows
            .iter()
            .filter(|row| registration.grants.may_subscribe(row.topic))
            .find(|row| row.name == request.row)
        else {
            return Err(refused(
                self,
                Status::permission_denied(format!(
                    "{} is not a row this plugin's roles hear",
                    request.row
                )),
            ));
        };
        let open = self.selector(&request, row)?;
        {
            let inner = self
                .subscriptions
                .inner
                .lock()
                .expect("subscriptions poisoned");
            if inner.streams.is_empty() {
                drop(inner);
                return Err(refused(
                    self,
                    Status::failed_precondition(
                        "no stream is open: a subscription delivers on the plugin's one \
                         Receive stream, opened first",
                    ),
                ));
            }
            if let Some(held) = inner.open.get(&id) {
                if held.asked == open.asked {
                    return Ok(Response::new(plugin::SubscribeReply {}));
                }
                drop(inner);
                return Err(refused(
                    self,
                    Status::already_exists(format!("{id} is open with another selector")),
                ));
            }
            if inner.open.len() >= MOST_OPEN {
                drop(inner);
                return Err(refused(
                    self,
                    Status::resource_exhausted(format!(
                        "subscription_id: a plugin holds at most {MOST_OPEN} open subscriptions"
                    )),
                ));
            }
        }
        // A chain's window, held to the listing (ruling 883).
        let mut open = open;
        if open.window.is_some() {
            open.listed = self.listed_within(&request).await?;
        }
        let want = want_of(&open);
        {
            let mut inner = self
                .subscriptions
                .inner
                .lock()
                .expect("subscriptions poisoned");
            let Some((generation, stream)) = inner.newest() else {
                return Err(Status::failed_precondition("the stream closed"));
            };
            open.generation = generation;
            // A window widens on the listing's changes, so the row that
            // carries them is heard too, where the plugin may hear it.
            let mut starting: Vec<&'static Row> = Vec::new();
            if stream.forwarded.insert(row.name) {
                starting.push(row);
            }
            if open.window.is_some() {
                if let Some(listing) = rows.iter().find(|r| {
                    r.payload_type == "meridian.v1.SeriesRecordedEvent"
                        && registration.grants.may_subscribe(r.topic)
                }) {
                    if stream.forwarded.insert(listing.name) {
                        starting.push(listing);
                    }
                }
            }
            for started in starting {
                (stream.start)(started);
            }
            inner.open.insert(id.clone(), open);
        }
        // Its want kept current from now, not from the next lease.
        if let Some(want) = want {
            renew(&self.bus, vec![want]).await;
        }
        tracing::info!(
            instance = self.instance_id(),
            subscription_id = id,
            row = row.name,
            open = self.subscriptions.open_count(),
            "a subscription opened"
        );
        Ok(Response::new(plugin::SubscribeReply {}))
    }

    /// W4.3: a subscription dropped; an id not open is answered as dropped.
    pub(crate) async fn unsubscribe_typed(
        &self,
        request: plugin::UnsubscribeRequest,
    ) -> Result<Response<plugin::UnsubscribeReply>, Status> {
        self.admitted()?;
        self.subscriptions
            .inner
            .lock()
            .expect("subscriptions poisoned")
            .open
            .remove(&request.subscription_id);
        Ok(Response::new(plugin::UnsubscribeReply {}))
    }

    /// A request held to its row: the subjects, series, window and fields
    /// it may name.
    #[allow(clippy::result_large_err)] // a refusal is the call's own Status, as every operation's
    fn selector(
        &self,
        request: &plugin::SubscribeRequest,
        row: &'static Row,
    ) -> Result<Open, Status> {
        let bad = |status: Status| Err(refused(self, status));
        if !meridian_pb::bounds::SUBSCRIBE_REQUEST_SUBJECTS_COUNT.admits(request.subjects.len()) {
            return bad(Status::invalid_argument(format!(
                "subjects names {}; at most 500",
                request.subjects.len()
            )));
        }
        if !meridian_pb::bounds::SUBSCRIBE_REQUEST_SERIES_KEYS_COUNT
            .admits(request.series_keys.len())
        {
            return bad(Status::invalid_argument(format!(
                "series_keys names {}; at most 500",
                request.series_keys.len()
            )));
        }
        if !meridian_pb::bounds::SUBSCRIBE_REQUEST_FIELDS_COUNT.admits(request.fields.len()) {
            return bad(Status::invalid_argument(format!(
                "fields names {}; at most 32",
                request.fields.len()
            )));
        }
        let side = |text: &str, name: &str| -> Result<Option<Date>, Status> {
            if text.is_empty() {
                return Ok(None);
            }
            Date::parse(text)
                .map(Some)
                .map_err(|_| Status::invalid_argument(format!("{name} {text:?} is no plain date")))
        };
        let from =
            side(&request.expiration_from, "expiration_from").map_err(|s| refused(self, s))?;
        let to = side(&request.expiration_to, "expiration_to").map_err(|s| refused(self, s))?;
        let windowed = from.is_some() || to.is_some();
        if windowed && !request.all_series {
            return bad(Status::invalid_argument(
                "expiration_from is set only with all_series",
            ));
        }
        if let (Some(from), Some(to)) = (&from, &to) {
            if to < from {
                return bad(Status::invalid_argument(
                    "expiration_to is on or after expiration_from",
                ));
            }
        }
        let lake = is_lake(row);
        if !lake
            && (!request.series_keys.is_empty() || request.all_series || !request.fields.is_empty())
        {
            return bad(Status::invalid_argument(format!(
                "fields: {} is a record delivered whole, narrowed by the accounts in subjects alone",
                row.name
            )));
        }
        if lake {
            let entries = fields_type(row)
                .map(meridian_domain::lake::fields_of)
                .unwrap_or_default();
            for (i, field) in request.fields.iter().enumerate() {
                if !entries.contains(&field.as_str()) {
                    return bad(Status::invalid_argument(format!(
                        "fields[{i}] names {field}, no dictionary entry of {}",
                        fields_type(row).unwrap_or(row.payload_type)
                    )));
                }
            }
        }
        let mut asked = request.clone();
        asked.subscription_id.clear();
        Ok(Open {
            asked,
            row: row.name,
            lake,
            wanted: if lake { wanted_type(row) } else { None },
            subjects: request.subjects.iter().cloned().collect(),
            series: request
                .series_keys
                .iter()
                .map(|k| series_named(&domain_identifier(k)))
                .collect(),
            all_series: request.all_series,
            window: windowed.then_some((from, to)),
            listed: BTreeSet::new(),
            fields: request.fields.iter().cloned().collect(),
            generation: 0,
        })
    }

    /// The series the lake's listing holds expiring in a request's window,
    /// read as the plugin, every page.
    async fn listed_within(
        &self,
        request: &plugin::SubscribeRequest,
    ) -> Result<BTreeSet<String>, Status> {
        let mut listed = BTreeSet::new();
        let mut cursor = String::new();
        loop {
            let asked = ListSeriesRequest {
                subjects: request
                    .subjects
                    .iter()
                    .map(|s| SubjectRef {
                        entity_id: s.clone(),
                        ..Default::default()
                    })
                    .collect(),
                expiration_from: request.expiration_from.clone(),
                expiration_to: request.expiration_to.clone(),
                paths: EXPIRY_PATHS.iter().map(|p| p.to_string()).collect(),
                cursor: cursor.clone(),
                ..Default::default()
            };
            let page: ListSeriesReply = self
                .call_typed(
                    LIST_SERIES,
                    "meridian.v1.ListSeriesRequest",
                    asked,
                    None,
                    None,
                )
                .await
                .map_err(|failed| {
                    Status::failed_precondition(format!(
                        "the window's listing could not be read: {}",
                        failed.message()
                    ))
                })?
                .into_inner();
            for series in &page.series {
                let key = series
                    .meta
                    .as_ref()
                    .map(|m| row_series(&m.series_key))
                    .unwrap_or_default();
                listed.extend(key);
            }
            if page.next_cursor.is_empty() {
                return Ok(listed);
            }
            cursor = page.next_cursor;
        }
    }
}

/// The open selectors' wants renewed at the lake, at most 500 a message
/// (W10.7; ruling 885), as the plugin: its sidecar stamps the read scope as
/// applying, so the lake reads the plugin's entitlements. A renewal not
/// answered is said, and the next lease renews again.
pub(crate) async fn renew(bus: &meridian_bus::Bus, wants: Vec<RenewedWant>) {
    for chunk in wants.chunks(WANTS_AT_ONCE) {
        let payload = RenewWantsRequest {
            wants: chunk.to_vec(),
        }
        .encode_to_vec();
        let stamp = meridian_bus::Stamp {
            account_scope: Some(Vec::new()),
            ..Default::default()
        };
        if let Err(failed) = bus
            .call_stamped(
                RENEW_WANTS,
                "meridian.v1.RenewWantsRequest",
                payload,
                None,
                None,
                &stamp,
            )
            .await
        {
            tracing::warn!(%failed, wants = chunk.len(), "a renewal of wants was not answered");
        }
    }
}

/// Every lease, the open selectors' wants renewed, until the stream of
/// `generation` closes.
pub(crate) fn renew_every_lease(
    bus: Arc<meridian_bus::Bus>,
    registry: Arc<Registry>,
    generation: u64,
) {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(LEASE);
        every.tick().await;
        loop {
            every.tick().await;
            if !registry.is_open(generation) {
                return;
            }
            let wants = registry.wants(generation);
            if !wants.is_empty() {
                renew(&bus, wants).await;
            }
        }
    });
}

#[cfg(test)]
mod tests;
