//! What an upgrade is checked against before it changes anything (H1,
//! ruling 586; kernel/an-upgrade-is-checked-before-it-is-applied):
//! `meridian-conductor preflight`, run from the release being installed --
//! as the chart's pre-upgrade hook, so a blocked check stops `helm upgrade`
//! (and `meridian upgrade-deployment`) before any resource changes, or by
//! hand with the same image.
//!
//! **It changes nothing.** Every database session it opens is read-only
//! (`default_transaction_read_only`), it holds no Kubernetes right, it calls
//! nothing on the bus, and what it reads of backups is a directory listing.
//! Its report holds no deployment data: counts, sizes, versions and the
//! names of stores and templates, never an account or a person.
//!
//! Each check answers **ready**, **warning** or **blocked**, with its reason,
//! or **not checked** with why; the whole answers the worst of them.
//!
//! - **Headroom:** the database's size against its volume, its memory as
//!   configured against the container's limit, its connections in use
//!   against `max_connections`; the largest bus answer core builds from
//!   stored records -- the dashboard's AccessRecords -- against the broker's
//!   `max_payload`.
//! - **Mixed versions:** each store's schema against this release's (a
//!   database newer than the release is blocked: that is a downgrade); and
//!   the stored records this release would read differently -- a template
//!   carrying fields this release does not know, or naming a model its
//!   engine does not have.
//! - **Migrations:** each pending migration additive (`CREATE`, `ADD`), or
//!   named as not: a rollback to the release before cannot read what it
//!   leaves.
//! - **Plugins:** the launched instances, each keeping the sidecar it runs
//!   until relaunched.
//! - **Backups:** the newest backup's age.

use std::path::Path;

use prost::Message;
use serde_json::{json, Value};

/// A check's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Ready,
    NotChecked,
    Warning,
    Blocked,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Ready => "ready",
            Verdict::NotChecked => "not_checked",
            Verdict::Warning => "warning",
            Verdict::Blocked => "blocked",
        }
    }
}

/// One check, its answer and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub check: &'static str,
    pub verdict: Verdict,
    pub why: String,
}

fn finding(check: &'static str, verdict: Verdict, why: impl Into<String>) -> Finding {
    Finding {
        check,
        verdict,
        why: why.into(),
    }
}

/// The whole: the worst answer, a check not made never counting as passing
/// nor as failing.
pub fn overall(findings: &[Finding]) -> Verdict {
    let worst = findings
        .iter()
        .map(|f| f.verdict)
        .filter(|v| *v != Verdict::NotChecked)
        .max()
        .unwrap_or(Verdict::Ready);
    worst.max(Verdict::Ready)
}

/// The report as it is printed, one JSON object on stdout.
pub fn report_json(release: &str, findings: &[Finding]) -> Value {
    json!({
        "preflight": overall(findings).word(),
        "release": release,
        "checks": findings.iter().map(|f| json!({
            "check": f.check,
            "verdict": f.verdict.word(),
            "why": f.why,
        })).collect::<Vec<_>>(),
    })
}

// ── Judging, apart from reading ────────────────────────────────────────────

/// A share past which a limit warns, and past which it blocks.
pub const WARN_AT: f64 = 0.75;
pub const BLOCK_AT: f64 = 0.90;

fn share(used: u64, limit: u64) -> f64 {
    if limit == 0 {
        0.0
    } else {
        used as f64 / limit as f64
    }
}

fn percent(used: u64, limit: u64) -> String {
    format!("{:.0}%", share(used, limit) * 100.0)
}

/// A size in words: `812 MiB`.
pub fn bytes_said(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A Kubernetes quantity in bytes: `8Gi`, `512Mi`, `1G`, `1048576`; `1MB` as
/// NATS writes it, 1,048,576.
pub fn quantity(said: &str) -> Option<u64> {
    let said = said.trim();
    if said.is_empty() {
        return None;
    }
    let split = said
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(said.len());
    let (number, unit) = said.split_at(split);
    let number: f64 = number.parse().ok()?;
    let factor: f64 = match unit {
        "" => 1.0,
        "Ki" | "KB" | "kb" => 1024.0,
        "Mi" | "MB" | "mb" => 1024.0 * 1024.0,
        "Gi" | "GB" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "Ti" => 1024.0_f64.powi(4),
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        _ => return None,
    };
    Some((number * factor) as u64)
}

/// The database's size against its volume.
pub fn judge_disk(used: u64, volume: Option<u64>) -> Finding {
    let Some(volume) = volume else {
        return finding(
            "database_disk",
            Verdict::NotChecked,
            format!(
                "the database holds {}; its volume is not the chart's, so its operator watches its disk",
                bytes_said(used)
            ),
        );
    };
    let s = share(used, volume);
    let why = format!(
        "the database holds {} of its {} volume ({})",
        bytes_said(used),
        bytes_said(volume),
        percent(used, volume)
    );
    let verdict = if s >= BLOCK_AT {
        Verdict::Blocked
    } else if s >= WARN_AT {
        Verdict::Warning
    } else {
        Verdict::Ready
    };
    let why = match verdict {
        Verdict::Blocked => format!("{why}: a migration may not fit; grow the volume first"),
        Verdict::Warning => format!("{why}: grow the volume soon"),
        _ => why,
    };
    finding("database_disk", verdict, why)
}

/// The database's memory as configured -- shared buffers, and work memory
/// for every connection it allows -- against the container's limit.
pub fn judge_memory(
    shared_buffers: u64,
    work_mem: u64,
    max_connections: u64,
    limit: Option<u64>,
) -> Finding {
    let configured = shared_buffers + work_mem * max_connections;
    let Some(limit) = limit else {
        return finding(
            "database_memory",
            Verdict::NotChecked,
            format!(
                "the database may use {} as configured; its memory limit is not the chart's",
                bytes_said(configured)
            ),
        );
    };
    let why = format!(
        "shared buffers {} and {} work memory for each of {} connections: {} of its {} limit ({})",
        bytes_said(shared_buffers),
        bytes_said(work_mem),
        max_connections,
        bytes_said(configured),
        bytes_said(limit),
        percent(configured, limit)
    );
    // Work memory is a ceiling every connection rarely reaches at once: past
    // the limit warns, never blocks.
    if configured > limit {
        finding(
            "database_memory",
            Verdict::Warning,
            format!("{why}: under load it can exceed its limit"),
        )
    } else {
        finding("database_memory", Verdict::Ready, why)
    }
}

/// The connections in use against those allowed.
pub fn judge_connections(in_use: u64, max: u64) -> Finding {
    let why = format!(
        "{in_use} of {max} connections in use ({})",
        percent(in_use, max)
    );
    if share(in_use, max) >= BLOCK_AT {
        finding(
            "database_connections",
            Verdict::Warning,
            format!(
                "{why}: the new components' connections may be refused while the old ones close"
            ),
        )
    } else {
        finding("database_connections", Verdict::Ready, why)
    }
}

/// A bus answer built from stored records against the broker's payload
/// limit.
pub fn judge_answer(answer: &str, bytes: u64, max_payload: u64) -> Finding {
    let why = format!(
        "{answer} is {} of the broker's {} payload limit ({})",
        bytes_said(bytes),
        bytes_said(max_payload),
        percent(bytes, max_payload)
    );
    let s = share(bytes, max_payload);
    if s >= BLOCK_AT {
        finding(
            "bus_answers",
            Verdict::Blocked,
            format!("{why}: it may stop being answered; raise the limit or page it first"),
        )
    } else if s >= WARN_AT {
        finding(
            "bus_answers",
            Verdict::Warning,
            format!("{why}: near the limit"),
        )
    } else {
        finding("bus_answers", Verdict::Ready, why)
    }
}

/// One store's schema against this release's.
pub fn judge_schema(
    store: &str,
    applied: Option<i64>,
    latest: i64,
    pending: &[(i64, &str)],
) -> Finding {
    match applied {
        None => finding(
            "schema",
            Verdict::Ready,
            format!("{store}: no schema yet; this release's migration makes it"),
        ),
        Some(at) if at > latest => finding(
            "schema",
            Verdict::Blocked,
            format!(
                "{store}: the database is at schema version {at}, newer than this release's {latest}: \
                 this would be a downgrade, which reads it wrongly; install the release that wrote it"
            ),
        ),
        Some(at) if at < latest => finding(
            "schema",
            Verdict::Warning,
            format!(
                "{store}: {} migration(s) pending ({}): while the release rolls out, components still on \
                 the old version wait, not ready, until they are replaced",
                pending.len(),
                pending
                    .iter()
                    .map(|(v, name)| format!("{v} {name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Some(at) => finding(
            "schema",
            Verdict::Ready,
            format!("{store}: at schema version {at}, this release's"),
        ),
    }
}

/// Why a migration's SQL is not additive, or none: what drops, renames,
/// retypes, deletes or empties something an older release reads.
pub fn not_additive(sql: &str) -> Option<&'static str> {
    // Comments are not statements.
    let code: String = sql
        .lines()
        .map(|line| line.split("--").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    let words: Vec<&str> = code.split_whitespace().collect();
    let has = |pattern: &[&str]| words.windows(pattern.len()).any(|w| w == pattern);
    if has(&["DROP", "TABLE"]) {
        Some("drops a table")
    } else if has(&["DROP", "COLUMN"]) {
        Some("drops a column")
    } else if has(&["RENAME"]) || has(&["RENAME", "TO"]) {
        Some("renames something")
    } else if has(&["ALTER", "COLUMN"]) && (has(&["TYPE"]) || has(&["SET", "NOT", "NULL"])) {
        Some("changes a column's type or makes it required")
    } else if has(&["DELETE", "FROM"]) {
        Some("deletes rows")
    } else if has(&["TRUNCATE"]) {
        Some("empties a table")
    } else {
        None
    }
}

/// The pending migrations of every store, each additive or named.
pub fn judge_additive(pending: &[(String, i64, &str, &str)]) -> Finding {
    let named: Vec<String> = pending
        .iter()
        .filter_map(|(store, version, name, sql)| {
            not_additive(sql).map(|why| format!("{store} {version} {name} {why}"))
        })
        .collect();
    if pending.is_empty() {
        finding(
            "migrations_additive",
            Verdict::Ready,
            "no migration pending",
        )
    } else if named.is_empty() {
        finding(
            "migrations_additive",
            Verdict::Ready,
            format!("{} pending migration(s), each additive", pending.len()),
        )
    } else {
        finding(
            "migrations_additive",
            Verdict::Warning,
            format!(
                "not additive: {}; a rollback to the release before cannot read what they leave, so take a backup first",
                named.join("; ")
            ),
        )
    }
}

/// The stored templates this release would read differently: one carrying
/// fields it does not know (decoded and written again, it is not the same
/// bytes), or naming a model its engine does not have.
pub fn judge_templates(stored: &[(String, String, Vec<u8>)]) -> Finding {
    let mut differently = Vec::new();
    for (id, version, bytes) in stored {
        match meridian_domain::v1::SecurityTemplate::decode(bytes.as_slice()) {
            Err(failed) => differently.push(format!("{id} {version} does not decode ({failed})")),
            Ok(template) => {
                // Written again shorter: the fields it does not know were
                // dropped (a length, not the bytes, so a map's order never
                // reads as a difference).
                if template.encoded_len() < bytes.len() {
                    differently.push(format!(
                        "{id} {version} carries fields this release does not read"
                    ));
                }
                if let Some(package) = &template.calculation_package {
                    let models = std::iter::once(package.model_id.as_str())
                        .chain(package.model_bindings.iter().map(|b| b.model_id.as_str()))
                        .filter(|m| !m.is_empty());
                    for model in models {
                        if !meridian_analytics_engine::definitions::MODELS.contains(&model) {
                            differently.push(format!(
                                "{id} {version} names the model {model}, which this release's engine does not have"
                            ));
                        }
                    }
                }
            }
        }
    }
    if differently.is_empty() {
        finding(
            "stored_records",
            Verdict::Ready,
            format!(
                "{} stored template version(s), each read as written",
                stored.len()
            ),
        )
    } else {
        finding(
            "stored_records",
            Verdict::Warning,
            format!(
                "read differently by this release: {}",
                differently.join("; ")
            ),
        )
    }
}

/// The newest backup's age.
pub fn judge_backups(
    newest_ns: Option<i64>,
    configured: bool,
    readable: bool,
    now_ns: i64,
) -> Finding {
    const DAY_NS: i64 = 86_400 * 1_000_000_000;
    if !configured {
        return finding(
            "backup",
            Verdict::Warning,
            "no backup is configured: take one with `meridian backup` before upgrading",
        );
    }
    if !readable {
        return finding(
            "backup",
            Verdict::NotChecked,
            "backups are kept on a claim of the administrator's own, which this check does not mount: \
             see that its newest is recent before upgrading",
        );
    }
    match newest_ns {
        None => finding(
            "backup",
            Verdict::Warning,
            "no backup has been written yet: take one with `meridian backup` before upgrading",
        ),
        Some(at) => {
            let hours = (now_ns - at).max(0) / (3_600 * 1_000_000_000);
            if now_ns - at > DAY_NS {
                finding(
                    "backup",
                    Verdict::Warning,
                    format!("the newest backup is {hours} hours old: take one with `meridian backup` before upgrading"),
                )
            } else {
                finding(
                    "backup",
                    Verdict::Ready,
                    format!("the newest backup is {hours} hours old"),
                )
            }
        }
    }
}

/// The launched plugins: each keeps the sidecar it runs until relaunched.
pub fn judge_plugins(launched: &[String]) -> Finding {
    if launched.is_empty() {
        finding("plugins_relaunch", Verdict::Ready, "no plugin is launched")
    } else {
        finding(
            "plugins_relaunch",
            Verdict::Ready,
            format!(
                "{} launched plugin(s) keep the sidecar they run until each is relaunched; \
                 `meridian upgrade-deployment` lists them afterwards",
                launched.len()
            ),
        )
    }
}

// ── Reading ────────────────────────────────────────────────────────────────

/// What the chart tells the check, beside the databases.
#[derive(Debug, Default, Clone)]
pub struct Inputs {
    /// Each store and its database URL; a store with none is skipped.
    pub stores: Vec<(&'static str, String)>,
    pub volume: Option<u64>,
    pub memory_limit: Option<u64>,
    pub max_payload: u64,
    /// The backups' directory, mounted read-only, where backups are kept.
    pub backups: Option<String>,
    pub backup_configured: bool,
    pub now_ns: i64,
}

impl Inputs {
    /// From the environment the chart's hook sets.
    pub fn from_env(now_ns: i64) -> Inputs {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let stores = [
            ("configuration store", "MERIDIAN_CONFIG_DATABASE_URL"),
            ("street store", "MERIDIAN_STREET_DATABASE_URL"),
            ("book of record", "MERIDIAN_BOR_DATABASE_URL"),
            ("lake", "MERIDIAN_LAKE_DATABASE_URL"),
            ("instrument store", "MERIDIAN_DATABASE_URL"),
            ("dashboard", "MERIDIAN_DASHBOARD_DATABASE_URL"),
        ]
        .into_iter()
        .filter_map(|(store, name)| var(name).map(|url| (store, url)))
        .collect();
        Inputs {
            stores,
            volume: var("MERIDIAN_PREFLIGHT_DATABASE_VOLUME").and_then(|q| quantity(&q)),
            memory_limit: var("MERIDIAN_PREFLIGHT_DATABASE_MEMORY").and_then(|q| quantity(&q)),
            max_payload: var("MERIDIAN_PREFLIGHT_MAX_PAYLOAD")
                .and_then(|q| quantity(&q))
                .unwrap_or(1024 * 1024),
            backups: var("MERIDIAN_PREFLIGHT_BACKUPS"),
            backup_configured: var("MERIDIAN_PREFLIGHT_BACKUP_CONFIGURED").is_some(),
            now_ns,
        }
    }
}

fn read_only(url: &str) -> Result<postgres::Client, String> {
    let mut client =
        postgres::Client::connect(url, postgres::NoTls).map_err(|failed| failed.to_string())?;
    client
        .batch_execute("SET default_transaction_read_only = on")
        .map_err(|failed| failed.to_string())?;
    Ok(client)
}

fn table_exists(client: &mut postgres::Client, table: &str) -> bool {
    client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
        .map(|row| row.get::<_, bool>(0))
        .unwrap_or(false)
}

fn applied(client: &mut postgres::Client, table: &str) -> Result<Option<i64>, String> {
    if !table_exists(client, table) {
        return Ok(None);
    }
    client
        .query_one(&format!("SELECT max(version) FROM {table}"), &[])
        .map(|row| row.get::<_, Option<i64>>(0))
        .map_err(|failed| failed.to_string())
}

/// One migration: its version, its name and its SQL.
type Migration = (i64, &'static str, &'static str);

/// A store's history table and this release's migrations, where it keeps
/// one: the instrument store's are additions under `IF NOT EXISTS`, applied
/// whole every release, so it has none.
fn migrations_of(store: &str) -> Option<(&'static str, Vec<Migration>)> {
    match store {
        "configuration store" => Some((
            "config_schema_migration",
            meridian_config::migrations::MIGRATIONS
                .iter()
                .map(|m| (m.version, m.name, m.sql))
                .collect(),
        )),
        "street store" => Some((
            "schema_migration",
            meridian_street::migrations::MIGRATIONS
                .iter()
                .map(|m| (m.version, m.name, m.sql))
                .collect(),
        )),
        "book of record" => Some((
            "book_schema_migration",
            meridian_bor::migrations::MIGRATIONS
                .iter()
                .map(|m| (m.version, m.name, m.sql))
                .collect(),
        )),
        "lake" => Some((
            "lake_schema_migration",
            meridian_lake::migrations::MIGRATIONS
                .iter()
                .map(|m| (m.version, m.name, m.sql))
                .collect(),
        )),
        "dashboard" => Some((
            "dashboard_schema_migration",
            meridian_dashboard::database::migrations(),
        )),
        _ => None,
    }
}

fn setting_bytes(client: &mut postgres::Client, name: &str) -> Option<u64> {
    let row = client
        .query_opt(
            "SELECT setting::bigint, COALESCE(unit, '') FROM pg_settings WHERE name = $1",
            &[&name],
        )
        .ok()??;
    let value: i64 = row.get(0);
    let unit: String = row.get(1);
    let factor: u64 = match unit.as_str() {
        "8kB" => 8 * 1024,
        "kB" => 1024,
        "MB" => 1024 * 1024,
        "" => 1,
        _ => return None,
    };
    Some(value.max(0) as u64 * factor)
}

/// Every check, against what the chart gives. A database that does not
/// answer is a check not made, said so.
pub fn run(inputs: &Inputs) -> Vec<Finding> {
    let mut findings = Vec::new();
    if inputs.stores.is_empty() {
        findings.push(finding(
            "database",
            Verdict::NotChecked,
            "no database is configured yet: first run has not finished, so there is nothing to upgrade from",
        ));
    }
    // The database's own headroom, once per database however many stores
    // share it.
    let mut seen_urls: Vec<&str> = Vec::new();
    let mut pending_all: Vec<(String, i64, &'static str, &'static str)> = Vec::new();
    for (store, url) in &inputs.stores {
        let mut client = match read_only(url) {
            Ok(client) => client,
            Err(failed) => {
                findings.push(finding(
                    "database",
                    Verdict::Blocked,
                    format!("the {store}'s database does not answer: {failed}"),
                ));
                continue;
            }
        };
        if !seen_urls.contains(&url.as_str()) {
            seen_urls.push(url);
            match client.query_one("SELECT pg_database_size(current_database())", &[]) {
                Ok(row) => findings.push(judge_disk(
                    row.get::<_, i64>(0).max(0) as u64,
                    inputs.volume,
                )),
                Err(failed) => findings.push(finding(
                    "database_disk",
                    Verdict::NotChecked,
                    failed.to_string(),
                )),
            }
            match (
                setting_bytes(&mut client, "shared_buffers"),
                setting_bytes(&mut client, "work_mem"),
                setting_bytes(&mut client, "max_connections"),
            ) {
                (Some(shared), Some(work), Some(max)) => {
                    findings.push(judge_memory(shared, work, max, inputs.memory_limit));
                    if let Ok(row) = client.query_one("SELECT count(*) FROM pg_stat_activity", &[])
                    {
                        findings.push(judge_connections(row.get::<_, i64>(0).max(0) as u64, max));
                    }
                }
                _ => findings.push(finding(
                    "database_memory",
                    Verdict::NotChecked,
                    "the database's memory settings could not be read",
                )),
            }
        }
        if let Some((table, migrations)) = migrations_of(store) {
            match applied(&mut client, table) {
                Ok(at) => {
                    let latest = migrations.last().map(|m| m.0).unwrap_or(0);
                    let pending: Vec<(i64, &str, &str)> = migrations
                        .iter()
                        .filter(|(v, ..)| at.is_none_or(|at| *v > at))
                        .copied()
                        .collect();
                    findings.push(judge_schema(
                        store,
                        at,
                        latest,
                        &pending.iter().map(|(v, n, _)| (*v, *n)).collect::<Vec<_>>(),
                    ));
                    // A store with no schema yet migrates from nothing: no
                    // rollback reads it.
                    if at.is_some() {
                        pending_all.extend(
                            pending
                                .into_iter()
                                .map(|(v, n, sql)| (store.to_string(), v, n, sql)),
                        );
                    }
                }
                Err(failed) => findings.push(finding(
                    "schema",
                    Verdict::NotChecked,
                    format!("{store}: {failed}"),
                )),
            }
        }
        if *store == "instrument store" && table_exists(&mut client, "instrument_template") {
            match client.query(
                "SELECT template_id, template_version, record FROM instrument_template",
                &[],
            ) {
                Ok(rows) => {
                    let stored: Vec<(String, String, Vec<u8>)> = rows
                        .iter()
                        .map(|r| (r.get(0), r.get(1), r.get(2)))
                        .collect();
                    findings.push(judge_templates(&stored));
                }
                Err(failed) => findings.push(finding(
                    "stored_records",
                    Verdict::NotChecked,
                    failed.to_string(),
                )),
            }
        }
        if *store == "configuration store" {
            // Read as this release reads it: a store whose schema this
            // release migrates first may not be readable yet.
            match meridian_config::PostgresStore::connect(url, 1)
                .and_then(|store| meridian_config::store::Store::snapshot(&store))
            {
                Ok(snapshot) => {
                    let launched: Vec<String> = snapshot
                        .catalogue
                        .launches
                        .iter()
                        .filter(|l| {
                            l.state == meridian_domain::v1::PluginLaunchState::Launched as i32
                        })
                        .map(|l| l.instance_id.clone())
                        .collect();
                    findings.push(judge_plugins(&launched));
                    let records = meridian_config::access_records_of(snapshot, inputs.now_ns);
                    findings.push(judge_answer(
                        "the dashboard's access records",
                        records.encoded_len() as u64,
                        inputs.max_payload,
                    ));
                }
                Err(failed) => {
                    findings.push(finding(
                        "bus_answers",
                        Verdict::NotChecked,
                        format!("the configuration store is read after this release's migration: {failed}"),
                    ));
                }
            }
        }
    }
    findings.push(judge_additive(&pending_all));
    let newest = inputs.backups.as_deref().and_then(newest_backup);
    findings.push(judge_backups(
        newest,
        inputs.backup_configured,
        inputs.backups.is_some(),
        inputs.now_ns,
    ));
    findings
}

/// The newest `.meridian-backup` file's time, in a directory listing.
fn newest_backup(dir: &str) -> Option<i64> {
    std::fs::read_dir(Path::new(dir))
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".meridian-backup")
        })
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .filter_map(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_nanos() as i64)
        .max()
}

#[cfg(test)]
mod tests;
