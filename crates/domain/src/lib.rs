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
//! are the data dictionary's, generated into `meridian_pb::bounds`. And
//! [`text`], the characters a ticket's text may hold, which a plugin's
//! sidecar and the dashboard both refuse alike (W4.12, W6.21). And
//! [`setting_table`], a table setting's rows, which the dashboard and the
//! conductor check alike, cell by cell (W6.11, contract v14).

pub mod asset_class;
pub mod date;
pub mod exact;
pub mod instrument_type;
pub mod money;
pub mod setting_table;
pub mod text;
pub mod zones;

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

/// A count as a sentence says it, "2,190": a hold's days and a span's
/// records (contract v16), which the sidecar's refusal, the conductor's and
/// the dashboard's pages each name, and must name alike.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_count_is_grouped_by_thousands() {
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(999), "999");
        assert_eq!(super::thousands(2190), "2,190");
        assert_eq!(super::thousands(1_000_000_000), "1,000,000,000");
    }
}

#[allow(clippy::all)]
pub mod v1 {
    include!("v1.rs");
}
