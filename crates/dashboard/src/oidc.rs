//! Signing a person in through the firm's directory, over OpenID Connect.
//!
//! The authorisation-code flow with PKCE, against whatever provider the
//! deployment names: the firm's own, or the bundled Zitadel standing in for
//! one or brokering a SAML or LDAP directory. The dashboard speaks OIDC and
//! nothing else (spec, ruling 1).
//!
//! # Fresh, every time
//!
//! Every sign-in asks the directory to authenticate anew (`prompt=login`,
//! `max_age=0`), and the answer is refused when its `auth_time` is earlier
//! than the moment this sign-in began. The check is what the rule rests on,
//! not the parameters: a broker that answered from its own session instead of
//! returning to the directory would carry groups the directory may have since
//! withdrawn, and its `auth_time` gives it away (decisions/015). No refresh
//! token is asked for or kept.
//!
//! # What is kept
//!
//! The directory's word reduces at once to three things: a deployment-local
//! subject (issuer and subject), a name to show, and the directory groups
//! presented. Nothing else from the token is stored, and nothing in it
//! decides what the person may do.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use openidconnect::core::{
    CoreAuthPrompt, CoreAuthenticationFlow, CoreClient, CoreProviderMetadata,
};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
};

use crate::clock::{MINUTE_NS, SECOND_NS};

/// How long a started sign-in may take to come back.
pub const PENDING_NS: i64 = 10 * MINUTE_NS;

/// How far the directory's clock may run behind this one before an
/// authentication that happened after the sign-in began looks as if it came
/// before. Small, because every second of it is a second a broker's cached
/// session could hide in.
pub const SKEW_NS: i64 = 30 * SECOND_NS;

pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    /// None for a public client, which PKCE makes safe.
    pub client_secret: Option<String>,
    /// Where the directory sends the person back: this dashboard's `/callback`.
    pub redirect_url: String,
    /// The claim carrying directory groups. `groups` for Entra ID and for the
    /// bundled Zitadel's group hook.
    pub groups_claim: String,
    /// Audiences besides the client id that an ID token may also name. Empty
    /// for most directories. Zitadel always adds the id of the project the
    /// client belongs to, and a token naming an audience not listed here is
    /// refused, as OpenID Connect Core 3.1.3.7 says it must be.
    pub trusted_audiences: Vec<String>,
}

/// Who the directory says signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub display_name: String,
    pub groups: Vec<String>,
}

type Provider = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

struct Pending {
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    started_at_ns: i64,
}

pub struct Oidc {
    provider: Provider,
    http: reqwest::Client,
    groups_claim: String,
    trusted_audiences: Vec<String>,
    pending: Mutex<HashMap<String, Pending>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(String);

/// The provider's HTTP, over the runtime's own client. Redirects are not
/// followed: a directory that answers discovery or the token exchange with a
/// redirect is refused, rather than sending the dashboard somewhere else.
async fn send(
    http: reqwest::Client,
    request: openidconnect::HttpRequest,
) -> Result<openidconnect::HttpResponse, HttpError> {
    let request =
        reqwest::Request::try_from(request).map_err(|failed| HttpError(failed.to_string()))?;
    let response = http
        .execute(request)
        .await
        .map_err(|failed| HttpError(failed.to_string()))?;
    let mut built = http::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        built = built.header(name, value);
    }
    let body = response
        .bytes()
        .await
        .map_err(|failed| HttpError(failed.to_string()))?;
    built
        .body(body.to_vec())
        .map_err(|failed| HttpError(failed.to_string()))
}

impl Oidc {
    /// Read the provider's discovery document and keys.
    pub async fn discover(config: &OidcConfig) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|failed| failed.to_string())?;
        let issuer = IssuerUrl::new(config.issuer.clone())
            .map_err(|failed| format!("the issuer: {failed}"))?;
        let client = http.clone();
        let metadata = CoreProviderMetadata::discover_async(issuer, &move |request| {
            send(client.clone(), request)
        })
        .await
        .map_err(|failed| {
            format!("the directory's discovery document could not be read: {failed}")
        })?;
        let provider = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.client_id.clone()),
            config.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(config.redirect_url.clone())
                .map_err(|failed| format!("the redirect URL: {failed}"))?,
        );
        Ok(Self {
            provider,
            http,
            groups_claim: config.groups_claim.clone(),
            trusted_audiences: config.trusted_audiences.clone(),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Start a sign-in: where to send the browser, and the state that binds
    /// its return to this browser.
    pub fn begin(&self, now_ns: i64) -> (String, String) {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = self
            .provider
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("profile".into()))
            .add_scope(Scope::new("email".into()))
            .set_pkce_challenge(challenge)
            .add_prompt(CoreAuthPrompt::Login)
            .set_max_age(Duration::ZERO)
            .url();
        let mut pending = self.pending.lock().expect("pending lock poisoned");
        pending.retain(|_, p| now_ns - p.started_at_ns <= PENDING_NS);
        pending.insert(
            state.secret().clone(),
            Pending {
                verifier,
                nonce,
                started_at_ns: now_ns,
            },
        );
        (url.to_string(), state.secret().clone())
    }

    /// Finish one: exchange the code, verify the token, and insist it is a
    /// fresh authentication. A state is used once, whatever the outcome.
    pub async fn finish(&self, state: &str, code: &str, now_ns: i64) -> Result<Identity, String> {
        let pending = self
            .pending
            .lock()
            .expect("pending lock poisoned")
            .remove(state)
            .ok_or("this sign-in was not started here, or has already been used")?;
        if now_ns - pending.started_at_ns > PENDING_NS {
            return Err("this sign-in took too long; start again".into());
        }

        let client = self.http.clone();
        let tokens = self
            .provider
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|failed| failed.to_string())?
            .set_pkce_verifier(pending.verifier)
            .request_async(&move |request| send(client.clone(), request))
            .await
            .map_err(|failed| format!("the directory refused the sign-in: {failed}"))?;

        let id_token = tokens
            .id_token()
            .ok_or("the directory returned no ID token")?;
        let trusted = self.trusted_audiences.clone();
        let verifier = self
            .provider
            .id_token_verifier()
            .set_other_audience_verifier_fn(move |audience| trusts(&trusted, audience));
        let claims = id_token
            .claims(&verifier, &pending.nonce)
            .map_err(|failed| format!("the directory's ID token did not verify: {failed}"))?;

        let authenticated_at = claims
            .auth_time()
            .and_then(|at| at.timestamp_nanos_opt())
            .ok_or("the directory did not say when this person authenticated")?;
        fresh(authenticated_at, pending.started_at_ns)?;

        // The token verified above; its payload is read again only for the
        // two claims the core claim set does not name.
        let payload = raw_payload(&id_token.to_string())?;
        Ok(Identity {
            subject: format!("{}|{}", claims.issuer().as_str(), claims.subject().as_str()),
            display_name: display_name(&payload, claims.subject().as_str()),
            groups: groups(&payload, &self.groups_claim),
        })
    }
}

/// Refused when the directory authenticated the person before this sign-in
/// began, beyond the skew: it answered from a session instead of asking.
pub fn fresh(authenticated_at_ns: i64, started_at_ns: i64) -> Result<(), String> {
    if authenticated_at_ns + SKEW_NS < started_at_ns {
        return Err(
            "the directory answered from an earlier sign-in instead of asking you again; \
             sign out of it and try again"
                .into(),
        );
    }
    Ok(())
}

/// Whether an audience other than the client id is one this dashboard was
/// told to accept. Exact match only: no prefix, no case folding.
pub fn trusts(trusted: &[String], audience: &str) -> bool {
    trusted.iter().any(|t| t == audience)
}

fn raw_payload(compact: &str) -> Result<serde_json::Value, String> {
    let part = compact
        .split('.')
        .nth(1)
        .ok_or("the ID token is not a JWT")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .map_err(|failed| failed.to_string())?;
    serde_json::from_slice(&bytes).map_err(|failed| failed.to_string())
}

pub fn display_name(payload: &serde_json::Value, subject: &str) -> String {
    ["name", "preferred_username", "email"]
        .iter()
        .find_map(|claim| {
            payload
                .get(claim)
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or(subject)
        .to_string()
}

/// The directory groups a token presents. A claim that is absent, or not a
/// list of strings, presents none: a malformed claim grants nothing.
pub fn groups(payload: &serde_json::Value, claim: &str) -> Vec<String> {
    payload
        .get(claim)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|g| g.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const T0: i64 = 1_790_380_800_000_000_000;

    #[test]
    fn an_authentication_after_the_sign_in_began_is_fresh() {
        assert!(fresh(T0 + SECOND_NS, T0).is_ok());
        assert!(fresh(T0 - SKEW_NS, T0).is_ok(), "within the skew");
    }

    #[test]
    fn an_answer_from_an_earlier_session_is_refused() {
        let refused = fresh(T0 - SKEW_NS - 1, T0).unwrap_err();
        assert!(refused.contains("earlier sign-in"));
        assert!(
            fresh(T0 - 3_600 * SECOND_NS, T0).is_err(),
            "an hour-old session"
        );
    }

    #[test]
    fn only_a_listed_audience_is_trusted() {
        let trusted = vec!["391790364140240901".to_string()];
        assert!(trusts(&trusted, "391790364140240901"));
        assert!(!trusts(&trusted, "391790364140240902"));
        assert!(!trusts(&trusted, "39179036414024090"));
        assert!(!trusts(&[], "391790364140240901"), "none by default");
    }

    #[test]
    fn groups_come_only_from_a_list_of_strings() {
        assert_eq!(
            groups(&json!({"groups": ["ops", "trading-desk"]}), "groups"),
            ["ops", "trading-desk"]
        );
        assert!(groups(&json!({"groups": "ops"}), "groups").is_empty());
        assert!(groups(&json!({}), "groups").is_empty());
        assert_eq!(groups(&json!({"groups": ["ops", 7]}), "groups"), ["ops"]);
    }

    #[test]
    fn a_name_falls_back_to_the_subject() {
        assert_eq!(
            display_name(&json!({"name": "Ada Park"}), "8812"),
            "Ada Park"
        );
        assert_eq!(
            display_name(&json!({"email": "ada@x.org"}), "8812"),
            "ada@x.org"
        );
        assert_eq!(display_name(&json!({"name": ""}), "8812"), "8812");
    }
}
