//! What a bus backend has to provide.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use meridian_domain::v1::Envelope;
use tokio::sync::{mpsc, Notify};

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

    /// A question past what the broker carries, refused before it was sent
    /// rather than lost on the way.
    #[error("a question to `{topic}` is {size} bytes, past the broker's limit of {most} bytes, and was not sent")]
    TooLarge {
        topic: String,
        size: usize,
        most: usize,
    },
}

/// How a component's refusal carries its reason code across the bus.
///
/// A handler answers with words, and where a plugin must tell its refusal
/// apart from every other, a code from the typed-operations refusal catalogue
/// beside them (meridian.v1.RefusalReason; spec/typed-sidecar-operations,
/// section 7). The code rides at the front of the handler's words, in both
/// the in-process path and the broker's refusal header, so neither path needs
/// a second channel; the sidecar takes it off and sends it beside the
/// status, and the words alone to the plugin (contract v8, open point 13 of
/// sdk-contract/the-book-holds-positions). A refusal with no code is words
/// alone, as every refusal was before.
///
/// Where the refusal names what the command left out (contract v9's
/// REFUSAL_REASON_INCOMPLETE), the fields ride with the code, each by its
/// path in the command, separated by `;`: `[refusal-reason 11
/// fields=positions[0].lots;sources] words`. A path holds no space and no
/// `;`, so neither the code's end nor a field's is ever mistaken.
const REFUSAL_REASON_PREFIX: &str = "[refusal-reason ";
const REFUSAL_FIELDS: &str = "fields=";

/// A handler's refusal, carrying `reason` -- a meridian.v1.RefusalReason's
/// number, never 0 -- beside its words.
pub fn refusal(reason: i32, words: impl std::fmt::Display) -> String {
    format!("{REFUSAL_REASON_PREFIX}{reason}] {words}")
}

/// A handler's refusal naming each field the command left out, beside its
/// code and its words.
pub fn refusal_naming(reason: i32, fields: &[String], words: impl std::fmt::Display) -> String {
    if fields.is_empty() {
        return refusal(reason, words);
    }
    let named = fields.join(";");
    format!("{REFUSAL_REASON_PREFIX}{reason} {REFUSAL_FIELDS}{named}] {words}")
}

/// The code, the fields and the words, where the detail carries a code.
fn refusal_parts(detail: &str) -> Option<(i32, Vec<&str>, &str)> {
    let rest = detail.strip_prefix(REFUSAL_REASON_PREFIX)?;
    let (head, words) = rest.split_once("] ")?;
    let (number, fields) = match head.split_once(' ') {
        Some((number, named)) => (
            number,
            named
                .strip_prefix(REFUSAL_FIELDS)?
                .split(';')
                .filter(|field| !field.is_empty())
                .collect(),
        ),
        None => (head, Vec::new()),
    };
    let reason: i32 = number.parse().ok()?;
    (reason > 0).then_some((reason, fields, words))
}

/// A refusal's reason code and its words, where it carries one.
pub fn read_refusal(detail: &str) -> Option<(i32, &str)> {
    refusal_parts(detail).map(|(reason, _, words)| (reason, words))
}

/// The fields a refusal names as left out; none where it names none.
pub fn refusal_fields(detail: &str) -> Vec<String> {
    refusal_parts(detail)
        .map(|(_, fields, _)| fields.into_iter().map(str::to_string).collect())
        .unwrap_or_default()
}

/// A component's refusal that carries no reason code but a status of its own
/// (contract v22): a command a component reads as malformed, naming each
/// field (`INVALID_ARGUMENT`), or one whose record's account is outside the
/// write scope the sidecar stamped (`PERMISSION_DENIED`, W4.9; the
/// order-path plan's protos, point 8). The sidecar sends the status beside
/// a refusal with no code and the fields named, as it sends its own.
/// Carried at the front of the words, as a reason code is:
/// `[refusal-status invalid-argument fields=quantity;cash_quantity] words`.
const REFUSAL_STATUS_PREFIX: &str = "[refusal-status ";

/// The status a [`refusal_with_status`] asks the sidecar to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalStatus {
    InvalidArgument,
    PermissionDenied,
}

impl RefusalStatus {
    fn word(self) -> &'static str {
        match self {
            RefusalStatus::InvalidArgument => "invalid-argument",
            RefusalStatus::PermissionDenied => "permission-denied",
        }
    }
}

/// A handler's refusal with a status and no reason code, naming each field
/// the command got wrong, where it names any.
pub fn refusal_with_status(
    status: RefusalStatus,
    fields: &[String],
    words: impl std::fmt::Display,
) -> String {
    if fields.is_empty() {
        return format!("{REFUSAL_STATUS_PREFIX}{}] {words}", status.word());
    }
    format!(
        "{REFUSAL_STATUS_PREFIX}{} {REFUSAL_FIELDS}{}] {words}",
        status.word(),
        fields.join(";")
    )
}

/// The status, the fields and the words, where the detail carries a status.
pub fn read_refusal_status(detail: &str) -> Option<(RefusalStatus, Vec<String>, &str)> {
    let rest = detail.strip_prefix(REFUSAL_STATUS_PREFIX)?;
    let (head, words) = rest.split_once("] ")?;
    let (word, fields) = match head.split_once(' ') {
        Some((word, named)) => (
            word,
            named
                .strip_prefix(REFUSAL_FIELDS)?
                .split(';')
                .filter(|field| !field.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        None => (head, Vec::new()),
    };
    let status = match word {
        "invalid-argument" => RefusalStatus::InvalidArgument,
        "permission-denied" => RefusalStatus::PermissionDenied,
        _ => return None,
    };
    Some((status, fields, words))
}

#[cfg(test)]
mod refusal_status_tests {
    use super::*;

    #[test]
    fn a_status_and_its_fields_read_back_and_are_never_a_reason_code() {
        let said = refusal_with_status(
            RefusalStatus::InvalidArgument,
            &["quantity".into(), "cash_quantity".into()],
            "an order states exactly one",
        );
        let (status, fields, words) = read_refusal_status(&said).unwrap();
        assert_eq!(status, RefusalStatus::InvalidArgument);
        assert_eq!(fields, vec!["quantity", "cash_quantity"]);
        assert_eq!(words, "an order states exactly one");
        assert!(read_refusal(&said).is_none());
        let denied = refusal_with_status(RefusalStatus::PermissionDenied, &[], "outside");
        assert_eq!(
            read_refusal_status(&denied).map(|(status, fields, _)| (status, fields.len())),
            Some((RefusalStatus::PermissionDenied, 0))
        );
        assert!(read_refusal_status("[refusal-reason 21] words").is_none());
    }
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

/// How many messages one subscription has lost.
///
/// A handle rather than a number, so whoever forwards a subscription's
/// deliveries -- a sidecar handing them to its plugin -- can keep reading the
/// count after the subscription itself has become a stream. A count that has
/// risen since it was last read is a loss the subscriber did not see happen,
/// and the only way it can find out.
#[derive(Debug, Clone, Default)]
pub struct DropCount(Arc<AtomicU64>);

impl DropCount {
    /// Messages lost so far.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn add(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A live subscription. Dropping it unsubscribes.
pub struct Subscription {
    pub(crate) rx: mpsc::Receiver<Delivery>,
    pub(crate) pattern: String,
    pub(crate) dropped: DropCount,
}

impl Subscription {
    pub(crate) fn new(rx: mpsc::Receiver<Delivery>, pattern: &str, dropped: DropCount) -> Self {
        Self {
            rx,
            pattern: pattern.to_string(),
            dropped,
        }
    }

    /// The next message, or `None` once the bus has shut down.
    pub async fn recv(&mut self) -> Option<Delivery> {
        self.rx.recv().await
    }

    /// The next delivery already queued, without waiting; None when nothing
    /// is.
    pub fn try_recv(&mut self) -> Option<Delivery> {
        self.rx.try_recv().ok()
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Messages this subscription has lost: its queue was full when they
    /// arrived, or they were not envelopes. Every backend counts the same way.
    pub fn dropped(&self) -> u64 {
        self.dropped.get()
    }

    /// The same count, as a handle that outlives [`Subscription::into_stream`].
    pub fn drop_count(&self) -> DropCount {
        self.dropped.clone()
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
            .field("dropped", &self.dropped.get())
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

    /// Subscribe to a topic pattern, every subscriber hearing every message
    /// (contract v25: "every subscriber"). See [`crate::topic`] for the
    /// grammar.
    fn subscribe(&self, pattern: &str) -> Subscription;

    /// Subscribe as one of a group (contract v25; Q15; kernel/the-bus-has-no-
    /// queue-groups): each message reaches one member of `group` -- one copy
    /// of a component sharing the work with its others -- and every other
    /// group and plain subscriber as usual. At most once, as everything on
    /// this bus: a member that dies mid-work loses that message.
    fn subscribe_shared(&self, pattern: &str, group: &str) -> Subscription;

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
    ///
    /// From contract v25 every copy of a component offering a topic shares
    /// it as one `group` (the component's name), so each request is answered
    /// once, not once per copy with all but the first answer thrown away.
    fn serve(&self, _topic: &str, _group: &str, _handler: Handler) {}

    /// The same, and signal once an answer has actually reached the broker.
    ///
    /// For a component that replies and then stops. Returning from a handler
    /// says the answer was composed, not that it was sent, and a process that
    /// exits on the strength of the first takes the answer with it.
    ///
    /// Defaulted to permitting immediately, because a backend with no wire
    /// cannot hold an answer in flight: the router answered from its own map
    /// before this was ever reached, so there is nothing left to wait for and
    /// a caller that waited would wait forever.
    fn serve_delivered(&self, topic: &str, group: &str, handler: Handler, delivered: Arc<Notify>) {
        self.serve(topic, group, handler);
        delivered.notify_one();
    }

    /// Messages dropped across every subscription on this backend: a
    /// subscriber's queue was full, or what arrived was not an envelope. Each
    /// subscription's own share is [`Subscription::dropped`].
    ///
    /// Exposed because at-most-once delivery is only defensible if the losses
    /// are observable. A silent drop is indistinguishable from a message that
    /// was never sent.
    fn dropped(&self) -> u64;
}

#[cfg(test)]
mod refusal_tests {
    use super::*;

    #[test]
    fn a_refusal_carries_its_code_beside_its_words() {
        let said = refusal(4, "ACC-1 has an opening balance standing");
        assert_eq!(
            read_refusal(&said),
            Some((4, "ACC-1 has an opening balance standing"))
        );
    }

    #[test]
    fn a_refusal_names_the_fields_it_left_out() {
        let fields = vec!["positions[0].lots".to_string(), "sources".to_string()];
        let said = refusal_naming(11, &fields, "the opening balance is incomplete");
        assert_eq!(
            read_refusal(&said),
            Some((11, "the opening balance is incomplete"))
        );
        assert_eq!(refusal_fields(&said), fields);
        // A refusal naming none reads as v8's did.
        assert_eq!(refusal_naming(4, &[], "standing"), refusal(4, "standing"));
        assert!(refusal_fields(&refusal(4, "standing")).is_empty());
        assert!(refusal_fields("no statement STMT-1").is_empty());
    }

    #[test]
    fn words_alone_carry_no_code() {
        assert_eq!(read_refusal("no statement STMT-1"), None);
        // Nor does a code of 0, the unspecified reason, or one not a number.
        assert_eq!(read_refusal("[refusal-reason 0] nothing"), None);
        assert_eq!(read_refusal("[refusal-reason four] nothing"), None);
    }
}
