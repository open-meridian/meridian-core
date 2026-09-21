//! The dashboard: the one address a firm's staff use for their deployment.
//!
//! It signs people in through the firm's directory over OpenID Connect -- a
//! SAML or LDAP directory brokered by the bundled Zitadel -- and decides what
//! they may reach from the permissions the conductor's configuration store
//! holds, evaluated by [`meridian_access`]. It holds no deployment key and
//! never reaches the platform; what goes there goes through the conductor.
//! spec/deployment-dashboard-and-access.
//!
//! # Where the bounds live
//!
//! Sessions end after 30 minutes idle or 12 hours absolute ([`session`]).
//! The records are read every 30 seconds and refused past 10 minutes
//! ([`records`]). All four are decisions/015's, stated as constants rather
//! than configuration, because a bound somebody can widen in a values file is
//! not a bound.

pub mod clock;
pub mod html;
pub mod records;
pub mod session;
pub mod web;

pub use clock::{Clock, SystemClock};
pub use records::{refresh, refresh_forever, RecordsCache, Stale};
pub use session::{Session, Sessions};
pub use web::{router, App};
