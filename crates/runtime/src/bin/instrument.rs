//! The deployment's instrument store, as its own process.
//!
//! Answers instrument questions locally and applies what the conductor publishes
//! after pulling or escalating. It holds no key and reaches no network: the
//! platform connection and the deployment's identity are the conductor's, per
//! decision 011.
//!
//! For identity, a replica rather than a cache: it keeps answering from what it holds when
//! the platform is unreachable, which is what lets a deployment stay useful
//! through somebody else's outage.
//!
//! `meridian-instrument migrate` applies its schema. `public-key` moved to the conductor with
//! the key it prints the public half of.

use std::sync::Arc;

use meridian_instrument::{InstrumentService, PostgresStore, SystemClock};
use meridian_runtime::{bus_from_env, report_inward_forever, required, shutdown, var};

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
            .map(|()| tracing::info!("the instrument store's schema is applied"))
            .map_err(|failed| {
                format!("the instrument store's schema could not be applied: {failed}")
            });
    }

    let store = PostgresStore::connect(&url, 8).map_err(|failed| failed.to_string())?;
    store.verify().map_err(|failed| failed.to_string())?;

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "instrument-1".into());

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;
            let reporting_bus = Arc::clone(&bus);

            let service = InstrumentService::new(bus, Arc::new(store), Arc::new(SystemClock));

            // Registered and subscribed before this returns, so nothing is
            // published into the gap between starting and listening.
            let running = service.start();

            // W5.20. Said on the bus for the conductor to carry outward. This
            // process holds no key, and giving it one so it could report
            // directly would put the deployment's identity back in a store.
            tokio::spawn(
                async move { report_inward_forever(reporting_bus, "instrument", 0).await },
            );

            tracing::info!(instance_id, "the instrument store is serving");

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
