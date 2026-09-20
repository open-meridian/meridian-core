//! The deployment's ledger, as its own process.
//!
//! Statements, holdings and positions: what a connector read from a custodian,
//! and the one store in a deployment whose contents nothing can rebuild. It
//! talks to the bus and to its database, and to nothing else — in particular
//! not to the platform, which is why this process holds no deployment key.
//!
//! Separate from the replica because they have opposite properties. The
//! replica holds what the platform can send again, so a bad upgrade is fixed
//! by refetching; this holds a custodian's statement from last month, which
//! exists in one place. Tying them together made every replica change inherit
//! the ledger's caution.
//!
//! `ledger migrate` applies its schema, once per release. Starting verifies
//! and refuses a schema it does not recognise.

use std::sync::Arc;

use meridian_kernel::service::SystemClock;
use meridian_kernel::PostgresStore;
use meridian_runtime::{
    bus_from_env, key_at, now_ns, platform_from_env, report_forever, required, shutdown, var,
};

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
    let url = required("MERIDIAN_LEDGER_DATABASE_URL")?;

    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return PostgresStore::connect(&url, 1)
            .and_then(|store| store.migrate())
            .map(|()| {
                tracing::info!(
                    version = meridian_kernel::migrations::latest(),
                    "the ledger's schema is applied"
                )
            })
            .map_err(|failed| format!("the ledger's schema could not be applied: {failed}"));
    }

    let store = PostgresStore::connect(&url, 8).map_err(|failed| failed.to_string())?;

    // Verified, never applied. N replicas starting together would race to
    // apply the same migration, and a process that migrates on start changes a
    // customer's database because somebody restarted a pod.
    store.verify().map_err(|failed| failed.to_string())?;

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "ledger-1".into());

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;

            // Registered before anything can call them. A component that
            // announces itself and then cannot answer is worse than one that
            // has not arrived.
            meridian_kernel::service::serve(bus.clone(), Arc::new(store), Arc::new(SystemClock));

            // W5.19, when this process has a key to say it with. The ledger
            // does not talk to the platform otherwise, and a deployment that
            // does not mount one here simply goes unreported rather than
            // failing to start.
            if let Some(path) = var("MERIDIAN_KEY_PATH") {
                match key_at(&path).and_then(platform_from_env) {
                    Ok(platform) => {
                        let schema = meridian_kernel::migrations::latest();
                        tokio::spawn(
                            async move { report_forever(platform, "ledger", schema).await },
                        );
                    }
                    Err(failed) => tracing::warn!(%failed, "not reporting to the platform"),
                }
            }

            tracing::info!(
                instance_id,
                started_at_ns = now_ns(),
                "the ledger is serving"
            );
            shutdown().await;
            tracing::info!("stopping");
            Ok(())
        })
}
