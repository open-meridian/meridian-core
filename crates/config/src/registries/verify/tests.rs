//! The verification against a vector the platform's own signing code made
//! (registry_signing.py's envelope and sign_document, in Python's
//! cryptography, with a key made for it and thrown away), and against a test
//! signer for the cases the vector does not cover.

use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use serde_json::{json, Value};

use super::*;

/// A key pair standing in for the platform's KMS key.
pub(crate) struct TestSigner {
    pair: EcdsaKeyPair,
    rng: SystemRandom,
}

impl TestSigner {
    pub(crate) fn new() -> TestSigner {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        TestSigner { pair, rng }
    }

    pub(crate) fn pem(&self) -> String {
        let mut der = P256_SPKI_PREFIX.to_vec();
        der.extend_from_slice(self.pair.public_key().as_ref());
        format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            base64::engine::general_purpose::STANDARD.encode(der)
        )
    }

    pub(crate) fn keys(&self) -> PlatformKeys {
        PlatformKeys::from_pems(&[self.pem()]).unwrap()
    }

    fn sign(&self, message: &[u8]) -> String {
        let signature = self.pair.sign(&self.rng, message).unwrap();
        base64::engine::general_purpose::STANDARD.encode(signature.as_ref())
    }

    /// A DSSE envelope over a document's canonical JSON, as the platform
    /// writes one.
    pub(crate) fn envelope(&self, payload_type: &str, document: &Value) -> String {
        let body = canonical_json(document);
        json!({
            "payloadType": payload_type,
            "payload": base64::engine::general_purpose::STANDARD.encode(&body),
            "signatures": [{"keyid": "test", "sig": self.sign(&pae(payload_type, body.as_bytes()))}],
        })
        .to_string()
    }

    /// An in-toto statement about `digest`, enveloped.
    pub(crate) fn statement(&self, predicate_type: &str, digest: &str, predicate: Value) -> String {
        let (algorithm, value) = digest.split_once(':').unwrap();
        self.envelope(
            IN_TOTO,
            &json!({
                "_type": "https://in-toto.io/Statement/v1",
                "subject": [{"name": "ghcr.io/open-meridian/plugins/x", "digest": {algorithm: value}}],
                "predicateType": predicate_type,
                "predicate": predicate,
            }),
        )
    }

    /// A document carrying its own signature, as SigningKeyStatus does.
    pub(crate) fn sign_document(&self, mut document: Value) -> Value {
        let body = canonical_json(&document);
        document["signature"] = Value::String(self.sign(&pae(KEY_STATUS_TYPE, body.as_bytes())));
        document
    }

    /// A PluginPullStatus signed as the platform signs it (ruling 196).
    pub(crate) fn sign_pull_status(&self, mut document: Value) -> Value {
        let body = canonical_json(&document);
        document["signature"] =
            Value::String(self.sign(&pae(super::PULL_STATUS_TYPE, body.as_bytes())));
        document
    }
}

fn vector() -> Value {
    serde_json::from_str(include_str!("platform-vector.json")).unwrap()
}

#[test]
fn what_the_platform_signs_verifies_here() {
    let vector = vector();
    let keys =
        PlatformKeys::from_pems(&[vector["public_key"].as_str().unwrap().to_string()]).unwrap();
    // The key status, its key's name holding characters past ASCII and the
    // basic plane, which the platform's canonical JSON escapes.
    key_status_signed(&vector["key_status"], &keys).unwrap();
    let mut changed = vector["key_status"].clone();
    changed["keys"][0]["fingerprint"] = "SHA256:other".into();
    assert!(key_status_signed(&changed, &keys).is_err());
    // The listing statement, as ListedVersion.listing_statement carries it.
    let listed = listing_statement(
        vector["listing_statement"].as_str().unwrap(),
        &keys,
        "open-meridian/snaptrade",
        "1.4.0",
        "community",
        vector["digest"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(listed.signer_key, "SHA256:abc");
    assert_eq!(listed.roles, ["custody"]);
    assert_eq!(listed.floor, "v25");
    assert_eq!(
        listed.listed_at_ns,
        rfc3339_ns("2026-10-16T12:00:00.123456Z").unwrap()
    );
}

#[test]
fn canonical_json_is_python_s() {
    let value = json!({"b": 1, "a": "é\u{2028}😀\u{7f}\n\"\\", "c": [true, null, -5]});
    assert_eq!(
        canonical_json(&value),
        r#"{"a":"\u00e9\u2028\ud83d\ude00\u007f\n\"\\","b":1,"c":[true,null,-5]}"#
    );
}

#[test]
fn a_statement_is_held_to_its_key_its_type_and_its_subject() {
    let signer = TestSigner::new();
    let digest = format!("sha256:{}", "cd".repeat(32));
    let predicate = json!({"listing": "acme/fx", "version": "0.2.0", "registry": "verified",
        "signer_key": "SHA256:k", "listed_at": "2026-10-16T09:30:00Z",
        "roles": ["oms"], "approved_roles": ["oms"], "review_id": "RV-1", "floor": "v24"});
    let signed = signer.statement(LISTING_PREDICATE, &digest, predicate.clone());
    let keys = signer.keys();
    let listed =
        listing_statement(&signed, &keys, "acme/fx", "0.2.0", "verified", &digest).unwrap();
    assert_eq!(listed.approved_roles, ["oms"]);
    assert_eq!(listed.review_id, "RV-1");

    let other = TestSigner::new();
    for (case, json, says) in [
        (
            "another key",
            other.statement(LISTING_PREDICATE, &digest, predicate.clone()),
            "no signature on it is the platform's",
        ),
        (
            "another image",
            signer.statement(
                LISTING_PREDICATE,
                &format!("sha256:{}", "ef".repeat(32)),
                predicate.clone(),
            ),
            "its subject is not",
        ),
        (
            "another predicate",
            signer.statement(PROVENANCE_PREDICATE, &digest, predicate.clone()),
            "not https://open-meridian.dev/attestation/plugin-listing/v1",
        ),
        ("none", String::new(), "none was given"),
        ("not JSON", "{".into(), "not a DSSE envelope"),
    ] {
        let refused =
            listing_statement(&json, &keys, "acme/fx", "0.2.0", "verified", &digest).unwrap_err();
        assert!(refused.contains(says), "{case}: {refused}");
    }
    for (listing, version, registry, says) in [
        ("acme/other", "0.2.0", "verified", "the listing"),
        ("acme/fx", "0.3.0", "verified", "the version"),
        ("acme/fx", "0.2.0", "community", "the registry"),
    ] {
        let refused =
            listing_statement(&signed, &keys, listing, version, registry, &digest).unwrap_err();
        assert!(refused.contains(says), "{refused}");
    }
    // A payload tampered with after signing.
    let mut envelope: Value = serde_json::from_str(&signed).unwrap();
    let mut statement: Value = serde_json::from_slice(
        &base64::engine::general_purpose::STANDARD
            .decode(envelope["payload"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    statement["predicate"]["roles"] = json!(["oms", "ems"]);
    envelope["payload"] = base64::engine::general_purpose::STANDARD
        .encode(statement.to_string())
        .into();
    assert!(open_envelope(&envelope.to_string(), &keys).is_err());
    // No key in the chart: nothing verifies.
    assert!(open_envelope(&signed, &PlatformKeys::default())
        .unwrap_err()
        .contains("no platform key"));
}

#[test]
fn the_key_status_is_checked_and_a_signer_held_to_it() {
    let signer = TestSigner::new();
    let key = |fingerprint: &str, registered: i64, revoked: i64, not_after: i64| {
        meridian_domain::v1::SigningKeyRecord {
            key_id: format!("SGK-{fingerprint}"),
            organisation: "acme".into(),
            fingerprint: fingerprint.into(),
            registered_at_ns: registered,
            revoked_at_ns: revoked,
            not_trusted_after_ns: not_after,
            ..Default::default()
        }
    };
    let status = meridian_domain::v1::SigningKeyStatus {
        issued_at_ns: 100,
        keys: vec![
            key("A", 10, 0, 0),
            key("B", 10, 60, 40),
            key("C", 70, 0, 0),
            key("D", 10, 30, 0),
        ],
        signature: String::new(),
    };
    assert!(signer_trusted(&status, "acme", "A", 50).is_ok());
    assert!(
        signer_trusted(&status, "acme", "B", 40).is_ok(),
        "built before not-trusted-after"
    );
    assert!(signer_trusted(&status, "acme", "B", 50)
        .unwrap_err()
        .contains("revoked"));
    assert!(signer_trusted(&status, "acme", "C", 50)
        .unwrap_err()
        .contains("registered after"));
    assert!(signer_trusted(&status, "acme", "D", 50)
        .unwrap_err()
        .contains("revoked"));
    assert!(signer_trusted(&status, "other", "A", 50)
        .unwrap_err()
        .contains("not a key of other"));

    let document = signer.sign_document(json!({"issued_at_ns": 100, "keys": []}));
    key_status_signed(&document, &signer.keys()).unwrap();
    assert!(key_status_signed(&document, &TestSigner::new().keys()).is_err());
    assert!(
        key_status_signed(&json!({"issued_at_ns": 100, "keys": []}), &signer.keys())
            .unwrap_err()
            .contains("no signature")
    );
}

#[test]
fn a_key_that_is_not_a_p256_public_key_is_refused_naming_it() {
    let refused = PlatformKeys::from_pems(&[
        "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----".into(),
    ])
    .unwrap_err();
    assert!(refused.contains("key 1"), "{refused}");
    assert!(PlatformKeys::from_pems(&["not base64 !!".into()]).is_err());
}

#[test]
fn rfc3339_times_read_as_python_and_go_write_them() {
    assert_eq!(rfc3339_ns("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(rfc3339_ns("1970-01-01T01:00:00+01:00"), Some(0));
    assert_eq!(
        rfc3339_ns("2026-10-16T00:00:00Z"),
        Some(1_792_108_800_000_000_000)
    );
    assert_eq!(
        rfc3339_ns("2026-10-16T00:00:00.5-00:30"),
        Some(1_792_108_800_000_000_000 + 1_800_000_000_000 + 500_000_000)
    );
    assert_eq!(rfc3339_ns("not a time"), None);
}

/// v26's core (found on the development deployment 2026-10-18): the
/// builder attaches each attestation as a Sigstore bundle. Against real
/// cosign 2.5.2 output, signed with the fixture's throwaway key: each
/// bundle's envelope verifies against that key as the platform's, exactly
/// as a bare envelope does -- the legacy `.att` layer too -- and against
/// any other key nothing does, whatever the bundle's own verification
/// material says.
#[test]
fn a_sigstore_bundles_envelope_verifies_against_the_platforms_keys_alone() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../domain/src/attestation/cosign-2.5.2.json"
    ))
    .unwrap();
    let digest = fixture["digest"].as_str().unwrap();
    let keys =
        PlatformKeys::from_pems(&[fixture["public_key"].as_str().unwrap().to_string()]).unwrap();
    let other = PlatformKeys::from_pems(&[TestSigner::new().pem()]).unwrap();
    let blobs = &fixture["blobs"];
    let mut stated = Vec::new();
    let layers = fixture["manifests"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|manifest| manifest["layers"].as_array().unwrap().clone())
        .chain(fixture["legacy_att"]["layers"].as_array().unwrap().clone());
    for layer in layers {
        let blob = &blobs[layer["digest"].as_str().unwrap()];
        let envelope = meridian_domain::attestation::envelope_in(&layer, blob)
            .unwrap()
            .to_string();
        let predicate = [PROVENANCE_PREDICATE, METADATA_PREDICATE]
            .into_iter()
            .find(|predicate| statement(&envelope, &keys, predicate, digest).is_ok())
            .expect("each envelope verifies as one of the two");
        stated.push(predicate);
        assert_eq!(
            statement(&envelope, &other, predicate, digest).unwrap_err(),
            "no signature on it is the platform's"
        );
    }
    assert_eq!(
        stated
            .iter()
            .filter(|p| **p == PROVENANCE_PREDICATE)
            .count(),
        3,
        "two bundles and the legacy .att"
    );
    assert_eq!(stated.len(), 5);
}
