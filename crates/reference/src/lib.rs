//! The deployment's replica of the security master.
//!
//! A replica rather than a cache, and the difference is what happens when the
//! platform is unreachable. A cache with an expiry stops answering; a replica
//! keeps answering from what it holds, which is what the deployment needs in
//! order to stay useful through somebody else's outage.
//!
//! # What it does
//!
//! Answers instrument questions locally (W3.1, W3.6). Reports a miss as a fact
//! rather than a request (W3.2). Pulls from the platform when the local answer
//! is nothing (W3.3), escalates when the platform does not know either (W3.4),
//! and applies what comes back under a monotonic version (W3.5).
//!
//! # What it never does
//!
//! It never mints identity. Reporting a miss is not requesting a mint, and the
//! authority to create an instrument belongs to the platform. That separation is
//! why a burst of misses cannot become a burst of instruments.
//!
//! # The constraint it is built under
//!
//! The platform may scale, move, be redirected regionally, and go down and come
//! back, without this crate restarting or being reconfigured. It holds one
//! address, its own identifier and a private key, and nothing about the
//! platform's shape.

pub mod apply;
pub mod assertions;
pub mod platform;
pub mod resolve;
pub mod service;
pub mod store;

mod memory;

pub use apply::{apply, Outcome};
pub use assertions::{DeploymentKey, SigningError};
pub use memory::MemoryStore;
pub use platform::{Config, HttpTransport, Platform, PlatformError, Reaction, Transport};
pub use resolve::{missing_instrument, resolve_identifier, resolve_instrument};
pub use service::{Handled, Reactor, SystemClock};
pub use store::{Applied, Identifier, Instrument, Store, StoreError};

pub type Result<T> = std::result::Result<T, StoreError>;

use std::sync::Arc;

use meridian_bus::Bus;

/// The replica, wired up.
///
/// The three pieces are independent and each is testable on its own: a store, a
/// platform client, a bus. This is the one place that says how a running
/// deployment holds them together, so a process that wants a replica does not
/// have to know the order.
pub struct Replica {
    bus: Arc<Bus>,
    store: Arc<dyn Store>,
    platform: Arc<Platform>,
    clock: Arc<dyn service::Clock>,
}

impl Replica {
    pub fn new(
        bus: Arc<Bus>,
        store: Arc<dyn Store>,
        platform: Arc<Platform>,
        clock: Arc<dyn service::Clock>,
    ) -> Self {
        Self {
            bus,
            store,
            platform,
            clock,
        }
    }

    /// Subscribe, register the query handlers, and hand back the loop to run.
    ///
    /// Not an `async fn`, and that is the point: everything a caller must have
    /// in place before the first message arrives happens before this returns,
    /// and only the consuming loop is left to await. Doing the registration
    /// inside the returned future would leave a window where the replica is
    /// started and answers nothing, which a caller cannot see and cannot wait
    /// for.
    ///
    /// The subscription is taken first for the same reason. At-most-once
    /// delivery drops what arrives before a subscriber exists, and it drops it
    /// silently.
    ///
    /// ```ignore
    /// let running = tokio::spawn(replica.start());
    /// ```
    pub fn start(self) -> impl std::future::Future<Output = ()> {
        let misses = self.bus.subscribe(service::INSTRUMENT_MISSING);
        service::serve_queries(&self.bus, self.store.clone());

        Reactor::new(self.bus, self.store, self.platform, self.clock).consume(misses)
    }
}
