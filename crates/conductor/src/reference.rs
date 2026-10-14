//! What the conductor pulls from the platform for the instrument store, and
//! the licence list it sends by (contract v24; W3.24, W3.25, W3.27).
//!
//! On start, daily, and -- the platform not reached -- every few minutes
//! until it is, which is how a reconnect is noticed: the licence list, the
//! template catalogue's versions after the highest seen, and the
//! replacement feed after the last sequence read. Each is handed to the
//! store on the bus as it came; the conductor writes into no other
//! component's store (decisions/011). A platform without v24's routes is
//! unreachable ([`crate::platform::Platform::pull_licence_list`] and the
//! rest), never an answer: nothing is published, and it is tried again.
//!
//! **What may be sent** (W3's invariant, W3.25): identifiers in schemes on
//! the list read last -- the three FIGI levels one family, a ticker with its
//! venue `ticker_and_venue` -- and never the deployment's own: no `LCL-` ID
//! and no `open_meridian_local`. Until a list is read, the open schemes the
//! contract names.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::v1::{
    Identifier as PbIdentifier, LicenceList, LicencePace, PullReplacementsReply, PullTemplatesReply,
};
use prost::Message;

use crate::platform::{Platform, PlatformError};

/// W3.24, W3.25, W3.27: what was pulled, for the instrument store.
pub const TEMPLATES_PULLED: &str = "platform.reference.event.templates-pulled";
pub const LICENCE_LIST_PULLED: &str = "platform.reference.event.licence-list-pulled";
pub const REPLACEMENTS_PULLED: &str = "platform.reference.event.replacements-pulled";

/// The deployment-only scheme a resolved `LCL-` ID is kept under; never sent.
pub const LOCAL_SCHEME: &str = "open_meridian_local";

/// The open schemes sendable before any list is read.
pub const OPEN_FAMILIES: [&str; 7] = [
    "figi",
    "lei",
    "ticker_and_venue",
    "caip19",
    "iso4217",
    "sec_series",
    "sec_class",
];

/// How often the pulls run once the platform answers, and how soon they are
/// tried again while it does not.
pub const EVERY: Duration = Duration::from_secs(86_400);
pub const AGAIN_AFTER: Duration = Duration::from_secs(300);
pub const FIRST_AGAIN_AFTER: Duration = Duration::from_secs(60);

/// The feed's page, as the platform serves it.
const FEED_PAGE: usize = 1000;

/// The licence list the conductor sends by: the one read last.
pub type Licence = Arc<RwLock<Option<LicenceList>>>;

/// The family a wire scheme is licensed by (W3.25, ruled 2026-10-14).
pub fn family(scheme: &str) -> &str {
    match scheme {
        "figi" | "share_class_figi" | "composite_figi" => "figi",
        "ticker" => "ticker_and_venue",
        other => other,
    }
}

/// The identifiers a request may carry (W3.25; W3's invariant): each in a
/// scheme whose family the list names, or the platform's own ID; never the
/// deployment's.
pub fn sendable_on(identifiers: &[PbIdentifier], list: Option<&LicenceList>) -> Vec<PbIdentifier> {
    let named = |scheme: &str| match list {
        Some(list) => list.schemes.iter().any(|held| held == family(scheme)),
        None => OPEN_FAMILIES.contains(&family(scheme)),
    };
    identifiers
        .iter()
        .filter(|identifier| {
            identifier.scheme != LOCAL_SCHEME
                && !identifier.value.starts_with("LCL-")
                && !identifier.value.trim().is_empty()
                && ((identifier.scheme == meridian_symbology::GLOBAL_ID
                    && identifier.value.starts_with("INS-"))
                    || named(&identifier.scheme))
        })
        .cloned()
        .collect()
}

/// Pulls and hands on what the platform publishes for every deployment.
pub struct ReferencePuller {
    bus: Arc<Bus>,
    platform: Arc<Platform>,
    clock: Arc<dyn meridian_clock::Clock>,
    licence: Licence,
    /// The highest template version and feed sequence seen by this process:
    /// the store keeps its own and passes over what it holds.
    highest_template: Mutex<i64>,
    feed_cursor: Mutex<i64>,
}

impl ReferencePuller {
    pub fn new(
        bus: Arc<Bus>,
        platform: Arc<Platform>,
        clock: Arc<dyn meridian_clock::Clock>,
        licence: Licence,
    ) -> Self {
        Self {
            bus,
            platform,
            clock,
            licence,
            highest_template: Mutex::new(0),
            feed_cursor: Mutex::new(0),
        }
    }

    /// One round: the list, the catalogue, the feed. The first failure
    /// ends it, the platform not reached.
    pub async fn pull_once(&self) -> Result<(), PlatformError> {
        let now = self.clock.now_ns();
        let list = self.platform.pull_licence_list(now).await?;
        if let Ok(mut held) = self.licence.write() {
            *held = Some(list.clone());
        }
        self.publish(
            LICENCE_LIST_PULLED,
            "meridian.v1.LicenceList",
            list.encode_to_vec(),
        );

        let after = self.highest_template.lock().map(|held| *held).unwrap_or(0);
        let templates = self.platform.pull_templates(after, now).await?;
        if !templates.is_empty() {
            let highest = templates
                .iter()
                .filter_map(|template| template.template_version.trim().parse::<i64>().ok())
                .max()
                .unwrap_or(0);
            if let Ok(mut held) = self.highest_template.lock() {
                *held = (*held).max(highest);
            }
            self.publish(
                TEMPLATES_PULLED,
                "meridian.v1.PullTemplatesReply",
                PullTemplatesReply { templates }.encode_to_vec(),
            );
        }

        loop {
            let after = self.feed_cursor.lock().map(|held| *held).unwrap_or(0);
            let entries = self.platform.pull_replacements(after, now).await?;
            let full = entries.len() >= FEED_PAGE;
            if entries.is_empty() {
                break;
            }
            let read_to = entries
                .iter()
                .map(|entry| entry.sequence)
                .max()
                .unwrap_or(after);
            self.publish(
                REPLACEMENTS_PULLED,
                "meridian.v1.PullReplacementsReply",
                PullReplacementsReply { entries }.encode_to_vec(),
            );
            if let Ok(mut held) = self.feed_cursor.lock() {
                *held = read_to.max(after);
            }
            if !full || read_to <= after {
                break;
            }
        }
        Ok(())
    }

    fn publish(&self, topic: &str, kind: &str, payload: Vec<u8>) {
        if let Err(failed) = self.bus.publish(topic, kind, payload, None, None) {
            tracing::warn!(%failed, topic, "what the platform published could not be handed on; pulled again later");
        }
    }

    /// Pull now, then daily; the platform not reached, every few minutes
    /// until it is. The first round is handed on twice, a minute apart and
    /// whole: a store starting after this one missed the first (at-most-once
    /// delivery), and keeps each thing once.
    pub async fn start(self: Arc<Self>) {
        let mut rounds = 0;
        loop {
            let wait = match self.pull_once().await {
                Ok(()) if rounds == 0 => {
                    rounds += 1;
                    if let Ok(mut held) = self.highest_template.lock() {
                        *held = 0;
                    }
                    if let Ok(mut held) = self.feed_cursor.lock() {
                        *held = 0;
                    }
                    FIRST_AGAIN_AFTER
                }
                Ok(()) => EVERY,
                Err(failed) => {
                    tracing::info!(%failed, "the platform's reference data was not pulled; tried again shortly");
                    AGAIN_AFTER
                }
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// The list a deployment sends by before any is read, for a page to show.
pub fn default_licence() -> LicenceList {
    LicenceList {
        version: 0,
        schemes: OPEN_FAMILIES
            .iter()
            .map(|scheme| scheme.to_string())
            .collect(),
        pace: Some(LicencePace {
            batch_size: 100,
            batches_per_minute: 6,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{licence_from_json, replacements_from_json, templates_from_json};

    #[test]
    fn the_platforms_json_reads_as_the_contract_spells_it() {
        let list = licence_from_json(&serde_json::json!({
            "version": "3",
            "schemes": ["figi", "lei", "ticker_and_venue", "caip19", "iso4217"],
            "pace": {"batch_size": 100, "batches_per_minute": 6}
        }))
        .unwrap();
        assert_eq!(list.version, 3);
        assert_eq!(list.pace.unwrap().batch_size, 100);
        assert!(licence_from_json(
            &serde_json::json!({"version": "4", "schemes": ["open_meridian_local"]})
        )
        .is_err());

        let feed = replacements_from_json(&serde_json::json!({"entries": [
            {"sequence": "1043", "instrument_id": "INS-A", "replaced_by": "INS-B"},
            {"sequence": "1044", "instrument_id": "INS-C", "decommissioned": true}
        ]}))
        .unwrap();
        assert_eq!((feed[0].sequence, feed[1].decommissioned), (1043, true));
        assert!(replacements_from_json(
            &serde_json::json!({"entries": [{"sequence": "1", "instrument_id": "LCL-X"}]})
        )
        .is_err());

        let templates = templates_from_json(&serde_json::json!({"templates": [
            {"template_id": "TPL-money-market-fund", "template_version": "2", "name": "Money market fund",
             "asset_class": "fund", "instrument_type": "money_market_fund", "base": false,
             "fields": [{"path": "identifiers", "type": "identifier", "required": true}],
             "match_rule": ["share_class_figi", "sec_class"]}
        ]}))
        .unwrap();
        assert_eq!(
            templates[0].asset_class,
            meridian_domain::v1::AssetClass::Fund as i32
        );
        assert_eq!(
            templates[0].instrument_type,
            meridian_domain::v1::InstrumentType::MoneyMarketFund as i32
        );
        assert!(templates_from_json(
            &serde_json::json!({"templates": [{"template_id": "TPL-x", "asset_class": "bond"}]})
        )
        .is_err());
    }

    #[test]
    fn only_listed_schemes_travel_and_never_the_deployments_own() {
        let held = |scheme: &str, value: &str| PbIdentifier {
            scheme: scheme.into(),
            value: value.into(),
            ..Default::default()
        };
        let asked = vec![
            held("composite_figi", "BBG000BWQFY7"),
            held("cusip", "037833100"),
            held(LOCAL_SCHEME, "LCL-01J8"),
            held("figi", "LCL-SNEAKY"),
            held(meridian_symbology::GLOBAL_ID, "INS-01J8"),
        ];
        let sent: Vec<String> = sendable_on(&asked, None)
            .into_iter()
            .map(|i| i.scheme)
            .collect();
        assert_eq!(sent, vec!["composite_figi", meridian_symbology::GLOBAL_ID]);
        let mut list = default_licence();
        list.schemes.retain(|scheme| scheme != "figi");
        assert!(sendable_on(&asked, Some(&list))
            .iter()
            .all(|i| !i.scheme.contains("figi")));
    }
}
