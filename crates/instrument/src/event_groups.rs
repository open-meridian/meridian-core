//! Event groups: a venue's grouping of event contracts, kept in the
//! instrument master (contract v26; W1.30, W3.29, W3.30;
//! spec/the-contract-gaps-v26-and-v27-close, A1 to A9, ruled 2026-10-14).
//!
//! A venue's `data` plugin reports the grouping its venue states -- the
//! venue, its event's reference, the members it resolved, the two flags as
//! the venue states them, the series and when it took effect -- and the
//! store keeps it: under the platform's `EVG-` ID once the platform matched
//! or minted it (asked through the conductor, which alone dials the
//! platform), otherwise as the deployment's own `LCL-` group while it waits,
//! the gap measure A8 allows. A group with a member under a local record is
//! never asked about: the request names members by their `INS-` IDs, never
//! the deployment and never an `LCL-` ID.
//!
//! **Versions** (A3): a group is the venue's event, its identity the venue
//! and the event's reference; each change of a flag or a member is a new
//! dated version, every version kept, so a reader computing a set's sum
//! reads the version in force at its moment. Each version kept takes the
//! next place in the store's sequence of event groups, which a reader
//! catches up by after a loss, and is heard as EventGroupChanged (W3.30).
//! **The flags as stated** (A2): unknown is never false; a flag the venue
//! does not state is `NOT_STATED`, and an unset one reads so. **Core records,
//! it does not police** (A7): that one member of a mutually exclusive and
//! exhaustive group pays is a reader's check.

use std::collections::BTreeMap;
use std::sync::Arc;

use meridian_bus::Bus;
use meridian_domain::v1::{
    AskPlatformForEventGroupReply, AskPlatformForEventGroupRequest, EscalateEventGroupReply,
    EscalateEventGroupRequest, EventGroup, EventGroupFlag, ListEventGroupsReply,
    ListEventGroupsRequest, ReportEventGroupReply, ReportEventGroupRequest,
};
use prost::Message;

use crate::store::{Result, Store};

/// W3.29: a venue's data plugin reports a grouping.
pub const REPORT_EVENT_GROUP: &str = "platform.reference.command.report-event-group";
/// W3.30: the groups holding given instruments, or changed after a sequence.
pub const LIST_EVENT_GROUPS: &str = "platform.reference.query.list-event-groups";
/// W3.30: a group reached a new version.
pub const EVENT_GROUP_CHANGED: &str = "platform.reference.event.event-group-changed";
/// W3.29: the store asks the conductor, which alone dials the platform.
pub const ASK_PLATFORM_FOR_EVENT_GROUP: &str =
    "platform.reference.query.ask-platform-for-event-group";

/// The platform's group IDs begin with this (decisions/034: never reused).
pub const PLATFORM_PREFIX: &str = "EVG-";

/// The most members one report names: as many as a read names subjects.
pub const MOST_MEMBERS: usize = 500;
/// The most characters of a venue's event reference, its series and its
/// title, as the venue writes them.
pub const MOST_REF: usize = 256;
pub const MOST_TITLE: usize = 1024;

/// What the platform said of a group the store asked about.
#[derive(Debug, Clone, PartialEq)]
pub enum Asked {
    Answered(EscalateEventGroupReply),
    /// Not reached within the bounded ask, or without v26's route: the store
    /// keeps its own group and asks again at the next report.
    Unreachable(String),
}

/// How the store asks the platform about a group: over the bus, through the
/// conductor, in a deployment; a fake in a test.
pub trait AskGroup: Send + Sync {
    fn ask(&self, request: EscalateEventGroupRequest) -> Asked;
}

/// A report's outcome: the answer, and each version newly kept, which the
/// service announces.
#[derive(Debug, Clone, PartialEq)]
pub struct Reported {
    pub reply: ReportEventGroupReply,
    pub kept: Vec<EventGroup>,
}

/// A report refused, naming each field by its path, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub fields: Vec<String>,
    pub words: String,
}

fn refused(field: impl Into<String>, words: impl Into<String>) -> Refused {
    let field = field.into();
    Refused {
        words: format!("{field}: {}", words.into()),
        fields: vec![field],
    }
}

/// A flag as kept: an unset one is the venue not stating it (A2).
pub fn stated(flag: i32) -> i32 {
    match EventGroupFlag::try_from(flag) {
        Ok(EventGroupFlag::True) => EventGroupFlag::True as i32,
        Ok(EventGroupFlag::False) => EventGroupFlag::False as i32,
        _ => EventGroupFlag::NotStated as i32,
    }
}

/// A flag as a person reads it.
pub fn flag_words(flag: i32) -> &'static str {
    match EventGroupFlag::try_from(stated(flag)) {
        Ok(EventGroupFlag::True) => "yes",
        Ok(EventGroupFlag::False) => "no",
        _ => "not stated",
    }
}

/// A group's identity: the venue and its event's reference.
fn identity(group: &EventGroup) -> (String, String) {
    (group.venue_id.clone(), group.venue_event_ref.clone())
}

/// Whether two versions say the same: the same members and flags. A title or
/// a series alone is no new version.
fn same(group: &EventGroup, members: &[String], exclusive: i32, exhaustive: i32) -> bool {
    let mut held = group.member_instrument_ids.clone();
    held.sort();
    held == members
        && stated(group.mutually_exclusive) == exclusive
        && stated(group.exhaustive) == exhaustive
}

/// Each identity's version in force at `as_of_ns` (0 for now): the latest
/// in effect then, the one kept last where two took effect at once.
pub fn in_force(versions: &[EventGroup], as_of_ns: i64) -> Vec<EventGroup> {
    let mut by_identity: BTreeMap<(String, String), &EventGroup> = BTreeMap::new();
    for version in versions {
        if as_of_ns > 0 && version.valid_from_ns > as_of_ns {
            continue;
        }
        let key = identity(version);
        let later = by_identity.get(&key).is_none_or(|held| {
            (version.valid_from_ns, version.sequence) >= (held.valid_from_ns, held.sequence)
        });
        if later {
            by_identity.insert(key, version);
        }
    }
    let mut out: Vec<EventGroup> = by_identity.into_values().cloned().collect();
    out.sort_by(|a, b| a.event_group_id.cmp(&b.event_group_id));
    out
}

/// A member's bounds as its venue states them, offered on its record as its
/// binary event contract's attributes with the floor, cap and unit the venue
/// states (contract v27, ruling 464): never in force until a person accepts
/// it; a record whose attributes already say the same is left alone.
fn offer_bounds(
    store: &dyn Store,
    member: &str,
    bound: &meridian_domain::v1::EventGroupMemberBound,
    instance_id: &str,
    now_ns: i64,
) -> Result<()> {
    use meridian_domain::instrument_type::{contract_from_text, contract_to_text};
    let Some(mut record) = store.by_id(member)? else {
        return Ok(());
    };
    // The bounds complete attributes the record holds -- its payout and
    // close -- and are offered with them; a record holding none has nothing
    // they could complete.
    let Some(held) = contract_from_text(&record.binary_event_contract) else {
        return Ok(());
    };
    let offered = meridian_domain::v1::BinaryEventContract {
        floor: bound.floor.clone(),
        cap: bound.cap.clone(),
        bound_unit: bound.bound_unit.clone(),
        ..held.clone()
    };
    if offered == held {
        return Ok(());
    }
    let offer = crate::store::Offer {
        field: crate::store::Field::BinaryEventContract,
        value: contract_to_text(&offered),
        identifier: None,
        source: format!("offered by {instance_id} from its venue's event group report"),
        instance_id: instance_id.to_string(),
        offered_at_ns: now_ns,
        valid_until_ns: 0,
        provenance: Vec::new(),
    };
    let expected = record.version;
    if !crate::resolve::keep_offers(&mut record, vec![offer]) {
        return Ok(());
    }
    record.version = expected + 1;
    record.record_time_ns = now_ns;
    let mut entry = crate::one_id::version_of(&record, "offer", Vec::new(), "", now_ns);
    entry.instance_id = instance_id.to_string();
    // A record changed meanwhile keeps its own: the venue states the bounds
    // again on its next report.
    let _ = store.write(record, expected, entry)?;
    Ok(())
}

/// W3.29: keep what a venue's data plugin reports, asking the platform to
/// match or mint it where every member is the platform's.
pub fn report(
    store: &dyn Store,
    ask: &dyn AskGroup,
    request: &ReportEventGroupRequest,
    now_ns: i64,
) -> Result<std::result::Result<Reported, Refused>> {
    let venue_id = request.venue_id.trim();
    let event_ref = request.venue_event_ref.trim();
    if venue_id.is_empty() {
        return Ok(Err(refused(
            "venue_id",
            "the venue that states the group, its venue master ID",
        )));
    }
    if event_ref.is_empty() || event_ref.chars().count() > MOST_REF {
        return Ok(Err(refused(
            "venue_event_ref",
            format!("the venue's reference for its event, 1 to {MOST_REF} characters"),
        )));
    }
    if request.series_id.chars().count() > MOST_REF {
        return Ok(Err(refused(
            "series_id",
            format!("at most {MOST_REF} characters"),
        )));
    }
    if request.title.chars().count() > MOST_TITLE {
        return Ok(Err(refused(
            "title",
            format!("at most {MOST_TITLE} characters"),
        )));
    }
    if request.member_instrument_ids.is_empty()
        || request.member_instrument_ids.len() > MOST_MEMBERS
    {
        return Ok(Err(refused(
            "member_instrument_ids",
            format!("1 to {MOST_MEMBERS} members, each resolved (W3.1) before it is sent"),
        )));
    }
    // Each member as the store holds it now: one replaced since followed to
    // what replaced it (W3.8), one it does not hold refused by its place.
    let mut members = Vec::new();
    for (i, member) in request.member_instrument_ids.iter().enumerate() {
        let mut id = member.trim().to_string();
        for _ in 0..8 {
            match store.replacement_of(&id)? {
                Some(next) => id = next,
                None => break,
            }
        }
        if id.is_empty() || store.by_id(&id)?.is_none() {
            return Ok(Err(refused(
                format!("member_instrument_ids[{i}]"),
                format!("{member:?} is no record this deployment holds; resolve it (W3.1) first"),
            )));
        }
        if members.contains(&id) {
            return Ok(Err(refused(
                format!("member_instrument_ids[{i}]"),
                format!("{id} is named twice"),
            )));
        }
        members.push(id);
    }
    // Contract v27 (W3.29; ruling 464): each range member's floor, cap and
    // unit as the venue states them, offered on the member's record -- a
    // person's value wins, and nothing is in force until accepted.
    let mut bounds = Vec::new();
    for (i, bound) in request.member_bounds.iter().enumerate() {
        let Some(at) = request
            .member_instrument_ids
            .iter()
            .position(|member| member.trim() == bound.instrument_id.trim())
        else {
            return Ok(Err(refused(
                format!("member_bounds[{i}].instrument_id"),
                format!("{:?} is not one of the report's members", bound.instrument_id),
            )));
        };
        bounds.push((members[at].clone(), bound.clone()));
    }
    for (member, bound) in &bounds {
        offer_bounds(store, member, bound, &request.publisher_instance_id, now_ns)?;
    }
    members.sort();
    let exclusive = stated(request.mutually_exclusive);
    let exhaustive = stated(request.exhaustive);
    let valid_from_ns = if request.valid_from_ns > 0 {
        request.valid_from_ns
    } else {
        now_ns
    };
    let versions = store.event_groups()?;
    let current = in_force(&versions, 0)
        .into_iter()
        .find(|group| group.venue_id == venue_id && group.venue_event_ref == event_ref);
    // Said already: answered as it stands, nothing kept and nothing asked.
    if let Some(current) = &current {
        if same(current, &members, exclusive, exhaustive) {
            return Ok(Ok(Reported {
                reply: ReportEventGroupReply {
                    event_group: Some(current.clone()),
                    pending_review: false,
                },
                kept: Vec::new(),
            }));
        }
    }
    let mut pending_review = false;
    let platforms = members.iter().all(|id| id.starts_with("INS-"));
    if platforms {
        let asked = ask.ask(EscalateEventGroupRequest {
            venue_id: venue_id.to_string(),
            venue_event_ref: event_ref.to_string(),
            title: request.title.clone(),
            member_instrument_ids: members.clone(),
            mutually_exclusive: exclusive,
            exhaustive,
            series_id: request.series_id.clone(),
            valid_from_ns,
            request_key: crate::one_id::request_key(),
        });
        match asked {
            Asked::Answered(answer) => match answer.event_group {
                Some(group) if group.event_group_id.starts_with(PLATFORM_PREFIX) => {
                    let group = EventGroup {
                        provisional: false,
                        member_instrument_ids: {
                            let mut held = group.member_instrument_ids.clone();
                            held.sort();
                            held
                        },
                        ..group
                    };
                    let held = versions.iter().find(|kept| {
                        kept.event_group_id == group.event_group_id && kept.version == group.version
                    });
                    if let Some(held) = held {
                        return Ok(Ok(Reported {
                            reply: ReportEventGroupReply {
                                event_group: Some(held.clone()),
                                pending_review: false,
                            },
                            kept: Vec::new(),
                        }));
                    }
                    let kept: Vec<EventGroup> = store
                        .keep_event_group(&group, now_ns)?
                        .into_iter()
                        .collect();
                    return Ok(Ok(Reported {
                        reply: ReportEventGroupReply {
                            event_group: kept.first().cloned().or(Some(group)),
                            pending_review: false,
                        },
                        kept,
                    }));
                }
                Some(other) => {
                    tracing::warn!(
                        group = other.event_group_id,
                        "the platform answered a group under an ID that is not its own; kept as \
                         the deployment's"
                    );
                }
                None => pending_review = answer.pending_review,
            },
            Asked::Unreachable(why) => {
                tracing::info!(
                    venue = venue_id,
                    event = event_ref,
                    why,
                    "a group kept as the deployment's while the platform cannot be reached"
                );
            }
        }
    }
    // The deployment's own group, a new version of it where it holds one.
    let (event_group_id, version) = match &current {
        Some(held) if !held.event_group_id.starts_with(PLATFORM_PREFIX) => {
            (held.event_group_id.clone(), held.version + 1)
        }
        _ => (crate::ids::local(now_ns), 1),
    };
    let group = EventGroup {
        event_group_id,
        venue_id: venue_id.to_string(),
        venue_event_ref: event_ref.to_string(),
        title: request.title.clone(),
        member_instrument_ids: members,
        mutually_exclusive: exclusive,
        exhaustive,
        series_id: request.series_id.clone(),
        version,
        valid_from_ns,
        record_time_ns: now_ns,
        provisional: true,
        sequence: 0,
    };
    let kept: Vec<EventGroup> = store
        .keep_event_group(&group, now_ns)?
        .into_iter()
        .collect();
    Ok(Ok(Reported {
        reply: ReportEventGroupReply {
            event_group: kept.first().cloned().or(Some(group)),
            pending_review,
        },
        kept,
    }))
}

/// W3.30: the groups holding given instruments or given groups, in force at
/// a time; or every version kept after a sequence, for a reader catching up.
pub fn list(
    store: &dyn Store,
    request: &ListEventGroupsRequest,
) -> Result<std::result::Result<ListEventGroupsReply, Refused>> {
    use meridian_pb::bounds::LIST_EVENT_GROUPS_REQUEST_PAGE_SIZE_RANGE as PAGE;
    if !PAGE.admits(request.page_size as i64) {
        return Ok(Err(refused(
            "page_size",
            format!("0 to {}, 0 for 100", PAGE.most),
        )));
    }
    let page = match request.page_size {
        0 => 100,
        size => size as usize,
    };
    let start: usize = match request.cursor.trim() {
        "" => 0,
        cursor => match cursor.parse() {
            Ok(n) => n,
            Err(_) => {
                return Ok(Err(refused(
                    "cursor",
                    "not a cursor this store wrote; start again from the first page",
                )))
            }
        },
    };
    let versions = store.event_groups()?;
    let sequence = versions
        .iter()
        .map(|group| group.sequence)
        .max()
        .unwrap_or(0);
    let wanted = |group: &EventGroup| {
        (request.event_group_ids.is_empty()
            || request.event_group_ids.contains(&group.event_group_id))
            && (request.instrument_ids.is_empty()
                || group
                    .member_instrument_ids
                    .iter()
                    .any(|member| request.instrument_ids.contains(member)))
    };
    let matched: Vec<EventGroup> = if request.after_sequence > 0 {
        let mut changed: Vec<EventGroup> = versions
            .into_iter()
            .filter(|group| group.sequence > request.after_sequence && wanted(group))
            .collect();
        changed.sort_by_key(|group| group.sequence);
        changed
    } else {
        in_force(&versions, request.as_of_ns)
            .into_iter()
            .filter(wanted)
            .collect()
    };
    let end = (start + page).min(matched.len());
    let next_cursor = if end < matched.len() {
        end.to_string()
    } else {
        String::new()
    };
    Ok(Ok(ListEventGroupsReply {
        event_groups: matched.get(start..end).unwrap_or_default().to_vec(),
        sequence,
        next_cursor,
    }))
}

/// The conductor asked over the bus, within the bounded ask (W3.29).
pub struct BusAskGroup {
    pub bus: Arc<Bus>,
    pub within: std::time::Duration,
}

impl AskGroup for BusAskGroup {
    fn ask(&self, request: EscalateEventGroupRequest) -> Asked {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Asked::Unreachable("no runtime".into());
        };
        let asked = handle
            .block_on(
                self.bus.call(
                    ASK_PLATFORM_FOR_EVENT_GROUP,
                    "meridian.v1.AskPlatformForEventGroupRequest",
                    AskPlatformForEventGroupRequest {
                        match_or_mint: Some(request),
                    }
                    .encode_to_vec(),
                    None,
                    Some(self.within),
                ),
            )
            .map_err(|failed| failed.to_string())
            .and_then(|(_, bytes)| {
                AskPlatformForEventGroupReply::decode(&bytes[..]).map_err(|f| f.to_string())
            });
        match asked {
            Ok(reply) if reply.reachable => Asked::Answered(reply.answer.unwrap_or_default()),
            Ok(reply) => Asked::Unreachable(reply.detail),
            Err(failed) => Asked::Unreachable(failed),
        }
    }
}

/// Register the report and the read on the bus, announcing each version
/// kept (W3.29, W3.30).
pub fn serve(
    bus: &Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn meridian_clock::Clock>,
    writing: crate::service::Writing,
    ask: Arc<dyn AskGroup>,
) {
    {
        let (store, announcing) = (store.clone(), Arc::clone(bus));
        bus.serve(REPORT_EVENT_GROUP, move |envelope| {
            let request: ReportEventGroupRequest =
                decode(&envelope, "meridian.v1.ReportEventGroupRequest")?;
            let now_ns = clock.now_ns();
            let reported = {
                let _held = writing.lock().map_err(|_| "the write lock is poisoned")?;
                report(store.as_ref(), ask.as_ref(), &request, now_ns)
                    .map_err(|failed| failed.to_string())?
            }
            .map_err(|refusal| {
                meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::Unspecified as i32,
                    &refusal.fields,
                    refusal.words,
                )
            })?;
            let meta = envelope.meta.as_ref();
            for group in &reported.kept {
                if let Err(failed) = announcing.publish(
                    EVENT_GROUP_CHANGED,
                    "meridian.v1.EventGroup",
                    group.encode_to_vec(),
                    meta.map(|meta| meta.correlation_id.as_str()),
                    meta.map(|meta| meta.message_id.as_str()),
                ) {
                    tracing::warn!(%failed, group = group.event_group_id, "a group's new version could not be announced");
                }
            }
            tracing::info!(
                by = request.publisher_instance_id,
                venue = request.venue_id,
                event = request.venue_event_ref,
                kept = reported.kept.len(),
                "an event group reported"
            );
            Ok((
                "meridian.v1.ReportEventGroupReply".to_string(),
                reported.reply.encode_to_vec(),
            ))
        });
    }
    bus.serve(LIST_EVENT_GROUPS, move |envelope| {
        let request: ListEventGroupsRequest =
            decode(&envelope, "meridian.v1.ListEventGroupsRequest")?;
        let reply = list(store.as_ref(), &request)
            .map_err(|failed| failed.to_string())?
            .map_err(|refusal| {
                meridian_bus::refusal_naming(
                    meridian_pb::v1::RefusalReason::Unspecified as i32,
                    &refusal.fields,
                    refusal.words,
                )
            })?;
        Ok((
            "meridian.v1.ListEventGroupsReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}

fn decode<M: Message + Default>(
    envelope: &meridian_bus::Envelope,
    wanted: &str,
) -> std::result::Result<M, String> {
    if envelope.payload_type != wanted {
        return Err(format!("expected {wanted}, got {}", envelope.payload_type));
    }
    M::decode(&envelope.payload[..]).map_err(|failed| format!("undecodable {wanted}: {failed}"))
}

#[cfg(test)]
mod tests;
