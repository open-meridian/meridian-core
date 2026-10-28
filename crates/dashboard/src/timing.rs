//! Each request's timing line (ruling 798): measure before tuning.
//!
//! One line at info per request the dashboard answers: its method, its
//! route -- the route's pattern, never the path a person typed, or on a
//! plugin's host the plugin's instance and its page's path, never its query
//! -- its status, the time to its answer's head, and each bus call it asked
//! on the way, by topic and time. No payload, no person: the lines already
//! carried elsewhere say who signed in. A streamed answer's head is when
//! its first byte left; a plugin's page is drawn first and filled after
//! (ruling 797), so its head is what a person waits for.
//!
//! Its health probe and the files served as they are (the kit, the icons)
//! are not lined: a probe every few seconds would bury the rest.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::header::HOST;
use axum::middleware::Next;
use axum::response::Response;

use crate::web::App;

/// The route a request was answered by, as its router matched it.
#[derive(Debug, Clone)]
struct Route(String);

/// The most bus calls a line names; past it, how many more.
const CALLS_NAMED: usize = 24;

/// Inside the router: what it matched, carried out on the answer.
pub async fn named(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string());
    let mut response = next.run(request).await;
    if let Some(route) = route {
        response.extensions_mut().insert(Route(route));
    }
    response
}

fn quiet(route: &str) -> bool {
    route == "/healthz" || route.starts_with("/.meridian/ui/") || crate::brand::is_icon(route)
}

/// Outermost: the line, once the answer's head is ready.
pub async fn timed(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let plugin = app.plugins.as_ref().and_then(|plugins| {
        let named = request
            .headers()
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .or_else(|| request.uri().authority().map(|a| a.to_string()))?;
        match plugins.host(&named) {
            crate::plugins::Host::Plugin(instance) => Some(instance),
            _ => None,
        }
    });
    let started = Instant::now();
    let (response, calls) = meridian_bus::timed(next.run(request)).await;
    let ms = started.elapsed().as_secs_f64() * 1e3;
    let route = match (&plugin, response.extensions().get::<Route>()) {
        (Some(instance), _) => format!("plugin {instance} {path}"),
        (None, Some(Route(route))) => route.clone(),
        (None, None) => "unrouted".to_string(),
    };
    if quiet(if plugin.is_some() { &path } else { &route }) {
        return response;
    }
    tracing::info!(
        method,
        route,
        status = response.status().as_u16(),
        ms = format!("{ms:.1}"),
        calls = calls.len(),
        bus = meridian_bus::calls_said(&calls, CALLS_NAMED),
        "a request answered"
    );
    response
}
