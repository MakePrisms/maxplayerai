//! Keyed work units for `serve` (T7, field report 2026-10-12 §4b/§5).
//!
//! Before: serve ran every inbox message, hand-off request and recovery item one at a time, so
//! one slow mint call (a handed-off `receive`, a wallet recovery item, or another swap's step)
//! held the whole loop for up to its budget (120 s HTTP, 1040 s `nostr://`) and an unrelated
//! swap's claim or refund waited behind it.
//!
//! Now each piece of work is a unit that names the journal rows it may write as **keys**, and
//! serve polls every started unit concurrently **inside its one task** (no `tokio::spawn`: the
//! task-local claim scopes of [`crate::backoff::with_claim`], progress and admission output keep
//! working per unit). A unit starts only when none of its keys is held by a running unit, and
//! units sharing a key start in the order they were queued, so:
//!
//! - every row still has exactly one writer at a time (single writer per key; the process still
//!   holds `owner.lock` and no other process writes the home);
//! - each unit keeps its own journal-before-effect order, budgets, deadlines and ambiguous-outcome
//!   handling unchanged: it is the same code that ran serially before;
//! - work on different keys never waits on a slow mint it does not use;
//! - **urgent** units (a live swap's recovery, its retained or inbound steps: claim/refund
//!   bearing) may overtake units that are only *waiting* (quote requests, handed-off
//!   list/take/cancel). A waiting non-urgent unit would otherwise reserve its lot key while it
//!   waits for `admit`, chaining a slow mint on lot A into lot B's claim. An overtaken unit loses
//!   nothing: it re-reads the journal and re-runs its checks when it starts. Urgent units still
//!   never run beside a holder of their keys, keep FIFO among themselves, and are never
//!   overtaken by non-urgent ones.
//!
//! Keys (see `coordinator::*_keys`): `lot:<id>` (a maker lot: its listing row and every maker
//! swap on it), `taker` (this home's taker swaps: at most one is open), `admit` (anything that
//! selects wallet proofs or creates a swap or listing), `listings` (every listing row write that
//! is not a single known lot, including `list`), `wallet:<mint>` (one mint's funding,
//! withdrawal, receive and send journals),
//! `publications` (the durable publication retry pass). Units are deduplicated by id while
//! queued or running.
use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    pin::Pin,
};

use futures_util::stream::{FuturesUnordered, StreamExt};

/// Most units running at once. Active swaps are bounded far lower (4 maker, 1 taker); this only
/// caps a burst of listing or message units.
pub const MAX_RUNNING: usize = 32;

pub type UnitFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>>;
type Running<'a> = Pin<Box<dyn Future<Output = (String, Vec<String>, anyhow::Result<()>)> + 'a>>;

struct Queued<W> {
    id: String,
    keys: Vec<String>,
    work: W,
    urgent: bool,
}

/// Units waiting for their keys, and the running ones. Not `Send`: it lives in serve's task.
pub struct Scheduler<'a, W> {
    queue: VecDeque<Queued<W>>,
    running: FuturesUnordered<Running<'a>>,
    held: HashSet<String>,
    ids: HashSet<String>,
}
impl<W> Default for Scheduler<'_, W> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            running: FuturesUnordered::new(),
            held: HashSet::new(),
            ids: HashSet::new(),
        }
    }
}
impl<'a, W> Scheduler<'a, W> {
    /// Queue `work` under `id` with `keys`. False (and `work` returned) when a unit with this id
    /// is already queued or running: tick-generated work is never doubled.
    pub fn push(&mut self, id: String, keys: Vec<String>, work: W) -> Result<(), W> {
        self.push_as(id, keys, work, false)
    }
    /// [`push`](Self::push) for an urgent (claim/refund-bearing) unit: it may overtake waiting
    /// non-urgent units on a shared key, never a running holder.
    pub fn push_as(
        &mut self,
        id: String,
        keys: Vec<String>,
        work: W,
        urgent: bool,
    ) -> Result<(), W> {
        if !self.ids.insert(id.clone()) {
            return Err(work);
        }
        self.queue.push_back(Queued {
            id,
            keys,
            work,
            urgent,
        });
        Ok(())
    }
    /// Whether none of `keys` is held by a running unit or named by a queued one. Optional
    /// recovery work is queued only then, so it never takes a place in line ahead of claims.
    pub fn free(&self, keys: &[String]) -> bool {
        keys.iter()
            .all(|k| !self.held.contains(k) && !self.queue.iter().any(|q| q.keys.contains(k)))
    }
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }
    pub fn running(&self) -> usize {
        self.running.len()
    }
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
    /// Start every queued unit whose keys are free, in queue order. A unit that must wait also
    /// reserves its keys for this scan, so a later unit on the same key never overtakes it,
    /// except that an urgent unit is blocked only by waiting urgent units.
    pub fn start_ready(&mut self, mut start: impl FnMut(W) -> UnitFuture<'a>) {
        let mut blocked: HashSet<String> = HashSet::new();
        let mut blocked_urgent: HashSet<String> = HashSet::new();
        let mut waiting = VecDeque::with_capacity(self.queue.len());
        while let Some(q) = self.queue.pop_front() {
            let reserved = if q.urgent { &blocked_urgent } else { &blocked };
            let free = q
                .keys
                .iter()
                .all(|k| !self.held.contains(k) && !reserved.contains(k));
            if free && self.running.len() < MAX_RUNNING {
                self.held.extend(q.keys.iter().cloned());
                let fut = start(q.work);
                let (id, keys) = (q.id, q.keys);
                self.running
                    .push(Box::pin(async move { (id, keys, fut.await) }));
            } else {
                blocked.extend(q.keys.iter().cloned());
                if q.urgent {
                    blocked_urgent.extend(q.keys.iter().cloned());
                }
                waiting.push_back(q);
            }
        }
        self.queue = waiting;
    }
    /// The next unit to finish (cancel safe: dropping this future keeps every unit running).
    /// Pending forever while nothing runs.
    pub async fn next(&mut self) -> (String, anyhow::Result<()>) {
        match self.running.next().await {
            Some((id, keys, result)) => {
                for k in &keys {
                    self.held.remove(k);
                }
                self.ids.remove(&id);
                (id, result)
            }
            None => std::future::pending().await,
        }
    }
    /// Remove every unit that has not started (a stop admits nothing new).
    pub fn drain_queued(&mut self) -> Vec<W> {
        self.queue
            .drain(..)
            .map(|q| {
                self.ids.remove(&q.id);
                q.work
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc, time::Duration};

    fn unit<'a>(log: Rc<RefCell<Vec<String>>>, name: &'static str, ms: u64) -> UnitFuture<'a> {
        Box::pin(async move {
            log.borrow_mut().push(format!("start {name}"));
            tokio::time::sleep(Duration::from_millis(ms)).await;
            log.borrow_mut().push(format!("end {name}"));
            Ok(())
        })
    }

    /// Units on different keys overlap; units sharing a key run one at a time, in queue order,
    /// and a waiting unit is never overtaken by a later unit on its key.
    #[tokio::test(start_paused = true)]
    async fn keys_serialize_and_disjoint_keys_overlap() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut s: Scheduler<'_, (&'static str, u64)> = Scheduler::default();
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        s.push("slow".into(), k(&["wallet"]), ("slow", 1000))
            .unwrap();
        s.push("a1".into(), k(&["lot:a"]), ("a1", 10)).unwrap();
        s.push("both".into(), k(&["lot:a", "admit"]), ("both", 10))
            .unwrap();
        s.push("a2".into(), k(&["lot:a"]), ("a2", 10)).unwrap();
        s.push("admit".into(), k(&["admit"]), ("admit", 10))
            .unwrap();
        assert!(
            s.push("a1".into(), k(&["lot:a"]), ("dup", 1)).is_err(),
            "a queued id is not doubled"
        );
        let start = |log: &Rc<RefCell<Vec<String>>>| {
            let log = log.clone();
            move |(name, ms): (&'static str, u64)| unit(log.clone(), name, ms)
        };
        s.start_ready(start(&log));
        assert_eq!(s.running(), 2, "slow and a1 run; the rest wait for lot:a");
        let mut done = vec![];
        while s.running() > 0 {
            let (id, r) = s.next().await;
            r.unwrap();
            done.push(id);
            s.start_ready(start(&log));
        }
        assert_eq!(done, ["a1", "both", "a2", "admit", "slow"]);
        let log = log.borrow();
        let pos = |e: &str| log.iter().position(|l| l == e).unwrap();
        assert!(
            pos("end slow") > pos("end admit"),
            "SAFETY: nothing waited on the slow key"
        );
        assert!(pos("end a1") < pos("start both"));
        assert!(pos("end both") < pos("start a2"), "FIFO on a shared key");
        assert!(
            pos("end both") < pos("start admit"),
            "admit queued after `both` does not overtake it on their shared key"
        );
    }

    /// Review of #1155, F1: a slow quote request on lot A holds `admit`; a request for lot B
    /// waits for `admit` (reserving `lot:b`); lot B's claim (urgent) must not wait for lot A's
    /// slow mint. Non-urgent units keep FIFO among themselves.
    #[tokio::test(start_paused = true)]
    async fn urgent_claim_on_lot_b_overtakes_a_waiting_request_and_skips_slow_lot_a() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut s: Scheduler<'_, (&'static str, u64)> = Scheduler::default();
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        s.push("slowA".into(), k(&["admit", "lot:a"]), ("slowA", 60_000))
            .unwrap();
        s.push("reqB".into(), k(&["admit", "lot:b"]), ("reqB", 10))
            .unwrap();
        s.push("listB".into(), k(&["lot:b"]), ("listB", 10))
            .unwrap();
        s.push_as("claimB".into(), k(&["lot:b"]), ("claimB", 10), true)
            .unwrap();
        s.push_as("claimB2".into(), k(&["lot:b"]), ("claimB2", 10), true)
            .unwrap();
        let start = |log: &Rc<RefCell<Vec<String>>>| {
            let log = log.clone();
            move |(name, ms): (&'static str, u64)| unit(log.clone(), name, ms)
        };
        s.start_ready(start(&log));
        while s.running() > 0 {
            s.next().await.1.unwrap();
            s.start_ready(start(&log));
        }
        let log = log.borrow();
        let pos = |e: &str| log.iter().position(|l| l == e).unwrap();
        assert!(
            pos("end claimB") < pos("end slowA"),
            "SAFETY: lot B's claim waited for lot A's slow mint: {log:?}"
        );
        assert!(pos("end claimB") < pos("start claimB2"), "urgent FIFO");
        assert!(pos("end claimB2") < pos("end slowA"));
        assert!(
            pos("end reqB") < pos("start listB"),
            "non-urgent FIFO: {log:?}"
        );
    }
}
