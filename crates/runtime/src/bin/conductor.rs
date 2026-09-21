//! The deployment's end of the link to the platform, as its own process.
//!
//! Holds the key, makes the outbound connection, carries a miss to the platform
//! and the answer back onto the bus, and reports what the deployment is running.
//! Decision 011.
//!
//! It holds the configuration store: what a deployment admin authors, which
//! nothing else can rebuild. It held no database until 2026-09-21, on the
//! argument that a control process accumulating a store becomes the one nobody
//! writes migrations for. Configuration was ruled to live here, as it did in
//! v1, so the store came with its migration history, and
//! `meridian-conductor migrate` applies it once per release. Starting verifies
//! and refuses a schema it does not recognise.
//!
//! `conductor public-key` prints the public half of the deployment's key,
//! generating one if there is none. That moved here with the key: the process
//! that holds a private half is the process that can speak for its public one.

use std::sync::Arc;

use meridian_conductor::{Conductor, SystemClock, INSTRUMENT_MISSING};
use meridian_config::PostgresStore;
use meridian_runtime::{
    bus_from_env, key_at, now_ns, platform_from_env, report_forever, required, shutdown, var,
    PlatformUpstream,
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
    let command = std::env::args().nth(1);

    // Before the key, because migrating needs a database and nothing else: a
    // migration job holds no key and should not need one.
    if command.as_deref() == Some("migrate") {
        let url = required("MERIDIAN_CONFIG_DATABASE_URL")?;
        return PostgresStore::connect(&url, 1)
            .and_then(|store| store.migrate())
            .map(|()| {
                tracing::info!(
                    version = meridian_config::migrations::latest(),
                    "the configuration store's schema is applied"
                )
            })
            .map_err(|failed| {
                format!("the configuration store's schema could not be applied: {failed}")
            });
    }

    let key_path = var("MERIDIAN_KEY_PATH").unwrap_or_else(|| "/var/lib/meridian/key.pem".into());
    let key = key_at(&key_path)?;

    // And before the database, because printing the public half needs the key
    // and nothing else.
    if command.as_deref() == Some("public-key") {
        println!(
            "{}",
            key.public_key_pem().map_err(|failed| failed.to_string())?
        );
        return Ok(());
    }

    let url = required("MERIDIAN_CONFIG_DATABASE_URL")?;
    let store = PostgresStore::connect(&url, 8).map_err(|failed| failed.to_string())?;
    // Verified, never applied, for the reason every store gives.
    store.verify().map_err(|failed| failed.to_string())?;

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "conductor-1".into());
    let platform = platform_from_env(key)?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;

            // Subscribed before the loop starts, for the reason at-most-once
            // delivery makes unforgiving: what arrives before a subscriber
            // exists is dropped, and dropped silently.
            let misses = bus.subscribe(INSTRUMENT_MISSING);

            // The config domain: registered and subscribed before the loop
            // starts, so nothing the dashboard or a sidecar asks is missed.
            meridian_config::serve(
                Arc::clone(&bus),
                Arc::new(store),
                Arc::new(meridian_config::SystemClock),
                Arc::new(PlatformUpstream::new(Arc::clone(&platform))),
            );

            let carrying = Arc::clone(&bus);
            let conductor = Conductor::new(carrying, Arc::clone(&platform), Arc::new(SystemClock));
            let running = tokio::spawn(conductor.consume(misses));

            // W5.19 outward, W5.20 inward: this component holds the key, so it
            // is the one that can tell the platform anything, and what it tells
            // it includes what the others have said about themselves.
            let reporting = Arc::clone(&platform);
            let collecting = Arc::clone(&bus);
            let schema = meridian_config::migrations::latest();
            tokio::spawn(async move {
                report_forever(reporting, collecting, "conductor", schema).await
            });

            tracing::info!(
                instance_id,
                platform = platform.address(),
                started_at_ns = now_ns(),
                "the conductor is connected"
            );

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
