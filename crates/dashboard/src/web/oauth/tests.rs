//! The OAuth endpoints over HTTP, on the branch where this deployment holds
//! the accounts: the fixtures register-client, authorise-client,
//! issue-client-token and revoke-clients-delegation, and a token acting on
//! the CLI's surface and nowhere else.

use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, WWW_AUTHENTICATE};
use axum::http::Request;
use meridian_domain::v1::{
    AccessEntry, AccessGroup, AccessRecords, AccountGroup, AccountRecord, AccountState, Permission,
    UserGroup,
};
use tower::ServiceExt;

use super::*;
use crate::accounts::Accounts as _;
use crate::clock::{Clock, MINUTE_NS};
use crate::web::tests::{app_with, body_of, T0};
use crate::web::{caller_of, router, SESSION_COOKIE};

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const BACK: &str = "http://127.0.0.1:53682/callback";
const HOST_NAME: &str = "dash.firm.example";

/// A clock a test moves.
pub(in crate::web) struct Moving(pub AtomicI64);
impl Clock for Moving {
    fn now_ns(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

pub(in crate::web) fn records() -> AccessRecords {
    AccessRecords {
        accounts: vec![AccountRecord {
            account_id: "ACC-1".into(),
            name: "Main".into(),
            state: AccountState::Open as i32,
            ..Default::default()
        }],
        account_groups: vec![AccountGroup {
            account_group_id: "AG-1".into(),
            name: "Desk".into(),
            account_ids: vec!["ACC-1".into()],
            ..Default::default()
        }],
        access_groups: vec![AccessGroup {
            access_group_id: "AX-1".into(),
            name: "Traders".into(),
            entries: vec![AccessEntry {
                plugin_instance_id: "oms-1".into(),
                level: meridian_access::AccessLevel::Write as i32,
                role: String::new(),
            }],
            ..Default::default()
        }],
        user_groups: vec![UserGroup {
            user_group_id: "UG-1".into(),
            name: "Admins".into(),
            directory_groups: vec![],
            logins: vec!["local|ada".into()],
        }],
        permissions: vec![
            Permission {
                permission_id: "P-1".into(),
                user_group_id: "UG-1".into(),
                account_group_id: String::new(),
                access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
            },
            Permission {
                permission_id: "P-2".into(),
                user_group_id: "UG-1".into(),
                account_group_id: "AG-1".into(),
                access_group_id: "AX-1".into(),
            },
        ],
        ..Default::default()
    }
}

/// Ada's account held here, the records making her a deployment admin
/// writing on oms-1, a dashboard at https://dash.firm.example, and a clock
/// the test moves (the records read at every moment it moves to).
pub(in crate::web) fn app() -> (Arc<App>, Arc<Moving>) {
    let accounts = crate::accounts::InMemory::default();
    accounts
        .put(&crate::accounts::LocalAccount {
            name: "ada".into(),
            display_name: "Ada Park".into(),
            password_hash: crate::accounts::hash_password("correct horse battery").expect("hashed"),
            created_at_ns: T0,
            ..Default::default()
        })
        .expect("stored");
    let clock = Arc::new(Moving(AtomicI64::new(T0)));
    let app = app_with(Some(records()), T0, T0);
    let mut built = Arc::try_unwrap(app).ok().expect("one reference");
    built.accounts = Some(Arc::new(accounts));
    built.clock = clock.clone();
    built.public_url = format!("https://{HOST_NAME}");
    (Arc::new(built), clock)
}

/// Moved, and the records read again then, as the dashboard reads them
/// every 30 seconds.
pub(in crate::web) fn at(app: &App, clock: &Moving, now: i64) {
    clock.0.store(now, Ordering::SeqCst);
    app.records.store(records(), now);
}

/// The routes, and one terminal path answering with who it acts for.
fn routes(app: Arc<App>) -> Router {
    let probed = Arc::clone(&app);
    router(app).route(
        "/terminal/probe",
        get(move |headers: HeaderMap| async move {
            match caller_of(&probed, &headers).await {
                Ok(caller) => format!(
                    "{} {}",
                    caller.person.subject,
                    caller.delegation_id().unwrap_or("session")
                )
                .into_response(),
                Err(refusal) => *refusal,
            }
        }),
    )
}

pub(in crate::web) async fn send(app: &Arc<App>, request: Request<Body>) -> Response {
    routes(Arc::clone(app))
        .oneshot(request)
        .await
        .expect("a response")
}

async fn get_page(app: &Arc<App>, path: &str) -> Response {
    send(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn post_form(app: &Arc<App>, path: &str, body: String) -> Response {
    send(
        app,
        Request::post(path)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

async fn post_json(app: &Arc<App>, path: &str, body: serde_json::Value) -> Response {
    send(
        app,
        Request::post(path)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn json_of(response: Response) -> serde_json::Value {
    serde_json::from_str(&body_of(response).await).expect("JSON")
}

fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page
        .find(&marker)
        .unwrap_or_else(|| panic!("no {name} in {page}"))
        + marker.len();
    page[start..start + page[start..].find('"').unwrap()].to_string()
}

fn location(response: &Response) -> reqwest::Url {
    reqwest::Url::parse(
        response
            .headers()
            .get(LOCATION)
            .expect("a location")
            .to_str()
            .unwrap(),
    )
    .expect("an address")
}

fn query(url: &reqwest::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn encoded(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

pub(in crate::web) async fn register_cli(app: &Arc<App>) -> String {
    let response = post_json(
        app,
        "/oauth/register",
        serde_json::json!({
            "client_name": "meridian on ada-laptop",
            "redirect_uris": [BACK],
            "software_id": "meridian-cli",
            "token_endpoint_auth_method": "none",
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let said = json_of(response).await;
    assert_eq!(said["token_endpoint_auth_method"], "none");
    said["client_id"].as_str().expect("an id").to_string()
}

fn authorising(client_id: &str, resource: &str) -> String {
    format!(
        "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
         &code_challenge={CHALLENGE}&code_challenge_method=S256&state=st-1&resource={}",
        encoded(client_id),
        encoded(BACK),
        encoded(resource)
    )
}

fn terminal_resource() -> String {
    format!("https://{HOST_NAME}/terminal")
}

/// Through the sign-in to the consent page: its request and confirm.
async fn consenting(app: &Arc<App>, client_id: &str) -> (String, String, String) {
    consenting_for(app, client_id, &terminal_resource()).await
}

/// The same, for a resource named.
async fn consenting_for(
    app: &Arc<App>,
    client_id: &str,
    resource: &str,
) -> (String, String, String) {
    let asked = get_page(app, &authorising(client_id, resource)).await;
    assert_eq!(asked.status(), StatusCode::OK);
    let page = body_of(asked).await;
    assert!(page.contains("Sign in to allow a client"), "{page}");
    let id = hidden(&page, "authorize");
    let signed_in = post_form(
        app,
        "/sign-in",
        format!("name=ada&password=correct+horse+battery&authorize={id}"),
    )
    .await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    assert!(
        signed_in.headers().get(SET_COOKIE).is_none(),
        "consenting makes no browser session"
    );
    let page = body_of(signed_in).await;
    (hidden(&page, "request"), hidden(&page, "confirm"), page)
}

/// Consented with `answer`, and the code that came back.
async fn coded(app: &Arc<App>, client_id: &str, answer: &str) -> String {
    let (request, confirm, _) = consenting(app, client_id).await;
    let decided = post_form(
        app,
        "/oauth/authorize",
        format!("request={request}&confirm={confirm}&decision=allow&{answer}"),
    )
    .await;
    assert_eq!(
        decided.status(),
        StatusCode::FOUND,
        "{}",
        body_of(decided).await
    );
    let back = location(&decided);
    assert_eq!(back.path(), "/callback");
    assert_eq!(query(&back, "state").as_deref(), Some("st-1"));
    assert_eq!(query(&back, "iss"), Some(format!("https://{HOST_NAME}")));
    query(&back, "code").expect("a code")
}

async fn exchanged(app: &Arc<App>, client_id: &str, code: &str) -> Response {
    post_form(
        app,
        "/oauth/token",
        format!(
            "grant_type=authorization_code&code={code}&code_verifier={VERIFIER}\
             &redirect_uri={}&client_id={}&resource={}",
            encoded(BACK),
            encoded(client_id),
            encoded(&terminal_resource())
        ),
    )
    .await
}

/// All the way through, as `meridian connect` goes: the token answer.
pub(in crate::web) async fn connected(app: &Arc<App>, answer: &str) -> (String, serde_json::Value) {
    let client_id = register_cli(app).await;
    let code = coded(app, &client_id, answer).await;
    let response = exchanged(app, &client_id, &code).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    (client_id, json_of(response).await)
}

async fn refreshed(app: &Arc<App>, client_id: &str, refresh_token: &str) -> Response {
    post_form(
        app,
        "/oauth/token",
        format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            encoded(refresh_token),
            encoded(client_id)
        ),
    )
    .await
}

/// A deployment admin's terminal path, with a body that does not read: 403
/// when the caller is not admitted as one, 422 when they are.
async fn stopping(app: &Arc<App>, token: &str) -> Response {
    send(
        app,
        Request::post("/terminal/plugins/stop")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from("not json"))
            .unwrap(),
    )
    .await
}

async fn probe(app: &Arc<App>, token: &str) -> (StatusCode, String) {
    let response = send(
        app,
        Request::get("/terminal/probe")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    (response.status(), body_of(response).await)
}

#[tokio::test]
async fn the_metadata_names_the_endpoints_and_no_identity_server() {
    let (app, _) = app();
    let said = json_of(get_page(&app, "/.well-known/oauth-authorization-server").await).await;
    assert_eq!(said["issuer"], "https://dash.firm.example");
    assert_eq!(
        said["token_endpoint"],
        "https://dash.firm.example/oauth/token"
    );
    assert_eq!(
        said["code_challenge_methods_supported"],
        serde_json::json!(["S256"])
    );
    assert_eq!(
        said["grant_types_supported"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert!(said.get("jwks_uri").is_none() && said.get("userinfo_endpoint").is_none());
    let resource =
        json_of(get_page(&app, "/.well-known/oauth-protected-resource/terminal").await).await;
    assert_eq!(resource["resource"], "https://dash.firm.example/terminal");
    assert_eq!(
        resource["authorization_servers"],
        serde_json::json!(["https://dash.firm.example"])
    );
    assert_eq!(
        get_page(&app, "/.well-known/openid-configuration")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

/// A development deployment served over HTTPS at a `.localhost` name (task
/// kernel/a-development-deployment-serves-https): everything an MCP client
/// reads names that https origin, from MERIDIAN_DASHBOARD_URL alone, whatever
/// Host a request arrived with.
#[tokio::test]
async fn a_local_https_address_is_the_issuer_and_the_mcp_resource() {
    let (app, _) = app();
    let mut built = Arc::try_unwrap(app).ok().expect("one reference");
    built.public_url = "https://meridian.localhost".into();
    let app = Arc::new(built);
    let resource = json_of(get_page(&app, "/.well-known/oauth-protected-resource/mcp").await).await;
    assert_eq!(resource["resource"], "https://meridian.localhost/mcp");
    assert_eq!(
        resource["authorization_servers"],
        serde_json::json!(["https://meridian.localhost"])
    );
    let said = json_of(get_page(&app, "/.well-known/oauth-authorization-server").await).await;
    assert_eq!(said["issuer"], "https://meridian.localhost");
    assert_eq!(
        said["authorization_endpoint"],
        "https://meridian.localhost/oauth/authorize"
    );
}

#[tokio::test]
async fn registration_grants_nothing_and_refuses_what_it_does_not_take() {
    let (app, _) = app();
    for (body, why) in [
        (
            serde_json::json!({"client_name": "x", "redirect_uris": ["http://evil.example/cb"]}),
            "plain HTTP elsewhere",
        ),
        (
            serde_json::json!({"client_name": "x", "redirect_uris": [BACK], "token_endpoint_auth_method": "client_secret_basic"}),
            "a secret",
        ),
        (
            serde_json::json!({"client_name": "x", "redirect_uris": [BACK], "grant_types": ["client_credentials"]}),
            "another grant",
        ),
        (serde_json::json!({"redirect_uris": [BACK]}), "no name"),
    ] {
        let response = post_json(&app, "/oauth/register", body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{why}");
        assert_eq!(
            json_of(response).await["error"],
            "invalid_client_metadata",
            "{why}"
        );
    }
    let client_id = register_cli(&app).await;
    // Registered, and still nothing: its id alone acts for nobody.
    assert_eq!(probe(&app, &client_id).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_unknown_client_or_redirect_is_refused_on_a_page_and_sent_nowhere() {
    let (app, _) = app();
    let client_id = register_cli(&app).await;
    let unknown = get_page(&app, &authorising("mdc_nobody", &terminal_resource())).await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    assert!(unknown.headers().get(LOCATION).is_none());
    let elsewhere = get_page(
        &app,
        &authorising(&client_id, &terminal_resource())
            .replace(&encoded(BACK), &encoded("http://127.0.0.1:53682/elsewhere")),
    )
    .await;
    assert_eq!(elsewhere.status(), StatusCode::BAD_REQUEST);
    assert!(elsewhere.headers().get(LOCATION).is_none());
    let page = body_of(elsewhere).await;
    assert!(!page.contains("password"), "no sign-in offered: {page}");
}

#[tokio::test]
async fn a_known_client_asking_wrongly_is_sent_back_with_why() {
    let (app, _) = app();
    let client_id = register_cli(&app).await;
    let plain = get_page(
        &app,
        &authorising(&client_id, &terminal_resource()).replace("S256", "plain"),
    )
    .await;
    assert_eq!(plain.status(), StatusCode::FOUND);
    assert_eq!(
        query(&location(&plain), "error").as_deref(),
        Some("invalid_request")
    );
    for resource in [
        "https://elsewhere.example/terminal",
        "https://elsewhere.example/mcp",
        "https://dash.firm.example/other",
        "",
    ] {
        let wrong = get_page(&app, &authorising(&client_id, resource)).await;
        assert_eq!(wrong.status(), StatusCode::FOUND, "{resource}");
        let back = location(&wrong);
        assert_eq!(query(&back, "error").as_deref(), Some("invalid_target"));
        assert_eq!(query(&back, "state").as_deref(), Some("st-1"));
    }
}

#[tokio::test]
async fn the_consent_page_names_the_client_where_its_codes_go_and_the_clis_default() {
    let (app, _) = app();
    let client_id = register_cli(&app).await;
    let (_, _, page) = consenting(&app, &client_id).await;
    assert!(page.contains("meridian on ada-laptop"), "{page}");
    assert!(page.contains("<code>127.0.0.1</code>"), "{page}");
    assert!(page.contains("Ada Park"), "{page}");
    assert!(
        page.contains("value=\"everything\" checked"),
        "the CLI's default is everything: {page}"
    );
    assert!(page.contains("value=\"90\" selected"), "{page}");
    // What she holds is what she may tick, and nothing else is offered.
    assert!(page.contains("value=\"oms-1::write\""), "{page}");
    assert!(page.contains("value=\"oms-1::read\""), "{page}");
    assert!(!page.contains("value=\"oms-1::admin\""), "{page}");
    assert!(page.contains("name=\"deployment_admin\""), "{page}");
    assert!(page.contains("value=\"AG-1\""), "{page}");
}

#[tokio::test]
async fn consent_for_the_mcp_surface_lists_the_tools_each_row_reaches() {
    // Contract v12 (W6.17, W6.20, Q5): rows of access, each with its tools,
    // never single tools; the CLI's own page lists none.
    let (app, _) = app();
    let client_id = register_cli(&app).await;
    let (_, _, page) = consenting_for(&app, &client_id, &format!("https://{HOST_NAME}/mcp")).await;
    assert!(page.contains("Complete instrument records"), "{page}");
    assert!(page.contains("those added later included"), "{page}");
    let (_, _, terminal) = consenting(&app, &register_cli(&app).await).await;
    assert!(
        !terminal.contains("Complete instrument records"),
        "{terminal}"
    );
}

#[tokio::test]
async fn a_cli_connects_and_its_token_acts_on_the_terminals_paths_alone() {
    let (app, _) = app();
    let (_, said) = connected(&app, "covers=everything&days=90").await;
    assert_eq!(said["token_type"], "Bearer");
    assert_eq!(said["expires_in"], 600);
    assert_eq!(said["subject"], "local|ada");
    assert_eq!(said["delegation_expires_at"], "2026-12-25T00:00:00Z");
    assert_eq!(said["delegation_lapses_soon"], false);
    let access = said["access_token"].as_str().unwrap();
    let delegation = said["delegation_id"].as_str().unwrap();
    assert!(access.starts_with("mda_"));
    assert!(said["refresh_token"].as_str().unwrap().starts_with("mdr_"));

    assert_eq!(
        probe(&app, access).await,
        (StatusCode::OK, format!("local|ada {delegation}"))
    );
    // On a browser's path a token is nothing: the home asks for a sign-in.
    let home = send(
        &app,
        Request::get("/")
            .header(AUTHORIZATION, format!("Bearer {access}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(body_of(home).await.contains("Sign in"));
    // And a browser's cookie is nothing on the terminal's.
    let key = app.sessions.start("local|ada", "Ada Park", vec![], T0);
    let cookied = send(
        &app,
        Request::get("/terminal/probe")
            .header(COOKIE, format!("__Host-{SESSION_COOKIE}={key}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(cookied.status(), StatusCode::UNAUTHORIZED);
    let challenge = cookied.headers()[WWW_AUTHENTICATE]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.contains(
            "resource_metadata=\"https://dash.firm.example/.well-known/oauth-protected-resource/terminal\""
        ),
        "{challenge}"
    );
}

#[tokio::test]
async fn an_access_token_lapses_in_ten_minutes_and_a_refresh_brings_the_next() {
    let (app, clock) = app();
    let (client_id, said) = connected(&app, "covers=everything&days=90").await;
    let access = said["access_token"].as_str().unwrap().to_string();
    let refresh = said["refresh_token"].as_str().unwrap().to_string();

    at(&app, &clock, T0 + 10 * MINUTE_NS + 1);
    let (status, body) = probe(&app, &access).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("\"reason\":\"expired\""), "{body}");

    let next = refreshed(&app, &client_id, &refresh).await;
    assert_eq!(next.status(), StatusCode::OK);
    let next = json_of(next).await;
    assert_eq!(next["delegation_id"], said["delegation_id"]);
    assert_eq!(
        probe(&app, next["access_token"].as_str().unwrap()).await.0,
        StatusCode::OK
    );

    // A delegation outlives decisions/015's 12 hours by refreshing.
    let mut refresh = next["refresh_token"].as_str().unwrap().to_string();
    let mut now = T0 + 10 * MINUTE_NS + 1;
    for _ in 0..3 {
        now += 6 * crate::clock::HOUR_NS;
        at(&app, &clock, now);
        let next = json_of(refreshed(&app, &client_id, &refresh).await).await;
        refresh = next["refresh_token"]
            .as_str()
            .expect("refreshed")
            .to_string();
        assert_eq!(
            probe(&app, next["access_token"].as_str().unwrap()).await.0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn a_refresh_token_presented_twice_revokes_the_delegation() {
    let (app, _) = app();
    let (client_id, said) = connected(&app, "covers=everything&days=30").await;
    let first = said["refresh_token"].as_str().unwrap();
    let next = json_of(refreshed(&app, &client_id, first).await).await;
    let again = refreshed(&app, &client_id, first).await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    let again = json_of(again).await;
    assert_eq!(again["error"], "invalid_grant");
    assert_eq!(again["reason"], "reused");
    let (status, body) = probe(&app, next["access_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("\"reason\":\"revoked\""), "{body}");
}

#[tokio::test]
async fn a_code_presented_twice_revokes_what_it_granted() {
    let (app, _) = app();
    let client_id = register_cli(&app).await;
    let code = coded(&app, &client_id, "covers=everything&days=30").await;
    let first = json_of(exchanged(&app, &client_id, &code).await).await;
    let second = exchanged(&app, &client_id, &code).await;
    assert_eq!(second.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_of(second).await["reason"], "reused");
    assert_eq!(
        probe(&app, first["access_token"].as_str().unwrap()).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_client_revokes_its_own_delegation() {
    let (app, _) = app();
    let (client_id, said) = connected(&app, "covers=everything&days=30").await;
    let revoked = post_form(
        &app,
        "/oauth/revoke",
        format!(
            "token={}&token_type_hint=refresh_token&client_id={}",
            encoded(said["refresh_token"].as_str().unwrap()),
            encoded(&client_id)
        ),
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::OK);
    let (status, body) = probe(&app, said["access_token"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("revoked"), "{body}");
    // Revoking what is already gone, or was never issued, says the same.
    let again = post_form(
        &app,
        "/oauth/revoke",
        format!("token=mdr_never&client_id={}", encoded(&client_id)),
    )
    .await;
    assert_eq!(again.status(), StatusCode::OK);
}

#[tokio::test]
async fn declining_sends_the_client_a_refusal_and_no_code() {
    let (app, _) = app();
    let client_id = register_cli(&app).await;
    let (request, confirm, _) = consenting(&app, &client_id).await;
    let declined = post_form(
        &app,
        "/oauth/authorize",
        format!("request={request}&confirm={confirm}&decision=deny"),
    )
    .await;
    assert_eq!(declined.status(), StatusCode::FOUND);
    let back = location(&declined);
    assert_eq!(query(&back, "error").as_deref(), Some("access_denied"));
    assert_eq!(query(&back, "code"), None);
}

#[tokio::test]
async fn a_person_consents_only_to_what_they_hold() {
    let (app, _) = app();
    let client_id = register_cli(&app).await;
    for answer in [
        "covers=some&level=oms-1:admin&days=30",
        "covers=some&level=other-1:read&days=30",
        "covers=some&account_group=AG-9&days=30",
        "covers=everything&days=365",
    ] {
        let (request, confirm, _) = consenting(&app, &client_id).await;
        let refused = post_form(
            &app,
            "/oauth/authorize",
            format!("request={request}&confirm={confirm}&decision=allow&{answer}"),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{answer}");
        assert!(refused.headers().get(LOCATION).is_none(), "{answer}");
    }
}

#[tokio::test]
async fn a_narrowed_delegation_is_refused_where_it_does_not_reach() {
    let (app, _) = app();
    let (_, narrowed) = connected(
        &app,
        "covers=some&level=oms-1:read&account_group=AG-1&days=30",
    )
    .await;
    let access = narrowed["access_token"].as_str().unwrap();
    // Not the deployment admin's capabilities, though she holds them.
    let listing = stopping(&app, access).await;
    assert_eq!(listing.status(), StatusCode::FORBIDDEN);
    assert!(body_of(listing)
        .await
        .contains("does not cover the deployment admin"));
    // And Open on oms-1 is not View.
    let opened = send(
        &app,
        Request::post("/terminal/plugins/oms-1/open?level=write")
            .header(AUTHORIZATION, format!("Bearer {access}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(opened.status(), StatusCode::FORBIDDEN);
    // View is: refused only because this dashboard serves no plugin pages.
    let viewed = send(
        &app,
        Request::post("/terminal/plugins/oms-1/open?level=read")
            .header(AUTHORIZATION, format!("Bearer {access}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(viewed.status(), StatusCode::SERVICE_UNAVAILABLE);
    let delegation = app
        .delegations
        .delegation(narrowed["delegation_id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(
        delegation.last_refusal.is_some(),
        "the refusal is recorded beside the delegation"
    );
}

#[tokio::test]
async fn everything_reaches_the_deployment_admins_capabilities() {
    let (app, _) = app();
    let (_, said) = connected(&app, "covers=everything&days=30").await;
    let listing = stopping(&app, said["access_token"].as_str().unwrap()).await;
    assert_eq!(
        listing.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "admitted as a deployment admin, then its body read"
    );
}

#[tokio::test]
async fn an_account_removed_ends_its_delegations_at_the_next_refresh() {
    let (app, _) = app();
    let (client_id, said) = connected(&app, "covers=everything&days=30").await;
    let mut built = Arc::try_unwrap(app).ok().expect("one reference");
    // The same dashboard, its accounts emptied: the account is gone.
    built.accounts = Some(Arc::new(crate::accounts::InMemory::default()));
    let app = Arc::new(built);
    let refused = refreshed(&app, &client_id, said["refresh_token"].as_str().unwrap()).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_of(refused).await["reason"], "revoked");
}

#[tokio::test]
async fn a_withdrawn_permission_reaches_a_delegation_at_its_next_request() {
    let (app, _) = app();
    let (_, said) = connected(&app, "covers=everything&days=30").await;
    let access = said["access_token"].as_str().unwrap();
    let mut withdrawn = records();
    withdrawn
        .permissions
        .retain(|p| p.access_group_id != meridian_access::DEPLOYMENT_ADMIN);
    app.records.store(withdrawn, T0);
    let listing = stopping(&app, access).await;
    assert_eq!(listing.status(), StatusCode::FORBIDDEN);
}

// ── The consent page at a firm's scale (kernel/the-consent-page-at-scale) ──

/// A client that is not the CLI, sending its codes to the same address.
async fn register_agent(app: &Arc<App>) -> String {
    let response = post_json(
        app,
        "/oauth/register",
        serde_json::json!({ "client_name": "Claude", "redirect_uris": [BACK] }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    json_of(response).await["client_id"]
        .as_str()
        .expect("an id")
        .to_string()
}

/// Between `<div class="choices">` and the end of its fieldset: the
/// individual choices.
fn choices_of(page: &str) -> &str {
    let start = page.find("<div class=\"choices\">").expect("choices");
    let end = start
        + page[start..]
            .find("</div></fieldset><label>Until")
            .expect("their end");
    &page[start..end]
}

#[tokio::test]
async fn the_individual_choices_show_only_for_only_what_is_ticked_and_need_no_script() {
    let (app, _) = app();
    let (_, _, page) = consenting(&app, &register_cli(&app).await).await;
    // One question at a time: while Everything is chosen the choices and
    // their summary are hidden, by the stylesheet alone; Only what is ticked
    // shows them. Nothing about it is the script's.
    assert!(page.contains(
        "form.consent:has(input[name=covers][value=everything]:checked) .choices,\
         form.consent:has(input[name=covers][value=everything]:checked) .summary-some,\
         form.consent:has(input[name=covers][value=some]:checked) .summary-everything{display:none}"
    ));
    // Never hidden by an attribute, which only a script could take away.
    assert!(page.contains("<div class=\"choices\"><fieldset"), "{page}");
    let choices = choices_of(&page);
    for control in [
        "name=\"deployment_admin\"",
        "name=\"level\"",
        "name=\"account_group\"",
    ] {
        assert!(choices.contains(control), "{control} among the choices");
        assert!(
            !page.replacen(choices, "", 1).contains(control),
            "{control} only among the choices"
        );
    }
    // The form's controls are plain ones: posted as they are without the
    // script, which only searches and says the summary again.
    assert!(page.contains("<form method=\"post\" action=\"/oauth/authorize\" class=\"consent\">"));
}

#[tokio::test]
async fn a_new_client_starts_with_nothing_ticked_and_everything_offered_not_chosen() {
    let (app, _) = app();
    let (_, _, page) = consenting(&app, &register_agent(&app).await).await;
    assert!(page.contains("value=\"everything\">"), "offered: {page}");
    assert!(!page.contains("value=\"everything\" checked"), "{page}");
    assert!(page.contains("value=\"some\" checked"), "{page}");
    let choices = choices_of(&page);
    assert!(!choices.contains(" checked"), "nothing ticked: {choices}");
    assert!(
        choices.contains("<option value=\"\" selected>Nothing</option>"),
        "{choices}"
    );
    assert!(!choices.contains("\" selected>View"), "{choices}");
    assert!(page.contains("value=\"30\" selected"), "{page}");
    assert!(
        page.contains("Nothing is ticked, so it could reach nothing."),
        "{page}"
    );
}

#[tokio::test]
async fn one_level_is_picked_per_plugin_and_includes_those_held_below_it() {
    let (app, _) = app();
    let (_, _, page) = consenting(&app, &register_agent(&app).await).await;
    // One select for oms-1, never a checkbox per level.
    assert_eq!(page.matches("<select name=\"level\"").count(), 1, "{page}");
    assert!(!page.contains("type=\"checkbox\" name=\"level\""), "{page}");
    assert!(
        page.contains(
            "<option value=\"oms-1::write\" data-short=\"Open\" data-accounts>Open, with View</option>"
        ),
        "{page}"
    );
    // Open picked: the delegation covers Open and the View it includes.
    let (_, said) = connected(
        &app,
        "covers=some&level=oms-1:write&account_group=AG-1&days=30",
    )
    .await;
    let delegation = app
        .delegations
        .delegation(said["delegation_id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        delegation.covers.plugins,
        BTreeSet::from([
            ("oms-1".to_string(), String::new(), "write".to_string()),
            ("oms-1".to_string(), String::new(), "read".to_string())
        ])
    );
    let access = said["access_token"].as_str().unwrap();
    for level in ["write", "read"] {
        let opened = send(
            &app,
            Request::post(format!("/terminal/plugins/oms-1/open?level={level}"))
                .header(AUTHORIZATION, format!("Bearer {access}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        // Admitted; refused only because this dashboard serves no plugin pages.
        assert_eq!(opened.status(), StatusCode::SERVICE_UNAVAILABLE, "{level}");
    }
    // A plugin left at Nothing sends an empty level, which covers nothing.
    let (_, nothing) = connected(&app, "covers=some&level=&days=30").await;
    let delegation = app
        .delegations
        .delegation(nothing["delegation_id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delegation.covers, Covers::default());
}

#[tokio::test]
async fn same_as_my_last_client_fills_the_choices_for_the_person_to_review() {
    let (app, _) = app();
    // Before: the CLI, narrowed to View on oms-1 and the Desk's accounts.
    connected(
        &app,
        "covers=some&level=oms-1:read&account_group=AG-1&days=30",
    )
    .await;
    let agent = register_agent(&app).await;
    let (request, confirm, page) = consenting(&app, &agent).await;
    assert!(page.contains("value=\"last\""), "offered: {page}");
    assert!(page.contains("meridian on ada-laptop"), "{page}");
    assert!(page.contains("oms-1 (View); accounts in Desk"), "{page}");
    // Asked for, the page again, filled in from it: nothing decided, and no
    // code sent anywhere.
    let filled = post_form(
        &app,
        "/oauth/authorize",
        format!("request={request}&confirm={confirm}&decision=last"),
    )
    .await;
    assert_eq!(filled.status(), StatusCode::OK);
    assert!(filled.headers().get(LOCATION).is_none());
    let page = body_of(filled).await;
    assert!(page.contains("data-started=\"last\""), "{page}");
    assert!(page.contains("value=\"some\" checked"), "{page}");
    assert!(
        page.contains("value=\"oms-1::read\" data-short=\"View\" data-accounts selected"),
        "{page}"
    );
    assert!(page.contains("value=\"AG-1\" checked"), "{page}");
    assert!(!page.contains("value=\"last\""), "offered once: {page}");
    // The summary says what that comes to, before Allow.
    assert!(
        page.contains(
            "<span data-summary>It may use View on oms-1; reach the accounts in Desk. Nothing \
             else, and never more than you hold.</span>"
        ),
        "{page}"
    );
    // And the same authorisation is still the person's to answer.
    assert_eq!(hidden(&page, "request"), request);
    let decided = post_form(
        &app,
        "/oauth/authorize",
        format!(
            "request={request}&confirm={confirm}&decision=allow&covers=some\
             &level=oms-1:read&account_group=AG-1&days=30"
        ),
    )
    .await;
    assert_eq!(decided.status(), StatusCode::FOUND);
    assert!(query(&location(&decided), "code").is_some());
}

#[tokio::test]
async fn the_summary_before_allow_states_what_the_client_may_do() {
    let (app, _) = app();
    let (_, _, cli) = consenting(&app, &register_cli(&app).await).await;
    let summary = cli.find("What it will be able to do").expect("a summary");
    let allow = cli.find("value=\"allow\"").expect("Allow");
    assert!(summary < allow, "the summary comes before Allow");
    assert!(cli.contains(
        "It may do anything you may do on this deployment, as that changes: every plugin at every \
         level you hold, and every account you reach, and the deployment admin&#39;s capabilities. \
         For <span data-days>90</span> days"
    ), "{cli}");
    let (_, _, mcp) = consenting_for(
        &app,
        &register_cli(&app).await,
        &format!("https://{HOST_NAME}/mcp"),
    )
    .await;
    assert!(mcp.contains("those added later included"), "{mcp}");
    // A choice with levels and no account group says it reaches none.
    let holdable = Holdable {
        plugins: BTreeMap::from([(
            ("oms-1".to_string(), String::new()),
            vec![AccessLevel::Write, AccessLevel::Read],
        )]),
        account_groups: BTreeMap::new(),
        deployment_admin: true,
    };
    let picked = holdable.picked(&Covers {
        plugins: BTreeSet::from([("oms-1".into(), String::new(), "write".into())]),
        deployment_admin: true,
        ..Covers::default()
    });
    assert_eq!(
        may_do(&picked, &BTreeMap::new()),
        "It may use Open on oms-1; use the deployment admin's capabilities. It reaches no \
         account: tick an account group for that. Nothing else, and never more than you hold."
    );
}

/// A firm's records: Ada writing on 40 plugin instances, managing the first
/// ten, and reaching 300 account groups of two accounts each.
fn a_firm() -> AccessRecords {
    let mut records = records();
    records.accounts = (0..600)
        .map(|i| AccountRecord {
            account_id: format!("ACC-{i:03}"),
            name: format!("Account {i}"),
            state: AccountState::Open as i32,
            ..Default::default()
        })
        .collect();
    records.account_groups = (0..300)
        .map(|i| AccountGroup {
            account_group_id: format!("AG-{i:03}"),
            name: format!("Fund {i:03}"),
            account_ids: vec![format!("ACC-{:03}", 2 * i), format!("ACC-{:03}", 2 * i + 1)],
            ..Default::default()
        })
        .collect();
    let entries = |level: meridian_access::AccessLevel, n: usize| {
        (0..n)
            .map(|i| AccessEntry {
                plugin_instance_id: format!("plugin-{i:02}"),
                level: level as i32,
                role: String::new(),
            })
            .collect()
    };
    records.access_groups = vec![
        AccessGroup {
            access_group_id: "AX-1".into(),
            name: "Traders".into(),
            entries: entries(meridian_access::AccessLevel::Write, 40),
            ..Default::default()
        },
        AccessGroup {
            access_group_id: "AX-2".into(),
            name: "Operators".into(),
            entries: entries(meridian_access::AccessLevel::Admin, 10),
            ..Default::default()
        },
    ];
    records.permissions.retain(|p| p.permission_id == "P-1");
    records.permissions.extend((0..300).map(|i| Permission {
        permission_id: format!("P-G{i:03}"),
        user_group_id: "UG-1".into(),
        account_group_id: format!("AG-{i:03}"),
        access_group_id: "AX-1".into(),
    }));
    records.permissions.push(Permission {
        permission_id: "P-ops".into(),
        user_group_id: "UG-1".into(),
        account_group_id: String::new(),
        access_group_id: "AX-2".into(),
    });
    records
}

#[tokio::test]
async fn forty_plugins_and_three_hundred_account_groups_make_one_short_page() {
    let (app, _) = app();
    app.records.store(a_firm(), T0);
    let agent = register_agent(&app).await;
    let (request, confirm, page) = consenting(&app, &agent).await;
    // Plugins by instance, one level picked on each.
    assert_eq!(page.matches("<select name=\"level\"").count(), 40);
    assert!(!page.contains("type=\"checkbox\" name=\"level\""));
    assert!(page.contains(
        "<option value=\"plugin-00::admin\" data-short=\"Manage\" data-accounts>Manage, with Open and \
         View</option>"
    ), "manage includes open and view, as holding it does");
    assert!(
        !page.contains("value=\"plugin-10::admin\""),
        "held on ten only"
    );
    // Account groups searched, with how many accounts each holds, and how
    // many there are and are chosen (the picker's status, said by its
    // script); every one a checkbox posting as it is.
    assert_eq!(page.matches("name=\"account_group\"").count(), 300);
    assert_eq!(
        page.matches("data-picker-search>").count(),
        2,
        "both searched"
    );
    assert!(page.contains("aria-label=\"Search account groups\""));
    assert!(page.contains("aria-label=\"Search plugins\""));
    assert!(page.contains(
        "<span class=\"option-label\">Fund 123</span> <span class=\"id\">2 accounts</span>"
    ));
    assert!(page.contains("data-also=\"AG-123\""), "found by its ID too");
    // One question, one choice per row: a page a person reads.
    let size = page.len();
    println!("the consent page at 40 plugins and 300 account groups: {size} bytes");
    assert!(size < 150_000, "{size} bytes");
    // And finishing it is a handful of fields: one search, one level, one
    // group.
    let decided = post_form(
        &app,
        "/oauth/authorize",
        format!(
            "request={request}&confirm={confirm}&decision=allow&covers=some\
             &level=plugin-07:admin&level=plugin-22:read&account_group=AG-123&days=7"
        ),
    )
    .await;
    assert_eq!(
        decided.status(),
        StatusCode::FOUND,
        "{}",
        body_of(decided).await
    );
}

// ── The consent page lists what tools/list lists (contract v17) ─────────

/// ops-1 holds custody and operations and reports a tool on each level and
/// one naming no role; Ada is a deployment admin and nothing else, Cat
/// administers custody, and Ben reads operations for one account group.
fn three_kinds() -> AccessRecords {
    use meridian_domain::v1::KnownPluginRoles;
    let entry = |role: &str, level: meridian_access::AccessLevel| AccessEntry {
        plugin_instance_id: "ops-1".into(),
        level: level as i32,
        role: role.into(),
    };
    let people = |id: &str, name: &str| UserGroup {
        user_group_id: format!("UG-{id}"),
        name: name.into(),
        directory_groups: vec![],
        logins: vec![format!("local|{id}")],
    };
    let permit = |id: &str, group: &str, accounts: &str| Permission {
        permission_id: format!("P-{id}"),
        user_group_id: format!("UG-{id}"),
        account_group_id: accounts.into(),
        access_group_id: group.into(),
    };
    AccessRecords {
        accounts: records().accounts,
        account_groups: records().account_groups,
        access_groups: vec![
            AccessGroup {
                access_group_id: "AX-cat".into(),
                name: "Custody admins".into(),
                entries: vec![entry("custody", meridian_access::AccessLevel::Admin)],
                ..Default::default()
            },
            AccessGroup {
                access_group_id: "AX-ben".into(),
                name: "Operations readers".into(),
                entries: vec![entry("operations", meridian_access::AccessLevel::Read)],
                ..Default::default()
            },
        ],
        user_groups: vec![
            people("ada", "Admins"),
            people("cat", "Custody"),
            people("ben", "Operations"),
        ],
        permissions: vec![
            permit("ada", meridian_access::DEPLOYMENT_ADMIN, ""),
            permit("cat", "AX-cat", ""),
            permit("ben", "AX-ben", "AG-1"),
        ],
        known_plugins: vec![KnownPluginRoles {
            plugin_instance_id: "ops-1".into(),
            roles: vec!["custody".into(), "operations".into()],
        }],
        ..Default::default()
    }
}

/// The three of them signing in here, ops-1 running and reporting its tools.
fn app_of_three() -> Arc<App> {
    use meridian_access::AccessLevel::{Admin, Read, Write};
    use meridian_domain::v1::PluginReport;
    use meridian_pb::v1::ToolDeclaration;
    let accounts = crate::accounts::InMemory::default();
    for (name, display_name) in [("ada", "Ada Park"), ("cat", "Cat Ruiz"), ("ben", "Ben Ito")] {
        accounts
            .put(&crate::accounts::LocalAccount {
                name: name.into(),
                display_name: display_name.into(),
                password_hash: crate::accounts::hash_password("correct horse battery")
                    .expect("hashed"),
                created_at_ns: T0,
                ..Default::default()
            })
            .expect("stored");
    }
    let tool = |name: &str, roles: &[&str], levels: &[meridian_access::AccessLevel], reads| {
        ToolDeclaration {
            name: name.into(),
            title: name.replace('_', " "),
            levels: levels.iter().map(|l| *l as i32).collect(),
            reads,
            roles: roles.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        }
    };
    let app = app_with(Some(three_kinds()), T0, T0);
    let mut built = Arc::try_unwrap(app).ok().expect("one reference");
    built.accounts = Some(Arc::new(accounts));
    built.clock = Arc::new(Moving(AtomicI64::new(T0)));
    built.public_url = format!("https://{HOST_NAME}");
    built.health.hear(
        "ops-1",
        PluginReport {
            plugin_instance_id: "ops-1".into(),
            roles: vec!["custody".into(), "operations".into()],
            registered: true,
            healthy: true,
            reported_at_ns: T0,
            last_heartbeat_at_ns: T0,
            declared_tools: vec![
                tool("read_positions", &["operations"], &[Read], true),
                tool("post_breaks", &["custody"], &[Write], false),
                tool("tune_feed", &["custody"], &[Admin], false),
                tool("plugin_status", &[], &[Read, Admin], true),
            ],
            ..Default::default()
        },
    );
    Arc::new(built)
}

/// The consent page `name` is shown for an agent asking for `/mcp`.
async fn consent_for_mcp(app: &Arc<App>, client_id: &str, name: &str) -> String {
    let asked = get_page(
        app,
        &authorising(client_id, &format!("https://{HOST_NAME}/mcp")),
    )
    .await;
    let page = body_of(asked).await;
    let id = hidden(&page, "authorize");
    let signed_in = post_form(
        app,
        "/sign-in",
        format!("name={name}&password=correct+horse+battery&authorize={id}"),
    )
    .await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    body_of(signed_in).await
}

/// Each row's tools as the page lists them: `data-reach` to the names in
/// its `data-tools`.
fn rows_listed(page: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut rows = BTreeMap::new();
    for part in page.split("<p data-reach=\"").skip(1) {
        let reach = &part[..part.find('"').unwrap()];
        let marker = "data-tools=\"";
        let at = part.find(marker).unwrap() + marker.len();
        let names = &part[at..at + part[at..].find('"').unwrap()];
        rows.insert(
            reach.to_string(),
            names.split_whitespace().map(String::from).collect(),
        );
    }
    rows
}

/// Every row the page offers, at the most it offers: each select's highest
/// level, the deployment admin's capabilities where offered, every account
/// group; as the form posts them.
fn everything_offered(page: &str) -> String {
    let mut fields = Vec::new();
    for select in page.split("<select name=\"level\"").skip(1) {
        let marker = "<option value=\"";
        let top = select
            .split(marker)
            .skip(1)
            .map(|option| &option[..option.find('"').unwrap()])
            .find(|value| !value.is_empty())
            .expect("a level held");
        fields.push(format!("level={}", encoded(top)));
    }
    if page.contains("name=\"deployment_admin\"") {
        fields.push("deployment_admin=1".into());
    }
    for group in page.split("name=\"account_group\" value=\"").skip(1) {
        fields.push(format!(
            "account_group={}",
            &group[..group.find('"').unwrap()]
        ));
    }
    fields.join("&")
}

/// What `tools/list` answers through a delegation `name` allowed with
/// `answer` on the consent page.
async fn listed_through(app: &Arc<App>, name: &str, answer: &str) -> BTreeSet<String> {
    let client_id = register_agent(app).await;
    let page = consent_for_mcp(app, &client_id, name).await;
    let decided = post_form(
        app,
        "/oauth/authorize",
        format!(
            "request={}&confirm={}&decision=allow&covers=some&days=30&{answer}",
            hidden(&page, "request"),
            hidden(&page, "confirm")
        ),
    )
    .await;
    assert_eq!(
        decided.status(),
        StatusCode::FOUND,
        "{}",
        body_of(decided).await
    );
    let code = query(&location(&decided), "code").expect("a code");
    let issued = post_form(
        app,
        "/oauth/token",
        format!(
            "grant_type=authorization_code&code={code}&code_verifier={VERIFIER}\
             &redirect_uri={}&client_id={}&resource={}",
            encoded(BACK),
            encoded(&client_id),
            encoded(&format!("https://{HOST_NAME}/mcp"))
        ),
    )
    .await;
    assert_eq!(issued.status(), StatusCode::OK);
    let token = json_of(issued).await["access_token"]
        .as_str()
        .expect("a token")
        .to_string();
    let listed = send(
        app,
        Request::post("/mcp")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    json_of(listed).await["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().expect("a name").to_string())
        .collect()
}

#[tokio::test]
async fn the_consent_page_lists_each_tool_tools_list_answers_for_each_kind_of_person() {
    // Contract v17: the page says, under each row, every tool a delegation
    // covering it is listed -- core's plugin-area tools with the plugin's
    // own -- from the one source tools/list answers from; allowing all it
    // offers lists exactly the union of what its rows say.
    let app = app_of_three();
    for (name, must, never) in [
        (
            "ada",
            vec![
                "dashboard__complete_instruments",
                "dashboard__file_ticket",
                "dashboard__list_plugins",
                "dashboard__read_plugin_summary",
                "dashboard__set_hold",
                "dashboard__launch_plugin",
                "dashboard__read_plugin_access",
            ],
            vec!["dashboard__read_plugin_settings", "ops-1__tune_feed"],
        ),
        (
            "cat",
            vec![
                "dashboard__read_plugin_settings",
                "dashboard__set_plugin_settings",
                "dashboard__read_moves",
                "dashboard__file_ticket",
                "ops-1__tune_feed",
                "ops-1__plugin_status",
            ],
            vec![
                "dashboard__set_hold",
                "dashboard__complete_instruments",
                "ops-1__read_positions",
            ],
        ),
        (
            "ben",
            vec![
                "dashboard__list_plugins",
                "dashboard__file_ticket",
                "ops-1__read_positions",
                "ops-1__plugin_status",
            ],
            vec![
                "dashboard__read_plugin_settings",
                "dashboard__read_plugin_summary",
                "dashboard__set_hold",
                "ops-1__tune_feed",
            ],
        ),
    ] {
        let page = consent_for_mcp(&app, &register_agent(&app).await, name).await;
        let rows = rows_listed(&page);
        assert!(!rows.is_empty(), "{name}: no row lists a tool: {page}");
        let said: BTreeSet<String> = rows.values().flatten().cloned().collect();
        for tool in &must {
            assert!(
                said.contains(*tool),
                "{name}: the page lists {tool}: {rows:?}"
            );
        }
        for tool in &never {
            assert!(
                !said.contains(*tool),
                "{name}: the page grants no {tool}: {rows:?}"
            );
        }
        // Each row's titles are shown, reads and acts apart.
        assert!(page.contains("Reads: "), "{name}");
        let listed = listed_through(&app, name, &everything_offered(&page)).await;
        assert_eq!(said, listed, "{name}: the page and tools/list agree");
    }
}
