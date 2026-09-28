//! The deployment's instrument store.
//!
//! For identity it is a replica rather than a cache, and the difference is what
//! happens when the platform is unreachable. A cache with an expiry stops
//! answering; a replica
//! keeps answering from what it holds, which is what the deployment needs in
//! order to stay useful through somebody else's outage.
//!
//! # What it does
//!
//! Answers instrument questions locally (W3.1, W3.6). Stands a placeholder in
//! for an identifier set nothing matched, and announces it until it is
//! replaced (W3.7). Applies under a monotonic version what the conductor
//! publishes after pulling or escalating (W3.5), and records the placeholder a
//! record replaces (W3.8).
//!
//! It does not reach the platform. W3.3 and W3.4 are the conductor's, and so is
//! the key that would let anything here try: decision 011.
//!
//! # What it never does
//!
//! It never mints identity. Reporting a miss is not requesting a mint, and the
//! authority to create an instrument belongs to the platform. That separation is
//! why a burst of misses cannot become a burst of instruments.
//!
//! What it mints is a placeholder, `LCL-`, one per identifier set, and nothing
//! else. A placeholder is a name for a holding while its identity is unknown,
//! not an instrument: it is never written into the instrument table, and the
//! platform's `INS-` ID replaces it.
//!
//! # The constraint it is built under
//!
//! The platform may scale, move, be redirected regionally, and go down and come
//! back, without this crate restarting or being reconfigured. That is easier
//! than it was: this crate now holds no address, no identifier and no key, and
//! learns what the platform said only because somebody published it.

pub mod apply;
pub mod ids;
pub mod placeholder;
pub mod postgres;
pub mod resolve;
pub mod service;
pub mod store;

mod memory;

pub use apply::{apply, Outcome};
pub use memory::MemoryStore;
pub use postgres::PostgresStore;
pub use resolve::{missing_instrument, resolve_identifier, resolve_instrument, Resolution};
pub use service::{Handled, Reactor, SystemClock};
pub use store::{
    Applied, Identifier, IdentifierSet, Instrument, Placeholder, Replaced, Stood, Store, StoreError,
};

pub type Result<T> = std::result::Result<T, StoreError>;

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::Bus;

/// The instrument store, wired up.
///
/// A store and a bus, and nothing else. It held a platform client until
/// 2026-09-21, which is what put the deployment's private key in the same
/// process as the instrument tables; decision 011 moved both to the conductor.
/// What arrives here now arrives on the bus like everything else.
pub struct InstrumentService {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    clock: Arc<dyn service::Clock>,
    announce_every: Duration,
}

impl InstrumentService {
    pub fn new(bus: Arc<Bus>, store: Arc<dyn Store>, clock: Arc<dyn service::Clock>) -> Self {
        Self {
            bus,
            store,
            clock,
            announce_every: service::ANNOUNCE_EVERY,
        }
    }

    /// How often outstanding placeholders are announced again, in place of
    /// [`service::ANNOUNCE_EVERY`].
    pub fn announcing_every(mut self, every: Duration) -> Self {
        self.announce_every = every;
        self
    }

    /// Subscribe, register the query handlers, and hand back the loop to run.
    ///
    /// Not an `async fn`, and that is the point: everything a caller must have
    /// in place before the first message arrives happens before this returns,
    /// and only the consuming loop is left to await. Doing the registration
    /// inside the returned future would leave a window where the instrument store is
    /// started and answers nothing, which a caller cannot see and cannot wait
    /// for.
    ///
    /// The subscription is taken first for the same reason. At-most-once
    /// delivery drops what arrives before a subscriber exists, and it drops it
    /// silently.
    ///
    /// The loop also announces every outstanding placeholder, at once and then
    /// on the interval (W3.7), and ends when the bus shuts down.
    ///
    /// ```ignore
    /// let running = tokio::spawn(service.start());
    /// ```
    pub fn start(self) -> impl std::future::Future<Output = ()> {
        let pulled = self.bus.subscribe(service::INSTRUMENT_PULLED);
        service::serve_queries(&self.bus, self.store.clone(), self.clock.clone());

        let announcing = service::announce_forever(
            self.bus.clone(),
            self.store.clone(),
            self.clock.clone(),
            self.announce_every,
        );
        let consuming = Reactor::new(self.bus, self.store, self.clock).consume(pulled);

        async move {
            tokio::select! {
                _ = consuming => {}
                _ = announcing => {}
            }
        }
    }
}
