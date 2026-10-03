//! The IDs the instrument store mints for the deployment's records.
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
//! `LCL-`: the deployment's own ID for a record nothing matched (W3.7), its
//! key for life (decisions/030). The prefix says the deployment minted it, and
//! nothing about whether it is resolved: a global ID the platform holds joins
//! the record as an identifier and never replaces this.

use rand::RngCore;

/// What every ID this store mints begins with.
pub const LOCAL_PREFIX: &str = "LCL-";

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

/// A new ID for a record minted now.
pub fn local(now_ns: i64) -> String {
    mint("LCL", millis(now_ns))
}

fn millis(now_ns: i64) -> u64 {
    (now_ns.max(0) / 1_000_000) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_id_has_the_shape_every_minted_identifier_has() {
        // The prefix, then ten characters of time and sixteen of randomness.
        // The fixtures' IDs are shorter and are illustrations, not mints.
        let minted = local(1_757_376_000_000_000_000);
        assert!(minted.starts_with(LOCAL_PREFIX));
        assert_eq!(minted.len(), LOCAL_PREFIX.len() + 26);
        assert!(minted[4..].bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn local_ids_sort_by_when_they_were_minted() {
        let earlier = local(1_757_376_000_000_000_000);
        let later = local(1_757_376_001_000_000_000);
        assert!(earlier < later);
    }

    #[test]
    fn two_minted_in_the_same_millisecond_are_still_different() {
        let now = 1_757_376_000_000_000_000;
        assert_ne!(local(now), local(now));
    }
}
