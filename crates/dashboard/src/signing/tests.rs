use std::collections::BTreeMap;
use std::sync::Mutex;

use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use meridian_first_run::cluster::{Cluster, ClusterError, Workload};

use super::*;

const T0: i64 = 1_790_380_800_000_000_000;

/// A cluster that keeps what it is told, in order, and can refuse one call.
#[derive(Default)]
struct Kept {
    done: Mutex<Vec<String>>,
    secret: Mutex<BTreeMap<String, Vec<u8>>>,
    config: Mutex<BTreeMap<String, String>>,
    refuse_secret: bool,
}

#[async_trait::async_trait]
impl Cluster for Kept {
    async fn put_secret(
        &self,
        name: &str,
        values: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), ClusterError> {
        if self.refuse_secret {
            return Err(ClusterError("forbidden".into()));
        }
        self.done.lock().unwrap().push(format!("secret {name}"));
        self.secret.lock().unwrap().extend(values.clone());
        Ok(())
    }
    async fn put_config_map(
        &self,
        name: &str,
        values: &BTreeMap<String, String>,
    ) -> Result<(), ClusterError> {
        self.done.lock().unwrap().push(format!("config-map {name}"));
        self.config.lock().unwrap().extend(values.clone());
        Ok(())
    }
    async fn scale(&self, _: Workload, _: &str, _: u32) -> Result<(), ClusterError> {
        unreachable!("the key Job scales nothing")
    }
    async fn restart(&self, _: &str) -> Result<(), ClusterError> {
        unreachable!("the key Job restarts nothing")
    }
    async fn drop_own_rights(&self, binding: &str) -> Result<(), ClusterError> {
        self.done.lock().unwrap().push(format!("dropped {binding}"));
        Ok(())
    }
    async fn secret_has_key(&self, _: &str, key: &str) -> Result<bool, ClusterError> {
        Ok(self.secret.lock().unwrap().contains_key(key))
    }
}

fn claims() -> CallerClaims {
    CallerClaims {
        subject: "local|ada".into(),
        display_name: "Ada".into(),
        audience_instance_id: "reference-1".into(),
        access: vec![],
        issued_at_ns: T0,
        expires_at_ns: T0 + 60_000_000_000,
        assertion_id: "a-1".into(),
    }
}

#[tokio::test]
async fn a_key_is_made_public_half_first_and_the_rights_given_up() {
    let cluster = Kept::default();
    let made = provision(&cluster, "d-signing", "d-keys", "d-key-job", T0)
        .await
        .unwrap();
    let Provisioned::Made(key_id) = made else {
        panic!("{made:?}")
    };
    assert!(key_id.starts_with("dashboard-2026-09-"), "{key_id}");
    assert_eq!(
        *cluster.done.lock().unwrap(),
        ["config-map d-keys", "secret d-signing", "dropped d-key-job"],
        "no moment when the dashboard signs with a key no sidecar holds"
    );
    assert!(cluster
        .config
        .lock()
        .unwrap()
        .contains_key(&format!("{key_id}.pem")));
    assert_eq!(cluster.secret.lock().unwrap()[KEY_ID], key_id.as_bytes());
}

#[tokio::test]
async fn a_key_already_made_is_kept_and_the_rights_still_given_up() {
    let cluster = Kept::default();
    provision(&cluster, "s", "c", "b", T0).await.unwrap();
    let before = cluster.secret.lock().unwrap().clone();
    assert_eq!(
        provision(&cluster, "s", "c", "b", T0).await.unwrap(),
        Provisioned::Kept
    );
    assert_eq!(*cluster.secret.lock().unwrap(), before, "untouched");
    assert_eq!(cluster.done.lock().unwrap().last().unwrap(), "dropped b");
}

#[tokio::test]
async fn a_failed_write_still_gives_the_rights_up() {
    let cluster = Kept {
        refuse_secret: true,
        ..Default::default()
    };
    assert!(provision(&cluster, "s", "c", "b", T0).await.is_err());
    assert_eq!(cluster.done.lock().unwrap().last().unwrap(), "dropped b");
}

#[test]
fn what_is_signed_verifies_with_the_public_half_and_nothing_else() {
    let pair = generate(T0).unwrap();
    let key = SigningKey::from_pkcs8_pem(&pair.private_pem).unwrap();
    let assertion = Signer::holding(&pair.key_id, key).sign(&claims()).unwrap();
    assert_eq!(assertion.key_id, pair.key_id);
    assert_eq!(
        CallerClaims::decode(assertion.claims.as_slice()).unwrap(),
        claims()
    );

    let public = VerifyingKey::from_public_key_pem(&pair.public_pem).unwrap();
    let signature = Signature::from_slice(&assertion.signature).unwrap();
    assert!(public.verify(&assertion.claims, &signature).is_ok());

    let mut altered = assertion.claims.clone();
    altered[3] ^= 1;
    assert!(
        public.verify(&altered, &signature).is_err(),
        "a changed claim does not verify"
    );
    let other = VerifyingKey::from_public_key_pem(&generate(T0).unwrap().public_pem).unwrap();
    assert!(
        other.verify(&assertion.claims, &signature).is_err(),
        "nor another key"
    );
}

#[test]
fn the_key_is_read_once_it_is_there_and_not_before() {
    let dir = std::env::temp_dir().join(format!("meridian-signing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let signer = Signer::at(&dir);
    assert!(signer
        .sign(&claims())
        .unwrap_err()
        .contains("no signing key yet"));

    let pair = generate(T0).unwrap();
    std::fs::write(dir.join(KEY_ID), format!("{}\n", pair.key_id)).unwrap();
    std::fs::write(dir.join(PRIVATE_KEY), &pair.private_pem).unwrap();
    assert_eq!(signer.sign(&claims()).unwrap().key_id, pair.key_id);
}
