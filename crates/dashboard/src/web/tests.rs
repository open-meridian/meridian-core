use axum::body::{to_bytes, Body};
use axum::http::Request;
use meridian_bus::MemoryBackend;
use meridian_domain::v1::{AccessRecords, Permission, UserGroup};
use tower::ServiceExt;

use super::*;
use crate::records::CEILING_NS;

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

const T0: i64 = 1_790_380_800_000_000_000;
const ADA: &str = "https://directory.example.org|8812";

fn app_with(records: Option<AccessRecords>, read_at: i64, now: i64) -> Arc<App> {
    let cache = Arc::new(RecordsCache::default());
    if let Some(records) = records {
        cache.store(records, read_at);
    }
    Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: cache,
        sessions: Arc::new(Sessions::default()),
        clock: Arc::new(At(now)),
        bus: Arc::new(Bus::single("dashboard-1", Arc::new(MemoryBackend::new()))),
        oidc: None,
        directory: None,
        accounts: None,
        secure_cookies: true,
    })
}

fn admins() -> AccessRecords {
    AccessRecords {
        user_groups: vec![UserGroup {
            user_group_id: "UG-1".into(),
            name: "Admins".into(),
            directory_groups: vec![],
            logins: vec![ADA.into()],
        }],
        permissions: vec![Permission {
            permission_id: "P-1".into(),
            user_group_id: "UG-1".into(),
            account_group_id: String::new(),
            access_group_id: meridian_access::DEPLOYMENT_ADMIN.into(),
        }],
        ..Default::default()
    }
}

async fn get(app: Arc<App>, path: &str, cookie: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    let response = router(app)
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn a_dashboard_with_fresh_records_serves() {
    let app = app_with(Some(AccessRecords::default()), T0, T0);
    assert_eq!(get(app.clone(), "/", None).await.0, StatusCode::OK);
    assert_eq!(get(app, "/healthz", None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_dashboard_past_the_ceiling_refuses_every_page_and_says_so() {
    let stale = || app_with(Some(AccessRecords::default()), T0, T0 + CEILING_NS + 1);
    let (status, body) = get(stale(), "/", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("over 10 minutes"));
    assert_eq!(
        get(stale(), "/healthz", None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        get(stale(), "/sign-in", None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn a_dashboard_that_has_never_read_serves_nothing() {
    assert_eq!(
        get(app_with(None, 0, T0), "/", None).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn without_a_directory_configured_sign_in_says_so() {
    let (status, body) = get(
        app_with(Some(AccessRecords::default()), T0, T0),
        "/sign-in",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("no directory is configured"));
}

#[tokio::test]
async fn a_signed_in_person_sees_what_the_records_give_them_now() {
    let app = app_with(Some(admins()), T0, T0);
    let key = app.sessions.start(ADA, "Ada <Park>", vec![], T0);
    let (status, body) = get(app.clone(), "/", Some(&format!("{SESSION_COOKIE}={key}"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Ada &lt;Park&gt;"), "names are escaped");
    assert!(body.contains("deployment admin"));

    // Withdrawn in the records: the same live session loses it at once,
    // because access is evaluated per request and never kept in a session.
    app.records.store(AccessRecords::default(), T0);
    let (_, after) = get(app, "/", Some(&format!("{SESSION_COOKIE}={key}"))).await;
    assert!(!after.contains("deployment admin"));
}

#[test]
fn cookies_are_http_only_lax_and_secure_over_https() {
    let app = app_with(None, 0, T0);
    let value = set_cookie(&app, SESSION_COOKIE, "k", "/", 60);
    let text = value.to_str().unwrap();
    assert!(text.contains("HttpOnly") && text.contains("SameSite=Lax") && text.contains("Secure"));
}

#[test]
fn a_cookie_is_read_by_name_among_others() {
    let mut headers = HeaderMap::new();
    headers.insert(
        COOKIE,
        HeaderValue::from_static("a=1; meridian_session=abc; b=2"),
    );
    assert_eq!(cookie(&headers, SESSION_COOKIE).as_deref(), Some("abc"));
    assert_eq!(cookie(&headers, "missing"), None);
}

// ── Signing in against a directory this deployment binds itself ────────────
//
// Decision 018. The sign-in page is here rather than at somebody else's
// address, which is what removes the second address and the issuer whose URL
// had to resolve from a pod and a browser at once.

fn app_with_a_directory() -> Arc<App> {
    let app = app_with(Some(admins()), T0, T0);
    let mut built = Arc::try_unwrap(app).ok().expect("one reference");
    built.directory = Some(Arc::new(crate::directory::Directory {
        // No servers: every test here refuses before anything is dialled, and
        // a test that reached the network would be an e2e wearing a disguise.
        base_dn: "ou=people,dc=example,dc=org".into(),
        user_filter: "(uid={})".into(),
        group_attribute: "memberOf".into(),
        ..Default::default()
    }));
    Arc::new(built)
}

async fn body_of(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

#[tokio::test]
async fn sign_in_serves_a_form_rather_than_sending_the_browser_away() {
    let response = router(app_with_a_directory())
        .oneshot(
            Request::builder()
                .uri("/sign-in")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("a response");

    // Not a redirect. There is nowhere to redirect to, and that absence is
    // the point of 018.
    assert_eq!(response.status(), StatusCode::OK);
    let page = body_of(response).await;
    assert!(page.contains("name=\"password\""), "{page}");
    assert!(page.contains("action=\"/sign-in\""), "{page}");
}

#[tokio::test]
async fn a_refused_sign_in_does_not_say_which_half_was_wrong() {
    // An empty password is refused before anything is dialled, which is what
    // lets this exercise the refusal without a server standing up.
    let response = router(app_with_a_directory())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sign-in")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("name=alice&password="))
                .unwrap(),
        )
        .await
        .expect("a response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let page = body_of(response).await;
    assert!(page.contains("were not accepted"), "{page}");
    // Naming the person back would confirm the name exists, which is how a
    // sign-in page becomes a way to enumerate a firm's staff.
    assert!(!page.contains("alice"), "{page}");
}

#[tokio::test]
async fn a_password_post_where_no_directory_is_bound_is_refused() {
    let response = router(app_with(Some(admins()), T0, T0))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sign-in")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("name=alice&password=whatever"))
                .unwrap(),
        )
        .await
        .expect("a response");

    assert_ne!(response.status(), StatusCode::SEE_OTHER);
    let page = body_of(response).await;
    assert!(
        page.contains("does not sign people in with a password"),
        "{page}"
    );
}

// ── And the branch where this deployment holds the account ──────────────────

fn app_holding_ada() -> Arc<App> {
    let accounts = crate::accounts::InMemory::default();
    accounts
        .put(&crate::accounts::LocalAccount {
            name: "ada".into(),
            display_name: "Ada Park".into(),
            password_hash: crate::accounts::hash_password("correct horse battery").expect("hashed"),
            groups: vec!["Admins".into()],
            created_at_ns: T0,
            ..Default::default()
        })
        .expect("stored");

    let app = app_with(Some(admins()), T0, T0);
    let mut built = Arc::try_unwrap(app).ok().expect("one reference");
    built.accounts = Some(Arc::new(accounts));
    Arc::new(built)
}

async fn posting(app: Arc<App>, body: &'static str) -> Response {
    router(app)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/sign-in")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("a response")
}

#[tokio::test]
async fn the_right_password_starts_a_session() {
    let response = posting(app_holding_ada(), "name=ada&password=correct+horse+battery").await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response
        .headers()
        .get(SET_COOKIE)
        .expect("a session cookie")
        .to_str()
        .expect("ascii")
        .to_string();
    assert!(cookie.starts_with(SESSION_COOKIE), "{cookie}");
}

#[tokio::test]
async fn a_locked_account_is_told_so_rather_than_refused() {
    let app = app_holding_ada();
    for _ in 0..crate::accounts::LOCK_AFTER {
        posting(Arc::clone(&app), "name=ada&password=not+it").await;
    }

    // Even with the right password, and said plainly: somebody locked out and
    // not told keeps trying and cannot tell it from a wrong password.
    let response = posting(app, "name=ada&password=correct+horse+battery").await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let page = body_of(response).await;
    assert!(page.contains("Too many attempts"), "{page}");
}

#[tokio::test]
async fn a_wrong_password_here_reads_as_it_does_on_the_other_branch() {
    let response = posting(app_holding_ada(), "name=ada&password=not+it").await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // No session, checked on the headers. An earlier version of this looked
    // for the cookie's name in the page body, where a cookie never appears,
    // so it passed whatever the handler did.
    assert!(
        response.headers().get(SET_COOKIE).is_none(),
        "a refused sign-in set a cookie: {:?}",
        response.headers().get(SET_COOKIE)
    );

    let page = body_of(response).await;
    assert!(page.contains("were not accepted"), "{page}");
}
