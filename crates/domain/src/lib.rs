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
//! same reason, [`asset_class`]'s reading of an asset class from text. The
//! bounds on a value -- an account's free text, a Decimal's places and digits --
//! are the data dictionary's, generated into `meridian_pb::bounds`.

pub mod asset_class;
pub mod exact;
pub mod instrument_type;

/// The roles at the edge, which alone may own storage for their raw external
/// records (decisions/028, ruled point 1 and its amendment for `reporting`;
/// meridian-design's matrix/boundaries/roles.yaml marks the same seven, and
/// the chart says them for the launcher). Read by the sidecar and the
/// conductor, each refusing a declaration asking for storage without one.
pub const EDGE_ROLES: [&str; 7] = [
    "ccm",
    "custody",
    "dgm",
    "match",
    "reporting",
    "servicing",
    "settlement",
];

#[allow(clippy::all)]
pub mod v1 {
    include!("v1.rs");
}
