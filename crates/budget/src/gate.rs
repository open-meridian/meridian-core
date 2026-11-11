//! Fair shares: work queued per caller and served in turn (section 10a).
//!
//! Every component that answers more than one caller queues work per
//! calling instance or person and serves the queues in turn -- weighted fair
//! queuing with every caller's weight one, which is round robin over the
//! callers waiting -- within decisions/024's traffic classes: order-path
//! commands and venue facts are served first. So a flood fills only the
//! flooder's queue: its own requests wait behind each other, and every other
//! caller's next request is served after at most one of its.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::Overloaded;

/// decisions/024's traffic classes, in the order they are served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Order-path commands and venue facts: served first, and a venue fact
    /// refused for overload is sent again by its sender, never dropped.
    First,
    /// Everything else.
    Rest,
}

impl Class {
    /// The class a request on `topic` is in: the book's commands are the
    /// order path's and the venue's facts (decisions/024).
    pub fn of_topic(topic: &str) -> Self {
        let mut parts = topic.split('.');
        let (domain, kind) = (parts.nth(1), parts.next());
        if domain == Some("book") && kind == Some("command") {
            Class::First
        } else {
            Class::Rest
        }
    }

    fn index(self) -> usize {
        match self {
            Class::First => 0,
            Class::Rest => 1,
        }
    }
}

/// How many requests one caller may have waiting, per turn the component
/// gives at once: a caller with more is refused at once rather than queued.
const WAITING_PER_CALLER_PER_TURN: usize = 4;
/// How many requests may wait in all, per turn: past it, every caller is
/// refused at once.
const WAITING_PER_TURN: usize = 32;

/// A component's turns: at most `permits` requests answered at once, each
/// turn handed to the callers waiting in turn.
#[derive(Clone)]
pub struct Gate {
    inner: Arc<Inner>,
}

struct Inner {
    permits: usize,
    state: Mutex<State>,
    /// How long a turn took, smoothed, in nanoseconds: what a retry-after is
    /// estimated from.
    served_ns: AtomicU64,
    /// Requests refused since the process started.
    refused: AtomicU64,
}

#[derive(Default)]
struct State {
    busy: usize,
    waiting: usize,
    classes: [Fair; 2],
}

#[derive(Default)]
struct Fair {
    /// Callers with a request waiting, in the order their turn comes.
    turn: VecDeque<String>,
    by_caller: HashMap<String, VecDeque<oneshot::Sender<Turn>>>,
}

impl Fair {
    fn next(&mut self) -> Option<oneshot::Sender<Turn>> {
        let caller = self.turn.pop_front()?;
        let queue = self.by_caller.get_mut(&caller)?;
        let next = queue.pop_front();
        if queue.is_empty() {
            self.by_caller.remove(&caller);
        } else {
            self.turn.push_back(caller);
        }
        next
    }
}

/// One request's turn, given back when it is dropped and handed to the
/// next caller waiting.
pub struct Turn {
    inner: Option<Arc<Inner>>,
    started: Instant,
}

impl Gate {
    pub fn new(permits: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                permits: permits.max(1),
                state: Mutex::new(State::default()),
                served_ns: AtomicU64::new(50_000_000),
                refused: AtomicU64::new(0),
            }),
        }
    }

    /// Requests refused since the process started.
    pub fn refused(&self) -> u64 {
        self.inner.refused.load(Ordering::Relaxed)
    }

    pub fn permits(&self) -> usize {
        self.inner.permits
    }

    /// How many requests are waiting now, every class and caller together.
    pub fn waiting(&self) -> usize {
        self.inner.lock().waiting
    }

    /// How many turns are taken now.
    pub fn busy(&self) -> usize {
        self.inner.lock().busy
    }

    /// `caller`'s turn for a request of `class`: at once where a turn is free
    /// and nobody waits, otherwise in its turn among the callers waiting,
    /// within `wait`; refused with a retry-after where it does not come, or
    /// at once where the caller already has its share waiting.
    pub async fn admit(
        &self,
        class: Class,
        caller: &str,
        wait: Duration,
    ) -> Result<Turn, Overloaded> {
        let receiver = {
            let mut state = self.inner.lock();
            // A turn is freed only once nobody waits (release), so a free
            // one means none waits: taken at once.
            if state.busy < self.inner.permits {
                state.busy += 1;
                return Ok(self.inner.turn());
            }
            let per_caller = WAITING_PER_CALLER_PER_TURN * self.inner.permits;
            let most = WAITING_PER_TURN * self.inner.permits;
            let fair = &mut state.classes[class.index()];
            let mine = fair.by_caller.get(caller).map_or(0, VecDeque::len);
            if mine >= per_caller {
                self.inner.refused.fetch_add(1, Ordering::Relaxed);
                let seconds = self.inner.retry_after(mine);
                return Err(Overloaded::new(
                    seconds,
                    format!(
                        "overloaded: {caller} already has {mine} request(s) waiting, its share"
                    ),
                ));
            }
            if state.waiting >= most {
                self.inner.refused.fetch_add(1, Ordering::Relaxed);
                let seconds = self.inner.retry_after(state.waiting);
                return Err(Overloaded::new(
                    seconds,
                    format!(
                        "overloaded: {} request(s) are waiting, the most",
                        state.waiting
                    ),
                ));
            }
            let (sender, receiver) = oneshot::channel();
            let fair = &mut state.classes[class.index()];
            if !fair.by_caller.contains_key(caller) {
                fair.turn.push_back(caller.to_string());
            }
            fair.by_caller
                .entry(caller.to_string())
                .or_default()
                .push_back(sender);
            state.waiting += 1;
            receiver
        };
        match tokio::time::timeout(wait, receiver).await {
            Ok(Ok(turn)) => Ok(turn),
            // Timed out: the request left waiting is skipped when its turn
            // comes; a turn handed to it as it gave up returns itself.
            _ => {
                self.inner.refused.fetch_add(1, Ordering::Relaxed);
                let waiting = self.inner.lock().waiting;
                Err(Overloaded::new(
                    self.inner.retry_after(waiting),
                    format!(
                        "overloaded: {caller} waited {} ms for a turn among {waiting} request(s) waiting",
                        wait.as_millis()
                    ),
                ))
            }
        }
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn turn(self: &Arc<Self>) -> Turn {
        Turn {
            inner: Some(Arc::clone(self)),
            started: Instant::now(),
        }
    }

    /// About how long `waiting` requests take to be served by its turns.
    fn retry_after(&self, waiting: usize) -> u32 {
        let served = self.served_ns.load(Ordering::Relaxed) as f64 / 1e9;
        ((waiting + 1) as f64 * served / self.permits as f64).ceil() as u32
    }

    /// A turn given back: handed to the next caller waiting, first class
    /// first, or freed.
    fn release(self: &Arc<Self>) {
        loop {
            let next = {
                let mut state = self.lock();
                let next = state.classes.iter_mut().find_map(Fair::next);
                match next {
                    Some(sender) => {
                        state.waiting -= 1;
                        sender
                    }
                    None => {
                        state.busy -= 1;
                        return;
                    }
                }
            };
            // The turn stays taken, handed over. A request that gave up
            // waiting is skipped: its turn comes back, disarmed, and goes to
            // the next.
            match next.send(self.turn()) {
                Ok(()) => return,
                Err(mut unclaimed) => unclaimed.disarm(),
            }
        }
    }
}

impl Turn {
    fn disarm(&mut self) {
        self.inner = None;
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let took = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let before = inner.served_ns.load(Ordering::Relaxed);
        inner
            .served_ns
            .store(before - before / 8 + took / 8, Ordering::Relaxed);
        inner.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_book_s_commands_are_the_first_class_and_everything_else_the_rest() {
        assert_eq!(
            Class::of_topic("platform.book.command.report-execution"),
            Class::First
        );
        assert_eq!(
            Class::of_topic("platform.book.command.create-placement"),
            Class::First
        );
        assert_eq!(
            Class::of_topic("platform.book.query.list-orders"),
            Class::Rest
        );
        assert_eq!(
            Class::of_topic("platform.lake.query.list-statistics"),
            Class::Rest
        );
    }

    /// One flooder with many waiting and a quiet caller with one: the quiet
    /// caller's request is served after at most one of the flooder's.
    #[tokio::test]
    async fn a_flood_fills_only_the_flooders_queue_and_the_others_are_served_in_turn() {
        let gate = Gate::new(1);
        let held = gate
            .admit(Class::Rest, "flooder", Duration::ZERO)
            .await
            .unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for n in 0..3 {
            let (gate, order) = (gate.clone(), Arc::clone(&order));
            waiting.push(tokio::spawn(async move {
                let turn = gate
                    .admit(Class::Rest, "flooder", Duration::from_secs(5))
                    .await
                    .unwrap();
                order.lock().unwrap().push(format!("flooder-{n}"));
                drop(turn);
            }));
            tokio::task::yield_now().await;
        }
        let (quiet_gate, quiet_order) = (gate.clone(), Arc::clone(&order));
        waiting.push(tokio::spawn(async move {
            let turn = quiet_gate
                .admit(Class::Rest, "quiet", Duration::from_secs(5))
                .await
                .unwrap();
            quiet_order.lock().unwrap().push("quiet".into());
            drop(turn);
        }));
        tokio::task::yield_now().await;
        assert_eq!(gate.waiting(), 4);
        drop(held);
        for task in waiting {
            task.await.unwrap();
        }
        let order = order.lock().unwrap().clone();
        let quiet = order.iter().position(|said| said == "quiet").unwrap();
        assert!(
            quiet <= 1,
            "the quiet caller waited behind {quiet} of the flooder's: {order:?}"
        );
        assert_eq!((gate.busy(), gate.waiting()), (0, 0));
    }

    #[tokio::test]
    async fn order_path_commands_are_served_before_the_rest() {
        let gate = Gate::new(1);
        let held = gate
            .admit(Class::Rest, "reader", Duration::ZERO)
            .await
            .unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for (class, name) in [(Class::Rest, "read"), (Class::First, "fill")] {
            let (gate, order) = (gate.clone(), Arc::clone(&order));
            waiting.push(tokio::spawn(async move {
                let turn = gate
                    .admit(class, name, Duration::from_secs(5))
                    .await
                    .unwrap();
                order.lock().unwrap().push(name);
                drop(turn);
            }));
            tokio::task::yield_now().await;
        }
        drop(held);
        for task in waiting {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["fill", "read"]);
    }

    #[tokio::test]
    async fn past_its_share_a_caller_is_refused_at_once_and_a_wait_that_ends_is_refused_with_a_retry_after(
    ) {
        let gate = Gate::new(1);
        let held = gate
            .admit(Class::Rest, "flooder", Duration::ZERO)
            .await
            .unwrap();
        let mut waiting = Vec::new();
        for _ in 0..WAITING_PER_CALLER_PER_TURN {
            let gate = gate.clone();
            waiting.push(tokio::spawn(async move {
                gate.admit(Class::Rest, "flooder", Duration::from_millis(200))
                    .await
                    .map(drop)
            }));
            tokio::task::yield_now().await;
        }
        let refused = gate
            .admit(Class::Rest, "flooder", Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert!(refused.words.contains("its share"), "{refused}");
        // Another caller still waits its turn, and is refused when its wait ends.
        let ended = gate
            .admit(Class::Rest, "other", Duration::from_millis(50))
            .await
            .err()
            .unwrap();
        assert!(
            ended.retry_after_seconds >= 1 && ended.words.contains("waited"),
            "{ended}"
        );
        for task in waiting {
            assert!(task.await.unwrap().is_err());
        }
        // Every refusal counted, for the component's report.
        assert_eq!(gate.refused(), 2 + WAITING_PER_CALLER_PER_TURN as u64);
        drop(held);
        // The requests that gave up are skipped; the turn is free again.
        assert_eq!(gate.busy(), 0);
        assert!(gate
            .admit(Class::Rest, "other", Duration::ZERO)
            .await
            .is_ok());
    }
}
