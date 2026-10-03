//! Plugins from a terminal: against a registry that records what reached it
//! and a conductor stood in for on the bus.

use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::Request as HttpRequest;
use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    AccessRecords, Permission, PluginCatalogue, PluginMetadata, PluginVersion,
    RecordPluginUploadRequest, UserGroup,
};
use tower::ServiceExt;

use super::*;
use crate::records::RecordsCache;
use crate::session::Sessions;
use crate::terminal::{check, Terminals};
use crate::web::router;
use crate::Clock;

const T0: i64 = 1_790_380_800_000_000_000;
const DIGEST: &str = "sha256:3f1c0e7a9b2d4c6e8f0a1b3c5d7e9f1a2b4c6d8e0f1a3b5c7d9e1f2a4b6c8d0e";

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

/// What reached the registry: method, path and query, headers, body.
type Reached = Arc<Mutex<Vec<(String, String, HeaderMap, Vec<u8>)>>>;

async fn registry() -> (String, Reached) {
    let reached: Reached = Arc::default();
    let recording = Arc::clone(&reached);
    let app = Router::new().fallback(move |request: Request| {
        let recording = Arc::clone(&recording);
        async move {
            let (parts, body) = request.into_parts();
            let body = to_bytes(body, usize::MAX).await.unwrap().to_vec();
            let path = parts.uri.to_string();
            recording.lock().unwrap().push((
                parts.method.to_string(),
                path.clone(),
                parts.headers,
                body,
            ));
            match (parts.method.as_str(), path.as_str()) {
                ("HEAD", p) if p.contains("/manifests/") => {
                    if p.ends_with(DIGEST) {
                        StatusCode::OK.into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
                    }
                }
                ("POST", _) => (
                    StatusCode::ACCEPTED,
                    [(
                        "location",
                        "/v2/plugins/snaptrade/blobs/uploads/u-1?_state=s",
                    )],
                )
                    .into_response(),
                ("PATCH", _) => StatusCode::ACCEPTED.into_response(),
                ("PUT", _) => {
                    (StatusCode::CREATED, [("docker-content-digest", DIGEST)]).into_response()
                }
                _ => StatusCode::OK.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{at}"), reached)
}

/// Ada administers this deployment; Bob does not.
fn records() -> AccessRecords {
    AccessRecords {
        user_groups: vec![UserGroup {
            user_group_id: "UG-1".into(),
            name: "Admins".into(),
            directory_groups: vec![],
            logins: vec!["local|ada".into()],
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

/// A conductor answering W8 as told: snaptrade 0.1.0 is recorded.
fn conductor(bus: &Bus) {
    bus.serve(PLUGIN_CATALOGUE, |_| {
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            PluginCatalogue {
                versions: vec![PluginVersion {
                    metadata: Some(PluginMetadata {
                        name: "snaptrade".into(),
                        version: "0.1.0".into(),
                        roles: vec!["custody".into()],
                        ..Default::default()
                    }),
                    image_digest: DIGEST.into(),
                    ..Default::default()
                }],
                launches: vec![meridian_domain::v1::PluginLaunch {
                    instance_id: "snaptrade-1".into(),
                    name: "snaptrade".into(),
                    version: "0.1.0".into(),
                    state: PluginLaunchState::Launched as i32,
                    ..Default::default()
                }],
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(RECORD_PLUGIN_UPLOAD, |envelope| {
        let request = RecordPluginUploadRequest::decode(&envelope.payload[..]).unwrap();
        let metadata = request.metadata.clone().unwrap();
        if metadata.version == "0.1.0" {
            return Err(
                "snaptrade 0.1.0 is already recorded; an uploaded version is never replaced".into(),
            );
        }
        if metadata.roles.iter().any(|r| r == "dashboard") {
            return Err("`dashboard` is one of the deployment's own components".into());
        }
        Ok((
            "meridian.v1.PluginVersion".into(),
            PluginVersion {
                metadata: request.metadata,
                image_digest: request.image_digest,
                uploaded_by: envelope.meta.unwrap_or_default().acting_for_subject,
                uploaded_at_ns: T0,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(LAUNCH_PLUGIN, |envelope| {
        let request = LaunchPluginRequest::decode(&envelope.payload[..]).unwrap();
        if request.approved_roles != vec!["custody".to_string()] {
            return Err("the approval names roles none, and snaptrade 0.1.0 declares custody; an approval of something other than what runs is no approval".into());
        }
        Ok((
            "meridian.v1.PluginLaunch".into(),
            PluginLaunch {
                instance_id: request.instance_id,
                state: PluginLaunchState::Launched as i32,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(STOP_PLUGIN, |envelope| {
        let request = StopPluginRequest::decode(&envelope.payload[..]).unwrap();
        if request.instance_id != "snaptrade-1" {
            return Err(format!("no launch of {} is live", request.instance_id));
        }
        Ok((
            "meridian.v1.PluginLaunch".into(),
            PluginLaunch {
                instance_id: request.instance_id,
                state: PluginLaunchState::Stopped as i32,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
}

struct Harness {
    app: Arc<App>,
    reached: Reached,
    ada: String,
    bob: String,
    browser: String,
}

/// A terminal session, by the steps a CLI takes.
async fn terminal_session(terminals: &Terminals, subject: &str) -> String {
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    const BACK: &str = "http://127.0.0.1:53682/callback";
    let id = terminals.open(check(BACK, CHALLENGE, "S256", "st").unwrap(), T0);
    let person = Person {
        subject: subject.into(),
        display_name: subject.into(),
        directory_groups: vec![],
        signed_in_at_ns: T0,
    };
    let confirm = terminals.signed_in(&id, person, T0).unwrap();
    let (_, code) = terminals.decide(&id, &confirm, true, T0).unwrap();
    terminals
        .exchange(&code.unwrap(), VERIFIER, BACK, T0)
        .await
        .expect("the store answers")
        .unwrap()
        .session
}

async fn harness() -> Harness {
    let (base, reached) = registry().await;
    let bus = Arc::new(Bus::single(
        "dashboard-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::SystemClock),
    ));
    conductor(&bus);
    let cache = Arc::new(RecordsCache::default());
    cache.store(records(), T0);
    let terminals = Arc::new(Terminals::default());
    let sessions = Arc::new(Sessions::default());
    let ada = terminal_session(&terminals, "local|ada").await;
    let bob = terminal_session(&terminals, "local|bob").await;
    let browser = sessions.start("local|ada", "Ada", vec![], T0);
    let app = Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: cache,
        sessions,
        terminals,
        delegations: Arc::new(crate::delegation::Delegations::default()),
        public_url: String::new(),
        clock: Arc::new(At(T0)),
        bus,
        oidc: None,
        directory: None,
        accounts: None,
        sign_in_failures: Default::default(),
        secure_cookies: true,
        plugins: None,
        registry: Some(Arc::new(Registry::new(base).unwrap())),
        custody: Arc::default(),
        health: Arc::default(),
        kit: None,
        bounds: Arc::default(),
    });
    Harness {
        app,
        reached,
        ada,
        bob,
        browser: format!("__Host-meridian_session={browser}"),
    }
}

async fn send(
    h: &Harness,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: &str,
) -> (StatusCode, HeaderMap, String) {
    let mut request = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(session) = bearer {
        request = request.header(AUTHORIZATION, format!("Bearer {session}"));
    }
    let response = router(Arc::clone(&h.app))
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_deployment_admins_terminal_session_is_admitted() {
    let h = harness().await;
    let push = "/terminal/registry/v2/plugins/snaptrade/blobs/uploads/";
    assert_eq!(
        send(&h, "POST", push, None, "").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&h, "POST", push, Some(&h.bob), "").await.0,
        StatusCode::FORBIDDEN
    );
    // A browser's session is no terminal session: refused as if absent.
    let response = router(Arc::clone(&h.app))
        .oneshot(
            HttpRequest::post(push)
                .header(COOKIE, &h.browser)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    for (method, path) in [
        ("GET", "/terminal/plugins"),
        ("POST", "/terminal/plugins/launch"),
        ("POST", "/terminal/plugins/stop"),
    ] {
        assert_eq!(
            send(&h, method, path, Some(&h.bob), "{}").await.0,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    assert!(
        h.reached.lock().unwrap().is_empty(),
        "nothing reached the registry"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_is_passed_through_step_by_step_and_its_location_kept_here() {
    let h = harness().await;
    let base = "/terminal/registry/v2/plugins/snaptrade";
    let (status, _, _) = send(
        &h,
        "HEAD",
        &format!("{base}/blobs/{DIGEST}"),
        Some(&h.ada),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, headers, _) = send(
        &h,
        "POST",
        &format!("{base}/blobs/uploads/"),
        Some(&h.ada),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        headers["location"], "/terminal/registry/v2/plugins/snaptrade/blobs/uploads/u-1?_state=s",
        "the client follows it back through here"
    );
    let (status, _, _) = send(
        &h,
        "PATCH",
        &format!("{base}/blobs/uploads/u-1?_state=s"),
        Some(&h.ada),
        "layer bytes",
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, headers, _) = send(
        &h,
        "PUT",
        &format!("{base}/manifests/0.2.0"),
        Some(&h.ada),
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], DIGEST);

    let reached = h.reached.lock().unwrap();
    assert_eq!(reached.len(), 4);
    assert_eq!(
        reached[2].1,
        "/v2/plugins/snaptrade/blobs/uploads/u-1?_state=s"
    );
    assert_eq!(reached[2].3, b"layer bytes", "streamed through");
    assert!(
        reached
            .iter()
            .all(|(_, _, headers, _)| headers.get(AUTHORIZATION).is_none()),
        "the session stays here"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_but_pushing_reaches_the_registry() {
    let h = harness().await;
    let base = "/terminal/registry/v2/plugins";
    for (method, path, status) in [
        (
            "GET",
            format!("{base}/snaptrade/blobs/{DIGEST}"),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            "DELETE",
            format!("{base}/snaptrade/manifests/0.1.0"),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            "GET",
            format!("{base}/snaptrade/manifests/0.1.0"),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            "GET",
            format!("{base}/snaptrade/tags/list"),
            StatusCode::NOT_FOUND,
        ),
        (
            "PUT",
            format!("{base}/Snap_Trade/manifests/1"),
            StatusCode::NOT_FOUND,
        ),
        (
            "PUT",
            format!("{base}/snaptrade/manifests/0.1.0"),
            StatusCode::CONFLICT,
        ),
        (
            "POST",
            format!("{base}/snaptrade/blobs/uploads/?mount={DIGEST}&from=e2e/busybox"),
            StatusCode::FORBIDDEN,
        ),
    ] {
        assert_eq!(
            send(&h, method, &path, Some(&h.ada), "").await.0,
            status,
            "{method} {path}"
        );
    }
    assert!(h.reached.lock().unwrap().is_empty());
}

fn upload_body(version: &str, digest: &str, roles: &[&str]) -> String {
    serde_json::json!({
        "name": "snaptrade", "version": version, "roles": roles,
        "interface": true, "sdk_version": "0.2.0", "image_digest": digest,
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_is_recorded_for_the_admin_once_its_image_is_there() {
    let h = harness().await;
    let missing = format!("sha256:{}", "b".repeat(64));
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &upload_body("0.2.0", &missing, &["custody"]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("push the image first"), "{body}");

    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &upload_body("0.2.0", DIGEST, &["custody"]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let recorded: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(recorded["uploaded_by"], "local|ada");
    assert_eq!(recorded["uploaded_at"], "2026-09-26T00:00:00Z");

    let (status, _, _) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &upload_body("0.1.0", DIGEST, &["custody"]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "already recorded");
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &upload_body("0.3.0", DIGEST, &["dashboard"]),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A CLI built before decisions/026 sends the tags its pyproject names:
    // refused, saying why, and nothing is recorded. None is no declaration.
    let mut tagged: serde_json::Value =
        serde_json::from_str(&upload_body("0.4.0", DIGEST, &["custody"])).unwrap();
    tagged["tags"] = serde_json::json!(["holdings"]);
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &tagged.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.contains("declares tags (holdings)") && body.contains("decisions/026"),
        "{body}"
    );
    tagged["tags"] = serde_json::json!([]);
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &tagged.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalogue_launching_and_stopping() {
    let h = harness().await;
    let (status, _, body) = send(&h, "GET", "/terminal/plugins", Some(&h.ada), "").await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed["versions"][0]["image_digest"], DIGEST);
    assert_eq!(listed["launches"][0]["state"], "launched");

    let launch = |roles: &[&str]| {
        serde_json::json!({"name": "snaptrade", "version": "0.1.0", "instance_id": "snaptrade-2",
                           "approved_roles": roles})
        .to_string()
    };
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/launch",
        Some(&h.ada),
        &launch(&["custody"]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body.contains("\"state\":\"launched\""), "{body}");
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/launch",
        Some(&h.ada),
        &launch(&[]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("declares custody"), "showing both: {body}");

    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/stop",
        Some(&h.ada),
        r#"{"instance_id":"snaptrade-1"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"state\":\"stopped\""), "{body}");
    let (status, _, _) = send(
        &h,
        "POST",
        "/terminal/plugins/stop",
        Some(&h.ada),
        r#"{"instance_id":"nothing-1"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn a_blob_is_mounted_from_another_plugins_repository_alone() {
    for held in [
        None,
        Some("digest=sha256:ab"),
        Some("mount=sha256:ab&from=plugins/reference-plugin"),
        Some("mount=sha256:ab&from=plugins%2Freference-plugin"),
    ] {
        assert!(mounts_from_a_plugin(held), "{held:?}");
    }
    for refused in [
        "mount=sha256:ab&from=e2e/busybox",
        "mount=sha256:ab&from=plugins/../e2e/busybox",
        "mount=sha256:ab&from=plugins/Reference",
        "mount=sha256:ab&from=plugins/a&from=elsewhere/b",
        "from=",
    ] {
        assert!(!mounts_from_a_plugin(Some(refused)), "{refused}");
    }
}
