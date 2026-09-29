//! Secret settings, sealed before the configuration store writes them.
//!
//! A setting a plugin declares secret is encrypted with the deployment's
//! settings key before it reaches the database, and opened only here, to be
//! delivered to that plugin's sidecar (spec/deployment-dashboard-and-access,
//! requirement 35 as amended 2026-09-28). A dump or a backup of the database
//! alone reveals no secret.
//!
//! # The key
//!
//! Thirty-two random bytes in a Secret the chart creates once and only the
//! conductor mounts. Made inside the cluster by a Job, the way the dashboard's
//! signing key is ([`provision`]): the chart makes the Secret empty so a Role
//! can name it, and the Job fills it, creates nothing, and gives its rights up
//! (decisions/016). An upgrade never replaces it; a Job that finds a key does
//! nothing else.
//!
//! Read when first needed and kept once found, so the conductor need not
//! restart when the Job finishes. Until there is one, a secret is refused
//! rather than stored readable, and every other setting still works.
//!
//! # The format
//!
//! One version byte, a 24-byte nonce drawn at random for every write, and the
//! XChaCha20-Poly1305 ciphertext with its tag. The plugin instance and the
//! setting's name are bound in as associated data, so a sealed value copied to
//! another row of the table does not open there.
//!
//! Nothing here implements `Debug` over a key or a value, and no error names
//! either.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use meridian_first_run::cluster::Cluster;
use rand::RngCore;

/// In the Secret, and so the file name where it is mounted.
pub const KEY_FILE: &str = "key";
pub const KEY_LEN: usize = 32;

const VERSION: u8 = 1;
const NONCE_LEN: usize = 24;
/// Separates the associated data from any other use this key could be put to.
const CONTEXT: &[u8] = b"meridian.plugin-setting.v1";

/// A new key.
pub fn generate() -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    key
}

/// The deployment's settings key, read from where the Secret is mounted when
/// first needed and kept once found.
pub struct SettingsKey {
    dir: Option<PathBuf>,
    held: RwLock<Option<XChaCha20Poly1305>>,
}

impl SettingsKey {
    /// The key in `dir`, once there is one.
    pub fn at(dir: impl Into<PathBuf>) -> SettingsKey {
        SettingsKey {
            dir: Some(dir.into()),
            held: RwLock::new(None),
        }
    }

    /// No key, and none to come: every secret is refused.
    pub fn none() -> SettingsKey {
        SettingsKey {
            dir: None,
            held: RwLock::new(None),
        }
    }

    /// A key already in hand, for tests.
    pub fn holding(key: &[u8; KEY_LEN]) -> SettingsKey {
        SettingsKey {
            dir: None,
            held: RwLock::new(Some(XChaCha20Poly1305::new(key.into()))),
        }
    }

    fn cipher(&self) -> Result<XChaCha20Poly1305, String> {
        if let Some(cipher) = self.held.read().expect("key lock poisoned").as_ref() {
            return Ok(cipher.clone());
        }
        let Some(dir) = &self.dir else {
            return Err("this conductor has no settings key".into());
        };
        let bytes = std::fs::read(dir.join(KEY_FILE)).map_err(|_| {
            "there is no settings key yet: the chart's settings-key Job makes it".to_string()
        })?;
        let key: [u8; KEY_LEN] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| format!("the settings key is not {KEY_LEN} bytes"))?;
        let cipher = XChaCha20Poly1305::new((&key).into());
        *self.held.write().expect("key lock poisoned") = Some(cipher.clone());
        Ok(cipher)
    }

    /// Whether a secret could be sealed now.
    pub fn available(&self) -> bool {
        self.cipher().is_ok()
    }

    /// Seal one plugin's secret setting.
    pub fn seal(&self, plugin: &str, name: &str, value: &str) -> Result<Vec<u8>, String> {
        let cipher = self.cipher()?;
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let aad = associated(plugin, name);
        let sealed = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| format!("setting {name} could not be sealed"))?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + sealed.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Open what [`seal`](Self::seal) made for the same plugin and setting.
    pub fn open(&self, plugin: &str, name: &str, sealed: &[u8]) -> Result<String, String> {
        let cipher = self.cipher()?;
        let refused = || {
            format!("setting {name} of {plugin} does not open with this deployment's settings key")
        };
        if sealed.len() < 1 + NONCE_LEN || sealed[0] != VERSION {
            return Err(refused());
        }
        let (nonce, body) = sealed[1..].split_at(NONCE_LEN);
        let aad = associated(plugin, name);
        let opened = cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: &aad,
                },
            )
            .map_err(|_| refused())?;
        String::from_utf8(opened).map_err(|_| refused())
    }
}

fn associated(plugin: &str, name: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(CONTEXT.len() + plugin.len() + name.len() + 2);
    aad.extend_from_slice(CONTEXT);
    aad.push(0);
    aad.extend_from_slice(plugin.as_bytes());
    aad.push(0);
    aad.extend_from_slice(name.as_bytes());
    aad
}

/// What the Job did.
#[derive(Debug, PartialEq, Eq)]
pub enum Provisioned {
    Made,
    /// The Secret held a key already; nothing was touched.
    Kept,
}

/// Make the settings key if there is none, and give up the rights to.
///
/// Never replaces one: every secret already stored was sealed with it. The
/// Job's rights go whatever happened, since a Job that failed still holds
/// them otherwise.
pub async fn provision(
    cluster: &dyn Cluster,
    secret: &str,
    binding: &str,
) -> Result<Provisioned, String> {
    let outcome: Result<Provisioned, String> = async {
        if cluster
            .secret_has_key(secret, KEY_FILE)
            .await
            .map_err(|failed| failed.0)?
        {
            return Ok(Provisioned::Kept);
        }
        cluster
            .put_secret(
                secret,
                &BTreeMap::from([(KEY_FILE.to_string(), generate().to_vec())]),
            )
            .await
            .map_err(|failed| failed.0)?;
        Ok(Provisioned::Made)
    }
    .await;
    let dropped = cluster.drop_own_rights(binding).await;
    let outcome = outcome?;
    dropped.map_err(|failed| {
        format!(
            "the settings key was made, but the rights were not given up: {}",
            failed.0
        )
    })?;
    Ok(outcome)
}

#[cfg(test)]
mod tests;
