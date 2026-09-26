//! The front door: the verifier on its own, then the listener in front of a
//! stand-in plugin that records what reached it.

use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::pkcs8::EncodePublicKey;
use ed25519_dalek::{Signer as _, SigningKey};
use meridian_bus::{Bus, MemoryBackend};
use meridian_pb::v1::sidecar_service_server::SidecarService;
use meridian_pb::v1::{CallerAssertion, CallerClaims, InterfaceDeclaration, RegisterRequest};
use prost::Message;
use tonic::Request;

use super::*;
use crate::grants::Contract;
use crate::service::Identity;

const INSTANCE: &str = "snaptrade-1";
const KEY_ID: &str = "dashboard-2026-09-0a1b2c3d";
const SECOND: i64 = 1_000_000_000;
const NOW: i64 = 1_790_000_000 * SECOND;

fn key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

fn claims(assertion_id: &str) -> CallerClaims {
    CallerClaims {
        subject: "person-1".into(),
        display_name: "Ada".into(),
        audience_instance_id: INSTANCE.into(),
        issued_at_ns: NOW,
        expires_at_ns: NOW + 60 * SECOND,
        assertion_id: assertion_id.into(),
        ..Default::default()
    }
}

/// What the dashboard sends: the claims, signed, in one header.
fn signed(key: &SigningKey, key_id: &str, claims: &CallerClaims) -> String {
    let claims = claims.encode_to_vec();
    let assertion = CallerAssertion {
        signature: key.sign(&claims).to_bytes().to_vec(),
        claims,
        key_id: key_id.into(),
    };
    URL_SAFE_NO_PAD.encode(assertion.encode_to_vec())
}

#[test]
fn an_assertion_the_dashboard_signed_for_this_instance_is_admitted() {
    let key = key();
    let verifier = Verifier::holding(INSTANCE, KEY_ID, key.verifying_key());
    let admitted = verifier
        .verify(Some(&signed(&key, KEY_ID, &claims("a-1"))), NOW + SECOND)
        .unwrap();
    assert_eq!(admitted.subject, "person-1");
}

#[test]
fn every_way_an_assertion_is_wrong_is_refused_and_named() {
    let key = key();
    let stranger = self::key();
    let verifier = Verifier::holding(INSTANCE, KEY_ID, key.verifying_key());

    let tampered = {
        let mut assertion = CallerAssertion::decode(
            URL_SAFE_NO_PAD
                .decode(signed(&key, KEY_ID, &claims("a-2")))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let mut widened = claims("a-2");
        widened.subject = "somebody-else".into();
        assertion.claims = widened.encode_to_vec();
        URL_SAFE_NO_PAD.encode(assertion.encode_to_vec())
    };
    let for_another = {
        let mut c = claims("a-3");
        c.audience_instance_id = "snaptrade-2".into();
        signed(&key, KEY_ID, &c)
    };
    let long_lived = {
        let mut c = claims("a-4");
        c.expires_at_ns = NOW + 61 * SECOND;
        signed(&key, KEY_ID, &c)
    };
    let without_id = signed(&key, KEY_ID, &claims(""));

    let cases: Vec<(&str, Option<String>, i64, Refusal)> = vec![
        ("nothing presented", None, NOW, Refusal::Missing),
        (
            "not base64",
            Some("not base64!".into()),
            NOW,
            Refusal::Malformed(String::new()),
        ),
        (
            "signed by a key it does not hold",
            Some(signed(&key, "dashboard-other", &claims("a-5"))),
            NOW,
            Refusal::UnknownKey("dashboard-other".into()),
        ),
        (
            "signed by another key under this key's id",
            Some(signed(&stranger, KEY_ID, &claims("a-6"))),
            NOW,
            Refusal::BadSignature,
        ),
        (
            "claims changed after signing",
            Some(tampered),
            NOW,
            Refusal::BadSignature,
        ),
        (
            "for another instance of the same plugin",
            Some(for_another),
            NOW,
            Refusal::ForAnotherInstance("snaptrade-2".into()),
        ),
        (
            "living past 60 seconds",
            Some(long_lived),
            NOW,
            Refusal::LivesTooLong,
        ),
        (
            "expired",
            Some(signed(&key, KEY_ID, &claims("a-7"))),
            NOW + 66 * SECOND,
            Refusal::OutOfDate,
        ),
        (
            "issued in the future",
            Some(signed(&key, KEY_ID, &claims("a-8"))),
            NOW - 6 * SECOND,
            Refusal::OutOfDate,
        ),
        (
            "carrying no id to refuse a replay by",
            Some(without_id),
            NOW,
            Refusal::Malformed(String::new()),
        ),
    ];
    for (case, header, at, expected) in cases {
        let refused = verifier.verify(header.as_deref(), at).unwrap_err();
        match (&refused, &expected) {
            (Refusal::Malformed(_), Refusal::Malformed(_)) => {}
            _ => assert_eq!(refused, expected, "{case}"),
        }
    }
}

#[test]
fn an_assertion_is_admitted_once_and_its_id_forgotten_once_it_could_not_be_replayed() {
    let key = key();
    let verifier = Verifier::holding(INSTANCE, KEY_ID, key.verifying_key());
    let header = signed(&key, KEY_ID, &claims("a-1"));

    verifier.verify(Some(&header), NOW).unwrap();
    assert_eq!(
        verifier.verify(Some(&header), NOW + SECOND),
        Err(Refusal::Replayed)
    );

    // Two minutes on, the first is out of date whatever it claims, so the
    // record of it goes: the cache holds 60 seconds of traffic, not all of it.
    let mut later = claims("a-2");
    later.issued_at_ns = NOW + 120 * SECOND;
    later.expires_at_ns = NOW + 180 * SECOND;
    verifier
        .verify(Some(&signed(&key, KEY_ID, &later)), NOW + 120 * SECOND)
        .unwrap();
    let seen = verifier.seen.lock().unwrap();
    assert_eq!(seen.keys().collect::<Vec<_>>(), vec!["a-2"]);
}

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("meridian-front-door-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_key_written_after_the_sidecar_started_is_read_when_first_named() {
    // The Job may finish after the sidecar starts; no restart follows it.
    let dir = scratch("late-key");
    let key = key();
    let verifier = Verifier::new(INSTANCE, &dir);
    let header = signed(&key, KEY_ID, &claims("a-1"));

    assert_eq!(
        verifier.verify(Some(&header), NOW),
        Err(Refusal::UnknownKey(KEY_ID.into()))
    );

    let pem = key
        .verifying_key()
        .to_public_key_pem(Default::default())
        .unwrap();
    std::fs::write(dir.join(format!("{KEY_ID}.pem")), pem).unwrap();
    verifier.verify(Some(&header), NOW).unwrap();

    // Kept once read: the file going does not unlearn it.
    std::fs::remove_dir_all(&dir).unwrap();
    verifier
        .verify(Some(&signed(&key, KEY_ID, &claims("a-2"))), NOW)
        .unwrap();
}

#[test]
fn a_key_id_that_names_a_path_outside_the_directory_is_not_read() {
    let dir = scratch("climbing");
    let key = key();
    let pem = key
        .verifying_key()
        .to_public_key_pem(Default::default())
        .unwrap();
    // A key an attacker placed beside the directory, not in it.
    std::fs::write(dir.join("planted.pem"), pem).unwrap();
    // The directory exists, as the mount does, so `keys/..` resolves.
    std::fs::create_dir_all(dir.join("keys")).unwrap();
    let verifier = Verifier::new(INSTANCE, dir.join("keys"));
    for key_id in ["../planted", "..", "", "keys/../../planted"] {
        assert_eq!(
            verifier.verify(Some(&signed(&key, key_id, &claims(key_id))), NOW),
            Err(Refusal::UnknownKey(key_id.into())),
            "{key_id:?}"
        );
    }
}

/// An assertion for now, for the listener, which reads the real clock.
fn fresh(key: &SigningKey, assertion_id: &str) -> String {
    let mut c = claims(assertion_id);
    c.issued_at_ns = now_ns();
    c.expires_at_ns = c.issued_at_ns + 60 * SECOND;
    signed(key, KEY_ID, &c)
}

/// What reached the stand-in plugin, one entry per request.
type Reached = Arc<Mutex<Vec<(HeaderMap, String, String)>>>;

/// A plugin serving a page on loopback, recording each request it is sent.
async fn plugin() -> (u16, Reached) {
    let reached: Reached = Arc::default();
    let recorded = Arc::clone(&reached);
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        let recorded = Arc::clone(&recorded);
        async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            recorded.lock().unwrap().push((
                parts.headers,
                parts.uri.to_string(),
                String::from_utf8(body.to_vec()).unwrap(),
            ));
            if parts.uri.path() == "/moved" {
                return (
                    axum::http::StatusCode::SEE_OTHER,
                    [("location", "/signed-in"), ("x-plugin", "holdings")],
                    "",
                );
            }
            (
                axum::http::StatusCode::OK,
                [("x-plugin", "holdings"), ("x-served", "page")],
                "the page",
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, reached)
}

fn contract() -> Contract {
    Contract::parse(
        "topic\tkind\tpublisher\tsubscriber\n",
        "name\tkind\ncustody\trole\n",
    )
    .unwrap()
}

/// A sidecar whose plugin registered, declaring `port` for its page, and its
/// front door listening; the door's address comes back.
async fn front_door(key: &SigningKey, port: Option<u32>) -> String {
    let bus = Arc::new(Bus::single(INSTANCE, Arc::new(MemoryBackend::new())));
    let sidecar = Arc::new(Sidecar::under(
        &contract(),
        bus,
        "dep-local-1",
        Identity::new(INSTANCE, vec!["custody".into()]),
    ));
    let reply = sidecar
        .register(Request::new(RegisterRequest {
            schema_version: "v1".into(),
            interface: port.map(|loopback_port| InterfaceDeclaration {
                loopback_port,
                title: "Holdings".into(),
            }),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(reply.admitted, "{}", reply.refusal_reason);

    let door = router(
        FrontDoor::new(
            sidecar,
            Verifier::holding(INSTANCE, KEY_ID, key.verifying_key()),
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, door).await.unwrap() });
    format!("http://{address}")
}

#[tokio::test]
async fn a_verified_request_reaches_the_plugin_carrying_only_the_identity_verified() {
    let key = key();
    let (port, reached) = plugin().await;
    let door = front_door(&key, Some(port.into())).await;
    let assertion = fresh(&key, "a-1");

    let answer = reqwest::Client::new()
        .post(format!("{door}/orders?page=2"))
        .header(HEADER, &assertion)
        .header("cookie", "meridian_session=the-persons-dashboard-session")
        .header("authorization", "Bearer somebody-elses")
        .header("x-requested-with", "the-page")
        .body("quantity=5")
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 200);
    assert_eq!(answer.headers()["x-plugin"], "holdings");
    assert_eq!(answer.text().await.unwrap(), "the page");

    let reached = reached.lock().unwrap();
    let (headers, uri, body) = &reached[0];
    assert_eq!(uri, "/orders?page=2");
    assert_eq!(body, "quantity=5");
    assert_eq!(
        headers.get_all(HEADER).iter().collect::<Vec<_>>(),
        vec![assertion.as_str()]
    );
    assert!(
        headers.get("cookie").is_none(),
        "a cookie reached the plugin"
    );
    assert!(
        headers.get("authorization").is_none(),
        "a credential reached the plugin"
    );
    assert_eq!(headers["x-requested-with"], "the-page");
}

#[tokio::test]
async fn a_redirect_goes_back_to_the_browser_rather_than_being_followed() {
    let key = key();
    let (port, reached) = plugin().await;
    let door = front_door(&key, Some(port.into())).await;

    let answer = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(format!("{door}/moved"))
        .header(HEADER, fresh(&key, "a-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 303);
    assert_eq!(answer.headers()["location"], "/signed-in");
    assert_eq!(
        reached.lock().unwrap().len(),
        1,
        "the redirect was followed"
    );
}

#[tokio::test]
async fn a_refused_request_never_reaches_the_plugin() {
    let key = key();
    let (port, reached) = plugin().await;
    let door = front_door(&key, Some(port.into())).await;
    let client = reqwest::Client::new();

    let bare = client.get(format!("{door}/")).send().await.unwrap();
    assert_eq!(bare.status(), 401);

    // A second header beside a good one: somebody adding theirs.
    let doubled = client
        .get(format!("{door}/"))
        .header(HEADER, fresh(&key, "a-1"))
        .header(HEADER, fresh(&key, "a-2"))
        .send()
        .await
        .unwrap();
    assert_eq!(doubled.status(), 401);

    let mut expired = claims("a-3");
    expired.issued_at_ns = now_ns() - 120 * SECOND;
    expired.expires_at_ns = expired.issued_at_ns + 60 * SECOND;
    let stale = client
        .get(format!("{door}/"))
        .header(HEADER, signed(&key, KEY_ID, &expired))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 403);
    assert!(reached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_plugin_that_serves_no_page_is_not_found_and_one_that_is_down_is_said_so() {
    let key = key();

    let no_page = front_door(&key, None).await;
    let answer = reqwest::Client::new()
        .get(format!("{no_page}/"))
        .header(HEADER, fresh(&key, "a-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 404);

    // A port nothing listens on: bound, then let go.
    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let down = front_door(&key, Some(port.into())).await;
    let answer = reqwest::Client::new()
        .get(format!("{down}/"))
        .header(HEADER, fresh(&key, "a-2"))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), 502);
}

#[tokio::test]
async fn an_interface_on_a_port_that_is_not_one_is_refused_at_registration() {
    for loopback_port in [0, 65_536] {
        let bus = Arc::new(Bus::single(INSTANCE, Arc::new(MemoryBackend::new())));
        let sidecar = Sidecar::under(
            &contract(),
            bus,
            "dep-local-1",
            Identity::new(INSTANCE, vec!["custody".into()]),
        );
        let reply = sidecar
            .register(Request::new(RegisterRequest {
                schema_version: "v1".into(),
                interface: Some(InterfaceDeclaration {
                    loopback_port,
                    title: "Holdings".into(),
                }),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!reply.admitted, "{loopback_port}");
        assert!(
            reply.refusal_reason.contains("not a port"),
            "{}",
            reply.refusal_reason
        );
    }
}
