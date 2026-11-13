//! The Resilience page's tools (W6.39, W6.20; contract v30, stage D; MCP
//! at function parity): each a read over the conductor's row the page reads,
//! listed to a delegation covering the deployment admin's capabilities, with
//! no rule of its own.
//!
//! - `read_resilience_profile` over ReadResilienceProfile: the level, what
//!   the next lacks, each failure class and tier with its state, reason and
//!   changes, never blind per live data type.
//! - `read_resilience` over ReadResilience: the members, the partitions and
//!   a page of the history.
//! - `split_partition` over SplitPartition (step 3d; section 6.1,
//!   `dashboard__split_partition` approved 2026-11-12): a lake range halved
//!   by a deployment admin, a note saying why kept as the split's reason,
//!   answered with its `PartitionSplit`. The Partitions tab's Split dialog,
//!   which will send the same act, is stage M's.
//!
//! The moves of a partition and an account are step 3d's stage M's and have
//! no tool yet. No tool reads a secret or changes who holds access.

use std::time::Duration;

use serde_json::{json, Value};

use super::instruments::{ask, bus_refused, integer, object, only, stamp_said, text, Problems};
use super::{Area, Caller, Spec};
use crate::admin::resilience as page;
use crate::web::App;
use meridian_domain::v1::{
    ComponentReport, PartitionOwner, PartitionSplit, ReadResilienceProfileReply,
    ReadResilienceProfileRequest, ReadResilienceReply, ReadResilienceRequest, ResilienceChange,
    ResilienceLine, SplitPartitionReply, SplitPartitionRequest,
};

const WAIT: Duration = Duration::from_secs(10);

/// How long a split's one transaction is waited for.
const CHANGING: Duration = Duration::from_secs(20);

/// The act a split sends (step 3d; section 6.1): the range's writer's.
pub const SPLIT_PARTITION: &str = "platform.lake.command.split-partition";

const fn spec(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    reads: bool,
    input_schema: fn() -> Value,
) -> Spec {
    Spec {
        name,
        title,
        description,
        reads,
        open_world: false,
        input_schema,
        area: Area::Resilience,
    }
}

pub static SPECS: &[Spec] = &[
    spec(
        "read_resilience_profile",
        "Read the resilience profile",
        "what the deployment's configuration survives, computed from its chart values as the \
release states them, its members, nodes and Leases and the source priorities, never from a \
preset's name: its level (desktop, single_node, multi_node, multi_zone) and the changes the next \
lacks; each failure class and each tier met, met_by_restart (within the tolerant bound) or \
not_met, with its reason, and for each not met the change that would meet it (a chart value and \
its value, nodes or zones to add, a standby) and its cost; a class or tier is met only where CI \
tests that configuration; and never blind per live data type (a second source, standby pods, age \
and source shown). It changes nothing: a change it names is made by meridian upgrade-deployment \
--set, or at Plugins and Data sources. Answers `unchanged`.",
        true,
        nothing_schema,
    ),
    spec(
        "read_resilience",
        "Read the deployment's members, partitions and history",
        "each member of each component as it reported last (its mode writer, standby or reader, \
zone, lag behind its writer in ms, partitions and restarts), each partition of the lake and the \
book with its writer and epoch, and a page of the history, newest first: each takeover of a \
writer, integrity finding and switch of a default read's source (cursor: next_cursor from the \
page before). The book's own takeovers and integrity findings are not in it yet. Answers \
`unchanged`.",
        true,
        history_schema,
    ),
    spec(
        "split_partition",
        "Split a lake range",
        "halve one of the lake's ranges at its midpoint (a dataset's ID for its full range, or \
<dataset>/<lo>-<hi> for one split before, lowercase hex, hi inclusive), with a note saying why, \
kept as the split's reason. One transaction of the range's writer: the range closes at its \
sequence N with its rows kept, and its two halves open after it, numbering from N + 1 and \
staying with its writer group. Nothing is copied and it cannot abort; writes to the range wait \
for that one commit. Refused naming partition for the control partition, a book partition, a \
range the lake has not open and a range of one hash value. Answers the split as recorded: the \
parent, its children, N, who split it, why and when.",
        false,
        split_schema,
    ),
];

fn split_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "partition": {"type": "string", "minLength": 1, "description": "the range: a dataset's ID, or <dataset>/<lo>-<hi>"},
            "note": {"type": "string", "minLength": 1, "maxLength": 2000, "description": "why, kept with the split's record as its reason"},
        },
        "required": ["partition", "note"],
        "additionalProperties": false,
    })
}

/// A split as the tool answers it.
pub fn split_json(split: &PartitionSplit) -> Value {
    json!({
        "parent": split.parent,
        "children": split.children,
        "closing_sequence": split.closing_sequence,
        "split_by": split.split_by,
        "reason": split.reason,
        "split_at_ns": stamp_said(split.split_at_ns),
    })
}

fn nothing_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

fn history_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "page_size": {"type": "integer", "minimum": 0, "maximum": 200, "description": "history items a page holds; 0 for the read's default, 50"},
            "cursor": {"type": "string", "description": "next_cursor from the page before; empty for the first page"},
        },
        "additionalProperties": false,
    })
}

fn change_json(c: &ResilienceChange) -> Value {
    json!({"change": c.change, "cost": c.cost})
}

fn word(said: &str) -> String {
    said.to_lowercase().replace([' ', '-'], "_")
}

fn line_json(l: &ResilienceLine) -> Value {
    json!({
        "subject": l.subject,
        "state": word(page::state_word(l.state)),
        "reason": l.reason,
        "changes": l.changes.iter().map(change_json).collect::<Vec<_>>(),
    })
}

/// The profile as the tool answers it: what the Profile tab draws.
pub fn profile_json(p: &ReadResilienceProfileReply) -> Value {
    json!({
        "level": word(page::level_word(p.level)),
        "next_level": page::next_level_word(p.level).map(word),
        "next_level_changes": p.next_level_changes.iter().map(change_json).collect::<Vec<_>>(),
        "failures": p.failures.iter().map(line_json).collect::<Vec<_>>(),
        "tiers": p.tiers.iter().map(line_json).collect::<Vec<_>>(),
        "never_blind": p.never_blind.iter().map(|l| json!({
            "data_type": l.data_type,
            "second_source": l.second_source,
            "standby_pods": l.standby_pods,
            "age_and_source": l.age_and_source,
        })).collect::<Vec<_>>(),
        "computed_at_ns": stamp_said(p.computed_at_ns),
    })
}

fn member_json(m: &ComponentReport) -> Value {
    json!({
        "component": m.component,
        "member": m.member,
        "mode": m.mode,
        "zone": m.zone,
        "lag_ms": m.lag_ms,
        "partitions": m.partitions,
        "restarts": m.restarts,
        "last_restart_reason": m.last_restart_reason,
    })
}

fn partition_json(p: &PartitionOwner) -> Value {
    json!({
        "component": p.component,
        "partition": p.partition,
        "group": p.group,
        "writer": p.writer,
        "epoch": p.epoch,
    })
}

/// The members, partitions and history as the tool answers them: what the
/// other three tabs draw.
pub fn resilience_json(r: &ReadResilienceReply) -> Value {
    json!({
        "members": r.members.iter().map(member_json).collect::<Vec<_>>(),
        "partitions": r.partitions.iter().map(partition_json).collect::<Vec<_>>(),
        "history": r.history.iter().map(|item| {
            let (at, kind, words) = page::history_words(item);
            json!({"at_ns": stamp_said(at), "kind": word(kind), "words": words})
        }).collect::<Vec<_>>(),
        "next_cursor": r.next_cursor,
        "not_listed": page::BOOK_NOT_LISTED,
    })
}

/// The tool, called: its arguments read, the row sent, the answer typed.
pub async fn call(app: &App, caller: &Caller, spec: &Spec, arguments: Value) -> Value {
    let mut problems = Problems::default();
    let Some(top) = object(&arguments, "", &mut problems) else {
        return problems.refusal();
    };
    match spec.name {
        "read_resilience_profile" => {
            only(top, &[], "", &mut problems);
            if !problems.is_empty() {
                return problems.refusal();
            }
            let reply: Result<ReadResilienceProfileReply, _> = ask(
                app,
                caller,
                page::READ_RESILIENCE_PROFILE,
                "meridian.v1.ReadResilienceProfileRequest",
                ReadResilienceProfileRequest {},
                WAIT,
            )
            .await;
            match reply {
                Ok(reply) => json!({"outcome": "unchanged", "data": profile_json(&reply)}),
                Err(failed) => bus_refused(failed, ""),
            }
        }
        "read_resilience" => {
            only(top, &["page_size", "cursor"], "", &mut problems);
            let size = integer(top, "page_size", "", false, &mut problems);
            if !(0..=200).contains(&size) {
                problems.add("page_size", "0 to 200");
            }
            let request = ReadResilienceRequest {
                page_size: u32::try_from(size).unwrap_or_default(),
                cursor: text(top, "cursor", "", false, &mut problems),
            };
            if !problems.is_empty() {
                return problems.refusal();
            }
            let reply: Result<ReadResilienceReply, _> = ask(
                app,
                caller,
                page::READ_RESILIENCE,
                "meridian.v1.ReadResilienceRequest",
                request,
                WAIT,
            )
            .await;
            match reply {
                Ok(reply) => json!({"outcome": "unchanged", "data": resilience_json(&reply)}),
                Err(failed) => bus_refused(failed, ""),
            }
        }
        "split_partition" => {
            only(top, &["partition", "note"], "", &mut problems);
            let request = SplitPartitionRequest {
                partition: text(top, "partition", "", true, &mut problems),
                reason: text(top, "note", "", true, &mut problems),
            };
            if request.reason.chars().count() > 2_000 {
                problems.add("note", "at most 2,000 characters");
            }
            if !problems.is_empty() {
                return problems.refusal();
            }
            let reply: Result<SplitPartitionReply, _> = ask(
                app,
                caller,
                SPLIT_PARTITION,
                "meridian.v1.SplitPartitionRequest",
                request,
                CHANGING,
            )
            .await;
            match reply {
                Ok(SplitPartitionReply { split: Some(split) }) => {
                    json!({"outcome": "made", "data": {"split": split_json(&split)}})
                }
                Ok(_) => super::refused(
                    "unavailable",
                    "the lake answered no split: read the Resilience history before asking again",
                    Vec::new(),
                ),
                Err(failed) => bus_refused(failed, ""),
            }
        }
        other => super::refused("not_listed", &format!("no tool {other}"), Vec::new()),
    }
}

/// The tabs and route the Resilience page draws, each against the tool that
/// reaches it (W6.20; the parity check). [`tests`] fails the build where a
/// tab or a route has none.
pub const PARITY: &[(&str, &str)] = &[
    ("tab profile", "read_resilience_profile"),
    ("tab members", "read_resilience"),
    // The Partitions tab, where stage M's Split dialog sends the same act.
    ("tab partitions", "read_resilience split_partition"),
    ("tab history", "read_resilience"),
    (page::PATH, "read_resilience_profile read_resilience"),
    // What `meridian status --profile` reads (contract v30).
    ("/terminal/resilience-profile", "read_resilience_profile"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn names_tools(by: &str) -> bool {
        let tools: Vec<&str> = SPECS.iter().map(|spec| spec.name).collect();
        let named: Vec<&str> = by.split(' ').filter(|w| !w.is_empty()).collect();
        !named.is_empty() && named.iter().all(|name| tools.contains(name))
    }

    /// Every tab and the route the page draws name a tool, and every tool
    /// reaches something the page draws; each a read, at the deployment
    /// admin's level, changing nothing.
    #[test]
    fn every_tab_and_route_names_its_tool_and_every_tool_reads() {
        for (id, _) in page::TABS {
            let key = format!("tab {id}");
            let by = PARITY
                .iter()
                .find_map(|(drawn, by)| (*drawn == key).then_some(*by))
                .unwrap_or_else(|| panic!("{key} is drawn and names no tool"));
            assert!(names_tools(by), "{key}: {by:?}");
        }
        let routes: Vec<&str> = include_str!("../admin.rs")
            .split(".route(")
            .skip(1)
            .filter_map(|rest| rest.trim_start().strip_prefix("resilience::PATH"))
            .collect();
        assert_eq!(routes.len(), 1, "the page's one route");
        for (key, by) in PARITY {
            assert!(names_tools(by), "{key}: {by:?}");
        }
        for spec in SPECS {
            assert!(PARITY
                .iter()
                .any(|(_, by)| by.split(' ').any(|w| w == spec.name)));
            assert_eq!(spec.area, Area::Resilience, "{}", spec.name);
            // Each a read but the split, the one change (step 3d).
            assert_eq!(spec.reads, spec.name.starts_with("read_"), "{}", spec.name);
            assert!(
                spec.reads || spec.name == "split_partition",
                "{}",
                spec.name
            );
        }
        let open = |deployment_admin| {
            Area::Resilience.open_to(&meridian_access::Access {
                deployment_admin,
                ..Default::default()
            })
        };
        assert!(open(true) && !open(false));
    }

    /// The answers say what the page draws, by the same words.
    #[test]
    fn the_answers_are_the_pages() {
        let profile = ReadResilienceProfileReply {
            level: meridian_domain::v1::ServiceLevel::MultiNode as i32,
            failures: vec![ResilienceLine {
                subject: "a zone's node pool".into(),
                state: meridian_domain::v1::ResilienceState::NotMet as i32,
                reason: "every node in one zone".into(),
                changes: vec![ResilienceChange {
                    change: "add nodes in two more zones and set zones".into(),
                    cost: "two nodes or more".into(),
                }],
            }],
            ..Default::default()
        };
        let said = profile_json(&profile);
        assert_eq!(said["level"], "multi_node");
        assert_eq!(said["next_level"], "multi_zone");
        assert_eq!(said["failures"][0]["state"], "not_met");
        assert_eq!(
            said["failures"][0]["changes"][0]["cost"],
            "two nodes or more"
        );
        let read = resilience_json(&ReadResilienceReply::default());
        assert_eq!(read["not_listed"], page::BOOK_NOT_LISTED);
    }
}
