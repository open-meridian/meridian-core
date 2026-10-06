use super::*;
use crate::session::ABSOLUTE_NS;

const T0: i64 = 1_790_380_800_000_000_000;
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
/// RFC 7636, appendix B: the challenge for the verifier above.
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

#[test]
fn the_rfc_7636_example_verifies() {
    assert!(verifies(VERIFIER, CHALLENGE));
    assert!(!verifies(
        "not-the-verifier-not-the-verifier-not-the-ver",
        CHALLENGE
    ));
    assert!(!verifies("short", CHALLENGE), "RFC 7636 wants 43 to 128");
}

#[test]
fn a_moment_is_written_as_rfc_3339() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(T0), "2026-09-26T00:00:00Z");
    assert_eq!(rfc3339(951_782_400 * SECOND_NS), "2000-02-29T00:00:00Z");
    assert_eq!(
        rfc3339(T0 + ABSOLUTE_NS + 61 * SECOND_NS + 999_999_999),
        "2026-09-26T12:01:01Z"
    );
}
