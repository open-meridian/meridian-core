//! W6.16: a local administrator who lost their password, back in with a code
//! from the platform (design/a-lost-local-password).
//!
//! An owner or admin of the deployment's organisation issues a password-reset
//! code on the platform. Here, somebody who cannot sign in gives it, the login
//! to reset and a new password twice. The code goes to the conductor, which
//! presents it to the platform; once it is honoured, the login is checked --
//! a local account holding deployment admin -- and its password set, its
//! failures and lock cleared, and every session it held ended.
//!
//! The code is spent before the login is checked. Checked first, this page
//! would tell anybody, with no code at all, which logins are administrators.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use meridian_domain::v1::{ClaimCodePurpose, RedeemClaimCodeReply, RedeemClaimCodeRequest};
use prost::Message;

use super::{redirect, App};
use crate::accounts;
use crate::first_run::{ACCOUNT_PASSWORD_MIN, REDEEM_CLAIM_CODE};
use crate::html::{escape, page};

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/sign-in/reset", get(form).post(reset))
}

/// Offered where this deployment holds the accounts, and nowhere else: a
/// directory's password is the directory's to reset.
fn offered(app: &App) -> Option<&Arc<dyn accounts::Accounts>> {
    match (&app.accounts, &app.directory) {
        (Some(held), None) => Some(held),
        _ => None,
    }
}

fn not_offered() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(page(
            "Reset a password",
            "<h1>Reset a password</h1><p>This deployment signs people in through a \
             directory, which is where their passwords are reset.</p>\
             <p><a href=\"/sign-in\">Sign in</a></p>",
        )),
    )
        .into_response()
}

const SCRIPT: &str = r#"(function () {
  var form = document.getElementById("reset");
  if (!form) return;
  form.querySelectorAll("[data-reveal]").forEach(function (button) {
    button.hidden = false;
    button.addEventListener("click", function (event) {
      event.preventDefault();
      var field = button.previousElementSibling;
      var showing = field.type === "text";
      field.type = showing ? "password" : "text";
      button.textContent = showing ? "Show" : "Hide";
    });
  });
  var password = form.elements["password"], again = form.elements["password_again"];
  function matching() {
    again.setCustomValidity(again.value && again.value !== password.value
      ? "The two passwords differ." : "");
  }
  password.addEventListener("input", matching);
  again.addEventListener("input", matching);
})();"#;

fn reset_page(refusal: &str, login: &str) -> String {
    let told = if refusal.is_empty() {
        String::new()
    } else {
        format!("<p class=\"refused\">{}</p>", escape(refusal))
    };
    let reveal = "<button type=\"button\" class=\"reveal\" data-reveal hidden>Show</button>";
    page(
        "Reset a password",
        &format!(
            "<h1>Reset a password</h1>\
             <p class=\"hint\">For a local administrator who cannot sign in. An owner or \
             admin of this deployment's organisation issues a password-reset code on \
             open-meridian.com. The code is spent when you submit this, even when the \
             login is not one it can reset.</p>{told}\
             <form method=\"post\" action=\"/sign-in/reset\" id=\"reset\">\
             <label>Password-reset code<input name=\"code\" autocomplete=\"off\" required></label>\
             <label>Login<input name=\"login\" value=\"{login}\" autocomplete=\"username\" required></label>\
             <div class=\"grid-2\">\
             <label>New password<input type=\"password\" name=\"password\" \
             autocomplete=\"new-password\" minlength=\"{ACCOUNT_PASSWORD_MIN}\" required>{reveal}</label>\
             <label>New password, again<input type=\"password\" name=\"password_again\" \
             autocomplete=\"new-password\" minlength=\"{ACCOUNT_PASSWORD_MIN}\" required>{reveal}</label>\
             </div>\
             <p class=\"hint\">At least {ACCOUNT_PASSWORD_MIN} characters, and the same twice.</p>\
             <button type=\"submit\" class=\"primary\">Set the password</button>\
             </form><p class=\"hint\"><a href=\"/sign-in\">Back to sign in</a></p>\
             <script>{SCRIPT}</script>",
            login = escape(login),
        ),
    )
}

async fn form(State(app): State<Arc<App>>) -> Response {
    if offered(&app).is_none() {
        return not_offered();
    }
    Html(reset_page("", "")).into_response()
}

fn refused_here(said: &str, login: &str, status: StatusCode) -> Response {
    (status, Html(reset_page(said, login))).into_response()
}

async fn reset(
    State(app): State<Arc<App>>,
    Form(fields): Form<HashMap<String, String>>,
) -> Response {
    let Some(accounts) = offered(&app).cloned() else {
        return not_offered();
    };
    let field = |name: &str| fields.get(name).map(String::as_str).unwrap_or_default();
    let (code, login, password) = (
        field("code").trim(),
        field("login").trim(),
        field("password"),
    );

    // Before the code is presented, so a mistyped password does not spend it.
    if password.chars().count() < ACCOUNT_PASSWORD_MIN {
        return refused_here(
            &format!("A password needs at least {ACCOUNT_PASSWORD_MIN} characters. The code was not used."),
            login,
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }
    if field("password_again") != password {
        return refused_here(
            "The two passwords differ. The code was not used.",
            login,
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }

    let asked = RedeemClaimCodeRequest {
        code: code.to_string(),
        purpose: ClaimCodePurpose::ResetLocalAdmin as i32,
    };
    let reply = match app
        .bus
        .call(
            REDEEM_CLAIM_CODE,
            "meridian.v1.RedeemClaimCodeRequest",
            asked.encode_to_vec(),
            None,
            None,
        )
        .await
    {
        Ok((_, payload)) => RedeemClaimCodeReply::decode(&payload[..]).unwrap_or_default(),
        Err(failed) => {
            return refused_here(
                &format!(
                    "This deployment could not ask the platform: {failed}. The code was not used."
                ),
                login,
                StatusCode::SERVICE_UNAVAILABLE,
            )
        }
    };
    if !reply.redeemed {
        return refused_here(
            &format!(
                "The platform did not accept that code: {}.",
                reply.refusal_reason
            ),
            login,
            StatusCode::UNAUTHORIZED,
        );
    }

    // Spent. Now the login: a local account, holding deployment admin.
    let now = app.clock.now_ns();
    let records = match app.records.current(now) {
        Ok(records) => records,
        Err(stale) => {
            return refused_here(
                &format!("{stale}. The code was spent: ask for another."),
                login,
                StatusCode::SERVICE_UNAVAILABLE,
            )
        }
    };
    let name = login.to_string();
    let new_password = password.to_string();
    let setting = Arc::clone(&accounts);
    let set = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
        let Some(mut account) = setting.by_name(&name)? else {
            return Ok(None);
        };
        let subject = meridian_access::local_login(&account.name);
        if !meridian_access::person_access(&records, &subject, &account.groups).deployment_admin {
            return Ok(None);
        }
        account.password_hash = accounts::hash_password(&new_password)?;
        setting.put(&account)?;
        // `put` keeps the counters, as a password change should; a reset
        // is what lifts them.
        setting.count_attempt(&account.name, true, now)?;
        Ok(Some(subject))
    })
    .await;
    let subject = match set {
        Ok(Ok(Some(subject))) => subject,
        Ok(Ok(None)) => {
            return refused_here(
                "That login is not a local account holding deployment admin here. \
                 The code was spent: ask for another.",
                login,
                StatusCode::UNPROCESSABLE_ENTITY,
            )
        }
        Ok(Err(failed)) => {
            tracing::warn!(%failed, "a password reset could not be written");
            return refused_here(
                "This deployment could not set that password. The code was spent: ask for another.",
                login,
                StatusCode::SERVICE_UNAVAILABLE,
            );
        }
        Err(joined) => {
            tracing::error!(%joined, "a password reset did not finish");
            return refused_here(
                "This deployment could not set that password. The code was spent: ask for another.",
                login,
                StatusCode::SERVICE_UNAVAILABLE,
            );
        }
    };

    let browsers = app.sessions.end_person(&subject);
    let terminals = app.terminals.end_person(&subject);
    app.sign_in_failures.clear(login);
    tracing::info!(
        login,
        browsers,
        terminals,
        "a local administrator's password was reset with a code from the platform"
    );
    redirect("/sign-in?reset=done")
}

#[cfg(test)]
mod tests;
