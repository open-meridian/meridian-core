//! What the replica holds, and what any store of it must provide.
//!
//! The same shape the bus uses: a trait with one in-process implementation, and
//! Postgres behind it when compose arrives. Writing against the trait first is
//! what keeps SQL out of the domain logic rather than discovering it there
//! later.
//!
//! # Why a version, and why it is the only bookmark
//!
//! Every record carries a version the platform assigns, monotonic per
//! instrument. Applying one at or below the version held is a no-op. That single
//! rule does most of the work in this crate:
//!
//! - a retry after an ambiguous failure is harmless, so the retry policy can be
//!   simple rather than exactly-once,
//! - two overlapping pulls of the same instrument cannot corrupt anything,
//! - and the highest version held **is** the resume cursor, so nothing has to
//!   maintain a separate marker that can go stale while the platform is away.
//!
//! The last point is what makes recovery from an outage automatic. There is no
//! bookmark to repair because there is no bookmark.

use std::collections::HashMap;

/// One identifier in an instrument's set, valid over a window.
///
/// Dated because identifiers are reused: a delisted instrument frees a ticker
/// and somebody else is given it. A lookup without an as-of is a bug waiting for
/// a reassignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identifier {
    pub scheme: String,
    pub value: String,

    /// The namespace a source-scoped identifier belongs to, e.g. `snaptrade`.
    /// Empty for a global scheme.
    pub source: String,

    pub valid_from_ns: i64,

    /// `None` while the mapping is still current.
    pub valid_to_ns: Option<i64>,
}

impl Identifier {
    /// Whether this mapping was true at `as_of_ns`.
    pub fn covers(&self, as_of_ns: i64) -> bool {
        self.valid_from_ns <= as_of_ns && self.valid_to_ns.is_none_or(|end| end > as_of_ns)
    }
}

/// A replica's copy of an instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instrument {
    /// The canonical identity, and the only thing a position or an order ever
    /// stores. Never reused: an identifier retired here is not later given to
    /// something else, so this field answers "which instrument" for all time
    /// and needs no as-of. Everything dated hangs off it as an attribute,
    /// tickers included.
    pub instrument_id: String,
    pub identifiers: Vec<Identifier>,
    pub asset_class: String,
    pub currency: String,
    pub exchange_mic: String,
    pub description: String,
    pub lifecycle_state: String,

    /// Assigned by the platform, monotonic. The apply gate and the resume
    /// cursor are both this number.
    pub version: i64,

    pub valid_from_ns: i64,
    pub record_time_ns: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the replica is unavailable: {0}")]
    Unavailable(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// What an applied record did, so a caller knows whether to announce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Written. The replica now holds this version.
    Stored,

    /// Ignored: an equal or older version than the one held.
    ///
    /// Not an error and not a failure. It is the expected outcome of a retry
    /// and of two pulls racing, and treating it as either would make callers
    /// defensive about something harmless.
    AlreadyCurrent,
}

/// Where a replica keeps what it has been told.
pub trait Store: Send + Sync {
    /// The instrument, if it is held.
    fn by_id(&self, instrument_id: &str) -> Result<Option<Instrument>>;

    /// Every instrument this identifier mapped to at `as_of_ns`.
    ///
    /// Dated, always. Resolving without a moment is how a reused ticker
    /// resolves to whoever holds it now rather than whoever held it then.
    ///
    /// All of them rather than one of them, because "more than one matched" is
    /// an answer the caller has to act on. A store that returned the first
    /// match would be picking, and a pick made here would be invisible to the
    /// step that is supposed to refuse it.
    ///
    /// Ordered by instrument identifier so a repeated query is a repeated
    /// answer.
    fn matching(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
        as_of_ns: i64,
    ) -> Result<Vec<Instrument>>;

    /// Write, unless what is held is already at or beyond this version.
    ///
    /// The gate lives in the store rather than in the caller because every
    /// inbound path lands here, and a check spread across callers is a check
    /// enforced by whichever caller remembered.
    fn apply(&self, instrument: Instrument) -> Result<Applied>;

    /// The highest version held, per instrument.
    ///
    /// The resume cursor, and deliberately derived rather than recorded: a
    /// separate marker is a thing that can disagree with the data it describes.
    fn version_of(&self, instrument_id: &str) -> Result<Option<i64>>;

    /// How many instruments are held. For a dashboard and for tests.
    fn count(&self) -> Result<usize>;
}

/// A store's contents, for an implementation to reuse.
#[derive(Debug, Default)]
pub(crate) struct Held {
    pub(crate) by_id: HashMap<String, Instrument>,
}

impl Held {
    pub(crate) fn apply(&mut self, instrument: Instrument) -> Applied {
        if let Some(existing) = self.by_id.get(&instrument.instrument_id) {
            if existing.version >= instrument.version {
                return Applied::AlreadyCurrent;
            }
        }
        self.by_id
            .insert(instrument.instrument_id.clone(), instrument);
        Applied::Stored
    }

    pub(crate) fn matching(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
        as_of_ns: i64,
    ) -> Vec<Instrument> {
        let mut found: Vec<Instrument> = self
            .by_id
            .values()
            .filter(|instrument| {
                instrument.identifiers.iter().any(|identifier| {
                    identifier.scheme == scheme
                        && identifier.value == value
                        && identifier.source == source
                        && identifier.covers(as_of_ns)
                })
            })
            .cloned()
            .collect();

        // A HashMap iterates in whatever order it likes, and an ambiguous
        // resolution that reports a different pair of candidates each time is
        // an incident nobody can reproduce.
        found.sort_by(|left, right| left.instrument_id.cmp(&right.instrument_id));
        found
    }
}
