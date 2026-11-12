//! Group commit (step 3d; spec/a-deployment-scales-out-and-fails-over-by-
//! configuration section 6.5): one transaction in flight per partition, and
//! the commands arriving meanwhile join the next, up to 512 KiB of them.
//!
//! **No linger.** A command arriving at an idle partition is committed at
//! once, by its own caller, alone: light load sees no added latency (Kafka's
//! `linger.ms` 0; Postgres's own group commit). Only while a transaction is
//! in flight do commands queue, and the next transaction takes them all.
//!
//! **The leader.** The caller whose command finds no transaction in flight
//! leads: it takes the queue's front -- its own command first -- runs one
//! transaction for the batch, answers each command's caller, and, where
//! commands are waiting, hands the lead to the first of them, whose own
//! command is then the next batch's first. Nobody waits for a linger, and a
//! leader never commits more than the batch its own command was in.
//!
//! **Each command decided by its own caller.** A decision is a closure over
//! its caller's stack, so the leader never calls another caller's: it hands
//! that caller the book it replayed in the batch's transaction and the
//! partition's head, the caller decides on its own thread, asking the leader
//! for any entry's body the decision names, and hands the decision back.
//! The leader decides its own command itself.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use crate::book::Book;
use crate::journal::Body;
use crate::store::{with_bodies, Acted, Decide, Decided, Result, StoreError};

/// The most request bytes one batch takes (the change log's page, 512 KiB):
/// a command larger than it is a batch of its own.
pub const BATCH_MOST_BYTES: usize = meridian_changes::PAGE_MOST;

/// One command as it was asked of the store.
#[derive(Debug, Clone)]
pub struct Command {
    pub account_id: String,
    pub message_id: String,
    pub idempotency_key: String,
    pub request: Vec<u8>,
}

/// What the leader asks a waiting caller.
enum Asked {
    /// Decide your command against this book at this head.
    Decide { book: Box<Book>, head: u64 },
    /// The bodies your decision named.
    Bodies(Result<Vec<(String, Body)>>),
    /// Lead the next batch: your command is its first.
    Lead,
    /// Your command's outcome.
    Done(Result<Acted>),
}

/// What a waiting caller answers the leader.
enum Answered {
    Bodies(Vec<String>),
    Decided(Box<Result<Decided>>),
}

/// A command waiting in its partition's lane, with its caller's line.
pub struct Pending {
    id: u64,
    pub command: Command,
    to: Sender<Asked>,
    from: Receiver<Answered>,
}

/// The leader's own decision, and which command is its own.
pub struct Decider<'a, 'b> {
    mine: u64,
    decide: &'a mut Decide<'b>,
}

impl Pending {
    /// Its command decided against `book` at `head`: by the leader itself
    /// where it is the leader's own, else by its caller. `load` reads the
    /// bodies a decision names, in the batch's transaction.
    pub fn decide(
        &self,
        decider: &mut Decider<'_, '_>,
        mut book: Book,
        head: u64,
        mut load: impl FnMut(&str, &[String]) -> Result<Vec<(String, Body)>>,
    ) -> Result<Decided> {
        if self.id == decider.mine {
            let decide = &mut decider.decide;
            return with_bodies(&mut book, load, |book| decide(book, head));
        }
        let account = book.account_id.clone();
        if self
            .to
            .send(Asked::Decide {
                book: Box::new(book),
                head,
            })
            .is_err()
        {
            return Err(gone());
        }
        loop {
            match self.from.recv() {
                Ok(Answered::Bodies(ids)) => {
                    if self.to.send(Asked::Bodies(load(&account, &ids))).is_err() {
                        return Err(gone());
                    }
                }
                Ok(Answered::Decided(decided)) => return *decided,
                Err(_) => return Err(gone()),
            }
        }
    }

    /// The bytes it counts for in a batch.
    fn size(&self) -> usize {
        self.command.request.len().max(1)
    }
}

fn gone() -> StoreError {
    StoreError::Unavailable("the command's caller stopped waiting for its batch".into())
}

/// A partition's lane: the commands waiting for the transaction in flight.
#[derive(Default)]
pub struct Lane {
    state: Mutex<LaneState>,
}

#[derive(Default)]
struct LaneState {
    queue: VecDeque<Pending>,
    /// A caller is leading a batch, or has been handed the lead.
    leading: bool,
    next: u64,
}

impl Lane {
    fn state(&self) -> std::sync::MutexGuard<'_, LaneState> {
        self.state.lock().expect("a partition's lane is poisoned")
    }

    /// How many commands wait in it now.
    pub fn waiting(&self) -> usize {
        self.state().queue.len()
    }

    /// The queue's front: at least one command, then each that fits in
    /// [`BATCH_MOST_BYTES`].
    fn take(&self) -> Vec<Pending> {
        let mut state = self.state();
        let mut batch = Vec::new();
        let mut bytes = 0;
        while let Some(next) = state.queue.front() {
            if !batch.is_empty() && bytes + next.size() > BATCH_MOST_BYTES {
                break;
            }
            bytes += next.size();
            batch.extend(state.queue.pop_front());
        }
        batch
    }

    /// The lead handed to the first command waiting, or given up where none
    /// waits. A waiting caller gone (its thread ended) is passed over.
    fn hand_on(&self) {
        let mut state = self.state();
        while let Some(first) = state.queue.front() {
            if first.to.send(Asked::Lead).is_ok() {
                return;
            }
            state.queue.pop_front();
        }
        state.leading = false;
    }
}

/// Hands the lead on however its leader leaves: a leader that panicked
/// leaves no caller waiting for a lead nobody holds.
struct Leading<'a>(&'a Lane);

impl Drop for Leading<'_> {
    fn drop(&mut self) {
        self.0.hand_on();
    }
}

/// `command` committed through `lane`: alone where no transaction is in
/// flight, else in the next batch. `run` runs one batch's transaction and
/// answers each of its commands' outcomes, in their order.
pub fn act<F>(lane: &Lane, command: Command, decide: &mut Decide<'_>, run: F) -> Result<Acted>
where
    F: Fn(&[Pending], &mut Decider<'_, '_>) -> Vec<Result<Acted>>,
{
    let (to_caller, asked) = channel();
    let (answer, from_caller) = channel();
    let (mine, lead) = {
        let mut state = lane.state();
        state.next += 1;
        let id = state.next;
        state.queue.push_back(Pending {
            id,
            command,
            to: to_caller,
            from: from_caller,
        });
        let lead = !state.leading;
        state.leading = true;
        (id, lead)
    };
    if lead {
        return led(lane, mine, decide, &run);
    }
    loop {
        match asked.recv() {
            Ok(Asked::Decide { mut book, head }) => {
                let decided = with_bodies(
                    &mut book,
                    |_, ids| {
                        answer
                            .send(Answered::Bodies(ids.to_vec()))
                            .map_err(|_| gone())?;
                        match asked.recv() {
                            Ok(Asked::Bodies(loaded)) => loaded,
                            _ => Err(gone()),
                        }
                    },
                    |book| decide(book, head),
                );
                if answer.send(Answered::Decided(Box::new(decided))).is_err() {
                    return Err(gone());
                }
            }
            Ok(Asked::Lead) => return led(lane, mine, decide, &run),
            Ok(Asked::Done(done)) => return done,
            Ok(Asked::Bodies(_)) => {
                return Err(StoreError::Unavailable(
                    "a batch's leader sent bodies nobody asked for".into(),
                ))
            }
            Err(_) => {
                return Err(StoreError::Unavailable(
                    "the batch this command waited in stopped before answering it".into(),
                ))
            }
        }
    }
}

/// Lead batches until this caller's own command is answered, then hand on.
fn led<F>(lane: &Lane, mine: u64, decide: &mut Decide<'_>, run: &F) -> Result<Acted>
where
    F: Fn(&[Pending], &mut Decider<'_, '_>) -> Vec<Result<Acted>>,
{
    let _leading = Leading(lane);
    let mut decider = Decider { mine, decide };
    loop {
        let batch = lane.take();
        if batch.is_empty() {
            return Err(StoreError::Unavailable(
                "a batch's leader found its own command gone from its lane".into(),
            ));
        }
        let outcomes = run(&batch, &mut decider);
        let mut own = None;
        for (pending, outcome) in batch.into_iter().zip(outcomes) {
            if pending.id == mine {
                own = Some(outcome);
            } else {
                let _ = pending.to.send(Asked::Done(outcome));
            }
        }
        if let Some(own) = own {
            return own;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn command(n: usize, bytes: usize) -> Command {
        Command {
            account_id: format!("ACC-{n}"),
            message_id: String::new(),
            idempotency_key: String::new(),
            request: vec![0; bytes],
        }
    }

    /// One command alone, its lane idle: led by its own caller, a batch of
    /// one, nobody waiting.
    #[test]
    fn a_command_at_an_idle_partition_is_its_own_batch() {
        let lane = Lane::default();
        let sizes = Mutex::new(Vec::new());
        let mut decide: Box<Decide<'_>> =
            Box::new(|_, _| Err(StoreError::Invalid("refused".into())));
        let acted = act(&lane, command(1, 10), &mut decide, |batch, _| {
            sizes.lock().unwrap().push(batch.len());
            batch
                .iter()
                .map(|_| Err(StoreError::Invalid("x".into())))
                .collect()
        });
        assert!(acted.is_err());
        assert_eq!(*sizes.lock().unwrap(), vec![1]);
        assert!(!lane.state().leading, "the lead is given up");
    }

    /// Callers arriving while a batch is in flight join the next one, each
    /// answered its own outcome; a batch never passes 512 KiB.
    #[test]
    fn commands_arriving_meanwhile_join_the_next_batch_up_to_its_bytes() {
        let lane = Arc::new(Lane::default());
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(std::sync::Barrier::new(2));
        let first = {
            let (lane, sizes, gate) = (Arc::clone(&lane), Arc::clone(&sizes), Arc::clone(&gate));
            std::thread::spawn(move || {
                let mut decide: Box<Decide<'_>> =
                    Box::new(|_, _| Err(StoreError::Invalid("unused".into())));
                act(&lane, command(0, 10), &mut decide, |batch, _| {
                    if batch.iter().any(|p| p.command.account_id == "ACC-0") {
                        // Held in flight until the others queue.
                        gate.wait();
                        while lane.waiting() < 6 {
                            std::thread::yield_now();
                        }
                    }
                    sizes.lock().unwrap().push(batch.len());
                    batch
                        .iter()
                        .map(|p| Err(StoreError::Invalid(p.command.account_id.clone())))
                        .collect()
                })
            })
        };
        gate.wait();
        let others: Vec<_> = (1..=6)
            .map(|n| {
                let (lane, sizes) = (Arc::clone(&lane), Arc::clone(&sizes));
                std::thread::spawn(move || {
                    let mut decide: Box<Decide<'_>> =
                        Box::new(|_, _| Err(StoreError::Invalid("unused".into())));
                    // Three of 200 KiB fit 512 KiB twice over two batches.
                    act(&lane, command(n, 200 * 1024), &mut decide, |batch, _| {
                        sizes.lock().unwrap().push(batch.len());
                        batch
                            .iter()
                            .map(|p| Err(StoreError::Invalid(p.command.account_id.clone())))
                            .collect()
                    })
                })
            })
            .collect();
        let said = |outcome: Result<Acted>| match outcome {
            Err(StoreError::Invalid(words)) => words,
            _ => panic!("an outcome not its own"),
        };
        assert_eq!(said(first.join().unwrap()), "ACC-0");
        let mut answered: Vec<String> = others
            .into_iter()
            .map(|other| said(other.join().unwrap()))
            .collect();
        answered.sort();
        assert_eq!(
            answered,
            (1..=6).map(|n| format!("ACC-{n}")).collect::<Vec<_>>(),
            "each caller is answered its own command's outcome"
        );
        let sizes = sizes.lock().unwrap().clone();
        assert_eq!(sizes[0], 1, "the first alone");
        assert_eq!(&sizes[1..], &[2, 2, 2], "then 2 of 200 KiB a batch");
        assert!(!lane.state().leading);
    }
}
