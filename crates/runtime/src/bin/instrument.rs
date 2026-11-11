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

/// What a reader of the instrument store serves: its reads that write
/// nothing and ask nothing of the platform. Every other is the writer's.
const READER_SERVES: &[&str] = &[
    meridian_instrument::service::RESOLVE_INSTRUMENT,
    "platform.reference.query.read-instrument-history",
    "platform.reference.query.list-replacements",
    "platform.reference.query.list-calendars",
    "platform.reference.query.read-calendar",
    "platform.reference.query.list-templates",
    "platform.reference.query.read-template",
    "platform.reference.query.list-event-groups",
    "platform.reference.query.compute-date",
];

/// An instrument reader's following: its hot layer emptied by each change.
struct RecordsFollow(Arc<meridian_instrument::hot::HotStore>);

impl meridian_runtime::member::Apply for RecordsFollow {
    fn apply(&self, _: &str, _: &[meridian_changes::Change]) {
        self.0.follow();
    }

    fn reset(&self, _: &str) {
        self.0.follow();
    }
}

fn run() -> Result<(), String> {
    // Every path this member needs, probed from inside its pod: what
    // `meridian status` and `doctor` read (step 3b).
    if std::env::args().nth(1).as_deref() == Some("probe") {
        return meridian_runtime::probe::run(&meridian_runtime::probe::Paths {
            component: "instrument",
            domain: Some("reference"),
            database: Some("MERIDIAN_DATABASE_URL"),
            record: Some("instrument"),
        });
    }
    let url = required("MERIDIAN_DATABASE_URL")?;

    // Before the key is touched: a migration job holds database credentials and
    // has no business holding the deployment's private key.
    // One scrub at once, from inside the pod (step 3b): what it found, as JSON.
    let scrub_url = var("MERIDIAN_DATABASE_URL").unwrap_or_default();
    if std::env::args().nth(1).as_deref() == Some("scrub") {
        let said = meridian_runtime::scrub::run_once(
            "instrument",
            || {
                let store =
                    PostgresStore::connect(&scrub_url, 1).map_err(|failed| failed.to_string())?;
                store.scrub().map_err(|failed| failed.to_string())
            },
            |scrubbed, member, started, finished| {
                PostgresStore::connect(&scrub_url, 1)
                    .and_then(|store| store.record_scrub(scrubbed, member, started, finished))
                    .map_err(|failed| failed.to_string())
            },
        )?;
        println!("{said}");
        return Ok(());
    }
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        meridian_runtime::migrate_once_it_answers(
            "the instrument store's database",
            &url,
            |url| PostgresStore::connect(url, 1).map_err(|failed| failed.to_string()),
            |store| {
                store
                    .migrate(&*clock())
                    .map_err(|failed| failed.to_string())
            },
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
    let postgres = Arc::new(wait_for_store(
        "the instrument store's database",
        &url,
        |url| PostgresStore::connect(url, 8).map_err(|failed| failed.to_string()),
        |store| {
            store
                .verify()
                .map_err(|failed| Wait::NotYet(failed.to_string()))
        },
    )?);
    // The deployment every change is made in, its time, and the writer's
    // epoch, adopted before it serves (step 3a; ruling 1060).
    meridian_changes::set_origin(&var("MERIDIAN_DEPLOYMENT_ID").unwrap_or_default());
    meridian_changes::set_clock(clock());
    let member = meridian_runtime::member::Member::from_env("instrument", Some("reference"))?;
    // A reader's record-read replica, made before the runtime and held here,
    // as the primary is, so its blocking client closes its connections after
    // the runtime has stopped, never on its worker (contract v30, stage D;
    // a reader made it inside the runtime from stage C2, connecting in
    // block_in_place after PR CI 43014977978's "Cannot start a runtime from
    // within a runtime").
    let replica: Option<Arc<PostgresStore>> = if member.is_reader() {
        meridian_runtime::read_replica(
            "the instrument store's read replica",
            "MERIDIAN_INSTRUMENT_READ_DATABASE_URL",
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
    // The hot layer in front of the record (step 3a): records by ID and what
    // each became, bounded by instrument.hotLayer.recordsMiB.
    let bound = meridian_instrument::hot::bytes_from_mib(
        var("MERIDIAN_INSTRUMENT_HOT_RECORDS_MIB").as_deref(),
    )?;
    tracing::info!(
        records_mib = bound >> 20,
        "the instrument store's hot layer is bounded"
    );
    let hot = Arc::new(meridian_instrument::hot::HotStore::new(
        postgres.clone(),
        bound,
    ));
    let store: Arc<dyn meridian_instrument::Store> = hot.clone();

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "instrument-1".into());
    // The analytics component, whose resolves are read-only (contract v28's
    // fourth amendment, W3.1).
    if let Some(analytics) = var("MERIDIAN_ANALYTICS_INSTANCE_ID") {
        meridian_instrument::service::reads_for_analytics(&analytics);
    }

    // The store is borrowed, never moved in, so it outlives the runtime
    // (meridian_runtime::on_runtime).
    on_runtime(async {
        // Its working budget (stage M), less the hot layer's bound.
        meridian_runtime::install_budget("instrument", bound as u64)?;
        let bus = bus_from_env(&instance_id).await?;
        // A reader member (contract v30, stage B): records by ID, what each
        // became and their history, never writing or asking the platform.
        if member.is_reader() {
            bus.serve_only(|topic| READER_SERVES.contains(&topic));
            // Made before the runtime and held outside it (stage D): its
            // connections close after the runtime has stopped.
            let replica = replica.clone().map(|replica| {
                let at: Arc<dyn meridian_bus::read_after::Position> =
                    Arc::new(meridian_runtime::member::LogHeads::of(replica.clone()));
                (replica as Arc<dyn meridian_instrument::Store>, at)
            });
            let reading = Arc::new(meridian_instrument::reader::ReaderStore::new(
                hot.clone(),
                postgres.clone(),
                replica,
            ));
            InstrumentService::new(bus.clone(), reading, clock()).serve_reads();
            member.read(
                &bus,
                meridian_runtime::member::Followed::from_heads([]),
                Arc::new(RecordsFollow(hot.clone())),
                // Its primary, listing every partition its log keeps: what
                // the reader reconciles with as its writer answers it, as
                // the lake's and the book's readers do.
                Arc::new(meridian_runtime::member::LogHeads::listing(
                    postgres.clone(),
                    &meridian_instrument::postgres::CHANGES.partitions(),
                )),
            )?;
            meridian_runtime::background(async move {
                report_inward_forever(bus, "instrument", 0).await
            });
            tracing::info!(
                instance_id,
                "the instrument store is serving reads, as a reader"
            );
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
                    postgres.adopt().map_err(|failed| {
                        format!("the instrument store could not adopt its epoch: {failed}")
                    })
                },
                |by| {
                    postgres.take_over(by).map_err(|failed| {
                        format!("the instrument store could not take over: {failed}")
                    })
                },
            )
        })?;
        tracing::info!(epoch, "the instrument store writes under its epoch");
        let watched = postgres.clone();
        let writing = postgres.clone();
        holding.keep(
            move || writing.epoch(),
            move || watched.head_epoch().map_err(|failed| failed.to_string()),
        );
        // A new database timeline raises the epoch (contract v30, stage C).
        let following = postgres.clone();
        holding.follow_timeline(&bus, "reference", move || {
            following
                .follow_timeline()
                .map_err(|failed| failed.to_string())
        });
        // The scrub, under the lease (step 3b, integrity at rest).
        let scrubbing = scrub_url.clone();
        meridian_runtime::scrub::forever("instrument", move || {
            meridian_runtime::scrub::run_once(
                "instrument",
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
        let reporting_bus = Arc::clone(&bus);
        // The change log, served a page at a time and its tail published
        // after each commit (step 3a; topics.md, "Component topics").
        meridian_changes::serve(&bus, "reference", postgres.clone());
        meridian_changes::captured::Tail::spawn(
            url.clone(),
            meridian_instrument::postgres::CHANGES,
            Arc::clone(&bus),
            "reference",
        );

        let service = InstrumentService::new(bus, Arc::clone(&store), clock());

        // Registered and subscribed before this returns, so nothing is
        // published into the gap between starting and listening.
        let running = service.start();

        // W5.20. Said on the bus for the conductor to carry outward. This
        // process holds no key, and giving it one so it could report
        // directly would put the deployment's identity back in a store.
        meridian_runtime::background(async move {
            report_inward_forever(reporting_bus, "instrument", 0).await
        });

        tracing::info!(instance_id, "the instrument store is serving");
        ready.serving();

        tokio::select! {
            _ = running => {
                tracing::warn!("the bus shut down");
                Ok(())
            }
            _ = shutdown() => {
                tracing::info!("stopping");
                holding.release().await;
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
