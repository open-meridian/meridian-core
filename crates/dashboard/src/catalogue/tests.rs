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

/// A launch or a stop from the terminal is sent stamped with the person,
/// the delegation and its client (W8.3, W8.4, contract v17): the stand-in
/// refuses one that is not, so the tests below hold the stamp.
fn stamped(envelope: &meridian_bus::Envelope) -> Result<(), String> {
    let meta = envelope.meta.clone().unwrap_or_default();
    if meta.acting_for_subject.is_empty()
        || meta.acting_through_delegation.is_empty()
        || meta.acting_through_client.is_empty()
    {
        return Err(format!(
            "not stamped with the delegation and client: {meta:?}"
        ));
    }
    Ok(())
}

/// A conductor answering W8 as told: snaptrade 0.1.0 is recorded.
fn conductor(bus: &Bus) {
    bus.serve(PLUGIN_CATALOGUE, |_| {
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            PluginCatalogue {
                next_cursor: String::new(),
                versions: vec![PluginVersion {
                    metadata: Some(PluginMetadata {
                        name: "snaptrade".into(),
                        version: "0.1.0".into(),
                        roles: vec!["custody".into()],
                        ..Default::default()
                    }),
                    image_digest: DIGEST.into(),
                    listing: "open-meridian/snaptrade".into(),
                    source_registry: "community".into(),
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
        // A plugin bundle (W8.14): its listing, source registry, key status
        // and listing statement carried whole for the conductor to verify.
        if !request.listing.is_empty() {
            let status: serde_json::Value =
                serde_json::from_str(&request.key_status).unwrap_or_default();
            if request.listing != "open-meridian/snaptrade"
                || request.source_registry != "community"
                || status["issued_at_ns"] != 1_792_000_000_000_000_000i64
                || request.listing_statement != "{\"payloadType\":\"x\"}"
            {
                return Err(format!("the bundle not carried whole: {request:?}"));
            }
        }
        // Contract v25: the SDK's declaration read whole, and the texts
        // carried beside it for the conductor to hold to its digests.
        if metadata.version == "0.5.0" {
            let declaration = metadata.declaration.clone().unwrap_or_default();
            let carried = declaration.settings.len() == 1
                && declaration.settings[0].name == "region"
                && declaration.interface.as_ref().map(|i| i.loopback_port) == Some(8000)
                && declaration.tools.len() == 1
                && declaration.reads_external_accounts
                && declaration.documentation.len() == 1
                && declaration.documentation[0].bytes == 5
                && request.documentation.len() == 1
                && request.documentation[0].section == "overview"
                && request.documentation[0].markdown == "# Hi\n";
            if !carried {
                return Err(format!("not carried whole: {request:?}"));
            }
        }
        Ok((
            "meridian.v1.PluginVersion".into(),
            PluginVersion {
                metadata: request.metadata,
                image_digest: request.image_digest,
                uploaded_by: envelope.meta.unwrap_or_default().acting_for_subject,
                uploaded_at_ns: T0,
                source_registry: "local".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(LAUNCH_PLUGIN, |envelope| {
        stamped(&envelope)?;
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
    // Contract v26 (W8.2, W8.12): the catalogue's copy, 0.1.0 pulled.
    bus.serve(crate::registry::REGISTRY_PLUGINS, |_| {
        Ok((
            "meridian.v1.ListRegistryPluginsReply".into(),
            meridian_domain::v1::ListRegistryPluginsReply {
                listings: vec![meridian_domain::v1::PluginListing {
                    listing: "open-meridian/snaptrade".into(),
                    versions: vec![meridian_domain::v1::ListedVersion {
                        listing: "open-meridian/snaptrade".into(),
                        version: "0.1.0".into(),
                        registry: "community".into(),
                        pull: Some(meridian_domain::v1::PluginPull {
                            kind: meridian_domain::v1::PullReasonKind::TermsBreach as i32,
                            reason: "a breach".into(),
                            pulled_at_ns: T0,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    // Contract v26: the registry's acts, as the terminal sends them.
    bus.serve(crate::registry::INSTALL_PLUGIN, |envelope| {
        stamped(&envelope)?;
        let request =
            meridian_domain::v1::InstallPluginRequest::decode(&envelope.payload[..]).unwrap();
        if request.version == "9.9.9" {
            return Err(
                "open-meridian/snaptrade 9.9.9 needs contract v99; this deployment runs v26".into(),
            );
        }
        Ok((
            "meridian.v1.PluginVersion".into(),
            PluginVersion {
                metadata: Some(PluginMetadata {
                    name: "snaptrade".into(),
                    version: request.version,
                    ..Default::default()
                }),
                listing: request.listing,
                source_registry: request.registry,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::registry::UPDATE_PLUGIN, |envelope| {
        stamped(&envelope)?;
        let request =
            meridian_domain::v1::UpdatePluginRequest::decode(&envelope.payload[..]).unwrap();
        Ok((
            "meridian.v1.PluginLaunch".into(),
            PluginLaunch {
                instance_id: request.instance_id,
                version: request.version,
                state: PluginLaunchState::Launched as i32,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(crate::catalogue::RETIRE_PLUGIN_VERSION, |envelope| {
        stamped(&envelope)?;
        let request =
            meridian_domain::v1::RetirePluginVersionRequest::decode(&envelope.payload[..]).unwrap();
        if request.version == "0.1.0" {
            return Err("snaptrade 0.1.0 is run by snaptrade-1; nothing retired".into());
        }
        Ok((
            "meridian.v1.PluginVersion".into(),
            PluginVersion {
                metadata: Some(PluginMetadata {
                    name: request.name,
                    version: request.version,
                    ..Default::default()
                }),
                retired_by: envelope.meta.unwrap_or_default().acting_for_subject,
                retired_at_ns: T0,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(STOP_PLUGIN, |envelope| {
        stamped(&envelope)?;
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
    let delegations = Arc::new(crate::delegation::Delegations::default());
    let sessions = Arc::new(Sessions::default());
    // What `meridian connect` holds: a delegation to the CLI (W6.13, W6.17).
    let ada = crate::delegation::connected_for_tests(&delegations, "local|ada", T0).await;
    let bob = crate::delegation::connected_for_tests(&delegations, "local|bob", T0).await;
    let browser = sessions.start("local|ada", "Ada", vec![], T0);
    let app = Arc::new(App {
        first_run: false,
        wizard: Arc::new(crate::first_run::WizardSession::default()),
        records: cache,
        sessions,
        delegations,
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
        tickets: Arc::default(),
        prepared: Arc::default(),
        service_accounts: Arc::default(),
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
async fn the_books_health_is_read_from_a_deployment_admins_terminal() {
    // ReadBookHealthFromTerminal (contract v25; ruling 155): what `meridian
    // status` reads, every page, a deployment admin's alone.
    let h = harness().await;
    h.app.bus.serve(READ_BOOK_HEALTH, |envelope| {
        stamped(&envelope)?;
        let asked =
            meridian_domain::v1::ReadBookHealthRequest::decode(&envelope.payload[..]).unwrap();
        let (accounts, next_cursor) = if asked.cursor.is_empty() {
            (vec!["ACC-1"], "ACC-1")
        } else {
            (vec!["ACC-2"], "")
        };
        Ok((
            "".into(),
            meridian_domain::v1::ReadBookHealthReply {
                accounts: accounts
                    .into_iter()
                    .map(|account| meridian_domain::v1::AccountBookHealth {
                        account_id: account.into(),
                        journal_entries: 2_400,
                        entries_since_snapshot: 400,
                        last_snapshot_at_ns: T0,
                    })
                    .collect(),
                next_cursor: next_cursor.into(),
            }
            .encode_to_vec(),
        ))
    });
    assert_eq!(
        send(&h, "GET", "/terminal/book-health", Some(&h.bob), "")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, _, body) = send(&h, "GET", "/terminal/book-health", Some(&h.ada), "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let read: serde_json::Value = serde_json::from_str(&body).unwrap();
    let accounts = read["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2, "every page: {body}");
    assert_eq!(accounts[1]["account_id"], "ACC-2");
    assert_eq!(accounts[0]["entries_since_snapshot"], 400);
    assert_eq!(accounts[0]["last_snapshot_at_ns"], T0.to_string());
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

    // Contract v25: the declaration as the SDK prints it (protobuf's JSON
    // for its settings, interface and tools) and the texts from its docs/.
    let mut documented: serde_json::Value =
        serde_json::from_str(&upload_body("0.5.0", DIGEST, &["custody"])).unwrap();
    documented["declaration"] = serde_json::json!({
        "secret_settings": [],
        "settings": [{"name": "region", "label": "Region", "type": "SETTING_TYPE_STRING"}],
        "interface": {"loopback_port": 8000, "title": "SnapTrade",
                      "pages": [{"path": "/admin/connections", "title": "Connections",
                                 "levels": ["ACCESS_LEVEL_ADMIN"]}]},
        "tools": [{"name": "read_connections", "title": "Read the connections",
                   "description": "Each connection.", "method": "GET",
                   "path": "/admin/connections", "levels": ["ACCESS_LEVEL_ADMIN"],
                   "reads": true}],
        "reads_external_accounts": true,
        "documentation": [{"name": "overview",
            "digest": format!("sha256:{}", "c".repeat(64)), "bytes": 5}],
    });
    documented["documentation"] =
        serde_json::json!([{"section": "overview", "markdown": "# Hi\n"}]);
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &documented.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // Ruling 176: a false claim in a section refuses the upload, naming it.
    let mut claiming = documented.clone();
    claiming["documentation"] =
        serde_json::json!([{"section": "overview", "markdown": "Endorsed by Open Meridian."}]);
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &claiming.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.contains("documentation.overview") && body.contains("claims endorsement"),
        "{body}"
    );
    // A text naming a field the CLI never sends is refused, not dropped.
    documented["documentation"][0]["html"] = serde_json::json!("<p>Hi</p>");
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &documented.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalogue_launching_and_stopping() {
    let h = harness().await;
    let (status, _, body) = send(&h, "GET", "/terminal/plugins", Some(&h.ada), "").await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed["versions"][0]["image_digest"], DIGEST);
    assert_eq!(listed["launches"][0]["state"], "launched");
    // Contract v26: the version's source and its pull, and the instance
    // running it not kept.
    assert_eq!(listed["versions"][0]["source_registry"], "community");
    assert_eq!(
        listed["versions"][0]["pull"]["kind"], "terms_breach",
        "{body}"
    );
    assert_eq!(
        listed["versions"][0]["pull"]["warning"],
        "pulled: a breach of the Platform Terms"
    );
    assert_eq!(listed["launches"][0]["pull_kept"], false);

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

/// Contract v26 (the v26 plan's question 237): `meridian plugin install`,
/// `update` and `retire` send JSON the dashboard turns into the bus's
/// commands for a deployment admin, each with a note, stamped with the
/// delegation; a refusal says its status and its words.
#[tokio::test(flavor = "multi_thread")]
async fn a_registry_version_is_installed_updated_and_retired_from_the_terminal() {
    let h = harness().await;
    for path in [
        "/terminal/plugins/install",
        "/terminal/plugins/update",
        "/terminal/plugins/retire",
    ] {
        assert_eq!(
            send(&h, "POST", path, Some(&h.bob), "{}").await.0,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    let install = |version: &str, note: &str| {
        serde_json::json!({"listing": "open-meridian/snaptrade", "version": version,
                           "registry": "community", "note": note})
        .to_string()
    };
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/install",
        Some(&h.ada),
        &install("0.18.0", "First-party custody plugin."),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let said: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (said["version"].as_str(), said["source_registry"].as_str()),
        (Some("0.18.0"), Some("community"))
    );
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/install",
        Some(&h.ada),
        &install("0.18.0", " "),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("\"fields\":[\"note\"]"), "{body}");
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/install",
        Some(&h.ada),
        &install("9.9.9", "Newer."),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("needs contract v99"), "{body}");

    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/update",
        Some(&h.ada),
        r#"{"instance_id":"snaptrade-1","version":"0.18.1","approved_roles":["custody"],"note":"Fixes the pager."}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("\"version\":\"0.18.1\""), "{body}");

    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/retire",
        Some(&h.ada),
        r#"{"name":"snaptrade","version":"0.1.0","note":"Old."}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins/retire",
        Some(&h.ada),
        r#"{"name":"snaptrade","version":"0.0.9","note":"Never launched."}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let said: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        !said["retired_by"].as_str().unwrap_or_default().is_empty(),
        "{body}"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_is_pushed_into_its_listing_s_repository_and_carried_whole() {
    let h = harness().await;
    // The image and its referrers, into plugins/{organisation}/{plugin}.
    let base = "/terminal/registry/v2/plugins/open-meridian/snaptrade";
    for (method, path, status) in [
        ("HEAD", format!("{base}/blobs/{DIGEST}"), StatusCode::OK),
        (
            "POST",
            format!("{base}/blobs/uploads/"),
            StatusCode::ACCEPTED,
        ),
        (
            "PATCH",
            format!("{base}/blobs/uploads/u-1?_state=s"),
            StatusCode::ACCEPTED,
        ),
        (
            "PUT",
            format!("{base}/manifests/1.4.0"),
            StatusCode::CREATED,
        ),
        (
            "PUT",
            format!("{base}/manifests/sha256-{}", "3f".repeat(32)),
            StatusCode::CREATED,
        ),
        (
            "GET",
            format!("{base}/manifests/1.4.0"),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            "PUT",
            format!("{base}/x/manifests/1.4.0"),
            StatusCode::NOT_FOUND,
        ),
        (
            "PUT",
            format!("{base}/Snap_Trade/manifests/1"),
            StatusCode::NOT_FOUND,
        ),
    ] {
        assert_eq!(
            send(&h, method, &path, Some(&h.ada), "").await.0,
            status,
            "{method} {path}"
        );
    }
    {
        let reached = h.reached.lock().unwrap();
        assert_eq!(reached.len(), 5);
        assert_eq!(
            reached[0].1,
            format!("/v2/plugins/open-meridian/snaptrade/blobs/{DIGEST}")
        );
        assert_eq!(
            reached[3].1,
            "/v2/plugins/open-meridian/snaptrade/manifests/1.4.0"
        );
    }
    // The version recorded, its listing's repository checked for the image.
    let mut bundle: serde_json::Value =
        serde_json::from_str(&upload_body("1.4.0", DIGEST, &["custody"])).unwrap();
    bundle["listing"] = serde_json::json!("open-meridian/snaptrade");
    bundle["source_registry"] = serde_json::json!("community");
    bundle["key_status"] = serde_json::json!({"issued_at_ns": 1_792_000_000_000_000_000i64,
        "keys": [], "signature": "c2ln"});
    bundle["listing_statement"] = serde_json::json!("{\"payloadType\":\"x\"}");
    let (status, _, body) = send(
        &h,
        "POST",
        "/terminal/plugins",
        Some(&h.ada),
        &bundle.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(h
        .reached
        .lock()
        .unwrap()
        .iter()
        .any(|(method, path, _, _)| method == "HEAD"
            && path == &format!("/v2/plugins/open-meridian/snaptrade/manifests/{DIGEST}")));
    // A listing that is not organisation/plugin naming this plugin, or a
    // bundle without its key status, is refused before the conductor hears.
    for (field, value) in [
        ("listing", serde_json::json!("open-meridian/kraken")),
        ("listing", serde_json::json!("snaptrade")),
        ("listing", serde_json::json!("Open_Meridian/snaptrade")),
        ("key_status", serde_json::Value::Null),
        ("source_registry", serde_json::json!("")),
    ] {
        let mut spoiled = bundle.clone();
        spoiled[field] = value;
        let (status, _, body) = send(
            &h,
            "POST",
            "/terminal/plugins",
            Some(&h.ada),
            &spoiled.to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {body}");
    }
}
