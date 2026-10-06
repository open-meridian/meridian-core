//! A terminal path's credential over HTTP: an access token on a delegation
//! (W6.18), and nothing else from contract v15, when the terminal sessions
//! from before delegations were retired with the CLI before 0.1.25.

use axum::body::Body;
use axum::http::Request;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use tower::ServiceExt;

use super::*;
use crate::web::router;
use crate::web::tests::{app_holding_ada, body_of};

/// The dashboard's routes, and one terminal path that answers with who its
/// credential names.
fn routes(app: Arc<App>) -> Router {
    let probed = Arc::clone(&app);
    router(app).route(
        "/terminal/probe",
        get(move |headers: HeaderMap| async move {
            match caller_of(&probed, &headers).await {
                Ok(caller) => caller.person.subject.into_response(),
                Err(refusal) => *refusal,
            }
        }),
    )
}

async fn send(app: &Arc<App>, request: Request<Body>) -> (StatusCode, String) {
    let response = routes(Arc::clone(app))
        .oneshot(request)
        .await
        .expect("a response");
    let status = response.status();
    (status, body_of(response).await)
}

#[tokio::test]
async fn the_terminal_sessions_from_before_delegations_are_gone() {
    let app = app_holding_ada();
    for (method, path) in [
        ("GET", "/terminal/authorize?redirect_uri=x"),
        ("POST", "/terminal/authorize"),
        ("POST", "/terminal/token"),
        ("POST", "/terminal/sign-out"),
        ("POST", "/admin/end-terminal-sessions"),
    ] {
        let (status, _) = send(
            &app,
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("code=x"))
                .unwrap(),
        )
        .await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}: {status}"
        );
    }
}

#[tokio::test]
async fn a_credential_that_is_not_a_delegations_is_refused_as_unknown() {
    let app = app_holding_ada();
    for presented in [None, Some("a-terminal-session-from-before")] {
        let mut request = Request::get("/terminal/probe");
        if let Some(presented) = presented {
            request = request.header(AUTHORIZATION, format!("Bearer {presented}"));
        }
        let (status, body) = send(&app, request.body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{presented:?}");
        assert!(body.contains("\"reason\":\"unknown\""), "{body}");
    }
}

#[tokio::test]
async fn a_cli_older_than_this_serves_is_refused_saying_what_it_serves() {
    let app = app_holding_ada();
    let asked = |version: Option<&'static str>, path: &'static str| {
        let app = Arc::clone(&app);
        async move {
            let mut request = Request::get(path);
            if let Some(version) = version {
                request = request.header(CLI_VERSION, version);
            }
            send(&app, request.body(Body::empty()).unwrap()).await
        }
    };
    // 0.1.25 is the first CLI connecting by delegation (contract v15).
    for old in ["0.1.24", "0.0.9", "0.1.0", "not-a-version", "1.2", ""] {
        let (status, body) = asked(Some(old), "/terminal/plugins").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{old}");
        assert!(body.contains(r#""error":"cli_version""#), "{old}: {body}");
        assert!(
            body.contains("serves meridian 0.1.25 or later"),
            "{old}: {body}"
        );
    }
    for served in [Some("0.1.25"), Some("0.1.34"), Some("0.2.0-dev+abc"), None] {
        let (_, body) = asked(served, "/terminal/plugins").await;
        assert!(!body.contains("cli_version"), "{served:?}: {body}");
    }
    // Only the terminal's paths: a browser's page is not the CLI's.
    let (_, body) = asked(Some("0.0.1"), "/sign-in").await;
    assert!(!body.contains("cli_version"), "{body}");
}
