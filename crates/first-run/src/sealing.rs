//! How a credential crosses the bus without the bus seeing it.
//!
//! The wizard collects database passwords, an LDAP bind password and a
//! directory's client secret, and hands them to the first-run Job to test and
//! to write. The bus carries them, and the bus is a broker with a credential
//! of its own on a pod network that is plaintext unless the cluster encrypts
//! it. Ruled 2026-09-22: seal each credential to the Job instead of trusting
//! the transport, so nothing between the two sees anything but ciphertext.
//!
//! HPKE (RFC 9180) in its base mode: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256
//! and ChaCha20-Poly1305. The Job makes its key pair in memory when it starts
//! and never writes it down, so a Job that restarts has a new one and anything
//! sealed to the old one is refused rather than silently unreadable.
//!
//! Each credential is sealed with the name of the field it fills as associated
//! data. A sealed database password cannot be moved into the LDAP bind
//! password's place: the open fails, because the name no longer matches.

use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem as KemTrait, OpModeR, OpModeS, Serializable};
use meridian_domain::v1::SealedCredential;
use rand::rngs::OsRng;

type Kem = X25519HkdfSha256;
type Aead = ChaCha20Poly1305;
type Kdf = HkdfSha256;

/// What the empty info string is: HPKE's per-exchange context, which this does
/// not need because every seal names its field in the associated data.
const INFO: &[u8] = b"meridian first run";

/// The Job's key pair, made when it starts and gone when it stops.
pub struct SealingKey {
    private: <Kem as KemTrait>::PrivateKey,
    public: <Kem as KemTrait>::PublicKey,
    /// Names this pair, so a credential says which one it was sealed to and a
    /// Job that has restarted can say so rather than failing to decrypt.
    pub key_id: String,
}

impl SealingKey {
    pub fn new(key_id: impl Into<String>) -> Self {
        let (private, public) = Kem::gen_keypair(&mut OsRng);
        Self {
            private,
            public,
            key_id: key_id.into(),
        }
    }

    /// The half the dashboard seals to, as the contract carries it.
    pub fn public_key(&self) -> Vec<u8> {
        self.public.to_bytes().to_vec()
    }

    /// Open what was sealed to this key, for the field it was sealed for.
    ///
    /// Refuses a credential sealed to another key by name, because that is a
    /// restarted Job rather than an attack and the answer is "seal it again".
    pub fn open(&self, sealed: &SealedCredential, field: &str) -> Result<Vec<u8>, String> {
        if sealed.key_id != self.key_id {
            return Err(format!(
                "this credential was sealed to {}, and this Job holds {}: seal it again",
                sealed.key_id, self.key_id
            ));
        }

        let encapped = <Kem as KemTrait>::EncappedKey::from_bytes(&sealed.encapsulated_key)
            .map_err(|failed| format!("the sealed credential's key is unreadable: {failed}"))?;

        hpke::single_shot_open::<Aead, Kdf, Kem>(
            &OpModeR::Base,
            &self.private,
            &encapped,
            INFO,
            &sealed.ciphertext,
            field.as_bytes(),
        )
        .map_err(|failed| format!("the credential for {field} would not open: {failed}"))
    }
}

/// Seal one credential to the Job, for one field.
///
/// The field name travels as associated data rather than in the message, so
/// moving a sealed value into another field breaks the seal instead of
/// quietly filling the wrong one.
pub fn seal(
    public_key: &[u8],
    key_id: &str,
    field: &str,
    secret: &[u8],
) -> Result<SealedCredential, String> {
    let recipient = <Kem as KemTrait>::PublicKey::from_bytes(public_key)
        .map_err(|failed| format!("that is not a sealing key: {failed}"))?;

    let (encapped, ciphertext) = hpke::single_shot_seal::<Aead, Kdf, Kem, _>(
        &OpModeS::Base,
        &recipient,
        INFO,
        secret,
        field.as_bytes(),
        &mut OsRng,
    )
    .map_err(|failed| format!("the credential for {field} could not be sealed: {failed}"))?;

    Ok(SealedCredential {
        key_id: key_id.to_string(),
        encapsulated_key: encapped.to_bytes().to_vec(),
        ciphertext,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_sealed_to_a_job_opens_only_there() {
        let job = SealingKey::new("frk-1");
        let sealed = seal(
            &job.public_key(),
            &job.key_id,
            "runtime_database.serving.password",
            b"hunter2",
        )
        .unwrap();

        // The bus carries this, and this is all it carries.
        assert_ne!(sealed.ciphertext, b"hunter2");
        assert!(!sealed.encapsulated_key.is_empty());

        assert_eq!(
            job.open(&sealed, "runtime_database.serving.password")
                .unwrap(),
            b"hunter2"
        );
    }

    #[test]
    fn another_job_cannot_open_it() {
        let job = SealingKey::new("frk-1");
        let sealed = seal(&job.public_key(), &job.key_id, "field", b"hunter2").unwrap();

        let stranger = SealingKey {
            key_id: "frk-1".into(),
            ..SealingKey::new("frk-1")
        };
        assert!(stranger.open(&sealed, "field").is_err());
    }

    #[test]
    fn a_credential_cannot_be_moved_into_another_field() {
        let job = SealingKey::new("frk-1");
        let sealed = seal(
            &job.public_key(),
            &job.key_id,
            "ldap.bind_password",
            b"hunter2",
        )
        .unwrap();

        let moved = job.open(&sealed, "runtime_database.serving.password");

        assert!(
            moved.is_err(),
            "the field is the associated data, so moving one breaks the seal"
        );
    }

    #[test]
    fn a_restarted_job_says_to_seal_it_again() {
        let before = SealingKey::new("frk-1");
        let sealed = seal(&before.public_key(), &before.key_id, "field", b"hunter2").unwrap();

        let after = SealingKey::new("frk-2");
        let refusal = after.open(&sealed, "field").expect_err("a new pair");

        assert!(refusal.contains("seal it again"), "{refusal}");
    }
}
