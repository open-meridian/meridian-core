//! QueryLake (contract v26, W10.10; sdk-contract/the-lake-has-a-bounded-query-tool):
//! one read-only query over one of the lake's kinds, for subjects over a
//! range of valid times or business dates, as of a recorded time, under a
//! source choice, naming the fields wanted and a row limit.
//!
//! Answered by the reads' own rules, as the instance it is asked for: a
//! plugin's own, stamped by its sidecar; for the dashboard's tool, the
//! instance the person names, whose entitlements bound the answer. The role
//! a kind is read by is checked where the roles are known -- the sidecar for
//! a plugin, the dashboard for its tool -- as the reads' rows grant them.
//! Capped: at most 500 subjects, `row_limit` 1 to 10,000 (1,000 when 0);
//! past the limit the reply is cut and `truncated` says so, never paged.
//! Nothing is asked of a data plugin and nothing is recorded: what a list
//! read would want is not wanted here.

use std::collections::{BTreeMap, BTreeSet};

use meridian_bus::{refusal_with_status, Envelope, RefusalStatus};
use meridian_domain::date::Date;
use meridian_domain::lake;
use meridian_domain::v1::{
    DatasetRef, ListOutcomesRequest, QueryLakeReply, QueryLakeRequest, Unanswered, Watermark,
};

use crate::config::Reader;
use crate::row::{DataType, Observation};
use crate::service::{reader_of, Lake, ReadRequest};

/// At most this many subjects (W10.10).
pub const MOST_SUBJECTS: usize = 500;
/// The row limit's bounds, and what 0 reads.
pub const MOST_ROWS: u32 = 10_000;
pub const DEFAULT_ROWS: u32 = 1_000;
/// The most business dates a price or bar query walks.
pub const MOST_DATES: i64 = 3_660;
/// A trade or quote range is read in pieces no longer than this, each
/// within any dataset's day as it declares it.
const PIECE_NS: i64 = 15 * 60 * 1_000_000_000;
const MINUTE_NS: i64 = 60 * 1_000_000_000;
/// One read's page.
const PAGE: u32 = 500;

fn invalid(field: &str, words: impl std::fmt::Display) -> String {
    refusal_with_status(RefusalStatus::InvalidArgument, &[field.to_string()], words)
}

fn denied(field: &str, words: impl std::fmt::Display) -> String {
    refusal_with_status(RefusalStatus::PermissionDenied, &[field.to_string()], words)
}

/// The kind a request names, as the lake's data type.
pub fn kind_of(kind: &str) -> Option<DataType> {
    match kind {
        "prices" => Some(DataType::Price),
        "bars" => Some(DataType::Bar),
        "trades" => Some(DataType::Trade),
        "quotes" => Some(DataType::Quote),
        "outcomes" => Some(DataType::Outcome),
        // Contract v27, the rates slice: read as prices are.
        "rates" => Some(DataType::Rate),
        _ => None,
    }
}

/// The fields named, each an entry of the kind's own message; the row's
/// meta always kept. An empty set reads every field the asker may read.
pub fn fields_named(kind: DataType, fields: &[String]) -> Result<BTreeSet<String>, String> {
    let own = lake::fields_of(kind.name());
    let mut named = BTreeSet::new();
    for (i, field) in fields.iter().enumerate() {
        let field = field.trim();
        if !own.contains(&field) {
            return Err(invalid(
                &format!("fields[{i}]"),
                format!(
                    "fields[{i}]: {field:?} is not a field of {}; its fields are {}",
                    kind.name(),
                    own.join(", ")
                ),
            ));
        }
        named.insert(field.to_string());
    }
    if !named.is_empty() {
        named.insert(format!("{}.meta", kind.name()));
    }
    Ok(named)
}

/// A row with only the fields named.
fn keep_named(row: &mut Observation, named: &BTreeSet<String>) {
    match row {
        Observation::Price(price) => {
            lake::strip_price(price, named);
        }
        Observation::Bar(bar) => {
            lake::strip_bar(bar, named);
        }
        Observation::Trade(trade) => {
            lake::strip_trade(trade, named);
        }
        Observation::Quote(quote) => {
            lake::strip_quote(quote, named);
        }
        Observation::Outcome(outcome) => {
            lake::strip_outcome(outcome, named);
        }
        Observation::Rate(rate) => {
            lake::strip_rate(rate, named);
        }
        // Never a QueryLake kind: read by ListExecutionCosts alone; nor,
        // in contract v28, statistics or series (ListStatistics, ListSeries).
        Observation::ExecutionCost(_)
        | Observation::Statistic(_)
        | Observation::Series(_)
        | Observation::Derived(_)
        | Observation::Scenario(_)
        | Observation::CorporateAction(_)
        | Observation::Filing(_)
        | Observation::ReportedFact(_)
        | Observation::RatingAction(_)
        | Observation::HoldingsReport(_)
        | Observation::Holding(_) => {}
    }
}

/// How the query reads time: the latest, business dates, or a valid-time
/// range.
#[derive(Debug, Clone, PartialEq)]
enum Span {
    Latest,
    Dates(Vec<String>),
    Range(i64, i64),
}

fn span_of(request: &QueryLakeRequest, kind: DataType, now_ns: i64) -> Result<Span, String> {
    let dated = !request.from_date.is_empty() || !request.to_date.is_empty();
    let ranged = request.valid_from_ns != 0 || request.valid_until_ns != 0;
    if dated && ranged {
        return Err(invalid(
            "from_date",
            "business dates or a valid-time range: at most one is given",
        ));
    }
    if request.valid_from_ns < 0 || request.valid_until_ns < 0 {
        return Err(invalid("valid_from_ns", "valid times are never negative"));
    }
    if kind == DataType::Outcome && (dated || ranged) {
        return Err(invalid(
            if dated { "from_date" } else { "valid_from_ns" },
            "outcomes are read as they stand: no dates and no range",
        ));
    }
    if dated {
        let read = |field: &str, text: &str| -> Result<Option<Date>, String> {
            meridian_domain::date::optional(field, text).map_err(|why| invalid(field, why))
        };
        let from = read("from_date", &request.from_date)?;
        let to = read("to_date", &request.to_date)?;
        let (from, to) = match (from, to) {
            (Some(from), Some(to)) => (from, to),
            (Some(one), None) | (None, Some(one)) => (one, one),
            (None, None) => unreachable!("dated"),
        };
        if to.days_since_epoch() < from.days_since_epoch() {
            return Err(invalid("to_date", "to_date is on or after from_date"));
        }
        if to.days_since_epoch() - from.days_since_epoch() >= MOST_DATES {
            return Err(invalid(
                "to_date",
                format!("at most {MOST_DATES} business dates in one query"),
            ));
        }
        if kind.real_time() {
            // Trades and quotes are read by valid time: the UTC days named.
            return Ok(Span::Range(
                from.days_since_epoch() * 86_400 * 1_000_000_000,
                (to.days_since_epoch() + 1) * 86_400 * 1_000_000_000,
            ));
        }
        let dates = (from.days_since_epoch()..=to.days_since_epoch())
            .filter_map(Date::from_days)
            .map(|date| date.to_string())
            .collect();
        return Ok(Span::Dates(dates));
    }
    if ranged {
        let until = if request.valid_until_ns == 0 {
            now_ns
        } else {
            request.valid_until_ns
        };
        if until <= request.valid_from_ns {
            return Err(invalid(
                "valid_until_ns",
                "valid_until_ns is after valid_from_ns",
            ));
        }
        return Ok(Span::Range(request.valid_from_ns, until));
    }
    Ok(Span::Latest)
}

/// Each piece of a trade or quote range, in order: 15 minutes at a time on
/// UTC quarter hours, so none crosses a dataset's day where it ends on one.
fn pieces(from_ns: i64, until_ns: i64) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    let mut at = from_ns;
    while at < until_ns {
        let next = ((at.div_euclid(PIECE_NS)) + 1) * PIECE_NS;
        let end = next.min(until_ns);
        out.push((at, end));
        at = end;
    }
    out
}

impl Lake {
    /// W10.10: QueryLake.
    pub(crate) fn query_lake(
        &self,
        request: QueryLakeRequest,
        envelope: &Envelope,
    ) -> Result<QueryLakeReply, String> {
        let reader = match reader_of(envelope) {
            Reader::Plugin(own) => {
                if !request.instance_id.is_empty() && request.instance_id != own {
                    return Err(denied(
                        "instance_id",
                        "instance_id: a plugin queries as itself",
                    ));
                }
                Reader::Plugin(own)
            }
            Reader::Core => {
                if request.instance_id.trim().is_empty() {
                    return Err(invalid(
                        "instance_id",
                        "instance_id: the plugin instance whose entitlements bound the answer",
                    ));
                }
                Reader::Plugin(request.instance_id.trim().to_string())
            }
        };
        self.query_as(reader, request)
    }

    pub(crate) fn query_as(
        &self,
        reader: Reader,
        request: QueryLakeRequest,
    ) -> Result<QueryLakeReply, String> {
        let kind = kind_of(request.kind.trim()).ok_or_else(|| {
            invalid(
                "kind",
                format!(
                    "kind: {:?} is not one of {}",
                    request.kind,
                    lake::QUERY_KINDS.join(", ")
                ),
            )
        })?;
        if request.subjects.is_empty() || request.subjects.len() > MOST_SUBJECTS {
            return Err(invalid(
                "subjects",
                format!(
                    "subjects names {}; 1 to {MOST_SUBJECTS}",
                    request.subjects.len()
                ),
            ));
        }
        if request.row_limit > MOST_ROWS {
            return Err(invalid(
                "row_limit",
                format!("row_limit {}; 1 to {MOST_ROWS}", request.row_limit),
            ));
        }
        if request.as_of_ns < 0 {
            return Err(invalid("as_of_ns", "as_of_ns is never negative"));
        }
        let limit = match request.row_limit {
            0 => DEFAULT_ROWS,
            n => n,
        } as usize;
        let named = fields_named(kind, &request.fields)?;
        let now = self.clock.now_ns();
        let as_of = if request.as_of_ns == 0 {
            now
        } else {
            request.as_of_ns
        };
        let span = span_of(&request, kind, now)?;

        let mut rows: Vec<Observation> = Vec::new();
        let mut unanswered: BTreeMap<(String, String, String, i32), Unanswered> = BTreeMap::new();
        let mut datasets: BTreeMap<String, DatasetRef> = BTreeMap::new();
        let mut watermark = Watermark::default();
        let mut keep = |answer: crate::service::ReadAnswer, rows: &mut Vec<Observation>| -> bool {
            for missing in answer.unanswered {
                let key = (
                    missing
                        .subject
                        .as_ref()
                        .map(|s| s.entity_id.clone())
                        .unwrap_or_default(),
                    missing.dataset.clone(),
                    missing.field.clone(),
                    missing.reason,
                );
                unanswered.entry(key).or_insert(missing);
            }
            for dataset in answer.datasets {
                datasets.entry(dataset.dataset.clone()).or_insert(dataset);
            }
            watermark = answer.watermark;
            rows.extend(answer.rows);
            rows.len() > limit
        };

        if kind == DataType::Outcome {
            let answered = self.list_outcomes_as(
                reader.clone(),
                ListOutcomesRequest {
                    subjects: request.subjects.clone(),
                    as_of_ns: as_of,
                    ..Default::default()
                },
            )?;
            rows.extend(answered.outcomes.into_iter().map(Observation::Outcome));
            watermark = answered.watermark.unwrap_or_default();
        } else {
            let ask =
                |at_ns: i64, business_date: String, from_ns: i64, until_ns: i64, cursor: String| {
                    ReadRequest {
                        data_type: kind,
                        subjects: request.subjects.clone(),
                        kinds: Vec::new(),
                        interval_ns: 0,
                        sources: request.sources.clone(),
                        at_ns,
                        business_date,
                        valid_from_ns: from_ns,
                        valid_until_ns: until_ns,
                        as_of_ns: as_of,
                        page_size: PAGE,
                        cursor,
                        after: None,
                        tenors: Vec::new(),
                        published_as_of_ns: 0,
                        series_keys: Vec::new(),
                        all_series: false,
                        dates: None,
                    }
                };
            // Every page of one read, until the limit is passed.
            let mut whole = |make: &dyn Fn(String) -> ReadRequest,
                             rows: &mut Vec<Observation>|
             -> Result<bool, String> {
                let mut cursor = String::new();
                loop {
                    let answer = self.read_as(reader.clone(), make(cursor), false)?;
                    let next = answer.next_cursor.clone();
                    if keep(answer, rows) {
                        return Ok(true);
                    }
                    if next.is_empty() {
                        return Ok(false);
                    }
                    cursor = next;
                }
            };
            match &span {
                Span::Latest => {
                    whole(&|cursor| ask(0, String::new(), 0, 0, cursor), &mut rows)?;
                }
                Span::Dates(dates) => {
                    for date in dates {
                        if whole(&|cursor| ask(0, date.clone(), 0, 0, cursor), &mut rows)? {
                            break;
                        }
                    }
                }
                Span::Range(from_ns, until_ns) if kind.real_time() => {
                    'pieces: for (from, until) in pieces(*from_ns, *until_ns) {
                        match whole(
                            &|cursor| ask(0, String::new(), from, until, cursor),
                            &mut rows,
                        ) {
                            Ok(true) => break,
                            Ok(false) => {}
                            // A dataset whose day ends off a quarter hour:
                            // that piece a minute at a time.
                            Err(why) if why.contains("spans more than one day") => {
                                let mut at = from;
                                while at < until {
                                    let end = (at + MINUTE_NS).min(until);
                                    if whole(
                                        &|cursor| ask(0, String::new(), at, end, cursor),
                                        &mut rows,
                                    )? {
                                        break 'pieces;
                                    }
                                    at = end;
                                }
                            }
                            Err(why) => return Err(why),
                        }
                    }
                }
                Span::Range(from_ns, until_ns) => {
                    whole(
                        &|cursor| ask(0, String::new(), *from_ns, *until_ns, cursor),
                        &mut rows,
                    )?;
                }
            }
        }

        let truncated = rows.len() > limit;
        rows.truncate(limit);
        if !named.is_empty() {
            for row in &mut rows {
                keep_named(row, &named);
            }
        }
        let mut reply = QueryLakeReply {
            unanswered: unanswered.into_values().collect(),
            datasets: datasets.into_values().collect(),
            watermark: Some(watermark),
            truncated,
            ..Default::default()
        };
        for row in rows {
            match row {
                Observation::Price(price) => reply.prices.push(price),
                Observation::Bar(bar) => reply.bars.push(bar),
                Observation::Trade(trade) => reply.trades.push(trade),
                Observation::Quote(quote) => reply.quotes.push(quote),
                Observation::Outcome(outcome) => reply.outcomes.push(outcome),
                Observation::Rate(rate) => reply.rates.push(rate),
                Observation::ExecutionCost(_)
                | Observation::Statistic(_)
                | Observation::Series(_)
                | Observation::Derived(_)
                | Observation::Scenario(_)
                | Observation::CorporateAction(_)
                | Observation::Filing(_)
                | Observation::ReportedFact(_)
                | Observation::RatingAction(_)
                | Observation::HoldingsReport(_)
                | Observation::Holding(_) => {}
            }
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_its_fields_and_its_span_are_read_or_refused_naming_the_field() {
        assert_eq!(kind_of("prices"), Some(DataType::Price));
        assert_eq!(kind_of("candles"), None);
        let named = fields_named(
            DataType::Price,
            &[
                "meridian.v1.Price.price".into(),
                "meridian.v1.Price.kind".into(),
            ],
        )
        .unwrap();
        assert!(named.contains("meridian.v1.Price.meta"));
        let wrong = fields_named(DataType::Price, &["meridian.v1.Trade.price".into()]).unwrap_err();
        assert!(wrong.contains("fields[0]"), "{wrong}");
        let dated = QueryLakeRequest {
            from_date: "2026-09-01".into(),
            to_date: "2026-09-03".into(),
            ..Default::default()
        };
        assert_eq!(
            span_of(&dated, DataType::Price, 0).unwrap(),
            Span::Dates(vec![
                "2026-09-01".into(),
                "2026-09-02".into(),
                "2026-09-03".into()
            ])
        );
        assert!(matches!(
            span_of(&dated, DataType::Trade, 0).unwrap(),
            Span::Range(_, _)
        ));
        assert!(span_of(&dated, DataType::Outcome, 0)
            .unwrap_err()
            .contains("from_date"));
        let both = QueryLakeRequest {
            valid_from_ns: 1,
            ..dated
        };
        assert!(span_of(&both, DataType::Price, 0).is_err());
        assert_eq!(
            pieces(0, 40 * MINUTE_NS),
            vec![
                (0, PIECE_NS),
                (PIECE_NS, 2 * PIECE_NS),
                (2 * PIECE_NS, 40 * MINUTE_NS)
            ]
        );
    }
}
