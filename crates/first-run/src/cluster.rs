//! The four things first run is allowed to do to a cluster, and nothing else.
//!
//! Written against the Kubernetes API directly rather than through a client
//! library, because the whole surface is four calls and a library would bring
//! a model of every resource that exists to make them. What bounds this is not
//! the code here but the Role the chart renders: it names each resource, it
//! permits `create` on none of them, and this deletes its own binding when the
//! configuration is applied (decisions/016).
//!
//! - **Update a named Secret**: the database, the directory, the
//!   directory's settings. Update, never create: the chart makes them empty so
//!   that RBAC can name them, since `create` cannot be narrowed to a name.
//! - **Restart a named Deployment**: what makes a component read what was
//!   just written.
//! - **Delete its own RoleBinding**: which is what "gives up its rights"
//!   means, rather than intends.

use std::collections::BTreeMap;

/// What the API answered, or why it could not be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterError(pub String);

impl std::fmt::Display for ClusterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What is being scaled, because the two live at different paths and the Job's
/// Role names each one separately.
///
/// The bundled directory is a Deployment; the database this chart can bring is
/// a StatefulSet, so that its claim comes from a volumeClaimTemplate and does
/// not exist until somebody chooses it (decisions/016).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    Deployment,
    StatefulSet,
}

impl Workload {
    fn path(self) -> &'static str {
        match self {
            Workload::Deployment => "deployments",
            Workload::StatefulSet => "statefulsets",
        }
    }
}

/// The calls first run makes, as a trait so its own tests need no cluster.
#[async_trait::async_trait]
pub trait Cluster: Send + Sync {
    /// Merge keys into a Secret that already exists. Values are the plain
    /// bytes; the encoding is this implementation's business.
    async fn put_secret(
        &self,
        name: &str,
        values: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), ClusterError>;

    /// Merge keys into a ConfigMap that already exists: what is public, such
    /// as the half of the dashboard's key every sidecar verifies with.
    async fn put_config_map(
        &self,
        name: &str,
        values: &BTreeMap<String, String>,
    ) -> Result<(), ClusterError>;

    /// How many replicas a named workload should run. One is how the
    /// database this chart can bring is started (decisions/016).
    async fn scale(&self, kind: Workload, name: &str, replicas: u32) -> Result<(), ClusterError>;

    /// Roll a named Deployment, so it reads what was just written.
    async fn restart(&self, name: &str) -> Result<(), ClusterError>;

    /// Delete this Job's own RoleBinding.
    async fn drop_own_rights(&self, binding: &str) -> Result<(), ClusterError>;

    /// Whether a named Secret already holds a key.
    ///
    /// How this Job knows a deployment has been configured already: the
    /// database Secret holds its URL only because somebody went through the
    /// wizard. A Job that finds one gives up its rights and exits, so
    /// installing the same release twice leaves nothing holding them.
    async fn secret_has_key(&self, name: &str, key: &str) -> Result<bool, ClusterError>;
}

/// The real one: the in-cluster API, reached with the pod's own service
/// account.
pub struct ApiServer {
    http: reqwest::Client,
    base: String,
    namespace: String,
    token: String,
}

/// Where a pod finds its own credentials, which is also what makes this
/// runnable nowhere else.
const TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
const NAMESPACE_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

impl ApiServer {
    /// Read the pod's own service account and trust the cluster's CA.
    ///
    /// The CA is read rather than skipped: a Job that accepted any certificate
    /// would accept anything that got in front of the API, and it is writing
    /// credentials.
    pub fn in_cluster() -> Result<Self, ClusterError> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST").map_err(|_| {
            ClusterError(
                "KUBERNETES_SERVICE_HOST is not set: this runs in a pod or not at all".into(),
            )
        })?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_string());

        let token = std::fs::read_to_string(TOKEN_PATH)
            .map_err(|failed| ClusterError(format!("no service account token: {failed}")))?;
        let namespace = std::fs::read_to_string(NAMESPACE_PATH)
            .map_err(|failed| ClusterError(format!("no namespace: {failed}")))?;
        let ca = std::fs::read(CA_PATH)
            .map_err(|failed| ClusterError(format!("no cluster CA: {failed}")))?;
        let ca = reqwest::Certificate::from_pem(&ca)
            .map_err(|failed| ClusterError(format!("the cluster CA is unreadable: {failed}")))?;

        let http = reqwest::Client::builder()
            .add_root_certificate(ca)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|failed| ClusterError(failed.to_string()))?;

        Ok(Self {
            http,
            base: format!("https://{host}:{port}"),
            namespace: namespace.trim().to_string(),
            token: token.trim().to_string(),
        })
    }

    async fn patch(
        &self,
        path: &str,
        content_type: &str,
        body: serde_json::Value,
    ) -> Result<(), ClusterError> {
        let url = format!("{}{path}", self.base);
        let response = self
            .http
            .patch(&url)
            .bearer_auth(&self.token)
            .header("Content-Type", content_type)
            .body(body.to_string())
            .send()
            .await
            .map_err(|failed| ClusterError(format!("PATCH {path}: {failed}")))?;

        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        // The body, because a 403 here names the resource RBAC did not allow
        // and that is the whole diagnosis.
        let detail = response.text().await.unwrap_or_default();
        Err(ClusterError(format!("PATCH {path} -> {status}: {detail}")))
    }
}

#[async_trait::async_trait]
impl Cluster for ApiServer {
    async fn put_secret(
        &self,
        name: &str,
        values: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), ClusterError> {
        use base64::Engine;

        let data: serde_json::Map<String, serde_json::Value> = values
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    serde_json::Value::String(
                        base64::engine::general_purpose::STANDARD.encode(value),
                    ),
                )
            })
            .collect();

        // A merge patch, so keys this deployment already holds and this
        // configuration does not mention are left alone.
        self.patch(
            &format!("/api/v1/namespaces/{}/secrets/{name}", self.namespace),
            "application/merge-patch+json",
            serde_json::json!({ "data": data }),
        )
        .await
    }

    async fn put_config_map(
        &self,
        name: &str,
        values: &BTreeMap<String, String>,
    ) -> Result<(), ClusterError> {
        // The same merge patch as a Secret's, in plain text.
        self.patch(
            &format!("/api/v1/namespaces/{}/configmaps/{name}", self.namespace),
            "application/merge-patch+json",
            serde_json::json!({ "data": values }),
        )
        .await
    }

    async fn scale(&self, kind: Workload, name: &str, replicas: u32) -> Result<(), ClusterError> {
        self.patch(
            &format!(
                "/apis/apps/v1/namespaces/{}/{}/{name}/scale",
                self.namespace,
                kind.path()
            ),
            "application/merge-patch+json",
            serde_json::json!({"spec": {"replicas": replicas}}),
        )
        .await
    }

    async fn restart(&self, name: &str) -> Result<(), ClusterError> {
        // The annotation `kubectl rollout restart` writes, for the same
        // reason: it changes the pod template, and changing the pod template
        // is what a rollout is.
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();

        self.patch(
            &format!(
                "/apis/apps/v1/namespaces/{}/deployments/{name}",
                self.namespace
            ),
            "application/merge-patch+json",
            serde_json::json!({
                "spec": {"template": {"metadata": {"annotations": {
                    "meridian.dev/restarted-at": at.to_string()
                }}}}
            }),
        )
        .await
    }

    async fn secret_has_key(&self, name: &str, key: &str) -> Result<bool, ClusterError> {
        let path = format!("/api/v1/namespaces/{}/secrets/{name}", self.namespace);
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|failed| ClusterError(format!("GET {path}: {failed}")))?;

        let status = response.status().as_u16();
        if status == 404 {
            return Ok(false);
        }
        if !(200..300).contains(&status) {
            let detail = response.text().await.unwrap_or_default();
            return Err(ClusterError(format!("GET {path} -> {status}: {detail}")));
        }

        let body = response
            .text()
            .await
            .map_err(|failed| ClusterError(format!("GET {path}: {failed}")))?;
        let body: serde_json::Value = serde_json::from_str(&body)
            .map_err(|failed| ClusterError(format!("GET {path}: {failed}")))?;
        Ok(body["data"]
            .get(key)
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.is_empty()))
    }

    async fn drop_own_rights(&self, binding: &str) -> Result<(), ClusterError> {
        let path = format!(
            "/apis/rbac.authorization.k8s.io/v1/namespaces/{}/rolebindings/{binding}",
            self.namespace
        );
        let response = self
            .http
            .delete(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|failed| ClusterError(format!("DELETE {path}: {failed}")))?;

        let status = response.status().as_u16();
        // Gone already is the state this call is for, so it is not a failure.
        if (200..300).contains(&status) || status == 404 {
            return Ok(());
        }
        let detail = response.text().await.unwrap_or_default();
        Err(ClusterError(format!("DELETE {path} -> {status}: {detail}")))
    }
}
