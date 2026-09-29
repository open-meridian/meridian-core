use std::collections::BTreeMap;
use std::sync::Mutex;

use meridian_first_run::cluster::{Cluster, ClusterError, Workload};

use super::*;

/// Obviously not a real credential, and long enough to find in bytes.
const SECRET: &str = "sk-test-not-a-real-key-7f3a";

fn key() -> SettingsKey {
    SettingsKey::holding(&[7u8; KEY_LEN])
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn a_secret_opens_to_what_was_sealed_and_is_not_its_own_bytes() {
    let key = key();
    let sealed = key.seal("snaptrade-1", "api_key", SECRET).unwrap();
    assert!(
        !contains(&sealed, SECRET.as_bytes()),
        "the plaintext is not in what is stored"
    );
    assert_eq!(key.open("snaptrade-1", "api_key", &sealed).unwrap(), SECRET);
}

#[test]
fn the_same_secret_sealed_twice_is_stored_differently() {
    let key = key();
    let once = key.seal("snaptrade-1", "api_key", SECRET).unwrap();
    let twice = key.seal("snaptrade-1", "api_key", SECRET).unwrap();
    assert_ne!(once, twice, "a fresh nonce every write");
}

#[test]
fn a_sealed_value_moved_to_another_plugin_or_setting_does_not_open() {
    let key = key();
    let sealed = key.seal("snaptrade-1", "api_key", SECRET).unwrap();
    assert!(key.open("snaptrade-2", "api_key", &sealed).is_err());
    assert!(key.open("snaptrade-1", "user_secret", &sealed).is_err());
}

#[test]
fn another_key_or_an_altered_byte_does_not_open_and_says_nothing_of_the_value() {
    let sealed = key().seal("snaptrade-1", "api_key", SECRET).unwrap();
    let other = SettingsKey::holding(&[9u8; KEY_LEN]);
    let refused = other.open("snaptrade-1", "api_key", &sealed).unwrap_err();
    assert!(!refused.contains(SECRET));
    assert!(refused.contains("api_key"), "names the setting: {refused}");

    let mut altered = sealed.clone();
    let last = altered.len() - 1;
    altered[last] ^= 1;
    assert!(key().open("snaptrade-1", "api_key", &altered).is_err());
    assert!(key().open("snaptrade-1", "api_key", &sealed[..10]).is_err());
}

#[test]
fn with_no_key_a_secret_is_refused_rather_than_stored_readable() {
    let none = SettingsKey::none();
    assert!(!none.available());
    let refused = none.seal("snaptrade-1", "api_key", SECRET).unwrap_err();
    assert!(!refused.contains(SECRET));
}

#[test]
fn the_key_is_read_once_it_is_mounted_and_kept() {
    let dir = std::env::temp_dir().join(format!(
        "meridian-settings-key-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let key = SettingsKey::at(&dir);
    assert!(!key.available(), "before the Job has made it");

    std::fs::write(dir.join(KEY_FILE), [1u8; 5]).unwrap();
    let short = key.seal("p", "n", SECRET).unwrap_err();
    assert!(short.contains("32 bytes"), "{short}");

    std::fs::write(dir.join(KEY_FILE), generate()).unwrap();
    let sealed = key.seal("p", "n", SECRET).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        key.open("p", "n", &sealed).unwrap(),
        SECRET,
        "kept once found"
    );
}

/// A cluster that keeps what it is told, in order, and can refuse one call.
#[derive(Default)]
struct Kept {
    done: Mutex<Vec<String>>,
    secret: Mutex<BTreeMap<String, Vec<u8>>>,
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
        _: &str,
        _: &BTreeMap<String, String>,
    ) -> Result<(), ClusterError> {
        unreachable!("the settings key is published nowhere")
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

#[tokio::test]
async fn a_key_is_made_once_and_the_rights_given_up() {
    let cluster = Kept::default();
    assert_eq!(
        provision(&cluster, "settings-key", "key-job")
            .await
            .unwrap(),
        Provisioned::Made
    );
    assert_eq!(
        *cluster.done.lock().unwrap(),
        ["secret settings-key", "dropped key-job"]
    );
    let made = cluster.secret.lock().unwrap()[KEY_FILE].clone();
    assert_eq!(made.len(), KEY_LEN);

    assert_eq!(
        provision(&cluster, "settings-key", "key-job")
            .await
            .unwrap(),
        Provisioned::Kept
    );
    assert_eq!(
        cluster.secret.lock().unwrap()[KEY_FILE],
        made,
        "never replaced: every secret stored was sealed with it"
    );
    assert_eq!(
        cluster.done.lock().unwrap().last().unwrap(),
        "dropped key-job"
    );
}

#[tokio::test]
async fn a_failed_write_still_gives_the_rights_up() {
    let cluster = Kept {
        refuse_secret: true,
        ..Default::default()
    };
    assert!(provision(&cluster, "s", "b").await.is_err());
    assert_eq!(cluster.done.lock().unwrap().last().unwrap(), "dropped b");
}
