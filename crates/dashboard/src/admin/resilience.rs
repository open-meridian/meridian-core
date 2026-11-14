//! The Resilience page (W6.39; contract v30, stage D; the resilience spec's
//! section 12): a deployment admin's, under Settings' Deployment.
//!
//! Four tabs, each a table of one-line rows on the kit's pager, so the page
//! fits one screen at a desk and on a phone:
//!
//! - **Profile**: the level the configuration meets and what the next
//!   lacks; each failure class and tier met, met by restart or not met, with
//!   its reason and the change that would meet it and its cost; never blind
//!   per live data type (`ReadResilienceProfile`, the conductor's).
//! - **Members**: each member of each component, its mode, zone, lag and
//!   restarts, as it reported last (`ReadResilience`).
//!   The profile's advice -- partitions to add, a range to split, a writer
//!   group to add -- is its own lines, in the same shape.
//! - **Partitions**: each partition of the lake and the book, its writer
//!   group, writer and epoch; a partition's Move, a lake range's Split, and
//!   Move account, each a dialog with a note saying why and Cancel (step 3d,
//!   stage M; section 6.8), each preparable.
//! - **History**: the takeovers, moves, splits, integrity findings and
//!   source switches, newest first, a page at a time.
//!
//! The counts that add members stay chart values (`meridian
//! upgrade-deployment --set`), never set here. Every tab and dialog has its
//! tool ([`crate::mcp::resilience`]), and a tool's answer is what this page
//! shows.

use std::time::Duration;

use meridian_bus::BusError;
use meridian_domain::v1::{
    resilience_history_item::Item, ComponentReport, IntegrityCheck, MoveAbortCause, NeverBlindLine,
    PartitionOwner, ReadResilienceProfileReply, ReadResilienceProfileRequest, ReadResilienceReply,
    ReadResilienceRequest, ResilienceHistoryItem, ResilienceLine, ResilienceState, ServiceLevel,
    TakeoverReason,
};
use prost::Message;

use crate::html::escape;

pub const READ_RESILIENCE: &str = "platform.config.query.read-resilience";
pub const READ_RESILIENCE_PROFILE: &str = "platform.deployment.query.read-resilience-profile";

/// Where the page is.
pub const PATH: &str = "/admin/resilience";

/// Where its dialogs post: a partition's move, a lake range's split, an
/// account's move.
pub const MOVE_PARTITION: &str = "/admin/resilience/move-partition";
pub const SPLIT_PARTITION: &str = "/admin/resilience/split-partition";
pub const MOVE_ACCOUNT: &str = "/admin/resilience/move-account";

/// A page of the history, as the page asks it.
pub const HISTORY_PAGE: u32 = 50;

const WAIT: Duration = Duration::from_secs(10);

/// The Partitions table's columns, carried by this page alone: on a phone
/// its writer and epoch give way to the Move and Split buttons.
const PARTITIONS_STYLE: &str = "table.partitions th.num{width:4rem}table.partitions th.actions{width:9rem}\
@media (max-width:640px){table.partitions .wide{display:none}table.partitions th:first-child{width:3.5rem}\
table.partitions th.num{width:3rem}table.partitions th.actions{width:10rem}}";

/// The page's tabs.
pub const TABS: [(&str, &str); 4] = [
    ("profile", "Profile"),
    ("members", "Members"),
    ("partitions", "Partitions"),
    ("history", "History"),
];

async fn read<R: Message + Default>(
    bus: &meridian_bus::Bus,
    subject: &str,
    topic: &str,
    request_type: &str,
    request: impl Message,
) -> Result<R, String> {
    let (_, bytes) = bus
        .call_for(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            Some(WAIT),
            subject,
        )
        .await
        .map_err(|failed| match failed {
            BusError::HandlerFailed { detail, .. } => detail,
            other => other.to_string(),
        })?;
    R::decode(&bytes[..]).map_err(|failed| format!("an undecodable reply: {failed}"))
}

/// The resilience profile, as the conductor answers it.
pub async fn profile(
    bus: &meridian_bus::Bus,
    subject: &str,
) -> Result<ReadResilienceProfileReply, String> {
    read(
        bus,
        subject,
        READ_RESILIENCE_PROFILE,
        "meridian.v1.ReadResilienceProfileRequest",
        ReadResilienceProfileRequest {},
    )
    .await
}

/// The members, partitions and a page of the history.
pub async fn resilience(
    bus: &meridian_bus::Bus,
    subject: &str,
    page_size: u32,
    cursor: &str,
) -> Result<ReadResilienceReply, String> {
    read(
        bus,
        subject,
        READ_RESILIENCE,
        "meridian.v1.ReadResilienceRequest",
        ReadResilienceRequest {
            page_size,
            cursor: cursor.to_string(),
        },
    )
    .await
}

// ── Words ─────────────────────────────────────────────────────────────────────

pub fn level_word(level: i32) -> &'static str {
    match ServiceLevel::try_from(level).unwrap_or(ServiceLevel::Unspecified) {
        ServiceLevel::Desktop => "Desktop",
        ServiceLevel::SingleNode => "Single node",
        ServiceLevel::MultiNode => "Multi-node",
        ServiceLevel::MultiZone => "Multi-zone",
        ServiceLevel::Unspecified => "not said",
    }
}

/// The level above `level`, where there is one.
pub fn next_level_word(level: i32) -> Option<&'static str> {
    match ServiceLevel::try_from(level).unwrap_or(ServiceLevel::Unspecified) {
        ServiceLevel::Desktop => Some("Single node"),
        ServiceLevel::SingleNode => Some("Multi-node"),
        ServiceLevel::MultiNode => Some("Multi-zone"),
        _ => None,
    }
}

pub fn state_word(state: i32) -> &'static str {
    match ResilienceState::try_from(state).unwrap_or(ResilienceState::Unspecified) {
        ResilienceState::Met => "met",
        ResilienceState::MetByRestart => "met by restart",
        ResilienceState::NotMet => "not met",
        ResilienceState::Unspecified => "not said",
    }
}

fn state_badge(state: i32) -> String {
    let tone = match ResilienceState::try_from(state).unwrap_or(ResilienceState::Unspecified) {
        ResilienceState::Met => " good",
        ResilienceState::NotMet => " warn",
        _ => "",
    };
    format!("<span class=\"badge{tone}\">{}</span>", state_word(state))
}

pub fn takeover_word(reason: i32) -> &'static str {
    match TakeoverReason::try_from(reason).unwrap_or(TakeoverReason::Unspecified) {
        TakeoverReason::LeaseLapsed => "its lease lapsed",
        TakeoverReason::SteppedDown => "the writer stepped down",
        TakeoverReason::Moved => "a partition moved",
        TakeoverReason::DatabaseTimeline => "the database's timeline moved",
        TakeoverReason::Unspecified => "not said",
    }
}

/// Why a move did not switch, in words (MoveAbortCause, approved
/// 2026-11-12).
pub fn abort_word(cause: i32) -> &'static str {
    match MoveAbortCause::try_from(cause).unwrap_or(MoveAbortCause::Unspecified) {
        MoveAbortCause::Lag => "its target still lagged when the switch's time ran out",
        MoveAbortCause::Timeout => "its switch did not finish in time",
        MoveAbortCause::Digest => "the two digests differed",
        MoveAbortCause::Unspecified => "not said",
    }
}

pub fn check_word(check: i32) -> &'static str {
    match IntegrityCheck::try_from(check).unwrap_or(IntegrityCheck::Unspecified) {
        IntegrityCheck::PageChecksum => "a page's checksum",
        IntegrityCheck::RowDigest => "a row's digest",
        IntegrityCheck::JournalChain => "the journal's chain",
        IntegrityCheck::MemberDigest => "two members' digests",
        IntegrityCheck::Unspecified => "not said",
    }
}

/// A history item's when, kind and words, as the page and the tool both
/// say them.
pub fn history_words(item: &ResilienceHistoryItem) -> (i64, &'static str, String) {
    match &item.item {
        Some(Item::WriterTookOver(r)) => (
            r.took_over_at_ns,
            "takeover",
            format!(
                "{}: {} took over{}, epoch {}: {}",
                r.component,
                r.member,
                if r.previous_member.is_empty() {
                    String::new()
                } else {
                    format!(" from {}", r.previous_member)
                },
                r.epoch,
                takeover_word(r.reason)
            ),
        ),
        Some(Item::PartitionMoved(r)) => (
            r.moved_at_ns,
            "partition moved",
            format!(
                "{} {}: group {} to {}, epoch {}",
                r.component, r.partition, r.from_group, r.to_group, r.epoch
            ),
        ),
        // Contract v30's 3d records (approved 2026-11-12), shown once 3d
        // records them: a lake range's split, and a move that did not switch.
        Some(Item::PartitionSplit(r)) => (
            r.split_at_ns,
            "partition split",
            format!(
                "lake {}: into {} after sequence {}, by {}",
                r.parent,
                r.children.join(" and "),
                r.closing_sequence,
                r.split_by
            ),
        ),
        Some(Item::PartitionMoveAborted(r)) => (
            r.aborted_at_ns,
            "move aborted",
            if r.account_id.is_empty() {
                format!(
                    "{} {}: to group {} not switched, {}",
                    r.component,
                    r.partition,
                    r.target_group,
                    abort_word(r.cause)
                )
            } else {
                format!(
                    "{} account {}: to {} not switched, {}",
                    r.component,
                    r.account_id,
                    r.target_partition,
                    abort_word(r.cause)
                )
            },
        ),
        Some(Item::SourceSwitched(r)) => (
            r.switched_at_ns,
            "source switched",
            format!(
                "{}: {} to {}, {}",
                crate::admin::data_sources::type_word(&r.data_type),
                r.from_dataset,
                r.to_dataset,
                crate::admin::data_sources::reason_word(r.reason)
            ),
        ),
        Some(Item::AccountMoved(r)) => (
            r.moved_at_ns,
            "account moved",
            format!(
                "{}: {} to {}, by {}",
                r.account_id, r.from_partition, r.to_partition, r.moved_by
            ),
        ),
        Some(Item::IntegrityFinding(r)) => (
            r.found_at_ns,
            "integrity finding",
            format!(
                "{}{}: {} failed{}",
                r.component,
                if r.partition.is_empty() {
                    String::new()
                } else {
                    format!(" {}", r.partition)
                },
                check_word(r.check),
                if r.first_bad_sequence > 0 {
                    format!(" from sequence {}", r.first_bad_sequence)
                } else {
                    String::new()
                }
            ),
        ),
        None => (0, "not said", String::new()),
    }
}

/// A never-blind line's state and what it lacks.
pub fn never_blind_words(line: &NeverBlindLine) -> (ResilienceState, String, String) {
    let mut lacks = Vec::new();
    let mut changes = Vec::new();
    if !line.second_source {
        lacks.push("no second source in its priority");
        changes.push("entitle a second source and add it to the priority");
    }
    if !line.standby_pods {
        lacks.push("its sources run no standby pod");
        changes.push("launch its sources with a standby");
    }
    if !line.age_and_source {
        lacks.push("its pages show no age and source");
    }
    if lacks.is_empty() {
        (
            ResilienceState::Met,
            "a second source, standby pods, age and source shown".to_string(),
            String::new(),
        )
    } else {
        let shown = if line.age_and_source {
            "age and source shown; "
        } else {
            ""
        };
        (
            ResilienceState::NotMet,
            format!("{shown}{}", lacks.join("; ")),
            changes.join("; "),
        )
    }
}

fn ms_of(ns: i64) -> (String, String) {
    let at = crate::custody::utc(ns);
    let (day, time) = at.split_once(' ').unwrap_or((at.as_str(), ""));
    (day.to_string(), time.to_string())
}

// ── The tabs ──────────────────────────────────────────────────────────────────

fn section(id: &str, title: &str, about: &str, action: &str, body: &str) -> String {
    format!(
        "<section class=\"admin-section\" id=\"{id}\">\
         <div class=\"section-head\"><div><h2>{title}</h2><p class=\"hint\">{about}</p></div>{action}</div>\
         {body}</section>"
    )
}

fn line_row(kind: &str, line: &ResilienceLine) -> String {
    let change = line
        .changes
        .iter()
        .map(|c| format!("{} ({})", c.change, c.cost))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "<tr data-line=\"{kind}\"><td title=\"{subject}\">{subject}</td><td title=\"{state}\">{badge}</td>\
         <td class=\"wide\" title=\"{reason}\">{reason}</td><td title=\"{change}\">{change_or}</td></tr>",
        subject = escape(&line.subject),
        badge = state_badge(line.state),
        state = state_word(line.state),
        reason = escape(&line.reason),
        change = escape(&change),
        change_or = if change.is_empty() {
            "&mdash;".to_string()
        } else {
            escape(&change)
        },
    )
}

fn profile_tab(said: &Result<ReadResilienceProfileReply, String>) -> String {
    let profile = match said {
        Ok(profile) => profile,
        Err(why) => {
            return format!(
                "<p class=\"refused\">The conductor did not answer: {}</p>",
                escape(why)
            )
        }
    };
    let next = match next_level_word(profile.level) {
        Some(next) if !profile.next_level_changes.is_empty() => format!(
            "; {next} lacks {}",
            escape(
                &profile
                    .next_level_changes
                    .iter()
                    .map(|c| format!("{} ({})", c.change, c.cost))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        ),
        _ => String::new(),
    };
    let (day, time) = ms_of(profile.computed_at_ns);
    let head = format!(
        "<p class=\"one-line\" data-level=\"{level}\" title=\"computed {day} {time}\">\
         <strong>{level}</strong>{next}</p>",
        level = level_word(profile.level),
    );
    let mut rows: String = profile
        .failures
        .iter()
        .map(|l| line_row("failure", l))
        .collect();
    rows.push_str(
        &profile
            .advice
            .iter()
            .map(|l| line_row("advice", l))
            .collect::<String>(),
    );
    rows.push_str(
        &profile
            .tiers
            .iter()
            .map(|l| line_row("tier", l))
            .collect::<String>(),
    );
    for line in &profile.never_blind {
        let (state, reason, change) = never_blind_words(line);
        rows.push_str(&line_row(
            "never-blind",
            &ResilienceLine {
                subject: format!(
                    "never blind: {}",
                    crate::admin::data_sources::type_word(&line.data_type)
                ),
                state: state as i32,
                reason,
                changes: if change.is_empty() {
                    Vec::new()
                } else {
                    vec![meridian_domain::v1::ResilienceChange {
                        change,
                        cost: "a second vendor connection, or a pod each".to_string(),
                    }]
                },
            },
        ));
    }
    format!(
        "{head}<om-pager search=\"Search the profile\"><table class=\"list one-line profile\" \
         data-profile-lines=\"{n}\"><thead><tr><th>Failure, tier or advice</th><th>State</th>\
         <th class=\"wide\">Why</th><th>What would meet it</th></tr></thead><tbody>{rows}</tbody>\
         </table></om-pager>",
        n = profile.failures.len()
            + profile.advice.len()
            + profile.tiers.len()
            + profile.never_blind.len()
    )
}

fn member_row(m: &ComponentReport) -> String {
    let lag = if m.mode == "standby" || m.mode == "reader" {
        format!("{} ms", m.lag_ms)
    } else {
        "\u{2014}".to_string()
    };
    format!(
        "<tr data-member=\"{member}\"><td title=\"{component}\">{component}</td><td title=\"{member}\">{member}</td>\
         <td title=\"{mode}\">{mode}</td><td title=\"{zone}\">{zone}</td><td class=\"num\" title=\"{lag}\">{lag}</td>\
         <td class=\"wide num\" title=\"{restarts} {why}\">{restarts}</td></tr>",
        component = escape(&m.component),
        member = escape(if m.member.is_empty() {
            &m.component
        } else {
            &m.member
        }),
        mode = escape(if m.mode.is_empty() {
            "one copy"
        } else {
            &m.mode
        }),
        zone = escape(if m.zone.is_empty() {
            "not said"
        } else {
            &m.zone
        }),
        why = escape(&m.last_restart_reason),
        restarts = m.restarts,
    )
}

fn members_tab(members: &[ComponentReport]) -> String {
    if members.is_empty() {
        return "<p class=\"empty\" data-members=\"0\">No member has reported yet.</p>".to_string();
    }
    let rows: String = members.iter().map(member_row).collect();
    format!(
        "<om-pager search=\"Search members\"><table class=\"list one-line members\" data-members=\"{n}\">\
         <thead><tr><th>Component</th><th>Member</th><th>Mode</th><th>Zone</th><th class=\"num\">Lag</th>\
         <th class=\"wide num\">Restarts</th></tr></thead><tbody>{rows}</tbody></table></om-pager>",
        n = members.len()
    )
}

/// Whether a partition can be moved: every one but a store's `control`.
fn movable(p: &PartitionOwner) -> bool {
    p.partition != "control"
}

/// Whether a partition can be split: a lake range, never `control`.
fn splittable(p: &PartitionOwner) -> bool {
    p.component == "lake" && movable(p)
}

fn partitions_tab(partitions: &[PartitionOwner]) -> String {
    if partitions.is_empty() {
        return "<p class=\"empty\" data-partitions=\"0\">No partition answered: the lake's and the \
                book's writers are not reachable now.</p>"
            .to_string();
    }
    let rows: String = partitions
        .iter()
        .map(|p| {
            let fill = |dialog: &str, title: String| {
                format!(
                    "<button type=\"button\" data-dialog-open=\"{dialog}\" data-title=\"{title}\" \
                     data-fill=\"{fill}\">{label}</button>",
                    title = escape(&title),
                    fill = escape(
                        &serde_json::json!({"fields": {
                            "partition": p.partition,
                            "target_group": "",
                            "note": "",
                        }})
                        .to_string()
                    ),
                    label = if dialog == "move-partition" { "Move" } else { "Split" },
                )
            };
            let actions = format!(
                "{}{}",
                if movable(p) {
                    fill("move-partition", format!("Move {}", p.partition))
                } else {
                    String::new()
                },
                if splittable(p) {
                    fill("split-partition", format!("Split {}", p.partition))
                } else {
                    String::new()
                },
            );
            format!(
                "<tr data-partition=\"{partition}\"><td title=\"{component}\">{component}</td><td title=\"{partition}\">{partition}</td>\
                 <td class=\"num\" title=\"writer group {group}\">{group}</td>\
                 <td class=\"wide\" title=\"{writer_or}\">{writer_or}</td><td class=\"wide num\" title=\"{epoch}\">{epoch}</td>\
                 <td class=\"actions\">{actions}</td></tr>",
                component = escape(&p.component),
                partition = escape(&p.partition),
                group = p.group,
                writer_or = if p.writer.is_empty() {
                    "no member holds its lease".to_string()
                } else {
                    escape(&p.writer)
                },
                epoch = p.epoch,
            )
        })
        .collect();
    format!(
        "<om-pager search=\"Search partitions\"><table class=\"list one-line partitions\" data-partitions=\"{n}\">\
         <thead><tr><th>Store</th><th>Partition</th><th class=\"num\" title=\"Writer group\">Group</th>\
         <th class=\"wide\">Writer</th><th class=\"wide num\">Epoch</th><th class=\"actions\"></th></tr></thead>\
         <tbody>{rows}</tbody></table></om-pager>",
        n = partitions.len()
    )
}

/// The Partitions tab's three dialogs, each with a note saying why, Cancel,
/// and Prepare beside the act (W6.39; section 6.8).
fn dialogs(partitions: &[PartitionOwner], token: &str) -> String {
    let book: Vec<&str> = partitions
        .iter()
        .filter(|p| p.component == "bor" && movable(p))
        .map(|p| p.partition.as_str())
        .collect();
    let foot = |label: &str| {
        format!(
            "<div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>\
             <button type=\"submit\" name=\"prepare\" value=\"1\" data-prepare>Prepare</button>\
             <button type=\"submit\" class=\"primary\">{label}</button></div>"
        )
    };
    let note =
        "<label>Note <input name=\"note\" maxlength=\"2000\" required placeholder=\"Why\"></label>";
    format!(
        "<dialog id=\"move-partition\" aria-labelledby=\"move-partition-title\"><form method=\"post\" \
         action=\"{MOVE_PARTITION}\">{token}\
         <div class=\"dialog-head\"><h2 id=\"move-partition-title\">Move a partition</h2></div>\
         <div class=\"dialog-body\"><input type=\"hidden\" name=\"partition\">\
         <label>Writer group <input name=\"target_group\" type=\"number\" min=\"0\" step=\"1\" \
         inputmode=\"numeric\" required></label>{note}\
         <p class=\"hint\">The group follows its log first, then writes to it wait at most \
         moves.holdSeconds while it switches. A move that does not switch leaves it where it is, \
         its cause in the history.</p></div>{move_foot}</form></dialog>\
         <dialog id=\"split-partition\" aria-labelledby=\"split-partition-title\"><form method=\"post\" \
         action=\"{SPLIT_PARTITION}\">{token}\
         <div class=\"dialog-head\"><h2 id=\"split-partition-title\">Split a range</h2></div>\
         <div class=\"dialog-body\"><input type=\"hidden\" name=\"partition\">{note}\
         <p class=\"hint\">Halved at its midpoint in one commit of its writer, its halves \
         numbered on from where it closes and staying with its group. Nothing is copied.</p></div>\
         {split_foot}</form></dialog>\
         <dialog id=\"move-account\" aria-labelledby=\"move-account-title\"><form method=\"post\" \
         action=\"{MOVE_ACCOUNT}\">{token}\
         <div class=\"dialog-head\"><h2 id=\"move-account-title\">Move an account</h2></div>\
         <div class=\"dialog-body\"><label>Account <input name=\"account_id\" required \
         autocomplete=\"off\" placeholder=\"ACC-...\"></label>\
         <label>To partition <input name=\"target_partition\" list=\"book-partitions\" required \
         autocomplete=\"off\" placeholder=\"P1\"></label>\
         <datalist id=\"book-partitions\">{book}</datalist>{note}\
         <p class=\"hint\">Its chain closes where it is and continues there, so it reads the same; \
         acts on it wait between the two.</p></div>{account_foot}</form></dialog>",
        move_foot = foot("Move"),
        split_foot = foot("Split"),
        account_foot = foot("Move"),
        book = book
            .iter()
            .map(|p| format!("<option value=\"{}\">", escape(p)))
            .collect::<String>(),
    )
}

fn history_tab(history: &[ResilienceHistoryItem], next_cursor: &str, cursor: &str) -> String {
    let older = if next_cursor.is_empty() {
        String::new()
    } else {
        format!(
            " <a href=\"{PATH}?cursor={}#history\" data-older>Older</a>",
            escape(next_cursor)
        )
    };
    let newest = if cursor.is_empty() {
        String::new()
    } else {
        format!(" <a href=\"{PATH}#history\" data-newest>Newest</a>")
    };
    // The book's takeovers and findings are listed with every store's from
    // step 3d (section 6.3, option (c)): its control journal holds them.
    let foot = if newest.is_empty() && older.is_empty() {
        String::new()
    } else {
        format!("<p class=\"hint one-line\">{newest}{older}</p>")
    };
    if history.is_empty() {
        return format!(
            "<p class=\"empty\" data-history=\"0\">No takeover, finding or source switch recorded.</p>{foot}"
        );
    }
    let rows: String = history
        .iter()
        .map(|item| {
            let (at, kind, words) = history_words(item);
            let (day, time) = ms_of(at);
            format!(
                "<tr data-history-item=\"{kind}\"><td title=\"{day} {time}\">{day}<span class=\"dates\"> {time}</span></td>\
                 <td title=\"{kind}\">{kind}</td><td class=\"wide\" title=\"{words}\">{words}</td></tr>",
                words = escape(&words),
            )
        })
        .collect();
    format!(
        "<om-pager search=\"Search the history\"><table class=\"list one-line history\" data-history=\"{n}\">\
         <thead><tr><th>When</th><th>What</th><th class=\"wide\">Detail</th></tr></thead>\
         <tbody>{rows}</tbody></table></om-pager>{foot}",
        n = history.len()
    )
}

/// The page whole: its tabs, their tables and dialogs, and the script that
/// shows one tab at a time.
pub fn page(
    profile: &Result<ReadResilienceProfileReply, String>,
    read: &Result<ReadResilienceReply, String>,
    cursor: &str,
    token: &str,
) -> String {
    let unread = |why: &str| {
        format!(
            "<p class=\"refused\">The conductor did not answer: {}</p>",
            escape(why)
        )
    };
    let (members, partitions, history, dialogs) = match read {
        Ok(reply) => (
            members_tab(&reply.members),
            partitions_tab(&reply.partitions),
            history_tab(&reply.history, &reply.next_cursor, cursor),
            dialogs(&reply.partitions, token),
        ),
        Err(why) => (unread(why), unread(why), unread(why), dialogs(&[], token)),
    };
    let move_account =
        "<button type=\"button\" data-dialog-open=\"move-account\">Move account</button>";
    let tabs: String = TABS
        .iter()
        .map(|(id, title)| format!("<a href=\"#{id}\" data-tab=\"{id}\">{title}</a>"))
        .collect();
    format!(
        "<style>{PARTITIONS_STYLE}</style><div class=\"admin\"><div class=\"page-head\"><h1>Resilience</h1></div>\
         <nav class=\"tabs\">{tabs}</nav>{}{}{}{}{dialogs}</div>\
         <script>{}{}{}</script>",
        section(
            "profile",
            "Profile",
            "What the configuration survives, from its chart values, its members, nodes and \
             Leases and the source priorities: met only where CI tests it. It changes nothing; \
             each change it names is made by meridian upgrade-deployment --set, at Plugins and \
             Data sources, or on Partitions.",
            "",
            &profile_tab(profile)
        ),
        section(
            "members",
            "Members",
            "Each member of each component as it reported last: its mode, zone, how far behind \
             its writer, and its restarts.",
            "",
            &members
        ),
        section(
            "partitions",
            "Partitions",
            "Each partition of the lake and the book, its writer group, the member writing it \
             and the epoch it writes under. Moved to another group, or a lake range split, one at \
             a time.",
            move_account,
            &partitions
        ),
        section(
            "history",
            "History",
            "Each takeover of a writer, move, split, integrity finding and switch of a default \
             read's source, newest first.",
            "",
            &history
        ),
        super::overview::SCRIPT_START,
        super::overview::SCRIPT_END,
        super::data_sources::PAGER_SCRIPT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{ResilienceChange, SourceSwitched, WriterTookOver};

    fn reply() -> ReadResilienceReply {
        ReadResilienceReply {
            members: vec![ComponentReport {
                component: "bor".into(),
                member: "om-bor-1".into(),
                mode: "standby".into(),
                zone: "zone-a".into(),
                lag_ms: 12,
                ..Default::default()
            }],
            partitions: vec![
                PartitionOwner {
                    component: "lake".into(),
                    partition: "control".into(),
                    writer: "om-lake-0".into(),
                    epoch: 3,
                    ..Default::default()
                },
                PartitionOwner {
                    component: "lake".into(),
                    partition: "kraken-1:quotes".into(),
                    group: 1,
                    writer: "om-lake-1".into(),
                    epoch: 2,
                },
                PartitionOwner {
                    component: "bor".into(),
                    partition: "P1".into(),
                    group: 1,
                    writer: "om-bor-1".into(),
                    epoch: 1,
                },
            ],
            history: vec![
                ResilienceHistoryItem {
                    item: Some(Item::SourceSwitched(SourceSwitched {
                        data_type: "meridian.v1.Quote".into(),
                        from_dataset: "kraken-1:ticker".into(),
                        to_dataset: "coinbase-1:ticker".into(),
                        reason: 2,
                        switched_at_ns: 2,
                        ..Default::default()
                    })),
                },
                ResilienceHistoryItem {
                    item: Some(Item::WriterTookOver(WriterTookOver {
                        component: "street".into(),
                        member: "om-street-1".into(),
                        previous_member: "om-street-0".into(),
                        epoch: 4,
                        reason: 1,
                        took_over_at_ns: 1,
                        ..Default::default()
                    })),
                },
            ],
            next_cursor: "50".into(),
        }
    }

    fn profile() -> ReadResilienceProfileReply {
        ReadResilienceProfileReply {
            level: ServiceLevel::Desktop as i32,
            next_level_changes: vec![ResilienceChange {
                change: "backup.walArchive: on, with a backup schedule".into(),
                cost: "a backup volume and its storage".into(),
            }],
            failures: vec![ResilienceLine {
                subject: "a pod".into(),
                state: ResilienceState::MetByRestart as i32,
                reason: "one book writer, no standby".into(),
                changes: vec![ResilienceChange {
                    change: "bor.standby: 1".into(),
                    cost: "one pod".into(),
                }],
            }],
            advice: vec![ResilienceLine {
                subject: "the book's partitions".into(),
                state: ResilienceState::NotMet as i32,
                reason: "bor.partitions is 1 and bor.writerGroups 1".into(),
                changes: vec![ResilienceChange {
                    change: "bor.partitions: 2".into(),
                    cost: "none now".into(),
                }],
            }],
            never_blind: vec![NeverBlindLine {
                data_type: "meridian.v1.Quote".into(),
                second_source: true,
                standby_pods: false,
                age_and_source: true,
            }],
            ..Default::default()
        }
    }

    /// Every tab is drawn, one-line rows on the kit's pager; the level and
    /// what the next lacks; a line's change and cost; the history's older
    /// page; the advice in the profile's shape; the Partitions tab's three
    /// dialogs, each with a note and Cancel (step 3d, stage M).
    #[test]
    fn the_page_draws_every_tab_its_advice_and_its_dialogs() {
        let body = page(&Ok(profile()), &Ok(reply()), "", "");
        for (id, _) in TABS {
            assert!(body.contains(&format!("id=\"{id}\"")), "{id}");
        }
        assert!(body.contains("<strong>Desktop</strong>; Single node lacks backup.walArchive"));
        assert!(body.contains("met by restart"));
        assert!(body.contains("bor.standby: 1 (one pod)"));
        assert!(body.contains("never blind: Quote"));
        assert!(body.contains("its sources run no standby pod"));
        assert!(body.contains("data-members=\"1\"") && body.contains("12 ms"));
        assert!(body.contains("data-partitions=\"3\"") && body.contains("om-lake-0"));
        assert!(body.contains("street: om-street-1 took over from om-street-0, epoch 4"));
        assert!(body.contains("Quote: kraken-1:ticker to coinbase-1:ticker, stalled"));
        assert!(body.contains("?cursor=50#history"));
        assert!(
            !body.contains("not listed here yet"),
            "the book's records are listed from step 3d"
        );
        assert_eq!(body.matches("<om-pager").count(), 4);
        assert!(body.contains("data-line=\"advice\""));
        assert!(body.contains("bor.partitions: 2 (none now)"));
        assert!(body.contains("data-profile-lines=\"3\""));
        // control is neither moved nor split; a lake range is both, a book
        // partition moved only.
        assert!(!body.contains("data-title=\"Move control\""));
        assert!(body.contains("data-title=\"Move kraken-1:quotes\""));
        assert!(body.contains("data-title=\"Split kraken-1:quotes\""));
        assert!(body.contains("data-title=\"Move P1\""));
        assert!(!body.contains("data-title=\"Split P1\""));
        assert!(body.contains("<option value=\"P1\">"));
        for (id, action) in [
            ("move-partition", MOVE_PARTITION),
            ("split-partition", SPLIT_PARTITION),
            ("move-account", MOVE_ACCOUNT),
        ] {
            let at = body.find(&format!("<dialog id=\"{id}\"")).expect(id);
            let dialog = &body[at..at + body[at..].find("</dialog>").expect(id)];
            assert!(dialog.contains(&format!("action=\"{action}\"")), "{id}");
            assert!(
                dialog.contains("name=\"note\" maxlength=\"2000\" required"),
                "{id}"
            );
            assert!(dialog.contains("data-dialog-close>Cancel"), "{id}");
            assert!(dialog.contains("data-prepare>Prepare"), "{id}");
        }
        assert!(body.contains("data-dialog-open=\"move-account\">Move account"));
    }

    /// Contract v30's 3d records (approved 2026-11-12), said once 3d records
    /// them: a lake range's split, naming its children and who split it;
    /// a partition's move and an account's that did not switch, each with
    /// its cause.
    #[test]
    fn a_split_and_a_move_not_switched_are_said() {
        use meridian_domain::v1::{PartitionMoveAborted, PartitionSplit};
        let split = ResilienceHistoryItem {
            item: Some(Item::PartitionSplit(PartitionSplit {
                parent: "kraken-1:quotes".into(),
                children: vec![
                    "kraken-1:quotes/0000-7fff".into(),
                    "kraken-1:quotes/8000-ffff".into(),
                ],
                closing_sequence: 41,
                split_by: "local|ada".into(),
                reason: "hot".into(),
                split_at_ns: 7,
            })),
        };
        assert_eq!(
            history_words(&split),
            (
                7,
                "partition split",
                "lake kraken-1:quotes: into kraken-1:quotes/0000-7fff and \
                 kraken-1:quotes/8000-ffff after sequence 41, by local|ada"
                    .to_string()
            )
        );
        let digest = ResilienceHistoryItem {
            item: Some(Item::PartitionMoveAborted(PartitionMoveAborted {
                component: "lake".into(),
                partition: "kraken-1:quotes".into(),
                target_group: 1,
                cause: MoveAbortCause::Digest as i32,
                aborted_at_ns: 8,
                ..Default::default()
            })),
        };
        let (at, kind, words) = history_words(&digest);
        assert_eq!((at, kind), (8, "move aborted"));
        assert_eq!(
            words,
            "lake kraken-1:quotes: to group 1 not switched, the two digests differed"
        );
        let account = ResilienceHistoryItem {
            item: Some(Item::PartitionMoveAborted(PartitionMoveAborted {
                component: "bor".into(),
                partition: "P0".into(),
                account_id: "ACC-1".into(),
                target_partition: "P1".into(),
                cause: MoveAbortCause::Timeout as i32,
                aborted_at_ns: 9,
                ..Default::default()
            })),
        };
        assert_eq!(
            history_words(&account).2,
            "bor account ACC-1: to P1 not switched, its switch did not finish in time"
        );
    }

    /// Where the conductor does not answer, each tab says so in place.
    #[test]
    fn a_read_not_answered_is_said_in_place() {
        let body = page(&Err("timed out".into()), &Err("timed out".into()), "", "");
        assert_eq!(
            body.matches("The conductor did not answer: timed out")
                .count(),
            4
        );
    }
}
