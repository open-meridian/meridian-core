//! Which identifier to believe first.
//!
//! One table, deliberately in a crate of its own. The replica falls through
//! this order when resolving locally; the conductor tries global identifiers in
//! this order when asking the platform. Two orderings would be two things to
//! keep in step, and the day they disagreed a deployment would resolve one way
//! and the master another, with nothing failing to say so.
//!
//! It lived in the replica until 2026-09-21, which was fine while the platform
//! client lived there too. Decision 011 moved the client out, and the choice
//! was this crate or a copy. A copy of a priority table is the kind of drift
//! nothing detects, because both halves keep compiling and only the answers
//! diverge.
//!
//! Owns no store and depends on nothing but the wire types, so depending on it
//! says nothing about who may reach what.

use meridian_pb::v1::Identifier as PbIdentifier;

/// Global schemes, strongest first.
///
/// Ordered by how hard the scheme is to confuse: a FIGI names a listing, an
/// ISIN names an issue, and the national schemes below it are narrower still
/// in coverage while being no more precise.
///
/// Global meaning meaningful outside any one rail. A brokerage symbol is not
/// here at any position: it is ranked below everything in this table, however
/// it is spelled.
pub const GLOBAL_PRIORITY: [&str; 4] = ["figi", "isin", "cusip", "sedol"];

/// Where in the fallback order this identifier sits. Lower is stronger.
pub fn rank(identifier: &PbIdentifier) -> usize {
    if !identifier.source.is_empty() {
        // Source-scoped, and therefore weakest whatever it calls itself.
        return GLOBAL_PRIORITY.len() + 1;
    }

    GLOBAL_PRIORITY
        .iter()
        .position(|scheme| *scheme == identifier.scheme)
        // A global scheme nobody ranked still outranks a brokerage symbol.
        .unwrap_or(GLOBAL_PRIORITY.len())
}

/// The global identifiers in a set, strongest first.
///
/// Source-scoped ones are dropped rather than ordered last: this is what the
/// platform is asked, and a brokerage symbol means nothing outside the rail
/// that issued it, so sending one centrally would make the master's answer
/// depend on who happened to ask.
pub fn global_identifiers_strongest_first(identifiers: &[PbIdentifier]) -> Vec<&PbIdentifier> {
    let mut global: Vec<&PbIdentifier> = identifiers
        .iter()
        .filter(|identifier| identifier.source.is_empty())
        .collect();

    global.sort_by_key(|identifier| rank(identifier));
    global
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identifier(scheme: &str, source: &str) -> PbIdentifier {
        PbIdentifier {
            scheme: scheme.into(),
            value: "x".into(),
            source: source.into(),
        }
    }

    #[test]
    fn a_global_scheme_outranks_a_source_scoped_one() {
        assert!(rank(&identifier("figi", "")) < rank(&identifier("figi", "snaptrade")));
        assert!(rank(&identifier("sedol", "")) < rank(&identifier("symbol", "snaptrade")));
    }

    #[test]
    fn an_unranked_global_scheme_still_outranks_a_symbol() {
        assert!(rank(&identifier("wkn", "")) < rank(&identifier("symbol", "snaptrade")));
    }

    #[test]
    fn the_table_orders_the_ranked_schemes() {
        let mut previous = 0;
        for scheme in GLOBAL_PRIORITY {
            let here = rank(&identifier(scheme, ""));
            assert!(here >= previous, "{scheme} is out of order");
            previous = here;
        }
    }

    #[test]
    fn asking_the_platform_drops_source_scoped_identifiers() {
        let held = vec![
            identifier("symbol", "snaptrade"),
            identifier("isin", ""),
            identifier("figi", ""),
        ];

        let asked = global_identifiers_strongest_first(&held);

        assert_eq!(asked.len(), 2);
        assert_eq!(asked[0].scheme, "figi");
        assert_eq!(asked[1].scheme, "isin");
    }
}
