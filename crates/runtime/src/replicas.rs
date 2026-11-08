//! Replica integrity (step 3b part 4; the product owner's ruling of
//! 2026-11-07, item 4): the database's members compared, and a standby
//! whose copy is bad re-cloned.
//!
//! Each store's own scrub re-reads its record on the primary (integrity at
//! rest, [`meridian_changes::integrity`]). A standby copies what it is
//! given, but its own disk can still rot beneath it, and a promotion would
//! then serve the rot. So the chart's replica scrub Job (`meridian-conductor
//! replicas`, on `database.brought.replicaScrub.schedule`, rendered only
//! where the database has standbys) reads every member in turn:
//!
//! - **What is compared.** Each store's log -- the captured change logs of
//!   the configuration, instrument and street stores, the lake's rows and
//!   the book's journal -- partition by partition, over the sequences every
//!   member holds: a digest of each row as it reads, in order. Every page
//!   is read, so a page that fails its checksum on a member (SQLSTATE
//!   XX001) is that member's finding too.
//! - **Who is wrong.** A standby whose digest differs from the primary's,
//!   or which failed a page, once more after it has replayed past the
//!   primary's position at the start (a lagging copy is not a bad one).
//!   The primary is never re-cloned: where both standbys agree against it,
//!   that is said, for a person to switch over.
//! - **What is done.** In `ha` (`--reclone`), the standby is re-cloned from
//!   the primary through Patroni's REST API (`POST /reinitialize`), with the
//!   Patroni credential the chart mounts in this Job alone, and its copy is
//!   compared again once it streams. Each finding is kept as an
//!   `IntegrityFinding` in the store whose log differed, naming the member
//!   and the outcome: re-cloned and matching, or not.
//!
//! The stores hold no new credential for any of it: the Job reads with the
//! serving role every store already has, and only it holds Patroni's. With
//! internal TLS, Patroni's REST API is asked over TLS by each member's own
//! name, its certificate verified by that name against the internal CA
//! (ruled 2026-11-08).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// A store's log, as this compares it: its table, partition and sequence.
pub struct Log {
    pub store: &'static str,
    /// The `<prefix>_integrity_finding` its findings are kept in.
    pub prefix: &'static str,
    pub table: &'static str,
    pub partition: &'static str,
    pub sequence: &'static str,
}

/// Every store's log on the brought database.
pub const LOGS: &[Log] = &[
    Log {
        store: "conductor",
        prefix: "config",
        table: "config_change",
        partition: "partition",
        sequence: "sequence",
    },
    Log {
        store: "instrument",
        prefix: "instrument",
        table: "instrument_change",
        partition: "partition",
        sequence: "sequence",
    },
    Log {
        store: "street",
        prefix: "street",
        table: "street_change",
        partition: "partition",
        sequence: "sequence",
    },
    Log {
        store: "lake",
        prefix: "lake",
        table: "lake_row",
        partition: "dataset",
        sequence: "sequence",
    },
    Log {
        store: "bor",
        prefix: "book",
        table: "book_entry",
        partition: "partition",
        sequence: "first_sequence",
    },
];

/// Each partition's lowest and highest sequence, by partition.
pub type Ranges = BTreeMap<String, (i64, i64)>;

/// Each log's findings, by its prefix: the member, by its index, and why.
type Found = BTreeMap<String, Vec<(usize, String)>>;

/// One member's answer for one log: each partition's row count and digest
/// over the range compared, or the page it could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    Digests(BTreeMap<String, (i64, Vec<u8>)>),
    /// A page failed its checksum, or the read failed otherwise.
    Unreadable(String),
    /// The table is not there (the store not migrated in this database).
    Absent,
}

/// What a member's copy is, against the primary's: the same, or which
/// logs differ and why.
pub fn differs(primary: &Read, member: &Read) -> Option<String> {
    match (primary, member) {
        (_, Read::Unreadable(why)) => Some(format!("its copy could not be read: {why}")),
        (Read::Digests(want), Read::Digests(have)) => {
            let mut said = Vec::new();
            for (partition, (count, digest)) in want {
                match have.get(partition) {
                    None => said.push(format!("partition {partition} is missing")),
                    Some((n, d)) if n != count || d != digest => said.push(format!(
                        "partition {partition}: {n} rows digest {} where the primary's {count} \
                         digest {}",
                        meridian_changes::integrity::short(d),
                        meridian_changes::integrity::short(digest)
                    )),
                    Some(_) => {}
                }
            }
            for partition in have.keys().filter(|p| !want.contains_key(*p)) {
                said.push(format!("partition {partition} is the member's alone"));
            }
            (!said.is_empty()).then(|| said.join("; "))
        }
        _ => None,
    }
}

/// Each partition's range every member holds: from the highest lowest
/// sequence (a primary's retention may have let rows go a standby still
/// holds) to the lowest highest (a standby may not have them yet).
pub fn common_ranges(each: &[BTreeMap<String, (i64, i64)>]) -> BTreeMap<String, (i64, i64)> {
    let mut common: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let Some((first, rest)) = each.split_first() else {
        return common;
    };
    for (partition, &(low, high)) in first {
        let mut range = (low, high);
        let mut everywhere = true;
        for other in rest {
            match other.get(partition) {
                Some(&(l, h)) => range = (range.0.max(l), range.1.min(h)),
                None => everywhere = false,
            }
        }
        if everywhere && range.0 <= range.1 {
            common.insert(partition.clone(), range);
        }
    }
    common
}

/// A member by its address: the URL's own host kept, so a TLS connection
/// is verified against the name every store connects by (internal TLS), and
/// the connection made to the member's address beside it (`hostaddr`).
fn connect(url: &str, address: std::net::IpAddr) -> Result<postgres::Client, String> {
    let mut config: postgres::Config = url
        .parse()
        .map_err(|failed| format!("the database URL is unreadable: {failed}"))?;
    config.hostaddr(address);
    config.connect_timeout(Duration::from_secs(10));
    let (config, tls) =
        meridian_changes::tls::secured(config, meridian_changes::tls::names_ssl_mode(url))
            .map_err(|failed| failed.to_string())?;
    config
        .connect(tls)
        .map_err(|failed| crate::preflight::with_causes(&failed))
}

fn exists(client: &mut postgres::Client, table: &str) -> Result<bool, String> {
    client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
        .map(|row| row.get(0))
        .map_err(|failed| failed.to_string())
}

/// Each partition's lowest and highest sequence on one member.
fn ranges(client: &mut postgres::Client, log: &Log) -> Result<Option<Ranges>, String> {
    if !exists(client, log.table)? {
        return Ok(None);
    }
    let rows = client
        .query(
            &format!(
                "SELECT {p}::text, min({s})::bigint, max({s})::bigint FROM {t} GROUP BY {p}",
                p = log.partition,
                s = log.sequence,
                t = log.table
            ),
            &[],
        )
        .map_err(|failed| crate::preflight::with_causes(&failed))?;
    Ok(Some(
        rows.iter()
            .map(|row| (row.get(0), (row.get(1), row.get(2))))
            .collect(),
    ))
}

/// One member's digests of one log over the common ranges: every row read,
/// in order, each as its whole text.
fn digests(
    client: &mut postgres::Client,
    log: &Log,
    common: &BTreeMap<String, (i64, i64)>,
) -> Read {
    match exists(client, log.table) {
        Ok(false) => return Read::Absent,
        Err(failed) => return Read::Unreadable(failed),
        Ok(true) => {}
    }
    let partitions: Vec<&str> = common.keys().map(String::as_str).collect();
    let lows: Vec<i64> = common.values().map(|r| r.0).collect();
    let highs: Vec<i64> = common.values().map(|r| r.1).collect();
    let query = format!(
        "WITH r(p, lo, hi) AS (SELECT * FROM unnest($1::text[], $2::bigint[], $3::bigint[]))
         SELECT r.p, count(*), sha256(string_agg(sha256(convert_to(t::text, 'UTF8')), ''::bytea
                                                  ORDER BY t.{s}))
           FROM {t} t JOIN r ON t.{p}::text = r.p AND t.{s} BETWEEN r.lo AND r.hi
          GROUP BY r.p",
        p = log.partition,
        s = log.sequence,
        t = log.table
    );
    match client.query(&query, &[&partitions, &lows, &highs]) {
        Ok(rows) => Read::Digests(
            rows.iter()
                .map(|row| (row.get(0), (row.get(1), row.get(2))))
                .collect(),
        ),
        Err(failed) => Read::Unreadable(crate::preflight::with_causes(&failed)),
    }
}

/// A member of the brought database: Patroni's name for it (its pod's) and
/// its address.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub address: std::net::IpAddr,
}

/// Where the Job finds the database's members and Patroni.
pub struct Setup {
    /// The serving URL, whose role reads every store's tables.
    pub url: String,
    /// The members' headless Service, which names every member's address,
    /// ready or not.
    pub service: String,
    /// How many members the chart runs.
    pub expected: usize,
    /// Patroni's REST credential, this Job's alone; none to compare only.
    pub patroni: Option<(String, String)>,
    pub reclone: bool,
}

impl Setup {
    /// From the chart's environment: MERIDIAN_DATABASE_URL, the members'
    /// headless Service as MERIDIAN_REPLICAS_SERVICE and how many it runs as
    /// MERIDIAN_REPLICAS_MEMBERS, Patroni's credential as
    /// MERIDIAN_PATRONI_USERNAME/_PASSWORD, and MERIDIAN_REPLICAS_RECLONE.
    pub fn from_env() -> Result<Self, String> {
        let url = crate::required("MERIDIAN_DATABASE_URL")?;
        let service = crate::required("MERIDIAN_REPLICAS_SERVICE")?;
        let expected: usize = crate::required("MERIDIAN_REPLICAS_MEMBERS")?
            .parse()
            .map_err(|_| "MERIDIAN_REPLICAS_MEMBERS is not a count".to_string())?;
        let patroni = match (
            crate::var("MERIDIAN_PATRONI_USERNAME"),
            crate::var("MERIDIAN_PATRONI_PASSWORD"),
        ) {
            (Some(user), Some(password)) => Some((user, password)),
            _ => None,
        };
        let reclone = crate::var("MERIDIAN_REPLICAS_RECLONE").as_deref() == Some("true");
        Ok(Self {
            url,
            service,
            expected,
            patroni,
            reclone,
        })
    }
}

/// Patroni's REST API as this Job reaches it: by each member's address, or,
/// with internal TLS, over TLS by the member's own Service's name
/// (`<set>-<n>.<namespace>.svc.cluster.local`, which its certificate
/// carries), verified by that name against the internal CA (ruled
/// 2026-11-08).
pub struct Api {
    http: reqwest::Client,
    /// With internal TLS: the database's StatefulSet, and the domain each
    /// member's own Service is named in.
    named: Option<(String, String)>,
}

impl Api {
    pub fn new(setup: &Setup) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
        let named = match meridian_changes::tls::internal_ca() {
            None => None,
            Some(path) => {
                let pem = std::fs::read(&path).map_err(|failed| {
                    format!("the internal CA at {path} is unreadable: {failed}")
                })?;
                for certificate in reqwest::Certificate::from_pem_bundle(&pem)
                    .map_err(|failed| format!("the internal CA at {path} is not one: {failed}"))?
                {
                    builder = builder.add_root_certificate(certificate);
                }
                Some(named_by(&setup.service)?)
            }
        };
        let http = builder
            .build()
            .map_err(|failed| format!("Patroni's client could not be built: {failed}"))?;
        Ok(Self { http, named })
    }

    /// Where a member's `path` is asked.
    pub fn url(&self, name: &str, address: std::net::IpAddr, path: &str) -> String {
        match &self.named {
            Some((_, within)) => format!("https://{name}.{within}:8008/{path}"),
            None => format!("http://{}/{path}", std::net::SocketAddr::new(address, 8008)),
        }
    }
}

/// The database's StatefulSet and the domain its members' own Services are
/// named in, from the members' headless Service
/// (`<set>-members.<namespace>.svc.cluster.local`).
pub fn named_by(service: &str) -> Result<(String, String), String> {
    service
        .split_once('.')
        .and_then(|(members, within)| {
            members
                .strip_suffix("-members")
                .map(|set| (set.to_string(), within.to_string()))
        })
        .ok_or_else(|| format!("the members' Service {service} is not <set>-members.<domain>"))
}

/// Every member, each named by Patroni's own word for it: by the headless
/// Service's addresses, or with internal TLS by each member's own
/// Service. All of them, or none: a member that cannot be found is one
/// whose copy cannot be compared.
async fn discover(api: &Api, setup: &Setup) -> Result<Vec<Member>, String> {
    use std::net::ToSocketAddrs as _;
    if let Some((set, within)) = &api.named {
        let mut members = Vec::new();
        for n in 0..setup.expected {
            let name = format!("{set}-{n}");
            let address = format!("{name}.{within}:5432")
                .to_socket_addrs()
                .map_err(|failed| format!("member {name}'s Service did not resolve: {failed}"))?
                .map(|at| at.ip())
                .next()
                .ok_or_else(|| format!("member {name}'s Service names no address"))?;
            let said = patroni_said(api, &name, address)
                .await
                .and_then(|said| said["patroni"]["name"].as_str().map(str::to_string));
            if said.as_deref() != Some(name.as_str()) {
                return Err(format!(
                    "Patroni at {name} did not say it is {name}, over TLS verified by that name"
                ));
            }
            members.push(Member { name, address });
        }
        return Ok(members);
    }
    let addresses: std::collections::BTreeSet<std::net::IpAddr> = format!("{}:5432", setup.service)
        .to_socket_addrs()
        .map_err(|failed| {
            format!(
                "the members' Service {} did not resolve: {failed}",
                setup.service
            )
        })?
        .map(|at| at.ip())
        .collect();
    let mut members = Vec::new();
    for address in addresses {
        let name = patroni_said(api, "", address)
            .await
            .and_then(|said| said["patroni"]["name"].as_str().map(str::to_string))
            .ok_or_else(|| format!("Patroni at {address} did not say which member it is"))?;
        members.push(Member { name, address });
    }
    members.sort_by(|a, b| a.name.cmp(&b.name));
    if members.len() != setup.expected {
        return Err(format!(
            "{} member(s) found of the {} the chart runs: {}",
            members.len(),
            setup.expected,
            members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(members)
}

/// Whether a member is the primary, and how far it has replayed or written.
fn position(client: &mut postgres::Client) -> Result<(bool, String), String> {
    let row = client
        .query_one(
            "SELECT NOT pg_is_in_recovery(),
                    CASE WHEN pg_is_in_recovery() THEN pg_last_wal_replay_lsn()::text
                         ELSE pg_current_wal_lsn()::text END",
            &[],
        )
        .map_err(|failed| failed.to_string())?;
    Ok((
        row.get(0),
        row.get::<_, Option<String>>(1).unwrap_or_default(),
    ))
}

fn replayed_past(client: &mut postgres::Client, lsn: &str) -> bool {
    client
        .query_one(
            "SELECT pg_last_wal_replay_lsn() >= $1::text::pg_lsn",
            &[&lsn],
        )
        .ok()
        .and_then(|row| row.get::<_, Option<bool>>(0))
        .unwrap_or(false)
}

/// One comparison of every member against the primary: each standby's
/// findings by log, and which member is primary.
fn compare(setup: &Setup, members: &[Member]) -> Result<(String, Found), String> {
    let mut clients = Vec::new();
    let mut primary = None;
    let mut primary_lsn = String::new();
    for (i, member) in members.iter().enumerate() {
        let mut client = connect(&setup.url, member.address)
            .map_err(|failed| format!("{} could not be reached: {failed}", member.name))?;
        let (is_primary, lsn) = position(&mut client)?;
        if is_primary {
            primary = Some(i);
            primary_lsn = lsn;
        }
        clients.push(client);
    }
    let primary = primary.ok_or("no member answers as the primary")?;
    // A standby behind the primary's position at the start is waited for,
    // a little, before it is compared: lag is not rot.
    let until = Instant::now() + Duration::from_secs(30);
    for (i, client) in clients.iter_mut().enumerate() {
        if i == primary {
            continue;
        }
        while !replayed_past(client, &primary_lsn) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let mut found: Found = BTreeMap::new();
    for log in LOGS {
        let mut each = Vec::new();
        let mut unreadable = BTreeMap::new();
        for (i, client) in clients.iter_mut().enumerate() {
            match ranges(client, log) {
                Ok(Some(r)) => each.push((i, r)),
                Ok(None) => {}
                Err(why) => {
                    unreadable.insert(i, why);
                }
            }
        }
        if each.is_empty() && unreadable.is_empty() {
            continue;
        }
        let common = common_ranges(&each.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>());
        let reads: Vec<Read> = clients
            .iter_mut()
            .enumerate()
            .map(|(i, client)| match unreadable.get(&i) {
                Some(why) => Read::Unreadable(why.clone()),
                None => digests(client, log, &common),
            })
            .collect();
        for (i, read) in reads.iter().enumerate() {
            if i == primary {
                continue;
            }
            if let Some(why) = differs(&reads[primary], read) {
                found
                    .entry(log.prefix.to_string())
                    .or_default()
                    .push((i, why));
            }
        }
        if let Read::Unreadable(why) = &reads[primary] {
            found.entry(log.prefix.to_string()).or_default().push((
                primary,
                format!("the primary's copy could not be read: {why}"),
            ));
        }
    }
    Ok((members[primary].name.clone(), found))
}

/// Patroni's own word on a member (GET /patroni).
async fn patroni_said(api: &Api, name: &str, address: std::net::IpAddr) -> Option<Value> {
    let body = api
        .http
        .get(api.url(name, address, "patroni"))
        .send()
        .await
        .ok()?
        .bytes()
        .await
        .ok()?;
    serde_json::from_slice(&body).ok()
}

/// A member's role, state, replication and server start, by Patroni.
async fn patroni_state(api: &Api, member: &Member) -> Option<[String; 4]> {
    let said = patroni_said(api, &member.name, member.address).await?;
    let text = |key: &str| said[key].as_str().unwrap_or_default().to_string();
    Some([
        text("role"),
        text("state"),
        text("replication_state"),
        text("postmaster_start_time"),
    ])
}

/// Re-clone a standby from the primary through Patroni (`POST
/// /reinitialize`), and wait for it to stream again: a server started
/// since the request, a replica, streaming.
async fn reinit(
    api: &Api,
    member: &Member,
    credential: &(String, String),
    within: Duration,
) -> Result<(), String> {
    let before = patroni_state(api, member)
        .await
        .map(|[_, _, _, started]| started)
        .unwrap_or_default();
    let answered = api
        .http
        .post(api.url(&member.name, member.address, "reinitialize"))
        .basic_auth(&credential.0, Some(&credential.1))
        .header("Content-Type", "application/json")
        .body(json!({ "force": true }).to_string())
        .send()
        .await
        .map_err(|failed| format!("Patroni could not be asked: {failed}"))?;
    if !answered.status().is_success() {
        let status = answered.status();
        let body = answered.text().await.unwrap_or_default();
        // Patroni's own words, not a page of HTML.
        let said: String = body
            .lines()
            .find(|line| !line.trim_start().starts_with('<') && !line.trim().is_empty())
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        return Err(format!("Patroni refused the re-clone ({status}) {said}"));
    }
    let until = Instant::now() + within;
    loop {
        if let Some([role, state, replication, started]) = patroni_state(api, member).await {
            if started != before
                && !started.is_empty()
                && (role == "replica" || role == "sync_standby")
                && state == "running"
                && replication == "streaming"
            {
                return Ok(());
            }
        }
        if Instant::now() > until {
            return Err(format!(
                "it did not stream again within {} s",
                within.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Keep a finding in the store whose log differed, on the primary.
fn keep(
    setup: &Setup,
    primary: std::net::IpAddr,
    prefix: &str,
    member: &str,
    what: &str,
) -> Result<(), String> {
    let mut client = connect(&setup.url, primary)?;
    let clock = crate::clock();
    let scrubbed = meridian_changes::integrity::Scrubbed {
        checked: 0,
        filled: 0,
        findings: vec![meridian_changes::integrity::Finding {
            partition: "replicas".into(),
            sequence: 0,
            committed_at_ns: 0,
            what: what.to_string(),
        }],
    };
    let now = clock.now_ns();
    meridian_changes::integrity::record(&mut client, prefix, &scrubbed, member, now, now + 1)
        .map_err(|failed| crate::preflight::with_causes(&failed))
}

/// One run of the Job: compare, re-clone where told, keep what was found,
/// and say it as JSON.
pub fn run(setup: &Setup) -> Result<Value, String> {
    let started = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?;
    let api = Api::new(setup)?;
    // A Job's pod is admitted by the database's NetworkPolicy once the
    // cluster's policy controller has seen it, which can be seconds after it
    // starts (refused until then): asked again for up to a minute.
    let until = Instant::now() + Duration::from_secs(60);
    let members = loop {
        let found = runtime.block_on(discover(&api, setup)).and_then(|members| {
            let primary = members.first().map(|m| m.address);
            match primary {
                Some(address) => connect(&setup.url, address).map(|_| members),
                None => Err("no member found".into()),
            }
        });
        match found {
            Ok(members) => break members,
            Err(failed) if Instant::now() < until => {
                tracing::info!("the database's members not reached yet: {failed}");
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(failed) => return Err(failed),
        }
    };
    let (primary, first) = compare(setup, &members)?;
    if first.is_empty() {
        return Ok(json!({
            "replicas": "matching", "primary": primary,
            "members": members.len(), "ms": started.elapsed().as_millis() as u64,
        }));
    }
    // Once more, before anything is done: a copy that only lagged matches
    // by now.
    let (primary, found) = compare(setup, &members)?;
    let detected_ms = started.elapsed().as_millis() as u64;
    let primary_address = members
        .iter()
        .find(|m| m.name == primary)
        .map(|m| m.address)
        .ok_or("the primary is not among the members")?;
    let mut bad: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();
    for (prefix, members) in &found {
        for (i, why) in members {
            bad.entry(*i)
                .or_default()
                .push((prefix.clone(), why.clone()));
        }
    }
    let mut said = Vec::new();
    for (i, logs) in &bad {
        let member = &members[*i];
        let why = logs
            .iter()
            .map(|(prefix, why)| format!("{prefix}: {why}"))
            .collect::<Vec<_>>()
            .join("; ");
        let (outcome, recloned_ms) = if member.name == primary {
            (
                "the primary's own copy: never re-cloned here; a person switches over and \
                 re-clones it as a standby"
                    .to_string(),
                None,
            )
        } else if !setup.reclone {
            (
                "not re-cloned: re-cloning is off (tolerant)".to_string(),
                None,
            )
        } else if let Some(credential) = &setup.patroni {
            let began = Instant::now();
            let done = runtime.block_on(reinit(&api, member, credential, Duration::from_secs(300)));
            let took = began.elapsed().as_millis() as u64;
            match done {
                Err(why) => (format!("re-clone failed: {why}"), Some(took)),
                Ok(()) => {
                    // Compared again, now it streams.
                    let still = compare(setup, &members)
                        .map(|(_, again)| {
                            again
                                .values()
                                .flatten()
                                .any(|(j, _)| members[*j].name == member.name)
                        })
                        .unwrap_or(true);
                    if still {
                        (
                            format!(
                                "re-cloned through Patroni in {:.1} s, but its copy still differs",
                                took as f64 / 1000.0
                            ),
                            Some(took),
                        )
                    } else {
                        (
                            format!("re-cloned through Patroni in {:.1} s; its copy matches the primary's again", took as f64 / 1000.0),
                            Some(took),
                        )
                    }
                }
            }
        } else {
            (
                "not re-cloned: no Patroni credential is mounted".to_string(),
                None,
            )
        };
        let what = format!(
            "member {}'s copy differs from the primary's ({why}): {outcome}",
            member.name
        );
        tracing::error!(member = member.name, "integrity finding: {what}");
        for (prefix, _) in logs {
            if let Err(failed) = keep(setup, primary_address, prefix, &member.name, &what) {
                tracing::error!(prefix, "the finding could not be kept: {failed}");
            }
        }
        said.push(json!({
            "member": member.name, "why": why, "outcome": outcome,
            "detected_ms": detected_ms, "recloned_ms": recloned_ms,
        }));
    }
    Ok(json!({
        "replicas": "differed", "primary": primary, "members": members.len(),
        "findings": said, "ms": started.elapsed().as_millis() as u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digests(rows: &[(&str, i64, u8)]) -> Read {
        Read::Digests(
            rows.iter()
                .map(|(p, n, d)| (p.to_string(), (*n, vec![*d; 32])))
                .collect(),
        )
    }

    #[test]
    fn the_same_copy_differs_in_nothing_and_a_changed_one_is_named() {
        let primary = digests(&[("a", 3, 1), ("b", 2, 2)]);
        assert_eq!(
            differs(&primary, &digests(&[("a", 3, 1), ("b", 2, 2)])),
            None
        );
        let said = differs(&primary, &digests(&[("a", 3, 9), ("b", 2, 2)])).unwrap();
        assert!(said.contains("partition a"), "{said}");
        assert!(!said.contains("partition b"), "{said}");
        let missing = differs(&primary, &digests(&[("a", 3, 1)])).unwrap();
        assert!(missing.contains("partition b is missing"), "{missing}");
        let unread = differs(
            &primary,
            &Read::Unreadable("page 0 failed its checksum".into()),
        )
        .unwrap();
        assert!(unread.contains("checksum"), "{unread}");
    }

    #[test]
    fn the_range_compared_is_what_every_member_holds() {
        let a: BTreeMap<String, (i64, i64)> =
            [("x".to_string(), (1, 10)), ("y".to_string(), (5, 9))].into();
        let b: BTreeMap<String, (i64, i64)> =
            [("x".to_string(), (3, 8)), ("z".to_string(), (1, 2))].into();
        let common = common_ranges(&[a, b]);
        assert_eq!(common.get("x"), Some(&(3, 8)));
        assert!(
            !common.contains_key("y"),
            "a partition a member lacks is not compared"
        );
        assert!(!common.contains_key("z"));
    }

    #[test]
    fn with_internal_tls_a_member_is_asked_by_its_own_services_name() {
        let (set, within) =
            named_by("om-meridian-runtime-database-members.om.svc.cluster.local").unwrap();
        assert_eq!(set, "om-meridian-runtime-database");
        assert_eq!(within, "om.svc.cluster.local");
        assert!(named_by("om-database.om.svc.cluster.local").is_err());
        let address: std::net::IpAddr = "10.42.0.7".parse().unwrap();
        let named = Api {
            http: reqwest::Client::new(),
            named: Some((set, within)),
        };
        assert_eq!(
            named.url("om-meridian-runtime-database-1", address, "patroni"),
            "https://om-meridian-runtime-database-1.om.svc.cluster.local:8008/patroni"
        );
        let plain = Api {
            http: reqwest::Client::new(),
            named: None,
        };
        assert_eq!(
            plain.url("om-meridian-runtime-database-1", address, "patroni"),
            "http://10.42.0.7:8008/patroni"
        );
    }
}
