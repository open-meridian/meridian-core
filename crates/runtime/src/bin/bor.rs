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

/// A book reader's following: each entry's account's book forgotten.
struct BookFollows(Arc<meridian_bor::reader::ReaderStore>);

impl meridian_runtime::member::Apply for BookFollows {
    fn apply(&self, _: &str, changes: &[meridian_changes::Change]) {
        self.0.follow(changes);
    }

    fn reset(&self, _: &str) {
        self.0.forget_all();
    }
}

fn run() -> Result<(), String> {
    // Every path this member needs, probed from inside its pod: what
    // `meridian status` and `doctor` read (step 3b).
    if std::env::args().nth(1).as_deref() == Some("probe") {
        return meridian_runtime::probe::run(&meridian_runtime::probe::Paths {
            component: "bor",
            domain: Some("book"),
            database: Some("MERIDIAN_BOR_DATABASE_URL"),
            record: Some("book"),
        });
    }
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
        // One scrub at once, from inside the pod (step 3b): what it found,
        // as JSON.
        Some("scrub") => {
            let said = meridian_runtime::scrub::run_once(
                "book",
                || {
                    let store =
                        PostgresStore::connect(&url, 1).map_err(|failed| failed.to_string())?;
                    store.scrub().map_err(|failed| failed.to_string())
                },
                |scrubbed, member, started, finished| {
                    PostgresStore::connect(&url, 1)
                        .and_then(|store| store.record_scrub(scrubbed, member, started, finished))
                        .map_err(|failed| failed.to_string())
                },
            )?;
            println!("{said}");
            return Ok(());
        }
        Some(other) => {
            return Err(format!(
                "{other:?} is not a command; migrate, rebuild or scrub"
            ))
        }
        None => {}
    }

    let ready = Ready::from_env();
    // Its own database login and its sessions' bounds (section 10a).
    meridian_runtime::serve_database_as("bor");
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
    // The deployment every change is made in, and the writer's epoch,
    // adopted before the first command (step 3a; ruling 1060).
    meridian_changes::set_origin(&var("MERIDIAN_DEPLOYMENT_ID").unwrap_or_default());
    let member = meridian_runtime::member::Member::from_env("bor", Some("book"))?;
    // A reader's record-read replica, made before the runtime and held here,
    // as the primary is, so its blocking client closes its connections after
    // the runtime has stopped, never on its worker (contract v30, stage D;
    // a reader made it inside the runtime from stage C2, connecting in
    // block_in_place after PR CI 43014977978's "Cannot start a runtime from
    // within a runtime").
    let replica: Option<Arc<PostgresStore>> = if member.is_reader() {
        meridian_runtime::read_replica(
            "the book's read replica",
            "MERIDIAN_BOR_READ_DATABASE_URL",
            |url| PostgresStore::connect(url, 4).map_err(|failed| failed.to_string()),
            |store| {
                store
                    .verify()
                    .map_err(|failed| Wait::NotYet(failed.to_string()))
            },
        )?
        .map(Arc::new)
    } else {
        None
    };

    let books_bound =
        meridian_bor::writer::books_bytes_from_mib(var("MERIDIAN_BOR_HOT_BOOKS_MIB").as_deref())?;
    tracing::info!(
        books_mib = books_bound >> 20,
        "the book's hot layer is bounded"
    );

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "bor-1".into());

    on_runtime(async {
        // Its working budget (stage M), less the hot layer's bound.
        meridian_runtime::install_budget("bor", books_bound as u64)?;
        let bus = bus_from_env(&instance_id).await?;
        // A reader member (contract v30, stage B): the book's reads alone,
        // its books kept by the writer's change log, never acting.
        if member.is_reader() {
            bus.serve_only(meridian_bus::read_after::is_read);
            // Made before the runtime and held outside it (stage D): its
            // connections close after the runtime has stopped.
            let replica = replica
                .clone()
                .map(|replica| replica as Arc<dyn meridian_bor::Store>);
            let start =
                tokio::task::block_in_place(|| meridian_bor::Store::heads(postgres.as_ref()))
                    .map_err(|failed| format!("the book's heads could not be read: {failed}"))?;
            let followed = meridian_runtime::member::Followed::from_heads(start);
            // Its replies' watermarks: what it has applied, for a read from
            // the books it holds (spec section 5).
            let applied = Arc::clone(&followed);
            let reading = Arc::new(meridian_bor::reader::ReaderStore::new(
                postgres.clone(),
                replica,
                books_bound,
                Arc::new(move || applied.heads().into_iter().collect()),
            ));
            meridian_bor::service::serve(bus.clone(), reading.clone(), clock());
            let primary = postgres.clone();
            member.read(
                &bus,
                followed,
                Arc::new(BookFollows(reading)),
                Arc::new(meridian_bus::read_after::AllHeads(move || {
                    meridian_bor::Store::heads(primary.as_ref())
                        .map_err(|failed| failed.to_string())
                })),
            )?;
            let reporting = bus.clone();
            let schema = meridian_bor::migrations::latest();
            meridian_runtime::background(async move {
                report_inward_forever(reporting, "bor", schema).await
            });
            tracing::info!(instance_id, "the book is serving reads, as a reader");
            shutdown().await;
            tracing::info!("stopping");
            return Ok(());
        }
        // A standby waits here (step 3b): following the writer's change log,
        // serving nothing, until it takes the lease. With no lease -- the
        // tolerant default, one member -- it writes at once, as it always has.
        let holding = member.wait_to_write(&bus).await?;
        let epoch = tokio::task::block_in_place(|| {
            holding.epoch(
                || {
                    meridian_bor::Store::adopt(postgres.as_ref())
                        .map_err(|failed| format!("the book could not adopt its epoch: {failed}"))
                },
                |by| {
                    meridian_bor::Store::take_over(postgres.as_ref(), by)
                        .map_err(|failed| format!("the book could not take over: {failed}"))
                },
            )
        })?;
        tracing::info!(epoch, "the book writes under its epoch");
        let watched = postgres.clone();
        let writing = postgres.clone();
        holding.keep(
            move || meridian_bor::Store::epoch(writing.as_ref()),
            move || {
                meridian_bor::Store::head_epoch(watched.as_ref())
                    .map_err(|failed| failed.to_string())
            },
        );
        // The scrub, under the lease (step 3b, integrity at rest).
        let scrubbing = url.clone();
        meridian_runtime::scrub::forever("book", move || {
            meridian_runtime::scrub::run_once(
                "book",
                || {
                    let store = PostgresStore::connect(&scrubbing, 1)
                        .map_err(|failed| failed.to_string())?;
                    store.scrub().map_err(|failed| failed.to_string())
                },
                |scrubbed, member, started, finished| {
                    PostgresStore::connect(&scrubbing, 1)
                        .and_then(|store| store.record_scrub(scrubbed, member, started, finished))
                        .map_err(|failed| failed.to_string())
                },
            )
        });
        // Every commit, whichever path makes it, told on its partition's
        // tail (step 3a).
        // and each account's book at its head held in memory, bounded by
        // bor.hotLayer.booksMiB (the hot layer).
        let store: Arc<dyn meridian_bor::Store> = Arc::new(meridian_bor::writer::Writer::bounded(
            postgres.clone(),
            bus.clone(),
            books_bound,
        ));
        // A new database timeline raises the epoch (contract v30, stage C):
        // the books held let go of what the record may have lost.
        let following = Arc::clone(&store);
        holding.follow_timeline(&bus, "book", move || {
            following
                .follow_timeline()
                .map_err(|failed| failed.to_string())
        });

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
        meridian_runtime::background(meridian_bor::cash::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_bor::cash::SWEEP_EVERY,
        ));

        // W9.9: subscribed before this returns; only the following is
        // spawned. And swept, for a replacement said while this was away.
        meridian_runtime::background(meridian_bor::service::follow_replacements(
            bus.clone(),
            Arc::clone(&store),
        ));
        meridian_runtime::background(meridian_bor::service::sweep_forever(
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
        meridian_runtime::background(meridian_bor::order_service::expire_forever(
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
        // Contract v29 (W11.11, W11.27; decisions/031): once, the
        // fixed-income records held before the upgrade backfilled.
        let backfilling = bus.clone();
        let held = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || {
            meridian_bor::fixed_income_backfill::backfill(&backfilling, held.as_ref())
        })
        .await
        {
            Ok(Ok(written)) if written > 0 => tracing::info!(
                written,
                "the fixed-income records held before the upgrade are backfilled"
            ),
            Ok(Ok(_)) => {}
            Ok(Err(why)) => tracing::warn!(why, "the fixed-income backfill waits"),
            Err(failed) => tracing::warn!(%failed, "the fixed-income backfill failed"),
        }
        // Contract v29 (W11.11; ruling 992): the factors a pool's fills
        // await, read from the lake on start and every quarter hour, each
        // published one revised as the book's own record.
        meridian_runtime::background(meridian_bor::factors::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_bor::factors::SWEEP_EVERY,
        ));
        meridian_runtime::background(meridian_bor::booking_service::sweep_forever(
            bus.clone(),
            Arc::clone(&store),
            meridian_bor::booking_service::SWEEP_EVERY,
        ));

        // Contract v26 (W11.34): the affirmation deadline and the
        // deployment's time zone, read from the conductor now and again, for
        // a held booking's due_by.
        meridian_runtime::background(meridian_bor::deadline::follow_forever(
            bus.clone(),
            meridian_bor::deadline::READ_EVERY,
        ));

        // W5.20, said on the bus for the instrument store to carry outward.
        let reporting = bus.clone();
        let schema = meridian_bor::migrations::latest();
        meridian_runtime::background(async move {
            report_inward_forever(reporting, "bor", schema).await
        });

        tracing::info!(instance_id, started_at_ns = now_ns(), "the book is serving");
        ready.serving();
        shutdown().await;
        tracing::info!("stopping");
        holding.release().await;
        Ok(())
    })
}
