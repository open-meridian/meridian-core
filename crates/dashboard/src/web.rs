//! The dashboard's HTTP surface.
//!
//! Every page that shows anything about the deployment first asks
//! [`RecordsCache::current`], and refuses with its sentence when the records
//! are stale: a dashboard past the ceiling serves nothing but that refusal
//! and its own health.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::clock::Clock;
use crate::html::{escape, page};
use crate::records::RecordsCache;
use crate::session::Sessions;

pub struct App {
    pub records: Arc<RecordsCache>,
    pub sessions: Arc<Sessions>,
    pub clock: Arc<dyn Clock>,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(home))
        .with_state(app)
}

/// Alive, and whether the records are fresh enough to serve. A load balancer
/// takes a stale dashboard out rather than sending people to a refusal.
async fn healthz(State(app): State<Arc<App>>) -> Response {
    match app.records.current(app.clock.now_ns()) {
        Ok(_) => (StatusCode::OK, "serving\n").into_response(),
        Err(stale) => (StatusCode::SERVICE_UNAVAILABLE, format!("{stale}\n")).into_response(),
    }
}

async fn home(State(app): State<Arc<App>>) -> Response {
    if let Err(stale) = app.records.current(app.clock.now_ns()) {
        return refused(&stale.to_string());
    }
    Html(page(
        "Sign in",
        "<h1>Meridian</h1><p><a href=\"/sign-in\">Sign in</a> with your firm's directory.</p>",
    ))
    .into_response()
}

/// A refusal a person can read, with the status that says it is not their doing.
pub fn refused(sentence: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Html(page(
            "Unavailable",
            &format!(
                "<h1>Unavailable</h1><p class=\"refused\">{}</p>",
                escape(sentence)
            ),
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use meridian_domain::v1::AccessRecords;
    use tower::ServiceExt;

    use super::*;
    use crate::records::CEILING_NS;

    struct At(i64);
    impl Clock for At {
        fn now_ns(&self) -> i64 {
            self.0
        }
    }

    const T0: i64 = 1_790_380_800_000_000_000;

    fn app(read_at: Option<i64>, now: i64) -> Router {
        let records = Arc::new(RecordsCache::default());
        if let Some(at) = read_at {
            records.store(AccessRecords::default(), at);
        }
        router(Arc::new(App {
            records,
            sessions: Arc::new(Sessions::default()),
            clock: Arc::new(At(now)),
        }))
    }

    async fn status(router: Router, path: &str) -> StatusCode {
        router
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_dashboard_with_fresh_records_serves() {
        assert_eq!(status(app(Some(T0), T0), "/").await, StatusCode::OK);
        assert_eq!(status(app(Some(T0), T0), "/healthz").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_dashboard_past_the_ceiling_refuses_every_page_and_says_so() {
        let stale = || app(Some(T0), T0 + CEILING_NS + 1);
        assert_eq!(status(stale(), "/").await, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            status(stale(), "/healthz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn a_dashboard_that_has_never_read_serves_nothing() {
        assert_eq!(
            status(app(None, T0), "/").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
