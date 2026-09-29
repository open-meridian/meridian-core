//! How much an account's free text holds (W6.3): the conductor refuses more,
//! naming the field, and the dashboard's form says so before it is sent. Here
//! because both read it and this is the crate they share.

/// The most characters an account's custodian, type or owner holds.
pub const LABEL_MOST: usize = 200;

/// The most characters an account's note holds.
pub const NOTE_MOST: usize = 2_000;
