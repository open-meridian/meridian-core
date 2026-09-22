//! What the wizard asks for, tested and then applied.
//!
//! Two questions and one command, all from the dashboard (W7.4, W7.5):
//!
//! - the sealing key, which every credential after it is sealed to;
//! - a check: does this answer work, and if not what is wrong with it;
//! - apply: write it all, in one order, idempotently, and then give up the
//!   rights that did the writing.
//!
//! A check never writes. That is worth stating because one of the answers is
//! "create Zitadel's database with this privileged connection", and the
//! difference between testing that and doing it is the difference between a
//! wizard somebody can back out of and one they cannot
//! (spec/installation-and-first-run, requirement 14).

use std::collections::BTreeMap;

use meridian_domain::v1::{
    first_run_check_request::Answer, login_backend_answer::Backend, AddressesAnswer, DatabaseLogin,
    FirstRunApplied, FirstRunCheckReply, FirstRunCheckRequest, FirstRunConfiguration,
    LoginBackendAnswer, RuntimeDatabaseAnswer,
};

use crate::cluster::Cluster;
use crate::sealing::SealingKey;

/// The names the chart gives what this writes. Passed in rather than built
/// here, because they are the release's and the Role names the same ones: a
/// name this did not get from the chart is a name RBAC would refuse anyway.
#[derive(Debug, Clone)]
pub struct Names {
    pub database_secret: String,
    pub zitadel_database_secret: String,
    pub dashboard_oidc_secret: String,
    pub ldap_bind_secret: String,
    pub zitadel_egress_policy: String,
    pub own_binding: String,
    /// Restarted after the Secrets are written, in this order.
    pub restart: Vec<String>,
    /// Scaled to zero when the firm brought its own directory.
    pub bundled_identity: Vec<String>,
}

pub struct FirstRun {
    pub key: SealingKey,
    pub names: Names,
    pub cluster: Box<dyn Cluster>,
    /// Tests a Postgres login, returning what is wrong with it. Injected so
    /// this crate's tests need no database and the real one needs no mock.
    pub probe: Box<dyn DatabaseProbe>,
}

/// What a database answer is checked against.
pub trait DatabaseProbe: Send + Sync {
    /// Connect as this login and report the findings: empty is a pass.
    ///
    /// `may_create` is what the role is expected to be able to do. The
    /// migrating role may create a table and the serving role may not, and
    /// a serving role that may is the finding worth having: it is the one
    /// that turns a restart into a migration.
    fn check(&self, login: &DatabaseLogin, password: &[u8], may_create: bool) -> Vec<String>;
}

impl FirstRun {
    /// Whether somebody has already been through the wizard.
    ///
    /// Asked once at start. A deployment that has been configured has no work
    /// for this Job, and a Job with no work gives up its rights rather than
    /// waiting with them (decisions/016).
    pub async fn already_configured(&self) -> Result<bool, String> {
        self.cluster
            .secret_has_key(&self.names.database_secret, "url")
            .await
            .map_err(|failed| failed.to_string())
    }

    /// Give up the rights without applying anything, for a Job that found its
    /// work already done.
    pub async fn stand_down(&self) -> Result<(), String> {
        self.cluster
            .drop_own_rights(&self.names.own_binding)
            .await
            .map_err(|failed| failed.to_string())
    }

    /// W7.4. Test one answer, writing nothing.
    pub fn check(&self, request: &FirstRunCheckRequest) -> FirstRunCheckReply {
        let findings = match &request.answer {
            Some(Answer::RuntimeDatabase(database)) => self.check_database(database),
            Some(Answer::LoginBackend(backend)) => self.check_backend(backend),
            Some(Answer::Addresses(addresses)) => check_addresses(addresses),
            None => vec!["nothing to check".into()],
        };
        FirstRunCheckReply {
            passed: findings.is_empty(),
            findings,
        }
    }

    fn check_database(&self, database: &RuntimeDatabaseAnswer) -> Vec<String> {
        let mut findings = Vec::new();
        for (login, may_create, what) in [
            (&database.serving, false, "serving"),
            (&database.migrating, true, "migrating"),
        ] {
            let Some(login) = login else {
                findings.push(format!("no {what} login"));
                continue;
            };
            match self.open_password(login, &format!("runtime_database.{what}.password")) {
                Err(refusal) => findings.push(refusal),
                Ok(password) => findings.extend(self.probe.check(login, &password, may_create)),
            }
        }
        findings
    }

    fn check_backend(&self, backend: &LoginBackendAnswer) -> Vec<String> {
        match &backend.backend {
            Some(Backend::Bundled(bundled)) => {
                let mut findings = Vec::new();
                if bundled.version.trim().is_empty() {
                    // Ruling 16: its version is the administrator's, and a
                    // blank one means the wizard chose for them.
                    findings.push("no Zitadel version confirmed".into());
                }
                if bundled.egress_cidrs.is_empty() {
                    findings.push(
                        "no address ranges for Zitadel to reach: it would reach nothing".into(),
                    );
                }
                findings
            }
            Some(Backend::Oidc(oidc)) => {
                let mut findings = Vec::new();
                if oidc.issuer.trim().is_empty() {
                    findings.push("no issuer".into());
                }
                if oidc.client_id.trim().is_empty() {
                    findings.push("no client id".into());
                }
                findings
            }
            None => vec!["no login backend chosen".into()],
        }
    }

    fn open_password(&self, login: &DatabaseLogin, field: &str) -> Result<Vec<u8>, String> {
        let Some(sealed) = &login.password else {
            return Err(format!("no password for {field}"));
        };
        self.key.open(sealed, field)
    }

    /// W7.5. Write everything, in one order, and give up the rights.
    ///
    /// Idempotent at every step, so applying the same configuration again
    /// completes a partial one: that is what makes a failed apply something
    /// an administrator can retry rather than a deployment to rebuild.
    pub async fn apply(&self, configuration: &FirstRunConfiguration) -> FirstRunApplied {
        let mut steps: Vec<String> = Vec::new();
        let mut applied = FirstRunApplied::default();

        if let Err(refusal) = self.write_database(configuration).await {
            applied.steps = steps;
            applied.refusal_reason = refusal;
            return applied;
        }
        steps.push("secrets".into());

        if let Err(refusal) = self.write_identity(configuration).await {
            applied.steps = steps;
            applied.refusal_reason = refusal;
            return applied;
        }
        steps.push("identity".into());

        if let Err(refusal) = self.roll(configuration).await {
            applied.steps = steps;
            applied.refusal_reason = refusal;
            return applied;
        }
        steps.push("restart".into());

        // Last, and only after every step succeeded: a Job that gave these up
        // early could not finish, and one that never gives them up leaves a
        // right in the deployment that nothing needs (decisions/016).
        match self.cluster.drop_own_rights(&self.names.own_binding).await {
            Ok(()) => {
                steps.push("rights released".into());
                applied.rights_released = true;
            }
            Err(failed) => {
                applied.steps = steps;
                applied.refusal_reason = format!("the rights could not be given up: {failed}");
                return applied;
            }
        }

        applied.applied = true;
        applied.steps = steps;
        applied
    }

    async fn write_database(&self, configuration: &FirstRunConfiguration) -> Result<(), String> {
        let Some(database) = &configuration.runtime_database else {
            return Err("no database in this configuration".into());
        };
        let serving = database
            .serving
            .as_ref()
            .ok_or("no serving login".to_string())?;
        let migrating = database
            .migrating
            .as_ref()
            .ok_or("no migrating login".to_string())?;

        let values = BTreeMap::from([
            (
                "url".to_string(),
                url_for(
                    serving,
                    &self.open_password(serving, "runtime_database.serving.password")?,
                )
                .into_bytes(),
            ),
            (
                "migrate-url".to_string(),
                url_for(
                    migrating,
                    &self.open_password(migrating, "runtime_database.migrating.password")?,
                )
                .into_bytes(),
            ),
        ]);

        self.cluster
            .put_secret(&self.names.database_secret, &values)
            .await
            .map_err(|failed| failed.to_string())
    }

    async fn write_identity(&self, configuration: &FirstRunConfiguration) -> Result<(), String> {
        let Some(backend) = configuration
            .login_backend
            .as_ref()
            .and_then(|b| b.backend.as_ref())
        else {
            return Err("no login backend in this configuration".into());
        };

        match backend {
            Backend::Bundled(bundled) => {
                if let Some(existing) = bundled
                    .database
                    .as_ref()
                    .and_then(|database| database.route.as_ref())
                {
                    self.write_zitadel_database(existing).await?;
                }
                self.cluster
                    .set_egress_cidrs(&self.names.zitadel_egress_policy, &bundled.egress_cidrs)
                    .await
                    .map_err(|failed| failed.to_string())?;
                if let Some(meridian_domain::v1::bundled_zitadel_answer::Directory::Ldap(ldap)) =
                    &bundled.directory
                {
                    let password = ldap
                        .bind_password
                        .as_ref()
                        .ok_or("no LDAP bind password".to_string())
                        .and_then(|sealed| self.key.open(sealed, "ldap.bind_password"))?;
                    self.cluster
                        .put_secret(
                            &self.names.ldap_bind_secret,
                            &BTreeMap::from([("password".to_string(), password)]),
                        )
                        .await
                        .map_err(|failed| failed.to_string())?;
                }
                Ok(())
            }
            Backend::Oidc(oidc) => {
                // The firm's own directory: the bundle is not used, so it is
                // left at zero replicas rather than waiting forever for a
                // database nobody is going to configure.
                let mut values = BTreeMap::from([
                    ("issuer".to_string(), oidc.issuer.clone().into_bytes()),
                    ("client-id".to_string(), oidc.client_id.clone().into_bytes()),
                ]);
                if let Some(sealed) = &oidc.client_secret {
                    values.insert(
                        "client-secret".to_string(),
                        self.key.open(sealed, "oidc.client_secret")?,
                    );
                }
                self.cluster
                    .put_secret(&self.names.dashboard_oidc_secret, &values)
                    .await
                    .map_err(|failed| failed.to_string())?;

                for name in &self.names.bundled_identity {
                    self.cluster
                        .scale(name, 0)
                        .await
                        .map_err(|failed| failed.to_string())?;
                }
                Ok(())
            }
        }
    }

    async fn write_zitadel_database(
        &self,
        route: &meridian_domain::v1::zitadel_database_answer::Route,
    ) -> Result<(), String> {
        use meridian_domain::v1::zitadel_database_answer::Route;

        let (login, field) = match route {
            Route::Existing(login) => (login, "zitadel_database.existing.password"),
            // Creating it is the administrator's own act, taken with a
            // privileged connection that is used once and never stored. The
            // role Zitadel holds is the one written here, never the
            // privileged one (ruling 16).
            Route::Create(create) => (
                create
                    .privileged
                    .as_ref()
                    .ok_or("no privileged connection to create Zitadel's database with")?,
                "zitadel_database.create.role_password",
            ),
        };

        let password = self.open_password(login, field)?;
        let dsn = url_for(login, &password);
        self.cluster
            .put_secret(
                &self.names.zitadel_database_secret,
                &BTreeMap::from([("dsn".to_string(), dsn.into_bytes())]),
            )
            .await
            .map_err(|failed| failed.to_string())
    }

    async fn roll(&self, _configuration: &FirstRunConfiguration) -> Result<(), String> {
        for name in &self.names.restart {
            self.cluster
                .restart(name)
                .await
                .map_err(|failed| failed.to_string())?;
        }
        Ok(())
    }
}

/// A Postgres URL, with the password escaped rather than pasted.
fn url_for(login: &DatabaseLogin, password: &[u8]) -> String {
    let password = String::from_utf8_lossy(password);
    let ssl_mode = if login.ssl_mode.trim().is_empty() {
        "verify-full"
    } else {
        login.ssl_mode.trim()
    };
    format!(
        "postgres://{}:{}@{}:{}/{}?sslmode={ssl_mode}",
        encode(&login.role),
        encode(&password),
        login.host,
        if login.port == 0 { 5432 } else { login.port },
        login.database
    )
}

/// Percent-encoding for the two fields that are a person's to choose. A
/// password with an `@` in it otherwise makes a URL that names another host.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn check_addresses(addresses: &AddressesAnswer) -> Vec<String> {
    let mut findings = Vec::new();
    if addresses.dashboard_url.trim().is_empty() {
        findings.push("no address for the dashboard: the directory sends people back to it".into());
    }
    findings
}

#[cfg(test)]
#[path = "service/tests.rs"]
mod tests;
