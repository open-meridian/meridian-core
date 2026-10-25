//! The client a deployment calls the platform with. W3.3 and W3.4.
//!
//! Ask the platform about one of the deployment's records when a person asks,
//! by its open identifiers (W3.3, contract v10); and the client of the
//! platform's escalation route, which a deployment no longer calls (W3.4).
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
//! It does not mint, and it sends no licensed scheme's value: a deployment's
//! records are its own (decisions/030), and asking is by open identifiers
//! alone.

use std::sync::Arc;
use std::time::Duration;

use meridian_domain::asset_class;
use meridian_domain::v1::{
    AssetClass, ClaimCodePurpose, DiagnosticBundle, DiagnosticBundleReceipt,
    EscalateInstrumentReply, EscalateInstrumentRequest, FieldRequirement,
    Identifier as PbIdentifier, InstrumentRecord as PbInstrument, LicenceList, LicencePace,
    RedeemClaimCodeReply, ReplacementFeedEntry, SecurityTemplate, VenueRecord,
};
use serde::Deserialize;

use crate::assertions::{DeploymentKey, SigningError, MAX_LIFETIME_SECONDS};
use meridian_symbology::{open_identifiers_strongest_first, GLOBAL_ID};

/// What a pull asks the platform by, in order (W3.3): the open identifiers,
/// strongest first, then an administrator's own code -- a `symbol` with the
/// administrator as its source (ruling 545). A symbol reaches here only
/// where the licence list admitted it (`reference::sendable_on`), and is
/// asked with its source, without which the platform finds nothing.
pub fn pulled_by(identifiers: &[PbIdentifier]) -> Vec<&PbIdentifier> {
    let mut asked = open_identifiers_strongest_first(identifiers);
    asked.extend(identifiers.iter().filter(|identifier| {
        identifier.scheme == "symbol"
            && !identifier.source.is_empty()
            && !identifier.value.trim().is_empty()
    }));
    asked
}

/// The resolve path for an identifier, its source named where it has one.
fn resolve_path(identifier: &PbIdentifier, as_of_ns: i64) -> String {
    let mut path = format!(
        "/api/v1/reference/identifiers/resolve?scheme={}&value={}&as_of_ns={}",
        encode(&identifier.scheme),
        encode(&identifier.value),
        as_of_ns
    );
    if !identifier.source.is_empty() {
        path.push_str(&format!("&source={}", encode(&identifier.source)));
    }
    path
}

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
    /// answering. The caller keeps serving from the instrument store either way; the
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

    /// How many redirects one request may follow.
    pub redirects: u8,

    /// How long each signed note claims to be good for.
    pub assertion_lifetime_secs: u64,
}

impl Config {
    /// Defaults chosen so a platform that is genuinely down is not hammered:
    /// three attempts a quarter-second apart and doubling.
    pub fn new(address: impl Into<String>, deployment_id: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            deployment_id: deployment_id.into(),
            attempts: 3,
            backoff: Duration::from_millis(250),
            redirects: 3,
            assertion_lifetime_secs: MAX_LIFETIME_SECONDS,
        }
    }
}

/// The deployment's end of the connection to the platform.
pub struct Platform {
    config: Config,
    key: DeploymentKey,
    transport: Arc<dyn Transport>,
}

/// One component of this deployment, as it describes itself. W5.19.
///
/// The fields are the contract's and nothing more. Adding one that names an
/// account, an instrument or a person would put a customer's business on a
/// path that leaves their cluster unprompted, which
/// `intent/deployment-lifecycle-and-plugin-distribution` rules out.
#[derive(Clone, Debug)]
pub struct ComponentReport {
    pub component: String,
    pub version: String,
    pub schema_version: i64,
    pub health: &'static str,
    pub detail: String,
    pub started_at_ns: i64,
}

impl ComponentReport {
    pub fn serving(
        component: &str,
        version: &str,
        schema_version: i64,
        started_at_ns: i64,
    ) -> Self {
        Self {
            component: component.into(),
            version: version.into(),
            schema_version,
            health: "COMPONENT_HEALTH_SERVING",
            detail: String::new(),
            started_at_ns,
        }
    }

    pub fn degraded(mut self, detail: impl Into<String>) -> Self {
        self.health = "COMPONENT_HEALTH_DEGRADED";
        self.detail = detail.into();
        self
    }
}

impl Platform {
    pub fn new(config: Config, key: DeploymentKey, transport: Arc<dyn Transport>) -> Self {
        Self {
            config,
            key,
            transport,
        }
    }

    /// The address this deployment is configured with. Never a redirect target.
    pub fn address(&self) -> &str {
        &self.config.address
    }

    /// W3.3. Ask the platform about one of the deployment's records, when a
    /// person asks: by its open identifiers, strongest first -- its global ID
    /// by the master's own name for it, then a FIGI, an ISO 4217 code -- and
    /// never a licensed scheme's value or a source-scoped symbol, which never
    /// leave the deployment (W3; intent/vendor-sourced-reference-data). The
    /// first record found, or `None` when the platform holds none by them.
    ///
    /// Nothing escalates and nothing is minted: from contract v10 a
    /// deployment's records are its own (decisions/030), and the platform
    /// records what it was asked and did not hold for its staff (W1.13).
    pub async fn ask(
        &self,
        identifiers: &[PbIdentifier],
        as_of_ns: i64,
        now_ns: i64,
    ) -> Result<Option<PbInstrument>, PlatformError> {
        for identifier in pulled_by(identifiers) {
            let found = if identifier.scheme == GLOBAL_ID {
                self.pull_instrument(&identifier.value, as_of_ns, now_ns)
                    .await?
            } else {
                self.pull_identifier(identifier, as_of_ns, now_ns).await?
            };
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// W3.3 with the venues its answer names (contract v18): the first
    /// record found by the open identifiers, strongest first, and its listing
    /// venue and that venue's operating venue, which the instrument store
    /// keeps (W3.5).
    pub async fn ask_with_venues(
        &self,
        identifiers: &[PbIdentifier],
        as_of_ns: i64,
        now_ns: i64,
    ) -> Result<Option<(PbInstrument, Vec<VenueRecord>)>, PlatformError> {
        for identifier in pulled_by(identifiers) {
            let path = if identifier.scheme == GLOBAL_ID {
                format!(
                    "/api/v1/reference/instruments/{}?as_of_ns={}",
                    encode(&identifier.value),
                    as_of_ns
                )
            } else {
                resolve_path(identifier, as_of_ns)
            };
            let response = self.call(Method::Get, &path, None, now_ns).await?;
            if let Some(found) = found_with_venues(response)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// W3.15's ask, by W3.3's pull (contract v18): the one venue a reported
    /// venue's public codes name -- a MIC under `iso10383`, or a vendor's
    /// code as a `symbol` with its source -- or none where the platform holds
    /// none. Never the whole list: one venue at a time, on demand.
    pub async fn pull_venue(
        &self,
        identifier: &PbIdentifier,
        as_of_ns: i64,
        now_ns: i64,
    ) -> Result<Vec<VenueRecord>, PlatformError> {
        let path = resolve_path(identifier, as_of_ns);
        let response = self.call(Method::Get, &path, None, now_ns).await?;
        if response.status == 404 {
            return Ok(Vec::new());
        }
        let reply: WireReply = read(&response)?;
        if !reply.found.unwrap_or(true) {
            return Ok(Vec::new());
        }
        reply.venues.into_iter().map(into_venue).collect()
    }

    /// W3.16 (contract v22): calendars and date rules, by the one subject
    /// the query names -- a calendar's ID, a currency's cash instrument, a
    /// venue, a pair or a market -- and, on a refresh, only versions after
    /// the highest held. Never the whole set. The platform's answer as JSON,
    /// for [`crate::calendars`] to read; none where it holds nothing (404).
    pub async fn pull_calendars(
        &self,
        query: &str,
        now_ns: i64,
    ) -> Result<Option<serde_json::Value>, PlatformError> {
        let path = format!("/api/v1/reference/calendars?{query}");
        let response = self.call(Method::Get, &path, None, now_ns).await?;
        if response.status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&response.status) {
            return Err(PlatformError::Refused {
                status: response.status,
            });
        }
        serde_json::from_slice(&response.body)
            .map(Some)
            .map_err(|failed| PlatformError::Malformed(failed.to_string()))
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

    /// W3.3, by an identifier the master might know it under, with its
    /// source where it has one: an administrator's symbol is found only by
    /// its administrator (ruling 545).
    pub async fn pull_identifier(
        &self,
        identifier: &PbIdentifier,
        as_of_ns: i64,
        now_ns: i64,
    ) -> Result<Option<PbInstrument>, PlatformError> {
        let path = resolve_path(identifier, as_of_ns);
        let response = self.call(Method::Get, &path, None, now_ns).await?;
        found_or_nothing(response)
    }

    /// W3.4, from contract v24 (decisions/037; the one-ID spec's requirement
    /// 7): the store's matching and minting request, carried as it came --
    /// what to look for, the template and its version, what the record's
    /// sources state, offered, and a request key -- and nothing else. It never
    /// names the deployment or its `LCL-` record: the deployment is the signed
    /// note's, which says who may ask, and the pairing stays here (W3.8).
    ///
    /// The platform's answer whole: a match, a mint from its own source, a
    /// near-match minted and flagged, or the request queued for its staff
    /// (`pending_review`). A refusal or an unreadable answer is the caller's
    /// to absorb: the store keeps a provisional record and asks again.
    pub async fn escalate(
        &self,
        request: &EscalateInstrumentRequest,
        now_ns: i64,
    ) -> Result<EscalateInstrumentReply, PlatformError> {
        self.escalate_with_venues(request, now_ns)
            .await
            .map(|(reply, _)| reply)
    }

    /// [`Platform::escalate`], with the venue master's records the answer
    /// names -- its record's listing venue -- as W3.3's answer carries them
    /// (contract v18), for the instrument store to keep.
    pub async fn escalate_with_venues(
        &self,
        request: &EscalateInstrumentRequest,
        now_ns: i64,
    ) -> Result<(EscalateInstrumentReply, Vec<VenueRecord>), PlatformError> {
        let body = serde_json::to_vec(&escalation_body(request))
            .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;
        let response = self
            .v24_call(
                Method::Post,
                "/api/v1/reference/escalate-instrument",
                Some(body),
                now_ns,
            )
            .await?;
        let reply: WireReply = read(&response)?;
        let venues = reply
            .venues
            .into_iter()
            .map(into_venue)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            EscalateInstrumentReply {
                instrument: reply.instrument.map(into_record).transpose()?,
                minted: reply.minted.unwrap_or(false),
                conflict: reply.conflict.unwrap_or(false),
                near_match: reply.near_match.unwrap_or(false),
                pending_review: reply.pending_review.unwrap_or(false),
            },
            venues,
        ))
    }

    /// W3.29, W1.30 (contract v26): a venue's grouping of event contracts,
    /// its members by their `INS-` IDs, carried as it came; the platform's
    /// answer whole -- its `EVG-` group at its version, or queued for its
    /// staff. A platform without the route answers as unreachable.
    pub async fn escalate_event_group(
        &self,
        request: &meridian_domain::v1::EscalateEventGroupRequest,
        now_ns: i64,
    ) -> Result<meridian_domain::v1::EscalateEventGroupReply, PlatformError> {
        let body = serde_json::to_vec(&event_group_body(request))
            .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;
        let response = self
            .v24_call(
                Method::Post,
                "/api/v1/reference/escalate-event-group",
                Some(body),
                now_ns,
            )
            .await?;
        let value: serde_json::Value = read_json(&response)?;
        event_group_reply_from_json(&value).map_err(PlatformError::Malformed)
    }

    /// A route contract v24 added (W3.4, W3.24, W3.25, W3.27, W3.28). A
    /// platform without it answers 404 -- or 405, 410 or 501 -- and is
    /// treated as unreachable, never as an answer (the build plan's step 4):
    /// the store keeps a provisional record, nothing is sent, and the ask is
    /// tried again on reconnecting.
    async fn v24_call(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        now_ns: i64,
    ) -> Result<Response, PlatformError> {
        let response = self.call(method, path, body, now_ns).await?;
        if matches!(response.status, 404 | 405 | 410 | 501) {
            return Err(PlatformError::Unreachable {
                attempts: 1,
                last: format!(
                    "the platform does not serve {path} (HTTP {}): a release before contract v24",
                    response.status
                ),
            });
        }
        Ok(response)
    }

    /// W3.24 (contract v24): the template catalogue. `None` asks for the
    /// whole of it, with no subject and no `after_version`; `Some` asks one
    /// template's versions after the highest held, beside its subject by
    /// the contract's name `template_id`, as W3.16's calendar pull is (ruled
    /// 2026-10-14): a version belongs to its template, so an `after_version`
    /// is never sent alone. Nothing of the deployment is sent.
    pub async fn pull_templates(
        &self,
        after: Option<(&str, i64)>,
        now_ns: i64,
    ) -> Result<Vec<SecurityTemplate>, PlatformError> {
        let path = templates_path(after);
        let response = self.v24_call(Method::Get, &path, None, now_ns).await?;
        let value = json_of(&response)?;
        templates_from_json(&value).map_err(PlatformError::Malformed)
    }

    /// PullCatalogue (W8.10, contract v25): listings changed after the
    /// sequence held, and the signing keys' status, as the platform wrote
    /// them, for the config domain to read and verify.
    pub async fn pull_registry_catalogue(
        &self,
        after_sequence: u64,
        now_ns: i64,
    ) -> Result<serde_json::Value, String> {
        let path = format!("/api/v1/registry/catalogue?after_sequence={after_sequence}");
        let response = self
            .v24_call(Method::Get, &path, None, now_ns)
            .await
            .map_err(|failed| failed.to_string())?;
        registry_json(&response)
    }

    /// DownloadListedVersion (W8.10, contract v25): one resolution request,
    /// signed as the deployment and counted by the platform as an install.
    pub async fn resolve_listed_version(
        &self,
        listing: &str,
        version: &str,
        registry: &str,
        now_ns: i64,
    ) -> Result<serde_json::Value, String> {
        let (organisation, plugin) = listing
            .split_once('/')
            .ok_or_else(|| format!("{listing:?} is not organisation/plugin"))?;
        let path =
            format!("/api/v1/registry/listings/{organisation}/{plugin}/versions/{version}/resolve");
        let body = serde_json::to_vec(&serde_json::json!({
            "listing": listing, "version": version, "registry": registry,
        }))
        .map_err(|failed| failed.to_string())?;
        let response = self
            .v24_call(Method::Post, &path, Some(body), now_ns)
            .await
            .map_err(|failed| failed.to_string())?;
        registry_json(&response)
    }

    /// ReportPluginUsage (W5.19, contract v25): the week's counts per
    /// installed registry version, answered with whether the platform
    /// received them.
    pub async fn report_plugin_usage(
        &self,
        report: &meridian_domain::v1::PluginUsageReport,
        now_ns: i64,
    ) -> Result<bool, String> {
        let body = serde_json::to_vec(&serde_json::json!({
            "period_start": report.period_start,
            "period_end": report.period_end,
            "counts": report.counts.iter().map(|count| serde_json::json!({
                "listing": count.listing,
                "version": count.version,
                "registry": count.registry,
                "instances": count.instances,
                "people_read": count.people_read,
                "people_write": count.people_write,
                "people_admin": count.people_admin,
                "daily_operations": count.daily_operations,
                "daily_requests": count.daily_requests,
            })).collect::<Vec<_>>(),
        }))
        .map_err(|failed| failed.to_string())?;
        let response = self
            .v24_call(
                Method::Post,
                "/api/v1/registry/usage-reports",
                Some(body),
                now_ns,
            )
            .await
            .map_err(|failed| failed.to_string())?;
        registry_json(&response).map(|_| true)
    }

    /// W3.25 (contract v24): the licence list -- the schemes whose
    /// identifiers a deployment may send, and the pace it sends them at.
    pub async fn pull_licence_list(&self, now_ns: i64) -> Result<LicenceList, PlatformError> {
        let response = self
            .v24_call(Method::Get, "/api/v1/reference/licence-list", None, now_ns)
            .await?;
        let value = json_of(&response)?;
        licence_from_json(&value).map_err(PlatformError::Malformed)
    }

    /// W3.27 (contract v24): the platform's replacements and decommissions
    /// after a sequence, the same for every deployment, at most a thousand.
    pub async fn pull_replacements(
        &self,
        after_sequence: i64,
        now_ns: i64,
    ) -> Result<Vec<ReplacementFeedEntry>, PlatformError> {
        let path = format!("/api/v1/reference/replacements?after_sequence={after_sequence}");
        let response = self.v24_call(Method::Get, &path, None, now_ns).await?;
        let value = json_of(&response)?;
        replacements_from_json(&value).map_err(PlatformError::Malformed)
    }

    /// W3.28 (contract v24): the deployment's usage statistics, counts per
    /// scheme only -- never a value, a holding, a quantity or an account --
    /// answered with whether the platform received them.
    pub async fn report_identifier_usage(
        &self,
        report: &meridian_domain::v1::IdentifierUsageReport,
        now_ns: i64,
    ) -> Result<bool, PlatformError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "period_start": report.period_start,
            "period_end": report.period_end,
            "licensed_schemes": report.licensed_schemes,
            "scheme_counts": report
                .scheme_counts
                .iter()
                .map(|count| serde_json::json!({
                    "scheme": count.scheme,
                    "instruments_carrying": count.instruments_carrying,
                    "instruments_only_by": count.instruments_only_by,
                    "misses_only_by": count.misses_only_by,
                }))
                .collect::<Vec<_>>(),
        }))
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;
        let response = self
            .v24_call(
                Method::Post,
                "/api/v1/reference/usage-reports",
                Some(body),
                now_ns,
            )
            .await?;
        let value = json_of(&response)?;
        Ok(value
            .get("received")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }

    /// W5.19. What this deployment is running, as its components describe
    /// themselves.
    ///
    /// About the software and never about the business: this carries versions,
    /// schema versions and health, and there is deliberately no shape here for
    /// anything a deployment processed or for whom. Content leaves a
    /// deployment only in a bundle its administrator sends.
    ///
    /// A failure is the caller's to shrug at. A platform that cannot be told
    /// what we run changes nothing about our ability to run it, which is the
    /// same reasoning that lets the instrument store serve through an outage.
    pub async fn report_components(
        &self,
        components: &[ComponentReport],
        now_ns: i64,
    ) -> Result<(), PlatformError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "deployment_id": self.config.deployment_id,
            "components": components
                .iter()
                .map(|component| serde_json::json!({
                    "component": component.component,
                    "version": component.version,
                    "schema_version": component.schema_version,
                    "health": component.health,
                    "detail": component.detail,
                    "started_at_ns": component.started_at_ns,
                }))
                .collect::<Vec<_>>(),
        }))
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;

        self.call(
            Method::Post,
            "/api/v1/reference/deployments/components",
            Some(body),
            now_ns,
        )
        .await
        .map(|_| ())
    }

    /// W5.24. Register the key this deployment generated, with a code.
    ///
    /// The one call here that no registered key vouches for, because the
    /// deployment has none yet: it is signed by the key it enrols, which
    /// proves the sender holds the private half, and the code proves they are
    /// entitled to register it. The platform checks both together.
    ///
    /// Nobody handles either half of the key. That is the whole reason this
    /// exists: the alternative was an operator copying a public key out of a
    /// pod's log into a form on another machine.
    pub async fn enrol_key(
        &self,
        code: &str,
        public_key_pem: &str,
        now_ns: i64,
    ) -> Result<Enrolment, PlatformError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "deployment_id": self.config.deployment_id,
            "enrolment_code": code,
            "public_key_pem": public_key_pem,
        }))
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;

        let response = self
            .call(
                Method::Post,
                "/api/v1/reference/deployments/keys/enrol",
                Some(body),
                now_ns,
            )
            .await?;
        let record: WireEnrolment = read_json(&response)?;
        Ok(Enrolment {
            key_id: record.key_id,
            fingerprint: record.fingerprint,
        })
    }

    /// W5.22. Present a claim code a deployment admin is redeeming.
    ///
    /// The body is the code and nothing else. The note this call is signed
    /// with already says which deployment is asking, and nothing here says who
    /// is redeeming: the platform learns that a code was used, never by whom.
    pub async fn honour_claim_code(
        &self,
        code: &str,
        purpose: i32,
        now_ns: i64,
    ) -> Result<RedeemClaimCodeReply, PlatformError> {
        let body = serde_json::to_vec(&serde_json::json!({
            "code": code,
            "purpose": ClaimCodePurpose::try_from(purpose)
                .unwrap_or(ClaimCodePurpose::Unspecified)
                .as_str_name(),
        }))
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;
        let response = self
            .call(
                Method::Post,
                "/api/v1/reference/deployments/claim-codes/redeem",
                Some(body),
                now_ns,
            )
            .await?;
        let reply: WireRedemption = read_json(&response)?;
        Ok(RedeemClaimCodeReply {
            redeemed: reply.redeemed,
            refusal_reason: reply.refusal_reason.unwrap_or_default(),
            // Only a first-run redemption brings one back, and only once.
            first_admin_code: reply.first_admin_code.unwrap_or_default(),
            first_admin_code_expires_at_ns: reply
                .first_admin_code_expires_at_ns
                .unwrap_or_default(),
        })
    }

    /// W5.23. Send a diagnostic bundle, exactly as its deployment admin
    /// approved it.
    ///
    /// Nothing is added: not who sent it, and not the deployment's identity,
    /// which the signed note already carries and the platform records itself.
    /// Content is base64, as proto's JSON form writes bytes.
    pub async fn submit_diagnostic_bundle(
        &self,
        bundle: &DiagnosticBundle,
        now_ns: i64,
    ) -> Result<DiagnosticBundleReceipt, PlatformError> {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;

        let body = serde_json::to_vec(&serde_json::json!({
            "bundle_id": bundle.bundle_id,
            "assembled_at_ns": bundle.assembled_at_ns,
            "items": bundle
                .items
                .iter()
                .map(|item| serde_json::json!({
                    "name": item.name,
                    "content_type": item.content_type,
                    "content": STANDARD.encode(&item.content),
                }))
                .collect::<Vec<_>>(),
        }))
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))?;
        let response = self
            .call(
                Method::Post,
                "/api/v1/reference/deployments/diagnostic-bundles",
                Some(body),
                now_ns,
            )
            .await?;
        let receipt: WireReceipt = read_json(&response)?;
        Ok(DiagnosticBundleReceipt {
            bundle_id: receipt.bundle_id,
            received_at_ns: receipt.received_at_ns,
            retain_until_ns: receipt.retain_until_ns,
        })
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
}

/// A 2xx answer's JSON.
/// A registry row's answer, or its refusal in the platform's own words.
fn registry_json(response: &Response) -> Result<serde_json::Value, String> {
    let value: Option<serde_json::Value> = serde_json::from_slice(&response.body).ok();
    if !(200..300).contains(&response.status) {
        let said = value
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no reason given");
        return Err(format!(
            "the platform refused it (HTTP {}): {said}",
            response.status
        ));
    }
    value.ok_or_else(|| "the platform's answer is not JSON".to_string())
}

/// The catalogue's path: the whole of it, or one template's versions after
/// the highest held, its subject named beside `after_version`.
pub(crate) fn templates_path(after: Option<(&str, i64)>) -> String {
    match after {
        Some((template_id, after_version)) if !template_id.trim().is_empty() => format!(
            "/api/v1/reference/templates?template_id={}&after_version={}",
            encode(template_id.trim()),
            after_version.max(0)
        ),
        _ => "/api/v1/reference/templates".to_string(),
    }
}

fn json_of(response: &Response) -> Result<serde_json::Value, PlatformError> {
    if !(200..300).contains(&response.status) {
        return Err(PlatformError::Refused {
            status: response.status,
        });
    }
    serde_json::from_slice(&response.body)
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))
}

/// A JSON number or a string holding one: the platform writes int64 as a
/// string (proto3 JSON).
fn int_of(value: Option<&serde_json::Value>) -> i64 {
    match value {
        Some(serde_json::Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(serde_json::Value::String(text)) => text.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn text_of(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// The catalogue as the platform's JSON spells it (W3.24): each template's
/// class and type by the ruled spelling, an unknown one refused.
pub fn templates_from_json(value: &serde_json::Value) -> Result<Vec<SecurityTemplate>, String> {
    let mut out = Vec::new();
    for item in value
        .get("templates")
        .and_then(|templates| templates.as_array())
        .ok_or("the answer carries no templates")?
    {
        let template_id = text_of(item.get("template_id"));
        if !template_id.starts_with("TPL-") {
            return Err(format!(
                "a template arrived as {template_id:?}, not the platform's"
            ));
        }
        let class = text_of(item.get("asset_class"));
        let asset_class = asset_class::read(&class).ok_or_else(|| {
            format!("{template_id} names asset class {class:?}, which the schema does not define")
        })?;
        let kind = text_of(item.get("instrument_type"));
        let instrument_type = meridian_domain::instrument_type::read(&kind).ok_or_else(|| {
            format!("{template_id} names type {kind:?}, which the schema does not define")
        })?;
        // Its calculation package (contract v26, W1.24): data, refused with
        // the version where it does not stand, never kept half.
        let calculation_package =
            meridian_domain::template::package_from_json(item.get("calculation_package"))
                .map_err(|said| format!("{template_id} {said}"))?;
        out.push(SecurityTemplate {
            calculation_package,
            template_version: text_of(item.get("template_version")),
            name: text_of(item.get("name")),
            asset_class: asset_class as i32,
            instrument_type: instrument_type as i32,
            base: item
                .get("base")
                .and_then(|base| base.as_bool())
                .unwrap_or(false),
            calculation: text_of(item.get("calculation")),
            fields: item
                .get("fields")
                .and_then(|fields| fields.as_array())
                .map(|fields| {
                    fields
                        .iter()
                        .map(|field| {
                            let required = field
                                .get("required")
                                .and_then(|r| r.as_bool())
                                .unwrap_or(false);
                            // Contract v28 (574): the requirement level by its
                            // word or enum name; a version before it reads
                            // required as mint and its absence as optional.
                            let requirement = meridian_domain::template::requirement_of(
                                &text_of(field.get("requirement")),
                                required,
                            );
                            meridian_domain::v1::TemplateField {
                                path: text_of(field.get("path")),
                                r#type: text_of(field.get("type")),
                                required: required || requirement == FieldRequirement::Mint,
                                requirement: requirement as i32,
                                repeated: field
                                    .get("repeated")
                                    .and_then(|r| r.as_bool())
                                    .unwrap_or(false),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            match_rule: item
                .get("match_rule")
                .and_then(|rule| rule.as_array())
                .map(|rule| rule.iter().map(|scheme| text_of(Some(scheme))).collect())
                .unwrap_or_default(),
            // Contract v28 (579): a withdrawn version, readable and
            // runnable, names the version new mints use.
            withdrawn: item
                .get("withdrawn")
                .and_then(|w| w.as_bool())
                .unwrap_or(false),
            successor_version: text_of(item.get("successor_version")),
            template_id,
        });
    }
    Ok(out)
}

/// The licence list as the platform's JSON spells it (W3.25). A list naming
/// the deployment-only scheme is refused: it never leaves a deployment.
pub fn licence_from_json(value: &serde_json::Value) -> Result<LicenceList, String> {
    let schemes: Vec<String> = value
        .get("schemes")
        .and_then(|schemes| schemes.as_array())
        .ok_or("the answer carries no schemes")?
        .iter()
        .map(|scheme| text_of(Some(scheme)))
        .filter(|scheme| !scheme.is_empty())
        .collect();
    if schemes.iter().any(|scheme| scheme == "open_meridian_local") {
        return Err(
            "a licence list naming open_meridian_local, which never leaves a deployment".into(),
        );
    }
    let pace = value.get("pace");
    // The administrators the administrator_symbol family admits (ruling
    // 545); absent from a list before it, so none.
    let administrators = value
        .get("administrators")
        .and_then(|administrators| administrators.as_array())
        .map(|administrators| {
            administrators
                .iter()
                .map(|administrator| text_of(Some(administrator)))
                .filter(|administrator| !administrator.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok(LicenceList {
        version: int_of(value.get("version")),
        schemes,
        pace: Some(LicencePace {
            batch_size: (int_of(pace.and_then(|pace| pace.get("batch_size"))) as i32).clamp(0, 100),
            batches_per_minute: int_of(pace.and_then(|pace| pace.get("batches_per_minute"))) as i32,
        }),
        administrators,
    })
}

/// The replacement feed as the platform's JSON spells it (W3.27).
pub fn replacements_from_json(
    value: &serde_json::Value,
) -> Result<Vec<ReplacementFeedEntry>, String> {
    value
        .get("entries")
        .and_then(|entries| entries.as_array())
        .ok_or_else(|| "the answer carries no entries".to_string())?
        .iter()
        .map(|entry| {
            let instrument_id = text_of(entry.get("instrument_id"));
            if !instrument_id.starts_with("INS-") {
                return Err(format!(
                    "a replacement names {instrument_id:?}, not a platform record"
                ));
            }
            Ok(ReplacementFeedEntry {
                sequence: int_of(entry.get("sequence")),
                instrument_id,
                replaced_by: text_of(entry.get("replaced_by")),
                decommissioned: entry
                    .get("decommissioned")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false),
                // Contract v28 (579): a series past its last lifecycle date.
                expired: entry
                    .get("expired")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// EscalateInstrument's request as the platform's JSON spells it (W3.4,
/// contract v24): the asset class in lower case without its prefix (ruled
/// 2026-10-14), each offered field by its path with its source's words.
/// Never `requesting_deployment_id` or `placeholder_instrument_id`, retired
/// with v9's shape: the platform refuses either.
pub fn escalation_body(request: &EscalateInstrumentRequest) -> serde_json::Value {
    serde_json::json!({
        "source": request.source,
        "asset_class": asset_class::lower(
            AssetClass::try_from(request.asset_class).unwrap_or(AssetClass::Unspecified)
        ),
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
        "template_id": request.template_id,
        "template_version": request.template_version,
        "fields": request
            .fields
            .iter()
            .map(|field| serde_json::json!({
                "path": field.path,
                "value": field.value,
                "source": field
                    .provenance
                    .as_ref()
                    .map(|provenance| provenance.source.clone())
                    .unwrap_or_default(),
            }))
            .collect::<Vec<_>>(),
        "request_key": request.request_key,
    })
}

/// W3.29's request as the platform's JSON spells it: the flags by their
/// enum names, int64 as text.
pub fn event_group_body(
    request: &meridian_domain::v1::EscalateEventGroupRequest,
) -> serde_json::Value {
    // Unknown is never false (A2): unset travels as not stated.
    let flag = |value: i32| {
        use meridian_domain::v1::EventGroupFlag;
        match EventGroupFlag::try_from(value) {
            Ok(EventGroupFlag::True) => EventGroupFlag::True.as_str_name(),
            Ok(EventGroupFlag::False) => EventGroupFlag::False.as_str_name(),
            _ => EventGroupFlag::NotStated.as_str_name(),
        }
    };
    serde_json::json!({
        "venue_id": request.venue_id,
        "venue_event_ref": request.venue_event_ref,
        "title": request.title,
        "member_instrument_ids": request.member_instrument_ids,
        "mutually_exclusive": flag(request.mutually_exclusive),
        "exhaustive": flag(request.exhaustive),
        "series_id": request.series_id,
        "valid_from_ns": request.valid_from_ns.to_string(),
        "request_key": request.request_key,
    })
}

/// The platform's answer to a group (W1.30): its `EVG-` group, read
/// strictly where it names what is kept -- the ID, the venue, the members,
/// the flags -- and refused where it does not.
pub fn event_group_reply_from_json(
    value: &serde_json::Value,
) -> Result<meridian_domain::v1::EscalateEventGroupReply, String> {
    use meridian_domain::v1::{EscalateEventGroupReply, EventGroup, EventGroupFlag};
    let flag = |held: Option<&serde_json::Value>| -> Result<i32, String> {
        match held {
            None | Some(serde_json::Value::Null) => Ok(EventGroupFlag::NotStated as i32),
            Some(serde_json::Value::String(name)) => EventGroupFlag::from_str_name(name)
                .map(|flag| flag as i32)
                .ok_or_else(|| format!("a flag {name:?} the schema does not define")),
            Some(serde_json::Value::Number(n)) => n
                .as_i64()
                .and_then(|n| EventGroupFlag::try_from(n as i32).ok())
                .map(|flag| flag as i32)
                .ok_or_else(|| format!("a flag {n} the schema does not define")),
            Some(other) => Err(format!("a flag {other} the schema does not define")),
        }
    };
    let event_group = match value.get("event_group").filter(|held| held.is_object()) {
        None => None,
        Some(group) => {
            let event_group_id = text_of(group.get("event_group_id"));
            if !event_group_id.starts_with("EVG-") {
                return Err(format!(
                    "a group arrived as {event_group_id:?}, not the platform's"
                ));
            }
            Some(EventGroup {
                event_group_id,
                venue_id: text_of(group.get("venue_id")),
                venue_event_ref: text_of(group.get("venue_event_ref")),
                title: text_of(group.get("title")),
                member_instrument_ids: group
                    .get("member_instrument_ids")
                    .and_then(|members| members.as_array())
                    .map(|members| members.iter().map(|m| text_of(Some(m))).collect())
                    .unwrap_or_default(),
                mutually_exclusive: flag(group.get("mutually_exclusive"))?,
                exhaustive: flag(group.get("exhaustive"))?,
                series_id: text_of(group.get("series_id")),
                version: int_of(group.get("version")).max(1),
                valid_from_ns: int_of(group.get("valid_from_ns")),
                record_time_ns: 0,
                provisional: false,
                sequence: 0,
            })
        }
    };
    Ok(EscalateEventGroupReply {
        event_group,
        minted: value
            .get("minted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        pending_review: value
            .get("pending_review")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

/// A reply from the platform, in the shape of the contract's messages.
///
/// Unknown fields are ignored, matching protobuf. What is not ignored is a
/// field that arrived meaning something else: see [`into_record`].
#[derive(Debug, Deserialize)]
struct WireReply {
    found: Option<bool>,
    minted: Option<bool>,
    conflict: Option<bool>,
    /// Contract v24 (W1.25, W3.4): minted and flagged for review.
    #[serde(default)]
    near_match: Option<bool>,
    /// Contract v24 (W1.25, W3.4): queued for the platform's staff.
    #[serde(default)]
    pending_review: Option<bool>,
    instrument: Option<WireRecord>,
    /// The venues the answer names (contract v18).
    #[serde(default)]
    venues: Vec<WireVenue>,
}

/// A venue master's record, as the platform's answer carries it (W3.5).
#[derive(Debug, Deserialize)]
struct WireVenue {
    #[serde(default)]
    venue_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    country_code: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    identifiers: Vec<WireIdentifier>,
    #[serde(default)]
    operating_venue_id: String,
    #[serde(default)]
    time_zone: String,
    #[serde(default)]
    version: i64,
    #[serde(default)]
    valid_from_ns: i64,
    #[serde(default)]
    record_time_ns: i64,
    #[serde(default)]
    lifecycle_state: String,
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
    listing_venue_id: String,
    #[serde(default)]
    description: String,
    /// Its type, spelled as the asset class is (contract v19); empty for none.
    #[serde(default)]
    instrument_type: String,
    /// A binary event contract's payout and close (contract v19); null for
    /// a record that is not one.
    #[serde(default)]
    binary_event_contract: Option<WireContract>,
    #[serde(default)]
    lifecycle_state: String,
    #[serde(default)]
    version: i64,
    #[serde(default)]
    valid_from_ns: i64,
    #[serde(default)]
    record_time_ns: i64,
    /// Contract v24 (W1.25, W3.4): the template it was minted from.
    #[serde(default)]
    template_id: String,
    #[serde(default)]
    template_version: String,
}

/// meridian.v1.BinaryEventContract as the platform's answer carries it.
#[derive(Debug, Deserialize)]
struct WireContract {
    #[serde(default)]
    payout: Option<WireMoney>,
    #[serde(default)]
    closes_at_ns: i64,
    /// A range market's bounds and their unit (contract v26, W1.24): the
    /// template's next version's fields; absent for a contract with none.
    #[serde(default)]
    floor: Option<WireBound>,
    #[serde(default)]
    cap: Option<WireBound>,
    #[serde(default)]
    bound_unit: String,
}

/// One bound of a range market as the platform's answer carries it: the
/// value as text, so nothing passes through a float.
#[derive(Debug, Deserialize)]
struct WireBound {
    #[serde(default)]
    value: String,
    #[serde(default)]
    inclusive: bool,
}

/// A Money as the platform's answer carries it: the amount as text, so
/// nothing passes through a float.
#[derive(Debug, Deserialize)]
struct WireMoney {
    #[serde(default)]
    amount: String,
    #[serde(default)]
    currency_code: String,
    #[serde(default)]
    instrument_id: String,
}

#[derive(Debug, Deserialize)]
struct WireIdentifier {
    #[serde(default)]
    scheme: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    source: String,
    /// Its window on the record (contract v19, the plan's Q34): from,
    /// inclusive, and until, exclusive, 0 while it is on the record. A
    /// deployment's pull carries the identifiers the master ended, closed.
    #[serde(default)]
    valid_from_ns: i64,
    #[serde(default)]
    valid_until_ns: i64,
    /// A listing identifier's trading currency, ISO 4217, as the master
    /// holds it per listing (contract v25, W3.1); absent on every other.
    #[serde(default)]
    trading_currency: String,
}

impl WireIdentifier {
    fn into_wire(self) -> PbIdentifier {
        PbIdentifier {
            scheme: self.scheme,
            value: self.value,
            source: self.source,
            valid_from_ns: self.valid_from_ns,
            valid_until_ns: self.valid_until_ns,
            trading_currency: self.trading_currency,
        }
    }
}

/// The reply's record and the venues it names, or nothing when the platform
/// said it had none.
fn found_with_venues(
    response: Response,
) -> Result<Option<(PbInstrument, Vec<VenueRecord>)>, PlatformError> {
    if response.status == 404 {
        return Ok(None);
    }
    let reply: WireReply = read(&response)?;
    if !reply.found.unwrap_or(true) {
        return Ok(None);
    }
    let Some(record) = reply.instrument else {
        return Ok(None);
    };
    let venues = reply
        .venues
        .into_iter()
        .map(into_venue)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some((into_record(record)?, venues)))
}

/// A venue as the instrument store keeps it, its kind and lifecycle checked
/// as an instrument's state is: a name the schema does not define is refused,
/// never read as unspecified.
fn into_venue(venue: WireVenue) -> Result<VenueRecord, PlatformError> {
    if !venue.venue_id.starts_with("VEN-") {
        return Err(PlatformError::Malformed(format!(
            "a venue arrived with ID {:?}, not the venue master's",
            venue.venue_id
        )));
    }
    let kind = meridian_domain::v1::VenueKind::from_str_name(&venue.kind).ok_or_else(|| {
        PlatformError::Malformed(format!(
            "{} arrived as kind {:?}, which the schema does not define",
            venue.venue_id, venue.kind
        ))
    })? as i32;
    let lifecycle_state =
        meridian_domain::v1::InstrumentLifecycleState::from_str_name(&venue.lifecycle_state)
            .ok_or_else(|| {
                PlatformError::Malformed(format!(
                    "{} arrived in lifecycle state {:?}, which the schema does not define",
                    venue.venue_id, venue.lifecycle_state
                ))
            })? as i32;
    Ok(VenueRecord {
        venue_id: venue.venue_id,
        name: venue.name,
        country_code: venue.country_code,
        kind,
        identifiers: venue
            .identifiers
            .into_iter()
            .map(WireIdentifier::into_wire)
            .collect(),
        operating_venue_id: venue.operating_venue_id,
        time_zone: venue.time_zone,
        version: venue.version,
        valid_from_ns: venue.valid_from_ns,
        record_time_ns: venue.record_time_ns,
        lifecycle_state,
    })
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

/// What the platform says it registered, which the wizard shows so an
/// administrator can compare it with the fingerprint on the platform's own
/// page. A code spent by somebody else leaves one they do not recognise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolment {
    pub key_id: String,
    pub fingerprint: String,
}

#[derive(Deserialize)]
struct WireEnrolment {
    key_id: String,
    fingerprint: String,
}

#[derive(Deserialize)]
struct WireRedemption {
    redeemed: bool,
    refusal_reason: Option<String>,
    first_admin_code: Option<String>,
    first_admin_code_expires_at_ns: Option<i64>,
}

#[derive(Deserialize)]
struct WireReceipt {
    bundle_id: String,
    received_at_ns: i64,
    retain_until_ns: i64,
}

fn read_json<T: serde::de::DeserializeOwned>(response: &Response) -> Result<T, PlatformError> {
    if !(200..300).contains(&response.status) {
        return Err(PlatformError::Refused {
            status: response.status,
        });
    }
    serde_json::from_slice(&response.body)
        .map_err(|failed| PlatformError::Malformed(failed.to_string()))
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

    // The enum's name, or empty for a stub with none. What the free-text field
    // held before the enum is read too, while a platform that has not yet
    // migrated its master still sends it (sdk-contract/asset-class-is-an-enum);
    // a class it did not plainly mean is refused, as an undefined state is.
    let asset_class = asset_class::legacy(&record.asset_class).ok_or_else(|| {
        PlatformError::Malformed(format!(
            "{} arrived with asset class {:?}, which the schema does not define",
            record.instrument_id, record.asset_class
        ))
    })? as i32;

    let lifecycle_state =
        meridian_domain::v1::InstrumentLifecycleState::from_str_name(&record.lifecycle_state)
            .ok_or_else(|| {
                PlatformError::Malformed(format!(
                    "{} arrived in lifecycle state {:?}, which the schema does not define",
                    record.instrument_id, record.lifecycle_state
                ))
            })? as i32;

    // Its type (contract v19): the enum's name or the ruled spelling; one the
    // schema does not define is refused, as an undefined class is.
    let instrument_type = meridian_domain::instrument_type::read(&record.instrument_type)
        .ok_or_else(|| {
            PlatformError::Malformed(format!(
                "{} arrived with instrument type {:?}, which the schema does not define",
                record.instrument_id, record.instrument_type
            ))
        })? as i32;
    let binary_event_contract = match record.binary_event_contract {
        None => None,
        Some(contract) => {
            let payout = match contract.payout {
                None => None,
                Some(money) => Some(meridian_domain::v1::Money {
                    amount: if money.amount.trim().is_empty() {
                        None
                    } else {
                        Some(
                            money
                                .amount
                                .trim()
                                .parse::<meridian_domain::exact::Exact>()
                                .map_err(|_| {
                                    PlatformError::Malformed(format!(
                                        "{} arrived with a payout of {:?}, which is no decimal",
                                        record.instrument_id, money.amount
                                    ))
                                })?
                                .to_wire(),
                        )
                    },
                    currency_code: money.currency_code,
                    instrument_id: money.instrument_id,
                }),
            };
            let bound = |held: Option<WireBound>, which: &str| {
                held.map(|held| {
                    held.value
                        .trim()
                        .parse::<meridian_domain::exact::Exact>()
                        .map(|value| meridian_domain::v1::EventContractBound {
                            value: Some(value.to_wire()),
                            inclusive: held.inclusive,
                        })
                        .map_err(|_| {
                            PlatformError::Malformed(format!(
                                "{} arrived with a {which} of {:?}, which is no decimal",
                                record.instrument_id, held.value
                            ))
                        })
                })
                .transpose()
            };
            Some(meridian_domain::v1::BinaryEventContract {
                payout,
                closes_at_ns: contract.closes_at_ns,
                floor: bound(contract.floor, "floor")?,
                cap: bound(contract.cap, "cap")?,
                bound_unit: contract.bound_unit,
            })
        }
    };

    Ok(PbInstrument {
        instrument_id: record.instrument_id,
        identifiers: record
            .identifiers
            .into_iter()
            .map(WireIdentifier::into_wire)
            .collect(),
        instrument_type,
        binary_event_contract,
        asset_class,
        currency: record.currency,
        exchange_mic: record.exchange_mic,
        listing_venue_id: record.listing_venue_id,
        description: record.description,
        lifecycle_state,
        version: record.version,
        valid_from_ns: record.valid_from_ns,
        record_time_ns: record.record_time_ns,
        template_id: record.template_id,
        template_version: record.template_version,
        ..Default::default()
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
pub(crate) fn encode(value: &str) -> String {
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
            .user_agent(concat!("meridian-instrument/", env!("CARGO_PKG_VERSION")))
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
    use std::sync::Mutex;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use base64::Engine as _;

    use meridian_domain::v1::BundleItem;

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

        /// Each request's body, as text.
        pub(crate) fn bodies(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|request| {
                    String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).to_string()
                })
                .collect()
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
                "asset_class": "ASSET_CLASS_EQUITY",
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

    #[tokio::test]
    async fn a_key_is_enrolled_with_the_code_that_carried_the_install() {
        let transport = Fake::new(vec![Ok(reply(
            201,
            "{\"key_id\": \"KEY-1\", \"fingerprint\": \"SHA256:abc\"}",
        ))]);
        let platform = platform(Arc::clone(&transport));

        let enrolled = platform
            .enrol_key(
                "ENR-4H7D-QW2M-8XPC",
                "-----BEGIN PUBLIC KEY-----\nA\n-----END PUBLIC KEY-----\n",
                NOW,
            )
            .await
            .unwrap();

        assert_eq!(enrolled.fingerprint, "SHA256:abc");
        assert_eq!(
            transport.urls(),
            vec![format!("{ADDRESS}/api/v1/reference/deployments/keys/enrol")]
        );
        let seen = transport.seen.lock().unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(seen[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["enrolment_code"], "ENR-4H7D-QW2M-8XPC");
        assert!(body["public_key_pem"]
            .as_str()
            .unwrap()
            .contains("PUBLIC KEY"));
        // Signed like every other call, and by the key it enrols: the platform
        // verifies the note against the key in this body and against nothing
        // it already holds, because it holds none yet.
        assert!(!seen[0].assertion.is_empty());
    }

    #[tokio::test]
    async fn a_claim_code_is_presented_with_nothing_about_the_person() {
        let transport = Fake::new(vec![Ok(reply(200, "{\"redeemed\": true}"))]);
        let platform = platform(Arc::clone(&transport));

        let answered = platform
            .honour_claim_code("7KQ2-MX4P-9RTD", ClaimCodePurpose::FirstAdmin as i32, NOW)
            .await
            .unwrap();
        assert!(answered.redeemed);
        assert_eq!(
            transport.urls(),
            vec![format!(
                "{ADDRESS}/api/v1/reference/deployments/claim-codes/redeem"
            )]
        );
        let seen = transport.seen.lock().unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(seen[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(body["code"], "7KQ2-MX4P-9RTD");
        assert_eq!(body["purpose"], "CLAIM_CODE_PURPOSE_FIRST_ADMIN");
    }

    #[tokio::test]
    async fn a_refused_claim_says_why() {
        let transport = Fake::new(vec![Ok(reply(
            200,
            "{\"redeemed\": false, \"refusal_reason\": \"already used\"}",
        ))]);
        let answered = platform(transport)
            .honour_claim_code("7KQ2-MX4P-9RTD", ClaimCodePurpose::FirstAdmin as i32, NOW)
            .await
            .unwrap();
        assert!(!answered.redeemed);
        assert_eq!(answered.refusal_reason, "already used");
    }

    #[tokio::test]
    async fn a_bundle_is_sent_as_approved_with_nothing_added() {
        let transport = Fake::new(vec![Ok(reply(
            200,
            "{\"bundle_id\": \"DB-1\", \"received_at_ns\": 5, \"retain_until_ns\": 6}",
        ))]);
        let platform = platform(Arc::clone(&transport));
        let bundle = DiagnosticBundle {
            bundle_id: "DB-1".into(),
            assembled_at_ns: 3,
            items: vec![BundleItem {
                name: "component-reports.json".into(),
                content_type: "application/json".into(),
                content: b"[]".to_vec(),
            }],
            deployment_id: String::new(),
        };

        let receipt = platform
            .submit_diagnostic_bundle(&bundle, NOW)
            .await
            .unwrap();
        assert_eq!(receipt.retain_until_ns, 6);

        let seen = transport.seen.lock().unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(seen[0].body.as_ref().unwrap()).unwrap();
        let mut keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(keys, ["assembled_at_ns", "bundle_id", "items"]);
        assert_eq!(body["items"][0]["content"], "W10=");
    }

    #[tokio::test]
    async fn a_component_report_reaches_the_contracted_path() {
        let transport = Fake::new(vec![Ok(reply(200, "{\"recorded_at_ns\": 1}"))]);
        let platform = platform(Arc::clone(&transport));

        platform
            .report_components(&[ComponentReport::serving("street", "1.4.2", 2, NOW)], NOW)
            .await
            .expect("the report should have been accepted");

        assert_eq!(
            transport.urls(),
            vec![format!("{ADDRESS}/api/v1/reference/deployments/components")]
        );
    }

    #[tokio::test]
    async fn a_report_carries_software_and_nothing_about_the_business() {
        // The boundary as a test rather than as a comment: every key in the
        // body is named here, so a field added later has to be added here too,
        // where somebody has to decide whether it is about software.
        let transport = Fake::new(vec![Ok(reply(200, "{\"recorded_at_ns\": 1}"))]);
        let platform = platform(Arc::clone(&transport));

        platform
            .report_components(
                &[ComponentReport::serving("street", "1.4.2", 2, NOW)
                    .degraded("the platform is unreachable")],
                NOW,
            )
            .await
            .unwrap();

        let seen = transport.seen.lock().unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(seen[0].body.as_ref().unwrap()).unwrap();

        let mut top: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        top.sort();
        assert_eq!(top, vec!["components", "deployment_id"]);

        let mut fields: Vec<&str> = body["components"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort();
        assert_eq!(
            fields,
            vec![
                "component",
                "detail",
                "health",
                "schema_version",
                "started_at_ns",
                "version"
            ]
        );
        assert_eq!(body["components"][0]["health"], "COMPONENT_HEALTH_DEGRADED");
    }

    pub(crate) fn platform(transport: Arc<Fake>) -> Platform {
        Platform::new(
            Config::new(ADDRESS, DEPLOYMENT),
            DeploymentKey::generate(),
            transport,
        )
    }

    fn escalation() -> EscalateInstrumentRequest {
        EscalateInstrumentRequest {
            source: "snaptrade".into(),
            asset_class: AssetClass::Equity as i32,
            identifiers: vec![
                PbIdentifier {
                    scheme: "symbol".into(),
                    value: "ZZTOP".into(),
                    source: "snaptrade".into(),
                    ..Default::default()
                },
                PbIdentifier {
                    scheme: "figi".into(),
                    value: "BBG000ZZTOP1".into(),
                    source: String::new(),
                    ..Default::default()
                },
            ],
            as_of_ns: AS_OF,
            template_id: "TPL-equity".into(),
            template_version: "1".into(),
            fields: vec![meridian_domain::v1::TemplateFieldValue {
                path: "description".into(),
                value: "ZZ Top Holdings Inc. common stock".into(),
                provenance: None,
            }],
            request_key: "01JB2N7Q0000000000000000RK".into(),
        }
    }

    fn identifier(scheme: &str, value: &str, source: &str) -> PbIdentifier {
        PbIdentifier {
            scheme: scheme.into(),
            value: value.into(),
            source: source.into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_venue_is_pulled_by_its_codes_one_at_a_time_and_an_unknown_kind_refused() {
        let venue = serde_json::json!({"found": true, "venues": [{
            "venue_id": "VEN-01JA0000000000000XNYS0", "name": "New York Stock Exchange",
            "country_code": "US", "kind": "VENUE_KIND_EXCHANGE",
            "identifiers": [{"scheme": "iso10383", "value": "XNYS", "source": ""}],
            "operating_venue_id": "", "time_zone": "America/New_York", "version": 1,
            "valid_from_ns": 1, "record_time_ns": 2,
            "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE"}]})
        .to_string();
        let transport = Fake::new(vec![
            Ok(reply(200, &venue)),
            Ok(reply(
                404,
                r#"{"found": false, "miss_reason": "MISS_REASON_NOT_FOUND"}"#,
            )),
            Ok(reply(
                200,
                &venue.replace("VENUE_KIND_EXCHANGE", "VENUE_KIND_CASINO"),
            )),
        ]);
        let platform = platform(transport.clone());
        let mic = PbIdentifier {
            scheme: "iso10383".into(),
            value: "XNYS".into(),
            source: String::new(),
            ..Default::default()
        };
        let pulled = platform.pull_venue(&mic, AS_OF, NOW).await.unwrap();
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].venue_id, "VEN-01JA0000000000000XNYS0");
        assert_eq!(pulled[0].time_zone, "America/New_York");
        let vendor = PbIdentifier {
            scheme: "symbol".into(),
            value: "coinbase".into(),
            source: "coinbase".into(),
            ..Default::default()
        };
        assert!(platform
            .pull_venue(&vendor, AS_OF, NOW)
            .await
            .unwrap()
            .is_empty());
        assert!(platform.pull_venue(&mic, AS_OF, NOW).await.is_err());
        let urls = transport.urls();
        assert!(
            urls[0].contains("identifiers/resolve?scheme=iso10383&value=XNYS"),
            "{urls:?}"
        );
        assert!(urls[1].ends_with("&source=coinbase"), "{urls:?}");
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
    async fn a_range_markets_bounds_arrive_with_its_record() {
        // Contract v26 (W1.24, A4): the floor and cap, each with whether it
        // is inclusive, and their unit; a bound that is no decimal refused.
        let body = |floor: &str| {
            serde_json::json!({
                "found": true,
                "instrument": {
                    "instrument_id": "INS-KXHIGHNY",
                    "identifiers": [{"scheme": "ticker", "value": "KXHIGHNY-26OCT18-B71.5",
                                     "source": "KLSH"}],
                    "asset_class": "ASSET_CLASS_EVENT_CONTRACT",
                    "instrument_type": "binary_event_contract",
                    "binary_event_contract": {
                        "payout": {"amount": "1", "currency_code": "USD", "instrument_id": ""},
                        "closes_at_ns": 0,
                        "floor": {"value": floor, "inclusive": true},
                        "cap": {"value": "72", "inclusive": false},
                        "bound_unit": "degrees_f"
                    },
                    "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
                    "version": 1
                }
            })
            .to_string()
        };
        let record = platform(Fake::new(vec![Ok(reply(200, &body("71")))]))
            .pull_instrument("INS-KXHIGHNY", AS_OF, NOW)
            .await
            .unwrap()
            .unwrap();
        let contract = record.binary_event_contract.unwrap();
        assert_eq!(
            meridian_domain::instrument_type::contract_to_text(&contract),
            "1|USD||0|=71|72|degrees_f"
        );
        let refused = platform(Fake::new(vec![Ok(reply(200, &body("seventy")))]))
            .pull_instrument("INS-KXHIGHNY", AS_OF, NOW)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("floor"), "{refused}");
    }

    #[tokio::test]
    async fn a_pull_carries_the_event_contract_and_each_identifiers_window() {
        // Contract v19: the type and its payout and close, and the
        // identifiers the master ended, their windows closed (the plan's Q34).
        let body = serde_json::json!({
            "found": true,
            "instrument": {
                "instrument_id": "INS-KXFED",
                "identifiers": [
                    {"scheme": "ticker", "value": "KXFED-26NOV", "source": "",
                     "valid_from_ns": 5, "valid_until_ns": 0},
                    {"scheme": "ticker", "value": "KXFED-OLD", "source": "",
                     "valid_from_ns": 1, "valid_until_ns": 5},
                    {"scheme": "figi", "value": "BBG0KXFEDCAD", "source": "",
                     "valid_from_ns": 5, "valid_until_ns": 0, "trading_currency": "CAD"}
                ],
                "asset_class": "ASSET_CLASS_EVENT_CONTRACT",
                "instrument_type": "INSTRUMENT_TYPE_BINARY_EVENT_CONTRACT",
                "binary_event_contract": {
                    "payout": {"amount": "1", "currency_code": "USD", "instrument_id": ""},
                    "closes_at_ns": 1793548800000000000_i64
                },
                "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
                "version": 2
            }
        })
        .to_string();
        let transport = Fake::new(vec![Ok(reply(200, &body))]);
        let record = platform(transport)
            .pull_instrument("INS-KXFED", AS_OF, NOW)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.instrument_type,
            meridian_domain::v1::InstrumentType::BinaryEventContract as i32
        );
        let contract = record.binary_event_contract.unwrap();
        assert_eq!(contract.closes_at_ns, 1_793_548_800_000_000_000);
        let payout = contract.payout.unwrap();
        assert_eq!(payout.currency_code, "USD");
        // A listing's trading currency (contract v25, W3.1), and none on
        // every other identifier.
        assert_eq!(record.identifiers[2].trading_currency, "CAD");
        assert!(record.identifiers[0].trading_currency.is_empty());
        assert_eq!(
            meridian_domain::exact::Exact::from_wire(&payout.amount.unwrap())
                .unwrap()
                .to_string(),
            "1"
        );
        assert_eq!(record.identifiers[1].valid_until_ns, 5, "ended, closed");
        assert_eq!(record.identifiers[0].valid_until_ns, 0);

        let wrong = body.replace(
            "INSTRUMENT_TYPE_BINARY_EVENT_CONTRACT",
            "INSTRUMENT_TYPE_ETF",
        );
        let refused = platform(Fake::new(vec![Ok(reply(200, &wrong))]))
            .pull_instrument("INS-KXFED", AS_OF, NOW)
            .await;
        assert!(refused.is_err(), "a type the schema does not define");
    }

    #[tokio::test]
    async fn an_administrators_symbol_is_pulled_with_its_source() {
        let transport = Fake::new(vec![
            Ok(reply(404, "{\"found\": false}")),
            Ok(reply(200, &record_json("INS-SOFR"))),
        ]);
        let platform = platform(transport.clone());
        // Ruling 545: after the open identifiers, the administrator's code,
        // asked with the administrator named.
        let asked = [
            identifier("symbol", "SOFR", "nyfed"),
            identifier("figi", "BBG000ZZTOP1", ""),
        ];
        let found = platform.ask(&asked, AS_OF, NOW).await.unwrap();
        assert_eq!(found.unwrap().instrument_id, "INS-SOFR");
        let urls = transport.urls();
        assert!(urls[0].contains("scheme=figi"), "{urls:?}");
        assert!(
            urls[1].contains("scheme=symbol&value=SOFR") && urls[1].ends_with("&source=nyfed"),
            "{urls:?}"
        );
    }

    #[tokio::test]
    async fn a_master_that_does_not_hold_it_is_nothing_rather_than_an_error() {
        let transport = Fake::new(vec![Ok(reply(
            404,
            "{\"found\": false, \"miss_reason\": \"MISS_REASON_NOT_FOUND\"}",
        ))]);

        let found = platform(transport)
            .pull_identifier(&identifier("figi", "BBG000ZZTOP1", ""), AS_OF, NOW)
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
    async fn an_ask_sends_open_identifiers_only_strongest_first() {
        // W3.3: never a licensed scheme's value, never a source-scoped symbol.
        let transport = Fake::new(vec![
            Ok(reply(404, "{\"found\": false}")),
            Ok(reply(200, &record_json("INS-USD"))),
        ]);
        let found = platform(transport.clone())
            .ask(
                &[
                    identifier("cusip", "037833100", ""),
                    identifier("symbol", "USD", "snaptrade"),
                    identifier("iso4217", "USD", ""),
                    identifier("figi", "BBG000USD001", ""),
                ],
                AS_OF,
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(found.unwrap().instrument_id, "INS-USD");
        let urls = transport.urls();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("scheme=figi"), "{}", urls[0]);
        assert!(urls[1].contains("scheme=iso4217"), "{}", urls[1]);
        assert!(!urls
            .iter()
            .any(|url| url.contains("cusip") || url.contains("symbol")));
    }

    #[tokio::test]
    async fn a_group_is_asked_by_its_members_ins_ids_and_its_evg_answer_read() {
        // Contract v26 (W3.29, W1.30).
        use meridian_domain::v1::{EscalateEventGroupRequest, EventGroupFlag};
        let answer = serde_json::json!({
            "event_group": {
                "event_group_id": "EVG-01JEVG00000000000000000001",
                "venue_id": "VEN-K", "venue_event_ref": "KXFED-26NOV", "title": "Fed",
                "member_instrument_ids": ["INS-A", "INS-B"],
                "mutually_exclusive": "EVENT_GROUP_FLAG_TRUE",
                "version": "2", "valid_from_ns": "5"
            },
            "minted": false, "pending_review": false
        })
        .to_string();
        let transport = Fake::new(vec![Ok(reply(200, &answer))]);
        let request = EscalateEventGroupRequest {
            venue_id: "VEN-K".into(),
            venue_event_ref: "KXFED-26NOV".into(),
            member_instrument_ids: vec!["INS-A".into(), "INS-B".into()],
            mutually_exclusive: EventGroupFlag::True as i32,
            request_key: "k".into(),
            ..Default::default()
        };
        let answered = platform(transport.clone())
            .escalate_event_group(&request, NOW)
            .await
            .unwrap();
        let group = answered.event_group.unwrap();
        assert_eq!(group.version, 2);
        assert_eq!(group.valid_from_ns, 5);
        assert_eq!(
            group.exhaustive,
            EventGroupFlag::NotStated as i32,
            "unknown is never false"
        );
        assert!(transport.urls()[0].ends_with("/api/v1/reference/escalate-event-group"));
        let body: serde_json::Value = serde_json::from_str(&transport.bodies()[0]).unwrap();
        assert_eq!(body["mutually_exclusive"], "EVENT_GROUP_FLAG_TRUE");
        assert_eq!(body["exhaustive"], "EVENT_GROUP_FLAG_NOT_STATED");
        assert_eq!(body["member_instrument_ids"][1], "INS-B");
        // Under an ID that is not the platform's: refused; a platform before
        // v26: unreachable.
        let wrong = answer.replace("EVG-01JEVG", "LCL-01JEVG");
        assert!(platform(Fake::new(vec![Ok(reply(200, &wrong))]))
            .escalate_event_group(&request, NOW)
            .await
            .is_err());
        let before = platform(Fake::new(vec![Ok(reply(404, "{}"))]))
            .escalate_event_group(&request, NOW)
            .await
            .unwrap_err();
        assert!(
            matches!(before, PlatformError::Unreachable { .. }),
            "{before}"
        );
    }

    #[tokio::test]
    async fn an_ask_by_the_global_id_pulls_the_record_by_its_own_name() {
        let transport = Fake::new(vec![Ok(reply(200, &record_json("INS-ONE")))]);
        platform(transport.clone())
            .ask(&[identifier(GLOBAL_ID, "INS-ONE", "")], AS_OF, NOW)
            .await
            .unwrap()
            .unwrap();
        assert!(transport.urls()[0].contains("/api/v1/reference/instruments/INS-ONE"));
    }

    #[tokio::test]
    async fn an_ask_the_platform_holds_nothing_for_is_nothing_and_mints_nothing() {
        let transport = Fake::new(vec![Ok(reply(404, "{\"found\": false}"))]);
        let found = platform(transport.clone())
            .ask(&[identifier("figi", "BBG000NONE01", "")], AS_OF, NOW)
            .await
            .unwrap();
        assert!(found.is_none());
        assert!(!transport
            .urls()
            .iter()
            .any(|url| url.contains("escalate-instrument")));
        let none = platform(Fake::new(vec![]))
            .ask(&[identifier("cusip", "037833100", "")], AS_OF, NOW)
            .await
            .unwrap();
        assert!(none.is_none(), "nothing open to ask by, nothing asked");
    }

    #[tokio::test]
    async fn an_escalation_never_names_the_deployment_or_its_local_record() {
        // W3.4, contract v24 (the one-ID spec's requirement 7): what to look
        // for, the template, the offered fields and a request key; never the
        // deployment's LCL- ID or its name, which v9's shape sent.
        let transport = Fake::new(vec![Ok(reply(
            201,
            &serde_json::json!({
                "instrument": serde_json::from_str::<serde_json::Value>(
                    &record_json("INS-01J8XQ4M7K0000000000ZZTP")
                ).unwrap()["instrument"],
                "minted": true, "near_match": false, "pending_review": false, "conflict": false,
            })
            .to_string(),
        ))]);
        let answer = platform(transport.clone())
            .escalate(&escalation(), NOW)
            .await
            .unwrap();
        assert!(answer.minted && !answer.pending_review);
        assert_eq!(
            answer.instrument.unwrap().instrument_id,
            "INS-01J8XQ4M7K0000000000ZZTP"
        );
        let body = transport.seen.lock().unwrap()[0].body.clone().unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(sent.get("placeholder_instrument_id").is_none());
        assert!(sent.get("requesting_deployment_id").is_none());
        assert!(!String::from_utf8_lossy(&body).contains("LCL-"));
        assert!(!String::from_utf8_lossy(&body).contains(DEPLOYMENT));
        assert_eq!(
            sent["asset_class"], "equity",
            "lower case, as ruled for the JSON"
        );
        assert_eq!(sent["template_id"], "TPL-equity");
        assert_eq!(sent["request_key"], "01JB2N7Q0000000000000000RK");
        assert_eq!(sent["fields"][0]["path"], "description");
    }

    #[tokio::test]
    async fn a_request_queued_for_staff_is_said_and_carries_no_record() {
        let transport = Fake::new(vec![Ok(reply(
            200,
            "{\"minted\": false, \"pending_review\": true, \"near_match\": false, \"instrument\": null}",
        ))]);
        let answer = platform(transport)
            .escalate(&escalation(), NOW)
            .await
            .unwrap();
        assert!(answer.pending_review && !answer.minted);
        assert!(answer.instrument.is_none());
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
    async fn a_record_with_an_asset_class_the_schema_does_not_define_is_refused() {
        // sdk-contract/asset-class-is-an-enum: a class is one the enum defines,
        // or none. One that is neither is refused, not read as none.
        let transport = Fake::new(vec![Ok(reply(
            200,
            &serde_json::json!({
                "found": true,
                "instrument": {
                    "instrument_id": "INS-ONE",
                    "asset_class": "equities-ish",
                    "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
                }
            })
            .to_string(),
        ))]);

        let failed = platform(transport)
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap_err();

        match failed {
            PlatformError::Malformed(detail) => {
                assert!(detail.contains("equities-ish"), "{detail}")
            }
            other => panic!("expected a malformed record, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_class_from_before_the_enum_is_read_as_the_enums() {
        // A platform that has not yet migrated its master sends what the free
        // text held; its migration maps it the same way.
        let transport = Fake::new(vec![Ok(reply(
            200,
            &serde_json::json!({
                "found": true,
                "instrument": {
                    "instrument_id": "INS-ONE",
                    "asset_class": "ETF",
                    "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
                }
            })
            .to_string(),
        ))]);

        let record = platform(transport)
            .pull_instrument("INS-ONE", AS_OF, NOW)
            .await
            .unwrap()
            .expect("found");

        assert_eq!(record.asset_class, AssetClass::Fund as i32);
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
    async fn an_outage_is_an_error_here_and_nothing_downstream() {
        // The requirement the split exists for, from this side. A platform
        // outage is an error on this call and nothing at all downstream: the
        // instrument store is a separate process holding a separate store, and it never
        // hears about this.
        //
        // The other half of the assertion -- that a resolve still answers --
        // moved to meridian-instrument with the instrument store on 2026-09-21. It is
        // structural there rather than tested: that crate has no platform
        // client to fail, and no key that would let it build one.
        let transport = Fake::new(vec![failure("no route to host")]);

        assert!(platform(transport)
            .pull_instrument("INS-OTHER", AS_OF, NOW)
            .await
            .unwrap_err()
            .is_outage());
    }
}
