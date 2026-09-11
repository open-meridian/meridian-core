//! The client a deployment calls the platform with. W3.3 and W3.4.
//!
//! Pull an instrument the master already knows, escalate one neither side
//! knows, and do both under a throttle so a burst of misses is not a burst of
//! anything.
//!
//! # What this file is really enforcing
//!
//! The product constraint is that the platform may scale, move, be redirected
//! regionally, and go down and come back, without a deployment restarting or
//! being reconfigured. Nearly every decision below is that constraint applied
//! to one detail.
//!
//! **One address, and it is configuration.** No instance list, no region, no
//! failover order. Nothing here has to change when the platform grows.
//!
//! **The audience is always that address**, never wherever a request lands. A
//! regional audience is a one-way door: adding a region would then mean every
//! deployment resigning under new configuration.
//!
//! **Redirects are followed and forgotten.** Being sent somewhere closer is the
//! platform's decision to make per request. Remembering it is how a deployment
//! ends up pinned to a region that has since gone away, so the configured
//! address is never written to and every call starts from it again.
//!
//! **A dropped connection is expected.** Scaling in closes them mid-flight, so
//! a failure is retried with a growing delay and a bound. Bounded because every
//! deployment retrying without one turns a brief platform problem into a
//! sustained one.
//!
//! **An outage is invisible downstream.** Every failure here is an error the
//! caller absorbs; nothing queues, nothing blocks a local resolve, and nothing
//! needs a person to restart it when the platform returns. Recovery needs no
//! bookmark either, because the highest version held is the cursor.
//!
//! # What it refuses to do
//!
//! It does not mint. Escalation asks the platform to, and the platform may
//! decline. It also does not escalate an ambiguous miss: ambiguity means the
//! instrument exists more than once, and answering that by creating a third is
//! the one mistake here that cannot be undone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_pb::v1::{
    EscalateInstrumentRequest, Identifier as PbIdentifier, InstrumentRecord as PbInstrument,
    MissReason, MissingInstrumentDetectedEvent,
};
use serde::Deserialize;

use crate::assertions::{DeploymentKey, SigningError, MAX_LIFETIME_SECONDS};
use crate::resolve::global_identifiers_strongest_first;

/// How a request is made. Two verbs is all the contract uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One request, already signed.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: String,

    /// The signed note, without its header prefix.
    pub assertion: String,

    pub body: Option<Vec<u8>>,
}

/// What came back.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,

    /// The `Location` header, empty when there was none.
    pub location: String,

    pub body: Vec<u8>,
}

/// A request that did not produce a response.
///
/// Always retryable, which is the point of the type: a connection closed by a
/// platform scaling in is indistinguishable from one closed by a platform
/// falling over, and both are answered the same way.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct TransportFailure(pub String);

/// How a request actually reaches the platform.
///
/// Behind a trait so the policy above can be tested without a network and
/// without a clock. The real implementation is [`HttpTransport`].
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, request: Request) -> Result<Response, TransportFailure>;
}

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("the platform is unreachable after {attempts} attempts: {last}")]
    Unreachable { attempts: u32, last: String },

    #[error("the platform refused the request: HTTP {status}")]
    Refused { status: u16 },

    #[error("the platform's answer could not be read: {0}")]
    Malformed(String),

    #[error("redirected more than {0} times")]
    TooManyRedirects(u8),

    #[error("refused a redirect from {from} to {to}")]
    UnsafeRedirect { from: String, to: String },

    #[error("could not sign the request: {0}")]
    Signing(#[from] SigningError),
}

impl PlatformError {
    /// Whether this is the platform being away rather than the platform
    /// answering. The caller keeps serving from the replica either way; the
    /// difference is what it is worth logging as.
    pub fn is_outage(&self) -> bool {
        matches!(self, PlatformError::Unreachable { .. })
    }
}

/// Everything a deployment is configured with.
///
/// One address, its own identifier, and policy. Nothing describing the
/// platform's shape, because the platform's shape is allowed to change without
/// anybody editing this.
#[derive(Debug, Clone)]
pub struct Config {
    /// The platform's base address, e.g. `https://platform.meridian.example`.
    pub address: String,

    pub deployment_id: String,

    /// Attempts per request, the first included.
    pub attempts: u32,

    /// The first delay after a failure. Each further wait doubles it.
    pub backoff: Duration,

    /// How long the same identifier set is left alone after being reacted to.
    pub throttle: Duration,

    /// How many redirects one request may follow.
    pub redirects: u8,

    /// How long each signed note claims to be good for.
    pub assertion_lifetime_secs: u64,
}

impl Config {
    /// Defaults chosen so a platform that is genuinely down is not hammered:
    /// three attempts a quarter-second apart and doubling, and a five-minute
    /// throttle on reacting to the same identifiers twice.
    pub fn new(address: impl Into<String>, deployment_id: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            deployment_id: deployment_id.into(),
            attempts: 3,
            backoff: Duration::from_millis(250),
            throttle: Duration::from_secs(300),
            redirects: 3,
            assertion_lifetime_secs: MAX_LIFETIME_SECONDS,
        }
    }
}

/// What reacting to a miss did.
#[derive(Debug, Clone, PartialEq)]
pub enum Reaction {
    /// The same identifiers were reacted to recently. Nothing was sent.
    Throttled,

    /// The master already knew it. W3.3, and most misses are this.
    Pulled(Box<PbInstrument>),

    /// Neither side knew it, so the platform minted a stub. W3.4.
    Minted(Box<PbInstrument>),

    /// The miss was ambiguous, so nothing was minted.
    ///
    /// More than one instrument matched locally. Whatever that is, it is not an
    /// instrument nobody has heard of, and minting one would answer a
    /// duplication problem by adding a duplicate.
    AmbiguityNotMinted,

    /// The platform neither knew it nor minted one. Its decision, not ours.
    Declined,
}

/// The deployment's end of the connection to the platform.
pub struct Platform {
    config: Config,
    key: DeploymentKey,
    transport: Arc<dyn Transport>,

    /// Identifier set to the moment it was last reacted to.
    ///
    /// Swept on every use, so it holds at most the sets seen inside one window
    /// rather than every set ever missed.
    reacted: Mutex<HashMap<String, i64>>,
}

impl Platform {
    pub fn new(config: Config, key: DeploymentKey, transport: Arc<dyn Transport>) -> Self {
        Self {
            config,
            key,
            transport,
            reacted: Mutex::new(HashMap::new()),
        }
    }

    /// The address this deployment is configured with. Never a redirect target.
    pub fn address(&self) -> &str {
        &self.config.address
    }

    /// W3.3 and W3.4 together: the whole reaction to a miss.
    ///
    /// The throttle is here rather than in either step because the workflow puts
    /// it here: a repeated miss inside the window is skipped before anything is
    /// sent. On failure the entry is cleared, so a transient outage delays an
    /// escalation instead of suppressing it permanently.
    pub async fn react_to_miss(
        &self,
        event: &MissingInstrumentDetectedEvent,
        now_ns: i64,
    ) -> Result<Reaction, PlatformError> {
        let key = throttle_key(&event.identifiers);
        if !self.begin(&key, now_ns) {
            return Ok(Reaction::Throttled);
        }

        match self.react(event, now_ns).await {
            Ok(reaction) => Ok(reaction),
            Err(failed) => {
                // The fixture's postcondition. A platform that was away must not
                // leave this identifier set unreportable for the rest of the
                // window.
                self.clear(&key);
                Err(failed)
            }
        }
    }

    async fn react(
        &self,
        event: &MissingInstrumentDetectedEvent,
        now_ns: i64,
    ) -> Result<Reaction, PlatformError> {
        // Global schemes only, strongest first. A brokerage symbol is
        // meaningless outside its own namespace, so sending one centrally would
        // make the master's answer depend on which rail happened to ask.
        for identifier in global_identifiers_strongest_first(&event.identifiers) {
            if let Some(record) = self
                .pull_identifier(
                    &identifier.scheme,
                    &identifier.value,
                    event.as_of_ns,
                    now_ns,
                )
                .await?
            {
                return Ok(Reaction::Pulled(Box::new(record)));
            }
        }

        if event.reason == MissReason::Ambiguous as i32 {
            return Ok(Reaction::AmbiguityNotMinted);
        }

        match self.escalate(event, now_ns).await? {
            Some(record) => Ok(Reaction::Minted(Box::new(record))),
            None => Ok(Reaction::Declined),
        }
    }

    /// W3.3, by the master's own name for the instrument.
    pub async fn pull_instrument(
        &self,
        instrument_id: &str,
        as_of_ns: i64,
        now_ns: i64,
    ) -> Result<Option<PbInstrument>, PlatformError> {
        let path = format!(
            "/api/v1/reference/instruments/{}?as_of_ns={}",
            encode(instrument_id),
            as_of_ns
        );
        let response = self.call(Method::Get, &path, None, now_ns).await?;
        found_or_nothing(response)
    }

    /// W3.3, by an identifier the master might know it under.
    pub async fn pull_identifier(
        &self,
        scheme: &str,
        value: &str,
        as_of_ns: i64,
        now_ns: i64,
    ) -> Result<Option<PbInstrument>, PlatformError> {
        let path = format!(
            "/api/v1/reference/identifiers/resolve?scheme={}&value={}&as_of_ns={}",
            encode(scheme),
            encode(value),
            as_of_ns
        );
        let response = self.call(Method::Get, &path, None, now_ns).await?;
        found_or_nothing(response)
    }

    /// W3.4. Ask the platform to mint identity from what nobody could resolve.
    ///
    /// `None` when the platform declined. Nothing here can mint, and nothing
    /// here retries a decline: the authority to create identity sits on the far
    /// side of this call by design.
    pub async fn escalate(
        &self,
        event: &MissingInstrumentDetectedEvent,
        now_ns: i64,
    ) -> Result<Option<PbInstrument>, PlatformError> {
        let request = EscalateInstrumentRequest {
            source: event.source.clone(),
            asset_class: event.asset_class.clone(),
            identifiers: event.identifiers.clone(),
            as_of_ns: event.as_of_ns,
            requesting_deployment_id: self.config.deployment_id.clone(),
        };

        let body = serde_json::to_vec(&serde_json::json!({
            "source": request.source,
            "asset_class": request.asset_class,
            "identifiers": request
                .identifiers
                .iter()
                .map(|identifier| serde_json::json!({
                    "scheme": identifier.scheme,
                    "value": identifier.value,
                    "source": identifier.source,
                }))
                .collect::<Vec<_>>(),
            "as_of_ns": request.as_of_ns,
            "requesting_deployment_id": request.requesting_deployment_id,
        }))
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;

        let response = self
            .call(
                Method::Post,
                "/api/v1/reference/escalate-instrument",
                Some(body),
                now_ns,
            )
            .await?;

        if response.status == 404 {
            return Ok(None);
        }
        let reply: WireReply = read(&response)?;
        if !reply.minted.unwrap_or(true) {
            return Ok(None);
        }
        reply.instrument.map(into_record).transpose()
    }

    /// One logical request: signed once, retried, and redirected by hand.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        now_ns: i64,
    ) -> Result<Response, PlatformError> {
        // Signed for the configured address, whatever address the request ends
        // up at. The audience names who a deployment believes it is talking to,
        // and that does not change because the platform sent it next door.
        let assertion = self.key.note(
            &self.config.deployment_id,
            &self.config.address,
            (now_ns.max(0) / 1_000_000_000) as u64,
            self.config.assertion_lifetime_secs,
        )?;

        let mut url = join(&self.config.address, path);
        let mut hops = 0u8;

        loop {
            let response = self.attempt(method, &url, &assertion, body.clone()).await?;

            let Some(target) = redirect_target(&response) else {
                return Ok(response);
            };
            if hops >= self.config.redirects {
                return Err(PlatformError::TooManyRedirects(self.config.redirects));
            }

            let next = absolute(&url, &target)
                .ok_or_else(|| PlatformError::Malformed(format!("unusable redirect: {target}")))?;
            if downgrades(&url, &next) {
                // The assertion travels in a header. A redirect that takes it
                // off TLS is either a mistake or somebody's plan.
                return Err(PlatformError::UnsafeRedirect {
                    from: url,
                    to: next,
                });
            }

            url = next;
            hops += 1;
        }
    }

    /// One hop, retried with a growing delay.
    async fn attempt(
        &self,
        method: Method,
        url: &str,
        assertion: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, PlatformError> {
        let mut last = String::new();
        let mut wait = self.config.backoff;

        for attempt in 1..=self.config.attempts.max(1) {
            let request = Request {
                method,
                url: url.to_string(),
                assertion: assertion.to_string(),
                body: body.clone(),
            };

            match self.transport.send(request).await {
                Ok(response) if retryable(response.status) => {
                    last = format!("HTTP {}", response.status);
                }
                Ok(response) => return Ok(response),
                Err(failure) => last = failure.0,
            }

            if attempt < self.config.attempts.max(1) {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
        }

        Err(PlatformError::Unreachable {
            attempts: self.config.attempts.max(1),
            last,
        })
    }

    /// Whether this identifier set may be reacted to now, recording it if so.
    fn begin(&self, key: &str, now_ns: i64) -> bool {
        let window = self.config.throttle.as_nanos() as i64;
        let mut reacted = self.reacted.lock().expect("the throttle lock is poisoned");

        // Swept here rather than on a timer, so the map holds one window's
        // worth of keys and nothing has to remember to clean it up.
        reacted.retain(|_, at| now_ns.saturating_sub(*at) < window);

        if reacted.contains_key(key) {
            return false;
        }
        reacted.insert(key.to_string(), now_ns);
        true
    }

    fn clear(&self, key: &str) {
        self.reacted
            .lock()
            .expect("the throttle lock is poisoned")
            .remove(key);
    }
}

/// A reply from the platform, in the shape of the contract's messages.
///
/// Unknown fields are ignored, matching protobuf. What is not ignored is a
/// field that arrived meaning something else: see [`into_record`].
#[derive(Debug, Deserialize)]
struct WireReply {
    found: Option<bool>,
    minted: Option<bool>,
    instrument: Option<WireRecord>,
}

#[derive(Debug, Deserialize)]
struct WireRecord {
    #[serde(default)]
    instrument_id: String,
    #[serde(default)]
    identifiers: Vec<WireIdentifier>,
    #[serde(default)]
    asset_class: String,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    exchange_mic: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    lifecycle_state: String,
    #[serde(default)]
    version: i64,
    #[serde(default)]
    valid_from_ns: i64,
    #[serde(default)]
    record_time_ns: i64,
}

#[derive(Debug, Deserialize)]
struct WireIdentifier {
    #[serde(default)]
    scheme: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    source: String,
}

/// The reply's record, or nothing when the platform said it had none.
fn found_or_nothing(response: Response) -> Result<Option<PbInstrument>, PlatformError> {
    if response.status == 404 {
        return Ok(None);
    }

    let reply: WireReply = read(&response)?;
    if !reply.found.unwrap_or(true) {
        return Ok(None);
    }
    reply.instrument.map(into_record).transpose()
}

fn read(response: &Response) -> Result<WireReply, PlatformError> {
    if !(200..300).contains(&response.status) {
        return Err(PlatformError::Refused {
            status: response.status,
        });
    }

    serde_json::from_slice(&response.body)
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))
}

/// The wire record as the rest of the crate expects it.
///
/// Two fields are checked rather than defaulted, because both have already gone
/// wrong in the direction a default hides. An instrument with no identity is not
/// an instrument, and a lifecycle state under a name the schema does not define
/// used to arrive as "unspecified" and be applied without complaint.
fn into_record(record: WireRecord) -> Result<PbInstrument, PlatformError> {
    if record.instrument_id.is_empty() {
        return Err(PlatformError::Malformed(
            "a record arrived with no instrument identifier".into(),
        ));
    }

    let lifecycle_state =
        meridian_pb::v1::InstrumentLifecycleState::from_str_name(&record.lifecycle_state)
            .ok_or_else(|| {
                PlatformError::Malformed(format!(
                    "{} arrived in lifecycle state {:?}, which the schema does not define",
                    record.instrument_id, record.lifecycle_state
                ))
            })? as i32;

    Ok(PbInstrument {
        instrument_id: record.instrument_id,
        identifiers: record
            .identifiers
            .into_iter()
            .map(|identifier| PbIdentifier {
                scheme: identifier.scheme,
                value: identifier.value,
                source: identifier.source,
            })
            .collect(),
        asset_class: record.asset_class,
        currency: record.currency,
        exchange_mic: record.exchange_mic,
        description: record.description,
        lifecycle_state,
        version: record.version,
        valid_from_ns: record.valid_from_ns,
        record_time_ns: record.record_time_ns,
    })
}

/// Statuses worth trying again. Everything else is an answer.
fn retryable(status: u16) -> bool {
    status >= 500 || status == 429 || status == 408
}

fn redirect_target(response: &Response) -> Option<String> {
    let redirected = matches!(response.status, 301 | 302 | 303 | 307 | 308);
    if redirected && !response.location.is_empty() {
        Some(response.location.clone())
    } else {
        None
    }
}

fn join(address: &str, path: &str) -> String {
    format!(
        "{}/{}",
        address.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// A `Location` resolved against the request it answered.
fn absolute(from: &str, location: &str) -> Option<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Some(location.to_string());
    }
    if let Some(path) = location.strip_prefix('/') {
        return Some(format!("{}/{}", origin(from)?, path));
    }
    None
}

fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split('/').next()?;
    Some(format!("{scheme}://{host}"))
}

/// Whether following this redirect would take the assertion off TLS.
fn downgrades(from: &str, to: &str) -> bool {
    from.starts_with("https://") && !to.starts_with("https://")
}

/// Percent-encoding for the few characters that can appear in an identifier.
///
/// Hand-rolled rather than pulled in: the alphabet an identifier uses is small
/// and known, and this is the whole of what a query string here needs.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// One key for one identifier set, order-independent.
///
/// Sorted, because two reports of the same miss listing the same identifiers in
/// a different order are the same miss, and a throttle that disagreed would let
/// the second through.
fn throttle_key(identifiers: &[PbIdentifier]) -> String {
    let mut parts: Vec<String> = identifiers
        .iter()
        .map(|identifier| {
            format!(
                "{}:{}:{}",
                identifier.scheme, identifier.source, identifier.value
            )
        })
        .collect();
    parts.sort();
    parts.join("|")
}

/// The real transport: HTTPS, with the pieces the constraint needs turned on.
///
/// Two settings here are load-bearing rather than tuning.
///
/// **No idle connections are kept.** A pooled connection is a machine chosen
/// once, and scaling in retires machines. Letting each request open its own
/// connection is what makes the address resolve again every time, so a
/// deployment follows the platform as it moves without knowing that it did.
///
/// **Redirects are not followed here.** They are followed in [`Platform::call`],
/// where the scheme can be checked and the hop counted, and where nothing
/// remembers the destination afterwards.
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    /// `timeout` bounds one attempt, not the retry sequence.
    pub fn new(timeout: Duration) -> Result<Self, PlatformError> {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(concat!("meridian-reference/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;

        Ok(Self { client })
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn send(&self, request: Request) -> Result<Response, TransportFailure> {
        let mut outbound = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url),
        };

        // Deliberately not `Bearer`. This names one deployment and one audience
        // and expires in a minute, and calling it a bearer token would invite
        // somebody to treat it like one.
        outbound = outbound.header("Authorization", format!("Meridian {}", request.assertion));

        if let Some(body) = request.body {
            outbound = outbound
                .header("Content-Type", "application/json")
                .body(body);
        }

        let response = outbound
            .send()
            .await
            .map_err(|failed| TransportFailure(failed.to_string()))?;

        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();

        let body = response
            .bytes()
            .await
            .map_err(|failed| TransportFailure(failed.to_string()))?
            .to_vec();

        Ok(Response {
            status,
            location,
            body,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use base64::Engine as _;

    use super::*;

    pub(crate) const ADDRESS: &str = "https://platform.meridian.example";
    pub(crate) const DEPLOYMENT: &str = "dep-local-1";
    pub(crate) const AS_OF: i64 = 1_757_289_600_000_000_000;
    const NOW: i64 = 1_757_376_000_000_000_000;

    /// A transport that answers from a script and remembers what it was asked.
    ///
    /// The last scripted answer repeats, so a test that wants "always fails" or
    /// "always redirects" says it once.
    pub(crate) struct Fake {
        script: Mutex<VecDeque<Result<Response, TransportFailure>>>,
        seen: Mutex<Vec<Request>>,
    }

    impl Fake {
        pub(crate) fn new(script: Vec<Result<Response, TransportFailure>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into()),
                seen: Mutex::new(Vec::new()),
            })
        }

        pub(crate) fn urls(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|request| request.url.clone())
                .collect()
        }

        pub(crate) fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        /// The claims of the note sent on one request.
        fn claims(&self, index: usize) -> serde_json::Value {
            let seen = self.seen.lock().unwrap();
            let payload = seen[index].assertion.split('.').nth(1).unwrap();
            serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap()
        }
    }

    #[async_trait::async_trait]
    impl Transport for Fake {
        async fn send(&self, request: Request) -> Result<Response, TransportFailure> {
            self.seen.lock().unwrap().push(request);

            let mut script = self.script.lock().unwrap();
            if script.len() > 1 {
                return script.pop_front().unwrap();
            }
            script
                .front()
                .cloned()
                .unwrap_or_else(|| Ok(reply(404, "{\"found\": false}")))
        }
    }

    pub(crate) fn reply(status: u16, body: &str) -> Response {
        Response {
            status,
            location: String::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn redirect(status: u16, location: &str) -> Response {
        Response {
            status,
            location: location.into(),
            body: Vec::new(),
        }
    }

    pub(crate) fn failure(detail: &str) -> Result<Response, TransportFailure> {
        Err(TransportFailure(detail.into()))
    }

    /// A record in the shape the platform serialises.
    pub(crate) fn record_json(instrument_id: &str) -> String {
        serde_json::json!({
            "found": true,
            "instrument": {
                "instrument_id": instrument_id,
                "identifiers": [{"scheme": "figi", "value": "BBG000B9XRY4", "source": ""}],
                "asset_class": "EQUITY",
                "currency": "USD",
                "exchange_mic": "XNAS",
                "description": "Apple Inc. common stock",
                "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
                "version": 4,
                "valid_from_ns": AS_OF,
                "record_time_ns": AS_OF,
            }
        })
        .to_string()
    }

    pub(crate) fn platform(transport: Arc<Fake>) -> Platform {
        Platform::new(
            Config::new(ADDRESS, DEPLOYMENT),
            DeploymentKey::generate(),
            transport,
        )
    }

    fn miss(reason: MissReason) -> MissingInstrumentDetectedEvent {
        MissingInstrumentDetectedEvent {
            source: "snaptrade".into(),
            asset_class: "EQUITY".into(),
            identifiers: vec![
                PbIdentifier {
                    scheme: "symbol".into(),
                    value: "ZZTOP".into(),
                    source: "snaptrade".into(),
                },
                PbIdentifier {
                    scheme: "figi".into(),
                    value: "BBG000ZZTOP1".into(),
                    source: String::new(),
                },
            ],
            as_of_ns: AS_OF,
            publisher_instance_id: "custody-snaptrade-1".into(),
            reason: reason as i32,
            observed_at_ns: NOW,
        }
    }

    #[tokio::test]
    async fn a_pull_returns_what_the_master_holds() {
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ONE")))]);
        let platform = platform(transport.clone());

        let record = platform
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(record.instrument_id, "INS-ONE");
        assert_eq!(record.version, 4);
        assert!(transport.urls()[0].starts_with(&format!(
            "{ADDRESS}/api/v1/reference/instruments/INS-ONE?as_of_ns="
        )));
    }

    #[tokio::test]
    async fn a_master_that_does_not_hold_it_is_nothing_rather_than_an_error() {
        let transport = Fake::new(vec![Ok(reply(
            404,
            "{\"found\": false, \"miss_reason\": \"MISS_REASON_NOT_FOUND\"}",
        ))]);

        let found = platform(transport)
            .pull_identifier("figi", "BBG000ZZTOP1", AS_OF, NOW)
            .await
            .unwrap();

        assert!(found.is_none());
    }

    #[tokio::test]
    async fn every_request_names_the_deployment_and_the_configured_address() {
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ONE")))]);
        platform(transport.clone())
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap();

        let claims = transport.claims(0);
        assert_eq!(claims["iss"], DEPLOYMENT);
        assert_eq!(claims["sub"], DEPLOYMENT);
        assert_eq!(claims["aud"], ADDRESS);
        assert_eq!(
            claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
            60
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_connection_is_retried_and_then_gives_up() {
        // Scaling in closes connections mid-flight, so this is the expected
        // case rather than the exceptional one. It ends, because every
        // deployment retrying without a bound turns a brief problem into a
        // sustained one.
        let transport = Fake::new(vec![failure("connection reset")]);
        let failed = platform(transport.clone())
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap_err();

        assert!(failed.is_outage());
        assert!(matches!(
            failed,
            PlatformError::Unreachable { attempts: 3, .. }
        ));
        assert_eq!(transport.calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_recovers_is_not_a_failure() {
        let transport = Fake::new(vec![
            failure("connection reset"),
            Ok(reply(503, "")),
            Ok(reply(200, &record_json("INS-ONE"))),
        ]);

        let record = platform(transport.clone())
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(record.instrument_id, "INS-ONE");
        assert_eq!(transport.calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_an_answer_and_is_not_retried() {
        // Sending the same rejected request twice is not a recovery strategy,
        // and doing it under load is how a refusal becomes an outage.
        let transport = Fake::new(vec![Ok(reply(401, "{\"error\": \"not authenticated\"}"))]);

        let failed = platform(transport.clone())
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap_err();

        assert!(matches!(failed, PlatformError::Refused { status: 401 }));
        assert!(!failed.is_outage());
        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn a_redirect_to_a_region_is_followed() {
        let transport = Fake::new(vec![
            Ok(redirect(
                307,
                "https://eu.platform.meridian.example/api/v1/reference/instruments/INS-ONE",
            )),
            Ok(reply(200, &record_json("INS-ONE"))),
        ]);

        let record = platform(transport.clone())
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(record.instrument_id, "INS-ONE");
        assert!(transport.urls()[1].starts_with("https://eu.platform.meridian.example/"));
    }

    #[tokio::test]
    async fn a_redirect_is_not_remembered() {
        // Being sent somewhere closer is the platform's decision to make on any
        // request. Keeping it is how a deployment ends up pinned to a region
        // that has gone away.
        let transport = Fake::new(vec![
            Ok(redirect(
                307,
                "https://eu.platform.meridian.example/api/v1/reference/instruments/INS-ONE",
            )),
            Ok(reply(200, &record_json("INS-ONE"))),
        ]);
        let platform = platform(transport.clone());

        platform
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap();
        platform
            .pull_instrument("INS-TWO", AS_OF, NOW)
            .await
            .unwrap();

        let urls = transport.urls();
        assert!(urls[0].starts_with(ADDRESS));
        assert!(
            urls[2].starts_with(ADDRESS),
            "the second call went to {}",
            urls[2]
        );
        assert_eq!(platform.address(), ADDRESS);
    }

    #[tokio::test]
    async fn a_redirected_request_still_names_the_configured_address_as_its_audience() {
        // A regional audience is a one-way door: adding a region would mean
        // every deployment resigning under new configuration.
        let transport = Fake::new(vec![
            Ok(redirect(
                307,
                "https://eu.platform.meridian.example/api/v1/reference/instruments/INS-ONE",
            )),
            Ok(reply(200, &record_json("INS-ONE"))),
        ]);

        platform(transport.clone())
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap();

        assert_eq!(transport.claims(1)["aud"], ADDRESS);
    }

    #[tokio::test]
    async fn a_redirect_off_tls_is_refused() {
        // The assertion travels in a header.
        let transport = Fake::new(vec![Ok(redirect(
            302,
            "http://eu.platform.meridian.example/x",
        ))]);

        let failed = platform(transport)
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap_err();

        assert!(matches!(failed, PlatformError::UnsafeRedirect { .. }));
    }

    #[tokio::test]
    async fn a_redirect_that_never_arrives_gives_up() {
        let transport = Fake::new(vec![Ok(redirect(
            307,
            "/api/v1/reference/instruments/INS-ONE",
        ))]);

        let failed = platform(transport)
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap_err();

        assert!(matches!(failed, PlatformError::TooManyRedirects(3)));
    }

    #[tokio::test]
    async fn a_miss_is_pulled_on_global_schemes_and_never_on_a_symbol() {
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ONE")))]);

        let reaction = platform(transport.clone())
            .react_to_miss(&miss(MissReason::NotFound), NOW)
            .await
            .unwrap();

        assert!(matches!(reaction, Reaction::Pulled(_)));
        let urls = transport.urls();
        assert_eq!(urls.len(), 1);
        assert!(urls[0].contains("scheme=figi"));
        assert!(!urls.iter().any(|url| url.contains("symbol")));
    }

    #[tokio::test]
    async fn a_miss_the_master_does_not_know_is_escalated() {
        let transport = Fake::new(vec![
            Ok(reply(404, "{\"found\": false}")),
            Ok(reply(
                201,
                &serde_json::json!({
                    "minted": true,
                    "instrument": {
                        "instrument_id": "LCL-01J8",
                        "asset_class": "EQUITY",
                        "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_DEFINE",
                        "version": 1,
                    }
                })
                .to_string(),
            )),
        ]);

        let reaction = platform(transport.clone())
            .react_to_miss(&miss(MissReason::NotFound), NOW)
            .await
            .unwrap();

        match reaction {
            Reaction::Minted(record) => {
                assert_eq!(record.instrument_id, "LCL-01J8");
                assert_eq!(
                    record.lifecycle_state,
                    meridian_pb::v1::InstrumentLifecycleState::Define as i32
                );
            }
            other => panic!("expected a mint, got {other:?}"),
        }

        // The escalation carries every identifier, the brokerage symbol
        // included: they are the stub an administrator completes.
        let body = transport.seen.lock().unwrap()[1].body.clone().unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(sent["identifiers"].as_array().unwrap().len(), 2);
        assert_eq!(sent["requesting_deployment_id"], DEPLOYMENT);
        assert_eq!(sent["source"], "snaptrade");
    }

    #[tokio::test]
    async fn an_ambiguous_miss_is_not_minted() {
        // More than one instrument matched. Whatever that is, it is not an
        // instrument nobody has heard of, and minting one would answer a
        // duplication problem by adding a duplicate.
        let transport = Fake::new(vec![Ok(reply(404, "{\"found\": false}"))]);

        let reaction = platform(transport.clone())
            .react_to_miss(&miss(MissReason::Ambiguous), NOW)
            .await
            .unwrap();

        assert_eq!(reaction, Reaction::AmbiguityNotMinted);
        assert!(!transport
            .urls()
            .iter()
            .any(|url| url.contains("escalate-instrument")));
    }

    #[tokio::test]
    async fn a_declined_escalation_is_not_an_error() {
        let transport = Fake::new(vec![
            Ok(reply(404, "{\"found\": false}")),
            Ok(reply(200, "{\"minted\": false}")),
        ]);

        let reaction = platform(transport)
            .react_to_miss(&miss(MissReason::NotFound), NOW)
            .await
            .unwrap();

        assert_eq!(reaction, Reaction::Declined);
    }

    #[tokio::test]
    async fn a_repeated_miss_inside_the_window_is_skipped() {
        // A burst of misses is not a burst of mints.
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ONE")))]);
        let platform = platform(transport.clone());

        platform
            .react_to_miss(&miss(MissReason::NotFound), NOW)
            .await
            .unwrap();
        let again = platform
            .react_to_miss(&miss(MissReason::NotFound), NOW + 1_000_000_000)
            .await
            .unwrap();

        assert_eq!(again, Reaction::Throttled);
        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn the_throttle_lets_go_once_the_window_passes() {
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ONE")))]);
        let platform = platform(transport.clone());

        platform
            .react_to_miss(&miss(MissReason::NotFound), NOW)
            .await
            .unwrap();
        let later = platform
            .react_to_miss(&miss(MissReason::NotFound), NOW + 301_000_000_000)
            .await
            .unwrap();

        assert!(matches!(later, Reaction::Pulled(_)));
        assert_eq!(transport.calls(), 2);
    }

    #[tokio::test]
    async fn the_order_identifiers_are_listed_in_does_not_defeat_the_throttle() {
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ONE")))]);
        let platform = platform(transport.clone());

        platform
            .react_to_miss(&miss(MissReason::NotFound), NOW)
            .await
            .unwrap();

        let mut reordered = miss(MissReason::NotFound);
        reordered.identifiers.reverse();
        let again = platform.react_to_miss(&reordered, NOW).await.unwrap();

        assert_eq!(again, Reaction::Throttled);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_reaction_clears_the_throttle_so_a_later_report_retries() {
        // The fixture's postcondition. A transient outage must not permanently
        // suppress an escalation.
        let transport = Fake::new(vec![failure("connection reset")]);
        let platform = platform(transport.clone());

        assert!(platform
            .react_to_miss(&miss(MissReason::NotFound), NOW)
            .await
            .unwrap_err()
            .is_outage());

        let second = platform
            .react_to_miss(&miss(MissReason::NotFound), NOW + 1_000_000_000)
            .await;

        assert!(
            second.is_err(),
            "the second report was throttled instead of retried"
        );
        assert_eq!(transport.calls(), 6);
    }

    #[tokio::test]
    async fn a_record_in_a_state_the_schema_does_not_define_is_refused() {
        // The platform sent its column's spelling here for a while, and a
        // client that defaulted read every instrument as unspecified and applied
        // it. Loud on both sides now.
        let transport = Fake::new(vec![Ok(reply(
            200,
            &serde_json::json!({
                "found": true,
                "instrument": {"instrument_id": "INS-ONE", "lifecycle_state": "DEFINED"}
            })
            .to_string(),
        ))]);

        let failed = platform(transport)
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap_err();

        match failed {
            PlatformError::Malformed(detail) => assert!(detail.contains("DEFINED"), "{detail}"),
            other => panic!("expected a malformed record, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_record_with_no_identity_is_refused() {
        let transport = Fake::new(vec![Ok(reply(
            200,
            "{\"found\": true, \"instrument\": {\"lifecycle_state\": \"INSTRUMENT_LIFECYCLE_STATE_ACTIVE\"}}",
        ))]);

        let failed = platform(transport)
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap_err();

        assert!(matches!(failed, PlatformError::Malformed(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn the_replica_keeps_answering_while_the_platform_is_away() {
        // The requirement the whole crate exists for. A platform outage is an
        // error on this call and nothing at all downstream.
        use crate::{apply, resolve_identifier, MemoryStore};
        use meridian_pb::v1::ResolveIdentifierRequest;

        let store = MemoryStore::new();
        apply(
            &store,
            PbInstrument {
                instrument_id: "INS-HELD".into(),
                identifiers: vec![PbIdentifier {
                    scheme: "figi".into(),
                    value: "BBG000B9XRY4".into(),
                    source: String::new(),
                }],
                lifecycle_state: meridian_pb::v1::InstrumentLifecycleState::Active as i32,
                version: 1,
                valid_from_ns: AS_OF - 1,
                ..Default::default()
            },
            NOW,
        )
        .unwrap();

        let transport = Fake::new(vec![failure("no route to host")]);
        assert!(platform(transport)
            .pull_instrument("INS-OTHER", AS_OF, NOW)
            .await
            .unwrap_err()
            .is_outage());

        let reply = resolve_identifier(
            &store,
            &ResolveIdentifierRequest {
                identifiers: vec![PbIdentifier {
                    scheme: "figi".into(),
                    value: "BBG000B9XRY4".into(),
                    source: String::new(),
                }],
                as_of_ns: AS_OF,
                exchange_mic: String::new(),
                currency: String::new(),
            },
        )
        .unwrap();

        assert!(reply.found);
        assert_eq!(reply.instrument_id, "INS-HELD");
    }
}
