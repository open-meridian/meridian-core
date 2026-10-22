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
//! conductor check alike, cell by cell (W6.11, contract v14). And
//! [`declaration`], a version's declaration as the SDK prints it in JSON
//! ([`protojson`] for the messages registration sends), which the dashboard
//! reads from an upload and a harness's sidecar from its file alike (W8.1,
//! W4.1, contract v25).

pub mod asset_class;
pub mod attestation;
pub mod date;
pub mod declaration;
pub mod exact;
pub mod instrument_type;
pub mod lake;
pub mod money;
pub mod protojson;
pub mod semver;
pub mod sessions;
pub mod setting_table;
pub mod template;
pub mod text;
pub mod zone;
pub mod zones;

/// The roles at the edge, which alone may own storage for their raw external
/// records (decisions/028, ruled point 1 and its amendment for `reporting`;
/// meridian-design's matrix/boundaries/roles.yaml marks the same seven, and
/// the chart says them for the launcher). Read by the sidecar and the
/// conductor, each refusing a declaration asking for storage without one.
pub const EDGE_ROLES: [&str; 7] = [
    "confirmation",
    "connectivity",
    "custody",
    "data",
    "reporting",
    "servicing",
    "settlement",
];

/// The three roles renamed at contract v20 (the product owner, 2026-10-10:
/// plans/an-agents-prepared-change-waits-for-the-person, "Three roles
/// renamed"), each old code name and the name it became. A plugin built
/// before v20 still declares the old one, and deployment data written
/// before v20 names it: each is read as the new name for one revision, and
/// the configuration store's one-time rewrite renames what it keeps, each
/// rename its own record (decisions/031).
pub const RENAMED_ROLES: [(&str, &str); 3] = [
    ("ccm", "connectivity"),
    ("dgm", "data"),
    ("match", "confirmation"),
];

/// A role's name now: the new name of one renamed at v20, any other as given.
pub fn role_now(role: &str) -> &str {
    RENAMED_ROLES
        .iter()
        .find(|(old, _)| *old == role)
        .map_or(role, |(_, new)| new)
}

/// Each of `roles` by its name now ([`role_now`]), in the same order.
pub fn roles_now(roles: &[String]) -> Vec<String> {
    roles
        .iter()
        .map(|role| role_now(role).to_string())
        .collect()
}

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
    fn a_renamed_role_reads_as_its_new_name_and_any_other_as_given() {
        assert_eq!(super::role_now("dgm"), "data");
        assert_eq!(super::role_now("ccm"), "connectivity");
        assert_eq!(super::role_now("match"), "confirmation");
        for kept in ["data", "custody", "reporting", ""] {
            assert_eq!(super::role_now(kept), kept);
        }
        assert_eq!(
            super::roles_now(&["custody".into(), "dgm".into()]),
            vec!["custody".to_string(), "data".to_string()]
        );
        // Every edge role is a role now, none an old name.
        for role in super::EDGE_ROLES {
            assert_eq!(super::role_now(role), role);
        }
    }

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
