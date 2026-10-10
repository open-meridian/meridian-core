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
//! `meridian-street migrate` applies its schema, once per release. Starting verifies:
//! it waits for a database that does not answer yet or a schema the migration
//! has not reached, and refuses one newer than it understands.

use std::sync::Arc;

use meridian_runtime::{
    bus_from_env, clock, now_ns, on_runtime, report_inward_forever, required, shutdown, var,
    wait_for_store, Ready, Wait,
};
use meridian_street::store::StoreError;
use meridian_street::PostgresStore;

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
    let url = required("MERIDIAN_STREET_DATABASE_URL")?;

    if std::env::args().nth(1).as_deref() == Some("migrate") {
        meridian_runtime::migrate_once_it_answers(
            "the street store's database",
            &url,
            |url| PostgresStore::connect(url, 1).map_err(|failed| failed.to_string()),
            |store| {
                store
                    .migrate(&*clock())
                    .map_err(|failed| failed.to_string())
            },
        )
        .map(|()| {
            tracing::info!(
                version = meridian_street::migrations::latest(),
                "the street store's schema is applied"
            )
        })
        .map_err(|failed| format!("the street store's schema could not be applied: {failed}"))?;
        // The tables belong to the role that just made them, and the serving
        // role has to be able to read them.
        return grant_if_serving(&url);
    }

    // Not ready until it serves, whatever this pod said before.
    let ready = Ready::from_env();

    // Verified, never applied. N replicas starting together would race to
    // apply the same migration, and a process that migrates on start changes a
    // customer's database because somebody restarted a pod. So a schema the
    // migration Job has not reached yet is waited for, not applied, and one a
    // newer release made is refused.
    let store: Arc<dyn meridian_street::Store> = Arc::new(wait_for_store(
        "the street store's database",
        &url,
        |url| PostgresStore::connect(url, 8).map_err(|failed| failed.to_string()),
        |store| {
            store.verify().map_err(|failed| match failed {
                StoreError::SchemaAhead(_) => Wait::Refused(failed.to_string()),
                other => Wait::NotYet(other.to_string()),
            })
        },
    )?);

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "street-1".into());

    // The store is borrowed, never moved in, so it outlives the runtime
    // (meridian_runtime::on_runtime).
    on_runtime(async {
        let bus = bus_from_env(&instance_id).await?;

        // Registered before anything can call them. A component that
        // announces itself and then cannot answer is worse than one that
        // has not arrived.
        meridian_street::service::serve(bus.clone(), Arc::clone(&store), clock());
        // Each currency's cash instrument (contract v18): the codes held
        // before resolved once and said to be filled in, then any the
        // instrument store did not answer for.
        // Read from the store, which blocks: off the runtime's own threads.
        tokio::task::block_in_place(|| meridian_street::cash::load(store.as_ref()));
        tokio::spawn(meridian_street::cash::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_street::cash::SWEEP_EVERY,
        ));

        // W3.9. Subscribed before this returns, like the handlers above;
        // only the moving is spawned.
        tokio::spawn(meridian_street::service::follow_replacements(
            bus.clone(),
            Arc::clone(&store),
        ));

        // W2.13 (contract v14): every custody plugin's sync status, kept
        // and announced. Subscribed before this returns, as above.
        tokio::spawn(meridian_street::service::follow_sync_statuses(
            bus.clone(),
            Arc::clone(&store),
        ));

        // And asked about, for a replacement said while this process was
        // not listening.
        tokio::spawn(meridian_street::service::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            clock(),
            meridian_street::service::SWEEP_EVERY,
        ));

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
        ready.serving();
        shutdown().await;
        tracing::info!("stopping");
        Ok(())
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
