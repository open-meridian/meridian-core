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
use meridian_domain::v1::bundled_zitadel_answer::Directory;
use meridian_domain::v1::first_run_check_request::Answer as CheckAnswer;
use meridian_domain::v1::login_backend_answer::Backend;
use meridian_domain::v1::zitadel_database_answer::Route;
use meridian_domain::v1::{
    administrator_answer::Named, AddressesAnswer, AdministratorAnswer, BundledZitadelAnswer,
    ClaimCodePurpose, DatabaseLogin, EnrolWithCodeRequest, EnrolmentState, EnrolmentStateRequest,
    FirstRunApplied, FirstRunCheckReply, FirstRunCheckRequest, FirstRunConfiguration,
    FirstRunSealingKey, FirstRunSealingKeyRequest, LdapDirectoryAnswer, LocalAccountAnswer,
    LoginBackendAnswer, OidcProviderAnswer, RedeemClaimCodeReply, RedeemClaimCodeRequest,
    RuntimeDatabaseAnswer, SealedCredential, ZitadelDatabaseAnswer,
};
use prost::Message;
use std::collections::HashMap;

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
        .of(cookie(&headers, WIZARD_COOKIE).as_deref(), now);

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

    let (request, refusals) = match answers(&app, &fields).await {
        Err(refusal) => return Html(open_page(&fields, &[refusal], "")).into_response(),
        Ok(answers) => answers.to_check(),
    };
    if !refusals.is_empty() {
        return Html(open_page(&fields, &refusals, "")).into_response();
    }

    let findings = match ask(
        &app,
        CHECK_ANSWER,
        "meridian.v1.FirstRunCheckRequest",
        request.encode_to_vec(),
    )
    .await
    {
        Err(failed) => vec![failed],
        Ok(payload) => {
            let reply = FirstRunCheckReply::decode(&payload[..]).unwrap_or_default();
            if reply.passed {
                vec![]
            } else {
                reply.findings
            }
        }
    };

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
        Err(refusal) => return Html(open_page(&fields, &[refusal], "")).into_response(),
        Ok(answers) => answers.to_configuration(),
    };

    let applied = match ask(
        &app,
        APPLY,
        "meridian.v1.FirstRunConfiguration",
        configuration.encode_to_vec(),
    )
    .await
    {
        Err(failed) => return Html(open_page(&fields, &[failed], "")).into_response(),
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
        return Html(open_page(&fields, &findings, "")).into_response();
    }

    // Shown this once and never again: the platform keeps only its hash, and
    // this dashboard is about to restart into directory sign-in.
    app.wizard.end();
    Html(applied_page(&wizard.first_admin_code, &applied.steps)).into_response()
}

fn live(app: &Arc<App>, headers: &HeaderMap) -> Option<Wizard> {
    if !app.first_run {
        return None;
    }
    app.wizard.of(
        cookie(headers, WIZARD_COOKIE).as_deref(),
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
async fn ask(
    app: &Arc<App>,
    topic: &str,
    payload_type: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>, String> {
    app.bus
        .call(topic, payload_type, payload, None, None)
        .await
        .map(|(_, payload)| payload)
        .map_err(|failed| format!("the first-run job did not answer: {failed}"))
}

/// What the wizard shows once everything is applied.
fn applied_page(first_admin_code: &str, steps: &[String]) -> String {
    page(
        "This deployment is configured",
        &format!(
            "<h1>This deployment is configured</h1>\
             <p>{}</p>\
             <h2>Your first administrator's code</h2>\
             <p><code>{}</code></p>\
             <p>Copy it now: it is shown once, and the platform keeps only its \
             hash. Sign in through the directory you configured and redeem it \
             there; that makes you this deployment's first administrator.</p>\
             <p>The components are restarting into what you configured. This \
             page will not come back.</p>",
            escape(&steps.join(", ")),
            escape(first_admin_code)
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
    fn to_check(&self) -> (FirstRunCheckRequest, Vec<String>) {
        (
            FirstRunCheckRequest {
                answer: Some(CheckAnswer::RuntimeDatabase(self.database.clone())),
            },
            Vec::new(),
        )
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

    let database = RuntimeDatabaseAnswer {
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
    };

    let backend = match field("backend").as_str() {
        // The firm's own directory: the bundled Zitadel is not used, and the
        // Job leaves it at zero replicas.
        "oidc" => LoginBackendAnswer {
            backend: Some(Backend::Oidc(OidcProviderAnswer {
                issuer: field("oidc_issuer"),
                client_id: field("oidc_client_id"),
                client_secret: match field("oidc_client_secret").as_str() {
                    "" => None,
                    secret => Some(seal_field("oidc.client_secret", secret)?),
                },
                groups_claim: field("oidc_groups_claim"),
                trusted_audiences: split(&field("oidc_trusted_audiences")),
            })),
        },
        _ => LoginBackendAnswer {
            backend: Some(Backend::Bundled(BundledZitadelAnswer {
                version: field("zitadel_version"),
                database: Some(ZitadelDatabaseAnswer {
                    route: Some(Route::Existing(DatabaseLogin {
                        host: field("zitadel_db_host"),
                        port: field("zitadel_db_port").parse().unwrap_or(5432),
                        database: field("zitadel_db_name"),
                        role: field("zitadel_db_role"),
                        password: Some(seal_field(
                            "zitadel_database.existing.password",
                            &field("zitadel_db_password"),
                        )?),
                        ssl_mode: field("zitadel_db_sslmode"),
                    })),
                }),
                egress_cidrs: split(&field("zitadel_egress")),
                roles: split(&field("zitadel_roles")),
                directory: match field("directory").as_str() {
                    "ldap" => Some(Directory::Ldap(LdapDirectoryAnswer {
                        name: field("ldap_name"),
                        servers: split(&field("ldap_servers")),
                        start_tls: field("ldap_start_tls") == "on",
                        base_dn: field("ldap_base_dn"),
                        bind_dn: field("ldap_bind_dn"),
                        bind_password: Some(seal_field(
                            "ldap.bind_password",
                            &field("ldap_bind_password"),
                        )?),
                        user_object_class: field("ldap_user_object_class"),
                        user_filter: field("ldap_user_filter"),
                    })),
                    // A firm with no directory of its own: the first
                    // administrator gets an account in the bundled Zitadel.
                    _ => Some(Directory::LocalAccount(LocalAccountAnswer {
                        login_name: field("admin_login"),
                        email: field("admin_email"),
                        given_name: field("admin_given_name"),
                        family_name: field("admin_family_name"),
                        initial_password: Some(seal_field(
                            "local_account.initial_password",
                            &field("admin_password"),
                        )?),
                    })),
                },
            })),
        },
    };

    Ok(Answers {
        database,
        backend,
        addresses: AddressesAnswer {
            dashboard_url: field("dashboard_url"),
            zitadel_url: field("zitadel_url"),
        },
        administrator: administrator(fields),
    })
}

/// Who administers this deployment once it is configured (W7.5).
///
/// One of two, and the form offers exactly the one that applies: on the
/// bundled directory with a local account, that account is the administrator
/// and there is nothing to ask twice; otherwise a directory group, because a
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
    let named = match (field("directory"), field("admin_login")) {
        ("local", login) if !login.is_empty() => Named::LocalAccountLogin(login.to_string()),
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

/// The wizard itself: one form, tested and then applied.
///
/// One page rather than a sequence of them, because the browser is what holds
/// the answers between the test and the apply. Nothing is kept here, and a
/// password typed in is sealed on its way out and forgotten.
fn open_page(fields: &Fields, findings: &[String], passed: &str) -> String {
    let value = |name: &str| escape(fields.get(name).map(String::as_str).unwrap_or_default());
    let text = |name: &str, label: &str, placeholder: &str| {
        format!(
            "<label>{label}<input name=\"{name}\" value=\"{}\" placeholder=\"{}\"></label>",
            value(name),
            escape(placeholder)
        )
    };
    // A password is never rendered back: what the browser holds, it re-posts.
    let secret = |name: &str, label: &str| {
        format!(
            "<label>{label}<input type=\"password\" name=\"{name}\" autocomplete=\"off\"></label>"
        )
    };

    let told = if findings.is_empty() {
        if passed.is_empty() {
            String::new()
        } else {
            format!("<p class=\"passed\">{}</p>", escape(passed))
        }
    } else {
        format!(
            "<ul class=\"refusal\">{}</ul>",
            findings
                .iter()
                .map(|finding| format!("<li>{}</li>", escape(finding)))
                .collect::<String>()
        )
    };

    page(
        "Set up this deployment",
        &format!(
            "<h1>Set up this deployment</h1>{told}\
             <form method=\"post\">\
             <h2>Database</h2>\
             <p>Two roles on one database: the migrating role may create a \
             table and the serving role must not. Both are tested before \
             anything is written.</p>\
             {}{}{}{}\
             {}{}\
             {}{}\
             <h2>Signing in</h2>\
             <p>Choose the bundled directory, or connect the firm's own.</p>\
             <label>Backend<select name=\"backend\">\
             <option value=\"bundled\">Bundled (Zitadel in this cluster)</option>\
             <option value=\"oidc\">The firm's own OpenID Connect provider</option>\
             </select></label>\
             {}{}{}\
             {}{}{}{}{}\
             <label>Directory<select name=\"directory\">\
             <option value=\"local\">No directory: make me an account</option>\
             <option value=\"ldap\">Connect the firm's LDAP</option>\
             </select></label>\
             {}{}{}{}\
             {}{}{}{}\
             {}{}{}\
             <h2>Administrators</h2>\
             <p>Who runs this deployment once it is set up. With a directory, \
             name a group: its members hold deployment admin, and adding \
             somebody later is a change in your directory rather than here. \
             With no directory, the account above is the administrator and \
             this is left empty.</p>\
             <p>A group is checked against LDAP and the bundled directory. \
             Against your own OpenID Connect provider it cannot be checked: \
             a provider states a person's groups inside their own token, and \
             listing a directory's groups is a separate interface for every \
             vendor. Spell it carefully there.</p>\
             {}\
             <h2>Addresses</h2>\
             <p>Where a browser reaches this deployment. The directory sends \
             people back to the first of them.</p>\
             {}{}\
             <h2>Apply</h2>\
             <p>Test as often as you like: nothing is written until you apply. \
             Applying writes it all at once and restarts what changed. When \
             it is done, the administrators named above sign in through the \
             directory you configured; nothing else is redeemed.</p>\
             <button type=\"submit\" formaction=\"/first-run/check\">Test</button>\
             <button type=\"submit\" formaction=\"/first-run/apply\">Apply</button>\
             </form>",
            text("db_host", "Host", "postgres.firm.internal"),
            text("db_port", "Port", "5432"),
            text("db_name", "Database", "meridian"),
            text("db_sslmode", "TLS mode", "verify-full"),
            text("db_serving_role", "Serving role", "meridian_app"),
            secret("db_serving_password", "Serving password"),
            text("db_migrating_role", "Migrating role", "meridian_migrate"),
            secret("db_migrating_password", "Migrating password"),
            text("zitadel_version", "Zitadel version", "v4.17.3"),
            text(
                "zitadel_egress",
                "Address ranges Zitadel may reach",
                "10.20.0.0/16"
            ),
            text("zitadel_roles", "Roles Zitadel issues", ""),
            text(
                "zitadel_db_host",
                "Zitadel database host",
                "postgres.firm.internal"
            ),
            text("zitadel_db_port", "Zitadel database port", "5432"),
            text("zitadel_db_name", "Zitadel database", "zitadel"),
            text("zitadel_db_role", "Zitadel database role", "zitadel"),
            secret("zitadel_db_password", "Zitadel database password"),
            text("admin_login", "Your login name", ""),
            text("admin_email", "Your email", ""),
            text("admin_given_name", "Given name", ""),
            secret("admin_password", "Your password"),
            text(
                "ldap_servers",
                "LDAP servers",
                "ldaps://ldap.firm.internal:636"
            ),
            text("ldap_base_dn", "Base DN", "dc=firm,dc=internal"),
            text("ldap_bind_dn", "Bind DN", ""),
            secret("ldap_bind_password", "Bind password"),
            text("oidc_issuer", "Issuer", "https://directory.firm.example"),
            text("oidc_client_id", "Client id", ""),
            secret("oidc_client_secret", "Client secret"),
            text(
                "admin_group",
                "Administrators' directory group",
                "meridian-admins"
            ),
            text(
                "dashboard_url",
                "This dashboard",
                "https://meridian.firm.example"
            ),
            text(
                "zitadel_url",
                "Zitadel, when bundled",
                "https://id.meridian.firm.example"
            ),
        ),
    )
}

#[cfg(test)]
#[path = "first_run/tests.rs"]
mod tests;
