//! The custodian's activity, and each sync status (contract v14: W2.10 to
//! W2.14).
//!
//! A custody plugin reports each activity on an account as the custodian
//! states it, and the street keeps it as reported, unique on its source,
//! account and the custodian's identifier, and announces it whole (W2.12) so
//! a reconciliation waiting on a cause re-runs its candidates. The street
//! hears every sync status a custody plugin publishes, beside the dashboard,
//! and keeps each as a record of its own (W2.13), so an `operations` plugin
//! tells a connection that needs a person to sign in again apart from data
//! that is merely old; and a read of an account's activity answers how far
//! back its history reaches from the latest it keeps (W2.11).
//!
//! From contract v15 an activity whose instrument resolves later is
//! re-resolved (W2.15): the street keeps each re-resolution as a record of
//! its own beside the activity as first recorded, which never changes,
//! chained per account apart from the activities, announces it whole
//! (W2.16), and answers it beside the activity on a read.
//!
//! # Nothing is derived
//!
//! Activity is evidence, never a source (the spec's requirement 8): nothing
//! here derives a position, a lot or a cash figure from it, and nothing in the
//! rest of the street reads it. The record is kept as the plugin encoded it;
//! the store takes out only what a read selects and orders by.

use meridian_domain::v1::{
    ActivityReResolution, ActivityReResolvedEvent, ActivityRecordedEvent, CustodialActivity,
    ListActivitiesReply, ListActivitiesRequest, ListSyncStatusesReply, ListSyncStatusesRequest,
    ReResolveActivityReply, ReResolveActivityRequest, RecordActivityReply, RecordActivityRequest,
    SyncStatusEvent, SyncStatusRecordedEvent,
};
use meridian_pb::bounds::{
    LIST_ACTIVITIES_REQUEST_PAGE_SIZE_RANGE, LIST_SYNC_STATUSES_REQUEST_PAGE_SIZE_RANGE,
};
use prost::Message;

use crate::amounts::Quantity;
use crate::ids;
use crate::positions::{as_of, limit, since};
use crate::record::{to_wire_cause, to_wire_journal};
use crate::store::{
    ActivitiesRead, Activity, Cause, Completed, Kept, ReResolution, Result, Scope, Store,
    StoreError, SyncStatus, SyncStatusesRead,
};

/// What recording an activity did: the reply, and the event to announce when
/// it was recorded now. A redelivery announces nothing, so a subscriber's
/// arithmetic never depends on how many times it heard.
#[derive(Debug)]
pub struct RecordedActivity {
    pub reply: RecordActivityReply,
    pub event: Option<ActivityRecordedEvent>,
}

/// W2.10, then W2.12: one activity recorded against the account the sidecar
/// stamped, or the one held answered as already recorded.
pub fn record_activity(
    store: &dyn Store,
    request: &RecordActivityRequest,
    cause: &Cause,
) -> Result<RecordedActivity> {
    let activity = request.activity.as_ref().ok_or_else(|| {
        StoreError::Edge("activity is unset; a record carries its activity".into())
    })?;
    let units = Quantity::reported("activity.units", activity.units.as_ref())?;

    let (held, kept) = store.record_activity(
        Activity {
            activity_id: ids::activity(cause.committed_at_ns),
            account_id: request.account_id.clone(),
            external_account_id: request.external_account_id.clone(),
            source: request.source.clone(),
            external_activity_id: activity.external_activity_id.clone(),
            trade_date: activity.trade_date.clone(),
            kind: activity.kind,
            instrument_id: activity.instrument_id.clone(),
            units,
            record: activity.encode_to_vec(),
            recorded: Completed::default(),
        },
        cause,
    )?;

    Ok(RecordedActivity {
        reply: RecordActivityReply {
            activity_id: held.activity_id.clone(),
            already_recorded: kept == Kept::AlreadyRecorded,
        },
        event: match kept {
            Kept::Recorded => Some(activity_recorded(&held)?),
            Kept::AlreadyRecorded => None,
        },
    })
}

/// What re-resolving an activity did: the reply, and the event to announce
/// when a re-resolution was recorded now. One answered as already recorded
/// announces nothing.
#[derive(Debug)]
pub struct ReResolved {
    pub reply: ReResolveActivityReply,
    pub event: Option<ActivityReResolvedEvent>,
}

/// W2.15, then W2.16: the activity named by its source, the account the
/// sidecar stamped and the custodian's identifier re-resolved, its
/// re-resolution kept beside it; or, naming what its latest resolution
/// names, answered as already recorded. Refused naming the activity when
/// none is recorded under that identity.
pub fn re_resolve_activity(
    store: &dyn Store,
    request: &ReResolveActivityRequest,
    cause: &Cause,
) -> Result<ReResolved> {
    let (held, kept) = store.re_resolve(
        ReResolution {
            activity_id: String::new(),
            account_id: request.account_id.clone(),
            source: request.source.clone(),
            external_activity_id: request.external_activity_id.clone(),
            instrument_id: request.instrument_id.clone(),
            provenance: request
                .provenance
                .as_ref()
                .map(Message::encode_to_vec)
                .unwrap_or_default(),
            resolved_at_ns: request.resolved_at_ns,
            recorded: Completed::default(),
        },
        cause,
    )?;
    Ok(ReResolved {
        reply: ReResolveActivityReply {
            activity_id: held.activity_id.clone(),
            already_recorded: kept == Kept::AlreadyRecorded,
        },
        event: match kept {
            Kept::Recorded => Some(activity_re_resolved(&held)?),
            Kept::AlreadyRecorded => None,
        },
    })
}

/// W2.11: activity within the reader's scope, each as it was announced, with
/// the named account's `history_from`.
pub fn list_activities(
    store: &dyn Store,
    request: &ListActivitiesRequest,
    scope: &Scope,
) -> Result<ListActivitiesReply> {
    let page = store.activities(&ActivitiesRead {
        scope: scope.clone(),
        account_id: request.account_id.clone(),
        trade_date_from: request.trade_date_from.clone(),
        trade_date_to: request.trade_date_to.clone(),
        limit: limit(request.page_size, LIST_ACTIVITIES_REQUEST_PAGE_SIZE_RANGE),
        cursor: request.cursor.clone(),
        since: since(request.since.as_ref()),
    })?;
    Ok(ListActivitiesReply {
        activities: page
            .activities
            .iter()
            .map(activity_recorded)
            .collect::<Result<_>>()?,
        next_cursor: page.next_cursor,
        as_of: Some(as_of(page.as_of)),
        history_from: page.history_from,
        re_resolutions: page
            .re_resolutions
            .iter()
            .map(activity_re_resolution)
            .collect::<Result<_>>()?,
    })
}

/// W2.13: a sync status a custody plugin published, kept as published and
/// announced. Every one heard is recorded; one for an unlinked external
/// account is kept with its account empty and delivered to no plugin.
pub fn record_sync_status(
    store: &dyn Store,
    event: &SyncStatusEvent,
    cause: &Cause,
) -> Result<SyncStatusRecordedEvent> {
    let held = store.record_sync_status(
        SyncStatus {
            account_id: event.account_id.clone(),
            external_account_id: event.external_account_id.clone(),
            source: event.source.clone(),
            state: event.state,
            history_from: event.history_from.clone(),
            record: event.encode_to_vec(),
            recorded: Completed::default(),
            not_known_before: String::new(),
        },
        cause,
    )?;
    sync_status_recorded(&held)
}

/// W2.14: the latest sync status of each connection within the reader's
/// scope, or every one recorded since a watermark.
pub fn list_sync_statuses(
    store: &dyn Store,
    request: &ListSyncStatusesRequest,
    scope: &Scope,
) -> Result<ListSyncStatusesReply> {
    let page = store.sync_statuses(&SyncStatusesRead {
        scope: scope.clone(),
        account_id: request.account_id.clone(),
        limit: limit(
            request.page_size,
            LIST_SYNC_STATUSES_REQUEST_PAGE_SIZE_RANGE,
        ),
        cursor: request.cursor.clone(),
        since: since(request.since.as_ref()),
    })?;
    Ok(ListSyncStatusesReply {
        statuses: page
            .statuses
            .iter()
            .map(sync_status_recorded)
            .collect::<Result<_>>()?,
        next_cursor: page.next_cursor,
        as_of: Some(as_of(page.as_of)),
    })
}

/// An activity as it was announced (W2.12) and as it is read (W2.11).
fn activity_recorded(activity: &Activity) -> Result<ActivityRecordedEvent> {
    Ok(ActivityRecordedEvent {
        activity_id: activity.activity_id.clone(),
        account_id: activity.account_id.clone(),
        external_account_id: activity.external_account_id.clone(),
        source: activity.source.clone(),
        activity: Some(decoded::<CustodialActivity>(
            &activity.record,
            "an activity",
        )?),
        recorded_at_ns: activity.recorded.cause.committed_at_ns,
        journal: Some(to_wire_journal(activity.recorded.change)),
        cause: Some(to_wire_cause(&activity.recorded.cause)),
    })
}

/// A re-resolution as the street keeps it, announced (W2.16) and read
/// (W2.11).
fn activity_re_resolution(re: &ReResolution) -> Result<ActivityReResolution> {
    Ok(ActivityReResolution {
        activity_id: re.activity_id.clone(),
        account_id: re.account_id.clone(),
        instrument_id: re.instrument_id.clone(),
        provenance: Some(decoded(&re.provenance, "a re-resolution's provenance")?),
        resolved_at_ns: re.resolved_at_ns,
        recorded_at_ns: re.recorded.cause.committed_at_ns,
        journal: Some(to_wire_journal(re.recorded.change)),
    })
}

/// A re-resolution announced (W2.16): its record whole, its account, who
/// caused it and its number at the top, as every delivered record carries
/// them.
fn activity_re_resolved(re: &ReResolution) -> Result<ActivityReResolvedEvent> {
    Ok(ActivityReResolvedEvent {
        account_id: re.account_id.clone(),
        re_resolution: Some(activity_re_resolution(re)?),
        cause: Some(to_wire_cause(&re.recorded.cause)),
        journal: Some(to_wire_journal(re.recorded.change)),
    })
}

/// A sync status as it was announced (W2.13) and as it is read (W2.14).
fn sync_status_recorded(status: &SyncStatus) -> Result<SyncStatusRecordedEvent> {
    Ok(SyncStatusRecordedEvent {
        status: Some(decoded::<SyncStatusEvent>(&status.record, "a sync status")?),
        recorded_at_ns: status.recorded.cause.committed_at_ns,
        journal: Some(to_wire_journal(status.recorded.change)),
        cause: Some(to_wire_cause(&status.recorded.cause)),
    })
}

/// A record the store kept, read back. One that does not decode is a row no
/// path here wrote, so it is reported rather than answered empty.
fn decoded<M: Message + Default>(record: &[u8], what: &str) -> Result<M> {
    M::decode(record).map_err(|failed| {
        StoreError::Unavailable(format!(
            "{what} the street kept does not read back: {failed}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use meridian_domain::v1::{ActivityKind, PartitionSequence, SyncState, Watermark};

    use super::*;
    use crate::amounts::testing::{quantity, usd};
    use crate::MemoryStore;

    const NOW: i64 = 1_759_276_800_000_000_000;

    fn by(instance: &str, at: i64) -> Cause {
        Cause {
            instance_id: instance.into(),
            correlation_id: "corr-1".into(),
            causation_id: format!("msg-{at}"),
            committed_at_ns: at,
            ..Default::default()
        }
    }

    fn reinvestment(account: &str, id: &str, trade_date: &str) -> RecordActivityRequest {
        RecordActivityRequest {
            account_id: account.into(),
            external_account_id: format!("SNAP-{account}"),
            source: "snaptrade".into(),
            activity: Some(CustodialActivity {
                external_activity_id: id.into(),
                kind: ActivityKind::Reinvestment as i32,
                instrument_id: "INS-SPAXX".into(),
                trade_date: trade_date.into(),
                units: quantity("3.27"),
                amount: usd("-3.27"),
                description: "REINVESTMENT SPAXX".into(),
                ..Default::default()
            }),
        }
    }

    fn sync(account: &str, state: SyncState, history_from: &str) -> SyncStatusEvent {
        SyncStatusEvent {
            source: "snaptrade".into(),
            account_id: account.into(),
            external_account_id: format!("SNAP-{account}"),
            state: state as i32,
            history_from: history_from.into(),
            observed_at_ns: NOW,
            ..Default::default()
        }
    }

    fn within(accounts: &[&str]) -> Scope {
        Scope::Within(
            accounts
                .iter()
                .map(|a| a.to_string())
                .collect::<BTreeSet<_>>(),
        )
    }

    fn watermark(sequence: u64) -> Watermark {
        Watermark {
            partitions: vec![PartitionSequence {
                partition: "street".into(),
                sequence,
            }],
        }
    }

    #[test]
    fn an_activity_is_recorded_once_and_announced_whole_with_its_cause() {
        let store = MemoryStore::new();
        let request = reinvestment("ACC-1", "a3f0", "2026-09-30");
        let first = record_activity(&store, &request, &by("custody-1", NOW)).unwrap();
        assert!(first.reply.activity_id.starts_with("ACT-"));
        assert!(!first.reply.already_recorded);
        let event = first.event.expect("a new activity is announced");
        assert_eq!(event.activity, request.activity);
        assert_eq!(event.account_id, "ACC-1");
        assert_eq!(event.external_account_id, "SNAP-ACC-1");
        assert_eq!(event.recorded_at_ns, NOW);
        let journal = event.journal.unwrap();
        assert_eq!((journal.sequence, journal.previous_sequence), (1, 0));
        assert_eq!(event.cause.unwrap().instance_id, "custody-1");

        // Sent again: the one held, nothing announced, nothing numbered.
        let again = record_activity(&store, &request, &by("custody-1", NOW + 1)).unwrap();
        assert!(again.reply.already_recorded);
        assert_eq!(again.reply.activity_id, first.reply.activity_id);
        assert!(again.event.is_none());
        let read = list_activities(
            &store,
            &ListActivitiesRequest::default(),
            &Scope::Everything,
        )
        .unwrap();
        assert_eq!(read.activities.len(), 1);
        assert_eq!(read.as_of.unwrap().partitions[0].sequence, 1);
    }

    #[test]
    fn the_same_identifier_on_another_account_or_source_is_another_activity() {
        let store = MemoryStore::new();
        record_activity(
            &store,
            &reinvestment("ACC-1", "a3f0", "2026-09-30"),
            &by("c", NOW),
        )
        .unwrap();
        let other_account = record_activity(
            &store,
            &reinvestment("ACC-2", "a3f0", "2026-09-30"),
            &by("c", NOW),
        )
        .unwrap();
        assert!(!other_account.reply.already_recorded);
        let mut restated = reinvestment("ACC-1", "a3f0", "2026-09-30");
        restated.source = "plaid".into();
        assert!(
            !record_activity(&store, &restated, &by("c", NOW))
                .unwrap()
                .reply
                .already_recorded
        );
    }

    #[test]
    fn an_activity_is_chained_per_account_apart_from_positions_and_statements() {
        let store = MemoryStore::new();
        let one = record_activity(
            &store,
            &reinvestment("ACC-1", "1", "2026-09-01"),
            &by("c", NOW),
        )
        .unwrap()
        .event
        .unwrap();
        let other = record_activity(
            &store,
            &reinvestment("ACC-2", "2", "2026-09-01"),
            &by("c", NOW),
        )
        .unwrap()
        .event
        .unwrap();
        let two = record_activity(
            &store,
            &reinvestment("ACC-1", "3", "2026-09-02"),
            &by("c", NOW),
        )
        .unwrap()
        .event
        .unwrap();
        assert_eq!(one.journal.unwrap().previous_sequence, 0);
        assert_eq!(other.journal.unwrap().previous_sequence, 0);
        let two = two.journal.unwrap();
        assert_eq!((two.sequence, two.previous_sequence), (3, 1));
    }

    #[test]
    fn an_activity_naming_no_account_or_identifier_is_refused() {
        let store = MemoryStore::new();
        let unlinked = reinvestment("", "1", "2026-09-01");
        assert!(record_activity(&store, &unlinked, &by("c", NOW)).is_err());
        let unnamed = reinvestment("ACC-1", "", "2026-09-01");
        assert!(record_activity(&store, &unnamed, &by("c", NOW)).is_err());
        let mut empty = reinvestment("ACC-1", "1", "2026-09-01");
        empty.activity = None;
        assert!(record_activity(&store, &empty, &by("c", NOW)).is_err());
    }

    #[test]
    fn activity_is_read_by_trade_date_inclusive_and_paged() {
        let store = MemoryStore::new();
        for (id, date) in [
            ("c", "2026-09-30"),
            ("a", "2026-09-09"),
            ("b", "2026-09-15"),
        ] {
            record_activity(&store, &reinvestment("ACC-1", id, date), &by("c", NOW)).unwrap();
        }
        let first = list_activities(
            &store,
            &ListActivitiesRequest {
                account_id: "ACC-1".into(),
                trade_date_from: "2026-09-09".into(),
                trade_date_to: "2026-09-30".into(),
                page_size: 2,
                ..Default::default()
            },
            &Scope::Everything,
        )
        .unwrap();
        let dates: Vec<_> = first
            .activities
            .iter()
            .map(|a| a.activity.as_ref().unwrap().trade_date.clone())
            .collect();
        assert_eq!(dates, ["2026-09-09", "2026-09-15"]);
        assert!(!first.next_cursor.is_empty());
        let second = list_activities(
            &store,
            &ListActivitiesRequest {
                account_id: "ACC-1".into(),
                trade_date_from: "2026-09-09".into(),
                trade_date_to: "2026-09-30".into(),
                page_size: 2,
                cursor: first.next_cursor,
                ..Default::default()
            },
            &Scope::Everything,
        )
        .unwrap();
        assert_eq!(second.activities.len(), 1);
        assert_eq!(
            second.activities[0].activity.as_ref().unwrap().trade_date,
            "2026-09-30"
        );
        assert!(second.next_cursor.is_empty());

        let narrowed = list_activities(
            &store,
            &ListActivitiesRequest {
                trade_date_from: "2026-09-10".into(),
                trade_date_to: "2026-09-15".into(),
                ..Default::default()
            },
            &Scope::Everything,
        )
        .unwrap();
        assert_eq!(narrowed.activities.len(), 1);
    }

    #[test]
    fn activity_since_a_watermark_is_read_in_the_order_recorded() {
        let store = MemoryStore::new();
        for (id, date) in [
            ("c", "2026-09-30"),
            ("a", "2026-09-09"),
            ("b", "2026-09-15"),
        ] {
            record_activity(&store, &reinvestment("ACC-1", id, date), &by("c", NOW)).unwrap();
        }
        let since = list_activities(
            &store,
            &ListActivitiesRequest {
                since: Some(watermark(1)),
                ..Default::default()
            },
            &Scope::Everything,
        )
        .unwrap();
        let ids: Vec<_> = since
            .activities
            .iter()
            .map(|a| a.activity.as_ref().unwrap().external_activity_id.clone())
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn a_plugins_read_of_activity_is_answered_within_its_scope() {
        let store = MemoryStore::new();
        record_activity(
            &store,
            &reinvestment("ACC-1", "1", "2026-09-01"),
            &by("c", NOW),
        )
        .unwrap();
        record_activity(
            &store,
            &reinvestment("ACC-9", "2", "2026-09-01"),
            &by("c", NOW),
        )
        .unwrap();
        let scoped = list_activities(
            &store,
            &ListActivitiesRequest::default(),
            &within(&["ACC-1"]),
        )
        .unwrap();
        assert_eq!(scoped.activities.len(), 1);
        assert_eq!(scoped.activities[0].account_id, "ACC-1");
        let outside = ListActivitiesRequest {
            account_id: "ACC-9".into(),
            ..Default::default()
        };
        assert!(matches!(
            list_activities(&store, &outside, &within(&["ACC-1"])),
            Err(StoreError::OutOfScope(_))
        ));
        let empty =
            list_activities(&store, &ListActivitiesRequest::default(), &within(&[])).unwrap();
        assert!(empty.activities.is_empty());
    }

    #[test]
    fn history_from_is_the_named_accounts_latest_sync_status() {
        let store = MemoryStore::new();
        record_activity(
            &store,
            &reinvestment("ACC-1", "1", "2026-09-01"),
            &by("c", NOW),
        )
        .unwrap();
        let named = ListActivitiesRequest {
            account_id: "ACC-1".into(),
            ..Default::default()
        };
        // None heard: empty, which is not saying the history is complete.
        assert_eq!(
            list_activities(&store, &named, &Scope::Everything)
                .unwrap()
                .history_from,
            ""
        );
        record_sync_status(
            &store,
            &sync("ACC-1", SyncState::Current, "2024-09-08"),
            &by("c", NOW),
        )
        .unwrap();
        record_sync_status(
            &store,
            &sync("ACC-2", SyncState::Current, "2020-01-01"),
            &by("c", NOW),
        )
        .unwrap();
        record_sync_status(
            &store,
            &sync("ACC-1", SyncState::Current, "2024-10-04"),
            &by("c", NOW),
        )
        .unwrap();
        assert_eq!(
            list_activities(&store, &named, &Scope::Everything)
                .unwrap()
                .history_from,
            "2024-10-04"
        );
        // One account's source's, so empty where none is named.
        assert_eq!(
            list_activities(
                &store,
                &ListActivitiesRequest::default(),
                &Scope::Everything
            )
            .unwrap()
            .history_from,
            ""
        );
    }

    #[test]
    fn every_sync_status_heard_is_a_record_chained_per_account() {
        let store = MemoryStore::new();
        let first = record_sync_status(
            &store,
            &sync("ACC-1", SyncState::NeedsSignIn, "2024-10-04"),
            &by("custody-1", NOW),
        )
        .unwrap();
        assert_eq!(
            first.status.as_ref().unwrap().state,
            SyncState::NeedsSignIn as i32
        );
        assert_eq!(first.cause.as_ref().unwrap().instance_id, "custody-1");
        let again = record_sync_status(
            &store,
            &sync("ACC-1", SyncState::NeedsSignIn, "2024-10-04"),
            &by("custody-1", NOW + 1),
        )
        .unwrap();
        let journal = again.journal.unwrap();
        assert_eq!((journal.sequence, journal.previous_sequence), (2, 1));
    }

    #[test]
    fn a_connections_first_sync_status_records_that_nothing_is_known_before_it() {
        // decisions/031, point 4: once per connection, at the first the
        // street heard, and never on the wire to a plugin.
        let store = MemoryStore::new();
        let first = record_sync_status(
            &store,
            &sync("ACC-1", SyncState::Current, ""),
            &by("c", NOW),
        )
        .unwrap();
        record_sync_status(
            &store,
            &sync("ACC-1", SyncState::NeedsSignIn, ""),
            &by("c", NOW + 1),
        )
        .unwrap();
        record_sync_status(
            &store,
            &sync("ACC-2", SyncState::Stale, ""),
            &by("c", NOW + 2),
        )
        .unwrap();
        let mut unlinked = sync("", SyncState::Current, "");
        unlinked.external_account_id = "SNAP-ACC-9".into();
        record_sync_status(&store, &unlinked, &by("c", NOW + 3)).unwrap();
        record_sync_status(&store, &unlinked, &by("c", NOW + 4)).unwrap();

        let every = store
            .sync_statuses(&SyncStatusesRead {
                scope: Scope::Everything,
                account_id: String::new(),
                limit: 10,
                cursor: String::new(),
                since: Some(0),
            })
            .unwrap();
        let gaps: Vec<(&str, bool, i64)> = every
            .statuses
            .iter()
            .map(|s| {
                (
                    s.account_id.as_str(),
                    !s.not_known_before.is_empty(),
                    s.recorded.cause.committed_at_ns,
                )
            })
            .collect();
        assert_eq!(
            gaps,
            vec![
                ("ACC-1", true, NOW),
                ("ACC-1", false, NOW + 1),
                ("ACC-2", true, NOW + 2),
                ("", true, NOW + 3),
                ("", false, NOW + 4),
            ]
        );
        assert_eq!(
            every.statuses[0].not_known_before,
            crate::SYNC_STATUS_NOT_KNOWN_BEFORE
        );
        // The record announced is the sync status as published, nothing more.
        assert_eq!(first.status.unwrap(), sync("ACC-1", SyncState::Current, ""));
    }

    #[test]
    fn the_latest_sync_status_of_each_connection_is_read_or_every_one_since() {
        let store = MemoryStore::new();
        record_sync_status(
            &store,
            &sync("ACC-1", SyncState::Current, ""),
            &by("c", NOW),
        )
        .unwrap();
        record_sync_status(&store, &sync("ACC-2", SyncState::Stale, ""), &by("c", NOW)).unwrap();
        record_sync_status(
            &store,
            &sync("ACC-1", SyncState::NeedsSignIn, ""),
            &by("c", NOW),
        )
        .unwrap();
        // An unlinked external account's, kept with no account.
        let mut unlinked = sync("", SyncState::Disabled, "");
        unlinked.external_account_id = "SNAP-ROTH".into();
        record_sync_status(&store, &unlinked, &by("c", NOW)).unwrap();

        let latest = list_sync_statuses(
            &store,
            &ListSyncStatusesRequest::default(),
            &within(&["ACC-1", "ACC-2"]),
        )
        .unwrap();
        let states: Vec<_> = latest
            .statuses
            .iter()
            .map(|s| {
                let status = s.status.as_ref().unwrap();
                (status.account_id.clone(), status.state)
            })
            .collect();
        assert_eq!(
            states,
            [
                ("ACC-1".to_string(), SyncState::NeedsSignIn as i32),
                ("ACC-2".to_string(), SyncState::Stale as i32)
            ]
        );

        // The dashboard reads every connection, the unlinked one too.
        let everything = list_sync_statuses(
            &store,
            &ListSyncStatusesRequest::default(),
            &Scope::Everything,
        )
        .unwrap();
        assert_eq!(everything.statuses.len(), 3);

        // Since a watermark: every one after it, in the order recorded.
        let since = list_sync_statuses(
            &store,
            &ListSyncStatusesRequest {
                since: Some(watermark(1)),
                ..Default::default()
            },
            &within(&["ACC-1", "ACC-2"]),
        )
        .unwrap();
        let sequences: Vec<_> = since
            .statuses
            .iter()
            .map(|s| s.journal.as_ref().unwrap().sequence)
            .collect();
        assert_eq!(sequences, [2, 3]);

        // Paged by connection.
        let first = list_sync_statuses(
            &store,
            &ListSyncStatusesRequest {
                page_size: 1,
                ..Default::default()
            },
            &Scope::Everything,
        )
        .unwrap();
        assert_eq!(first.statuses.len(), 1);
        let rest = list_sync_statuses(
            &store,
            &ListSyncStatusesRequest {
                page_size: 5,
                cursor: first.next_cursor,
                ..Default::default()
            },
            &Scope::Everything,
        )
        .unwrap();
        assert_eq!(rest.statuses.len(), 2);

        let outside = ListSyncStatusesRequest {
            account_id: "ACC-9".into(),
            ..Default::default()
        };
        assert!(list_sync_statuses(&store, &outside, &within(&["ACC-1"])).is_err());
    }

    #[test]
    fn recording_activity_moves_no_position() {
        // Requirement 8: nothing is derived from it.
        let store = MemoryStore::new();
        record_activity(
            &store,
            &reinvestment("ACC-1", "1", "2026-09-01"),
            &by("c", NOW),
        )
        .unwrap();
        assert!(store
            .custodial_position("ACC-1", "INS-SPAXX", crate::store::Side::Long)
            .unwrap()
            .is_none());
        assert!(store.instruments_held().unwrap().is_empty());
    }

    // ── An activity re-resolved (contract v15: W2.15, W2.16) ──

    /// A 401(k)'s reinvestment, its plan code OQKR not yet linked.
    fn unresolved(account: &str, id: &str) -> RecordActivityRequest {
        let mut request = reinvestment(account, id, "2026-09-30");
        let activity = request.activity.as_mut().unwrap();
        activity.instrument_id = String::new();
        activity.instrument_as_reported = Some(meridian_pb::v1::AsReported {
            scheme: "snaptrade:plan-code".into(),
            code: "OQKR".into(),
            text: "OQKR".into(),
        });
        request
    }

    fn linked(person: &str) -> meridian_pb::v1::Provenance {
        meridian_pb::v1::Provenance {
            field: "instrument_id".into(),
            kind: meridian_pb::v1::ProvenanceKind::Supplied as i32,
            person: person.into(),
            ..Default::default()
        }
    }

    fn re_resolution(account: &str, id: &str, instrument: &str) -> ReResolveActivityRequest {
        ReResolveActivityRequest {
            account_id: account.into(),
            external_account_id: format!("SNAP-{account}"),
            source: "snaptrade".into(),
            external_activity_id: id.into(),
            instrument_id: instrument.into(),
            provenance: Some(linked("Ada Park, in the plan-code links")),
            resolved_at_ns: NOW - 60,
        }
    }

    #[test]
    fn a_re_resolution_is_kept_beside_the_activity_which_never_changes() {
        let store = MemoryStore::new();
        let first = record_activity(&store, &unresolved("ACC-1", "oqkr-1"), &by("c", NOW))
            .unwrap()
            .event
            .unwrap();
        let done = re_resolve_activity(
            &store,
            &re_resolution("ACC-1", "oqkr-1", "INS-VIGIX"),
            &by("custody-1", NOW + 5),
        )
        .unwrap();
        assert_eq!(done.reply.activity_id, first.activity_id);
        assert!(!done.reply.already_recorded);
        let event = done
            .event
            .expect("a re-resolution recorded now is announced");
        assert_eq!(event.account_id, "ACC-1");
        let re = event.re_resolution.clone().unwrap();
        assert_eq!(
            (re.activity_id.as_str(), re.account_id.as_str()),
            (first.activity_id.as_str(), "ACC-1")
        );
        assert_eq!(re.instrument_id, "INS-VIGIX");
        assert_eq!(
            re.provenance,
            Some(linked("Ada Park, in the plan-code links"))
        );
        assert_eq!((re.resolved_at_ns, re.recorded_at_ns), (NOW - 60, NOW + 5));
        // Its own number, at the top as in the record, chained apart.
        assert_eq!(event.journal, re.journal);
        let journal = re.journal.unwrap();
        assert_eq!((journal.sequence, journal.previous_sequence), (2, 0));
        assert_eq!(event.cause.unwrap().instance_id, "custody-1");

        let read = list_activities(
            &store,
            &ListActivitiesRequest::default(),
            &Scope::Everything,
        )
        .unwrap();
        assert_eq!(
            read.activities,
            vec![first],
            "the activity as first recorded"
        );
        assert_eq!(read.re_resolutions, vec![event.re_resolution.unwrap()]);
    }

    #[test]
    fn a_re_resolution_naming_the_latest_is_already_recorded_and_takes_no_number() {
        let store = MemoryStore::new();
        record_activity(&store, &unresolved("ACC-1", "oqkr-1"), &by("c", NOW)).unwrap();
        let request = re_resolution("ACC-1", "oqkr-1", "INS-VIGIX");
        re_resolve_activity(&store, &request, &by("c", NOW + 1)).unwrap();
        let again = re_resolve_activity(&store, &request, &by("c", NOW + 2)).unwrap();
        assert!(again.reply.already_recorded && again.event.is_none());
        assert!(!again.reply.activity_id.is_empty());

        // Changed, then removed: each its own record, chained per account.
        let changed = re_resolve_activity(
            &store,
            &re_resolution("ACC-1", "oqkr-1", "INS-VFIAX"),
            &by("c", NOW + 3),
        )
        .unwrap()
        .event
        .unwrap();
        let journal = changed.journal.unwrap();
        assert_eq!((journal.sequence, journal.previous_sequence), (3, 2));
        let removed = re_resolve_activity(
            &store,
            &re_resolution("ACC-1", "oqkr-1", ""),
            &by("c", NOW + 4),
        )
        .unwrap()
        .event
        .expect("unresolved again is a re-resolution");
        assert_eq!(removed.re_resolution.unwrap().instrument_id, "");
        let read = list_activities(
            &store,
            &ListActivitiesRequest::default(),
            &Scope::Everything,
        )
        .unwrap();
        let instruments: Vec<_> = read
            .re_resolutions
            .iter()
            .map(|re| re.instrument_id.as_str())
            .collect();
        assert_eq!(instruments, ["INS-VIGIX", "INS-VFIAX", ""]);
    }

    #[test]
    fn a_re_resolution_naming_what_the_activity_first_named_is_already_recorded() {
        let store = MemoryStore::new();
        let mut request = reinvestment("ACC-1", "linked-1", "2026-09-30");
        request
            .activity
            .as_mut()
            .unwrap()
            .provenance
            .push(linked("Ada Park, in the plan-code links"));
        record_activity(&store, &request, &by("c", NOW)).unwrap();
        let same = re_resolve_activity(
            &store,
            &re_resolution("ACC-1", "linked-1", "INS-SPAXX"),
            &by("c", NOW + 1),
        )
        .unwrap();
        assert!(same.reply.already_recorded && same.event.is_none());
    }

    #[test]
    fn re_resolutions_are_chained_apart_from_the_activities() {
        let store = MemoryStore::new();
        record_activity(&store, &unresolved("ACC-1", "1"), &by("c", NOW)).unwrap();
        re_resolve_activity(
            &store,
            &re_resolution("ACC-1", "1", "INS-VIGIX"),
            &by("c", NOW),
        )
        .unwrap();
        let second = record_activity(&store, &unresolved("ACC-1", "2"), &by("c", NOW))
            .unwrap()
            .event
            .unwrap()
            .journal
            .unwrap();
        assert_eq!(
            (second.sequence, second.previous_sequence),
            (3, 1),
            "the activities' chain sees no gap from a re-resolution"
        );
    }

    #[test]
    fn a_re_resolution_naming_no_recorded_activity_or_saying_not_how_is_refused() {
        let store = MemoryStore::new();
        record_activity(&store, &unresolved("ACC-1", "1"), &by("c", NOW)).unwrap();
        let none = re_resolve_activity(
            &store,
            &re_resolution("ACC-1", "never-sent", "INS-VIGIX"),
            &by("c", NOW),
        )
        .unwrap_err();
        assert!(matches!(none, StoreError::NoSuchActivity(_)));
        assert!(none
            .to_string()
            .contains("never-sent from snaptrade on ACC-1"));
        let elsewhere = re_resolution("ACC-2", "1", "INS-VIGIX");
        assert!(matches!(
            re_resolve_activity(&store, &elsewhere, &by("c", NOW)),
            Err(StoreError::NoSuchActivity(_))
        ));

        let mut unsaid = re_resolution("ACC-1", "1", "INS-VIGIX");
        unsaid.provenance = None;
        let mut reported = re_resolution("ACC-1", "1", "INS-VIGIX");
        reported.provenance.as_mut().unwrap().kind =
            meridian_pb::v1::ProvenanceKind::Reported as i32;
        let mut undated = re_resolution("ACC-1", "1", "INS-VIGIX");
        undated.resolved_at_ns = 0;
        let unlinked = re_resolution("", "1", "INS-VIGIX");
        for refused in [unsaid, reported, undated, unlinked] {
            assert!(matches!(
                re_resolve_activity(&store, &refused, &by("c", NOW)),
                Err(StoreError::Edge(_))
            ));
        }
        let read = list_activities(
            &store,
            &ListActivitiesRequest::default(),
            &Scope::Everything,
        )
        .unwrap();
        assert!(read.re_resolutions.is_empty(), "nothing refused is kept");
    }

    #[test]
    fn since_a_watermark_re_resolutions_are_read_with_the_activities_once_each() {
        let store = MemoryStore::new();
        // 1 activity, 2 re-resolution, 3 activity, 4 re-resolution of the
        // first, on another account a 5 activity and a 6 re-resolution.
        record_activity(&store, &unresolved("ACC-1", "1"), &by("c", NOW)).unwrap();
        re_resolve_activity(
            &store,
            &re_resolution("ACC-1", "1", "INS-VIGIX"),
            &by("c", NOW),
        )
        .unwrap();
        record_activity(&store, &unresolved("ACC-1", "2"), &by("c", NOW)).unwrap();
        re_resolve_activity(
            &store,
            &re_resolution("ACC-1", "1", "INS-VFIAX"),
            &by("c", NOW),
        )
        .unwrap();
        record_activity(&store, &unresolved("ACC-9", "9"), &by("c", NOW)).unwrap();
        re_resolve_activity(
            &store,
            &re_resolution("ACC-9", "9", "INS-VIGIX"),
            &by("c", NOW),
        )
        .unwrap();

        let mut seen = Vec::new();
        let mut cursor = String::new();
        loop {
            let page = list_activities(
                &store,
                &ListActivitiesRequest {
                    since: Some(watermark(1)),
                    page_size: 2,
                    cursor: cursor.clone(),
                    ..Default::default()
                },
                &within(&["ACC-1"]),
            )
            .unwrap();
            assert!(page.activities.len() + page.re_resolutions.len() <= 2);
            seen.extend(
                page.activities
                    .iter()
                    .map(|a| a.journal.as_ref().unwrap().sequence),
            );
            seen.extend(
                page.re_resolutions
                    .iter()
                    .map(|re| re.journal.as_ref().unwrap().sequence),
            );
            if page.next_cursor.is_empty() {
                break;
            }
            cursor = page.next_cursor;
        }
        seen.sort_unstable();
        assert_eq!(
            seen,
            [2, 3, 4],
            "each once, within the scope, after the watermark"
        );

        // By trade date: every re-resolution of the activities answered.
        let dated = list_activities(
            &store,
            &ListActivitiesRequest {
                account_id: "ACC-1".into(),
                page_size: 1,
                ..Default::default()
            },
            &Scope::Everything,
        )
        .unwrap();
        assert_eq!(dated.activities.len(), 1);
        assert_eq!(dated.re_resolutions.len(), 2);
        assert!(dated
            .re_resolutions
            .iter()
            .all(|re| re.activity_id == dated.activities[0].activity_id));
    }
}
