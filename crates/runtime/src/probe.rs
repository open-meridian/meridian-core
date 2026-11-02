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
//!   `full_page_writes`, which integrity at rest needs on, and whether it is
//!   a primary), and the API server where the member holds a lease.
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

/// Reach `address` by TCP within [`WITHIN`].
fn reach(path: &str, address: &str) -> Value {
    let started = Instant::now();
    let reached = address
        .to_socket_addrs()
        .map_err(|failed| format!("{address} did not resolve: {failed}"))
        .and_then(|mut addrs| {
            addrs
                .next()
                .ok_or_else(|| format!("{address} resolved to nothing"))
        })
        .and_then(|socket| {
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
fn database(url: &str) -> Value {
    let Some(address) = address_of(url, 5432) else {
        return json!({ "path": "database", "ok": false, "why": "no database address is set" });
    };
    let mut found = reach("database", &address);
    if found["ok"] != json!(true) {
        return found;
    }
    let started = Instant::now();
    let mut config: postgres::Config = match url.parse() {
        Ok(config) => config,
        Err(failed) => {
            found["ok"] = json!(false);
            found["why"] = json!(format!("the database's URL did not read: {failed}"));
            return found;
        }
    };
    config.connect_timeout(WITHIN);
    match config.connect(postgres::NoTls) {
        Ok(mut client) => {
            let setting = |client: &mut postgres::Client, name: &str| {
                client
                    .query_one(&format!("SHOW {name}"), &[])
                    .map(|row| row.get::<_, String>(0))
                    .unwrap_or_default()
            };
            found["data_checksums"] = json!(setting(&mut client, "data_checksums"));
            found["full_page_writes"] = json!(setting(&mut client, "full_page_writes"));
            found["primary"] = json!(client
                .query_one("SELECT NOT pg_is_in_recovery()", &[])
                .map(|row| row.get::<_, bool>(0))
                .unwrap_or(false));
            found["ms"] = json!(started.elapsed().as_millis() as u64);
        }
        Err(failed) => {
            found["ok"] = json!(false);
            found["why"] = json!(format!(
                "{address} refused a session: {}",
                failed
                    .as_db_error()
                    .map_or(failed.to_string(), |e| e.message().to_string())
            ));
        }
    }
    found
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
        found.push(database(&url));
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
            Some(runtime.block_on(answers(domain)))
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
