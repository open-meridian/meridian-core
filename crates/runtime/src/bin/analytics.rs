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
use meridian_analytics::standing::{self, BusPeers};
use meridian_analytics::{engine_version, queue, worker, INSTANCE};
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

fn whole(name: &str, default: u64) -> Result<u64, String> {
    match var(name) {
        None => Ok(default),
        Some(text) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("{name} is {text:?}; a whole number")),
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
        // Contract v30 (W12.4): the chart's analytics.standing.leaseSeconds
        // and analytics.standing.most.
        standing: standing::Settings {
            lease: Duration::from_secs(whole(
                "MERIDIAN_ANALYTICS_STANDING_LEASE_SECONDS",
                standing::LEASE.as_secs(),
            )?),
            most: whole("MERIDIAN_ANALYTICS_STANDING_MOST", standing::MOST as u64)? as usize,
            ..standing::Settings::default()
        },
        ..Config::default()
    };
    // This coordinator's name among the others, its pod's (question 13):
    // where none is given, one of its own, never another's.
    let member = {
        let pod = meridian_runtime::member::member_name();
        if pod.is_empty() {
            format!("coordinator-{:x}", std::process::id())
        } else {
            standing::member_named(&pod)
        }
    };
    let unit_timeout = millis("MERIDIAN_ANALYTICS_UNIT_TIMEOUT_MS", 10_000)?;
    let ready = Ready::from_env();

    on_runtime(async move {
        let bus = bus_from_env(&instance_id).await?;
        // Its own hop's ages, to the monitor (W4.16; ruling 606).
        meridian_runtime::background(meridian_runtime::report_latency_forever(Arc::clone(&bus)));
        // The units' work queue on JetStream, where the broker keeps one
        // (step 3b; broker.jetstream.replicas: 3); the queue group otherwise.
        // Never an outage of its own: where the stream cannot be made in a
        // minute (its servers still forming their group after a rolling
        // restart: "no suitable peers for placement"), the units go by the
        // queue group as without one, and a worker serves the queue group
        // always, so a coordinator on either path is answered.
        let queue = if queue::on() {
            let url = var("MERIDIAN_BROKER_URL").ok_or("MERIDIAN_BROKER_URL is not set")?;
            let mut made = Err(String::new());
            for _ in 0..12 {
                made = queue::Units::connect(&url).await;
                if made.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            match made {
                Ok(units) => {
                    tracing::info!("the units go by JetStream's work queue");
                    Some(units)
                }
                Err(failed) => {
                    tracing::warn!(%failed, "the units' work queue is not there; units go by the queue group");
                    None
                }
            }
        } else {
            None
        };
        if works {
            let cache = Arc::new(Cache::new(UNITS_KEPT));
            if let Some(units) = &queue {
                let units = Arc::clone(units);
                let taking = Arc::clone(&cache);
                meridian_runtime::background(async move {
                    if let Err(failed) = units.work(taking).await {
                        tracing::error!("{failed}; stopping, to be restarted");
                        std::process::exit(1);
                    }
                });
            }
            worker::serve(&bus, cache);
        }
        if coordinates {
            let handle = tokio::runtime::Handle::current();
            let executor = service::pool_queued(&bus, handle.clone(), unit_timeout, queue.clone());
            let read_timeout = config.read_timeout;
            let coordinator =
                Coordinator::new(Arc::clone(&bus), handle.clone(), clock(), config, executor);
            // Contract v30 (W12.4): the live coordinators, heard and told on
            // the component's own topic; the questions they hand this one;
            // what it hears for its standing jobs; and their recomputing.
            coordinator.use_peers(BusPeers::start(
                Arc::clone(&bus),
                handle,
                member.clone(),
                read_timeout,
            ));
            service::serve(&bus, &coordinator);
            standing::coordinator::serve_standing(&bus, &coordinator);
            coordinator.hold_standing_forever();
            tracing::info!(member, "this coordinator holds standing jobs");
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
