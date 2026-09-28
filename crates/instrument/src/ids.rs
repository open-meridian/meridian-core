//! Identifiers the instrument store mints: placeholders, and nothing else.
//!
//! The same shape the platform mints: a prefix, forty-eight bits of
//! milliseconds and eighty of randomness, in Crockford's base32. Written here
//! in thirty lines rather than shared, because the alphabet and the layout are
//! the whole of it and a shared crate between a public runtime and a private
//! control plane is a dependency in the wrong direction. The street store
//! keeps its own copy for the same reason.
//!
//! Time first, so identifiers sort by creation. A page of them reads in order
//! and an index on them stays well behaved.
//!
//! Only `LCL-`. Identity is the platform's and is `INS-`; what this store may
//! mint is a stand-in for an identifier set nothing matched (W3.7), and the
//! prefix is what lets anybody reading one know it is waiting to be replaced.

use rand::RngCore;

/// What every placeholder begins with, and what an instrument ID the platform
/// minted before it minted only `INS-` began with too. See
/// [`crate::placeholder`] for why both are treated alike.
pub const PLACEHOLDER_PREFIX: &str = "LCL-";

/// No I, L, O or U, so an identifier read aloud or copied off a screen cannot
/// become a different valid one.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn encode(mut value: u128, length: usize) -> String {
    let mut out = vec![0u8; length];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(value & 0x1F) as usize];
        value >>= 5;
    }
    String::from_utf8(out).expect("the alphabet is ascii")
}

fn mint(prefix: &str, now_ms: u64) -> String {
    let mut random = [0u8; 10];
    rand::rngs::OsRng.fill_bytes(&mut random);

    let mut padded = [0u8; 16];
    padded[6..].copy_from_slice(&random);

    format!(
        "{prefix}-{}{}",
        encode(now_ms as u128, 10),
        encode(u128::from_be_bytes(padded), 16)
    )
}

pub fn placeholder(now_ns: i64) -> String {
    mint("LCL", millis(now_ns))
}

fn millis(now_ns: i64) -> u64 {
    (now_ns.max(0) / 1_000_000) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placeholder_has_the_shape_every_minted_identifier_has() {
        // The prefix, then ten characters of time and sixteen of randomness.
        // The fixtures' IDs are shorter and are illustrations, not mints.
        let minted = placeholder(1_757_376_000_000_000_000);
        assert!(minted.starts_with(PLACEHOLDER_PREFIX));
        assert_eq!(minted.len(), PLACEHOLDER_PREFIX.len() + 26);
        assert!(minted[4..].bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn placeholders_sort_by_when_they_were_minted() {
        let earlier = placeholder(1_757_376_000_000_000_000);
        let later = placeholder(1_757_376_001_000_000_000);
        assert!(earlier < later);
    }

    #[test]
    fn two_minted_in_the_same_millisecond_are_still_different() {
        let now = 1_757_376_000_000_000_000;
        assert_ne!(placeholder(now), placeholder(now));
    }
}
