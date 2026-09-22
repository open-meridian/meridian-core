//! Setting up the bundled Zitadel for the dashboard: what the chart's setup
//! Job runs after install and after every upgrade (spec, ruling 14).
//!
//! Idempotent: every object is found by name first and made only when
//! missing, so a second run changes nothing and reports the same values. It
//! makes the dashboard's project (with any roles the values name) and its
//! OIDC client, the directory connection when one is configured -- with
//! automatic update on and `memberOf` mapped, which the trial showed must be
//! so or a removed group lingers -- the hook's two targets, and the three
//! executions that route Zitadel's calls to them.
//!
//! It holds Zitadel's admin token for as long as it runs, and nothing that
//! runs day to day ever does. What it learns goes to a [`Sink`]: two named
//! Secrets in a cluster, or files for the end-to-end run.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Value};

pub const PROJECT: &str = "meridian-dashboard";
pub const APP: &str = "meridian-dashboard";
pub const INTENT_TARGET: &str = "meridian-group-hook-intent";
pub const TOKEN_TARGET: &str = "meridian-group-hook-token";
pub const INTENT_METHOD: &str = "/zitadel.user.v2.UserService/RetrieveIdentityProviderIntent";

/// A directory reached over LDAP, brokered by Zitadel.
#[derive(Debug, Clone)]
pub struct Ldap {
    pub name: String,
    pub servers: Vec<String>,
    pub start_tls: bool,
    pub base_dn: String,
    pub bind_dn: String,
    pub bind_password: String,
    pub user_object_class: String,
    pub user_filter: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Where to reach Zitadel's API from here, e.g. its in-cluster service.
    pub api_url: String,
    /// The instance's external domain, sent as the Host Zitadel routes by
    /// when `api_url` is not the public address.
    pub host: Option<String>,
    /// What setup presents on every call: a JWT the system API user signs for
    /// itself (`system_user`). Zitadel's first-instance token is minted once
    /// and never again, so a deployment that lost it could not be administered
    /// at all; this can always be signed afresh.
    pub bearer: String,
    /// The dashboard's `/callback`.
    pub redirect_uri: String,
    /// Where Zitadel reaches the group hook.
    pub hook_url: String,
    /// Roles on the dashboard's project, standing in for groups for people
    /// made in Zitadel itself.
    pub roles: Vec<String>,
    pub ldap: Option<Ldap>,
    /// Allows an http redirect URI, for a development deployment only.
    pub dev_mode: bool,
}

/// What setup learns, keyed as the Secrets carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub project_id: String,
    pub client_id: String,
    pub intent_signing_key: String,
    pub token_signing_key: String,
    pub ldap_idp_id: Option<String>,
}

impl Outcome {
    /// The dashboard's Secret: its client, and the audience Zitadel adds.
    pub fn dashboard(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("client-id".into(), self.client_id.clone()),
            ("trusted-audiences".into(), self.project_id.clone()),
        ])
    }

    /// The hook's Secret: a key per target, and the project whose roles count.
    pub fn hook(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("intent-signing-key".into(), self.intent_signing_key.clone()),
            ("token-signing-key".into(), self.token_signing_key.clone()),
            ("project-id".into(), self.project_id.clone()),
        ])
    }
}

/// Where the outcome goes.
#[async_trait::async_trait]
pub trait Sink: Send + Sync {
    async fn put(&self, name: &str, values: &BTreeMap<String, String>) -> Result<(), String>;
}

struct Zitadel {
    /// The organisation every management call is made against.
    ///
    /// A system API user belongs to no organisation, so Zitadel cannot infer
    /// one and answers "organisation not found" until it is told. It is
    /// resolved once, from the instance's default, and sent on every call.
    org_id: std::sync::Mutex<Option<String>>,
    http: reqwest::Client,
    config: Config,
}

#[derive(Debug)]
struct Failure {
    status: u16,
    body: String,
    what: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} -> {}: {}", self.what, self.status, self.body)
    }
}

impl Zitadel {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, Failure> {
        let what = format!("{method} {path}");
        let mut request = self
            .http
            .request(
                method,
                format!("{}{path}", self.config.api_url.trim_end_matches('/')),
            )
            .bearer_auth(&self.config.bearer)
            .header("Accept", "application/json");
        if let Some(org) = self.org_id.lock().ok().and_then(|held| held.clone()) {
            request = request.header("x-zitadel-orgid", org);
        }
        if let Some(host) = &self.config.host {
            request = request.header("Host", host);
        }
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_string());
        }
        let response = request.send().await.map_err(|failed| Failure {
            status: 0,
            body: failed.to_string(),
            what: what.clone(),
        })?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(Failure {
                status,
                body: text,
                what,
            });
        }
        Ok(serde_json::from_str(&text).unwrap_or(json!({})))
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value, Failure> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    /// Make it; one that already exists is the same outcome.
    async fn ensure(&self, path: &str, body: Value) -> Result<(), Failure> {
        match self.post(path, body).await {
            Ok(_) => Ok(()),
            Err(failed) if failed.status == 409 || failed.body.contains("AlreadyExists") => Ok(()),
            Err(failed) => Err(failed),
        }
    }
}

fn equals(field: &str, value: &str) -> Value {
    json!({field: value, "method": "TEXT_QUERY_METHOD_EQUALS"})
}

fn first_id(found: &Value, list: &str, id: &str) -> Option<String> {
    found[list].as_array()?.first()?[id]
        .as_str()
        .map(String::from)
}

/// Wait until Zitadel answers an authenticated call, and learn which
/// organisation to work in.
///
/// Its readiness endpoint turns green before the gateway in front of its API
/// can reach it, so this asks something that needs both the API and the
/// credential. The instance's default organisation is that question and its
/// answer at once: a system API user is in no organisation, and every
/// management call after this names the one it found.
async fn ready(z: &Zitadel) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..90 {
        match z
            .call(reqwest::Method::GET, "/admin/v1/orgs/default", None)
            .await
        {
            Ok(found) => {
                let id = found["org"]["id"]
                    .as_str()
                    .ok_or("Zitadel named no default organisation")?;
                if let Ok(mut held) = z.org_id.lock() {
                    *held = Some(id.to_string());
                }
                return Ok(());
            }
            Err(failed) => last = failed.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(format!(
        "Zitadel did not answer within three minutes: {last}"
    ))
}

pub async fn run(
    config: Config,
    sink: &dyn Sink,
    dashboard_secret: &str,
    hook_secret: &str,
) -> Result<Outcome, String> {
    let z = Zitadel {
        org_id: std::sync::Mutex::new(None),
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|failed| failed.to_string())?,
        config,
    };
    ready(&z).await?;
    let e = |failed: Failure| failed.to_string();

    // The project, whose roles are groups for people made in Zitadel.
    let found = z
        .post(
            "/management/v1/projects/_search",
            json!({"queries": [{"nameQuery": equals("name", PROJECT)}]}),
        )
        .await
        .map_err(e)?;
    let project_id = match first_id(&found, "result", "id") {
        Some(id) => id,
        None => z
            .post(
                "/management/v1/projects",
                json!({"name": PROJECT, "projectRoleAssertion": true}),
            )
            .await
            .map_err(e)?["id"]
            .as_str()
            .ok_or("Zitadel made a project and returned no id")?
            .to_string(),
    };
    for role in &z.config.roles {
        z.ensure(
            &format!("/management/v1/projects/{project_id}/roles"),
            json!({"roleKey": role, "displayName": role}),
        )
        .await
        .map_err(e)?;
    }

    // The dashboard's client: authorisation code with PKCE, no secret.
    let found = z
        .post(
            &format!("/management/v1/projects/{project_id}/apps/_search"),
            json!({"queries": [{"nameQuery": equals("name", APP)}]}),
        )
        .await
        .map_err(e)?;
    let client_id = match found["result"].as_array().and_then(|r| r.first()) {
        Some(app) => app["oidcConfig"]["clientId"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        None => z
            .post(
                &format!("/management/v1/projects/{project_id}/apps/oidc"),
                json!({
                    "name": APP,
                    "redirectUris": [z.config.redirect_uri],
                    "responseTypes": ["OIDC_RESPONSE_TYPE_CODE"],
                    "grantTypes": ["OIDC_GRANT_TYPE_AUTHORIZATION_CODE"],
                    "appType": "OIDC_APP_TYPE_WEB",
                    "authMethodType": "OIDC_AUTH_METHOD_TYPE_NONE",
                    "devMode": z.config.dev_mode,
                    "accessTokenType": "OIDC_TOKEN_TYPE_BEARER",
                    "idTokenRoleAssertion": true,
                    "idTokenUserinfoAssertion": true,
                }),
            )
            .await
            .map_err(e)?["clientId"]
            .as_str()
            .ok_or("Zitadel made the dashboard's client and returned no id")?
            .to_string(),
    };
    if client_id.is_empty() {
        return Err("the dashboard's client exists but has no client id".into());
    }

    // The directory, when there is one, with the settings whose absence left
    // a removed group in place during the trial.
    let mut ldap_idp_id = None;
    if let Some(ldap) = &z.config.ldap {
        let found = z
            .post(
                "/admin/v1/idps/templates/_search",
                json!({"queries": [{"idpNameQuery": equals("name", &ldap.name)}]}),
            )
            .await
            .map_err(e)?;
        let id = match first_id(&found, "result", "id") {
            Some(id) => id,
            None => z
                .post(
                    "/admin/v1/idps/ldap",
                    json!({
                        "name": ldap.name,
                        "servers": ldap.servers,
                        "startTls": ldap.start_tls,
                        "baseDn": ldap.base_dn,
                        "bindDn": ldap.bind_dn,
                        "bindPassword": ldap.bind_password,
                        "userBase": "dn",
                        "userObjectClasses": [ldap.user_object_class],
                        "userFilters": [ldap.user_filter],
                        "timeout": "10s",
                        "attributes": {
                            "idAttribute": ldap.user_filter, "firstNameAttribute": "givenName",
                            "lastNameAttribute": "sn", "displayNameAttribute": "cn",
                            "preferredUsernameAttribute": ldap.user_filter, "emailAttribute": "mail",
                            "profileAttribute": "memberOf"
                        },
                        "providerOptions": {"isLinkingAllowed": true, "isCreationAllowed": true,
                                            "isAutoCreation": true, "isAutoUpdate": true}
                    }),
                )
                .await
                .map_err(e)?["id"]
                .as_str()
                .ok_or("Zitadel made the LDAP provider and returned no id")?
                .to_string(),
        };
        z.ensure("/admin/v1/policies/login/idps", json!({"idpId": id}))
            .await
            .map_err(e)?;
        ldap_idp_id = Some(id);
    }

    // The hook's targets. Zitadel reads a target's key back on search, so an
    // existing target is reused rather than re-keyed.
    let targets = z
        .post("/v2/actions/targets/search", json!({}))
        .await
        .map_err(e)?;
    let keyed = |name: &str| -> Option<(String, String)> {
        targets["targets"]
            .as_array()?
            .iter()
            .find(|t| t["name"] == name)
            .map(|t| {
                (
                    t["id"].as_str().unwrap_or_default().to_string(),
                    t["signingKey"].as_str().unwrap_or_default().to_string(),
                )
            })
    };
    let target = async |name: &str,
                        path: &str,
                        existing: Option<(String, String)>|
           -> Result<(String, String), String> {
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let made = z
            .post(
                "/v2/actions/targets",
                json!({"name": name, "endpoint": format!("{}{path}", z.config.hook_url.trim_end_matches('/')),
                       "timeout": "10s", "restCall": {"interruptOnError": true}}),
            )
            .await
            .map_err(e)?;
        Ok((
            made["id"].as_str().unwrap_or_default().to_string(),
            made["signingKey"].as_str().unwrap_or_default().to_string(),
        ))
    };
    let intent_existing = keyed(INTENT_TARGET);
    let token_existing = keyed(TOKEN_TARGET);
    let (intent_id, intent_signing_key) = target(INTENT_TARGET, "/intent", intent_existing).await?;
    let (token_id, token_signing_key) = target(TOKEN_TARGET, "/token", token_existing).await?;
    if intent_signing_key.is_empty() || token_signing_key.is_empty() {
        return Err("a target has no signing key to give the hook".into());
    }

    // Route Zitadel's calls: the intent response to /intent, both token
    // functions to /token. Setting an execution replaces it, so this is safe
    // to repeat.
    let executions = [
        (json!({"response": {"method": INTENT_METHOD}}), &intent_id),
        (json!({"function": {"name": "preaccesstoken"}}), &token_id),
        (json!({"function": {"name": "preuserinfo"}}), &token_id),
    ];
    for (condition, target) in executions {
        z.call(
            reqwest::Method::PUT,
            "/v2/actions/executions",
            Some(json!({"condition": condition, "targets": [target]})),
        )
        .await
        .map_err(e)?;
    }

    let outcome = Outcome {
        project_id,
        client_id,
        intent_signing_key,
        token_signing_key,
        ldap_idp_id,
    };
    sink.put(dashboard_secret, &outcome.dashboard()).await?;
    sink.put(hook_secret, &outcome.hook()).await?;
    Ok(outcome)
}

/// Files in a directory: one per key, for the end-to-end run.
pub struct Files(pub std::path::PathBuf);

#[async_trait::async_trait]
impl Sink for Files {
    async fn put(&self, _name: &str, values: &BTreeMap<String, String>) -> Result<(), String> {
        for (key, value) in values {
            let path = self.0.join(key);
            let partial = self.0.join(format!("{key}.partial"));
            std::fs::write(&partial, value).map_err(|failed| failed.to_string())?;
            std::fs::rename(&partial, &path).map_err(|failed| failed.to_string())?;
        }
        Ok(())
    }
}

/// Two Secrets the chart made, in this pod's namespace, updated through the
/// Kubernetes API with this pod's service account -- which may read and
/// update those two Secrets and nothing else.
pub struct Secrets {
    http: reqwest::Client,
    api: String,
    namespace: String,
    token: String,
}

const SERVICE_ACCOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

impl Secrets {
    pub fn in_cluster() -> Result<Self, String> {
        let read = |name: &str| {
            std::fs::read_to_string(format!("{SERVICE_ACCOUNT}/{name}"))
                .map_err(|failed| format!("{SERVICE_ACCOUNT}/{name}: {failed}"))
        };
        let ca = reqwest::Certificate::from_pem(read("ca.crt")?.as_bytes())
            .map_err(|failed| failed.to_string())?;
        let host = std::env::var("KUBERNETES_SERVICE_HOST").map_err(|_| "not in a cluster")?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
        Ok(Self {
            http: reqwest::Client::builder()
                .add_root_certificate(ca)
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|failed| failed.to_string())?,
            api: format!("https://{host}:{port}"),
            namespace: read("namespace")?.trim().to_string(),
            token: read("token")?.trim().to_string(),
        })
    }
}

#[async_trait::async_trait]
impl Sink for Secrets {
    async fn put(&self, name: &str, values: &BTreeMap<String, String>) -> Result<(), String> {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;

        let url = format!(
            "{}/api/v1/namespaces/{}/secrets/{name}",
            self.api, self.namespace
        );
        let data: BTreeMap<&String, String> = values
            .iter()
            .map(|(k, v)| (k, STANDARD.encode(v)))
            .collect();
        // A merge patch on a Secret the chart made: update, never create,
        // because RBAC cannot narrow `create` to a name.
        let response = self
            .http
            .patch(&url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/merge-patch+json")
            .body(json!({"data": data}).to_string())
            .send()
            .await
            .map_err(|failed| failed.to_string())?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "the Secret {name} could not be written: {status}: {body}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_secret_carries_what_its_reader_needs_and_nothing_else() {
        let outcome = Outcome {
            project_id: "P".into(),
            client_id: "C".into(),
            intent_signing_key: "KI".into(),
            token_signing_key: "KT".into(),
            ldap_idp_id: None,
        };
        let dashboard = outcome.dashboard();
        assert_eq!(
            dashboard.keys().collect::<Vec<_>>(),
            ["client-id", "trusted-audiences"]
        );
        assert!(
            !dashboard.values().any(|v| v.starts_with('K')),
            "no signing key reaches the dashboard"
        );
        let hook = outcome.hook();
        assert_eq!(
            hook.keys().collect::<Vec<_>>(),
            ["intent-signing-key", "project-id", "token-signing-key"]
        );
        assert!(
            !hook.values().any(|v| v == "C"),
            "the hook does not need the client"
        );
    }
}
