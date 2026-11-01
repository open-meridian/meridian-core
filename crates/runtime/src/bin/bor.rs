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
    // How many entries an account's journal grows by between snapshots
    // (contract v25, W9.10): the chart's `bor.snapshotInterval`.
    let interval =
        meridian_bor::snapshot::interval_from(var("MERIDIAN_BOR_SNAPSHOT_INTERVAL").as_deref())?;

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
            let store = PostgresStore::connect(&url, 1)
                .map_err(|failed| failed.to_string())?
                .with_snapshot_interval(interval);
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
    let postgres: Arc<PostgresStore> = Arc::new(wait_for_store(
        "the book's database",
        &url,
        |url| {
            PostgresStore::connect(url, 8)
                .map(|store| store.with_snapshot_interval(interval))
                .map_err(|failed| failed.to_string())
        },
        |store| {
            store.verify().map_err(|failed| match failed {
                StoreError::SchemaAhead(_) => Wait::Refused(failed.to_string()),
                other => Wait::NotYet(other.to_string()),
            })
        },
    )?);
    let store: Arc<dyn meridian_bor::Store> = postgres.clone();

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "bor-1".into());

    on_runtime(async {
        let bus = bus_from_env(&instance_id).await?;

        meridian_bor::service::serve(bus.clone(), Arc::clone(&store), clock());
        // The start's snapshots (v26; W9.10): every account whose journal
        // since its latest snapshot of this format reaches the interval --
        // a quiet one, or one upgraded from an older format -- kept now, off
        // the runtime's threads, while the book serves.
        tokio::task::spawn_blocking(move || match postgres.backfill_snapshots(None) {
            Ok(kept) => tracing::info!(kept, "the book's snapshots are taken where due"),
            Err(failed) => tracing::warn!(%failed, "the book's start-up snapshots stopped"),
        });
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

        // The order path (W11, contract v22): orders whose time in force
        // ended by the deployment's clock expired, as the book's own act;
        // and, once at the upgrade, every account held recorded as waiting
        // for compliance (W9.13, decisions/031).
        let backfilling = bus.clone();
        let held = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || {
            meridian_bor::order_service::backfill_waits(&backfilling, held.as_ref())
        })
        .await
        {
            Ok(Ok(recorded)) if recorded > 0 => tracing::info!(
                recorded,
                "every account the book held is recorded as waiting for compliance"
            ),
            Ok(Ok(_)) => {}
            Ok(Err(why)) => tracing::warn!(
                why,
                "the accounts held could not be recorded as waiting for compliance"
            ),
            Err(failed) => tracing::warn!(%failed, "the waiting backfill failed"),
        }
        tokio::spawn(meridian_bor::order_service::expire_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_bor::order_service::EXPIRE_EVERY,
        ));

        // Booking (W11.30, contract v24): the child entries every booking
        // owes the other accounts it allocates to, swept for on start and
        // every quarter hour; and, once at the upgrade, the escape hatch for
        // orders on local records recorded on by default (W11.36,
        // decisions/031).
        let backfilling = bus.clone();
        let held = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || {
            meridian_bor::booking_service::backfill_local_record_orders(&backfilling, held.as_ref())
        })
        .await
        {
            Ok(Ok(true)) => tracing::info!("orders on local records are allowed, by default"),
            Ok(Ok(false)) => {}
            Ok(Err(why)) => tracing::warn!(why, "the escape hatch's default could not be recorded"),
            Err(failed) => tracing::warn!(%failed, "the escape hatch's backfill failed"),
        }
        // Contract v26 (W11.38, decisions/031): the agents' guard recorded
        // on at the upgrade, once.
        let backfilling = bus.clone();
        let held = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || {
            meridian_bor::booking_service::backfill_agent_orders_demo_only(
                &backfilling,
                held.as_ref(),
            )
        })
        .await
        {
            Ok(Ok(true)) => tracing::info!("agents may place orders on demo connections only"),
            Ok(Ok(false)) => {}
            Ok(Err(why)) => tracing::warn!(why, "the agents' guard could not be recorded"),
            Err(failed) => tracing::warn!(%failed, "the agents' guard's backfill failed"),
        }
        // Contract v29 (W11.11; ruling 992): the factors a pool's fills
        // await, read from the lake on start and every quarter hour, each
        // published one revised as the book's own record.
        tokio::spawn(meridian_bor::factors::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_bor::factors::SWEEP_EVERY,
        ));
        tokio::spawn(meridian_bor::booking_service::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_bor::booking_service::SWEEP_EVERY,
        ));

        // Contract v26 (W11.34): the affirmation deadline and the
        // deployment's time zone, read from the conductor now and again, for
        // a held booking's due_by.
        tokio::spawn(meridian_bor::deadline::follow_forever(
            bus.clone(),
            meridian_bor::deadline::READ_EVERY,
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
