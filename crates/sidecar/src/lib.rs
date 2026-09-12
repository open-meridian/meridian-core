//! The surface a Meridian plugin binds to.
//!
//! One sidecar process per plugin, listening on loopback in the plugin's own
//! network namespace. The plugin reaches this and reaches nothing else: it
//! never learns the bus address, never holds a broker credential, and never
//! discovers another plugin. See decisions/007.
//!
//! The operations W4 declares are register, publish, subscribe, call, heartbeat
//! and leave. The set is not closed: it grows as workflows demand, and what an
//! operation has to be is generic, with something for the sidecar to enforce or
//! stamp by carrying it. Typed role-specific operations are a different thing
//! and do not belong here; they are generated from the function matrix and live
//! above this layer.

pub mod grants;
mod service;

pub use grants::{GrantTable, Grants};
pub use service::{Registration, Sidecar};

/// The address a plugin expects its sidecar on.
///
/// Loopback, always. A sidecar reachable from another host would be a way
/// around the boundary it exists to enforce.
pub const DEFAULT_BIND: &str = "127.0.0.1:9191";

pub use meridian_pb::v1::sidecar_service_server::SidecarServiceServer;
