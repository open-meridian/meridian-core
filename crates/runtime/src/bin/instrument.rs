//! The deployment's instrument store, as its own process.
//!
//! The deployment's own instrument records (decisions/030): mints and matches
//! them, keeps each value with its source and every version, and answers what
//! an instrument is; the dashboard completes and merges them for a deployment
//! admin (W3.10 to W3.13). It holds no key and reaches no network: asking the
//! platform about a record is the conductor's, when a person asks (decision
//! 011, W3.3).
//!
//! `meridian-instrument migrate` applies its schema, and at contract v10
//! sources what an older release held (the spec's Q8). `public-key` moved to the conductor with
//! the key it prints the public half of.

use std::sync::Arc;

use meridian_instrument::{InstrumentService, PostgresStore};
use meridian_runtime::{
    bus_from_env, clock, on_runtime, report_inward_forever, required, shutdown, var,
    wait_for_store, Ready, Wait,
};

fn main() {
    meridian_runtime::answer_version();
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
        meridian_runtime::migrate_once_it_answers(
            "the instrument store's database",
            &url,
            |url| PostgresStore::connect(url, 1).map_err(|failed| failed.to_string()),
            |store| store.migrate().map_err(|failed| failed.to_string()),
        )
        .map(|()| tracing::info!("the instrument store's schema is applied"))
        .map_err(|failed| {
            format!("the instrument store's schema could not be applied: {failed}")
        })?;
        return grant_if_serving(&url);
    }

    // Not ready until it serves, whatever this pod said before.
    let ready = Ready::from_env();

    // Waited for rather than refused: in an upgrade this starts beside the Job
    // that migrates for it.
    let store: Arc<dyn meridian_instrument::Store> = Arc::new(wait_for_store(
        "the instrument store's database",
        &url,
        |url| PostgresStore::connect(url, 8).map_err(|failed| failed.to_string()),
        |store| {
            store
                .verify()
                .map_err(|failed| Wait::NotYet(failed.to_string()))
        },
    )?);

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "instrument-1".into());

    // The store is borrowed, never moved in, so it outlives the runtime
    // (meridian_runtime::on_runtime).
    on_runtime(async {
        let bus = bus_from_env(&instance_id).await?;
        let reporting_bus = Arc::clone(&bus);

        let service = InstrumentService::new(bus, Arc::clone(&store), clock());

        // Registered and subscribed before this returns, so nothing is
        // published into the gap between starting and listening.
        let running = service.start();

        // W5.20. Said on the bus for the conductor to carry outward. This
        // process holds no key, and giving it one so it could report
        // directly would put the deployment's identity back in a store.
        tokio::spawn(async move { report_inward_forever(reporting_bus, "instrument", 0).await });

        tracing::info!(instance_id, "the instrument store is serving");
        ready.serving();

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

/// Grant to the serving role, when this deployment has one.
///
/// A deployment configured by its wizard holds two logins; one an
/// administrator supplied by hand may hold a single connection, and then this
/// does nothing.
fn grant_if_serving(migrating_url: &str) -> Result<(), String> {
    match var("MERIDIAN_SERVING_DATABASE_URL") {
        Some(serving) => meridian_runtime::grant_serving(migrating_url, &serving),
        None => Ok(()),
    }
}
