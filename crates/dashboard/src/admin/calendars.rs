//! The Instruments page's Calendars tab (contract v22; W3.21;
//! spec/settlement-calendars-are-core-reference-data, Q18): the settlement
//! calendars and date rules the deployment holds, read-only, one line each,
//! and a calendar shown one year at a time. A deployment edits no calendar
//! (the spec's Q3): a disagreement goes to the platform. The same reads are
//! `dashboard__list_calendars` and `dashboard__read_calendar`.

use std::time::Duration;

use meridian_domain::v1::{
    CalendarDayKind, CalendarKind, ListCalendarsReply, ListCalendarsRequest, ReadCalendarReply,
    ReadCalendarRequest, Weekday,
};
use prost::Message;

use crate::html::escape;

/// W3.21: the calendars held.
pub const LIST_CALENDARS: &str = "platform.reference.query.list-calendars";
/// W3.19: a calendar's days over a range.
pub const READ_CALENDAR: &str = "platform.reference.query.read-calendar";

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

/// Every calendar held, every page read, and the rules.
pub async fn held(bus: &meridian_bus::Bus) -> Result<ListCalendarsReply, String> {
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
                ..Default::default()
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

/// The tab: the calendars and rules held, and the year of the one chosen.
pub fn tab(
    listed: &Result<ListCalendarsReply, String>,
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
                 <span class=\"id\">{id}</span></td><td>{kind}</td><td>{version}</td>\
                 <td>{coverage}</td><td class=\"wide\">{refreshed}</td></tr>",
                id = escape(&calendar.calendar_id),
                name = escape(&calendar.name),
                kind = kind_words(calendar.kind),
                version = calendar.version,
                coverage = escape(&calendar.coverage_to_date),
                refreshed = escape(&crate::custody::utc(calendar.refreshed_at_ns)),
            )
        })
        .collect();
    let calendars = if rows.is_empty() {
        "<p class=\"empty\" data-calendars=\"0\">None yet: a calendar is pulled when a \
         currency, a venue or a date read first needs it.</p>"
            .to_string()
    } else {
        format!(
            "<om-pager search=\"Search calendars\"><table class=\"list one-line\" \
             data-calendars=\"{n}\"><thead><tr><th>Calendar</th><th>Kind</th><th>Version</th>\
             <th>Coverage to</th><th class=\"wide\">Last refreshed</th></tr></thead>\
             <tbody>{rows}</tbody></table></om-pager>",
            n = listed.calendars.len()
        )
    };
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
        });
        let page = tab(&listed, Some(("CAL-USSS", 2026, &read)));
        assert!(page.contains("Columbus Day") && page.contains("Federal Reserve holidays"));
        // The year in place of the lists, which a Close returns to.
        assert!(!page.contains("data-calendars="));
        let lists = tab(&listed, None);
        assert!(lists.contains("US &lt;securities&gt; settlement"));
        assert!(lists.contains("Securities settlement"));
        assert!(lists.contains("data-calendars=\"1\""));
        assert!(lists.contains("US/equity"));
        assert!(page.contains("Saturday and Sunday from 2016-01-01"));
        assert!(page.contains("year=2025") && page.contains("year=2027"));
        // Nothing in the tab sets a calendar.
        assert!(!page.contains("<form"));
        let empty = tab(&Ok(ListCalendarsReply::default()), None);
        assert!(empty.contains("data-calendars=\"0\"") && empty.contains("data-rules=\"0\""));
        assert!(tab(&Err("no handler".into()), None).contains("not answering"));
    }
}
