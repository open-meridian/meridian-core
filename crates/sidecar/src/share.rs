//! A plugin's own share at its sidecar (stage M; the resilience spec's
//! section 10a): bulkheads, so one plugin flooding core fills only its own
//! compartment.
//!
//! Every plugin reaches core only through its sidecar, which holds its
//! share: at most `sidecar.limits.inFlight` requests at once,
//! `sidecar.limits.requestsPerSecond` with a burst of a second's worth, and
//! `sidecar.limits.answerMiB` of answers held or paged in flight for it at
//! once. Past any of them only that plugin is refused, `RESOURCE_EXHAUSTED`
//! with `REFUSAL_REASON_RATE_LIMITED` and a retry-after (contract v30), and
//! nothing is sent to core. A bulk act, a parent with its children
//! (decisions/024), is one call and counts as one request. Registering,
//! the heartbeat and leaving are the plugin's lifeline and never counted.
//!
//! The chart sets each (`sidecar.limits.*`) and a plugin's launch may
//! override them; the binary's own defaults stand where neither says.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use meridian_pb::v1::RefusalReason;
use tonic::body::Body;
use tonic::codegen::{http, BoxFuture, Service};
use tonic::server::NamedService;

/// `sidecar.limits.inFlight`, as the chart hands it to the sidecar.
pub const IN_FLIGHT_ENV: &str = "MERIDIAN_SIDECAR_IN_FLIGHT";
/// `sidecar.limits.requestsPerSecond`.
pub const REQUESTS_PER_SECOND_ENV: &str = "MERIDIAN_SIDECAR_REQUESTS_PER_SECOND";
/// `sidecar.limits.answerMiB`.
pub const ANSWER_MIB_ENV: &str = "MERIDIAN_SIDECAR_ANSWER_MIB";

/// `sidecar.limits.writeMiBPerSecond`: a data plugin's lake write share
/// (step 3d; the resilience spec's section 6.9).
pub const WRITE_MIB_PER_SECOND_ENV: &str = "MERIDIAN_SIDECAR_WRITE_MIB_PER_SECOND";

/// How long a write with no deadline of its caller's may be held: the bus's
/// own bound on an ask.
pub const WRITE_HELD_MOST: Duration = Duration::from_secs(5);

tokio::task_local! {
    /// When the call being answered stops being waited for, as its
    /// `grpc-timeout` said; none where it said none. Set by [`Shared`]
    /// around each call, read where a write is held to its share.
    pub static CALL_DEADLINE: Option<Instant>;
}

/// How long a write may be held now: to its caller's deadline, or
/// [`WRITE_HELD_MOST`] where the call named none or this is no call.
pub fn call_within(now: Instant) -> Duration {
    CALL_DEADLINE
        .try_with(|deadline| *deadline)
        .ok()
        .flatten()
        .map_or(WRITE_HELD_MOST, |deadline| {
            deadline.saturating_duration_since(now)
        })
}

/// gRPC's `grpc-timeout`: at most eight digits and a unit.
fn grpc_timeout(said: &str) -> Option<Duration> {
    let (digits, unit) = said.split_at(said.len().checked_sub(1)?);
    if digits.is_empty() || digits.len() > 8 {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n * 3600),
        "M" => Duration::from_secs(n * 60),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

/// The calls never counted: the plugin's lifeline.
const LIFELINE: [&str; 3] = [
    "/meridian.v1.SidecarService/Register",
    "/meridian.v1.SidecarService/Heartbeat",
    "/meridian.v1.SidecarService/Leave",
];

/// A plugin's share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub in_flight: usize,
    pub requests_per_second: u32,
    pub answer_mib: u64,
}

impl Limits {
    /// Where neither the chart nor the launch says: the chart's own,
    /// set from kernel/throughput-at-wealth-platform-scale's bench (ruled
    /// 2026-11-11). The book takes the most proposals with about 8 in
    /// flight and more than 200 a second across many accounts; a page is
    /// at most 1 MiB.
    pub const DEFAULT: Limits = Limits {
        in_flight: 16,
        requests_per_second: 400,
        answer_mib: 32,
    };

    /// The limits as given, each refused naming its chart value where it is
    /// not a positive whole number; the default where unsaid.
    pub fn read(
        in_flight: Option<&str>,
        per_second: Option<&str>,
        answer_mib: Option<&str>,
    ) -> Result<Self, String> {
        fn said<T: std::str::FromStr + PartialOrd + Default>(
            value: Option<&str>,
            name: &str,
            default: T,
        ) -> Result<T, String> {
            match value.map(str::trim).filter(|v| !v.is_empty()) {
                None => Ok(default),
                Some(text) => text
                    .parse::<T>()
                    .ok()
                    .filter(|n| *n > T::default())
                    .ok_or_else(|| format!("{name} is {text:?}; a whole number above 0")),
            }
        }
        Ok(Self {
            in_flight: said(
                in_flight,
                "sidecar.limits.inFlight",
                Self::DEFAULT.in_flight,
            )?,
            requests_per_second: said(
                per_second,
                "sidecar.limits.requestsPerSecond",
                Self::DEFAULT.requests_per_second,
            )?,
            answer_mib: said(
                answer_mib,
                "sidecar.limits.answerMiB",
                Self::DEFAULT.answer_mib,
            )?,
        })
    }

    /// What the sidecar holds apart from its own budget (the resilience
    /// spec's section 10a; stage B's ruling 5): the plugin's answers in
    /// flight, `answerMiB`, and its `read_after` session's
    /// [`crate::read_after::MOST_BYTES`]. What is left of its working budget
    /// is the sidecar's own work's -- its registrations, heartbeats,
    /// reports and deliveries -- which a plugin flooding it cannot take.
    pub fn held_apart(&self) -> u64 {
        self.answer_mib * 1024 * 1024 + crate::read_after::MOST_BYTES as u64
    }

    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok();
        Self::read(
            var(IN_FLIGHT_ENV).as_deref(),
            var(REQUESTS_PER_SECOND_ENV).as_deref(),
            var(ANSWER_MIB_ENV).as_deref(),
        )
    }

    /// One shard's part of the instance's share (step 3d; the resilience
    /// spec's section 7): each limit divided by the shard count, rounded
    /// down, so the shards together never hold more than the instance's
    /// share; and none is left with nothing, so a limit below the shard
    /// count is one each, the only case the shards together pass it.
    pub fn per_shard(self, shards: u32) -> Self {
        let shards = shards.max(1);
        Self {
            in_flight: (self.in_flight / shards as usize).max(1),
            requests_per_second: (self.requests_per_second / shards).max(1),
            answer_mib: (self.answer_mib / u64::from(shards)).max(1),
        }
    }
}

/// A data plugin's lake write share (step 3d; the resilience spec's section
/// 6.9): `sidecar.limits.writeMiBPerSecond`, in bytes a second with a burst
/// of a second's worth. Past it the sidecar holds the write, and so its
/// acknowledgement, for as long as the share takes to cover it, as Kafka's
/// quotas delay a response: then it is sent, in its order. A write whose
/// hold would outlast its caller's deadline is not sent, and is refused
/// `RATE_LIMITED` with a retry-after. A write's bytes are taken when it is
/// asked and never given back, so one whose caller stopped waiting while it
/// was held was not sent and still counts: what reaches the lake's stream
/// stays within the share whatever deadline a caller names or abandons.
/// Other plugins' writes are other sidecars', and untouched.
pub struct WriteShare {
    bytes_per_second: f64,
    /// What is left of the share, below 0 while writes sent are still being
    /// covered; and when it was last filled.
    left: Mutex<(f64, Instant)>,
    refused: AtomicU64,
}

impl WriteShare {
    /// `mib_per_second` MiB a second, divided among `shards`.
    pub fn new(mib_per_second: u32, shards: u32) -> Arc<Self> {
        let bytes_per_second =
            f64::from(mib_per_second.max(1)) * 1024.0 * 1024.0 / f64::from(shards.max(1));
        Arc::new(Self {
            bytes_per_second,
            left: Mutex::new((bytes_per_second, Instant::now())),
            refused: AtomicU64::new(0),
        })
    }

    /// The share as given: none where unsaid, refused naming its chart value
    /// where it is not a positive whole number.
    pub fn read(said: Option<&str>, shards: u32) -> Result<Option<Arc<Self>>, String> {
        match said.map(str::trim).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(text) => text
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .map(|n| Some(Self::new(n, shards)))
                .ok_or_else(|| {
                    format!("sidecar.limits.writeMiBPerSecond is {text:?}; a whole number above 0")
                }),
        }
    }

    pub fn from_env(shards: u32) -> Result<Option<Arc<Self>>, String> {
        Self::read(
            std::env::var(WRITE_MIB_PER_SECOND_ENV).ok().as_deref(),
            shards,
        )
    }

    pub fn bytes_per_second(&self) -> u64 {
        self.bytes_per_second as u64
    }

    /// Writes refused since the sidecar started.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// A write of `bytes` taken from the share at `now`: how long it is
    /// held before it is sent, nothing within the share; or, where that
    /// is longer than `within`, the write refused and nothing taken.
    pub fn take(&self, bytes: usize, now: Instant, within: Duration) -> Result<Duration, Refused> {
        let rate = self.bytes_per_second;
        let mut left = self
            .left
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let filled =
            (left.0 + now.saturating_duration_since(left.1).as_secs_f64() * rate).min(rate);
        let after = filled - bytes as f64;
        let held = if after >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-after / rate)
        };
        if held > within {
            *left = (filled, now);
            drop(left);
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Err(Refused {
                retry_after_seconds: ((held - within).as_secs_f64().ceil() as u32).max(1),
                words: format!(
                    "rate limited: this plugin is past its lake write share, \
                     sidecar.limits.writeMiBPerSecond ({:.2} MiB a second here)",
                    rate / 1024.0 / 1024.0
                ),
            });
        }
        *left = (after, now);
        Ok(held)
    }
}

/// The share's accounting, one per sidecar, beside every service it fronts.
pub struct Share {
    limits: Limits,
    in_flight: AtomicUsize,
    answers: AtomicU64,
    bucket: Mutex<Bucket>,
    refused: AtomicU64,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

/// Why a call was refused, and when to ask again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub retry_after_seconds: u32,
    pub words: String,
}

impl Share {
    pub fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            in_flight: AtomicUsize::new(0),
            answers: AtomicU64::new(0),
            bucket: Mutex::new(Bucket {
                tokens: f64::from(limits.requests_per_second),
                at: Instant::now(),
            }),
            refused: AtomicU64::new(0),
        })
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Calls refused since the sidecar started.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// A call's place in the share, or why not: within its answers' bytes,
    /// its rate and its requests at once.
    pub fn admit(self: &Arc<Self>) -> Result<InFlight, Refused> {
        let answer_bytes = self.limits.answer_mib * 1024 * 1024;
        let held = self.answers.load(Ordering::Relaxed);
        if held >= answer_bytes {
            return Err(self.refuse(
                1,
                format!(
                    "rate limited: {} MiB of answers are in flight for this plugin, its sidecar.limits.answerMiB of {}",
                    held.div_ceil(1024 * 1024),
                    self.limits.answer_mib
                ),
            ));
        }
        {
            let mut bucket = self
                .bucket
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let rate = f64::from(self.limits.requests_per_second);
            let now = Instant::now();
            bucket.tokens =
                (bucket.tokens + now.duration_since(bucket.at).as_secs_f64() * rate).min(rate);
            bucket.at = now;
            if bucket.tokens < 1.0 {
                let seconds = ((1.0 - bucket.tokens) / rate).ceil() as u32;
                return Err(self.refuse(
                    seconds,
                    format!(
                        "rate limited: this plugin is past its sidecar.limits.requestsPerSecond of {}",
                        self.limits.requests_per_second
                    ),
                ));
            }
            bucket.tokens -= 1.0;
        }
        let before = self.in_flight.fetch_add(1, Ordering::AcqRel);
        if before >= self.limits.in_flight {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(self.refuse(
                1,
                format!(
                    "rate limited: this plugin has {before} requests in flight, its sidecar.limits.inFlight"
                ),
            ));
        }
        Ok(InFlight {
            share: Arc::clone(self),
        })
    }

    fn refuse(&self, seconds: u32, words: String) -> Refused {
        self.refused.fetch_add(1, Ordering::Relaxed);
        Refused {
            retry_after_seconds: seconds.max(1),
            words,
        }
    }
}

/// One call's place in its plugin's requests at once, given back when its
/// answer is made.
pub struct InFlight {
    share: Arc<Share>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.share.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A service fronted by its plugin's share.
#[derive(Clone)]
pub struct Shared<S> {
    inner: S,
    share: Arc<Share>,
}

impl<S> Shared<S> {
    pub fn new(inner: S, share: Arc<Share>) -> Self {
        Self { inner, share }
    }
}

impl<S: NamedService> NamedService for Shared<S> {
    const NAME: &'static str = S::NAME;
}

impl<S> Service<http::Request<Body>> for Shared<S>
where
    S: Service<http::Request<Body>, Response = http::Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        if LIFELINE.contains(&request.uri().path()) {
            return Box::pin(self.inner.call(request));
        }
        let path = request.uri().path().to_string();
        let place = match self.share.admit() {
            Ok(place) => place,
            Err(refused) => {
                tracing::info!(
                    call = path,
                    retry_after_seconds = refused.retry_after_seconds,
                    "{}",
                    refused.words
                );
                let status = crate::typed::refused_for_load(
                    RefusalReason::RateLimited,
                    refused.retry_after_seconds,
                    refused.words,
                );
                return Box::pin(async move { Ok(status.into_http()) });
            }
        };
        // The one polled ready is the one called; a clone takes its place.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let share = Arc::clone(&self.share);
        // When its caller stops waiting, for a write held to its share.
        let deadline = request
            .headers()
            .get("grpc-timeout")
            .and_then(|said| said.to_str().ok())
            .and_then(grpc_timeout)
            .map(|within| Instant::now() + within);
        Box::pin(async move {
            let answered = CALL_DEADLINE.scope(deadline, inner.call(request)).await?;
            // Its place is given back once its answer is made; the answer's
            // bytes count against answerMiB while they are in flight.
            drop(place);
            let (parts, body) = answered.into_parts();
            Ok(http::Response::from_parts(
                parts,
                Body::new(Counted {
                    inner: body,
                    share,
                    held: 0,
                }),
            ))
        })
    }
}

/// An answer's body, its bytes counted against its plugin's answerMiB from
/// the frame's making until the next is asked for or the answer ends.
struct Counted {
    inner: Body,
    share: Arc<Share>,
    held: u64,
}

impl Counted {
    fn give_back(&mut self) {
        if self.held > 0 {
            self.share.answers.fetch_sub(self.held, Ordering::Relaxed);
            self.held = 0;
        }
    }
}

impl http_body::Body for Counted {
    type Data = tonic::codegen::Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.give_back();
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled {
            if let Some(data) = frame.data_ref() {
                let bytes = data.len() as u64;
                self.share.answers.fetch_add(bytes, Ordering::Relaxed);
                self.held = bytes;
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.give_back();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_plugins_answers_and_its_session_are_held_apart_and_a_share_leaving_nothing_is_refused() {
        let mib = 1024 * 1024;
        assert_eq!(
            Limits::DEFAULT.held_apart(),
            32 * mib + crate::read_after::MOST_BYTES as u64
        );
        // The chart's own: 256 MiB at 80 %, less the default share, leaves
        // the sidecar its own work's 170 MiB or so.
        let settings = |limit_mib: u64| meridian_budget::Settings {
            limit_bytes: Some(limit_mib * mib),
            ..meridian_budget::Settings::unbounded()
        };
        let process = meridian_budget::Process::new(
            "sidecar",
            settings(256),
            Limits::DEFAULT.held_apart(),
            4,
        )
        .expect("the default share leaves the sidecar its own budget");
        let own = process.budget.working().unwrap();
        assert!(own > 160 * mib && own < 173 * mib, "{own}");
        // A share as large as the sidecar's whole budget leaves it nothing.
        let greedy = Limits {
            answer_mib: 205,
            ..Limits::DEFAULT
        };
        assert!(
            meridian_budget::Process::new("sidecar", settings(256), greedy.held_apart(), 4)
                .is_err()
        );
    }

    #[test]
    fn the_limits_default_where_unsaid_and_a_value_not_a_positive_number_is_refused_naming_it() {
        assert_eq!(Limits::read(None, None, None).unwrap(), Limits::DEFAULT);
        assert_eq!(
            Limits::read(Some("4"), Some("10"), Some("8")).unwrap(),
            Limits {
                in_flight: 4,
                requests_per_second: 10,
                answer_mib: 8
            }
        );
        for (a, b, c, name) in [
            (Some("0"), None, None, "sidecar.limits.inFlight"),
            (None, Some("-1"), None, "sidecar.limits.requestsPerSecond"),
            (None, None, Some("lots"), "sidecar.limits.answerMiB"),
        ] {
            let said = Limits::read(a, b, c).unwrap_err();
            assert!(said.contains(name), "{said}");
        }
    }

    #[test]
    fn past_its_requests_at_once_a_plugin_is_refused_and_a_place_given_back_is_taken_again() {
        let share = Share::new(Limits {
            in_flight: 2,
            requests_per_second: 1000,
            answer_mib: 64,
        });
        let first = share.admit().unwrap();
        let _second = share.admit().unwrap();
        let refused = share.admit().err().unwrap();
        assert!(
            refused.words.contains("inFlight") && refused.retry_after_seconds >= 1,
            "{refused:?}"
        );
        drop(first);
        assert!(share.admit().is_ok());
        assert_eq!(share.refused(), 1);
    }

    #[test]
    fn past_its_rate_a_plugin_is_refused_with_how_long_until_a_request_is_let_through() {
        let share = Share::new(Limits {
            in_flight: 100,
            requests_per_second: 5,
            answer_mib: 64,
        });
        for _ in 0..5 {
            drop(share.admit().unwrap());
        }
        let refused = share.admit().err().unwrap();
        assert!(refused.words.contains("requestsPerSecond"), "{refused:?}");
        assert_eq!(refused.retry_after_seconds, 1);
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            share.admit().is_ok(),
            "a token back after a fifth of a second"
        );
    }

    #[test]
    fn one_shards_part_is_the_instances_share_divided_and_never_nothing() {
        let whole = Limits {
            in_flight: 16,
            requests_per_second: 400,
            answer_mib: 32,
        };
        assert_eq!(whole.per_shard(1), whole);
        assert_eq!(whole.per_shard(0), whole);
        // Rounded down: three shards hold 15, 399 and 30 between them,
        // never more than the instance's share (review finding 11).
        assert_eq!(
            whole.per_shard(3),
            Limits {
                in_flight: 5,
                requests_per_second: 133,
                answer_mib: 10
            }
        );
        for shards in 1..=16u32 {
            let part = whole.per_shard(shards);
            assert!(
                part.in_flight * shards as usize <= whole.in_flight,
                "{shards}"
            );
            assert!(part.requests_per_second * shards <= whole.requests_per_second);
            assert!(part.answer_mib * u64::from(shards) <= whole.answer_mib);
        }
        let small = Limits {
            in_flight: 1,
            requests_per_second: 1,
            answer_mib: 1,
        };
        assert_eq!(small.per_shard(64), small);
    }

    #[test]
    fn a_write_within_its_share_is_not_held_and_one_past_it_is_held_until_covered() {
        let share = WriteShare::new(1, 1);
        let mib = 1024 * 1024;
        let start = Instant::now();
        let within = Duration::from_secs(5);
        // A second's worth at once: its burst.
        assert_eq!(share.take(mib, start, within), Ok(Duration::ZERO));
        // Half a second's worth more is held half a second, then sent.
        let held = share.take(mib / 2, start, within).unwrap();
        assert!(
            (held.as_secs_f64() - 0.5).abs() < 0.01,
            "held {held:?} for half a second's bytes"
        );
        // What was taken is still being covered: the next waits behind it.
        let next = share.take(mib / 2, start, within).unwrap();
        assert!((next.as_secs_f64() - 1.0).abs() < 0.01, "{next:?}");
        // Time covers it: two seconds on, the share is whole again.
        assert_eq!(
            share.take(mib, start + Duration::from_secs(2), within),
            Ok(Duration::ZERO)
        );
        assert_eq!(share.refused(), 0);
    }

    #[test]
    fn a_write_held_past_its_callers_deadline_is_refused_and_takes_nothing() {
        let share = WriteShare::new(1, 1);
        let mib = 1024 * 1024;
        let start = Instant::now();
        assert_eq!(
            share.take(mib, start, Duration::from_secs(1)),
            Ok(Duration::ZERO)
        );
        // Three seconds' bytes, a caller waiting one: refused, with how long
        // until it would be held no longer than that.
        let refused = share
            .take(3 * mib, start, Duration::from_secs(1))
            .unwrap_err();
        assert!(refused.words.contains("writeMiBPerSecond"), "{refused:?}");
        assert_eq!(refused.retry_after_seconds, 2);
        assert_eq!(share.refused(), 1);
        // Nothing was taken by the refused write: a small one is held only
        // for itself.
        let held = share.take(mib / 4, start, Duration::from_secs(1)).unwrap();
        assert!((held.as_secs_f64() - 0.25).abs() < 0.01, "{held:?}");
    }

    /// Review finding 5: a caller naming an hour's deadline is refused
    /// nothing, and sends no faster for it: each write is sent when the
    /// share has covered it, so by any moment no more than the share's
    /// bytes for the time gone, and its burst, have been.
    #[test]
    fn writes_with_a_long_deadline_are_each_sent_only_when_the_share_has_covered_them() {
        let share = WriteShare::new(1, 1);
        let mib = 1024 * 1024;
        let start = Instant::now();
        let hour = Duration::from_secs(3600);
        for sent in 1..=10u32 {
            let held = share.take(mib, start, hour).unwrap();
            // The burst and a MiB a second: the n-th MiB at n - 1 seconds.
            assert!(
                (held.as_secs_f64() - f64::from(sent - 1)).abs() < 0.01,
                "write {sent} held {held:?}"
            );
        }
        assert_eq!(share.refused(), 0);
    }

    #[test]
    fn the_write_share_is_divided_among_shards_unset_is_none_and_a_bad_value_is_named() {
        assert!(WriteShare::read(None, 1).unwrap().is_none());
        assert!(WriteShare::read(Some(" "), 2).unwrap().is_none());
        let whole = WriteShare::read(Some("8"), 1).unwrap().unwrap();
        let half = WriteShare::read(Some("8"), 2).unwrap().unwrap();
        assert_eq!(whole.bytes_per_second(), 8 * 1024 * 1024);
        assert_eq!(half.bytes_per_second(), 4 * 1024 * 1024);
        for bad in ["0", "-1", "lots"] {
            let said = WriteShare::read(Some(bad), 1).err().unwrap();
            assert!(said.contains("sidecar.limits.writeMiBPerSecond"), "{said}");
        }
    }

    #[test]
    fn a_calls_deadline_is_read_as_grpc_says_it_and_none_holds_a_write_five_seconds_at_most() {
        assert_eq!(grpc_timeout("3S"), Some(Duration::from_secs(3)));
        assert_eq!(grpc_timeout("250m"), Some(Duration::from_millis(250)));
        assert_eq!(grpc_timeout("1M"), Some(Duration::from_secs(60)));
        assert_eq!(grpc_timeout("S"), None);
        assert_eq!(grpc_timeout("123456789S"), None);
        assert_eq!(grpc_timeout("5x"), None);
        assert_eq!(call_within(Instant::now()), WRITE_HELD_MOST);
    }

    #[test]
    fn past_its_answers_in_flight_a_plugin_is_refused_until_they_are_sent() {
        let share = Share::new(Limits {
            in_flight: 100,
            requests_per_second: 1000,
            answer_mib: 1,
        });
        share.answers.fetch_add(1024 * 1024, Ordering::Relaxed);
        let refused = share.admit().err().unwrap();
        assert!(refused.words.contains("answerMiB"), "{refused:?}");
        share.answers.fetch_sub(1024 * 1024, Ordering::Relaxed);
        assert!(share.admit().is_ok());
    }
}
