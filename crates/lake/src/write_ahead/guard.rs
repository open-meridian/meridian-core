//! The write-ahead stream's guard while the stream exists (v31a item 5,
//! 0-J; INTERIM(v32): decision 3 removes the stream).
//!
//! **Stuck full.** The stream refuses a new write once it holds its bound
//! (`discard: new`), and drops a write older than its age bound. Standing
//! at either bound for a while is no fault of a plugin's: the guard looks at
//! the stream every [`LOOK_EVERY`], and where every look for [`STUCK_AFTER`]
//! found it at a bound, it is *stuck full*. The writer's member says so in
//! its report, degraded, with the figures that tell its causes apart -- the
//! bytes it holds against its bound, its oldest write's age against its age
//! bound, and how many writes it holds past the lake's place -- until a
//! look finds it below both. The dashboard files that as a standing notice
//! and its clearing ([`STUCK_FULL`]). Why a stream stands full is not
//! judged here. The bounds are the stream's own, as its leader says them
//! at that look, never the chart's: a stream kept as it was made is judged
//! as it was made.
//!
//! **Never made again over writes the lake has not recorded.** The writer
//! makes the stream only where the broker says there is none, and brings it
//! to this release's bounds only where that drops no write past the lake's
//! place ([`bounds_drop`]): an age bound its oldest such write is past is
//! refused, naming the stream, the stream kept as it is and its member
//! saying so ([`said`]). A lower byte bound drops none, but under one the
//! stream holds more than, it refuses every new write (`discard: new`)
//! until the writes it holds age out: the lake does not refuse its own
//! writes over a setting, so such a bound is not applied ([`bytes_kept`]).
//! The stream keeps its own byte bound, its member saying so, and the next
//! writer's start applies the bound once what the stream holds fits it.

use std::time::Duration;

use async_nats::jetstream::stream;

use super::Bounds;

/// How often the guard looks at the stream.
pub const LOOK_EVERY: Duration = Duration::from_secs(5);

/// How long the stream stands at a bound, at every look, before it is said
/// stuck full: twice the writer's stall bound ([`super::STALL_AFTER`]), so
/// a reader made again from its place has had its chance to drain it.
pub const STUCK_AFTER: Duration = Duration::from_secs(30);

/// The room a stream must have for a write not to be refused: the broker's
/// largest message (`broker.limits.maxPayload`, 1 MiB), so a stream with
/// less free is at its byte bound for some write.
pub const LARGEST_WRITE: u64 = 1 << 20;

/// The words a stuck-full stream is said with, first in the writer
/// member's report (`ComponentReport.detail`): what the dashboard's judge
/// reads to file its standing notice, and `meridian status` shows.
pub use meridian_bus::write_ahead::STUCK_FULL;

/// The stream as its leader says it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    pub messages: u64,
    pub bytes: u64,
    /// Its first and last sequences.
    pub first: u64,
    pub last: u64,
    /// Its oldest write's time, ns since the epoch.
    pub oldest_ns: i64,
}

impl Standing {
    pub fn of(state: &stream::State) -> Self {
        Self {
            messages: state.messages,
            bytes: state.bytes,
            first: state.first_sequence,
            last: state.last_sequence,
            oldest_ns: i64::try_from(state.first_timestamp.unix_timestamp_nanos()).unwrap_or(0),
        }
    }

    /// Its writes past the lake's place `next`: the writes it holds that
    /// the lake has not recorded.
    pub fn past(&self, next: u64) -> u64 {
        if self.messages == 0 {
            return 0;
        }
        (self.last + 1).saturating_sub(next.max(self.first))
    }

    /// Its oldest write's age at `now_ns`; none where it holds none.
    pub fn oldest_age(&self, now_ns: i64) -> Option<Duration> {
        (self.messages > 0)
            .then(|| Duration::from_nanos(now_ns.saturating_sub(self.oldest_ns).max(0) as u64))
    }

    /// Whether it stands at its byte bound -- some write refused -- or its
    /// oldest write is one the lake has not recorded and is about to be
    /// dropped at its age bound.
    pub fn at_a_bound(&self, bounds: &Bounds, next: u64, now_ns: i64) -> bool {
        // A bound of none (the broker's -1 bytes, 0 s) is never stood at.
        let max_bytes = u64::try_from(bounds.max_bytes).unwrap_or(0);
        let by_bytes = max_bytes > 0 && self.bytes.saturating_add(LARGEST_WRITE) > max_bytes;
        let by_age = !bounds.max_age.is_zero()
            && self.first >= next
            && self.past(next) > 0
            && self
                .oldest_age(now_ns)
                .is_some_and(|age| age + STUCK_AFTER >= bounds.max_age);
        by_bytes || by_age
    }

    /// The words it is said with while stuck full: [`STUCK_FULL`], then
    /// the figures that tell its causes apart.
    pub fn words(&self, bounds: &Bounds, next: u64, now_ns: i64, stuck_for: Duration) -> String {
        let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
        let age = self.oldest_age(now_ns).unwrap_or_default();
        format!(
            "{STUCK_FULL} for {}s: holds {:.1} MiB of its {:.1} MiB bound, its oldest write {}s \
             old of its {}s bound, {} write(s) past the lake's place (the lake at {}, the stream \
             at {}); a write it has no room for is refused for load, none it holds is dropped",
            stuck_for.as_secs(),
            mib(self.bytes),
            mib(u64::try_from(bounds.max_bytes).unwrap_or(0)),
            age.as_secs(),
            bounds.max_age.as_secs(),
            self.past(next),
            next.saturating_sub(1),
            self.last
        )
    }
}

/// The guard's memory of its looks.
#[derive(Debug)]
pub struct Full {
    after: Duration,
    /// Since when every look found the stream at a bound.
    since: Option<tokio::time::Instant>,
}

impl Full {
    pub fn new(after: Duration) -> Self {
        Self { after, since: None }
    }

    /// One look at `at`: the stream standing so, the lake's place `next`,
    /// the stream's own `bounds` at that look, the wall clock `now_ns`. The words it is said with while stuck full;
    /// none while it is not.
    pub fn looked(
        &mut self,
        at: tokio::time::Instant,
        standing: &Standing,
        next: u64,
        bounds: &Bounds,
        now_ns: i64,
    ) -> Option<String> {
        if !standing.at_a_bound(bounds, next, now_ns) {
            self.since = None;
            return None;
        }
        let since = *self.since.get_or_insert(at);
        let stuck_for = at.duration_since(since);
        (stuck_for >= self.after).then(|| standing.words(bounds, next, now_ns, stuck_for))
    }
}

/// Whether bringing the stream as it stands to `bounds` would drop a write
/// past the lake's place `next`: an age bound its oldest write the lake has
/// not recorded is past. The words it is refused with, naming the stream;
/// none where the bounds drop none. A byte bound lower than it holds drops
/// none: the stream keeps every write and refuses new ones until it is
/// under it (`discard: new`; seen on a real broker, 160 writes kept), which
/// is why such a bound is not applied ([`bytes_kept`]).
pub fn bounds_drop(
    name: &str,
    standing: &Standing,
    next: u64,
    bounds: &Bounds,
    now_ns: i64,
) -> Option<String> {
    let past = standing.past(next);
    if past == 0 {
        return None;
    }
    let unrecorded_oldest = standing.first >= next;
    let age = standing.oldest_age(now_ns).unwrap_or_default();
    let why = if unrecorded_oldest && age > bounds.max_age {
        format!(
            "its oldest write, not yet recorded, is {}s old, past the {}s bound",
            age.as_secs(),
            bounds.max_age.as_secs()
        )
    } else {
        return None;
    };
    Some(format!(
        "the write-ahead stream {name} is kept as it is, not brought to this release's bounds: \
         {why}, and it holds {past} write(s) the lake has not recorded, which the bound would drop"
    ))
}

/// Whether the byte bound of `bounds` is not applied to the stream as it
/// stands, its own being `held_max_bytes`: a lower bound than its own (none
/// where that is not above 0) that it holds more than. Under it the stream
/// would refuse every write until the writes it holds aged out. The words
/// its member says, naming the stream; none where the bound is not lower,
/// or what the stream holds fits it. A higher bound is always applied.
pub fn bytes_kept(
    name: &str,
    standing: &Standing,
    held_max_bytes: i64,
    bounds: &Bounds,
) -> Option<String> {
    let new = u64::try_from(bounds.max_bytes)
        .ok()
        .filter(|new| *new > 0)?;
    let own = u64::try_from(held_max_bytes).ok().filter(|own| *own > 0);
    if own.is_some_and(|own| own <= new) || standing.bytes <= new {
        return None;
    }
    let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
    let own = own.map_or_else(
        || "no byte bound".to_string(),
        |own| format!("its {:.1} MiB bound", mib(own)),
    );
    Some(format!(
        "the write-ahead stream {name} keeps {own}, not brought to this release's {:.1} MiB: \
         it holds {:.1} MiB, and under the lower bound it would refuse every write until the \
         writes it holds aged out; the bound is applied at a writer's start once what the \
         stream holds fits it",
        mib(new),
        mib(standing.bytes)
    ))
}

/// What the writer's member says of its stream at a look: stuck full, in
/// its words; and where the stream was not brought to this release's
/// bounds, that refusal -- after them, or alone. None where neither.
pub fn said(stuck: Option<String>, kept: Option<&str>) -> Option<String> {
    match (stuck, kept) {
        (Some(stuck), Some(kept)) => Some(format!("{stuck}; {kept}")),
        (Some(stuck), None) => Some(stuck),
        (None, kept) => kept.map(str::to_string),
    }
}

#[cfg(test)]
mod tests;
