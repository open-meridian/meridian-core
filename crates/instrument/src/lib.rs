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

pub mod calendars;
pub mod companies;
pub mod complete;
pub mod event_groups;
pub mod ids;
pub mod list;
pub mod one_id;
pub mod postgres;
pub mod reask;
pub mod record;
pub mod replace;
pub mod resolve;
pub mod service;
pub mod store;
pub mod usage;
pub mod venues;
pub mod zone;

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
        let pulled = self
            .bus
            .subscribe_shared(service::INSTRUMENT_PULLED, "keeping");
        let missing = self
            .bus
            .subscribe_shared(service::INSTRUMENT_MISSING, "keeping");
        // The calendars and rules the conductor pulled (W3.16, contract v22),
        // subscribed before anything else is in place, as the others are.
        let calendars = service::keep_calendars(
            Arc::clone(&self.bus),
            self.store.clone(),
            self.clock.clone(),
        );
        // What the conductor pulled of the platform's reference data
        // (contract v24, W3.24, W3.25, W3.27), subscribed before anything
        // is in place.
        let reference = service::keep_reference_forever(
            Arc::clone(&self.bus),
            self.store.clone(),
            self.clock.clone(),
        );
        let writing = Arc::new(Mutex::new(()));
        service::serve_all(
            &self.bus,
            self.store.clone(),
            self.clock.clone(),
            writing.clone(),
        );
        let reference = reference(writing.clone());
        // Asking the platform again, and the upgrade's migration (W3.4,
        // W3.25, W3.26), at the licence list's pace.
        let asking = service::ask_again_forever(
            Arc::clone(&self.bus),
            self.store.clone(),
            self.clock.clone(),
            writing.clone(),
        );
        let reactor =
            Reactor::sharing(self.bus, self.store, self.clock, writing).consume(pulled, missing);
        async move {
            tokio::join!(reactor, calendars, reference, asking);
        }
    }
}
