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
