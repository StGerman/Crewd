//! Several workers side by side (#119): per-worker capacity, a per-worker rate-limit pause, and
//! the pin that keeps a continuation on the worker holding its session.
//!
//! One global limit and one account-wide pause could not serve two providers: a Claude
//! five-hour limit stopped dispatch to Grok, which is exactly the idle time a second worker
//! exists to fill. So capacity and the pause are both keyed by worker name, and dispatch is
//! overflow — the first worker in config order with a free slot that is not paused. A
//! continuation whose worker is full waits for it: a session id means nothing to another
//! provider. One whose worker is paused overflows, and `launch` parks the paused worker's
//! session and hands the next worker a brief of where the run stopped (#165).

use std::sync::Arc;

use super::{RateLimitPause, Reservations, Scheduler};
use crate::model::{Feedback, session_id};
use crate::worker::{Session, Worker};

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

    /// The worker the next dispatch goes to, if any can take it. An unpinned issue gets the
    /// first worker in order that is not paused and has a free slot. A pinned one gets its own
    /// worker, and waits for it while it is only full: a full worker frees a slot within a run,
    /// and moving would cost the session. A paused one may be minutes to hours from its
    /// window, so its issue overflows like an unpinned one and `launch` hands it off (#165).
    pub(super) fn pick_worker(&self, pin: Option<&str>, reserved: &Reservations) -> Option<String> {
        let open = |w: &WorkerPool| !self.paused(&w.name) && self.worker_slots(w, reserved) > 0;
        match self.resolve_pin(pin) {
            Some(p) if !self.paused(&p) => {
                self.pool(&p).filter(|w| open(w)).map(|w| w.name.clone())
            }
            _ => self.workers.iter().find(|w| open(w)).map(|w| w.name.clone()),
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

/// How much of the previous worker's last message a handoff brief carries (#165). The tail, not
/// the head: a run cut off by a rate limit stops mid-thought, and the end of its last message is
/// where it stood.
const HANDOFF_TEXT_BYTES: usize = 4096;

impl Scheduler {
    /// The session `worker` runs `issue_id` under, and the worker it takes the issue over from
    /// when that is another one.
    ///
    /// Each worker resumes only its own session (#119). The pinned worker's is
    /// `issue_state.session_id`; dispatching anywhere else is a handoff (#165), which parks the
    /// pinned session with the worktree's `head` rather than overwriting it. A worker resumes one
    /// it parked earlier only while `head` has not moved since: once the other worker has
    /// committed, that conversation describes a tree that is gone. A session no run names a
    /// worker for predates v13: with one worker it can only be that worker's, and with several,
    /// overflow may have sent it to another provider, so it is not resumed.
    pub(super) fn choose_session(
        &self,
        issue_id: &str,
        worker: &str,
        head: Option<&str>,
    ) -> anyhow::Result<(Session, Option<String>)> {
        let pin = self.store.session_worker(issue_id)?;
        let stored = self.store.get(issue_id)?.and_then(|s| s.session_id);
        let resumable = match pin.as_deref() {
            Some(p) if p == worker => stored.clone(),
            None if self.workers.len() == 1 => stored.clone(),
            _ => self
                .store
                .take_parked_session(issue_id, worker)?
                .filter(|(_, at)| at.is_some() && at.as_deref() == head)
                .map(|(id, _)| id),
        };
        let from = pin.filter(|p| p != worker);
        if let (Some(from), Some(id)) = (from.as_deref(), stored.as_deref()) {
            self.store.park_session(issue_id, from, id, head)?;
        }
        let session = match resumable {
            Some(id) => Session::Resume(id),
            None => Session::New(session_id(issue_id, self.clock.wall().0)),
        };
        if stored.as_deref() != Some(session.id()) {
            self.store.set_session(self.clock.as_ref(), issue_id, Some(session.id()))?;
        }
        Ok((session, from))
    }

    /// What the worker taking an issue over from `from` is told of where `from` stopped: why its
    /// last run ended, and that run's last message as `from` itself reads its transcript. The
    /// raw stream stays a post-mortem; only the text reaches the prompt.
    pub(super) fn handoff_brief(&self, issue_id: &str, from: &str) -> anyhow::Result<Feedback> {
        let last =
            self.store.runs_for(issue_id)?.into_iter().find(|r| r.worker.as_deref() == Some(from));
        let why = match self.rate_limit_pauses.get(from) {
            Some(p) => {
                format!("its account hit a rate limit ({}), and its window has not reset", p.kind)
            }
            None => match last.as_ref().and_then(|r| r.outcome.as_deref()) {
                Some(o) => format!("its last run ended {o}"),
                None => "its last run left no outcome".into(),
            },
        };
        let last_text = last
            .and_then(|r| r.transcript)
            .and_then(|t| std::fs::read(t).ok())
            .zip(self.pool(from))
            .and_then(|(bytes, pool)| pool.worker.last_text(&String::from_utf8_lossy(&bytes)))
            .map(|t| tail(&t, HANDOFF_TEXT_BYTES));
        Ok(Feedback::Handoff { from: from.to_string(), why, last_text })
    }
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}
