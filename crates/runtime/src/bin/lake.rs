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

fn run() -> Result<(), String> {
    // Every path this member needs, probed from inside its pod: what
    // `meridian status` and `doctor` read (step 3b).
    if std::env::args().nth(1).as_deref() == Some("probe") {
        return meridian_runtime::probe::run(&meridian_runtime::probe::Paths {
            component: "lake",
            domain: Some("lake"),
            database: Some("MERIDIAN_LAKE_DATABASE_URL"),
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
        Some(other) => return Err(format!("{other:?} is not a command; migrate")),
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
    let store: Arc<dyn meridian_lake::Store> = meridian_lake::hot::HotStore::new(record, bounds);
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
        let bus = bus_from_env(&instance_id).await?;
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
        // Every handler and subscription in place before this returns.
        let lake = meridian_lake::service::serve(bus.clone(), Arc::clone(&store), clock());
        lake.build_at_most(answers_at_once);

        // W5.20, said on the bus for the instrument store to carry outward:
        // the component's health and schema, never a row or a dataset.
        let reporting = bus.clone();
        let schema = meridian_lake::migrations::latest();
        tokio::spawn(async move { report_inward_forever(reporting, "lake", schema).await });
        // Its own hop's ages, to the monitor (W4.16; ruling 606).
        tokio::spawn(meridian_runtime::report_latency_forever(bus.clone()));

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
