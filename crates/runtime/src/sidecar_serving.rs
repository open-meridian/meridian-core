//! A plugin's sidecar serving (step 3d; the resilience spec's section 7):
//! what the `meridian-sidecar` binary serves, once for an instance's one
//! pod, or as a shard of several -- each serving of a shard on a runtime of
//! its own, so nothing of it outlives the shard (item 49, ruled
//! 2026-11-16). Here, apart from the binary, so it is tested whole.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use meridian_bus::Bus;
use meridian_sidecar::front_door::{self, FrontDoor, Verifier};
use meridian_sidecar::share::{Limits, Share, Shared};
use meridian_sidecar::{
    Identity, OlderPagesRead, PluginOperationsServer, Sidecar, SidecarServiceServer,
};

use crate::lease::Leases;
use crate::shards::{self, Served, ShardSet};
use crate::var;

/// How each serving's bus is made: the broker's, from the environment, in
/// the binary; one in memory in a test. A new bus each serving, so its
/// stamp and its gate are that serving's alone.
pub type MakeBus =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Arc<Bus>, String>> + Send>> + Send + Sync>;

/// What each serving of this sidecar is made from, read once at its start.
pub struct Serving {
    pub deployment_id: String,
    pub identity: Identity,
    pub verifier: Option<Arc<Verifier>>,
    pub front: Option<(
        std::net::SocketAddr,
        Arc<Verifier>,
        Option<tokio_rustls::TlsAcceptor>,
    )>,
    pub listening: std::net::SocketAddr,
    pub instance_id: String,
    pub limits: Limits,
    pub write_share: Option<Arc<meridian_sidecar::share::WriteShare>>,
    /// The declaration a harness's sidecar holds its plugin's registration
    /// to, and the file it was read from.
    pub declaration: Option<(String, meridian_pb::v1::PluginDeclaration)>,
    /// Each serving's bus.
    pub bus: MakeBus,
}

/// How long a lost shard's serving is given to end -- its surface, front
/// door, bus and every task they started -- before the pod stands by again.
const SERVING_ENDS_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a taker holds what it hears of its instance's wants for its
/// plugin's stream (ruling 184): one full period of the lake's redelivery,
/// its 60 s and the 15 s between its sweeps, counted from the take's own
/// delivery. Within it the hold is the only way a want reaches a stream
/// not yet open; past it the lake has delivered each standing want again,
/// and a stream opening later is served at the lake's next redelivery, as
/// before the hold.
const HOLD_WANTS_FOR: std::time::Duration =
    std::time::Duration::from_nanos(meridian_lake::service::REDELIVER_STANDING_NS as u64)
        .saturating_add(meridian_lake::service::SWEEP_EVERY);

/// How long a take waits for its hold's subscriptions to be at the broker:
/// a broker answers in milliseconds, and a take is never held up for long
/// against its Lease.
const HELD_AT_THE_BROKER_WITHIN: std::time::Duration = std::time::Duration::from_secs(1);

/// One pod of several (step 3d; the resilience spec's section 7): nothing
/// is served -- no plugin surface, no front door, nothing on the bus --
/// until a shard's Lease is this pod's. A standby waits here, and its
/// plugin's registration waits with it.
///
/// **A shard lost stands by again in this process** (item 32 as item 49
/// ruled it, 2026-11-16; review c5a5 F3): its Lease showing another holder,
/// a write refused as fenced, or a take that did not complete. Its serving
/// -- the plugin's surface, the front door, the bus and its stamp, and every
/// task they started -- runs on a runtime of its own, ended whole when the
/// shard is lost, so nothing of it outlives the shard: the plugin's stream
/// ends with its connection, and the SDK leaves as it does when a sidecar
/// ends. The pod then stands by under a new holder name. Its process ends
/// only for a fault of its own: a surface or front door that stops, a bus
/// set up wrongly -- an error from its setup, never a broker that does not
/// answer in time, which is an outage and a take that did not happen
/// (review c5a6 M3) -- or a Lease setting refused.
pub async fn serve_as_shards<L: Leases + 'static>(
    serving: Arc<Serving>,
    count: u32,
    leases: impl Fn() -> Result<Arc<L>, String>,
    (renew, duration): (std::time::Duration, std::time::Duration),
    stop: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    let instance = serving.identity.instance_id.clone();
    if instance.is_empty() {
        return Err(format!(
            "{} needs MERIDIAN_PLUGIN_INSTANCE_ID: a shard is a pod of an instance",
            shards::SHARDS_ENV
        ));
    }
    // A data shard writes on through an API server outage, fenced by its
    // epoch; every other role stops by the clock (step 3d, checkpoint 5,
    // item 32: a property of the role).
    let writes_on = shards::writes_on(&serving.identity.roles);
    shards::serve_shards(
        || {
            // This process's take, not only its pod's, as a store's member
            // names itself: each standing by again is a new holder.
            let holder = format!("{}_{:x}", crate::member::member_name(), crate::now_ns());
            // A Lease client of its own each time: none of its connections
            // outlives the serving that made it (review c5a6 L5).
            let set = ShardSet::new(leases()?, &instance, count, &holder, renew, duration)?;
            Ok(if writes_on { set.writing_on() } else { set })
        },
        |set, mine, others| {
            let (serving, stop, instance) = (Arc::clone(&serving), stop.clone(), instance.clone());
            async move {
                tracing::info!(
                    instance,
                    shard = mine,
                    of = count,
                    "this pod holds a shard's lease"
                );
                let held = Arc::new(meridian_sidecar::shards::Shards::new(mine).of(count));
                held.set_live(others);
                // A refusal heard was the last serving's, whose bus is gone.
                meridian_sidecar::shards::clear_fenced();
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("sidecar-shard")
                    .build()
                    .map_err(|failed| failed.to_string())?;
                let ended = runtime.spawn(serve(serving, Some((set, held)), stop)).await;
                // Ended whole: every task the serving started, its
                // connections and its listeners with it.
                let _ = tokio::task::spawn_blocking(move || {
                    runtime.shutdown_timeout(SERVING_ENDS_WITHIN)
                })
                .await;
                ended.map_err(|failed| format!("the shard's serving failed: {failed}"))?
            }
        },
        stop.clone(),
    )
    .await
}

/// One serving: the bus set up, the shard's epoch raised where it is a
/// shard, and the plugin's surface and the front door served until the pod
/// is told to stop, a shard it holds is lost, or either stops.
pub async fn serve<L: Leases + 'static>(
    serving: Arc<Serving>,
    mut shard: Option<(ShardSet<L>, Arc<meridian_sidecar::shards::Shards>)>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<Served, String> {
    use crate::shards::Kept;
    let Serving {
        deployment_id,
        identity,
        verifier,
        front,
        listening,
        instance_id,
        limits,
        write_share,
        declaration,
        bus: make_bus,
    } = &*serving;
    let (listening, limits) = (*listening, *limits);
    let made = make_bus();
    // Bounded where a shard is held: its Lease is not renewed until the
    // take, so a bus that cannot be set up inside the Lease's lapse gives
    // the shard up rather than take it late (review c5a M2).
    let bus = match &mut shard {
        None => made.await?,
        Some((set, _)) => match tokio::time::timeout(set.lasts(), made).await {
            Ok(Ok(bus)) => bus,
            // A bus set up wrongly -- an address, a credential -- is a
            // fault of the pod's own: its process ends, its Lease given up.
            Ok(Err(failed)) => {
                set.release(crate::now_ns()).await;
                return Err(format!("the bus was not set up: {failed}"));
            }
            // A broker that did not answer in time is an outage, not the
            // pod's own fault: a take that did not happen, however many
            // times running (review c5a6 M3).
            Err(_) => {
                set.release(crate::now_ns()).await;
                return Ok(Served::NotTaken(
                    "the bus was not set up inside the shard's Lease: the broker did not answer"
                        .into(),
                ));
            }
        },
    };
    // A shard taken: its epoch raised by the lake in every writer group
    // before anything is served, and stamped on every write from here on
    // (step 3d, checkpoint 5, item 32). A take not confirmed gives the
    // Lease up, and the pod stands by again (item 49).
    let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut holding = None;
    if let Some((set, shards)) = &mut shard {
        // Listening for the instance's wants from before the take (ruling
        // 184, option B; item 33): the lake delivers its standing wants
        // again as it answers, and the plugin's stream opens only later.
        // Only where the plugin hears wants.
        if meridian_sidecar::shards::hears_wants(&identity.roles) {
            shards.hold_wants(&bus, &identity.instance_id, HOLD_WANTS_FOR);
            // Held at the broker before the take is sent, or the take's own
            // delivery could pass it by. A broker slower than this holds the
            // take up no longer, and the lake's redelivery serves.
            match tokio::time::timeout(HELD_AT_THE_BROKER_WITHIN, bus.subscribed()).await {
                Ok(Ok(())) => {}
                Ok(Err(failed)) => tracing::warn!(
                    %failed,
                    "the take's hold of its wants is not known to be at the broker: the take goes on"
                ),
                Err(_) => tracing::warn!(
                    "the take's hold of its wants was not at the broker in time: the take goes on"
                ),
            }
        }
        // Renewed just before the take and just after it (review c5a M2,
        // M3): a take that stalled past its lapse is never sent.
        let raised = crate::shards::take_epoch(&bus, &identity.instance_id, set).await;
        if let Err(failed) = raised {
            set.release(crate::now_ns()).await;
            return Ok(Served::NotTaken(format!(
                "the shard's epoch was not raised: {failed}"
            )));
        }
        // Nothing sent once its serving has ended, whatever it still holds
        // until its runtime is shut (review c5a6 L3); and a data shard writes
        // on past its renew deadline with its lake record writes alone,
        // every other send waiting for a renewal (review c5a M1, ruled
        // 2026-11-16).
        let renewal = shards::writes_on(&identity.roles).then(|| set.renewal());
        bus.gate_sends(shards::serving_gate(Arc::clone(&ended), renewal))?;
        holding = set.holding();
    }
    // Its own hops' ages -- a record batch accepted, a row delivered -- to
    // the monitor, under its plugin's instance (W4.16; 606).
    tokio::spawn(crate::report_latency_forever(bus.clone()));
    let mut sidecar = Sidecar::new(bus, deployment_id, identity.clone());
    if let Some(verifier) = verifier {
        sidecar = sidecar.with_verifier(Arc::clone(verifier));
    }
    if let Some(share) = write_share {
        tracing::info!(
            bytes_per_second = share.bytes_per_second(),
            "the plugin's lake write share at its sidecar"
        );
        sidecar = sidecar.with_write_share(Arc::clone(share));
    }
    // Kept while it serves: its own Lease renewed, the others read.
    let mut keeping = None;
    if let Some((set, shards)) = shard {
        sidecar = sidecar.as_shard(Arc::clone(&shards));
        let live = Arc::clone(&shards);
        keeping = Some(tokio::spawn(
            set.keep(move |others| live.set_live(others), stop.clone()),
        ));
    }
    // A harness's sidecar, which no launch made, holds its plugin's
    // registration to the declaration an upload would carry (W4.1,
    // contract v25): the SDK's JSON, as `meridian-declaration` prints it.
    // The launcher never sets it.
    if let Some((path, declared)) = declaration {
        sidecar = sidecar.held_to(declared.clone());
        tracing::info!(path, "registration held to the declaration in this file");
    }
    let sidecar = Arc::new(sidecar);

    // A live instance of a development deployment: the development
    // endpoint behind the front door (spec/live-plugin-development). Both
    // told, or none.
    if let Some(live) = meridian_sidecar::live::Live::from_env(
        var("MERIDIAN_LIVE_DIR"),
        var("MERIDIAN_DEVELOPMENT").as_deref() == Some("true"),
        sidecar.clock(),
    ) {
        sidecar.go_live(Arc::new(live));
        tracing::info!("live: the development endpoint is on");
    }

    tracing::info!(instance_id, %listening, "the sidecar is serving");

    // What it knows of its plugin, for the conductor and the dashboard
    // (W4.8): now, on each registration, and every 30s.
    tokio::spawn(meridian_sidecar::report::report_forever(Arc::clone(
        &sidecar,
    )));

    // Bound as it is first served: a port it cannot bind ends the serving
    // as a door that stopped does, its Lease given up (review c5a6 L4).
    let door_sidecar = Arc::clone(&sidecar);
    let front_door = async {
        let Some((at, verifier, tls)) = front else {
            return std::future::pending().await;
        };
        let door = FrontDoor::new(door_sidecar, Arc::clone(verifier))?;
        let listener = tokio::net::TcpListener::bind(*at)
            .await
            .map_err(|failed| format!("the front door could not bind {at}: {failed}"))?;
        tracing::info!(%at, tls = tls.is_some(), "the front door is open to the dashboard");
        serve_door(listener, front_door::router(door), tls.clone())
            .await
            .map_err(|failed| format!("the front door stopped: {failed}"))
    };

    // The plugin's own share (stage M; the resilience spec's section 10a):
    // past it, only this plugin is refused.
    tracing::info!(
        in_flight = limits.in_flight,
        requests_per_second = limits.requests_per_second,
        answer_mib = limits.answer_mib,
        "the plugin's share at its sidecar"
    );
    let share = Share::new(limits);
    let surface = tonic::transport::Server::builder()
        // An older plugin's admin pages, read as pages at `admin` while its
        // contract is accepted (W4.8).
        .add_service(Shared::new(
            OlderPagesRead::new(SidecarServiceServer::from_arc(Arc::clone(&sidecar))),
            Arc::clone(&share),
        ))
        // The typed operations (spec/typed-sidecar-operations).
        .add_service(Shared::new(
            PluginOperationsServer::from_arc(sidecar),
            share,
        ))
        .serve(listening);

    // Whichever ended first.
    enum Woke {
        Surface(Result<(), String>),
        Door(Result<(), String>),
        Fenced,
        Kept(Result<Kept, tokio::task::JoinError>),
        Told,
    }
    let kept = async {
        match keeping.as_mut() {
            Some(keeping) => keeping.await,
            None => std::future::pending().await,
        }
    };
    let woke = tokio::select! {
        // A port already taken lands here, and so does a surface that
        // stopped later. Either way no plugin can reach this sidecar, so
        // it exits non-zero and an orchestrator restarts it rather than
        // leaving it up and unreachable.
        served = surface => Woke::Surface(
            served.map_err(|failed| format!("the sidecar surface at {listening} stopped: {failed}")),
        ),
        // The page is how a person reaches the plugin, so a door that
        // stopped is a sidecar to restart, as the surface is.
        stopped = front_door => Woke::Door(stopped),
        // A write refused as fenced (step 3d, checkpoint 5, item 32):
        // another pod took this shard.
        _ = meridian_sidecar::shards::fenced() => Woke::Fenced,
        // Its Lease shows another holder.
        kept = kept => Woke::Kept(kept),
        _ = crate::shards::told(&mut stop) => Woke::Told,
    };
    // Ended: nothing it still holds sends from here, until its runtime is
    // shut (review c5a6 L3).
    ended.store(true, std::sync::atomic::Ordering::SeqCst);
    // Ended unplanned -- fenced, its surface or front door stopped, its
    // keeping failed -- its Lease given up where it still names this pod,
    // so the next take need not wait out a lapse (review c5a6 L4). A Lease
    // shown held by another is left as it is; a planned stop released it.
    if matches!(
        woke,
        Woke::Surface(_) | Woke::Door(_) | Woke::Fenced | Woke::Kept(Err(_))
    ) {
        if let Some(keeping) = &keeping {
            keeping.abort();
        }
        if let Some(holding) = &holding {
            holding.give_up().await;
        }
    }
    match woke {
        Woke::Surface(ended) | Woke::Door(ended) => ended.map(|()| Served::Stopped),
        Woke::Fenced => Ok(Served::Lost(
            "a store refused a write as fenced: this shard's epoch is no longer the instance's"
                .into(),
        )),
        Woke::Kept(Ok(Kept::Lost(why))) => {
            Ok(Served::Lost(format!("the shard's lease is lost: {why}")))
        }
        Woke::Kept(Ok(Kept::Released)) => Ok(Served::Stopped),
        Woke::Kept(Err(failed)) => Err(format!("the shard's keeping failed: {failed}")),
        Woke::Told => {
            tracing::info!("stopping");
            // Its shard given up, so the standby takes it at once: its
            // keeping heard the same word.
            if let Some(keeping) = keeping {
                let _ = keeping.await;
            }
            Ok(Served::Stopped)
        }
    }
}

/// The front door served: over TLS where the deployment's internal TLS is
/// on, each connection's handshake its own task so one slow client holds up
/// no other; in the clear, as axum serves it, otherwise.
fn serve_door(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    tls: Option<tokio_rustls::TlsAcceptor>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>> {
    let Some(tls) = tls else {
        return Box::pin(async move { axum::serve(listener, router).await });
    };
    Box::pin(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            let tls = tls.clone();
            let service = hyper_util::service::TowerToHyperService::new(router.clone());
            tokio::spawn(async move {
                // A client that does not speak TLS -- plaintext, the
                // refusal internal TLS promises -- ends here.
                let Ok(stream) = tls.accept(stream).await else {
                    return;
                };
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(stream), service)
                .await;
            });
        }
    })
}

#[cfg(test)]
mod tests;
