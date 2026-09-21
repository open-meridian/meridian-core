//! The deployment's street store, as its own process.
//!
//! Statements, holdings and positions: what a connector read from a custodian,
//! and the one store in a deployment whose contents nothing can rebuild. It
//! talks to the bus and to its database, and to nothing else — in particular
//! not to the platform, which is why this process holds no deployment key.
//!
//! Separate from the instrument store because they have opposite properties. The
//! instrument store holds what the platform can send again, so a bad upgrade is fixed
//! by refetching; this holds a custodian's statement from last month, which
//! exists in one place. Tying them together made every instrument-store change inherit
//! the street store's caution.
//!
//! `meridian-street migrate` applies its schema, once per release. Starting verifies
//! and refuses a schema it does not recognise.

use std::sync::Arc;

use meridian_runtime::{bus_from_env, now_ns, report_inward_forever, required, shutdown, var};
use meridian_street::service::SystemClock;
use meridian_street::PostgresStore;

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
    let url = required("MERIDIAN_STREET_DATABASE_URL")?;

    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return PostgresStore::connect(&url, 1)
            .and_then(|store| store.migrate())
            .map(|()| {
                tracing::info!(
                    version = meridian_street::migrations::latest(),
                    "the street store's schema is applied"
                )
            })
            .map_err(|failed| format!("the street store's schema could not be applied: {failed}"));
    }

    let store = PostgresStore::connect(&url, 8).map_err(|failed| failed.to_string())?;

    // Verified, never applied. N replicas starting together would race to
    // apply the same migration, and a process that migrates on start changes a
    // customer's database because somebody restarted a pod.
    store.verify().map_err(|failed| failed.to_string())?;

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "street-1".into());

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;

            // Registered before anything can call them. A component that
            // announces itself and then cannot answer is worse than one that
            // has not arrived.
            meridian_street::service::serve(bus.clone(), Arc::new(store), Arc::new(SystemClock));

            // W5.20. Said on the bus, for the instrument store to carry outward: this
            // process holds no key, and giving it one so it could report
            // directly would make it a second thing able to authenticate as
            // the whole deployment.
            let reporting = bus.clone();
            let schema = meridian_street::migrations::latest();
            tokio::spawn(async move { report_inward_forever(reporting, "street", schema).await });

            tracing::info!(
                instance_id,
                started_at_ns = now_ns(),
                "the street store is serving"
            );
            shutdown().await;
            tracing::info!("stopping");
            Ok(())
        })
}
