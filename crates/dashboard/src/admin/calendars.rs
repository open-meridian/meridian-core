//! The Instruments page's Calendars tab (contract v22; W3.21;
//! spec/settlement-calendars-are-core-reference-data, Q18): the settlement
//! calendars and date rules the deployment holds, read-only, one line each,
//! and a calendar shown one year at a time. A deployment edits no calendar
//! (the spec's Q3): a disagreement goes to the platform. The same reads are
//! `dashboard__list_calendars` and `dashboard__read_calendar`.
//!
//! From contract v23 (W3.21; v23's Q14 and Q15) each line shows its subject
//! resolved -- a currency's ISO 4217 code, from the deployment's record
//! carrying the platform's ID; a settlement system's name and country; the
//! venues by their `VEN-` IDs until the dashboard may read a venue by its ID,
//! and from contract v25 (D9) each by its MIC with its name beside it -- and
//! the tab is filtered by kind and searched by text on the store's
//! side, so it searches rather than pages.

use std::collections::BTreeMap;

use std::time::Duration;

use meridian_domain::v1::{
    CalendarDayKind, CalendarKind, HeldCalendar, ListCalendarsReply, ListCalendarsRequest,
    ListInstrumentsToCompleteReply, ListInstrumentsToCompleteRequest, ReadCalendarReply,
    ReadCalendarRequest, ResolveVenueReply, ResolveVenueRequest, Weekday,
};
use prost::Message;

use crate::html::escape;

/// W3.21: the calendars held.
pub const LIST_CALENDARS: &str = "platform.reference.query.list-calendars";
/// W3.19: a calendar's days over a range.
pub const READ_CALENDAR: &str = "platform.reference.query.read-calendar";
/// W3.14: a venue held, read by its ID (contract v25, D9).
pub const RESOLVE_VENUE: &str = "platform.reference.query.resolve-venue";

const WAIT: Duration = Duration::from_secs(5);

/// The most calendars the tab reads: every page, up to this many lines.
const MOST: usize = 2_000;

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

/// What the tab is filtered by (contract v23): a kind, unspecified for
/// every kind, and a text the store matches on the name and the subject's
/// codes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub kind: i32,
    pub query: String,
}

impl Filter {
    /// From the page's query: `calendar_kind`, the kind's enum name, and
    /// `calendar_query`. A kind the schema does not define is every kind.
    pub fn from_page(kind: &str, query: &str) -> Self {
        Self {
            kind: CalendarKind::from_str_name(kind)
                .map(|kind| kind as i32)
                .unwrap_or_default(),
            query: query.trim().chars().take(QUERY_MOST).collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.kind == 0 && self.query.is_empty()
    }
}

/// The longest text the tab searches by.
const QUERY_MOST: usize = 100;

/// Every calendar held that answers the filter, every page read, and the
/// rules.
pub async fn held(bus: &meridian_bus::Bus, filter: &Filter) -> Result<ListCalendarsReply, String> {
    let mut all = ListCalendarsReply::default();
    let mut cursor = String::new();
    loop {
        let page: ListCalendarsReply = ask(
            bus,
            LIST_CALENDARS,
            "meridian.v1.ListCalendarsRequest",
            ListCalendarsRequest {
                page_size: 200,
                cursor: cursor.clone(),
                kind: filter.kind,
                query: filter.query.clone(),
            },
        )
        .await?;
        all.calendars.extend(page.calendars);
        all.rules.extend(page.rules);
        if page.next_cursor.is_empty() || all.calendars.len() >= MOST {
            return Ok(all);
        }
        cursor = page.next_cursor;
    }
}

/// What each line's subject reads as: the ISO 4217 code of each currency a
/// line names by its platform `INS-` ID, from the deployment's record carrying that ID (its `iso4217`
/// identifier, else its currency); an ID no record carries is left out, and
/// the line shows the ID itself.
pub async fn subject_codes(
    bus: &meridian_bus::Bus,
    listed: &ListCalendarsReply,
) -> BTreeMap<String, String> {
    let mut codes = BTreeMap::new();
    for id in listed
        .calendars
        .iter()
        .map(|calendar| calendar.instrument_id.as_str())
        .filter(|id| !id.is_empty())
    {
        if codes.contains_key(id) {
            continue;
        }
        let found: Result<ListInstrumentsToCompleteReply, String> = ask(
            bus,
            super::instruments::LIST_INSTRUMENTS_TO_COMPLETE,
            "meridian.v1.ListInstrumentsToCompleteRequest",
            ListInstrumentsToCompleteRequest {
                include_complete: true,
                instrument_id: String::new(),
                page_size: 10,
                cursor: String::new(),
                search: id.to_string(),
                provisional: false,
            },
        )
        .await;
        let code = found.ok().and_then(|found| {
            found
                .instruments
                .iter()
                .filter_map(|each| each.instrument.as_ref())
                // The record under the `INS-` ID the calendar names (contract
                // v24, requirement 19); from contract v25 no `open_meridian`
                // identifier is matched for a record not yet migrated.
                .find(|record| record.instrument_id == id)
                .and_then(|record| {
                    record
                        .identifiers
                        .iter()
                        .find(|held| held.scheme == "iso4217" && held.valid_until_ns == 0)
                        .map(|held| held.value.clone())
                        .or_else(|| (!record.currency.is_empty()).then(|| record.currency.clone()))
                })
        });
        if let Some(code) = code {
            codes.insert(id.to_string(), code);
        }
    }
    // Each venue by its MIC, ISO 10383's code, with its name beside it
    // (contract v25, D9): read by its `VEN-` ID from the venues held. A
    // venue not held, or not read, is left out, and the line shows its ID.
    let venue_ids: std::collections::BTreeSet<&str> = listed
        .calendars
        .iter()
        .flat_map(|calendar| calendar.venue_ids.iter().map(String::as_str))
        .collect();
    for id in venue_ids {
        let found: Result<ResolveVenueReply, String> = ask(
            bus,
            RESOLVE_VENUE,
            "meridian.v1.ResolveVenueRequest",
            ResolveVenueRequest {
                venue_id: id.to_string(),
                ..Default::default()
            },
        )
        .await;
        if let Some(words) = found
            .ok()
            .and_then(|found| found.venue)
            .map(|venue| venue_words(&venue))
        {
            codes.insert(id.to_string(), words);
        }
    }
    codes
}

/// A venue as a person reads it: its MIC, the one in force, and its name;
/// the name alone where it carries no MIC.
fn venue_words(venue: &meridian_domain::v1::VenueRecord) -> String {
    let mic = venue
        .identifiers
        .iter()
        .find(|held| held.scheme == "iso10383" && held.valid_until_ns == 0)
        .map(|held| held.value.as_str());
    match (mic, venue.name.is_empty()) {
        (Some(mic), false) => format!("{mic} ({})", venue.name),
        (Some(mic), true) => mic.to_string(),
        (None, false) => venue.name.clone(),
        (None, true) => venue.venue_id.clone(),
    }
}

/// A line's subject as a person reads it: the currency's ISO 4217 code (its
/// platform ID where no record carries it), each venue by its MIC and name
/// (its `VEN-` ID where it was not read), the settlement system's name and
/// country.
fn subject_words(calendar: &HeldCalendar, codes: &BTreeMap<String, String>) -> String {
    let mut words = Vec::new();
    if !calendar.instrument_id.is_empty() {
        words.push(
            codes
                .get(&calendar.instrument_id)
                .cloned()
                .unwrap_or_else(|| calendar.instrument_id.clone()),
        );
    }
    if !calendar.venue_ids.is_empty() {
        let venues: Vec<String> = calendar
            .venue_ids
            .iter()
            .map(|id| codes.get(id).cloned().unwrap_or_else(|| id.clone()))
            .collect();
        words.push(format!("venues {}", venues.join(", ")));
    }
    if let Some(system) = calendar.settlement_system.as_ref() {
        words.push(if system.country_code.is_empty() {
            system.name.clone()
        } else {
            format!("{}, {}", system.name, system.country_code)
        });
    }
    if words.is_empty() {
        "—".into()
    } else {
        words.join("; ")
    }
}

/// The filter's form: a kind and a text, sent to the store.
fn filter_form(filter: &Filter) -> String {
    let options: String = [
        (CalendarKind::Unspecified, "Every kind"),
        (CalendarKind::Currency, "Currency"),
        (CalendarKind::Trading, "Trading"),
        (CalendarKind::SecuritiesSettlement, "Securities settlement"),
    ]
    .iter()
    .map(|(kind, words)| {
        let value = if *kind == CalendarKind::Unspecified {
            ""
        } else {
            kind.as_str_name()
        };
        format!(
            "<option value=\"{value}\"{chosen}>{words}</option>",
            chosen = if *kind as i32 == filter.kind {
                " selected"
            } else {
                ""
            },
        )
    })
    .collect();
    format!(
        "<form class=\"filter-row calendar-filter\" method=\"get\" \
         action=\"/admin/instruments#calendars\"><select name=\"calendar_kind\" \
         aria-label=\"Kind\">{options}</select><input class=\"filter\" type=\"search\" \
         name=\"calendar_query\" maxlength=\"{QUERY_MOST}\" value=\"{query}\" \
         placeholder=\"Name, ISO code or system\" aria-label=\"Search calendars\">\
         <button type=\"submit\">Show</button></form>",
        query = escape(&filter.query),
    )
}

/// One calendar's year: its days listed from 1 January to 31 December.
pub async fn year(
    bus: &meridian_bus::Bus,
    calendar_id: &str,
    year: u16,
) -> Result<ReadCalendarReply, String> {
    ask(
        bus,
        READ_CALENDAR,
        "meridian.v1.ReadCalendarRequest",
        ReadCalendarRequest {
            calendar_id: calendar_id.to_string(),
            from_date: format!("{year:04}-01-01"),
            to_date: format!("{year:04}-12-31"),
            as_of_ns: 0,
        },
    )
    .await
}

fn kind_words(kind: i32) -> &'static str {
    match CalendarKind::try_from(kind) {
        Ok(CalendarKind::Currency) => "Currency",
        Ok(CalendarKind::Trading) => "Trading",
        Ok(CalendarKind::SecuritiesSettlement) => "Securities settlement",
        _ => "—",
    }
}

fn rule_words(kind: i32) -> &'static str {
    match meridian_pb::v1::DateRuleKind::try_from(kind) {
        Ok(meridian_pb::v1::DateRuleKind::Settlement) => "Settlement date",
        Ok(meridian_pb::v1::DateRuleKind::Spot) => "Spot date",
        _ => "—",
    }
}

fn weekday_words(day: i32) -> &'static str {
    match Weekday::try_from(day) {
        Ok(Weekday::Monday) => "Monday",
        Ok(Weekday::Tuesday) => "Tuesday",
        Ok(Weekday::Wednesday) => "Wednesday",
        Ok(Weekday::Thursday) => "Thursday",
        Ok(Weekday::Friday) => "Friday",
        Ok(Weekday::Saturday) => "Saturday",
        Ok(Weekday::Sunday) => "Sunday",
        _ => "—",
    }
}

/// The tab: the calendars and rules held that answer the filter, each with
/// its subject, and the year of the one chosen.
pub fn tab(
    listed: &Result<ListCalendarsReply, String>,
    filter: &Filter,
    codes: &BTreeMap<String, String>,
    shown: Option<(&str, u16, &Result<ReadCalendarReply, String>)>,
) -> String {
    let head = "<div class=\"section-head\"><div><h2>Calendars</h2><p class=\"hint\">The \
                settlement calendars and date rules this deployment pulled from the platform, \
                read-only: a disagreement goes to the platform, and a venue's or counterparty's \
                date is kept as stated. A date worked out from them names the rule and each \
                calendar, with their versions.</p></div></div>";
    let listed = match listed {
        Ok(listed) => listed,
        Err(why) => {
            return format!(
                "<section class=\"admin-section\" id=\"calendars\">{head}<p class=\"refused\">The \
                 instrument store is not answering: {}</p></section>",
                escape(why)
            )
        }
    };
    let rows: String = listed
        .calendars
        .iter()
        .map(|calendar| {
            format!(
                "<tr data-calendar=\"{id}\"><td title=\"{name}\"><a href=\"/admin/instruments?calendar={id}#calendars\">{name}</a> \
                 <span class=\"id\">{id}</span></td><td>{kind}</td>\
                 <td title=\"{subject}\" data-subject>{subject}</td><td>{version}</td>\
                 <td>{coverage}</td><td class=\"wide\">{refreshed}</td></tr>",
                id = escape(&calendar.calendar_id),
                name = escape(&calendar.name),
                kind = kind_words(calendar.kind),
                subject = escape(&subject_words(calendar, codes)),
                version = calendar.version,
                coverage = escape(&calendar.coverage_to_date),
                refreshed = escape(&crate::custody::utc(calendar.refreshed_at_ns)),
            )
        })
        .collect();
    let calendars = if rows.is_empty() && !filter.is_empty() {
        "<p class=\"empty\" data-calendars=\"0\">No calendar held answers this kind and \
         text.</p>"
            .to_string()
    } else if rows.is_empty() {
        "<p class=\"empty\" data-calendars=\"0\">None yet: a calendar is pulled when a \
         currency, a venue or a date read first needs it.</p>"
            .to_string()
    } else {
        format!(
            "<om-pager><table class=\"list one-line\" \
             data-calendars=\"{n}\"><thead><tr><th>Calendar</th><th>Kind</th><th>Subject</th>\
             <th>Version</th><th>Coverage to</th><th class=\"wide\">Last refreshed</th></tr>\
             </thead><tbody>{rows}</tbody></table></om-pager>",
            n = listed.calendars.len()
        )
    };
    let calendars = format!("{}{calendars}", filter_form(filter));
    let rule_rows: String = listed
        .rules
        .iter()
        .map(|rule| {
            format!(
                "<tr><td>{kind}</td><td>{subject}</td><td>{version}</td>\
                 <td class=\"wide\">{refreshed}</td></tr>",
                kind = rule_words(rule.kind),
                subject = escape(&rule.subject),
                version = rule.version,
                refreshed = escape(&crate::custody::utc(rule.refreshed_at_ns)),
            )
        })
        .collect();
    let rules = if rule_rows.is_empty() {
        "<p class=\"empty\" data-rules=\"0\">No date rule held yet.</p>".to_string()
    } else {
        format!(
            "<table class=\"list one-line\" data-rules=\"{n}\"><thead><tr><th>Rule</th>\
             <th>Subject</th><th>Version</th><th class=\"wide\">Last refreshed</th></tr></thead>\
             <tbody>{rule_rows}</tbody></table>",
            n = listed.rules.len()
        )
    };
    // A calendar's year in place of the lists, so the tab fits one screen;
    // Close returns to them.
    if let Some((id, year, read)) = shown {
        return format!(
            "<section class=\"admin-section\" id=\"calendars\">{head}{}</section>",
            shown_year(id, year, read)
        );
    }
    format!(
        "<section class=\"admin-section\" id=\"calendars\">{head}{calendars}\
         <h3>Date rules</h3>{rules}</section>"
    )
}

/// One calendar's year: its weekends, then each listed day, one line each,
/// with the year before and after.
fn shown_year(calendar_id: &str, year: u16, read: &Result<ReadCalendarReply, String>) -> String {
    let id = escape(calendar_id);
    let nav = format!(
        "<p class=\"form-foot\"><a href=\"/admin/instruments?calendar={id}&amp;year={before}#calendars\">\
         {before}</a> <strong>{year}</strong> \
         <a href=\"/admin/instruments?calendar={id}&amp;year={after}#calendars\">{after}</a> \
         <a href=\"/admin/instruments#calendars\">Close</a></p>",
        before = year.saturating_sub(1),
        after = year.saturating_add(1),
    );
    let read = match read {
        Ok(read) if read.found => read,
        Ok(_) => {
            return format!(
                "<div class=\"calendar-year\" data-year=\"{year}\"><h3>{id}</h3><p class=\"refused\">\
                 Not held: it is asked of the platform, and shown once pulled.</p>{nav}</div>"
            )
        }
        Err(why) => {
            return format!(
                "<div class=\"calendar-year\" data-year=\"{year}\"><h3>{id}</h3><p class=\"refused\">{}</p>{nav}</div>",
                escape(why)
            )
        }
    };
    let weekends = read
        .weekends
        .iter()
        .map(|weekend| {
            format!(
                "{} from {}",
                weekend
                    .weekdays
                    .iter()
                    .map(|day| weekday_words(*day))
                    .collect::<Vec<_>>()
                    .join(" and "),
                escape(&weekend.valid_from_date)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let coverage = read
        .coverage
        .as_ref()
        .map(|c| format!("{} to {}", escape(&c.from_date), escape(&c.to_date)))
        .unwrap_or_else(|| "none stated".into());
    let rows: String = read
        .days
        .iter()
        .map(|day| {
            let source = read
                .sources
                .iter()
                .find(|source| source.source_id == day.source)
                .map(|source| source.name.as_str())
                .unwrap_or(day.source.as_str());
            format!(
                "<tr><td>{date}</td><td>{kind}</td><td title=\"{name}\">{name}</td>\
                 <td class=\"wide\" title=\"{source}\">{source}</td></tr>",
                date = escape(&day.date),
                kind = if day.kind == CalendarDayKind::Open as i32 {
                    "Open (a weekend day)"
                } else {
                    "Closed"
                },
                name = escape(&day.name),
                source = escape(source),
            )
        })
        .collect();
    let days = if rows.is_empty() {
        format!("<p class=\"empty\">No day listed in {year}.</p>")
    } else {
        format!(
            "<om-pager><table class=\"list one-line\" data-days=\"{n}\"><thead><tr><th>Date</th>\
             <th>Kind</th><th>Name</th><th class=\"wide\">Source</th></tr></thead>\
             <tbody>{rows}</tbody></table></om-pager>",
            n = read.days.len()
        )
    };
    format!(
        "<div class=\"calendar-year\" data-year=\"{year}\"><h3>{id}, version {version}</h3>\
         <p class=\"hint\">Weekend: {weekends}. Coverage: {coverage}; outside it a day's status is \
         not known.</p>{days}{nav}</div>",
        version = read.version,
        weekends = if weekends.is_empty() {
            "none listed".into()
        } else {
            weekends
        },
    )
}

#[cfg(test)]
mod tests {
    use meridian_domain::v1::{
        CalendarCoverage, CalendarDay, CalendarSource, CalendarWeekend, HeldCalendar, HeldDateRule,
    };

    use super::*;

    #[test]
    fn the_tab_lists_one_line_each_and_shows_a_year_escaped() {
        let listed = Ok(ListCalendarsReply {
            calendars: vec![HeldCalendar {
                calendar_id: "CAL-USSS".into(),
                name: "US <securities> settlement".into(),
                kind: CalendarKind::SecuritiesSettlement as i32,
                version: 4,
                coverage_to_date: "2028-12-31".into(),
                refreshed_at_ns: 1_760_400_000_000_000_000,
                ..Default::default()
            }],
            next_cursor: String::new(),
            rules: vec![HeldDateRule {
                kind: meridian_pb::v1::DateRuleKind::Settlement as i32,
                subject: "US/equity".into(),
                version: 2,
                refreshed_at_ns: 1_760_400_000_000_000_000,
            }],
        });
        let read = Ok(ReadCalendarReply {
            found: true,
            calendar_id: "CAL-USSS".into(),
            version: 4,
            days: vec![CalendarDay {
                date: "2026-10-12".into(),
                kind: CalendarDayKind::Closed as i32,
                name: "Columbus Day".into(),
                source: "fed".into(),
            }],
            weekends: vec![CalendarWeekend {
                weekdays: vec![Weekday::Saturday as i32, Weekday::Sunday as i32],
                valid_from_date: "2016-01-01".into(),
            }],
            coverage: Some(CalendarCoverage {
                from_date: "2016-01-01".into(),
                to_date: "2028-12-31".into(),
            }),
            sources: vec![CalendarSource {
                source_id: "fed".into(),
                name: "Federal Reserve holidays".into(),
                ..Default::default()
            }],
            trading_sessions: None,
        });
        let none = BTreeMap::new();
        let page = tab(
            &listed,
            &Filter::default(),
            &none,
            Some(("CAL-USSS", 2026, &read)),
        );
        assert!(page.contains("Columbus Day") && page.contains("Federal Reserve holidays"));
        // The year in place of the lists, which a Close returns to.
        assert!(!page.contains("data-calendars="));
        let lists = tab(&listed, &Filter::default(), &none, None);
        assert!(lists.contains("US &lt;securities&gt; settlement"));
        assert!(lists.contains("Securities settlement"));
        assert!(lists.contains("data-calendars=\"1\""));
        assert!(lists.contains("US/equity"));
        assert!(page.contains("Saturday and Sunday from 2016-01-01"));
        assert!(page.contains("year=2025") && page.contains("year=2027"));
        // Nothing in the tab sets a calendar: its one form is the filter's GET.
        assert!(!page.contains("<form"));
        assert_eq!(lists.matches("<form").count(), 1);
        assert!(lists.contains("method=\"get\""));
        let empty = tab(
            &Ok(ListCalendarsReply::default()),
            &Filter::default(),
            &none,
            None,
        );
        assert!(empty.contains("data-calendars=\"0\"") && empty.contains("data-rules=\"0\""));
        assert!(empty.contains("None yet"));
        assert!(
            tab(&Err("no handler".into()), &Filter::default(), &none, None)
                .contains("not answering")
        );
    }

    #[test]
    fn each_line_shows_its_subject_resolved_and_the_tab_is_filtered_by_kind_and_text() {
        use meridian_domain::v1::SettlementSystem;
        let listed = Ok(ListCalendarsReply {
            calendars: vec![
                HeldCalendar {
                    calendar_id: "CAL-USD".into(),
                    name: "US dollar".into(),
                    kind: CalendarKind::Currency as i32,
                    instrument_id: "INS-USD".into(),
                    ..Default::default()
                },
                HeldCalendar {
                    calendar_id: "CAL-CHF".into(),
                    name: "Swiss franc".into(),
                    kind: CalendarKind::Currency as i32,
                    instrument_id: "INS-CHF".into(),
                    ..Default::default()
                },
                HeldCalendar {
                    calendar_id: "CAL-XNYS".into(),
                    name: "US equity venues".into(),
                    kind: CalendarKind::Trading as i32,
                    venue_ids: vec!["VEN-XNYS".into(), "VEN-XNAS".into()],
                    ..Default::default()
                },
                HeldCalendar {
                    calendar_id: "CAL-USSS".into(),
                    name: "US securities settlement".into(),
                    kind: CalendarKind::SecuritiesSettlement as i32,
                    settlement_system: Some(SettlementSystem {
                        name: "DTC <&>".into(),
                        country_code: "US".into(),
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let codes = BTreeMap::from([
            ("INS-USD".to_string(), "USD".to_string()),
            (
                "VEN-XNYS".to_string(),
                "XNYS (New York Stock Exchange)".to_string(),
            ),
        ]);
        let filter = Filter::from_page("CALENDAR_KIND_CURRENCY", "  dollar ");
        assert_eq!(
            filter,
            Filter {
                kind: CalendarKind::Currency as i32,
                query: "dollar".into()
            }
        );
        let page = tab(&listed, &filter, &codes, None);
        assert!(page.contains("<th>Subject</th>"));
        assert!(page.contains("data-subject>USD</td>"), "the ISO 4217 code");
        assert!(
            page.contains("data-subject>INS-CHF</td>"),
            "a currency no record carries is shown by its platform ID"
        );
        // A venue by its MIC and name (contract v25, D9); one not read by
        // its ID.
        assert!(page.contains("venues XNYS (New York Stock Exchange), VEN-XNAS"));
        assert!(page.contains("DTC &lt;&amp;&gt;, US"));
        assert!(page.contains("<option value=\"CALENDAR_KIND_CURRENCY\" selected>"));
        assert!(page.contains("name=\"calendar_query\" maxlength=\"100\" value=\"dollar\""));
        // A filter that matched nothing says so, not "none yet".
        let none = tab(&Ok(ListCalendarsReply::default()), &filter, &codes, None);
        assert!(none.contains("No calendar held answers"));
        // A kind the schema does not define is every kind; a long text is cut.
        let loose = Filter::from_page("CALENDAR_KIND_NOT_A_KIND", &"x".repeat(300));
        assert_eq!(loose.kind, 0);
        assert_eq!(loose.query.chars().count(), 100);
    }

    #[test]
    fn a_venue_reads_as_its_mic_and_name() {
        use meridian_domain::v1::{Identifier, VenueRecord};
        let mic = |value: &str, until: i64| Identifier {
            scheme: "iso10383".into(),
            value: value.into(),
            valid_until_ns: until,
            ..Default::default()
        };
        let venue = VenueRecord {
            venue_id: "VEN-XNYS".into(),
            name: "New York Stock Exchange".into(),
            identifiers: vec![mic("XOLD", 5), mic("XNYS", 0)],
            ..Default::default()
        };
        assert_eq!(venue_words(&venue), "XNYS (New York Stock Exchange)");
        let unnamed = VenueRecord {
            venue_id: "VEN-Q".into(),
            ..Default::default()
        };
        assert_eq!(venue_words(&unnamed), "VEN-Q");
    }
}
