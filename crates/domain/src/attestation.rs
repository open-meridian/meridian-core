//! The DSSE envelope a referrer's layer holds (W8.3, W8.10, W8.14; the
//! registries spec's requirements 20, 22 and 24), found 2026-10-18 on the
//! development deployment: the platform's builder attaches each attestation
//! as a Sigstore bundle (`application/vnd.dev.sigstore.bundle.v0.3+json`,
//! cosign 2.5's `--new-bundle-format`) -- an OCI manifest with an empty
//! config and one layer holding the bundle, whose `dsseEnvelope` is the
//! envelope -- where earlier images carried the bare envelope as the layer
//! (`application/vnd.dsse.envelope.v1+json`, cosign's `.att`).
//!
//! What this answers is unverified: the caller verifies the envelope's
//! signature against the deployment's platform keys, exactly as for a bare
//! one, and never reads the bundle's own `verificationMaterial`, which is
//! the signer's word about itself.

use serde_json::Value;

/// Whether a media type is a Sigstore bundle's, any version.
pub fn is_bundle(media_type: &str) -> bool {
    let media = media_type.split(';').next().unwrap_or_default().trim();
    let Some(rest) = media.strip_prefix("application/vnd.dev.sigstore.bundle") else {
        return false;
    };
    let Some(version) = rest.strip_suffix("+json") else {
        return false;
    };
    version.is_empty()
        || version
            .strip_prefix(".v")
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
}

/// The envelope `blob` holds as the referrer's `layer` names it: a bare
/// DSSE envelope, or the `dsseEnvelope` of a Sigstore bundle by the layer's
/// media type or the bundle's own; `None` for anything else. Unverified.
pub fn envelope_in(layer: &Value, blob: &Value) -> Option<Value> {
    if !blob.is_object() {
        return None;
    }
    if blob.get("payloadType").is_some() {
        return Some(blob.clone());
    }
    let inner = blob
        .get("dsseEnvelope")
        .filter(|inner| inner.is_object() && inner.get("payloadType").is_some())?;
    let bundled = [layer.get("mediaType"), blob.get("mediaType")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(is_bundle);
    // The envelope alone: never the bundle's verificationMaterial.
    bundled.then(|| {
        serde_json::json!({
            "payload": inner.get("payload").cloned().unwrap_or(Value::Null),
            "payloadType": inner.get("payloadType").cloned().unwrap_or(Value::Null),
            "signatures": inner.get("signatures").cloned().unwrap_or(Value::Null),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real cosign 2.5.2 output (the platform's tests/fixtures/referrers,
    /// captured 2026-10-17 with a throwaway key whose public half it holds).
    pub(crate) const COSIGN: &str = include_str!("attestation/cosign-2.5.2.json");

    #[test]
    fn a_bundle_layer_gives_its_dsse_envelope_and_a_bare_one_itself() {
        let fixture: Value = serde_json::from_str(COSIGN).unwrap();
        let blobs = &fixture["blobs"];
        let mut found = 0;
        for manifest in fixture["manifests"].as_object().unwrap().values() {
            for layer in manifest["layers"].as_array().unwrap() {
                let blob = &blobs[layer["digest"].as_str().unwrap()];
                let envelope = envelope_in(layer, blob).expect("a bundle's envelope");
                assert_eq!(envelope["payloadType"], "application/vnd.in-toto+json");
                assert!(envelope.get("verificationMaterial").is_none());
                assert!(envelope["signatures"][0]["sig"].is_string());
                found += 1;
            }
        }
        assert_eq!(found, 4);
        let legacy = &fixture["legacy_att"]["layers"][0];
        let bare = envelope_in(legacy, &blobs[legacy["digest"].as_str().unwrap()]).unwrap();
        assert_eq!(bare["payloadType"], "application/vnd.in-toto+json");
    }

    #[test]
    fn anything_else_is_no_envelope() {
        let layer = serde_json::json!({"mediaType": "application/json"});
        assert_eq!(
            envelope_in(&layer, &serde_json::json!({"spdxVersion": "2.3"})),
            None
        );
        // A dsseEnvelope under a media type that is no bundle's.
        let inner = serde_json::json!({"dsseEnvelope": {"payloadType": "x", "payload": ""}});
        assert_eq!(envelope_in(&layer, &inner), None);
        assert_eq!(envelope_in(&layer, &Value::String("x".into())), None);
        assert!(is_bundle("application/vnd.dev.sigstore.bundle.v0.3+json"));
        assert!(is_bundle(
            "application/vnd.dev.sigstore.bundle+json; version=0.2"
        ));
        assert!(!is_bundle("application/vnd.dev.sigstore.bundle.vx+json"));
        assert!(!is_bundle("application/vnd.dsse.envelope.v1+json"));
    }
}
