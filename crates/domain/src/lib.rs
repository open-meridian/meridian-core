//! The domain messages: holdings, reference data and accounts.
//!
//! Generated from `proto/` by `make codegen`, and never edited by hand;
//! `make check-codegen` fails when `v1.rs` and the protos disagree. These are
//! the runtime's own traffic past the sidecar. What a plugin sees is the
//! sidecar's surface in meridian-schema, which carries none of them.

#![allow(clippy::all)]

pub mod v1 {
    include!("v1.rs");
}
