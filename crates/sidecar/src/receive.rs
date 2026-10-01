//! What a plugin's roles hear, delivered typed (W4.3).
//!
//! One stream per plugin, `PluginOperations/Receive`: each item a `Delivery`
//! of what is known of it -- the envelope's identifiers and time, the row,
//! the record's journal reference and cause, and whether the cause was this
//! plugin's own act (Q6) -- and the row's message in plugin-facing form, or a
//! `Lost` where this sidecar dropped something it knows of. The rows, and how
//! each one's message is read, are generated from the matrix and
//! matrix/scoped.tsv into `operations.rs`; this is the part that does not
//! vary by row.
//!
//! **Within the read scope.** Each message is read for the account its row's
//! message names (scoped.tsv), and delivered only when that account is in the
//! plugin's read scope, as the conductor last said it (W4.11); a message
//! declared unscoped is delivered whatever the scope. The sidecar sees an
//! out-of-scope message before it filters, within the trust boundary; the
//! plugin never does.
//!
//! **Bounded, at most once, never silent.** At most 1024 deliveries wait for
//! the plugin; one past that is dropped, and so may the bus drop one for the
//! subscription. Either way a `Lost` takes its place in the stream's order,
//! carrying how many and of which rows where this knows -- its own queue --
//! and none of either where it does not -- the bus's drop count risen -- so a
//! loss at the end of a stream is seen at once rather than at the next
//! change (spec/plugins-hear-and-read, Q1; decisions/024). Which of the two
//! it was is logged here; the plugin's move is the same either way: it reads
//! the changes since the last it saw from the store.

use std::collections::{BTreeSet, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::Delivery as Heard;
use meridian_pb::plugin::v1 as plugin;
use tokio::sync::{watch, Notify};
use tokio_stream::Stream;
use tonic::{Response, Status};

use crate::configuration::Configuration;
use crate::service::Sidecar;

/// The most deliveries that wait for the plugin (W4.3).
pub(crate) const QUEUE: usize = 1024;

/// How often a subscription's drop count is read when nothing arrives on it,
/// so a loss at the end of a stream is told without a later delivery.
const DROPS_READ_EVERY: Duration = Duration::from_millis(250);

/// One row a plugin's roles may hear: its topic, and how its message is read.
pub(crate) struct Row {
    pub name: &'static str,
    pub step: &'static str,
    pub topic: &'static str,
    pub payload_type: &'static str,
    pub read: fn(&[u8]) -> Result<Read, prost::DecodeError>,
}

/// A row's message, read: the account it names, `None` when its row is
/// declared unscoped; its journal reference and cause; and the item.
pub(crate) struct Read {
    pub account: Option<String>,
    pub journal: Option<plugin::JournalRef>,
    pub cause: Option<plugin::ChangeCause>,
    pub item: plugin::delivery::Item,
}

pub type Deliveries = Pin<Box<dyn Stream<Item = Result<plugin::Delivery, Status>> + Send>>;

/// Deliveries dropped since the last `Lost`: how many and of which rows,
/// where this knows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Loss {
    dropped: u64,
    rows: BTreeSet<String>,
    /// Some were dropped uncounted, or of rows not known: the count and the
    /// rows are then said to be unknown, which is every row.
    count_unknown: bool,
    rows_unknown: bool,
}

impl Loss {
    fn add(&mut self, dropped: Option<u64>, row: Option<&str>) {
        match dropped {
            Some(dropped) => self.dropped += dropped,
            None => self.count_unknown = true,
        }
        match row {
            Some(row) => {
                self.rows.insert(row.to_string());
            }
            None => self.rows_unknown = true,
        }
    }

    fn delivered(self) -> plugin::Delivery {
        plugin::Delivery {
            meta: Some(plugin::DeliveryMeta::default()),
            item: Some(plugin::delivery::Item::Lost(plugin::Lost {
                dropped: if self.count_unknown { 0 } else { self.dropped },
                rows: if self.rows_unknown {
                    Vec::new()
                } else {
                    self.rows.into_iter().collect()
                },
            })),
        }
    }
}

/// What waits for the plugin, in order, and a loss not yet said.
#[derive(Default)]
struct Queue {
    items: VecDeque<plugin::Delivery>,
    /// Deliveries waiting, its losses not counted.
    waiting: usize,
    lost: Option<Loss>,
}

/// The queue, and what wakes the stream when it has something.
#[derive(Default)]
struct Waiting {
    queue: Mutex<Queue>,
    ready: Notify,
}

impl Waiting {
    /// A delivery queued after any loss before it, or dropped and counted
    /// when the queue is full.
    fn push(&self, delivery: plugin::Delivery, row: &str) {
        let mut queue = self.queue.lock().expect("receive queue poisoned");
        if queue.waiting >= QUEUE {
            if queue.lost.is_none() {
                tracing::warn!(row, "the plugin's delivery queue is full; dropping");
            }
            queue
                .lost
                .get_or_insert_with(Loss::default)
                .add(Some(1), Some(row));
        } else {
            if let Some(loss) = queue.lost.take() {
                queue.items.push_back(loss.delivered());
            }
            queue.items.push_back(delivery);
            queue.waiting += 1;
        }
        drop(queue);
        self.ready.notify_one();
    }

    /// Deliveries lost before they reached this, said in the stream's order.
    fn lose(&self, dropped: Option<u64>, row: Option<&str>) {
        let mut queue = self.queue.lock().expect("receive queue poisoned");
        queue
            .lost
            .get_or_insert_with(Loss::default)
            .add(dropped, row);
        drop(queue);
        self.ready.notify_one();
    }

    /// The next item: a delivery, or, once every delivery before it has
    /// gone, the loss after them.
    fn pop(&self) -> Option<plugin::Delivery> {
        let mut queue = self.queue.lock().expect("receive queue poisoned");
        if let Some(item) = queue.items.pop_front() {
            if !matches!(item.item, Some(plugin::delivery::Item::Lost(_))) {
                queue.waiting -= 1;
            }
            return Some(item);
        }
        queue.lost.take().map(Loss::delivered)
    }
}

// Status is tonic's error throughout the operations; boxing it here alone
// would buy nothing.
#[allow(clippy::result_large_err)]
impl Sidecar {
    /// W4.3: the rows asked for, or every row the plugin's roles hear, as one
    /// stream. Subscribed before this answers, so nothing published after the
    /// plugin has its answer is missed by the stream: the SDK reads the store
    /// once it has it.
    pub(crate) async fn receive_typed(
        &self,
        request: plugin::ReceiveRequest,
        rows: &'static [Row],
    ) -> Result<Response<Deliveries>, Status> {
        let registration = self.admitted()?;
        let heard: Vec<&'static Row> = rows
            .iter()
            .filter(|row| registration.grants.may_subscribe(row.topic))
            .collect();
        let chosen: Vec<&'static Row> = if request.rows.is_empty() {
            heard
        } else {
            let mut chosen = Vec::new();
            for name in &request.rows {
                match heard.iter().find(|row| row.name == name) {
                    Some(row) => chosen.push(*row),
                    None => {
                        let refusal = format!("{name} is not a row this plugin's roles hear");
                        self.note_refusal(&refusal);
                        return Err(Status::permission_denied(refusal));
                    }
                }
            }
            chosen
        };

        let waiting = Arc::new(Waiting::default());
        let (open, closed) = watch::channel(());
        for row in chosen {
            let subscription = self.bus.subscribe(row.topic);
            tokio::spawn(forward(
                row,
                subscription,
                Arc::clone(&waiting),
                self.configuration.clone(),
                self.instance_id().to_string(),
                closed.clone(),
            ));
        }

        // The stream: whatever waits, in order, until the plugin stops
        // listening, which ends every forwarder with it.
        let (sending, receiving) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            let _open = open;
            loop {
                while let Some(item) = waiting.pop() {
                    if sending.send(Ok(item)).await.is_err() {
                        return;
                    }
                }
                tokio::select! {
                    _ = waiting.ready.notified() => {}
                    _ = sending.closed() => return,
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(receiving),
        )))
    }
}

/// One row's subscription, read, filtered by the read scope and queued for
/// the plugin, its losses said; until the stream ends or the bus does.
async fn forward(
    row: &'static Row,
    mut subscription: meridian_bus::Subscription,
    waiting: Arc<Waiting>,
    configuration: Configuration,
    instance: String,
    mut closed: watch::Receiver<()>,
) {
    let drops = subscription.drop_count();
    let mut seen = drops.get();
    let mut every = tokio::time::interval(DROPS_READ_EVERY);
    loop {
        let heard = tokio::select! {
            heard = subscription.recv() => match heard {
                Some(heard) => Some(heard),
                None => return,
            },
            _ = every.tick() => None,
            _ = closed.changed() => return,
        };
        // Before what arrives after it: the drop happened first.
        let now = drops.get();
        if now > seen {
            tracing::warn!(
                row = row.name,
                dropped = now - seen,
                "the bus dropped deliveries for the plugin's subscription"
            );
            seen = now;
            waiting.lose(None, None);
        }
        if let Some(heard) = heard {
            deliver(row, heard, &waiting, &configuration, &instance).await;
        }
    }
}

/// One message heard: read, held to the read scope, and queued.
async fn deliver(
    row: &'static Row,
    heard: Heard,
    waiting: &Waiting,
    configuration: &Configuration,
    instance: &str,
) {
    let envelope = heard.envelope;
    if envelope.payload_type != row.payload_type {
        tracing::warn!(
            row = row.name,
            step = row.step,
            payload_type = envelope.payload_type,
            "a delivery under another type than its row's; not delivered"
        );
        waiting.lose(Some(1), Some(row.name));
        return;
    }
    let read = match (row.read)(&envelope.payload) {
        Ok(read) => read,
        Err(failed) => {
            tracing::warn!(row = row.name, step = row.step, %failed, "a delivery did not read");
            waiting.lose(Some(1), Some(row.name));
            return;
        }
    };
    if let Some(account) = &read.account {
        match configuration.current(configuration.now_ns()).await {
            Ok(held) if held.read_account_ids.contains(account) => {}
            // Outside the scope: filtered, which is not a loss.
            Ok(_) => return,
            // Unknown whether it is in the scope: not delivered, and said.
            Err(failed) => {
                tracing::warn!(row = row.name, %failed, "the read scope could not be read");
                waiting.lose(Some(1), Some(row.name));
                return;
            }
        }
    }
    let meta = envelope.meta.unwrap_or_default();
    let own = read
        .cause
        .as_ref()
        .is_some_and(|cause| cause.instance_id == instance);
    waiting.push(
        plugin::Delivery {
            meta: Some(plugin::DeliveryMeta {
                message_id: meta.message_id,
                correlation_id: meta.correlation_id,
                causation_id: meta.causation_id,
                published_at_ns: meta.published_at_ns,
                row: row.name.to_string(),
                journal: read.journal,
                cause: read.cause,
                own,
            }),
            item: Some(read.item),
        },
        row.name,
    );
}

#[cfg(test)]
mod tests;
