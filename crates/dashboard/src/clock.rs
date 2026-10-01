//! Where the time comes from: the deployment's one clock (decisions/024),
//! given by whoever wires this up, so a test of a bound does not wait for it.

pub use meridian_clock::Clock;

pub const SECOND_NS: i64 = 1_000_000_000;
pub const MINUTE_NS: i64 = 60 * SECOND_NS;
pub const HOUR_NS: i64 = 60 * MINUTE_NS;
