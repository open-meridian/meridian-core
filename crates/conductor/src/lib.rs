//! The conductor: the one component that coordinates a deployment.
//!
//! An orchestra's parts play independently and one role turns them into a
//! single performance. That is this: the street store and the instrument store answer
//! the bus without asking anyone, and this component holds the deployment's
//! identity, speaks for the whole of it outward, and brings back what the
//! others cannot fetch for themselves. Decision 011.
//!
//! The name carries an invariant worth knowing before anybody scales this:
//! there is one conductor. Redundancy here means a standby, not a second one.
//!
//! # What it does
//!
//! Holds the deployment's private key, which is what lets it speak for the
//! deployment at all. Carries a person's ask about one of the deployment's
//! records to the platform, and the answer back onto the bus (W3.3, contract
//! v10). Collects what every component says about itself and reports it as
//! one picture (W5.19, W5.20).
//!
//! # Why this is not the instrument store
//!
//! It was, until 2026-09-21, and not because anything ruled it should be. When
//! the runtime split into separate processes the instrument store happened to be the one
//! already talking to the platform, so the key went with it. The argument that
//! kept a key out of the street store -- a second key-holder is a second thing
//! that can authenticate as the whole deployment -- was never turned on the
//! instrument store.
//!
//! Reference data is the surface most exposed to what the platform sends, which
//! makes the instrument store the worst place to also keep the credential that speaks
//! for the customer.
//!
//! # What it pulls, it publishes
//!
//! It never writes into another component's store. A person's ask arrives on
//! the bus, this asks the platform, and the answer goes back out on the bus for
//! the instrument store to keep through its own path. `make check-crate-boundaries` refuses
//! the shortcut, and the shortcut is the one somebody takes for convenience.
//!
//! # What it holds
//!
//! One address, the deployment's identifier and a private key. No store of
//! reference data: what the platform answered can be asked again.

pub mod assertions;
pub mod platform;

mod ask;

pub use ask::{
    public_codes, Clock, Conductor, VenueAsker, ASK_PLATFORM_FOR_INSTRUMENT, INSTRUMENT_PULLED,
    VENUE_MISSING,
};
pub use assertions::{DeploymentKey, SigningError};
pub use platform::{
    ComponentReport, Config, Enrolment, HttpTransport, Platform, PlatformError, Transport,
};
