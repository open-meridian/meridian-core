//! What a client's credentials and the dashboard's own share: the person a
//! credential names, the store being away, PKCE's check and a token's hash.
//!
//! The terminal sessions from before delegations (W6.13's own sign-in, code
//! and session) are retired: the CLI connects by delegation since 0.1.25
//! (decisions/029; spec/clients-act-on-a-persons-delegation, requirement 22),
//! and from contract v15 the dashboard serves no older CLI
//! ([`crate::web::terminal::OLDEST_CLI`]) and keeps no terminal session
//! (dashboard migration 6). What remains here is what a delegation uses.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::clock::SECOND_NS;

/// Who a credential names: the person who signed in to grant it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub subject: String,
    pub display_name: String,
    pub directory_groups: Vec<String>,
    pub signed_in_at_ns: i64,
}

pub(crate) fn url_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

pub(crate) fn unreserved(byte: u8) -> bool {
    url_safe(byte) || byte == b'.' || byte == b'~'
}

/// RFC 7636's S256: the challenge is the verifier's SHA-256, base64url, as
/// delegations' authorisation codes check it ([`crate::delegation`]).
pub(crate) fn verifies(verifier: &str, challenge: &str) -> bool {
    if !(43..=128).contains(&verifier.len()) || !verifier.bytes().all(unreserved) {
        return false;
    }
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    same(computed.as_bytes(), challenge.as_bytes())
}

/// Equal, in a time that does not depend on where they differ.
pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// A token's key where it is kept: never the token itself.
pub fn hashed(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

/// The store could not be asked. Said as a 503, never as a refusal of the
/// credential: a CLI told its credential is unknown tells its person to sign
/// in again, which a database that is briefly away does not call for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the dashboard's store could not be read: {}", self.0)
    }
}

/// A moment as RFC 3339 in UTC, to the second. Here rather than a date
/// crate, because this is the one date the dashboard writes.
pub fn rfc3339(at_ns: i64) -> String {
    let seconds = at_ns.div_euclid(SECOND_NS);
    let (days, of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Howard Hinnant's days-from-civil, inverted.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

#[cfg(test)]
mod tests;
