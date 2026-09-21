//! The group hook: how directory groups reach the dashboard's token through
//! the bundled Zitadel, which carries none by itself.
//!
//! An Actions v2 target in Zitadel, serving two calls, each refused unless
//! Zitadel signed it with this target's key (spec/deployment-dashboard-and-
//! access, requirement 7; decisions/015):
//!
//! - `POST /intent`, after a brokered sign-in: writes the person's complete
//!   group list onto their Zitadel user, replacing what was there, and an
//!   empty list when they have none.
//! - `POST /token`, when a token is issued: sets the `groups` claim from it.
//!
//! It is on the access path: what it writes becomes the directory groups the
//! dashboard's user groups are matched against. So it verifies every call,
//! holds nothing but the signing key, and is reviewed as access-control code.

pub mod groups;
pub mod signature;

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;

pub use groups::SamlAttributes;

pub struct Hook {
    pub signing_key: Vec<u8>,
    /// The dashboard's project in Zitadel, whose roles stand in for groups
    /// for people made in Zitadel itself.
    pub project_id: String,
    pub saml: SamlAttributes,
    pub now_s: fn() -> i64,
}

pub fn system_now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

pub fn router(hook: Arc<Hook>) -> Router {
    Router::new()
        .route("/intent", post(intent))
        .route("/token", post(token))
        .with_state(hook)
}

/// Verify, then parse. Nothing is read from a body that did not verify.
fn verified(hook: &Hook, headers: &HeaderMap, body: &Bytes) -> Result<Value, Box<Response>> {
    let header = headers
        .get(signature::HEADER)
        .and_then(|value| value.to_str().ok());
    if let Err(refusal) = signature::verify(header, body, &hook.signing_key, (hook.now_s)()) {
        tracing::warn!("refused a call: {refusal}");
        return Err(Box::new(
            (StatusCode::UNAUTHORIZED, refusal.to_string()).into_response(),
        ));
    }
    serde_json::from_slice(body)
        .map_err(|failed| Box::new((StatusCode::BAD_REQUEST, failed.to_string()).into_response()))
}

async fn intent(State(hook): State<Arc<Hook>>, headers: HeaderMap, body: Bytes) -> Response {
    match verified(&hook, &headers, &body) {
        Ok(payload) => Json(groups::on_intent(payload, &hook.saml)).into_response(),
        Err(refused) => *refused,
    }
}

async fn token(State(hook): State<Arc<Hook>>, headers: HeaderMap, body: Bytes) -> Response {
    match verified(&hook, &headers, &body) {
        Ok(payload) => Json(groups::on_token(&payload, &hook.project_id)).into_response(),
        Err(refused) => *refused,
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    const NOW: i64 = 1_790_380_800;

    fn hook() -> Router {
        router(Arc::new(Hook {
            signing_key: b"98KmsU67".to_vec(),
            project_id: "P-DASH".into(),
            saml: SamlAttributes::default(),
            now_s: || NOW,
        }))
    }

    async fn call(path: &str, body: &str, header: Option<String>) -> (StatusCode, String) {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(header) = header {
            request = request.header("ZITADEL-Signature", header);
        }
        let response = hook()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn a_signed_call_is_answered() {
        let body = r#"{"user_grants":[{"project_id":"P-DASH","roles":["traders"]}]}"#;
        let (status, reply) = call(
            "/token",
            body,
            Some(signature::sign(b"98KmsU67", NOW, body.as_bytes())),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(reply.contains("traders"));
    }

    #[tokio::test]
    async fn an_unsigned_or_forged_call_writes_nothing() {
        let body = r#"{"response":{"idpInformation":{"ldap":{"attributes":{"memberOf":["cn=admins"]}}},"updateUser":{}}}"#;
        assert_eq!(
            call("/intent", body, None).await.0,
            StatusCode::UNAUTHORIZED
        );
        let forged = signature::sign(b"not-the-key", NOW, body.as_bytes());
        let (status, reply) = call("/intent", body, Some(forged)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            !reply.contains("metadata"),
            "a refused call returns no user to write"
        );
    }
}
