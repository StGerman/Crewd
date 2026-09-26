//! Several workers side by side (#119): per-worker capacity, a per-worker rate-limit pause, and
//! the pin that keeps a continuation on the worker holding its session.
//!
//! One global limit and one account-wide pause could not serve two providers: a Claude
//! five-hour limit stopped dispatch to Grok, which is exactly the idle time a second worker
//! exists to fill. So capacity and the pause are both keyed by worker name, and dispatch is
//! overflow — the first worker in config order with a free slot that is not paused. A
//! continuation is never overflowed: a session id means nothing to another provider, so one
//! whose worker is full or paused waits for it.

use std::sync::Arc;

use super::{RateLimitPause, Reservations, Scheduler};
use crate::worker::Worker;

/// One configured worker and its own slots.
pub struct WorkerPool {
    /// What run rows, pauses and observers call it; unique across the pools.
    pub name: String,
    pub worker: Arc<dyn Worker>,
    pub max_concurrent: usize,
}

impl Scheduler {
    /// Replace the single worker `new` was given with several, in dispatch order. An empty list
    /// is ignored rather than leaving the scheduler with nothing to dispatch to; `preflight`
    /// rejects a config that would produce one.
    pub fn set_workers(&mut self, pools: Vec<WorkerPool>) {
        if !pools.is_empty() {
            self.workers = pools;
        }
    }

    /// Global capacity: the sum of every worker's slots.
    pub(super) fn capacity(&self) -> usize {
        self.workers.iter().map(|w| w.max_concurrent).sum()
    }

    pub(super) fn pool(&self, name: &str) -> Option<&WorkerPool> {
        self.workers.iter().find(|w| w.name == name)
    }

    /// The worker a pinned name resolves to. A name no configured worker carries — the config
    /// changed since the session began — pins nothing, and `launch` then starts a fresh session
    /// rather than resuming one on a provider that never held it.
    pub(super) fn resolve_pin(&self, pin: Option<&str>) -> Option<String> {
        pin.filter(|p| self.pool(p).is_some()).map(str::to_string)
    }

    /// Where an unpinned reservation is counted: the first worker, the one overflow tries first.
    pub(super) fn default_worker(&self) -> String {
        self.workers[0].name.clone()
    }

    /// Free slots on one worker. Gating runs and reserved continuations count against the worker
    /// that produced them, as well as running ones.
    ///
    /// A gate is work on this machine — a rebase and then whatever `gate.commands` names, which
    /// for this repository is a `cargo test`. Counting only `running` frees the slot the moment
    /// the agent exits, so a fast worker in front of a slow gate lets `dispatch_new` start
    /// another agent while the last one's suite is still compiling. Nothing bounds that: the
    /// gates accumulate, and `max_concurrent` stops describing how many builds the host is
    /// running. The claim is held across the gate for the same reason, so counting it here is
    /// what makes the two agree.
    ///
    /// A continuation waiting out its delay counts too, or `dispatch_new` hands its slot to
    /// whatever became eligible in the same tick and the continuing issue loses its place in
    /// the milestone order at every session boundary (#86).
    pub(super) fn worker_slots(&self, pool: &WorkerPool, reserved: &Reservations) -> usize {
        let used = self
            .running
            .values()
            .chain(self.gating.values().map(|g| &g.run))
            .filter(|r| r.worker == pool.name)
            .count()
            + reserved.values().filter(|r| r.worker == pool.name).count();
        pool.max_concurrent.saturating_sub(used)
    }

    /// The worker the next dispatch goes to, if any can take it. A pinned issue gets its own
    /// worker or waits, even while another has room; an unpinned one gets the first worker in
    /// order that is not paused and has a free slot.
    pub(super) fn pick_worker(&self, pin: Option<&str>, reserved: &Reservations) -> Option<String> {
        let open = |w: &WorkerPool| !self.paused(&w.name) && self.worker_slots(w, reserved) > 0;
        match self.resolve_pin(pin) {
            Some(p) => self.pool(&p).filter(|w| open(w)).map(|w| w.name.clone()),
            None => self.workers.iter().find(|w| open(w)).map(|w| w.name.clone()),
        }
    }

    pub(super) fn paused(&self, worker: &str) -> bool {
        self.rate_limit_pauses.contains_key(worker)
    }

    /// Lift every pause whose `resets_at` has passed, so a tick that finds a window already
    /// reset needs no separate step remembering to un-pause (#37). True while every worker is
    /// still paused, which is when dispatch has nowhere to go at all.
    pub(super) fn all_rate_limited(&mut self) -> bool {
        let now = self.clock.wall().0;
        self.rate_limit_pauses.retain(|worker, p| {
            let live = now < p.resets_at;
            if !live {
                tracing::info!(worker, kind = %p.kind, "rate limit window reset; resuming dispatch");
            }
            live
        });
        self.workers.iter().all(|w| self.paused(&w.name))
    }

    /// Pause one worker until `resets_at_ms`, widening rather than replacing a pause it already
    /// has, in case two runs interrupted by the same limit report it with a few seconds' drift.
    pub(super) fn pause_worker(&mut self, worker: &str, kind: String, resets_at_ms: i64) {
        let resets_at = self
            .rate_limit_pauses
            .get(worker)
            .map_or(resets_at_ms, |p| p.resets_at.max(resets_at_ms));
        self.rate_limit_pauses.insert(
            worker.to_string(),
            RateLimitPause { worker: worker.to_string(), kind, resets_at },
        );
    }

    /// The published pauses, in dispatch order.
    pub(super) fn published_pauses(&self) -> Vec<RateLimitPause> {
        self.workers.iter().filter_map(|w| self.rate_limit_pauses.get(&w.name).cloned()).collect()
    }
}
