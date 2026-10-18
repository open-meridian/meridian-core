//! Event groups (contract v26, W3.29, W3.30): kept, versioned, asked of the
//! platform where every member is its own, and read in force or caught up.

use std::sync::Mutex;

use super::*;
use crate::store::{Instrument, Version};
use crate::MemoryStore;

const NOW: i64 = 1_792_300_000_000_000_000;
const KALSHI: &str = "VEN-01JAE0000000000000000KLSH";

fn held(store: &MemoryStore, id: &str) {
    let record = Instrument {
        instrument_id: id.into(),
        version: 1,
        ..Default::default()
    };
    let first = Version {
        instrument_id: id.into(),
        version: 1,
        operation: "mint".into(),
        changes: Vec::new(),
        person: String::new(),
        acting_through_delegation: String::new(),
        client_name: String::new(),
        prepared_call_id: String::new(),
        instance_id: String::new(),
        note: String::new(),
        merged_instrument_id: String::new(),
        record_time_ns: 0,
    };
    store.mint(record, &format!("held:{id}"), first).unwrap();
}

/// A platform that answers as told, and remembers what it was asked.
#[derive(Default)]
struct Platform {
    answers: Mutex<Vec<Asked>>,
    asked: Mutex<Vec<EscalateEventGroupRequest>>,
}

impl Platform {
    fn answering(answers: Vec<Asked>) -> Self {
        Platform {
            answers: Mutex::new(answers),
            asked: Mutex::default(),
        }
    }
}

impl AskGroup for Platform {
    fn ask(&self, request: EscalateEventGroupRequest) -> Asked {
        self.asked.lock().unwrap().push(request);
        let mut answers = self.answers.lock().unwrap();
        if answers.is_empty() {
            Asked::Unreachable("not answering".into())
        } else {
            answers.remove(0)
        }
    }
}

fn reported(members: &[&str], exhaustive: EventGroupFlag) -> ReportEventGroupRequest {
    ReportEventGroupRequest {
        venue_id: KALSHI.into(),
        venue_event_ref: "KXFED-26NOV".into(),
        title: "Fed decision in November".into(),
        member_instrument_ids: members.iter().map(|m| m.to_string()).collect(),
        mutually_exclusive: EventGroupFlag::True as i32,
        exhaustive: exhaustive as i32,
        series_id: "KXFED".into(),
        valid_from_ns: NOW - 1,
        raw_record: None,
        publisher_instance_id: "data-kalshi-1".into(),
    }
}

fn platform_group(version: i64, members: &[&str]) -> EventGroup {
    EventGroup {
        event_group_id: "EVG-01JEVG00000000000000000001".into(),
        venue_id: KALSHI.into(),
        venue_event_ref: "KXFED-26NOV".into(),
        title: "Fed decision in November".into(),
        member_instrument_ids: members.iter().map(|m| m.to_string()).collect(),
        mutually_exclusive: EventGroupFlag::True as i32,
        exhaustive: EventGroupFlag::NotStated as i32,
        series_id: "KXFED".into(),
        version,
        valid_from_ns: NOW - 1,
        ..Default::default()
    }
}

#[test]
fn a_group_of_the_platforms_members_is_asked_and_kept_under_its_evg_id() {
    let store = MemoryStore::new();
    held(&store, "INS-A");
    held(&store, "INS-B");
    let platform = Platform::answering(vec![Asked::Answered(EscalateEventGroupReply {
        event_group: Some(platform_group(1, &["INS-A", "INS-B"])),
        minted: true,
        pending_review: false,
    })]);
    let done = report(
        &store,
        &platform,
        &reported(&["INS-B", "INS-A"], EventGroupFlag::Unspecified),
        NOW,
    )
    .unwrap()
    .unwrap();
    let group = done.reply.event_group.unwrap();
    assert_eq!(group.event_group_id, "EVG-01JEVG00000000000000000001");
    assert!(!group.provisional);
    assert_eq!(group.sequence, 1);
    assert_eq!(group.record_time_ns, NOW);
    assert_eq!(done.kept.len(), 1, "announced");
    // What was asked: members by their INS- IDs, sorted; an unstated flag
    // never false (A2); a request key; no deployment and no LCL- ID.
    let asked = platform.asked.lock().unwrap()[0].clone();
    assert_eq!(asked.member_instrument_ids, vec!["INS-A", "INS-B"]);
    assert_eq!(asked.exhaustive, EventGroupFlag::NotStated as i32);
    assert!(!asked.request_key.is_empty());
    // The same again: answered as it stands, nothing asked, nothing kept.
    let again = report(
        &store,
        &platform,
        &reported(&["INS-A", "INS-B"], EventGroupFlag::NotStated),
        NOW + 1,
    )
    .unwrap()
    .unwrap();
    assert!(again.kept.is_empty());
    assert_eq!(platform.asked.lock().unwrap().len(), 1);
}

#[test]
fn a_member_under_a_local_record_keeps_the_deployments_own_group_and_versions_it() {
    let store = MemoryStore::new();
    held(&store, "INS-A");
    held(&store, "LCL-B");
    let platform = Platform::default();
    let first = report(
        &store,
        &platform,
        &reported(&["INS-A", "LCL-B"], EventGroupFlag::NotStated),
        NOW,
    )
    .unwrap()
    .unwrap();
    let group = first.reply.event_group.unwrap();
    assert!(
        group.event_group_id.starts_with("LCL-"),
        "{}",
        group.event_group_id
    );
    assert!(group.provisional);
    assert_eq!(group.version, 1);
    assert!(
        platform.asked.lock().unwrap().is_empty(),
        "never asked with an LCL- member"
    );
    // A flag changed: a new version of the same group.
    let second = report(
        &store,
        &platform,
        &reported(&["INS-A", "LCL-B"], EventGroupFlag::False),
        NOW + 10,
    )
    .unwrap()
    .unwrap();
    let changed = second.reply.event_group.unwrap();
    assert_eq!(changed.event_group_id, group.event_group_id);
    assert_eq!(changed.version, 2);
    assert_eq!(changed.sequence, 2);
    // Read in force at a moment, and now; caught up after a sequence.
    let at = |as_of_ns| {
        list(
            &store,
            &ListEventGroupsRequest {
                instrument_ids: vec!["LCL-B".into()],
                as_of_ns,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
    };
    assert_eq!(at(0).event_groups[0].version, 2);
    assert_eq!(at(0).sequence, 2);
    let caught_up = list(
        &store,
        &ListEventGroupsRequest {
            after_sequence: 1,
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(caught_up.event_groups.len(), 1);
    assert_eq!(caught_up.event_groups[0].version, 2);
    let none = list(
        &store,
        &ListEventGroupsRequest {
            instrument_ids: vec!["INS-OTHER".into()],
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    assert!(none.event_groups.is_empty());
}

#[test]
fn a_platform_that_queues_or_cannot_be_reached_leaves_the_deployments_group() {
    let store = MemoryStore::new();
    held(&store, "INS-A");
    let platform = Platform::answering(vec![Asked::Answered(EscalateEventGroupReply {
        event_group: None,
        minted: false,
        pending_review: true,
    })]);
    let queued = report(
        &store,
        &platform,
        &reported(&["INS-A"], EventGroupFlag::True),
        NOW,
    )
    .unwrap()
    .unwrap();
    assert!(queued.reply.pending_review);
    assert!(queued.reply.event_group.unwrap().provisional);
    // Later the platform answers: its group replaces the deployment's in
    // every read from then, the deployment's kept in history.
    held(&store, "INS-B");
    let platform = Platform::answering(vec![Asked::Answered(EscalateEventGroupReply {
        event_group: Some(platform_group(1, &["INS-A", "INS-B"])),
        minted: true,
        pending_review: false,
    })]);
    report(
        &store,
        &platform,
        &reported(&["INS-A", "INS-B"], EventGroupFlag::NotStated),
        NOW + 5,
    )
    .unwrap()
    .unwrap();
    let now = list(&store, &ListEventGroupsRequest::default())
        .unwrap()
        .unwrap();
    assert_eq!(now.event_groups.len(), 1);
    assert!(now.event_groups[0]
        .event_group_id
        .starts_with(PLATFORM_PREFIX));
    assert_eq!(store.event_groups().unwrap().len(), 2, "every version kept");
}

#[test]
fn a_report_is_refused_naming_what_does_not_stand() {
    let store = MemoryStore::new();
    held(&store, "INS-A");
    let platform = Platform::default();
    let refused_for = |request: ReportEventGroupRequest| {
        report(&store, &platform, &request, NOW)
            .unwrap()
            .unwrap_err()
            .fields
    };
    assert_eq!(
        refused_for(reported(&["INS-A", "INS-GONE"], EventGroupFlag::True)),
        vec!["member_instrument_ids[1]"]
    );
    assert_eq!(
        refused_for(reported(&["INS-A", "INS-A"], EventGroupFlag::True)),
        vec!["member_instrument_ids[1]"]
    );
    assert_eq!(
        refused_for(reported(&[], EventGroupFlag::True)),
        vec!["member_instrument_ids"]
    );
    assert_eq!(
        refused_for(ReportEventGroupRequest {
            venue_id: String::new(),
            ..reported(&["INS-A"], EventGroupFlag::True)
        }),
        vec!["venue_id"]
    );
    assert_eq!(
        refused_for(ReportEventGroupRequest {
            venue_event_ref: " ".into(),
            ..reported(&["INS-A"], EventGroupFlag::True)
        }),
        vec!["venue_event_ref"]
    );
    let page = list(
        &store,
        &ListEventGroupsRequest {
            page_size: 501,
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap_err();
    assert_eq!(page.fields, vec!["page_size"]);
}

#[test]
fn a_flag_unset_reads_as_not_stated_and_says_so() {
    assert_eq!(stated(0), EventGroupFlag::NotStated as i32);
    assert_eq!(
        stated(EventGroupFlag::False as i32),
        EventGroupFlag::False as i32
    );
    assert_eq!(flag_words(0), "not stated");
    assert_eq!(flag_words(EventGroupFlag::True as i32), "yes");
}
