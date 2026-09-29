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
    harness_with(instances, None).await
}

/// The same, the sidecar live on a development deployment when given a folder.
async fn harness_with(instances: &[&str], live: Option<std::path::PathBuf>) -> Harness {
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
                admin_pages: vec![],
            }),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    if let Some(dir) = live {
        sidecar.go_live(Arc::new(meridian_sidecar::live::Live::new(dir)));
    }
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
        sign_in_failures: Default::default(),
        secure_cookies: true,
        plugins: Some(Arc::new(plugins)),
        registry: None,
        custody: Arc::default(),
        health: Arc::default(),
        kit: Some(Arc::new(kit())),
    });
    (app, session)
}

/// A kit of one stylesheet, as the image's is laid out.
fn kit() -> crate::kit::Kit {
    let root = std::env::temp_dir().join(format!("meridian-kit-{}", token()));
    std::fs::create_dir_all(root.join("0.1.0")).unwrap();
    std::fs::write(root.join("0.1.0/meridian.css"), ":root{}").unwrap();
    crate::kit::Kit::at(root).unwrap()
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

/// Open the plugin from the dashboard, as the frame does: the code's path on
/// the plugin's host.
async fn opened(h: &Harness, instance: &str) -> String {
    let answer = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{instance}/enter"),
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
    assert!(
        !claims.deployment_admin,
        "somebody who does not administer the deployment is asserted as not doing so (W6.9)"
    );
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
    let code = plugins.mint(Came::Browser(h.session.clone()), INSTANCE, now);
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
    for (path, back) in [
        ("/admin", "/enter?path=%2Fadmin"),
        ("/", "/enter"),
        ("/healthz", "/enter?path=%2Fhealthz"),
        ("/first-run", "/enter?path=%2Ffirst-run"),
    ] {
        let answer = get(&h.app, PLUGIN_HOST, path, &[dashboard_cookie(&h)]).await;
        assert_eq!(answer.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(
            answer.headers[LOCATION],
            format!("https://{DASHBOARD}/plugins/{INSTANCE}{back}"),
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
    // Not launched through the catalogue, so named by its instance alone.
    assert!(
        home.body.contains(
            "<li data-instance=\"snaptrade-1\"><a class=\"plugin-card\" href=\"/plugins/snaptrade-1\">"
        ),
        "{}",
        home.body
    );
    assert!(home
        .body
        .contains("<span class=\"plugin-name\">snaptrade-1</span>"));
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

#[test]
fn plugin_pages_need_a_name_to_go_below() {
    for named in [
        "https://meridian.test",
        "http://localhost:18480",
        "https://dash.firm.example",
    ] {
        assert_eq!(pages_possible(named), Ok(()), "{named}");
    }
    for address in [
        "http://127.0.0.1:18480",
        "https://[::1]:8443",
        "https://10.0.0.7",
    ] {
        let refused = pages_possible(address).unwrap_err();
        assert!(refused.contains("an IP address"), "{refused}");
        assert!(Plugins::new(
            address,
            "http://{instance}.sidecars:9292",
            Signer::holding(KEY_ID, SigningKey::from_bytes(&[7; 32])),
        )
        .is_err());
    }
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
    let kept = plugins.mint(Came::Browser(h.session.clone()), INSTANCE, now);

    plugins.sweep(&h.app.sessions, &h.app.terminals, now);
    assert_eq!(
        plugins.entered.lock().unwrap().len(),
        1,
        "a live session is kept"
    );
    assert!(plugins.codes.lock().unwrap().contains_key(&kept));

    h.app.sessions.end(&h.session);
    plugins.sweep(&h.app.sessions, &h.app.terminals, now + CODE_NS + 1);
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

/// Ada as a deployment admin, holding no access entry on any plugin.
fn admin_records() -> AccessRecords {
    let mut held = records(&[]);
    held.permissions.push(Permission {
        permission_id: "P-admin".into(),
        user_group_id: "UG-1".into(),
        account_group_id: String::new(),
        access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
    });
    held
}

fn claims_reaching(h: &Harness) -> CallerClaims {
    let reached = h.reached.lock().unwrap();
    let (headers, _) = reached.last().expect("a request reached the plugin");
    let assertion = CallerAssertion::decode(
        URL_SAFE_NO_PAD
            .decode(headers[CALLER].to_str().unwrap())
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    CallerClaims::decode(assertion.claims.as_slice()).unwrap()
}

#[tokio::test]
async fn a_deployment_admin_opens_any_plugin_asserted_with_only_what_they_hold() {
    // Ruling 19: a plugin with nothing to grant is still opened by the
    // people who administer the deployment, and opening is not access.
    let h = harness(&[]).await;
    h.app.records.store(admin_records(), h.app.clock.now_ns());
    let plugin_session = entered(&h).await;
    let answer = get(
        &h.app,
        PLUGIN_HOST,
        "/",
        std::slice::from_ref(&plugin_session),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let claims = claims_reaching(&h);
    assert_eq!(claims.subject, ADA);
    assert!(claims.access.is_empty(), "{:?}", claims.access);
    // And said to be one, so the plugin serves its admin page to them (W6.9).
    assert!(claims.deployment_admin);
}

#[tokio::test]
async fn an_admin_asserted_with_what_they_hold_where_they_hold_something() {
    let h = harness(&[INSTANCE]).await;
    let mut held = admin_records();
    held.access_groups = records(&[INSTANCE]).access_groups;
    h.app.records.store(held, h.app.clock.now_ns());
    let plugin_session = entered(&h).await;
    get(
        &h.app,
        PLUGIN_HOST,
        "/",
        std::slice::from_ref(&plugin_session),
    )
    .await;
    let claims = claims_reaching(&h);
    assert_eq!(claims.access.len(), 1);
    assert_eq!(claims.access[0].read_account_ids, vec!["ACC-1".to_string()]);
    assert!(claims.access[0].write_account_ids.is_empty());
}

#[tokio::test]
async fn an_admin_who_stops_being_one_is_refused_on_the_next_request() {
    let h = harness(&[]).await;
    h.app.records.store(admin_records(), h.app.clock.now_ns());
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

/// A conductor whose catalogue has snaptrade-1 launched and stopped-1 not.
fn serving_launched(h: &Harness) {
    h.app.bus.serve(crate::catalogue::PLUGIN_CATALOGUE, |_| {
        let launch = |instance: &str, state: meridian_domain::v1::PluginLaunchState| {
            meridian_domain::v1::PluginLaunch {
                instance_id: instance.into(),
                name: "snaptrade".into(),
                version: "0.1.0".into(),
                state: state as i32,
                ..Default::default()
            }
        };
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            meridian_domain::v1::PluginCatalogue {
                versions: vec![],
                launches: vec![
                    launch(INSTANCE, meridian_domain::v1::PluginLaunchState::Launched),
                    launch("stopped-1", meridian_domain::v1::PluginLaunchState::Stopped),
                ],
            }
            .encode_to_vec(),
        ))
    });
}

#[tokio::test]
async fn an_admins_home_links_every_plugin_launched() {
    let h = harness(&[]).await;
    h.app.records.store(admin_records(), h.app.clock.now_ns());
    serving_launched(&h);
    let home = get(&h.app, DASHBOARD, "/", &[dashboard_cookie(&h)]).await;
    let card = home
        .body
        .split("<li data-instance=\"snaptrade-1\">")
        .nth(1)
        .unwrap_or_else(|| panic!("{}", home.body))
        .split("</li>")
        .next()
        .unwrap();
    // Opened in the frame, named by the plugin and the instance both.
    assert!(card.contains("href=\"/plugins/snaptrade-1\""), "{card}");
    assert!(card.contains("<span class=\"plugin-name\">snaptrade</span>"));
    assert!(card.contains("<span class=\"plugin-instance\">snaptrade-1</span>"));
    assert!(card.contains("as admin"), "holding nothing on it: {card}");
    assert!(!home.body.contains("stopped-1"), "{}", home.body);
    // Both views, the list first; the tiles a switch away.
    assert!(home
        .body
        .contains("<ul class=\"plugins list\" data-plugins>"));
    assert!(home.body.contains("data-view=\"tiles\""));
}

#[tokio::test]
async fn somebody_who_is_not_an_admin_is_not_shown_what_is_launched() {
    let h = harness(&[]).await;
    serving_launched(&h);
    let home = get(&h.app, DASHBOARD, "/", &[dashboard_cookie(&h)]).await;
    assert!(
        home.body.contains("No plugins for you yet"),
        "{}",
        home.body
    );
    assert!(!home.body.contains("snaptrade-1"), "{}", home.body);
}

/// A terminal session for Ada, as `meridian connect` gets one.
fn terminal(app: &Arc<App>) -> String {
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const BACK: &str = "http://127.0.0.1:53682/callback";
    let now = app.clock.now_ns();
    let terminals = &app.terminals;
    let id = terminals.open(
        crate::terminal::check(BACK, CHALLENGE, "S256", "st").unwrap(),
        now,
    );
    let person = crate::terminal::Person {
        subject: ADA.into(),
        display_name: "Ada".into(),
        directory_groups: vec![],
        signed_in_at_ns: now,
    };
    let confirm = terminals.signed_in(&id, person, now).unwrap();
    let (_, code) = terminals.decide(&id, &confirm, true, now).unwrap();
    terminals
        .exchange(&code.unwrap(), VERIFIER, BACK, now)
        .unwrap()
        .session
}

async fn develop(app: &Arc<App>, method: Method, path: &str, bearer: &str, body: &str) -> Answer {
    let response = router(Arc::clone(app))
        .oneshot(
            HttpRequest::builder()
                .method(method)
                .uri(path)
                .header(HOST, DASHBOARD)
                .header(AUTHORIZATION, format!("Bearer {bearer}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
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

#[tokio::test]
async fn a_deployment_admin_sends_a_live_plugin_a_change_from_the_terminal() {
    let dir = std::env::temp_dir().join(format!("meridian-dash-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let h = harness_with(&[], Some(dir.clone())).await;
    h.app.records.store(admin_records(), h.app.clock.now_ns());
    let session = terminal(&h.app);
    let change = r#"{"files":{"src/page.py":"cHJpbnQoMSk="}}"#;

    let sent = develop(
        &h.app,
        Method::PUT,
        &format!("/terminal/plugins/{INSTANCE}/dev/files"),
        &session,
        change,
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    assert!(sent.body.contains(r#""revision":1"#), "{}", sent.body);
    assert_eq!(
        std::fs::read_to_string(dir.join("src/page.py")).unwrap(),
        "print(1)"
    );
    let events = develop(
        &h.app,
        Method::GET,
        &format!("/terminal/plugins/{INSTANCE}/dev/events?since=0"),
        &session,
        "",
    )
    .await;
    assert!(
        events.body.contains(r#""event":"synced""#),
        "{}",
        events.body
    );
    assert!(
        h.reached.lock().unwrap().is_empty(),
        "nothing reached the plugin"
    );

    // Not a deployment admin: refused here, before the sidecar is asked.
    h.app
        .records
        .store(records(&[INSTANCE]), h.app.clock.now_ns());
    let refused = develop(
        &h.app,
        Method::PUT,
        &format!("/terminal/plugins/{INSTANCE}/dev/files"),
        &session,
        change,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    // A path that is not one of the three.
    h.app.records.store(admin_records(), h.app.clock.now_ns());
    let other = develop(
        &h.app,
        Method::GET,
        &format!("/terminal/plugins/{INSTANCE}/dev/secrets"),
        &session,
        "",
    )
    .await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_instance_that_is_not_live_has_no_development_path() {
    let h = harness(&[]).await;
    h.app.records.store(admin_records(), h.app.clock.now_ns());
    let session = terminal(&h.app);
    let answer = develop(
        &h.app,
        Method::PUT,
        &format!("/terminal/plugins/{INSTANCE}/dev/files"),
        &session,
        "{}",
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND, "{}", answer.body);
    assert!(h.reached.lock().unwrap().is_empty());
}

// ── W6.15: from a terminal ───────────────────────────────────────────────

#[tokio::test]
async fn a_terminal_link_enters_the_plugins_host_once_and_ends_with_the_terminal_session() {
    let h = harness(&[INSTANCE]).await;
    let session = terminal(&h.app);
    let opened = develop(
        &h.app,
        Method::POST,
        &format!("/terminal/plugins/{INSTANCE}/open"),
        &session,
        "",
    )
    .await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
    let said: serde_json::Value = serde_json::from_str(&opened.body).unwrap();
    let url = said["url"].as_str().unwrap();
    let prefix = format!("https://{INSTANCE}.plugins.{DASHBOARD}");
    assert!(
        url.starts_with(&format!("{prefix}{ENTER_PATH}?code=")),
        "{url}"
    );
    let path = &url[prefix.len()..];

    // Any browser: no dashboard cookie is needed, or sent.
    let entered = get(&h.app, PLUGIN_HOST, path, &[]).await;
    assert_eq!(entered.status, StatusCode::SEE_OTHER, "{}", entered.body);
    let cookie = entered.headers[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let page = get(&h.app, PLUGIN_HOST, "/", std::slice::from_ref(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(page.body, "the page");
    // Once.
    assert_eq!(
        get(&h.app, PLUGIN_HOST, path, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    // It never reaches the dashboard's own pages.
    let home = get(&h.app, DASHBOARD, "/", std::slice::from_ref(&cookie)).await;
    assert!(!home.body.contains("Ada"), "{}", home.body);

    // The terminal session ends, and the host's with it.
    h.app.terminals.end(&session);
    let after = get(&h.app, PLUGIN_HOST, "/", &[cookie]).await;
    assert_eq!(
        after.status,
        StatusCode::SEE_OTHER,
        "sent back to the dashboard"
    );
}

#[tokio::test]
async fn a_terminal_reads_the_page_as_the_person_is_served_it() {
    let h = harness(&[INSTANCE]).await;
    let session = terminal(&h.app);
    let read = develop(
        &h.app,
        Method::GET,
        &format!("/terminal/plugins/{INSTANCE}/page?path=%2Fholdings%3Fpage%3D2"),
        &session,
        "",
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    let said: serde_json::Value = serde_json::from_str(&read.body).unwrap();
    assert_eq!(said["status"], 200);
    assert_eq!(said["body"], "the page");
    let reached = h.reached.lock().unwrap();
    assert_eq!(reached.len(), 1);
    assert_eq!(reached[0].1, "/holdings?page=2");
}

#[tokio::test]
async fn a_terminal_is_held_to_what_opening_the_page_is_held_to() {
    let h = harness(&["another-plugin"]).await;
    let session = terminal(&h.app);
    for (method, path) in [
        (Method::POST, format!("/terminal/plugins/{INSTANCE}/open")),
        (Method::GET, format!("/terminal/plugins/{INSTANCE}/page")),
    ] {
        let refused = develop(&h.app, method.clone(), &path, &session, "").await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{path}: {}",
            refused.body
        );
        let nobody = develop(&h.app, method, &path, "not-a-session", "").await;
        assert_eq!(nobody.status, StatusCode::UNAUTHORIZED, "{path}");
    }
    assert!(h
        .app
        .plugins
        .as_ref()
        .unwrap()
        .codes
        .lock()
        .unwrap()
        .is_empty());
    assert!(h.reached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_browser_cookie_is_no_terminal_session_for_these_paths() {
    let h = harness(&[INSTANCE]).await;
    let answer = send(
        &h.app,
        DASHBOARD,
        Method::POST,
        &format!("/terminal/plugins/{INSTANCE}/open"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

#[test]
fn a_page_path_is_a_path_on_the_plugins_host_and_nothing_else() {
    for path in [
        "/",
        "/holdings",
        "/holdings?page=2",
        "/a/b.css",
        "/.well-known/x",
    ] {
        assert_eq!(page_path(path), Ok(()), "{path}");
    }
    for path in [
        "",
        "holdings",
        "//elsewhere.example/x",
        "https://elsewhere.example/",
        "/.meridian/dev/files",
        "/.meridian",
        "/.meridian?x",
        "/a b",
        "/a#b",
        "/a\\b",
    ] {
        assert!(page_path(path).is_err(), "{path}");
    }
}

// ── The frame, the theme and the kit (spec/plugin-pages-share-one-kit.md) ──

#[tokio::test]
async fn the_frame_draws_the_header_around_the_plugins_page_and_hands_it_the_theme() {
    let h = harness(&[INSTANCE]).await;
    serving_launched(&h);
    let frame = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(frame.status, StatusCode::OK, "{}", frame.body);
    let head = frame.body.split("</header>").next().unwrap();
    // The plugin's name and the instance's, the way back, and the person.
    assert!(
        head.contains("<strong>snaptrade</strong><code>snaptrade-1</code>"),
        "{head}"
    );
    assert!(head.contains("<a href=\"/\">Plugins</a>"));
    assert!(head.contains("Ada") && head.contains("/sign-out"));
    assert!(
        !head.contains("Admin portal"),
        "Ada administers nothing here"
    );
    // The page below, entered through the dashboard with the theme on its
    // address, and told again by message on every load, to its origin alone.
    assert!(frame.body.contains(
        "<iframe src=\"/plugins/snaptrade-1/enter?path=%2F&amp;om-scheme=default&amp;om-mode=system\
         &amp;om-direction=green-up\""
    ), "{}", frame.body);
    assert!(frame
        .body
        .contains(&format!("data-origin=\"https://{PLUGIN_HOST}\"")));
    assert!(frame.body.contains("\"meridian:theme\", version: 2"));

    // At a page of the plugin's, where the frame is asked for one.
    let at = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}?path=%2Fholdings%3Fpage%3D2"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert!(
        at.body.contains("enter?path=%2Fholdings%3Fpage%3D2&amp;"),
        "{}",
        at.body
    );

    // The person's mode, where they chose one.
    let dark = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}"),
        &[dashboard_cookie(&h), "__Host-meridian_mode=dark".into()],
    )
    .await;
    assert!(dark.body.contains("om-mode=dark"), "{}", dark.body);

    // The dashboard's own pages are framed by nobody else.
    assert_eq!(frame.headers["x-frame-options"], "SAMEORIGIN");
    assert_eq!(
        frame.headers["content-security-policy"],
        "frame-ancestors 'self'"
    );
}

#[tokio::test]
async fn the_frames_way_in_lands_on_the_page_asked_for_carrying_the_theme() {
    let h = harness(&[INSTANCE]).await;
    let answer = get(
        &h.app,
        DASHBOARD,
        &format!(
            "/plugins/{INSTANCE}/enter?path=%2Fadmin%3Ftab%3D2&om-scheme=harbour&om-mode=dark\
             &om-direction=red-up"
        ),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    let location = answer.headers[LOCATION].to_str().unwrap();
    let path = &location[format!("https://{PLUGIN_HOST}").len()..];
    let redeemed = get(&h.app, PLUGIN_HOST, path, &[]).await;
    assert_eq!(redeemed.status, StatusCode::SEE_OTHER, "{}", redeemed.body);
    assert_eq!(
        redeemed.headers[LOCATION],
        "/admin?tab=2&om-scheme=harbour&om-mode=dark&om-direction=red-up"
    );

    // Nothing the kit would not take is carried onto the plugin's address,
    // and nowhere but a page on the plugin's own host.
    let odd = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}/enter?om-scheme=Evil%22Scheme&om-mode=sepia&om-direction=up"),
        &[dashboard_cookie(&h)],
    )
    .await;
    let location = odd.headers[LOCATION].to_str().unwrap();
    let path = &location[format!("https://{PLUGIN_HOST}").len()..];
    let redeemed = get(&h.app, PLUGIN_HOST, path, &[]).await;
    assert_eq!(
        redeemed.headers[LOCATION],
        "/?om-scheme=default&om-mode=system&om-direction=green-up"
    );
    for elsewhere in [
        "https%3A%2F%2Felsewhere.example",
        "%2F%2Felsewhere.example",
        "%2F.meridian%2Fenter",
    ] {
        let refused = get(
            &h.app,
            DASHBOARD,
            &format!("/plugins/{INSTANCE}/enter?path={elsewhere}"),
            &[dashboard_cookie(&h)],
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{elsewhere}");
    }
}

#[tokio::test]
async fn a_page_in_a_window_of_its_own_goes_back_to_the_frame_and_one_in_the_frame_to_its_way_in() {
    let h = harness(&[INSTANCE]).await;
    let asked = |dest: &'static str| {
        let app = Arc::clone(&h.app);
        async move {
            router(app)
                .oneshot(
                    HttpRequest::get("/holdings?page=2")
                        .header(HOST, PLUGIN_HOST)
                        .header("sec-fetch-dest", dest)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        }
    };
    let window = asked("document").await;
    assert_eq!(
        window.headers()[LOCATION],
        format!("https://{DASHBOARD}/plugins/{INSTANCE}?path=%2Fholdings%3Fpage%3D2")
    );
    let framed = asked("iframe").await;
    assert_eq!(
        framed.headers()[LOCATION],
        format!("https://{DASHBOARD}/plugins/{INSTANCE}/enter?path=%2Fholdings%3Fpage%3D2")
    );
}

#[tokio::test]
async fn the_kit_is_served_on_the_plugins_host_to_anybody_and_its_page_framed_by_the_dashboard_alone(
) {
    let h = harness(&[INSTANCE]).await;
    // No session: the kit is the same files for everybody.
    let kit = get(&h.app, PLUGIN_HOST, "/.meridian/ui/0.1.0/meridian.css", &[]).await;
    assert_eq!(kit.status, StatusCode::OK, "{}", kit.body);
    assert_eq!(kit.headers["content-type"], "text/css; charset=utf-8");
    assert_eq!(kit.body, ":root{}");
    assert_eq!(
        get(
            &h.app,
            PLUGIN_HOST,
            "/.meridian/ui/0.1.0/../../etc/passwd",
            &[]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert!(
        h.reached.lock().unwrap().is_empty(),
        "the plugin never saw it"
    );
    // And on the dashboard's own host, for its own pages.
    let own = get(&h.app, DASHBOARD, "/.meridian/ui/0.1.0/meridian.css", &[]).await;
    assert_eq!(own.status, StatusCode::OK);

    let plugin_session = entered(&h).await;
    let page = get(
        &h.app,
        PLUGIN_HOST,
        "/",
        std::slice::from_ref(&plugin_session),
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.headers["content-security-policy"],
        format!("frame-ancestors https://{DASHBOARD}").as_str()
    );
}

#[test]
fn where_no_frame_can_hold_a_session_the_page_opens_in_a_window_of_its_own() {
    // A cookie set in a frame from another site is refused, and below a host
    // with no domain every plugin's host is another site.
    let signer = || Signer::holding(KEY_ID, SigningKey::from_bytes(&[7; 32]));
    for (address, frames) in [
        ("http://localhost:8088", false),
        ("http://dashboard:8080", false),
        ("http://meridian.localhost", true),
        ("https://meridian.firm.example", true),
    ] {
        let plugins = Plugins::new(address, "http://{instance}:9292", signer()).unwrap();
        assert_eq!(plugins.frames(), frames, "{address}");
    }
}
