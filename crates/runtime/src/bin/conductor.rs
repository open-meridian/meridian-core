//! The deployment's end of the link to the platform, as its own process.
//!
//! Holds the key, makes the outbound connection, carries a miss to the platform
//! and the answer back onto the bus, and reports what the deployment is running.
//! Decision 011.
//!
//! It holds the configuration store: what a deployment admin authors, which
//! nothing else can rebuild. It held no database until 2026-09-21, on the
//! argument that a control process accumulating a store becomes the one nobody
//! writes migrations for. Configuration was ruled to live here, as it did in
//! v1, so the store came with its migration history, and
//! `meridian-conductor migrate` applies it once per release. Starting verifies
//! and refuses a schema it does not recognise.
//!
//! `conductor public-key` prints the public half of the deployment's key,
//! generating one if there is none. That moved here with the key: the process
//! that holds a private half is the process that can speak for its public one.

use std::sync::{Arc, Mutex};

use meridian_conductor::{Conductor, SystemClock, INSTRUMENT_MISSING};
use meridian_config::PostgresStore;
use meridian_domain::v1::{EnrolWithCodeRequest, EnrolmentState};
use meridian_runtime::{
    bus_from_env, key_at, now_ns, platform_from_env, report_forever, required, shutdown, var,
    PlatformUpstream,
};

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

/// W7.2, the wizard's first question. In the config domain, which is where
/// everything the dashboard asks the conductor lives.
const ENROLMENT_STATE: &str = "platform.config.query.enrolment";
const ENROL_WITH_CODE: &str = "platform.config.command.enrol-with-code";

fn run() -> Result<(), String> {
    let command = std::env::args().nth(1);

    // Before the key, because migrating needs a database and nothing else: a
    // migration job holds no key and should not need one.
    if command.as_deref() == Some("migrate") {
        let url = required("MERIDIAN_CONFIG_DATABASE_URL")?;
        PostgresStore::connect(&url, 1)
            .and_then(|store| store.migrate())
            .map(|()| {
                tracing::info!(
                    version = meridian_config::migrations::latest(),
                    "the configuration store's schema is applied"
                )
            })
            .map_err(|failed| {
                format!("the configuration store's schema could not be applied: {failed}")
            })?;
        return grant_if_serving(&url);
    }

    let key_path = var("MERIDIAN_KEY_PATH").unwrap_or_else(|| "/var/lib/meridian/key.pem".into());
    let key = key_at(&key_path)?;

    // And before the database, because printing the public half needs the key
    // and nothing else.
    if command.as_deref() == Some("public-key") {
        println!(
            "{}",
            key.public_key_pem().map_err(|failed| failed.to_string())?
        );
        return Ok(());
    }

    // No database is first run, not a misconfiguration: the wizard is where a
    // deployment's database is chosen, and the wizard cannot run without this
    // component, which enrols the deployment and relays its claim code. What
    // the conductor holds in the meantime is nothing anybody has authored
    // yet, so memory is the honest place for it.
    let store: Arc<dyn meridian_config::Store> = match var("MERIDIAN_CONFIG_DATABASE_URL") {
        Some(url) => {
            let store = PostgresStore::connect(&url, 8).map_err(|failed| failed.to_string())?;
            // Verified, never applied, for the reason every store gives.
            store.verify().map_err(|failed| failed.to_string())?;
            Arc::new(store)
        }
        None => {
            tracing::info!(
                "no database is configured: the configuration store is in memory \
                 until first run applies one"
            );
            Arc::new(meridian_config::MemoryStore::default())
        }
    };

    install_named_administrator(store.as_ref());

    let instance_id = var("MERIDIAN_INSTANCE_ID").unwrap_or_else(|| "conductor-1".into());
    let public_key_pem = key.public_key_pem().map_err(|failed| failed.to_string())?;
    let platform = platform_from_env(key)?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|failed| failed.to_string())?
        .block_on(async {
            // W5.24, before anything asks the platform for anything: a fresh
            // deployment has generated a key nothing has registered, and an
            // enrolment code is what registers it. Never fatal. A deployment
            // whose code has expired or been spent keeps running and says so,
            // because the wizard is where somebody enters a new one and the
            // wizard is served by a component that has to be up.
            let enrolment = Arc::new(Mutex::new(
                enrol(&platform, &public_key_pem, &key_path).await,
            ));

            let bus = bus_from_env(&instance_id).await?;

            // W7.2. The wizard's first page, and everything it shows until a
            // claim code is redeemed. The conductor holds the key and is what
            // enrols, so it is what knows.
            serve_enrolment_state(&bus, Arc::clone(&enrolment));
            serve_enrol_with_code(
                &bus,
                Arc::clone(&enrolment),
                Arc::clone(&platform),
                public_key_pem.clone(),
                key_path.clone(),
            );

            // Subscribed before the loop starts, for the reason at-most-once
            // delivery makes unforgiving: what arrives before a subscriber
            // exists is dropped, and dropped silently.
            let misses = bus.subscribe(INSTRUMENT_MISSING);

            // The config domain: registered and subscribed before the loop
            // starts, so nothing the dashboard or a sidecar asks is missed.
            meridian_config::serve(
                Arc::clone(&bus),
                Arc::clone(&store),
                Arc::new(meridian_config::SystemClock),
                Arc::new(PlatformUpstream::new(Arc::clone(&platform))),
            );

            let carrying = Arc::clone(&bus);
            let conductor = Conductor::new(carrying, Arc::clone(&platform), Arc::new(SystemClock));
            let running = tokio::spawn(conductor.consume(misses));

            // W5.19 outward, W5.20 inward: this component holds the key, so it
            // is the one that can tell the platform anything, and what it tells
            // it includes what the others have said about themselves.
            let reporting = Arc::clone(&platform);
            let collecting = Arc::clone(&bus);
            let schema = meridian_config::migrations::latest();
            tokio::spawn(async move {
                report_forever(reporting, collecting, "conductor", schema).await
            });

            tracing::info!(
                instance_id,
                platform = platform.address(),
                started_at_ns = now_ns(),
                "the conductor is connected"
            );

            tokio::select! {
                _ = running => {
                    tracing::warn!("the bus shut down");
                    Ok(())
                }
                _ = shutdown() => {
                    tracing::info!("stopping");
                    Ok(())
                }
            }
        })
}

/// Answer what the wizard shows before anything is redeemed.
///
/// Shared rather than moved in, because enrolment is no longer only something
/// that happened at start: a code entered in the wizard changes this, and a
/// page that still showed the old answer would be telling somebody their
/// repair had failed.
fn serve_enrolment_state(bus: &Arc<meridian_bus::Bus>, state: Arc<Mutex<EnrolmentState>>) {
    use prost::Message;

    bus.serve(ENROLMENT_STATE, move |envelope| {
        if envelope.payload_type != "meridian.v1.EnrolmentStateRequest" {
            return Err(format!(
                "{ENROLMENT_STATE} expects meridian.v1.EnrolmentStateRequest, and this is {}",
                envelope.payload_type
            ));
        }
        let held = state.lock().expect("enrolment lock poisoned").clone();
        Ok((
            "meridian.v1.EnrolmentState".to_string(),
            held.encode_to_vec(),
        ))
    });
}

/// W7.2. Enrol with a code somebody entered in the wizard.
///
/// Requirement 9's other half: the wizard said "issue another code and give it
/// to this deployment" and had nowhere to put one, so the only way out of a
/// spent or mistyped code was a `helm upgrade`.
///
/// Unauthenticated, and it can be nothing else: a deployment with no key
/// cannot sign, so nothing it might present could be checked. The code is the
/// credential, which is what it was made to be.
fn serve_enrol_with_code(
    bus: &Arc<meridian_bus::Bus>,
    state: Arc<Mutex<EnrolmentState>>,
    platform: Arc<meridian_conductor::Platform>,
    public_key_pem: String,
    key_path: String,
) {
    use prost::Message;

    // Taken here rather than inside the handler: handlers run on a blocking
    // thread, where there is no runtime to find.
    let runtime = tokio::runtime::Handle::current();

    bus.serve(ENROL_WITH_CODE, move |envelope| {
        if envelope.payload_type != "meridian.v1.EnrolWithCodeRequest" {
            return Err(format!(
                "{ENROL_WITH_CODE} expects meridian.v1.EnrolWithCodeRequest, and this is {}",
                envelope.payload_type
            ));
        }
        let request = EnrolWithCodeRequest::decode(&envelope.payload[..])
            .map_err(|failed| format!("that is not an EnrolWithCodeRequest: {failed}"))?;

        let code = request.code.trim().to_string();
        if code.is_empty() {
            let mut held = state.lock().expect("enrolment lock poisoned").clone();
            held.refusal_reason = "no code was entered".into();
            return Ok((
                "meridian.v1.EnrolmentState".to_string(),
                held.encode_to_vec(),
            ));
        }

        let answered = runtime.block_on(enrol_with_code(
            &platform,
            &public_key_pem,
            &key_path,
            &code,
        ));
        *state.lock().expect("enrolment lock poisoned") = answered.clone();
        Ok((
            "meridian.v1.EnrolmentState".to_string(),
            answered.encode_to_vec(),
        ))
    });
}

/// Register the deployment's own key, once, with a code the install carried.
///
/// The marker beside the key is what makes it once: it is written where the
/// key lives, so a deployment that keeps its key keeps the knowledge that the
/// key is registered, and one that lost its volume enrols again with a new
/// code exactly as a new deployment would.
async fn enrol(
    platform: &Arc<meridian_conductor::Platform>,
    public_key_pem: &str,
    key_path: &str,
) -> EnrolmentState {
    let held = |enrolled, fingerprint: String, refusal_reason: String| {
        enrolment_state(public_key_pem, enrolled, fingerprint, refusal_reason)
    };

    let marker = std::path::Path::new(key_path).with_extension("enrolled");
    if let Ok(fingerprint) = std::fs::read_to_string(&marker) {
        return held(true, fingerprint.trim().to_string(), String::new());
    }

    let Some(code) = var("MERIDIAN_ENROLMENT_CODE") else {
        return held(false, String::new(), "no enrolment code".into());
    };

    enrol_with_code(platform, public_key_pem, key_path, &code).await
}

/// What the wizard is shown, built in one place so a retry and a start cannot
/// describe the same deployment differently.
fn enrolment_state(
    public_key_pem: &str,
    enrolled: bool,
    fingerprint: String,
    refusal_reason: String,
) -> EnrolmentState {
    EnrolmentState {
        deployment_id: var("MERIDIAN_DEPLOYMENT_ID").unwrap_or_default(),
        enrolled,
        fingerprint,
        refusal_reason,
        public_key_pem: public_key_pem.to_string(),
    }
}

/// W7.2. Enrol with a code, from the install or from the wizard.
///
/// Requirement 9: a code that expired or was already spent leaves a deployment
/// running and unenrolled, and a new one entered in the wizard fixes it with
/// nothing reinstalled. Both routes arrive here, so the marker is written and
/// the refusal is worded once.
async fn enrol_with_code(
    platform: &Arc<meridian_conductor::Platform>,
    public_key_pem: &str,
    key_path: &str,
    code: &str,
) -> EnrolmentState {
    let state = |enrolled, fingerprint: String, refusal_reason: String| {
        enrolment_state(public_key_pem, enrolled, fingerprint, refusal_reason)
    };
    let marker = std::path::Path::new(key_path).with_extension("enrolled");

    // A deployment that already holds a registered key does not enrol again,
    // and the platform refuses it anyway. Saying so here is the difference
    // between an answer and a round trip that ends in "already used".
    if let Ok(fingerprint) = std::fs::read_to_string(&marker) {
        return state(true, fingerprint.trim().to_string(), String::new());
    }

    match platform.enrol_key(code, public_key_pem, now_ns()).await {
        Ok(enrolled) => {
            // The fingerprint, so an administrator comparing it with the
            // platform's own page can see that this deployment's key is the
            // one registered, and not somebody else's spent from the same
            // code.
            tracing::info!(
                key_id = enrolled.key_id,
                fingerprint = enrolled.fingerprint,
                "this deployment enrolled its key"
            );
            if let Err(failed) = std::fs::write(&marker, &enrolled.fingerprint) {
                tracing::warn!(
                    %failed,
                    "the key is enrolled and the marker could not be written; \
                     the next start will try to enrol again and be refused"
                );
            }
            state(true, enrolled.fingerprint, String::new())
        }
        Err(failed) => {
            tracing::error!(
                %failed,
                "this deployment could not enrol its key. Issue another enrolment \
                 code on the platform and give it to the deployment"
            );
            state(false, String::new(), failed.to_string())
        }
    }
}

/// W7.6. Write the permission for whoever the wizard named, once.
///
/// Applying the configuration recorded them; the permission had to wait,
/// because access records live in this store and this store's database was
/// one of the things being configured. The Job writes the Secret and restarts
/// this component, so by the time this runs there is somewhere to write to
/// (decisions/017).
///
/// Never fatal. A deployment that cannot write this is a deployment somebody
/// recovers with a claim code, and refusing to start would take away the
/// dashboard they would redeem it in.
fn install_named_administrator(store: &dyn meridian_config::Store) {
    let group = var("MERIDIAN_ADMINISTRATOR_DIRECTORY_GROUP").unwrap_or_default();
    let login = var("MERIDIAN_ADMINISTRATOR_LOGIN").unwrap_or_default();

    match meridian_config::install_named_administrator(
        store,
        &meridian_config::SystemClock,
        &group,
        &login,
    ) {
        // Every start after the first, and every deployment whose
        // administrator arrived another way. The write refuses itself when one
        // exists, which is what makes this safe to call unconditionally.
        Ok(false) => {}
        Ok(true) => tracing::info!(
            directory_group = group,
            login = login,
            "the administrator the wizard named holds deployment admin"
        ),
        Err(failed) => tracing::error!(
            %failed,
            "the administrator the wizard named could not be written. \
             Issue a claim code on the platform and redeem it at first sign-in"
        ),
    }
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
