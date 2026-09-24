//! The dashboard, as its own process: the one address a firm's staff use.
//!
//! Signs people in through the firm's directory and serves what they may
//! reach. It holds no deployment key and no database of its own: it reads the
//! access records from the conductor's configuration store over the bus, as
//! the instance its launch configuration names, with the `admin` role's
//! grants. Sessions live in memory, so a restart signs everyone out.
//!
//! Refuses to serve until it has read the records once, and again whenever it
//! has not read them for 10 minutes (decisions/015).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use meridian_dashboard::directory::Directory;
use meridian_dashboard::oidc::{Oidc, OidcConfig};
use meridian_dashboard::{
    refresh, refresh_forever, router, App, RecordsCache, Sessions, SystemClock, WizardSession,
};
use meridian_runtime::{bus_from_env, now_ns, shutdown, var};

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
    // Both, because the chart knows the bundled Zitadel's issuer from the
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
            // Comma-separated. For the bundled Zitadel, the dashboard's project id.
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
        base_dn: var("MERIDIAN_LDAP_BASE_DN").unwrap_or_default(),
        bind_dn: var("MERIDIAN_LDAP_BIND_DN").unwrap_or_default(),
        bind_password: var("MERIDIAN_LDAP_BIND_PASSWORD").unwrap_or_default(),
        // `{}` is the name somebody typed, escaped before it is put here.
        user_filter: var("MERIDIAN_LDAP_USER_FILTER").unwrap_or_else(|| "(uid={})".into()),
        group_attribute: var("MERIDIAN_LDAP_GROUP_ATTRIBUTE").unwrap_or_else(|| "memberOf".into()),
    });
    if directory.is_some() && provider.is_some() {
        return Err(
            "both MERIDIAN_OIDC_ISSUER and MERIDIAN_LDAP_SERVERS are set, and this \
                    dashboard signs people in one way: whose groups a person arrived with \
                    would depend on which they used"
                .into(),
        );
    }

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
                None => {
                    // Not an error, and the ordinary state of a deployment
                    // nobody has set up yet: no directory means the wizard,
                    // which is where a directory is configured (W7).
                    tracing::info!(
                        "no directory is configured: this deployment serves its first-run wizard"
                    );
                    None
                }
            };

            // First run is the absence of a directory rather than a flag, so
            // ending it is the configuration landing and nothing anybody can
            // switch back from inside the dashboard (requirement 18).
            let first_run = oidc.is_none() && directory.is_none();

            let app = router(Arc::new(App {
                first_run,
                wizard: Arc::new(WizardSession::default()),
                records,
                sessions,
                clock,
                bus,
                oidc,
                directory: directory.map(Arc::new),
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
/// Zitadel may still be starting. Past that, exit with the reason, and let the
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
