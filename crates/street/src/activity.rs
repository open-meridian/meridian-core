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
//! # Nothing is derived
//!
//! Activity is evidence, never a source (the spec's requirement 8): nothing
//! here derives a position, a lot or a cash figure from it, and nothing in the
//! rest of the street reads it. The record is kept as the plugin encoded it;
//! the store takes out only what a read selects and orders by.

use meridian_domain::v1::{
    ActivityRecordedEvent, CustodialActivity, ListActivitiesReply, ListActivitiesRequest,
    ListSyncStatusesReply, ListSyncStatusesRequest, RecordActivityReply, RecordActivityRequest,
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
    ActivitiesRead, Activity, Cause, Completed, Kept, Result, Scope, Store, StoreError, SyncStatus,
    SyncStatusesRead,
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
}
