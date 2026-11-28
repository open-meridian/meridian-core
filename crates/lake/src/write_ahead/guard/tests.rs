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

/// The broker's refusal of the guard's looks, or its silence, over a while
/// (INTERIM(v32), the proposal of 2026-11-27, A1): the times are looks,
/// every LOOK_EVERY, from `start`.
fn refusals(
    refused: &mut Refused,
    start: tokio::time::Instant,
    from_s: u64,
    to_s: u64,
    now: i64,
) -> Option<String> {
    let mut said = None;
    for s in (from_s..=to_s).step_by(LOOK_EVERY.as_secs() as usize) {
        said = refused.refused(
            start + Duration::from_secs(s),
            now + s as i64 * S,
            "JetStream system temporarily unavailable (503, 10008)",
        );
    }
    said
}

/// 30 s of looks the broker refused: the writer's member says so, its own
/// state, apart from stuck full, since the first refusal and with the
/// broker's last words; not a look before; never stuck full.
#[tokio::test(start_paused = true)]
async fn thirty_seconds_of_refused_looks_are_said_as_their_own_state() {
    let now = 1_700_000_000 * S;
    let start = tokio::time::Instant::now();
    let mut refused = Refused::new(REFUSED_AFTER);
    assert_eq!(
        refusals(&mut refused, start, 0, 25, now),
        None,
        "25 s: nothing yet"
    );
    let said = refused
        .refused(
            start + REFUSED_AFTER,
            now + 30 * S,
            "JetStream not enabled (10076)",
        )
        .expect("refused for 30 s");
    assert!(said.starts_with(REFUSED), "{said}");
    assert!(!said.contains(STUCK_FULL), "{said}");
    assert_eq!(
        meridian_bus::write_ahead::refused_since(&said),
        Some(now),
        "since the first refusal: {said}"
    );
    for part in ["for 30s", "JetStream not enabled (10076)"] {
        assert!(said.contains(part), "{part} in {said}");
    }
    assert!(!said.contains("disk"), "nothing inferred: {said}");
    // It goes on being said while the broker refuses.
    assert!(refused
        .refused(start + Duration::from_secs(35), now + 35 * S, "timed out")
        .is_some_and(|said| said.contains("for 35s")));
}

/// Never on a single refusal: a broker answering again within 30 s says
/// nothing, and a write it accepted then ends the spell; the bound counts
/// again from the next refusal.
#[tokio::test(start_paused = true)]
async fn twenty_nine_seconds_of_refusals_then_an_accepted_write_say_nothing() {
    let now = 1_700_000_000 * S;
    let start = tokio::time::Instant::now();
    let at = |s: u64| start + Duration::from_secs(s);
    let mut refused = Refused::new(REFUSED_AFTER);
    assert_eq!(refused.answered(at(0), 100), None);
    assert_eq!(refusals(&mut refused, start, 1, 26, now), None);
    assert_eq!(refused.refused(at(29), now + 29 * S, "refused"), None);
    assert_eq!(
        refused.answered(at(29) + Duration::from_millis(500), 101),
        None,
        "a write accepted at 29.5 s"
    );
    assert_eq!(
        refused.refused(at(31), now + 31 * S, "refused"),
        None,
        "the spell begins again at 31 s"
    );
    assert_eq!(
        refusals(&mut refused, start, 36, 56, now),
        None,
        "25 s of the new spell"
    );
    assert!(
        refused.refused(at(61), now + 61 * S, "refused").is_some(),
        "30 s of it"
    );
    // One answer within the bound, no write: the spell ends unsaid all the same.
    let mut answered = Refused::new(REFUSED_AFTER);
    assert_eq!(refusals(&mut answered, start, 0, 25, now), None);
    assert_eq!(answered.answered(at(27), 100), None);
    assert_eq!(answered.refused(at(30), now + 30 * S, "refused"), None);
}

/// A look the broker does not answer counts as a refused one: 30 s of
/// silence, or of silence and refusals mixed, is said the same way.
#[tokio::test(start_paused = true)]
async fn unanswered_looks_count_as_refused_ones() {
    let now = 1_700_000_000 * S;
    let start = tokio::time::Instant::now();
    let at = |s: u64| start + Duration::from_secs(s);
    let mut silent = Refused::new(REFUSED_AFTER);
    // A look timing out takes its own five seconds: one every ten.
    for s in [0, 10, 20] {
        assert_eq!(silent.refused(at(s), now + s as i64 * S, "timed out"), None);
    }
    let said = silent
        .refused(at(30), now + 30 * S, "timed out")
        .expect("30 s unanswered");
    assert!(
        said.starts_with(REFUSED) && said.contains("timed out"),
        "{said}"
    );
    let mut mixed = Refused::new(REFUSED_AFTER);
    mixed.refused(at(0), now, "no responders");
    mixed.refused(
        at(10),
        now + 10 * S,
        "JetStream system temporarily unavailable (503, 10008)",
    );
    mixed.refused(at(20), now + 20 * S, "timed out");
    assert!(mixed
        .refused(at(30), now + 30 * S, "no responders")
        .is_some());
}

/// Said, it stands while the broker refuses and while its answers show no
/// write accepted, and clears at the first look finding one: the stream's
/// last write past where the last answered look found it. A stream made
/// again under its name, its last lower, is counted from there.
#[tokio::test(start_paused = true)]
async fn the_state_clears_once_a_write_is_accepted_again() {
    let now = 1_700_000_000 * S;
    let start = tokio::time::Instant::now();
    let at = |s: u64| start + Duration::from_secs(s);
    let mut refused = Refused::new(REFUSED_AFTER);
    assert_eq!(refused.answered(at(0), 4_000), None);
    assert!(refusals(&mut refused, start, 5, 35, now).is_some());
    // Answered again, the stream where it was: no write accepted yet.
    let still = refused
        .answered(at(40), 4_000)
        .expect("no write accepted since");
    assert!(still.starts_with(REFUSED), "{still}");
    assert_eq!(
        meridian_bus::write_ahead::refused_since(&still),
        Some(now + 5 * S)
    );
    assert!(
        refused.refused(at(45), now + 45 * S, "refused").is_some(),
        "refused again: still said"
    );
    assert_eq!(
        refused.answered(at(50), 4_001),
        None,
        "a write accepted: cleared"
    );
    assert_eq!(refused.answered(at(55), 4_001), None);
    // A later spell is its own, since its own first refusal.
    assert_eq!(refusals(&mut refused, start, 60, 85, now), None);
    let again = refused
        .refused(at(90), now + 90 * S, "refused")
        .expect("a second spell");
    assert_eq!(
        meridian_bus::write_ahead::refused_since(&again),
        Some(now + 60 * S)
    );
    // The stream made again while refused: its last lower, counted from it.
    assert!(refused.answered(at(95), 3).is_some());
    assert_eq!(refused.answered(at(100), 4), None);
}

/// Both said at once -- a stream stuck full, its broker then refusing the
/// looks -- the stuck-full words first, as the dashboard reads them, and
/// the refusal's after them, each read back.
#[test]
fn stuck_full_and_refused_are_said_together_stuck_first() {
    let stuck = meridian_bus::write_ahead::stuck_full_since(40, 0);
    let refused = meridian_bus::write_ahead::refused_for(30, 30 * S);
    let both = said(said(Some(stuck), Some(&refused)), None).unwrap();
    assert_eq!(meridian_bus::write_ahead::stuck_since(&both), Some(0));
    assert_eq!(
        meridian_bus::write_ahead::refused_since(&both),
        Some(30 * S)
    );
}

/// A spell is dated from when its first refused or unanswered look was
/// sent, not from when that look returned (the consensus of 2026-11-28,
/// part 2 (B)): the broker stopped answering by then, and an unanswered
/// look returns only at its request's 5 s timeout. Looks sent at 0, 10 and
/// 20 s, each timing out 5 s on: at the third's return, 25 s, nothing; the
/// fourth, sent at 30 s, is said at its return, 35 s after the first was
/// sent, since then, in those words. A look returning 30 s after the first
/// was sent says it then: dated from the first's return, a look later.
#[tokio::test(start_paused = true)]
async fn a_spell_is_dated_from_when_its_first_look_was_sent() {
    let now = 1_700_000_000 * S;
    let start = tokio::time::Instant::now();
    let at = |s: u64| start + Duration::from_secs(s);
    let timeout = 5;
    let mut refused = Refused::new(REFUSED_AFTER);
    for sent in [0, 10, 20] {
        assert_eq!(
            refused.refused_look(
                at(sent),
                now + sent as i64 * S,
                at(sent + timeout),
                "timed out"
            ),
            None,
            "the look sent at {sent} s"
        );
    }
    let said = refused
        .refused_look(at(30), now + 30 * S, at(30 + timeout), "timed out")
        .expect("every look for 30 s unanswered, from the first one sent");
    assert_eq!(
        meridian_bus::write_ahead::refused_since(&said),
        Some(now),
        "since the first look was sent: {said}"
    );
    assert!(said.contains("for 35s"), "{said}");

    // A look that returns 30 s after the first was sent says it then: an
    // unanswered look 25 s on, timing out at 30 s.
    let mut slow = Refused::new(REFUSED_AFTER);
    assert_eq!(slow.refused_look(at(0), now, at(5), "timed out"), None);
    assert!(slow
        .refused_look(at(25), now + 25 * S, at(30), "timed out")
        .is_some_and(|said| said.contains("for 30s")));
}

/// The diagnosis of bench-0m's stuck-full stream (2026-11-28), question 2.
/// FAILS on main (2ac4c91f): the guard says a stream *stuck full* whenever
/// it stands at its byte bound, whatever the lake's place. bench-0m's
/// stream stood so with the lake caught up -- 54,830 writes held in 256
/// MiB, the lake's place past the last -- because under `Limits` retention
/// a recorded write leaves the stream only at its age bound: nothing was
/// stuck, and nothing the lake could do would drain it. What such a stream
/// is said with instead is the coordinator's to decide; this holds only
/// that it is not the stuck-full words, which the dashboard files as
/// `component-write-ahead-stuck-full`. It contradicts
/// `a_stream_full_of_writes_the_lake_recorded_is_stuck_full_saying_none_is_past_its_place`
/// above, which pins today's behaviour: a fix changes one of the two.
#[tokio::test(start_paused = true)]
async fn a_stream_at_its_byte_bound_whose_every_write_the_lake_recorded_is_not_said_stuck_full() {
    let now = 1_700_000_000 * S;
    let mut full = Full::new(STUCK_AFTER, MIB);
    let start = tokio::time::Instant::now();
    // bench-0m at 22:53:38Z: the stream at its last write, 54,830, the
    // lake's place the one after it; its oldest write 9 minutes old.
    let recorded = standing(1, 54_830, 256 * MIB - 5_000, now, 540);
    assert_eq!(recorded.past(54_831), 0, "the lake has recorded every write");
    full.looked(start, &recorded, 54_831, &bounds(), now);
    let said = full.looked(start + STUCK_AFTER, &recorded, 54_831, &bounds(), now);
    assert!(
        !said.as_deref().is_some_and(|said| said.starts_with(STUCK_FULL)),
        "a stream whose every write the lake recorded is said stuck full: {said:?}"
    );
}

/// The diagnosis of bench-0m's stuck-full stream (2026-11-28), question 1:
/// the mechanism, on a real broker. PASSES on main. The stream as the lake
/// makes it (`Limits`, `discard: new`), filled to its byte bound, every
/// write then read and acknowledged -- the lake caught up -- still refuses
/// a new write ("maximum bytes exceeded"), and goes on refusing until the
/// writes it holds reach its age bound: recording a write frees no room.
/// So above `max_bytes / max_age` bytes a second the stream stands at its
/// byte bound for good, taking only what ages out.
///
/// Run by hand, with JetStream on: `docker run --rm -d -p 14333:4222
/// nats:2.10.29-alpine -js`, then `MERIDIAN_TEST_JETSTREAM_URL=
/// nats://localhost:14333 cargo test -p meridian-lake --lib
/// a_stream_the_lake_recorded_whole_refuses_writes_at_its_byte_bound_until_they_age_out
/// -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a JetStream broker: MERIDIAN_TEST_JETSTREAM_URL"]
async fn a_stream_the_lake_recorded_whole_refuses_writes_at_its_byte_bound_until_they_age_out() {
    use async_nats::jetstream::{self, consumer};
    use futures_util::StreamExt as _;
    use meridian_bus::write_ahead::{subject, STREAM};
    let url = std::env::var("MERIDIAN_TEST_JETSTREAM_URL").unwrap();
    let client = async_nats::connect(&url).await.unwrap();
    let context = jetstream::new(client.clone());
    let _ = context.delete_stream(STREAM).await;
    let made = Bounds {
        max_age: Duration::from_secs(20),
        max_bytes: 2 << 20,
        replicas: 1,
    };
    let mut stream = context
        .create_stream(crate::write_ahead::stream_config(&made))
        .await
        .unwrap();
    let to = subject("drive-data", "scale0");
    // bench-0m's write: 20 rows, about 5 KB in the stream.
    let write = vec![7u8; 5_000];
    let words = |failed: &dyn std::error::Error| {
        let mut said = failed.to_string();
        let mut cause = failed.source();
        while let Some(next) = cause {
            said.push_str(&format!(": {next}"));
            cause = next.source();
        }
        said
    };
    let began = std::time::Instant::now();
    let mut held = 0u64;
    let refused = loop {
        let sent = context.publish(to.clone(), write.clone().into()).await.unwrap();
        match sent.await {
            Ok(_) => held += 1,
            Err(failed) => break words(&failed),
        }
        assert!(held < 10_000, "the stream never refused a write");
    };
    let filled_in = began.elapsed();
    assert!(refused.contains("maximum bytes exceeded"), "{refused}");
    // The lake records every write it holds: each read and acknowledged.
    let reader = stream
        .create_consumer(consumer::pull::Config {
            ack_policy: consumer::AckPolicy::Explicit,
            ..Default::default()
        })
        .await
        .unwrap();
    let mut messages = reader.messages().await.unwrap();
    for _ in 0..held {
        let message = messages.next().await.unwrap().unwrap();
        message.double_ack().await.unwrap();
    }
    // Caught up, and refused all the same.
    let still = context.publish(to.clone(), write.clone().into()).await.unwrap();
    let still = words(&still.await.expect_err("refused though every write was recorded"));
    assert!(still.contains("maximum bytes exceeded"), "{still}");
    let info = stream.info().await.unwrap().clone();
    let standing = Standing::of(&info.state);
    let next = standing.last + 1;
    assert_eq!((standing.messages, standing.past(next)), (held, 0));
    // What the guard says of it today, shown, not asserted (the test above
    // holds what it should not say).
    let mut full = Full::new(Duration::ZERO, 1 << 20);
    let now_ns = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap();
    let held_bounds = Bounds {
        max_age: info.config.max_age,
        max_bytes: info.config.max_bytes,
        replicas: info.config.num_replicas,
    };
    eprintln!(
        "held {held} writes, {} bytes of {}; filled in {filled_in:?}; the guard: {:?}",
        standing.bytes,
        made.max_bytes,
        full.looked(tokio::time::Instant::now(), &standing, next, &held_bounds, now_ns)
    );
    // Refused until the first writes reach the age bound, then taken.
    let taken_after = loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let sent = context.publish(to.clone(), write.clone().into()).await.unwrap();
        if sent.await.is_ok() {
            break began.elapsed();
        }
        assert!(
            began.elapsed() < made.max_age * 2,
            "no write taken within twice the age bound"
        );
    };
    eprintln!(
        "a write was taken again {taken_after:?} after the first write, the age bound {:?}",
        made.max_age
    );
    assert!(
        taken_after >= made.max_age,
        "a write was taken {taken_after:?} after the first, before any reached the age bound"
    );
}
