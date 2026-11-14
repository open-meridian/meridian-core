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
//!   answered with its `PartitionSplit`; the Partitions tab's Split dialog
//!   sends the same act.
//! - `move_partition` over MovePartition (step 3d, stage M; section 6.8,
//!   `dashboard__move_partition` approved with the act): a lake range, to
//!   the lake on `platform.lake.command.move-partition`, or a book partition,
//!   to the book on `platform.book.command.move-partition`, moved to another
//!   writer group; answered when the target begins to follow it.
//! - `move_account` over MoveAccount (section 6.8; W9.8): an account
//!   continued in another book partition, answered with its `AccountMoved`.
//!
//! Each act carries a note saying why, kept as its record's reason, and is
//! preparable. No tool reads a secret or changes who holds access.

use std::time::Duration;

use serde_json::{json, Value};

use super::instruments::{ask, bus_refused, integer, object, only, stamp_said, text, Problems};
use super::{Area, Caller, Spec};
use crate::admin::resilience as page;
use crate::web::App;
use meridian_domain::v1::{
    AccountMoved, ComponentReport, MoveAccountReply, MoveAccountRequest, MovePartitionReply,
    MovePartitionRequest, PartitionOwner, PartitionSplit, ReadResilienceProfileReply,
    ReadResilienceProfileRequest, ReadResilienceReply, ReadResilienceRequest, ResilienceChange,
    ResilienceLine, SplitPartitionReply, SplitPartitionRequest,
};

const WAIT: Duration = Duration::from_secs(10);

/// How long a split's one transaction is waited for.
const CHANGING: Duration = Duration::from_secs(20);

/// The act a split sends (step 3d; section 6.1): the range's writer's.
pub const SPLIT_PARTITION: &str = "platform.lake.command.split-partition";

/// The acts a move sends (step 3d, stage M; section 6.8): a lake range's to
/// the lake, a book partition's to the book.
pub const MOVE_LAKE_PARTITION: &str = "platform.lake.command.move-partition";
pub const MOVE_BOOK_PARTITION: &str = "platform.book.command.move-partition";

/// An account's move, to the book (section 6.8; W9.8).
pub const MOVE_ACCOUNT: &str = "platform.book.command.move-account";

/// The most a note may be, as every act's reason (1 to 2,000 characters).
pub const MOST_NOTE: usize = 2_000;

/// Whether `partition` names one of the book's partitions (`P0` and those
/// placed after it), whose move goes to the book; any other, to the lake.
pub fn is_book_partition(partition: &str) -> bool {
    partition
        .strip_prefix('P')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The topic a partition's move is sent on.
pub fn move_topic(partition: &str) -> &'static str {
    if is_book_partition(partition) {
        MOVE_BOOK_PARTITION
    } else {
        MOVE_LAKE_PARTITION
    }
}

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
page before), the book's own among them. Answers \
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
    spec(
        "move_partition",
        "Move a partition to another writer group",
        "move one of the lake's ranges (a dataset's ID, or <dataset>/<lo>-<hi> for one split \
before) or one of the book's partitions (P0 and those after it) to another writer group, \
numbered from 0, with a note saying why, kept as the move's reason. The target first follows \
the partition's log (copying it first where it writes another database); the switch starts \
only once its lag is under the store's readyLagMs; writes to the partition are then held for \
at most moves.holdSeconds, never past the caller's deadline, and answered OVERLOADED with a \
retry-after past either; the source closes, the two digests are compared, the epoch is raised \
and the target owns. A move that does not switch leaves the source owning, recorded with its \
cause: lag, timeout or digest. Refused naming partition for the control partition, an account \
(move_account moves one) and a partition the store does not have, or while a move or split of \
it is under way; naming target_group for the group that owns it and one the deployment does \
not have. Answers the group it moves from and when it began; the Resilience history holds its \
record, PartitionMoved or PartitionMoveAborted.",
        false,
        move_partition_schema,
    ),
    spec(
        "move_account",
        "Move an account to another book partition",
        "continue an account in another of the book's partitions (P0 and those after it), with a \
note saying why, kept as the move's reason: an entry in its partition closes its chain, naming \
the new partition, and the next opens it there, linked by value, so its positions, lots and \
orders read the same before and after. Acts on the account wait while it is between the two. \
Refused naming target_partition for its own partition and one the book does not have; naming \
account_id for one the book does not hold, and while a move of it elsewhere is under way. \
Answers the move as recorded: the account, the partitions, the closing and opening sequences, \
who moved it, why and when.",
        false,
        move_account_schema,
    ),
];

fn note_schema() -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": MOST_NOTE, "description": "why, kept with the move's record as its reason"})
}

fn move_partition_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "partition": {"type": "string", "minLength": 1, "description": "a lake range (a dataset's ID, or <dataset>/<lo>-<hi>) or a book partition (P0, P1, ...)"},
            "target_group": {"type": "integer", "minimum": 0, "description": "the writer group to move it to, numbered from 0"},
            "note": note_schema(),
        },
        "required": ["partition", "target_group", "note"],
        "additionalProperties": false,
    })
}

fn move_account_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "account_id": {"type": "string", "minLength": 1, "description": "the account to move"},
            "target_partition": {"type": "string", "minLength": 1, "description": "the book partition it continues in: P0, P1, ..."},
            "note": note_schema(),
        },
        "required": ["account_id", "target_partition", "note"],
        "additionalProperties": false,
    })
}

/// A partition's move as the tool answers it: begun.
pub fn moving_json(partition: &str, reply: &MovePartitionReply) -> Value {
    json!({
        "partition": partition,
        "from_group": reply.from_group,
        "started_at_ns": stamp_said(reply.started_at_ns),
    })
}

/// An account's move as the tool answers it: done.
pub fn account_moved_json(moved: &AccountMoved) -> Value {
    json!({
        "account_id": moved.account_id,
        "from_partition": moved.from_partition,
        "closing_sequence": moved.closing_sequence,
        "to_partition": moved.to_partition,
        "opening_sequence": moved.opening_sequence,
        "moved_by": moved.moved_by,
        "reason": moved.reason,
        "moved_at_ns": stamp_said(moved.moved_at_ns),
    })
}

/// A note's own checks, as every act's: given, and at most 2,000
/// characters.
fn check_note(note: &str, problems: &mut Problems) {
    if note.chars().count() > MOST_NOTE {
        problems.add("note", "at most 2,000 characters");
    }
}

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
        "advice": p.advice.iter().map(line_json).collect::<Vec<_>>(),
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
            check_note(&request.reason, &mut problems);
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
        "move_partition" => {
            only(
                top,
                &["partition", "target_group", "note"],
                "",
                &mut problems,
            );
            let partition = text(top, "partition", "", true, &mut problems);
            let group = integer(top, "target_group", "", true, &mut problems);
            let target_group = u32::try_from(group).unwrap_or_else(|_| {
                problems.add("target_group", "a writer group, numbered from 0");
                0
            });
            let request = MovePartitionRequest {
                partition,
                target_group,
                reason: text(top, "note", "", true, &mut problems),
            };
            check_note(&request.reason, &mut problems);
            if !problems.is_empty() {
                return problems.refusal();
            }
            let partition = request.partition.clone();
            let reply: Result<MovePartitionReply, _> = ask(
                app,
                caller,
                move_topic(&partition),
                "meridian.v1.MovePartitionRequest",
                request,
                CHANGING,
            )
            .await;
            match reply {
                Ok(reply) => {
                    json!({"outcome": "made", "data": {"move": moving_json(&partition, &reply)}})
                }
                Err(failed) => bus_refused(failed, ""),
            }
        }
        "move_account" => {
            only(
                top,
                &["account_id", "target_partition", "note"],
                "",
                &mut problems,
            );
            let request = MoveAccountRequest {
                account_id: text(top, "account_id", "", true, &mut problems),
                target_partition: text(top, "target_partition", "", true, &mut problems),
                reason: text(top, "note", "", true, &mut problems),
            };
            check_note(&request.reason, &mut problems);
            if !problems.is_empty() {
                return problems.refusal();
            }
            let reply: Result<MoveAccountReply, _> = ask(
                app,
                caller,
                MOVE_ACCOUNT,
                "meridian.v1.MoveAccountRequest",
                request,
                CHANGING,
            )
            .await;
            match reply {
                Ok(MoveAccountReply { moved: Some(moved) }) => {
                    json!({"outcome": "made", "data": {"moved": account_moved_json(&moved)}})
                }
                Ok(_) => super::refused(
                    "unavailable",
                    "the book answered no move: read the Resilience history before asking again",
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
    // The Partitions tab and its Move, Split and Move account dialogs.
    (
        "tab partitions",
        "read_resilience move_partition split_partition move_account",
    ),
    ("tab history", "read_resilience"),
    (page::PATH, "read_resilience_profile read_resilience"),
    (page::MOVE_PARTITION, "move_partition"),
    (page::SPLIT_PARTITION, "split_partition"),
    (page::MOVE_ACCOUNT, "move_account"),
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
            .filter_map(|rest| rest.trim_start().strip_prefix("resilience::"))
            .map(|rest| rest.split(',').next().unwrap_or_default())
            .collect();
        assert_eq!(
            routes,
            ["PATH", "MOVE_PARTITION", "SPLIT_PARTITION", "MOVE_ACCOUNT"],
            "the page and its three dialogs' posts"
        );
        for (key, by) in PARITY {
            assert!(names_tools(by), "{key}: {by:?}");
        }
        for spec in SPECS {
            assert!(PARITY
                .iter()
                .any(|(_, by)| by.split(' ').any(|w| w == spec.name)));
            assert_eq!(spec.area, Area::Resilience, "{}", spec.name);
            // Each a read but the split and the two moves (step 3d), each
            // preparable.
            assert_eq!(spec.reads, spec.name.starts_with("read_"), "{}", spec.name);
            assert_eq!(
                !spec.reads,
                crate::prepared::CORE_PREPARABLE
                    .contains(&format!("dashboard__{}", spec.name).as_str()),
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
        let advised = profile_json(&ReadResilienceProfileReply {
            advice: vec![ResilienceLine {
                subject: "the book's partitions".into(),
                state: meridian_domain::v1::ResilienceState::NotMet as i32,
                reason: "bor.partitions is 1 and bor.writerGroups 1".into(),
                changes: vec![ResilienceChange {
                    change: "bor.partitions: 2".into(),
                    cost: "none now".into(),
                }],
            }],
            ..Default::default()
        });
        assert_eq!(advised["advice"][0]["subject"], "the book's partitions");
        assert_eq!(
            advised["advice"][0]["changes"][0]["change"],
            "bor.partitions: 2"
        );
        let read = resilience_json(&ReadResilienceReply::default());
        assert!(
            read.get("not_listed").is_none(),
            "the book's records are in it"
        );
    }

    /// A book partition's move goes to the book, a lake range's to the lake.
    #[test]
    fn a_move_goes_to_the_partitions_store() {
        assert_eq!(move_topic("P0"), MOVE_BOOK_PARTITION);
        assert_eq!(move_topic("P12"), MOVE_BOOK_PARTITION);
        assert_eq!(move_topic("kraken-1:quotes"), MOVE_LAKE_PARTITION);
        assert_eq!(move_topic("kraken-1:quotes/0000-7fff"), MOVE_LAKE_PARTITION);
        assert_eq!(move_topic("P"), MOVE_LAKE_PARTITION);
        assert_eq!(move_topic("Pa"), MOVE_LAKE_PARTITION);
        let moved = account_moved_json(&AccountMoved {
            account_id: "ACC-7".into(),
            from_partition: "P0".into(),
            closing_sequence: 81_240,
            to_partition: "P1".into(),
            opening_sequence: 1,
            moved_by: "local|ada".into(),
            reason: "busy".into(),
            moved_at_ns: 1,
        });
        assert_eq!(moved["closing_sequence"], 81_240);
        assert_eq!(moved["to_partition"], "P1");
    }
}
