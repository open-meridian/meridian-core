//! The platform's registry rows as they arrive (contract v25): the proto's
//! JSON mapping, every time in nanoseconds (an integer, or a string as
//! proto3's JSON writes an int64), enums by name, a version's declaration
//! as the SDK prints it (W8.1). Read strictly where a field decides what is
//! installed -- a digest, a registry, a statement -- and leniently where it
//! is a count or a word the platform may add to.

use meridian_domain::v1::{
    DocumentationText, DownloadListedVersionReply, ListedVersion, PluginBuild, PluginListing,
    PluginMetadata, PluginPull, PluginPullStatus, PullCatalogueReply, PullReasonKind,
    SigningKeyRecord, SigningKeyStatus,
};
use serde_json::Value;

fn text(value: &Value, name: &str) -> String {
    value[name].as_str().unwrap_or_default().to_string()
}

fn number(value: &Value, name: &str) -> i64 {
    match &value[name] {
        Value::Number(n) => n.as_i64().unwrap_or_default(),
        Value::String(s) => s.parse().unwrap_or_default(),
        _ => 0,
    }
}

fn words(value: &Value, name: &str) -> Vec<String> {
    value[name]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|word| word.as_str().map(String::from))
        .collect()
}

fn is_digest(digest: &str) -> bool {
    digest
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A listing's name, `organisation/plugin`, each a lower-case DNS label.
pub fn is_listing(listing: &str) -> bool {
    let label = |part: &str| {
        !part.is_empty()
            && part.len() <= 39
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !part.starts_with('-')
            && !part.ends_with('-')
    };
    match listing.split_once('/') {
        Some((organisation, plugin)) => label(organisation) && label(plugin),
        None => false,
    }
}

fn pull(value: &Value) -> Option<PluginPull> {
    if !value.is_object() {
        return None;
    }
    let kind = match value["kind"].as_str() {
        Some(name) => PullReasonKind::from_str_name(name).unwrap_or(PullReasonKind::Unspecified),
        None => PullReasonKind::Unspecified,
    };
    Some(PluginPull {
        listing: text(value, "listing"),
        version: text(value, "version"),
        kind: kind as i32,
        reason: text(value, "reason"),
        pulled_at_ns: number(value, "pulled_at_ns"),
        notice_id: text(value, "notice_id"),
        restored_at_ns: number(value, "restored_at_ns"),
        // Contract v27 (ruling 311).
        by_bundle_issued_at_ns: number(value, "by_bundle_issued_at_ns"),
    })
}

fn build(value: &Value) -> PluginBuild {
    PluginBuild {
        build_id: text(value, "build_id"),
        listing: text(value, "listing"),
        version: text(value, "version"),
        registry: text(value, "registry"),
        repository: text(value, "repository"),
        commit: text(value, "commit"),
        tag: text(value, "tag"),
        signer_fingerprint: text(value, "signer_fingerprint"),
        builder: text(value, "builder"),
        image_digest: text(value, "image_digest"),
        log_url: text(value, "log_url"),
        requested_at_ns: number(value, "requested_at_ns"),
        finished_at_ns: number(value, "finished_at_ns"),
        // The platform's page for the build (contract v26, W8.8).
        page_url: text(value, "page_url"),
        ..Default::default()
    }
}

/// The vendor's texts a listed version carries (contract v26, W8.10), each
/// by its section; checked against the declaration's digests where they
/// are drawn, never here.
fn documentation(value: &Value) -> Vec<DocumentationText> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|text_of| DocumentationText {
            section: text(text_of, "section"),
            markdown: text(text_of, "markdown"),
        })
        .filter(|text_of| !text_of.section.is_empty())
        .collect()
}

/// Every pulled version in the catalogue as the platform signed it
/// (contract v26, W8.14), or `None` where the answer carries none.
pub fn pull_status(value: &Value) -> Option<PluginPullStatus> {
    if !value.is_object() {
        return None;
    }
    Some(PluginPullStatus {
        issued_at_ns: number(value, "issued_at_ns"),
        pulls: value["pulls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(pull)
            .collect(),
        signature: text(value, "signature"),
    })
}

/// A version's metadata, its declaration as the SDK prints it; one whose
/// declaration does not read is refused, naming the version.
fn metadata(value: &Value, at: &str) -> Result<PluginMetadata, String> {
    let declaration = match &value["declaration"] {
        Value::Null => None,
        Value::Object(fields) if fields.is_empty() => None,
        held => {
            let declared: meridian_domain::declaration::Declared =
                serde_json::from_value(held.clone())
                    .map_err(|failed| format!("{at}.metadata.declaration: {failed}"))?;
            Some(
                meridian_domain::declaration::from_json(&declared)
                    .map_err(|why| format!("{at}.metadata.declaration: {why}"))?,
            )
        }
    };
    Ok(PluginMetadata {
        name: text(value, "name"),
        version: text(value, "version"),
        roles: words(value, "roles"),
        interface: value["interface"].as_bool().unwrap_or(false),
        sdk_version: text(value, "sdk_version"),
        declaration,
    })
}

/// One listed version, read from the catalogue.
pub fn listed_version(value: &Value, at: &str) -> Result<ListedVersion, String> {
    let version = ListedVersion {
        listing: text(value, "listing"),
        version: text(value, "version"),
        registry: text(value, "registry"),
        image_digest: text(value, "image_digest"),
        metadata: Some(metadata(&value["metadata"], at)?),
        approved_roles: words(value, "approved_roles"),
        schema_version: text(value, "schema_version"),
        build: Some(build(&value["build"])),
        listed_at_ns: number(value, "listed_at_ns"),
        withdrawn_at_ns: number(value, "withdrawn_at_ns"),
        pull: pull(&value["pull"]),
        sequence: number(value, "sequence").max(0) as u64,
        listing_statement: text(value, "listing_statement"),
        // Contract v26 (W8.10; ruling 274): reviewed for verified, and the
        // vendor's texts for the Documentation tab before install.
        reviewed: value["reviewed"].as_bool().unwrap_or(false),
        documentation: documentation(&value["documentation"]),
    };
    if !matches!(version.registry.as_str(), "community" | "verified") {
        return Err(format!(
            "{at}.registry is {:?}, not community or verified",
            version.registry
        ));
    }
    if !is_digest(&version.image_digest) {
        return Err(format!("{at}.image_digest is not a sha256 digest"));
    }
    Ok(version)
}

/// A listing, read from the catalogue; a version that does not read is
/// refused, naming it, and the listing with it.
pub fn listing(value: &Value, at: &str) -> Result<PluginListing, String> {
    let name = text(value, "listing");
    if !is_listing(&name) {
        return Err(format!("{at}.listing {name:?} is not organisation/plugin"));
    }
    let versions = value["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(n, version)| listed_version(version, &format!("{at}.versions[{n}]")))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(stray) = versions.iter().find(|version| version.listing != name) {
        return Err(format!(
            "{at}: a version of {} is listed under {name}",
            stray.listing
        ));
    }
    Ok(PluginListing {
        listing: name,
        repository: text(value, "repository"),
        versions,
        regions: words(value, "regions"),
        asset_classes: words(value, "asset_classes"),
        support_contact: text(value, "support_contact"),
        stars: number(value, "stars").max(0) as u64,
        installs: number(value, "installs").max(0) as u64,
        instances: number(value, "instances").max(0) as u64,
        counts_shown: value["counts_shown"].as_bool().unwrap_or(false),
        // Contract v26 (W8.10; ruling 274): agents' answers carry these in
        // place of the notice.
        reviewed: value["reviewed"].as_bool().unwrap_or(false),
        terms_url: text(value, "terms_url"),
        // Contract v26 (ruling 306): the publishing organisation's name.
        publisher: text(value, "publisher"),
        // Contract v27 (ruling 330): validated by the platform.
        support_url: text(value, "support_url"),
        support_email: text(value, "support_email"),
        prices: Vec::new(),
        // Contract v28 (W8.16): the listing that replaces it, or empty.
        replaced_by: Some(text(value, "replaced_by"))
            .filter(|successor| is_listing(successor))
            .unwrap_or_default(),
    })
}

/// SigningKeyStatus, as the platform answers it.
pub fn key_status(value: &Value) -> SigningKeyStatus {
    SigningKeyStatus {
        issued_at_ns: number(value, "issued_at_ns"),
        keys: value["keys"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|key| SigningKeyRecord {
                key_id: text(key, "key_id"),
                organisation: text(key, "organisation"),
                name: text(key, "name"),
                public_key: text(key, "public_key"),
                fingerprint: text(key, "fingerprint"),
                registered_at_ns: number(key, "registered_at_ns"),
                registered_by_person_id: text(key, "registered_by_person_id"),
                revoked_at_ns: number(key, "revoked_at_ns"),
                revoked_by_person_id: text(key, "revoked_by_person_id"),
                revoke_reason: text(key, "revoke_reason"),
                not_trusted_after_ns: number(key, "not_trusted_after_ns"),
            })
            .collect(),
        signature: text(value, "signature"),
    }
}

/// PullCatalogueReply, as the platform answers it.
pub fn catalogue(value: &Value) -> Result<PullCatalogueReply, String> {
    let listings = value["listings"]
        .as_array()
        .ok_or("the catalogue holds no listings list")?
        .iter()
        .enumerate()
        .map(|(n, item)| listing(item, &format!("listings[{n}]")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PullCatalogueReply {
        listings,
        sequence: number(value, "sequence").max(0) as u64,
        key_status: value
            .get("key_status")
            .filter(|status| status.is_object())
            .map(key_status),
        // Contract v26 (W8.10; ruling 274): the Terms' address, which the
        // install dialog's one line links.
        terms_url: text(value, "terms_url"),
        packs: Vec::new(),
    })
}

/// DownloadListedVersionReply, as the platform answers a resolution.
pub fn resolution(value: &Value) -> Result<DownloadListedVersionReply, String> {
    let reply = DownloadListedVersionReply {
        image_digest: text(value, "image_digest"),
        pull_from: text(value, "pull_from"),
        pull_token: text(value, "pull_token"),
        pull_token_expires_at_ns: number(value, "pull_token_expires_at_ns"),
        listing_statement: text(value, "listing_statement"),
        // Contract v26 (W8.14; ruling 196): the catalogue's pulls, signed.
        pull_status: pull_status(&value["pull_status"]),
    };
    if !is_digest(&reply.image_digest) {
        return Err("the resolution names no sha256 digest".into());
    }
    if reply.pull_from.is_empty() || reply.pull_from.contains(char::is_whitespace) {
        return Err("the resolution names no registry to pull from".into());
    }
    Ok(reply)
}
