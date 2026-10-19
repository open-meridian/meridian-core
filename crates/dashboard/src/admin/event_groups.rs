//! The event-group tab on an instrument's page (contract v26, W6.9, W3.30;
//! the v26 plan's question 241, ruled 2026-10-17): for a member of an event
//! group, the group's title and venue reference, its flags as stated, and
//! its members with each one's bounds, as the group stood at a date; paged
//! by the kit, one screen. Read by ListEventGroups; no page of its own.
//! Under parity `dashboard__list_event_groups`.

use std::collections::HashMap;
use std::time::Duration;

use meridian_domain::v1::{
    EventGroup, EventGroupFlag, InstrumentRecord, ListEventGroupsReply, ListEventGroupsRequest,
};
use prost::Message;

use crate::html::escape;

pub const LIST_EVENT_GROUPS: &str = "platform.reference.query.list-event-groups";

/// The most members whose records the tab reads for their bounds.
pub const MOST_MEMBERS: usize = 200;

/// ListEventGroups, asked for the person.
pub async fn list(
    bus: &meridian_bus::Bus,
    subject: &str,
    request: ListEventGroupsRequest,
) -> Result<ListEventGroupsReply, String> {
    let (_, bytes) = bus
        .call_for(
            LIST_EVENT_GROUPS,
            "meridian.v1.ListEventGroupsRequest",
            request.encode_to_vec(),
            None,
            Some(Duration::from_secs(5)),
            subject,
        )
        .await
        .map_err(|failed| failed.to_string())?;
    ListEventGroupsReply::decode(&bytes[..])
        .map_err(|failed| format!("the instrument store's answer did not read: {failed}"))
}

/// A flag as stated: yes, no, or not stated (never read as no).
pub fn flag_words(flag: i32) -> &'static str {
    match EventGroupFlag::try_from(flag) {
        Ok(EventGroupFlag::True) => "yes",
        Ok(EventGroupFlag::False) => "no",
        _ => "not stated",
    }
}

/// The moment a date asked names: the end of that UTC day; 0 (now) where
/// none or one that does not read.
pub fn as_of_ns(date: &str) -> i64 {
    meridian_domain::date::Date::parse(date.trim())
        .map(|day| (day.days_since_epoch() + 1) * 86_400_000_000_000 - 1)
        .unwrap_or(0)
}

/// The tab's section: empty where the instrument is in no group.
pub fn section(
    instrument_id: &str,
    groups: &[EventGroup],
    members: &HashMap<String, InstrumentRecord>,
    as_of: &str,
) -> String {
    if groups.is_empty() {
        return String::new();
    }
    let date_form = format!(
        "<form class=\"filter-bar\" method=\"get\" action=\"/admin/instruments/{id}#event-groups\">\
         <label class=\"grow\">As it stood on <input type=\"date\" name=\"as_of\" value=\"{d}\"></label>\
         <button type=\"submit\">Read</button></form>",
        id = escape(instrument_id),
        d = escape(as_of),
    );
    let bodies: String = groups
        .iter()
        .map(|group| {
            let rows: String = group
                .member_instrument_ids
                .iter()
                .map(|member| {
                    let record = members.get(member);
                    let bounds = record
                        .and_then(|r| r.binary_event_contract.as_ref())
                        .map(|c| {
                            meridian_domain::instrument_type::contract_words(
                                &meridian_domain::instrument_type::contract_to_text(c),
                            )
                        })
                        .unwrap_or_default();
                    format!(
                        "<tr data-member=\"{m}\"{here}><td class=\"keep\"><a href=\"/admin/instruments/{m}\">{m}</a></td>\
                         <td class=\"wide\" title=\"{d}\">{d}</td><td title=\"{b}\">{b}</td></tr>",
                        m = escape(member),
                        here = if member == instrument_id { " aria-current=\"true\"" } else { "" },
                        d = escape(record.map(|r| r.description.as_str()).unwrap_or_default()),
                        b = escape(&bounds),
                    )
                })
                .collect();
            format!(
                "<div class=\"event-group\" data-event-group=\"{gid}\"><h3>{title}</h3>\
                 <p class=\"hint\"><code>{gid}</code> · {venue_ref} · version {version}{provisional} · \
                 mutually exclusive: {me} · exhaustive: {ex}</p>\
                 <om-pager><table class=\"list one-line plugin-table\"><thead><tr><th>Member</th>\
                 <th class=\"wide\">Description</th><th>Pays, and its bounds</th></tr></thead>\
                 <tbody>{rows}</tbody></table></om-pager></div>",
                gid = escape(&group.event_group_id),
                title = escape(if group.title.is_empty() { &group.venue_event_ref } else { &group.title }),
                venue_ref = escape(&group.venue_event_ref),
                version = group.version,
                provisional = if group.provisional { " · the deployment's own" } else { "" },
                me = flag_words(group.mutually_exclusive),
                ex = flag_words(group.exhaustive),
            )
        })
        .collect();
    format!(
        "<section class=\"admin-section\" id=\"event-groups\"><div class=\"section-head\"><div>\
         <h2>Event groups</h2><p class=\"hint\">The groups this contract is a member of, as a venue \
         states them, and each member's bounds.</p></div></div>{date_form}{bodies}</section>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_sees_its_group_its_flags_as_stated_and_each_members_bounds() {
        let group = EventGroup {
            event_group_id: "EVG-01".into(),
            venue_event_ref: "KXHIGHNY-26OCT18".into(),
            title: "Highest temperature in NYC".into(),
            member_instrument_ids: vec!["INS-A".into(), "INS-B".into()],
            mutually_exclusive: EventGroupFlag::True as i32,
            exhaustive: 0,
            version: 2,
            ..Default::default()
        };
        let mut members = HashMap::new();
        members.insert(
            "INS-A".to_string(),
            InstrumentRecord {
                description: "60 to 61".into(),
                ..Default::default()
            },
        );
        let said = section("INS-A", &[group], &members, "2026-10-18");
        assert!(said.contains("id=\"event-groups\""), "{said}");
        assert!(
            said.contains("mutually exclusive: yes") && said.contains("exhaustive: not stated")
        );
        assert!(said.contains("data-member=\"INS-B\"") && said.contains("60 to 61"));
        assert!(said.contains("value=\"2026-10-18\""));
        assert!(section("INS-A", &[], &members, "").is_empty());
        assert_eq!(as_of_ns("1970-01-01"), 86_400_000_000_000 - 1);
        assert_eq!(as_of_ns(""), 0);
    }
}
