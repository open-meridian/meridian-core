//! The registries against a stand-in platform: its catalogue, resolutions
//! and referrers made and signed here as the platform makes them.

use std::sync::Mutex;

use serde_json::{json, Value};

use super::verify::tests::TestSigner;
use super::verify::{LISTING_PREDICATE, METADATA_PREDICATE, PROVENANCE_PREDICATE};
use super::*;
use crate::MemoryStore;

pub(crate) const LISTING: &str = "open-meridian/snaptrade";
pub(crate) const FINGERPRINT: &str = "SHA256:tagkey";

pub(crate) fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

/// What the stand-in answers, changed by each test.
pub(crate) struct Platform {
    pub(crate) signer: TestSigner,
    pub(crate) catalogue: Mutex<Value>,
    pub(crate) resolution: Mutex<Value>,
    pub(crate) referrers: Mutex<Vec<String>>,
    pub(crate) asked: Mutex<Vec<String>>,
}

/// The stand-in plugin's one section, which its declaration names by
/// digest and size.
pub(crate) const OVERVIEW: &str = "Reads accounts.";

fn declaration() -> Value {
    use sha2::Digest;
    json!({"secret_settings": ["consumer_key"], "not_carried": [], "storage": null,
           "documentation": [{"name": "overview",
               "digest": format!("sha256:{}", hex::encode(sha2::Sha256::digest(OVERVIEW))),
               "bytes": OVERVIEW.len()}]})
}

pub(crate) fn metadata(roles: &[&str]) -> Value {
    json!({"name": "snaptrade", "version": "1.4.0", "roles": roles, "interface": true,
           "sdk_version": "0.29.0", "declaration": declaration()})
}

impl Platform {
    pub(crate) fn new() -> Arc<Platform> {
        let signer = TestSigner::new();
        let image = digest(0xab);
        let statement = signer.statement(
            LISTING_PREDICATE,
            &image,
            json!({"listing": LISTING, "version": "1.4.0", "registry": "community",
                   "listed_at": "2026-10-16T12:00:00Z", "signer_key": FINGERPRINT,
                   "roles": ["custody"], "floor": "v25"}),
        );
        let key_status = signer.sign_document(json!({
            "issued_at_ns": 1_792_000_000_000_000_000i64,
            "keys": [{"key_id": "SGK-1", "organisation": "open-meridian", "name": "release",
                      "public_key": "ssh-ed25519 AAAA", "fingerprint": FINGERPRINT,
                      "registered_at_ns": 1, "registered_by_person_id": "", "revoked_at_ns": 0,
                      "revoked_by_person_id": "", "revoke_reason": "", "not_trusted_after_ns": 0}],
        }));
        let catalogue = json!({
            "listings": [{
                "listing": LISTING,
                "repository": "https://github.com/open-meridian/plugin-snaptrade",
                "versions": [{
                    "listing": LISTING, "version": "1.4.0", "registry": "community",
                    "image_digest": image, "metadata": metadata(&["custody"]),
                    "approved_roles": [], "schema_version": "v25",
                    "build": {"signer_fingerprint": FINGERPRINT}, "listed_at_ns": 1,
                    "withdrawn_at_ns": 0, "sequence": 7, "listing_statement": statement,
                }],
                "regions": ["US"], "asset_classes": ["equity"], "support_contact": "",
                "stars": 2, "installs": 0, "instances": 0, "counts_shown": false,
            }],
            "sequence": 7,
            "key_status": key_status,
        });
        let resolution = json!({
            "image_digest": image,
            "pull_from": "ghcr.io/open-meridian/plugins/open-meridian/snaptrade",
            "pull_token": "", "pull_token_expires_at_ns": 0,
            "listing_statement": statement,
        });
        let referrers = vec![
            signer.statement(
                PROVENANCE_PREDICATE,
                &image,
                json!({"repository": "open-meridian/plugin-snaptrade", "tag": "v1.4.0",
                       "commit": "c".repeat(40), "signer": FINGERPRINT}),
            ),
            signer.statement(
                METADATA_PREDICATE,
                &image,
                json!({"metadata": metadata(&["custody"]), "schema_version": "v25",
                       "documentation": [{"name": "overview", "text": "Reads accounts."}]}),
            ),
        ];
        Arc::new(Platform {
            signer,
            catalogue: Mutex::new(catalogue),
            resolution: Mutex::new(resolution),
            referrers: Mutex::new(referrers),
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl Registries for Platform {
    fn pull_catalogue(&self, after: u64) -> Fut<'_, Result<Value, String>> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("catalogue after {after}"));
        let answer = self.catalogue.lock().unwrap().clone();
        Box::pin(async move { Ok(answer) })
    }
    fn resolve<'a>(
        &'a self,
        listing: &'a str,
        version: &'a str,
        registry: &'a str,
    ) -> Fut<'a, Result<Value, String>> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("resolve {listing} {version} {registry}"));
        let answer = self.resolution.lock().unwrap().clone();
        Box::pin(async move { Ok(answer) })
    }
    fn copy<'a>(
        &'a self,
        from: &'a str,
        _token: &'a str,
        digest: &'a str,
        repository: &'a str,
    ) -> Fut<'a, Result<Vec<String>, String>> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("copy {from}@{digest} into {repository}"));
        let answer = self.referrers.lock().unwrap().clone();
        Box::pin(async move { Ok(answer) })
    }
    fn referrers<'a>(
        &'a self,
        repository: &'a str,
        _digest: &'a str,
    ) -> Fut<'a, Result<Vec<String>, String>> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("referrers of {repository}"));
        let answer = self.referrers.lock().unwrap().clone();
        Box::pin(async move { Ok(answer) })
    }
    fn report_usage<'a>(&'a self, _report: &'a PluginUsageReport) -> Fut<'a, Result<bool, String>> {
        Box::pin(async move { Ok(true) })
    }
}

/// A call to the store from a test's own async body, as a blocking caller
/// makes it: the memory store, like Postgres's, refuses to block a runtime.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(work)
}

fn hold(platform: &Arc<Platform>) -> Hold {
    Hold::new(
        Arc::clone(platform) as Arc<dyn Registries>,
        platform.signer.keys(),
        Arc::new(MemoryStore::new()),
        Arc::new(At(1_792_200_000_000_000_000)),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalogue_is_pulled_after_its_sequence_and_kept_with_its_signed_key_status() {
    let platform = Platform::new();
    let held = hold(&platform);
    let kept = held.pull().await.unwrap();
    assert_eq!(kept.sequence, 7);
    assert_eq!(kept.listings[0].listing, LISTING);
    assert_eq!(kept.pulled_at_ns, 1_792_200_000_000_000_000);
    assert_eq!(
        blocking(|| held.key_status()).unwrap().keys[0].fingerprint,
        FINGERPRINT
    );
    // The next pull reads after what it holds; a listing changed replaces
    // the copy's whole.
    platform.catalogue.lock().unwrap()["listings"][0]["stars"] = json!(5);
    let kept = held.pull().await.unwrap();
    assert_eq!(kept.listings.len(), 1);
    assert_eq!(kept.listings[0].stars, 5);
    assert_eq!(platform.asked.lock().unwrap()[1], "catalogue after 7");
    // A key status signed by another key changes nothing.
    let forged = TestSigner::new().sign_document(json!({"issued_at_ns": 1, "keys": []}));
    platform.catalogue.lock().unwrap()["key_status"] = forged;
    platform.catalogue.lock().unwrap()["listings"][0]["stars"] = json!(9);
    assert!(held
        .pull()
        .await
        .unwrap_err()
        .contains("not the platform's"));
    assert_eq!(
        blocking(|| held.held()).unwrap().unwrap().listings[0].stars,
        5
    );
}

#[tokio::test]
async fn browse_reads_the_copy_searched_and_filtered() {
    let platform = Platform::new();
    let held = hold(&platform);
    let request = |search: &str, registry: &str, role: &str| ListRegistryPluginsRequest {
        search: search.into(),
        registry: registry.into(),
        role: role.into(),
        ..Default::default()
    };
    // Nothing pulled: nothing listed, the platform not asked.
    assert!(held
        .list(&request("", "", ""))
        .await
        .unwrap()
        .listings
        .is_empty());
    assert!(platform.asked.lock().unwrap().is_empty());
    let pulled = held
        .list(&ListRegistryPluginsRequest {
            pull_now: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(pulled.listings.len(), 1);
    assert!(pulled.catalogue_pulled_at_ns > 0);
    assert_eq!(
        held.list(&request("snaptrade", "", ""))
            .await
            .unwrap()
            .listings
            .len(),
        1
    );
    assert_eq!(
        held.list(&request("US", "community", "custody"))
            .await
            .unwrap()
            .listings
            .len(),
        1
    );
    assert!(held
        .list(&request("kraken", "", ""))
        .await
        .unwrap()
        .listings
        .is_empty());
    assert!(held
        .list(&request("", "verified", ""))
        .await
        .unwrap()
        .listings
        .is_empty());
    assert!(held
        .list(&request("", "", "oms"))
        .await
        .unwrap()
        .listings
        .is_empty());
}

#[tokio::test]
async fn a_version_is_fetched_verified_and_named_by_its_listing() {
    let platform = Platform::new();
    let held = hold(&platform);
    held.pull().await.unwrap();
    let fetched = held.fetch(LISTING, "1.4.0", "community").await.unwrap();
    let metadata = fetched.version.metadata.clone().unwrap();
    assert_eq!(
        metadata.name, LISTING,
        "a registry version is named by its listing"
    );
    assert_eq!(metadata.roles, ["custody"]);
    assert_eq!(fetched.version.source_registry, "community");
    assert_eq!(fetched.version.listing, LISTING);
    assert_eq!(fetched.documentation[0].markdown, "Reads accounts.");
    let asked = platform.asked.lock().unwrap().clone();
    assert!(asked.contains(&format!(
        "copy ghcr.io/open-meridian/plugins/open-meridian/snaptrade@{} into plugins/{LISTING}",
        digest(0xab)
    )));
    held.reverify(&fetched.version, &fetched.statement)
        .await
        .unwrap();
}

/// Each check refuses, naming it; there is no fallback and no override.
#[tokio::test]
async fn each_check_that_fails_refuses_the_version_naming_it() {
    type Spoil = fn(&Platform);
    let cases: Vec<(&str, Spoil, &str)> = vec![
        (
            "pulled",
            |p| {
                p.catalogue.lock().unwrap()["listings"][0]["versions"][0]["pull"] = json!({"listing": LISTING, "version": "1.4.0", "kind": "PULL_REASON_KIND_TERMS_BREACH",
                       "reason": "malware", "pulled_at_ns": 5, "restored_at_ns": 0});
            },
            "not offered: it was pulled",
        ),
        (
            "withdrawn",
            |p| {
                p.catalogue.lock().unwrap()["listings"][0]["versions"][0]["withdrawn_at_ns"] =
                    json!(5);
            },
            "withdrawn by its vendor",
        ),
        (
            "the floor",
            |p| {
                p.catalogue.lock().unwrap()["listings"][0]["versions"][0]["schema_version"] =
                    json!("v99");
            },
            "the floor",
        ),
        (
            "another digest",
            |p| {
                p.resolution.lock().unwrap()["image_digest"] = json!(digest(0xcd));
            },
            "the digest",
        ),
        (
            "no statement",
            |p| {
                p.resolution.lock().unwrap()["listing_statement"] = json!("");
            },
            "the resolution carried none",
        ),
        (
            "a statement other than the catalogue's",
            |p| {
                let other = p.signer.statement(
                    LISTING_PREDICATE,
                    &digest(0xab),
                    json!({"listing": LISTING, "version": "1.4.0", "registry": "community",
                       "signer_key": FINGERPRINT, "roles": ["custody", "oms"]}),
                );
                p.resolution.lock().unwrap()["listing_statement"] = json!(other);
            },
            "not the catalogue's",
        ),
        (
            "a statement by another key",
            |p| {
                let forged = TestSigner::new().statement(LISTING_PREDICATE, &digest(0xab),
                json!({"listing": LISTING, "version": "1.4.0", "registry": "community", "signer_key": FINGERPRINT}));
                p.resolution.lock().unwrap()["listing_statement"] = json!(forged.clone());
                p.catalogue.lock().unwrap()["listings"][0]["versions"][0]["listing_statement"] =
                    json!(forged);
            },
            "the listing statement: no signature on it is the platform's",
        ),
        (
            "no provenance",
            |p| {
                p.referrers.lock().unwrap().remove(0);
            },
            "the provenance",
        ),
        (
            "a provenance naming another signer",
            |p| {
                let other = p.signer.statement(
                    PROVENANCE_PREDICATE,
                    &digest(0xab),
                    json!({"signer": "SHA256:other"}),
                );
                p.referrers.lock().unwrap()[0] = other;
            },
            "names the signer key",
        ),
        (
            "another declaration in the image",
            |p| {
                let mut meta = metadata(&["custody"]);
                meta["declaration"]["secret_settings"] = json!(["other"]);
                let other = p.signer.statement(
                    METADATA_PREDICATE,
                    &digest(0xab),
                    json!({"metadata": meta}),
                );
                p.referrers.lock().unwrap()[1] = other;
            },
            "the declaration",
        ),
        (
            "other roles in the image",
            |p| {
                let other = p.signer.statement(
                    METADATA_PREDICATE,
                    &digest(0xab),
                    json!({"metadata": metadata(&["custody", "oms"])}),
                );
                p.referrers.lock().unwrap()[1] = other;
            },
            "the roles",
        ),
        (
            "a revoked signer",
            |p| {
                let revoked = p.signer.sign_document(json!({
                "issued_at_ns": 2, "keys": [{"key_id": "SGK-1", "organisation": "open-meridian",
                "fingerprint": FINGERPRINT, "registered_at_ns": 1, "revoked_at_ns": 3,
                "not_trusted_after_ns": 3}]}));
                p.catalogue.lock().unwrap()["key_status"] = revoked;
            },
            "the signing key",
        ),
    ];
    for (case, spoil, says) in cases {
        let platform = Platform::new();
        spoil(&platform);
        let held = hold(&platform);
        held.pull().await.unwrap();
        let refused = held.fetch(LISTING, "1.4.0", "").await.unwrap_err();
        assert!(refused.contains(says), "{case}: {refused}");
    }
}

#[tokio::test]
async fn a_verified_version_s_roles_are_its_review_s() {
    let platform = Platform::new();
    {
        let image = digest(0xab);
        let statement = platform.signer.statement(
            LISTING_PREDICATE,
            &image,
            json!({"listing": LISTING, "version": "1.4.0", "registry": "verified",
                   "listed_at": "2026-10-16T12:00:00Z", "signer_key": FINGERPRINT,
                   "roles": ["custody"], "approved_roles": ["custody"], "review_id": "RV-1"}),
        );
        let mut catalogue = platform.catalogue.lock().unwrap();
        catalogue["listings"][0]["versions"][0]["registry"] = json!("verified");
        catalogue["listings"][0]["versions"][0]["approved_roles"] = json!(["custody"]);
        catalogue["listings"][0]["versions"][0]["listing_statement"] = json!(statement.clone());
        platform.resolution.lock().unwrap()["listing_statement"] = json!(statement);
        // The image declares more than the review approved.
        platform.referrers.lock().unwrap()[1] = platform.signer.statement(
            METADATA_PREDICATE,
            &image,
            json!({"metadata": metadata(&["custody", "operations"])}),
        );
        catalogue["listings"][0]["versions"][0]["metadata"] = metadata(&["custody", "operations"]);
    }
    let held = hold(&platform);
    held.pull().await.unwrap();
    let fetched = held.fetch(LISTING, "1.4.0", "verified").await.unwrap();
    assert_eq!(fetched.version.metadata.unwrap().roles, ["custody"]);
}

#[test]
fn semver_orders_releases_above_their_pre_releases() {
    use std::cmp::Ordering::*;
    assert_eq!(semver_cmp("1.10.0", "1.9.9"), Some(Greater));
    assert_eq!(semver_cmp("1.0.0", "1.0.0-rc.1"), Some(Greater));
    assert_eq!(semver_cmp("1.0.0-rc.2", "1.0.0-rc.10"), Some(Less));
    assert_eq!(semver_cmp("1.0.0-alpha", "1.0.0-alpha.1"), Some(Less));
    assert_eq!(semver_cmp("1.0.0+build", "1.0.0"), Some(Equal));
    assert_eq!(semver_cmp("1.0", "1.0.0"), None);
    assert!(is_prerelease("2.0.0-beta") && !is_prerelease("2.0.0+meta"));
}

/// What `meridian plugin upload --bundle` names of the stand-in's 1.4.0:
/// its key status and listing statement, as the download kept them.
fn bundled(platform: &Platform) -> (String, String) {
    (
        platform.catalogue.lock().unwrap()["key_status"].to_string(),
        platform.resolution.lock().unwrap()["listing_statement"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

fn bundle<'a>(key_status: &'a str, statement: &'a str) -> Bundle<'a> {
    Bundle {
        listing: LISTING,
        version: "1.4.0",
        registry: "community",
        digest: "",
        key_status,
        listing_statement: statement,
        pull_status: "",
    }
}

/// Contract v26 (ruling 196): a bundle's signed pull status is checked
/// against the chart's keys, the newer kept, and the version it names warned
/// in the catalogue's copy; one not the platform's is refused naming it, and
/// an older one leaves the newer held.
#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_s_pull_status_is_checked_kept_when_newer_and_warns_the_version_it_names() {
    let platform = Platform::new();
    let held = hold(&platform);
    held.pull().await.unwrap();
    let (key_status, statement) = bundled(&platform);
    let image = digest(0xab);
    let status = |issued: i64, restored: i64| {
        platform
            .signer
            .sign_pull_status(json!({
                "issued_at_ns": issued,
                "pulls": [{"listing": LISTING, "version": "1.4.0", "kind": "PULL_REASON_KIND_TERMS_BREACH",
                           "reason": "a breach", "pulled_at_ns": 10, "notice_id": "",
                           "restored_at_ns": restored}],
            }))
            .to_string()
    };
    let forged = TestSigner::new()
        .sign_pull_status(json!({"issued_at_ns": 5, "pulls": []}))
        .to_string();
    let refused = held
        .carried(&Bundle {
            digest: &image,
            pull_status: &forged,
            ..bundle(&key_status, &statement)
        })
        .await
        .unwrap_err();
    assert!(refused.starts_with("the pull status:"), "{refused}");
    let newer = status(2_000, 0);
    held.carried(&Bundle {
        digest: &image,
        pull_status: &newer,
        ..bundle(&key_status, &statement)
    })
    .await
    .unwrap();
    let listed = held.list(&Default::default()).await.unwrap();
    let pulled = listed.listings[0].versions[0].pull.clone().expect("warned");
    assert_eq!(
        (pulled.pulled_at_ns, pulled.reason.as_str()),
        (10, "a breach")
    );
    // An older record leaves the newer held.
    let older = status(1_000, 20);
    held.carried(&Bundle {
        digest: &image,
        pull_status: &older,
        ..bundle(&key_status, &statement)
    })
    .await
    .unwrap();
    let listed = held.list(&Default::default()).await.unwrap();
    assert_eq!(
        listed.listings[0].versions[0]
            .pull
            .as_ref()
            .unwrap()
            .restored_at_ns,
        0
    );
    // A later restoration clears it.
    let restored = status(3_000, 30);
    held.carried(&Bundle {
        digest: &image,
        pull_status: &restored,
        ..bundle(&key_status, &statement)
    })
    .await
    .unwrap();
    let listed = held.list(&Default::default()).await.unwrap();
    assert_eq!(
        listed.listings[0].versions[0]
            .pull
            .as_ref()
            .unwrap()
            .restored_at_ns,
        30
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_is_verified_against_the_chart_s_keys_with_nothing_asked_of_the_platform() {
    let platform = Platform::new();
    let held = hold(&platform);
    let (key_status, statement) = bundled(&platform);
    let image = digest(0xab);
    let carried = held
        .carried(&Bundle {
            digest: &image,
            ..bundle(&key_status, &statement)
        })
        .await
        .unwrap();
    assert_eq!(carried.version.listing, LISTING);
    assert_eq!(carried.version.source_registry, "community");
    assert_eq!(
        carried.version.key_status_issued_at_ns,
        1_792_000_000_000_000_000
    );
    assert_eq!(carried.version.metadata.as_ref().unwrap().name, LISTING);
    assert_eq!(
        carried.version.metadata.as_ref().unwrap().roles,
        ["custody"]
    );
    assert_eq!(carried.documentation[0].markdown, OVERVIEW);
    assert_eq!(carried.statement, statement);
    // The referrers are read from the deployment's own registry, and nothing
    // from the platform, which an air-gapped deployment never reaches.
    assert_eq!(
        *platform.asked.lock().unwrap(),
        [format!("referrers of plugins/{LISTING}")]
    );
    // The bundle's key status is the newest held, for the launch's check.
    let kept = blocking(|| held.held()).unwrap().unwrap();
    assert_eq!(kept.pulled_at_ns, 0, "no catalogue was pulled");
    assert!(kept.listings.is_empty());
    assert_eq!(
        blocking(|| held.key_status()).unwrap().keys[0].fingerprint,
        FINGERPRINT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_that_does_not_verify_is_refused_naming_the_check_and_keeps_nothing() {
    let platform = Platform::new();
    let held = hold(&platform);
    let (key_status, statement) = bundled(&platform);
    let image = digest(0xab);
    let at = |key_status, statement| Bundle {
        digest: &image,
        ..bundle(key_status, statement)
    };
    let forged = TestSigner::new()
        .sign_document(json!({"issued_at_ns": 1, "keys": []}))
        .to_string();
    for (bundle, check) in [
        (at(&forged, &statement), "the key status"),
        (at("{not json", &statement), "the key status"),
        (
            Bundle {
                version: "1.5.0",
                ..at(&key_status, &statement)
            },
            "the listing statement",
        ),
        (
            Bundle {
                registry: "verified",
                ..at(&key_status, &statement)
            },
            "the listing statement",
        ),
        (
            Bundle {
                registry: "local",
                ..at(&key_status, &statement)
            },
            "the source registry",
        ),
        (
            Bundle {
                digest: &digest(0xcd),
                ..at(&key_status, &statement)
            },
            "the listing statement",
        ),
    ] {
        let refused = held.carried(&bundle).await.unwrap_err();
        assert!(refused.contains(check), "{check}: {refused}");
    }
    // The provenance gone from the referrers pushed with it.
    platform.referrers.lock().unwrap().remove(0);
    let refused = held
        .carried(&at(&key_status, &statement))
        .await
        .unwrap_err();
    assert!(refused.contains("the provenance"), "{refused}");
    assert!(
        blocking(|| held.held()).unwrap().is_none(),
        "a refusal keeps nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_s_signer_is_checked_against_the_newer_key_status_held() {
    let platform = Platform::new();
    let held = hold(&platform);
    let (key_status, statement) = bundled(&platform);
    let image = digest(0xab);
    // A later key status, held from an earlier bundle or a pull, revokes the
    // signer from before the build: the older bundle's status does not
    // restore it.
    let revoked = platform.signer.sign_document(json!({
        "issued_at_ns": 1_792_100_000_000_000_000i64,
        "keys": [{"key_id": "SGK-1", "organisation": "open-meridian", "name": "release",
                  "public_key": "ssh-ed25519 AAAA", "fingerprint": FINGERPRINT,
                  "registered_at_ns": 1, "registered_by_person_id": "",
                  "revoked_at_ns": 2, "revoked_by_person_id": "", "revoke_reason": "lost",
                  "not_trusted_after_ns": 2}],
    }));
    blocking(|| {
        held.store().put_registry_catalogue(&HeldCatalogue {
            key_status: revoked.to_string(),
            ..Default::default()
        })
    })
    .unwrap();
    let refused = held
        .carried(&Bundle {
            digest: &image,
            ..bundle(&key_status, &statement)
        })
        .await
        .unwrap_err();
    assert!(refused.contains("revoked"), "{refused}");
    // The newer status, carried in a later bundle, is the one kept and the
    // one whose age the version shows.
    let mut held_status: Value = serde_json::from_str(&key_status).unwrap();
    held_status.as_object_mut().unwrap().remove("signature");
    held_status["issued_at_ns"] = json!(1_792_150_000_000_000_000i64);
    let newer = platform.signer.sign_document(held_status).to_string();
    let carried = held
        .carried(&Bundle {
            digest: &image,
            ..bundle(&newer, &statement)
        })
        .await
        .unwrap();
    assert_eq!(
        carried.version.key_status_issued_at_ns,
        1_792_150_000_000_000_000
    );
    assert_eq!(
        blocking(|| held.key_status()).unwrap().issued_at_ns,
        1_792_150_000_000_000_000
    );
}

#[tokio::test]
async fn a_registry_this_deployment_does_not_take_is_refused_at_install() {
    // `registries.taken` (ruling 616; spec/starter-packs, requirement 26).
    let platform = Platform::new();
    let held = hold(&platform).taking(vec!["Verified ".into()]);
    held.pull().await.unwrap();
    let Err(refused) = held.fetch(LISTING, "1.4.0", "community").await else {
        panic!("a registry not taken was fetched");
    };
    assert!(
        refused.contains("does not take the community registry"),
        "{refused}"
    );
    assert_eq!(held.taken_refused("verified"), None);
    // Empty takes every registry, as before.
    assert_eq!(hold(&platform).taken_refused("community"), None);
}

/// Found on dev 2026-10-26: a page of Browse's copy reached 1,445,337
/// bytes, past the broker's 1 MiB, and was never sent. A page ends within
/// its budget whatever its size asks; every listing is read once and in
/// order by following the cursor; a listing past the budget is a page of
/// its own.
#[test]
fn a_page_of_listings_ends_within_its_budget_and_the_cursor_reads_on() {
    use prost::Message;
    let listing = |n: usize, bytes: usize| PluginListing {
        listing: format!("open-meridian/plugin-{n:02}"),
        regions: vec!["x".repeat(bytes)],
        ..Default::default()
    };
    let mut listings: Vec<PluginListing> = (0..10).map(|n| listing(n, 200 * 1024)).collect();
    listings.insert(4, listing(99, 700 * 1024));
    let total = listings.len();
    let budget = LIST_PAGE_BUDGET;
    let mut read = Vec::new();
    let mut cursor = String::new();
    let mut pages = 0;
    loop {
        let start: usize = cursor.parse().unwrap_or(0);
        let (page, next) = page_of(listings.clone(), start, 200, budget);
        assert!(!page.is_empty());
        let reply = ListRegistryPluginsReply {
            listings: page.clone(),
            next_cursor: next.clone(),
            catalogue_pulled_at_ns: 1,
        };
        assert!(
            page.len() == 1 || reply.encoded_len() <= 512 * 1024,
            "a page of {} is {} bytes",
            page.len(),
            reply.encoded_len()
        );
        read.extend(page.into_iter().map(|l| l.listing));
        pages += 1;
        if next.is_empty() {
            break;
        }
        cursor = next;
    }
    let every: Vec<String> = listings.iter().map(|l| l.listing.clone()).collect();
    assert_eq!(read, every, "each listing once, in order");
    assert!(pages >= 6, "{pages} pages for {total} listings");
    // A page size still caps a page, and small listings fill it.
    let small: Vec<PluginListing> = (0..10).map(|n| listing(n, 10)).collect();
    let (page, next) = page_of(small.clone(), 0, 4, budget);
    assert_eq!((page.len(), next.as_str()), (4, "4"));
    let (page, next) = page_of(small, 8, 4, budget);
    assert_eq!((page.len(), next.as_str()), (2, ""));
}
