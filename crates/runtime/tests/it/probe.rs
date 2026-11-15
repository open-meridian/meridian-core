//! The probe's reads of a member's database, against Postgres.

use std::time::{Duration, Instant};

fn client() -> postgres::Client {
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    );
    postgres::Client::connect(&url, postgres::NoTls).unwrap()
}

/// Review finding 13: the partitions are read inside the database path's
/// bound, so a slow read of them -- a count over a large book -- must not
/// use that bound up and have a healthy database reported as not
/// answering. It is given a second of its own; past it the probe says no
/// partitions, and the session answers what is asked next.
#[test]
fn a_slow_read_of_the_partitions_is_given_up_and_the_session_answers_on() {
    let mut client = client();
    // The lake's partitions, as slow to read as a count over a large book:
    // this session's own, in place of the table.
    client
        .batch_execute(
            "CREATE TEMP VIEW lake_partition AS \
             SELECT 'feed:prices'::text AS dataset, 0 AS writer_group FROM pg_sleep(4)",
        )
        .unwrap();
    let began = Instant::now();
    let read = meridian_runtime::probe::partitions_read(&mut client, "lake");
    assert_eq!(read, None, "a read past its bound says no partitions");
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "the read was waited for {:?}",
        began.elapsed()
    );
    // The bound was the read's alone: the session's own is as it was.
    let bound: String = client
        .query_one("SHOW statement_timeout", &[])
        .unwrap()
        .get(0);
    assert_eq!(bound, "0");
    let one: i32 = client.query_one("SELECT 1", &[]).unwrap().get(0);
    assert_eq!(one, 1);
}
