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
