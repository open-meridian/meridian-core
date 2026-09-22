//! The group hook, as its own process: Zitadel's Actions v2 target for
//! carrying directory groups into the dashboard's token.
//!
//! Runs only where the chart bundles Zitadel. It holds one kind of
//! secret, its two targets' signing keys, reaches nothing, and is reached only by Zitadel,
//! which a NetworkPolicy says. Every call is refused unless Zitadel signed it.

use std::net::SocketAddr;
use std::sync::Arc;

use meridian_group_hook::{router, system_now_s, Hook, SamlAttributes};
use meridian_runtime::{now_ns, required, shutdown, var};

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
    if std::env::args().nth(1).as_deref() == Some("setup") {
        return setup();
    }
    let listen: SocketAddr = var("MERIDIAN_GROUP_HOOK_LISTEN")
        .unwrap_or_else(|| "0.0.0.0:8090".into())
        .parse()
        .map_err(|failed| format!("MERIDIAN_GROUP_HOOK_LISTEN is not an address: {failed}"))?;
    let defaults = SamlAttributes::default();
    let hook = Arc::new(Hook {
        // One key per target: Zitadel keys each target on its own, and the
        // two calls arrive through two targets.
        intent_signing_key: required("MERIDIAN_GROUP_HOOK_INTENT_SIGNING_KEY")?.into_bytes(),
        token_signing_key: required("MERIDIAN_GROUP_HOOK_TOKEN_SIGNING_KEY")?.into_bytes(),
        project_id: required("MERIDIAN_GROUP_HOOK_PROJECT_ID")?,
        saml: SamlAttributes {
            groups: var("MERIDIAN_SAML_GROUPS_ATTRIBUTE").unwrap_or(defaults.groups),
            username: var("MERIDIAN_SAML_USERNAME_ATTRIBUTE").unwrap_or(defaults.username),
            given_name: var("MERIDIAN_SAML_GIVEN_NAME_ATTRIBUTE").unwrap_or(defaults.given_name),
            family_name: var("MERIDIAN_SAML_FAMILY_NAME_ATTRIBUTE").unwrap_or(defaults.family_name),
            email: var("MERIDIAN_SAML_EMAIL_ATTRIBUTE").unwrap_or(defaults.email),
        },
        now_s: system_now_s,
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .map_err(|failed| format!("could not listen on {listen}: {failed}"))?;
            tracing::info!(%listen, started_at_ns = now_ns(), "the group hook is listening");
            axum::serve(listener, router(hook))
                .with_graceful_shutdown(async {
                    shutdown().await;
                    tracing::info!("stopping");
                })
                .await
                .map_err(|failed| failed.to_string())
        })
}

/// A value from the environment, or from the file `<NAME>_FILE` names, waited
/// for: Zitadel writes its admin token only once its own setup has run.
fn secret(name: &str) -> Result<String, String> {
    if let Some(value) = var(name) {
        return Ok(value.trim().to_string());
    }
    let file = required(&format!("{name}_FILE"))?;
    for _ in 0..90 {
        if let Ok(value) = std::fs::read_to_string(&file) {
            if !value.trim().is_empty() {
                return Ok(value.trim().to_string());
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    Err(format!("{file} did not appear within three minutes"))
}

fn list(name: &str) -> Vec<String> {
    var(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

fn setup() -> Result<(), String> {
    use meridian_group_hook::setup::{self, Config, Files, Ldap, Secrets, Sink};

    let ldap = match var("MERIDIAN_LDAP_SERVERS") {
        None => None,
        Some(_) => Some(Ldap {
            name: var("MERIDIAN_LDAP_NAME").unwrap_or_else(|| "Directory".into()),
            servers: list("MERIDIAN_LDAP_SERVERS"),
            start_tls: var("MERIDIAN_LDAP_START_TLS").as_deref() == Some("true"),
            base_dn: required("MERIDIAN_LDAP_BASE_DN")?,
            bind_dn: required("MERIDIAN_LDAP_BIND_DN")?,
            bind_password: secret("MERIDIAN_LDAP_BIND_PASSWORD")?,
            user_object_class: var("MERIDIAN_LDAP_USER_OBJECT_CLASS")
                .unwrap_or_else(|| "inetOrgPerson".into()),
            user_filter: var("MERIDIAN_LDAP_USER_FILTER").unwrap_or_else(|| "uid".into()),
        }),
    };
    let config = Config {
        api_url: required("MERIDIAN_ZITADEL_API_URL")?,
        host: var("MERIDIAN_ZITADEL_HOST"),
        admin_token: secret("MERIDIAN_ZITADEL_ADMIN_TOKEN")?,
        redirect_uri: required("MERIDIAN_DASHBOARD_REDIRECT_URI")?,
        hook_url: required("MERIDIAN_GROUP_HOOK_URL")?,
        roles: list("MERIDIAN_ZITADEL_ROLES"),
        ldap,
        dev_mode: var("MERIDIAN_ZITADEL_DEV_MODE").as_deref() == Some("true"),
    };
    // "secrets:<dashboard>,<hook>" in a cluster, "files:<dir>" otherwise.
    let output = required("MERIDIAN_SETUP_OUTPUT")?;
    let (sink, dashboard, hook): (Box<dyn Sink>, String, String) = match output.split_once(':') {
        Some(("secrets", names)) => {
            let (dashboard, hook) = names
                .split_once(',')
                .ok_or("MERIDIAN_SETUP_OUTPUT is secrets:<dashboard>,<hook>")?;
            (
                Box::new(Secrets::in_cluster()?),
                dashboard.into(),
                hook.into(),
            )
        }
        Some(("files", dir)) => (Box::new(Files(dir.into())), String::new(), String::new()),
        _ => return Err("MERIDIAN_SETUP_OUTPUT is secrets:<a>,<b> or files:<dir>".into()),
    };

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let outcome = setup::run(config, sink.as_ref(), &dashboard, &hook).await?;
            tracing::info!(
                project = outcome.project_id,
                client = outcome.client_id,
                directory = outcome.ldap_idp_id.as_deref().unwrap_or("none"),
                "the bundled Zitadel is set up for the dashboard"
            );
            Ok(())
        })
}
