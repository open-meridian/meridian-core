//! A person's way to a plugin's page: the sidecar's HTTP listener.
//!
//! Decisions 014 and 021, W6.9. The dashboard serves each plugin instance's
//! page on that instance's own origin and, per request, signs who the person
//! is and what they hold on the plugin, in one header, `Meridian-Caller`. This
//! listener admits the dashboard alone (a NetworkPolicy in the chart), and for
//! every request:
//!
//! 1. verifies the assertion: a key it holds, a signature that verifies, this
//!    instance as its audience, in date, living no longer than 60 seconds, and
//!    an id it has not seen within them, so one captured in flight cannot be
//!    replayed;
//! 2. removes every identity the request arrived claiming -- another
//!    `Meridian-Caller`, a cookie, an `Authorization` -- and forwards exactly
//!    one `Meridian-Caller`, the one it verified;
//! 3. streams the request to the loopback port the plugin declared at
//!    registration, and the answer back, without reading or holding either.
//!
//! The dashboard's public keys are files in a mounted ConfigMap, one per key
//! id, read when an id is first seen, so a key made after this started, or a
//! rotation's second key, needs no restart.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use meridian_pb::v1::{CallerAssertion, CallerClaims};
use prost::Message;

use crate::service::Sidecar;

/// The one header an assertion travels in (plans/a-person-reaches-a-plugin,
/// ruling 2).
pub const HEADER: &str = "meridian-caller";

/// The longest an assertion may live, and the most a clock may disagree.
const LIFETIME_NS: i64 = 60 * 1_000_000_000;
const SKEW_NS: i64 = 5 * 1_000_000_000;

/// Why a request did not reach the plugin. Each is said plainly in the
/// response, and none reaches the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Malformed(String),
    UnknownKey(String),
    BadSignature,
    ForAnotherInstance(String),
    OutOfDate,
    LivesTooLong,
    Replayed,
    NoInterface,
    PluginUnreachable(String),
}

impl Refusal {
    fn status(&self) -> StatusCode {
        match self {
            Refusal::Missing
            | Refusal::Malformed(_)
            | Refusal::UnknownKey(_)
            | Refusal::BadSignature => StatusCode::UNAUTHORIZED,
            Refusal::ForAnotherInstance(_)
            | Refusal::OutOfDate
            | Refusal::LivesTooLong
            | Refusal::Replayed => StatusCode::FORBIDDEN,
            Refusal::NoInterface => StatusCode::NOT_FOUND,
            Refusal::PluginUnreachable(_) => StatusCode::BAD_GATEWAY,
        }
    }

    pub fn said(&self) -> String {
        match self {
            Refusal::Missing => "no Meridian-Caller: open this plugin from the dashboard".into(),
            Refusal::Malformed(why) => format!("the Meridian-Caller does not read: {why}"),
            Refusal::UnknownKey(id) => {
                format!("the assertion is signed by {id}, which this sidecar does not hold")
            }
            Refusal::BadSignature => "the assertion's signature does not verify".into(),
            Refusal::ForAnotherInstance(audience) => {
                format!("the assertion is for {audience}, not this plugin")
            }
            Refusal::OutOfDate => "the assertion is out of date".into(),
            Refusal::LivesTooLong => "the assertion claims to live longer than 60 seconds".into(),
            Refusal::Replayed => "the assertion has been presented already".into(),
            Refusal::NoInterface => "this plugin serves no page".into(),
            Refusal::PluginUnreachable(why) => format!("the plugin did not answer: {why}"),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (self.status(), self.said()).into_response()
    }
}

/// Verifies the dashboard's assertions for one plugin instance.
pub struct Verifier {
    instance: String,
    dir: PathBuf,
    keys: RwLock<HashMap<String, VerifyingKey>>,
    /// Ids seen, and when each may be forgotten: once its assertion is out of
    /// date it cannot be replayed anyway. Bounded by what 60 seconds brings.
    seen: Mutex<HashMap<String, i64>>,
}

impl Verifier {
    pub fn new(instance: impl Into<String>, dir: impl Into<PathBuf>) -> Verifier {
        Verifier {
            instance: instance.into(),
            dir: dir.into(),
            keys: RwLock::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// One holding a key already, for tests.
    pub fn holding(instance: &str, key_id: &str, key: VerifyingKey) -> Verifier {
        let verifier = Verifier::new(instance, PathBuf::new());
        verifier
            .keys
            .write()
            .expect("key lock poisoned")
            .insert(key_id.to_string(), key);
        verifier
    }

    fn key(&self, key_id: &str) -> Result<VerifyingKey, Refusal> {
        if let Some(key) = self.keys.read().expect("key lock poisoned").get(key_id) {
            return Ok(*key);
        }
        // A key id names a file, so it is held to what a file name may be here
        // before anything is read: nothing climbs out of the directory.
        let named = !key_id.is_empty()
            && key_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
            && !key_id.starts_with('.');
        if !named {
            return Err(Refusal::UnknownKey(key_id.to_string()));
        }
        let pem = std::fs::read_to_string(self.dir.join(format!("{key_id}.pem")))
            .map_err(|_| Refusal::UnknownKey(key_id.to_string()))?;
        let key = VerifyingKey::from_public_key_pem(&pem)
            .map_err(|_| Refusal::UnknownKey(key_id.to_string()))?;
        self.keys
            .write()
            .expect("key lock poisoned")
            .insert(key_id.to_string(), key);
        Ok(key)
    }

    /// The claims a header carries, if every check holds: the assertion's
    /// own, and that it has not reached this door before.
    pub fn verify(&self, header: Option<&str>, now_ns: i64) -> Result<CallerClaims, Refusal> {
        let header = header.ok_or(Refusal::Missing)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(header.trim())
            .map_err(|failed| Refusal::Malformed(failed.to_string()))?;
        let assertion = CallerAssertion::decode(bytes.as_slice())
            .map_err(|failed| Refusal::Malformed(failed.to_string()))?;
        let claims = self.vouched(&assertion, now_ns)?;
        let mut seen = self.seen.lock().expect("seen lock poisoned");
        seen.retain(|_, forget_at| *forget_at >= now_ns);
        if seen
            .insert(claims.assertion_id.clone(), claims.expires_at_ns + SKEW_NS)
            .is_some()
        {
            return Err(Refusal::Replayed);
        }
        Ok(claims)
    }

    /// The claims an assertion carries, if it is the dashboard's, for this
    /// instance, in date and short-lived -- without the replay rule. A command
    /// sent for a person (W4.9) carries the assertion the plugin was handed at
    /// this door, which has therefore been seen once already; and only the
    /// plugin can reach the surface that carries it.
    pub fn vouched(
        &self,
        assertion: &CallerAssertion,
        now_ns: i64,
    ) -> Result<CallerClaims, Refusal> {
        let key = self.key(&assertion.key_id)?;
        let signature =
            Signature::from_slice(&assertion.signature).map_err(|_| Refusal::BadSignature)?;
        key.verify(&assertion.claims, &signature)
            .map_err(|_| Refusal::BadSignature)?;
        let claims = CallerClaims::decode(assertion.claims.as_slice())
            .map_err(|failed| Refusal::Malformed(failed.to_string()))?;

        if claims.audience_instance_id != self.instance {
            return Err(Refusal::ForAnotherInstance(claims.audience_instance_id));
        }
        if claims.expires_at_ns - claims.issued_at_ns > LIFETIME_NS {
            return Err(Refusal::LivesTooLong);
        }
        if now_ns > claims.expires_at_ns + SKEW_NS || now_ns + SKEW_NS < claims.issued_at_ns {
            return Err(Refusal::OutOfDate);
        }
        if claims.assertion_id.is_empty() {
            return Err(Refusal::Malformed("the assertion carries no id".into()));
        }
        Ok(claims)
    }
}

/// What the listener needs.
#[derive(Clone)]
pub struct FrontDoor {
    pub sidecar: Arc<Sidecar>,
    pub verifier: Arc<Verifier>,
    pub client: reqwest::Client,
}

/// Headers that describe one connection rather than the request, so go no
/// further than it, either way.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The identities a request may arrive claiming, which the plugin is never
/// told by anybody but this sidecar; and the host, which is the plugin's own.
const CLAIMED: [&str; 4] = ["cookie", "authorization", HEADER, "host"];

fn kept(headers: &HeaderMap, dropped: &[&[&str]]) -> HeaderMap {
    let mut kept = HeaderMap::new();
    for (name, value) in headers {
        if !dropped.iter().any(|names| names.contains(&name.as_str())) {
            kept.append(name.clone(), value.clone());
        }
    }
    kept
}

impl FrontDoor {
    pub fn new(
        sidecar: Arc<Sidecar>,
        verifier: impl Into<Arc<Verifier>>,
    ) -> Result<FrontDoor, String> {
        Ok(FrontDoor {
            sidecar,
            verifier: verifier.into(),
            client: FrontDoor::client()?,
        })
    }

    /// A client that passes the plugin's answer on as it came. A redirect goes
    /// back to the browser, which follows it through the dashboard with a new
    /// assertion; followed here, it would be a second request under the first
    /// one's, to wherever the plugin said. No proxy from the environment, and
    /// no overall timeout, since a page may stream for as long as it is open.
    pub fn client() -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|failed| format!("the front door's client could not be built: {failed}"))
    }
}

pub fn router(front_door: FrontDoor) -> Router {
    Router::new().fallback(pass).with_state(front_door)
}

async fn pass(State(front_door): State<FrontDoor>, request: Request) -> Response {
    match through(&front_door, request, now_ns()).await {
        Ok(response) => response,
        Err(refusal) => refusal.into_response(),
    }
}

async fn through(
    front_door: &FrontDoor,
    request: Request,
    now_ns: i64,
) -> Result<Response, Refusal> {
    // One, or none. Two is somebody adding theirs to the dashboard's, and
    // which of them the plugin would believe is not a question to leave open.
    let mut presented = request.headers().get_all(HEADER).iter();
    let header = presented.next().cloned();
    if presented.next().is_some() {
        return Err(Refusal::Malformed("more than one Meridian-Caller".into()));
    }
    let text = header
        .as_ref()
        .map(|value| {
            value
                .to_str()
                .map_err(|_| Refusal::Malformed("not text".into()))
        })
        .transpose()?;
    front_door.verifier.verify(text, now_ns)?;
    let verified = header.expect("verified above");

    let port = front_door
        .sidecar
        .registration()
        .and_then(|registration| registration.interface_port)
        .ok_or(Refusal::NoInterface)?;

    let (parts, body) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut headers = kept(&parts.headers, &[&HOP_BY_HOP, &CLAIMED]);
    headers.insert(HeaderName::from_static(HEADER), verified);
    let answer = front_door
        .client
        .request(parts.method, format!("http://127.0.0.1:{port}{path}"))
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
        .map_err(|failed| Refusal::PluginUnreachable(failed.to_string()))?;

    let mut response = Response::builder().status(answer.status());
    for (name, value) in &kept(answer.headers(), &[&HOP_BY_HOP]) {
        response = response.header(name, value);
    }
    response
        .body(Body::from_stream(answer.bytes_stream()))
        .map_err(|failed| Refusal::PluginUnreachable(failed.to_string()))
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
