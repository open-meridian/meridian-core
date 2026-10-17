//! The deployment's own plugin registry, as the conductor reaches it inside
//! the cluster (spec/the-local-plugin-registry; contract v25).
//!
//! The registry is reached by this release's own pods alone; from outside,
//! only the dashboard's authenticated upload writes to it, and that path
//! deletes nothing. The conductor releases a retired version's image here
//! (W8.15): its manifest deleted by digest, the layers the registry's own to
//! collect.

use std::time::Duration;

/// An OCI distribution registry at a base URL, `http://<host>:5000`.
pub struct LocalRegistry {
    base: String,
    client: reqwest::Client,
}

/// The manifest types a deletion accepts, as the upload pushed them.
const MANIFESTS: &str = "application/vnd.oci.image.manifest.v1+json, \
    application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.docker.distribution.manifest.list.v2+json";

impl LocalRegistry {
    pub fn new(base: impl Into<String>) -> Result<LocalRegistry, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|failed| format!("the registry client could not be built: {failed}"))?;
        Ok(LocalRegistry {
            base: base.into().trim_end_matches('/').to_string(),
            client,
        })
    }

    /// Release an image from a repository by its manifest's digest. Gone
    /// already is released.
    pub async fn release(&self, repository: &str, digest: &str) -> Result<(), String> {
        if !digest.starts_with("sha256:") || digest.contains('/') {
            return Err(format!("{digest:?} is not an image digest"));
        }
        if repository.split('/').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }) {
            return Err(format!("{repository:?} is not a repository here"));
        }
        let url = format!("{}/v2/{repository}/manifests/{digest}", self.base);
        let answer = self
            .client
            .delete(&url)
            .header(reqwest::header::ACCEPT, MANIFESTS)
            .send()
            .await
            .map_err(|failed| format!("the registry could not be reached: {failed}"))?;
        match answer.status().as_u16() {
            200..=299 | 404 => Ok(()),
            405 => Err("the registry does not delete; its storage's delete is off".into()),
            status => Err(format!("the registry answered {status}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn what_is_not_an_image_or_a_repository_is_never_asked() {
        let registry = LocalRegistry::new("http://127.0.0.1:9").unwrap();
        assert!(registry
            .release("plugins/snaptrade", "latest")
            .await
            .unwrap_err()
            .contains("not an image digest"));
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(registry
            .release("plugins/../etc", &digest)
            .await
            .unwrap_err()
            .contains("not a repository"));
        assert!(registry
            .release("plugins//x", &digest)
            .await
            .unwrap_err()
            .contains("not a repository"));
    }
}
