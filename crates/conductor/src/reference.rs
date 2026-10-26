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
//!
//! **A store that starts after a hand-off** (found by plugin-fred's e2e,
//! 2026-10-24): the bus delivers at most once, so a store not yet listening
//! when the round is handed on misses it, and until the next daily round
//! would send by the open schemes alone. The store says it started, as every
//! component does (W5.20, `platform.deployment.event.component-report`,
//! after it has subscribed); the puller hears it and, for a store started
//! after its last hand-off, hands the round on again at once.

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

/// The open schemes sendable before any list is read (W3.25): from contract
/// 28's core release C2 the SEC's `cik` too, a company's public identifier
/// (W1.32).
pub const OPEN_FAMILIES: [&str; 8] = [
    "figi",
    "lei",
    "ticker_and_venue",
    "caip19",
    "iso4217",
    "sec_series",
    "sec_class",
    "cik",
];

/// How often the pulls run once the platform answers, and how soon they are
/// tried again while it does not.
pub const EVERY: Duration = Duration::from_secs(86_400);
pub const AGAIN_AFTER: Duration = Duration::from_secs(300);
pub const FIRST_AGAIN_AFTER: Duration = Duration::from_secs(60);

/// Where each component says what it runs and when it started (W5.20).
pub const COMPONENT_REPORT: &str = "platform.deployment.event.component-report";

/// The component the round is handed to.
pub const STORE_COMPONENT: &str = "instrument";

/// A start this close before the last hand-off is still taken as after it:
/// the store's clock and this one are two machines' (skew, never a gap).
pub const START_SKEW_NS: i64 = 10_000_000_000;

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

/// The family an administrator's own code is licensed by (ruling 545): wire
/// `symbol` with the administrator in its `source`, for an administrator
/// the list names.
pub const ADMINISTRATOR_SYMBOL: &str = "administrator_symbol";

/// The family an identifier is licensed by under a list: a `symbol` whose
/// source is an administrator the list names is `administrator_symbol`
/// (ruling 545); any other, its scheme's family.
pub fn family_on<'a>(identifier: &'a PbIdentifier, list: &LicenceList) -> &'a str {
    if identifier.scheme == "symbol"
        && !identifier.source.is_empty()
        && list.administrators.contains(&identifier.source)
    {
        ADMINISTRATOR_SYMBOL
    } else {
        family(&identifier.scheme)
    }
}

/// The identifiers a request may carry (W3.25; W3's invariant): each in a
/// scheme whose family the list names, or the platform's own ID; never the
/// deployment's.
pub fn sendable_on(identifiers: &[PbIdentifier], list: Option<&LicenceList>) -> Vec<PbIdentifier> {
    let named = |identifier: &PbIdentifier| match list {
        Some(list) => list
            .schemes
            .iter()
            .any(|held| held == family_on(identifier, list)),
        None => OPEN_FAMILIES.contains(&family(&identifier.scheme)),
    };
    identifiers
        .iter()
        .filter(|identifier| {
            identifier.scheme != LOCAL_SCHEME
                && !identifier.value.starts_with("LCL-")
                && !identifier.value.trim().is_empty()
                && ((identifier.scheme == meridian_symbology::GLOBAL_ID
                    && identifier.value.starts_with("INS-"))
                    || named(identifier))
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
    /// When the licence list was last handed on, and the store's start last
    /// heard: a store started after the hand-off is handed the round again.
    handed_at_ns: Mutex<i64>,
    store_started_ns: Mutex<i64>,
    store_started: tokio::sync::Notify,
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
            handed_at_ns: Mutex::new(0),
            store_started_ns: Mutex::new(0),
            store_started: tokio::sync::Notify::new(),
        }
    }

    /// Whether a store's report says it started after the last hand-off,
    /// and so missed it: a start not heard before, at or after the
    /// hand-off less the skew allowed. Nothing handed on yet needs no
    /// repeat; the round under way reaches it.
    pub fn store_missed_it(&self, started_at_ns: i64) -> bool {
        let Ok(mut heard) = self.store_started_ns.lock() else {
            return false;
        };
        if *heard == started_at_ns {
            return false;
        }
        *heard = started_at_ns;
        let handed = self.handed_at_ns.lock().map(|held| *held).unwrap_or(0);
        handed > 0 && started_at_ns >= handed - START_SKEW_NS
    }

    /// The store's reports, heard for as long as this runs: one that
    /// started after the last hand-off wakes the round.
    async fn hear_store_starts(self: Arc<Self>, mut reports: meridian_bus::Subscription) {
        while let Some(delivery) = reports.recv().await {
            let Ok(report) =
                meridian_domain::v1::ComponentReport::decode(&delivery.envelope.payload[..])
            else {
                continue;
            };
            if report.component == STORE_COMPONENT && self.store_missed_it(report.started_at_ns) {
                tracing::info!(
                    started_at_ns = report.started_at_ns,
                    "the instrument store started after the reference data was handed on; handing it on again"
                );
                self.store_started.notify_one();
            }
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
        if let Ok(mut held) = self.handed_at_ns.lock() {
            *held = self.clock.now_ns();
        }

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
    /// delivery), and keeps each thing once. A store heard starting after a
    /// hand-off is handed a whole round at once, whenever it starts.
    pub async fn start(self: Arc<Self>) {
        // Subscribed before the first hand-off, so no start goes unheard.
        let reports = self.bus.subscribe_every(COMPONENT_REPORT);
        tokio::spawn(Arc::clone(&self).hear_store_starts(reports));
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
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.store_started.notified() => {
                    // Whole, as the first round's repeat is.
                    if let Ok(mut held) = self.highest_template.lock() {
                        held.clear();
                    }
                    if let Ok(mut held) = self.feed_cursor.lock() {
                        *held = 0;
                    }
                }
            }
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
        administrators: Vec::new(),
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
        assert!(list.administrators.is_empty());
        // Ruling 545: the administrators the administrator_symbol family admits.
        let admitting = licence_from_json(&serde_json::json!({
            "version": "5",
            "schemes": ["figi", "administrator_symbol"],
            "administrators": ["nyfed", "frb", "ecb", "ustreasury"]
        }))
        .unwrap();
        assert_eq!(
            admitting.administrators,
            ["nyfed", "frb", "ecb", "ustreasury"]
        );
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

    /// A store that starts after the first hand-off (plugin-fred's e2e,
    /// 2026-10-24): its start, heard on the component reports, has the round
    /// handed on again at once -- not a minute or a day later -- and a start
    /// from before the hand-off, which heard it, does not.
    #[tokio::test]
    async fn a_store_starting_after_the_hand_off_is_handed_the_list_again() {
        const NOW: i64 = 1_792_300_000_000_000_000;
        let licence = serde_json::json!({"version": "7",
                                         "schemes": ["figi", "administrator_symbol"],
                                         "administrators": ["nyfed"],
                                         "pace": {"batch_size": 100, "batches_per_minute": 6}});
        let round = |licence: &serde_json::Value| {
            vec![
                answer(licence.clone()),
                answer(serde_json::json!({"templates": []})),
                answer(serde_json::json!({"entries": []})),
            ]
        };
        let fake = crate::platform::tests::Fake::new([round(&licence), round(&licence)].concat());
        let clock = Arc::new(meridian_clock::ManualClock::at(NOW));
        let bus = Arc::new(Bus::single(
            "conductor-1",
            Arc::new(meridian_bus::MemoryBackend::new()),
            clock.clone(),
        ));
        let puller = Arc::new(ReferencePuller::new(
            Arc::clone(&bus),
            Arc::new(crate::platform::tests::platform(fake.clone())),
            clock,
            Licence::default(),
        ));
        // Something on the bus before the store is, to see the first hand-off.
        let mut early = bus.subscribe_every(LICENCE_LIST_PULLED);
        tokio::spawn(Arc::clone(&puller).start());
        tokio::time::timeout(Duration::from_secs(2), early.recv())
            .await
            .expect("the first hand-off")
            .unwrap();

        // The store starts now: subscribed, then its first report.
        let mut store = bus.subscribe_every(LICENCE_LIST_PULLED);
        let report = |started_at_ns: i64| {
            meridian_domain::v1::ComponentReport {
                component: STORE_COMPONENT.into(),
                started_at_ns,
                ..Default::default()
            }
            .encode_to_vec()
        };
        // A start a minute before the hand-off heard it: nothing again.
        bus.publish(
            COMPONENT_REPORT,
            "meridian.v1.ComponentReport",
            report(NOW - 60_000_000_000),
            None,
            None,
        )
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), store.recv())
                .await
                .is_err(),
            "a store up before the hand-off is not handed it again"
        );
        bus.publish(
            COMPONENT_REPORT,
            "meridian.v1.ComponentReport",
            report(NOW + 130_000_000),
            None,
            None,
        )
        .unwrap();
        let again = tokio::time::timeout(Duration::from_secs(2), store.recv())
            .await
            .expect("the list handed on again, well inside FIRST_AGAIN_AFTER")
            .unwrap();
        let list = LicenceList::decode(&again.envelope.payload[..]).unwrap();
        assert_eq!(list.version, 7);
        assert_eq!(list.administrators, ["nyfed"]);
        // Heard again at its next report, the same start is not a new one.
        assert!(!puller.store_missed_it(NOW + 130_000_000));
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
            held("cik", "0000320193"),
        ];
        let sent: Vec<String> = sendable_on(&asked, None)
            .into_iter()
            .map(|i| i.scheme)
            .collect();
        // With no list read, the open schemes: the CIK among them (W3.25).
        assert_eq!(
            sent,
            vec!["composite_figi", meridian_symbology::GLOBAL_ID, "cik"]
        );
        let mut list = default_licence();
        list.schemes.retain(|scheme| scheme != "figi");
        assert!(sendable_on(&asked, Some(&list))
            .iter()
            .all(|i| !i.scheme.contains("figi")));
    }

    #[test]
    fn an_administrators_symbol_travels_only_where_the_list_admits_it() {
        let symbol = |value: &str, source: &str| PbIdentifier {
            scheme: "symbol".into(),
            value: value.into(),
            source: source.into(),
            ..Default::default()
        };
        let asked = vec![
            symbol("SOFR", "nyfed"),
            symbol("ESTR", "ecb"),
            symbol("AAPL", "snaptrade"),
        ];
        let sources = |list: Option<&LicenceList>| -> Vec<String> {
            sendable_on(&asked, list)
                .into_iter()
                .map(|i| i.source)
                .collect()
        };
        // Ruling 545: the family listed and the source admitted.
        let mut admitting = default_licence();
        admitting.schemes.push(ADMINISTRATOR_SYMBOL.into());
        admitting.administrators = vec!["nyfed".into()];
        assert_eq!(sources(Some(&admitting)), vec!["nyfed"]);
        // Without the family, or with no list read, none travels.
        let mut unlisted = admitting.clone();
        unlisted
            .schemes
            .retain(|scheme| scheme != ADMINISTRATOR_SYMBOL);
        assert!(sources(Some(&unlisted)).is_empty());
        assert!(sources(None).is_empty());
        assert!(sources(Some(&default_licence())).is_empty());
    }
}
