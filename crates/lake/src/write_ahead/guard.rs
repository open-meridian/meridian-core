//! The write-ahead stream's guard while the stream exists (v31a item 5,
//! 0-J; INTERIM(v32): decision 3 removes the stream).
//!
//! **Stuck full.** The stream refuses a new write once it holds its bound
//! (`discard: new`), and drops a write older than its age bound. Standing
//! at either bound for a while is no fault of a plugin's: the guard looks at
//! the stream every [`LOOK_EVERY`], and where every look for [`STUCK_AFTER`]
//! found it at a bound, it is *stuck full*. The writer's member says so in
//! its report, degraded, with the figures that tell its causes apart -- the
//! bytes it holds against its bound, the age of the oldest write the lake
//! has not recorded against its age bound, and how many writes it holds
//! past the lake's place -- until a
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

    /// Whether its oldest write is one the lake recorded while it holds
    /// writes the lake has not: then its oldest write the lake has not
    /// recorded is the one at the place `next`, whose time the stream's
    /// state does not say, and is read from the stream ([`Self::from_place`]).
    pub fn oldest_is_recorded(&self, next: u64) -> bool {
        self.messages > 0 && self.first < next && next <= self.last
    }

    /// It from the lake's place `next`, the write there made at `at_ns`:
    /// its first and oldest the write at the place, where its own oldest
    /// is one the lake recorded ([`Self::oldest_is_recorded`]); itself
    /// where not.
    #[must_use]
    pub fn from_place(mut self, next: u64, at_ns: i64) -> Self {
        if self.oldest_is_recorded(next) {
            self.first = next;
            self.oldest_ns = at_ns;
        }
        self
    }

    /// The age at `now_ns` of the oldest write the lake has not recorded:
    /// none where it holds none past the place `next`, nor where its
    /// oldest is one the lake recorded (read from the place first,
    /// [`Self::from_place`]).
    pub fn unrecorded_age(&self, next: u64, now_ns: i64) -> Option<Duration> {
        (self.first >= next && self.past(next) > 0)
            .then(|| self.oldest_age(now_ns))
            .flatten()
    }

    /// Its oldest write's age at `now_ns`; none where it holds none.
    pub fn oldest_age(&self, now_ns: i64) -> Option<Duration> {
        (self.messages > 0)
            .then(|| Duration::from_nanos(now_ns.saturating_sub(self.oldest_ns).max(0) as u64))
    }

    /// Whether it stands at its byte bound -- some write refused -- or the
    /// oldest write the lake has not recorded is about to be dropped at its
    /// age bound. Taken from the lake's place first ([`Self::from_place`]),
    /// so that write is its oldest once drops have started. `largest_write`
    /// is the room it must have for a write not to be refused: the broker's
    /// largest message as the broker says it (`broker.limits.maxPayload`),
    /// so a stream with less free is at its byte bound for some write.
    pub fn at_a_bound(&self, bounds: &Bounds, largest_write: u64, next: u64, now_ns: i64) -> bool {
        // A bound of none (the broker's -1 bytes, 0 s) is never stood at.
        let max_bytes = u64::try_from(bounds.max_bytes).unwrap_or(0);
        let by_bytes = max_bytes > 0 && self.bytes.saturating_add(largest_write) > max_bytes;
        let by_age = !bounds.max_age.is_zero()
            && self
                .unrecorded_age(next, now_ns)
                .is_some_and(|age| age + STUCK_AFTER >= bounds.max_age);
        by_bytes || by_age
    }

    /// The words it is said with while stuck full: [`STUCK_FULL`], for how
    /// long and since when -- `since_ns`, the wall clock at the first look
    /// that found it at a bound, which names the spell for the dashboard's
    /// judge -- then the figures that tell its causes apart.
    pub fn words(
        &self,
        bounds: &Bounds,
        next: u64,
        now_ns: i64,
        stuck_for: Duration,
        since_ns: i64,
    ) -> String {
        let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
        let age = match self.unrecorded_age(next, now_ns) {
            Some(age) => format!(
                "the oldest write the lake has not recorded {}s old of its {}s bound",
                age.as_secs(),
                bounds.max_age.as_secs()
            ),
            None => format!(
                "no write the lake has not recorded to age against its {}s bound",
                bounds.max_age.as_secs()
            ),
        };
        format!(
            "{}: holds {:.1} MiB of its {:.1} MiB bound, {age}, {} write(s) \
             past the lake's place (the lake at {}, the stream at {}); a write it has no room \
             for is refused for load, none it holds is dropped",
            meridian_bus::write_ahead::stuck_full_since(stuck_for.as_secs(), since_ns),
            mib(self.bytes),
            mib(u64::try_from(bounds.max_bytes).unwrap_or(0)),
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
    /// The broker's largest message, bytes.
    largest_write: u64,
    /// Since when every look found the stream at a bound, and the wall
    /// clock then.
    since: Option<(tokio::time::Instant, i64)>,
}

impl Full {
    pub fn new(after: Duration, largest_write: u64) -> Self {
        Self {
            after,
            largest_write,
            since: None,
        }
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
        if !standing.at_a_bound(bounds, self.largest_write, next, now_ns) {
            self.since = None;
            return None;
        }
        let (since, since_ns) = *self.since.get_or_insert((at, now_ns));
        let stuck_for = at.duration_since(since);
        (stuck_for >= self.after).then(|| standing.words(bounds, next, now_ns, stuck_for, since_ns))
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
