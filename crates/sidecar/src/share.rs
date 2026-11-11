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
use std::time::Instant;

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
    /// Where neither the chart nor the launch says: above what a data
    /// plugin's relaunch asks (71 reads in two minutes on dev, 2026-11-09)
    /// and what the order driver sends, until
    /// kernel/throughput-at-wealth-platform-scale's targets set the floor.
    pub const DEFAULT: Limits = Limits {
        in_flight: 32,
        requests_per_second: 200,
        answer_mib: 64,
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
        Box::pin(async move {
            let answered = inner.call(request).await?;
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
            64 * mib + crate::read_after::MOST_BYTES as u64
        );
        // The chart's own: 256 MiB at 80 %, less the default share, leaves
        // the sidecar its own work's 140 MiB or so.
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
        assert!(own > 128 * mib && own < 141 * mib, "{own}");
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
