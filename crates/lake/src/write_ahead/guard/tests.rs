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
    let mut full = Full::new(STUCK_AFTER, MIB);
    let start = tokio::time::Instant::now();
    let at = |s: u64| start + Duration::from_secs(s);
    // 256 MiB held of 256: no room for the broker's largest write. The lake
    // at 900 of 4,000.
    let stuck = standing(1, 4_000, 256 * MIB - 1_000, now, 500).from_place(901, now - 120 * S);
    assert_eq!(full.looked(at(0), &stuck, 901, &bounds(), now), None);
    assert_eq!(full.looked(at(25), &stuck, 901, &bounds(), now), None);
    let said = full
        .looked(at(30), &stuck, 901, &bounds(), now)
        .expect("stuck full after 30 s at its bound");
    assert!(said.starts_with(STUCK_FULL), "{said}");
    for figure in [
        // Since the first look that found it at a bound, 1,700,000,000 s.
        "for 30s, since 2023-11-14T22:13:20Z: holds 256.0 MiB of its 256.0 MiB bound",
        "the oldest write the lake has not recorded 120s old of its 600s bound",
        "3100 write(s) past the lake's place",
        "the lake at 900, the stream at 4000",
    ] {
        assert!(said.contains(figure), "{figure} in {said}");
    }
    // Drained below its bound by more than the largest write: no longer.
    let drained = standing(3_000, 4_000, 200 * MIB, now, 60);
    assert_eq!(full.looked(at(35), &drained, 3_500, &bounds(), now), None);
    assert_eq!(
        full.looked(at(40), &stuck, 3_500, &bounds(), now + 40 * S),
        None
    );
    assert_eq!(full.looked(at(65), &stuck, 3_500, &bounds(), now), None);
    // Another spell, said since its own first look at a bound.
    let said = full.looked(at(70), &stuck, 3_500, &bounds(), now + 70 * S);
    assert_eq!(
        said.as_deref()
            .and_then(meridian_bus::write_ahead::stuck_since),
        Some(now + 40 * S),
        "{said:?}"
    );
}

/// Full of writes the lake recorded -- its place at the stream's last --
/// is stuck full all the same: a plugin's write is refused whoever's the
/// cause, and the figures say none is past the lake's place.
#[tokio::test(start_paused = true)]
async fn a_stream_full_of_writes_the_lake_recorded_is_stuck_full_saying_none_is_past_its_place() {
    let now = 1_700_000_000 * S;
    let mut full = Full::new(STUCK_AFTER, MIB);
    let start = tokio::time::Instant::now();
    let recorded = standing(1, 4_000, 256 * MIB, now, 590);
    full.looked(start, &recorded, 4_001, &bounds(), now);
    let said = full
        .looked(start + STUCK_AFTER, &recorded, 4_001, &bounds(), now)
        .expect("stuck full");
    assert!(said.contains(" 0 write(s) past the lake's place"), "{said}");
    assert!(
        said.contains("no write the lake has not recorded to age against its 600s bound"),
        "{said}"
    );
}

/// At its age bound: its oldest write one the lake has not recorded, within
/// STUCK_AFTER of being dropped. Never where the oldest is one the lake
/// recorded, nor where it holds none.
#[test]
fn a_stream_is_at_its_age_bound_only_where_its_oldest_write_is_not_recorded_and_about_to_drop() {
    let now = 1_700_000_000 * S;
    let b = bounds();
    let unrecorded_old = standing(100, 200, MIB, now, 580);
    assert!(unrecorded_old.at_a_bound(&b, MIB, 100, now));
    assert!(
        !unrecorded_old.at_a_bound(&b, MIB, 201, now),
        "every write recorded"
    );
    // Its oldest recorded, the lake at 150: its oldest write the lake has
    // not recorded is the one at 150, whose time is read from the stream
    // (the review of the stream guard, finding 3). 580 s old of 600, it is
    // about to be dropped; a minute old, it is not.
    let recorded_old = standing(100, 200, MIB, now, 590);
    assert!(recorded_old.oldest_is_recorded(150));
    assert!(!recorded_old.oldest_is_recorded(100) && !recorded_old.oldest_is_recorded(201));
    let about_to_drop = recorded_old.from_place(150, now - 580 * S);
    assert_eq!((about_to_drop.first, about_to_drop.past(150)), (150, 51));
    assert!(
        about_to_drop.at_a_bound(&b, MIB, 150, now),
        "the write at the lake's place is about to be dropped"
    );
    assert!(!recorded_old
        .from_place(150, now - 60 * S)
        .at_a_bound(&b, MIB, 150, now));
    assert_eq!(
        recorded_old.from_place(100, now - 5 * S),
        recorded_old,
        "its oldest is the one at the place: nothing read"
    );
    let young = standing(100, 200, MIB, now, 60);
    assert!(!young.at_a_bound(&b, MIB, 100, now));
    let empty = Standing {
        messages: 0,
        bytes: 0,
        first: 201,
        last: 200,
        oldest_ns: 0,
    };
    assert!(!empty.at_a_bound(&b, MIB, 1, now));
    assert_eq!(empty.past(1), 0);
}

/// Never made again over writes the lake has not recorded (v31a item 5):
/// an age bound its oldest unrecorded write is past is refused, naming the
/// stream; one that drops nothing the lake has not recorded is not, and a
/// byte bound lowered under what the stream holds drops none (the review of
/// the stream guard, finding 1): the stream keeps its writes and refuses
/// new ones.
#[test]
fn bounds_that_would_drop_a_write_the_lake_has_not_recorded_are_refused_naming_the_stream() {
    let now = 1_700_000_000 * S;
    let lower_bytes = Bounds {
        max_bytes: 64 << 20,
        ..bounds()
    };
    let held = standing(1, 4_000, 100 * MIB, now, 300);
    assert_eq!(
        bounds_drop("LAKE_WRITE_AHEAD", &held, 3_000, &lower_bytes, now),
        None,
        "a byte bound under what it holds drops no write"
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
    assert!(refused.contains("LAKE_WRITE_AHEAD"), "{refused}");
    assert!(
        refused.contains("4000 write(s) the lake has not recorded"),
        "{refused}"
    );
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

/// The review of the stream guard, finding 1: the bounds a look is judged
/// by are the stream's own, and a bound the stream does not have -- the
/// broker's -1 bytes, 0 s -- is never stood at. A refused bring-to-bounds is
/// said in the member's detail: after the stuck-full words, which stay
/// first for the dashboard's judge, or alone.
#[test]
fn a_stream_is_judged_by_its_own_bounds_and_a_refused_bring_to_bounds_is_said() {
    let now = 1_700_000_000 * S;
    // 137 MiB held: at the bound of a stream made at 128 MiB, not of one
    // made at 256.
    let held = standing(1, 160, 137 * MIB, now, 60);
    let at_128 = Bounds {
        max_bytes: 128 << 20,
        ..bounds()
    };
    assert!(held.at_a_bound(&at_128, MIB, 1, now));
    assert!(!held.at_a_bound(&bounds(), MIB, 1, now));
    let none = Bounds {
        max_bytes: -1,
        max_age: Duration::ZERO,
        ..bounds()
    };
    assert!(!standing(1, 160, 137 * MIB, now, 86_400).at_a_bound(&none, MIB, 1, now));
    // The room a write needs is the broker's largest message, as the
    // broker says it (finding 5): 137 MiB of 140 has room for a 1 MiB
    // write, not for a 4 MiB one.
    let at_140 = Bounds {
        max_bytes: 140 << 20,
        ..bounds()
    };
    assert!(!held.at_a_bound(&at_140, MIB, 1, now));
    assert!(held.at_a_bound(&at_140, 4 * MIB, 1, now));
    let kept = "the write-ahead stream LAKE_WRITE_AHEAD is kept as it is";
    assert_eq!(said(None, None), None);
    assert_eq!(said(None, Some(kept)).as_deref(), Some(kept));
    let both = said(Some(format!("{STUCK_FULL} for 30s")), Some(kept)).unwrap();
    assert!(
        both.starts_with(STUCK_FULL) && both.ends_with(kept),
        "{both}"
    );
}

/// The lowered byte bound: a byte bound under what the stream holds is not
/// applied -- the stream would refuse every write until the writes it holds
/// aged out -- and its member says why. One the stream's writes fit, the
/// same bound, and a higher one are applied.
#[test]
fn a_byte_bound_lower_than_the_stream_holds_is_not_applied_and_said() {
    let now = 1_700_000_000 * S;
    let held = standing(1, 160, 137 * MIB, now, 60);
    let to = |mib: i64| Bounds {
        max_bytes: mib << 20,
        ..bounds()
    };
    let kept = bytes_kept("LAKE_WRITE_AHEAD", &held, 256 << 20, &to(128), MIB)
        .expect("137 MiB held of a new 128 MiB bound");
    for part in [
        "LAKE_WRITE_AHEAD keeps its 256.0 MiB bound",
        "this release's 128.0 MiB",
        "it holds 137.0 MiB",
    ] {
        assert!(kept.contains(part), "{part} in {kept}");
    }
    assert_eq!(
        bytes_kept("S", &held, 256 << 20, &to(192), MIB),
        None,
        "it fits"
    );
    assert_eq!(
        bytes_kept("S", &held, 256 << 20, &to(256), MIB),
        None,
        "the same"
    );
    assert_eq!(
        bytes_kept("S", &held, 128 << 20, &to(512), MIB),
        None,
        "raised"
    );
    // A stream with no byte bound is lowered to one only where it fits.
    assert!(bytes_kept("S", &held, -1, &to(128), MIB)
        .is_some_and(|kept| kept.contains("keeps no byte bound")));
    assert_eq!(bytes_kept("S", &held, -1, &to(256), MIB), None);
}

/// The review of 4c, fix B item 3: a stream within one of the broker's
/// largest writes of the lower byte bound keeps its own too. Lowered, it
/// would stand at the bound and refuse its next write of that size.
#[test]
fn a_stream_within_one_largest_write_of_a_lower_byte_bound_keeps_its_bound() {
    let now = 1_700_000_000 * S;
    let to = |mib: i64| Bounds {
        max_bytes: mib << 20,
        ..bounds()
    };
    let largest = 8 * MIB;
    // 124 MiB held of a new 128: half a largest write under it.
    let near = standing(1, 160, 128 * MIB - largest / 2, now, 60);
    let kept = bytes_kept("LAKE_WRITE_AHEAD", &near, 256 << 20, &to(128), largest)
        .expect("no room for the broker's largest write under the new bound");
    for part in [
        "LAKE_WRITE_AHEAD keeps its 256.0 MiB bound",
        "this release's 128.0 MiB",
        "it holds 124.0 MiB",
        "8.0 MiB",
    ] {
        assert!(kept.contains(part), "{part} in {kept}");
    }
    // Room for exactly one largest write: the bound is applied.
    let room = standing(1, 160, 128 * MIB - largest, now, 60);
    assert_eq!(bytes_kept("S", &room, 256 << 20, &to(128), largest), None);
    // The same and a higher bound are applied whatever it holds.
    assert_eq!(bytes_kept("S", &near, 128 << 20, &to(128), largest), None);
    assert_eq!(bytes_kept("S", &near, 128 << 20, &to(256), largest), None);
}
