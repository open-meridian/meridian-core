//! The domain messages: holdings, reference data and accounts.
//!
//! Generated from `proto/` by `make codegen`, and never edited by hand;
//! `make check-codegen` fails when `v1.rs` and the protos disagree. These are
//! the runtime's own traffic past the sidecar. What a plugin sees is the
//! sidecar's surface in meridian-schema, which carries none of them.
//!
//! And one type written by hand, [`exact::Exact`]: a `meridian.v1.Decimal` as
//! the runtime holds it. Here because every component that reads a quantity
//! has to read it the same way, and this is the crate they all share. For the
//! same reason, [`account`]'s bounds on an account's free text, and
//! [`asset_class`]'s reading of an asset class from text.

pub mod account;
pub mod asset_class;
pub mod exact;

#[allow(clippy::all)]
pub mod v1 {
    include!("v1.rs");
}
