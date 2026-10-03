//! Which identifier to believe first.
//!
//! One table, deliberately in a crate of its own. The instrument store falls through
//! this order when resolving locally; the conductor tries global identifiers in
//! this order when asking the platform. Two orderings would be two things to
//! keep in step, and the day they disagreed a deployment would resolve one way
//! and the master another, with nothing failing to say so.
//!
//! It lived in the instrument store until 2026-09-21, which was fine while the platform
//! client lived there too. Decision 011 moved the client out, and the choice
//! was this crate or a copy. A copy of a priority table is the kind of drift
//! nothing detects, because both halves keep compiling and only the answers
//! diverge.
//!
//! Owns no store and depends on nothing but the wire types, so depending on it
//! says nothing about who may reach what.

use meridian_domain::v1::Identifier as PbIdentifier;

/// The scheme a record carries the platform's global ID under, once the
/// platform answered a person's ask with it (W3.5; decisions/030: added, never
/// substituted for the record's own ID).
pub const GLOBAL_ID: &str = "open_meridian";

/// Global schemes, strongest first.
///
/// Ordered by how hard the scheme is to confuse: the platform's global ID
/// names one instrument for all time, a FIGI names a listing, an ISIN names an
/// issue, and the national schemes below it are narrower still in coverage
/// while being no more precise.
///
/// Global meaning meaningful outside any one rail. A brokerage symbol is not
/// here at any position: it is ranked below everything in this table, however
/// it is spelled.
pub const GLOBAL_PRIORITY: [&str; 5] = [GLOBAL_ID, "figi", "isin", "cusip", "sedol"];

/// Schemes whose values are licensed (intent/vendor-sourced-reference-data,
/// ruled 2026-10-02: a licensed identifier is a key, never an answer). A
/// deployment keeps what its plugins reported under its own licence and never
/// sends one to the platform; it counts how many it holds, CUSIP Global
/// Services counting toward its 500-identifier threshold (W3.11).
pub const LICENSED: [&str; 3] = ["cusip", "isin", "sedol"];

/// Schemes whose values are open, and so the only ones a deployment asks the
/// platform by (W3.3): the global ID, a FIGI, an ISO 4217 code.
pub const OPEN: [&str; 3] = [GLOBAL_ID, "figi", "iso4217"];

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

/// The open identifiers in a set, strongest first: what the platform is asked
/// by (W3.3). A licensed scheme's value and a source-scoped symbol are dropped,
/// never sent.
pub fn open_identifiers_strongest_first(identifiers: &[PbIdentifier]) -> Vec<&PbIdentifier> {
    global_identifiers_strongest_first(identifiers)
        .into_iter()
        .filter(|identifier| OPEN.contains(&identifier.scheme.as_str()))
        .collect()
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
    fn asking_the_platform_sends_open_identifiers_only() {
        let held = vec![
            identifier("cusip", ""),
            identifier("symbol", "snaptrade"),
            identifier("iso4217", ""),
            identifier("figi", ""),
            identifier(GLOBAL_ID, ""),
        ];

        let asked: Vec<&str> = open_identifiers_strongest_first(&held)
            .into_iter()
            .map(|identifier| identifier.scheme.as_str())
            .collect();

        assert_eq!(asked, vec![GLOBAL_ID, "figi", "iso4217"]);
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
