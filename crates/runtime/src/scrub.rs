//! The scrub, run by a store's writer (step 3b; spec/a-deployment-scales-
//! out-and-fails-over-by-configuration section 9; plan 5j): its record
//! re-read every `<store>.scrub.intervalDays`, at low priority and under its
//! lease, each finding an `IntegrityFinding` kept beside its control
//! partition and said loudly in its log, each run kept for `status`.
//! `meridian-<store> scrub` runs one at once, from inside its pod, and
//! prints what it found as one JSON object.

use std::sync::Arc;
use std::time::Duration;

use meridian_changes::integrity::Scrubbed;
use serde_json::{json, Value};

/// A scrub's run as `status` and the CLI read it.
pub fn report(component: &str, scrubbed: &Scrubbed, finished_at_ns: i64) -> Value {
    json!({
        "component": component,
        "finished_at_ns": finished_at_ns,
        "checked": scrubbed.checked,
        "filled": scrubbed.filled,
        "findings": scrubbed.findings.iter().map(|finding| json!({
            "partition": finding.partition,
            "sequence": finding.sequence,
            "committed_at_ns": finding.committed_at_ns,
            "what": finding.what,
        })).collect::<Vec<_>>(),
    })
}

/// One scrub: the record re-read, its findings and its run kept.
pub fn run_once<S, R>(component: &str, scrub: S, record: R) -> Result<Value, String>
where
    S: FnOnce() -> Result<Scrubbed, String>,
    R: FnOnce(&Scrubbed, &str, i64, i64) -> Result<(), String>,
{
    let clock = crate::clock();
    let started_at_ns = clock.now_ns();
    let scrubbed = scrub()?;
    let finished_at_ns = clock.now_ns().max(started_at_ns + 1);
    record(
        &scrubbed,
        &crate::member::member_name(),
        started_at_ns,
        finished_at_ns,
    )?;
    for finding in &scrubbed.findings {
        tracing::error!(
            component,
            partition = finding.partition,
            sequence = finding.sequence,
            committed_at_ns = finding.committed_at_ns,
            "integrity finding: {}",
            finding.what
        );
    }
    Ok(report(component, &scrubbed, finished_at_ns))
}

/// Scrub every interval while this process writes: the first within ten
/// minutes of taking the lease, then every `scrub_every()`; none where it
/// is 0. Off the runtime's workers: it reads the whole record.
pub fn forever<F>(component: &'static str, once: F)
where
    F: Fn() -> Result<Value, String> + Send + Sync + 'static,
{
    let Some(every) = meridian_changes::integrity::scrub_every() else {
        tracing::info!(component, "the scrub is off (scrub.intervalDays 0)");
        return;
    };
    let once = Arc::new(once);
    tokio::spawn(async move {
        let mut wait = every.min(Duration::from_secs(600));
        loop {
            tokio::time::sleep(wait).await;
            wait = every;
            let run = Arc::clone(&once);
            match tokio::task::spawn_blocking(move || run()).await {
                Ok(Ok(said)) => tracing::info!(component, %said, "scrubbed the record"),
                Ok(Err(failed)) => tracing::warn!(component, %failed, "the scrub did not finish"),
                Err(failed) => tracing::warn!(component, %failed, "the scrub stopped"),
            }
        }
    });
}
