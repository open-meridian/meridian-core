//! The wizard, and what it refuses to be before somebody is let in.
//!
//! A deployment installs with no database and no directory: the wizard is
//! where both are configured, so it has to serve before either exists
//! (spec/installation-and-first-run, requirement 5). That makes it the least
//! authenticated page this dashboard will ever have, and in a cloud it may be
//! reachable from the internet.
//!
//! So two rules, and everything here is one of them:
//!
//! - **Until a first-run claim code is redeemed, one page answers and nothing
//!   else does** (requirement 11): the enrolment state, the key's fingerprint,
//!   and a field for the code. The rest is `404`, not a redirect, because a
//!   redirect tells a prober that a page exists.
//! - **The wizard holds no right to change the cluster.** It asks the
//!   first-run Job, which holds them and gives them up (decisions/016).
//!
//! The code is verified by the platform, through the conductor, exactly as the
//! first administrator's is (W7.3, W5.22). Redeeming one also brings back the
//! first administrator's code, issued in the same act, which the wizard shows
//! once when the configuration is applied and never writes down.

use std::sync::{Arc, Mutex};

use axum::extract::{Form, State};
use axum::http::header::{LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use meridian_domain::v1::{
    ClaimCodePurpose, EnrolmentState, EnrolmentStateRequest, RedeemClaimCodeReply,
    RedeemClaimCodeRequest,
};
use prost::Message;
use std::collections::HashMap;

use crate::html::{escape, page};
use crate::session::{token, ABSOLUTE_NS};
use crate::web::{cookie, set_cookie, App};

pub const ENROLMENT_STATE: &str = "platform.config.query.enrolment";
pub const REDEEM_CLAIM_CODE: &str = "platform.config.command.redeem-claim-code";
pub const WIZARD_COOKIE: &str = "meridian_first_run";

/// One wizard session, bound to the browser that redeemed the code.
///
/// One, because a deployment is set up once by one person. A second browser
/// with a second code would be two people configuring one deployment, and the
/// last to press apply would win silently.
#[derive(Debug, Clone)]
pub struct Wizard {
    pub token: String,
    pub started_at_ns: i64,
    /// Issued with the first-run code in the same act on the platform
    /// (ruling 3). Held here until the configuration is applied, shown once
    /// then, and never stored anywhere else.
    pub first_admin_code: String,
}

#[derive(Default)]
pub struct WizardSession(Mutex<Option<Wizard>>);

impl WizardSession {
    pub fn start(&self, first_admin_code: String, now_ns: i64) -> String {
        let wizard = Wizard {
            token: token(),
            started_at_ns: now_ns,
            first_admin_code,
        };
        let key = wizard.token.clone();
        if let Ok(mut held) = self.0.lock() {
            *held = Some(wizard);
        }
        key
    }

    /// The live session this request is in, if it is in one. Bounded like a
    /// signed-in person's, because a wizard left open in an office is the same
    /// exposure as a session left open (decisions/015).
    pub fn of(&self, presented: Option<&str>, now_ns: i64) -> Option<Wizard> {
        let held = self.0.lock().ok()?;
        let wizard = held.as_ref()?;
        if presented? != wizard.token {
            return None;
        }
        if now_ns - wizard.started_at_ns > ABSOLUTE_NS {
            return None;
        }
        Some(wizard.clone())
    }

    /// Ends first run for this process. What makes it permanent is the
    /// configuration the Job wrote: the dashboard restarts into a directory
    /// and never serves these pages again.
    pub fn end(&self) {
        if let Ok(mut held) = self.0.lock() {
            *held = None;
        }
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/first-run", get(first_page))
        .route("/first-run/claim", post(claim))
}

/// What the wizard answers before a code is redeemed, and after.
async fn first_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !app.first_run {
        return not_here();
    }

    let now = app.clock.now_ns();
    let held = app
        .wizard
        .of(cookie(&headers, WIZARD_COOKIE).as_deref(), now);

    let enrolment = enrolment_state(&app).await;
    match held {
        None => Html(closed_page(&enrolment, "")).into_response(),
        Some(_) => Html(open_page(&enrolment)).into_response(),
    }
}

/// W7.3. Redeem a first-run code, and start the one wizard session.
async fn claim(
    State(app): State<Arc<App>>,
    Form(fields): Form<HashMap<String, String>>,
) -> Response {
    if !app.first_run {
        return not_here();
    }

    let code = fields.get("code").map(String::as_str).unwrap_or_default();
    let request = RedeemClaimCodeRequest {
        code: code.trim().to_string(),
        purpose: ClaimCodePurpose::FirstRun as i32,
    };

    let answered = app
        .bus
        .call(
            REDEEM_CLAIM_CODE,
            "meridian.v1.RedeemClaimCodeRequest",
            request.encode_to_vec(),
            None,
            None,
        )
        .await;

    let reply = match answered {
        Ok((_, payload)) => RedeemClaimCodeReply::decode(&payload[..]).unwrap_or_default(),
        // The conductor is what reaches the platform. Unreachable is not
        // "refused": the code may be perfectly good and the deployment is not
        // yet able to ask.
        Err(failed) => {
            let enrolment = enrolment_state(&app).await;
            return Html(closed_page(
                &enrolment,
                &format!("The deployment could not ask the platform: {failed}"),
            ))
            .into_response();
        }
    };

    if !reply.redeemed {
        let enrolment = enrolment_state(&app).await;
        let reason = if reply.refusal_reason.is_empty() {
            "That code was refused.".to_string()
        } else {
            format!("That code was refused: {}.", reply.refusal_reason)
        };
        return Html(closed_page(&enrolment, &reason)).into_response();
    }

    let key = app.wizard.start(reply.first_admin_code, app.clock.now_ns());

    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        set_cookie(&app, WIZARD_COOKIE, &key, "/", ABSOLUTE_NS / 1_000_000_000),
    );
    headers.insert(LOCATION, "/first-run".parse().expect("a fixed path"));
    (StatusCode::SEE_OTHER, headers).into_response()
}

/// Not a redirect: a page that redirects tells somebody it is there.
fn not_here() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

async fn enrolment_state(app: &Arc<App>) -> EnrolmentState {
    match app
        .bus
        .call(
            ENROLMENT_STATE,
            "meridian.v1.EnrolmentStateRequest",
            EnrolmentStateRequest {}.encode_to_vec(),
            None,
            None,
        )
        .await
    {
        Ok((_, payload)) => EnrolmentState::decode(&payload[..]).unwrap_or_default(),
        Err(failed) => EnrolmentState {
            refusal_reason: format!("the conductor did not answer: {failed}"),
            ..Default::default()
        },
    }
}

fn enrolment_summary(state: &EnrolmentState) -> String {
    if state.enrolled {
        format!(
            "<p>This deployment is <strong>{}</strong>, and its key is registered \
             with the platform.</p>\
             <p>Its fingerprint is <code>{}</code>. The platform shows the same \
             one beside this deployment. If they differ, somebody else spent \
             the enrolment code: revoke that key on the platform.</p>",
            escape(&state.deployment_id),
            escape(&state.fingerprint)
        )
    } else {
        format!(
            "<p>This deployment is <strong>{}</strong>, and its key is \
             <strong>not registered</strong>: {}.</p>\
             <p>Issue another enrolment code on the platform and give it to \
             this deployment. Nothing needs reinstalling.</p>",
            escape(&state.deployment_id),
            escape(if state.refusal_reason.is_empty() {
                "no reason given"
            } else {
                &state.refusal_reason
            })
        )
    }
}

/// The only page the wizard serves until a code is redeemed.
fn closed_page(state: &EnrolmentState, refusal: &str) -> String {
    let refusal = if refusal.is_empty() {
        String::new()
    } else {
        format!("<p class=\"refusal\">{}</p>", escape(refusal))
    };
    page(
        "Set up this deployment",
        &format!(
            "<h1>Set up this deployment</h1>\
             {}\
             {refusal}\
             <form method=\"post\" action=\"/first-run/claim\">\
             <label for=\"code\">First-run code</label>\
             <input id=\"code\" name=\"code\" autocomplete=\"off\" required>\
             <button type=\"submit\">Continue</button>\
             </form>\
             <p>A deployment administrator of the organisation that owns this \
             deployment issues the code on the platform. It is single use and \
             lasts a day.</p>",
            enrolment_summary(state)
        ),
    )
}

/// What a redeemed session sees. The steps themselves are the next slice;
/// until they exist this says what has happened and what has not.
fn open_page(state: &EnrolmentState) -> String {
    page(
        "Set up this deployment",
        &format!(
            "<h1>Set up this deployment</h1>\
             {}\
             <p>The code was accepted. The database, the login backend and the \
             addresses are configured from here.</p>",
            enrolment_summary(state)
        ),
    )
}

#[cfg(test)]
#[path = "first_run/tests.rs"]
mod tests;
