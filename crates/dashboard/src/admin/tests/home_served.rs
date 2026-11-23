//! The home page, "Your plugins", served for a real browser to look at, in
//! both its views, at a phone's, a narrow pane's and a desktop's width.
//!
//! Twenty-four instances shaped like a development deployment's:
//! connectivity plugins that trade, long plugin names under the
//! `open-meridian/` prefix, short ones beside them, one not healthy, one
//! starting, ones with a setting-up step open, one stopped and one never
//! launched -- each status Settings › Plugins draws -- behind a stand-in conductor and lake
//! answering the access records, the catalogue's launches and the datasets
//! listing the home reads. snaptrade-1 is Working with an account it
//! reported that nothing links: a row with something in Needs you.
//!
//! Ignored in the ordinary run: it serves until it is stopped. Two people
//! are signed in, a deployment admin and a member reading four; the
//! sessions it starts are printed for the browser to present as
//! `meridian_session`. `MERIDIAN_UI_DIR` is the
//! kit's build, `MERIDIAN_HOME_PORT` the port (8080 by default).

use std::sync::Arc;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessRecords, DatasetEntitlement, DatasetLiveness, DatasetRef,
    ExternalAccount, ExternalAccountsEvent, KnownPluginRoles, ListDatasetsReply, Permission, PluginCatalogue, PluginLaunch,
    PluginLaunchState, PluginReport, PluginSettingValue, PluginSettingsRecord, SignInRecord,
    UserGroup,
};
use meridian_pb::v1::{AccessLevel, DatasetLicence};
use prost::Message;

use crate::clock::Clock;
use crate::records::{RecordsCache, ACCESS_RECORDS};
use crate::session::Sessions;
use crate::web::{router, App};

const ADMIN: &str = "https://directory.example.org|admin";
const MEMBER: &str = "https://directory.example.org|member";
const SECOND_NS: i64 = 1_000_000_000;

/// Each instance: its id, the plugin's name (none: installed, not launched
/// from the catalogue), its roles, and how it is.
struct Shaped {
    instance: &'static str,
    name: &'static str,
    roles: &'static [&'static str],
    state: PluginLaunchState,
    healthy: Option<&'static str>,
    environment: &'static str,
    licensed: bool,
    alive_ago_s: Option<i64>,
    stale_after_s: i64,
}

const SHAPED: [Shaped; 24] = [
    Shaped {
        instance: "coinbase-1",
        name: "open-meridian/coinbase",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "ecb-1",
        name: "open-meridian/ecb",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "kalshi-1",
        name: "open-meridian/kalshi",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "polymarket-1",
        name: "open-meridian/polymarket",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "pyth-1",
        name: "open-meridian/pyth",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: None,
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "sample-multi-asset-1",
        name: "sample-multi-asset",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "sample-operations-1",
        name: "sample-operations",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "sample-options-1",
        name: "sample-options",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "sample-pricing-1",
        name: "sample-pricing",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "sample-reporting-1",
        name: "sample-reporting",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "tiingo-1",
        name: "open-meridian/tiingo",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "truefx-1",
        name: "open-meridian/truefx",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "us-treasury-1",
        name: "open-meridian/us-treasury",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "workspace-1",
        name: "workspace",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "alpaca-1",
        name: "open-meridian/alpaca",
        roles: &["connectivity", "data"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "paper",
        licensed: true,
        alive_ago_s: Some(20),
        stale_after_s: 60,
    },
    Shaped {
        instance: "desk-1",
        name: "desk",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "fed-board-1",
        name: "open-meridian/fed-board",
        roles: &["data"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: true,
        alive_ago_s: Some(3 * 60 + 5),
        stale_after_s: 0,
    },
    Shaped {
        instance: "kraken-1",
        name: "open-meridian/kraken",
        roles: &["data"],
        state: PluginLaunchState::Stopped,
        healthy: None,
        environment: "",
        licensed: true,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "ny-fed-1",
        name: "",
        roles: &["data"],
        state: PluginLaunchState::Unspecified,
        healthy: None,
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "oanda-1",
        name: "open-meridian/oanda",
        roles: &["connectivity", "data"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "sandbox",
        licensed: true,
        alive_ago_s: Some(4),
        stale_after_s: 30,
    },
    Shaped {
        instance: "polymarket-us-1",
        name: "open-meridian/polymarket-us",
        roles: &["data"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "sample-fx-1",
        name: "sample-fx",
        roles: &["signal"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: false,
        alive_ago_s: None,
        stale_after_s: 0,
    },
    Shaped {
        instance: "snaptrade-1",
        name: "open-meridian/snaptrade",
        roles: &["connectivity", "data"],
        state: PluginLaunchState::Launched,
        healthy: Some(""),
        environment: "",
        licensed: true,
        alive_ago_s: Some(40),
        stale_after_s: 0,
    },
    Shaped {
        instance: "tradier-1",
        name: "open-meridian/tradier",
        roles: &["connectivity", "data"],
        state: PluginLaunchState::Launched,
        healthy: Some("Tradier refused the access token <401 Unauthorized>"),
        environment: "paper",
        licensed: true,
        alive_ago_s: Some(900),
        stale_after_s: 120,
    },
];

fn records() -> AccessRecords {
    AccessRecords {
        user_groups: vec![
            UserGroup {
                user_group_id: "UG-1".into(),
                name: "Admins".into(),
                directory_groups: vec![],
                logins: vec![ADMIN.into()],
            },
            UserGroup {
                user_group_id: "UG-2".into(),
                name: "Desk".into(),
                directory_groups: vec![],
                logins: vec![MEMBER.into()],
            },
        ],
        permissions: [
            meridian_access::DEPLOYMENT_ADMIN,
            meridian_access::ALL_PLUGINS_ADMIN,
            "AX-DATA",
        ]
        .iter()
        .enumerate()
        .map(|(n, group)| Permission {
            permission_id: format!("P-{n}"),
            user_group_id: "UG-1".into(),
            account_group_id: String::new(),
            access_group_id: (*group).into(),
        })
        .chain(std::iter::once(Permission {
            permission_id: "P-member".into(),
            user_group_id: "UG-2".into(),
            account_group_id: "AG-ALL".into(),
            access_group_id: "AX-READ".into(),
        }))
        .collect(),
        // Ada holds admin on the two not running, through a group of her
        // own: the home lists only what a person holds or what runs. Max, a
        // member, reads four.
        access_groups: vec![
            AccessGroup {
                access_group_id: "AX-READ".into(),
                name: "Desk readers".into(),
                entries: ["desk-1", "workspace-1", "tradier-1", "kraken-1"]
                    .iter()
                    .map(|instance| AccessEntry {
                        plugin_instance_id: (*instance).into(),
                        level: AccessLevel::Read as i32,
                        role: "signal".into(),
                    })
                    .collect(),
                built_in: false,
            },
            AccessGroup {
                access_group_id: "AX-DATA".into(),
                name: "Data".into(),
                entries: ["kraken-1", "ny-fed-1"]
                    .iter()
                    .map(|instance| AccessEntry {
                        plugin_instance_id: (*instance).into(),
                        level: AccessLevel::Admin as i32,
                        role: "data".into(),
                    })
                    .collect(),
                built_in: false,
            },
        ],
        people: vec![
            SignInRecord {
                subject: ADMIN.into(),
                display_name: "Ada Admin".into(),
                ..Default::default()
            },
            SignInRecord {
                subject: MEMBER.into(),
                display_name: "Max Member".into(),
                ..Default::default()
            },
        ],
        account_groups: vec![meridian_domain::v1::AccountGroup {
            account_group_id: "AG-ALL".into(),
            name: "All accounts".into(),
            ..Default::default()
        }],
        known_plugins: SHAPED
            .iter()
            .map(|shaped| KnownPluginRoles {
                plugin_instance_id: shaped.instance.into(),
                roles: shaped.roles.iter().map(|r| r.to_string()).collect(),
            })
            .collect(),
        plugin_settings: SHAPED
            .iter()
            .filter(|shaped| !shaped.environment.is_empty())
            .map(|shaped| PluginSettingsRecord {
                plugin_instance_id: shaped.instance.into(),
                values: vec![PluginSettingValue {
                    name: "environment".into(),
                    value: shaped.environment.into(),
                }],
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn catalogue(now: i64) -> PluginCatalogue {
    PluginCatalogue {
        launches: SHAPED
            .iter()
            .filter(|shaped| shaped.state != PluginLaunchState::Unspecified)
            .map(|shaped| PluginLaunch {
                instance_id: shaped.instance.into(),
                name: shaped.name.into(),
                version: "0.4.0".into(),
                roles: shaped.roles.iter().map(|r| r.to_string()).collect(),
                launched_by: ADMIN.into(),
                // pyth-1 launched moments ago and not heard from: Starting.
                launched_at_ns: if shaped.instance == "pyth-1" {
                    now - 10 * SECOND_NS
                } else {
                    now - 86_400 * SECOND_NS
                },
                state: shaped.state as i32,
                stopped_by: if shaped.state == PluginLaunchState::Stopped {
                    ADMIN.into()
                } else {
                    String::new()
                },
                stopped_at_ns: if shaped.state == PluginLaunchState::Stopped {
                    now - 2 * 3_600 * SECOND_NS
                } else {
                    0
                },
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn lake(now: i64) -> ListDatasetsReply {
    let mut reply = ListDatasetsReply::default();
    for shaped in SHAPED.iter().filter(|s| s.roles.contains(&"data")) {
        let dataset = format!("{}:daily", shaped.instance);
        reply.datasets.push(DatasetRef {
            dataset: dataset.clone(),
            instance: shaped.instance.into(),
            stale_after_ns: shaped.stale_after_s * SECOND_NS,
            liveness: shaped
                .alive_ago_s
                .map(|ago| DatasetLiveness {
                    dataset: dataset.clone(),
                    venue_id: String::new(),
                    alive_at_ns: now - ago * SECOND_NS,
                    instance: shaped.instance.into(),
                })
                .into_iter()
                .collect(),
            ..Default::default()
        });
        if shaped.licensed {
            reply.licences.push(DatasetLicence {
                dataset: dataset.clone(),
                kept: true,
                updated_at_ns: now - 86_400 * SECOND_NS,
                ..Default::default()
            });
            reply.entitlements.push(DatasetEntitlement {
                dataset: dataset.clone(),
                instance: "desk-1".into(),
                allowed: true,
                ..Default::default()
            });
        }
    }
    reply
}

fn report(shaped: &Shaped, now: i64) -> Option<PluginReport> {
    let detail = shaped.healthy?;
    Some(PluginReport {
        plugin_instance_id: shaped.instance.into(),
        roles: shaped.roles.iter().map(|r| r.to_string()).collect(),
        registered: true,
        healthy: detail.is_empty(),
        health_detail: detail.into(),
        last_heartbeat_at_ns: now,
        reported_at_ns: now,
        contract_version: "v30".into(),
        ..Default::default()
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "serves until stopped: for a browser to look at the home page"]
async fn serve_the_home_for_a_browser() {
    let clock = meridian_clock::SystemClock;
    let bus = Arc::new(Bus::single(
        "dashboard-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    bus.serve(ACCESS_RECORDS, |_| {
        Ok(("".into(), records().encode_to_vec()))
    });
    bus.serve(crate::catalogue::PLUGIN_CATALOGUE, |_| {
        let now = meridian_clock::SystemClock.now_ns();
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            catalogue(now).encode_to_vec(),
        ))
    });
    bus.serve(crate::admin::data_sources::LIST_DATASETS, |_| {
        let now = meridian_clock::SystemClock.now_ns();
        Ok(("".into(), lake(now).encode_to_vec()))
    });
    let health: Arc<crate::health::Health> = Arc::default();
    let hear = {
        let health = Arc::clone(&health);
        move || {
            let now = meridian_clock::SystemClock.now_ns();
            for shaped in &SHAPED {
                if let Some(report) = report(shaped, now) {
                    health.hear(shaped.instance, report);
                }
            }
        }
    };
    hear();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            hear();
        }
    });
    // An account snaptrade-1 reported that nothing links: Working, and
    // something in Needs you (`tr.attention`).
    let custody: Arc<crate::custody::Custody> = Arc::default();
    custody.hear_accounts(
        "snaptrade-1",
        ExternalAccountsEvent {
            accounts: vec![ExternalAccount {
                external_account_id: "SNAP-1".into(),
                name: "Individual Brokerage 1234".into(),
                venue_account_type: "Individual".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let kit = crate::kit::Kit::at(
        std::env::var("MERIDIAN_UI_DIR").unwrap_or_else(|_| crate::kit::IN_IMAGE.into()),
    )
    .expect("the kit, which the browser's pages load");
    crate::html::use_kit(kit.base());
    let port: u16 = std::env::var("MERIDIAN_HOME_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(8080);
    let cache = Arc::new(RecordsCache::default());
    cache.store(records(), clock.now_ns());
    let sessions = Arc::new(Sessions::default());
    let session = sessions.start(ADMIN, "Ada Admin", vec![], clock.now_ns());
    let member = sessions.start(MEMBER, "Max Member", vec![], clock.now_ns());
    let app = Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: Arc::clone(&cache),
        sessions,
        delegations: Arc::new(crate::delegation::Delegations::default()),
        public_url: String::new(),
        clock: Arc::new(clock),
        bus: Arc::clone(&bus),
        oidc: None,
        directory: None,
        accounts: None,
        sign_in_failures: Default::default(),
        secure_cookies: false,
        // Each instance openable from its card, as on a deployment.
        plugins: Some(Arc::new(
            crate::plugins::Plugins::new(
                &format!("http://localhost:{port}"),
                "http://{instance}.sidecars.invalid:9",
                crate::signing::Signer::holding(
                    "dashboard-test",
                    ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
                ),
            )
            .unwrap(),
        )),
        registry: None,
        custody,
        health,
        kit: Some(Arc::new(kit)),
        bounds: Arc::default(),
        turns: Arc::default(),
        tickets: Arc::default(),
        prepared: Arc::default(),
        service_accounts: Arc::default(),
    });
    tokio::spawn(crate::records::refresh_forever(
        bus,
        cache,
        Arc::new(meridian_clock::SystemClock),
    ));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("the port");
    println!("SESSION={session}");
    println!("MEMBER_SESSION={member}");
    println!("serving the home on port {port}");
    axum::serve(listener, router(app)).await.unwrap();
}
