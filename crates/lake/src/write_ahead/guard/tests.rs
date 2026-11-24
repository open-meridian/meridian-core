use super::*;

const S: i64 = 1_000_000_000;
const MIB: u64 = 1 << 20;

fn bounds() -> Bounds {
    Bounds {
        max_age: Duration::from_secs(600),
        max_bytes: 256 << 20,
        replicas: 3,
    }
}

/// A stream of writes 1..=last, `bytes` held, its oldest `old_s` seconds old
/// at `now_ns`.
fn standing(first: u64, last: u64, bytes: u64, now_ns: i64, old_s: i64) -> Standing {
    Standing {
        messages: last + 1 - first,
        bytes,
        first,
        last,
        oldest_ns: now_ns - old_s * S,
    }
}

/// v31a item 5 (0-J): a stream standing at its byte bound at every look
/// for STUCK_AFTER is stuck full, said with the figures that tell its
/// causes apart; not a look before; and a look below its bound clears it,
/// the clock starting again from the next look at a bound.
#[tokio::test(start_paused = true)]
async fn a_stream_at_its_byte_bound_for_the_stuck_bound_is_said_stuck_full_until_it_drains() {
    let now = 1_700_000_000 * S;
    let mut full = Full::new(STUCK_AFTER);
    let start = tokio::time::Instant::now();
    let at = |s: u64| start + Duration::from_secs(s);
    // 256 MiB held of 256: no room for the broker's largest write. The lake
    // at 900 of 4,000.
    let stuck = standing(1, 4_000, 256 * MIB - 1_000, now, 120);
    assert_eq!(full.looked(at(0), &stuck, 901, &bounds(), now), None);
    assert_eq!(full.looked(at(25), &stuck, 901, &bounds(), now), None);
    let said = full
        .looked(at(30), &stuck, 901, &bounds(), now)
        .expect("stuck full after 30 s at its bound");
    assert!(said.starts_with(STUCK_FULL), "{said}");
    for figure in [
        "for 30s",
        "holds 256.0 MiB of its 256.0 MiB bound",
        "its oldest write 120s old of its 600s bound",
        "3100 write(s) past the lake's place",
        "the lake at 900, the stream at 4000",
    ] {
        assert!(said.contains(figure), "{figure} in {said}");
    }
    // Drained below its bound by more than the largest write: no longer.
    let drained = standing(3_000, 4_000, 200 * MIB, now, 60);
    assert_eq!(full.looked(at(35), &drained, 3_500, &bounds(), now), None);
    assert_eq!(full.looked(at(40), &stuck, 3_500, &bounds(), now), None);
    assert_eq!(full.looked(at(65), &stuck, 3_500, &bounds(), now), None);
    assert!(full.looked(at(70), &stuck, 3_500, &bounds(), now).is_some());
}

/// Full of writes the lake recorded -- its place at the stream's last --
/// is stuck full all the same: a plugin's write is refused whoever's the
/// cause, and the figures say none is past the lake's place.
#[tokio::test(start_paused = true)]
async fn a_stream_full_of_writes_the_lake_recorded_is_stuck_full_saying_none_is_past_its_place() {
    let now = 1_700_000_000 * S;
    let mut full = Full::new(STUCK_AFTER);
    let start = tokio::time::Instant::now();
    let recorded = standing(1, 4_000, 256 * MIB, now, 590);
    full.looked(start, &recorded, 4_001, &bounds(), now);
    let said = full
        .looked(start + STUCK_AFTER, &recorded, 4_001, &bounds(), now)
        .expect("stuck full");
    assert!(said.contains(" 0 write(s) past the lake's place"), "{said}");
}

/// At its age bound: its oldest write one the lake has not recorded, within
/// STUCK_AFTER of being dropped. Never where the oldest is one the lake
/// recorded, nor where it holds none.
#[test]
fn a_stream_is_at_its_age_bound_only_where_its_oldest_write_is_not_recorded_and_about_to_drop() {
    let now = 1_700_000_000 * S;
    let b = bounds();
    let unrecorded_old = standing(100, 200, MIB, now, 580);
    assert!(unrecorded_old.at_a_bound(&b, 100, now));
    assert!(
        !unrecorded_old.at_a_bound(&b, 201, now),
        "every write recorded"
    );
    let recorded_old = standing(100, 200, MIB, now, 580);
    assert!(
        !recorded_old.at_a_bound(&b, 150, now),
        "its oldest recorded"
    );
    let young = standing(100, 200, MIB, now, 60);
    assert!(!young.at_a_bound(&b, 100, now));
    let empty = Standing {
        messages: 0,
        bytes: 0,
        first: 201,
        last: 200,
        oldest_ns: 0,
    };
    assert!(!empty.at_a_bound(&b, 1, now));
    assert_eq!(empty.past(1), 0);
}

/// Never made again over writes the lake has not recorded (v31a item 5):
/// a bound lowered under what the stream holds past the lake's place is
/// refused, naming the stream; one that drops nothing the lake has not
/// recorded is not.
#[test]
fn bounds_that_would_drop_a_write_the_lake_has_not_recorded_are_refused_naming_the_stream() {
    let now = 1_700_000_000 * S;
    let lower_bytes = Bounds {
        max_bytes: 64 << 20,
        ..bounds()
    };
    let held = standing(1, 4_000, 100 * MIB, now, 300);
    let refused = bounds_drop("LAKE_WRITE_AHEAD", &held, 3_000, &lower_bytes, now)
        .expect("a byte bound under what it holds, writes past the place");
    assert!(refused.contains("LAKE_WRITE_AHEAD"), "{refused}");
    assert!(
        refused.contains("1001 write(s) the lake has not recorded"),
        "{refused}"
    );
    assert_eq!(
        bounds_drop("LAKE_WRITE_AHEAD", &held, 4_001, &lower_bytes, now),
        None,
        "every write recorded: nothing the lake has not recorded is dropped"
    );
    let shorter = Bounds {
        max_age: Duration::from_secs(20),
        ..bounds()
    };
    // Its oldest past the place, 300 s old, against 20 s.
    let refused = bounds_drop("LAKE_WRITE_AHEAD", &held, 1, &shorter, now)
        .expect("an age bound its oldest unrecorded write is past");
    assert!(
        refused.contains("300s old, past the 20s bound"),
        "{refused}"
    );
    // The oldest past the place 5 s old (the caller read it), the stream's
    // own oldest recorded: nothing unrecorded is dropped.
    let young_past = standing(3_000, 4_000, 100 * MIB, now, 5);
    assert_eq!(
        bounds_drop("LAKE_WRITE_AHEAD", &young_past, 3_000, &shorter, now),
        None
    );
    assert_eq!(
        bounds_drop("LAKE_WRITE_AHEAD", &held, 3_000, &bounds(), now),
        None,
        "the same bounds drop nothing"
    );
}
