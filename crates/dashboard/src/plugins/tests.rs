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
    AccessEntry, AccessGroup, AccessRecords, AccountGroup, AccountRecord, AccountState, Permission,
    UserGroup,
};
use meridian_pb::v1::plugin_figure::Value as FigureValue;
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{
    CallerAssertion, FigureState, InterfaceDeclaration, PluginFigure, RegisterRequest,
};
use meridian_sidecar::front_door::{self, FrontDoor, Verifier};
use meridian_sidecar::{Contract, Identity, Sidecar};
use tower::ServiceExt;

use super::*;
use crate::records::RecordsCache;
use crate::web::router;
use crate::Clock;
use meridian_clock::SystemClock;

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
            name: "Custody readers".into(),
            entries: instances
                .iter()
                .map(|instance| AccessEntry {
                    plugin_instance_id: instance.to_string(),
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
    let bus = Arc::new(Bus::single(
        INSTANCE,
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
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
            schema_version: "v5".into(),
            interface: Some(InterfaceDeclaration {
                loopback_port: plugin_port.into(),
                title: "Holdings".into(),
                pages: vec![],
            }),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);
    if let Some(dir) = live {
        sidecar.go_live(Arc::new(meridian_sidecar::live::Live::new(
            dir,
            Arc::new(meridian_clock::SystemClock),
        )));
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
        bus: Arc::new(Bus::single(
            "dashboard-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::SystemClock),
        )),
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

/// A kit of one stylesheet, as the image's is laid out: one version.
fn kit() -> crate::kit::Kit {
    let root = std::env::temp_dir().join(format!("meridian-kit-{}", token()));
    std::fs::create_dir_all(root.join("0.3.0")).unwrap();
    std::fs::write(root.join("0.3.0/meridian.css"), ":root{}").unwrap();
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
    assert_eq!(claims.read_account_ids, vec!["ACC-1".to_string()]);
    assert!(claims.write_account_ids.is_empty());
    assert_eq!(claims.expires_at_ns - claims.issued_at_ns, 60 * SECOND_NS);
    assert!(
        !claims.deployment_admin,
        "somebody who does not administer the deployment is asserted as not doing so (W6.9)"
    );
    assert_eq!(
        claims.level,
        AccessLevel::Read as i32,
        "opened at the one level she holds, View"
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
    let code = plugins.mint(
        Came::Browser(h.session.clone()),
        INSTANCE,
        AccessLevel::Read,
        now,
    );
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
    // Not launched through the catalogue, so named by its instance alone;
    // and a button for the one level she holds, View (W6.9). The name is no
    // link: the button is the way in.
    assert!(
        home.body.contains(
            "<li data-instance=\"snaptrade-1\"><div class=\"plugin-card\">\
             <span class=\"plugin-main\"><span class=\"plugin-icon\" aria-hidden=\"true\">s</span>\
             <span class=\"plugin-text\"><span class=\"plugin-name\">snaptrade-1</span></span></span>\
             <span class=\"plugin-levels\""
        ),
        "{}",
        home.body
    );
    assert_eq!(
        home.body.matches("href=\"/plugins/snaptrade-1").count(),
        1,
        "one way in, View's"
    );
    assert!(!home.body.contains("/admin/plugins/"));
    assert!(home.body.contains(
        "<a class=\"plugin-level\" data-level=\"read\" href=\"/plugins/snaptrade-1?level=read\">View</a>"
    ));
    assert!(
        !home.body.contains("data-level=\"write\"") && !home.body.contains("data-level=\"admin\"")
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
    let kept = plugins.mint(
        Came::Browser(h.session.clone()),
        INSTANCE,
        AccessLevel::Read,
        now,
    );

    plugins.sweep(&h.app.sessions, &h.app.terminals, now).await;
    assert_eq!(
        plugins.entered.lock().unwrap().len(),
        1,
        "a live session is kept"
    );
    assert!(plugins.codes.lock().unwrap().contains_key(&kept));

    h.app.sessions.end(&h.session);
    plugins
        .sweep(&h.app.sessions, &h.app.terminals, now + CODE_NS + 1)
        .await;
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

/// Ada as a deployment admin linked to All plugins (admin), as first run
/// links her, holding no data grant on any plugin.
fn admin_records() -> AccessRecords {
    let mut held = deployment_admin_alone();
    held.permissions.push(Permission {
        permission_id: "P-all-plugins".into(),
        user_group_id: "UG-1".into(),
        account_group_id: String::new(),
        access_group_id: meridian_access::ALL_PLUGINS_ADMIN.into(),
    });
    held
}

/// Ada as a deployment admin whose link to All plugins (admin) was
/// withdrawn: the deployment's capabilities, and nothing on any plugin.
fn deployment_admin_alone() -> AccessRecords {
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

/// Open the plugin at `level` and send one request on its host.
async fn at_level(h: &Harness, level: &str) -> Answer {
    let answer = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}/enter?level={level}"),
        &[dashboard_cookie(h)],
    )
    .await;
    if answer.status != StatusCode::SEE_OTHER {
        return answer;
    }
    let location = answer.headers[LOCATION].to_str().unwrap().to_string();
    let path = &location[format!("https://{PLUGIN_HOST}").len()..];
    let redeemed = get(&h.app, PLUGIN_HOST, path, &[]).await;
    let cookie = redeemed.headers[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    get(&h.app, PLUGIN_HOST, "/", &[cookie]).await
}

#[tokio::test]
async fn a_deployment_admin_linked_to_all_plugins_admin_opens_any_plugin_at_manage_with_no_account()
{
    // The product owner, 2026-09-30, superseding ruling 19: a deployment
    // admin is admin on a plugin through All plugins (admin), and reaches no
    // account's data by it.
    let h = harness(&[]).await;
    h.app.records.store(admin_records(), h.app.clock.now_ns());
    let answer = at_level(&h, "admin").await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let claims = claims_reaching(&h);
    assert_eq!(claims.subject, ADA);
    assert_eq!(claims.level, AccessLevel::Admin as i32, "Manage");
    assert!(
        claims.read_account_ids.is_empty() && claims.write_account_ids.is_empty(),
        "{claims:?}"
    );
    // Said to be a deployment admin, which names a new account when linking.
    assert!(claims.deployment_admin);
    // And nothing at a data level she does not hold.
    let refused = at_level(&h, "read").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(
        refused.body.contains("You do not hold read"),
        "{}",
        refused.body
    );
}

#[tokio::test]
async fn a_deployment_admin_alone_holds_nothing_on_a_plugin() {
    let h = harness(&[]).await;
    h.app
        .records
        .store(deployment_admin_alone(), h.app.clock.now_ns());
    let refused = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(
        refused.body.contains("no access on snaptrade-1"),
        "{}",
        refused.body
    );
    assert!(h.reached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_person_holding_admin_and_a_data_level_chooses_and_each_session_carries_its_level_alone()
{
    let h = harness(&[INSTANCE]).await;
    let mut held = admin_records();
    held.access_groups = records(&[INSTANCE]).access_groups;
    held.permissions.extend(records(&[INSTANCE]).permissions);
    h.app.records.store(held, h.app.clock.now_ns());

    at_level(&h, "admin").await;
    let manage = claims_reaching(&h);
    assert_eq!(manage.level, AccessLevel::Admin as i32);
    assert!(manage.read_account_ids.is_empty() && manage.write_account_ids.is_empty());

    at_level(&h, "view").await;
    let view = claims_reaching(&h);
    assert_eq!(view.level, AccessLevel::Read as i32);
    assert_eq!(view.read_account_ids, vec!["ACC-1".to_string()]);
    assert!(view.write_account_ids.is_empty());

    let refused = at_level(&h, "write").await;
    assert_eq!(
        refused.status,
        StatusCode::FORBIDDEN,
        "she reads, and writes nothing"
    );
    let odd = at_level(&h, "owner").await;
    assert_eq!(odd.status, StatusCode::FORBIDDEN);
    assert!(odd.body.contains("is not a level"), "{}", odd.body);
}

#[tokio::test]
async fn a_writer_opens_by_open_and_by_view_each_cut_to_its_level() {
    let h = harness(&[INSTANCE]).await;
    let mut held = records(&[INSTANCE]);
    held.access_groups[0].entries[0].level = AccessLevel::Write as i32;
    h.app.records.store(held, h.app.clock.now_ns());
    at_level(&h, "write").await;
    let open = claims_reaching(&h);
    assert_eq!(open.level, AccessLevel::Write as i32);
    assert_eq!(open.write_account_ids, vec!["ACC-1".to_string()]);
    at_level(&h, "read").await;
    let view = claims_reaching(&h);
    assert_eq!(view.level, AccessLevel::Read as i32);
    assert!(view.write_account_ids.is_empty(), "View acts on nothing");
    assert!(
        at_level(&h, "admin").await.status == StatusCode::FORBIDDEN,
        "no Manage without admin"
    );
}

#[tokio::test]
async fn an_admin_whose_link_is_withdrawn_is_refused_on_the_next_request() {
    let h = harness(&[]).await;
    h.app.records.store(admin_records(), h.app.clock.now_ns());
    let plugin_session = entered(&h).await;
    h.app
        .records
        .store(deployment_admin_alone(), h.app.clock.now_ns());
    let answer = get(
        &h.app,
        PLUGIN_HOST,
        "/",
        std::slice::from_ref(&plugin_session),
    )
    .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert!(
        answer.body.contains("no longer hold admin"),
        "{}",
        answer.body
    );
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
    // Through All plugins (admin): Manage alone, into its area, named by
    // the plugin and the instance both.
    assert!(
        card.contains("href=\"/plugins/snaptrade-1?level=admin\""),
        "{card}"
    );
    assert!(
        card.contains("data-level=\"admin\" href=\"/plugins/snaptrade-1?level=admin\">Manage</a>")
    );
    assert!(
        !card.contains(">Open</a>") && !card.contains(">View</a>"),
        "{card}"
    );
    assert!(card.contains("<span class=\"plugin-name\">snaptrade</span>"));
    assert!(card.contains("<span class=\"plugin-instance\">snaptrade-1</span>"));
    assert!(!home.body.contains("stopped-1"), "{}", home.body);
    // Both views, the list first; the tiles a switch away.
    assert!(home
        .body
        .contains("<ul class=\"plugins list\" id=\"home-plugins\" data-plugins>"));
    assert!(home.body.contains("data-view=\"tiles\""));
    assert!(
        !home.body.contains("data-filter=\"home-plugins\""),
        "one plugin needs no search box"
    );
}

/// The product owner, 2026-09-30: lists are built for 100 and more. Past a
/// screenful, home offers a search box over its plugins, which are in the
/// order a person reads them, by name then instance, and says when a search
/// matches none.
#[tokio::test]
async fn an_admins_home_with_many_plugins_sorts_them_by_name_and_offers_a_search() {
    let h = harness(&[]).await;
    h.app.records.store(admin_records(), h.app.clock.now_ns());
    h.app.bus.serve(crate::catalogue::PLUGIN_CATALOGUE, |_| {
        let launches = (0..150)
            .map(|i| meridian_domain::v1::PluginLaunch {
                // Instances in one order, names in another.
                instance_id: format!("plugin-{i:03}"),
                name: format!("Plugin {:03}", 149 - i),
                version: "0.1.0".into(),
                state: meridian_domain::v1::PluginLaunchState::Launched as i32,
                ..Default::default()
            })
            .collect();
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            meridian_domain::v1::PluginCatalogue {
                versions: vec![],
                launches,
            }
            .encode_to_vec(),
        ))
    });
    let home = get(&h.app, DASHBOARD, "/", &[dashboard_cookie(&h)]).await;
    assert_eq!(home.body.matches("<li data-instance=").count(), 150);
    let order: Vec<&str> = home
        .body
        .split("<li data-instance=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap())
        .collect();
    assert_eq!(order[0], "plugin-149", "Plugin 000 first");
    assert_eq!(order[149], "plugin-000", "Plugin 149 last");
    let search = home.body.split("<ul class=").next().unwrap();
    assert!(
        search.contains("data-filter=\"home-plugins\" hidden")
            && search.contains("aria-label=\"Search your plugins\"")
            && search.contains("data-filter-count=\"home-plugins\""),
        "a search box, hidden until the script shows it"
    );
    assert!(home.body.contains(
        "<p class=\"empty\" data-filter-none=\"home-plugins\" hidden>No plugin matches that search.</p>"
    ));
}

#[tokio::test]
async fn somebody_who_is_not_an_admin_is_not_shown_what_is_launched() {
    // A deployment admin whose link to All plugins (admin) is withdrawn
    // included: being one lists no plugin.
    let h = harness(&[]).await;
    h.app
        .records
        .store(deployment_admin_alone(), h.app.clock.now_ns());
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
async fn terminal(app: &Arc<App>) -> String {
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
        .await
        .expect("the store answers")
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
    let session = terminal(&h.app).await;
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
    let session = terminal(&h.app).await;
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
    let session = terminal(&h.app).await;
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
    h.app.terminals.end(&session).await.unwrap();
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
    let session = terminal(&h.app).await;
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
async fn a_terminal_names_the_level_to_open_at_and_is_refused_one_not_held() {
    // W6.15: as the home's buttons do; she holds read alone.
    let h = harness(&[INSTANCE]).await;
    let session = terminal(&h.app).await;
    let read = develop(
        &h.app,
        Method::GET,
        &format!("/terminal/plugins/{INSTANCE}/page?path=%2F&level=view"),
        &session,
        "",
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    let said: serde_json::Value = serde_json::from_str(&read.body).unwrap();
    assert_eq!(said["level"], "read");
    assert_eq!(claims_reaching(&h).level, AccessLevel::Read as i32);
    for (method, path) in [
        (
            Method::GET,
            format!("/terminal/plugins/{INSTANCE}/page?path=%2F&level=admin"),
        ),
        (
            Method::POST,
            format!("/terminal/plugins/{INSTANCE}/open?level=write"),
        ),
    ] {
        let refused = develop(&h.app, method, &path, &session, "").await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{path}");
        assert!(refused.body.contains("You do not hold"), "{}", refused.body);
    }
    assert_eq!(h.reached.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_terminal_is_held_to_what_opening_the_page_is_held_to() {
    let h = harness(&["another-plugin"]).await;
    let session = terminal(&h.app).await;
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
async fn the_area_draws_one_heading_and_one_tab_row_around_the_page_in_a_seamless_frame() {
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
    // The plugin's name, its instance on hover, the last crumb: its page's
    // status dot is beside the name title below, not here; the way back, and
    // the person.
    assert!(
        head.contains(
            "<span class=\"here\" aria-current=\"page\" title=\"snaptrade-1\">snaptrade</span></nav>"
        ),
        "{head}"
    );
    assert!(!head.contains("page-status"), "{head}");
    assert!(
        head.contains("<a href=\"/\">Home</a><span class=\"sep\" aria-hidden=\"true\">/</span>")
    );
    assert!(head.contains("Ada") && head.contains("/sign-out"));
    assert!(
        !head.contains("href=\"/admin\""),
        "Ada administers nothing here"
    );
    // She holds read alone: View, the plugin's `/` its one tab, since it
    // declares no page at read (W4.8).
    let body = frame.body.split("</header>").nth(1).unwrap();
    assert!(
        body.contains("<div class=\"plugin-area\" data-level=\"read\">"),
        "{body}"
    );
    assert!(body.contains("data-level=\"read\">View</span>"), "{body}");
    assert_eq!(
        body.matches("<nav class=\"tabs view-tabs\"").count(),
        1,
        "one tab row"
    );
    assert!(body.contains(
        "href=\"/plugins/snaptrade-1?level=read&amp;tab=home\" data-tab=\"home\" data-page=\"/\""
    ));
    assert!(
        !body.contains("/admin/plugins/"),
        "no way to the admin portal for somebody who administers nothing"
    );
    // Before the plugin's name, a house Home.
    assert!(
        body.contains(
            "<div class=\"area-title\"><a class=\"home-link\" href=\"/\" aria-label=\"Home\" title=\"Home\"><svg"
        ) && body.contains(
            "</svg></a><h1 title=\"snaptrade\">snaptrade</h1><span class=\"title-status\" id=\"page-status\"></span></div>"
        ),
        "{body}"
    );
    // The page below, entered through the dashboard at the session's level
    // with the theme on its address, seamless: framed, told so by message on
    // every load to its origin alone, as tall as it says, its header actions
    // and status drawn by the dashboard.
    assert!(
        body.contains(&format!(
            "<iframe class=\"plugin-frame\" id=\"plugin-page\" data-page=\"/\" \
             src=\"/plugins/snaptrade-1/enter?path=%2F&amp;level=read&amp;om-scheme=default\
             &amp;om-mode=system&amp;om-direction=green-up&amp;om-framed=1\" \
             title=\"snaptrade &middot; Home\" data-plugin-frame data-seamless \
             data-origin=\"https://{PLUGIN_HOST}\" data-actions=\"page-actions\" \
             data-status=\"page-status\"></iframe>"
        )),
        "{body}"
    );
    assert!(body.contains("<div class=\"actions\" id=\"page-actions\""));
    assert!(frame
        .body
        .contains("{ type: \"meridian:theme\", version: seamless ? 3 : 2,"));

    // At a page of the plugin's, where the area is asked for one.
    let at = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}?path=%2Fholdings%3Fpage%3D2"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert!(
        at.body
            .contains("enter?path=%2Fholdings%3Fpage%3D2&amp;level=read&amp;"),
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
async fn each_button_shows_the_pages_at_its_level_and_no_way_to_the_portal_even_to_an_admin() {
    let h = harness(&[INSTANCE]).await;
    let mut held = admin_records();
    held.access_groups = records(&[INSTANCE]).access_groups;
    held.permissions.extend(records(&[INSTANCE]).permissions);
    h.app.records.store(held, h.app.clock.now_ns());
    h.app.health.hear(
        INSTANCE,
        meridian_domain::v1::PluginReport {
            plugin_instance_id: INSTANCE.into(),
            registered: true,
            declared_interface: Some(InterfaceDeclaration {
                loopback_port: 8000,
                title: "SnapTrade".into(),
                pages: vec![
                    meridian_pb::v1::PageDeclaration {
                        path: "/admin/connections".into(),
                        title: "Connections".into(),
                        levels: vec![AccessLevel::Admin as i32],
                    },
                    meridian_pb::v1::PageDeclaration {
                        path: "/statements".into(),
                        title: "Statements".into(),
                        levels: vec![AccessLevel::Write as i32, AccessLevel::Read as i32],
                    },
                ],
            }),
            ..Default::default()
        },
    );
    let area = |level: &'static str| {
        let h = &h;
        async move {
            get(
                &h.app,
                DASHBOARD,
                &format!("/plugins/{INSTANCE}?level={level}"),
                &[dashboard_cookie(h)],
            )
            .await
            .body
        }
    };
    // Manage opens on the dashboard's own Summary, first in the tab row,
    // drawn here and not framed; the plugin's page at admin a tab away.
    let manage = area("admin").await;
    assert!(
        manage.contains(
            "data-tab=\"summary\" data-drawn class=\"here\" aria-current=\"page\">Summary</a>"
        ),
        "{manage}"
    );
    assert!(!manage.contains("<iframe"), "{manage}");
    assert!(!manage.contains("Statements"), "{manage}");
    // Its admin, a deployment admin too, is shown no way from here to its
    // tabs in the admin portal (the product owner, 2026-09-30).
    assert!(!manage.contains("/admin/plugins/"), "{manage}");
    let connections = area("admin&tab=connections").await;
    assert!(
        connections.contains(
            "data-page=\"/admin/connections\" class=\"here\" aria-current=\"page\">Connections</a>"
        ),
        "{connections}"
    );
    assert!(connections.contains("enter?path=%2Fadmin%2Fconnections&amp;level=admin&amp;"));
    // Manage and View, the two she holds, to move between.
    assert!(manage.contains("<nav class=\"level-switch\""), "{manage}");
    let view = area("read").await;
    assert!(
        view.contains(
            "data-page=\"/statements\" class=\"here\" aria-current=\"page\">Statements</a>"
        ),
        "{view}"
    );
    assert!(!view.contains("Connections"), "{view}");
    assert!(view.contains("enter?path=%2Fstatements&amp;level=read&amp;"));
}

// ── Settings and status under Manage, drawn by the dashboard ─────────────

/// Obviously not a real credential, and long enough to find in a page.
const TYPED_SECRET: &str = "sk-test-typed-into-the-area-5c1e";

/// SnapTrade's settings as the conductor keeps them: a secret that is set,
/// and how often to read, held at 900.
fn snaptrade_settings() -> meridian_domain::v1::PluginSettingsRecord {
    use meridian_pb::v1::{SettingDeclaration, SettingType};
    meridian_domain::v1::PluginSettingsRecord {
        plugin_instance_id: INSTANCE.into(),
        values: vec![meridian_domain::v1::PluginSettingValue {
            name: "poll_seconds".into(),
            value: "900".into(),
        }],
        secrets_set: vec!["client_id".into()],
        declared_settings: vec![
            SettingDeclaration {
                name: "client_id".into(),
                label: "Client ID".into(),
                r#type: SettingType::String as i32,
                required: true,
                secret: true,
                ..Default::default()
            },
            SettingDeclaration {
                name: "poll_seconds".into(),
                label: "Read every".into(),
                r#type: SettingType::Integer as i32,
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

/// SnapTrade as its sidecar reports it now: healthy, at contract v6, its
/// two pages at admin and Statements at write and read, and the figures it
/// reports, as the plugin-report fixture carries them.
fn snaptrade_reporting(h: &Harness) {
    fn page(path: &str, title: &str, levels: &[AccessLevel]) -> meridian_pb::v1::PageDeclaration {
        meridian_pb::v1::PageDeclaration {
            path: path.into(),
            title: title.into(),
            levels: levels.iter().map(|l| *l as i32).collect(),
        }
    }
    h.app.health.hear(
        INSTANCE,
        meridian_domain::v1::PluginReport {
            plugin_instance_id: INSTANCE.into(),
            registered: true,
            healthy: true,
            contract_version: "v6".into(),
            reported_at_ns: h.app.clock.now_ns(),
            figures: vec![
                PluginFigure {
                    label: "Connections".into(),
                    value: Some(FigureValue::Count(3)),
                    state: FigureState::Warn as i32,
                    why: "1 connection needs attention: the brokerage asked to reconnect".into(),
                    ..Default::default()
                },
                PluginFigure {
                    label: "Accounts reached".into(),
                    value: Some(FigureValue::Count(7)),
                    ..Default::default()
                },
                PluginFigure {
                    label: "Last read".into(),
                    value: Some(FigureValue::AtNs(1_790_380_500_000_000_000)),
                    ..Default::default()
                },
            ],
            declared_interface: Some(InterfaceDeclaration {
                loopback_port: 8000,
                title: "SnapTrade".into(),
                pages: vec![
                    page("/admin/connections", "Connections", &[AccessLevel::Admin]),
                    page("/admin/accounts", "Account links", &[AccessLevel::Admin]),
                    page(
                        "/statements",
                        "Statements",
                        &[AccessLevel::Write, AccessLevel::Read],
                    ),
                ],
            }),
            ..Default::default()
        },
    );
}

/// The tab row's keys, in order, and the one the page is on.
fn tab_row(body: &str) -> (Vec<String>, String) {
    let nav = body
        .split("<nav class=\"tabs view-tabs\"")
        .nth(1)
        .and_then(|rest| rest.split("</nav>").next())
        .unwrap_or_default();
    let mut keys = Vec::new();
    let mut here = String::new();
    for link in nav.split(" data-tab=\"").skip(1) {
        let key = link.split('"').next().unwrap().to_string();
        if link
            .split('>')
            .next()
            .unwrap()
            .contains("aria-current=\"page\"")
        {
            here = key.clone();
        }
        keys.push(key);
    }
    (keys, here)
}

/// A form posted to the dashboard as Ada.
async fn post_form(h: &Harness, path: &str, form: &str) -> Answer {
    let response = router(Arc::clone(&h.app))
        .oneshot(
            HttpRequest::builder()
                .method(Method::POST)
                .uri(path)
                .header(HOST, DASHBOARD)
                .header(COOKIE, dashboard_cookie(h))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form.to_string()))
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

/// Under Manage the dashboard draws its own Summary and Settings tabs first
/// in the one tab row -- Summary, Settings, Connections, Account links for
/// SnapTrade -- and the area opens on Summary (the product owner,
/// 2026-10-01): the plugin's status, then the figures it reports as tiles,
/// in its order, a figure's state the tile's mark and its why the note.
/// Settings is the admin portal's settings form alone, which
/// posts to the area's own address, reaches the conductor as the person's,
/// and comes back to the tab. A secret's field is always empty, and what was
/// typed into it is in no page after.
#[tokio::test]
async fn under_manage_the_dashboard_draws_summary_then_settings_opens_on_summary_and_never_shows_a_secret(
) {
    let h = harness(&[INSTANCE]).await;
    let mut held = admin_records();
    held.access_groups = records(&[INSTANCE]).access_groups;
    held.permissions.extend(records(&[INSTANCE]).permissions);
    held.plugin_settings = vec![snaptrade_settings()];
    h.app.records.store(held, h.app.clock.now_ns());
    snaptrade_reporting(&h);
    serving_launched(&h);

    let manage = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}?level=admin"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(manage.status, StatusCode::OK, "{}", manage.body);
    let body = manage.body.split("</header>").nth(1).unwrap();
    assert_eq!(
        tab_row(body),
        (
            vec![
                "summary".into(),
                "settings".into(),
                "connections".into(),
                "account-links".into()
            ],
            "summary".into()
        )
    );
    assert!(!body.contains("<iframe"), "drawn, not framed: {body}");
    // Named, it is the same Summary.
    let named = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}?level=admin&tab=summary"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(named.status, StatusCode::OK, "{}", named.body);
    assert_eq!(tab_row(&named.body).1, "summary");
    assert!(named.body.contains("id=\"status\""), "{}", named.body);
    // The status: its health, its why on hover, the version the catalogue
    // launched and the contract it registered with, and the place kept for
    // restarting and moving versions, which are not built yet.
    let status = body
        .split("<section class=\"panel padded\" id=\"status\">")
        .nth(1)
        .and_then(|rest| rest.split("</section>").next())
        .expect("the status panel");
    assert!(
        status.contains("<span class=\"badge good\">Healthy</span>"),
        "{status}"
    );
    assert!(status.contains("<dd data-version>0.1.0</dd>"), "{status}");
    assert!(status.contains("<dd data-contract>v6</dd>"), "{status}");
    assert!(status.contains("data-reserved=\"lifecycle\""), "{status}");
    assert!(!status.contains("<button"), "a place kept, not buttons yet");
    // Then the plugin's figures, below core's status, as tiles in its order:
    // Connections marked warn, its why the note beside it; Last read a time
    // as every moment here is shown. No settings form on Summary.
    let figures = body
        .split("</section><section class=\"figures\" id=\"figures\" aria-label=\"What the plugin reports\">")
        .nth(1)
        .and_then(|rest| rest.split("</section>").next())
        .expect("the figures, after the status");
    let labels: Vec<&str> = figures
        .split("<span class=\"figure-label\">")
        .skip(1)
        .map(|rest| rest.split('<').next().unwrap())
        .collect();
    assert_eq!(labels, ["Connections", "Accounts reached", "Last read"]);
    assert!(
        figures.contains(
            "<button type=\"button\" class=\"status-dot\" data-state=\"warn\" \
             aria-label=\"Needs attention\" data-note=\"Needs attention\" \
             aria-describedby=\"figures-0-why\"></button>"
        ),
        "{figures}"
    );
    assert!(
        figures.contains("id=\"figures-0-why\">1 connection needs attention"),
        "{figures}"
    );
    assert!(
        figures.contains("<p class=\"figure-value\">2026-09-25 23:55 UTC</p>"),
        "{figures}"
    );
    assert!(!body.contains("id=\"settings\""), "{body}");
    assert!(!body.contains("<form"), "{body}");

    // Settings: the form alone, posted here, its secret's field empty.
    let settings = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}?level=admin&tab=settings"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(settings.status, StatusCode::OK, "{}", settings.body);
    let settings = settings.body.split("</header>").nth(1).unwrap();
    assert_eq!(tab_row(settings).1, "settings");
    for summary in [
        "id=\"status\"",
        "id=\"figures\"",
        "data-reserved",
        "data-version",
    ] {
        assert!(!settings.contains(summary), "{summary} in {settings}");
    }
    assert!(
        settings.contains(&format!("action=\"/plugins/{INSTANCE}/settings\"")),
        "{settings}"
    );
    assert!(
        settings.contains("type=\"password\" name=\"secret.client_id\" value=\"\""),
        "{settings}"
    );
    // Of this plugin alone: nothing of the deployment's, no account's data,
    // no Access tab, and no way into the admin portal.
    for page in [body, settings] {
        for elsewhere in [
            "Growth",
            "ACC-1",
            "Operations",
            "id=\"access\"",
            "/admin/plugins/",
            "/admin#",
        ] {
            assert!(!page.contains(elsewhere), "{elsewhere} in {page}");
        }
    }

    // Saved: to the conductor as Ada's, the secret with it and nowhere else,
    // and back to the tab, saying so.
    let asked: Arc<Mutex<Vec<(meridian_domain::v1::SetPluginSettingsRequest, String)>>> =
        Arc::default();
    let keeping = Arc::clone(&asked);
    h.app.bus.serve(
        "platform.config.command.set-plugin-settings",
        move |envelope| {
            let request =
                meridian_domain::v1::SetPluginSettingsRequest::decode(&envelope.payload[..])
                    .unwrap();
            let by = envelope.meta.clone().unwrap_or_default().acting_for_subject;
            keeping.lock().unwrap().push((request, by));
            Ok(("".into(), snaptrade_settings().encode_to_vec()))
        },
    );
    let token = h
        .app
        .sessions
        .find(&h.session, h.app.clock.now_ns())
        .unwrap()
        .form_token;
    let saved = post_form(
        &h,
        &format!("/plugins/{INSTANCE}/settings"),
        &format!("form_token={token}&secret.client_id={TYPED_SECRET}&value.poll_seconds=600"),
    )
    .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    assert_eq!(
        saved.headers[LOCATION],
        format!("/plugins/{INSTANCE}?level=admin&tab=settings&saved=1").as_str()
    );
    assert!(!saved.body.contains(TYPED_SECRET));
    {
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        let (request, by) = &asked[0];
        assert_eq!(by, ADA);
        let named: Vec<(&str, &str)> = request
            .values
            .iter()
            .map(|v| (v.name.as_str(), v.value.as_str()))
            .collect();
        assert_eq!(
            named,
            [("client_id", TYPED_SECRET), ("poll_seconds", "600")]
        );
    }
    let back = get(
        &h.app,
        DASHBOARD,
        saved.headers[LOCATION].to_str().unwrap(),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert!(
        back.body.contains("<p class=\"notice good\">Saved.</p>"),
        "{}",
        back.body
    );
    assert_eq!(tab_row(&back.body).1, "settings");
    assert!(!back.body.contains(TYPED_SECRET));
    // Without the session's token, nothing is sent.
    let forged = post_form(
        &h,
        &format!("/plugins/{INSTANCE}/settings"),
        &format!("secret.client_id={TYPED_SECRET}"),
    )
    .await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    assert!(!forged.body.contains(TYPED_SECRET));
    assert_eq!(asked.lock().unwrap().len(), 1);
}

/// The head under Manage, as the product owner agreed it on 2026-10-01 ("yes,
/// dot on every tab"; the level switch "consistent across all level pages"):
/// Summary and Settings, which the dashboard draws, and Account links, a
/// framed page of the plugin's, each have the status dot right after the
/// plugin's name -- the plugin's health as the dashboard knows it, until and
/// unless the framed page tells its own -- and the same switch in the same
/// place, the last of the head's right-hand group, with a framed page's
/// actions immediately left of it ("maybe the circular arrow to the left of
/// the toggle").
#[tokio::test]
async fn every_tab_under_manage_has_the_dot_after_the_name_and_the_switch_in_the_same_place() {
    let h = harness(&[INSTANCE]).await;
    let mut held = admin_records();
    held.access_groups = records(&[INSTANCE]).access_groups;
    held.permissions.extend(records(&[INSTANCE]).permissions);
    held.plugin_settings = vec![snaptrade_settings()];
    h.app.records.store(held, h.app.clock.now_ns());
    snaptrade_reporting(&h);
    serving_launched(&h);

    let dot = "<span class=\"title-status\" id=\"page-status\"><button type=\"button\" \
               class=\"status-dot\" data-state=\"ok\" aria-label=\"Healthy\" data-note=\"Healthy\">\
               </button></span>";
    let mut sides = Vec::new();
    for tab in ["summary", "settings", "account-links"] {
        let page = get(
            &h.app,
            DASHBOARD,
            &format!("/plugins/{INSTANCE}?level=admin&tab={tab}"),
            &[dashboard_cookie(&h)],
        )
        .await;
        assert_eq!(page.status, StatusCode::OK, "{tab}: {}", page.body);
        let body = page.body.split("</header>").nth(1).unwrap();
        assert_eq!(tab_row(body).1, tab);
        let head = body
            .split("<div class=\"page-head\">")
            .nth(1)
            .and_then(|rest| rest.split("<nav class=\"tabs view-tabs\"").next())
            .expect("the area's head");
        // The dot right after the name, on every tab.
        assert!(
            head.contains(&format!("<h1 title=\"snaptrade\">snaptrade</h1>{dot}")),
            "{tab}: {head}"
        );
        // A framed page's actions left of the switch, the group's last;
        // nothing of the page's beside the name.
        let title = head.split("<div class=\"head-side\">").next().unwrap();
        let side = head
            .split("<div class=\"head-side\">")
            .nth(1)
            .expect("the right-hand group");
        assert!(!title.contains("actions"), "{tab}: {title}");
        assert_eq!(
            side.starts_with("<div class=\"actions\" id=\"page-actions\""),
            tab == "account-links",
            "{tab}: {side}"
        );
        let switch = if tab == "account-links" {
            side.split_once("</div>").expect("the actions' end").1
        } else {
            side
        };
        assert!(switch.contains("data-level=\"admin\""), "{tab}: {switch}");
        sides.push(switch.to_string());
    }
    // The switch the same, in the same place, on all three.
    assert!(sides.windows(2).all(|w| w[0] == w[1]), "{sides:?}");
    assert!(sides[0].contains("data-level=\"admin\""), "{}", sides[0]);
}

/// The dashboard's Summary and Settings are the plugin's admins' alone, a
/// deployment admin being one through All plugins (admin): a reader or a
/// writer sees neither tab under the buttons they hold and is refused both
/// under Manage, and the form, as is the admin of another plugin.
#[tokio::test]
async fn the_dashboards_summary_and_settings_are_for_the_plugins_admins_alone() {
    let h = harness(&[INSTANCE]).await;
    snaptrade_reporting(&h);
    let settings_of = |level: AccessLevel, on: &str| {
        let mut held = records(&[INSTANCE]);
        held.access_groups[0].entries = vec![AccessEntry {
            plugin_instance_id: on.into(),
            level: level as i32,
        }];
        if on != INSTANCE {
            // Reading this one too, so only Manage is refused.
            held.access_groups[0].entries.push(AccessEntry {
                plugin_instance_id: INSTANCE.into(),
                level: AccessLevel::Read as i32,
            });
        }
        held.plugin_settings = vec![snaptrade_settings()];
        held
    };
    let token = h
        .app
        .sessions
        .find(&h.session, h.app.clock.now_ns())
        .unwrap()
        .form_token;
    for (level, on, holding) in [
        (AccessLevel::Read, INSTANCE, "read"),
        (AccessLevel::Write, INSTANCE, "write"),
        (AccessLevel::Admin, "snaptrade-2", "read"),
    ] {
        h.app
            .records
            .store(settings_of(level, on), h.app.clock.now_ns());
        let own = get(
            &h.app,
            DASHBOARD,
            &format!("/plugins/{INSTANCE}?level={holding}"),
            &[dashboard_cookie(&h)],
        )
        .await;
        assert_eq!(own.status, StatusCode::OK, "{}", own.body);
        let drawn = ["id=\"status\"", "id=\"figures\"", "id=\"settings\""];
        let row = tab_row(&own.body).0;
        assert!(
            !row.contains(&"summary".to_string())
                && !row.contains(&"settings".to_string())
                && drawn.iter().all(|part| !own.body.contains(part)),
            "{level:?} on {on}: {}",
            own.body
        );
        for tab in ["summary", "settings"] {
            for asked in [
                format!("/plugins/{INSTANCE}?level=admin&tab={tab}"),
                format!("/plugins/{INSTANCE}?level=admin"),
                format!("/plugins/{INSTANCE}?level={holding}&tab={tab}"),
            ] {
                let page = get(&h.app, DASHBOARD, &asked, &[dashboard_cookie(&h)]).await;
                for part in drawn {
                    assert!(
                        !page.body.contains(part),
                        "{asked}: {part} in {}",
                        page.body
                    );
                }
                if asked.contains("level=admin") {
                    assert_eq!(page.status, StatusCode::FORBIDDEN, "{asked}");
                }
            }
        }
        let posted = post_form(
            &h,
            &format!("/plugins/{INSTANCE}/settings"),
            &format!("form_token={token}&value.poll_seconds=600"),
        )
        .await;
        assert_eq!(posted.status, StatusCode::FORBIDDEN, "{level:?} on {on}");
    }
}

/// Entered at a level with no page named -- a Home button's way in, or
/// `meridian plugin open --level` from a terminal -- the person lands on the
/// first page the plugin declares at that level, the area's first tab
/// there, and on its `/` only where it declares none at that level: a `/`
/// serving Open and View alone (the 0.10.0 scaffold's Accounts) would refuse
/// Manage with the plugin's 403.
#[tokio::test]
async fn entered_with_no_page_named_it_lands_on_the_first_page_at_that_level() {
    let h = harness(&[INSTANCE]).await;
    let mut held = admin_records();
    held.access_groups = records(&[INSTANCE]).access_groups;
    held.permissions.extend(records(&[INSTANCE]).permissions);
    h.app.records.store(held, h.app.clock.now_ns());
    let declare = |pages: &[(&str, &str, &[AccessLevel])]| {
        h.app.health.hear(
            INSTANCE,
            meridian_domain::v1::PluginReport {
                plugin_instance_id: INSTANCE.into(),
                registered: true,
                declared_interface: Some(InterfaceDeclaration {
                    loopback_port: 8000,
                    title: "Accounts".into(),
                    pages: pages
                        .iter()
                        .map(|(path, title, levels)| meridian_pb::v1::PageDeclaration {
                            path: path.to_string(),
                            title: title.to_string(),
                            levels: levels.iter().map(|l| *l as i32).collect(),
                        })
                        .collect(),
                }),
                ..Default::default()
            },
        )
    };
    // Where the way in lands on the plugin's host, once redeemed.
    let lands = |entrance: String| {
        let h = &h;
        async move {
            let prefix = format!("https://{PLUGIN_HOST}");
            let path = entrance
                .strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("{entrance}"))
                .to_string();
            let redeemed = get(&h.app, PLUGIN_HOST, &path, &[]).await;
            assert_eq!(redeemed.status, StatusCode::SEE_OTHER, "{}", redeemed.body);
            redeemed.headers[LOCATION].to_str().unwrap().to_string()
        }
    };
    let from_home = |level: &'static str| {
        let h = &h;
        async move {
            let answer = get(
                &h.app,
                DASHBOARD,
                &format!("/plugins/{INSTANCE}/enter?level={level}"),
                &[dashboard_cookie(h)],
            )
            .await;
            assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
            answer.headers[LOCATION].to_str().unwrap().to_string()
        }
    };
    let session = terminal(&h.app).await;
    let from_terminal = |level: &'static str| {
        let h = &h;
        let session = session.clone();
        async move {
            let opened = develop(
                &h.app,
                Method::POST,
                &format!("/terminal/plugins/{INSTANCE}/open?level={level}"),
                &session,
                "",
            )
            .await;
            assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
            let said: serde_json::Value = serde_json::from_str(&opened.body).unwrap();
            said["url"].as_str().unwrap().to_string()
        }
    };
    const ADMIN: &[AccessLevel] = &[AccessLevel::Admin];
    const DATA: &[AccessLevel] = &[AccessLevel::Write, AccessLevel::Read];
    declare(&[
        ("/", "Accounts", DATA),
        ("/setup", "Setup", ADMIN),
        ("/admin/links", "Links", ADMIN),
    ]);
    assert_eq!(lands(from_home("admin").await).await, "/setup");
    assert_eq!(lands(from_terminal("admin").await).await, "/setup");
    assert_eq!(lands(from_home("read").await).await, "/");
    assert_eq!(lands(from_terminal("read").await).await, "/");
    // A page named is the page entered, whatever the level's first.
    let named = get(
        &h.app,
        DASHBOARD,
        &format!("/plugins/{INSTANCE}/enter?level=admin&path=%2Fadmin%2Flinks"),
        &[dashboard_cookie(&h)],
    )
    .await;
    assert_eq!(
        lands(named.headers[LOCATION].to_str().unwrap().to_string()).await,
        "/admin/links"
    );
    // Declaring no page at Manage, its `/`.
    declare(&[("/", "Accounts", DATA)]);
    assert_eq!(lands(from_home("admin").await).await, "/");
    assert_eq!(lands(from_terminal("admin").await).await, "/");
}

#[tokio::test]
async fn the_frames_way_in_lands_on_the_page_asked_for_carrying_the_theme() {
    let h = harness(&[INSTANCE]).await;
    let answer = get(
        &h.app,
        DASHBOARD,
        &format!(
            "/plugins/{INSTANCE}/enter?path=%2Fadmin%3Ftab%3D2&om-scheme=harbour&om-mode=dark\
             &om-direction=red-up&om-framed=1"
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
        "/admin?tab=2&om-scheme=harbour&om-mode=dark&om-direction=red-up&om-framed=1"
    );

    // Nothing the kit would not take is carried onto the plugin's address,
    // and nowhere but a page on the plugin's own host.
    let odd = get(
        &h.app,
        DASHBOARD,
        &format!(
            "/plugins/{INSTANCE}/enter?om-scheme=Evil%22Scheme&om-mode=sepia&om-direction=up\
             &om-framed=yes"
        ),
        &[dashboard_cookie(&h)],
    )
    .await;
    let location = odd.headers[LOCATION].to_str().unwrap();
    let path = &location[format!("https://{PLUGIN_HOST}").len()..];
    let redeemed = get(&h.app, PLUGIN_HOST, path, &[]).await;
    assert_eq!(
        redeemed.headers[LOCATION],
        "/?om-scheme=default&om-mode=system&om-direction=green-up&om-framed=0"
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
    // No session: the kit is the same files for everybody. The image
    // carries 0.3.0, and a page that pinned 0.1.0 still gets it: any 0.x is
    // the newest 0.x carried (the product owner, 2026-09-30).
    for version in ["0.3.0", "0.1.0"] {
        let kit = get(
            &h.app,
            PLUGIN_HOST,
            &format!("/.meridian/ui/{version}/meridian.css"),
            &[],
        )
        .await;
        assert_eq!(kit.status, StatusCode::OK, "{version}: {}", kit.body);
        assert_eq!(kit.headers["content-type"], "text/css; charset=utf-8");
        assert_eq!(kit.body, ":root{}");
    }
    assert_eq!(
        get(&h.app, PLUGIN_HOST, "/.meridian/ui/1.0.0/meridian.css", &[])
            .await
            .status,
        StatusCode::NOT_FOUND,
        "another major is not carried"
    );
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
