//! Identifiers the configuration store mints.
//!
//! The same shape the platform mints: a prefix, forty-eight bits of
//! milliseconds and eighty of randomness, in Crockford's base32. Written here
//! in thirty lines rather than shared, because the alphabet and the layout are
//! the whole of it and a shared crate between a public runtime and a private
//! control plane is a dependency in the wrong direction.
//!
//! Time first, so identifiers sort by creation. A page of them reads in order
//! and an index on them stays well behaved.

use rand::RngCore;

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

pub fn mint(prefix: &str, now_ms: u64) -> String {
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

pub fn account(now_ns: i64) -> String {
    mint("ACC", millis(now_ns))
}

pub fn user_group(now_ns: i64) -> String {
    mint("UG", millis(now_ns))
}

pub fn account_group(now_ns: i64) -> String {
    mint("AG", millis(now_ns))
}

pub fn access_group(now_ns: i64) -> String {
    mint("AX", millis(now_ns))
}

pub fn permission(now_ns: i64) -> String {
    mint("PRM", millis(now_ns))
}

fn millis(now_ns: i64) -> u64 {
    (now_ns.max(0) / 1_000_000) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_names_what_it_identifies() {
        let minted = account(1_757_376_000_000_000_000);
        assert!(minted.starts_with("ACC-"));
        assert_eq!(minted.len(), "ACC-".len() + 26);
        assert!(minted[4..].bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn two_minted_in_the_same_millisecond_are_still_different() {
        let now = 1_757_376_000_000_000_000;
        assert_ne!(permission(now), permission(now));
    }
}
