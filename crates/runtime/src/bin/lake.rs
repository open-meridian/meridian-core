//! The lake, as its own process (W10, contract v18).
//!
//! What sources say about entities: prices and bars recorded by `dgm`
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
    let store: Arc<dyn meridian_lake::Store> = Arc::new(wait_for_store(
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

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "lake-1".into());

    on_runtime(async {
        let bus = bus_from_env(&instance_id).await?;
        // Every handler and subscription in place before this returns.
        let _lake = meridian_lake::service::serve(bus.clone(), Arc::clone(&store), clock());

        // W5.20, said on the bus for the instrument store to carry outward:
        // the component's health and schema, never a row or a dataset.
        let reporting = bus.clone();
        let schema = meridian_lake::migrations::latest();
        tokio::spawn(async move { report_inward_forever(reporting, "lake", schema).await });

        tracing::info!(instance_id, started_at_ns = now_ns(), "the lake is serving");
        ready.serving();
        shutdown().await;
        tracing::info!("stopping");
        Ok(())
    })
}
