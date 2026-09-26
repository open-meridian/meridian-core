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
use meridian_domain::v1::first_run_check_request::Answer as CheckAnswer;
use meridian_domain::v1::login_backend_answer::Backend;
use meridian_domain::v1::{
    administrator_answer::Named, AddressesAnswer, AdministratorAnswer, BroughtDatabase,
    ClaimCodePurpose, DatabaseLogin, EnrolWithCodeRequest, EnrolmentState, EnrolmentStateRequest,
    FirstRunApplied, FirstRunCheckReply, FirstRunCheckRequest, FirstRunConfiguration,
    FirstRunSealingKey, FirstRunSealingKeyRequest, LdapDirectoryAnswer, LocalAccountAnswer,
    LoginBackendAnswer, OidcProviderAnswer, RedeemClaimCodeReply, RedeemClaimCodeRequest,
    RuntimeDatabaseAnswer, SealedCredential,
};
use prost::Message;
use std::collections::HashMap;
use std::time::Duration;

use crate::html::{escape, page};
use crate::session::{token, ABSOLUTE_NS};
use crate::web::{cookie, set_cookie, App};

pub const ENROLMENT_STATE: &str = "platform.config.query.enrolment";
pub const REDEEM_CLAIM_CODE: &str = "platform.config.command.redeem-claim-code";
pub const ENROL_WITH_CODE: &str = "platform.config.command.enrol-with-code";
pub const SEALING_KEY: &str = "platform.config.query.first-run-sealing-key";
pub const CHECK_ANSWER: &str = "platform.config.query.check-first-run-answer";
pub const APPLY: &str = "platform.config.command.apply-first-run-configuration";
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
}

#[derive(Default)]
pub struct WizardSession(Mutex<Option<Wizard>>);

impl WizardSession {
    pub fn start(&self, now_ns: i64) -> String {
        let wizard = Wizard {
            token: token(),
            started_at_ns: now_ns,
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
        .route("/first-run/enrol", post(enrol))
        .route("/first-run/claim", post(claim))
        .route("/first-run/check", post(check))
        .route("/first-run/apply", post(apply))
}

/// What the wizard answers before a code is redeemed, and after.
async fn first_page(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !app.first_run {
        return not_here();
    }

    let now = app.clock.now_ns();
    let held = app
        .wizard
        .of(cookie(&app, &headers, WIZARD_COOKIE).as_deref(), now);

    let enrolment = enrolment_state(&app).await;
    match held {
        None => Html(closed_page(&enrolment, "")).into_response(),
        Some(_) => Html(open_page(&Fields::new(), &[], "")).into_response(),
    }
}

/// W7.2. Hand the conductor an enrolment code somebody entered.
///
/// The one first-run endpoint that answers before a claim code is redeemed,
/// and it must be: a deployment that never enrolled cannot redeem a claim
/// code at all, because redemption is a signed call and it has no key to sign
/// with. Requirement 9 promised this page would take a new code; until this
/// existed it told the reader to supply one and offered nowhere to put it.
async fn enrol(
    State(app): State<Arc<App>>,
    Form(fields): Form<HashMap<String, String>>,
) -> Response {
    if !app.first_run {
        return not_here();
    }

    let code = fields.get("code").map(String::as_str).unwrap_or_default();
    let request = EnrolWithCodeRequest {
        code: code.trim().to_string(),
    };

    let answered = app
        .bus
        .call(
            ENROL_WITH_CODE,
            "meridian.v1.EnrolWithCodeRequest",
            request.encode_to_vec(),
            None,
            None,
        )
        .await;

    match answered {
        Ok((_, payload)) => {
            let state = EnrolmentState::decode(&payload[..]).unwrap_or_default();
            let refusal = match state.enrolled {
                true => String::new(),
                false => format!("That enrolment code was refused: {}.", state.refusal_reason),
            };
            Html(closed_page(&state, &refusal)).into_response()
        }
        // The conductor is what holds the key and what enrols. Unreachable is
        // not a refused code, and saying so is the difference between issuing
        // another code and waiting a moment.
        Err(failed) => {
            let enrolment = enrolment_state(&app).await;
            Html(closed_page(
                &enrolment,
                &format!("The deployment could not be asked to enrol: {failed}"),
            ))
            .into_response()
        }
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

    // The reply still carries a first administrator's code, and this ignores
    // it: who administers a deployment is now named in the wizard and written
    // when the configuration is applied (ruling 3, amended). The code remains
    // the way a deployment left with no administrator is recovered, issued on
    // the platform when somebody asks for one.
    let key = app.wizard.start(app.clock.now_ns());

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
             <p>Issue another enrolment code on the platform and enter it \
             here. Nothing needs reinstalling.</p>\
             <form method=\"post\" action=\"/first-run/enrol\">\
             <label for=\"enrolment-code\">Enrolment code</label>\
             <input id=\"enrolment-code\" name=\"code\" autocomplete=\"off\" required>\
             <button type=\"submit\">Enrol</button>\
             </form>\
             <details><summary>Or register this deployment's key by hand</summary>\
             <p>On the platform, register the deployment and add this key to \
             it. It is the public half; the private half was made in this \
             cluster and has never left it.</p>\
             <pre>{}</pre></details>",
            escape(&state.deployment_id),
            escape(if state.refusal_reason.is_empty() {
                "no reason given"
            } else {
                &state.refusal_reason
            }),
            escape(&state.public_key_pem)
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

/// W7.4. Test what has been filled in, and write nothing.
async fn check(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let Some(_) = live(&app, &headers) else {
        return closed(&app, "").await;
    };

    let requests = match answers(&app, &fields).await {
        Err(refusal) => {
            return Html(open_page(&fields, &[(Step::Apply, refusal)], "")).into_response()
        }
        Ok(answers) => answers.to_check(),
    };

    // Every answer, one request each, because a check request carries one.
    // Until 2026-09-25 this sent the database alone, so a wrong directory
    // password read "passes" here and was refused only by Apply -- which
    // re-checks all four, and is where the cluster run found it.
    // In the order `to_check` asks them, each shown at its own step.
    let asked = [
        Step::Database,
        Step::SigningIn,
        Step::Address,
        Step::Administrators,
    ];
    // On the local route the account is the administrator and there is no
    // step of that name: what is wrong with it is shown where it is typed.
    let local = fields.get("backend").map(String::as_str) == Some("local");
    let mut findings = Vec::new();
    for (step, request) in asked.into_iter().zip(requests) {
        let step = match step {
            Step::Administrators if local => Step::SigningIn,
            other => other,
        };
        match ask(
            &app,
            CHECK_ANSWER,
            "meridian.v1.FirstRunCheckRequest",
            request.encode_to_vec(),
            TESTING,
        )
        .await
        {
            Err(failed) => findings.push((Step::Apply, failed)),
            Ok(payload) => {
                let reply = FirstRunCheckReply::decode(&payload[..]).unwrap_or_default();
                if !reply.passed {
                    findings.extend(reply.findings.into_iter().map(|finding| (step, finding)));
                }
            }
        }
    }

    let passed = if findings.is_empty() {
        "Everything answered so far passes. Nothing has been written."
    } else {
        ""
    };
    Html(open_page(&fields, &findings, passed)).into_response()
}

/// W7.5. Apply everything at once, and show the first administrator's code.
async fn apply(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Form(fields): Form<Fields>,
) -> Response {
    let Some(wizard) = live(&app, &headers) else {
        return closed(&app, "").await;
    };

    let configuration = match answers(&app, &fields).await {
        Err(refusal) => {
            return Html(open_page(&fields, &[(Step::Apply, refusal)], "")).into_response()
        }
        Ok(answers) => answers.to_configuration(),
    };

    let applied = match ask(
        &app,
        APPLY,
        "meridian.v1.FirstRunConfiguration",
        configuration.encode_to_vec(),
        APPLYING,
    )
    .await
    {
        Err(failed) => {
            return Html(open_page(&fields, &[(Step::Apply, failed)], "")).into_response()
        }
        Ok(payload) => FirstRunApplied::decode(&payload[..]).unwrap_or_default(),
    };

    if !applied.applied {
        let mut findings = vec![applied.refusal_reason.clone()];
        findings.retain(|finding| !finding.is_empty());
        if applied.steps.is_empty() {
            findings.push("Nothing was written.".into());
        } else {
            findings.push(format!(
                "Applied so far: {}. Applying again completes it.",
                applied.steps.join(", ")
            ));
        }
        let findings: Vec<(Step, String)> = findings
            .into_iter()
            .map(|finding| (Step::Apply, finding))
            .collect();
        return Html(open_page(&fields, &findings, "")).into_response();
    }

    app.wizard.end();
    let _ = wizard;
    Html(applied_page(
        &configuration.administrator.clone().unwrap_or_default(),
        &applied.steps,
    ))
    .into_response()
}

fn live(app: &Arc<App>, headers: &HeaderMap) -> Option<Wizard> {
    if !app.first_run {
        return None;
    }
    app.wizard.of(
        cookie(app, headers, WIZARD_COOKIE).as_deref(),
        app.clock.now_ns(),
    )
}

async fn closed(app: &Arc<App>, refusal: &str) -> Response {
    if !app.first_run {
        return not_here();
    }
    let enrolment = enrolment_state(app).await;
    Html(closed_page(&enrolment, refusal)).into_response()
}

/// One request to the first-run Job.
/// How long the Job is given, per question, and why it is not the bus default.
///
/// The default is five seconds, which is right for a question a component
/// answers from memory and wrong for every question on this page. Applying
/// writes three Secrets, may start the database this chart brings and make
/// its roles, restarts two Deployments and deletes a RoleBinding: round trips
/// to a cluster's API server and a database that may be starting. Testing opens a database
/// connection, and sometimes an LDAP one, over a network somebody has just
/// described for the first time.
///
/// Getting this wrong is not a slow page. The Job finishes the work and the
/// dashboard stops listening, so the deployment is configured, the wizard
/// says the Job did not answer, and the first administrator's code -- of
/// which the platform keeps only a hash -- is never shown. That happened,
/// intermittently, and was recorded in the end-to-end test as a flake for a
/// day before it was read properly.
const APPLYING: Duration = Duration::from_secs(120);
const TESTING: Duration = Duration::from_secs(60);

async fn ask(
    app: &Arc<App>,
    topic: &str,
    payload_type: &str,
    payload: Vec<u8>,
    patience: Duration,
) -> Result<Vec<u8>, String> {
    app.bus
        .call(topic, payload_type, payload, None, Some(patience))
        .await
        .map(|(_, payload)| payload)
        .map_err(|failed| {
            format!(
                "the first-run job did not answer within {}s: {failed}. \
                 It may have finished anyway -- every step it takes is \
                 idempotent, so applying again completes whatever it did not.",
                patience.as_secs()
            )
        })
}

/// What the wizard shows once everything is applied.
///
/// It used to show a code to redeem. Nobody redeems anything now: applying
/// recorded who administers this deployment, and the conductor writes the
/// permission when it restarts onto the store it was just given (W7.6).
fn applied_page(administrator: &AdministratorAnswer, steps: &[String]) -> String {
    let who = match &administrator.named {
        Some(Named::DirectoryGroup(group)) => format!(
            "<p>Everybody in <strong>{}</strong> administers this deployment. \
             Sign in through the directory you configured; there is nothing to \
             redeem.</p>\
             <p>Adding an administrator later is a change in your directory \
             rather than here.</p>",
            escape(group)
        ),
        Some(Named::LocalAccountLogin(login)) => format!(
            "<p><strong>{}</strong> administers this deployment. Sign in with \
             the account and password you just gave; there is nothing to \
             redeem.</p>",
            escape(login)
        ),
        // Refused before anything was written, so this page is not reached
        // with nobody named. Said rather than left blank, because a page that
        // shows nothing here is a deployment somebody cannot get into.
        None => "<p class=\"refusal\">Nobody was named to administer this \
                 deployment. Issue a claim code on the platform and redeem it \
                 at the first sign-in.</p>"
            .to_string(),
    };

    page(
        "This deployment is configured",
        &format!(
            "<h1>This deployment is configured</h1>\
             <p>{}</p>\
             <h2>Signing in</h2>\
             {who}\
             <p>The components are restarting into what you configured. This \
             page will not come back.</p>",
            escape(&steps.join(", "))
        ),
    )
}

// ── What the form says, sealed to the Job ────────────────────────────────────

type Fields = HashMap<String, String>;

/// The wizard's answers, with every credential already sealed.
///
/// Built per request rather than kept between them: the browser carries what
/// was typed, and a dashboard that held database passwords between requests
/// would be holding them in the one component every browser in the firm can
/// reach.
struct Answers {
    database: RuntimeDatabaseAnswer,
    backend: LoginBackendAnswer,
    addresses: AddressesAnswer,
    administrator: AdministratorAnswer,
}

impl Answers {
    /// What Check tests: everything Apply will test first, in its order.
    fn to_check(&self) -> Vec<FirstRunCheckRequest> {
        [
            CheckAnswer::RuntimeDatabase(self.database.clone()),
            CheckAnswer::LoginBackend(self.backend.clone()),
            CheckAnswer::Addresses(self.addresses.clone()),
            CheckAnswer::Administrator(self.administrator.clone()),
        ]
        .into_iter()
        .map(|answer| FirstRunCheckRequest {
            answer: Some(answer),
        })
        .collect()
    }

    fn to_configuration(&self) -> FirstRunConfiguration {
        FirstRunConfiguration {
            runtime_database: Some(self.database.clone()),
            login_backend: Some(self.backend.clone()),
            addresses: Some(self.addresses.clone()),
            administrator: Some(self.administrator.clone()),
        }
    }
}

/// Read the form, and seal each credential to the Job that will open it.
///
/// The key is asked for on every request. A Job that has restarted has a new
/// one, and the seal names which one it used, so the alternative to asking is
/// a credential the Job cannot open and a wizard that cannot say why.
async fn answers(app: &Arc<App>, fields: &Fields) -> Result<Answers, String> {
    let key = ask(
        app,
        SEALING_KEY,
        "meridian.v1.FirstRunSealingKeyRequest",
        FirstRunSealingKeyRequest {}.encode_to_vec(),
        // Answered from memory: the Job made this key when it started.
        TESTING,
    )
    .await?;
    let key = FirstRunSealingKey::decode(&key[..])
        .map_err(|failed| format!("the first-run job's key is unreadable: {failed}"))?;

    let seal_field = |field: &str, secret: &str| {
        meridian_first_run::seal(&key.public_key, &key.key_id, field, secret.as_bytes())
    };

    let field = |name: &str| fields.get(name).cloned().unwrap_or_default();
    let login = |prefix: &str, role: &str, password: SealedCredential| DatabaseLogin {
        host: field(&format!("{prefix}_host")),
        port: field(&format!("{prefix}_port")).parse().unwrap_or(5432),
        database: field(&format!("{prefix}_name")),
        role: role.to_string(),
        password: Some(password),
        ssl_mode: field(&format!("{prefix}_sslmode")),
    };

    // Two routes, and the one that brings a database carries no credential at
    // all: its passwords are generated in the cluster and the Job reads them
    // from its own environment, so there is nothing here to seal and nothing
    // for anybody to type, lose or reuse.
    let database = match field("db_route").as_str() {
        "brought" => RuntimeDatabaseAnswer {
            serving: None,
            migrating: None,
            brought: Some(BroughtDatabase {
                serving_role: field("db_serving_role"),
                migrating_role: field("db_migrating_role"),
                database: field("db_name"),
            }),
        },
        _ => RuntimeDatabaseAnswer {
            serving: Some(login(
                "db",
                &field("db_serving_role"),
                seal_field(
                    "runtime_database.serving.password",
                    &field("db_serving_password"),
                )?,
            )),
            migrating: Some(login(
                "db",
                &field("db_migrating_role"),
                seal_field(
                    "runtime_database.migrating.password",
                    &field("db_migrating_password"),
                )?,
            )),
            brought: None,
        },
    };

    // One of three, and only what the firm has is asked about (decisions/018).
    // Anything else is refused rather than read as one of them: a form that
    // arrived without a choice has nothing true to default to.
    let backend = match field("backend").as_str() {
        // The firm's own provider. Nothing of ours signs anybody in.
        "oidc" => Backend::Oidc(OidcProviderAnswer {
            issuer: field("oidc_issuer"),
            client_id: field("oidc_client_id"),
            client_secret: match field("oidc_client_secret").as_str() {
                "" => None,
                secret => Some(seal_field("oidc.client_secret", secret)?),
            },
            groups_claim: field("oidc_groups_claim"),
            trusted_audiences: split(&field("oidc_trusted_audiences")),
        }),
        // The firm's LDAP, which the dashboard binds to itself.
        "ldap" => Backend::Ldap(LdapDirectoryAnswer {
            servers: split(&field("ldap_servers")),
            start_tls: field("ldap_start_tls") == "on",
            base_dn: field("ldap_base_dn"),
            bind_dn: field("ldap_bind_dn"),
            bind_password: Some(seal_field(
                "ldap.bind_password",
                &field("ldap_bind_password"),
            )?),
            user_filter: field("ldap_user_filter"),
        }),
        // A firm with no directory of its own: the deployment holds the
        // account, and first run makes it.
        "local" => Backend::LocalAccount(LocalAccountAnswer {
            login_name: field("admin_login"),
            email: field("admin_email"),
            given_name: field("admin_given_name"),
            family_name: field("admin_family_name"),
            initial_password: Some(seal_field(
                "local_account.initial_password",
                &field("admin_password"),
            )?),
        }),
        other => {
            return Err(format!(
                "choose how people sign in: {other:?} is not one of the three ways"
            ))
        }
    };
    let backend = LoginBackendAnswer {
        backend: Some(backend),
    };

    Ok(Answers {
        database,
        backend,
        addresses: AddressesAnswer {
            dashboard_url: field("dashboard_url"),
        },
        administrator: administrator(fields),
    })
}

/// Who administers this deployment once it is configured (W7.5).
///
/// One of two, and the form offers exactly the one that applies: where the
/// deployment holds the account, that account is the administrator and there
/// is nothing to ask twice; otherwise a directory group, because a
/// person cannot be named before they have signed in once -- a login is
/// matched against the issuer and subject joined, which nobody knows in
/// advance (`design/naming-a-person-before-they-sign-in`).
fn administrator(fields: &Fields) -> AdministratorAnswer {
    let field = |name: &str| {
        fields
            .get(name)
            .map(String::as_str)
            .unwrap_or_default()
            .trim()
    };

    // The local account route names itself. The wizard is already asking for
    // that login and password on this page, and asking again for the same
    // fact is how two answers come to disagree.
    // Named by the way people sign in, never by which field was filled: an
    // empty login on the local route used to fall through to the directory
    // group, and the wizard reported an empty *group* to somebody who had
    // been told to leave it empty.
    let named = match field("backend") {
        "local" => Named::LocalAccountLogin(field("admin_login").to_string()),
        _ => Named::DirectoryGroup(field("admin_group").to_string()),
    };

    AdministratorAnswer { named: Some(named) }
}

fn split(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// Which step of the wizard a finding is about, so that it is shown there
/// rather than in a list at the top that the person has to match up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Database,
    SigningIn,
    Administrators,
    Address,
    Apply,
}

impl Step {
    const ALL: [Step; 5] = [
        Step::Database,
        Step::SigningIn,
        Step::Administrators,
        Step::Address,
        Step::Apply,
    ];

    fn id(self) -> &'static str {
        match self {
            Step::Database => "database",
            Step::SigningIn => "signing-in",
            Step::Administrators => "administrators",
            Step::Address => "address",
            Step::Apply => "apply",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Step::Database => "Database",
            Step::SigningIn => "Signing in",
            Step::Administrators => "Administrators",
            Step::Address => "Address",
            Step::Apply => "Review and apply",
        }
    }
}

/// The wizard itself: one form, shown a step at a time, tested and then
/// applied (spec/installation-and-first-run, ruling 9).
///
/// One form rather than a page per step, because the browser is what holds
/// the answers between the steps, the test and the apply. Nothing is kept
/// here, and a password typed in is sealed on its way out and forgotten.
/// The steps are the page's script showing one section at a time; without
/// it every section shows, as the whole form. Fields a choice makes
/// irrelevant are hidden, never removed: the form keeps every field under
/// its name, which is what `meridian up --params` reads it by.
///
/// What else reads this page: each finding as a bare `<li>`, and nothing
/// else in one; `class="passed"` when a test passes. Both are kept.
fn open_page(fields: &Fields, findings: &[(Step, String)], passed: &str) -> String {
    // A first visit gets the defaults; a page re-rendered after a test keeps
    // exactly what was sent, even a field somebody emptied.
    let fresh = fields.is_empty();
    let given = |name: &str, default: &str| -> String {
        match fields.get(name) {
            Some(value) => value.clone(),
            None if fresh => default.to_string(),
            None => String::new(),
        }
    };
    let required = |yes: bool| if yes { " data-required" } else { "" };
    let text = |name: &str, label: &str, placeholder: &str, default: &str, needed: bool| {
        format!(
            "<label>{label}<input name=\"{name}\" value=\"{}\" placeholder=\"{}\"{}></label>",
            escape(&given(name, default)),
            escape(placeholder),
            required(needed)
        )
    };
    // A password is never rendered back: what the browser holds, it re-posts.
    let secret = |name: &str, label: &str, needed: bool| {
        format!(
            "<label>{label}<input type=\"password\" name=\"{name}\" autocomplete=\"off\"{}></label>",
            required(needed)
        )
    };
    // A choice keeps what was chosen. A page re-rendered after a failed test
    // used to show every select at its first option, so correcting a typo
    // and testing again quietly switched the database route or the way
    // people sign in back to the default.
    let choice = |name: &str, label: &str, default: &str, options: &[(&str, &str)]| {
        let chosen = given(name, default);
        format!(
            "<label>{label}<select name=\"{name}\">{}</select></label>",
            options
                .iter()
                .map(|(option, said)| format!(
                    "<option value=\"{option}\"{}>{}</option>",
                    if *option == chosen { " selected" } else { "" },
                    escape(said)
                ))
                .collect::<String>()
        )
    };
    let flag = |name: &str, label: &str| {
        format!(
            "<label><input type=\"checkbox\" name=\"{name}\" value=\"on\"{}> {label}</label>",
            if fields.get(name).map(String::as_str) == Some("on") {
                " checked"
            } else {
                ""
            }
        )
    };
    // Shown only while `field` is (or is not) `value`.
    let when = |field: &str, value: &str, inner: String| {
        format!("<div data-when=\"{field}\" data-is=\"{value}\">{inner}</div>")
    };
    let unless = |field: &str, value: &str, inner: String| {
        format!("<div data-when=\"{field}\" data-not=\"{value}\">{inner}</div>")
    };

    // Once each: the two database logins fail alike when the host is wrong.
    let mut said: Vec<(Step, String)> = Vec::new();
    for (step, finding) in findings {
        if !said.iter().any(|(_, already)| already == finding) {
            said.push((*step, finding.clone()));
        }
    }
    let found_in = |step: Step| -> String {
        let mine: String = said
            .iter()
            .filter(|(at, _)| *at == step)
            .map(|(_, finding)| format!("<li>{}</li>", escape(finding)))
            .collect();
        if mine.is_empty() {
            String::new()
        } else {
            format!("<ul class=\"refusal\">{mine}</ul>")
        }
    };
    let next = "<button type=\"button\" class=\"primary\" data-next>Next</button>";
    let back = "<button type=\"button\" data-back>Back</button>";
    let section = |step: Step, number: usize, inner: String, buttons: String| {
        format!(
            "<section class=\"step\" id=\"{id}\"><h2 data-title=\"{title}\">{number}. {title}</h2>\
             {found}{inner}<div>{buttons}</div></section>",
            id = step.id(),
            title = step.title(),
            found = found_in(step),
        )
    };

    let database = [
        choice(
            "db_route",
            "Where its database is",
            "brought",
            &[
                ("brought", "Start one inside this cluster"),
                ("external", "Use a database you already run"),
            ],
        ),
        when(
            "db_route",
            "brought",
            "<p class=\"hint\">For trying Meridian and for development. Nothing is \
             asked of you: it is started here, and its roles and passwords are made \
             here. It keeps its data if Meridian is removed and installed again. It \
             loses everything if this cluster is deleted. Nobody backs it up.</p>"
                .to_string(),
        ),
        when(
            "db_route",
            "external",
            [
                "<p class=\"hint\">Your own Postgres, one in Docker, or a managed one \
                 from your cloud: what anything you depend on should use. It needs two \
                 roles: the migrating role may create a table and the serving role must \
                 not. Both are tested before anything is written.</p>"
                    .to_string(),
                text("db_host", "Host", "postgres.firm.internal", "", true),
                text("db_port", "Port", "5432", "5432", false),
                text("db_name", "Database", "meridian", "meridian", true),
                text("db_sslmode", "TLS mode", "verify-full", "", false),
                text(
                    "db_serving_role",
                    "Serving role",
                    "meridian_app",
                    "meridian_app",
                    true,
                ),
                secret("db_serving_password", "Serving role's password", true),
                text(
                    "db_migrating_role",
                    "Migrating role",
                    "meridian_migrate",
                    "meridian_migrate",
                    true,
                ),
                secret("db_migrating_password", "Migrating role's password", true),
            ]
            .concat(),
        ),
    ]
    .concat();

    // One question, three answers, and each asks only about what the firm
    // has. Nothing here names software the firm did not choose.
    let signing_in = [
        "<p class=\"hint\">Connect what your firm already has, or, if it has nothing, \
         let this deployment hold an account for you.</p>"
            .to_string(),
        choice(
            "backend",
            "How people sign in",
            "local",
            &[
                ("local", "We have no directory: make me an account"),
                ("ldap", "Our LDAP or Active Directory"),
                ("oidc", "Our own OpenID Connect provider"),
            ],
        ),
        when(
            "backend",
            "local",
            [
                "<p class=\"hint\">This account is the deployment's administrator.</p>".to_string(),
                text("admin_login", "Your login name", "", "", true),
                text("admin_email", "Your email", "", "", false),
                text("admin_given_name", "Given name", "", "", false),
                text("admin_family_name", "Family name", "", "", false),
                secret("admin_password", "Your password", true),
            ]
            .concat(),
        ),
        when(
            "backend",
            "ldap",
            [
                text(
                    "ldap_servers",
                    "Servers, in order",
                    "ldaps://ldap.firm.internal:636",
                    "",
                    true,
                ),
                flag(
                    "ldap_start_tls",
                    "Use StartTLS (for an ldap:// address; an ldaps:// one is already encrypted)",
                ),
                text(
                    "ldap_base_dn",
                    "Where people are",
                    "ou=people,dc=firm,dc=internal",
                    "",
                    true,
                ),
                text(
                    "ldap_bind_dn",
                    "Account this deployment searches as",
                    "",
                    "",
                    false,
                ),
                secret("ldap_bind_password", "Its password", false),
                text(
                    "ldap_user_filter",
                    "How a person is found ({} is the name typed)",
                    "(uid={})",
                    "",
                    false,
                ),
            ]
            .concat(),
        ),
        when(
            "backend",
            "oidc",
            [
                text(
                    "oidc_issuer",
                    "Issuer, exactly as the provider states it",
                    "https://login.firm.example",
                    "",
                    true,
                ),
                text("oidc_client_id", "Client id", "", "", true),
                secret(
                    "oidc_client_secret",
                    "Client secret (none for a public client)",
                    false,
                ),
                text("oidc_groups_claim", "Groups claim", "groups", "", false),
                text(
                    "oidc_trusted_audiences",
                    "Other audiences a token may name, comma-separated (most providers need none)",
                    "",
                    "",
                    false,
                ),
            ]
            .concat(),
        ),
    ]
    .concat();

    let administrators = [
        when(
            "backend",
            "local",
            "<p class=\"hint\">The account made in the step before administers this \
             deployment: nothing to name here.</p>"
                .to_string(),
        ),
        unless(
            "backend",
            "local",
            [
                "<p class=\"hint\">Name a group in your directory: its members hold \
                 deployment admin, and adding somebody later is a change in your \
                 directory rather than here.</p>\
                 <p class=\"warn\"><strong>The group is not checked.</strong> A directory \
                 states a person's groups when they sign in; it is not asked to list them. \
                 Spell it carefully: a group that does not exist is a deployment nobody \
                 can administer, and getting back in then means a claim code from the \
                 platform.</p>"
                    .to_string(),
                text(
                    "admin_group",
                    "Administrators' directory group",
                    "meridian-admins",
                    "",
                    true,
                ),
            ]
            .concat(),
        ),
    ]
    .concat();

    let address = [
        "<p class=\"hint\">Where a browser reaches this dashboard. People are sent back \
         to it after signing in, and each plugin's page is served on a name below it.</p>"
            .to_string(),
        text(
            "dashboard_url",
            "This dashboard",
            "https://meridian.firm.example",
            "",
            true,
        ),
        "<p class=\"warn\" id=\"address-warning\" hidden>That is an IP address, and an \
         address has no names below it, so this deployment could serve no plugin pages. \
         Give it a name: on one machine, <code>http://localhost</code> with the same \
         port.</p>"
            .to_string(),
    ]
    .concat();

    // What Test found in the other steps, with the way back to each.
    let elsewhere: Vec<String> = Step::ALL
        .iter()
        .filter(|step| **step != Step::Apply && said.iter().any(|(at, _)| at == *step))
        .map(|step| {
            format!(
                "<a href=\"#{id}\" data-go=\"{id}\">{title}</a>",
                id = step.id(),
                title = step.title()
            )
        })
        .collect();
    let told = if !elsewhere.is_empty() {
        format!(
            "<p class=\"refused\">Test found something to fix in {}.</p>",
            elsewhere.join(", ")
        )
    } else if said.is_empty() && !passed.is_empty() {
        format!("<p class=\"passed\">{}</p>", escape(passed))
    } else {
        String::new()
    };
    let apply = format!(
        "{told}<div class=\"panel\" id=\"review\"><p class=\"hint\">Your answers, \
         without their passwords, show here.</p></div>\
         <p class=\"hint\">Test as often as you like: nothing is written until you \
         apply. Applying writes it all at once and restarts what changed. When it is \
         done, the administrator signs in the way you chose; nothing else is \
         redeemed.</p>"
    );

    let nav: String = Step::ALL
        .iter()
        .enumerate()
        .map(|(at, step)| {
            format!(
                "<a href=\"#{id}\" data-go=\"{id}\" data-title=\"{title}\">{n}. {title}</a>",
                id = step.id(),
                title = step.title(),
                n = at + 1
            )
        })
        .collect();

    page(
        "Set up this deployment",
        &format!(
            "<h1>Set up this deployment</h1>\
             <p class=\"hint\">A few short steps. Nothing is written until you apply.</p>\
             <nav class=\"steps\" hidden>{nav}</nav>\
             <form method=\"post\" id=\"wizard\">{}{}{}{}{}</form>\
             <script>{WIZARD_SCRIPT}</script>",
            section(Step::Database, 1, database, next.to_string()),
            section(Step::SigningIn, 2, signing_in, format!("{back}{next}")),
            section(
                Step::Administrators,
                3,
                administrators,
                format!("{back}{next}")
            ),
            section(Step::Address, 4, address, format!("{back}{next}")),
            section(
                Step::Apply,
                5,
                apply,
                format!(
                    "{back}<button type=\"submit\" formaction=\"/first-run/check\">Test</button>\
                     <button type=\"submit\" class=\"primary\" formaction=\"/first-run/apply\" \
                     data-apply>Apply</button>"
                ),
            ),
        ),
    )
}

/// The wizard's steps, in the browser. Static: nothing from a request is in
/// it. Without it the form is one page and every field shows.
const WIZARD_SCRIPT: &str = r#"(function () {
  var form = document.getElementById("wizard");
  if (!form) return;
  form.classList.add("js");
  var nav = document.querySelector("nav.steps");
  nav.hidden = false;
  var steps = Array.prototype.slice.call(form.querySelectorAll("section.step"));
  var current = null;

  function val(name) {
    var field = form.elements[name];
    if (!field) return "";
    if (field.type === "checkbox") return field.checked ? "on" : "";
    return (field.value || "").trim();
  }
  function branches() {
    form.querySelectorAll("[data-when]").forEach(function (part) {
      var value = val(part.dataset.when);
      var on = part.dataset.is !== undefined ? value === part.dataset.is : value !== part.dataset.not;
      part.classList.toggle("off", !on);
    });
  }
  // With an account here, that account is the administrator: no step.
  function skipped(step) { return step.id === "administrators" && val("backend") === "local"; }
  function shown() { return steps.filter(function (step) { return !skipped(step); }); }

  function show(id) {
    branches();
    var list = shown();
    var at = list.findIndex(function (step) { return step.id === id; });
    current = list[at < 0 ? 0 : at];
    steps.forEach(function (step) { step.classList.toggle("current", step === current); });
    list.forEach(function (step, i) {
      var heading = step.querySelector("h2");
      heading.textContent = (i + 1) + ". " + heading.dataset.title;
    });
    nav.querySelectorAll("a").forEach(function (link) {
      var step = document.getElementById(link.dataset.go);
      link.classList.toggle("off", skipped(step));
      link.classList.toggle("here", step === current);
      link.textContent = (list.indexOf(step) + 1) + ". " + link.dataset.title;
    });
    if (current.id === "apply") review();
    window.scrollTo(0, 0);
  }

  // A step's own required answers, checked in the browser before moving on.
  // Only while checking: a required field in a hidden step would stop Test.
  function complete(step) {
    var fine = true;
    step.querySelectorAll("[data-required]").forEach(function (field) {
      if (!fine || field.closest(".off")) return;
      field.required = true;
      if (!field.reportValidity()) fine = false;
      field.required = false;
    });
    return fine;
  }

  function review() {
    var panel = document.getElementById("review");
    panel.textContent = "";
    function line(name, said) {
      var p = document.createElement("p");
      var strong = document.createElement("strong");
      strong.textContent = name + ": ";
      p.appendChild(strong);
      p.appendChild(document.createTextNode(said));
      panel.appendChild(p);
    }
    line("Database", val("db_route") === "brought"
      ? "started inside this cluster"
      : val("db_host") + ":" + (val("db_port") || "5432") + "/" + val("db_name") +
        ", as " + val("db_serving_role") + " and " + val("db_migrating_role"));
    var backend = val("backend");
    line("Signing in", backend === "local" ? "an account here, " + val("admin_login")
      : backend === "ldap" ? "your LDAP, " + val("ldap_servers")
      : "your OpenID Connect provider, " + val("oidc_issuer"));
    line("Administrators", backend === "local" ? val("admin_login") + ", the account above"
      : "members of " + val("admin_group"));
    line("This dashboard", val("dashboard_url"));
  }

  // Apply once a test has passed, and not after an answer has changed since.
  var apply = form.querySelector("[data-apply]");
  var passed = document.querySelector(".passed");
  apply.disabled = !passed;
  function stale() {
    apply.disabled = true;
    if (passed) passed.hidden = true;
  }

  // The dashboard's address, suggested from the one this page was opened
  // at, with `localhost` for a loopback address, since plugin pages need a
  // name to sit below.
  var address = form.elements["dashboard_url"];
  var warning = document.getElementById("address-warning");
  if (address && !address.value) {
    var host = location.hostname;
    if (host === "127.0.0.1" || host === "[::1]" || host === "::1") host = "localhost";
    address.value = location.protocol + "//" + host + (location.port ? ":" + location.port : "");
  }
  function checkAddress() {
    var ip = false;
    try {
      var named = new URL(address.value).hostname;
      ip = /^\d+\.\d+\.\d+\.\d+$/.test(named) || named.indexOf(":") >= 0 || named.charAt(0) === "[";
    } catch (e) { ip = false; }
    warning.hidden = !ip;
  }

  form.addEventListener("click", function (event) {
    var list = shown();
    if (event.target.matches("[data-next]")) {
      event.preventDefault();
      if (complete(current)) show(list[list.indexOf(current) + 1].id);
    } else if (event.target.matches("[data-back]")) {
      event.preventDefault();
      show(list[Math.max(0, list.indexOf(current) - 1)].id);
    }
  });
  document.addEventListener("click", function (event) {
    var link = event.target.closest("[data-go]");
    if (link) { event.preventDefault(); show(link.dataset.go); }
  });
  form.addEventListener("change", function () { branches(); stale(); });
  form.addEventListener("input", function (event) {
    stale();
    if (event.target === address) checkAddress();
  });

  checkAddress();
  // Where to start: at the first step Test found something in, at the
  // review after a test, or at the beginning.
  var troubled = steps.filter(function (step) { return step.querySelector("ul.refusal"); });
  show(troubled.length ? troubled[0].id : passed ? "apply" : steps[0].id);
})();"#;

#[cfg(test)]
#[path = "first_run/tests.rs"]
mod tests;
