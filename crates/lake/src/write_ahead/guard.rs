//! The write-ahead stream's guard while the stream exists (v31a item 5,
//! 0-J; INTERIM(v32): decision 3 removes the stream).
//!
//! **At a bound.** The stream refuses a new write once it holds its byte
//! bound (`discard: new`), and drops a write older than its age bound. It
//! keeps a write the lake has recorded as long as one it has not (`Limits`
//! retention): only age frees its room, so its byte bound is a bound on
//! the rate it takes -- its byte bound every age bound -- whatever the
//! lake's speed. The guard looks at the stream every [`LOOK_EVERY`], and
//! where every look for [`STUCK_AFTER`] found it at a bound, the writer's
//! member says so in its report, degraded, as one of two states, each with
//! its own time since, until a look finds it below both bounds:
//!
//! - **Stuck full** ([`STUCK_FULL`]): the oldest write the lake has not
//!   recorded is about to be dropped at the age bound, or the stream is at
//!   its byte bound with the lake behind -- that write older than
//!   [`STUCK_AFTER`]. Said with the figures that tell its causes apart: the
//!   bytes it holds against its bound, that write's age against the age
//!   bound, and how many writes it holds past the lake's place.
//! - **At its rate limit** ([`RATE_LIMITED`]): at its byte bound with the
//!   lake keeping up. Writes arrive faster than its bounds hold; nothing
//!   is stuck, and nothing the lake does frees room. Said with the rate
//!   its own bounds give: `maxBytes / maxAge`, 447 KB/s at the chart's
//!   defaults.
//!   INTERIM(v32): this state, and the bound on the ha write rate, go when v32.2 takes the stream off the lake's write path.
//!
//! The dashboard files each as a standing notice and its clearing. The
//! bounds are the stream's own, as its leader says them at that look,
//! never the chart's: a stream kept as it was made is judged as it was
//! made.
//!
//! **Refused or not answered** (INTERIM(v32), the proposal of 2026-11-27,
//! A1: v32 takes the stream off the lake's write path and this guard with
//! it). A look the broker refuses, or does not answer, says nothing of the
//! stream's bounds; but where every look for [`REFUSED_AFTER`] was refused
//! or unanswered, the writer's member says so in its report, degraded, as
//! its own state ([`REFUSED`]), apart from stuck full: the broker refuses
//! the lake's write-ahead writes, or does not answer them, with its last
//! words. Never on one refusal: a look answered within the bound ends the
//! spell unsaid. Said, it stands until a write is accepted again -- a look
//! finds the stream's last write past where the last answered look found
//! it ([`Refused`]). Why the broker refuses is not inferred here.
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

/// How long the stream stands at a bound, at every look, before its member
/// says so; and how old the oldest write the lake has not recorded is
/// before the lake is behind ([`Standing::lake_behind`]). Twice the
/// writer's stall bound ([`super::STALL_AFTER`]), so a reader made again
/// from its place has had its chance to record what the stream holds past
/// it. Recording frees no room in the stream: only a write's age does.
pub const STUCK_AFTER: Duration = Duration::from_secs(30);

/// The words a stuck-full stream is said with, first in the writer
/// member's report (`ComponentReport.detail`): what the dashboard's judge
/// reads to file its standing notice, and `meridian status` shows.
pub use meridian_bus::write_ahead::STUCK_FULL;

/// The words a stream at its rate limit is said with, first in the writer
/// member's report: what the dashboard's judge files its standing notice
/// from.
pub use meridian_bus::write_ahead::RATE_LIMITED;

/// How long every look at the stream is refused or not answered before the
/// writer's member says so ([`REFUSED`]): the stuck-full bound's
/// ([`STUCK_AFTER`]). INTERIM(v32).
pub const REFUSED_AFTER: Duration = Duration::from_secs(30);

/// The words the broker's refusals are said with in the writer member's
/// report: what the dashboard's judge files its standing notice from.
/// INTERIM(v32).
pub use meridian_bus::write_ahead::REFUSED;

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
        self.at_byte_bound(bounds, largest_write) || self.at_age_bound(bounds, next, now_ns)
    }

    /// Whether it stands at its byte bound: less free than the broker's
    /// largest message, so some write is refused. A bound of none (the
    /// broker's -1 bytes) is never stood at.
    pub fn at_byte_bound(&self, bounds: &Bounds, largest_write: u64) -> bool {
        let max_bytes = u64::try_from(bounds.max_bytes).unwrap_or(0);
        max_bytes > 0 && self.bytes.saturating_add(largest_write) > max_bytes
    }

    /// Whether the oldest write the lake has not recorded is within
    /// [`STUCK_AFTER`] of being dropped at its age bound. A bound of none
    /// (the broker's 0 s) is never stood at.
    pub fn at_age_bound(&self, bounds: &Bounds, next: u64, now_ns: i64) -> bool {
        !bounds.max_age.is_zero()
            && self
                .unrecorded_age(next, now_ns)
                .is_some_and(|age| age + STUCK_AFTER >= bounds.max_age)
    }

    /// Whether the lake is behind the stream: the oldest write it has not
    /// recorded is [`STUCK_AFTER`] old or older. Never by how many writes
    /// are past its place -- under load a look finds a few in flight,
    /// milliseconds old -- but by the age of the oldest, which a stalled
    /// writer's reaches in that time. Taken from the lake's place first
    /// ([`Self::from_place`]).
    pub fn lake_behind(&self, next: u64, now_ns: i64) -> bool {
        self.unrecorded_age(next, now_ns)
            .is_some_and(|age| age >= STUCK_AFTER)
    }

    /// The words it is said with while stuck full: [`STUCK_FULL`], for how
    /// long and since when -- `since_ns`, the wall clock at the first look
    /// that found it stuck, which names the spell for the dashboard's
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

/// The bytes a second a stream of `bounds` takes at most, sustained: its
/// byte bound every age bound, since a write leaves it only at that age.
/// None where either bound is none.
pub fn rate(bounds: &Bounds) -> Option<f64> {
    let max_bytes = u64::try_from(bounds.max_bytes).ok().filter(|b| *b > 0)?;
    (!bounds.max_age.is_zero()).then(|| max_bytes as f64 / bounds.max_age.as_secs_f64())
}

/// A rate said in decimal units, as the chart's values are: 447 KB/s.
fn rate_words(bytes_a_second: f64) -> String {
    if bytes_a_second < 1_000_000.0 {
        format!("{:.0} KB/s", bytes_a_second / 1_000.0)
    } else {
        format!("{:.1} MB/s", bytes_a_second / 1_000_000.0)
    }
}

impl Standing {
    /// The words it is said with while at its rate limit: [`RATE_LIMITED`],
    /// for how long and since when -- `since_ns`, the wall clock at the
    /// first look that found it so, which names the spell for the
    /// dashboard's judge -- then its figures, the rate its own bounds give,
    /// and the two chart values that raise it.
    pub fn rate_limited_words(
        &self,
        bounds: &Bounds,
        next: u64,
        limited_for: Duration,
        since_ns: i64,
    ) -> String {
        let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
        let max_mib = mib(u64::try_from(bounds.max_bytes).unwrap_or(0));
        let max_age_s = bounds.max_age.as_secs();
        let rate = rate(bounds).map_or_else(String::new, |rate| format!(" ({})", rate_words(rate)));
        format!(
            "{}: holds {:.1} MiB of its {max_mib:.1} MiB bound, {} write(s) past the lake's \
             place (the lake at {}, the stream at {}); the lake is keeping up, and writes \
             arrive faster than the stream's bounds hold, {max_mib:.1} MiB every \
             {max_age_s}s{rate}; it takes a new write as the oldest it holds reach {max_age_s}s \
             old; a write it has no room for is refused for load, none it holds is dropped; a \
             higher lake.writeAhead.maxBytes, with broker.jetstream.size to hold it, raises \
             that rate",
            meridian_bus::write_ahead::rate_limited_for(limited_for.as_secs(), since_ns),
            mib(self.bytes),
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
    /// Since when every look found the stream at a bound.
    since: Option<tokio::time::Instant>,
    /// Since when every look found it stuck -- at its age bound, or at its
    /// byte bound with the lake behind -- and the wall clock then.
    stuck: Option<(tokio::time::Instant, i64)>,
    /// Since when every look found it at its byte bound with the lake
    /// keeping up, and the wall clock then: its own, never the stuck
    /// spell's, so each names its own event for the dashboard's judge.
    limited: Option<(tokio::time::Instant, i64)>,
    /// Whether the lake was behind at the last look of this stand at a
    /// bound that could tell; none where none could yet.
    behind: Option<bool>,
}

impl Full {
    pub fn new(after: Duration, largest_write: u64) -> Self {
        Self {
            after,
            largest_write,
            since: None,
            stuck: None,
            limited: None,
            behind: None,
        }
    }

    /// One look at `at`: the stream standing so, the lake's place `next`,
    /// the stream's own `bounds` at that look, the wall clock `now_ns`.
    /// The words it is said with once every look for the bound found it at
    /// a bound: the stuck-full words ([`Standing::words`]) where a write
    /// the lake has not recorded is about to be dropped, or it is at its
    /// byte bound with the lake behind; the rate-limit words
    /// ([`Standing::rate_limited_words`]) where it is at its byte bound
    /// with the lake keeping up. One or the other, each for how long and
    /// since when its own condition has held at every look. None while it
    /// is below both bounds.
    ///
    /// A stream with no age bound never frees room, so at its byte bound
    /// it is stuck full. Where its write at the lake's place was not read
    /// at this look, the lake is taken as the last look that could tell
    /// found it, and as behind where none could yet: the state that was
    /// said of every stream at its byte bound, and the one that warns of a
    /// drop.
    pub fn looked(
        &mut self,
        at: tokio::time::Instant,
        standing: &Standing,
        next: u64,
        bounds: &Bounds,
        now_ns: i64,
    ) -> Option<String> {
        let by_age = standing.at_age_bound(bounds, next, now_ns);
        if !by_age && !standing.at_byte_bound(bounds, self.largest_write) {
            *self = Self::new(self.after, self.largest_write);
            return None;
        }
        if !standing.oldest_is_recorded(next) {
            self.behind = Some(standing.lake_behind(next, now_ns));
        }
        let said = at.duration_since(*self.since.get_or_insert(at)) >= self.after;
        if by_age || self.behind.unwrap_or(true) || bounds.max_age.is_zero() {
            self.limited = None;
            let (since, since_ns) = *self.stuck.get_or_insert((at, now_ns));
            said.then(|| standing.words(bounds, next, now_ns, at.duration_since(since), since_ns))
        } else {
            self.stuck = None;
            let (since, since_ns) = *self.limited.get_or_insert((at, now_ns));
            said.then(|| {
                standing.rate_limited_words(bounds, next, at.duration_since(since), since_ns)
            })
        }
    }
}

/// The guard's memory of the broker's answers to its looks (INTERIM(v32),
/// the proposal of 2026-11-27, A1): since when every look was refused or
/// not answered, and where the last answered look found the stream's last
/// write -- what a write accepted since is told by.
#[derive(Debug)]
pub struct Refused {
    after: Duration,
    /// Since when every look was refused or not answered -- when the first
    /// of them was sent -- and the wall clock then, which names the spell
    /// for the dashboard's judge.
    since: Option<(tokio::time::Instant, i64)>,
    /// What was said of the last look refused, or not answered.
    why: String,
    /// Whether the spell is said: from the bound reached until a write is
    /// accepted again.
    said: bool,
    /// The stream's last write as the last answered look found it.
    last: Option<u64>,
}

impl Refused {
    pub fn new(after: Duration) -> Self {
        Self {
            after,
            since: None,
            why: String::new(),
            said: false,
            last: None,
        }
    }

    /// A look the broker refused at once, at `at`, the wall clock `now_ns`:
    /// one sent and returned then ([`Self::refused_look`]).
    pub fn refused(&mut self, at: tokio::time::Instant, now_ns: i64, why: &str) -> Option<String> {
        self.refused_look(at, now_ns, at, why)
    }

    /// A look sent at `sent`, the wall clock `sent_ns` then, that the broker
    /// had refused, or not answered, by `returned`; `why` what was said of
    /// it. A spell is dated from when its first such look was sent, not
    /// from when it returned (the consensus of 2026-11-28, part 2 (B)): the
    /// broker had stopped answering by then, and an unanswered look returns
    /// only at its request's timeout. The words it is said with once every
    /// look from then for the bound was, as this one returns; none before.
    pub fn refused_look(
        &mut self,
        sent: tokio::time::Instant,
        sent_ns: i64,
        returned: tokio::time::Instant,
        why: &str,
    ) -> Option<String> {
        let (since, _) = *self.since.get_or_insert((sent, sent_ns));
        self.why = why.to_string();
        self.said |= returned.duration_since(since) >= self.after;
        self.said.then(|| self.words(returned, false))
    }

    /// A look answered at `at`, the stream's last write `last`. Before the
    /// bound it ends the spell unsaid. Said, the spell stands until a write
    /// is accepted: `last` past where the last answered look found it (a
    /// stream made again under its name, its last lower, counted from its
    /// own). The words it is still said with; none once it is not.
    pub fn answered(&mut self, at: tokio::time::Instant, last: u64) -> Option<String> {
        let accepted = self.last.is_some_and(|before| last > before);
        self.last = Some(last);
        if !self.said || accepted {
            self.since = None;
            self.said = false;
            return None;
        }
        Some(self.words(at, true))
    }

    /// Its words at `at`: [`REFUSED`], for how long and since when, then
    /// what the broker last said; `answering` where it answers again with
    /// no write accepted since.
    fn words(&self, at: tokio::time::Instant, answering: bool) -> String {
        let (since, since_ns) = self.since.unwrap_or((at, 0));
        let tail = if answering {
            "; the broker answers again, no write accepted since"
        } else {
            ""
        };
        format!(
            "{}: every look the lake's guard took at the stream was refused or not answered; \
             the last: {}{tail}",
            meridian_bus::write_ahead::refused_for(at.duration_since(since).as_secs(), since_ns),
            self.why
        )
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
/// where that is not above 0) that what it holds, and the broker's largest
/// write (`largest_write`) with it, do not fit. Under it the stream would
/// stand at its bound ([`Standing::at_a_bound`]) and refuse a write until
/// the writes it holds aged out. The words its member says, naming the
/// stream; none where the bound is not lower, or the stream has room for
/// a write under it. A higher bound is always applied.
pub fn bytes_kept(
    name: &str,
    standing: &Standing,
    held_max_bytes: i64,
    bounds: &Bounds,
    largest_write: u64,
) -> Option<String> {
    let new = u64::try_from(bounds.max_bytes)
        .ok()
        .filter(|new| *new > 0)?;
    let own = u64::try_from(held_max_bytes).ok().filter(|own| *own > 0);
    if own.is_some_and(|own| own <= new) || standing.bytes.saturating_add(largest_write) <= new {
        return None;
    }
    let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
    let own = own.map_or_else(
        || "no byte bound".to_string(),
        |own| format!("its {:.1} MiB bound", mib(own)),
    );
    Some(format!(
        "the write-ahead stream {name} keeps {own}, not brought to this release's {:.1} MiB: \
         it holds {:.1} MiB, and under the lower bound it would have no room for the broker's \
         largest write ({:.1} MiB) and refuse writes until the writes it holds aged out; the \
         bound is applied at a writer's start once the stream has that room under it",
        mib(new),
        mib(standing.bytes),
        mib(largest_write)
    ))
}

/// What the writer's member says of its stream at a look: stuck full, or
/// at its rate limit, in its words; and where the stream was not brought to this release's
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
