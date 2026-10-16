//! Copying a listed version's image and its referrers into the deployment's
//! own registry, by digest (W8.10, contract v25; spec/plugins-are-shared-
//! through-registries, requirements 9, 10 and 22).
//!
//! The source is a registry the platform's resolution names: the
//! platform's public GitHub registry for community, pulled anonymously
//! with the token its `WWW-Authenticate` challenge offers; Artifact Registry
//! for verified, with the short-lived pull token the resolution carried.
//! Every manifest and blob is read by digest and its bytes checked against
//! it before anything is written. The referrers -- the provenance, the bill
//! of materials, the scan results and, for community, the listing statement
//! -- are read through the referrers API (OCI distribution 1.1), or its tag
//! schema where a registry has none, and copied beside the image; the
//! deployment's registry keeps them under the tag schema (`sha256-<hex>`),
//! which a registry without the referrers API still serves, and the
//! conductor reads them there again at launch.

use std::time::Duration;

use sha2::Digest;

const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json";

/// The most a single blob may be: the platform's cap on an image (1 GiB,
/// requirement 14's caps), with room for its layers' framing.
const MOST_BLOB: u64 = 1_100 * 1024 * 1024;

pub struct Oci {
    client: reqwest::Client,
    /// The deployment's own registry, `http://<host>:5000`.
    local: String,
}

fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&sha2::Sha256::digest(bytes)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_digest(digest: &str) -> bool {
    digest
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The tag schema's tag for a digest's referrers.
fn referrers_tag(digest: &str) -> String {
    digest.replacen(':', "-", 1)
}

/// A source: its host, its repository, and how to authenticate there.
struct Source<'a> {
    host: &'a str,
    name: &'a str,
    token: String,
}

impl Oci {
    pub fn new(local: impl Into<String>) -> Result<Oci, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|failed| format!("the registry client could not be built: {failed}"))?;
        Ok(Oci {
            client,
            local: local.into().trim_end_matches('/').to_string(),
        })
    }

    /// The anonymous token a registry's challenge offers for pulling
    /// `name`, or none where it asks for none.
    async fn anonymous_token(&self, host: &str, name: &str) -> Result<String, String> {
        let probe = self
            .client
            .get(format!("https://{host}/v2/"))
            .send()
            .await
            .map_err(|failed| format!("{host} could not be reached: {failed}"))?;
        if probe.status().as_u16() != 401 {
            return Ok(String::new());
        }
        let challenge = probe
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let field = |key: &str| {
            challenge
                .split(['"', ','])
                .collect::<Vec<_>>()
                .windows(2)
                .find(|pair| {
                    pair[0].trim().trim_start_matches("Bearer ").trim() == format!("{key}=")
                })
                .map(|pair| pair[1].to_string())
        };
        let realm = field("realm").ok_or_else(|| format!("{host} names no token realm"))?;
        let mut url =
            reqwest::Url::parse(&realm).map_err(|_| format!("{host}'s realm is not a URL"))?;
        if url.scheme() != "https" {
            return Err(format!("{host}'s token realm is not https"));
        }
        if let Some(service) = field("service") {
            url.query_pairs_mut().append_pair("service", &service);
        }
        url.query_pairs_mut()
            .append_pair("scope", &format!("repository:{name}:pull"));
        let body = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|failed| format!("{host} gave no pull token: {failed}"))?
            .bytes()
            .await
            .map_err(|failed| format!("{host}'s token answer did not arrive: {failed}"))?;
        let answer: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| format!("{host}'s token answer is not JSON"))?;
        Ok(answer["token"]
            .as_str()
            .or(answer["access_token"].as_str())
            .unwrap_or_default()
            .to_string())
    }

    async fn get(
        &self,
        source: &Source<'_>,
        path: &str,
        accept: &str,
    ) -> Result<(String, Vec<u8>), String> {
        let mut request = self
            .client
            .get(format!("https://{}/v2/{}/{path}", source.host, source.name))
            .header(reqwest::header::ACCEPT, accept);
        if !source.token.is_empty() {
            request = request.bearer_auth(&source.token);
        }
        let answer = request
            .send()
            .await
            .map_err(|failed| format!("{} could not be reached: {failed}", source.host))?;
        let status = answer.status().as_u16();
        if status != 200 {
            return Err(format!("{} answered HTTP {status} for {path}", source.host));
        }
        if answer
            .content_length()
            .is_some_and(|length| length > MOST_BLOB)
        {
            return Err(format!("{path} is larger than an image may be"));
        }
        let kind = answer
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .to_string();
        let bytes = answer
            .bytes()
            .await
            .map_err(|failed| format!("{path} did not arrive whole: {failed}"))?;
        Ok((kind, bytes.to_vec()))
    }

    /// Bytes read by digest, checked against it.
    async fn by_digest(
        &self,
        source: &Source<'_>,
        kind: &str,
        digest: &str,
        accept: &str,
    ) -> Result<(String, Vec<u8>), String> {
        if !is_digest(digest) {
            return Err(format!("{digest:?} is not a digest"));
        }
        let (media, bytes) = self
            .get(source, &format!("{kind}/{digest}"), accept)
            .await?;
        if digest_of(&bytes) != digest {
            return Err(format!(
                "the digest: what {} served for {digest} is not it",
                source.host
            ));
        }
        Ok((media, bytes))
    }

    async fn local_has_blob(&self, repository: &str, digest: &str) -> bool {
        self.client
            .head(format!("{}/v2/{repository}/blobs/{digest}", self.local))
            .send()
            .await
            .is_ok_and(|answer| answer.status().is_success())
    }

    async fn put_blob(&self, repository: &str, digest: &str, bytes: Vec<u8>) -> Result<(), String> {
        if self.local_has_blob(repository, digest).await {
            return Ok(());
        }
        let started = self
            .client
            .post(format!("{}/v2/{repository}/blobs/uploads/", self.local))
            .send()
            .await
            .map_err(|failed| {
                format!("the deployment's registry could not be reached: {failed}")
            })?;
        if started.status().as_u16() != 202 {
            return Err(format!(
                "the deployment's registry did not start an upload: HTTP {}",
                started.status()
            ));
        }
        let location = started
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or("the deployment's registry gave no upload location")?
            .to_string();
        let mut url = if location.starts_with("http") {
            reqwest::Url::parse(&location)
        } else {
            reqwest::Url::parse(&format!("{}{location}", self.local))
        }
        .map_err(|_| "the deployment's registry gave an unusable upload location".to_string())?;
        url.query_pairs_mut().append_pair("digest", digest);
        let finished = self
            .client
            .put(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|failed| format!("the deployment's registry took no blob: {failed}"))?;
        if finished.status().as_u16() != 201 {
            return Err(format!(
                "the deployment's registry refused a blob: HTTP {}",
                finished.status()
            ));
        }
        Ok(())
    }

    async fn put_manifest(
        &self,
        repository: &str,
        reference: &str,
        media: &str,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        let answer = self
            .client
            .put(format!(
                "{}/v2/{repository}/manifests/{reference}",
                self.local
            ))
            .header(reqwest::header::CONTENT_TYPE, media)
            .body(bytes)
            .send()
            .await
            .map_err(|failed| format!("the deployment's registry took no manifest: {failed}"))?;
        if !answer.status().is_success() {
            return Err(format!(
                "the deployment's registry refused a manifest: HTTP {}",
                answer.status()
            ));
        }
        Ok(())
    }

    /// One manifest and everything it names, copied; an index's manifests
    /// with it. Answers the manifest's media type and bytes.
    fn copy_manifest<'a>(
        &'a self,
        source: &'a Source<'a>,
        repository: &'a str,
        digest: &'a str,
    ) -> Copying<'a> {
        Box::pin(async move {
            let (media, bytes) = self.by_digest(source, "manifests", digest, ACCEPT).await?;
            let manifest: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| format!("the manifest {digest} is not JSON"))?;
            let media = manifest["mediaType"]
                .as_str()
                .map(String::from)
                .unwrap_or(media);
            for child in manifest["manifests"].as_array().into_iter().flatten() {
                let child = child["digest"].as_str().unwrap_or_default();
                self.copy_manifest(source, repository, child).await?;
            }
            let blobs = manifest["config"]
                .get("digest")
                .into_iter()
                .chain(
                    manifest["layers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|l| l.get("digest")),
                )
                .filter_map(|d| d.as_str())
                .map(String::from)
                .collect::<Vec<_>>();
            for blob in blobs {
                if !self.local_has_blob(repository, &blob).await {
                    let (_, content) = self.by_digest(source, "blobs", &blob, "*/*").await?;
                    self.put_blob(repository, &blob, content).await?;
                }
            }
            self.put_manifest(repository, digest, &media, bytes.clone())
                .await?;
            Ok((media, bytes))
        })
    }

    /// A blob, by digest, from where the envelopes are read, checked.
    async fn blob(&self, from: &From<'_>, digest: &str) -> Result<Vec<u8>, String> {
        match from {
            From::Remote(source) => self
                .by_digest(source, "blobs", digest, "*/*")
                .await
                .map(|(_, b)| b),
            From::Local(repository) => {
                let bytes = self
                    .local_get(repository, &format!("blobs/{digest}"), "*/*")
                    .await?;
                if digest_of(&bytes) != digest {
                    return Err(format!("the digest: the blob {digest} is not what it says"));
                }
                Ok(bytes)
            }
        }
    }

    /// The DSSE envelopes among a referrer manifest's layers.
    async fn envelopes_of(&self, from: &From<'_>, manifest: &serde_json::Value) -> Vec<String> {
        let mut found = Vec::new();
        for layer in manifest["layers"].as_array().into_iter().flatten() {
            let Some(digest) = layer["digest"].as_str() else {
                continue;
            };
            if let Ok(bytes) = self.blob(from, digest).await {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if value.get("payloadType").is_some() {
                        found.push(value.to_string());
                    }
                }
            }
        }
        found
    }

    /// Copy `pull_from@digest` and its referrers into `repository` in the
    /// deployment's own registry, answering each referrer's DSSE envelope.
    pub async fn copy(
        &self,
        pull_from: &str,
        pull_token: &str,
        digest: &str,
        repository: &str,
    ) -> Result<Vec<String>, String> {
        let (host, name) = pull_from
            .split_once('/')
            .ok_or_else(|| format!("{pull_from:?} names no registry and repository"))?;
        let token = if pull_token.is_empty() {
            self.anonymous_token(host, name).await?
        } else {
            pull_token.to_string()
        };
        let source = Source { host, name, token };
        self.copy_manifest(&source, repository, digest).await?;
        // The referrers: the API, or the tag schema where there is none.
        let listed = match self
            .get(&source, &format!("referrers/{digest}"), INDEX)
            .await
        {
            Ok((_, bytes)) => bytes,
            Err(_) => self
                .get(
                    &source,
                    &format!("manifests/{}", referrers_tag(digest)),
                    INDEX,
                )
                .await
                .map(|(_, bytes)| bytes)
                .map_err(|why| format!("the referrers of {digest} could not be read: {why}"))?,
        };
        let index: serde_json::Value = serde_json::from_slice(&listed)
            .map_err(|_| format!("the referrers of {digest} are not an index"))?;
        let mut envelopes = Vec::new();
        let mut kept = Vec::new();
        for referrer in index["manifests"].as_array().into_iter().flatten() {
            let Some(child) = referrer["digest"].as_str() else {
                continue;
            };
            let (_, bytes) = self.copy_manifest(&source, repository, child).await?;
            let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            envelopes.extend(self.envelopes_of(&From::Remote(&source), &manifest).await);
            kept.push(referrer.clone());
        }
        // Kept under the tag schema, which the deployment's registry serves.
        let local_index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": INDEX,
            "manifests": kept,
        });
        self.put_manifest(
            repository,
            &referrers_tag(digest),
            INDEX,
            serde_json::to_vec(&local_index).map_err(|failed| failed.to_string())?,
        )
        .await?;
        Ok(envelopes)
    }

    async fn local_get(
        &self,
        repository: &str,
        path: &str,
        accept: &str,
    ) -> Result<Vec<u8>, String> {
        let answer = self
            .client
            .get(format!("{}/v2/{repository}/{path}", self.local))
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .map_err(|failed| {
                format!("the deployment's registry could not be reached: {failed}")
            })?;
        if !answer.status().is_success() {
            return Err(format!(
                "the deployment's registry answered HTTP {} for {path}",
                answer.status()
            ));
        }
        answer
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|failed| format!("{path} did not arrive whole: {failed}"))
    }

    /// The DSSE envelopes the deployment's registry holds as referrers of
    /// `repository@digest`, each blob checked against its digest.
    pub async fn referrers(&self, repository: &str, digest: &str) -> Result<Vec<String>, String> {
        let listed = self
            .local_get(
                repository,
                &format!("manifests/{}", referrers_tag(digest)),
                INDEX,
            )
            .await
            .map_err(|why| format!("the image's referrers: {why}"))?;
        let index: serde_json::Value = serde_json::from_slice(&listed)
            .map_err(|_| "the image's referrers are not an index".to_string())?;
        let mut envelopes = Vec::new();
        for referrer in index["manifests"].as_array().into_iter().flatten() {
            let Some(child) = referrer["digest"].as_str().filter(|d| is_digest(d)) else {
                continue;
            };
            let bytes = self
                .local_get(repository, &format!("manifests/{child}"), ACCEPT)
                .await?;
            if digest_of(&bytes) != child {
                return Err(format!(
                    "the digest: the referrer {child} is not what it says"
                ));
            }
            let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            envelopes.extend(self.envelopes_of(&From::Local(repository), &manifest).await);
        }
        Ok(envelopes)
    }
}

/// A manifest being copied: its media type and bytes, once copied.
type Copying<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(String, Vec<u8>), String>> + Send + 'a>,
>;

/// Where referrers' envelopes are read from.
enum From<'a> {
    Remote(&'a Source<'a>),
    Local(&'a str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_checked_and_its_tag_schema_tag_made() {
        assert_eq!(
            digest_of(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(referrers_tag("sha256:ab"), "sha256-ab");
        assert!(is_digest(&format!("sha256:{}", "0".repeat(64))));
        assert!(!is_digest("sha256:../x"));
    }
}
