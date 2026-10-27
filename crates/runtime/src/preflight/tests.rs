use prost::Message;

use super::*;

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

#[test]
fn quantities_read_as_the_chart_and_the_broker_write_them() {
    assert_eq!(quantity("8Gi"), Some(8 * GIB));
    assert_eq!(quantity("512Mi"), Some(512 * MIB));
    assert_eq!(quantity("1MB"), Some(MIB));
    assert_eq!(quantity("1G"), Some(1_000_000_000));
    assert_eq!(quantity("1048576"), Some(MIB));
    assert_eq!(quantity("lots"), None);
    assert_eq!(quantity(""), None);
}

#[test]
fn disk_warns_at_three_quarters_and_blocks_at_nine_tenths() {
    assert_eq!(judge_disk(GIB, Some(8 * GIB)).verdict, Verdict::Ready);
    assert_eq!(
        judge_disk(6 * GIB + MIB, Some(8 * GIB)).verdict,
        Verdict::Warning
    );
    let blocked = judge_disk(7 * GIB + 600 * MIB, Some(8 * GIB));
    assert_eq!(blocked.verdict, Verdict::Blocked);
    assert!(
        blocked.why.contains("grow the volume first"),
        "{}",
        blocked.why
    );
    // A database the chart does not bring: its operator's to watch.
    assert_eq!(judge_disk(GIB, None).verdict, Verdict::NotChecked);
}

#[test]
fn memory_past_its_limit_warns_and_connections_near_the_max_warn() {
    assert_eq!(
        judge_memory(128 * MIB, 4 * MIB, 100, Some(GIB)).verdict,
        Verdict::Ready
    );
    assert_eq!(
        judge_memory(512 * MIB, 16 * MIB, 100, Some(GIB)).verdict,
        Verdict::Warning
    );
    assert_eq!(judge_memory(1, 1, 1, None).verdict, Verdict::NotChecked);
    assert_eq!(judge_connections(10, 100).verdict, Verdict::Ready);
    assert_eq!(judge_connections(95, 100).verdict, Verdict::Warning);
}

#[test]
fn a_bus_answer_near_the_brokers_limit_warns_and_past_it_blocks() {
    assert_eq!(judge_answer("a", 100 * 1024, MIB).verdict, Verdict::Ready);
    assert_eq!(judge_answer("a", 800 * 1024, MIB).verdict, Verdict::Warning);
    assert_eq!(
        judge_answer("a", 1000 * 1024, MIB).verdict,
        Verdict::Blocked
    );
}

#[test]
fn a_newer_database_is_a_downgrade_and_pending_migrations_warn() {
    assert_eq!(
        judge_schema("lake", Some(5), 4, &[]).verdict,
        Verdict::Blocked
    );
    let pending = judge_schema("lake", Some(3), 4, &[(4, "quality")]);
    assert_eq!(pending.verdict, Verdict::Warning);
    assert!(pending.why.contains("4 quality"), "{}", pending.why);
    assert_eq!(
        judge_schema("lake", Some(4), 4, &[]).verdict,
        Verdict::Ready
    );
    assert_eq!(judge_schema("lake", None, 4, &[]).verdict, Verdict::Ready);
}

#[test]
fn a_migration_that_drops_renames_or_deletes_is_not_additive() {
    assert_eq!(
        not_additive(
            "CREATE TABLE IF NOT EXISTS t (a int);\nALTER TABLE t ADD COLUMN IF NOT EXISTS b int;"
        ),
        None
    );
    assert_eq!(
        not_additive("-- we never DROP TABLE here\nCREATE INDEX i ON t (a);"),
        None
    );
    assert_eq!(not_additive("DROP TABLE old;"), Some("drops a table"));
    assert_eq!(
        not_additive("alter table t drop column a;"),
        Some("drops a column")
    );
    assert_eq!(
        not_additive("ALTER TABLE t RENAME TO u;"),
        Some("renames something")
    );
    assert_eq!(
        not_additive("ALTER TABLE t ALTER COLUMN a TYPE bigint;"),
        Some("changes a column's type or makes it required")
    );
    assert_eq!(
        not_additive("DELETE FROM t WHERE a = 1;"),
        Some("deletes rows")
    );
    let judged = judge_additive(&[
        ("lake".into(), 4, "quality", "CREATE TABLE x (a int);"),
        ("street store".into(), 9, "cleanup", "DROP TABLE y;"),
    ]);
    assert_eq!(judged.verdict, Verdict::Warning);
    assert!(
        judged.why.contains("street store 9 cleanup drops a table"),
        "{}",
        judged.why
    );
    assert_eq!(judge_additive(&[]).verdict, Verdict::Ready);
}

#[test]
fn every_migration_this_release_carries_is_judged() {
    // What this release itself would run against a database a release
    // behind: each named, none failing to read.
    for store in [
        "configuration store",
        "street store",
        "book of record",
        "lake",
        "dashboard",
    ] {
        let (table, migrations) = migrations_of(store).unwrap();
        assert!(table.ends_with("schema_migration"), "{store}");
        assert!(!migrations.is_empty(), "{store}");
        let versions: Vec<i64> = migrations.iter().map(|m| m.0).collect();
        let mut sorted = versions.clone();
        sorted.sort();
        assert_eq!(versions, sorted, "{store}'s migrations are in order");
    }
    assert!(migrations_of("instrument store").is_none());
}

#[test]
fn a_template_this_release_reads_differently_is_named() {
    let template = meridian_domain::v1::SecurityTemplate {
        template_id: "TPL-1".into(),
        template_version: "1".into(),
        name: "Listed option".into(),
        calculation_package: Some(meridian_domain::v1::CalculationPackage {
            model_id: "american_binomial".into(),
            model_version: 1,
            ..Default::default()
        }),
        ..Default::default()
    };
    let bytes = template.encode_to_vec();
    let fine = judge_templates(&[("TPL-1".into(), "1".into(), bytes.clone())]);
    assert_eq!(fine.verdict, Verdict::Ready, "{}", fine.why);
    // A field this release does not know (number 999, a string), appended.
    let mut newer = bytes.clone();
    newer.extend_from_slice(&[0xBA, 0x3E, 0x02, b'h', b'i']);
    let judged = judge_templates(&[("TPL-1".into(), "2".into(), newer)]);
    assert_eq!(judged.verdict, Verdict::Warning);
    assert!(judged.why.contains("does not read"), "{}", judged.why);
    let unknown_model = meridian_domain::v1::SecurityTemplate {
        calculation_package: Some(meridian_domain::v1::CalculationPackage {
            model_id: "heston".into(),
            ..Default::default()
        }),
        ..template
    };
    let judged = judge_templates(&[("TPL-1".into(), "3".into(), unknown_model.encode_to_vec())]);
    assert!(
        judged.why.contains("names the model heston"),
        "{}",
        judged.why
    );
}

#[test]
fn a_backup_older_than_a_day_or_none_warns() {
    let now = 10 * 86_400 * 1_000_000_000_i64;
    assert_eq!(
        judge_backups(Some(now - 3_600 * 1_000_000_000), true, true, now).verdict,
        Verdict::Ready
    );
    assert_eq!(
        judge_backups(Some(now - 2 * 86_400 * 1_000_000_000), true, true, now).verdict,
        Verdict::Warning
    );
    assert_eq!(
        judge_backups(None, true, true, now).verdict,
        Verdict::Warning
    );
    assert!(judge_backups(None, false, false, now)
        .why
        .contains("no backup is configured"));
    // Kept on the administrator's own claim: not mounted, not judged.
    assert_eq!(
        judge_backups(None, true, false, now).verdict,
        Verdict::NotChecked
    );
}

#[test]
fn the_whole_answers_the_worst_and_a_check_not_made_never_passes_or_fails() {
    let ready = finding("a", Verdict::Ready, "");
    let not = finding("b", Verdict::NotChecked, "");
    let warn = finding("c", Verdict::Warning, "");
    let block = finding("d", Verdict::Blocked, "");
    assert_eq!(overall(&[ready.clone(), not.clone()]), Verdict::Ready);
    assert_eq!(overall(&[ready.clone(), warn.clone()]), Verdict::Warning);
    assert_eq!(overall(&[warn, block, not]), Verdict::Blocked);
    let report = report_json("0.1.0", &[ready]);
    assert_eq!(report["preflight"], "ready");
    assert_eq!(report["checks"][0]["check"], "a");
}

#[test]
fn nothing_configured_yet_is_said_and_answers_without_a_database() {
    let findings = run(&Inputs {
        max_payload: MIB,
        now_ns: 1,
        ..Default::default()
    });
    assert!(findings
        .iter()
        .any(|f| f.check == "database" && f.verdict == Verdict::NotChecked));
    assert_ne!(overall(&findings), Verdict::Blocked);
}

// A hook's pod is new at every upgrade, and its first connections can be
// refused before the cluster's network policy admits it (CI run
// 40423226625): the database is waited for, as every component waits for
// its own, and one that never answers still blocks, saying why.

#[test]
fn a_database_refused_at_first_is_waited_for() {
    let mut tries = 0;
    let answered = answering(
        "the test's database",
        Instant::now() + Duration::from_secs(5),
        Duration::from_millis(1),
        || {
            tries += 1;
            if tries < 4 {
                Err("error connecting to server: Connection refused".to_string())
            } else {
                Ok(tries)
            }
        },
    );
    assert_eq!(answered, Ok(4));
}

#[test]
fn a_database_that_never_answers_is_given_up_on_with_its_last_reason() {
    let mut tries = 0;
    let answered: Result<(), String> = answering(
        "the test's database",
        Instant::now() + Duration::from_millis(30),
        Duration::from_millis(5),
        || {
            tries += 1;
            Err(format!("refused, attempt {tries}"))
        },
    );
    let why = answered.unwrap_err();
    assert!(tries > 1, "{tries}");
    assert!(
        why.starts_with(&format!("refused, attempt {tries}")),
        "{why}"
    );
    assert!(why.contains("waited"), "{why}");
    // Past its limit it is still asked once.
    let mut once = 0;
    let late: Result<(), String> = answering(
        "the test's database",
        Instant::now(),
        Duration::from_millis(5),
        || {
            once += 1;
            Err("refused".to_string())
        },
    );
    assert_eq!((once, late), (1, Err("refused".to_string())));
}

#[test]
fn a_database_that_does_not_answer_blocks_and_says_why() {
    // A port nothing listens on: refused, as the k3d cluster refused the
    // hook's first connections.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let findings = run(&Inputs {
        stores: vec![(
            "configuration store",
            format!("postgres://nobody:nothing@127.0.0.1:{port}/none"),
        )],
        max_payload: MIB,
        now_ns: 1,
        database_waits_at_most: Duration::from_millis(50),
        ..Default::default()
    });
    let database = findings.iter().find(|f| f.check == "database").unwrap();
    assert_eq!(database.verdict, Verdict::Blocked, "{database:?}");
    assert!(
        database.why.starts_with(
            "the configuration store's database does not answer: error connecting to server: "
        ),
        "{database:?}"
    );
    // The cause, not only the kind: the client's own words stop at "error
    // connecting to server", which is all CI's first run said.
    assert!(
        database.why.to_lowercase().contains("refused"),
        "{database:?}"
    );
    assert!(
        !database.why.contains("nothing"),
        "the password is never said: {database:?}"
    );
    assert_eq!(overall(&findings), Verdict::Blocked);
}
