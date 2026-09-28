use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::header::{COOKIE, LOCATION, SET_COOKIE};
use axum::http::{Request, StatusCode};
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::AccessRecords;
use tower::ServiceExt;

use meridian_first_run::SealingKey;

use super::*;

thread_local! {
    /// What the fake Job opened, for the test that cares that it could.
    static OPENED: std::cell::RefCell<Option<Arc<std::sync::Mutex<Vec<u8>>>>> =
        const { std::cell::RefCell::new(None) };
}
use crate::clock::Clock;
use crate::records::RecordsCache;
use crate::session::Sessions;
use crate::web::router;

const T0: i64 = 1_790_380_800_000_000_000;
const FINGERPRINT: &str = "SHA256:3b6f0c9d2a41e8f7c5d1a09b4e62f38c7d15a9e0b2c46f81d3e7a95c0b28f614";

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

/// A conductor that answers the two things the wizard asks it.
fn app(first_run: bool, state: EnrolmentState, redemption: RedeemClaimCodeReply) -> Arc<App> {
    // A Job that answers at once, on a bus with its ordinary default.
    app_with(
        first_run,
        state,
        redemption,
        Duration::ZERO,
        DEFAULT_TIMEOUT,
    )
}

/// The bus's own default, restated here so a test can be shorter than it and
/// mean something by that.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

fn app_with(
    first_run: bool,
    state: EnrolmentState,
    redemption: RedeemClaimCodeReply,
    applying: Duration,
    default_timeout: Duration,
) -> Arc<App> {
    let bus = Arc::new(
        Bus::single("dashboard-1", Arc::new(MemoryBackend::new()))
            .with_default_timeout(default_timeout),
    );
    bus.serve(ENROLMENT_STATE, move |_| {
        Ok((
            "meridian.v1.EnrolmentState".to_string(),
            state.encode_to_vec(),
        ))
    });
    // W7.2. A deployment that never enrolled cannot redeem a claim code at
    // all, so this is the one first-run endpoint that answers before one is.
    bus.serve(ENROL_WITH_CODE, move |envelope| {
        let asked = EnrolWithCodeRequest::decode(&envelope.payload[..]).unwrap();
        let answered = match asked.code.as_str() {
            "ENR-GOOD" => enrolled(),
            other => unenrolled(&format!("no such enrolment code: {other}")),
        };
        Ok((
            "meridian.v1.EnrolmentState".to_string(),
            answered.encode_to_vec(),
        ))
    });
    bus.serve(REDEEM_CLAIM_CODE, move |envelope| {
        let asked = RedeemClaimCodeRequest::decode(&envelope.payload[..]).unwrap();
        // The purpose is the point: a code for the first administrator is not
        // a code for the wizard, and the platform refuses the swap.
        assert_eq!(asked.purpose, ClaimCodePurpose::FirstRun as i32);
        Ok((
            "meridian.v1.RedeemClaimCodeReply".to_string(),
            redemption.encode_to_vec(),
        ))
    });

    let job = SealingKey::new("frk-1");
    let public_key = job.public_key();
    bus.serve(SEALING_KEY, move |_| {
        Ok((
            "meridian.v1.FirstRunSealingKey".to_string(),
            meridian_domain::v1::FirstRunSealingKey {
                public_key: public_key.clone(),
                key_id: "frk-1".to_string(),
            }
            .encode_to_vec(),
        ))
    });
    let checking = Arc::new(job);
    let opened = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let seen = Arc::clone(&opened);
    let job = Arc::clone(&checking);
    bus.serve(CHECK_ANSWER, move |envelope| {
        let request =
            meridian_domain::v1::FirstRunCheckRequest::decode(&envelope.payload[..]).unwrap();
        // What the Job can read is what the wizard sealed, and nothing else
        // on the bus can.
        if let Some(meridian_domain::v1::first_run_check_request::Answer::RuntimeDatabase(db)) =
            &request.answer
        {
            let sealed = db.serving.as_ref().unwrap().password.as_ref().unwrap();
            // The payload itself never carries the password in clear.
            assert!(!envelope.payload.windows(7).any(|w| w == b"hunter2"));
            let plain = job
                .open(sealed, "runtime_database.serving.password")
                .unwrap();
            *seen.lock().unwrap() = plain;
        }
        Ok((
            "meridian.v1.FirstRunCheckReply".to_string(),
            meridian_domain::v1::FirstRunCheckReply {
                passed: true,
                findings: vec![],
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(APPLY, move |_| {
        // A Job in a cluster writes Secrets, patches a policy, scales and
        // restarts before it answers. Here that is a sleep.
        std::thread::sleep(applying);
        Ok((
            "meridian.v1.FirstRunApplied".to_string(),
            meridian_domain::v1::FirstRunApplied {
                applied: true,
                steps: vec!["secrets".into(), "restart".into()],
                refusal_reason: String::new(),
                rights_released: true,
            }
            .encode_to_vec(),
        ))
    });
    OPENED.with(|held| *held.borrow_mut() = Some(opened));

    let records = Arc::new(RecordsCache::default());
    records.store(AccessRecords::default(), T0);
    Arc::new(App {
        first_run,
        wizard: Arc::new(WizardSession::default()),
        records,
        sessions: Arc::new(Sessions::default()),
        terminals: Arc::new(crate::terminal::Terminals::default()),
        clock: Arc::new(At(T0)),
        bus,
        oidc: None,
        directory: None,
        accounts: None,
        sign_in_failures: Default::default(),
        secure_cookies: true,
        plugins: None,
        registry: None,
        custody: Arc::default(),
    })
}

fn enrolled() -> EnrolmentState {
    EnrolmentState {
        deployment_id: "DEP-7".into(),
        enrolled: true,
        fingerprint: FINGERPRINT.into(),
        refusal_reason: String::new(),
        public_key_pem: PUBLIC_KEY.into(),
    }
}

/// The public half, as the wizard shows it for the route that registers a key
/// by hand. A public key, so nothing here is a credential.
const PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA\n-----END PUBLIC KEY-----";

async fn get(app: Arc<App>, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    read(app, request.body(Body::empty()).unwrap()).await
}

async fn claim_with(app: Arc<App>, code: &str) -> (StatusCode, String, Option<String>) {
    let request = Request::post("/first-run/claim")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("code={code}")))
        .unwrap();
    let response = router(app).oneshot(request).await.unwrap();
    let status = response.status();
    let cookie = response
        .headers()
        .get(SET_COOKIE)
        .map(|value| value.to_str().unwrap().to_string());
    let location = response
        .headers()
        .get(LOCATION)
        .map(|value| value.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        if body.is_empty() {
            location
        } else {
            String::from_utf8_lossy(&body).into_owned()
        },
        cookie,
    )
}

async fn read(app: Arc<App>, request: Request<Body>) -> (StatusCode, String) {
    let response = router(app).oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn a_configured_deployment_has_no_wizard_at_all() {
    // Not a redirect and not a refusal: a page that answers anything tells
    // somebody it is there, and after first run it is not (requirement 18).
    let app = app(false, enrolled(), RedeemClaimCodeReply::default());
    assert_eq!(
        get(app.clone(), "/first-run", None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        claim_with(app, "FR-9C2L-7TXB-K4QM").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn before_a_code_is_redeemed_the_wizard_shows_its_one_page() {
    let (status, body) = get(
        app(true, enrolled(), RedeemClaimCodeReply::default()),
        "/first-run",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("DEP-7"), "{body}");
    assert!(
        body.contains(FINGERPRINT),
        "the fingerprint is what an administrator compares: {body}"
    );
    assert!(body.contains("first-run/claim"), "{body}");
    // Nothing about the deployment's configuration, because none of it exists
    // and nobody has been let in.
    assert!(!body.contains("database"), "{body}");
}

#[tokio::test]
async fn a_deployment_that_has_not_enrolled_says_why_and_what_to_do() {
    let (_, body) = get(
        app(true, unenrolled("expired"), RedeemClaimCodeReply::default()),
        "/first-run",
        None,
    )
    .await;

    assert!(body.contains("not registered"), "{body}");
    assert!(body.contains("expired"), "{body}");
    assert!(body.contains("Nothing needs reinstalling"), "{body}");

    // Requirement 9 promised a new code fixes this with nothing reinstalled,
    // and for a while the page said so above a form that took a different
    // kind of code entirely.
    assert!(
        body.contains("/first-run/enrol"),
        "somewhere to put one: {body}"
    );

    // And the other way in, for a deployment that cannot reach the platform
    // at all: the public half, to register by hand (decisions/017).
    assert!(body.contains("BEGIN PUBLIC KEY"), "{body}");
}

fn unenrolled(why: &str) -> EnrolmentState {
    EnrolmentState {
        deployment_id: "DEP-7".into(),
        enrolled: false,
        fingerprint: String::new(),
        refusal_reason: why.into(),
        public_key_pem: PUBLIC_KEY.into(),
    }
}

#[tokio::test]
async fn a_new_enrolment_code_repairs_a_deployment_that_never_enrolled() {
    // The whole point of requirement 9: no reinstall, no helm upgrade, and
    // no claim code -- which could not work here anyway, because redeeming
    // one is a signed call and this deployment has no key registered to sign
    // as.
    let app = app(true, unenrolled("expired"), RedeemClaimCodeReply::default());

    let (status, body) = post_form(app, "/first-run/enrol", "code=ENR-GOOD").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(FINGERPRINT),
        "the key is registered now: {body}"
    );
    assert!(
        !body.contains("/first-run/enrol"),
        "and there is nothing left to enter: {body}"
    );
}

#[tokio::test]
async fn a_refused_enrolment_code_says_why_and_keeps_the_field() {
    let app = app(true, unenrolled("expired"), RedeemClaimCodeReply::default());

    let (_, body) = post_form(app, "/first-run/enrol", "code=ENR-WRONG").await;

    assert!(body.contains("no such enrolment code"), "{body}");
    assert!(
        body.contains("/first-run/enrol"),
        "still somewhere to try again: {body}"
    );
}

/// A form post with no session, which is what this endpoint is for.
async fn post_form(app: Arc<App>, path: &str, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .expect("a request");
    read(app, request).await
}

#[tokio::test]
async fn a_refused_code_says_why_and_starts_nothing() {
    let refused = RedeemClaimCodeReply {
        redeemed: false,
        refusal_reason: "issued for another purpose".into(),
        ..Default::default()
    };
    let (status, body, cookie) = claim_with(app(true, enrolled(), refused), "FR-WRONG").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("issued for another purpose"), "{body}");
    assert!(cookie.is_none(), "a refused code starts no session");
}

#[tokio::test]
async fn a_redeemed_code_starts_one_session_bound_to_that_browser() {
    let app = app(
        true,
        enrolled(),
        RedeemClaimCodeReply {
            redeemed: true,
            first_admin_code: "7KQ2-MX4P-9RTD".into(),
            ..Default::default()
        },
    );

    let (status, location, cookie) = claim_with(Arc::clone(&app), "FR-9C2L-7TXB-K4QM").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location, "/first-run");
    let cookie = cookie.expect("a redeemed code starts a session");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("Secure"), "{cookie}");

    let held = app
        .wizard
        .of(
            Some(cookie.split(';').next().unwrap().split('=').nth(1).unwrap()),
            T0,
        )
        .expect("the session is this browser's");
    // The session holds the wizard and nothing else. It used to carry a first
    // administrator's code to show at W7.5; the wizard names the
    // administrator now, and the permission is written when the conductor
    // restarts (decisions/017).
    assert_eq!(held.started_at_ns, T0);

    let (_, body) = get(
        Arc::clone(&app),
        "/first-run",
        Some(cookie.split(';').next().unwrap()),
    )
    .await;
    assert!(
        body.contains("/first-run/apply"),
        "the wizard itself: {body}"
    );
}

#[tokio::test]
async fn another_browser_is_not_the_session() {
    let app = app(
        true,
        enrolled(),
        RedeemClaimCodeReply {
            redeemed: true,
            first_admin_code: "7KQ2-MX4P-9RTD".into(),
            ..Default::default()
        },
    );
    claim_with(Arc::clone(&app), "FR-9C2L-7TXB-K4QM").await;

    let (status, body) = get(app, "/first-run", Some("meridian_first_run=somebody-elses")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("first-run/claim"),
        "the closed page, not the wizard: {body}"
    );
}

#[test]
fn a_session_does_not_outlive_its_bound() {
    let wizard = WizardSession::default();
    let key = wizard.start(T0);

    assert!(wizard.of(Some(&key), T0 + ABSOLUTE_NS).is_some());
    assert!(
        wizard.of(Some(&key), T0 + ABSOLUTE_NS + 1).is_none(),
        "a wizard left open is the exposure a session left open is (decisions/015)"
    );
}

async fn post(app: Arc<App>, path: &str, cookie: &str, body: &str) -> (StatusCode, String) {
    let request = Request::post(path)
        .header("content-type", "application/x-www-form-urlencoded")
        .header(COOKIE, cookie)
        .body(Body::from(body.to_string()))
        .unwrap();
    read(app, request).await
}

async fn redeemed(app: &Arc<App>) -> String {
    let (_, _, cookie) = claim_with(Arc::clone(app), "FR-9C2L-7TXB-K4QM").await;
    cookie
        .expect("a session")
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn applying_waits_for_a_job_slower_than_a_question_from_memory() {
    // The bug this pins reached the end-to-end test as an intermittent
    // failure and was recorded there as a flake for a day. The dashboard
    // asked the Job to apply and waited the bus default of five seconds.
    // Applying writes three Secrets, may start a database and make its
    // roles, restarts two Deployments and deletes a RoleBinding;
    // when that took longer, the Job finished the work and the dashboard
    // stopped listening. The deployment was configured, the wizard said the
    // Job had not answered, and the first administrator's code was never
    // shown -- and the platform keeps only its hash, so it was gone.
    //
    // The bus here gives up in 20ms unless a caller states its own bound, and
    // the Job takes 150ms. Only the caller's bound makes this pass.
    let app = app_with(
        true,
        enrolled(),
        RedeemClaimCodeReply {
            redeemed: true,
            first_admin_code: "7KQ2-MX4P-9RTD".into(),
            ..Default::default()
        },
        Duration::from_millis(150),
        Duration::from_millis(20),
    );
    let cookie = redeemed(&app).await;

    let (status, body) = post(app, "/first-run/apply", &cookie, ANSWERS).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("administers this deployment"),
        "a Job slower than the default still finishes the wizard: {body}"
    );
}

fn wizard_app() -> Arc<App> {
    app(
        true,
        enrolled(),
        RedeemClaimCodeReply {
            redeemed: true,
            first_admin_code: "7KQ2-MX4P-9RTD".into(),
            ..Default::default()
        },
    )
}

const ANSWERS: &str = "db_host=db.firm.internal&db_port=5432&db_name=meridian\
&db_serving_role=meridian_app&db_serving_password=hunter2\
&db_migrating_role=meridian_migrate&db_migrating_password=hunter2\
&backend=local&admin_login=ada&admin_password=hunter2\
&dashboard_url=https://meridian.firm.example";

#[tokio::test]
async fn a_credential_reaches_the_job_sealed_and_nothing_else_can_read_it() {
    let app = wizard_app();
    let cookie = redeemed(&app).await;

    let (status, body) = post(Arc::clone(&app), "/first-run/check", &cookie, ANSWERS).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("passes"), "{body}");
    // The Job opened it; the assertion inside the fake Job is that the bus
    // payload never carried it in clear.
    let opened = OPENED.with(|held| held.borrow().clone()).unwrap();
    let opened = opened.lock().unwrap().clone();
    assert_eq!(opened, b"hunter2");
}

#[tokio::test]
async fn applying_names_the_administrator_and_ends_the_wizard() {
    let app = wizard_app();
    let cookie = redeemed(&app).await;

    let (status, body) = post(Arc::clone(&app), "/first-run/apply", &cookie, ANSWERS).await;

    assert_eq!(status, StatusCode::OK);
    // Nobody redeems anything: applying recorded who administers this
    // deployment, and the conductor writes the permission when it restarts
    // onto the store it was just given (W7.6, decisions/017).
    assert!(body.contains("administers this deployment"), "{body}");
    assert!(!body.contains("7KQ2-MX4P-9RTD"), "no code to copy: {body}");

    // And the session is over: the same cookie now sees the closed page,
    // because what makes first run end is the configuration landing.
    let (_, again) = get(app, "/first-run", Some(&cookie)).await;
    assert!(again.contains("first-run/claim"), "{again}");
}

#[tokio::test]
async fn the_wizard_refuses_anybody_without_a_session() {
    let app = wizard_app();

    let (status, body) = post(
        Arc::clone(&app),
        "/first-run/apply",
        "meridian_first_run=not-a-session",
        ANSWERS,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("first-run/claim"), "the closed page: {body}");
}

#[test]
fn a_page_shown_again_keeps_what_was_chosen() {
    // Re-rendered after a failed test, every select used to fall back to its
    // first option: correcting a typo and testing again quietly switched a
    // firm that chose LDAP to an account held here, and an external database
    // to one started in the cluster.
    let fields: Fields = [
        ("backend".to_string(), "ldap".to_string()),
        ("db_route".to_string(), "brought".to_string()),
        ("ldap_start_tls".to_string(), "on".to_string()),
    ]
    .into_iter()
    .collect();

    let page = open_page(
        &fields,
        &[(Step::Database, "a finding".to_string())],
        "",
        "",
    );

    assert!(page.contains("<option value=\"ldap\" selected>"), "{page}");
    assert!(
        !page.contains("<option value=\"local\" selected>"),
        "{page}"
    );
    assert!(
        page.contains("<option value=\"brought\" selected>"),
        "{page}"
    );
    assert!(
        page.contains("name=\"ldap_start_tls\" value=\"on\" checked"),
        "{page}"
    );
}

#[test]
fn the_page_asks_three_ways_and_names_nothing_the_firm_did_not_choose() {
    let page = open_page(&Fields::default(), &[], "", "");
    for option in ["local", "ldap", "oidc"] {
        assert!(
            page.contains(&format!("<option value=\"{option}\"")),
            "{option}"
        );
    }
    for field in [
        "ldap_start_tls",
        "ldap_user_filter",
        "oidc_trusted_audiences",
        "admin_family_name",
    ] {
        assert!(
            page.contains(&format!("name=\"{field}\"")),
            "{field} is read and never asked for"
        );
    }
    let lower = page.to_lowercase();
    assert!(
        !lower.contains("zitadel") && !lower.contains("bundled"),
        "{page}"
    );
}

/// Every field the form has had, by name: what `meridian up --params` and
/// the runners read it by. Hidden by a choice is still in the form.
const EVERY_FIELD: [&str; 25] = [
    "db_route",
    "db_host",
    "db_port",
    "db_name",
    "db_sslmode",
    "db_serving_role",
    "db_serving_password",
    "db_migrating_role",
    "db_migrating_password",
    "backend",
    "admin_login",
    "admin_email",
    "admin_given_name",
    "admin_family_name",
    "admin_password",
    "ldap_servers",
    "ldap_start_tls",
    "ldap_base_dn",
    "ldap_bind_dn",
    "ldap_bind_password",
    "ldap_user_filter",
    "oidc_issuer",
    "oidc_client_id",
    "oidc_client_secret",
    "oidc_groups_claim",
];

#[test]
fn a_first_visit_starts_a_trial_and_every_field_stays_in_the_form() {
    let page = open_page(&Fields::default(), &[], "", "");
    // Ruling 9: the database started in the cluster, and an account here.
    assert!(
        page.contains("<option value=\"brought\" selected>"),
        "{page}"
    );
    assert!(page.contains("<option value=\"local\" selected>"), "{page}");
    for field in
        EVERY_FIELD
            .iter()
            .chain(&["oidc_trusted_audiences", "admin_group", "dashboard_url"])
    {
        assert!(page.contains(&format!("name=\"{field}\"")), "{field}");
    }
    // Without the script, the steps' navigation is not offered.
    assert!(page.contains("<nav class=\"steps\" hidden>"), "{page}");
}

#[test]
fn the_accounts_password_is_given_and_the_same_twice_before_anything_is_sealed() {
    let fields = |pairs: &[(&str, &str)]| -> Fields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let local = [("backend", "local"), ("admin_login", "ada")];

    let none = account_findings(&fields(&[
        local[0],
        local[1],
        ("admin_password", ""),
        ("admin_password_again", ""),
    ]));
    assert_eq!(none.len(), 1, "{none:?}");
    assert_eq!(none[0].0, Step::SigningIn);
    assert!(none[0].1.contains("type a password"));

    let differ = account_findings(&fields(&[
        local[0],
        local[1],
        ("admin_password", "correct horse battery"),
        ("admin_password_again", "correct horse batery"),
    ]));
    assert_eq!(differ.len(), 1, "{differ:?}");
    assert!(differ[0].1.contains("differ"));

    let same = account_findings(&fields(&[
        local[0],
        local[1],
        ("admin_password", "correct horse battery"),
        ("admin_password_again", "correct horse battery"),
    ]));
    assert!(same.is_empty(), "{same:?}");

    // Answered from a file, it is posted once, and the service holds the rest.
    let once = account_findings(&fields(&[
        local[0],
        local[1],
        ("admin_password", "correct horse battery"),
    ]));
    assert!(once.is_empty(), "{once:?}");
    // Not the local route: nothing of the account's is asked.
    assert!(account_findings(&fields(&[("backend", "ldap")])).is_empty());
}

#[test]
fn the_accounts_password_is_asked_twice_with_its_length_and_every_password_can_be_shown() {
    let page = open_page(&Fields::default(), &[], "", "");
    for name in ["admin_password", "admin_password_again"] {
        let at = page.find(&format!("name=\"{name}\"")).unwrap();
        let end = at + page[at..].find('>').unwrap();
        assert!(
            page[at..end].contains(&format!("minlength=\"{ACCOUNT_PASSWORD_MIN}\"")),
            "{name}"
        );
    }
    let passwords = page.matches("type=\"password\"").count();
    let shows = page.matches("data-reveal hidden>Show</button>").count();
    assert_eq!(passwords, shows, "every password field has its Show button");
    assert!(passwords >= 6, "{passwords}");
}

#[test]
fn a_password_manager_is_offered_the_administrators_login_and_nothing_else() {
    let page = open_page(&Fields::default(), &[], "", "");
    let input = |name: &str| -> String {
        let at = page
            .find(&format!("name=\"{name}\""))
            .unwrap_or_else(|| panic!("{name} is on the page"));
        let start = page[..at].rfind("<input").unwrap();
        let end = at + page[at..].find('>').unwrap();
        page[start..=end].to_string()
    };
    assert!(input("admin_login").contains("autocomplete=\"username\""));
    assert!(input("admin_password").contains("autocomplete=\"new-password\""));
    for somebody_elses in [
        "db_serving_password",
        "db_migrating_password",
        "ldap_bind_password",
        "oidc_client_secret",
    ] {
        let field = input(somebody_elses);
        assert!(
            field.contains("autocomplete=\"off\""),
            "{somebody_elses}: {field}"
        );
    }
}

#[test]
fn a_finding_is_shown_once_at_its_own_step_and_the_way_back_to_it_at_the_review() {
    let findings = [
        (Step::Database, "no host for the database".to_string()),
        (Step::Database, "no host for the database".to_string()),
        (Step::Address, "no address for the dashboard".to_string()),
    ];
    let page = open_page(&Fields::default(), &findings, "", "");
    let at = |id: &str| {
        let open = page
            .find(&format!("<section class=\"step\" id=\"{id}\">"))
            .unwrap();
        let close = open + page[open..].find("</section>").unwrap();
        &page[open..close]
    };
    assert!(
        at("database").contains("<li>no host for the database</li>"),
        "{page}"
    );
    assert_eq!(page.matches("<li>no host for the database</li>").count(), 1);
    assert!(at("address").contains("<li>no address for the dashboard</li>"));
    assert!(!at("signing-in").contains("<li>"));
    // What reads the page reads a finding as a bare `<li>`, and nothing else
    // is one.
    assert_eq!(page.matches("<li>").count(), 2, "{page}");
    let review = at("apply");
    assert!(
        review.contains("data-go=\"database\">Database</a>"),
        "{review}"
    );
    assert!(
        review.contains("data-go=\"address\">Address</a>"),
        "{review}"
    );
    assert!(!review.contains("class=\"passed\""));
}

#[test]
fn a_test_that_passes_says_so_where_it_is_applied() {
    let page = open_page(
        &Fields::default(),
        &[],
        "Everything answered so far passes.",
        "",
    );
    assert!(page.contains("<p class=\"passed\">Everything answered so far passes.</p>"));
    assert!(!page.contains("<li>"));
}

#[test]
fn the_local_account_is_the_administrator_even_before_its_login_is_typed() {
    // An empty login on the local route used to fall through to the
    // directory group, and the wizard said the *group* was empty to somebody
    // who had been told to leave it so.
    let fields: Fields = [
        ("backend".to_string(), "local".to_string()),
        ("admin_login".to_string(), "  ".to_string()),
        ("admin_group".to_string(), "".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        administrator(&fields).named,
        Some(Named::LocalAccountLogin(String::new()))
    );
    let fields: Fields = [
        ("backend".to_string(), "ldap".to_string()),
        ("admin_group".to_string(), "meridian-admins".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        administrator(&fields).named,
        Some(Named::DirectoryGroup("meridian-admins".into()))
    );
}

#[tokio::test]
async fn home_before_setup_points_at_the_wizard_rather_than_a_sign_in() {
    let (status, body) = get(wizard_app(), "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("not set up yet"), "{body}");
    assert!(body.contains("href=\"/first-run\""), "{body}");
    assert!(!body.contains("/sign-in"), "{body}");
}

#[test]
fn the_ingress_address_is_offered_and_what_was_sent_is_kept() {
    // Offered on a first visit: the chart's Ingress knows the name.
    let page = open_page(&Fields::default(), &[], "", "https://meridian.firm.example");
    assert!(
        page.contains("name=\"dashboard_url\" value=\"https://meridian.firm.example\""),
        "{page}"
    );
    // After a test, what the person sent stands, even an empty field.
    let fields: Fields = [("dashboard_url".to_string(), String::new())]
        .into_iter()
        .collect();
    let page = open_page(&fields, &[], "", "https://meridian.firm.example");
    assert!(page.contains("name=\"dashboard_url\" value=\"\""), "{page}");
}
