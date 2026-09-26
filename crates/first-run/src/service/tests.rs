use std::sync::{Arc, Mutex};

use meridian_domain::v1::{LdapDirectoryAnswer, OidcProviderAnswer};

use super::*;

/// An administrator for a configuration under test. Every configuration needs
/// one: a deployment nobody can administer is recovered only with a claim code
/// from the platform, so the Job refuses to write one (decisions/017).
fn administrator(group: &str) -> AdministratorAnswer {
    AdministratorAnswer {
        named: Some(Named::DirectoryGroup(group.into())),
    }
}
use crate::sealing::seal;

/// Remembers what it was asked to do, and answers yes.
///
/// The record is behind an `Arc` so a test can keep a handle on it after the
/// stand-in has been boxed into the run. It was write-only until 2026-09-23,
/// which is part of why the firm's-own-directory route went unexamined: the
/// tests could see that applying succeeded and not what it wrote.
#[derive(Default)]
struct Remembering {
    done: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Cluster for Remembering {
    async fn put_secret(
        &self,
        name: &str,
        values: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), crate::cluster::ClusterError> {
        let keys: Vec<&str> = values.keys().map(String::as_str).collect();
        self.done
            .lock()
            .unwrap()
            .push(format!("secret {name} {}", keys.join(",")));
        Ok(())
    }
    async fn put_config_map(
        &self,
        name: &str,
        values: &BTreeMap<String, String>,
    ) -> Result<(), crate::cluster::ClusterError> {
        let keys: Vec<&str> = values.keys().map(String::as_str).collect();
        self.done
            .lock()
            .unwrap()
            .push(format!("config-map {name} {}", keys.join(",")));
        Ok(())
    }

    async fn scale(
        &self,
        kind: crate::cluster::Workload,
        name: &str,
        replicas: u32,
    ) -> Result<(), crate::cluster::ClusterError> {
        self.done
            .lock()
            .unwrap()
            .push(format!("scale {kind:?} {name} {replicas}"));
        Ok(())
    }

    async fn restart(&self, name: &str) -> Result<(), crate::cluster::ClusterError> {
        self.done.lock().unwrap().push(format!("restart {name}"));
        Ok(())
    }

    async fn drop_own_rights(&self, binding: &str) -> Result<(), crate::cluster::ClusterError> {
        self.done.lock().unwrap().push(format!("dropped {binding}"));
        Ok(())
    }

    async fn secret_has_key(&self, _: &str, _: &str) -> Result<bool, crate::cluster::ClusterError> {
        Ok(false)
    }
}

/// Refuses whatever it is told to refuse, at the step named.
struct Refusing(&'static str);

impl crate::Provisioner for Refusing {
    /// A test that reaches this wanted a database made and there is none, so
    /// it says so rather than pretending one appeared.
    fn provision(&self, _: &DatabaseLogin, _: &[u8], _: &crate::Provision) -> Result<(), String> {
        Err("no database to provision in a test".into())
    }
}

#[async_trait::async_trait]
impl Cluster for Refusing {
    async fn put_secret(
        &self,
        _: &str,
        _: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), crate::cluster::ClusterError> {
        refuse_if(self.0, "secret")
    }
    async fn put_config_map(
        &self,
        _: &str,
        _: &BTreeMap<String, String>,
    ) -> Result<(), crate::cluster::ClusterError> {
        refuse_if(self.0, "config-map")
    }
    async fn scale(
        &self,
        _: crate::cluster::Workload,
        _: &str,
        _: u32,
    ) -> Result<(), crate::cluster::ClusterError> {
        refuse_if(self.0, "scale")
    }
    async fn restart(&self, _: &str) -> Result<(), crate::cluster::ClusterError> {
        refuse_if(self.0, "restart")
    }
    async fn drop_own_rights(&self, _: &str) -> Result<(), crate::cluster::ClusterError> {
        refuse_if(self.0, "drop")
    }

    async fn secret_has_key(&self, _: &str, _: &str) -> Result<bool, crate::cluster::ClusterError> {
        Ok(false)
    }
}

fn refuse_if(refusing: &str, step: &str) -> Result<(), crate::cluster::ClusterError> {
    if refusing == step {
        Err(crate::cluster::ClusterError(format!("{step} refused")))
    } else {
        Ok(())
    }
}

/// A directory that answers with these findings, and remembers the bind
/// password it was handed -- which is how a test sees the Job opened it.
#[derive(Default, Clone)]
struct Directories {
    findings: Vec<String>,
    handed: Arc<Mutex<Vec<String>>>,
}
impl DirectoryProbe for Directories {
    fn check(&self, _: &LdapDirectoryAnswer, bind_password: &[u8]) -> Vec<String> {
        self.handed
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(bind_password).into_owned());
        self.findings.clone()
    }
}

/// A provider that answers with these findings, and remembers which issuers
/// it was asked about.
#[derive(Default, Clone)]
struct Providers {
    findings: Vec<String>,
    asked: Arc<Mutex<Vec<String>>>,
}
impl ProviderProbe for Providers {
    fn check(&self, provider: &OidcProviderAnswer) -> Vec<String> {
        self.asked.lock().unwrap().push(provider.issuer.clone());
        self.findings.clone()
    }
}

struct Answers(Vec<String>);
impl DatabaseProbe for Answers {
    fn check(&self, _: &DatabaseLogin, _: &[u8], may_create: bool) -> Vec<String> {
        if may_create {
            Vec::new()
        } else {
            self.0.clone()
        }
    }
}

fn names() -> Names {
    Names {
        database_secret: "m-database".into(),
        dashboard_oidc_secret: "m-dashboard-oidc".into(),
        ldap_bind_secret: "m-ldap-bind".into(),
        addresses_secret: "m-addresses".into(),
        own_binding: "m-first-run".into(),
        restart: vec!["m-conductor".into(), "m-dashboard".into()],
    }
}

fn first_run(cluster: Box<dyn Cluster>, probe: Vec<String>) -> FirstRun {
    FirstRun {
        key: SealingKey::new("frk-1"),
        names: names(),
        cluster,
        provisioner: Box::new(Refusing("provision")),
        brought: None,
        probe: Box::new(Answers(probe)),
        directory_probe: Box::new(Directories::default()),
        provider_probe: Box::new(Providers::default()),
    }
}

fn login(run: &FirstRun, field: &str, role: &str) -> DatabaseLogin {
    DatabaseLogin {
        host: "db.firm.internal".into(),
        port: 5432,
        database: "meridian".into(),
        role: role.into(),
        password: Some(seal(&run.key.public_key(), &run.key.key_id, field, b"p@ss w/rd").unwrap()),
        ssl_mode: "verify-full".into(),
    }
}

fn database(run: &FirstRun) -> RuntimeDatabaseAnswer {
    RuntimeDatabaseAnswer {
        serving: Some(login(
            run,
            "runtime_database.serving.password",
            "meridian_app",
        )),
        migrating: Some(login(
            run,
            "runtime_database.migrating.password",
            "meridian_migrate",
        )),
        brought: None,
    }
}

#[test]
fn a_serving_role_that_may_create_tables_is_the_finding() {
    let run = first_run(
        Box::new(Remembering::default()),
        vec!["serving role meridian_app may create tables".into()],
    );
    let answer = FirstRunCheckRequest {
        answer: Some(Answer::RuntimeDatabase(database(&run))),
    };

    let reply = run.check(&answer);

    assert!(!reply.passed);
    assert_eq!(reply.findings.len(), 1, "{:?}", reply.findings);
    assert!(reply.findings[0].contains("may create tables"));
}

#[test]
fn a_database_that_passes_says_nothing() {
    let run = first_run(Box::new(Remembering::default()), vec![]);
    let reply = run.check(&FirstRunCheckRequest {
        answer: Some(Answer::RuntimeDatabase(database(&run))),
    });

    assert!(reply.passed, "{:?}", reply.findings);
    assert!(reply.findings.is_empty());
}

#[test]
fn a_credential_sealed_to_another_job_is_a_finding_not_a_crash() {
    let run = first_run(Box::new(Remembering::default()), vec![]);
    let stranger = SealingKey::new("frk-9");
    let mut database = database(&run);
    database.serving.as_mut().unwrap().password = Some(
        seal(
            &stranger.public_key(),
            &stranger.key_id,
            "runtime_database.serving.password",
            b"hunter2",
        )
        .unwrap(),
    );

    let reply = run.check(&FirstRunCheckRequest {
        answer: Some(Answer::RuntimeDatabase(database)),
    });

    assert!(!reply.passed);
    assert!(
        reply.findings[0].contains("seal it again"),
        "{:?}",
        reply.findings
    );
}

#[tokio::test]
async fn applying_writes_the_named_things_and_then_gives_up_the_rights() {
    let cluster = Box::new(Remembering::default());
    let seen = cluster as Box<dyn Cluster>;
    let run = first_run(seen, vec![]);
    let configuration = FirstRunConfiguration {
        administrator: Some(administrator("meridian-admins")),
        runtime_database: Some(database(&run)),
        login_backend: Some(LoginBackendAnswer {
            backend: Some(Backend::Ldap(
                // Whole, now that applying re-checks it: an answer
                // with no server passed until the check dialled one.
                LdapDirectoryAnswer {
                    servers: vec!["ldaps://one.firm.example".into()],
                    base_dn: "ou=people,dc=firm,dc=example".into(),
                    bind_dn: "cn=meridian,dc=firm,dc=example".into(),
                    bind_password: Some(
                        seal(
                            &run.key.public_key(),
                            &run.key.key_id,
                            "ldap.bind_password",
                            b"bind",
                        )
                        .unwrap(),
                    ),
                    ..Default::default()
                },
            )),
        }),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
        }),
    };

    let applied = run.apply(&configuration).await;

    assert!(applied.applied, "{}", applied.refusal_reason);
    assert!(applied.rights_released);
    assert_eq!(
        applied.steps,
        vec![
            "secrets",
            "identity",
            "addresses",
            "restart",
            "rights released"
        ]
    );
}

#[tokio::test]
async fn a_failed_step_keeps_the_rights_and_says_where_it_stopped() {
    let run = first_run(Box::new(Refusing("restart")), vec![]);
    let configuration = FirstRunConfiguration {
        administrator: Some(administrator("meridian-admins")),
        runtime_database: Some(database(&run)),
        login_backend: Some(LoginBackendAnswer {
            backend: Some(Backend::Oidc(OidcProviderAnswer {
                issuer: "https://directory.firm.example".into(),
                client_id: "meridian".into(),
                ..Default::default()
            })),
        }),
        addresses: None,
    };

    let applied = run.apply(&configuration).await;

    assert!(!applied.applied);
    assert!(
        !applied.rights_released,
        "a partial apply has to be retryable"
    );
    assert_eq!(applied.steps, vec!["secrets", "identity", "addresses"]);
    assert!(applied.refusal_reason.contains("restart"));
}

#[tokio::test]
async fn the_firms_own_directory_leaves_the_bundle_at_zero() {
    let run = first_run(Box::new(Remembering::default()), vec![]);
    let configuration = FirstRunConfiguration {
        administrator: Some(administrator("meridian-admins")),
        runtime_database: Some(database(&run)),
        login_backend: Some(LoginBackendAnswer {
            backend: Some(Backend::Oidc(OidcProviderAnswer {
                issuer: "https://directory.firm.example".into(),
                client_id: "meridian".into(),
                client_secret: Some(
                    seal(
                        &run.key.public_key(),
                        &run.key.key_id,
                        "oidc.client_secret",
                        b"shh",
                    )
                    .unwrap(),
                ),
                ..Default::default()
            })),
        }),
        addresses: None,
    };

    let applied = run.apply(&configuration).await;

    assert!(applied.applied, "{}", applied.refusal_reason);
}

#[test]
fn a_password_with_an_at_sign_cannot_rewrite_the_host() {
    let url = url_for(
        &DatabaseLogin {
            host: "db.firm.internal".into(),
            port: 5432,
            database: "meridian".into(),
            role: "meridian_app".into(),
            password: None,
            ssl_mode: String::new(),
        },
        b"p@ss:word/",
    );

    assert!(
        url.starts_with("postgres://meridian_app:p%40ss%3Aword%2F@db.firm.internal:5432/"),
        "{url}"
    );
    assert!(url.ends_with("?sslmode=verify-full"), "{url}");
}

#[test]
fn a_database_to_be_made_is_not_made_by_testing_it() {
    // Requirement 14: testing "start a database here and make its roles" is
    // not doing it. The cluster refuses every write and the provisioner every
    // creation, and the check passes -- it asked for neither. (This was the
    // identity server's database until that went; the route that brings the
    // runtime's own is the one left that makes anything.)
    let mut run = first_run(Box::new(Refusing("secret")), vec![]);
    run.brought = Some(BroughtServer {
        workload: "m-database".into(),
        host: "m-database".into(),
        port: 5432,
        superuser: "postgres".into(),
        superuser_password: "generated".into(),
        serving_password: "generated".into(),
        migrating_password: "generated".into(),
    });
    let reply = run.check(&FirstRunCheckRequest {
        answer: Some(Answer::RuntimeDatabase(RuntimeDatabaseAnswer {
            brought: Some(BroughtDatabase {
                serving_role: "meridian_app".into(),
                migrating_role: "meridian_migrate".into(),
                database: "meridian".into(),
            }),
            ..Default::default()
        })),
    });

    assert!(reply.passed, "{:?}", reply.findings);
}

#[tokio::test]
async fn the_addresses_are_written_as_the_components_read_them() {
    let remembering = Remembering::default();
    let run = FirstRun {
        key: SealingKey::new("frk-1"),
        names: names(),
        cluster: Box::new(remembering),
        provisioner: Box::new(Refusing("provision")),
        brought: None,
        probe: Box::new(Answers(vec![])),
        directory_probe: Box::new(Directories::default()),
        provider_probe: Box::new(Providers::default()),
    };
    let configuration = FirstRunConfiguration {
        administrator: Some(administrator("meridian-admins")),
        runtime_database: Some(database(&run)),
        login_backend: Some(LoginBackendAnswer {
            backend: Some(Backend::Oidc(OidcProviderAnswer {
                issuer: "https://directory.firm.example".into(),
                client_id: "meridian".into(),
                ..Default::default()
            })),
        }),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
        }),
    };

    let applied = run.apply(&configuration).await;

    assert!(applied.applied, "{}", applied.refusal_reason);
    assert!(applied.steps.contains(&"addresses".to_string()));
}

/// What the run wrote, as `secret <name> <comma-joined keys>` and so on.
fn watched() -> (Remembering, Arc<Mutex<Vec<String>>>) {
    let cluster = Remembering::default();
    let done = cluster.done.clone();
    (cluster, done)
}

fn firms_own_directory(groups_claim: &str) -> LoginBackendAnswer {
    LoginBackendAnswer {
        backend: Some(Backend::Oidc(OidcProviderAnswer {
            issuer: "https://directory.firm.example/".into(),
            client_id: "meridian".into(),
            groups_claim: groups_claim.into(),
            ..Default::default()
        })),
    }
}

#[tokio::test]
async fn the_firms_own_issuer_reaches_the_secret_the_dashboard_reads() {
    // The chart reads one issuer, out of the addresses Secret. Until
    // 2026-09-23 the firm's was written only into the dashboard's OIDC Secret,
    // which the chart reads the client id out of and not the issuer, so a
    // deployment pointed at the firm's provider came up with no issuer and
    // nobody could sign in.
    let (cluster, done) = watched();
    let run = first_run(Box::new(cluster), vec![]);
    let configuration = FirstRunConfiguration {
        administrator: Some(administrator("meridian-admins")),
        runtime_database: Some(database(&run)),
        login_backend: Some(firms_own_directory("")),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
        }),
    };

    let applied = run.apply(&configuration).await;

    assert!(applied.applied, "{}", applied.refusal_reason);
    let wrote = done.lock().unwrap().clone();
    let addresses = wrote
        .iter()
        .find(|line| line.starts_with("secret m-addresses "))
        .expect("the addresses secret was written");
    assert!(
        addresses.contains("issuer"),
        "the dashboard reads its issuer from here: {addresses}"
    );
}

#[tokio::test]
async fn the_groups_claim_is_written_only_when_the_wizard_was_told_one() {
    // Absent means the dashboard's own default, `groups`, which is what Entra
    // ID and Okta use. Writing an empty one would override that
    // default with nothing, and a claim of "" matches no claim at all: every
    // token would present no groups, the administrators' group would never
    // match, and nobody would hold deployment admin.
    for (given, expected) in [("roles", true), ("", false)] {
        let (cluster, done) = watched();
        let run = first_run(Box::new(cluster), vec![]);
        let configuration = FirstRunConfiguration {
            administrator: Some(administrator("meridian-admins")),
            runtime_database: Some(database(&run)),
            login_backend: Some(firms_own_directory(given)),
            addresses: None,
        };

        let applied = run.apply(&configuration).await;

        assert!(applied.applied, "{}", applied.refusal_reason);
        let wrote = done.lock().unwrap().clone();
        let oidc = wrote
            .iter()
            .find(|line| line.starts_with("secret m-dashboard-oidc "))
            .expect("the dashboard's OIDC secret was written")
            .clone();
        assert_eq!(
            oidc.contains("groups-claim"),
            expected,
            "given {given:?}: {oidc}"
        );
    }
}

#[tokio::test]
async fn the_first_administrators_account_is_written_where_the_dashboard_will_find_it() {
    // Collected and discarded until 2026-09-24. The wizard sealed a login, a
    // name and a password into `LocalAccountAnswer` and nothing opened it, so
    // the branch for a firm with no directory produced a deployment holding
    // an administrator's permission and no account to sign in as.
    //
    // Nothing caught it because the e2e asserted the permission, which was
    // written, rather than a sign-in, which was impossible.
    let (cluster, done) = watched();
    let run = first_run(Box::new(cluster), vec![]);
    let configuration = FirstRunConfiguration {
        administrator: Some(AdministratorAnswer {
            named: Some(Named::LocalAccountLogin("ada".into())),
        }),
        runtime_database: Some(database(&run)),
        login_backend: Some(LoginBackendAnswer {
            backend: Some(Backend::LocalAccount(
                meridian_domain::v1::LocalAccountAnswer {
                    login_name: "Ada".into(),
                    given_name: "Ada".into(),
                    family_name: "Park".into(),
                    initial_password: Some(
                        seal(
                            &run.key.public_key(),
                            &run.key.key_id,
                            "local_account.initial_password",
                            b"correct horse battery",
                        )
                        .unwrap(),
                    ),
                    ..Default::default()
                },
            )),
        }),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
        }),
    };

    let applied = run.apply(&configuration).await;

    assert!(applied.applied, "{}", applied.refusal_reason);
    let wrote = done.lock().unwrap().clone();
    let addresses = wrote
        .iter()
        .find(|line| line.starts_with("secret m-addresses "))
        .expect("the addresses secret was written");

    // The hash, which is what the dashboard makes the account from.
    assert!(
        addresses.contains("administrator-password-hash"),
        "the first administrator's password went nowhere: {addresses}"
    );
    // The account's name, for the dashboard to make it under, and the login
    // the permission names, for the conductor: two keys, because they are two
    // strings (`ada` and `local|ada`).
    assert!(addresses.contains("local-account-name"), "{addresses}");
    assert!(addresses.contains("administrator-login"), "{addresses}");
    // And the switch, without which the dashboard read none of it: on a
    // cluster it came back up in first run, serving its wizard again.
    assert!(addresses.contains("local-accounts"), "{addresses}");
}

#[tokio::test]
async fn the_firms_ldap_connection_reaches_the_dashboard_not_only_its_password() {
    // The dashboard binds to this directory itself (decisions/018), so what
    // it needs is the whole connection. Only the password was written until
    // 2026-09-24, because the rest was configured into the identity server
    // that used to do the binding -- so the dashboard would have come up
    // knowing a password and no server to send it to.
    let (cluster, done) = watched();
    let run = first_run(Box::new(cluster), vec![]);
    let configuration = FirstRunConfiguration {
        administrator: Some(administrator("meridian-admins")),
        runtime_database: Some(database(&run)),
        login_backend: Some(LoginBackendAnswer {
            backend: Some(Backend::Ldap(LdapDirectoryAnswer {
                servers: vec![
                    "ldaps://one.firm.example".into(),
                    "ldaps://two.firm.example".into(),
                ],
                base_dn: "ou=people,dc=firm,dc=example".into(),
                bind_dn: "cn=meridian,dc=firm,dc=example".into(),
                bind_password: Some(
                    seal(
                        &run.key.public_key(),
                        &run.key.key_id,
                        "ldap.bind_password",
                        b"bind-secret",
                    )
                    .unwrap(),
                ),
                ..Default::default()
            })),
        }),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
        }),
    };

    let applied = run.apply(&configuration).await;

    assert!(applied.applied, "{}", applied.refusal_reason);
    let wrote = done.lock().unwrap().clone();
    let addresses = wrote
        .iter()
        .find(|line| line.starts_with("secret m-addresses "))
        .expect("the addresses secret was written");
    // One way in: a firm with a directory gets no accounts of this
    // deployment's own, and a dashboard given both refuses to start.
    assert!(!addresses.contains("local-accounts"), "{addresses}");
    let ldap = wrote
        .iter()
        .find(|line| line.starts_with("secret m-ldap-bind "))
        .expect("the directory secret was written");

    for key in ["password", "servers", "base-dn", "bind-dn", "user-filter"] {
        assert!(ldap.contains(key), "{key} is missing from {ldap}");
    }
}

fn ldap_answer(run: &FirstRun, filter: &str) -> LoginBackendAnswer {
    LoginBackendAnswer {
        backend: Some(Backend::Ldap(LdapDirectoryAnswer {
            servers: vec!["ldaps://one.firm.example".into()],
            base_dn: "ou=people,dc=firm,dc=example".into(),
            bind_dn: "cn=meridian,dc=firm,dc=example".into(),
            bind_password: Some(
                seal(
                    &run.key.public_key(),
                    &run.key.key_id,
                    "ldap.bind_password",
                    b"bind-secret",
                )
                .unwrap(),
            ),
            user_filter: filter.into(),
            ..Default::default()
        })),
    }
}

fn check_backend_with(run: &FirstRun, backend: LoginBackendAnswer) -> FirstRunCheckReply {
    run.check(&FirstRunCheckRequest {
        answer: Some(Answer::LoginBackend(backend)),
    })
}

#[test]
fn an_ldap_answer_is_bound_to_with_the_password_the_wizard_sealed() {
    // Until 2026-09-25 every LDAP answer passed without anything being
    // dialled, so a wrong bind password was found by the first sign-in.
    let directories = Directories::default();
    let handed = directories.handed.clone();
    let mut run = first_run(Box::new(Remembering::default()), vec![]);
    run.directory_probe = Box::new(directories);

    let reply = check_backend_with(&run, ldap_answer(&run, ""));

    assert!(reply.passed, "{:?}", reply.findings);
    assert_eq!(
        *handed.lock().unwrap(),
        ["bind-secret"],
        "opened and handed over"
    );
}

#[test]
fn what_the_directory_says_is_wrong_is_what_the_wizard_shows() {
    let mut run = first_run(Box::new(Remembering::default()), vec![]);
    run.directory_probe = Box::new(Directories {
        findings: vec!["this deployment could not sign in to the directory".into()],
        ..Default::default()
    });

    let reply = check_backend_with(&run, ldap_answer(&run, ""));

    assert!(!reply.passed);
    assert_eq!(
        reply.findings,
        ["this deployment could not sign in to the directory"]
    );
}

#[test]
fn an_ldap_answer_missing_half_of_itself_is_told_so_without_dialling() {
    let directories = Directories::default();
    let handed = directories.handed.clone();
    let mut run = first_run(Box::new(Remembering::default()), vec![]);
    run.directory_probe = Box::new(directories);
    let mut answer = ldap_answer(&run, "(mail=alice@firm.example)");
    if let Some(Backend::Ldap(ldap)) = answer.backend.as_mut() {
        ldap.servers.clear();
        ldap.base_dn.clear();
    }

    let reply = check_backend_with(&run, answer);

    assert!(!reply.passed);
    assert_eq!(reply.findings.len(), 3, "{:?}", reply.findings);
    assert!(reply
        .findings
        .iter()
        .any(|f| f.contains("no directory server")));
    assert!(reply.findings.iter().any(|f| f.contains("no base DN")));
    // A filter naming one person finds them whoever signs in.
    assert!(reply.findings.iter().any(|f| f.contains("has no {}")));
    assert!(handed.lock().unwrap().is_empty(), "nothing was dialled");
}

#[test]
fn a_local_account_answer_dials_nothing() {
    let directories = Directories::default();
    let handed = directories.handed.clone();
    let mut run = first_run(Box::new(Remembering::default()), vec![]);
    run.directory_probe = Box::new(directories);

    let reply = check_backend_with(
        &run,
        LoginBackendAnswer {
            backend: Some(Backend::LocalAccount(Default::default())),
        },
    );

    assert!(reply.passed, "{:?}", reply.findings);
    assert!(handed.lock().unwrap().is_empty());
}

fn provider_answer(issuer: &str) -> LoginBackendAnswer {
    LoginBackendAnswer {
        backend: Some(Backend::Oidc(OidcProviderAnswer {
            issuer: issuer.into(),
            client_id: "meridian".into(),
            ..Default::default()
        })),
    }
}

#[test]
fn a_providers_issuer_is_asked_of_the_provider_exactly_as_given() {
    // Exactly: the trailing slash is the provider's to say, and a check that
    // tidied it would pass an issuer every token then contradicts.
    let providers = Providers::default();
    let asked = providers.asked.clone();
    let mut run = first_run(Box::new(Remembering::default()), vec![]);
    run.provider_probe = Box::new(providers);

    let reply = check_backend_with(&run, provider_answer("https://tenant.auth.example/"));

    assert!(reply.passed, "{:?}", reply.findings);
    assert_eq!(*asked.lock().unwrap(), ["https://tenant.auth.example/"]);
}

#[test]
fn what_the_provider_says_is_wrong_is_what_the_wizard_shows() {
    let mut run = first_run(Box::new(Remembering::default()), vec![]);
    run.provider_probe = Box::new(Providers {
        findings: vec!["unexpected issuer URI".into()],
        ..Default::default()
    });

    let reply = check_backend_with(&run, provider_answer("https://directory.firm.example"));

    assert!(!reply.passed);
    assert_eq!(reply.findings, ["unexpected issuer URI"]);
}

#[test]
fn a_provider_answer_missing_its_issuer_is_told_so_without_asking_anybody() {
    let providers = Providers::default();
    let asked = providers.asked.clone();
    let mut run = first_run(Box::new(Remembering::default()), vec![]);
    run.provider_probe = Box::new(providers);

    let reply = check_backend_with(&run, provider_answer(" "));

    assert_eq!(reply.findings, ["no issuer"]);
    assert!(asked.lock().unwrap().is_empty(), "nothing was fetched");
}

#[tokio::test]
async fn trusted_audiences_reach_the_key_the_chart_reads_them_from() {
    // The chart has read `trusted-audiences` out of the OIDC Secret all along,
    // and until 2026-09-25 nothing wrote it: a provider that names its
    // project in a token's audience had every sign-in refused.
    let (cluster, done) = watched();
    let run = first_run(Box::new(cluster), vec![]);
    let mut backend = firms_own_directory("");
    if let Some(Backend::Oidc(oidc)) = backend.backend.as_mut() {
        oidc.trusted_audiences = vec!["project-8812".into(), " ".into()];
    }
    let configuration = FirstRunConfiguration {
        administrator: Some(administrator("meridian-admins")),
        runtime_database: Some(database(&run)),
        login_backend: Some(backend),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
        }),
    };

    let applied = run.apply(&configuration).await;

    assert!(applied.applied, "{}", applied.refusal_reason);
    let wrote = done.lock().unwrap().clone();
    let oidc = wrote
        .iter()
        .find(|line| line.starts_with("secret m-dashboard-oidc "))
        .expect("the OIDC secret was written");
    assert!(oidc.contains("trusted-audiences"), "{oidc}");
}

#[tokio::test]
async fn start_tls_reaches_the_dashboard_only_when_it_was_asked_for() {
    for (asked, expected) in [(true, true), (false, false)] {
        let (cluster, done) = watched();
        let run = first_run(Box::new(cluster), vec![]);
        let mut backend = ldap_answer(&run, "");
        if let Some(Backend::Ldap(ldap)) = backend.backend.as_mut() {
            ldap.servers = vec!["ldap://ldap.firm.example:389".into()];
            ldap.start_tls = asked;
        }
        let configuration = FirstRunConfiguration {
            administrator: Some(administrator("meridian-admins")),
            runtime_database: Some(database(&run)),
            login_backend: Some(backend),
            addresses: Some(AddressesAnswer {
                dashboard_url: "https://meridian.firm.example".into(),
            }),
        };

        let applied = run.apply(&configuration).await;

        assert!(applied.applied, "{}", applied.refusal_reason);
        let wrote = done.lock().unwrap().clone();
        let ldap = wrote
            .iter()
            .find(|line| line.starts_with("secret m-ldap-bind "))
            .expect("the directory secret was written");
        assert_eq!(
            ldap.contains("start-tls"),
            expected,
            "asked {asked}: {ldap}"
        );
    }
}

#[test]
fn start_tls_on_an_address_already_encrypted_is_refused_before_dialling() {
    let directories = Directories::default();
    let handed = directories.handed.clone();
    let mut run = first_run(Box::new(Remembering::default()), vec![]);
    run.directory_probe = Box::new(directories);
    let mut answer = ldap_answer(&run, "");
    if let Some(Backend::Ldap(ldap)) = answer.backend.as_mut() {
        ldap.start_tls = true;
    }

    let reply = check_backend_with(&run, answer);

    assert!(!reply.passed);
    assert!(
        reply
            .findings
            .iter()
            .any(|f| f.contains("already encrypted")),
        "{:?}",
        reply.findings
    );
    assert!(handed.lock().unwrap().is_empty(), "nothing was dialled");
}
