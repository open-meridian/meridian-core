//! The dashboard, as its own process: the one address a firm's staff use.
//!
//! Signs people in through the firm's directory and serves what they may
//! reach. It holds no deployment key, and a database only where the deployment
//! holds its own accounts (decisions/018): it reads the access records from
//! the conductor's configuration store over the bus, as the instance its
//! launch configuration names, with the `admin` role's grants. Sessions live
//! in memory, so a restart signs everyone out.
//!
//! `meridian-dashboard migrate` applies the accounts table, once per release.
//!
//! Refuses to serve until it has read the records once, and again whenever it
//! has not read them for 10 minutes (decisions/015).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use meridian_dashboard::accounts::{Accounts as _, InPostgres};
use meridian_dashboard::directory::Directory;
use meridian_dashboard::oidc::{Oidc, OidcConfig};
use meridian_dashboard::{
    refresh, refresh_forever, router, App, RecordsCache, Sessions, SystemClock, WizardSession,
};
use meridian_runtime::{bus_from_env, now_ns, required, shutdown, var};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if let Err(failed) = run() {
        tracing::error!("{failed}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    // The accounts table, once per release, as the migrating role: the
    // migrate Job's container, beside every other store's. Made in every
    // deployment whichever way it signs people in, because the Job cannot
    // know which the wizard chose, and an empty table is cheaper than a race.
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        let url = required("MERIDIAN_LOCAL_ACCOUNTS_DATABASE_URL")?;
        InPostgres::connect(&url, 1)
            .and_then(|store| store.migrate())
            .map_err(|failed| format!("the accounts schema could not be applied: {failed}"))?;
        tracing::info!("the accounts schema is applied");
        // The table belongs to the role that just made it.
        return match var("MERIDIAN_SERVING_DATABASE_URL") {
            Some(serving) => meridian_runtime::grant_serving(&url, &serving),
            None => Ok(()),
        };
    }

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "dashboard-1".into());
    let listen: SocketAddr = var("MERIDIAN_DASHBOARD_LISTEN")
        .unwrap_or_else(|| "0.0.0.0:8080".into())
        .parse()
        .map_err(|failed| format!("MERIDIAN_DASHBOARD_LISTEN is not an address: {failed}"))?;

    // Where people reach this dashboard, which is where the directory sends
    // them back to. HTTPS everywhere but a developer's machine, and cookies
    // are marked Secure whenever it is.
    // Empty on a deployment nobody has configured yet: the wizard is what
    // asks for it. A directory is what needs it, and the dashboard refuses to
    // serve one without it rather than sending people back to a guess.
    let public_url = var("MERIDIAN_DASHBOARD_URL").unwrap_or_default();
    let secure_cookies = public_url.starts_with("https://");
    // Both, because the chart may know the provider's issuer from the
    // moment it renders and the client exists only once setup has made one.
    // An issuer with no client is a deployment part-way through first run, and
    // it signs nobody in.
    if !public_url.is_empty() && !secure_cookies {
        tracing::warn!(
            %public_url,
            "this dashboard is reached over http, so its session cookies are not marked Secure"
        );
    }
    if public_url.is_empty() && var("MERIDIAN_OIDC_ISSUER").is_some() {
        return Err(
            "MERIDIAN_DASHBOARD_URL is not set, and a directory needs it: \
                    it is where people are sent back to after signing in"
                .into(),
        );
    }
    let provider = var("MERIDIAN_OIDC_ISSUER")
        .zip(var("MERIDIAN_OIDC_CLIENT_ID"))
        .map(|(issuer, client_id)| OidcConfig {
            issuer,
            client_id,
            client_secret: var("MERIDIAN_OIDC_CLIENT_SECRET"),
            redirect_url: format!("{}/callback", public_url.trim_end_matches('/')),
            groups_claim: var("MERIDIAN_OIDC_GROUPS_CLAIM").unwrap_or_else(|| "groups".into()),
            // Comma-separated. Some providers name a project or resource here.
            trusted_audiences: var("MERIDIAN_OIDC_TRUSTED_AUDIENCES")
                .map(|list| {
                    list.split(',')
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default(),
        });

    // The other route (decisions/018). At most one of these is configured:
    // two ways in would mean a person's groups depending on which they used.
    let directory = var("MERIDIAN_LDAP_SERVERS").map(|servers| Directory {
        servers: servers
            .split(',')
            .map(str::trim)
            .filter(|server| !server.is_empty())
            .map(String::from)
            .collect(),
        start_tls: var("MERIDIAN_LDAP_START_TLS").as_deref() == Some("true"),
        base_dn: var("MERIDIAN_LDAP_BASE_DN").unwrap_or_default(),
        bind_dn: var("MERIDIAN_LDAP_BIND_DN").unwrap_or_default(),
        bind_password: var("MERIDIAN_LDAP_BIND_PASSWORD").unwrap_or_default(),
        // `{}` is the name somebody typed, escaped before it is put here.
        user_filter: var("MERIDIAN_LDAP_USER_FILTER").unwrap_or_else(|| "(uid={})".into()),
        group_attribute: var("MERIDIAN_LDAP_GROUP_ATTRIBUTE").unwrap_or_else(|| "memberOf".into()),
    });
    // The third branch: accounts this deployment holds itself, in tables of
    // its own in the database it already has. Chosen by first run saying so,
    // not by a database being reachable: every deployment has one, and a
    // chart hands its address to the dashboard whichever way it signs people
    // in. Deciding by the address put every deployment on this branch, and
    // deciding by its absence -- which is what this did -- left a cluster
    // with an account in a Secret and no dashboard that would read it.
    let accounts_url = match var("MERIDIAN_LOCAL_ACCOUNTS").as_deref() {
        None => None,
        Some("on") => Some(
            required("MERIDIAN_LOCAL_ACCOUNTS_DATABASE_URL").map_err(|_| {
                "this deployment holds its own accounts, and no database was given for them: \
             MERIDIAN_LOCAL_ACCOUNTS_DATABASE_URL is not set"
                    .to_string()
            })?,
        ),
        Some(other) => {
            return Err(format!(
                "MERIDIAN_LOCAL_ACCOUNTS is {other:?}, and the only value it takes is \"on\""
            ))
        }
    };

    if [
        provider.is_some(),
        directory.is_some(),
        accounts_url.is_some(),
    ]
    .iter()
    .filter(|set| **set)
    .count()
        > 1
    {
        return Err(
            "this dashboard signs people in one way, and more than one is \
                    configured: whose groups a person arrived with would depend on \
                    which they used"
                .into(),
        );
    }
    if directory.is_some() && provider.is_some() {
        return Err(
            "both MERIDIAN_OIDC_ISSUER and MERIDIAN_LDAP_SERVERS are set, and this \
                    dashboard signs people in one way: whose groups a person arrived with \
                    would depend on which they used"
                .into(),
        );
    }

    // Before the runtime starts, because this is the blocking Postgres
    // client and it makes a runtime of its own: constructed inside one it
    // panics in a destructor, which reads as a crash with no cause.
    //
    // Connected and verified here rather than at App construction, so a
    // database that is not there stops the dashboard with a sentence rather
    // than failing at the first sign-in.
    let accounts = match &accounts_url {
        Some(url) => {
            let store = InPostgres::connect(url, 4)
                .map_err(|failed| format!("the accounts database: {failed}"))?;
            store.verify()?;
            // The first administrator, as first run left it: a name
            // and a hash in a Secret. Made here at start rather than
            // written by the Job, because the Job may not reach
            // another component's store and this is the dashboard's.
            //
            // Only when absent. A restart must not put back an
            // account somebody deleted, nor undo a password they
            // changed.
            if let (Some(name), Some(hash)) = (
                var("MERIDIAN_LOCAL_ACCOUNT_NAME"),
                var("MERIDIAN_LOCAL_ACCOUNT_PASSWORD_HASH"),
            ) {
                let existing = store
                    .by_name(&name)
                    .map_err(|failed| format!("the accounts database: {failed}"))?;
                if existing.is_none() {
                    store
                        .put(&meridian_dashboard::accounts::LocalAccount {
                            name: name.clone(),
                            display_name: var("MERIDIAN_LOCAL_ACCOUNT_DISPLAY_NAME")
                                .unwrap_or_else(|| name.clone()),
                            password_hash: hash,
                            groups: Vec::new(),
                            created_at_ns: now_ns(),
                            ..Default::default()
                        })
                        .map_err(|failed| format!("the first administrator's account: {failed}"))?;
                    tracing::info!(name, "made the first administrator's account");
                }
            }
            Some(Arc::new(store) as Arc<dyn meridian_dashboard::accounts::Accounts>)
        }
        None => None,
    };

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;
            let records = Arc::new(RecordsCache::default());
            let sessions = Arc::new(Sessions::default());
            let clock = Arc::new(SystemClock);

            // Once before listening, so the first request finds records when
            // the conductor is up. When it is not, the dashboard still
            // listens, and says why it refuses rather than failing to start.
            if let Err(failed) = refresh(&bus, &records, clock.as_ref()).await {
                tracing::warn!("the access records could not be read yet: {failed}");
            }
            tokio::spawn(refresh_forever(Arc::clone(&bus), Arc::clone(&records), clock.clone()));

            let sweeping = Arc::clone(&sessions);
            tokio::spawn(async move {
                let mut every = tokio::time::interval(Duration::from_secs(60));
                loop {
                    every.tick().await;
                    sweeping.sweep(now_ns());
                }
            });

            let oidc = match &provider {
                Some(config) => {
                    let oidc = Arc::new(discover(config).await?);
                    // The directory's keys, read again every 15 minutes, so a
                    // rotation is picked up without a restart. A sign-in that
                    // meets a key it does not know also reads them at once.
                    let refreshing = Arc::clone(&oidc);
                    tokio::spawn(async move {
                        let mut every = tokio::time::interval(Duration::from_secs(15 * 60));
                        every.tick().await;
                        loop {
                            every.tick().await;
                            if let Err(failed) = refreshing.refresh().await {
                                tracing::warn!("the directory's keys could not be read again: {failed}");
                            }
                        }
                    });
                    Some(oidc)
                }
                None => None,
            };

            // First run is the absence of a directory rather than a flag, so
            // ending it is the configuration landing and nothing anybody can
            // switch back from inside the dashboard (requirement 18).
            let first_run = oidc.is_none() && directory.is_none() && accounts.is_none();
            if first_run {
                // Not an error, and the ordinary state of a deployment nobody
                // has set up yet: no way in means the wizard, which is where
                // one is configured (W7). Said only here, where it is true --
                // it used to be said whenever there was no provider, which a
                // deployment holding its own accounts read as being in first
                // run while it was not.
                tracing::info!(
                    "no way to sign in is configured: this deployment serves its first-run wizard"
                );
            }

            let app = router(Arc::new(App {
                first_run,
                wizard: Arc::new(WizardSession::default()),
                records,
                sessions,
                clock,
                bus,
                oidc,
                directory: directory.map(Arc::new),
                accounts,
                secure_cookies,
            }));
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .map_err(|failed| format!("could not listen on {listen}: {failed}"))?;

            tracing::info!(instance_id, %listen, started_at_ns = now_ns(), "the dashboard is listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    shutdown().await;
                    tracing::info!("stopping");
                })
                .await
                .map_err(|failed| failed.to_string())
        })
}

/// The directory's discovery document, retried for a minute: a bundled
/// A provider may still be starting. Past that, exit with the reason, and let the
/// cluster restart this rather than serve a sign-in that cannot work.
async fn discover(config: &OidcConfig) -> Result<Oidc, String> {
    let mut last = String::new();
    for _ in 0..12 {
        match Oidc::discover(config).await {
            Ok(oidc) => return Ok(oidc),
            Err(failed) => {
                tracing::warn!("{failed}; retrying");
                last = failed;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
    Err(format!(
        "the directory at {} could not be reached: {last}",
        config.issuer
    ))
}
