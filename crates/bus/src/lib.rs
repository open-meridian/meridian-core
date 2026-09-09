//! The Meridian message bus.
//!
//! Publish-subscribe with topic patterns, plus request-reply, over a backend
//! trait. One backend ships: in-process. That is not a placeholder for the
//! first milestone — every message in a single-host deployment stays in one
//! process, and adding a network broker before anything works would mean
//! debugging two systems at once.
//!
//! # Delivery
//!
//! At-most-once, per decisions/004. Each subscriber has a bounded queue; when
//! it fills, the message is dropped and counted rather than blocking the
//! publisher. Unbounded queueing turns one stuck subscriber into a stuck
//! deployment, and blocking the publisher does the same thing faster.
//!
//! # Request-reply
//!
//! [`Bus::call`] is answered in-process by a handler registered with
//! [`Bus::serve`]. There is no reply topic and no reply address on the wire,
//! which is why neither appears in the envelope: replies never traverse the
//! bus as messages. Plugins make calls through their sidecar and never serve
//! them, so nothing outside this process needs a way to answer one.

pub mod topic;

mod backend;
mod memory;
mod router;

pub use backend::{Backend, BusError, Delivery, Subscription};
pub use memory::MemoryBackend;
pub use router::{Bus, RouteRule};

/// Envelope and metadata, re-exported so consumers need not depend on the
/// generated crate directly to use the bus.
pub use meridian_pb::v1::{Envelope, MessageMeta};

pub type Result<T> = std::result::Result<T, BusError>;
