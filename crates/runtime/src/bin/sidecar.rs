//! A plugin's sidecar, as its own process.
//!
//! The one surface a plugin ever touches: gRPC on loopback, which means a
//! container in the same pod. It holds the broker credential the plugin beside
//! it has no mount for, and that filesystem seam is the boundary decisions 004
//! and 010 rest on.
//!
//! One per plugin, which is why it is a process rather than a thread inside
//! the runtime: a shared sidecar holds one registration, and a second plugin
//! admitted on it would inherit the first's grants.
//!
//! Who this serves is launch configuration and never what registers: a plugin
//! that named its own roles would be choosing its own privileges.

use meridian_runtime::lease::KubeLeases;
use meridian_runtime::sidecar_serving::{serve, serve_as_shards, MakeBus, Serving};
use meridian_runtime::{
    bus_from_env, bus_from_env_writing_ahead, names_from, required, stopping, var,
};
use std::sync::Arc;

use meridian_sidecar::front_door::Verifier;
use meridian_sidecar::share::Limits;
use meridian_sidecar::{Identity, DEFAULT_BIND};

/// Where the chart mounts the dashboard's public keys, one file per key id.
const DASHBOARD_KEYS: &str = "/etc/meridian/dashboard-keys";

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

/// The SDK's declaration JSON in `path`, read as an upload's is.
fn declaration_from(path: &str) -> Result<meridian_pb::v1::PluginDeclaration, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|failed| format!("MERIDIAN_PLUGIN_DECLARATION {path}: {failed}"))?;
    let declared: meridian_domain::declaration::Declared =
        serde_json::from_str(&text).map_err(|failed| {
            format!("MERIDIAN_PLUGIN_DECLARATION {path} is not a declaration: {failed}")
        })?;
    meridian_domain::declaration::from_json(&declared)
        .map_err(|why| format!("MERIDIAN_PLUGIN_DECLARATION {path}: {why}"))
}

fn run() -> Result<(), String> {
    let deployment_id = required("MERIDIAN_DEPLOYMENT_ID")?;

    // Roles, never a grant table: what they may do is the contract's, compiled
    // into this binary (decisions/020).
    let identity = Identity::new(
        var("MERIDIAN_PLUGIN_INSTANCE_ID").unwrap_or_default(),
        names_from(var("MERIDIAN_PLUGIN_ROLES")),
    );

    let address = var("MERIDIAN_SIDECAR_ADDRESS").unwrap_or_else(|| DEFAULT_BIND.into());
    let listening: std::net::SocketAddr = address
        .parse()
        .map_err(|failed| format!("{address} is not an address: {failed}"))?;

    // An override off loopback is a deployment decision, and it is the one that
    // opens the boundary, so it is said out loud rather than inferred later
    // from a port map.
    if !listening.ip().is_loopback() {
        tracing::warn!(
            %listening,
            "the sidecar is reachable beyond loopback; anything that can route \
             here can register as a plugin"
        );
    }

    // The dashboard's public keys, one file per key id, read when an id is
    // first met. The front door verifies each page request with them, and a
    // command sent for a person (W4.9) is verified with the same ones.
    let verifier = Some(identity.instance_id.clone())
        .filter(|id| !id.is_empty())
        .map(|instance| {
            let keys = var("MERIDIAN_DASHBOARD_KEYS_DIR").unwrap_or_else(|| DASHBOARD_KEYS.into());
            Arc::new(Verifier::new(instance, keys))
        });

    // The front door (decisions/014, 021): where the dashboard sends a
    // person's requests for this plugin's page. Off loopback by necessity,
    // since the dashboard is another pod, and closed to everything else by the
    // chart's NetworkPolicy; unset, there is none, and nobody reaches the page.
    let front = match var("MERIDIAN_FRONT_DOOR_ADDRESS") {
        None => None,
        Some(address) => {
            let Some(verifier) = verifier.clone() else {
                return Err(
                    "a front door needs MERIDIAN_PLUGIN_INSTANCE_ID: it admits only \
                     assertions for this instance"
                        .into(),
                );
            };
            let at: std::net::SocketAddr = address
                .parse()
                .map_err(|failed| format!("{address} is not an address: {failed}"))?;
            // Over TLS where the deployment's internal TLS is on (ruling
            // 1084): the chart's certificate for the sidecars' names, and
            // nothing in the clear.
            let tls = meridian_changes::tls::server_config()?
                .map(|config| tokio_rustls::TlsAcceptor::from(Arc::new(config)));
            Some((at, verifier, tls))
        }
    };

    // On the bus as its plugin's instance, so what it sends names the plugin
    // and what the conductor answers it -- its configuration, its links -- is
    // that plugin's: the conductor answers for the instance the envelope
    // names. A sidecar launched with no plugin instance falls back to its
    // own name, and has no plugin to answer for.
    let instance_id = Some(identity.instance_id.clone())
        .filter(|id| !id.is_empty())
        .or_else(|| var("MERIDIAN_INSTANCE_ID"))
        .unwrap_or_else(|| "sidecar-1".into());

    // Whether its plugin records the lake's rows: the data role's (W10.4).
    let writes_rows = identity.roles.iter().any(|role| role == "data");

    // The plugin's share, refused at start where the chart or the launch
    // set it to something that is not one.
    let limits = Limits::from_env()?;

    // Its instance's shards (step 3d; the resilience spec's section 7): set
    // by the launcher on a launch with several shards or a standby pod.
    // Each shard holds the instance's share divided by the count.
    let shard_count =
        meridian_runtime::shards::count(var(meridian_runtime::shards::SHARDS_ENV).as_deref())?;
    let limits = limits.per_shard(shard_count.unwrap_or(1));
    // A data plugin's lake write share (section 6.9), refused at start
    // where it is not one; no other role writes the lake's rows.
    let write_share = meridian_sidecar::share::WriteShare::from_env(shard_count.unwrap_or(1))?
        .filter(|_| writes_rows);

    // What every serving of this sidecar is made from: read once.
    let declaration = var("MERIDIAN_PLUGIN_DECLARATION")
        .map(|path| declaration_from(&path).map(|declared| (path, declared)))
        .transpose()?;
    // Each serving's bus, from the environment. A data plugin's lake writes
    // go by the lake's write-ahead stream in `ha` (the resilience spec's
    // section 4): held by the stream before the lake records them, so none
    // it was answered for is lost.
    let bus: MakeBus = {
        let instance_id = instance_id.clone();
        Arc::new(move || {
            let instance_id = instance_id.clone();
            Box::pin(async move {
                if writes_rows {
                    bus_from_env_writing_ahead(&instance_id).await
                } else {
                    bus_from_env(&instance_id).await
                }
            })
        })
    };
    let serving = Arc::new(Serving {
        deployment_id,
        identity,
        verifier,
        front,
        listening,
        instance_id,
        limits,
        write_share,
        declaration,
        bus,
    });

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            // Told to stop once, heard from here on: a SIGTERM that lands
            // while this pod waits for a shard, or between its shard and
            // its serving, is not one a later listener would have missed.
            let stop = stopping();
            // Its own budget (stage M), inside the desktop footprint's +15 %
            // (1058): the plugin's share keeps a flood off it. The plugin's
            // answers in flight (answerMiB) and its read_after session are
            // held apart, so what is left is the sidecar's own work's,
            // whatever the plugin does; a share that leaves it nothing is
            // refused at start, as the chart refuses it. Once per process:
            // a shard's pod serves many times in it.
            meridian_budget::install(
                "sidecar",
                serving.limits.held_apart(),
                meridian_budget::permits_from_env(),
            )?;
            let Some(count) = shard_count else {
                // An instance's one pod: served once, its process its own.
                return serve::<KubeLeases>(serving, None, stop).await.map(|_| ());
            };
            let whole = |name: &str, default: u64| -> Result<u64, String> {
                match var(name) {
                    None => Ok(default),
                    Some(said) => said
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| format!("{name} is {said:?}; a whole number")),
                }
            };
            let renew = std::time::Duration::from_secs(whole("MERIDIAN_LEASE_RENEW_SECONDS", 2)?);
            let duration =
                std::time::Duration::from_secs(whole("MERIDIAN_LEASE_DURATION_SECONDS", 6)?);
            // Each request gives up within half a renewal: a shard stops at
            // its renew deadline whatever is in flight, and one request lost
            // leaves it a second try before that.
            let leases = || KubeLeases::in_cluster(renew / 2).map(Arc::new);
            serve_as_shards(serving, count, leases, (renew, duration), stop).await
        })
}
