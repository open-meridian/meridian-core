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

/// The contract this sidecar implements. v2 is typed operations, acting-for,
/// and the settings, access and scope streams (spec/typed-sidecar-operations).
pub const CONTRACT_CURRENT: u32 = 2;

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
/// Separate from [`admit`] so the rule can be tested over a range that spans
/// versions. Today the floor and the current version are both 1, so there is
/// no older version to admit and no way to prove the property this module
/// exists for using the real constants. The day typed operations raise the
/// current version to 2, `admit` starts exercising exactly what is proved
/// below.
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
    fn the_current_contract_is_admitted() {
        assert_eq!(admit("v2"), Ok(2));
    }

    #[test]
    fn an_older_contract_inside_the_range_is_admitted() {
        // The property this module exists for, and the one an equality check
        // fails: a plugin built against v1 keeps registering with a sidecar
        // that has moved on to v2. Under `declared == current` this is refused.
        assert_eq!(admit_within(1, 2, "v1"), Ok(1));
        assert_eq!(admit_within(1, 2, "v2"), Ok(2));
        assert_ne!("v1", "v2", "an equality check would have refused the first");
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
                 (v2 through v2); rebuild it against v2 or later"
                    .into()
            )
        );
    }

    #[test]
    fn a_contract_newer_than_this_sidecar_is_refused_naming_both_halves() {
        assert_eq!(
            admit("v3"),
            Err(
                "the plugin was built against contract v3, newer than this sidecar \
                 (v2 through v2); upgrade the runtime, or rebuild the plugin against \
                 v2 or earlier"
                    .into()
            )
        );
    }

    #[test]
    fn a_plugin_that_declares_nothing_is_refused() {
        assert_eq!(
            admit(""),
            Err(
                "the plugin declared no contract version; this sidecar accepts v2 through v2"
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
