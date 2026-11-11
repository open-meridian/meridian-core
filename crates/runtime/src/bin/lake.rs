//! The lake, as its own process (W10, contract v18).
//!
//! What sources say about entities: prices and bars recorded by a data plugin
//! plugins, read by the reading roles, under the deployment's licences,
//! entitlements and priority. It talks to the bus and to its database and to
//! nothing else, and holds no deployment key: licensed data never leaves the
//! deployment, and nothing here reaches the platform (spec/the-lake,
//! requirement 14).
//!
//! Its own process and store, apart from the street's and the book's
//! (decisions/012, decisions/032): the store is its only writer.
//!
//! `meridian-lake migrate` applies its schema, once per release. Starting
//! verifies the schema and serves. One writer per partition: each batch
//! takes its dataset's head in Postgres, so a rolling update running two of
//! these still numbers each partition one batch at a time.

use std::sync::Arc;

use meridian_lake::store::StoreError;
use meridian_lake::PostgresStore;
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

/// A lake reader's following: each change applied to its hot layer.
struct LakeFollows(Arc<meridian_lake::hot::HotStore>);

impl meridian_runtime::member::Apply for LakeFollows {
    fn apply(&self, partition: &str, changes: &[meridian_changes::Change]) {
        self.0.follow(partition, changes);
    }

    fn reset(&self, partition: &str) {
        self.0.forget_dataset(partition);
    }
}

fn run() -> Result<(), String> {
    // Every path this member needs, probed from inside its pod: what
    // `meridian status` and `doctor` read (step 3b).
    if std::env::args().nth(1).as_deref() == Some("probe") {
        return meridian_runtime::probe::run(&meridian_runtime::probe::Paths {
            component: "lake",
            domain: Some("lake"),
            database: Some("MERIDIAN_LAKE_DATABASE_URL"),
            record: Some("lake"),
        });
    }
    let url = required("MERIDIAN_LAKE_DATABASE_URL")?;

    match std::env::args().nth(1).as_deref() {
        Some("migrate") => {
            meridian_runtime::migrate_once_it_answers(
                "the lake's database",
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
                    version = meridian_lake::migrations::latest(),
                    "the lake's schema is applied"
                )
            })
            .map_err(|failed| format!("the lake's schema could not be applied: {failed}"))?;
            return match var("MERIDIAN_SERVING_DATABASE_URL") {
                Some(serving) => meridian_runtime::grant_serving(&url, &serving),
                None => Ok(()),
            };
        }
        // One scrub at once, from inside the pod (step 3b): what it found,
        // as JSON.
        Some("scrub") => {
            let said = meridian_runtime::scrub::run_once(
                "lake",
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
        Some(other) => return Err(format!("{other:?} is not a command; migrate or scrub")),
        None => {}
    }

    let ready = Ready::from_env();
    let record: Arc<dyn meridian_lake::Store> = Arc::new(wait_for_store(
        "the lake's database",
        &url,
        |url| PostgresStore::connect(url, 8).map_err(|failed| failed.to_string()),
        |store| {
            store.verify().map_err(|failed| match failed {
                StoreError::SchemaAhead(_) => Wait::Refused(failed.to_string()),
                other => Wait::NotYet(other.to_string()),
            })
        },
    )?);

    // The hot layer in front of the record (step 3a), each layer bounded by
    // its chart value: lake.hotLayer.latestMiB, lake.hotLayer.listingsMiB.
    let bounds = meridian_lake::hot::Bounds::from_mib(
        var("MERIDIAN_LAKE_HOT_LATEST_MIB").as_deref(),
        var("MERIDIAN_LAKE_HOT_LISTINGS_MIB").as_deref(),
    )?;
    let hot = meridian_lake::hot::HotStore::new(Arc::clone(&record), bounds);
    let store: Arc<dyn meridian_lake::Store> = hot.clone();
    tracing::info!(
        latest_mib = bounds.latest_bytes >> 20,
        listings_mib = bounds.listings_bytes >> 20,
        "the lake's hot layer is bounded"
    );
    // The deployment every change is made in, and the writer's epoch,
    // adopted before the first batch (step 3a; ruling 1060).
    meridian_changes::set_origin(&var("MERIDIAN_DEPLOYMENT_ID").unwrap_or_default());
    let member = meridian_runtime::member::Member::from_env("lake", Some("lake"))?;
    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "lake-1".into());
    // How many whole answers are built at once (the chart's
    // lake.maxConcurrentAnswers; rulings 957, 958): refused, not guessed,
    // outside 1 to 8.
    let answers_at_once = meridian_lake::pages::answers_at_once(
        var("MERIDIAN_LAKE_MAX_CONCURRENT_ANSWERS").as_deref(),
    )?;
    // The analytics component, whose reads for a job are its client's (W12.2).
    if let Some(analytics) = var("MERIDIAN_ANALYTICS_INSTANCE_ID") {
        meridian_lake::service::reads_for_analytics(&analytics);
    }

    on_runtime(async {
        // Its working budget (stage M): the limit's share, less the hot
        // layer's bounds and the held answers' budget.
        meridian_runtime::install_budget(
            "lake",
            (bounds.latest_bytes + bounds.listings_bytes + meridian_lake::pages::MOST_BYTES) as u64,
        )?;
        let bus = bus_from_env(&instance_id).await?;
        // A reader member (contract v30, stage B): the lake's reads alone,
        // its hot layer kept by the writer's change log, never writing.
        if member.is_reader() {
            bus.serve_only(meridian_bus::read_after::is_read);
            let replica = match var("MERIDIAN_LAKE_READ_DATABASE_URL") {
                None => None,
                Some(read_url) => Some(Arc::new(wait_for_store(
                    "the lake's read replica",
                    &read_url,
                    |url| PostgresStore::connect(url, 4).map_err(|failed| failed.to_string()),
                    |store| {
                        store
                            .verify()
                            .map_err(|failed| Wait::NotYet(failed.to_string()))
                    },
                )?) as Arc<dyn meridian_lake::Store>),
            };
            // Where it starts following from: the primary's heads before its
            // hot layer holds anything.
            let start = tokio::task::block_in_place(|| record.heads())
                .map_err(|failed| format!("the lake's heads could not be read: {failed}"))?;
            let followed = meridian_runtime::member::Followed::from_heads(start);
            // Its replies' watermarks: what it has applied, for an answer
            // from its hot layer (spec section 5).
            let applied = Arc::clone(&followed);
            let reading = meridian_lake::reader::ReaderStore::new(
                Arc::clone(&hot),
                Arc::clone(&record),
                replica,
                Arc::new(move || applied.heads().into_iter().collect()),
            );
            let lake = meridian_lake::service::serve(bus.clone(), reading, clock());
            lake.build_at_most(answers_at_once);
            let primary = Arc::clone(&record);
            member.read(
                &bus,
                followed,
                Arc::new(LakeFollows(Arc::clone(&hot))),
                Arc::new(meridian_bus::read_after::AllHeads(move || {
                    primary.heads().map_err(|failed| failed.to_string())
                })),
            )?;
            let reporting = bus.clone();
            let schema = meridian_lake::migrations::latest();
            meridian_runtime::background(async move {
                report_inward_forever(reporting, "lake", schema).await
            });
            meridian_runtime::background(meridian_runtime::report_latency_forever(bus.clone()));
            tracing::info!(
                instance_id,
                answers_at_once,
                "the lake is serving reads, as a reader"
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
                    store
                        .adopt()
                        .map_err(|failed| format!("the lake could not adopt its epoch: {failed}"))
                },
                |by| {
                    store
                        .take_over(by)
                        .map_err(|failed| format!("the lake could not take over: {failed}"))
                },
            )
        })?;
        tracing::info!(epoch, "the lake writes under its epoch");
        let watched = Arc::clone(&store);
        holding.keep(epoch, move || {
            watched.head_epoch().map_err(|failed| failed.to_string())
        });
        // The scrub, under the lease (step 3b, integrity at rest).
        let scrubbing = url.clone();
        meridian_runtime::scrub::forever("lake", move || {
            meridian_runtime::scrub::run_once(
                "lake",
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
        // Every handler and subscription in place before this returns.
        let lake = meridian_lake::service::serve(bus.clone(), Arc::clone(&store), clock());
        lake.build_at_most(answers_at_once);

        // The write-ahead stream in `ha` (the resilience spec's section 4;
        // the rulings of 2026-11-08): the data plugins' writes taken from
        // it, each batch recorded with the stream's position, by this
        // member alone while it writes. The chart says so where the broker
        // keeps JetStream; `tolerant` has none.
        if write_ahead_on() {
            let url = required("MERIDIAN_BROKER_URL")?;
            let client = meridian_bus::NatsBackend::client(&url)
                .await
                .map_err(|failed| format!("the write-ahead stream's broker: {failed}"))?;
            let bounds = write_ahead_bounds()?;
            let writing = Arc::clone(&lake);
            meridian_runtime::background(async move {
                if let Err(failed) =
                    meridian_lake::write_ahead::write_forever(writing, client, bounds).await
                {
                    meridian_runtime::member::step_down("lake", &failed);
                }
            });
        }

        // W5.20, said on the bus for the instrument store to carry outward:
        // the component's health and schema, never a row or a dataset.
        let reporting = bus.clone();
        let schema = meridian_lake::migrations::latest();
        meridian_runtime::background(async move {
            report_inward_forever(reporting, "lake", schema).await
        });
        // Its own hop's ages, to the monitor (W4.16; ruling 606).
        meridian_runtime::background(meridian_runtime::report_latency_forever(bus.clone()));

        tracing::info!(
            instance_id,
            answers_at_once,
            started_at_ns = now_ns(),
            "the lake is serving"
        );
        ready.serving();
        shutdown().await;
        tracing::info!("stopping");
        holding.release().await;
        Ok(())
    })
}

/// Whether this deployment's data plugins write ahead to the stream: the
/// chart says so in `ha`, where the broker keeps JetStream at three.
fn write_ahead_on() -> bool {
    var("MERIDIAN_LAKE_WRITE_AHEAD").as_deref() == Some("jetstream")
}

/// The stream's bounds (`lake.writeAhead.maxAge`, `lake.writeAhead.maxBytes`):
/// how long it holds a write the lake has not recorded, and how much it
/// holds; refused where the chart says something that is neither. On the
/// broker's three servers, a majority holding each write before its plugin
/// is answered.
fn write_ahead_bounds() -> Result<meridian_lake::write_ahead::Bounds, String> {
    meridian_lake::write_ahead::Bounds::from_said(
        var("MERIDIAN_LAKE_WRITE_AHEAD_MAX_AGE").as_deref(),
        var("MERIDIAN_LAKE_WRITE_AHEAD_MAX_BYTES").as_deref(),
        3,
    )
}
