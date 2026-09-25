use std::sync::{Arc, Mutex};

use meridian_domain::v1::{
    BundledZitadelAnswer, CreateZitadelDatabase, LdapDirectoryAnswer, OidcProviderAnswer,
    ZitadelDatabaseAnswer,
};

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
            backend: Some(Backend::Bundled(BundledZitadelAnswer {
                version: "v4.17.3".into(),
                egress_cidrs: vec!["10.20.0.0/16".into()],
                database: Some(ZitadelDatabaseAnswer {
                    route: Some(
                        meridian_domain::v1::zitadel_database_answer::Route::Existing(login(
                            &run,
                            "zitadel_database.existing.password",
                            "zitadel",
                        )),
                    ),
                }),
                directory: Some(
                    meridian_domain::v1::bundled_zitadel_answer::Directory::Ldap(
                        LdapDirectoryAnswer {
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
                    ),
                ),
                ..Default::default()
            })),
        }),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
            zitadel_url: "https://id.meridian.firm.example".into(),
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
fn a_privileged_connection_creates_nothing_during_a_check() {
    let run = first_run(Box::new(Refusing("secret")), vec![]);
    let reply = run.check(&FirstRunCheckRequest {
        answer: Some(Answer::LoginBackend(LoginBackendAnswer {
            backend: Some(Backend::Bundled(BundledZitadelAnswer {
                version: "v4.17.3".into(),
                egress_cidrs: vec!["10.0.0.0/8".into()],
                database: Some(ZitadelDatabaseAnswer {
                    route: Some(meridian_domain::v1::zitadel_database_answer::Route::Create(
                        CreateZitadelDatabase {
                            privileged: Some(login(
                                &run,
                                "zitadel_database.create.role_password",
                                "superuser",
                            )),
                            database: "zitadel".into(),
                            role: "zitadel".into(),
                            role_password: None,
                        },
                    )),
                }),
                ..Default::default()
            })),
        })),
    });

    // The cluster would have refused any write, and the check passes: it made
    // none. Testing "create this" is not creating it (requirement 14).
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
            zitadel_url: "https://id.meridian.firm.example:8443".into(),
        }),
    };

    let applied = run.apply(&configuration).await;

    assert!(applied.applied, "{}", applied.refusal_reason);
    assert!(applied.steps.contains(&"addresses".to_string()));
}

#[test]
fn a_zitadel_address_becomes_what_zitadel_calls_itself() {
    // Zitadel puts its own address in every token, and the dashboard checks
    // that against the issuer it expects: one answer, spelled both ways.
    for (url, domain, port, secure) in [
        ("https://id.example", "id.example", "443", "true"),
        ("http://id.example:8080", "id.example", "8080", "false"),
        ("https://id.example:8443/", "id.example", "8443", "true"),
    ] {
        let split = super::split_zitadel_url(url).expect("a URL");
        assert_eq!(
            split,
            (domain.to_string(), port.to_string(), secure.to_string()),
            "{url}"
        );
    }
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
    // The chart reads one issuer, out of the addresses Secret. On the bundled
    // route that is Zitadel's address; on this route it is the firm's, and it
    // was written only into the dashboard's OIDC Secret, which the chart reads
    // the client id out of and not the issuer.
    //
    // So a deployment installed with the bundle rendered -- which is what
    // keeps the choice open until the wizard -- and then pointed at the firm's
    // directory came up with no issuer, and nobody could sign in.
    let (cluster, done) = watched();
    let run = first_run(Box::new(cluster), vec![]);
    let configuration = FirstRunConfiguration {
        administrator: Some(administrator("meridian-admins")),
        runtime_database: Some(database(&run)),
        login_backend: Some(firms_own_directory("")),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
            // Empty, because this deployment is not using the bundled Zitadel.
            zitadel_url: String::new(),
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
    // ID, Okta and Zitadel use. Writing an empty one would override that
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
            backend: Some(Backend::Bundled(BundledZitadelAnswer {
                version: "v4.17.3".into(),
                egress_cidrs: vec!["10.20.0.0/16".into()],
                directory: Some(
                    meridian_domain::v1::bundled_zitadel_answer::Directory::LocalAccount(
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
                    ),
                ),
                ..Default::default()
            })),
        }),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
            zitadel_url: "https://id.meridian.firm.example".into(),
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
            backend: Some(Backend::Bundled(BundledZitadelAnswer {
                version: "v4.17.3".into(),
                egress_cidrs: vec!["10.20.0.0/16".into()],
                directory: Some(
                    meridian_domain::v1::bundled_zitadel_answer::Directory::Ldap(
                        LdapDirectoryAnswer {
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
                        },
                    ),
                ),
                ..Default::default()
            })),
        }),
        addresses: Some(AddressesAnswer {
            dashboard_url: "https://meridian.firm.example".into(),
            ..Default::default()
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
