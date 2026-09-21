//! Zitadel's webhook signature, checked before a byte of the body is read.
//!
//! `ZITADEL-Signature: t=<unix seconds>,v1=<hex>[,v1=<hex>...]`, where each
//! `v1` is HMAC-SHA256 over `"<t>.<body>"` with the target's signing key.
//! Several `v1`s appear while a key is being rotated; any one matching is
//! enough. A timestamp more than 300 seconds old is refused, so a captured
//! call cannot be replayed later. Ported from Zitadel v4.17.3,
//! `pkg/actions/signing.go`, which is the other end of this.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const HEADER: &str = "zitadel-signature";
pub const TOLERANCE_S: i64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Unsigned,
    Malformed,
    TooOld,
    NoMatch,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Unsigned => "the call carries no Zitadel signature",
            Refusal::Malformed => "the Zitadel signature header is malformed",
            Refusal::TooOld => "the Zitadel signature is too old to accept",
            Refusal::NoMatch => "no Zitadel signature matches this body and key",
        })
    }
}

fn mac(key: &[u8], timestamp: i64, body: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// Accept the call only if one of its signatures is this key's over this body.
pub fn verify(header: Option<&str>, body: &[u8], key: &[u8], now_s: i64) -> Result<(), Refusal> {
    let header = header.filter(|h| !h.is_empty()).ok_or(Refusal::Unsigned)?;
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for pair in header.split(',') {
        let (name, value) = pair.split_once('=').ok_or(Refusal::Malformed)?;
        match name.trim() {
            "t" => {
                timestamp = Some(
                    value
                        .trim()
                        .parse::<i64>()
                        .map_err(|_| Refusal::Malformed)?,
                )
            }
            "v1" => {
                if let Ok(bytes) = hex::decode(value.trim()) {
                    signatures.push(bytes);
                }
            }
            _ => {}
        }
    }
    let timestamp = timestamp.ok_or(Refusal::Malformed)?;
    if signatures.is_empty() {
        return Err(Refusal::NoMatch);
    }
    if now_s - timestamp > TOLERANCE_S {
        return Err(Refusal::TooOld);
    }
    // Compared in constant time by the MAC itself.
    if signatures
        .iter()
        .any(|signature| mac(key, timestamp, body).verify_slice(signature).is_ok())
    {
        Ok(())
    } else {
        Err(Refusal::NoMatch)
    }
}

/// What Zitadel sends, for tests and for anyone checking this end by hand.
pub fn sign(key: &[u8], timestamp: i64, body: &[u8]) -> String {
    format!(
        "t={timestamp},v1={}",
        hex::encode(mac(key, timestamp, body).finalize().into_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_380_800;
    const KEY: &[u8] = b"98KmsU67";

    #[test]
    fn a_signature_zitadel_made_is_accepted() {
        let body = br#"{"fullMethod":"x"}"#;
        assert_eq!(verify(Some(&sign(KEY, NOW, body)), body, KEY, NOW), Ok(()));
    }

    #[test]
    fn a_changed_body_or_another_key_is_refused() {
        let header = sign(KEY, NOW, b"{}");
        assert_eq!(
            verify(Some(&header), b"{ }", KEY, NOW),
            Err(Refusal::NoMatch)
        );
        assert_eq!(
            verify(Some(&header), b"{}", b"other", NOW),
            Err(Refusal::NoMatch)
        );
    }

    #[test]
    fn an_unsigned_or_malformed_call_is_refused() {
        assert_eq!(verify(None, b"{}", KEY, NOW), Err(Refusal::Unsigned));
        assert_eq!(verify(Some(""), b"{}", KEY, NOW), Err(Refusal::Unsigned));
        assert_eq!(
            verify(Some("v1=abcd"), b"{}", KEY, NOW),
            Err(Refusal::Malformed)
        );
        assert_eq!(
            verify(Some(&format!("t={NOW}")), b"{}", KEY, NOW),
            Err(Refusal::NoMatch)
        );
    }

    #[test]
    fn a_replay_past_five_minutes_is_refused() {
        let header = sign(KEY, NOW, b"{}");
        assert_eq!(verify(Some(&header), b"{}", KEY, NOW + TOLERANCE_S), Ok(()));
        assert_eq!(
            verify(Some(&header), b"{}", KEY, NOW + TOLERANCE_S + 1),
            Err(Refusal::TooOld)
        );
    }

    #[test]
    fn during_a_key_rotation_either_signature_is_enough() {
        let old = sign(b"old-key", NOW, b"{}");
        let new = sign(KEY, NOW, b"{}");
        let v1_new = new.split_once(',').unwrap().1;
        let both = format!("{old},{v1_new}");
        assert_eq!(verify(Some(&both), b"{}", KEY, NOW), Ok(()));
        assert_eq!(verify(Some(&both), b"{}", b"old-key", NOW), Ok(()));
    }
}
