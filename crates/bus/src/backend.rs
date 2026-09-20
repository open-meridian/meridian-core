//! What a bus backend has to provide.

use std::time::Duration;

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

    /// Consume this subscription as a stream.
    ///
    /// Exists so a transport can hand deliveries straight to its client
    /// without reaching into the channel, which would make the channel type
    /// part of this crate's public surface.
    pub fn into_stream(self) -> tokio_stream::wrappers::ReceiverStream<Delivery> {
        tokio_stream::wrappers::ReceiverStream::new(self.rx)
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("pattern", &self.pattern)
            .finish()
    }
}

/// What a handler answers a call with: a payload type and bytes, or why not.
pub type HandlerReply = std::result::Result<(String, Vec<u8>), String>;

/// A registered answer to one topic.
pub type Handler = std::sync::Arc<dyn Fn(Envelope) -> HandlerReply + Send + Sync>;

/// A future a backend returns, boxed so the trait stays object-safe.
///
/// Only the call path pays for this. `publish` and `subscribe` stay
/// synchronous, which is the whole reason this trait is not async.
pub type Answer<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Envelope, BusError>> + Send + 'a>>;

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

    /// Ask a question of whoever serves this topic, wherever they are.
    ///
    /// Defaulted to "nobody serves this", so a backend that carries only
    /// publish and subscribe is still a backend, and the router's in-process
    /// path is unchanged.
    ///
    /// The distinction between nobody serving and nobody answering in time is
    /// the caller's whole diagnosis, so a backend must keep the two apart
    /// rather than reporting whichever its client reports.
    fn request<'a>(
        &'a self,
        topic: &'a str,
        _envelope: Envelope,
        _timeout: Duration,
    ) -> Answer<'a> {
        Box::pin(async move { Err(BusError::NoHandler(topic.to_string())) })
    }

    /// Offer this topic's answer to callers in other processes.
    ///
    /// Defaulted to nothing, because an in-process backend has nowhere else to
    /// offer it to: the router already holds the handler and answers locally.
    fn serve(&self, _topic: &str, _handler: Handler) {}

    /// Messages dropped because a subscriber's queue was full.
    ///
    /// Exposed because at-most-once delivery is only defensible if the losses
    /// are observable. A silent drop is indistinguishable from a message that
    /// was never sent.
    fn dropped(&self) -> u64;
}
