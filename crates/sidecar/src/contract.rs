//! Which contract versions this sidecar admits a plugin under.
//!
//! A range, not a match. A plugin upgrades independently of the runtime --
//! ruled 2026-09-20, the way an application does of an operating system -- so
//! one built against an older contract has to keep registering after the
//! runtime moves on. Under the equality check this replaced, the first change
//! to the version string would have refused every plugin in every deployment
//! at once, with a message saying only what was not supported.
//!
//! The ceiling is this sidecar's own version. A plugin built against a newer
//! contract is refused as firmly as an old one: it may call an operation the
//! sidecar cannot answer, and finding that out at registration is cheaper than
//! finding it at the first call.
//!
//! On the wire the version stays a string of the form `v<N>`, which it already
//! was. Changing the field's type would itself break every plugin already
//! built, which is the one thing this module exists to prevent.

/// The oldest contract a plugin may have been built against.
///
/// Raising it strands every plugin built against anything below it, so it
/// rises only by a recorded decision that names what it strands. How long an
/// old version must be honoured before that is allowed is not yet decided:
/// `sdk-contract/sidecar-version-range` in meridian-design. With the floor
/// equal to the current version nothing is stranded yet, so the question only
/// bites the first time somebody wants to raise it.
///
/// Raised to 2 by decisions/013, the first such decision, in the release that
/// deleted the generic operations: it stranded nothing outside our own
/// repositories.
pub const CONTRACT_FLOOR: u32 = 2;

/// The contract this sidecar implements.
///
/// Raised by every revision that adds something a plugin can depend on -- an
/// operation, a field on a stream or a response, a refusal code -- while the
/// floor stays, so older plugins keep registering and a plugin built for the
/// newer contract is refused by an older sidecar rather than running without
/// what it was built to read (sdk-contract/an-addition-raises-the-contract-version,
/// ruled 2026-09-30, after a plugin reading its links from the account-scope
/// stream registered with a sidecar that sent none).
///
/// v2 is typed operations, acting-for, and the settings, access and scope
/// streams (spec/typed-sidecar-operations). v3 is everything a plugin gained
/// after v2 was cut: reporting its external accounts and linking them, the
/// account side's fields, the caller's deployment-admin flag, what a setting
/// and an admin page declare, its links on the account-scope stream, and the
/// refusal code beside a refusal. v4 is the asset class as an enum rather than
/// free text, on the miss a plugin reports, and the refusal of an enum value
/// the contract does not define (sdk-contract/asset-class-is-an-enum). v5 is
/// one list of pages, each with the levels it serves, in place of the admin
/// pages, and the level a session was opened at in the claims, `admin` beside
/// `read` and `write` (sdk-contract/a-plugin-has-admins); a plugin built
/// before keeps its admin pages, read as pages at `admin` ([`crate::legacy`]).
/// v6 is the figures a plugin reports on its heartbeat, which its report
/// carries for the dashboard's Summary (sdk-contract/a-plugin-reports-its-figures);
/// a plugin built before reports none. v7 is typed delivery, the stream of
/// what a plugin's roles hear with its loss marker; reads within a scope that
/// is nothing when empty, refused naming an account outside it; the street
/// read since a watermark and its completed statements; and a statement
/// naming its external account and institution, its figures per margin
/// segment with their collateral, and a holding its cost, average cost, lots
/// and margin requirement (sdk-contract/plugins-read-positions-and-prices,
/// kernel/reads-are-scoped, sdk-contract/a-holding-carries-its-cost). A
/// plugin built before names no external account on a statement, which is
/// admitted with none, and sends its figures flat ([`crate::older`]). v8 is
/// the book of record (W9): its commands, reads and deliveries, a typed
/// operation carrying a oneof, a plugin-supplied idempotency key on the
/// book's commands, and the book's refusal codes carried beside `aborted`
/// (sdk-contract/the-book-holds-positions). v9 is the book's refusal of an
/// entry missing what downstream needs, `REFUSAL_REASON_INCOMPLETE` with each
/// missing field in the refusal's `fields`
/// (sdk-contract/the-book-refuses-what-downstream-cannot-use), and the
/// delegation and client a person acted through, named in the claims and
/// stamped on the envelope beside the person
/// (sdk-contract/delegations-at-the-deployment-contract); a plugin built
/// before reads neither, and the book's refusal applies to it all the same.
/// v10 is a deployment's own instrument identity and its completion
/// (decisions/030; sdk-contract/a-deployments-instrument-identity-is-its-own,
/// kernel/a-deployment-completes-its-instrument-records): what a plugin's
/// source states of a security on its resolve, a resolve answering a record
/// it minted in place of a placeholder, a record's values with their sources
/// and offers, the book's refusal of an instrument whose record lacks an
/// asset class or a currency and of a command it could not check
/// (`REFUSAL_REASON_REFERENCE_UNAVAILABLE`, beside `unavailable`), and the
/// client beside the delegation on the envelope and in the book's actor
/// (sdk-contract/the-book-records-the-delegation); a plugin built before
/// states nothing on its resolve, and the book's refusal applies to it all
/// the same. v11 is the edge keeping its own (spec/vendor-differences-have-a-
/// place-in-the-contract, slice A, custody half; sdk-contract/the-edge-keeps-
/// its-own, sdk-contract/the-street-counts-each-asset-once): a value as
/// reported beside its not-known value, checked for its shape alone, on an
/// account's kind and a miss's asset class; a row's raw record and each
/// closed value's provenance, the settled and pending quantities by value
/// date, each asset once, and a backfill as an amendment; the instrument type
/// under the asset class, stated at resolve; and the version's declaration
/// at registration, in the report, with the names not carried a plugin saw
/// on its heartbeat ([`crate::edge`]). A plugin built before sends its
/// account's type, its currency flag and "also counted in cash", accepted for
/// the notice the stability list gives, and declares nothing. v12 is the
/// deployment's MCP surface (spec/a-deployment-serves-its-mcp;
/// sdk-contract/a-deployment-serves-its-mcp-contract): the tools a plugin's
/// SDK derives from its typed routes, declared at registration, each checked
/// and refused alone, carried in the report, and the tool a call names in
/// the claims, admitted only at that tool's route ([`crate::tools`]). A
/// plugin built before declares none, and is on no agent's list. v13 is
/// tickets inside a deployment (spec/a-problem-seen-in-a-deployment-reaches-
/// someone-who-can-act, slice 1; sdk-contract/a-problem-reaches-someone-who-
/// can-act-contract): `SidecarService.FileTicket` and `FiledTickets`, by
/// which a plugin files a ticket for the person it is serving, never as
/// itself, and reads back what became of what it filed ([`crate::tickets`]).
/// A plugin built on them would meet `UNIMPLEMENTED` at its first filing on
/// an older sidecar, rather than a refusal at registration naming both
/// versions, which is why the version rose. A plugin built before files
/// nothing. v14 is the custodian's activity (spec/the-custodians-activity-
/// explains-a-break; sdk-contract/the-custodians-activity-contract): a
/// custody plugin records each activity as the custodian states it and an
/// operations plugin reads and hears it, with `history_from` on the sync
/// status and the read; each sync status the street keeps, heard and read
/// by an operations plugin; and the book's cause for income the custodian
/// reinvested, and a cause's link to the activity that explains it. A plugin
/// built before reports and reads no activity. v15 is a person's access
/// granted per role of a plugin (spec/access-is-granted-per-role-within-a-
/// plugin, decisions/033; sdk-contract/access-is-granted-per-role): each
/// role's level and accounts in the claims, the roles a page, tool and
/// setting serves at registration and in the report ([`crate::roles`]), the
/// access table per role, and a person's command admitted by the role whose
/// grants hold it. A plugin built before names no role on its declarations,
/// which on a plugin holding several serve every role, and reads the claims'
/// level and account sets, the union over the roles.
pub const CONTRACT_CURRENT: u32 = 15;

/// The contract version a plugin registered with, as a number: what the
/// rules for a plugin built before an addition read. Zero for one that does
/// not read, which registration has refused already.
pub fn declared(version: &str) -> u32 {
    version
        .strip_prefix('v')
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

/// Admit a plugin's declared contract version, or say why not.
///
/// A refusal names both what was declared and what is accepted, because the
/// fix belongs to whoever reads it -- the vendor rebuilding a plugin, or the
/// operator upgrading a runtime -- and a message naming only one half sends
/// the other looking.
pub fn admit(declared: &str) -> Result<u32, String> {
    admit_within(CONTRACT_FLOOR, CONTRACT_CURRENT, declared)
}

/// The rule itself, over any range.
///
/// Separate from [`admit`] so the rule can be tested over any range, including
/// the one an older sidecar holds: a plugin built for this sidecar's contract
/// meeting one still at the last.
pub fn admit_within(floor: u32, current: u32, declared: &str) -> Result<u32, String> {
    let accepts = format!("v{floor} through v{current}");

    if declared.is_empty() {
        // Admitted until 2026-09-21. That made saying nothing the safest thing
        // a vendor could do, and a field that may be omitted stops meaning
        // anything.
        return Err(format!(
            "the plugin declared no contract version; this sidecar accepts {accepts}"
        ));
    }

    let version = declared
        .strip_prefix('v')
        .and_then(|number| number.parse::<u32>().ok())
        .ok_or_else(|| {
            format!("contract version {declared:?} is not of the form v<N>; this sidecar accepts {accepts}")
        })?;

    if version < floor {
        return Err(format!(
            "the plugin was built against contract v{version}, older than this sidecar accepts \
             ({accepts}); rebuild it against v{floor} or later"
        ));
    }

    if version > current {
        return Err(format!(
            "the plugin was built against contract v{version}, newer than this sidecar \
             ({accepts}); upgrade the runtime, or rebuild the plugin against \
             v{current} or earlier"
        ));
    }

    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The refusal texts are the fixture's, word for word: meridian-design's
    // fixtures/sidecar/register.yaml pins them, so a vendor reading one sees
    // what the contract promises they will see.

    #[test]
    fn a_plugin_built_for_the_current_contract_is_admitted() {
        assert_eq!(admit("v10"), Ok(10));
    }

    #[test]
    fn a_plugin_built_before_the_last_addition_still_registers() {
        // The floor stays when the current version rises: a plugin built on
        // an SDK declaring v2 through v5 keeps registering with this sidecar.
        // Under `declared == current` it would be refused.
        assert_eq!(admit("v2"), Ok(2));
        assert_eq!(admit("v3"), Ok(3));
        assert_eq!(admit("v4"), Ok(4));
        assert_eq!(admit("v5"), Ok(5));
        assert_eq!(admit("v6"), Ok(6));
        assert_eq!(admit("v7"), Ok(7));
        assert_eq!(admit("v8"), Ok(8));
        assert_eq!(admit_within(1, 2, "v1"), Ok(1));
    }

    #[test]
    fn a_plugin_built_for_this_contract_is_refused_by_a_sidecar_at_the_last() {
        // On 2026-09-30 a plugin built to read its links from the
        // account-scope stream met a sidecar from before links, registered,
        // and ran with none, because both said v2. Declaring v3, it is refused
        // by a sidecar still at v2, naming both.
        assert_eq!(
            admit_within(2, 2, "v3"),
            Err(
                "the plugin was built against contract v3, newer than this sidecar \
                 (v2 through v2); upgrade the runtime, or rebuild the plugin against \
                 v2 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_built_for_the_asset_class_enum_is_refused_by_a_sidecar_at_v3() {
        // A plugin built on an SDK declaring v4 reports its misses with the
        // enum; a sidecar at v3 reads the old free-text field and would drop
        // the class. Refused at the door instead, naming both.
        assert_eq!(
            admit_within(2, 3, "v4"),
            Err(
                "the plugin was built against contract v4, newer than this sidecar \
                 (v2 through v3); upgrade the runtime, or rebuild the plugin against \
                 v3 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_declaring_pages_with_levels_is_refused_by_a_sidecar_at_v4() {
        // A plugin built on an SDK declaring v5 declares its pages with the
        // levels each serves and reads the session's level from its claims;
        // a sidecar at v4 would read neither. Refused at the door, in the
        // fixture's words.
        assert_eq!(
            admit_within(2, 4, "v5"),
            Err(
                "the plugin was built against contract v5, newer than this sidecar \
                 (v2 through v4); upgrade the runtime, or rebuild the plugin against \
                 v4 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_reporting_figures_is_refused_by_a_sidecar_at_v5() {
        // A plugin built on an SDK declaring v6 reports figures on its
        // heartbeat; a sidecar at v5 would drop them unread. Refused at the
        // door, in the fixture's words.
        assert_eq!(
            admit_within(2, 5, "v6"),
            Err(
                "the plugin was built against contract v6, newer than this sidecar \
                 (v2 through v5); upgrade the runtime, or rebuild the plugin against \
                 v5 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_receiving_typed_deliveries_is_refused_by_a_sidecar_at_v6() {
        // A plugin built on an SDK declaring v7 opens Receive, reads within
        // a scope and names its statement's external account; a sidecar at
        // v6 has no Receive and would read its statement's account from its
        // rows. Refused at the door, in the fixture's words.
        assert_eq!(
            admit_within(2, 6, "v7"),
            Err(
                "the plugin was built against contract v7, newer than this sidecar \
                 (v2 through v6); upgrade the runtime, or rebuild the plugin against \
                 v6 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_writing_the_book_is_refused_by_a_sidecar_at_v7() {
        // A plugin built on an SDK declaring v8 writes, reads and hears the
        // book and acts on its refusal codes; a sidecar at v7 has none of its
        // operations and carries no code from a component. Refused at the
        // door, in the fixture's words.
        assert_eq!(
            admit_within(2, 7, "v8"),
            Err(
                "the plugin was built against contract v8, newer than this sidecar \
                 (v2 through v7); upgrade the runtime, or rebuild the plugin against \
                 v7 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_reading_the_books_fields_and_its_delegation_is_refused_by_a_sidecar_at_v8() {
        // A plugin built on an SDK declaring v9 reads the book's INCOMPLETE
        // refusal with its fields and the delegation in its caller's claims;
        // a sidecar at v8 carries no fields and stamps no delegation.
        // Refused at the door, in the fixture's words.
        assert_eq!(
            admit_within(2, 8, "v9"),
            Err(
                "the plugin was built against contract v9, newer than this sidecar \
                 (v2 through v8); upgrade the runtime, or rebuild the plugin against \
                 v8 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_stating_what_its_source_says_is_refused_by_a_sidecar_at_v9() {
        // A plugin built on an SDK declaring v10 states what its source says
        // of a security on its resolve and reads a record's sources and the
        // book's refusal of an incomplete instrument record; a sidecar at v9
        // carries none of it. Refused at the door, in the fixture's words.
        assert_eq!(
            admit_within(2, 9, "v10"),
            Err(
                "the plugin was built against contract v10, newer than this sidecar \
                 (v2 through v9); upgrade the runtime, or rebuild the plugin against \
                 v9 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_version_is_read_as_its_number() {
        assert_eq!(declared("v6"), 6);
        assert_eq!(declared("v7"), 7);
        assert_eq!(declared("seven"), 0);
    }

    #[test]
    fn a_range_refuses_on_both_sides_of_it() {
        assert!(admit_within(2, 3, "v1").unwrap_err().contains("older than"));
        assert!(admit_within(2, 3, "v4").unwrap_err().contains("newer than"));
        assert!(admit_within(2, 3, "v1")
            .unwrap_err()
            .contains("v2 through v3"));
    }

    #[test]
    fn a_contract_older_than_the_floor_is_refused_naming_both_halves() {
        assert_eq!(
            admit("v1"),
            Err(
                "the plugin was built against contract v1, older than this sidecar accepts \
                 (v2 through v15); rebuild it against v2 or later"
                    .into()
            )
        );
    }

    #[test]
    fn a_contract_newer_than_this_sidecar_is_refused_naming_both_halves() {
        assert_eq!(
            admit("v16"),
            Err(
                "the plugin was built against contract v16, newer than this sidecar \
                 (v2 through v15); upgrade the runtime, or rebuild the plugin against \
                 v15 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_that_declares_nothing_is_refused() {
        assert_eq!(
            admit(""),
            Err(
                "the plugin declared no contract version; this sidecar accepts v2 through v15"
                    .into()
            )
        );
    }

    #[test]
    fn a_version_not_of_the_form_is_refused_rather_than_guessed_at() {
        for malformed in ["1", "version-1", "v", "v1.0", "V1", "v-1"] {
            assert!(admit(malformed).is_err(), "{malformed} was admitted");
        }
    }
}
