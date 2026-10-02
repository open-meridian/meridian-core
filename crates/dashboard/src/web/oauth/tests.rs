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
    let asked = get_page(app, &authorising(client_id, &terminal_resource())).await;
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
        "https://dash.firm.example/mcp",
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
    assert!(page.contains("value=\"oms-1:write\""), "{page}");
    assert!(page.contains("value=\"oms-1:read\""), "{page}");
    assert!(!page.contains("value=\"oms-1:admin\""), "{page}");
    assert!(page.contains("name=\"deployment_admin\""), "{page}");
    assert!(page.contains("value=\"AG-1\""), "{page}");
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
