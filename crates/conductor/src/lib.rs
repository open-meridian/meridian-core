//! The conductor: the one component that coordinates a deployment.
//!
//! An orchestra's parts play independently and one role turns them into a
//! single performance. That is this: the street store and the replica answer
//! the bus without asking anyone, and this component holds the deployment's
//! identity, speaks for the whole of it outward, and brings back what the
//! others cannot fetch for themselves. Decision 011.
//!
//! The name carries an invariant worth knowing before anybody scales this:
//! there is one conductor. Redundancy here means a standby, not a second one.
//! Two would pull the same record twice, escalate the same miss twice, and
//! defeat the throttle that exists so a burst of misses is not a burst of
//! mints.
//!
//! # What it does
//!
//! Holds the deployment's private key, which is what lets it speak for the
//! deployment at all. Carries a miss to the platform and the answer back onto
//! the bus (W3.3, W3.4). Collects what every component says about itself and
//! reports it as one picture (W5.19, W5.20).
//!
//! # Why this is not the replica
//!
//! It was, until 2026-09-21, and not because anything ruled it should be. When
//! the runtime split into separate processes the replica happened to be the one
//! already talking to the platform, so the key went with it. The argument that
//! kept a key out of the street store -- a second key-holder is a second thing
//! that can authenticate as the whole deployment -- was never turned on the
//! replica.
//!
//! Reference data is the surface most exposed to what the platform sends, which
//! makes the replica the worst place to also keep the credential that speaks
//! for the customer.
//!
//! # What it pulls, it publishes
//!
//! It never writes into another component's store. A miss arrives on the bus,
//! this asks the platform, and the record goes back out on the bus for the
//! replica to apply through its own path. `make check-crate-boundaries` refuses
//! the shortcut, and the shortcut is the one somebody takes for convenience.
//!
//! # Why it can act on a miss without being asked
//!
//! W3.2 is published only after W3.1 returned nothing, so a miss already means
//! the replica does not hold it. Nothing has to ask this component anything,
//! and no round trip enters the read path.
//!
//! # What it holds
//!
//! One address, the deployment's identifier, a private key, and a throttle
//! window. No store: what the platform published can be fetched again, which is
//! the whole difference between this and the two components either side of it.

pub mod assertions;
pub mod platform;

mod reactor;

pub use assertions::{DeploymentKey, SigningError};
pub use platform::{
    ComponentReport, Config, HttpTransport, Platform, PlatformError, Reaction, Transport,
};
pub use reactor::{Carried, Clock, Conductor, SystemClock, INSTRUMENT_MISSING, INSTRUMENT_PULLED};
