//! A terminal path's credential: an access token on a delegation (W6.18,
//! decisions/029), which `meridian connect` obtains through the dashboard's
//! OAuth authorisation ([`super::oauth`]).
//!
//! The terminal sessions from before delegations -- W6.13's own sign-in at
//! `/terminal/authorize`, its code at `/terminal/token`, `/terminal/sign-out`
//! and a deployment admin's ending of them -- were honoured for one release
//! beside delegations and are retired from contract v15 (v15's sweep): the
//! CLI connects by delegation since 0.1.25, and this dashboard serves no older
//! one ([`OLDEST_CLI`]).
//!
//! Two kinds of client, two kinds of credential, and neither accepted where
//! the other belongs: a bearer session only on `/terminal/` paths, which read
//! no cookie, and a browser's cookie everywhere else, which read no bearer.
//! So the form tokens protecting a browser's forms never have to reason about
//! a client that sends no cookie, and a terminal's session cannot be made to
//! drive a browser page.

use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::App;
use crate::delegation::{narrow, Covers, Refusal, Resource, ACCESS_PREFIX};
use crate::terminal::{Person, Unavailable};
use meridian_domain::v1::AccessRecords;

/// The header the CLI names its version in (W6.13; spec/the-cli, ruling 8).
pub const CLI_VERSION: &str = "meridian-cli-version";

/// The oldest CLI this dashboard serves. Raised when the terminal's surface
/// changes in a way an older CLI would get wrong, and never otherwise: to
/// 0.1.25 at contract v15, the first CLI connecting by delegation, when the
/// terminal sessions it replaced were retired.
pub const OLDEST_CLI: &str = "0.1.25";

/// Every request on a terminal path naming a CLI version older than
/// [`OLDEST_CLI`], or one that does not read as a version, is refused here
/// with what this serves, before any route sees it. One naming none is
/// served: anybody may do by hand what the CLI does.
pub(crate) async fn cli_version(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.uri().path().starts_with("/terminal/") {
        if let Some(named) = request.headers().get(CLI_VERSION) {
            if let Err(reason) = served(named.to_str().unwrap_or_default()) {
                return json(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({
                        "error": "cli_version",
                        "reason": reason,
                        "serves": format!("{OLDEST_CLI} or later"),
                    }),
                );
            }
        }
    }
    next.run(request).await
}

/// `x.y.z`, with anything after a `-` or `+` set aside.
fn version(named: &str) -> Option<[u64; 3]> {
    let core = named.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let read = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(read)
}

fn served(named: &str) -> Result<(), String> {
    let asked = version(named).ok_or_else(|| {
        format!(
            "`{named}` is not a CLI version; this deployment serves meridian {OLDEST_CLI} or later"
        )
    })?;
    let oldest = version(OLDEST_CLI).expect("OLDEST_CLI is a version");
    if asked < oldest {
        return Err(format!(
            "meridian {named} is older than this deployment serves; it serves meridian {OLDEST_CLI} or later"
        ));
    }
    Ok(())
}

/// Who a request on a terminal path acts for, and through what.
pub struct Caller {
    pub person: Person,
    pub through: Through,
}

/// What a terminal path's bearer credential was.
pub enum Through {
    /// An access token on a delegation (W6.18).
    Delegation {
        id: String,
        client_name: String,
        covers: Covers,
    },
}

impl Caller {
    /// What they may reach now: the person's access from the records as they
    /// are, cut to what the delegation covers, if it came through one.
    pub fn access(&self, records: &AccessRecords) -> meridian_access::Access {
        let access = meridian_access::person_access(
            records,
            &self.person.subject,
            &self.person.directory_groups,
        );
        match &self.through {
            Through::Delegation { covers, .. } => narrow(access, covers, records),
        }
    }

    pub fn delegation_id(&self) -> Option<&str> {
        match &self.through {
            Through::Delegation { id, .. } => Some(id),
        }
    }

    /// Note why a request through a delegation was refused, for the person
    /// and the admin to read beside it.
    pub async fn refused(&self, app: &App, why: &str) {
        if let Some(id) = self.delegation_id() {
            if let Err(failed) = app.delegations.refused(id, why, app.clock.now_ns()).await {
                tracing::warn!(%failed, "a delegation's refusal was not recorded");
            }
        }
    }
}

fn unavailable(unavailable: Unavailable) -> Box<Response> {
    // A store that cannot be asked is ours to fix, not the person's: a 503,
    // so the CLI does not tell them to sign in again.
    tracing::error!(%unavailable, "a terminal credential could not be read");
    Box::new(json(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({"error": "temporarily_unavailable", "error_description": unavailable.to_string()}),
    ))
}

/// 401, naming why, and a sentence for a delegation's refusals, which an
/// unsupervised client reports as what stopped it. A terminal session's
/// keeps the answer the CLIs that hold one already read.
fn invalid_token(
    app: &App,
    headers: &HeaderMap,
    reason: &str,
    sentence: Option<&str>,
) -> Box<Response> {
    let mut body = serde_json::json!({"error": "invalid_token", "reason": reason});
    if let Some(sentence) = sentence {
        body["error_description"] = sentence.into();
    }
    let mut response = json(StatusCode::UNAUTHORIZED, body);
    // Where a client finds who issues this surface's tokens (RFC 9728).
    let metadata = format!(
        "Bearer error=\"invalid_token\", resource_metadata=\"{}/.well-known/oauth-protected-resource/terminal\"",
        super::oauth::issuer(app, headers)
    );
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_str(&metadata).unwrap_or_else(|_| {
            axum::http::HeaderValue::from_static("Bearer error=\"invalid_token\"")
        }),
    );
    Box::new(response)
}

/// Who a request on a terminal path acts for -- an access token on a
/// delegation -- or the refusal to send back. Only for `/terminal/` paths, and it reads no cookie.
pub async fn caller_of(app: &App, headers: &HeaderMap) -> Result<Caller, Box<Response>> {
    let now = app.clock.now_ns();
    let Some(presented) = bearer(headers) else {
        return Err(invalid_token(app, headers, Refusal::Unknown.reason(), None));
    };
    if presented.starts_with(ACCESS_PREFIX) {
        let checked = app
            .delegations
            .check(
                &presented,
                Resource::Terminal,
                super::oauth::groups_bound(app),
                now,
            )
            .await
            .map_err(unavailable)?;
        return match checked {
            Ok(delegation) => Ok(Caller {
                person: Person {
                    subject: delegation.subject,
                    display_name: delegation.display_name,
                    directory_groups: delegation.directory_groups,
                    signed_in_at_ns: delegation.groups_read_at_ns,
                },
                through: Through::Delegation {
                    id: delegation.id,
                    client_name: delegation.client_name,
                    covers: delegation.covers,
                },
            }),
            Err(refusal) => Err(invalid_token(
                app,
                headers,
                refusal.reason(),
                Some(refusal.sentence()),
            )),
        };
    }
    // Anything else -- a terminal session from before delegations among
    // them -- names nobody here.
    Err(invalid_token(app, headers, Refusal::Unknown.reason(), None))
}

pub(crate) fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_string())
}

fn json(status: StatusCode, body: serde_json::Value) -> Response {
    // Never cached: a session is in one of these, and a refusal is about now.
    (status, [(CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

#[cfg(test)]
mod tests;
