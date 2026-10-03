//! What the instrument store holds, and what any store of it must provide.
//!
//! The same shape the bus uses: a trait with one in-process implementation, and
//! Postgres behind it. Writing against the trait first is what keeps SQL out of
//! the domain logic rather than discovering it there later.
//!
//! # The deployment's records (contract v10, decisions/030)
//!
//! Every record is the deployment's own, under its own ID for life: one this
//! store minted (`LCL-`), or one applied from the platform before v10, which
//! keeps its `INS-` ID. Each value in force says where it came from, and the
//! person who set or accepted it; values a plugin or the platform states are
//! offers beside them, in force only once a person accepts one.
//!
//! # Why a version, and why it gates every write
//!
//! Every change makes a new version, numbered here, monotonic per record, and
//! kept: [`Store::write`] stores the next version only while the record is
//! still at the one the change was made against. That single rule makes a page
//! open while a plugin joined an identifier safe -- the completion made against
//! the old version is refused rather than writing over the join -- and gives
//! the book the version each entry carries (W9.8).

use std::collections::HashMap;

/// One identifier in a record's set, valid over a window.
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

    /// The identifier without its dates.
    pub fn asked(&self) -> Asked {
        Asked {
            scheme: self.scheme.clone(),
            value: self.value.clone(),
            source: self.source.clone(),
        }
    }
}

/// Which value of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    AssetClass,
    Currency,
    Description,
    Identifier,
    /// Contract v11: the type within the asset class, and a money market
    /// fund's attributes.
    InstrumentType,
    MoneyMarketFund,
}

impl Field {
    /// As the store writes it, and as a refusal names it.
    pub fn name(self) -> &'static str {
        match self {
            Field::AssetClass => "asset_class",
            Field::Currency => "currency",
            Field::Description => "description",
            Field::Identifier => "identifier",
            Field::InstrumentType => "instrument_type",
            Field::MoneyMarketFund => "money_market_fund",
        }
    }

    pub fn parse(name: &str) -> Option<Field> {
        match name {
            "asset_class" => Some(Field::AssetClass),
            "currency" => Some(Field::Currency),
            "description" => Some(Field::Description),
            "identifier" => Some(Field::Identifier),
            "instrument_type" => Some(Field::InstrumentType),
            "money_market_fund" => Some(Field::MoneyMarketFund),
            _ => None,
        }
    }
}

/// Where a value in force on a record came from (W3, requirement 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub field: Field,

    /// For [`Field::Identifier`]: which identifier.
    pub identifier: Option<Asked>,

    /// In words: a statement, "ISO 4217", "the platform, record version 3",
    /// "reported by custody-snaptrade-1".
    pub source: String,

    /// The person who set or accepted it, stamped by core; empty for a
    /// plugin's report, the platform's global ID and a value applied from the
    /// platform before v10.
    pub person: String,

    /// The delegation the person acted through, and its client's name, when
    /// they set the value through a client on the deployment's MCP surface
    /// (contract v12, Q9): stamped from the envelope; empty otherwise.
    pub acting_through_delegation: String,
    pub client_name: String,

    /// The plugin instance whose resolve joined an identifier.
    pub instance_id: String,

    pub recorded_at_ns: i64,

    /// Why a value held was changed, where given.
    pub note: String,
}

/// A value offered for a record, in force only when a person accepts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub field: Field,

    /// As text: an asset class by its enum name, a currency, a description,
    /// an identifier's value.
    pub value: String,

    /// For [`Field::Identifier`]: the identifier offered.
    pub identifier: Option<Asked>,

    /// In words, as [`Source::source`].
    pub source: String,

    /// The plugin instance whose source stated it; empty for the platform.
    pub instance_id: String,

    pub offered_at_ns: i64,
}

impl Offer {
    /// Two offers are one when they offer the same value from the same place.
    pub fn same_as(&self, other: &Offer) -> bool {
        self.field == other.field
            && self.value == other.value
            && self.identifier == other.identifier
            && self.instance_id == other.instance_id
    }
}

/// One of the deployment's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instrument {
    /// The record's key for life, and the only thing a position or an order
    /// ever stores. Never reused: an identifier retired here is not later
    /// given to something else, so this field answers "which instrument" for
    /// all time and needs no as-of. Everything dated hangs off it as an
    /// attribute, tickers included.
    pub instrument_id: String,
    pub identifiers: Vec<Identifier>,

    /// The enum's name (`ASSET_CLASS_EQUITY`), or empty for none.
    pub asset_class: String,
    pub currency: String,
    pub exchange_mic: String,
    pub description: String,
    pub lifecycle_state: String,

    /// Numbered by this store, monotonic per record. A record applied from
    /// the platform before v10 kept the platform's number, and counts on from
    /// it.
    pub version: i64,

    pub valid_from_ns: i64,
    pub record_time_ns: i64,

    /// Its type within its asset class, the enum's name
    /// (`INSTRUMENT_TYPE_MONEY_MARKET_FUND`), or empty for none (contract v11).
    pub instrument_type: String,

    /// A money market fund's attributes, their enum names separated by a
    /// space -- category, investors, NAV, liquidity fee -- or empty until a
    /// person states them (contract v11).
    pub money_market_fund: String,

    /// Where each value in force came from.
    pub sources: Vec<Source>,

    /// What plugins and the platform offer beside them.
    pub offers: Vec<Offer>,
}

impl Instrument {
    /// Whether the record carries this identifier, whatever its dates.
    pub fn carries(&self, asked: &Asked) -> bool {
        self.identifiers.iter().any(|held| {
            held.scheme == asked.scheme && held.value == asked.value && held.source == asked.source
        })
    }

    /// The value in force for a field other than an identifier.
    pub fn value(&self, field: Field) -> &str {
        match field {
            Field::AssetClass => &self.asset_class,
            Field::Currency => &self.currency,
            Field::Description => &self.description,
            Field::Identifier => "",
            Field::InstrumentType => &self.instrument_type,
            Field::MoneyMarketFund => &self.money_market_fund,
        }
    }

    pub fn set_value(&mut self, field: Field, value: String) {
        match field {
            Field::AssetClass => self.asset_class = value,
            Field::Currency => self.currency = value,
            Field::Description => self.description = value,
            Field::Identifier => {}
            Field::InstrumentType => self.instrument_type = value,
            Field::MoneyMarketFund => self.money_market_fund = value,
        }
    }

    /// The source entry for a field other than an identifier, or for one
    /// identifier.
    pub fn source_of(&self, field: Field, identifier: Option<&Asked>) -> Option<&Source> {
        self.sources
            .iter()
            .find(|source| source.field == field && source.identifier.as_ref() == identifier)
    }

    /// Record where a value came from, in place of what was said before.
    pub fn set_source(&mut self, source: Source) {
        self.sources
            .retain(|held| !(held.field == source.field && held.identifier == source.identifier));
        self.sources.push(source);
    }
}

/// An identifier as a resolve asked it: undated, because the set is what one
/// holding was said to be on one date, and the date travels beside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Asked {
    pub scheme: String,
    pub value: String,

    /// Empty for a global scheme.
    pub source: String,
}

/// The identifiers a resolve carried, in one order whatever order they came
/// in, each once.
///
/// A record minted for a set is minted once: the same set asked twice, in any
/// order and by two resolves racing, meets one record.
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
    /// minting one record per set a property of the table rather than of
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

/// One change a version made, for the history (W3.12).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Change {
    /// [`Field::name`].
    pub field: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scheme: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    #[serde(default)]
    pub before: String,
    #[serde(default)]
    pub after: String,
    #[serde(default)]
    pub source: String,
}

/// One version of a record, append-only (W3.12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub instrument_id: String,
    pub version: i64,

    /// mint, join, offer, complete, platform, merge, merged-into, migrate.
    pub operation: String,
    pub changes: Vec<Change>,
    pub person: String,
    /// The delegation and client the person acted through (contract v12);
    /// empty otherwise.
    pub acting_through_delegation: String,
    pub client_name: String,
    pub instance_id: String,
    pub note: String,

    /// For a merge, the other record.
    pub merged_instrument_id: String,
    pub record_time_ns: i64,
}

/// Identifiers that met more than one record, listed until a merge settles
/// them (W3.1, W3.2, W3.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub identifiers: Vec<Asked>,
    pub instrument_ids: Vec<String>,
    pub reported_by: String,
    pub first_seen_ns: i64,
    pub last_seen_ns: i64,
}

impl Conflict {
    /// One per set of identifiers that disagree.
    pub fn key(&self) -> String {
        IdentifierSet::new(self.identifiers.clone()).key()
    }
}

/// What asking for a set's record did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stood {
    /// None was held for the set, so this one was, and it wants announcing.
    Minted,

    /// The set already had one, which is the answer. The candidate is
    /// discarded, never stored.
    AlreadyHeld,
}

/// What writing a record's next version did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Stored,

    /// The record moved past the version the change was made against: the
    /// version it is at now. Nothing was written.
    Stale {
        held: i64,
    },

    /// No such record.
    Missing,
}

/// What recording a replacement did. W3.8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replaced {
    /// New, and so announced once.
    Recorded,

    /// Recorded before, by this ID or another. Nothing changes: the first
    /// pairing stands.
    AlreadyRecorded { replaced_by: String },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the instrument store is unavailable: {0}")]
    Unavailable(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Where the instrument store keeps what it has been told.
pub trait Store: Send + Sync {
    /// The record, if it is held, with its sources and offers.
    fn by_id(&self, instrument_id: &str) -> Result<Option<Instrument>>;

    /// Every record this identifier mapped to at `as_of_ns`.
    ///
    /// Dated, always. Resolving without a moment is how a reused ticker
    /// resolves to whoever holds it now rather than whoever held it then.
    ///
    /// All of them rather than one of them, because "more than one matched" is
    /// an answer the caller has to act on. Ordered by ID so a repeated query
    /// is a repeated answer.
    fn matching(
        &self,
        scheme: &str,
        value: &str,
        source: &str,
        as_of_ns: i64,
    ) -> Result<Vec<Instrument>>;

    /// Every record held, ordered by ID: what the Instruments page lists
    /// (W3.11). A deployment holds what its accounts hold, so a read of all of
    /// them is a page's worth.
    fn all(&self) -> Result<Vec<Instrument>>;

    /// How many records are held. For a dashboard and for tests.
    fn count(&self) -> Result<usize>;

    /// The record minted for this identifier set, holding the candidate if
    /// the set has none (W3.7), with its first version.
    ///
    /// The candidate arrives already minted, because an ID is the caller's to
    /// make and whether it is kept is the store's to decide. One decision, so
    /// two resolves of one set racing each other meet one record.
    fn mint(
        &self,
        candidate: Instrument,
        set_key: &str,
        first: Version,
    ) -> Result<(Instrument, Stood)>;

    /// Write `record` as its next version -- its identifiers, sources and
    /// offers whole -- and `entry` into its history, unless it has moved past
    /// `expected`.
    fn write(&self, record: Instrument, expected: i64, entry: Version) -> Result<Written>;

    /// A record's versions, newest first.
    fn history(&self, instrument_id: &str) -> Result<Vec<Version>>;

    /// List a conflict, or bring one listed up to date.
    fn note_conflict(&self, conflict: Conflict) -> Result<()>;

    /// Every conflict listed, ordered by when it was first seen.
    fn conflicts(&self) -> Result<Vec<Conflict>>;

    /// Record that `replaced_id` is now `replaced_by`. W3.8.
    fn replace(&self, replaced_id: &str, replaced_by: &str, now_ns: i64) -> Result<Replaced>;

    /// What `instrument_id` was replaced by, if it was.
    fn replacement_of(&self, instrument_id: &str) -> Result<Option<String>>;
}

/// A store's contents, for an implementation to reuse.
#[derive(Debug, Default)]
pub(crate) struct Held {
    pub(crate) by_id: HashMap<String, Instrument>,

    /// A set's key to the record minted for it. The in-memory unique index.
    pub(crate) minted_for: HashMap<String, String>,

    pub(crate) history: HashMap<String, Vec<Version>>,

    pub(crate) conflicts: HashMap<String, Conflict>,

    /// Replaced ID to what replaced it, and when.
    pub(crate) replacements: HashMap<String, (String, i64)>,
}

impl Held {
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

    pub(crate) fn all(&self) -> Vec<Instrument> {
        let mut all: Vec<Instrument> = self.by_id.values().cloned().collect();
        all.sort_by(|left, right| left.instrument_id.cmp(&right.instrument_id));
        all
    }

    pub(crate) fn mint(
        &mut self,
        candidate: Instrument,
        set_key: &str,
        first: Version,
    ) -> (Instrument, Stood) {
        if let Some(held) = self
            .minted_for
            .get(set_key)
            .and_then(|instrument_id| self.by_id.get(instrument_id))
        {
            return (held.clone(), Stood::AlreadyHeld);
        }
        self.minted_for
            .insert(set_key.to_string(), candidate.instrument_id.clone());
        self.history
            .entry(candidate.instrument_id.clone())
            .or_default()
            .push(first);
        self.by_id
            .insert(candidate.instrument_id.clone(), candidate.clone());
        (candidate, Stood::Minted)
    }

    pub(crate) fn write(&mut self, record: Instrument, expected: i64, entry: Version) -> Written {
        let Some(held) = self.by_id.get(&record.instrument_id) else {
            return Written::Missing;
        };
        if held.version != expected {
            return Written::Stale { held: held.version };
        }
        self.history
            .entry(record.instrument_id.clone())
            .or_default()
            .push(entry);
        self.by_id.insert(record.instrument_id.clone(), record);
        Written::Stored
    }

    pub(crate) fn history(&self, instrument_id: &str) -> Vec<Version> {
        let mut versions = self.history.get(instrument_id).cloned().unwrap_or_default();
        versions.sort_by(|left, right| right.version.cmp(&left.version));
        versions
    }

    pub(crate) fn note_conflict(&mut self, conflict: Conflict) {
        let key = conflict.key();
        match self.conflicts.get_mut(&key) {
            Some(held) => {
                held.instrument_ids = conflict.instrument_ids;
                held.last_seen_ns = conflict.last_seen_ns;
                if held.reported_by.is_empty() {
                    held.reported_by = conflict.reported_by;
                }
            }
            None => {
                self.conflicts.insert(key, conflict);
            }
        }
    }

    pub(crate) fn conflicts(&self) -> Vec<Conflict> {
        let mut all: Vec<Conflict> = self.conflicts.values().cloned().collect();
        all.sort_by(|left, right| {
            (left.first_seen_ns, left.key()).cmp(&(right.first_seen_ns, right.key()))
        });
        all
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
