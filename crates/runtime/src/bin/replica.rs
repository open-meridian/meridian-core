//! The deployment's replica of the security master, as its own process.
//!
//! Answers instrument questions locally, pulls from the platform when it
//! cannot, escalates when the platform does not know either, and applies what
//! comes back. It holds the deployment's key, because it is the component that
//! talks to the platform.
//!
//! A replica rather than a cache: it keeps answering from what it holds when
//! the platform is unreachable, which is what lets a deployment stay useful
//! through somebody else's outage.
//!
//! `replica migrate` applies its schema and `replica public-key` prints the
//! public half of the deployment's key, generating one if there is none.

use std::sync::Arc;

use meridian_reference::{PostgresStore, Replica, SystemClock};
use meridian_runtime::{
    bus_from_env, key_at, platform_from_env, report_forever, required, shutdown, var,
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
    let url = required("MERIDIAN_DATABASE_URL")?;

    // Before the key is touched: a migration job holds database credentials and
    // has no business holding the deployment's private key.
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return PostgresStore::connect(&url, 1)
            .and_then(|store| store.migrate())
            .map(|()| tracing::info!("the replica's schema is applied"))
            .map_err(|failed| format!("the replica's schema could not be applied: {failed}"));
    }

    let key_path = var("MERIDIAN_KEY_PATH").unwrap_or_else(|| "/var/lib/meridian/key.pem".into());
    let key = key_at(&key_path)?;

    if std::env::args().nth(1).as_deref() == Some("public-key") {
        println!(
            "{}",
            key.public_key_pem().map_err(|failed| failed.to_string())?
        );
        return Ok(());
    }

    let store = PostgresStore::connect(&url, 8).map_err(|failed| failed.to_string())?;
    store.verify().map_err(|failed| failed.to_string())?;

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "replica-1".into());
    let platform = platform_from_env(key)?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;
            let reporting_bus = Arc::clone(&bus);

            let replica = Replica::new(
                bus,
                Arc::new(store),
                Arc::clone(&platform),
                Arc::new(SystemClock),
            );

            // Registered and subscribed before this returns, so nothing is
            // published into the gap between starting and listening.
            let running = replica.start();

            // W5.19 outward, W5.20 inward: this component holds the key, so
            // it is the one that can tell the platform anything, and what it
            // tells it includes what the others have said about themselves.
            let reporting = Arc::clone(&platform);
            let collecting = Arc::clone(&reporting_bus);
            tokio::spawn(async move { report_forever(reporting, collecting, "replica", 0).await });

            tracing::info!(instance_id, "the replica is serving");

            tokio::select! {
                _ = running => {
                    tracing::warn!("the bus shut down");
                    Ok(())
                }
                _ = shutdown() => {
                    tracing::info!("stopping");
                    Ok(())
                }
            }
        })
}
