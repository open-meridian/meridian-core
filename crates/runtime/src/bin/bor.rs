//! The deployment's book of record, as its own process (W9).
//!
//! The firm's own positions, breaks and figures: an append-only journal per
//! account partition, and the projections rebuilt from it. It talks to the
//! bus and to its database and to nothing else, and holds no deployment key;
//! what it runs reaches the platform by way of the instrument store, as the
//! street store's does.
//!
//! Its own process and store, apart from the street's (decisions/012): the
//! custodian's statement is a street record, and the book is verified
//! against it, never loaded from it. They share no database credential the
//! deployment does not choose to share.
//!
//! `meridian-bor migrate` applies its schema, once per release; `meridian-bor
//! rebuild` replays every partition's journal into the projections and
//! stops. Starting verifies the schema and serves. One writer per partition:
//! every command takes its partition's lock in Postgres, so a rolling update
//! running two of these still writes each partition one entry at a time.

use std::sync::Arc;

use meridian_bor::store::StoreError;
use meridian_bor::PostgresStore;
use meridian_runtime::{
    bus_from_env, clock, now_ns, on_runtime, report_inward_forever, required, shutdown, var,
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
    let url = required("MERIDIAN_BOR_DATABASE_URL")?;

    match std::env::args().nth(1).as_deref() {
        Some("migrate") => {
            meridian_runtime::migrate_once_it_answers(
                "the book's database",
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
                    version = meridian_bor::migrations::latest(),
                    "the book's schema is applied"
                )
            })
            .map_err(|failed| format!("the book's schema could not be applied: {failed}"))?;
            return match var("MERIDIAN_SERVING_DATABASE_URL") {
                Some(serving) => meridian_runtime::grant_serving(&url, &serving),
                None => Ok(()),
            };
        }
        Some("rebuild") => {
            // The projections made again from the journal alone (W9.8): what
            // a replay test holds the book to, and what an operator runs
            // after restoring a journal.
            let store = PostgresStore::connect(&url, 1).map_err(|failed| failed.to_string())?;
            store.verify().map_err(|failed| failed.to_string())?;
            let replayed = meridian_bor::Store::rebuild(&store)
                .map_err(|failed| format!("the book could not be rebuilt: {failed}"))?;
            tracing::info!(
                replayed,
                "the book's projections are rebuilt from its journal"
            );
            return Ok(());
        }
        Some(other) => return Err(format!("{other:?} is not a command; migrate or rebuild")),
        None => {}
    }

    let ready = Ready::from_env();
    let store: Arc<dyn meridian_bor::Store> = Arc::new(wait_for_store(
        "the book's database",
        &url,
        |url| PostgresStore::connect(url, 8).map_err(|failed| failed.to_string()),
        |store| {
            store.verify().map_err(|failed| match failed {
                StoreError::SchemaAhead(_) => Wait::Refused(failed.to_string()),
                other => Wait::NotYet(other.to_string()),
            })
        },
    )?);

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "bor-1".into());

    on_runtime(async {
        let bus = bus_from_env(&instance_id).await?;

        meridian_bor::service::serve(bus.clone(), Arc::clone(&store), clock());
        // Each currency's cash instrument (contract v18): the codes held
        // before resolved once and said to be filled in.
        // Read from the store, which blocks: off the runtime's own threads.
        tokio::task::block_in_place(|| meridian_bor::cash::load(store.as_ref()));
        tokio::spawn(meridian_bor::cash::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_bor::cash::SWEEP_EVERY,
        ));

        // W9.9: subscribed before this returns; only the following is
        // spawned. And swept, for a replacement said while this was away.
        tokio::spawn(meridian_bor::service::follow_replacements(
            bus.clone(),
            Arc::clone(&store),
        ));
        tokio::spawn(meridian_bor::service::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_bor::service::SWEEP_EVERY,
        ));

        // W5.20, said on the bus for the instrument store to carry outward.
        let reporting = bus.clone();
        let schema = meridian_bor::migrations::latest();
        tokio::spawn(async move { report_inward_forever(reporting, "bor", schema).await });

        tracing::info!(instance_id, started_at_ns = now_ns(), "the book is serving");
        ready.serving();
        shutdown().await;
        tracing::info!("stopping");
        Ok(())
    })
}
