use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use meridian_domain::v1::{AccessRecords, Permission, UserGroup};
use tower::ServiceExt;

use super::*;
use crate::web::router;
use crate::web::tests::{app_holding_ada, app_with, T0};

/// Ada's local login holding deployment admin, which a reset reaches.
fn local_admins() -> AccessRecords {
    AccessRecords {
        user_groups: vec![UserGroup {
            user_group_id: "UG-1".into(),
            name: "Admins".into(),
            directory_groups: vec![],
            logins: vec![meridian_access::local_login("ada")],
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

/// Ada's deployment, with a conductor that answers a reset code as `answer`
/// and counts how often it was asked.
fn deployment(answer: RedeemClaimCodeReply) -> (Arc<App>, Arc<AtomicUsize>) {
    let app = app_holding_ada();
    app.records.store(local_admins(), T0);
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&asked);
    app.bus.serve(REDEEM_CLAIM_CODE, move |envelope| {
        let request = RedeemClaimCodeRequest::decode(&envelope.payload[..]).unwrap();
        assert_eq!(request.purpose, ClaimCodePurpose::ResetLocalAdmin as i32);
        counted.fetch_add(1, Ordering::SeqCst);
        Ok((
            "meridian.v1.RedeemClaimCodeReply".to_string(),
            answer.encode_to_vec(),
        ))
    });
    (app, asked)
}

fn honoured() -> RedeemClaimCodeReply {
    RedeemClaimCodeReply {
        redeemed: true,
        ..Default::default()
    }
}

async fn post(app: &Arc<App>, path: &str, body: &str) -> (StatusCode, String, String) {
    let response = router(Arc::clone(app))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        location,
        String::from_utf8_lossy(&body).into_owned(),
    )
}

async fn get(app: &Arc<App>, path: &str) -> (StatusCode, String) {
    let response = router(Arc::clone(app))
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

const NEW: &str = "a+new+password+for+ada";

#[tokio::test]
async fn a_reset_code_sets_the_password_and_ends_what_the_old_one_opened() {
    let (app, asked) = deployment(honoured());
    let subject = meridian_access::local_login("ada");
    let session = app.sessions.start(&subject, "Ada Park", vec![], T0);

    let (status, location, body) = post(
        &app,
        "/sign-in/reset",
        &format!("code=7KQ2-MX4P-9RTD&login=ada&password={NEW}&password_again={NEW}"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
    assert_eq!(location, "/sign-in?reset=done");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert!(
        app.sessions.find(&session, T0).is_none(),
        "the old session ended"
    );

    let (signed_in, _, _) = post(&app, "/sign-in", &format!("name=ada&password={NEW}")).await;
    assert_eq!(
        signed_in,
        StatusCode::SEE_OTHER,
        "the new password signs in"
    );
    let (old, _, _) = post(&app, "/sign-in", "name=ada&password=correct+horse+battery").await;
    assert_eq!(old, StatusCode::UNAUTHORIZED, "the old one does not");

    let (_, page) = get(&app, "/sign-in?reset=done").await;
    assert!(page.contains("Your password is set."), "{page}");
}

#[tokio::test]
async fn passwords_that_differ_or_are_short_do_not_spend_the_code() {
    let (app, asked) = deployment(honoured());
    for body in [
        format!("code=C&login=ada&password={NEW}&password_again=something+else+entirely"),
        "code=C&login=ada&password=short&password_again=short".to_string(),
    ] {
        let (status, _, page) = post(&app, "/sign-in/reset", &body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(page.contains("The code was not used."), "{page}");
    }
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "the platform was never asked"
    );
}

#[tokio::test]
async fn a_code_the_platform_refuses_changes_nothing() {
    let (app, _) = deployment(RedeemClaimCodeReply {
        redeemed: false,
        refusal_reason: "already used".into(),
        ..Default::default()
    });
    let (status, _, page) = post(
        &app,
        "/sign-in/reset",
        &format!("code=C&login=ada&password={NEW}&password_again={NEW}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(page.contains("already used"), "{page}");
    let (still, _, _) = post(&app, "/sign-in", "name=ada&password=correct+horse+battery").await;
    assert_eq!(
        still,
        StatusCode::SEE_OTHER,
        "the old password still signs in"
    );
}

#[tokio::test]
async fn a_login_that_is_not_a_local_administrator_is_refused_after_the_code_is_spent() {
    let (app, asked) = deployment(honoured());
    let (status, _, page) = post(
        &app,
        "/sign-in/reset",
        &format!("code=C&login=nobody&password={NEW}&password_again={NEW}"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        page.contains("not a local account holding deployment admin"),
        "{page}"
    );
    assert!(page.contains("The code was spent"), "{page}");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "spent before the login was looked at"
    );
}

#[tokio::test]
async fn a_deployment_without_local_accounts_offers_no_reset() {
    let app = app_with(Some(local_admins()), T0, T0);
    let (status, _) = get(&app, "/sign-in/reset").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_sign_in_page_offers_the_reset_and_warns_the_same_for_every_name() {
    let (app, _) = deployment(honoured());
    let (_, page) = get(&app, "/sign-in").await;
    assert!(page.contains("href=\"/sign-in/reset\""), "{page}");

    for name in ["ada", "nobody"] {
        let mut said = Vec::new();
        for _ in 0..4 {
            let (_, _, page) = post(&app, "/sign-in", &format!("name={name}&password=wrong")).await;
            said.push(page);
        }
        assert!(
            !said[1].contains("more attempt"),
            "none before the third: {name}"
        );
        assert!(
            said[2].contains("2 more attempts before this username is locked"),
            "{name}"
        );
        assert!(
            said[3].contains("One more attempt locks this username"),
            "{name}"
        );
    }
}
