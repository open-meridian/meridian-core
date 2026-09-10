//! Signing the note a deployment presents on every request.
//!
//! There is no session, no token and no password. Before each request the
//! deployment signs a short statement naming itself, who it is talking to, and
//! when, and the platform verifies it against a public key the operator
//! registered. Nothing worth stealing crosses the wire, and nothing worth
//! stealing sits at the platform.
//!
//! # Three rules, and each has a reason
//!
//! **Sixty seconds, at most.** The platform refuses a note claiming longer.
//! That bound is what makes a copy nearly worthless: a note found in a log an
//! hour later has been useless for fifty-nine minutes. A longer-lived note would
//! be a bearer token with extra steps.
//!
//! **The audience is the platform's stable address, never wherever the request
//! lands.** A regional audience is a one-way door. The moment a note says it was
//! made for one region, adding another means every deployment resigning with new
//! configuration, and the platform loses the freedom to route. The audience
//! names the platform, not the instance answering.
//!
//! **Nothing here logs a note, or a private key.** A log file is where a note
//! would realistically leak, rather than off an encrypted wire.
//!
//! # Why this was written before anything calls the platform
//!
//! The platform's own verifier was written the other way round: built, shipped,
//! and never executed until a person clicked sign in, where it failed. Two
//! seconds of clock difference did it. This module is tested before anything
//! depends on it, for that reason and no other.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use ed25519_dalek::pkcs8::{
    spki::der::pem::LineEnding, DecodePrivateKey, EncodePrivateKey, EncodePublicKey,
};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

/// The longest a note may claim to be good for.
///
/// Matches what the platform enforces. Kept as a constant rather than a
/// parameter with a large default, because a lifetime is the one setting where
/// a generous value looks harmless and is not.
pub const MAX_LIFETIME_SECONDS: u64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("the private key could not be read: {0}")]
    Key(String),

    #[error("a note may not claim more than {MAX_LIFETIME_SECONDS}s; asked for {asked}s")]
    LifetimeTooLong { asked: u64 },

    #[error("a note needs a deployment identifier and an audience")]
    Incomplete,
}

/// The key a deployment signs with. The private half never leaves this process.
pub struct DeploymentKey {
    signing: SigningKey,
}

impl std::fmt::Debug for DeploymentKey {
    /// Deliberately says nothing. A key that prints itself ends up in a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeploymentKey(private, not shown)")
    }
}

impl DeploymentKey {
    /// A new keypair. The operator registers the public half and keeps this.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
        Self {
            signing: SigningKey::from_bytes(&bytes),
        }
    }

    /// Load a key the operator generated elsewhere.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Self, SigningError> {
        SigningKey::from_pkcs8_pem(pem)
            .map(|signing| Self { signing })
            .map_err(|e| SigningError::Key(e.to_string()))
    }

    /// The public half, in the form the platform's registration form accepts.
    pub fn public_key_pem(&self) -> Result<String, SigningError> {
        self.signing
            .verifying_key()
            .to_public_key_pem(LineEnding::LF)
            .map_err(|e| SigningError::Key(e.to_string()))
    }

    /// The private half, for an operator storing a key this process generated.
    ///
    /// Deliberately awkward to reach and never called inside this crate. A
    /// deployment that generates its own key has to write it down somewhere
    /// once; everything after that reads it rather than exports it.
    pub fn private_key_pem(&self) -> Result<String, SigningError> {
        self.signing
            .to_pkcs8_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| SigningError::Key(e.to_string()))
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// A signed note for one request.
    ///
    /// `now_secs` is passed in rather than read, so a test controls time instead
    /// of waiting for it.
    pub fn note(
        &self,
        deployment_id: &str,
        audience: &str,
        now_secs: u64,
        lifetime_secs: u64,
    ) -> Result<String, SigningError> {
        if deployment_id.is_empty() || audience.is_empty() {
            return Err(SigningError::Incomplete);
        }
        if lifetime_secs > MAX_LIFETIME_SECONDS {
            return Err(SigningError::LifetimeTooLong {
                asked: lifetime_secs,
            });
        }

        let header = serde_json::json!({ "alg": "EdDSA", "typ": "JWT" });

        // `jti` is unique per note and the platform ignores it today. It is here
        // so that adding replay detection later is a change at the edge rather
        // than a change to what every deployment sends.
        let mut nonce = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

        let claims = serde_json::json!({
            "iss": deployment_id,
            "sub": deployment_id,
            "aud": audience,
            "iat": now_secs,
            "exp": now_secs + lifetime_secs,
            "jti": B64.encode(nonce),
        });

        let signing_input = format!(
            "{}.{}",
            B64.encode(header.to_string()),
            B64.encode(claims.to_string())
        );
        let signature = self.signing.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            B64.encode(signature.to_bytes())
        ))
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::Verifier;

    use super::*;

    const DEPLOYMENT: &str = "DEP-01M26TEST0000000000000001";
    const AUDIENCE: &str = "https://platform.example/api/v1/reference";

    fn parts(note: &str) -> (serde_json::Value, serde_json::Value) {
        let segments: Vec<&str> = note.split('.').collect();
        assert_eq!(segments.len(), 3, "a note has three segments");
        let decode =
            |s: &str| serde_json::from_slice::<serde_json::Value>(&B64.decode(s).unwrap()).unwrap();
        (decode(segments[0]), decode(segments[1]))
    }

    #[test]
    fn a_note_names_the_deployment_as_both_issuer_and_subject() {
        let key = DeploymentKey::generate();
        let note = key.note(DEPLOYMENT, AUDIENCE, 1_000, 30).unwrap();
        let (_, claims) = parts(&note);
        assert_eq!(claims["iss"], DEPLOYMENT);
        assert_eq!(claims["sub"], DEPLOYMENT);
        assert_eq!(claims["aud"], AUDIENCE);
    }

    #[test]
    fn the_algorithm_is_named_and_is_a_public_key_one() {
        // A shared-secret family here would mean the public key anybody can read
        // doubles as the verification secret.
        let key = DeploymentKey::generate();
        let (header, _) = parts(&key.note(DEPLOYMENT, AUDIENCE, 1_000, 30).unwrap());
        assert_eq!(header["alg"], "EdDSA");
    }

    #[test]
    fn the_signature_verifies_against_the_public_half() {
        let key = DeploymentKey::generate();
        let note = key.note(DEPLOYMENT, AUDIENCE, 1_000, 30).unwrap();
        let (input, signature) = note.rsplit_once('.').unwrap();
        let bytes: [u8; 64] = B64.decode(signature).unwrap().try_into().unwrap();
        key.verifying_key()
            .verify(
                input.as_bytes(),
                &ed25519_dalek::Signature::from_bytes(&bytes),
            )
            .expect("the note must verify against the key that signed it");
    }

    #[test]
    fn another_keys_public_half_does_not_verify_it() {
        let key = DeploymentKey::generate();
        let stranger = DeploymentKey::generate();
        let note = key.note(DEPLOYMENT, AUDIENCE, 1_000, 30).unwrap();
        let (input, signature) = note.rsplit_once('.').unwrap();
        let bytes: [u8; 64] = B64.decode(signature).unwrap().try_into().unwrap();
        assert!(stranger
            .verifying_key()
            .verify(
                input.as_bytes(),
                &ed25519_dalek::Signature::from_bytes(&bytes)
            )
            .is_err());
    }

    #[test]
    fn expiry_follows_the_moment_it_was_issued() {
        let key = DeploymentKey::generate();
        let (_, claims) = parts(&key.note(DEPLOYMENT, AUDIENCE, 5_000, 45).unwrap());
        assert_eq!(claims["iat"], 5_000);
        assert_eq!(claims["exp"], 5_045);
    }

    #[test]
    fn a_note_claiming_longer_than_a_minute_is_refused_here_not_there() {
        // The platform refuses it too. Refusing at the source turns a remote
        // rejection nobody can read into a local error naming the number.
        let key = DeploymentKey::generate();
        let asked = MAX_LIFETIME_SECONDS + 1;
        match key.note(DEPLOYMENT, AUDIENCE, 1_000, asked) {
            Err(SigningError::LifetimeTooLong { asked: reported }) => {
                assert_eq!(reported, asked)
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn two_notes_for_the_same_second_are_still_different() {
        // So that adding replay detection later is a change at the edge rather
        // than a change to what every deployment sends.
        let key = DeploymentKey::generate();
        let first = key.note(DEPLOYMENT, AUDIENCE, 1_000, 30).unwrap();
        let second = key.note(DEPLOYMENT, AUDIENCE, 1_000, 30).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn a_note_without_a_deployment_or_an_audience_is_refused() {
        let key = DeploymentKey::generate();
        assert!(matches!(
            key.note("", AUDIENCE, 1_000, 30),
            Err(SigningError::Incomplete)
        ));
        assert!(matches!(
            key.note(DEPLOYMENT, "", 1_000, 30),
            Err(SigningError::Incomplete)
        ));
    }

    #[test]
    fn the_public_half_exports_in_the_form_the_platform_accepts() {
        let key = DeploymentKey::generate();
        let pem = key.public_key_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert!(
            !pem.contains("PRIVATE"),
            "the private half must never be exported here"
        );
    }

    #[test]
    fn a_key_never_prints_itself() {
        // A key that prints itself ends up in a log.
        let key = DeploymentKey::generate();
        assert!(!format!("{key:?}").contains("signing"));
        assert_eq!(format!("{key:?}"), "DeploymentKey(private, not shown)");
    }
}
