//! The deployment's instrument store.
//!
//! # What it holds (contract v10, decisions/030)
//!
//! The deployment's own instrument records, each under the deployment's ID for
//! life. It mints a record for an identifier set nothing matched (W3.7), and
//! matches the identifiers plugins report to one record by an identifier in
//! common, a contradiction being a conflict for a person rather than a match
//! (W3.1). Each value in force says its source and the person who set or
//! accepted it; plugins and the platform offer values beside them; every
//! change is a version kept for good (W3.12).
//!
//! # Who completes a record
//!
//! The deployment admin, at the dashboard's Instruments page (W3.10, ruled
//! 2026-10-02): this store refuses a completion with no person, a value with
//! no source, and a change to a value held with no note.
//!
//! # What it never does
//!
//! It never reaches the platform. Asking the platform about a record is a
//! person's choice, carried by the conductor, which alone holds the key
//! (decisions/011); the answer arrives on the bus and is kept as offers, its
//! global ID added as an identifier, never adopted as the key.

pub mod complete;
pub mod ids;
pub mod list;
pub mod postgres;
pub mod record;
pub mod replace;
pub mod resolve;
pub mod service;
pub mod store;

mod memory;

pub use memory::MemoryStore;
/// The scheme a record carries the platform's global ID under (W3.5).
pub use meridian_symbology::GLOBAL_ID;
pub use postgres::PostgresStore;
pub use resolve::{resolve_identifier, resolve_instrument, Resolution};
pub use service::{Clock, Handled, Reactor};
pub use store::{
    Asked, Conflict, Field, Identifier, IdentifierSet, Instrument, Offer, Replaced, Source, Stood,
    Store, StoreError, Version, Written,
};

pub type Result<T> = std::result::Result<T, StoreError>;

use std::sync::{Arc, Mutex};

use meridian_bus::Bus;

/// The instrument store, wired up: a store and a bus, and nothing else.
pub struct InstrumentService {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn service::Clock>,
}

impl InstrumentService {
    pub fn new(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn service::Clock>) -> Self {
        Self { bus, store, clock }
    }

    /// Subscribe, register the handlers, and hand back the loop to run.
    ///
    /// Not an `async fn`, and that is the point: everything a caller must have
    /// in place before the first message arrives happens before this returns,
    /// and only the consuming loop is left to await. The subscriptions are
    /// taken first: at-most-once delivery drops what arrives before a
    /// subscriber exists, and it drops it silently.
    pub fn start(self) -> impl std::future::Future<Output = ()> {
        let pulled = self.bus.subscribe(service::INSTRUMENT_PULLED);
        let missing = self.bus.subscribe(service::INSTRUMENT_MISSING);
        let writing = Arc::new(Mutex::new(()));
        service::serve_all(
            &self.bus,
            self.store.clone(),
            self.clock.clone(),
            writing.clone(),
        );
        Reactor::sharing(self.bus, self.store, self.clock, writing).consume(pulled, missing)
    }
}
