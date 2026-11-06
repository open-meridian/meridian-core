//! Integrity at rest (step 3b; spec/a-deployment-scales-out-and-fails-over-
//! by-configuration section 9; plan 5j): a replica copies what it is given,
//! corruption included, so integrity is checked where the data rests.
//!
//! - **Digests.** Each change a store keeps carries its digest beside it:
//!   SHA-256 of its body for a lake row and a captured row, and for a book
//!   entry SHA-256 of the previous entry's digest in its partition and then
//!   its body, so the journal is a chain and a changed or missing entry is
//!   named by sequence. A follower verifies every change it is given
//!   ([`verify`], [`Chain`]) and copies nothing that does not.
//! - **The scrub.** Each store's writer re-reads its record every
//!   `<store>.scrub.intervalDays` ([`scrub_every`]), at low priority, under
//!   its lease: every row read (so Postgres verifies each page's checksum as
//!   it reads it from disk), every digest checked and the chain followed. A
//!   row kept before digests is given its digest by the scrub; a row whose
//!   digest no longer matches is an `IntegrityFinding`, kept for good in the
//!   store's `<prefix>_integrity_finding` table beside its control partition
//!   ([`Finding`]), and every scrub's run in `<prefix>_scrub`, which `status`
//!   reads.

use std::time::Duration;

use postgres::GenericClient;
use sha2::{Digest as _, Sha256};

use crate::Change;

/// A body's digest: SHA-256 of it.
pub fn digest(body: &[u8]) -> Vec<u8> {
    Sha256::digest(body).to_vec()
}

/// A chained digest: SHA-256 of the previous entry's digest, then the body.
pub fn chained(previous: &[u8], body: &[u8]) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(previous);
    hash.update(body);
    hash.finalize().to_vec()
}

/// A read the database refused because a page failed its checksum
/// (SQLSTATE XX001, `data_corrupted`): a finding of the scrub's, not a
/// failure of it. Anything else is the scrub's own failure.
pub fn page_finding(failed: &postgres::Error, what: &str) -> Option<Finding> {
    let db = failed.as_db_error()?;
    (db.code() == &postgres::error::SqlState::DATA_CORRUPTED).then(|| Finding {
        partition: String::new(),
        sequence: 0,
        committed_at_ns: 0,
        what: format!("{what}: {}", db.message()),
    })
}

/// A digest as people read it, short: its first eight bytes in hex.
pub fn short(digest: &[u8]) -> String {
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Whether a change's body is the one its digest was taken of. A change
/// carrying no digest -- a record kept before digests -- is taken as it is.
pub fn verify(change: &Change) -> Result<(), String> {
    if change.digest.is_empty() || digest(&change.body) == change.digest {
        return Ok(());
    }
    Err(format!(
        "change {} of partition {} does not match its digest {}",
        change.sequence,
        change.partition,
        short(&change.digest)
    ))
}

/// A journal's chain, followed: each change's digest must be the previous
/// one's chained with its body. The first change followed is taken as it is
/// (its previous is not here), and one carrying no digest restarts it.
#[derive(Debug, Default, Clone)]
pub struct Chain {
    previous: Option<Vec<u8>>,
}

impl Chain {
    /// Follow from a digest already verified, or from nothing.
    pub fn after(previous: Option<Vec<u8>>) -> Self {
        Chain { previous }
    }

    pub fn follow(&mut self, change: &Change) -> Result<(), String> {
        if change.digest.is_empty() {
            self.previous = None;
            return Ok(());
        }
        if let Some(previous) = &self.previous {
            if chained(previous, &change.body) != change.digest {
                return Err(format!(
                    "entry {} of partition {} does not follow the entry before it: \
                     the journal's chain is broken there",
                    change.sequence, change.partition
                ));
            }
        }
        self.previous = Some(change.digest.clone());
        Ok(())
    }
}

/// How often a store's record is re-read: `MERIDIAN_SCRUB_INTERVAL_DAYS`
/// (the chart's `<store>.scrub.intervalDays`, 7 by default: Ceph's deep
/// scrub), 0 for never.
pub fn scrub_every() -> Option<Duration> {
    let days: f64 = std::env::var("MERIDIAN_SCRUB_INTERVAL_DAYS")
        .ok()
        .and_then(|days| days.trim().parse().ok())
        .unwrap_or(7.0);
    (days > 0.0).then(|| Duration::from_secs_f64(days * 86_400.0))
}

/// What a scrub found wrong with one row: an `IntegrityFinding`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The partition (a dataset, an account partition, the store's own).
    pub partition: String,
    /// The first bad sequence found there.
    pub sequence: u64,
    /// When it was committed, in the deployment's time, so the newest
    /// restore point before it can be named.
    pub committed_at_ns: i64,
    /// What was wrong, in plain words.
    pub what: String,
}

/// What one scrub of a store did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scrubbed {
    /// Rows read and checked.
    pub checked: u64,
    /// Rows kept before digests, given theirs.
    pub filled: u64,
    pub findings: Vec<Finding>,
}

/// The tables a store keeps its findings and its scrubs in, beside its
/// control partition. Run by the store's `migrate`; each statement may run
/// again.
pub fn tables_sql(prefix: &str) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {prefix}_integrity_finding (
             id              bigserial PRIMARY KEY,
             partition       text   NOT NULL,
             sequence        bigint NOT NULL,
             committed_at_ns bigint NOT NULL,
             what            text   NOT NULL,
             member          text   NOT NULL,
             origin          text   NOT NULL,
             found_at_ns     bigint NOT NULL,
             UNIQUE (partition, sequence, what)
         );
         CREATE TABLE IF NOT EXISTS {prefix}_scrub (
             started_at_ns  bigint PRIMARY KEY,
             finished_at_ns bigint NOT NULL,
             checked        bigint NOT NULL,
             filled         bigint NOT NULL,
             findings       bigint NOT NULL,
             member         text   NOT NULL
         );"
    )
}

/// Keep a scrub's findings, each once, and its run: in one transaction.
pub fn record(
    client: &mut impl GenericClient,
    prefix: &str,
    scrubbed: &Scrubbed,
    member: &str,
    started_at_ns: i64,
    finished_at_ns: i64,
) -> Result<(), postgres::Error> {
    let mut tx = client.transaction()?;
    for finding in &scrubbed.findings {
        tx.execute(
            &format!(
                "INSERT INTO {prefix}_integrity_finding
                     (partition, sequence, committed_at_ns, what, member, origin, found_at_ns)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (partition, sequence, what) DO NOTHING"
            ),
            &[
                &finding.partition,
                &(finding.sequence as i64),
                &finding.committed_at_ns,
                &finding.what,
                &member,
                &crate::origin(),
                &finished_at_ns,
            ],
        )?;
    }
    tx.execute(
        &format!(
            "INSERT INTO {prefix}_scrub
                 (started_at_ns, finished_at_ns, checked, filled, findings, member)
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (started_at_ns) DO NOTHING"
        ),
        &[
            &started_at_ns,
            &finished_at_ns,
            &(scrubbed.checked as i64),
            &(scrubbed.filled as i64),
            &(scrubbed.findings.len() as i64),
            &member,
        ],
    )?;
    tx.commit()
}

/// The last scrub, as `status` and the probe read it: when it finished,
/// what it checked, and every finding not yet answered, newest first (at
/// most ten).
pub fn last(
    client: &mut impl GenericClient,
    prefix: &str,
) -> Result<serde_json::Value, postgres::Error> {
    let scrub = client.query_opt(
        &format!(
            "SELECT finished_at_ns, checked, filled, findings FROM {prefix}_scrub
              ORDER BY started_at_ns DESC LIMIT 1"
        ),
        &[],
    )?;
    let findings = client.query(
        &format!(
            "SELECT partition, sequence, committed_at_ns, what FROM {prefix}_integrity_finding
              ORDER BY found_at_ns DESC, id DESC LIMIT 10"
        ),
        &[],
    )?;
    Ok(serde_json::json!({
        "last_scrub_at_ns": scrub.as_ref().map(|row| row.get::<_, i64>(0)),
        "checked": scrub.as_ref().map(|row| row.get::<_, i64>(1)),
        "filled": scrub.as_ref().map(|row| row.get::<_, i64>(2)),
        "findings": findings.iter().map(|row| serde_json::json!({
            "partition": row.get::<_, String>(0),
            "sequence": row.get::<_, i64>(1),
            "committed_at_ns": row.get::<_, i64>(2),
            "what": row.get::<_, String>(3),
        })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(sequence: u64, body: &[u8], digest: Vec<u8>) -> Change {
        Change {
            partition: "P0".into(),
            sequence,
            body: body.to_vec(),
            digest,
            ..Change::default()
        }
    }

    #[test]
    fn a_body_changed_beneath_its_digest_does_not_verify() {
        let kept = change(1, b"row", digest(b"row"));
        assert!(verify(&kept).is_ok());
        let rotted = change(1, b"rox", digest(b"row"));
        assert!(verify(&rotted)
            .unwrap_err()
            .contains("change 1 of partition P0"));
        // Kept before digests: taken as it is.
        assert!(verify(&change(1, b"row", Vec::new())).is_ok());
    }

    #[test]
    fn a_chain_names_the_entry_that_does_not_follow() {
        let first = chained(b"", b"one");
        let second = chained(&first, b"two");
        let mut chain = Chain::default();
        chain.follow(&change(1, b"one", first.clone())).unwrap();
        chain.follow(&change(2, b"two", second.clone())).unwrap();
        // An entry changed beneath the store: its digest no longer follows.
        let mut chain = Chain::after(Some(first.clone()));
        let said = chain.follow(&change(2, b"tw0", second)).unwrap_err();
        assert!(said.contains("entry 2 of partition P0"), "{said}");
        // A missing entry: the next one's digest follows nothing it is given.
        let third = chained(&chained(&first, b"two"), b"three");
        let mut chain = Chain::after(Some(first));
        assert!(chain.follow(&change(3, b"three", third)).is_err());
    }

    #[test]
    fn the_scrub_runs_weekly_unless_told() {
        assert_eq!(scrub_every(), Some(Duration::from_secs(7 * 86_400)));
    }
}
