//! What the custody connectors say about their connections, as last heard.
//!
//! Three things, each heard on the bus and none asked for: the accounts a
//! connection reaches (W2.8), the accounts refused for want of a link (W4.8),
//! and why a connection's data is or is not current (W2.1). The dashboard
//! counts the accounts nothing links on each plugin's health, leading to the
//! plugin's own admin pages where it links them (W6.4, W6.10), and shows each
//! connection's state on the plugin's overview, so a deployment admin knows
//! whose fix a connection that is not current is.
//!
//! Held in memory and nowhere else. Every one of them is said again by whoever
//! said it -- a connector reports its accounts and its sync state on each read,
//! and a sidecar its refusals every 30 seconds -- so a restart forgets them
//! only until then, and a store of them would be a second copy of something the
//! connector already owns.
//!
//! Whether an account is linked is not heard here: it is the conductor's, and
//! arrives with the records the dashboard reads (`AccessRecords::links`), so a
//! link made a moment ago takes an account off the unlinked list at once.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use meridian_bus::Bus;
use meridian_domain::v1::{
    ExternalAccount, ExternalAccountLink, ExternalAccountsEvent, SyncState, SyncStatusEvent,
    UnlinkedExternalAccountsEvent,
};
use prost::Message;

/// W2.8, from every connector at once.
pub const EXTERNAL_ACCOUNTS: &str = "platform.custody.*.event.external-accounts";

/// W2.1, from every connector at once.
pub const SYNC_STATUS: &str = "platform.custody.*.event.sync-status";

/// W4.8: what a sidecar refused because nobody had linked the account.
pub const UNLINKED_EXTERNAL_ACCOUNTS: &str = "platform.config.event.unlinked-external-accounts";

/// Everything heard, keyed by the plugin instance that said it.
#[derive(Debug, Clone, Default)]
pub struct Heard {
    /// Each instance's whole list, as its latest report gave it: an account
    /// missing from a later one is one the connection no longer reaches.
    pub reported: BTreeMap<String, Vec<ExternalAccount>>,

    /// Each instance's refusals, by external account: rows refused and when.
    pub refused: BTreeMap<String, BTreeMap<String, i64>>,

    /// The latest sync status of each instance's external account.
    pub sync: BTreeMap<(String, String), SyncStatusEvent>,
}

/// One external account with no link, and what is known of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unlinked {
    pub plugin_instance_id: String,
    pub external_account_id: String,

    /// The custodian's name for it and the venue's type, where the connector
    /// reported the account; empty for one known only from a refusal.
    pub name: String,
    pub venue_account_type: String,

    /// Rows the sidecar refused for it, where any were.
    pub refused_rows: i64,
}

#[derive(Debug, Default)]
pub struct Custody {
    heard: Mutex<Heard>,
}

impl Custody {
    fn heard(&self) -> std::sync::MutexGuard<'_, Heard> {
        // A poisoned lock here means a panic mid-insert into a map of display
        // data; what it holds is still worth showing.
        self.heard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn hear_accounts(&self, instance: &str, event: ExternalAccountsEvent) {
        self.heard()
            .reported
            .insert(instance.to_string(), event.accounts);
    }

    pub fn hear_refused(&self, event: UnlinkedExternalAccountsEvent) {
        self.heard().refused.insert(
            event.plugin_instance_id,
            event
                .accounts
                .into_iter()
                .map(|account| (account.external_account_id, account.refused_rows))
                .collect(),
        );
    }

    pub fn hear_sync(&self, instance: &str, event: SyncStatusEvent) {
        self.heard().sync.insert(
            (instance.to_string(), event.external_account_id.clone()),
            event,
        );
    }

    pub fn view(&self) -> Heard {
        self.heard().clone()
    }
}

impl Heard {
    /// Every account reported or refused that no link names, reported ones
    /// first in the order their connector gave them. One reported and refused
    /// both is one account, carrying its name and its refusals.
    pub fn unlinked(&self, links: &[ExternalAccountLink]) -> Vec<Unlinked> {
        let linked = |instance: &str, external: &str| {
            links.iter().any(|link| {
                link.plugin_instance_id == instance
                    && link.external_account_id == external
                    && !link.account_id.is_empty()
            })
        };
        let refused_rows = |instance: &str, external: &str| {
            self.refused
                .get(instance)
                .and_then(|accounts| accounts.get(external))
                .copied()
                .unwrap_or_default()
        };

        let mut found = Vec::new();
        for (instance, accounts) in &self.reported {
            for account in accounts {
                if !linked(instance, &account.external_account_id) {
                    found.push(Unlinked {
                        plugin_instance_id: instance.clone(),
                        external_account_id: account.external_account_id.clone(),
                        name: account.name.clone(),
                        venue_account_type: account.venue_account_type.clone(),
                        refused_rows: refused_rows(instance, &account.external_account_id),
                    });
                }
            }
        }
        for (instance, accounts) in &self.refused {
            for (external, rows) in accounts {
                let reported = self.reported.get(instance).is_some_and(|reported| {
                    reported
                        .iter()
                        .any(|account| &account.external_account_id == external)
                });
                if !reported && !linked(instance, external) {
                    found.push(Unlinked {
                        plugin_instance_id: instance.clone(),
                        external_account_id: external.clone(),
                        name: String::new(),
                        venue_account_type: String::new(),
                        refused_rows: *rows,
                    });
                }
            }
        }
        found
    }
}

/// Subscribe to all three and keep what arrives. Subscribed before this
/// returns, for the reason at-most-once delivery makes unforgiving: what
/// arrives before a subscriber exists is dropped.
pub fn listen(bus: &Bus, custody: Arc<Custody>) {
    let mut accounts = bus.subscribe(EXTERNAL_ACCOUNTS);
    let keeping = Arc::clone(&custody);
    tokio::spawn(async move {
        while let Some(delivery) = accounts.recv().await {
            let Some(instance) = instance_of(&delivery.envelope) else {
                continue;
            };
            match ExternalAccountsEvent::decode(&delivery.envelope.payload[..]) {
                Ok(event) => keeping.hear_accounts(&instance, event),
                Err(failed) => {
                    tracing::warn!(instance, "an account report did not decode: {failed}")
                }
            }
        }
    });

    let mut statuses = bus.subscribe(SYNC_STATUS);
    let keeping = Arc::clone(&custody);
    tokio::spawn(async move {
        while let Some(delivery) = statuses.recv().await {
            let Some(instance) = instance_of(&delivery.envelope) else {
                continue;
            };
            match SyncStatusEvent::decode(&delivery.envelope.payload[..]) {
                Ok(event) => keeping.hear_sync(&instance, event),
                Err(failed) => tracing::warn!(instance, "a sync status did not decode: {failed}"),
            }
        }
    });

    let mut refusals = bus.subscribe(UNLINKED_EXTERNAL_ACCOUNTS);
    tokio::spawn(async move {
        while let Some(delivery) = refusals.recv().await {
            match UnlinkedExternalAccountsEvent::decode(&delivery.envelope.payload[..]) {
                Ok(event) => custody.hear_refused(event),
                Err(failed) => {
                    tracing::warn!("an unlinked-accounts report did not decode: {failed}")
                }
            }
        }
    });
}

/// The plugin instance a custody event came from: the topic's own segment,
/// `platform.custody.{instance}.event...`, which the sidecar fills with its
/// plugin's instance and no other. It is also the instance a link names.
fn instance_of(envelope: &meridian_domain::v1::Envelope) -> Option<String> {
    let topic = envelope.meta.as_ref().map(|meta| meta.topic.as_str())?;
    let mut parts = topic.split('.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("platform"), Some("custody"), Some(instance)) if !instance.is_empty() => {
            Some(instance.to_string())
        }
        _ => None,
    }
}

/// What a sync status means for a person reading it: a word for the state,
/// and what to do about it, which is the point of saying why.
pub fn remedy(status: &SyncStatusEvent) -> (&'static str, &'static str) {
    match SyncState::try_from(status.state).unwrap_or(SyncState::Unspecified) {
        SyncState::Current => ("Current", "Nothing to do."),
        SyncState::Stale => (
            "Stale",
            "Wait: the connection is serving what it last read, and catches up on its own.",
        ),
        SyncState::NeedsSignIn => (
            "Needs sign-in",
            "Sign in again at the venue: nothing new is read until somebody does.",
        ),
        SyncState::Disabled => (
            "Disabled",
            "Re-enable the connection: it serves only what it last read until it is.",
        ),
        SyncState::DelayedByDesign => (
            "Delayed by design",
            "Expected: this venue reports late, and nothing is wrong.",
        ),
        SyncState::HoldingsUnavailable => (
            "Holdings unavailable",
            "Connect the account another way, or through another venue: holdings will not \
             arrive through this connection, however long it waits.",
        ),
        SyncState::Unspecified if status.connection_healthy => {
            ("Healthy", "The connector says nothing more.")
        }
        SyncState::Unspecified => (
            "Not healthy",
            "The connector does not say why; its detail may.",
        ),
    }
}

/// Whether a sync status asks nothing of anybody: current, late on purpose,
/// or healthy by the only measure a connector gave.
pub fn quiet(status: &SyncStatusEvent) -> bool {
    match SyncState::try_from(status.state).unwrap_or(SyncState::Unspecified) {
        SyncState::Current | SyncState::DelayedByDesign => true,
        SyncState::Unspecified => status.connection_healthy,
        _ => false,
    }
}

/// A time as a person reads it, in UTC, to the minute; or that it was not
/// said. Integer arithmetic on the days since 1970 (Howard Hinnant's
/// civil-from-days), because this is the one place the dashboard shows a
/// time and a date library for it would be a dependency to keep current.
pub fn utc(ns: i64) -> String {
    if ns <= 0 {
        return "not said".to_string();
    }
    let seconds = ns.div_euclid(1_000_000_000);
    let days = seconds.div_euclid(86_400);
    let minute_of_day = seconds.rem_euclid(86_400) / 60;

    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        minute_of_day / 60,
        minute_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use meridian_bus::MemoryBackend;
    use meridian_domain::v1::UnlinkedExternalAccount;

    use super::*;

    fn account(id: &str, name: &str, kind: &str) -> ExternalAccount {
        ExternalAccount {
            external_account_id: id.into(),
            name: name.into(),
            venue_account_type: kind.into(),
        }
    }

    fn link(instance: &str, external: &str, account: &str) -> ExternalAccountLink {
        ExternalAccountLink {
            plugin_instance_id: instance.into(),
            external_account_id: external.into(),
            account_id: account.into(),
        }
    }

    #[test]
    fn a_reported_account_with_no_link_is_unlinked_and_one_with_a_link_is_not() {
        let custody = Custody::default();
        custody.hear_accounts(
            "snaptrade-1",
            ExternalAccountsEvent {
                accounts: vec![
                    account("SNAP-1", "Individual Brokerage 1234", "Individual"),
                    account("SNAP-2", "Roth IRA 5678", "Roth IRA"),
                ],
            },
        );

        let unlinked = custody
            .view()
            .unlinked(&[link("snaptrade-1", "SNAP-1", "ACC-1")]);
        assert_eq!(
            unlinked,
            [Unlinked {
                plugin_instance_id: "snaptrade-1".into(),
                external_account_id: "SNAP-2".into(),
                name: "Roth IRA 5678".into(),
                venue_account_type: "Roth IRA".into(),
                refused_rows: 0,
            }]
        );

        // The same external identifier under another plugin is another
        // account, and so is a link that was removed.
        let unlinked = custody.view().unlinked(&[
            link("snaptrade-2", "SNAP-1", "ACC-1"),
            link("snaptrade-1", "SNAP-2", ""),
        ]);
        assert_eq!(unlinked.len(), 2);
    }

    #[test]
    fn a_later_report_replaces_the_list_rather_than_adding_to_it() {
        let custody = Custody::default();
        let reported = |ids: &[&str]| ExternalAccountsEvent {
            accounts: ids.iter().map(|id| account(id, id, "")).collect(),
        };
        custody.hear_accounts("snaptrade-1", reported(&["SNAP-1", "SNAP-2"]));
        custody.hear_accounts("snaptrade-1", reported(&["SNAP-2"]));

        let unlinked = custody.view().unlinked(&[]);
        assert_eq!(unlinked.len(), 1, "no longer reached is no longer listed");
        assert_eq!(unlinked[0].external_account_id, "SNAP-2");
    }

    #[test]
    fn a_refused_account_is_one_account_with_its_report_and_listed_even_without_one() {
        let custody = Custody::default();
        custody.hear_accounts(
            "snaptrade-1",
            ExternalAccountsEvent {
                accounts: vec![account("SNAP-1", "Individual Brokerage 1234", "Individual")],
            },
        );
        custody.hear_refused(UnlinkedExternalAccountsEvent {
            plugin_instance_id: "snaptrade-1".into(),
            accounts: vec![
                UnlinkedExternalAccount {
                    external_account_id: "SNAP-1".into(),
                    refused_rows: 12,
                    ..Default::default()
                },
                UnlinkedExternalAccount {
                    external_account_id: "st-acct-9902".into(),
                    refused_rows: 3,
                    ..Default::default()
                },
            ],
        });

        let unlinked = custody.view().unlinked(&[]);
        assert_eq!(unlinked.len(), 2);
        assert_eq!(unlinked[0].name, "Individual Brokerage 1234");
        assert_eq!(unlinked[0].refused_rows, 12);
        assert_eq!(unlinked[1].external_account_id, "st-acct-9902");
        assert_eq!(unlinked[1].refused_rows, 3);
        assert!(unlinked[1].name.is_empty());

        // Linked, and gone from the list, whatever the sidecar last said.
        assert!(custody
            .view()
            .unlinked(&[
                link("snaptrade-1", "SNAP-1", "ACC-1"),
                link("snaptrade-1", "st-acct-9902", "ACC-2")
            ])
            .is_empty());
    }

    #[test]
    fn every_state_says_what_to_do_about_it() {
        let said = |state: SyncState, healthy: bool| {
            remedy(&SyncStatusEvent {
                state: state as i32,
                connection_healthy: healthy,
                ..Default::default()
            })
        };
        assert_eq!(said(SyncState::Current, true).0, "Current");
        assert!(said(SyncState::Stale, true).1.starts_with("Wait"));
        assert!(said(SyncState::NeedsSignIn, false)
            .1
            .starts_with("Sign in again at the venue"));
        assert!(said(SyncState::Disabled, false)
            .1
            .starts_with("Re-enable the connection"));
        assert!(said(SyncState::DelayedByDesign, true)
            .1
            .starts_with("Expected"));
        // Not "wait": waiting changes nothing when the venue withholds them.
        let unavailable = said(SyncState::HoldingsUnavailable, true);
        assert_eq!(unavailable.0, "Holdings unavailable");
        assert!(unavailable.1.starts_with("Connect the account another way"));
        assert!(!unavailable.1.contains("Wait"));
        assert_eq!(said(SyncState::Unspecified, true).0, "Healthy");
        assert_eq!(said(SyncState::Unspecified, false).0, "Not healthy");
    }

    #[test]
    fn a_time_reads_as_a_utc_date_and_minute() {
        assert_eq!(utc(0), "not said");
        assert_eq!(utc(1_757_289_600_000_000_000), "2025-09-08 00:00 UTC");
        assert_eq!(utc(1_790_380_800_000_000_000), "2026-09-26 00:00 UTC");
        assert_eq!(utc(951_827_400_000_000_000), "2000-02-29 12:30 UTC");
    }

    #[tokio::test]
    async fn what_the_connectors_publish_is_heard_under_the_instance_that_said_it() {
        let bus = Bus::single("dashboard-1", Arc::new(MemoryBackend::new()));
        let custody = Arc::new(Custody::default());
        listen(&bus, Arc::clone(&custody));

        bus.publish(
            "platform.custody.snaptrade-1.event.external-accounts",
            "meridian.v1.ExternalAccountsEvent",
            ExternalAccountsEvent {
                accounts: vec![account("SNAP-1", "Brokerage", "MARGIN")],
            }
            .encode_to_vec(),
            None,
            None,
        )
        .unwrap();
        bus.publish(
            "platform.custody.snaptrade-1.event.sync-status",
            "meridian.v1.SyncStatusEvent",
            SyncStatusEvent {
                external_account_id: "SNAP-1".into(),
                account_id: "ACC-1".into(),
                state: SyncState::Disabled as i32,
                ..Default::default()
            }
            .encode_to_vec(),
            None,
            None,
        )
        .unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let heard = custody.view();
            if !heard.reported.is_empty() && !heard.sync.is_empty() {
                assert_eq!(
                    heard.reported["snaptrade-1"][0].external_account_id,
                    "SNAP-1"
                );
                let status = &heard.sync[&("snaptrade-1".to_string(), "SNAP-1".to_string())];
                assert_eq!(status.state, SyncState::Disabled as i32);
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "nothing was heard");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
