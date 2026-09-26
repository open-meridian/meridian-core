//! A person reaching a plugin's page: through this router, to the real
//! sidecar's front door, to a stand-in plugin that records what reached it.

use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, LOCATION};
use axum::http::Request as HttpRequest;
use axum::Router;
use ed25519_dalek::SigningKey;
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessLevel, AccessRecords, AccountGroup, AccountRecord,
    AccountState, Permission, UserGroup,
};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{CallerAssertion, InterfaceDeclaration, RegisterRequest};
use meridian_sidecar::front_door::{self, FrontDoor, Verifier};
use meridian_sidecar::{Contract, Identity, Sidecar};
use tower::ServiceExt;

use super::*;
use crate::records::RecordsCache;
use crate::web::router;
use crate::{Clock, SystemClock};

const INSTANCE: &str = "snaptrade-1";
const KEY_ID: &str = "dashboard-2026-09-0a1b2c3d";
const ADA: &str = "https://directory.example.org|8812";
const DASHBOARD: &str = "meridian.test:8443";
const PLUGIN_HOST: &str = "snaptrade-1.plugins.meridian.test:8443";

/// Ada reads `custody` on each instance named, for one account.
fn records(instances: &[&str]) -> AccessRecords {
    AccessRecords {
        accounts: vec![AccountRecord {
            account_id: "ACC-1".into(),
            name: "Growth".into(),
            state: AccountState::Open as i32,
            created_at_ns: 0,
        }],
        account_groups: vec![AccountGroup {
            account_group_id: "AcG-1".into(),
            name: "Growth".into(),
            account_ids: vec!["ACC-1".into()],
        }],
        user_groups: vec![UserGroup {
            user_group_id: "UG-1".into(),
            name: "Operations".into(),
            directory_groups: vec![],
            logins: vec![ADA.into()],
        }],
        access_groups: vec![AccessGroup {
            access_group_id: "AG-1".into(),
            name: "Custody readers".into(),
            entries: instances
                .iter()
                .map(|instance| AccessEntry {
                    plugin_instance_id: instance.to_string(),
                    tag: "custody".into(),
                    level: AccessLevel::Read as i32,
                })
                .collect(),
            built_in: false,
        }],
        permissions: vec![Permission {
            permission_id: "P-1".into(),
            user_group_id: "UG-1".into(),
            account_group_id: "AcG-1".into(),
            access_group_id: "AG-1".into(),
        }],
        ..Default::default()
    }
}

/// What reached the stand-in plugin, one entry per request.
type Reached = Arc<Mutex<Vec<(HeaderMap, String)>>>;

struct Harness {
    app: Arc<App>,
    reached: Reached,
    /// Ada's dashboard session.
    session: String,
}

/// A plugin behind its real sidecar and front door on loopback, a dashboard
/// holding the key the sidecar trusts, and Ada signed in to it.
async fn harness(instances: &[&str]) -> Harness {
    let reached: Reached = Arc::default();
    let recorded = Arc::clone(&reached);
    let plugin = Router::new().fallback(move |request: axum::extract::Request| {
        let recorded = Arc::clone(&recorded);
        async move {
            recorded
                .lock()
                .unwrap()
                .push((request.headers().clone(), request.uri().to_string()));
            (
                [
                    (
                        "set-cookie",
                        "meridian_session=chosen-by-the-plugin; Path=/",
                    ),
                    ("set-cookie", "wide=1; Domain=meridian.test; Path=/"),
                    ("set-cookie", "own=1; Path=/; HttpOnly"),
                    ("x-plugin", "holdings"),
                ],
                "the page",
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plugin_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, plugin).await.unwrap() });

    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let bus = Arc::new(Bus::single(INSTANCE, Arc::new(MemoryBackend::new())));
    let contract = Contract::parse("topic\tkind\tpublisher\tsubscriber\n", "name\tkind\n").unwrap();
    let sidecar = Arc::new(Sidecar::under(
        &contract,
        bus,
        "dep-local-1",
        Identity::new(INSTANCE, vec![]),
    ));
    let reply = sidecar
        .register(tonic::Request::new(RegisterRequest {
            schema_version: "v2".into(),
            interface: Some(InterfaceDeclaration {
                loopback_port: plugin_port.into(),
                title: "Holdings".into(),
            }),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
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

    let (app, session) = dashboard(instances, door_at, key);
    Harness {
        app,
        reached,
        session,
    }
}

/// A dashboard holding `key`, whose sidecar for snaptrade-1 is at `door_at`,
/// with Ada signed in to it.
fn dashboard(
    instances: &[&str],
    door_at: std::net::SocketAddr,
    key: SigningKey,
) -> (Arc<App>, String) {
    let plugins = Plugins::new(
        &format!("https://{DASHBOARD}"),
        // reqwest takes the port from the address, not the pinned name.
        &format!("http://{{instance}}.sidecars.invalid:{}", door_at.port()),
        Signer::holding(KEY_ID, key),
    )
    .unwrap()
    .resolving("snaptrade-1.sidecars.invalid", door_at);

    let clock = SystemClock;
    let cache = Arc::new(RecordsCache::default());
    cache.store(records(instances), clock.now_ns());
    let sessions = Arc::new(Sessions::default());
    let session = sessions.start(ADA, "Ada", vec![], clock.now_ns());
    let app = Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: cache,
        sessions,
        terminals: Arc::new(crate::terminal::Terminals::default()),
        clock: Arc::new(clock),
        bus: Arc::new(Bus::single("dashboard-1", Arc::new(MemoryBackend::new()))),
        oidc: None,
        directory: None,
        accounts: None,
        secure_cookies: true,
        plugins: Some(Arc::new(plugins)),
    });
    (app, session)
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

async fn send(
    app: &Arc<App>,
    host: &str,
    method: Method,
    path: &str,
    cookies: &[String],
) -> Answer {
    let mut request = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header(HOST, host);
    if !cookies.is_empty() {
        request = request.header(COOKIE, cookies.join("; "));
    }
    let response = router(Arc::clone(app))
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

async fn get(app: &Arc<App>, host: &str, path: &str, cookies: &[String]) -> Answer {
    send(app, host, Method::GET, path, cookies).await
}

fn dashboard_cookie(h: &Harness) -> String {
    format!("__Host-{SESSION_COOKIE}={}", h.session)
}

/// Open the plugin from the dashboard: the code's path on the plugin's host.
async fn opened(h: &Harness, instance: &str) -> String {
    let answer = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{instance}"),
        &[dashboard_cookie(h)],
    )
    .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    let location = answer.headers[LOCATION].to_str().unwrap().to_string();
    let prefix = format!("https://{instance}.plugins.{DASHBOARD}");
    assert!(location.starts_with(&prefix), "{location}");
    location[prefix.len()..].to_string()
}

/// Redeem it: the plugin host's cookie, as the browser would send it back.
async fn entered(h: &Harness) -> String {
    let path = opened(h, INSTANCE).await;
    let answer = get(&h.app, PLUGIN_HOST, &path, &[]).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.headers[LOCATION], "/");
    let set = answer.headers[SET_COOKIE].to_str().unwrap();
    assert!(set.starts_with("__Host-meridian_plugin_session="), "{set}");
    assert!(set.contains("Path=/;") && set.contains("HttpOnly") && set.contains("Secure"));
    assert!(!set.contains("Domain"), "host-only: {set}");
    set.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn a_person_with_access_opens_the_plugin_and_it_is_told_who_they_are() {
    let h = harness(&[INSTANCE]).await;
    let plugin_session = entered(&h).await;

    let mut request = HttpRequest::get("/holdings?page=2")
        .header(HOST, PLUGIN_HOST)
        .header(
            COOKIE,
            format!("{plugin_session}; {}", dashboard_cookie(&h)),
        )
        .header(AUTHORIZATION, "Bearer somebody-elses")
        .header(CALLER, "a forged assertion")
        .header("x-requested-with", "the-page");
    request = request.header("accept", "text/html");
    let response = router(Arc::clone(&h.app))
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["x-plugin"], "holdings");
    // Not one of ours, not one for the dashboard's domain, and not even one
    // of its own, which the sidecar would never let back in.
    assert!(headers.get(SET_COOKIE).is_none(), "{headers:?}");
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&body[..], b"the page");

    let reached = h.reached.lock().unwrap();
    let (headers, uri) = &reached[0];
    assert_eq!(uri, "/holdings?page=2");
    assert!(headers.get(COOKIE).is_none(), "a cookie reached the plugin");
    assert!(
        headers.get(AUTHORIZATION).is_none(),
        "a credential reached the plugin"
    );
    assert_eq!(headers["x-requested-with"], "the-page");
    let presented: Vec<_> = headers.get_all(CALLER).iter().collect();
    assert_eq!(presented.len(), 1, "one assertion, the dashboard's");
    let assertion = CallerAssertion::decode(
        URL_SAFE_NO_PAD
            .decode(presented[0].to_str().unwrap())
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(assertion.key_id, KEY_ID);
    let claims = CallerClaims::decode(assertion.claims.as_slice()).unwrap();
    assert_eq!(claims.subject, ADA);
    assert_eq!(claims.display_name, "Ada");
    assert_eq!(claims.audience_instance_id, INSTANCE);
    assert_eq!(claims.access.len(), 1);
    assert_eq!(claims.access[0].tag, "custody");
    assert_eq!(claims.access[0].read_account_ids, vec!["ACC-1".to_string()]);
    assert!(claims.access[0].write_account_ids.is_empty());
    assert_eq!(claims.expires_at_ns - claims.issued_at_ns, 60 * SECOND_NS);
}

#[tokio::test]
async fn each_request_carries_an_assertion_of_its_own() {
    // The sidecar refuses one it has seen, so a second request with the
    // first's would fail there; this is the dashboard minting afresh.
    let h = harness(&[INSTANCE]).await;
    let plugin_session = entered(&h).await;
    for _ in 0..3 {
        let answer = get(
            &h.app,
            PLUGIN_HOST,
            "/",
            std::slice::from_ref(&plugin_session),
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    }
    assert_eq!(h.reached.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_code_is_redeemed_once_on_its_own_host_within_its_minute() {
    let h = harness(&[INSTANCE, "snaptrade-2"]).await;

    let path = opened(&h, INSTANCE).await;
    assert_eq!(
        get(&h.app, PLUGIN_HOST, &path, &[]).await.status,
        StatusCode::SEE_OTHER
    );
    let again = get(&h.app, PLUGIN_HOST, &path, &[]).await;
    assert_eq!(again.status, StatusCode::UNAUTHORIZED);
    assert!(
        again.headers.get(SET_COOKIE).is_none(),
        "no session is made"
    );

    // Minted for one instance, presented on another's host.
    let path = opened(&h, INSTANCE).await;
    let elsewhere = get(&h.app, "snaptrade-2.plugins.meridian.test:8443", &path, &[]).await;
    assert_eq!(elsewhere.status, StatusCode::UNAUTHORIZED);
    // And gone for its own host too, having been tried.
    assert_eq!(
        get(&h.app, PLUGIN_HOST, &path, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );

    let plugins = h.app.plugins.as_ref().unwrap();
    let now = h.app.clock.now_ns();
    let code = plugins.mint(&h.session, INSTANCE, now);
    assert_eq!(
        plugins.redeem(&code, INSTANCE, now + CODE_NS + 1),
        None,
        "a minute old"
    );
}

#[tokio::test]
async fn somebody_without_access_is_refused_before_any_code_is_minted() {
    let h = harness(&["another-plugin"]).await;
    let answer = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert!(
        answer.body.contains("no access on snaptrade-1"),
        "{}",
        answer.body
    );
    assert!(h
        .app
        .plugins
        .as_ref()
        .unwrap()
        .codes
        .lock()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_instance_that_does_not_run_or_is_not_a_name_is_not_found() {
    let h = harness(&[INSTANCE, "ghost-1"]).await;
    let ghost = get(
        &h.app,
        DASHBOARD,
        "/plugins/ghost-1",
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(ghost.status, StatusCode::NOT_FOUND);
    assert!(
        ghost.body.contains("No plugin ghost-1 runs"),
        "{}",
        ghost.body
    );
    let odd = get(
        &h.app,
        DASHBOARD,
        "/plugins/Not_A.Name",
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(odd.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn nobody_signed_in_is_sent_to_sign_in() {
    let h = harness(&[INSTANCE]).await;
    let answer = get(&h.app, DASHBOARD, &format!("/plugins/{INSTANCE}"), &[]).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(answer.headers[LOCATION], "/sign-in");
}

#[tokio::test]
async fn a_plugins_host_serves_none_of_the_dashboards_pages() {
    let h = harness(&[INSTANCE]).await;
    // The dashboard's session is host-only, so a browser never sends it here;
    // sent anyway, it opens nothing of the dashboard's.
    for path in ["/admin", "/", "/healthz", "/first-run"] {
        let answer = get(&h.app, PLUGIN_HOST, path, &[dashboard_cookie(&h)]).await;
        assert_eq!(answer.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(
            answer.headers[LOCATION],
            format!("https://{DASHBOARD}/plugins/{INSTANCE}"),
            "{path}"
        );
    }
    let posted = send(
        &h.app,
        PLUGIN_HOST,
        Method::POST,
        "/admin/permissions",
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(posted.status, StatusCode::UNAUTHORIZED);
    for host in [
        "Bad_Name.plugins.meridian.test",
        "plugins.meridian.test",
        "a.b.plugins.meridian.test",
    ] {
        assert_eq!(
            get(&h.app, host, "/admin", &[]).await.status,
            StatusCode::NOT_FOUND,
            "{host}"
        );
    }
    assert!(h.reached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_plugin_session_ends_with_the_dashboard_session_it_came_from() {
    let h = harness(&[INSTANCE]).await;
    let plugin_session = entered(&h).await;
    h.app.sessions.end(&h.session);
    let answer = get(
        &h.app,
        PLUGIN_HOST,
        "/",
        std::slice::from_ref(&plugin_session),
    )
    .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert!(h.reached.lock().unwrap().is_empty());
    // And it is forgotten, not merely refused.
    assert!(h
        .app
        .plugins
        .as_ref()
        .unwrap()
        .entered
        .lock()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn access_withdrawn_is_withdrawn_on_the_next_request() {
    let h = harness(&[INSTANCE]).await;
    let plugin_session = entered(&h).await;
    h.app.records.store(records(&[]), h.app.clock.now_ns());
    let answer = get(
        &h.app,
        PLUGIN_HOST,
        "/",
        std::slice::from_ref(&plugin_session),
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert!(h.reached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_plugin_session_opens_its_own_instance_alone() {
    let h = harness(&[INSTANCE, "snaptrade-2"]).await;
    let plugin_session = entered(&h).await;
    let answer = get(
        &h.app,
        "snaptrade-2.plugins.meridian.test:8443",
        "/",
        std::slice::from_ref(&plugin_session),
    )
    .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert!(h.reached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn home_links_each_plugin_a_person_may_open() {
    let h = harness(&[INSTANCE]).await;
    let home = get(&h.app, DASHBOARD, "/", &[dashboard_cookie(&h)]).await;
    assert!(
        home.body
            .contains("<a href=\"/plugins/snaptrade-1\">snaptrade-1</a>"),
        "{}",
        home.body
    );
}

#[test]
fn hosts_are_read_as_the_browser_names_them() {
    let plugins = Plugins::new(
        "https://Meridian.Test:8443",
        "http://{instance}:9292",
        Signer::holding(KEY_ID, SigningKey::from_bytes(&[7; 32])),
    )
    .unwrap();
    assert_eq!(plugins.host("meridian.test:8443"), Host::Dashboard);
    assert_eq!(plugins.host("10.0.0.7:8080"), Host::Dashboard);
    assert_eq!(
        plugins.host("SnapTrade-1.plugins.meridian.test:8443"),
        Host::Plugin("snaptrade-1".into())
    );
    assert_eq!(
        plugins.host("snaptrade-1.plugins.meridian.test."),
        Host::Plugin("snaptrade-1".into())
    );
    assert_eq!(plugins.host("plugins.meridian.test"), Host::NoSuchPlugin);
    assert_eq!(plugins.host("-x.plugins.meridian.test"), Host::NoSuchPlugin);
    assert_eq!(
        plugins.host("x.y.plugins.meridian.test"),
        Host::NoSuchPlugin
    );
    assert_eq!(
        plugins.origin("snaptrade-1"),
        "https://snaptrade-1.plugins.meridian.test:8443"
    );
}

#[test]
fn a_front_door_that_does_not_name_the_instance_is_refused() {
    let refused = Plugins::new(
        "https://meridian.test",
        "http://one-sidecar:9292",
        Signer::holding(KEY_ID, SigningKey::from_bytes(&[7; 32])),
    );
    assert!(refused.is_err());
}

#[tokio::test]
async fn a_code_minted_before_signing_out_opens_nothing_after() {
    let h = harness(&[INSTANCE]).await;
    let path = opened(&h, INSTANCE).await;
    h.app.sessions.end(&h.session);
    let answer = get(&h.app, PLUGIN_HOST, &path, &[]).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert!(answer.headers.get(SET_COOKIE).is_none());
}

#[tokio::test]
async fn a_sweep_forgets_spent_codes_and_sessions_whose_dashboard_session_ended() {
    let h = harness(&[INSTANCE]).await;
    let plugins = h.app.plugins.as_ref().unwrap();
    let now = h.app.clock.now_ns();
    entered(&h).await;
    let kept = plugins.mint(&h.session, INSTANCE, now);

    plugins.sweep(&h.app.sessions, now);
    assert_eq!(
        plugins.entered.lock().unwrap().len(),
        1,
        "a live session is kept"
    );
    assert!(plugins.codes.lock().unwrap().contains_key(&kept));

    h.app.sessions.end(&h.session);
    plugins.sweep(&h.app.sessions, now + CODE_NS + 1);
    assert!(plugins.entered.lock().unwrap().is_empty());
    assert!(plugins.codes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_request_leaves_the_dashboard_carrying_the_assertion_and_nothing_it_arrived_claiming() {
    // What the dashboard sends, seen before any sidecar strips anything.
    let seen: Arc<Mutex<Vec<HeaderMap>>> = Arc::default();
    let recorded = Arc::clone(&seen);
    let recorder = Router::new().fallback(move |request: axum::extract::Request| {
        let recorded = Arc::clone(&recorded);
        async move {
            recorded.lock().unwrap().push(request.headers().clone());
            "recorded"
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, recorder).await.unwrap() });
    let (app, session) = dashboard(
        &[INSTANCE],
        at,
        SigningKey::generate(&mut rand::rngs::OsRng),
    );
    let h = Harness {
        app,
        reached: Arc::default(),
        session,
    };
    let plugin_session = entered(&h).await;

    let request = HttpRequest::get("/")
        .header(HOST, PLUGIN_HOST)
        .header(COOKIE, format!("{plugin_session}; own=1"))
        .header(AUTHORIZATION, "Bearer somebody-elses")
        .header(CALLER, "a forged assertion")
        .header("connection", "x-hop")
        .header("x-requested-with", "the-page");
    let response = router(Arc::clone(&h.app))
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = seen.lock().unwrap();
    let headers = &seen[0];
    assert!(headers.get(COOKIE).is_none(), "{headers:?}");
    assert!(headers.get(AUTHORIZATION).is_none(), "{headers:?}");
    let presented: Vec<_> = headers.get_all(CALLER).iter().collect();
    assert_eq!(presented.len(), 1);
    assert_ne!(presented[0], "a forged assertion");
    assert_ne!(headers[HOST], PLUGIN_HOST);
    assert_eq!(headers["x-requested-with"], "the-page");
}
