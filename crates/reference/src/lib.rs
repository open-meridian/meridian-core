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

pub mod assertions;
pub mod store;

mod memory;

pub use assertions::{DeploymentKey, SigningError};
pub use memory::MemoryStore;
pub use store::{Applied, Identifier, Instrument, Store, StoreError};

pub type Result<T> = std::result::Result<T, StoreError>;
