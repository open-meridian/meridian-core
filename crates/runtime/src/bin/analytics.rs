//! The analytics component, as its own process (W12; spec/the-analytics-
//! engine-answers-behind-one-api; spec/the-analytics-engines-first-release).
//!
//! `meridian-analytics coordinator` compiles a job, resolves its inputs on
//! the client's behalf, prices routed models through their `pricing`
//! plugins and answers, inline or later; `meridian-analytics worker` takes
//! units from the pool's queue group and computes them. With no command it
//! is both, one process answering its own units: compose, a developer's
//! shell. It holds nothing it cannot lose, no database and no key
//! ("Stateless compute, owned storage"; the one-API spec's Q12), and reaches
//! nothing but the bus.

use std::sync::Arc;
use std::time::Duration;

use meridian_analytics::service::{self, Config, Coordinator};
use meridian_analytics::{engine_version, worker, INSTANCE};
use meridian_analytics_engine::graph::Cache;
use meridian_runtime::{bus_from_env, clock, on_runtime, shutdown, var, Ready, VERSION};

/// How many units' parts a worker keeps, losable (Q12).
const UNITS_KEPT: usize = 4_096;

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

fn millis(name: &str, default: u64) -> Result<Duration, String> {
    match var(name) {
        None => Ok(Duration::from_millis(default)),
        Some(text) => text
            .trim()
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| format!("{name} is {text:?}; a whole number of milliseconds")),
    }
}

fn run() -> Result<(), String> {
    let (coordinates, works) = match std::env::args().nth(1).as_deref() {
        Some("coordinator") => (true, false),
        Some("worker") => (false, true),
        None => (true, true),
        Some(other) => return Err(format!("{other:?} is not a command; coordinator or worker")),
    };
    // Every copy of the component -- coordinators and workers alike -- one
    // instance on the bus: the queue group each topic's copies share, and
    // the publisher the lake reads a job's client from (W12.2), which the
    // chart tells the lake as MERIDIAN_ANALYTICS_INSTANCE_ID.
    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| INSTANCE.into());
    let config = Config {
        engine_version: engine_version(VERSION),
        inline: millis("MERIDIAN_ANALYTICS_INLINE_MS", 2_000)?,
        price_deadline: millis("MERIDIAN_ANALYTICS_PRICE_DEADLINE_MS", 5_000)?,
        read_timeout: millis("MERIDIAN_ANALYTICS_READ_TIMEOUT_MS", 8_000)?,
        ..Config::default()
    };
    let unit_timeout = millis("MERIDIAN_ANALYTICS_UNIT_TIMEOUT_MS", 10_000)?;
    let ready = Ready::from_env();

    on_runtime(async move {
        let bus = bus_from_env(&instance_id).await?;
        if works {
            worker::serve(&bus, Arc::new(Cache::new(UNITS_KEPT)));
        }
        if coordinates {
            let handle = tokio::runtime::Handle::current();
            let executor = service::pool(&bus, handle.clone(), unit_timeout);
            let coordinator = Coordinator::new(Arc::clone(&bus), handle, clock(), config, executor);
            service::serve(&bus, &coordinator);
        }
        tracing::info!(
            coordinates,
            works,
            version = VERSION,
            "the analytics component is serving"
        );
        ready.serving();
        shutdown().await;
        tracing::info!("stopping");
        Ok(())
    })
}
