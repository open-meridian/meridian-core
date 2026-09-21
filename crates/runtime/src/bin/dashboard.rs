//! The dashboard, as its own process: the one address a firm's staff use.
//!
//! Signs people in through the firm's directory and serves what they may
//! reach. It holds no deployment key and no database of its own: it reads the
//! access records from the conductor's configuration store over the bus, as
//! the instance its launch configuration names, with the `admin` role's
//! grants. Sessions live in memory, so a restart signs everyone out.
//!
//! Refuses to serve until it has read the records once, and again whenever it
//! has not read them for 10 minutes (decisions/015).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use meridian_dashboard::{
    refresh, refresh_forever, router, App, RecordsCache, Sessions, SystemClock,
};
use meridian_runtime::{bus_from_env, now_ns, shutdown, var};

fn main() {
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
    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "dashboard-1".into());
    let listen: SocketAddr = var("MERIDIAN_DASHBOARD_LISTEN")
        .unwrap_or_else(|| "0.0.0.0:8080".into())
        .parse()
        .map_err(|failed| format!("MERIDIAN_DASHBOARD_LISTEN is not an address: {failed}"))?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            let bus = bus_from_env(&instance_id).await?;
            let records = Arc::new(RecordsCache::default());
            let sessions = Arc::new(Sessions::default());
            let clock = Arc::new(SystemClock);

            // Once before listening, so the first request finds records when
            // the conductor is up. When it is not, the dashboard still
            // listens, and says why it refuses rather than failing to start.
            if let Err(failed) = refresh(&bus, &records, clock.as_ref()).await {
                tracing::warn!("the access records could not be read yet: {failed}");
            }
            tokio::spawn(refresh_forever(Arc::clone(&bus), Arc::clone(&records), clock.clone()));

            let sweeping = Arc::clone(&sessions);
            tokio::spawn(async move {
                let mut every = tokio::time::interval(Duration::from_secs(60));
                loop {
                    every.tick().await;
                    sweeping.sweep(now_ns());
                }
            });

            let app = router(Arc::new(App {
                records,
                sessions,
                clock,
            }));
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .map_err(|failed| format!("could not listen on {listen}: {failed}"))?;

            tracing::info!(instance_id, %listen, started_at_ns = now_ns(), "the dashboard is listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    shutdown().await;
                    tracing::info!("stopping");
                })
                .await
                .map_err(|failed| failed.to_string())
        })
}
