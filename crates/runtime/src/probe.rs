//! A member's probe of every path it needs (step 3b; spec/a-deployment-
//! scales-out-and-fails-over-by-configuration section 10; plan 5i; ruling
//! 1051): what `meridian status` and `meridian doctor` read, run in the pod
//! as `meridian-<component> probe`, and what CI's failover tests time a
//! takeover by.
//!
//! One JSON object on stdout, never a credential: each path's address as
//! `host:port` only, whether it answered and in how long, and why not.
//! - `member`: this member's mode, lag and partitions, as it wrote them
//!   beside its readiness (`member.json`), and whether it is ready.
//! - `paths`: the broker, the database (with its `data_checksums` and
//!   `full_page_writes`, which integrity at rest needs on, whether it is a
//!   primary, and where it archives its WAL, the archiver's last segment
//!   and whether it is failing), and the API server where the member holds
//!   a lease.
//! - `answers`: the component's change log asked on the bus, as a follower
//!   asks it -- answered by whichever member writes -- with the epoch and
//!   head it answered.
//!
//! Exit 0 when every path answers and the component does; 1 otherwise.

use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

use prost::Message as _;
use serde_json::{json, Value};

use crate::var;

/// How long each path is given.
const WITHIN: Duration = Duration::from_secs(2);

/// What a probe names a component's paths by.
pub struct Paths {
    pub component: &'static str,
    /// The change log's domain the component answers on, if it keeps one.
    pub domain: Option<&'static str>,
    /// The variable its database's URL is in, if it has one.
    pub database: Option<&'static str>,
    /// The prefix its record's tables are kept under, whose last scrub and
    /// integrity findings the probe reports (step 3b); none for a component
    /// with no record.
    pub record: Option<&'static str>,
}

/// `host:port` of a URL, nothing else of it: never a user or a password.
pub fn address_of(url: &str, default_port: u16) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    let host_port = rest.split(['/', '?', ',']).next()?.trim();
    if host_port.is_empty() {
        return None;
    }
    if host_port.starts_with('[') {
        return Some(host_port.to_string());
    }
    match host_port.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            Some(format!("{host}:{port}"))
        }
        _ => Some(format!("{host_port}:{default_port}")),
    }
}

/// `work`'s answer, or `None` where it took longer than `within`. The work
/// is left running on a thread of its own, never waited for: the probe
/// answers in its bound and exits.
fn within<T, F>(within: Duration, work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (said, heard) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = said.send(work());
    });
    heard.recv_timeout(within).ok()
}

/// `address` resolved within [`WITHIN`]. The system's resolver is never
/// waited on past it: with the cluster's DNS gone (CI 42300989974: the one
/// CoreDNS was on the node stopped), a name's lookup is retried for tens of
/// seconds, and a probe that waited would answer nothing at all -- not even
/// the member's own mode, which it reads from a file.
fn resolved(address: &str) -> Result<std::net::SocketAddr, String> {
    let asked = address.to_string();
    match within(WITHIN, move || {
        asked.to_socket_addrs().map(|mut addrs| addrs.next())
    }) {
        None => Err(format!(
            "{address} did not resolve within {} s: is the cluster's DNS answering?",
            WITHIN.as_secs()
        )),
        Some(Err(failed)) => Err(format!("{address} did not resolve: {failed}")),
        Some(Ok(None)) => Err(format!("{address} resolved to nothing")),
        Some(Ok(Some(socket))) => Ok(socket),
    }
}

/// Reach `address` by TCP within [`WITHIN`], its name resolved within it.
fn reach(path: &str, address: &str) -> Value {
    let started = Instant::now();
    let reached = resolved(address).and_then(|socket| {
        std::net::TcpStream::connect_timeout(&socket, WITHIN).map_err(|failed| {
            format!(
                "{address} did not answer within {} s: {failed}",
                WITHIN.as_secs()
            )
        })
    });
    let ms = started.elapsed().as_millis() as u64;
    match reached {
        Ok(_) => json!({ "path": path, "address": address, "ok": true, "ms": ms }),
        Err(why) => json!({ "path": path, "address": address, "ok": false, "ms": ms, "why": why }),
    }
}

/// The database reached, and what integrity at rest asks of it.
fn database(url: &str, record: Option<&str>) -> Value {
    let Some(address) = address_of(url, 5432) else {
        return json!({ "path": "database", "ok": false, "why": "no database address is set" });
    };
    let mut found = reach("database", &address);
    if found["ok"] != json!(true) {
        return found;
    }
    let started = Instant::now();
    let config: postgres::Config = match url.parse() {
        Ok(config) => config,
        Err(failed) => {
            found["ok"] = json!(false);
            found["why"] = json!(format!("the database's URL did not read: {failed}"));
            return found;
        }
    };
    // A session, and what it is asked, never waited on past twice the
    // bound: a primary being replaced can take a connection and not answer.
    let record = record.map(str::to_string);
    let named = meridian_changes::tls::names_ssl_mode(url);
    let session = within(WITHIN * 2, move || {
        session(config, named, record.as_deref())
    });
    match session {
        Some(Ok(said)) => {
            for (key, value) in said {
                found[key] = value;
            }
            found["ms"] = json!(started.elapsed().as_millis() as u64);
        }
        Some(Err(failed)) => {
            found["ok"] = json!(false);
            found["why"] = json!(format!("{address} refused a session: {failed}"));
        }
        None => {
            found["ok"] = json!(false);
            found["why"] = json!(format!(
                "{address} did not answer a session within {} s",
                (WITHIN * 2).as_secs()
            ));
        }
    }
    found
}

/// One session's answers: the settings integrity at rest needs, whether it
/// is a primary, and the record's last scrub and findings.
fn session(
    mut config: postgres::Config,
    named: bool,
    record: Option<&str>,
) -> Result<Vec<(&'static str, Value)>, String> {
    config.connect_timeout(WITHIN);
    // Over TLS where the deployment's internal TLS is on (ruling 1084).
    let (config, tls) = meridian_changes::tls::secured(config, named)
        .map_err(|failed| format!("the database's TLS: {failed}"))?;
    let mut client = config.connect(tls).map_err(|failed| {
        failed
            .as_db_error()
            .map_or(failed.to_string(), |e| e.message().to_string())
    })?;
    let mut setting = |name: &str| {
        client
            .query_one(&format!("SHOW {name}"), &[])
            .map(|row| row.get::<_, String>(0))
            .unwrap_or_default()
    };
    let mut said = vec![
        ("data_checksums", json!(setting("data_checksums"))),
        ("full_page_writes", json!(setting("full_page_writes"))),
    ];
    let archiving = setting("archive_mode");
    said.push((
        "primary",
        json!(client
            .query_one("SELECT NOT pg_is_in_recovery()", &[])
            .map(|row| row.get::<_, bool>(0))
            .unwrap_or(false)),
    ));
    // The WAL archive (backup.walArchive), where the database keeps one:
    // when it last archived, and whether it is failing now -- the newest
    // point a restore into a new cluster reaches is no later than that.
    if archiving == "on" {
        said.push(("archive", archived(&mut client)));
    }
    // Its record's last scrub and findings, where it keeps them.
    if let Some(prefix) = record {
        if let Ok(found) = meridian_changes::integrity::last(&mut client, prefix) {
            said.push(("integrity", found));
        }
    }
    Ok(said)
}

/// The archiver's own account (`pg_stat_archiver`): its last segment
/// archived, how long ago, and whether its newest attempt failed. Times in
/// UTC; nothing of a segment's content.
fn archived(client: &mut postgres::Client) -> Value {
    let asked = client.query_one(
        "SELECT coalesce(to_char(last_archived_time AT TIME ZONE 'UTC', \
         'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), ''), \
         coalesce(extract(epoch FROM now() - last_archived_time)::bigint, -1), \
         failed_count, \
         coalesce(last_failed_time > coalesce(last_archived_time, '-infinity'), false) \
         FROM pg_stat_archiver",
        &[],
    );
    match asked {
        Ok(row) => {
            let at: String = row.get(0);
            let ago: i64 = row.get(1);
            json!({
                "last_archived_at": at,
                "seconds_since": (ago >= 0).then_some(ago),
                "failed": row.get::<_, i64>(2),
                "failing": row.get::<_, bool>(3),
            })
        }
        Err(failed) => json!({ "why": failed.to_string() }),
    }
}

/// Ask the component's change log on the bus, as a follower asks it.
async fn answers(domain: &str) -> Value {
    let topic = meridian_changes::read_changes_topic(domain);
    let Some(url) = var("MERIDIAN_BROKER_URL") else {
        return json!({ "topic": topic, "ok": false, "why": "no broker is set" });
    };
    let started = Instant::now();
    let backend = match tokio::time::timeout(WITHIN, meridian_bus::NatsBackend::connect(&url)).await
    {
        Ok(Ok(backend)) => backend,
        Ok(Err(failed)) => {
            return json!({ "topic": topic, "ok": false, "why": format!("the broker refused: {failed}") })
        }
        Err(_) => {
            return json!({ "topic": topic, "ok": false, "why": "the broker did not answer within 2 s" })
        }
    };
    let probing = format!("{}-probe", var("MERIDIAN_INSTANCE_ID").unwrap_or_default());
    let bus = meridian_bus::Bus::single(&probing, std::sync::Arc::new(backend), crate::clock());
    let asked = meridian_changes::ReadChangesRequest {
        partition: meridian_changes::CONTROL.to_string(),
        after_sequence: i64::MAX as u64,
        page_size: 1,
    };
    match bus
        .call(
            &topic,
            "meridian.v1.ReadChangesRequest",
            asked.encode_to_vec(),
            None,
            Some(WITHIN),
        )
        .await
    {
        Ok((_, body)) => {
            let reply =
                meridian_changes::ReadChangesReply::decode(body.as_slice()).unwrap_or_default();
            json!({ "topic": topic, "ok": true, "ms": started.elapsed().as_millis() as u64,
                    "epoch": reply.epoch, "head": reply.head })
        }
        Err(failed) => {
            json!({ "topic": topic, "ok": false, "ms": started.elapsed().as_millis() as u64,
                               "why": failed.to_string() })
        }
    }
}

/// What this member wrote of itself beside its readiness.
fn member() -> Value {
    let Some(ready) = var("MERIDIAN_READY_FILE").map(std::path::PathBuf::from) else {
        return json!({});
    };
    let mut said: Value = ready
        .parent()
        .and_then(|dir| std::fs::read_to_string(dir.join("member.json")).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    said["ready"] = json!(ready.exists());
    said
}

/// Probe every path, print what was found, and answer whether all held.
pub fn run(paths: &Paths) -> Result<(), String> {
    let mut found = Vec::new();
    if let Some(url) = var("MERIDIAN_BROKER_URL") {
        if let Some(address) = address_of(&url, 4222) {
            found.push(reach("broker", &address));
        }
    }
    if let Some(url) = paths.database.and_then(var) {
        found.push(database(&url, paths.record));
    }
    if var("MERIDIAN_LEASE_NAME").is_some() {
        if let Some(host) = var("KUBERNETES_SERVICE_HOST") {
            let port = var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|| "443".into());
            let address = if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            };
            found.push(reach("api-server", &address));
        }
    }
    let answered = match paths.domain {
        Some(domain) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|failed| failed.to_string())?;
            let answered = runtime.block_on(answers(domain));
            // Never waiting on a lookup the broker's client left running
            // on the blocking pool: the answer is in, within its bound.
            runtime.shutdown_background();
            Some(answered)
        }
        None => None,
    };
    let held = found.iter().all(|path| path["ok"] == json!(true))
        && answered.as_ref().is_none_or(|a| a["ok"] == json!(true));
    let said = json!({
        "component": paths.component,
        "member": member(),
        "paths": found,
        "answers": answered,
        "ok": held,
    });
    println!("{said}");
    if held {
        Ok(())
    } else {
        Err(format!(
            "{}: a path it needs did not answer",
            paths.component
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_past_its_bound_is_not_waited_for() {
        let started = Instant::now();
        let said = within(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(5));
            1
        });
        assert_eq!(said, None);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(within(Duration::from_secs(1), || 2), Some(2));
    }

    #[test]
    fn a_name_that_does_not_resolve_says_so_in_its_bound() {
        let started = Instant::now();
        let said = reach("broker", "no-such-name.invalid:4222");
        assert_eq!(said["ok"], json!(false));
        assert!(said["why"]
            .as_str()
            .unwrap()
            .contains("no-such-name.invalid:4222"));
        assert!(started.elapsed() <= WITHIN + Duration::from_millis(500));
    }

    #[test]
    fn an_address_never_carries_a_credential() {
        assert_eq!(
            address_of("nats://lake:s3cret@broker.ns.svc:4222", 4222).as_deref(),
            Some("broker.ns.svc:4222")
        );
        assert_eq!(
            address_of("postgres://app:pw@db:5432/meridian?sslmode=disable", 5432).as_deref(),
            Some("db:5432")
        );
        assert_eq!(
            address_of("postgres://app@db/meridian", 5432).as_deref(),
            Some("db:5432")
        );
        assert_eq!(
            address_of("nats://a:b@n1:4222,nats://n2:4222", 4222).as_deref(),
            Some("n1:4222")
        );
        assert_eq!(address_of("", 1), None);
    }
}
