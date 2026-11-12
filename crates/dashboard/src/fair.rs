//! Fair shares at the dashboard (the resilience spec's section 10a).
//!
//! Every component that answers more than one caller queues work per
//! calling person or instance and serves the queues in turn (weighted fair
//! queuing, every weight one), so one caller's flood waits behind itself
//! and every other caller's next request is answered after at most one
//! more of its. The dashboard answers people's browsers, their terminals
//! and their agents:
//!
//! - **A browser's requests are keyed by the person** whose session they
//!   carry; a terminal's by its token, which is one person's; those that
//!   carry neither (a sign-in, the first-run wizard) wait together.
//! - **A delegation's calls on `/mcp` are keyed by the delegation**, taken
//!   in turn after the MCP bounds admit them ([`crate::mcp::bounds`]): the
//!   bounds stay their caps and no second rate is added -- a delegation
//!   with at most four calls at once never meets this gate's own share.
//!
//! The dashboard's health probe and the files it serves as they are (the
//! kit, the icons, the acts script) take no turn.
//!
//! Past its wait, or with its share already waiting, a request is refused
//! for load with a retry-after (`503 Service Unavailable` and `Retry-After`
//! to a browser or a terminal; a tool error, `rate_limited` with
//! `retry_after_seconds`, on `/mcp`, as the bounds refuse).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header::RETRY_AFTER, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use meridian_budget::{Class, Gate, Overloaded, Turn};

use crate::web::App;

/// Requests the dashboard answers at once; the rest wait their turn among
/// their callers'. Each mostly waits on the bus for the components it asks,
/// so this bounds the dashboard's own work, not theirs.
pub const AT_ONCE: usize = 32;

/// The dashboard's turns, shared by every request this process serves.
pub struct Turns {
    gate: Gate,
}

impl Default for Turns {
    fn default() -> Self {
        Self::at_once(AT_ONCE)
    }
}

impl Turns {
    pub fn at_once(permits: usize) -> Self {
        Self {
            gate: Gate::new(permits),
        }
    }

    /// `caller`'s turn, within [`meridian_budget::WAIT`]; refused for load
    /// where it does not come.
    pub async fn turn(&self, caller: &str) -> Result<Turn, Overloaded> {
        let turn = self
            .gate
            .admit(Class::Rest, caller, meridian_budget::WAIT)
            .await;
        if turn.is_err() {
            // In the dashboard's report beside its process's own refusals.
            meridian_budget::count_refusal();
        }
        turn
    }

    /// Requests refused for load since the dashboard started.
    pub fn refused(&self) -> u64 {
        self.gate.refused()
    }
}

/// Whose turn a request waits in: the person whose session it carries, the
/// terminal token it presents, or nobody's.
pub fn caller_of(app: &App, headers: &HeaderMap) -> String {
    if let Some(session) = crate::web::session_of(app, headers) {
        return format!("p:{}", session.subject);
    }
    match crate::web::bearer_token(headers) {
        Some(token) => format!("t:{}", crate::terminal::hashed(&token)),
        None => "anonymous".into(),
    }
}

/// What takes no turn: the probe, the files served as they are, and `/mcp`,
/// whose calls take theirs by delegation once the bounds admit them.
fn untimed(path: &str) -> bool {
    path == "/healthz"
        || path == crate::mcp::PATH
        || path == crate::html::ACTS_PATH
        || path.starts_with("/.meridian/ui/")
        || crate::brand::is_icon(path)
}

/// Each request in its caller's turn.
pub async fn in_turn(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    if untimed(request.uri().path()) {
        return next.run(request).await;
    }
    let caller = caller_of(&app, request.headers());
    match app.turns.turn(&caller).await {
        Ok(turn) => {
            let answered = next.run(request).await;
            drop(turn);
            answered
        }
        Err(over) => {
            tracing::info!(
                caller,
                retry_after_seconds = over.retry_after_seconds,
                "{}",
                over.words
            );
            refused(&over)
        }
    }
}

/// A request refused for load: 503 with its retry-after.
fn refused(over: &Overloaded) -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        format!(
            "The dashboard is busy with your other requests; try again in {} s.\n",
            over.retry_after_seconds
        ),
    )
        .into_response();
    if let Ok(after) = HeaderValue::from_str(&over.retry_after_seconds.to_string()) {
        response.headers_mut().insert(RETRY_AFTER, after);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    /// One person's flood of requests waits behind itself: another person's
    /// request is answered after at most one more of the flooder's, and the
    /// flooder alone is refused past its share.
    #[tokio::test]
    async fn one_persons_flood_waits_behind_itself_and_another_is_answered_in_turn() {
        let turns = Arc::new(Turns::at_once(1));
        let held = turns.turn("p:flooder").await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for n in 0..4 {
            let (turns, order) = (Arc::clone(&turns), Arc::clone(&order));
            waiting.push(tokio::spawn(async move {
                let turn = turns.turn("p:flooder").await.unwrap();
                order.lock().unwrap().push(format!("flooder-{n}"));
                tokio::time::sleep(Duration::from_millis(5)).await;
                drop(turn);
            }));
            tokio::task::yield_now().await;
        }
        // Its share waiting already: the flooder's next is refused at once.
        let refused = turns.turn("p:flooder").await.err().unwrap();
        assert!(refused.retry_after_seconds >= 1 && refused.words.contains("its share"));
        let (quiet, quiet_order) = (Arc::clone(&turns), Arc::clone(&order));
        waiting.push(tokio::spawn(async move {
            let turn = quiet.turn("p:quiet").await.unwrap();
            quiet_order.lock().unwrap().push("quiet".into());
            drop(turn);
        }));
        tokio::task::yield_now().await;
        drop(held);
        for task in waiting {
            task.await.unwrap();
        }
        let order = order.lock().unwrap().clone();
        let quiet = order.iter().position(|said| said == "quiet").unwrap();
        assert!(
            quiet <= 1,
            "the quiet person waited behind {quiet}: {order:?}"
        );
        assert_eq!(turns.refused(), 1);
    }

    #[test]
    fn the_probe_the_files_and_mcp_take_no_turn() {
        for path in ["/healthz", "/mcp", "/.meridian/ui/kit.css", "/favicon.ico"] {
            assert!(untimed(path), "{path}");
        }
        for path in ["/", "/admin", "/plugins/x", "/registry/v2/"] {
            assert!(!untimed(path), "{path}");
        }
    }
}
