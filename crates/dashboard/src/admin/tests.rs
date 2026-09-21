use std::sync::Mutex;

use axum::body::{to_bytes, Body};
use axum::http::header::COOKIE;
use axum::http::Request;
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{AccountRecord, Permission, RedeemClaimCodeReply};
use tower::ServiceExt;

use super::*;
use crate::clock::Clock;
use crate::records::RecordsCache;
use crate::session::Sessions;
use crate::web::{router, SESSION_COOKIE};

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

const T0: i64 = 1_790_380_800_000_000_000;
const ADA: &str = "https://directory.example.org|8812";

/// What the fake conductor was asked, and on whose behalf.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

fn admin_records() -> AccessRecords {
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
            access_group_id: DEPLOYMENT_ADMIN.into(),
        }],
        ..Default::default()
    }
}

struct Harness {
    app: Arc<App>,
    seen: Seen,
    session: String,
    form_token: String,
}

/// A dashboard whose records are `records`, with Ada signed in, and a fake
/// conductor answering define-account and redeem-claim-code.
fn harness(records: AccessRecords, refuse_with: Option<&'static str>) -> Harness {
    let bus = Arc::new(Bus::single("dashboard-1", Arc::new(MemoryBackend::new())));
    let seen: Seen = Arc::default();
    for topic in [
        "platform.config.command.define-account",
        "platform.config.command.redeem-claim-code",
    ] {
        let seen = Arc::clone(&seen);
        bus.serve(topic, move |envelope| {
            let meta = envelope.meta.clone().unwrap_or_default();
            seen.lock()
                .unwrap()
                .push((meta.topic.clone(), meta.acting_for_subject.clone()));
            if let Some(sentence) = refuse_with {
                return Err(sentence.to_string());
            }
            if meta.topic.ends_with("redeem-claim-code") {
                return Ok((
                    "".into(),
                    RedeemClaimCodeReply {
                        redeemed: true,
                        refusal_reason: String::new(),
                    }
                    .encode_to_vec(),
                ));
            }
            Ok((
                "".into(),
                AccountRecord {
                    account_id: "ACC-1".into(),
                    ..Default::default()
                }
                .encode_to_vec(),
            ))
        });
    }
    let cache = Arc::new(RecordsCache::default());
    cache.store(records, T0);
    let sessions = Arc::new(Sessions::default());
    let session = sessions.start(ADA, "Ada", vec![], T0);
    let form_token = sessions.find(&session, T0).unwrap().form_token;
    let app = Arc::new(App {
        records: cache,
        sessions,
        clock: Arc::new(At(T0)),
        bus,
        oidc: None,
        secure_cookies: true,
    });
    Harness {
        app,
        seen,
        session,
        form_token,
    }
}

async fn send(h: &Harness, request: Request<Body>) -> (StatusCode, String) {
    let response = router(Arc::clone(&h.app)).oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn get(h: &Harness, path: &str, signed_in: bool) -> Request<Body> {
    let mut request = Request::get(path);
    if signed_in {
        request = request.header(COOKIE, format!("{SESSION_COOKIE}={}", h.session));
    }
    request.body(Body::empty()).unwrap()
}

fn post(h: &Harness, path: &str, form: &str) -> Request<Body> {
    Request::post(path)
        .header(COOKIE, format!("{SESSION_COOKIE}={}", h.session))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(form.to_string()))
        .unwrap()
}

#[tokio::test]
async fn the_admin_pages_are_for_deployment_admins_only() {
    let h = harness(AccessRecords::default(), None);
    assert_eq!(
        send(&h, get(&h, "/admin", false)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&h, get(&h, "/admin", true)).await.0,
        StatusCode::FORBIDDEN
    );
    let token = h.form_token.clone();
    let (status, _) = send(
        &h,
        post(
            &h,
            "/admin/accounts",
            &format!("form_token={token}&name=Growth"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        h.seen.lock().unwrap().is_empty(),
        "a refused person's command is never sent"
    );

    let admin = harness(admin_records(), None);
    assert_eq!(
        send(&admin, get(&admin, "/admin", true)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_change_is_sent_on_the_admins_behalf() {
    let h = harness(admin_records(), None);
    let form = format!("form_token={}&account_id=&name=Growth", h.form_token);
    let (status, _) = send(&h, post(&h, "/admin/accounts", &form)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        h.seen.lock().unwrap().as_slice(),
        [(
            "platform.config.command.define-account".to_string(),
            ADA.to_string()
        )]
    );
}

#[tokio::test]
async fn a_form_without_the_sessions_token_is_refused_and_sends_nothing() {
    let h = harness(admin_records(), None);
    let (status, _) = send(
        &h,
        post(&h, "/admin/accounts", "form_token=forged&name=Growth"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(h.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_stores_refusal_is_shown_as_it_is() {
    let h = harness(
        admin_records(),
        Some("deployment admin is built in, and cannot be edited"),
    );
    let form = format!("form_token={}&name=Growth", h.form_token);
    let (status, body) = send(&h, post(&h, "/admin/accounts", &form)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("deployment admin is built in, and cannot be edited"),
        "{body}"
    );
}

#[tokio::test]
async fn the_claim_page_is_open_only_while_nobody_administers() {
    let open = harness(AccessRecords::default(), None);
    assert_eq!(
        send(&open, get(&open, "/claim", true)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(&open, get(&open, "/claim", false)).await.0,
        StatusCode::UNAUTHORIZED
    );

    let claimed = harness(admin_records(), None);
    assert_eq!(
        send(&claimed, get(&claimed, "/claim", true)).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn a_claim_is_redeemed_on_the_signed_in_persons_behalf() {
    let h = harness(AccessRecords::default(), None);
    let form = format!("form_token={}&code=7KQ2-MX4P-9RTD", h.form_token);
    let (status, _) = send(&h, post(&h, "/claim", &form)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        h.seen.lock().unwrap().as_slice(),
        [(
            "platform.config.command.redeem-claim-code".to_string(),
            ADA.to_string()
        )]
    );
}

#[test]
fn a_directory_group_is_a_whole_line_so_a_distinguished_name_survives() {
    let fields: Fields = [(
        "directory_groups".to_string(),
        "cn=traders,ou=groups,dc=firm,dc=com\r\n\n  ops  \n".to_string(),
    )]
    .into();
    assert_eq!(
        lines(&fields, "directory_groups"),
        ["cn=traders,ou=groups,dc=firm,dc=com", "ops"]
    );
}

#[test]
fn entries_are_plugin_tag_and_level_one_per_line() {
    let parsed = parse_entries("oms-1 oms write\n\n snaptrade-1 custody read ").unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].level, AccessLevel::Write as i32);
    assert_eq!(parsed[1].plugin_instance_id, "snaptrade-1");
    assert!(parse_entries("oms-1 oms admin")
        .unwrap_err()
        .contains("not read or write"));
    assert!(parse_entries("oms-1 write").is_err());
}
