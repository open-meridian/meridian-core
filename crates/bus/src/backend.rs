//! What a bus backend has to provide.

use meridian_pb::v1::Envelope;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("topic `{0}` is not publishable: it is empty, malformed, or a pattern")]
    NotPublishable(String),

    #[error("no handler is serving `{0}`")]
    NoHandler(String),

    #[error("call to `{topic}` timed out after {timeout_ms}ms")]
    Timeout { topic: String, timeout_ms: u64 },

    #[error("handler for `{topic}` failed: {detail}")]
    HandlerFailed { topic: String, detail: String },

    #[error("the bus is shutting down")]
    ShuttingDown,
}

/// One message delivered to a subscriber.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub envelope: Envelope,

    /// Position of this message in its topic's sequence, starting at 1.
    ///
    /// Per topic rather than global: a global counter would make every publish
    /// contend on one atomic, and no consumer has a use for a total order
    /// across unrelated topics.
    pub sequence: u64,
}

/// A live subscription. Dropping it unsubscribes.
pub struct Subscription {
    pub(crate) rx: mpsc::Receiver<Delivery>,
    pub(crate) pattern: String,
}

impl Subscription {
    /// The next message, or `None` once the bus has shut down.
    pub async fn recv(&mut self) -> Option<Delivery> {
        self.rx.recv().await
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("pattern", &self.pattern)
            .finish()
    }
}

/// A transport the bus can route onto.
///
/// Deliberately narrow. Everything above this trait -- routing rules,
/// correlation, request-reply -- is backend-independent, so a second backend
/// implements transport and inherits the rest.
pub trait Backend: Send + Sync {
    /// Deliver to every matching subscriber. Returns the topic sequence.
    ///
    /// Never blocks on a slow subscriber: a full queue drops the message for
    /// that subscriber alone.
    fn publish(&self, topic: &str, envelope: Envelope) -> Result<u64, BusError>;

    /// Subscribe to a topic pattern. See [`crate::topic`] for the grammar.
    fn subscribe(&self, pattern: &str) -> Subscription;

    /// Messages dropped because a subscriber's queue was full.
    ///
    /// Exposed because at-most-once delivery is only defensible if the losses
    /// are observable. A silent drop is indistinguishable from a message that
    /// was never sent.
    fn dropped(&self) -> u64;
}
