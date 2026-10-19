//! The conductor's verification of what it installs from a registry (W8.10,
//! contract v25; spec/plugins-are-shared-through-registries, requirements
//! 20 to 26), offline, against the platform's public keys the chart carries.
//!
//! - **A DSSE envelope** (in-toto's): its payload's signature by one of the
//!   platform's keys, ECDSA P-256 over DSSE's pre-authentication encoding.
//! - **The listing statement**: an in-toto statement whose subject is the
//!   image's digest and whose predicate type is the plugin-listing one,
//!   naming the listing, the version, the registry and the signer key.
//! - **The provenance and the image's metadata**: the image's referrers,
//!   each an envelope the same key signed, their subject the image.
//! - **SigningKeyStatus**: its `signature`, base64 of an ECDSA P-256
//!   signature over the PAE of the key-status payload type and the record's
//!   canonical JSON without `signature` -- sorted keys, no spaces, every
//!   character outside printable ASCII escaped, as the platform writes it
//!   (Python's `json.dumps(..., sort_keys=True, separators=(",", ":"))`).
//!
//! A check that fails or cannot be made refuses, naming the check. There is
//! no fallback and no override.

use base64::Engine;
use serde_json::Value;

/// The DSSE payload type of an in-toto statement.
pub const IN_TOTO: &str = "application/vnd.in-toto+json";
/// The listing statement's predicate type (requirement 21).
pub const LISTING_PREDICATE: &str = "https://open-meridian.dev/attestation/plugin-listing/v1";
/// The builder's provenance (requirement 20).
pub const PROVENANCE_PREDICATE: &str = "https://slsa.dev/provenance/v1";
/// The image's metadata attestation (approved 2026-10-16, item 153).
pub const METADATA_PREDICATE: &str = "https://open-meridian.dev/attestation/plugin-metadata/v1";
/// SigningKeyStatus's payload type (requirement 26).
pub const KEY_STATUS_TYPE: &str = "application/vnd.open-meridian.signing-key-status+json";
/// Contract v26 (W8.14; ruling 196): the payload type the platform signs
/// every pulled version's record under.
pub const PULL_STATUS_TYPE: &str = "application/vnd.open-meridian.plugin-pull-status+json";

/// The DER prefix of a P-256 public key's SubjectPublicKeyInfo, before its
/// uncompressed point: the algorithm `id-ecPublicKey` with the curve
/// `prime256v1`, then the bit string's header.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// The platform's public keys, as each chart release carries them: the
/// current one and the next, so the key rotates without a gap (requirement
/// 23).
#[derive(Clone, Default)]
pub struct PlatformKeys {
    points: Vec<Vec<u8>>,
}

impl std::fmt::Debug for PlatformKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlatformKeys({} keys)", self.points.len())
    }
}

impl PlatformKeys {
    /// From PEM public keys (`-----BEGIN PUBLIC KEY-----`), each an ECDSA
    /// P-256 key; one that is not is refused naming its place.
    pub fn from_pems(pems: &[String]) -> Result<PlatformKeys, String> {
        let mut points = Vec::new();
        for (n, pem) in pems.iter().enumerate() {
            let body: String = pem
                .lines()
                .filter(|line| !line.starts_with("-----"))
                .map(str::trim)
                .collect();
            let der = base64::engine::general_purpose::STANDARD
                .decode(body)
                .map_err(|_| format!("the platform's key {} is not a PEM public key", n + 1))?;
            if der.len() != P256_SPKI_PREFIX.len() + 65 || !der.starts_with(&P256_SPKI_PREFIX) {
                return Err(format!(
                    "the platform's key {} is not an ECDSA P-256 public key",
                    n + 1
                ));
            }
            points.push(der[P256_SPKI_PREFIX.len()..].to_vec());
        }
        Ok(PlatformKeys { points })
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Whether one of the keys signed `message` (an ASN.1 DER signature).
    pub fn signed(&self, message: &[u8], signature: &[u8]) -> bool {
        self.points.iter().any(|point| {
            ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, point)
                .verify(message, signature)
                .is_ok()
        })
    }
}

/// DSSE's pre-authentication encoding.
pub fn pae(payload_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {} {} ",
        payload_type.len(),
        payload_type,
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn b64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

/// The document a DSSE envelope (as JSON) carries, once one of its
/// signatures verifies with the platform's keys, and its payload type.
pub fn open_envelope(json: &str, keys: &PlatformKeys) -> Result<(String, Value), String> {
    if keys.is_empty() {
        return Err("the chart carries no platform key, so nothing can be verified".into());
    }
    let envelope: Value =
        serde_json::from_str(json).map_err(|_| "it is not a DSSE envelope".to_string())?;
    let payload_type = envelope["payloadType"]
        .as_str()
        .ok_or("it names no payloadType")?;
    let body = envelope["payload"]
        .as_str()
        .and_then(b64)
        .ok_or("its payload is not base64")?;
    let message = pae(payload_type, &body);
    let signed = envelope["signatures"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|signature| signature["sig"].as_str().and_then(b64))
        .any(|signature| keys.signed(&message, &signature));
    if !signed {
        return Err("no signature on it is the platform's".into());
    }
    let document = serde_json::from_slice(&body).map_err(|_| "its payload is not JSON")?;
    Ok((payload_type.to_string(), document))
}

/// An in-toto statement of `predicate_type` about `digest`, opened and
/// checked: its signature, its type and its subject.
pub fn statement(
    json: &str,
    keys: &PlatformKeys,
    predicate_type: &str,
    digest: &str,
) -> Result<Value, String> {
    let (payload_type, statement) = open_envelope(json, keys)?;
    if payload_type != IN_TOTO {
        return Err(format!(
            "its payload type is {payload_type}, not an in-toto statement"
        ));
    }
    if statement["predicateType"].as_str() != Some(predicate_type) {
        return Err(format!(
            "it states {}, not {predicate_type}",
            statement["predicateType"].as_str().unwrap_or("nothing")
        ));
    }
    let (algorithm, value) = digest.split_once(':').unwrap_or(("sha256", digest));
    let about = statement["subject"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|subject| subject["digest"][algorithm].as_str() == Some(value));
    if !about {
        return Err(format!("its subject is not {digest}"));
    }
    Ok(statement)
}

/// What a verified listing statement says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listed {
    pub listing: String,
    pub version: String,
    pub registry: String,
    /// The fingerprint of the key that signed the release tag.
    pub signer_key: String,
    pub listed_at_ns: i64,
    /// The roles a launch may approve: for verified, the review's.
    pub roles: Vec<String>,
    pub approved_roles: Vec<String>,
    pub floor: String,
    pub review_id: String,
}

/// The listing statement of `listing` `version` in `registry`, about
/// `digest`, verified (requirement 21).
pub fn listing_statement(
    json: &str,
    keys: &PlatformKeys,
    listing: &str,
    version: &str,
    registry: &str,
    digest: &str,
) -> Result<Listed, String> {
    if json.trim().is_empty() {
        return Err("the listing statement: none was given".into());
    }
    let statement = statement(json, keys, LISTING_PREDICATE, digest)
        .map_err(|why| format!("the listing statement: {why}"))?;
    let predicate = &statement["predicate"];
    let text = |name: &str| predicate[name].as_str().unwrap_or_default().to_string();
    let words = |name: &str| -> Vec<String> {
        predicate[name]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|role| role.as_str().map(String::from))
            .collect()
    };
    let listed = Listed {
        listing: text("listing"),
        version: text("version"),
        registry: text("registry"),
        signer_key: text("signer_key"),
        listed_at_ns: rfc3339_ns(&text("listed_at")).unwrap_or(0),
        roles: words("roles"),
        approved_roles: words("approved_roles"),
        floor: text("floor"),
        review_id: text("review_id"),
    };
    for (said, asked, what) in [
        (&listed.listing, listing, "listing"),
        (&listed.version, version, "version"),
        (&listed.registry, registry, "registry"),
    ] {
        if said != asked {
            return Err(format!(
                "the listing statement: it names the {what} {said:?}, not {asked:?}"
            ));
        }
    }
    if listed.signer_key.is_empty() {
        return Err("the listing statement: it names no signer key".into());
    }
    Ok(listed)
}

/// SigningKeyStatus as the platform answers it, its signature checked
/// against the canonical JSON of the record without it (requirement 26).
/// Whether a PluginPullStatus is the platform's (W8.14, contract v26):
/// signed as the key status is, under its own payload type.
pub fn pull_status_signed(document: &Value, keys: &PlatformKeys) -> Result<(), String> {
    if keys.is_empty() {
        return Err(
            "the chart carries no platform key, so the pull status cannot be checked".into(),
        );
    }
    let Value::Object(fields) = document else {
        return Err("the pull status is not a record".into());
    };
    let signature = fields
        .get("signature")
        .and_then(Value::as_str)
        .and_then(b64)
        .ok_or("the pull status carries no signature")?;
    let mut unsigned = fields.clone();
    unsigned.remove("signature");
    let body = canonical_json(&Value::Object(unsigned));
    if keys.signed(&pae(PULL_STATUS_TYPE, body.as_bytes()), &signature) {
        Ok(())
    } else {
        Err("the pull status's signature is not the platform's".into())
    }
}

pub fn key_status_signed(document: &Value, keys: &PlatformKeys) -> Result<(), String> {
    if keys.is_empty() {
        return Err(
            "the chart carries no platform key, so the key status cannot be checked".into(),
        );
    }
    let Value::Object(fields) = document else {
        return Err("the key status is not a record".into());
    };
    let signature = fields
        .get("signature")
        .and_then(Value::as_str)
        .and_then(b64)
        .ok_or("the key status carries no signature")?;
    let mut unsigned = fields.clone();
    unsigned.remove("signature");
    let body = canonical_json(&Value::Object(unsigned));
    if keys.signed(&pae(KEY_STATUS_TYPE, body.as_bytes()), &signature) {
        Ok(())
    } else {
        Err("the key status's signature is not the platform's".into())
    }
}

/// Whether the key whose fingerprint signed a release tag was trusted at
/// the build (requirement 24; item 74): the organisation's, registered by
/// then, and not revoked before it -- a revocation's "not trusted after"
/// time, or where it names none its revocation, after the build.
pub fn signer_trusted(
    status: &meridian_domain::v1::SigningKeyStatus,
    organisation: &str,
    fingerprint: &str,
    built_at_ns: i64,
) -> Result<(), String> {
    let Some(key) = status
        .keys
        .iter()
        .find(|key| key.organisation == organisation && key.fingerprint == fingerprint)
    else {
        return Err(format!(
            "the signing key: {fingerprint} is not a key of {organisation} in the key status"
        ));
    };
    if key.registered_at_ns > built_at_ns {
        return Err(format!(
            "the signing key: {fingerprint} was registered after the build"
        ));
    }
    if key.revoked_at_ns > 0 {
        let until = if key.not_trusted_after_ns > 0 {
            key.not_trusted_after_ns
        } else {
            key.revoked_at_ns
        };
        if until < built_at_ns {
            return Err(format!(
                "the signing key: {fingerprint} was revoked, trusted until before the build"
            ));
        }
    }
    Ok(())
}

/// JSON as the platform signs it: keys sorted, no spaces, each character
/// outside printable ASCII escaped `\uXXXX` (a pair past the basic plane),
/// numbers as written.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort();
            out.push('{');
            for (n, key) in keys.into_iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_canonical(&fields[key], out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// An RFC 3339 time (`2026-10-16T12:00:00.123456+00:00`, `...Z`) as
/// nanoseconds since the epoch.
pub fn rfc3339_ns(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', ' '])?;
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let (clock, offset_seconds) = if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        (clock, 0)
    } else {
        let at = rest.rfind(['+', '-'])?;
        let (clock, offset) = rest.split_at(at);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (h, m) = offset[1..]
            .split_once(':')
            .unwrap_or((&offset[1..3], &offset[3..]));
        (
            clock,
            sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60),
        )
    };
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut hms = hms.splitn(3, ':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next().unwrap_or("0").parse().ok()?;
    let nanos: i64 = if fraction.is_empty() {
        0
    } else {
        let digits: String = fraction.chars().take(9).collect();
        digits.parse::<i64>().ok()? * 10i64.pow(9 - digits.len() as u32)
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
    Some(seconds * 1_000_000_000 + nanos)
}

#[cfg(test)]
pub(crate) mod tests;
