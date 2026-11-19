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
            // A database past the first the chart brings (step 3d; section
            // 6.4): made, with its roles and each component's login, by its
            // superuser before its schema is applied.
            if let Some(serving) = var("MERIDIAN_SERVING_DATABASE_URL") {
                meridian_runtime::provision::provision_brought(&serving, &url)?;
            }
            meridian_runtime::migrate_once_it_answers(
                "the lake's database",
                &url,
                |url| PostgresStore::connect(url, 1).map_err(|failed| failed.to_string()),
                |store, lock_waits| {
                    store
                        .migrate_within(&*clock(), lock_waits)
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
    // Its writer group and the lake's count (step 3d; section 6.1): the
    // datasets it writes, and those it hands to their owners.
    // In ha each group's writer reads the write-ahead stream with its own
    // position, and commits its own datasets' writes (stage H).
    let groups = meridian_changes::groups::Groups::from_env()?;
    // Its pool of 8, and in ha 2 more for the write-ahead stream's writer --
    // its batch's transaction and its position -- so its commits never wait
    // behind the lake's reads (the chart's databaseConnections counts them).
    let pool = 8 + if write_ahead_on() {
        meridian_lake::write_ahead::CONNECTIONS
    } else {
        0
    };
    // Its own database login and its sessions' bounds (section 10a).
    meridian_runtime::serve_database_as("lake");
    // Its databases (step 3d; section 6.4; `lake.databases`): its group
    // writes to database `group mod count`, and reads fan out over them all.
    let databases = meridian_changes::databases::Databases::from_env("lake", &url)?;
    let own = databases.of_group(groups.mine());
    let connect = |d: u32, url: &str| {
        let what = if d == 0 {
            "the lake's database".to_string()
        } else {
            format!("the lake's database {d}")
        };
        wait_for_store(
            &what,
            url,
            |url| {
                // Its own database's pool, with the write-ahead writer's;
                // every other's for its reads.
                PostgresStore::connect(url, if d == own { pool } else { 8 })
                    .map(|store| {
                        store
                            .with_groups(groups)
                            .with_databases(databases.count())
                            .in_database(d)
                    })
                    .map_err(|failed| failed.to_string())
            },
            |store| {
                store.verify().map_err(|failed| match failed {
                    StoreError::SchemaAhead(_) => Wait::Refused(failed.to_string()),
                    other => Wait::NotYet(other.to_string()),
                })
            },
        )
    };
    // Every database's store, database 0 first: what the lake writes and
    // reads through, and what a move warms and switches (step 3d).
    let mut every_database: Vec<Arc<PostgresStore>> = Vec::new();
    if databases.several() {
        for (d, url) in databases.urls().iter().enumerate() {
            every_database.push(Arc::new(connect(d as u32, url)?));
        }
    } else {
        every_database.push(Arc::new(connect(0, &url)?));
    }
    let spread: Option<Arc<meridian_lake::spread::Spread>> = if databases.several() {
        tracing::info!(
            databases = databases.count(),
            own,
            "the lake writes its group's database and reads every one"
        );
        Some(Arc::new(meridian_lake::spread::Spread::new(
            every_database.clone(),
            groups,
        )))
    } else {
        None
    };
    let record: Arc<dyn meridian_lake::Store> = match &spread {
        Some(spread) => spread.clone(),
        None => every_database[0].clone(),
    };

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
    // A reader's record-read replica, made before the runtime and held here,
    // as the primary is, so its blocking client closes its connections after
    // the runtime has stopped, never on its worker (contract v30, stage D;
    // a reader made it inside the runtime from stage C2, connecting in
    // block_in_place after PR CI 43014977978's "Cannot start a runtime from
    // within a runtime").
    let replica: Option<Arc<dyn meridian_lake::Store>> = if member.is_reader() {
        lake_replica(&databases, groups)?
    } else {
        None
    };
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
        // The revision this member runs, said in the lake's database 0 for
        // as long as it runs, whatever its mode (step 3d; `lake_member`,
        // ruled 2026-11-15): what the control group's sweep waits on before
        // it moves a range home to another database.
        let revision = meridian_changes::members::revision(
            meridian_runtime::VERSION,
            databases.count(),
            groups.count(),
            None,
        );
        {
            let home = Arc::clone(&every_database[0]);
            meridian_runtime::member::keep_seen(
                "lake",
                &instance_id,
                revision.clone(),
                move |member, revision, started, now| {
                    // With what it serves as, in the one statement (stage
                    // T; approved 2026-11-15; the review's finding 9): its
                    // group, and its mode once it serves.
                    home.member_says(
                        member,
                        revision,
                        started,
                        now,
                        groups.mine(),
                        &meridian_runtime::member::serving_mode(),
                    )
                    .map_err(|failed| failed.to_string())
                },
            );
        }
        // Told to stop, a writer of a group past the first serves on until
        // another group's writer serves (stage T; approved 2026-11-15): the
        // look it waits on, at the store's members in database 0.
        // No clock is compared with another pod's (the review's findings
        // 9 and 10): it watches for a writer said again between its own
        // looks that it did not find at its first. Where no lease fences
        // its group, its own group's replacement serving is leave to stop
        // too (finding 3).
        let another_serves = {
            let home = Arc::clone(&every_database[0]);
            let me = meridian_runtime::member::seen_as(&instance_id);
            let watch = std::sync::Mutex::new(meridian_changes::members::Watch::new(
                groups.mine(),
                &revision,
                !member.leased(),
            ));
            move |_told_at_ns: i64, _now_ns: i64| {
                let others = home.members_but(&me).map_err(|failed| failed.to_string())?;
                Ok(watch.lock().expect("watch poisoned").serves(&others))
            }
        };
        // A standby takes no free lease while a newer member of its group
        // is there (ruled 2026-11-16): a rollout's member at the new count
        // takes it, never an old one whose takeover at the old count fences
        // the groups the new count added; unless nobody takes it within
        // one lease duration.
        let member = member.deferring_to_newer({
            let home = Arc::clone(&every_database[0]);
            let me = meridian_runtime::member::seen_as(&instance_id);
            let newer = std::sync::Mutex::new(meridian_changes::members::Newer::new(
                groups.mine(),
                &revision,
            ));
            move || {
                let others = home.members_but(&me).map_err(|failed| failed.to_string())?;
                Ok(newer.lock().expect("newer poisoned").there(&others))
            }
        });
        // A reader member (contract v30, stage B): the lake's reads alone,
        // its hot layer kept by the writer's change log, never writing.
        if member.is_reader() {
            bus.serve_only(meridian_bus::read_after::is_read);
            // Each range's writer group, as a writer reads them, at start and
            // every few seconds: a range is read where it is written (step
            // 3d; a reader that never read them took each range for its
            // dataset's rendezvous owner's once the count of databases was
            // raised).
            if groups.several() {
                tokio::task::block_in_place(|| record.read_owners_again()).map_err(|failed| {
                    format!("the ranges' writer groups could not be read: {failed}")
                })?;
                let reading = Arc::clone(&record);
                meridian_runtime::background(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        let record = Arc::clone(&reading);
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Err(failed) = record.read_owners_again() {
                                tracing::debug!(%failed, "the ranges' writer groups were not read again");
                            }
                        })
                        .await;
                    }
                });
            }
            // Made before the runtime and held outside it (stage D): its
            // connections close after the runtime has stopped.
            let replica = replica.clone();
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
        // Told to stop, heard from here on (stage T's review, finding 6):
        // once this member holds its lease, a signal while it is still
        // starting, or waiting on the broker to say it serves, is kept and
        // answered by giving the lease up, never by ending where it stands.
        let mut told = meridian_runtime::stopping();
        // A standby told to stop holds nothing and stops at once; a stop
        // that lands while it takes the lease lets the take finish, then
        // gives the lease up (ruled 2026-11-15).
        let Some(holding) = member.wait_to_write_unless(&bus, &mut told).await? else {
            tracing::info!("stopping");
            return Ok(());
        };
        // Each range's writer group (step 3d; `writer_group` ruled
        // 2026-11-14): the control partition's group names every one not
        // named yet, by its dataset's owner at the count now, so a later
        // change of the count moves nothing until a move moves it; every
        // member reads them at start and every few seconds, and again
        // wherever a write found them changed. Read before the epoch is
        // adopted, which goes by them.
        if groups.several() {
            tokio::task::block_in_place(|| -> Result<(), String> {
                for store in &every_database {
                    if groups.holds_control() {
                        let pinned = store.pin_owners().map_err(|failed| failed.to_string())?;
                        if pinned > 0 {
                            tracing::info!(pinned, "each range named its writer group");
                        }
                    }
                }
                // By the furthest row over every database, a written row
                // before a move's pointer (the review's C4).
                record
                    .read_owners_again()
                    .map_err(|failed| failed.to_string())?;
                Ok(())
            })?;
        }
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
        let writing = Arc::clone(&store);
        holding.keep(
            move || writing.epoch(),
            move || watched.head_epoch().map_err(|failed| failed.to_string()),
        );
        // Its partitions in its report, as its record lists them, and its
        // control partition: what a standby following it names (CI
        // 44556229125: a writer named none).
        let listing = Arc::clone(&record);
        holding.report_partitions(move || {
            listing
                .heads()
                .map(|heads| {
                    heads
                        .into_keys()
                        .chain([meridian_changes::CONTROL.to_string()])
                        .collect()
                })
                .map_err(|failed| failed.to_string())
        });
        // A new database timeline raises the epoch (contract v30, stage C):
        // the hot layer let go of what the record may have lost.
        let following = Arc::clone(&store);
        holding.follow_timeline(&bus, "lake", move || {
            following
                .follow_timeline()
                .map_err(|failed| failed.to_string())
        });
        // Several writer groups (step 3d; section 6.7): each write's
        // datasets of another group handed to it, and the writes handed to
        // this group answered here; a group past the first serves its
        // writes and the change log alone, and does none of the lake's own
        // work. Before the lake's handlers.
        if groups.several() {
            hot.for_group(groups);
            bus.hand_writes_on(
                meridian_lake::service::INTERNAL_RECORD,
                groups.mine(),
                meridian_lake::service::is_record_write,
            );
            // Its owner decided from the request first (the ownership
            // pre-check decided 2026-11-13): a write wholly another group's
            // handed to it before any of its rows is checked.
            bus.route_writes_by(move |topic, envelope| {
                meridian_lake::service::owner_of_write(groups, topic, envelope)
            });
            let reading = Arc::clone(&record);
            meridian_runtime::background(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    let record = Arc::clone(&reading);
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(failed) = record.read_owners_again() {
                            tracing::debug!(%failed, "the ranges' writer groups were not read again");
                        }
                    })
                    .await;
                }
            });
            if !groups.holds_control() {
                let changes = meridian_changes::read_changes_topic("lake");
                bus.serve_only(move |topic| {
                    meridian_lake::service::is_record_write(topic)
                        || topic == changes
                        // Its raise of a shard's epoch (step 3d, item 32).
                        || meridian_changes::shard_fence::is_raise_topic(topic)
                });
            }
            tracing::info!(
                group = groups.mine(),
                groups = groups.count(),
                "the lake writes its writer group's datasets, and hands every other's to their owners"
            );
        }
        // Several databases (step 3d; section 6.4): group 0's writer copies
        // what database 0 keeps about rows elsewhere into their databases
        // and relays each database's own control journal into database 0's,
        // as it starts and at each sweep, under its lease.
        if let (Some(spread), true) = (&spread, groups.holds_control()) {
            let sweeping = Arc::clone(spread);
            std::thread::spawn(move || loop {
                sweeping.reconcile_and_say();
                match sweeping.relay() {
                    Ok(0) => {}
                    Ok(relayed) => tracing::info!(
                        relayed,
                        "the lake's databases' own records relayed into database 0's"
                    ),
                    Err(failed) => {
                        tracing::warn!(%failed, "the lake's databases' own records were not relayed; again at the next sweep")
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(SWEEP_SECONDS));
            });
        }
        // The scrub, under the lease (step 3b, integrity at rest): the
        // control partition's group's, which reads every dataset, in every
        // database (each database's findings in its own journal).
        for scrubbing in databases.urls().to_vec() {
            if !groups.holds_control() {
                break;
            }
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
                            .and_then(|store| {
                                store.record_scrub(scrubbed, member, started, finished)
                            })
                            .map_err(|failed| failed.to_string())
                    },
                )
            });
        }
        // `lake_row` partitioned online (step 3d; section 6.2): the rows
        // kept before it copied behind into their ranges' months, a page at
        // a time, by the control partition's group's writer, under its lease;
        // nothing once none is left. The rows move inside Postgres: the
        // lake holds a page's row places, never its rows.
        if groups.holds_control() {
            let converting = url.clone();
            std::thread::spawn(move || convert_forever(&converting));
        }
        // A range's move (step 3d; section 6.8): the control partition's
        // group runs each, one at a time, over every database; and, with
        // several groups, those a changed count calls for, at start and
        // each minute after.
        if groups.holds_control() {
            let mover = Arc::new(
                meridian_lake::partition_moves::Mover::new(
                    every_database.clone(),
                    groups,
                    meridian_lake::partition_moves::Settings::from_env(),
                    clock(),
                )
                .waiting_for_rollout(
                    revision.clone(),
                    meridian_runtime::member::seen_every_and_within().1,
                )
                .logging(),
            );
            meridian_lake::partition_moves::serve(&bus, &mover);
            if groups.several() {
                meridian_runtime::background(async move {
                    loop {
                        let mover = Arc::clone(&mover);
                        let moved = tokio::task::spawn_blocking(move || mover.rebalance().len())
                            .await
                            .unwrap_or(0);
                        if moved > 0 {
                            tracing::info!(
                                moved,
                                "ranges moved by the changed count of writer groups"
                            );
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    }
                });
            }
        }
        // Every plugin shard's epoch, in every database this member writes,
        // raised to the highest its takes' records name before it serves: a
        // database that lost a raise, or joined later, holds it (review c5a
        // H1, L1).
        let reseating = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || reseating.reseat_shards()).await {
            Ok(Ok(0)) => {}
            Ok(Ok(raised)) => tracing::warn!(
                raised,
                "plugin shards' epochs raised to their records' as the lake starts"
            ),
            Ok(Err(failed)) => {
                return Err(format!(
                    "the plugin shards' epochs were not raised to their records': {failed}"
                ))
            }
            Err(failed) => return Err(format!("the plugin shards' raise did not run: {failed}")),
        }
        // Every handler and subscription in place before this returns.
        let lake =
            meridian_lake::service::serve_grouped(bus.clone(), Arc::clone(&store), clock(), groups);
        lake.build_at_most(answers_at_once);
        // The book's writer groups (`bor.writerGroups`; step 3d, checkpoint
        // 5, item 32): each raises a shard's epoch before a take is answered.
        lake.with_book_groups(book_writer_groups()?);

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
            // A shard's raise reads the stream's place (review c5a H2).
            lake.with_write_ahead(client.clone());
            let writing = Arc::clone(&lake);
            let stepping = holding.stepping_down();
            meridian_runtime::background(async move {
                if let Err(failed) =
                    meridian_lake::write_ahead::write_forever(writing, client, bounds, |why| {
                        match why {
                            Some(why) => meridian_runtime::member::said().set_waiting(why),
                            None => meridian_runtime::member::said().done_waiting(),
                        }
                    })
                    .await
                {
                    stepping.now(&failed).await;
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
        if ready.serving_on_unless(&bus, &mut told).await {
            let _ = told.wait_for(|told| *told).await;
        }
        tracing::info!("stopping");
        // No lease fences this writer's group (the tolerant default), and
        // its replacement starts beside it while it serves on: it commits
        // nothing more, from now (stage T's review, finding 3; ruled
        // 2026-11-15). Every write it receives is handed on -- its own
        // group's to its group, where its replacement answers once it
        // serves -- its reads and its shared work are left to the members
        // that serve them, and every commit of its own is refused. It only
        // keeps the store's public write topics served meanwhile. With a
        // lease nothing changes: no standby writes until it is given up.
        if !holding.took_over()
            && groups.mine() != 0
            && groups.several()
            && bus.stop_writing_here(meridian_bus::read_after::is_read)
        {
            for store in &every_database {
                store.stop_committing();
            }
            tracing::info!(
                writer_group = groups.mine(),
                "told to stop with no lease: committing nothing more, every write handed on"
            );
        }
        // A rollout keeps the store's public topics served (stage T).
        // Fenced meanwhile -- the count lowered, another group's writer
        // taking its partitions over before it serves -- it commits nothing
        // more through any database, at the fence, and answers each request
        // "ask again" until its serve-on ends (ruled 2026-11-18, item 110;
        // review-c5n2 M2), one that already hands every write on, with no
        // lease, too (item 120): nothing more handed on by the owners it
        // last read.
        let stopping = every_database.clone();
        holding
            .serve_on_until_another_group_serves(
                groups.mine(),
                groups.count(),
                meridian_runtime::now_ns(),
                another_serves,
                &bus,
                move || {
                    for store in &stopping {
                        store.stop_committing();
                    }
                },
            )
            .await;
        holding.release().await;
        Ok(())
    })
}

/// Copy the rows kept before the lake's ranges into their months until none
/// is left (step 3d; section 6.2): a page, then a pause, so the writer's own
/// work goes first; a page that fails tried again shortly.
fn convert_forever(url: &str) {
    let store = loop {
        match PostgresStore::connect(url, 1) {
            Ok(store) => break store,
            Err(failed) => {
                tracing::warn!(%failed, "the lake's rows could not be converted yet");
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    };
    let mut moved: u64 = 0;
    loop {
        match store.convert_page() {
            Ok(0) => {
                if moved > 0 {
                    tracing::info!(
                        moved,
                        "the lake's rows kept before its ranges are converted"
                    );
                }
                return;
            }
            Ok(rows) => {
                moved += rows;
                if moved % 100_000 < rows {
                    tracing::info!(moved, "the lake's rows kept before its ranges, converting");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(failed) => {
                tracing::warn!(%failed, "a page of the lake's rows was not converted; again shortly");
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }
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

/// How often group 0's writer reconciles the lake's databases and relays
/// their own records (step 3d; section 6.4).
const SWEEP_SECONDS: u64 = 30;

/// A reader's record-read replicas (contract v30, stage B; step 3d): one
/// database's where the lake has one, else every database's, each read from
/// its own replica where the chart names one -- database 0's from
/// `MERIDIAN_LAKE_READ_DATABASE_URL`, each past it from
/// `MERIDIAN_LAKE_READ_DATABASE_URL_<d>` -- and from the database itself
/// where it names none.
fn lake_replica(
    databases: &meridian_changes::databases::Databases,
    groups: meridian_changes::groups::Groups,
) -> Result<Option<Arc<dyn meridian_lake::Store>>, String> {
    let verify = |store: &PostgresStore| {
        store
            .verify()
            .map_err(|failed| Wait::NotYet(failed.to_string()))
    };
    let connect = |url: &str| {
        PostgresStore::connect(url, 4)
            .map(|store| store.with_groups(groups).with_databases(databases.count()))
            .map_err(|failed| failed.to_string())
    };
    let first = meridian_runtime::read_replica(
        "the lake's read replica",
        &meridian_changes::databases::read_url_var("lake", 0),
        connect,
        verify,
    )?;
    if !databases.several() {
        return Ok(first.map(|store| Arc::new(store) as Arc<dyn meridian_lake::Store>));
    }
    let Some(first) = first else {
        return Ok(None);
    };
    let mut stores = vec![Arc::new(first)];
    for (d, url) in databases.urls().iter().enumerate().skip(1) {
        let replica = var(&meridian_changes::databases::read_url_var("lake", d as u32))
            .unwrap_or_else(|| url.clone());
        // Each answering the ranges its database's groups write (step 3d).
        stores.push(Arc::new(
            wait_for_store(
                &format!("the lake's read replica of database {d}"),
                &replica,
                connect,
                verify,
            )?
            .in_database(d as u32),
        ));
    }
    Ok(Some(Arc::new(meridian_lake::spread::Spread::new(
        stores, groups,
    ))))
}

/// The book's count of writer groups, as the chart gives the lake
/// `bor.writerGroups` (step 3d, checkpoint 5, item 32): 1 where unset.
const BOR_WRITER_GROUPS_VAR: &str = "MERIDIAN_BOR_WRITER_GROUPS";

fn book_writer_groups() -> Result<u32, String> {
    match std::env::var(BOR_WRITER_GROUPS_VAR) {
        Err(_) => Ok(1),
        Ok(said) => said
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|count| (1..=64).contains(count))
            .ok_or_else(|| format!("{BOR_WRITER_GROUPS_VAR} is {said:?}; a whole number, 1 to 64")),
    }
}
