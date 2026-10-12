//! Service accounts (contract v21): created, changed, disabled and removed,
//! each change its own record; what a role runs as, among those granted on
//! it; and a plugin's call as its role's service account admitted through
//! the act's door -- made with an assertion naming the account and its owner,
//! prepared for everyone who may make it and the owner, or refused naming
//! the field -- through the real sidecar's front door to a stand-in plugin.

use std::sync::{Arc, Mutex};

use axum::body::to_bytes;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::SigningKey;
use meridian_access::AccessLevel;
use meridian_bus::{Bus, MemoryBackend};
use meridian_clock::SystemClock;
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessRecords, AccountGroup, AccountRecord, AccountState,
    KnownPluginRoles, Permission, SignInRecord, UserGroup,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    CallAsServiceAccountRequest, CallerAssertion, CallerClaims, InterfaceDeclaration,
    PreparedCallState, RegisterRequest, SubjectKind, ToolDeclaration,
};
use meridian_sidecar::front_door::{self, FrontDoor, Verifier};
use meridian_sidecar::{Contract, Identity, Sidecar};
use prost::Message;

use super::store::{Setting, State};
use super::{Asked, By};
use crate::plugins::Plugins;
use crate::prepared::levels::Level;
use crate::records::RecordsCache;
use crate::signing::Signer;
use crate::web::App;
use crate::Clock;

const INSTANCE: &str = "ops-1";
const ROLE: &str = "operations";
const KEY_ID: &str = "dashboard-2026-10-0a1b2c3d";
const ADMIN: &str = "local|harness";
const CAT: &str = "local|cat";
const BEA: &str = "local|bea";
const DAN: &str = "local|dan";
const DASHBOARD: &str = "meridian.test:8443";

fn admin() -> By {
    By {
        subject: ADMIN.into(),
        display_name: "Harness".into(),
        deployment_admin: true,
    }
}

fn person(subject: &str) -> By {
    By {
        subject: subject.into(),
        display_name: subject.trim_start_matches("local|").into(),
        deployment_admin: false,
    }
}

/// Cat owns the account; Bea writes on operations for ACC-1 by hand; Dan
/// holds nothing. The service account's user group, once it has an ID,
/// grants it `grant` on operations over ACC-1.
fn records(service_account: Option<(&str, AccessLevel)>) -> AccessRecords {
    let seen = |subject: &str, name: &str| SignInRecord {
        subject: subject.into(),
        display_name: name.into(),
        ..Default::default()
    };
    let mut held = AccessRecords {
        accounts: vec![AccountRecord {
            account_id: "ACC-1".into(),
            name: "Growth".into(),
            state: AccountState::Open as i32,
            ..Default::default()
        }],
        account_groups: vec![AccountGroup {
            account_group_id: "AcG-1".into(),
            name: "Growth".into(),
            account_ids: vec!["ACC-1".into()],
            built_in: false,
        }],
        user_groups: vec![UserGroup {
            user_group_id: "UG-bea".into(),
            name: "Operations".into(),
            directory_groups: vec![],
            logins: vec![BEA.into()],
        }],
        access_groups: vec![
            AccessGroup {
                access_group_id: "AG-write".into(),
                name: "Operations writers".into(),
                entries: vec![AccessEntry {
                    plugin_instance_id: INSTANCE.into(),
                    level: AccessLevel::Write as i32,
                    role: ROLE.into(),
                }],
                built_in: false,
            },
            AccessGroup {
                access_group_id: "AG-read".into(),
                name: "Operations readers".into(),
                entries: vec![AccessEntry {
                    plugin_instance_id: INSTANCE.into(),
                    level: AccessLevel::Read as i32,
                    role: ROLE.into(),
                }],
                built_in: false,
            },
        ],
        permissions: vec![Permission {
            permission_id: "P-bea".into(),
            user_group_id: "UG-bea".into(),
            account_group_id: "AcG-1".into(),
            access_group_id: "AG-write".into(),
        }],
        people: vec![
            seen(ADMIN, "Harness"),
            seen(CAT, "Cat Ruiz"),
            seen(BEA, "Bea Ortiz"),
            seen(DAN, "Dan Wu"),
        ],
        known_plugins: vec![KnownPluginRoles {
            plugin_instance_id: INSTANCE.into(),
            roles: vec![ROLE.into()],
        }],
        ..Default::default()
    };
    if let Some((id, level)) = service_account {
        held.user_groups.push(UserGroup {
            user_group_id: "UG-sa".into(),
            name: "The sweep".into(),
            directory_groups: vec![],
            logins: vec![id.into()],
        });
        held.permissions.push(Permission {
            permission_id: "P-sa".into(),
            user_group_id: "UG-sa".into(),
            account_group_id: "AcG-1".into(),
            access_group_id: if level == AccessLevel::Write {
                "AG-write".into()
            } else {
                "AG-read".into()
            },
        });
        // Even the deployment admin's capabilities give it no admin route
        // and no config command (W4.9, W6.32).
        held.permissions.push(Permission {
            permission_id: "P-sa-admin".into(),
            user_group_id: "UG-sa".into(),
            account_group_id: String::new(),
            access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
        });
    }
    held
}

fn tool(name: &str, method: &str, path: &str, reads: bool, checked_by: &str) -> ToolDeclaration {
    ToolDeclaration {
        name: name.into(),
        title: name.replace('_', " "),
        description: format!("{name}, for the test."),
        method: method.into(),
        path: path.into(),
        levels: vec![AccessLevel::Write as i32],
        reads,
        input_schema: r#"{"type":"object","properties":{"account_id":{"type":"string"},"break_ids":{"type":"array","items":{"type":"string"}}}}"#.into(),
        output_schema: if reads {
            r#"{"type":"object"}"#.into()
        } else {
            String::new()
        },
        roles: vec![ROLE.into()],
        checked_by: checked_by.into(),
        accounts: vec!["account_id".into()],
        made_on: String::new(),
    }
}

/// What reached the stand-in plugin: the claims, the method and path.
type Reached = Arc<Mutex<Vec<(CallerClaims, String, String)>>>;

struct Harness {
    app: Arc<App>,
    reached: Reached,
}

async fn harness() -> Harness {
    let reached: Reached = Arc::default();
    let recorded = Arc::clone(&reached);
    let plugin = Router::new().fallback(move |request: axum::extract::Request| {
        let recorded = Arc::clone(&recorded);
        async move {
            let (parts, body) = request.into_parts();
            let body = String::from_utf8(to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap();
            let header = parts
                .headers
                .get("meridian-caller")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let assertion =
                CallerAssertion::decode(URL_SAFE_NO_PAD.decode(header).unwrap().as_slice()).unwrap();
            let claims = CallerClaims::decode(assertion.claims.as_slice()).unwrap();
            let path = parts.uri.path().to_string();
            recorded
                .lock()
                .unwrap()
                .push((claims, parts.method.to_string(), path.clone()));
            let answer = if path == "/check" && body.contains("not-open") {
                r#"{"outcome":"refused","reason":"invalid_argument","fields":[{"path":"break_ids[0]"}],"detail":"break_ids[0]: is not open"}"#
            } else if path == "/check" {
                r#"{"outcome":"unchanged","detail":"would close 1 break"}"#
            } else {
                r#"{"outcome":"made","data":{"closed":1}}"#
            };
            ([("content-type", "application/json")], answer)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plugin_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, plugin).await.unwrap() });

    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let bus = Arc::new(Bus::single(
        INSTANCE,
        Arc::new(MemoryBackend::new()),
        Arc::new(SystemClock),
    ));
    let contract = Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n",
        "name\tkind\noperations\trole\n",
    )
    .unwrap();
    let sidecar = Arc::new(Sidecar::under(
        &contract,
        bus,
        "dep-local-1",
        Identity::new(INSTANCE, vec![ROLE.into()]),
    ));
    let reply = sidecar
        .register(tonic::Request::new(RegisterRequest {
            schema_version: "v21".into(),
            interface: Some(InterfaceDeclaration {
                loopback_port: plugin_port.into(),
                title: "Operations".into(),
                pages: vec![],
            }),
            tools: vec![
                tool(
                    "close_cleared_breaks",
                    "POST",
                    "/close",
                    false,
                    "check_close_cleared_breaks",
                ),
                tool("check_close_cleared_breaks", "POST", "/check", true, ""),
                tool("close_books", "POST", "/books", false, ""),
            ],
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    let report = sidecar.report(SystemClock.now_ns());
    let door = front_door::router(
        FrontDoor::new(
            sidecar,
            Verifier::holding(INSTANCE, KEY_ID, key.verifying_key()),
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let door_at = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, door).await.unwrap() });
    let plugins = Plugins::new(
        &format!("https://{DASHBOARD}"),
        &format!("http://{{instance}}.sidecars.invalid:{}", door_at.port()),
        Signer::holding(KEY_ID, key),
    )
    .unwrap()
    .resolving("ops-1.sidecars.invalid", door_at);
    let cache = Arc::new(RecordsCache::default());
    cache.store(records(None), SystemClock.now_ns());
    let health: Arc<crate::health::Health> = Arc::default();
    health.hear(INSTANCE, report);
    let app = Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: cache,
        sessions: Arc::default(),
        delegations: Arc::new(crate::delegation::Delegations::default()),
        public_url: String::new(),
        clock: Arc::new(SystemClock),
        bus: Arc::new(Bus::single(
            "dashboard-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(SystemClock),
        )),
        oidc: None,
        directory: None,
        accounts: None,
        sign_in_failures: Default::default(),
        secure_cookies: true,
        plugins: Some(Arc::new(plugins)),
        registry: None,
        custody: Arc::default(),
        health,
        kit: None,
        bounds: Arc::default(),
        tickets: Arc::default(),
        prepared: Arc::default(),
        service_accounts: Arc::default(),
    });
    Harness { app, reached }
}

fn asked(name: &str, setting: Option<Setting>) -> Asked {
    Asked {
        name: name.into(),
        purpose: "Closes breaks whose difference has gone".into(),
        owner_subject: CAT.into(),
        setting,
    }
}

/// A service account created by the admin, granted `grant` on operations,
/// and chosen for it at write.
async fn running(h: &Harness, setting: Setting, grant: AccessLevel) -> String {
    let account = super::create(&h.app, &admin(), &asked("The sweep", Some(setting)), "")
        .await
        .unwrap();
    let id = account.service_account_id.clone();
    h.app
        .records
        .store(records(Some((&id, grant))), h.app.clock.now_ns());
    if grant == AccessLevel::Write {
        assert!(super::choose(
            &h.app,
            &admin(),
            true,
            INSTANCE,
            ROLE,
            Some((&id, AccessLevel::Write)),
            "the sweep runs as it"
        )
        .await
        .unwrap());
    }
    id
}

fn call(tool: &str, arguments: &str, reason: &str) -> CallAsServiceAccountRequest {
    CallAsServiceAccountRequest {
        algorithm_id: Default::default(),
        role: ROLE.into(),
        tool_name: tool.into(),
        arguments_json: arguments.into(),
        reason: reason.into(),
    }
}

const ARGUMENTS: &str = r#"{"account_id":"ACC-1","break_ids":["BRK-1"]}"#;
const REASON: &str = "The difference was gone at the statement of 2026-09-10";

#[test]
fn an_sa_id_is_sa_and_a_ulid() {
    assert!(super::is_service_account("SA-01JAD000000000000000000001"));
    for not in [
        "",
        "SA-",
        "local|cat",
        "SA-01jad000000000000000000001",
        "PC-01JAD000000000000000000001",
    ] {
        assert!(!super::is_service_account(not), "{not:?}");
    }
}

#[tokio::test]
async fn a_service_account_is_created_changed_and_stopped_each_change_its_own_record() {
    let h = harness().await;
    // Only a deployment admin creates one; its owner must be a person the
    // deployment knows; prepare only by default.
    assert!(
        super::create(&h.app, &person(CAT), &asked("Mine", None), "")
            .await
            .is_err()
    );
    let unknown = Asked {
        owner_subject: "local|nobody".into(),
        ..asked("The sweep", None)
    };
    let refused = super::create(&h.app, &admin(), &unknown, "")
        .await
        .unwrap_err();
    assert_eq!(refused.field, "owner_subject");
    let long = asked(&"x".repeat(201), None);
    assert_eq!(
        super::create(&h.app, &admin(), &long, "")
            .await
            .unwrap_err()
            .field,
        "name"
    );
    let account = super::create(&h.app, &admin(), &asked("The sweep", None), "for the sweep")
        .await
        .unwrap();
    let id = account.service_account_id.clone();
    assert!(super::is_service_account(&id), "{id}");
    assert_eq!(account.setting, Setting::PrepareOnly);
    assert_eq!(account.state, State::Active);

    // Two parts changed: two records.
    let (changed, count) = super::change_parts(
        &h.app,
        &admin(),
        &id,
        &Asked {
            name: "The nightly sweep".into(),
            setting: Some(Setting::Act),
            ..asked("", None)
        },
        "it acts now",
    )
    .await
    .unwrap();
    assert_eq!(count, 2);
    assert_eq!(changed.setting, Setting::Act);
    // Nothing changed is no record.
    let (_, none) = super::change_parts(
        &h.app,
        &admin(),
        &id,
        &Asked {
            name: "The nightly sweep".into(),
            setting: Some(Setting::Act),
            ..asked("", None)
        },
        "",
    )
    .await
    .unwrap();
    assert_eq!(none, 0);

    // The owner may disable it and nothing more.
    assert!(
        super::set_state(&h.app, &person(DAN), &id, State::Disabled, "")
            .await
            .is_err()
    );
    let (_, lapsed) = super::set_state(&h.app, &person(CAT), &id, State::Disabled, "stop")
        .await
        .unwrap();
    assert_eq!(lapsed, 0);
    let refused = super::set_state(&h.app, &person(CAT), &id, State::Active, "")
        .await
        .unwrap_err();
    assert!(refused.detail.contains("nothing more"), "{refused}");
    super::set_state(&h.app, &admin(), &id, State::Active, "back")
        .await
        .unwrap();
    super::set_state(&h.app, &admin(), &id, State::Removed, "gone")
        .await
        .unwrap();
    assert!(
        super::change_parts(&h.app, &admin(), &id, &asked("Again", None), "")
            .await
            .is_err()
    );

    let records = h.app.service_accounts.changes(&id).await.unwrap();
    let said: Vec<String> = records.iter().map(|c| c.to.clone()).collect();
    assert_eq!(said.len(), 6, "{said:?}");
    assert!(
        said[0].starts_with("created: name \"The sweep\""),
        "{said:?}"
    );
    assert_eq!(
        &said[1..],
        [
            "name: The nightly sweep",
            "setting: act",
            "state: disabled",
            "state: active",
            "state: removed"
        ]
    );
    assert_eq!(records[3].by, CAT, "the owner's disable names the owner");
    assert!(records.iter().all(|c| c.scope.is_empty()));
}

#[tokio::test]
async fn a_role_runs_as_one_granted_on_it_at_no_more_than_its_grant() {
    let h = harness().await;
    let account = super::create(&h.app, &admin(), &asked("The sweep", None), "")
        .await
        .unwrap();
    let id = account.service_account_id.clone();
    // Not granted on the role: refused, naming it.
    let refused = super::choose(
        &h.app,
        &admin(),
        true,
        INSTANCE,
        ROLE,
        Some((&id, AccessLevel::Write)),
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(refused.field, "service_account_id");
    // Granted read: at read, never write.
    h.app.records.store(
        records(Some((&id, AccessLevel::Read))),
        h.app.clock.now_ns(),
    );
    let refused = super::choose(
        &h.app,
        &admin(),
        true,
        INSTANCE,
        ROLE,
        Some((&id, AccessLevel::Write)),
        "",
    )
    .await
    .unwrap_err();
    assert_eq!(refused.field, "level");
    assert!(super::choose(
        &h.app,
        &admin(),
        true,
        INSTANCE,
        ROLE,
        Some((&id, AccessLevel::Read)),
        ""
    )
    .await
    .unwrap());
    // Only the role's admin chooses.
    assert!(
        super::choose(&h.app, &person(BEA), false, INSTANCE, ROLE, None, "")
            .await
            .is_err()
    );
    // Cleared, its record scoped to the role.
    assert!(
        super::choose(&h.app, &admin(), true, INSTANCE, ROLE, None, "none for now")
            .await
            .unwrap()
    );
    let changes = h.app.service_accounts.changes(&id).await.unwrap();
    let scoped: Vec<(String, String, String)> = changes
        .iter()
        .filter(|c| !c.scope.is_empty())
        .map(|c| (c.scope.clone(), c.from.clone(), c.to.clone()))
        .collect();
    assert_eq!(
        scoped,
        vec![
            (
                "ops-1:operations".into(),
                String::new(),
                format!("{id} at read")
            ),
            (
                "ops-1:operations".into(),
                format!("{id} at read"),
                String::new()
            ),
        ]
    );
}

#[tokio::test]
async fn a_role_with_none_chosen_or_a_tool_not_its_act_is_refused_naming_the_field() {
    let h = harness().await;
    let none = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (none.status, none.fields.clone()),
        ("failed_precondition", vec!["role".to_string()])
    );
    assert_eq!(
        none.words,
        "role: operations runs as no service account; its admin chooses one on the plugin's Access tab"
    );
    let _ = running(&h, Setting::Act, AccessLevel::Write).await;
    for tool in [
        "check_close_cleared_breaks",
        "dashboard__make_prepared",
        "nothing_like_it",
    ] {
        let refused = super::admission::admit(&h.app, INSTANCE, call(tool, ARGUMENTS, REASON))
            .await
            .unwrap_err();
        assert_eq!(
            (refused.status, refused.fields.clone()),
            ("invalid_argument", vec!["tool_name".to_string()]),
            "{tool}"
        );
    }
    let mut other = call("close_cleared_breaks", ARGUMENTS, REASON);
    other.role = "custody".into();
    let refused = super::admission::admit(&h.app, INSTANCE, other)
        .await
        .unwrap_err();
    assert_eq!(refused.fields, vec!["role".to_string()]);
    let reasonless = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, ""),
    )
    .await
    .unwrap_err();
    assert_eq!(reasonless.fields, vec!["reason".to_string()]);
    for arguments in ["[1]", "not json"] {
        let refused = super::admission::admit(
            &h.app,
            INSTANCE,
            call("close_cleared_breaks", arguments, REASON),
        )
        .await
        .unwrap_err();
        assert_eq!(
            (refused.status, refused.fields.clone()),
            ("invalid_argument", vec!["arguments_json".to_string()]),
            "{arguments}"
        );
        assert_eq!(
            refused.words,
            "arguments_json: is not a JSON object: the act's typed record of inputs, an empty \
             object when it takes none"
        );
    }
    assert!(
        h.reached.lock().unwrap().is_empty(),
        "nothing reached the plugin"
    );
}

#[tokio::test]
async fn set_to_act_at_may_make_it_is_made_with_claims_naming_the_account_and_its_owner() {
    let h = harness().await;
    let id = running(&h, Setting::Act, AccessLevel::Write).await;
    let reply = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap();
    assert_eq!(reply.outcome, "made");
    assert!(reply.prepared_call_id.is_empty());
    assert!(
        reply.answer_json.contains("\"closed\":1"),
        "{}",
        reply.answer_json
    );
    let reached = h.reached.lock().unwrap().clone();
    assert_eq!(reached.len(), 1);
    let (claims, method, path) = &reached[0];
    assert_eq!((method.as_str(), path.as_str()), ("POST", "/close"));
    assert_eq!(claims.subject, id);
    assert_eq!(claims.subject_kind, SubjectKind::ServiceAccount as i32);
    assert_eq!(claims.owner_subject, CAT);
    assert_eq!(claims.tool_name, "close_cleared_breaks");
    assert_eq!(claims.level, AccessLevel::Write as i32);
    assert!(!claims.deployment_admin, "whatever it is granted, no admin");
    assert!(claims.delegation_id.is_empty() && claims.client_name.is_empty());
    assert_eq!(claims.write_account_ids, vec!["ACC-1".to_string()]);
    assert_eq!(claims.roles.len(), 1);
    assert_eq!(claims.roles[0].role, ROLE);
    assert!(claims.expires_at_ns - claims.issued_at_ns <= 60_000_000_000);

    // The call record names the account, its last use on its row.
    let calls = h
        .app
        .delegations
        .calls(crate::delegation::CallsOf::Person(id.clone()), 10)
        .await
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].tool.as_str(), calls[0].outcome.as_str()),
        ("ops-1__close_cleared_breaks", "made")
    );
    assert!(calls[0].delegation_id.is_empty());
    let account = h.app.service_accounts.account(&id).await.unwrap().unwrap();
    assert!(account.last_used_ns.is_some());
    assert!(account.last_refusal.is_empty());
}

#[tokio::test]
async fn set_to_prepare_only_it_prepares_by_the_check_tool_for_everyone_who_may_make_it_and_its_owner(
) {
    let h = harness().await;
    let id = running(&h, Setting::PrepareOnly, AccessLevel::Write).await;
    let reply = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap();
    assert_eq!(reply.outcome, "prepared");
    assert!(crate::prepared::is_prepared_call_id(
        &reply.prepared_call_id
    ));
    // Only the check reached the plugin: nothing in the deployment changed.
    let reached = h.reached.lock().unwrap().clone();
    assert_eq!(reached.len(), 1);
    assert_eq!(reached[0].2, "/check");
    assert_eq!(reached[0].0.tool_name, "check_close_cleared_breaks");

    let kept = h
        .app
        .prepared
        .get(&reply.prepared_call_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.preparer_subject, id);
    assert_eq!(
        kept.preparer_name,
        "The sweep, a service account owned by Cat Ruiz"
    );
    assert!(kept.through_delegation.is_empty());
    assert_eq!(kept.accounts, vec!["ACC-1".to_string()]);
    // Shown to everyone who may make it at once -- Bea, who writes on
    // operations -- and to its owner always; never to Dan.
    for (subject, expected) in [(BEA, true), (CAT, true), (DAN, false)] {
        let shown = crate::prepared::waiting_for(&h.app, subject)
            .await
            .iter()
            .any(|k| k.prepared_call_id == kept.prepared_call_id);
        assert_eq!(shown, expected, "{subject}");
    }
    // Its owner reads its contents only within her own read access: Cat
    // reads no account here, Bea reads ACC-1.
    let records_now = h.app.records.current(h.app.clock.now_ns()).unwrap();
    assert!(crate::prepared::contents_withheld(
        &h.app,
        &records_now,
        &kept,
        CAT
    ));
    assert!(!crate::prepared::contents_withheld(
        &h.app,
        &records_now,
        &kept,
        BEA
    ));
    let calls = h
        .app
        .delegations
        .calls(crate::delegation::CallsOf::Person(id.clone()), 10)
        .await
        .unwrap();
    assert_eq!(calls[0].outcome, "prepared");

    // The check's refusal is answered by path, nothing kept.
    let refused = super::admission::admit(
        &h.app,
        INSTANCE,
        call(
            "close_cleared_breaks",
            r#"{"account_id":"ACC-1","break_ids":["not-open"]}"#,
            REASON,
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (refused.status, refused.fields.clone()),
        ("failed_precondition", vec!["break_ids[0]".to_string()])
    );
    let account = h.app.service_accounts.account(&id).await.unwrap().unwrap();
    assert!(
        account.last_refusal.contains("break_ids[0]"),
        "{}",
        account.last_refusal
    );
}

#[tokio::test]
async fn the_strictest_level_binds_it_and_under_the_gate_its_owner_never_makes_what_it_prepared() {
    let h = harness().await;
    let id = running(&h, Setting::Act, AccessLevel::Write).await;
    let act = "ops-1__close_cleared_breaks";
    let by = crate::prepared::person_caller(ADMIN, "Harness", &[]);
    let now = h.app.clock.now_ns();

    // At prepare only, by the plugin admin's level: prepared, set to act or not.
    h.app
        .prepared
        .set_level(act, "act", Some((Level::PrepareOnly, false)), &by, "", now)
        .await
        .unwrap();
    let reply = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap();
    assert_eq!(reply.outcome, "prepared");

    // At may make with the gate on: prepared; its owner may not make it,
    // Bea may.
    h.app
        .prepared
        .set_level(act, "act", Some((Level::MayMake, true)), &by, "", now)
        .await
        .unwrap();
    let gated = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap();
    assert_eq!(gated.outcome, "prepared");
    let kept = h
        .app
        .prepared
        .get(&gated.prepared_call_id)
        .await
        .unwrap()
        .unwrap();
    // Cat writes on operations too, so the gate is all that stands between
    // her and making what her account prepared.
    let mut both = records(Some((&id, AccessLevel::Write)));
    both.user_groups[0].logins.push(CAT.into());
    h.app.records.store(both, now);
    let records = h.app.records.current(now).unwrap();
    let held = crate::prepared::resolve_kept(&h.app, &kept, &records)
        .await
        .unwrap();
    let eyes = h.app.prepared.eyes(&h.app, &held, &kept).await;
    assert!(eyes);
    let owner =
        crate::prepared::Prepared::may_make(&h.app, &records, &held, &kept, eyes, CAT).unwrap_err();
    assert!(owner.contains("its owner"), "{owner}");
    assert!(crate::prepared::Prepared::may_make(&h.app, &records, &held, &kept, eyes, BEA).is_ok());

    // With the gate off, its owner may, as anyone who may make the act.
    h.app
        .prepared
        .set_level(act, "act", Some((Level::MayMake, false)), &by, "", now)
        .await
        .unwrap();
    let eyes = h.app.prepared.eyes(&h.app, &held, &kept).await;
    assert!(!eyes);
    assert!(crate::prepared::Prepared::may_make(&h.app, &records, &held, &kept, eyes, CAT).is_ok());

    // At may not, by an access group holding the account alone: refused by
    // name, nothing kept.
    h.app
        .prepared
        .set_level(
            act,
            "group:AG-write",
            Some((Level::MayNot, false)),
            &by,
            "",
            now,
        )
        .await
        .unwrap();
    let refused = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (refused.status, refused.fields.clone()),
        ("permission_denied", vec!["tool_name".to_string()])
    );
    let account = h.app.service_accounts.account(&id).await.unwrap().unwrap();
    assert!(
        account.last_refusal.contains("may not"),
        "{}",
        account.last_refusal
    );
}

#[tokio::test]
async fn an_act_with_no_check_tool_is_made_set_to_act_and_refused_set_to_prepare_only() {
    let h = harness().await;
    let id = running(&h, Setting::Act, AccessLevel::Write).await;
    let reply = super::admission::admit(&h.app, INSTANCE, call("close_books", "{}", REASON))
        .await
        .unwrap();
    assert_eq!(reply.outcome, "made");
    super::change_parts(
        &h.app,
        &admin(),
        &id,
        &Asked {
            name: "The sweep".into(),
            setting: Some(Setting::PrepareOnly),
            ..asked("", None)
        },
        "",
    )
    .await
    .unwrap();
    let refused = super::admission::admit(&h.app, INSTANCE, call("close_books", "{}", REASON))
        .await
        .unwrap_err();
    assert_eq!(
        (refused.status, refused.fields.clone()),
        ("failed_precondition", vec!["tool_name".to_string()])
    );
    assert!(
        refused.words.contains("cannot be prepared"),
        "{}",
        refused.words
    );
}

#[tokio::test]
async fn disabled_its_next_call_is_refused_and_its_waiting_changes_lapse_its_owner_told() {
    let h = harness().await;
    let id = running(&h, Setting::PrepareOnly, AccessLevel::Write).await;
    let prepared = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap()
    .prepared_call_id;
    let (_, lapsed) = super::set_state(&h.app, &admin(), &id, State::Disabled, "paused")
        .await
        .unwrap();
    assert_eq!(lapsed, 1);
    let kept = h.app.prepared.get(&prepared).await.unwrap().unwrap();
    assert_eq!(kept.state, PreparedCallState::Lapsed as i32);
    let steps = h.app.prepared.steps(&prepared).await.unwrap();
    assert_eq!(
        steps.last().unwrap().note,
        "the service account that prepared it, The sweep, was disabled"
    );
    let told: Vec<(String, String)> = h
        .app
        .service_accounts
        .notices(CAT)
        .await
        .unwrap()
        .into_iter()
        .map(|n| (n.kind, n.prepared_call_id))
        .collect();
    assert_eq!(told.len(), 2, "{told:?}");
    assert!(told.contains(&("disabled".to_string(), String::new())));
    assert!(told.contains(&("lapsed".to_string(), prepared.clone())));

    let refused = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (refused.status, refused.fields.clone()),
        ("permission_denied", vec!["role".to_string()])
    );
    assert!(refused.words.contains("disabled"), "{}", refused.words);
}

#[tokio::test]
async fn its_grant_withdrawn_or_run_at_read_its_calls_are_refused_the_choice_standing() {
    let h = harness().await;
    let id = running(&h, Setting::Act, AccessLevel::Write).await;
    h.app.records.store(
        records(Some((&id, AccessLevel::Read))),
        h.app.clock.now_ns(),
    );
    let refused = super::admission::admit(
        &h.app,
        INSTANCE,
        call("close_cleared_breaks", ARGUMENTS, REASON),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (refused.status, refused.fields.clone()),
        ("permission_denied", vec!["role".to_string()])
    );
    // The choice stands, shown so on the tab.
    let chosen = h.app.service_accounts.runs_as().await.unwrap();
    assert_eq!(chosen.len(), 1);
    let records_now = h.app.records.current(h.app.clock.now_ns()).unwrap();
    let part =
        super::pages::runs_as_part(&h.app, &records_now, INSTANCE, &chosen, &|_| true, "<t>");
    assert!(part.contains("Granted read"), "{part}");
}

#[tokio::test]
async fn the_owner_reads_their_own_rows_and_a_deployment_admin_every_one() {
    let h = harness().await;
    let mine = super::create(&h.app, &admin(), &asked("Cat's sweep", None), "")
        .await
        .unwrap();
    let theirs = super::create(
        &h.app,
        &admin(),
        &Asked {
            owner_subject: BEA.into(),
            ..asked("Bea's sweep", None)
        },
        "",
    )
    .await
    .unwrap();
    let caller = |subject: &str| crate::mcp::Caller {
        subject: subject.into(),
        display_name: subject.into(),
        directory_groups: vec![],
        delegation_id: "DLG-1".into(),
        client_name: "Claude".into(),
        covers: crate::delegation::Covers::everything(),
        owner_subject: String::new(),
    };
    let spec = &crate::mcp::service_accounts::SPECS[0];
    let (said, _) =
        crate::mcp::service_accounts::call(&h.app, &caller(CAT), spec, serde_json::json!({})).await;
    let ids: Vec<&str> = said["data"]["service_accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["service_account_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![mine.service_account_id.as_str()]);
    // The harness's admin holds no deployment admin's permission in these
    // records; given one, every row.
    let mut with_admin = records(None);
    with_admin.user_groups.push(UserGroup {
        user_group_id: "UG-admin".into(),
        name: "Admins".into(),
        directory_groups: vec![],
        logins: vec![ADMIN.into()],
    });
    with_admin.permissions.push(Permission {
        permission_id: "P-admin".into(),
        user_group_id: "UG-admin".into(),
        account_group_id: String::new(),
        access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
    });
    h.app.records.store(with_admin, h.app.clock.now_ns());
    let (said, level) = crate::mcp::service_accounts::call(
        &h.app,
        &caller(ADMIN),
        spec,
        serde_json::json!({"limit": 1}),
    )
    .await;
    assert_eq!(level, "deployment admin");
    assert_eq!(
        said["data"]["service_accounts"].as_array().unwrap().len(),
        1
    );
    assert_eq!(said["data"]["next_cursor"], "1");
    let (said, _) = crate::mcp::service_accounts::call(
        &h.app,
        &caller(ADMIN),
        spec,
        serde_json::json!({"cursor": "1"}),
    )
    .await;
    assert_eq!(
        said["data"]["service_accounts"][0]["service_account_id"],
        theirs.service_account_id.as_str()
    );
    assert_eq!(
        said["data"]["service_accounts"][0]["setting"],
        "prepare_only"
    );
}

#[test]
fn every_service_account_page_names_its_tool_or_its_exception() {
    fn routes(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = source;
        while let Some(at) = rest.find(".route(") {
            rest = &rest[at + ".route(".len()..];
            if let Some(quoted) = rest.trim_start().strip_prefix('"') {
                if let Some(end) = quoted.find('"') {
                    found.push(quoted[..end].to_string());
                }
            }
        }
        found
    }
    let tools: Vec<&str> = crate::mcp::service_accounts::SPECS
        .iter()
        .chain(crate::mcp::plugin_area::SPECS)
        .map(|spec| spec.name)
        .collect();
    let drawn = routes(include_str!("pages.rs"));
    assert_eq!(drawn.len(), 5, "{drawn:?}");
    for path in drawn
        .iter()
        .map(String::as_str)
        .chain(["tab service-accounts", "view runs-as"])
    {
        let (_, by) = super::pages::PARITY
            .iter()
            .find(|(key, _)| *key == path)
            .unwrap_or_else(|| panic!("{path} is drawn and names no tool, nor a ruled exception"));
        assert!(
            by.split([' ', ';']).any(|word| tools.contains(&word)),
            "{path}: {by}"
        );
    }
}
