//! The dashboard's own signing key, and the assertions it signs with it.
//!
//! A person reaches a plugin through the dashboard, which signs, per request,
//! who they are and what they hold on that plugin; the plugin's sidecar
//! verifies the signature before anything reaches the plugin
//! (decisions/014). The key is the dashboard's and nobody else's -- never the
//! deployment's, which only the conductor holds.
//!
//! Made by a Job the chart runs (plans/a-person-reaches-a-plugin, ruling 4),
//! with the pattern first run uses: the chart makes an empty Secret and an
//! empty ConfigMap so a Role can name them, and the Job patches them by name,
//! creates nothing, and gives its rights up (decisions/016). The private half
//! goes to the Secret only the dashboard mounts; the public half to the
//! ConfigMap every sidecar mounts, named by its key id, so a rotation is a new
//! file beside the old one.
//!
//! Read when first needed and kept once found, by the dashboard and by each
//! sidecar alike, so neither has to restart when the Job finishes -- and the
//! Job needs no right to restart anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::{Signer as _, SigningKey};
use meridian_first_run::cluster::Cluster;
use meridian_pb::v1::{CallerAssertion, CallerClaims};
use prost::Message;

/// In the Secret: which key this is, and the key.
pub const KEY_ID: &str = "key-id";
pub const PRIVATE_KEY: &str = "private-key.pem";

/// A key pair, freshly made.
pub struct KeyPair {
    pub key_id: String,
    pub private_pem: String,
    pub public_pem: String,
}

/// A new key, named for when it was made so a rotation's two keys are told
/// apart at a glance, and made unique by what follows the date.
pub fn generate(now_ns: i64) -> Result<KeyPair, String> {
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let private_pem = key
        .to_pkcs8_pem(Default::default())
        .map_err(|failed| format!("the key could not be written: {failed}"))?
        .to_string();
    let public_pem = key
        .verifying_key()
        .to_public_key_pem(Default::default())
        .map_err(|failed| format!("the public half could not be written: {failed}"))?;
    let seconds = now_ns / 1_000_000_000;
    let days = seconds.div_euclid(86_400);
    let (year, month) = year_month(days);
    let mut unique = [0u8; 4];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut unique);
    let key_id = format!(
        "dashboard-{year:04}-{month:02}-{}",
        unique
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    Ok(KeyPair {
        key_id,
        private_pem,
        public_pem,
    })
}

/// Days since 1970 to a civil year and month, UTC.
fn year_month(days: i64) -> (i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month)
}

/// What the Job did.
#[derive(Debug, PartialEq, Eq)]
pub enum Provisioned {
    /// A key was made, under this id.
    Made(String),
    /// The Secret held a key already; nothing was touched.
    Kept,
}

/// Make the dashboard's key if it has none, and give up the rights to.
///
/// The public half is written before the private one, so there is no moment
/// when the dashboard signs with a key no sidecar can verify. And the Job's
/// rights go whatever happened: a Job that failed still holds them otherwise.
pub async fn provision(
    cluster: &dyn Cluster,
    secret: &str,
    config_map: &str,
    binding: &str,
    now_ns: i64,
) -> Result<Provisioned, String> {
    let outcome: Result<Provisioned, String> = async {
        if cluster
            .secret_has_key(secret, KEY_ID)
            .await
            .map_err(|failed| failed.0)?
        {
            return Ok(Provisioned::Kept);
        }
        let pair = generate(now_ns)?;
        cluster
            .put_config_map(
                config_map,
                &BTreeMap::from([(format!("{}.pem", pair.key_id), pair.public_pem)]),
            )
            .await
            .map_err(|failed| failed.0)?;
        cluster
            .put_secret(
                secret,
                &BTreeMap::from([
                    (KEY_ID.to_string(), pair.key_id.clone().into_bytes()),
                    (PRIVATE_KEY.to_string(), pair.private_pem.into_bytes()),
                ]),
            )
            .await
            .map_err(|failed| failed.0)?;
        Ok(Provisioned::Made(pair.key_id))
    }
    .await;
    let dropped = cluster.drop_own_rights(binding).await;
    let outcome = outcome?;
    dropped.map_err(|failed| {
        format!(
            "the key was made, but the rights were not given up: {}",
            failed.0
        )
    })?;
    Ok(outcome)
}

/// The key the dashboard signs with, read from its mounted Secret when first
/// needed and kept once found.
pub struct Signer {
    dir: PathBuf,
    held: RwLock<Option<(String, SigningKey)>>,
}

impl Signer {
    pub fn at(dir: impl Into<PathBuf>) -> Signer {
        Signer {
            dir: dir.into(),
            held: RwLock::new(None),
        }
    }

    /// A signer holding a key already, for tests.
    pub fn holding(key_id: &str, key: SigningKey) -> Signer {
        Signer {
            dir: PathBuf::new(),
            held: RwLock::new(Some((key_id.to_string(), key))),
        }
    }

    fn read(dir: &Path) -> Result<(String, SigningKey), String> {
        let key_id = std::fs::read_to_string(dir.join(KEY_ID))
            .map_err(|failed| format!("no signing key yet ({failed})"))?
            .trim()
            .to_string();
        let pem = std::fs::read_to_string(dir.join(PRIVATE_KEY))
            .map_err(|failed| format!("no signing key yet ({failed})"))?;
        let key = SigningKey::from_pkcs8_pem(&pem)
            .map_err(|failed| format!("the signing key does not read: {failed}"))?;
        Ok((key_id, key))
    }

    /// Sign these claims, or say why there is nothing to sign with yet.
    pub fn sign(&self, claims: &CallerClaims) -> Result<CallerAssertion, String> {
        if self.held.read().expect("signer lock poisoned").is_none() {
            let read = Signer::read(&self.dir)?;
            *self.held.write().expect("signer lock poisoned") = Some(read);
        }
        let held = self.held.read().expect("signer lock poisoned");
        let (key_id, key) = held.as_ref().expect("read above");
        let claims = claims.encode_to_vec();
        Ok(CallerAssertion {
            signature: key.sign(&claims).to_bytes().to_vec(),
            claims,
            key_id: key_id.clone(),
        })
    }
}

#[cfg(test)]
mod tests;
