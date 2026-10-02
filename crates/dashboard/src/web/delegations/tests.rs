//! Connected clients in a browser: the fixtures list-own-delegations,
//! revoke-own-delegation, list-persons-delegations and
//! revoke-persons-delegations, and the notice on the home.

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE};
use axum::http::Request;

use super::*;
use crate::delegation::DAY_NS;
use crate::web::oauth::tests::{app, at, connected, send};
use crate::web::tests::{body_of, T0};
use crate::web::SESSION_COOKIE;

fn browser(app: &App, subject: &str) -> (String, String) {
    let key = app
        .sessions
        .start(subject, "Ada Park", vec![], app.clock.now_ns());
    let session = app.sessions.find(&key, app.clock.now_ns()).unwrap();
    (format!("__Host-{SESSION_COOKIE}={key}"), session.form_token)
}

async fn get_as(app: &Arc<App>, path: &str, cookie: &str) -> Response {
    send(
        app,
        Request::get(path)
            .header(COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn post_as(app: &Arc<App>, path: &str, cookie: &str, body: String) -> Response {
    send(
        app,
        Request::post(path)
            .header(COOKIE, cookie)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

async fn acts(app: &Arc<App>, token: &str) -> bool {
    send(
        app,
        Request::get("/terminal/probe")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .status()
        == StatusCode::OK
}

#[tokio::test]
async fn a_person_sees_their_connected_clients_and_revokes_one() {
    let (app, _) = app();
    let (_, said) = connected(&app, "covers=everything&days=30").await;
    let id = said["delegation_id"].as_str().unwrap();
    let (cookie, token) = browser(&app, "local|ada");

    let listed = get_as(&app, "/delegations", &cookie).await;
    assert_eq!(listed.status(), StatusCode::OK);
    let page = body_of(listed).await;
    assert!(page.contains("Connected clients"), "{page}");
    assert!(page.contains("meridian on ada-laptop"), "{page}");
    assert!(page.contains("Everything you hold"), "{page}");
    assert!(
        page.contains(&format!("data-id=\"{id}\" data-state=\"live\"")),
        "{page}"
    );

    let forged = post_as(
        &app,
        &format!("/delegations/{id}/revoke"),
        &cookie,
        "form_token=guessed".into(),
    )
    .await;
    assert_eq!(forged.status(), StatusCode::BAD_REQUEST);
    assert!(acts(&app, said["access_token"].as_str().unwrap()).await);

    let revoked = post_as(
        &app,
        &format!("/delegations/{id}/revoke"),
        &cookie,
        format!("form_token={token}"),
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::SEE_OTHER);
    assert!(!acts(&app, said["access_token"].as_str().unwrap()).await);
    let page = body_of(get_as(&app, "/delegations", &cookie).await).await;
    assert!(page.contains("data-state=\"revoked\""), "{page}");
    assert!(page.contains("revoked by the person"), "{page}");
}

#[tokio::test]
async fn nobody_revokes_a_delegation_that_is_not_theirs_from_their_own_page() {
    let (app, _) = app();
    let (_, said) = connected(&app, "covers=everything&days=30").await;
    let id = said["delegation_id"].as_str().unwrap();
    let (cookie, token) = browser(&app, "local|bob");
    let refused = post_as(
        &app,
        &format!("/delegations/{id}/revoke"),
        &cookie,
        format!("form_token={token}"),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
    assert!(acts(&app, said["access_token"].as_str().unwrap()).await);
    let page = body_of(get_as(&app, "/delegations", &cookie).await).await;
    assert!(
        page.contains("No client acts on a delegation here"),
        "{page}"
    );
}

#[tokio::test]
async fn connected_clients_is_a_browsers_page_and_takes_no_token() {
    let (app, _) = app();
    let (_, said) = connected(&app, "covers=everything&days=30").await;
    let tokened = send(
        &app,
        Request::get("/delegations")
            .header(
                AUTHORIZATION,
                format!("Bearer {}", said["access_token"].as_str().unwrap()),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(tokened.status(), StatusCode::SEE_OTHER, "sent to sign in");
}

#[tokio::test]
async fn a_deployment_admin_lists_a_persons_delegations_and_revokes_them_all() {
    let (app, _) = app();
    let (_, first) = connected(&app, "covers=everything&days=30").await;
    let (_, second) = connected(&app, "covers=some&level=oms-1:read&days=7").await;
    let (cookie, token) = browser(&app, "local|ada");

    let overview = body_of(get_as(&app, "/admin", &cookie).await).await;
    assert!(overview.contains("id=\"connected-clients\""), "{overview}");
    assert!(
        overview.contains("href=\"/admin/people/local%7Cada/delegations\""),
        "{overview}"
    );
    assert!(overview.contains("data-count=\"2\""), "{overview}");

    let page = body_of(get_as(&app, "/admin/people/local%7Cada/delegations", &cookie).await).await;
    assert!(page.contains("oms-1 (View)"), "{page}");
    assert!(page.contains("Revoke them all"), "{page}");

    // One, by its id: the other keeps working.
    let one = post_as(
        &app,
        "/admin/people/local%7Cada/delegations/revoke",
        &cookie,
        format!(
            "form_token={token}&delegation_id={}",
            second["delegation_id"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(one.status(), StatusCode::SEE_OTHER);
    assert!(!acts(&app, second["access_token"].as_str().unwrap()).await);
    assert!(acts(&app, first["access_token"].as_str().unwrap()).await);

    let all = post_as(
        &app,
        "/admin/people/local%7Cada/delegations/revoke",
        &cookie,
        format!("form_token={token}&all=1"),
    )
    .await;
    assert_eq!(all.status(), StatusCode::SEE_OTHER);
    assert!(!acts(&app, first["access_token"].as_str().unwrap()).await);
    assert!(
        app.sessions
            .is_live(cookie.split('=').nth(1).unwrap(), app.clock.now_ns()),
        "their browser session is untouched"
    );
}

#[tokio::test]
async fn only_a_deployment_admin_sees_or_revokes_anybody_elses() {
    let (app, _) = app();
    let (_, said) = connected(&app, "covers=everything&days=30").await;
    let (cookie, token) = browser(&app, "local|bob");
    assert_eq!(
        get_as(&app, "/admin/people/local%7Cada/delegations", &cookie)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let refused = post_as(
        &app,
        "/admin/people/local%7Cada/delegations/revoke",
        &cookie,
        format!("form_token={token}&all=1"),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(acts(&app, said["access_token"].as_str().unwrap()).await);
}

#[tokio::test]
async fn the_home_says_a_week_ahead_that_a_delegation_lapses() {
    let (app, clock) = app();
    let (_, _) = connected(&app, "covers=everything&days=30").await;
    let (cookie, _) = browser(&app, "local|ada");
    let home = body_of(get_as(&app, "/", &cookie).await).await;
    assert!(!home.contains("data-delegation="), "{home}");

    at(&app, &clock, T0 + 25 * DAY_NS);
    let (cookie, _) = browser(&app, "local|ada");
    let home = body_of(get_as(&app, "/", &cookie).await).await;
    assert!(home.contains("data-delegation="), "{home}");
    assert!(home.contains("lapses in 5 days"), "{home}");
}
