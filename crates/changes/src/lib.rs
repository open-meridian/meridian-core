//! The change log each store keeps (step 3, plan Track 1;
//! spec/one-data-access-model section 3, "The change log"; rulings 1046,
//! 1056, 1057, 1060).
//!
//! Each partition of each store keeps one ordered log of its committed
//! changes. It is not a new record: it is the store's own committed changes
//! read in their order -- the lake's rows, the book's journal, and for the
//! instrument store, the street store and the conductor's configuration store
//! every row their transactions write, captured in the same transaction
//! ([`captured`]).
//!
//! - **Ordering.** A change takes the next sequence of its partition from the
//!   partition's head row inside its own transaction; a rollback takes it
//!   back, so there are no holes. A follower applies changes in that order.
//! - **Catch-up.** A follower asks for the changes after a sequence on
//!   `platform.{domain}.internal.read-changes`, a page of at most 512 KiB at a
//!   time ([`serve`]), and hears new ones on
//!   `platform.{domain}.internal.changes.{partition}`, published after commit,
//!   at most once ([`publish`]). A delivery missed is caught up by the read,
//!   never inferred.
//! - **Behind retention.** A follower whose sequence is below the partition's
//!   lowest kept is refused naming `after_sequence` ([`ReadError::Behind`]);
//!   it reloads the partition's state at its head and follows from there.
//! - **Fencing.** Each partition's head row carries an `epoch`; a writer
//!   commits only while the head row's epoch is its own, checked under the
//!   lock it already takes ([`Fence`]). A takeover raises it; a deposed
//!   writer's next commit fails.
//! - **Origin.** Every change carries the deployment it was made in (1049).
//!
//! These are component topics and core's own protos: the plugin contract and
//! `CONTRACT_CURRENT` do not move for them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use meridian_bus::Bus;
pub use meridian_domain::v1::{Change, ReadChangesReply, ReadChangesRequest};
use prost::Message;

pub mod busy;
pub mod captured;
pub mod databases;
pub mod group_commit;
pub mod groups;
pub mod integrity;
pub mod members;
pub mod moves;
pub mod records;
#[cfg(feature = "scratch")]
pub mod scratch;
pub mod serving;
pub mod shard_fence;
pub mod timeline;
pub mod tls;

/// The most a page of changes holds, encoded (1056: 512 KiB pages).
pub const PAGE_MOST: usize = 512 * 1024;

/// The partition every store keeps beside its others for its own control:
/// assignments, takeovers, moves, switches and findings (ruling 1060). The
/// book has kept one since its first migration.
pub const CONTROL: &str = "control";

/// How long a store waits on a database that has stopped answering before
/// it gives the connection up (step 3b; spec section 4): a primary lost
/// mid-connection -- its pod gone, its node cut -- sends no reset, and a
/// connection left to the kernel's defaults waits minutes for one. Each
/// bound is the URL's where it names one.
pub const DATABASE_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(5);

/// A store's database URL read, with the bounds above where it names none,
/// so after a failover the pool's next connection reaches the promoted
/// primary through the database's Service rather than waiting on the old.
pub fn database_config(url: &str) -> Result<postgres::Config, postgres::Error> {
    let mut config: postgres::Config = url.parse()?;
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(DATABASE_GIVE_UP);
    }
    if config.get_tcp_user_timeout().is_none() {
        config.tcp_user_timeout(DATABASE_GIVE_UP);
    }
    // The kernel's own idle time, two hours, unless the URL said otherwise.
    if config.get_keepalives_idle() == std::time::Duration::from_secs(2 * 60 * 60) {
        config
            .keepalives(true)
            .keepalives_idle(DATABASE_GIVE_UP)
            .keepalives_interval(std::time::Duration::from_secs(2))
            .keepalives_retries(2);
    }
    // TLS where the deployment's internal TLS is on, or the URL asks for
    // it; in the clear, as always, otherwise (ruling 1084).
    tls::database_tls_mode(&mut config, tls::names_ssl_mode(url));
    // A serving process's own login and its session's bounds (section 10a).
    serving::applied(&mut config);
    Ok(config)
}

/// How many connections a store's pool keeps open while it is idle (PO 67,
/// 2026-11-17). Each one is a database backend held for nothing while the
/// store is quiet, and the desktop footprint (ruling 1058) counts them. A
/// pool still opens more, up to its size, as they are wanted, and each it
/// opens reads the database's timeline first ([`timeline::Watching`]).
pub const IDLE_CONNECTIONS: u32 = 2;

/// A store's pool of at most `size` connections, keeping
/// [`IDLE_CONNECTIONS`] open while idle (fewer where it is smaller). Before
/// PO 67 a pool kept its whole size open from its start.
pub fn pool<M: r2d2::ManageConnection>(size: u32) -> r2d2::Builder<M> {
    let size = size.max(1);
    r2d2::Pool::builder()
        .max_size(size)
        .min_idle(Some(IDLE_CONNECTIONS.min(size)))
}

/// Where a store's members ask a partition's changes after a sequence.
pub fn read_changes_topic(domain: &str) -> String {
    format!("platform.{domain}.internal.read-changes")
}

/// Where a store's members hear each new change of a partition.
pub fn changes_topic(domain: &str, partition: &str) -> String {
    format!("platform.{domain}.internal.changes.{partition}")
}

/// Every partition's tail of a domain, as one subscription names it.
pub fn every_changes_topic(domain: &str) -> String {
    format!("platform.{domain}.internal.changes.*")
}

/// The bytes a page may hold for what was asked: 512 KiB where nothing, or
/// more than that, was.
pub fn page_most(asked: u64) -> usize {
    if asked == 0 || asked > PAGE_MOST as u64 {
        PAGE_MOST
    } else {
        asked as usize
    }
}

/// Why a read of changes was not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The store keeps no such partition.
    UnknownPartition(String),
    /// The asker is below the partition's lowest kept: it reloads the
    /// partition's state at its head and follows from there.
    Behind {
        after_sequence: u64,
        lowest_kept: u64,
    },
    /// The store could not be read.
    Unavailable(String),
}

/// The words a refusal behind retention starts with, which a follower reads
/// to know it must reload rather than retry.
pub const BEHIND: &str = "after_sequence is behind the partition's lowest kept";

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::UnknownPartition(partition) => {
                write!(f, "no partition {partition} is kept here")
            }
            ReadError::Behind {
                after_sequence,
                lowest_kept,
            } => write!(
                f,
                "{BEHIND}: after_sequence {after_sequence}, lowest kept {lowest_kept}; \
                 reload the partition at its head and follow from there"
            ),
            ReadError::Unavailable(detail) => write!(f, "the store could not be read: {detail}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// Whether a refusal's words say the asker is behind retention.
pub fn is_behind(detail: &str) -> bool {
    detail.contains(BEHIND)
}

/// The floor below which a partition's log no longer reaches: its lowest
/// kept sequence, or past its head where it keeps none. An asker after a
/// sequence below `floor - 1` would need changes no longer kept.
pub fn check_kept(after_sequence: u64, lowest_kept: u64, head: u64) -> Result<(), ReadError> {
    let floor = if lowest_kept == 0 {
        head + 1
    } else {
        lowest_kept
    };
    if after_sequence + 1 < floor && after_sequence < head {
        return Err(ReadError::Behind {
            after_sequence,
            lowest_kept,
        });
    }
    Ok(())
}

/// A page of changes being filled: at least one change whatever its size,
/// then as many as fit within the page's bytes.
#[derive(Debug)]
pub struct Page {
    most: usize,
    used: usize,
    pub changes: Vec<Change>,
}

impl Page {
    pub fn new(most: usize) -> Self {
        Self {
            most,
            used: 0,
            changes: Vec::new(),
        }
    }

    /// Take the change if it fits; false once the page is full, the change
    /// left for the next page.
    pub fn take(&mut self, change: Change) -> bool {
        let size = change.encoded_len() + 4;
        if !self.changes.is_empty() && self.used + size > self.most {
            return false;
        }
        self.used += size;
        self.changes.push(change);
        true
    }

    pub fn is_full(&self) -> bool {
        self.used >= self.most
    }
}

/// A store's change log, read a page at a time.
pub trait ChangeLog: Send + Sync {
    /// The changes of `partition` after `after_sequence`, in order, at most
    /// `most_bytes` encoded (and always at least one, where there is one),
    /// with the partition's head, lowest kept and epoch.
    fn read_changes(
        &self,
        partition: &str,
        after_sequence: u64,
        most_bytes: usize,
    ) -> Result<ReadChangesReply, ReadError>;
}

/// Serve `platform.{domain}.internal.read-changes` from `log`.
pub fn serve(bus: &Arc<Bus>, domain: &str, log: Arc<dyn ChangeLog>) {
    // Its reads held to their `read_after` (contract v30, stage B): never
    // answered from behind the watermark a read names, by this log's heads.
    bus.reads_after()
        .keep(domain, Arc::new(LogPosition(Arc::clone(&log))));
    let topic = read_changes_topic(domain);
    let named = topic.clone();
    let serving = interest_key(bus, domain);
    bus.serve(&topic, move |envelope| {
        if envelope.payload_type != "meridian.v1.ReadChangesRequest" {
            return Err(format!(
                "{named} takes meridian.v1.ReadChangesRequest, not {}",
                envelope.payload_type
            ));
        }
        let request = ReadChangesRequest::decode(envelope.payload.as_slice())
            .map_err(|failed| format!("a ReadChangesRequest did not read: {failed}"))?;
        if request.partition.is_empty() {
            return Err("partition is empty; name the partition whose changes are asked".into());
        }
        // A follower asking is a follower holding interest: the tail is
        // published while one does (ruling 1062).
        note_interest(&serving);
        let started = std::time::Instant::now();
        let reply = log
            .read_changes(
                &request.partition,
                request.after_sequence,
                page_most(request.page_size),
            )
            .map_err(|failed| failed.to_string())?;
        tracing::debug!(
            topic = named.as_str(),
            changes = reply.changes.len(),
            head = reply.head,
            bytes = reply.encoded_len(),
            ms = format!("{:.1}", started.elapsed().as_secs_f64() * 1e3),
            "a page of changes read"
        );
        Ok((
            "meridian.v1.ReadChangesReply".to_string(),
            reply.encode_to_vec(),
        ))
    });
}

/// What the tail has published since the process started (ruling 1062):
/// pages, their bytes as published (compressed), the changes they held, and
/// the changes committed while no follower held interest, published as
/// nothing. What the tail's traffic measurement reads.
pub static TAIL_MESSAGES: AtomicU64 = AtomicU64::new(0);
pub static TAIL_BYTES: AtomicU64 = AtomicU64::new(0);
pub static TAIL_CHANGES: AtomicU64 = AtomicU64::new(0);
pub static TAIL_WITHHELD: AtomicU64 = AtomicU64::new(0);

/// The tail's payload: one page, a `ReadChangesReply`, LZ4-compressed with
/// its length before it (ruling 1062: one compressed message per committed
/// transaction, as Postgres ships a commit and Kafka a producer's batch).
pub const TAIL_PAYLOAD: &str = "meridian.v1.ReadChangesReply+lz4";

/// How long a follower's last read keeps the tail published. A standby or
/// reader asks at least once a second (`Member`'s follower), so a follower
/// that stopped asking is gone well within this; with none, the writer
/// publishes nothing (the desktop; ruling 1062).
pub const INTEREST_LASTS: std::time::Duration = std::time::Duration::from_secs(10);

static INTEREST: OnceLock<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>> =
    OnceLock::new();

fn interest(
) -> std::sync::MutexGuard<'static, std::collections::HashMap<String, std::time::Instant>> {
    INTEREST
        .get_or_init(Default::default)
        .lock()
        .expect("the tail's interest is poisoned")
}

/// Which writer a follower's interest is in: this process's bus (one per
/// process; a test may hold several) and the store's domain.
fn interest_key(bus: &Bus, domain: &str) -> String {
    format!("{:p}/{domain}", bus as *const Bus)
}

fn note_interest(key: &str) {
    interest().insert(key.to_string(), std::time::Instant::now());
}

/// Whether a follower of `domain` on this bus asked within
/// [`INTEREST_LASTS`].
pub fn followed(bus: &Bus, domain: &str) -> bool {
    interest()
        .get(&interest_key(bus, domain))
        .is_some_and(|at| at.elapsed() < INTEREST_LASTS)
}

/// Publish one committed transaction's changes on their partition's tail,
/// after commit, at most once: one page (or, past 512 KiB, a page per 512
/// KiB), compressed, saying the sequence it follows (`after`, the
/// partition's head before the transaction) so a follower knows whether it
/// missed one. A member that misses one catches up by the read. Published
/// only while a follower holds interest; with none, nothing (ruling 1062).
pub fn publish(bus: &Bus, domain: &str, after: u64, changes: &[Change]) {
    if changes.is_empty() {
        return;
    }
    // Committed by this process, so a read naming no more is answered with
    // no look at the record (contract v30, stage B).
    for change in changes {
        bus.reads_after()
            .committed(domain, &change.partition, change.sequence);
    }
    if !followed(bus, domain) {
        TAIL_WITHHELD.fetch_add(changes.len() as u64, Ordering::Relaxed);
        return;
    }
    let mut after = after;
    let mut start = 0;
    while start < changes.len() {
        let partition = changes[start].partition.as_str();
        let end = changes[start..]
            .iter()
            .position(|change| change.partition != partition)
            .map_or(changes.len(), |at| start + at);
        let mut page = Page::new(PAGE_MOST);
        for change in &changes[start..end] {
            if !page.take(change.clone()) {
                after = publish_page(
                    bus,
                    domain,
                    partition,
                    after,
                    std::mem::take(&mut page.changes),
                );
                page = Page::new(PAGE_MOST);
                page.take(change.clone());
            }
        }
        publish_page(bus, domain, partition, after, page.changes);
        // A transaction over two partitions does not say where the second
        // starts: its follower reads what it lacks.
        after = 0;
        start = end;
    }
}

/// One page on the tail; answers its head, which the next page follows.
fn publish_page(bus: &Bus, domain: &str, partition: &str, after: u64, changes: Vec<Change>) -> u64 {
    let Some(last) = changes.last() else {
        return after;
    };
    let page = ReadChangesReply {
        head: last.sequence,
        lowest_kept: 0,
        epoch: last.epoch,
        after_sequence: after,
        changes,
    };
    let head = page.head;
    let count = page.changes.len() as u64;
    let payload = lz4_flex::compress_prepend_size(&page.encode_to_vec());
    let topic = changes_topic(domain, partition);
    TAIL_MESSAGES.fetch_add(1, Ordering::Relaxed);
    TAIL_BYTES.fetch_add(payload.len() as u64, Ordering::Relaxed);
    TAIL_CHANGES.fetch_add(count, Ordering::Relaxed);
    if let Err(failed) = bus.publish(&topic, TAIL_PAYLOAD, payload, None, None) {
        // Committed is committed: a follower catches up by the read.
        tracing::warn!(topic, %failed, "a change was committed and not published");
    }
    head
}

/// A page heard on the tail, decompressed and read.
pub fn read_tail(payload_type: &str, payload: &[u8]) -> Result<ReadChangesReply, String> {
    if payload_type != TAIL_PAYLOAD {
        return Err(format!(
            "the tail carries {TAIL_PAYLOAD}, not {payload_type}"
        ));
    }
    let raw = lz4_flex::decompress_size_prepended(payload)
        .map_err(|failed| format!("a page on the tail did not decompress: {failed}"))?;
    ReadChangesReply::decode(raw.as_slice())
        .map_err(|failed| format!("a page on the tail did not read: {failed}"))
}

/// Follow a partition from `after`: read pages until the head, applying each
/// change in order, and answer the sequence reached. A page that does not
/// start where the follower is, or a change out of order, is refused: a hole
/// is never inferred over.
pub fn catch_up<R, A>(mut read: R, mut after: u64, mut apply: A) -> Result<u64, ReadError>
where
    R: FnMut(u64) -> Result<ReadChangesReply, ReadError>,
    A: FnMut(&Change),
{
    loop {
        let page = read(after)?;
        for change in &page.changes {
            if change.sequence <= after {
                return Err(ReadError::Unavailable(format!(
                    "change {} came after {after}: the log is out of order",
                    change.sequence
                )));
            }
            apply(change);
            after = change.sequence;
        }
        if page.changes.is_empty() || after >= page.head {
            return Ok(after);
        }
    }
}

/// The deployment a store's changes are made in: its ID, set once at start
/// from what the chart says (`MERIDIAN_DEPLOYMENT_ID`), and empty where
/// nothing said (a test, a local stack).
static ORIGIN: OnceLock<String> = OnceLock::new();

/// Name this process's deployment, once.
pub fn set_origin(deployment_id: &str) {
    let _ = ORIGIN.set(deployment_id.to_string());
}

/// This process's deployment, as every change it makes carries it.
pub fn origin() -> &'static str {
    ORIGIN.get().map(String::as_str).unwrap_or("")
}

/// This process's member, its pod's name (step 3b): who a store's writer
/// says raised its epoch, where it raises one itself on a new database
/// timeline (contract v30, stage C). Set once by the runtime; empty in a
/// test.
static MEMBER: OnceLock<String> = OnceLock::new();

/// Name this process's member, once.
pub fn set_member(name: &str) {
    let _ = MEMBER.set(name.to_string());
}

/// This process's member.
pub fn member() -> &'static str {
    MEMBER.get().map(String::as_str).unwrap_or("")
}

/// How long a takeover waits for each deposed writer's session it ends to
/// go, in milliseconds (`pg_terminate_backend`'s timeout).
pub const DEPOSED_GONE_WITHIN_MS: i64 = 5_000;

/// End every other session holding a writer's lock on a store's head tables
/// -- a row taken `FOR UPDATE`, or written -- before a takeover raises the
/// epoch there: a deposed writer stopped mid-transaction (its node paused,
/// its process frozen) holds the head row it locked until its own client
/// moves again, and the server never hears it go (the frozen node's kernel
/// still answers TCP). The takeover's raise waited on it for 35 s in CI
/// (#99's ha run 42496994613, node-freeze, under the write driver's load),
/// past the component tier (1048). Its transaction was never acknowledged
/// and is aborted; a commit it had made already stays. Readers, which take
/// no row lock, are left alone. A session of the store's own role may end
/// another of that role, so no new right is needed. Answers how many ended.
pub fn end_deposed_writers(
    client: &mut impl postgres::GenericClient,
    heads: &[&str],
) -> Result<i64, postgres::Error> {
    let heads: Vec<String> = heads.iter().map(|head| head.to_string()).collect();
    let ended: i64 = client
        .query_one(
            "SELECT count(*) FILTER (WHERE pg_terminate_backend(pid, $2))
               FROM (SELECT DISTINCT l.pid
                       FROM pg_locks l
                      WHERE l.locktype = 'relation' AND l.granted
                        AND l.pid <> pg_backend_pid()
                        AND l.mode IN ('RowShareLock', 'RowExclusiveLock')
                        AND l.relation = ANY (SELECT to_regclass(name)::oid
                                                FROM unnest($1::text[]) AS name)) AS held",
            &[&heads, &DEPOSED_GONE_WITHIN_MS],
        )?
        .get(0);
    if ended > 0 {
        tracing::warn!(
            sessions = ended,
            "ended the deposed writer's sessions still holding the head rows, so the takeover is not held up by them"
        );
    }
    Ok(ended)
}

/// The deployment's clock, as the composition root hands it over: what a
/// captured log dates each change by.
static CLOCK: OnceLock<Arc<dyn meridian_clock::Clock>> = OnceLock::new();

/// Hand this process's clock over, once.
pub fn set_clock(clock: Arc<dyn meridian_clock::Clock>) {
    let _ = CLOCK.set(clock);
}

/// The deployment's time now; 0 where no clock was handed over.
pub fn now_ns() -> i64 {
    CLOCK.get().map_or(0, |clock| clock.now_ns())
}

/// A writer's fencing token (Kleppmann's): the epoch it adopted when it
/// started writing. The store that is the record checks it against the
/// partition's head row under the lock each commit already takes; a takeover
/// raises the head row's, and a deposed writer's next commit fails.
#[derive(Debug, Default)]
pub struct Fence {
    /// 0 until adopted.
    epoch: AtomicU64,
    /// Told to stop, and committing nothing more ([`Fence::stop`]).
    stopped: std::sync::atomic::AtomicBool,
}

impl Fence {
    pub const fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            stopped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Commit nothing more under this fence: every commit from here is
    /// refused as a fenced one is, whatever the head's epoch (step 3d,
    /// stage T's review, finding 3). What a writer with no lease does when
    /// told to stop while it serves on: its replacement starts beside it
    /// under the same epoch, which fences neither, so the one stopping
    /// stops committing itself, at once and for good. No epoch is raised
    /// and no takeover recorded; this process is not told it was deposed.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Whether [`Fence::stop`] was called.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// The epoch adopted; 0 if none yet.
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Adopt `epoch`: what a writer does at start, and after it takes over.
    pub fn adopt(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::SeqCst);
    }

    /// Adopt `epoch`, which this writer raised in the transaction `commit`
    /// commits, before that commit can be read; where it fails, the epoch
    /// held before is kept (PR CI 44283011761). A commit of this writer's
    /// own that waits behind the raise's locks reads the raised head row
    /// the moment it lands: adopted only after, that commit was refused as
    /// fenced by a newer writer, and the member stepped down from its own
    /// raise.
    pub fn adopt_committing<T, E>(
        &self,
        epoch: u64,
        commit: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let before = self.epoch.swap(epoch, Ordering::SeqCst);
        let committed = commit();
        if committed.is_err() {
            let _ = self
                .epoch
                .compare_exchange(epoch, before, Ordering::SeqCst, Ordering::SeqCst);
        }
        committed
    }

    /// Adopt `epoch` where nothing was adopted yet, and answer the one held.
    pub fn adopt_once(&self, epoch: u64) -> u64 {
        match self
            .epoch
            .compare_exchange(0, epoch, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => epoch,
            Err(held) => held,
        }
    }

    /// Whether a commit under this fence may proceed against a head row at
    /// `head_epoch`. A refusal is also said to the process
    /// ([`fenced_seen`]), whose member steps down (step 3b).
    pub fn admits(&self, head_epoch: u64) -> Result<(), Fenced> {
        let mine = self.epoch();
        if self.stopped() {
            return Err(Fenced {
                mine: Fenced::STOPPED,
                head: head_epoch,
            });
        }
        if mine == head_epoch {
            Ok(())
        } else {
            if head_epoch > mine && mine != 0 {
                FENCED_SEEN.store(true, Ordering::SeqCst);
            }
            Err(Fenced {
                mine,
                head: head_epoch,
            })
        }
    }
}

/// Who took a store's partitions over, and why: written as the store's
/// `WriterTookOver` record in the same transaction that raises the epoch
/// (step 3b; spec/a-deployment-scales-out-and-fails-over-by-configuration
/// section 3; decisions/031). Kept for good, one per epoch raised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TookOver {
    /// The member taking over: its pod's name.
    pub member: String,
    /// The member that held the lease before, its pod's name; empty where
    /// none did or it is not known (contract v30, stage D: the record's
    /// `previous_member`).
    pub previous: String,
    /// Why: the lease lapsed, the writer released it, or nobody held it.
    pub why: String,
    /// When, in the deployment's time.
    pub at_ns: i64,
}

impl TookOver {
    /// A takeover where no member is named: a test's, or a store's tool.
    pub fn unnamed(why: &str) -> Self {
        Self {
            member: String::new(),
            previous: String::new(),
            why: why.to_string(),
            at_ns: now_ns(),
        }
    }
}

/// The words each takeover is recorded with.
pub const LEASE_LAPSED: &str = "the writer's lease lapsed";
pub const LEASE_RELEASED: &str = "the writer released its lease";
pub const LEASE_FREE: &str = "no member held the lease";

/// Set once a commit of this process was refused by a newer epoch: another
/// member took the partition over, and this one steps down and follows as a
/// standby (spec/a-deployment-scales-out-and-fails-over-by-configuration
/// section 3, "A frozen writer").
static FENCED_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a commit of this process was refused by a newer epoch.
pub fn fenced_seen() -> bool {
    FENCED_SEEN.load(Ordering::SeqCst)
}

/// A commit refused by the epoch: another writer took the partition over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fenced {
    pub mine: u64,
    pub head: u64,
}

/// The words a fenced commit is refused with, everywhere.
pub const FENCED: &str = "fenced: this writer's epoch is no longer the partition's";

/// The words a commit is refused with once its writer was told to stop.
pub const STOPPED: &str = "this writer was told to stop and commits nothing more";

impl Fenced {
    /// `mine` where no epoch refused the commit: its writer was told to
    /// stop, and commits nothing more ([`Fence::stop`]).
    pub const STOPPED: u64 = u64::MAX;

    /// As the bus carries it to the caller: UNAVAILABLE with retry-after 1,
    /// asked again of the writer that serves by then (ruled 2026-11-18,
    /// items 110 and 120; the resilience spec's section 5). Nothing of the
    /// commit was made. What every store answers a fenced commit with
    /// (review-c5n5 F1): in plain words the sidecar said it ABORTED, with
    /// no retry-after.
    pub fn on_the_bus(&self) -> String {
        meridian_bus::refusal_status_retrying(
            meridian_bus::RefusalStatus::Unavailable,
            FENCED_RETRY_SECONDS,
            self,
        )
    }
}

/// How long a caller refused by a fenced writer waits before it asks again
/// ([`Fenced::on_the_bus`]; ruled 2026-11-18, item 120).
pub const FENCED_RETRY_SECONDS: u32 = 1;

impl std::fmt::Display for Fenced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.mine == Self::STOPPED {
            return write!(
                f,
                "{STOPPED}; its group's writer that replaces it commits it: ask again"
            );
        }
        write!(
            f,
            "{FENCED} (this writer's {}, the partition's {}); another writer took it over, \
             and this one steps down",
            self.mine, self.head
        )
    }
}

impl std::error::Error for Fenced {}

/// How long a migration waits for its schema's advisory lock: the
/// runtime's bound on a migration's wait (MIGRATION_WAITS_AT_MOST).
pub const SCHEMA_LOCK_WAITS_AT_MOST: std::time::Duration = std::time::Duration::from_secs(300);

/// A migration's schema lock (`pg_advisory_lock(key)`, held by the session
/// until it lets it go), waited for at most `wait` (review c5w3 L-c). Cut
/// off by the network alone, a migration's server session lives on holding
/// the lock until its keepalive ends it; the migration applied again
/// (meridian_runtime::migrate_once_it_answers) would wait for it past its
/// bound. Only the lock's wait is bounded: the migration's own statements
/// wait as they did. Refused past `wait` (SQLSTATE 55P03, lock not
/// available).
pub fn take_schema_lock(
    client: &mut impl postgres::GenericClient,
    key: i64,
    wait: std::time::Duration,
) -> Result<(), postgres::Error> {
    client.batch_execute(&format!(
        "SET lock_timeout = '{}ms'",
        wait.as_millis().max(1)
    ))?;
    let taken = client.execute("SELECT pg_advisory_lock($1)", &[&key]);
    let reset = client.batch_execute("RESET lock_timeout");
    taken?;
    reset
}

#[cfg(test)]
mod tests;

/// A store's member's position in its record, for `read_after` (contract
/// v30, stage B): each partition's head, as its change log reads it. A
/// partition the record does not hold is absent, so a read naming it waits.
struct LogPosition(Arc<dyn ChangeLog>);

impl meridian_bus::read_after::Position for LogPosition {
    fn heads(
        &self,
        partitions: &[String],
    ) -> Result<std::collections::HashMap<String, u64>, String> {
        let mut heads = std::collections::HashMap::new();
        for partition in partitions {
            // From past the head: the head alone, no change read.
            match self.0.read_changes(partition, i64::MAX as u64, 1) {
                Ok(reply) => {
                    heads.insert(partition.clone(), reply.head);
                }
                Err(ReadError::UnknownPartition(_)) => {}
                Err(failed) => return Err(failed.to_string()),
            }
        }
        Ok(heads)
    }
}
