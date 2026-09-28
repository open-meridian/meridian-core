//! What the instrument store holds, and what any store of it must provide.
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

/// The store's copy of an instrument.
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

/// An identifier as a resolve asked it: undated, because the set is what one
/// holding was said to be on one date, and the date travels beside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Asked {
    pub scheme: String,
    pub value: String,

    /// Empty for a global scheme.
    pub source: String,
}

/// The identifiers a resolve carried, in one order whatever order they came
/// in, each once.
///
/// A placeholder stands in for a set rather than for an identifier, and the
/// same set asked twice must meet the same placeholder. A connector that lists
/// a FIGI before a symbol on Monday and after it on Tuesday is describing one
/// holding, and two placeholders for it would be two positions for one
/// security.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentifierSet {
    members: Vec<Asked>,
}

impl IdentifierSet {
    /// Sorted by scheme, then source, then value, and deduplicated.
    pub fn new(members: impl IntoIterator<Item = Asked>) -> Self {
        let mut members: Vec<Asked> = members.into_iter().collect();
        members.sort_by(|left, right| {
            (&left.scheme, &left.source, &left.value).cmp(&(
                &right.scheme,
                &right.source,
                &right.value,
            ))
        });
        members.dedup();
        Self { members }
    }

    pub fn members(&self) -> &[Asked] {
        &self.members
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// One string per set, and a different string for a different set.
    ///
    /// Each field is written with its length in bytes ahead of it, so no value
    /// can be read as a separator: a symbol with a colon in it, or a source
    /// that happens to end the way a scheme begins, cannot make two sets
    /// collide. A store holds this under a unique index, which is what makes
    /// minting one placeholder per set a property of the table rather than of
    /// whichever caller got there first.
    pub fn key(&self) -> String {
        let mut key = String::new();
        for member in &self.members {
            for field in [&member.scheme, &member.source, &member.value] {
                key.push_str(&field.len().to_string());
                key.push(':');
                key.push_str(field);
            }
        }
        key
    }
}

/// The deployment's stand-in for an identifier set nothing matched. W3.7.
///
/// Not an instrument, and deliberately kept apart from them: it carries no
/// version because no authority has said anything about it, and holding it in
/// the instrument table would let it match a later resolve as though it were
/// one. It is what a holding is recorded against until the platform's `INS-`
/// ID replaces it, and it is never deleted, because records made with it keep
/// it and a reader holding one must still learn what it became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    /// `LCL-`, minted here.
    pub placeholder_id: String,
    pub identifiers: IdentifierSet,

    /// The namespace the identifiers were asked in, for the platform's sake.
    pub source: String,

    /// Empty today: a resolve carries no asset class. Kept so the store does
    /// not have to change shape the day one does.
    pub asset_class: String,

    /// The date the resolve that minted it asked about. An escalation targets
    /// the mapping effective then, not the one effective when it is sent.
    pub as_of_ns: i64,

    pub minted_at_ns: i64,
}

/// What asking for a set's placeholder did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stood {
    /// None was held for the set, so this one was, and it wants announcing.
    Minted,

    /// The set already had one, which is the answer. The candidate is
    /// discarded, never stored.
    AlreadyHeld,
}

/// What recording a replacement did. W3.8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replaced {
    /// New, and so announced once.
    Recorded,

    /// Recorded before, by this ID or another. Nothing changes: the first
    /// pairing the platform gave stands, and a re-announced placeholder's
    /// second answer is the same pairing anyway.
    AlreadyRecorded { replaced_by: String },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the instrument store is unavailable: {0}")]
    Unavailable(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// What an applied record did, so a caller knows whether to announce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Written. The instrument store now holds this version.
    Stored,

    /// Ignored: an equal or older version than the one held.
    ///
    /// Not an error and not a failure. It is the expected outcome of a retry
    /// and of two pulls racing, and treating it as either would make callers
    /// defensive about something harmless.
    AlreadyCurrent,
}

/// Where the instrument store keeps what it has been told.
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

    /// The placeholder for the candidate's identifier set, holding the
    /// candidate if the set has none. W3.7.
    ///
    /// The candidate arrives already minted, because an ID is the caller's to
    /// make and whether it is kept is the store's to decide. One decision, so
    /// two resolves of one set racing each other meet one placeholder: the
    /// in-memory store decides under its lock, and Postgres under the unique
    /// index on the set's key.
    fn stand_in(&self, candidate: Placeholder) -> Result<(Placeholder, Stood)>;

    /// The placeholder with this ID, if this store minted it.
    fn placeholder(&self, placeholder_id: &str) -> Result<Option<Placeholder>>;

    /// Every placeholder not yet replaced, ordered by ID. What W3.7 announces
    /// again at start and on an interval.
    fn outstanding(&self) -> Result<Vec<Placeholder>>;

    /// Every held instrument whose ID is `LCL-` and has not been replaced,
    /// ordered by ID. The legacy path: see [`crate::placeholder`].
    fn legacy_outstanding(&self) -> Result<Vec<Instrument>>;

    /// Record that `replaced_id` is now `replaced_by`. W3.8.
    ///
    /// For any ID, not only a placeholder this store minted: a legacy `LCL-`
    /// instrument is replaced the same way, and so is a placeholder whose row
    /// a restore from an older backup lost, because the street store may
    /// still hold positions under it.
    fn replace(&self, replaced_id: &str, replaced_by: &str, now_ns: i64) -> Result<Replaced>;

    /// What `instrument_id` was replaced by, if it was.
    fn replacement_of(&self, instrument_id: &str) -> Result<Option<String>>;
}

/// A store's contents, for an implementation to reuse.
#[derive(Debug, Default)]
pub(crate) struct Held {
    pub(crate) by_id: HashMap<String, Instrument>,

    pub(crate) placeholders: HashMap<String, Placeholder>,

    /// A set's key to its placeholder's ID. The in-memory unique index.
    pub(crate) placeholder_by_set: HashMap<String, String>,

    /// Replaced ID to what replaced it, and when.
    pub(crate) replacements: HashMap<String, (String, i64)>,
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

    pub(crate) fn stand_in(&mut self, candidate: Placeholder) -> (Placeholder, Stood) {
        let key = candidate.identifiers.key();
        if let Some(held) = self
            .placeholder_by_set
            .get(&key)
            .and_then(|placeholder_id| self.placeholders.get(placeholder_id))
        {
            return (held.clone(), Stood::AlreadyHeld);
        }

        self.placeholder_by_set
            .insert(key, candidate.placeholder_id.clone());
        self.placeholders
            .insert(candidate.placeholder_id.clone(), candidate.clone());
        (candidate, Stood::Minted)
    }

    pub(crate) fn outstanding(&self) -> Vec<Placeholder> {
        let mut outstanding: Vec<Placeholder> = self
            .placeholders
            .values()
            .filter(|placeholder| !self.replacements.contains_key(&placeholder.placeholder_id))
            .cloned()
            .collect();
        outstanding.sort_by(|left, right| left.placeholder_id.cmp(&right.placeholder_id));
        outstanding
    }

    pub(crate) fn legacy_outstanding(&self) -> Vec<Instrument> {
        let mut legacy: Vec<Instrument> = self
            .by_id
            .values()
            .filter(|instrument| {
                instrument
                    .instrument_id
                    .starts_with(crate::ids::PLACEHOLDER_PREFIX)
                    && !self.replacements.contains_key(&instrument.instrument_id)
            })
            .cloned()
            .collect();
        legacy.sort_by(|left, right| left.instrument_id.cmp(&right.instrument_id));
        legacy
    }

    pub(crate) fn replace(
        &mut self,
        replaced_id: &str,
        replaced_by: &str,
        now_ns: i64,
    ) -> Replaced {
        if let Some((held, _)) = self.replacements.get(replaced_id) {
            return Replaced::AlreadyRecorded {
                replaced_by: held.clone(),
            };
        }
        self.replacements
            .insert(replaced_id.to_string(), (replaced_by.to_string(), now_ns));
        Replaced::Recorded
    }
}
