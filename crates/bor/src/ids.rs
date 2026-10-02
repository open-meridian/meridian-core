//! Identifiers the book mints: an entry's, a lot's and a break's.
//!
//! The shape the street store and the platform mint: a prefix, forty-eight
//! bits of milliseconds and eighty of randomness, in Crockford's base32, so
//! they sort by when they were minted. Written here rather than shared with
//! the street store, which would make one store's crate a dependency of the
//! other's (decisions/012: the book and the street share nothing).

use rand::RngCore;

/// No I, L, O or U, so an identifier read aloud cannot become another.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn encode(mut value: u128, length: usize) -> String {
    let mut out = vec![0u8; length];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(value & 0x1F) as usize];
        value >>= 5;
    }
    String::from_utf8(out).expect("the alphabet is ascii")
}

/// `prefix-` and twenty-six characters, time first.
pub fn mint(prefix: &str, now_ns: i64) -> String {
    let mut random = [0u8; 10];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let mut padded = [0u8; 16];
    padded[6..].copy_from_slice(&random);
    let millis = (now_ns.max(0) / 1_000_000) as u128;
    format!(
        "{prefix}-{}{}",
        encode(millis, 10),
        encode(u128::from_be_bytes(padded), 16)
    )
}

pub const ENTRY: &str = "ENT";
pub const LOT: &str = "LOT";
pub const BREAK: &str = "BRK";

/// What a placeholder's instrument ID begins with: the instrument store mints
/// them (W3.7), and the book follows each one's replacement (W9.9).
pub const PLACEHOLDER_PREFIX: &str = "LCL-";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_has_its_prefix_and_sorts_by_time() {
        let earlier = mint(ENTRY, 1_757_376_000_000_000_000);
        let later = mint(ENTRY, 1_757_376_001_000_000_000);
        assert!(earlier.starts_with("ENT-"));
        assert_eq!(earlier.len(), "ENT-".len() + 26);
        assert!(earlier < later);
        assert_ne!(mint(LOT, 1), mint(LOT, 1));
    }
}
