//! The four fixtures' cases, over HTTP: connect-terminal,
//! exchange-terminal-code, end-own-terminal-session and
//! end-persons-terminal-sessions. The bounds are the store's and are tested
//! there, against a clock that moves.

use axum::body::Body;
use axum::http::header::COOKIE;
use axum::http::Request;
use axum::routing::get;
use tower::ServiceExt;

use super::*;
use crate::web::tests::{admins, app_holding_ada, app_with, body_of, ADA, T0};
use crate::web::{router, SESSION_COOKIE};

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const BACK: &str = "http://127.0.0.1:53682/callback";

fn asking(redirect_uri: &str, method: &str) -> String {
    format!(
        "/terminal/authorize?redirect_uri={}&code_challenge={CHALLENGE}\
         &code_challenge_method={method}&state=st-1",
        redirect_uri
            .replace(':', "%3A")
            .replace('/', "%2F")
            .replace('[', "%5B")
            .replace(']', "%5D")
    )
}

/// The dashboard's routes, and one terminal path that answers with who its
/// session names -- standing in for the terminal paths still to come.
fn routes(app: Arc<App>) -> Router {
    let probed = Arc::clone(&app);
    router(app).route(
        "/terminal/probe",
        get(move |headers: HeaderMap| async move {
            match terminal_session_of(&probed, &headers) {
                Ok(person) => person.subject.into_response(),
                Err(refusal) => *refusal,
            }
        }),
    )
}

async fn send(app: &Arc<App>, request: Request<Body>) -> Response {
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
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

fn hidden(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page
        .find(&marker)
        .unwrap_or_else(|| panic!("no {name} in {page}"))
        + marker.len();
    page[start..start + page[start..].find('"').unwrap()].to_string()
}

fn location(response: &Response) -> String {
    response
        .headers()
        .get(LOCATION)
        .expect("a location")
        .to_str()
        .unwrap()
        .to_string()
}

/// Ada, through the terminal sign-in, up to her confirmation page.
async fn signed_in_for_a_terminal(app: &Arc<App>) -> (String, String) {
    let asked = get_page(app, &asking(BACK, "S256")).await;
    assert_eq!(asked.status(), StatusCode::OK);
    let id = hidden(&body_of(asked).await, "terminal");
    let confirming = post_form(
        app,
        "/sign-in",
        format!("name=ada&password=correct+horse+battery&terminal={id}"),
    )
    .await;
    assert_eq!(confirming.status(), StatusCode::OK);
    assert!(
        confirming.headers().get(SET_COOKIE).is_none(),
        "a terminal sign-in makes no browser session"
    );
    let page = body_of(confirming).await;
    assert!(page.contains("Connect a terminal"), "{page}");
    assert!(page.contains("Ada Park"), "{page}");
    (hidden(&page, "request"), hidden(&page, "confirm"))
}

/// All the way through: the session a terminal would hold.
async fn connected(app: &Arc<App>) -> String {
    let (request, confirm) = signed_in_for_a_terminal(app).await;
    let decided = post_form(
        app,
        "/terminal/authorize",
        format!("request={request}&confirm={confirm}&decision=connect"),
    )
    .await;
    assert_eq!(decided.status(), StatusCode::FOUND);
    let back = location(&decided);
    let code = back
        .strip_prefix(&format!("{BACK}?code="))
        .and_then(|rest| rest.strip_suffix("&state=st-1"))
        .unwrap_or_else(|| panic!("not a code for this terminal: {back}"))
        .to_string();

    let exchanged = post_form(
        app,
        "/terminal/token",
        format!(
            "code={code}&code_verifier={VERIFIER}&redirect_uri={}",
            BACK.replace(':', "%3A").replace('/', "%2F")
        ),
    )
    .await;
    assert_eq!(exchanged.status(), StatusCode::OK);
    assert_eq!(exchanged.headers()[CACHE_CONTROL], "no-store");
    let reply: serde_json::Value = serde_json::from_str(&body_of(exchanged).await).unwrap();
    assert_eq!(reply["subject"], "local|ada");
    assert_eq!(reply["idle_seconds"], 1800);
    assert_eq!(
        reply["expires_at"], "2026-09-26T12:00:00Z",
        "12 hours from the sign-in"
    );
    reply["session"].as_str().expect("a session").to_string()
}

async fn probe(app: &Arc<App>, bearer: Option<&str>, cookie: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::get("/terminal/probe");
    if let Some(bearer) = bearer {
        request = request.header(AUTHORIZATION, format!("Bearer {bearer}"));
    }
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    let response = send(app, request.body(Body::empty()).unwrap()).await;
    (response.status(), body_of(response).await)
}

#[tokio::test]
async fn a_terminal_connects_and_its_session_is_honoured_on_the_terminals_paths() {
    let app = app_holding_ada();
    let session = connected(&app).await;
    assert_eq!(
        probe(&app, Some(&session), None).await,
        (StatusCode::OK, "local|ada".into())
    );
}

#[tokio::test]
async fn a_request_that_fails_its_checks_is_refused_before_any_sign_in_and_sent_nowhere() {
    let app = app_holding_ada();
    for (redirect_uri, method) in [
        ("http://localhost:53682/callback", "S256"),
        ("https://127.0.0.1:53682/callback", "S256"),
        ("http://127.0.0.1:53682/elsewhere", "S256"),
        (BACK, "plain"),
    ] {
        let response = get_page(&app, &asking(redirect_uri, method)).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{redirect_uri} {method}"
        );
        assert!(response.headers().get(LOCATION).is_none());
        let page = body_of(response).await;
        assert!(!page.contains("password"), "no sign-in offered: {page}");
    }
}

#[tokio::test]
async fn a_browser_session_already_held_is_neither_used_nor_extended() {
    let app = app_holding_ada();
    let key = app.sessions.start("local|ada", "Ada Park", vec![], T0);
    let response = send(
        &app,
        Request::get(asking(BACK, "S256"))
            .header(COOKIE, format!("{SESSION_COOKIE}={key}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(SET_COOKIE).is_none());
    let page = body_of(response).await;
    assert!(page.contains("name=\"password\""), "asked afresh: {page}");
    assert!(page.contains("Sign in to connect a terminal"), "{page}");
}

#[tokio::test]
async fn a_wrong_password_keeps_the_terminals_request_and_says_nothing_more() {
    let app = app_holding_ada();
    let asked = get_page(&app, &asking(BACK, "S256")).await;
    let id = hidden(&body_of(asked).await, "terminal");
    let refused = post_form(
        &app,
        "/sign-in",
        format!("name=ada&password=wrong&terminal={id}"),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    let page = body_of(refused).await;
    assert!(page.contains("were not accepted"), "{page}");
    assert_eq!(
        hidden(&page, "terminal"),
        id,
        "the next try is still for the terminal"
    );
}

#[tokio::test]
async fn declining_sends_the_terminal_a_refusal_and_no_code() {
    let app = app_holding_ada();
    let (request, confirm) = signed_in_for_a_terminal(&app).await;
    let declined = post_form(
        &app,
        "/terminal/authorize",
        format!("request={request}&confirm={confirm}&decision=decline"),
    )
    .await;
    assert_eq!(declined.status(), StatusCode::FOUND);
    assert_eq!(
        location(&declined),
        format!("{BACK}?error=access_denied&state=st-1")
    );
}

#[tokio::test]
async fn a_confirmation_without_its_token_is_refused_and_goes_nowhere() {
    let app = app_holding_ada();
    let (request, _) = signed_in_for_a_terminal(&app).await;
    let forged = post_form(
        &app,
        "/terminal/authorize",
        format!("request={request}&confirm=guessed&decision=connect"),
    )
    .await;
    assert_eq!(forged.status(), StatusCode::BAD_REQUEST);
    assert!(forged.headers().get(LOCATION).is_none());
}

#[tokio::test]
async fn a_bad_code_is_one_refusal_whatever_was_wrong_with_it() {
    let app = app_holding_ada();
    let refused = post_form(
        &app,
        "/terminal/token",
        format!("code=never-issued&code_verifier={VERIFIER}&redirect_uri=x"),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_of(refused).await,
        serde_json::json!({"error": "invalid_grant"}).to_string()
    );
}

#[tokio::test]
async fn neither_client_can_act_through_the_others_credential() {
    let app = app_holding_ada();
    let session = connected(&app).await;

    // A browser's cookie on a terminal path is not a session there.
    let key = app.sessions.start("local|ada", "Ada Park", vec![], T0);
    let cookie = format!("{SESSION_COOKIE}={key}");
    let (status, body) = probe(&app, None, Some(&cookie)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // And a terminal's session on a browser page is nobody.
    let home = send(
        &app,
        Request::get("/")
            .header(AUTHORIZATION, format!("Bearer {session}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let page = body_of(home).await;
    assert!(page.contains("href=\"/sign-in\""), "not signed in: {page}");
    assert!(!page.contains("Ada Park"), "{page}");
}

#[tokio::test]
async fn signing_out_ends_the_session_and_the_terminal_is_told_why() {
    let app = app_holding_ada();
    let session = connected(&app).await;
    let out = send(
        &app,
        Request::post("/terminal/sign-out")
            .header(AUTHORIZATION, format!("Bearer {session}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(out.status(), StatusCode::NO_CONTENT);

    let (status, body) = probe(&app, Some(&session), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        serde_json::json!({"error": "invalid_token", "reason": "ended"}).to_string()
    );

    let again = send(
        &app,
        Request::post("/terminal/sign-out")
            .header(AUTHORIZATION, format!("Bearer {session}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        again.status(),
        StatusCode::NO_CONTENT,
        "the CLI forgets it either way"
    );
}

#[tokio::test]
async fn no_session_at_all_is_refused_with_a_reason_too() {
    let app = app_holding_ada();
    let (status, body) = probe(&app, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("\"reason\":\"unknown\""), "{body}");
}

/// Ada as an administrator, holding a browser session, and her form token.
fn admin_session(app: &Arc<App>) -> (String, String) {
    let key = app.sessions.start(ADA, "Ada", vec![], T0);
    let token = app.sessions.find(&key, T0).unwrap().form_token;
    (format!("{SESSION_COOKIE}={key}"), token)
}

async fn ending(app: &Arc<App>, cookie: &str, body: String) -> Response {
    send(
        app,
        Request::post("/admin/end-terminal-sessions")
            .header("content-type", "application/x-www-form-urlencoded")
            .header(COOKIE, cookie)
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn an_admin_ends_all_of_a_persons_terminal_sessions_and_leaves_their_browser_alone() {
    let app = app_holding_ada();
    let one = connected(&app).await;
    let two = connected(&app).await;
    let browser = app.sessions.start("local|ada", "Ada Park", vec![], T0);
    let (cookie, token) = admin_session(&app);

    let listed = send(
        &app,
        Request::get("/admin")
            .header(COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let page = body_of(listed).await;
    assert!(page.contains("Terminal sessions"), "{page}");
    assert!(page.contains("local|ada</td><td>2</td>"), "{page}");

    let ended = ending(
        &app,
        &cookie,
        format!("login=local%7Cada&form_token={token}"),
    )
    .await;
    assert_eq!(ended.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&ended), "/admin?terminal_sessions_ended=2");

    for session in [one, two] {
        let (status, body) = probe(&app, Some(&session), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("\"reason\":\"ended\""), "{body}");
    }
    assert!(
        app.sessions.find(&browser, T0).is_some(),
        "their browser is untouched"
    );
}

#[tokio::test]
async fn only_an_admin_with_their_form_token_ends_anybodys_sessions() {
    let app = app_holding_ada();
    let session = connected(&app).await;

    // Not an administrator.
    let key = app.sessions.start("local|ada", "Ada Park", vec![], T0);
    let token = app.sessions.find(&key, T0).unwrap().form_token;
    let cookie = format!("{SESSION_COOKIE}={key}");
    let refused = ending(
        &app,
        &cookie,
        format!("login=local%7Cada&form_token={token}"),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    // An administrator, without the token that says the form was theirs.
    let (cookie, _) = admin_session(&app);
    let forged = ending(&app, &cookie, "login=local%7Cada&form_token=guessed".into()).await;
    assert_eq!(forged.status(), StatusCode::BAD_REQUEST);

    // A terminal's session, on a browser path.
    let bearer = send(
        &app,
        Request::post("/admin/end-terminal-sessions")
            .header("content-type", "application/x-www-form-urlencoded")
            .header(AUTHORIZATION, format!("Bearer {session}"))
            .body(Body::from("login=local%7Cada"))
            .unwrap(),
    )
    .await;
    assert_eq!(bearer.status(), StatusCode::UNAUTHORIZED);

    assert_eq!(
        probe(&app, Some(&session), None).await.0,
        StatusCode::OK,
        "still live"
    );
}

#[tokio::test]
async fn a_dashboard_not_set_up_or_past_its_ceiling_connects_nobody() {
    let mut fresh = Arc::try_unwrap(app_with(Some(admins()), T0, T0))
        .ok()
        .unwrap();
    fresh.first_run = true;
    let fresh = Arc::new(fresh);
    assert_eq!(
        get_page(&fresh, &asking(BACK, "S256")).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );

    let stale = app_with(Some(admins()), T0, T0 + crate::records::CEILING_NS + 1);
    assert_eq!(
        get_page(&stale, &asking(BACK, "S256")).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let exchanged = post_form(&stale, "/terminal/token", "code=x".into()).await;
    assert_eq!(exchanged.status(), StatusCode::SERVICE_UNAVAILABLE);
}
