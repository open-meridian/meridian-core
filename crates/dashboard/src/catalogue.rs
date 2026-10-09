//! Plugins, from a terminal (W8): the registry pass-through a plugin's image
//! is pushed through, and the catalogue, launching and stopping.
//!
//! Every route here admits a deployment admin acting from a terminal -- an
//! access token on a delegation covering the deployment admin's
//! capabilities (W6.18) -- and nothing else: not a browser's cookie, which is refused as if absent. The image
//! goes layer by layer in the registry's own protocol, streamed and never
//! held, under `plugins/{name}` alone: the registry is reachable from
//! outside the cluster only this way, and only to write and to ask whether a
//! layer is there. The rest become the conductor's bus commands, sent for
//! the admin, so the conductor records who did each; it decides, and the
//! launcher acts (decisions/019).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{CACHE_CONTROL, LOCATION};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use meridian_bus::BusError;
use meridian_domain::v1::{
    LaunchPluginRequest, PluginCatalogue, PluginCatalogueRequest, PluginLaunch, PluginLaunchState,
    PluginMetadata, PluginVersion, RecordPluginUploadRequest, StopPluginRequest,
};
use prost::Message;

use crate::terminal::{rfc3339, Person};
use crate::web::{caller_of, App};

pub const RECORD_PLUGIN_UPLOAD: &str = "platform.config.command.record-plugin-upload";
pub const PLUGIN_CATALOGUE: &str = "platform.config.query.plugin-catalogue";
pub const LAUNCH_PLUGIN: &str = "platform.config.command.launch-plugin";
pub const STOP_PLUGIN: &str = "platform.config.command.stop-plugin";

/// The deployment's registry, as this pod reaches it inside the cluster.
pub struct Registry {
    base: String,
    client: reqwest::Client,
}

impl Registry {
    pub fn new(base: impl Into<String>) -> Result<Registry, String> {
        let client = reqwest::Client::builder()
            // The registry's redirects are for the client, whose Location is
            // rewritten below; followed here, they would be followed twice.
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|failed| format!("the registry client could not be built: {failed}"))?;
        Ok(Registry {
            base: base.into().trim_end_matches('/').to_string(),
            client,
        })
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route(
            "/terminal/registry/v2/plugins/{name}/{*rest}",
            any(pass_through),
        )
        .route("/terminal/plugins", get(list).post(upload))
        .route("/terminal/plugins/launch", post(launch))
        .route("/terminal/plugins/stop", post(stop))
        .route(
            "/terminal/plugins/{instance}/dev/{what}",
            get(develop).put(develop),
        )
        .route(
            "/terminal/plugins/{instance}/open",
            post(crate::plugins::open_from_terminal),
        )
        .route(
            "/terminal/plugins/{instance}/page",
            get(crate::plugins::page_from_terminal),
        )
}

fn json(status: StatusCode, body: serde_json::Value) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn refused(status: StatusCode, reason: impl Into<String>) -> Response {
    json(status, serde_json::json!({ "error": reason.into() }))
}

/// A deployment admin acting from a terminal, and what every request sent
/// for them carries: the person, the delegation and its client (W4.9's
/// stamp; contract v17, W8.3 and W8.4).
struct Admin {
    person: Person,
    stamp: meridian_bus::Stamp,
}

/// A deployment admin acting from a terminal -- on a delegation covering the
/// deployment admin's capabilities -- or the refusal.
async fn admin(app: &App, headers: &HeaderMap) -> Result<Admin, Box<Response>> {
    let caller = caller_of(app, headers).await?;
    let records = app
        .records
        .current(app.clock.now_ns())
        .map_err(|stale| Box::new(refused(StatusCode::SERVICE_UNAVAILABLE, stale.to_string())))?;
    if !caller.access(&records).deployment_admin {
        let why = if caller.delegation_id().is_some()
            && meridian_access::person_access(
                &records,
                &caller.person.subject,
                &caller.person.directory_groups,
            )
            .deployment_admin
        {
            "this delegation does not cover the deployment admin's capabilities; connect again \
             to widen it"
        } else {
            "only a deployment admin brings plugins into this deployment"
        };
        caller.refused(app, why).await;
        return Err(Box::new(refused(StatusCode::FORBIDDEN, why)));
    }
    let crate::web::Through::Delegation {
        id, client_name, ..
    } = &caller.through;
    let stamp = meridian_bus::Stamp {
        acting_for_subject: caller.person.subject.clone(),
        acting_through_delegation: id.clone(),
        acting_through_client: client_name.clone(),
        account_scope: None,
    };
    Ok(Admin {
        person: caller.person,
        stamp,
    })
}

/// The admin, then their JSON: who is asking is settled before what they
/// sent is read, so a stranger learns nothing from a body's refusal.
async fn admin_asking<T: serde::de::DeserializeOwned>(
    app: &App,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(Admin, T), Box<Response>> {
    let person = admin(app, headers).await?;
    let asked = serde_json::from_slice(body).map_err(|failed| {
        Box::new(refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the request does not read: {failed}"),
        ))
    })?;
    Ok((person, asked))
}

/// A bus command or query sent for the admin, stamped with the delegation
/// and client they acted through, and its refusal's words.
async fn ask<Rep: Message + Default>(
    app: &App,
    admin: &Admin,
    topic: &str,
    request_type: &str,
    request: impl Message,
    timeout: Duration,
) -> Result<Rep, String> {
    let (_, bytes) = app
        .bus
        .call_stamped(
            topic,
            request_type,
            request.encode_to_vec(),
            None,
            Some(timeout),
            &admin.stamp,
        )
        .await
        .map_err(|failed| match failed {
            BusError::HandlerFailed { detail, .. } => detail,
            other => other.to_string(),
        })?;
    Rep::decode(&bytes[..]).map_err(|failed| format!("an undecodable reply: {failed}"))
}

fn is_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !name.contains("--")
}

// ── The registry, passed through ────────────────────────────────────────────

/// What a push may do, and nothing else: ask whether a layer is there, send
/// one, and last name them in a manifest (W8.1).
enum Step<'a> {
    Push,
    Manifest(&'a str),
}

fn step<'a>(method: &Method, rest: &'a str) -> Result<Step<'a>, StatusCode> {
    let segments: Vec<&str> = rest.split('/').collect();
    let allowed = match (segments.as_slice(), method.as_str()) {
        (["blobs", digest], "HEAD") if digest.starts_with("sha256:") => return Ok(Step::Push),
        (["blobs", "uploads", ""] | ["blobs", "uploads"], "POST") => return Ok(Step::Push),
        (["blobs", "uploads", id], "PATCH" | "PUT") if !id.is_empty() => return Ok(Step::Push),
        (["manifests", tag], "PUT") if !tag.is_empty() => return Ok(Step::Manifest(tag)),
        (["blobs", _] | ["blobs", "uploads", ..] | ["manifests", _], _) => false,
        _ => return Err(StatusCode::NOT_FOUND),
    };
    debug_assert!(!allowed);
    // A known path, asked the wrong way: reading back and deleting are not
    // this path's, whatever the registry would answer.
    Err(StatusCode::METHOD_NOT_ALLOWED)
}

/// The registry's Location, as the client must follow it: through here.
fn rewritten(location: &str) -> String {
    let path = match location.find("/v2/") {
        Some(at) => &location[at..],
        None => location,
    };
    format!("/terminal/registry{path}")
}

const PASSED: [&str; 8] = [
    "content-type",
    "content-length",
    "content-range",
    "range",
    "docker-content-digest",
    "docker-upload-uuid",
    "oci-subject",
    "accept",
];

async fn pass_through(
    State(app): State<Arc<App>>,
    Path((name, rest)): Path<(String, String)>,
    request: Request,
) -> Response {
    if let Err(refusal) = admin(&app, request.headers()).await {
        return *refusal;
    }
    let Some(registry) = &app.registry else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "this deployment runs no registry",
        );
    };
    if !is_name(&name) {
        return refused(StatusCode::NOT_FOUND, "not a plugin's name");
    }
    match step(request.method(), &rest) {
        Err(status) => return refused(status, "not a step of pushing a plugin's image"),
        Ok(Step::Manifest(version)) => {
            // A tag is a version, and a recorded version is never replaced.
            let catalogue = catalogue(&app).await;
            let recorded = catalogue.is_ok_and(|held| {
                held.versions.iter().any(|v| {
                    v.metadata
                        .as_ref()
                        .is_some_and(|m| m.name == name && m.version == version)
                })
            });
            if recorded {
                return refused(
                    StatusCode::CONFLICT,
                    format!("{name} {version} is recorded already; an uploaded version is never replaced"),
                );
            }
        }
        Ok(Step::Push) => {}
    }

    if !mounts_from_a_plugin(request.uri().query()) {
        return refused(
            StatusCode::FORBIDDEN,
            "a blob is mounted from another plugin's repository, and from nowhere else",
        );
    }

    let (parts, body) = request.into_parts();
    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!("{}/v2/plugins/{name}/{rest}{query}", registry.base);
    let mut forwarded = reqwest::header::HeaderMap::new();
    for (header, value) in &parts.headers {
        if PASSED.contains(&header.as_str()) {
            forwarded.append(header.clone(), value.clone());
        }
    }
    let answer = registry
        .client
        .request(parts.method, url)
        .headers(forwarded)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await;
    let answer = match answer {
        Ok(answer) => answer,
        Err(failed) => {
            return refused(
                StatusCode::BAD_GATEWAY,
                format!("the registry did not answer: {failed}"),
            )
        }
    };
    let mut response = Response::builder().status(answer.status());
    for (header, value) in answer.headers() {
        if header == LOCATION {
            if let Ok(location) = value.to_str() {
                response = response.header(LOCATION, rewritten(location));
            }
        } else if PASSED.contains(&header.as_str()) {
            response = response.header(header, value);
        }
    }
    response
        .body(Body::from_stream(answer.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// Whether every repository a request mounts a blob `from` is a plugin's.
/// The registry links a blob from any repository it holds into this one,
/// and what it holds beyond `plugins/` is none of an upload's business.
fn mounts_from_a_plugin(query: Option<&str>) -> bool {
    query.unwrap_or_default().split('&').all(|pair| {
        let Some(from) = pair.strip_prefix("from=") else {
            return true;
        };
        let from = from.replace("%2F", "/").replace("%2f", "/");
        from.strip_prefix("plugins/").is_some_and(is_name)
    })
}

/// W8.5 and W8.6: a live instance's files, output and events, for a deployment
/// admin's terminal, relayed to the instance's sidecar as them. The sidecar
/// answers whether the instance is live on a development deployment; this
/// holds the request to one of the three paths and the size of a change.
async fn develop(
    State(app): State<Arc<App>>,
    Path((instance, what)): Path<(String, String)>,
    request: Request,
) -> Response {
    let person = match admin(&app, request.headers()).await {
        Ok(admin) => admin.person,
        Err(refusal) => return *refusal,
    };
    let Some(plugins) = app.plugins.clone() else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "this dashboard reaches no plugin's sidecar: it needs its own address",
        );
    };
    if !crate::plugins::is_instance(&instance)
        || !matches!(what.as_str(), "files" | "output" | "events")
    {
        return refused(StatusCode::NOT_FOUND, "not a development path");
    }
    let method = request.method().clone();
    let query = request.uri().query().map(String::from);
    // A change's limit, and a little for the JSON around it.
    let body = match axum::body::to_bytes(request.into_body(), (16 << 20) + (64 << 10)).await {
        Ok(body) => body,
        Err(_) => {
            return refused(
                StatusCode::PAYLOAD_TOO_LARGE,
                "a change is at most 16 MB: a plugin's source, not its dependencies",
            )
        }
    };
    let asked = crate::plugins::Development {
        method,
        what,
        query,
        body,
    };
    plugins
        .develop(&instance, &person, asked, app.clock.now_ns())
        .await
}

/// Whether the registry holds this manifest in plugins/{name}.
async fn has_manifest(registry: &Registry, name: &str, digest: &str) -> Result<bool, String> {
    let answer = registry
        .client
        .head(format!(
            "{}/v2/plugins/{name}/manifests/{digest}",
            registry.base
        ))
        .header(
            "accept",
            "application/vnd.oci.image.manifest.v1+json, \
             application/vnd.oci.image.index.v1+json, \
             application/vnd.docker.distribution.manifest.v2+json, \
             application/vnd.docker.distribution.manifest.list.v2+json",
        )
        .send()
        .await
        .map_err(|failed| format!("the registry did not answer: {failed}"))?;
    Ok(answer.status().is_success())
}

// ── The catalogue ───────────────────────────────────────────────────────────

async fn catalogue(app: &App) -> Result<PluginCatalogue, String> {
    catalogue_within(app, Duration::from_secs(10)).await
}

/// The instances launched and not stopped, with the plugin each runs: for
/// the home page, which names both, and a deployment admin's, which lists
/// them all. None, said in the log, when the catalogue cannot be read in
/// time, since a page that waits on the conductor is a page that hangs.
pub(crate) async fn launches(app: &App) -> Vec<PluginLaunch> {
    match catalogue_within(app, Duration::from_secs(3)).await {
        Ok(held) => held
            .launches
            .into_iter()
            .filter(|launch| launch.state == PluginLaunchState::Launched as i32)
            .collect(),
        Err(failed) => {
            tracing::warn!("the launched plugins could not be read: {failed}");
            Vec::new()
        }
    }
}

async fn catalogue_within(app: &App, within: Duration) -> Result<PluginCatalogue, String> {
    let (_, bytes) = app
        .bus
        .call(
            PLUGIN_CATALOGUE,
            "meridian.v1.PluginCatalogueRequest",
            PluginCatalogueRequest {}.encode_to_vec(),
            None,
            Some(within),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    PluginCatalogue::decode(&bytes[..]).map_err(|failed| failed.to_string())
}

#[derive(serde::Deserialize)]
struct Upload {
    name: String,
    version: String,
    #[serde(default)]
    roles: Vec<String>,
    /// Read only to refuse: a plugin declares no tags (decisions/026), and a
    /// CLI built before that still sends what its pyproject names.
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    interface: bool,
    sdk_version: String,
    image_digest: String,
    /// The version's declaration, as the CLI read it from the built image
    /// (W8.1, contract v11); none from a CLI or a plugin before v11.
    #[serde(default)]
    declaration: Option<crate::declaration::Declared>,
}

async fn upload(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (person, upload): (Admin, Upload) = match admin_asking(&app, &headers, &body).await {
        Ok(asked) => asked,
        Err(refusal) => return *refusal,
    };
    let Some(registry) = &app.registry else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "this deployment runs no registry",
        );
    };
    if !is_name(&upload.name) {
        return refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("`{}` is not a plugin's name", upload.name),
        );
    }
    if !upload.tags.is_empty() {
        return refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{} {} declares tags ({}); a plugin declares none, and a person's access to \
                 it is read or write, the same for every plugin (decisions/026). Remove \
                 `tags` from [tool.meridian]",
                upload.name,
                upload.version,
                upload.tags.join(", ")
            ),
        );
    }
    // The image first: a version recorded for an image nobody pushed would be
    // a launch waiting to fail.
    match has_manifest(registry, &upload.name, &upload.image_digest).await {
        Ok(true) => {}
        Ok(false) => {
            return refused(
                StatusCode::CONFLICT,
                format!(
                    "{} is not a manifest in plugins/{}; push the image first",
                    upload.image_digest, upload.name
                ),
            )
        }
        Err(failed) => return refused(StatusCode::BAD_GATEWAY, failed),
    }
    let declaration = match upload
        .declaration
        .as_ref()
        .map(crate::declaration::from_json)
    {
        None => None,
        Some(Ok(declaration)) => Some(declaration),
        Some(Err(why)) => return refused(StatusCode::UNPROCESSABLE_ENTITY, why),
    };
    let request = RecordPluginUploadRequest {
        metadata: Some(PluginMetadata {
            name: upload.name,
            version: upload.version,
            roles: upload.roles,
            interface: upload.interface,
            sdk_version: upload.sdk_version,
            declaration,
        }),
        image_digest: upload.image_digest,
    };
    let recorded: Result<PluginVersion, String> = ask(
        &app,
        &person,
        RECORD_PLUGIN_UPLOAD,
        "meridian.v1.RecordPluginUploadRequest",
        request,
        Duration::from_secs(10),
    )
    .await;
    match recorded {
        Ok(version) => {
            let metadata = version.metadata.unwrap_or_default();
            json(
                StatusCode::CREATED,
                serde_json::json!({
                    "name": metadata.name,
                    "version": metadata.version,
                    "image_digest": version.image_digest,
                    "uploaded_by": version.uploaded_by,
                    "uploaded_at": rfc3339(version.uploaded_at_ns),
                }),
            )
        }
        Err(reason) if reason.contains("already recorded") => refused(StatusCode::CONFLICT, reason),
        Err(reason) => refused(StatusCode::UNPROCESSABLE_ENTITY, reason),
    }
}

fn state_name(state: i32) -> &'static str {
    match PluginLaunchState::try_from(state) {
        Ok(PluginLaunchState::Launched) => "launched",
        Ok(PluginLaunchState::Stopped) => "stopped",
        Ok(PluginLaunchState::Failed) => "failed",
        _ => "unknown",
    }
}

async fn list(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if let Err(refusal) = admin(&app, &headers).await {
        return *refusal;
    }
    let held = match catalogue(&app).await {
        Ok(held) => held,
        Err(failed) => return refused(StatusCode::BAD_GATEWAY, failed),
    };
    let versions: Vec<serde_json::Value> = held
        .versions
        .iter()
        .map(|version| {
            let m = version.metadata.clone().unwrap_or_default();
            serde_json::json!({
                "name": m.name, "version": m.version, "roles": m.roles,
                "interface": m.interface, "sdk_version": m.sdk_version,
                "image_digest": version.image_digest,
                "declaration": m.declaration.as_ref().map(crate::declaration::to_json),
            })
        })
        .collect();
    let launches: Vec<serde_json::Value> = held
        .launches
        .iter()
        .map(|launch| {
            serde_json::json!({
                "instance_id": launch.instance_id, "name": launch.name,
                "version": launch.version, "state": state_name(launch.state),
                "failure": launch.failure, "live": launch.live,
            })
        })
        .collect();
    json(
        StatusCode::OK,
        serde_json::json!({ "versions": versions, "launches": launches }),
    )
}

#[derive(serde::Deserialize)]
struct Launch {
    name: String,
    version: String,
    instance_id: String,
    #[serde(default)]
    approved_roles: Vec<String>,
    /// In the live shape; the launcher refuses it on a deployment not
    /// installed for development (W8.3).
    #[serde(default)]
    live: bool,
    /// Why, kept with the launch (contract v17); the CLI sends none yet.
    #[serde(default)]
    note: String,
}

async fn launch(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (person, asked): (Admin, Launch) = match admin_asking(&app, &headers, &body).await {
        Ok(asked) => asked,
        Err(refusal) => return *refusal,
    };
    let request = LaunchPluginRequest {
        name: asked.name,
        version: asked.version,
        instance_id: asked.instance_id,
        approved_roles: asked.approved_roles,
        live: asked.live,
        note: asked.note,
    };
    // Longer than the conductor gives the launcher, so its answer arrives.
    let launched: Result<PluginLaunch, String> = ask(
        &app,
        &person,
        LAUNCH_PLUGIN,
        "meridian.v1.LaunchPluginRequest",
        request,
        Duration::from_secs(40),
    )
    .await;
    match launched {
        Ok(launch) => json(
            StatusCode::CREATED,
            serde_json::json!({ "instance_id": launch.instance_id, "state": state_name(launch.state) }),
        ),
        Err(reason) if reason.contains("not in the catalogue") => {
            refused(StatusCode::NOT_FOUND, reason)
        }
        Err(reason) if reason.contains("no approval") || reason.contains("already launched") => {
            refused(StatusCode::CONFLICT, reason)
        }
        Err(reason) if reason.contains("launcher") => refused(StatusCode::BAD_GATEWAY, reason),
        Err(reason) => refused(StatusCode::UNPROCESSABLE_ENTITY, reason),
    }
}

#[derive(serde::Deserialize)]
struct Stop {
    instance_id: String,
    /// Why, kept with the stop (contract v17); the CLI sends none yet.
    #[serde(default)]
    note: String,
}

async fn stop(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (person, asked): (Admin, Stop) = match admin_asking(&app, &headers, &body).await {
        Ok(asked) => asked,
        Err(refusal) => return *refusal,
    };
    let stopped: Result<PluginLaunch, String> = ask(
        &app,
        &person,
        STOP_PLUGIN,
        "meridian.v1.StopPluginRequest",
        StopPluginRequest {
            instance_id: asked.instance_id,
            note: asked.note,
        },
        Duration::from_secs(40),
    )
    .await;
    match stopped {
        Ok(launch) => json(
            StatusCode::OK,
            serde_json::json!({ "instance_id": launch.instance_id, "state": state_name(launch.state) }),
        ),
        Err(reason) if reason.contains("is live") => refused(StatusCode::NOT_FOUND, reason),
        Err(reason) => refused(StatusCode::BAD_GATEWAY, reason),
    }
}

#[cfg(test)]
mod tests;
