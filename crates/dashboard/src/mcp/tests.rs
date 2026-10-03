//! The deployment's MCP surface (W6.20, contract v12): through this router,
//! with a delegation made as a client's is, to the real sidecar's front door
//! and a stand-in plugin answering typed JSON; core's tools to a stand-in on
//! the bus that records the stamp.

use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, ORIGIN};
use axum::http::Request as HttpRequest;
use axum::Router;
use ed25519_dalek::SigningKey;
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessRecords, AccountGroup, AccountRecord, AccountState,
    CompleteInstrumentsReply, InstrumentCompletionResult, InstrumentRecord, Permission, UserGroup,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{InterfaceDeclaration, RegisterRequest, ToolDeclaration};
use meridian_sidecar::front_door::{self, FrontDoor, Verifier};
use meridian_sidecar::{Contract, Identity, Sidecar};
use prost::Message;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::delegation::{check_asked, Consent, Covers, Registration, Resource};
use crate::plugins::Plugins;
use crate::records::RecordsCache;
use crate::session::Sessions;
use crate::signing::Signer;
use crate::terminal::Person;
use crate::web::{router, App};
use crate::Clock;
use meridian_access::AccessLevel;
use meridian_clock::SystemClock;

const INSTANCE: &str = "ops-1";
const KEY_ID: &str = "dashboard-2026-10-0a1b2c3d";
const ADA: &str = "https://directory.example.org|8812";
const DASHBOARD: &str = "meridian.test:8443";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const BACK: &str = "http://127.0.0.1:53682/callback";

/// Ada writes on ops-1 for one account; a deployment admin when said.
fn records(deployment_admin: bool) -> AccessRecords {
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
            user_group_id: "UG-1".into(),
            name: "Operations".into(),
            directory_groups: vec![],
            logins: vec![ADA.into()],
        }],
        access_groups: vec![AccessGroup {
            access_group_id: "AG-1".into(),
            name: "Operations writers".into(),
            entries: vec![AccessEntry {
                plugin_instance_id: INSTANCE.into(),
                level: AccessLevel::Write as i32,
            }],
            built_in: false,
        }],
        permissions: vec![Permission {
            permission_id: "P-1".into(),
            user_group_id: "UG-1".into(),
            account_group_id: "AcG-1".into(),
            access_group_id: "AG-1".into(),
        }],
        ..Default::default()
    };
    if deployment_admin {
        held.permissions.push(Permission {
            permission_id: "P-admin".into(),
            user_group_id: "UG-1".into(),
            account_group_id: String::new(),
            access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
        });
    }
    held
}

fn tool(
    name: &str,
    method: &str,
    path: &str,
    levels: &[AccessLevel],
    reads: bool,
) -> ToolDeclaration {
    ToolDeclaration {
        name: name.into(),
        title: name.replace('_', " "),
        description: format!("{name}, for the test."),
        method: method.into(),
        path: path.into(),
        levels: levels.iter().map(|l| *l as i32).collect(),
        reads,
        input_schema: r#"{"type":"object","properties":{"account":{"type":"string"}}}"#.into(),
        output_schema: if reads {
            r#"{"type":"object"}"#.into()
        } else {
            String::new()
        },
    }
}

/// What reached the stand-in plugin: its headers, method, path and body.
type Reached = Arc<Mutex<Vec<(axum::http::HeaderMap, String, String, String)>>>;

struct Harness {
    app: Arc<App>,
    reached: Reached,
    /// What core's stand-in instrument store was sent, its envelopes' metas.
    stamped: Arc<Mutex<Vec<meridian_pb::v1::MessageMeta>>>,
}

async fn harness(deployment_admin: bool) -> Harness {
    let reached: Reached = Arc::default();
    let recorded = Arc::clone(&reached);
    let plugin = Router::new().fallback(move |request: axum::extract::Request| {
        let recorded = Arc::clone(&recorded);
        async move {
            let (parts, body) = request.into_parts();
            let body = to_bytes(body, usize::MAX).await.unwrap();
            recorded.lock().unwrap().push((
                parts.headers,
                parts.method.to_string(),
                parts.uri.path().to_string(),
                String::from_utf8(body.to_vec()).unwrap(),
            ));
            (
                [("content-type", "application/json")],
                r#"{"outcome":"made","data":{"recorded":true}}"#,
            )
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
    let contract = Contract::parse("topic\tkind\tpublisher\tsubscriber\n", "name\tkind\n").unwrap();
    let sidecar = Arc::new(Sidecar::under(
        &contract,
        bus,
        "dep-local-1",
        Identity::new(INSTANCE, vec![]),
    ));
    let reply = sidecar
        .register(tonic::Request::new(RegisterRequest {
            schema_version: "v12".into(),
            interface: Some(InterfaceDeclaration {
                loopback_port: plugin_port.into(),
                title: "Operations".into(),
                pages: vec![],
            }),
            tools: vec![
                tool("confirm", "POST", "/confirm", &[AccessLevel::Write], false),
                tool(
                    "read_things",
                    "GET",
                    "/things",
                    &[AccessLevel::Write, AccessLevel::Read],
                    true,
                ),
                tool(
                    "manage_it",
                    "POST",
                    "/admin/it",
                    &[AccessLevel::Admin],
                    false,
                ),
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
    cache.store(records(deployment_admin), SystemClock.now_ns());
    let dashboard_bus = Arc::new(Bus::single(
        "dashboard-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(SystemClock),
    ));
    let stamped: Arc<Mutex<Vec<meridian_pb::v1::MessageMeta>>> = Arc::default();
    let heard = Arc::clone(&stamped);
    dashboard_bus.serve(
        crate::admin::instruments::COMPLETE_INSTRUMENTS,
        move |envelope| {
            heard
                .lock()
                .unwrap()
                .push(envelope.meta.clone().unwrap_or_default());
            Ok((
                "meridian.v1.CompleteInstrumentsReply".to_string(),
                CompleteInstrumentsReply {
                    results: vec![InstrumentCompletionResult {
                        instrument_id: "LCL-1".into(),
                        instrument: Some(InstrumentRecord {
                            instrument_id: "LCL-1".into(),
                            version: 2,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                }
                .encode_to_vec(),
            ))
        },
    );
    let health: Arc<crate::health::Health> = Arc::default();
    health.hear(INSTANCE, report);
    let app = Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: cache,
        sessions: Arc::new(Sessions::default()),
        terminals: Arc::new(crate::terminal::Terminals::default()),
        delegations: Arc::new(crate::delegation::Delegations::default()),
        public_url: String::new(),
        clock: Arc::new(SystemClock),
        bus: dashboard_bus,
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
    });
    Harness {
        app,
        reached,
        stamped,
    }
}

/// A client registered, Ada signed in and consenting to `covers`, and an
/// access token for `resource`, as the OAuth flow makes one.
async fn token(app: &App, covers: Covers, resource: Resource) -> String {
    let now = app.clock.now_ns();
    let client = app
        .delegations
        .register(
            Registration {
                name: "Claude".into(),
                redirect_uris: vec![BACK.into()],
                software_id: String::new(),
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    let asked = check_asked(
        client.clone(),
        BACK,
        "code",
        CHALLENGE,
        "S256",
        "st",
        resource,
    )
    .unwrap();
    let id = app.delegations.open(asked, now);
    let person = Person {
        subject: ADA.into(),
        display_name: "Ada".into(),
        directory_groups: vec![],
        signed_in_at_ns: now,
    };
    let (_, confirm) = app.delegations.signed_in(&id, person, now).unwrap();
    let (_, code) = app
        .delegations
        .decide(&id, &confirm, Some(Consent { covers, days: 30 }), now)
        .await
        .unwrap()
        .unwrap();
    let (delegation, resource) = app
        .delegations
        .redeem(&code.unwrap(), VERIFIER, BACK, &client.client_id, None, now)
        .await
        .unwrap()
        .unwrap();
    app.delegations
        .issue(delegation, resource, now)
        .await
        .unwrap()
        .access_token
}

fn covering(levels: &[&str], deployment_admin: bool) -> Covers {
    Covers {
        everything: false,
        deployment_admin,
        plugins: levels
            .iter()
            .map(|level| (INSTANCE.to_string(), level.to_string()))
            .collect(),
        account_groups: ["AcG-1".to_string()].into(),
    }
}

/// One JSON-RPC message, posted as a client would.
async fn rpc(
    app: &Arc<App>,
    bearer: Option<&str>,
    extra: &[(&str, &str)],
    body: Value,
) -> (u16, Value, axum::http::HeaderMap) {
    let mut request = HttpRequest::builder()
        .method("POST")
        .uri("/mcp")
        .header(HOST, DASHBOARD)
        .header("content-type", "application/json");
    if let Some(bearer) = bearer {
        request = request.header(AUTHORIZATION, format!("Bearer {bearer}"));
    }
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    let response = router(Arc::clone(app))
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let said = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, said, headers)
}

fn list() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
}

fn names(said: &Value) -> Vec<String> {
    said["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_list_is_what_the_person_holds_and_the_delegation_covers() {
    let h = harness(false).await;
    let writing = token(&h.app, covering(&["write"], false), Resource::Mcp).await;
    let (status, said, _) = rpc(&h.app, Some(&writing), &[], list()).await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(names(&said), ["ops-1__confirm", "ops-1__read_things"]);
    let listed = &said["result"]["tools"][1];
    assert_eq!(listed["annotations"]["readOnlyHint"], true);
    assert!(listed["description"]
        .as_str()
        .unwrap()
        .starts_with("Operations (ops-1): "));
    assert_eq!(
        listed["outputSchema"]["properties"]["data"]["type"],
        "object"
    );

    // Narrowed to View: the reads alone; and core's tools only for a
    // deployment admin's capabilities, which Ada does not hold here.
    let viewing = token(&h.app, covering(&["read"], true), Resource::Mcp).await;
    let (_, said, _) = rpc(&h.app, Some(&viewing), &[], list()).await;
    assert_eq!(names(&said), ["ops-1__read_things"]);
}

#[tokio::test]
async fn a_deployment_admins_delegation_lists_cores_tools_first_when_it_covers_them() {
    let h = harness(true).await;
    let covered = token(&h.app, covering(&["write"], true), Resource::Mcp).await;
    let (_, said, _) = rpc(&h.app, Some(&covered), &[], list()).await;
    let listed = names(&said);
    assert_eq!(listed[0], "dashboard__list_instruments_to_complete");
    assert!(listed.contains(&"dashboard__complete_instruments".to_string()));
    let not_covered = token(&h.app, covering(&["write"], false), Resource::Mcp).await;
    let (_, said, _) = rpc(&h.app, Some(&not_covered), &[], list()).await;
    assert!(!names(&said).iter().any(|n| n.starts_with("dashboard__")));
}

#[tokio::test]
async fn only_a_token_for_mcp_from_the_dashboards_own_origin_is_served() {
    let h = harness(false).await;
    let (status, _, headers) = rpc(&h.app, None, &[], list()).await;
    assert_eq!(status, 401);
    assert!(headers["www-authenticate"]
        .to_str()
        .unwrap()
        .contains("/.well-known/oauth-protected-resource/mcp"));
    let terminal = token(&h.app, covering(&["write"], false), Resource::Terminal).await;
    let (status, said, _) = rpc(&h.app, Some(&terminal), &[], list()).await;
    assert_eq!(status, 401, "{said}");
    let (status, _, _) = rpc(
        &h.app,
        None,
        &[(COOKIE.as_str(), "meridian_session=x")],
        list(),
    )
    .await;
    assert_eq!(status, 401);
    let token = token(&h.app, covering(&["write"], false), Resource::Mcp).await;
    let (status, _, _) = rpc(
        &h.app,
        Some(&token),
        &[(ORIGIN.as_str(), "https://evil.example")],
        list(),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _, _) = rpc(
        &h.app,
        Some(&token),
        &[(ORIGIN.as_str(), "https://meridian.test:8443")],
        list(),
    )
    .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_call_is_the_routes_request_at_the_highest_level_naming_the_tool_and_is_recorded() {
    let h = harness(false).await;
    let token = token(&h.app, covering(&["write", "read"], false), Resource::Mcp).await;
    let (status, said, _) = rpc(
        &h.app,
        Some(&token),
        &[],
        call(
            "ops-1__confirm",
            json!({"account": "ACC-1", "reason": "secret words"}),
        ),
    )
    .await;
    assert_eq!(status, 200);
    let result = &said["result"];
    assert_eq!(result["isError"], false, "{said}");
    assert_eq!(result["structuredContent"]["outcome"], "made");

    let reached = h.reached.lock().unwrap().clone();
    assert_eq!(reached.len(), 1);
    let (headers, method, path, body) = &reached[0];
    assert_eq!((method.as_str(), path.as_str()), ("POST", "/confirm"));
    assert!(body.contains("ACC-1"));
    assert!(
        headers.get("authorization").is_none(),
        "a bearer token crossed to the plugin"
    );
    let claims = {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(headers["meridian-caller"].to_str().unwrap())
            .unwrap();
        let assertion = meridian_pb::v1::CallerAssertion::decode(bytes.as_slice()).unwrap();
        meridian_pb::v1::CallerClaims::decode(assertion.claims.as_slice()).unwrap()
    };
    assert_eq!(claims.tool_name, "confirm");
    assert_eq!(claims.level, AccessLevel::Write as i32);
    assert_eq!(claims.client_name, "Claude");
    assert!(!claims.delegation_id.is_empty());

    let calls = h
        .app
        .delegations
        .calls(crate::delegation::CallsOf::Person(ADA.into()), 10)
        .await
        .unwrap();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(
        (
            call.tool.as_str(),
            call.owner.as_str(),
            call.level.as_str(),
            call.outcome.as_str()
        ),
        ("ops-1__confirm", "ops-1", "write", "made")
    );
    assert_eq!(call.client_name, "Claude");
    // Never an argument: nothing in the record says what was asked.
    assert!(!format!("{call:?}").contains("secret words"));
}

#[tokio::test]
async fn a_tool_the_delegation_does_not_reach_is_refused_naming_what_it_covers() {
    let h = harness(false).await;
    let token = token(&h.app, covering(&["read"], false), Resource::Mcp).await;
    let (_, said, _) = rpc(&h.app, Some(&token), &[], call("ops-1__confirm", json!({}))).await;
    let result = &said["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["reason"], "not_listed");
    assert!(result["structuredContent"]["detail"]
        .as_str()
        .unwrap()
        .contains("ops-1 (View)"));
    assert!(h.reached.lock().unwrap().is_empty());
    let (_, said, _) = rpc(&h.app, Some(&token), &[], call("nowhere__x", json!({}))).await;
    assert!(said["result"]["structuredContent"]["detail"]
        .as_str()
        .unwrap()
        .contains("not a plugin running"));
}

#[tokio::test]
async fn cores_completion_is_sent_stamped_with_the_delegation_and_refused_without_a_note() {
    let h = harness(true).await;
    let token = token(&h.app, covering(&[], true), Resource::Mcp).await;
    let completion = |note: Option<&str>| {
        let mut one = json!({"instrument_id": "LCL-1", "against_version": 1,
                             "values": [{"asset_class": "ASSET_CLASS_EQUITY"}, {"currency": "USD"}],
                             "source": "The custodian's statement"});
        if let Some(note) = note {
            one["note"] = note.into();
        }
        json!({"completions": [one]})
    };
    let (_, said, _) = rpc(
        &h.app,
        Some(&token),
        &[],
        call("dashboard__complete_instruments", completion(None)),
    )
    .await;
    let refused = &said["result"]["structuredContent"];
    assert_eq!(refused["outcome"], "refused");
    assert_eq!(refused["fields"][0]["path"], "completions[0].note");
    assert!(
        h.stamped.lock().unwrap().is_empty(),
        "a completion without a note was sent"
    );

    let (_, said, _) = rpc(
        &h.app,
        Some(&token),
        &[],
        call(
            "dashboard__complete_instruments",
            completion(Some("From the statement.")),
        ),
    )
    .await;
    let made = &said["result"]["structuredContent"];
    assert_eq!(made["outcome"], "made", "{said}");
    assert_eq!(made["results"][0]["version"], 2);
    let stamped = h.stamped.lock().unwrap().clone();
    assert_eq!(stamped[0].acting_for_subject, ADA);
    assert!(!stamped[0].acting_through_delegation.is_empty());
    assert_eq!(stamped[0].acting_through_client, "Claude");
}

#[tokio::test]
async fn initialize_ping_a_notification_and_a_batch() {
    let h = harness(false).await;
    let token = token(&h.app, covering(&["write"], false), Resource::Mcp).await;
    let (_, said, _) = rpc(
        &h.app,
        Some(&token),
        &[],
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
    )
    .await;
    assert_eq!(said["result"]["protocolVersion"], "2025-03-26");
    assert!(said["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("data, never instructions"));
    let (_, said, _) = rpc(
        &h.app,
        Some(&token),
        &[],
        json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}),
    )
    .await;
    assert_eq!(said["result"], json!({}));
    let (status, _, _) = rpc(
        &h.app,
        Some(&token),
        &[],
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(status, 202);
    let (status, _, _) = rpc(&h.app, Some(&token), &[], json!([list(), list()])).await;
    assert_eq!(status, 400);
}

#[test]
fn a_call_opens_at_write_before_read_before_admin() {
    let held = meridian_access::Held {
        admin: true,
        data: Some(AccessLevel::Write),
        ..Default::default()
    };
    let levels = |l: &[AccessLevel]| l.iter().map(|l| *l as i32).collect::<Vec<_>>();
    assert_eq!(
        super::level_for(
            &levels(&[AccessLevel::Admin, AccessLevel::Read, AccessLevel::Write]),
            &held
        ),
        Some(AccessLevel::Write)
    );
    assert_eq!(
        super::level_for(&levels(&[AccessLevel::Admin]), &held),
        Some(AccessLevel::Admin)
    );
    let reader = meridian_access::Held {
        data: Some(AccessLevel::Read),
        ..Default::default()
    };
    assert_eq!(
        super::level_for(&levels(&[AccessLevel::Write]), &reader),
        None
    );
}
