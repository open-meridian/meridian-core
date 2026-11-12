//! Group commit (step 3d; spec/a-deployment-scales-out-and-fails-over-by-
//! configuration section 6.5): one transaction in flight per partition, the
//! writes arriving meanwhile joining the next, up to 512 KiB of them.
//!
//! **No linger.** A write arriving at an idle partition is committed at once
//! by its own caller, alone, so light load sees no added latency (Kafka's
//! `linger.ms` 0; Postgres's own group commit). Only while a transaction is
//! in flight do writes queue, and the next transaction takes them all.
//!
//! **The leader.** The caller whose write finds no transaction in flight
//! leads: it takes the queue's front -- its own write first -- runs one
//! transaction for the batch through `run`, answers each caller, and where
//! writes are waiting hands the lead to the first of them, whose write is
//! then the next batch's first. A leader commits only the batch its own
//! write is in. The book's commands, each decided by its own caller, take
//! the book's own form of this (meridian-bor's `group_commit`).

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// The most bytes of writes one batch takes (the change log's page,
/// 512 KiB): a write larger than it is a batch of its own.
pub const BATCH_MOST_BYTES: usize = crate::PAGE_MOST;

/// Every partition's lane of one store.
pub struct Lanes<T, R> {
    lanes: Mutex<HashMap<String, Arc<Lane<T, R>>>>,
}

impl<T, R> Default for Lanes<T, R> {
    fn default() -> Self {
        Self {
            lanes: Mutex::new(HashMap::new()),
        }
    }
}

/// One partition's lane: the writes waiting for its transaction in flight.
pub struct Lane<T, R> {
    state: Mutex<State<T, R>>,
}

struct State<T, R> {
    queue: VecDeque<Waiting<T, R>>,
    /// A caller leads a batch, or has been handed the lead.
    leading: bool,
}

struct Waiting<T, R> {
    write: T,
    bytes: usize,
    to: Sender<Turn<R>>,
}

enum Turn<R> {
    Lead,
    Done(R),
}

impl<T, R> Default for Lane<T, R> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                leading: false,
            }),
        }
    }
}

impl<T, R> Lanes<T, R> {
    /// `partition`'s lane.
    pub fn lane(&self, partition: &str) -> Arc<Lane<T, R>> {
        Arc::clone(
            self.lanes
                .lock()
                .expect("lanes poisoned")
                .entry(partition.to_string())
                .or_default(),
        )
    }
}

impl<T, R> Lane<T, R> {
    fn state(&self) -> std::sync::MutexGuard<'_, State<T, R>> {
        self.state.lock().expect("a partition's lane is poisoned")
    }

    /// How many writes wait in it now.
    pub fn waiting(&self) -> usize {
        self.state().queue.len()
    }

    /// `write`, of `bytes`, committed through this lane: alone where no
    /// transaction is in flight, else in the next batch. `run` commits one
    /// batch, its writes in order, and answers each one's outcome in that
    /// order; `gone` is the outcome of a write whose batch stopped (its
    /// leader panicked) before answering it.
    pub fn commit(
        &self,
        write: T,
        bytes: usize,
        run: impl Fn(Vec<T>) -> Vec<R>,
        gone: impl Fn() -> R,
    ) -> R {
        let (to, turn) = channel();
        let lead = {
            let mut state = self.state();
            state.queue.push_back(Waiting {
                write,
                bytes: bytes.max(1),
                to,
            });
            let lead = !state.leading;
            state.leading = true;
            lead
        };
        if !lead {
            match turn.recv() {
                Ok(Turn::Done(outcome)) => return outcome,
                Ok(Turn::Lead) => {}
                Err(_) => return gone(),
            }
        }
        // Leading: the front is this caller's own write.
        let _leading = Leading(self);
        let batch = self.take();
        let mut senders = Vec::with_capacity(batch.len());
        let mut writes = Vec::with_capacity(batch.len());
        for waiting in batch {
            senders.push(waiting.to);
            writes.push(waiting.write);
        }
        let count = writes.len();
        let mut outcomes = run(writes).into_iter();
        let own = outcomes.next().unwrap_or_else(&gone);
        for to in senders.into_iter().skip(1) {
            let _ = to.send(Turn::Done(outcomes.next().unwrap_or_else(&gone)));
        }
        debug_assert!(count > 0);
        own
    }

    /// The queue's front: at least one write, then each that fits in
    /// [`BATCH_MOST_BYTES`].
    fn take(&self) -> Vec<Waiting<T, R>> {
        let mut state = self.state();
        let mut batch = Vec::new();
        let mut bytes = 0;
        while let Some(next) = state.queue.front() {
            if !batch.is_empty() && bytes + next.bytes > BATCH_MOST_BYTES {
                break;
            }
            bytes += next.bytes;
            batch.extend(state.queue.pop_front());
        }
        batch
    }

    /// The lead handed to the first write waiting, or given up where none
    /// waits. A caller gone is passed over.
    fn hand_on(&self) {
        let mut state = self.state();
        while let Some(first) = state.queue.front() {
            if first.to.send(Turn::Lead).is_ok() {
                return;
            }
            state.queue.pop_front();
        }
        state.leading = false;
    }
}

/// Hands the lead on however its leader leaves, a panic too.
struct Leading<'a, T, R>(&'a Lane<T, R>);

impl<T, R> Drop for Leading<'_, T, R> {
    fn drop(&mut self) {
        self.0.hand_on();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_at_an_idle_partition_is_its_own_batch() {
        let lane: Lane<u32, u32> = Lane::default();
        let sizes = Mutex::new(Vec::new());
        let said = lane.commit(
            7,
            10,
            |batch| {
                sizes.lock().unwrap().push(batch.len());
                batch.into_iter().map(|n| n * 2).collect()
            },
            || 0,
        );
        assert_eq!(said, 14);
        assert_eq!(*sizes.lock().unwrap(), vec![1]);
        assert!(!lane.state().leading);
    }

    #[test]
    fn writes_arriving_meanwhile_join_the_next_batch_up_to_its_bytes() {
        let lane: Arc<Lane<u32, u32>> = Arc::new(Lane::default());
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let run = {
            let (lane, sizes, gate) = (Arc::clone(&lane), Arc::clone(&sizes), Arc::clone(&gate));
            move |batch: Vec<u32>| {
                if batch.contains(&0) {
                    gate.wait();
                    while lane.waiting() < 6 {
                        std::thread::yield_now();
                    }
                }
                sizes.lock().unwrap().push(batch.len());
                batch.into_iter().map(|n| n + 100).collect::<Vec<u32>>()
            }
        };
        let first = {
            let (lane, run) = (Arc::clone(&lane), run.clone());
            std::thread::spawn(move || lane.commit(0, 10, run, || 0))
        };
        gate.wait();
        let others: Vec<_> = (1..=6)
            .map(|n| {
                let (lane, run) = (Arc::clone(&lane), run.clone());
                std::thread::spawn(move || lane.commit(n, 200 * 1024, run, || 0))
            })
            .collect();
        assert_eq!(first.join().unwrap(), 100);
        let mut said: Vec<u32> = others.into_iter().map(|o| o.join().unwrap()).collect();
        said.sort();
        assert_eq!(said, (101..=106).collect::<Vec<u32>>());
        let sizes = sizes.lock().unwrap().clone();
        assert_eq!(sizes, vec![1, 2, 2, 2], "two of 200 KiB fit 512 KiB");
        assert!(!lane.state().leading);
    }

    #[test]
    fn a_leader_that_panics_hands_the_lead_on() {
        let lane: Arc<Lane<u32, u32>> = Arc::new(Lane::default());
        let first = {
            let lane = Arc::clone(&lane);
            std::thread::spawn(move || {
                lane.commit(
                    1,
                    1,
                    |_| {
                        while lane.waiting() < 1 {
                            std::thread::yield_now();
                        }
                        panic!("the batch's transaction panicked")
                    },
                    || 0,
                )
            })
        };
        while !lane.state().leading {
            std::thread::yield_now();
        }
        let second = {
            let lane = Arc::clone(&lane);
            std::thread::spawn(move || lane.commit(2, 1, |b| b, || 0))
        };
        assert!(first.join().is_err());
        assert_eq!(second.join().unwrap(), 2, "the next write still commits");
    }
}
