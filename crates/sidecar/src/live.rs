//! A live plugin's development endpoint (W8.5, W8.6;
//! spec/live-plugin-development, requirement 8).
//!
//! On a development deployment, in the chart's live shape, the plugin runs
//! the SDK's dev runner from a folder this sidecar shares with it. Three paths
//! under the front door, each needing the dashboard's assertion for this
//! instance as every request there does, and none ever passed to the plugin:
//!
//! - `PUT /.meridian/dev/files`: files and deletions, written whole into the
//!   live folder, then the revision, which is what the runner restarts on --
//!   so a change half-written never runs;
//! - `GET /.meridian/dev/output?since=<revision>`: what the plugin printed;
//! - `GET /.meridian/dev/events?since=<revision>`: the runner's events and
//!   this sidecar's own -- `synced`, and what it refused the plugin.
//!
//! Anywhere else -- not the live shape, or not a development deployment --
//! there is no endpoint, and each path is a 404 answered here.
//!
//! Everything written is group-writable: the plugin runs as another user in
//! the pod's shared group, and replaces nothing of this sidecar's, but this
//! replaces what it seeded.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

/// Where the development paths are, under the front door.
pub const DEV_PREFIX: &str = "/.meridian/dev/";
/// The folder's own bookkeeping, which nobody sends a file into.
const BOOKS: &str = ".meridian";
/// The most one change may be: a plugin's source, not its dependencies.
pub const MOST: usize = 16 << 20;
/// Events kept of this sidecar's own.
const EVENTS_KEPT: usize = 2000;

pub struct Live {
    dir: PathBuf,
    /// One change at a time, so two cannot interleave their files and both
    /// claim the next revision.
    writing: Mutex<()>,
}

/// A change, as the dashboard relays it from `meridian plugin dev`.
#[derive(serde::Deserialize, Default)]
pub struct Change {
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub deleted: Vec<String>,
}

/// A path in the live folder, held to it: relative, no `..`, nothing absolute,
/// and not the folder's own bookkeeping.
pub fn held(path: &str) -> Result<PathBuf, String> {
    let as_given = Path::new(path);
    if path.is_empty() || as_given.is_absolute() {
        return Err(format!("`{path}` is not a path in the plugin"));
    }
    let mut kept = PathBuf::new();
    for part in as_given.components() {
        match part {
            Component::Normal(name) => kept.push(name),
            Component::CurDir => {}
            _ => return Err(format!("`{path}` reaches outside the plugin")),
        }
    }
    match kept.components().next() {
        None => Err(format!("`{path}` is not a path in the plugin")),
        Some(Component::Normal(first)) if first == BOOKS => {
            Err(format!("`{path}` is the live folder's own bookkeeping"))
        }
        _ => Ok(kept),
    }
}

impl Live {
    pub fn new(dir: impl Into<PathBuf>) -> Live {
        Live {
            dir: dir.into(),
            writing: Mutex::new(()),
        }
    }

    /// Only both: the live folder, and a deployment installed for
    /// development. Either alone is no endpoint.
    pub fn from_env(dir: Option<String>, development: bool) -> Option<Live> {
        match (dir, development) {
            (Some(dir), true) if !dir.is_empty() => Some(Live::new(dir)),
            _ => None,
        }
    }

    fn books(&self) -> PathBuf {
        self.dir.join(BOOKS)
    }

    pub fn revision(&self) -> u64 {
        std::fs::read_to_string(self.books().join("revision"))
            .ok()
            .and_then(|held| held.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Write a change: every path checked and every file decoded first, so a
    /// bad one writes nothing; then the files, each whole; then the revision.
    pub fn apply(&self, change: Change) -> Result<u64, (StatusCode, String)> {
        let unprocessable = |said: String| (StatusCode::UNPROCESSABLE_ENTITY, said);
        let mut writes = Vec::new();
        for (path, encoded) in &change.files {
            let at = held(path).map_err(unprocessable)?;
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|failed| unprocessable(format!("`{path}` is not base64: {failed}")))?;
            writes.push((at, bytes));
        }
        let mut deletions = Vec::new();
        for path in &change.deleted {
            deletions.push(held(path).map_err(unprocessable)?);
        }

        let _one = self.writing.lock().expect("live lock poisoned");
        let failed = |why: std::io::Error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the live folder could not be written: {why}"),
            )
        };
        for (at, bytes) in &writes {
            replace(&self.dir.join(at), bytes).map_err(failed)?;
        }
        for at in &deletions {
            match std::fs::remove_file(self.dir.join(at)) {
                Ok(()) => {}
                Err(gone) if gone.kind() == std::io::ErrorKind::NotFound => {}
                Err(other) => return Err(failed(other)),
            }
        }
        let next = self.revision() + 1;
        replace(&self.books().join("revision"), next.to_string().as_bytes()).map_err(failed)?;
        self.record(
            next,
            "synced",
            serde_json::json!({ "files": writes.len(), "deleted": deletions.len() }),
        );
        Ok(next)
    }

    /// One of this sidecar's own events, at the revision it is about.
    pub fn record(&self, revision: u64, event: &str, detail: serde_json::Value) {
        let mut line = serde_json::json!({
            "revision": revision,
            "event": event,
            "at": now_seconds(),
            "by": "sidecar",
        });
        if let (Some(line), serde_json::Value::Object(more)) = (line.as_object_mut(), detail) {
            line.extend(more);
        }
        let path = self.books().join("sidecar-events.jsonl");
        let _ = append_kept(&path, &line.to_string(), EVENTS_KEPT);
    }

    /// Something the plugin was refused, recorded against the revision it
    /// is running.
    pub fn refused(&self, reason: &str) {
        self.record(
            self.revision(),
            "refused",
            serde_json::json!({ "reason": reason }),
        );
    }

    fn lines(&self, file: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.books().join(file))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// What the plugin printed after a revision, or all of it kept when no
    /// revision is named -- the first run is revision 0, which nothing is after.
    pub fn output_since(&self, since: Option<u64>) -> serde_json::Value {
        let lines: Vec<serde_json::Value> = self
            .lines("output.jsonl")
            .into_iter()
            .filter(|entry| after(entry, since))
            .map(|entry| entry["line"].clone())
            .collect();
        serde_json::json!({ "revision": self.revision(), "lines": lines })
    }

    /// The runner's events and this sidecar's, after a revision, in the
    /// order they happened.
    pub fn events_since(&self, since: Option<u64>) -> serde_json::Value {
        let mut events: Vec<serde_json::Value> = self
            .lines("runner-events.jsonl")
            .into_iter()
            .chain(self.lines("sidecar-events.jsonl"))
            .filter(|entry| after(entry, since))
            .collect();
        events.sort_by(|a, b| {
            let at = |e: &serde_json::Value| e["at"].as_f64().unwrap_or(0.0);
            at(a).total_cmp(&at(b))
        });
        serde_json::json!({ "revision": self.revision(), "events": events })
    }
}

fn after(entry: &serde_json::Value, since: Option<u64>) -> bool {
    since.is_none_or(|since| entry["revision"].as_u64().unwrap_or(0) > since)
}

/// Answer a development path, `what` being what follows the prefix.
pub async fn answer(live: &Live, what: &str, request: Request) -> Response {
    let (endpoint, query) = what.split_once('?').unwrap_or((what, ""));
    let since = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("since="))
        .and_then(|held| held.parse().ok());
    let json =
        |status: StatusCode, body: serde_json::Value| (status, axum::Json(body)).into_response();
    match (request.method().clone(), endpoint) {
        (Method::PUT, "files") => {
            let body = match to_bytes(request.into_body(), MOST).await {
                Ok(body) => body,
                Err(_) => {
                    return (
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                        "a change is at most {MOST} bytes: a plugin's source, not its dependencies"
                    ),
                    )
                        .into_response()
                }
            };
            let change: Change = match serde_json::from_slice(&body) {
                Ok(change) => change,
                Err(failed) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        format!("the change does not read: {failed}"),
                    )
                        .into_response()
                }
            };
            match live.apply(change) {
                Ok(revision) => json(StatusCode::OK, serde_json::json!({ "revision": revision })),
                Err((status, said)) => (status, said).into_response(),
            }
        }
        (Method::GET, "output") => json(StatusCode::OK, live.output_since(since)),
        (Method::GET, "events") => json(StatusCode::OK, live.events_since(since)),
        _ => (StatusCode::NOT_FOUND, "not a development path").into_response(),
    }
}

/// Written beside and renamed over, group-writable, its folders too.
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    make_dirs(parent)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let beside = parent.join(format!(".{name}.meridian-partial"));
    std::fs::write(&beside, bytes)?;
    group_writable(&beside, 0o664)?;
    std::fs::rename(&beside, path)
}

fn make_dirs(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        make_dirs(parent)?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => group_writable(dir, 0o2775),
        Err(there) if there.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(other) => Err(other),
    }
}

#[cfg(unix)]
fn group_writable(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn group_writable(_: &Path, _: u32) -> std::io::Result<()> {
    Ok(())
}

fn append_kept(path: &Path, line: &str, kept: usize) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        make_dirs(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    drop(file);
    let _ = group_writable(path, 0o664);
    let held = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = held.lines().collect();
    if lines.len() > kept {
        replace(
            path,
            (lines[lines.len() - kept..].join("\n") + "\n").as_bytes(),
        )?;
    }
    Ok(())
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests;
