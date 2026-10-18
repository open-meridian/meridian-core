//! What the conductor pulls from the platform for the instrument store, and
//! the licence list it sends by (contract v24; W3.24, W3.25, W3.27).
//!
//! On start, daily, and -- the platform not reached -- every few minutes
//! until it is, which is how a reconnect is noticed: the licence list, the
//! template catalogue, and the
//! replacement feed after the last sequence read. The catalogue is asked
//! whole (no subject, no `after_version`) to find templates not yet held,
//! and each template held is asked for its versions after the highest seen
//! by its `template_id` (W3.24, ruled 2026-10-14). Each is handed to the
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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::v1::{
    Identifier as PbIdentifier, LicenceList, LicencePace, PullReplacementsReply,
    PullTemplatesReply, SecurityTemplate,
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
    /// The highest version of each template, by its ID, and the feed
    /// sequence, seen by this process: the store keeps its own and passes
    /// over what it holds.
    highest_template: Mutex<BTreeMap<String, i64>>,
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
            highest_template: Mutex::new(BTreeMap::new()),
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

        let templates = self.pull_templates(now).await?;
        if !templates.is_empty() {
            if let Ok(mut held) = self.highest_template.lock() {
                for template in &templates {
                    let version = version_of(template);
                    let highest = held.entry(template.template_id.clone()).or_insert(0);
                    *highest = (*highest).max(version);
                }
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

    /// W3.24: each template held asked for its versions after the highest
    /// seen, by its `template_id`; then the catalogue asked whole, for the
    /// templates not held yet. An answer with nothing newer is no failure;
    /// the platform away ends the round, to be tried again. A template the
    /// platform refuses to answer for is said and passed over, the others
    /// still asked.
    async fn pull_templates(&self, now: i64) -> Result<Vec<SecurityTemplate>, PlatformError> {
        let held: BTreeMap<String, i64> = self
            .highest_template
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default();
        let mut newer: Vec<SecurityTemplate> = Vec::new();
        for (template_id, after) in &held {
            match self
                .platform
                .pull_templates(Some((template_id, *after)), now)
                .await
            {
                Ok(found) => newer.extend(
                    found
                        .into_iter()
                        .filter(|t| t.template_id == *template_id && version_of(t) > *after),
                ),
                Err(failed) if failed.is_outage() => return Err(failed),
                Err(failed) => {
                    tracing::warn!(%failed, template_id, "a template's newer versions were not answered")
                }
            }
        }
        let whole = self.platform.pull_templates(None, now).await?;
        newer.extend(
            whole
                .into_iter()
                .filter(|t| !held.contains_key(&t.template_id)),
        );
        Ok(newer)
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
                        held.clear();
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

/// A template's version as a number; 0 where it does not read.
fn version_of(template: &SecurityTemplate) -> i64 {
    template.template_version.trim().parse().unwrap_or(0)
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

    fn template(id: &str, version: &str) -> serde_json::Value {
        serde_json::json!({"template_id": id, "template_version": version, "name": id,
                           "asset_class": "equity", "base": false})
    }

    fn answer(
        body: serde_json::Value,
    ) -> Result<crate::platform::Response, crate::platform::TransportFailure> {
        Ok(crate::platform::tests::reply(200, &body.to_string()))
    }

    /// The fixture's "a refresh after the highest version held" (W3.24,
    /// ruled 2026-10-14): the first pull asks the whole catalogue; a refresh
    /// asks each template held after its highest version by its
    /// template_id, an answer with nothing newer no failure, and the whole
    /// catalogue again only for templates not held. No after_version ever
    /// travels without its subject.
    #[tokio::test]
    async fn a_refresh_asks_each_template_after_its_highest_and_never_a_bare_after_version() {
        const NOW: i64 = 1_792_300_000_000_000_000;
        let licence = serde_json::json!({"version": "3", "schemes": ["figi"],
                                         "pace": {"batch_size": 100, "batches_per_minute": 6}});
        let none = serde_json::json!({"entries": []});
        let fake = crate::platform::tests::Fake::new(vec![
            // The first round: the list, the whole catalogue, the feed.
            answer(licence.clone()),
            answer(
                serde_json::json!({"templates": [template("TPL-equity", "1"), template("TPL-equity-base", "1")]}),
            ),
            answer(none.clone()),
            // The refresh: the list, each template held, the whole, the feed.
            answer(licence),
            answer(serde_json::json!({"templates": [template("TPL-equity", "2")]})),
            answer(serde_json::json!({"templates": []})),
            answer(serde_json::json!({"templates": [
                template("TPL-equity", "1"), template("TPL-equity", "2"),
                template("TPL-equity-base", "1"), template("TPL-fund", "1")]})),
            answer(none),
        ]);
        let clock = Arc::new(meridian_clock::ManualClock::at(NOW));
        let bus = Arc::new(Bus::single(
            "conductor-1",
            Arc::new(meridian_bus::MemoryBackend::new()),
            clock.clone(),
        ));
        let puller = ReferencePuller::new(
            Arc::clone(&bus),
            Arc::new(crate::platform::tests::platform(fake.clone())),
            clock,
            Licence::default(),
        );
        let mut pulled = bus.subscribe_every(TEMPLATES_PULLED);
        let handed = |delivery: meridian_bus::Delivery| {
            PullTemplatesReply::decode(&delivery.envelope.payload[..])
                .unwrap()
                .templates
                .iter()
                .map(|t| format!("{} {}", t.template_id, t.template_version))
                .collect::<Vec<_>>()
        };
        puller.pull_once().await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), pulled.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(handed(first), vec!["TPL-equity 1", "TPL-equity-base 1"]);
        puller.pull_once().await.unwrap();
        let refreshed = tokio::time::timeout(Duration::from_secs(2), pulled.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(handed(refreshed), vec!["TPL-equity 2", "TPL-fund 1"]);
        let urls = fake.urls();
        let templates: Vec<&String> = urls
            .iter()
            .filter(|u| u.contains("/reference/templates"))
            .collect();
        assert!(
            templates[0].ends_with("/api/v1/reference/templates"),
            "{templates:?}"
        );
        assert!(
            templates[1]
                .ends_with("/api/v1/reference/templates?template_id=TPL-equity&after_version=1"),
            "{templates:?}"
        );
        assert!(
            templates[2].ends_with("?template_id=TPL-equity-base&after_version=1"),
            "{templates:?}"
        );
        assert!(
            templates[3].ends_with("/api/v1/reference/templates"),
            "{templates:?}"
        );
        for url in &urls {
            assert!(
                !url.contains("after_version") || url.contains("template_id="),
                "an after_version sent without its subject: {url}"
            );
        }
    }

    #[test]
    fn the_catalogues_path_never_carries_a_bare_after_version() {
        assert_eq!(
            crate::platform::templates_path(None),
            "/api/v1/reference/templates"
        );
        assert_eq!(
            crate::platform::templates_path(Some(("", 4))),
            "/api/v1/reference/templates",
            "no subject, no after_version"
        );
        assert_eq!(
            crate::platform::templates_path(Some(("TPL-a b", 4))),
            "/api/v1/reference/templates?template_id=TPL-a%20b&after_version=4"
        );
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
