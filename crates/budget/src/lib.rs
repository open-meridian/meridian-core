//! A component's working memory: refused, never crashed (stage M).
//!
//! spec/a-deployment-scales-out-and-fails-over-by-configuration, section 10a
//! (the product owner's rule of 2026-11-09: memory exhaustion is handled
//! gracefully, a crash is never acceptable). The convention of databases
//! that serve many callers: a query's memory is reserved before it runs and
//! the one query is refused, not the server; and each caller gets its own
//! compartment, so a flood fills only the flooder's.
//!
//! - [`Settings`]: the container's limit, `memory.budgetPercent` (80) and
//!   `memory.shedPercent` (90), as the chart hands them to every component.
//! - [`Budget`]: the working budget -- the limit times the budget percent,
//!   less the component's hot layer's and held answers' bounds -- against
//!   which every request reserves its working memory before it starts
//!   ([`Reservation`]), growing it from its measured rows as it reads
//!   ([`grow`]), and freeing it when done.
//! - [`Gate`]: fair shares. Work is queued per calling instance or person
//!   and the queues are served in turn, within decisions/024's traffic
//!   classes: order-path commands and venue facts first ([`Class::First`]).
//! - [`Overloaded`]: a request that waited its while and found no room,
//!   refused `RESOURCE_EXHAUSTED` with `REFUSAL_REASON_OVERLOADED` and a
//!   retry-after (the bus words it).
//! - [`sweep`]: the last line. Above the shed line the largest request in
//!   flight is cancelled with the same refusal, so the kernel's OOM killer
//!   never fires; and the memory freed is handed back to the system with
//!   glibc's `malloc_trim`.
//!
//! One [`Process`] per component, [`install`]ed first thing by its binary;
//! the bus admits every request it answers through it.

mod budget;
mod gate;
mod settings;
pub mod sweep;

use std::sync::OnceLock;
use std::time::Duration;

pub use budget::{check, grow, shrink, within, Budget, BudgetCounts, Handle, Reservation, Shed};
pub use gate::{Class, Gate, Turn};
pub use settings::{
    Settings, BUDGET_PERCENT, BUDGET_PERCENT_ENV, LIMIT_ENV, SHED_PERCENT, SHED_PERCENT_ENV,
};

/// A request refused for load: it waited its while for a turn or for room
/// in the budget and found none, or it was the largest in flight above the
/// shed line. Nothing it would have done is done; the same request sent
/// again after `retry_after_seconds` is answered as it would have been.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overloaded {
    /// At least 1: when the refusing component expects room again.
    pub retry_after_seconds: u32,
    /// For a person reading a log: what was full, and for whom.
    pub words: String,
}

impl Overloaded {
    pub(crate) fn new(retry_after_seconds: u32, words: impl Into<String>) -> Self {
        Self {
            retry_after_seconds: retry_after_seconds.clamp(1, MOST_RETRY_AFTER_SECONDS),
            words: words.into(),
        }
    }
}

impl std::fmt::Display for Overloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; ask again in {} s",
            self.words, self.retry_after_seconds
        )
    }
}

/// The longest retry-after said, in seconds: the dictionary's bound on
/// `Refusal.retry_after_seconds` is 300, and a component that expects to be
/// full longer than this says so again when asked again.
pub const MOST_RETRY_AFTER_SECONDS: u32 = 30;

/// How long a request waits for its turn and its room before it is refused
/// ("a request waits briefly", section 10a): shorter than the bus's own
/// default bound of five seconds, so its caller hears the refusal and its
/// retry-after rather than a timeout.
pub const WAIT: Duration = Duration::from_millis(1500);

/// What every request reserves before it starts, beyond its own bytes: the
/// most its answer carries on the broker (`broker.limits.maxPayload`, 1 MiB),
/// since every answer a component sends is held whole before it is sent. A
/// read that holds more grows its reservation from its measured rows.
pub const FLOOR_BYTES: u64 = 1024 * 1024;

/// A component's budget and its gate: what the bus admits each request it
/// answers through.
pub struct Process {
    pub component: String,
    pub settings: Settings,
    pub budget: Budget,
    pub gate: Gate,
    /// The most resident memory its sweep has read, in bytes.
    peak_resident: std::sync::atomic::AtomicU64,
    /// Requests refused for load outside its gate and budget ([`count_refusal`]).
    refused_elsewhere: std::sync::atomic::AtomicU64,
}

/// A component's memory and what its budget did, all in one place, for its
/// report: read by [`Process::accounting`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Accounting {
    /// The container's limit, where one is known.
    pub limit: Option<u64>,
    /// Its working budget, where a limit is known.
    pub working: Option<u64>,
    /// Reserved now, and the most at once.
    pub reserved: u64,
    pub peak_reserved: u64,
    /// Resident now, and the most its sweep has read.
    pub resident: Option<u64>,
    pub peak_resident: u64,
    /// Reservations granted; requests refused for load, at its gate or its
    /// budget; requests cancelled above the shed line.
    pub granted: u64,
    pub refused: u64,
    pub shed: u64,
}

/// What [`Process::admit`] hands a request it admitted: its turn and its
/// reservation, both given back when the request is answered.
pub struct Admitted {
    pub turn: Turn,
    pub reservation: Reservation,
}

impl Admitted {
    /// Run `work` with this request's reservation as the thread's own, so
    /// a store's [`grow`] and [`check`] reach it.
    pub fn within<R>(&self, work: impl FnOnce() -> R) -> R {
        within(&self.reservation, work)
    }
}

impl Process {
    /// A process whose budget is `settings`' working budget less `held`.
    pub fn new(
        component: &str,
        settings: Settings,
        held: u64,
        permits: usize,
    ) -> Result<Self, String> {
        let working = settings
            .working_bytes(held)
            .map_err(|why| format!("{component}: {why}"))?;
        Ok(Self {
            component: component.to_string(),
            settings,
            budget: Budget::new(working),
            gate: Gate::new(permits),
            peak_resident: Default::default(),
            refused_elsewhere: Default::default(),
        })
    }

    /// A process with no budget known and the bus's permits: what a tool or
    /// a test runs with, and every component before its binary installs its
    /// own.
    pub fn unbounded(permits: usize) -> Self {
        Self {
            component: "unbounded".into(),
            settings: Settings::unbounded(),
            budget: Budget::new(None),
            gate: Gate::new(permits),
            peak_resident: Default::default(),
            refused_elsewhere: Default::default(),
        }
    }

    /// Its memory and what its budget did, for its report.
    pub fn accounting(&self) -> Accounting {
        let counts = self.budget.counts();
        let resident = sweep::resident_bytes();
        if let Some(resident) = resident {
            self.saw_resident(resident);
        }
        Accounting {
            limit: self.settings.limit_bytes,
            working: self.budget.working(),
            reserved: self.budget.reserved(),
            peak_reserved: counts.peak_reserved,
            resident,
            peak_resident: self
                .peak_resident
                .load(std::sync::atomic::Ordering::Relaxed),
            granted: counts.granted,
            refused: counts.refused
                + self.gate.refused()
                + self
                    .refused_elsewhere
                    .load(std::sync::atomic::Ordering::Relaxed),
            shed: counts.shed,
        }
    }

    /// Resident memory as its sweep read it, kept at its most.
    pub(crate) fn saw_resident(&self, resident: u64) {
        self.peak_resident
            .fetch_max(resident, std::sync::atomic::Ordering::Relaxed);
    }

    /// Admit one request: its turn among its class's callers, then its
    /// floor and its own bytes reserved, within [`WAIT`] in all.
    pub async fn admit(
        &self,
        class: Class,
        caller: &str,
        bytes: u64,
        what: &str,
    ) -> Result<Admitted, Overloaded> {
        let started = std::time::Instant::now();
        let turn = self.gate.admit(class, caller, WAIT).await?;
        let left = WAIT.saturating_sub(started.elapsed());
        let reservation = self
            .budget
            .reserve_async(FLOOR_BYTES + bytes, what, left)
            .await?;
        Ok(Admitted { turn, reservation })
    }
}

/// Count a request refused for load by this process outside its own gate
/// and budget -- the dashboard's turns for its pages, its terminals and
/// `/mcp`; a query its database cancelled -- so its report's refusals
/// include it.
pub fn count_refusal() {
    process()
        .refused_elsewhere
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

static INSTALLED: OnceLock<Process> = OnceLock::new();
static UNBOUNDED: OnceLock<Process> = OnceLock::new();

/// Install this component's budget from its environment, first thing in its
/// binary: the chart's limit and percents, less `held`, its hot layer's and
/// held answers' bounds. Refused where they leave nothing, as the chart
/// refuses such a configuration. With a Tokio runtime running, its sweep
/// starts too ([`sweep::forever`]).
pub fn install(component: &str, held: u64, permits: usize) -> Result<&'static Process, String> {
    let settings = Settings::from_env()?;
    let process = Process::new(component, settings, held, permits)?;
    match process.budget.working() {
        Some(working) => tracing::info!(
            component,
            limit_mib = settings.limit_bytes.unwrap_or(0) / MIB,
            working_mib = working / MIB,
            held_mib = held / MIB,
            budget_percent = settings.budget_percent,
            shed_percent = settings.shed_percent,
            "its working budget"
        ),
        None => tracing::info!(component, "no memory limit is set: its budget is unbounded"),
    }
    INSTALLED
        .set(process)
        .map_err(|_| format!("{component}: its budget is installed once"))?;
    let installed = INSTALLED.get().expect("set above");
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(sweep::forever(installed));
    }
    Ok(installed)
}

/// This process's budget and gate: the installed one, or an unbounded one
/// with the bus's permits until a binary installs its own.
pub fn process() -> &'static Process {
    INSTALLED
        .get()
        .unwrap_or_else(|| UNBOUNDED.get_or_init(|| Process::unbounded(permits_from_env())))
}

/// The installed process, where a binary installed one.
pub fn installed() -> Option<&'static Process> {
    INSTALLED.get()
}

/// The most requests a process answers at once (approved 2026-10-15, the
/// product owner, after the book's pod was OOMKilled): each is answered on
/// the blocking pool, and an unbounded burst there is as many threads, each
/// with an allocator arena of its own, as there are requests in flight. The
/// rest wait their turn at the gate.
pub const HANDLERS_AT_ONCE: usize = 16;

/// The bound a process runs with: `MERIDIAN_HANDLERS_AT_ONCE` where it is a
/// positive number, so a deployment lowers one component's without a
/// release; otherwise [`HANDLERS_AT_ONCE`].
pub fn handlers_at_once(said: Option<&str>) -> usize {
    said.and_then(|text| text.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(HANDLERS_AT_ONCE)
}

/// [`handlers_at_once`], from this process's environment.
pub fn permits_from_env() -> usize {
    handlers_at_once(std::env::var("MERIDIAN_HANDLERS_AT_ONCE").ok().as_deref())
}

pub(crate) const MIB: u64 = 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_at_a_gate_of_the_components_own_is_counted_in_its_report() {
        let before = process().accounting().refused;
        count_refusal();
        assert!(process().accounting().refused > before);
    }

    #[test]
    fn the_bound_is_sixteen_unless_a_positive_number_is_set() {
        assert_eq!(handlers_at_once(None), 16);
        assert_eq!(handlers_at_once(Some("4")), 4);
        assert_eq!(handlers_at_once(Some("0")), 16);
        assert_eq!(handlers_at_once(Some("many")), 16);
    }
}
