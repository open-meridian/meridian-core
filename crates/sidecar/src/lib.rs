//! The surface a Meridian plugin binds to.
//!
//! One sidecar process per plugin, listening on loopback in the plugin's own
//! network namespace. The plugin reaches this and reaches nothing else: it
//! never learns the bus address, never holds a broker credential, and never
//! discovers another plugin. See decisions/004.
//!
//! Six operations, derived from workflow W4: register, publish, subscribe,
//! call, heartbeat, leave. Typed role-specific operations are generated from
//! the function matrix and live above this layer, not in it.

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
