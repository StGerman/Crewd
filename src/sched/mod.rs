//! The coordination layer: one tick, one authority, no model in the loop.
//!
//! Tick order is deliberate. Reconciliation runs *first and unconditionally*, so a broken
//! config stops new dispatch without also stranding the runs already in flight. The one piece
//! of reconciliation that sits *behind* the config gate is the parked-issue sweep, because it
//! deletes workspaces on the strength of `is_terminal`, and an overlapping active/terminal
//! config — one of the things preflight rejects — is exactly what would make it delete the
//! workspace of an issue about to be dispatched.
//!
//! Two things are called a gate here and they are unrelated: `Config::preflight` gates
//! *dispatch*, and the handoff gate ([`crate::gate`]) gates a `Done` verdict. `harvest_gates`
//! is the second one's reconciliation step and runs with the rest of reconciliation, ahead of
//! preflight, because a run whose branch is mid-rebase must reach a verdict even under a config
//! typo — otherwise its claim is held for as long as the typo stands.
//!
//! Deviation from the plan: reconciliation lives here rather than in its own module, because
//! it mutates the same `running` map as dispatch and splitting it would mean threading the
//! whole scheduler through a free function for no readability gain.

pub mod delivery;
pub mod retry;
mod review_summary;
pub mod workers;

pub use delivery::DeliveryView;
pub use workers::WorkerPool;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::broker::{Broker, BrokerSession};

use crate::clock::{Clock, Mono, Wall};
use crate::config::Config;
use crate::forge::{Forge, Publisher};
use crate::gate::{self, Gate, GateHandle, Verdict};
use crate::model::{
    ErrorClass, Feedback, Issue, Outcome, Phase, ReviewVerdict, session_id, worktree_key,
};
use crate::project::{ProjectedIssue, Projector};
use crate::store::{RetryEntry, RunRecord, RunStart, Store};
use crate::tracker::{Tracker, TrackerError};
use crate::transcript::Transcripts;
use crate::worker::{Progress, RunHandle, Session, Spawn, Worker};
use crate::workspace::Workspace;

/// Continuations holding a concurrency slot through their delay (#86), by issue id.
type Reservations = HashMap<String, Reservation>;

/// What one reserved slot counts against: its state's limit, and the worker its continuation is
/// pinned to (#119).
#[derive(Clone)]
struct Reservation {
    state: String,
    worker: String,
}

/// Bounded wait for a worker to stop before the workspace may be touched.
const KILL_GRACE_MS: u64 = 10_000;

/// How much of an issue's run history the snapshot carries.
///
/// Deep enough to read a retry pattern — the same failure three times over, or a continuation
/// that keeps coming back — and shallow enough that a view rebuilt on every tick stays cheap.
/// An operator who needs more than this is asking a question for the database, not the
/// dashboard.
const RUNS_PER_ISSUE: usize = 5;

/// A tracker failure never stops the tick — every call site here already returns `Ok(())` and
/// tries again next poll — but a permanent one (a bad credential, most likely) will not
/// resolve itself, and retrying it silently at the poll interval forever looks identical to a
/// transient blip on `warn`-level logs alone. This is the one place that distinction is made,
/// so every call site sees it without repeating the branch.
fn log_tracker_failure(context: &str, e: &TrackerError) {
    if e.class().retryable() {
        tracing::warn!(error = %e, "{context}; will retry next poll");
    } else {
        tracing::error!(error = %e, "{context}; will not resolve on its own — check tracker credentials/config");
    }
}

/// The continuation brief for a conflict handed back to the agent (#111). It says the rebase
/// was aborted because an agent told otherwise goes looking for a half-finished rebase that is
/// not there; and it names the migration rule for `schema.rs` because the obvious resolution —
/// keep both sides' v<N> — edits a released migration, which the gate's own `cargo test` would
/// then fail on, spending a try on a rule the brief could have stated. The commands name the
/// commit, not the ref: a ref resolves in the agent's worktree, where an unset base's `HEAD` is
/// the agent's own branch. It offers a merge as well as a rebase because the gate accepts a
/// branch that already contains the base without rebasing it (#122).
fn conflict_brief(base: &str, base_sha: &str, paths: &[String], n: u32, max: u32) -> String {
    let mut s = format!(
        "the handoff gate's rebase onto {base} ({base_sha}) conflicted in {} (failure {n} of \
         {max}). The rebase was aborted, so the branch is where you left it. Another branch \
         appended to the same place; resolve it yourself, either way: `git rebase {base_sha}` \
         and `git rebase --continue`, or `git merge {base_sha}` and commit — keeping both \
         sides' additions in each conflicted file. Check the result still builds, and finish \
         again.",
        paths.join(", ")
    );
    if paths.iter().any(|p| p == "src/store/schema.rs") {
        s.push_str(
            " In `src/store/schema.rs`, a migration number already on the base is released and \
             must never be edited; renumber yours after it and move its hash in `RELEASED`.",
        );
    }
    s
}

struct Running {
    run_id: String,
    issue: Issue,
    handle: Arc<dyn RunHandle>,
    started: Mono,
    workspace: PathBuf,
    /// Where this run's raw stream is being written, if anywhere. Kept here so retention can
    /// be told which files belong to a live run — see [`Transcripts::prune`].
    transcript: Option<PathBuf>,
    last_progress: Progress,
    last_progress_at: Mono,
    /// Tracker state when this run began, to tell real progress from spinning.
    state_at_start: String,
    /// The pool that spawned it, whose slot it holds and whose pause its rate limit sets.
    worker: String,
    /// The run's review verdicts as delivery will apply them: read off the handle the moment
    /// the run reports `Done`, with each acceptance checked against the branch *then* — before
    /// a gate can rebase it and rewrite the commits they name. Empty until that moment.
    verdicts: Vec<ReviewVerdict>,
    /// This run's authority to write to the tracker, held for exactly as long as the run is in
    /// the `running` map. Never read — dropping it is the point. Every path that ends a run
    /// removes the entry, so every path revokes the token and deletes the config file without
    /// having to remember to.
    _broker: Option<BrokerSession>,
}

/// A run whose agent has said `Done` and whose branch is being rebased and re-gated before that
/// verdict is believed. It has left `running` — the agent process is gone, and with it the
/// broker session — but its claim is still held, so nothing can dispatch onto the worktree the
/// gate is working in. See [`crate::gate`].
struct Gating {
    run: Running,
    handle: Arc<dyn GateHandle>,
    started: Mono,
}

pub use libcrew::{RateLimitPause, Row, Snapshot};

pub struct Scheduler {
    pub cfg: Config,
    clock: Arc<dyn Clock>,
    store: Store,
    tracker: Arc<dyn Tracker>,
    /// In dispatch order; never empty. See [`workers`].
    workers: Vec<WorkerPool>,
    workspace: Arc<dyn Workspace>,
    projector: Arc<dyn Projector>,
    /// `None` when the broker could not start, or the operator turned it off. Dispatch carries
    /// on either way — an agent without tracker tools is a degrade, not a failure.
    broker: Option<Arc<Broker>>,
    /// `None` when transcripts are off or their root could not be created. Same contract as
    /// the broker and the projector: a run with no record on disk, never a run that did not
    /// happen.
    transcripts: Option<Transcripts>,
    /// `None` when the operator turned the gate off. Unlike the broker and the transcripts this
    /// is not a degrade: with no gate a `Done` verdict is applied as the agent reported it,
    /// which is the exact handoff issue #21 is about, so `main.rs` sets one whenever
    /// `gate.enabled` is true and the tests say so explicitly when they want it.
    gate: Option<Arc<dyn Gate>>,
    /// The delivery pair (see [`delivery`]). Both `None` until `set_delivery`, and inert even
    /// then unless `cfg.delivery.enabled` — the config decides, the wiring only enables.
    forge: Option<Arc<dyn Forge>>,
    publisher: Option<Arc<dyn Publisher>>,
    /// When each open delivery was last polled, so the forge is asked at
    /// `delivery.poll_interval_ms` rather than on every tick. Monotonic, like every interval.
    delivery_polled: HashMap<String, Mono>,
    /// The head each delivery's CI is pending on and when that wait began, on the monotonic
    /// clock so a wall-clock step cannot hand a pull request off early (#105). Rebuilt from
    /// the store's wall-clock record after a restart, since `Mono` does not cross one.
    ci_waits: HashMap<String, (String, Mono)>,
    running: HashMap<String, Running>,
    /// Runs between the agent's `Done` and the verdict the gate turns it into. Disjoint from
    /// `running`; an issue is in at most one of the two.
    gating: HashMap<String, Gating>,
    /// Latest issue snapshot seen for each id, for display and routing checks.
    seen: HashMap<String, Issue>,
    /// Consecutive `Continue` verdicts with no tracker state change, per issue.
    no_progress: HashMap<String, u32>,
    /// Whether startup reconciliation has run. See [`Scheduler::recover`].
    recovered: bool,
    /// When parked issues were last re-read from the tracker. `None` until the first sweep, so
    /// a restart reclaims what closed while the process was down without waiting a full
    /// interval. See [`Scheduler::sweep_parked`].
    last_parked_sweep: Option<Mono>,
    /// Keyed by worker name: set when that worker's agent CLI reported a rejected, account-wide
    /// rate limit, and cleared once `resets_at` has passed (#119). Not persisted — a restart mid-pause simply re-learns it from the
    /// next dispatch that hits the same limit, which costs one wasted dispatch and nothing more:
    /// that run still charges no attempt, the same as the one that set the pause in the first
    /// place (#37).
    rate_limit_pauses: HashMap<String, RateLimitPause>,
    ticks: u64,
    last_error: Option<String>,
}

impl Scheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        clock: Arc<dyn Clock>,
        store: Store,
        tracker: Arc<dyn Tracker>,
        worker: Arc<dyn Worker>,
        workspace: Arc<dyn Workspace>,
        projector: Arc<dyn Projector>,
    ) -> Self {
        // The one-element list a config without `[[workers]]` describes; `set_workers` replaces
        // it for one that has them.
        let first = cfg.workers().remove(0);
        let pool = WorkerPool {
            name: first.name(),
            worker,
            max_concurrent: first.max_concurrent.unwrap_or(cfg.agent.max_concurrent),
        };
        Self {
            cfg,
            clock,
            store,
            tracker,
            workers: vec![pool],
            workspace,
            projector,
            broker: None,
            transcripts: None,
            gate: None,
            forge: None,
            publisher: None,
            delivery_polled: HashMap::new(),
            ci_waits: HashMap::new(),
            running: HashMap::new(),
            gating: HashMap::new(),
            seen: HashMap::new(),
            no_progress: HashMap::new(),
            recovered: false,
            last_parked_sweep: None,
            rate_limit_pauses: HashMap::new(),
            ticks: 0,
            last_error: None,
        }
    }

    /// Attach a tool broker.
    ///
    /// A setter rather than a ninth constructor argument: it is optional by nature, and every
    /// existing caller — the scheduler's whole test suite included — is correct without it.
    pub fn set_broker(&mut self, broker: Option<Arc<Broker>>) {
        self.broker = broker;
    }

    pub fn broker(&self) -> Option<&Arc<Broker>> {
        self.broker.as_ref()
    }

    /// Attach a transcript root. A setter for the same reason `set_broker` is one: optional by
    /// nature, and every caller that does not set it is correct without it.
    pub fn set_transcripts(&mut self, transcripts: Option<Transcripts>) {
        self.transcripts = transcripts;
    }

    /// Attach a handoff gate. A setter like the others so every caller that predates it is
    /// still correct — but note the asymmetry in [`Scheduler::gate`]'s doc: leaving it unset is
    /// a decision, not a degrade.
    pub fn set_gate(&mut self, gate: Option<Arc<dyn Gate>>) {
        self.gate = gate;
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// Runs whose agent has finished and whose branch is being rebased and re-gated.
    pub fn gating_count(&self) -> usize {
        self.gating.len()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    // ---- tick ----------------------------------------------------------------

    pub fn tick(&mut self) -> anyhow::Result<()> {
        self.ticks += 1;
        self.last_error = None;

        // Once, ahead of everything else — including the config gate, because a stranded claim
        // must not stay stranded behind a typo in the config. Living here rather than in
        // `main` is deliberate: recovery that a second entry point can forget to call is
        // recovery that silently does not happen, which is the failure this exists to fix.
        if !self.recovered {
            self.recover()?;
        }

        // Unconditional: in-flight runs are reconciled even when config is broken.
        self.harvest_finished()?;
        self.observe_progress()?;
        self.harvest_gates()?;
        self.detect_stalls()?;
        self.refresh_running()?;
        // Reconciliation too: it reads the outside world about runs already over, and may
        // queue a retry that the gate below then decides whether to dispatch.
        self.advance_deliveries()?;

        if let Err(e) = self.cfg.preflight() {
            self.last_error = Some(format!("preflight: {e}"));
            tracing::error!(error = %e, "dispatch preflight failed; skipping dispatch this tick");
            self.publish()?;
            return Ok(());
        }

        self.sweep_parked()?;

        // Checked after the housekeeping above and before the two dispatch steps it guards:
        // reclaiming a closed parked issue's workspace has nothing to do with the account being
        // throttled, but starting a new agent does (#37). Only when every worker is paused;
        // otherwise `pick_worker` skips the paused ones and the rest keep dispatching (#119).
        if self.all_rate_limited() {
            self.publish()?;
            return Ok(());
        }

        self.dispatch_due_retries()?;
        self.dispatch_new()?;
        self.publish()?;
        Ok(())
    }

    // ---- startup recovery ----------------------------------------------------

    /// Release the claims of runs that did not survive the previous process.
    ///
    /// `running` is in-memory and the claim is in the store, so the two can only disagree
    /// across a process boundary. Every ordinary exit reconciles them — `shutdown()`, `Drop for
    /// Scheduler`, the interrupt arm in `main` — and a `SIGKILL`, an OOM kill or a host reboot
    /// reaches none of those. What is left afterwards is an issue marked `running` in a
    /// database with nothing running: [`Store::claim`] refuses it forever, `detect_stalls`
    /// iterates `running` and never sees it, and no retry row exists to bring it back. The
    /// issue silently stops being picked up, with no log line marking the moment it did.
    ///
    /// That inverts the store's contract. Losing `crew.db` degrades to stateless
    /// re-polling; *keeping* it across a hard kill is what produces incorrect behaviour.
    ///
    /// Adopting the work is not on the table — the agent went down with its parent and there is
    /// no handle left to supervise it through — so the claim is released and the worktree
    /// reconciled, which leaves the issue to be dispatched again from its branch.
    ///
    /// The conversation is not dropped with the claim. `release` leaves `session_id` alone, so
    /// the re-dispatch resumes what the killed agent was part-way through rather than
    /// re-orienting from cold; a name the CLI no longer holds still degrades through the
    /// zero-turn path that exists for exactly that.
    pub fn recover(&mut self) -> anyhow::Result<()> {
        self.recovered = true;

        // A claim matched by a live run is not stale. Nothing has populated `running` before
        // the first tick, so this filter is a no-op on the startup path it exists for; it is
        // what makes the method safe to call at any other point rather than only once.
        let stale: Vec<_> = self
            .store
            .claimed()?
            .into_iter()
            .filter(|s| {
                !self.running.contains_key(&s.issue_id) && !self.gating.contains_key(&s.issue_id)
            })
            .collect();
        if stale.is_empty() {
            return Ok(());
        }
        tracing::warn!(
            count = stale.len(),
            "claims held with no live run to match them; the last process did not exit cleanly"
        );

        for st in stale {
            let open_runs =
                self.store.close_open_runs(self.clock.as_ref(), &st.issue_id, "orphaned")?;

            // Removed rather than reused: a directory a kill caught mid-write is in whatever
            // half-applied state it was in at the time, and `git worktree prune` cannot
            // reconcile it because the directory itself still exists. The commits are not in
            // the directory — `Workspace::remove` deletes the branch only when git's own merged
            // check says it carries nothing HEAD already has, so a killed run's work survives
            // this and the next `prepare` attaches straight back to it.
            let workspace_removed = match self.workspace.remove(&st.issue_id, &st.identifier) {
                Ok(removed) => {
                    // The stored branch is only ever wrong in the direction of pointing at a
                    // ref that is gone; cleared here so it does not outlive the ref it named.
                    if removed.branch_deleted {
                        self.store.set_branch(self.clock.as_ref(), &st.issue_id, None)?;
                    }
                    true
                }
                Err(e) => {
                    // Not fatal, deliberately. A worktree that cannot be removed must not be
                    // what keeps the claim held, or this would reproduce the bug it fixes.
                    tracing::warn!(
                        issue_id = %st.issue_id, error = %e,
                        "orphaned workspace cleanup failed; releasing the claim anyway"
                    );
                    false
                }
            };

            // Released last. A second kill part-way through leaves the claim in place, so the
            // next startup sees the issue again and retries the cleanup; releasing first would
            // discard the only record that this issue still has a worktree to reconcile.
            self.store.release(self.clock.as_ref(), &st.issue_id)?;

            tracing::warn!(
                issue_id = %st.issue_id,
                identifier = %st.identifier,
                open_runs,
                workspace_removed,
                "recovered a claim stranded by a hard kill; the issue is dispatchable again"
            );
        }
        Ok(())
    }

    // ---- reconciliation ------------------------------------------------------

    fn harvest_finished(&mut self) -> anyhow::Result<()> {
        let done: Vec<(String, Outcome)> = self
            .running
            .iter()
            .filter_map(|(id, r)| r.handle.finished().map(|o| (id.clone(), o)))
            .collect();

        for (issue_id, outcome) in done {
            let mut r = self.running.remove(&issue_id).expect("just listed");

            // Checked before the outcome is looked at all: the CLI's own verdict for a run cut
            // short this way is ordinarily `Failed`, but that failure is the account's, not this
            // issue's, so it must never reach `apply_outcome` — no attempt charged, no
            // quarantine streak, and no `ErrorClass` (#37). A `resets_at` that is missing or
            // already past cannot be trusted to pause anything, so that case falls through to
            // the ordinary outcome below rather than risking a pause nothing ever lifts.
            if let Some(sig) = r.handle.rate_limit() {
                let now = self.clock.wall().0;
                // `checked_mul`, not `saturating_mul`: saturating turns a nonsense value into
                // `i64::MAX`, which is always in the future, so the one shape of malformed input
                // most likely to appear — an absurd number of seconds — would pause dispatch
                // for the life of the process. Overflow is `None` here, which is the same
                // "cannot be trusted" case as a missing value and takes the fallback below.
                let resets_at_ms = sig.resets_at.and_then(|secs| secs.checked_mul(1_000));
                match resets_at_ms {
                    Some(at) if at > now => {
                        self.pause_for_rate_limit(&issue_id, &r, sig.kind, at)?;
                        continue;
                    }
                    _ => {
                        tracing::warn!(
                            issue_id, kind = %sig.kind, resets_at = ?sig.resets_at,
                            "rate limit event carried no usable resets_at; \
                             falling back to ordinary backoff"
                        );
                    }
                }
            }

            if outcome == Outcome::Done {
                // Now, and not when delivery applies them: the gate below rebases the branch,
                // and a rebase rewrites the very shas these verdicts name. Checked against the
                // branch as the agent left it, an acceptance either names a commit on it or it
                // does not; checked afterwards, an honest one and an invented one look alike.
                r.verdicts = self.verified_verdicts(&issue_id, &r.workspace, r.handle.verdicts());
            }

            // `Done` is a claim, not a verdict, while there is a gate to check it against. The
            // run row stays open and the store claim stays held; what ends here is the agent's
            // authority — the broker session is dropped now rather than when the gate finishes,
            // because the agent that could have used it is gone.
            if outcome == Outcome::Done
                && let Some(gate) = &self.gate
            {
                r._broker = None;

                // Checkpoint the agent's final count before the entry leaves `running`.
                // `observe_progress` only walks `running`, and the run row stays open for the
                // whole gate, so without this the last durable figure is the previous tick's.
                // A hard kill during a gate — which is the long part of the run, not the short
                // one — would then have `recover()` close the row undercounting the attempt,
                // and the per-issue turn budget would be charged less than the agent spent.
                let final_turns = r.handle.progress().turns;
                if let Err(e) = self.store.record_progress(&r.run_id, final_turns) {
                    tracing::warn!(run_id = %r.run_id, error = %e, "progress checkpoint failed");
                }

                let handle = gate.start(&r.issue, &r.workspace);
                let now = self.clock.mono();
                tracing::info!(
                    issue_id, identifier = %r.issue.identifier,
                    workspace = %r.workspace.display(),
                    "run reports done; rebasing and gating before believing it"
                );
                self.gating.insert(issue_id, Gating { run: r, handle, started: now });
                continue;
            }

            let p = r.handle.progress();
            self.store.finish_run(
                self.clock.as_ref(),
                &r.run_id,
                outcome.label(),
                p.turns,
                p.tokens,
            )?;
            self.store.add_turns(&issue_id, p.turns)?;
            self.apply_outcome(&issue_id, &r, outcome)?;
        }
        Ok(())
    }

    /// An account-wide rate limit interrupted this run rather than the run failing on its own
    /// account, so the claim is released exactly as it was found
    /// (`Store::release_for_rate_limit`, not `release`) — the issue resumes at the attempt and
    /// session it was already on once the pause lifts. Only the run's own worker pauses: the
    /// limit is that provider's account, not every worker's (#119).
    fn pause_for_rate_limit(
        &mut self,
        issue_id: &str,
        r: &Running,
        kind: String,
        resets_at_ms: i64,
    ) -> anyhow::Result<()> {
        let p = r.handle.progress();
        self.store.finish_run(self.clock.as_ref(), &r.run_id, "rate_limited", p.turns, p.tokens)?;
        self.store.add_turns(issue_id, p.turns)?;
        self.store.release_for_rate_limit(self.clock.as_ref(), issue_id)?;

        tracing::warn!(
            issue_id, identifier = %r.issue.identifier, worker = %r.worker, kind, resets_at_ms,
            "account-wide rate limit; pausing this worker's dispatch until it resets"
        );
        self.pause_worker(&r.worker, kind, resets_at_ms);
        Ok(())
    }

    fn apply_outcome(
        &mut self,
        issue_id: &str,
        r: &Running,
        outcome: Outcome,
    ) -> anyhow::Result<()> {
        match outcome {
            // Done and Blocked both release the claim and then park the issue in whatever state
            // the tracker currently shows. Releasing alone would leave an issue that is still
            // sitting in an active state looking eligible, and the next tick would pick it
            // straight back up — the same runaway as the continuation path, by another route.
            Outcome::Done => {
                tracing::info!(issue_id, identifier = %r.issue.identifier, "run completed: done");
                self.no_progress.remove(issue_id);
                self.store.clear_gate_failures(issue_id)?;
                self.store.release(self.clock.as_ref(), issue_id)?;
                self.park_here(issue_id, r)?;
                // After the park, so the issue is in the state delivery polls it in. Delivery
                // is what turns this `Done` back into a `Continue` if CI or review disagree.
                self.queue_delivery(issue_id, r.verdicts.clone())?;
            }
            Outcome::Blocked { why } => {
                tracing::info!(issue_id, identifier = %r.issue.identifier, why, "run blocked; parking");
                self.no_progress.remove(issue_id);
                self.store.clear_gate_failures(issue_id)?;
                // The one verdict that ends with a human needing to act, so the reason has to
                // reach the dashboard and not just the log.
                self.store.set_note(self.clock.as_ref(), issue_id, &why)?;
                self.store.release(self.clock.as_ref(), issue_id)?;
                self.park_here(issue_id, r)?;
            }
            Outcome::Continue { why } => {
                let st = self.store.get(issue_id)?;
                let turns = st.as_ref().map(|s| s.cumulative_turns).unwrap_or(0);

                // The global brake the spec lacks: `max_turns` there is per session, and the
                // continuation loop starts a fresh session every time, so nothing bounds the
                // total.
                if turns >= self.cfg.agent.max_turns_per_issue {
                    tracing::warn!(
                        issue_id,
                        turns,
                        budget = self.cfg.agent.max_turns_per_issue,
                        "turn budget exhausted; quarantining instead of continuing"
                    );
                    self.store.record_failure(
                        self.clock.as_ref(),
                        issue_id,
                        ErrorClass::ConfigInvalid,
                        "turn budget exhausted",
                        self.cfg.agent.quarantine_after_identical,
                    )?;
                    return Ok(());
                }

                // Did the tracker state actually move while that run executed?
                let now_state = self.seen.get(issue_id).map(|i| i.state_key()).unwrap_or_default();
                let moved = now_state != r.state_at_start.trim().to_lowercase();
                let streak = if moved {
                    self.no_progress.remove(issue_id);
                    0
                } else {
                    let e = self.no_progress.entry(issue_id.to_string()).or_insert(0);
                    *e += 1;
                    *e - 1
                };

                let delay = retry::continuation_delay_ms(streak, self.cfg.polling.interval_ms);
                let held = self
                    .seen
                    .get(issue_id)
                    .map(|i| i.state_key())
                    .unwrap_or_else(|| r.issue.state_key());
                let due = Wall(self.clock.wall().0 + delay as i64);
                tracing::info!(issue_id, why, delay_ms = delay, moved, "run continues");
                self.store.schedule_retry(
                    self.clock.as_ref(),
                    issue_id,
                    due,
                    0,
                    &format!("continuation: {why}"),
                    Some(&held),
                )?;
            }
            Outcome::Failed { class, msg } => {
                tracing::warn!(issue_id, class = class.as_str(), msg, "run failed");

                // A run that never took a turn established nothing worth resuming into — and if
                // it was resuming, its target is the likeliest reason it produced nothing at
                // all: the CLI answers a name it no longer holds with "No conversation found
                // with session ID" and exits. Dropping the name here is what keeps a dead
                // session from failing every retry identically, straight into quarantine.
                if r.handle.progress().turns == 0 {
                    tracing::info!(issue_id, "run took no turns; dropping its session name");
                    self.store.set_session(self.clock.as_ref(), issue_id, None)?;
                }

                let quarantined = self.store.record_failure(
                    self.clock.as_ref(),
                    issue_id,
                    class,
                    &msg,
                    self.cfg.agent.quarantine_after_identical,
                )?;
                if quarantined {
                    tracing::error!(
                        issue_id,
                        class = class.as_str(),
                        retryable = class.retryable(),
                        "quarantined; no further dispatch until cleared"
                    );
                    self.no_progress.remove(issue_id);
                } else {
                    let attempt = self.store.get(issue_id)?.map(|s| s.attempt).unwrap_or(1);
                    let delay = retry::backoff_ms(attempt, self.cfg.agent.max_retry_backoff_ms);
                    let due = Wall(self.clock.wall().0 + delay as i64);
                    self.store.schedule_retry(
                        self.clock.as_ref(),
                        issue_id,
                        due,
                        attempt,
                        &msg,
                        None,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Record the state we last finished this issue in, so it is not immediately re-dispatched.
    fn park_here(&self, issue_id: &str, r: &Running) -> anyhow::Result<()> {
        let state =
            self.seen.get(issue_id).map(|i| i.state_key()).unwrap_or_else(|| r.issue.state_key());
        self.store.park(self.clock.as_ref(), issue_id, &state)?;
        Ok(())
    }

    /// Read every in-flight run's progress once per tick, and checkpoint the turn count.
    ///
    /// The read serves two consumers. `detect_stalls` wants to know *when* progress last moved,
    /// which `last_progress_at` records. And the store wants to know *how far* the run has got,
    /// because until this existed `run.turns` was written once, by `finish_run`, and the live
    /// count lived only in the handle — which dies with the process, so a hard kill lost how far
    /// every in-flight run had got and `recover()` closed each one at zero (issue #25). The
    /// conversation it then resumed had a recorded cost of nothing.
    ///
    /// Throttled by construction rather than by a timer: one read per run per tick, and a write
    /// only when the turn count moved since the last one. The tick runs at the poll interval,
    /// and the TUI's four-a-second repaint republishes the snapshot without ticking, so the
    /// write rate is bounded by `polling.interval_ms`, not by how chatty the agent is. A
    /// checkpoint that fails is logged and skipped — losing one costs a restart a tick's worth
    /// of progress, and this runs ahead of the config gate so it must not stop reconciliation.
    ///
    /// Separate from `detect_stalls` because that step is optional (`stall_timeout_ms = 0`) and
    /// this one is not: a deployment that turns stall detection off must not also, silently,
    /// turn off durable progress.
    fn observe_progress(&mut self) -> anyhow::Result<()> {
        let now = self.clock.mono();
        for r in self.running.values_mut() {
            let p = r.handle.progress();
            if p == r.last_progress {
                continue;
            }
            if p.turns != r.last_progress.turns
                && let Err(e) = self.store.record_progress(&r.run_id, p.turns)
            {
                tracing::warn!(run_id = %r.run_id, error = %e, "progress checkpoint failed");
            }
            r.last_progress = p;
            r.last_progress_at = now;
        }
        Ok(())
    }

    /// Turn finished gates into verdicts, and kill the ones that have run past the timeout.
    ///
    /// The gate never applies `Done` on its own authority: every verdict goes back through
    /// `apply_outcome`, so a gate-sent `Continue` is subject to the same turn budget, the same
    /// escalating delay and the same no-progress streak as one the agent asked for. That is
    /// what keeps this from being a fourth path around the brakes the continuation loop has.
    fn harvest_gates(&mut self) -> anyhow::Result<()> {
        let timeout = self.cfg.gate.timeout_ms;
        let now = self.clock.mono();

        let mut ready: Vec<(String, Verdict)> = Vec::new();
        for (id, g) in &self.gating {
            if let Some(v) = g.handle.finished() {
                ready.push((id.clone(), v));
            } else if timeout > 0 && now.saturating_since(g.started) > timeout {
                // Killed here, not in `terminate`: the issue is not leaving the scheduler's
                // hands, it is getting a verdict — a failure the agent is told about.
                let step = g.handle.step();
                let killed = g.handle.kill(KILL_GRACE_MS);
                tracing::warn!(issue_id = %id, step, ?killed, timeout_ms = timeout, "gate timed out");
                ready.push((
                    id.clone(),
                    Verdict::Failed {
                        step,
                        output: format!(
                            "the gate was still running after {timeout} ms and was stopped"
                        ),
                        // A timeout can land on either side of the rebase and this path cannot
                        // tell which. `false` is the cautious reading: it omits a claim about
                        // the tree rather than making one that might be wrong.
                        on_base: false,
                    },
                ));
            }
        }

        for (issue_id, verdict) in ready {
            let g = self.gating.remove(&issue_id).expect("just listed");
            let outcome = self.gate_outcome(&issue_id, &g.run, verdict)?;
            // The run row closes with the verdict the gate produced, not the one the agent
            // claimed: an operator reading `continue` on a run whose agent said done is reading
            // the fact that matters.
            let p = g.run.handle.progress();
            self.store.finish_run(
                self.clock.as_ref(),
                &g.run.run_id,
                outcome.label(),
                p.turns,
                p.tokens,
            )?;
            self.store.add_turns(&issue_id, p.turns)?;
            self.apply_outcome(&issue_id, &g.run, outcome)?;
        }
        Ok(())
    }

    /// What the scheduler makes of a gate's verdict.
    ///
    /// A conflict is a human's problem — unless every path in it is one `gate.agent_resolvable`
    /// names (#111) — and a failing command is the agent's, but the agent only gets
    /// `max_failures` consecutive tries: without that bound a suite the agent cannot make pass,
    /// or a conflict it cannot resolve, would be re-dispatched until the turn budget ran out,
    /// which is the runaway the verdict-plus-budget design exists to prevent, arriving by a new
    /// route.
    fn gate_outcome(
        &self,
        issue_id: &str,
        r: &Running,
        verdict: Verdict,
    ) -> anyhow::Result<Outcome> {
        let identifier = r.issue.identifier.as_str();
        let base = self.cfg.gate.base.as_deref();
        Ok(match verdict {
            Verdict::NoCommits => {
                tracing::info!(
                    issue_id,
                    identifier,
                    "branch holds no commits; nothing to hand off"
                );
                Outcome::Done
            }
            Verdict::Passed { rebased } => {
                tracing::info!(issue_id, identifier, rebased, "gate passed on the base");
                Outcome::Done
            }
            Verdict::Conflict { paths, base_sha }
                if gate::agent_resolvable(&paths, &self.cfg.gate.agent_resolvable) =>
            {
                let base = base.unwrap_or("the repository HEAD");
                let step = format!("rebase onto {base}");
                let detail = format!("conflicts in {}", paths.join(", "));
                self.gate_failure(issue_id, identifier, &step, &detail, |n, max| {
                    conflict_brief(base, &base_sha, &paths, n, max)
                })?
            }
            Verdict::Conflict { paths, base_sha } => {
                let base = base.unwrap_or("the repository HEAD");
                tracing::warn!(
                    issue_id,
                    identifier,
                    ?paths,
                    "rebase conflicts; blocking for a human"
                );
                // Queued for whichever run a human's unblocking dispatches next: `last_error`
                // reaches the dashboard, not the prompt, and without this the resumed session
                // is told it merely ran out of turns and reports its work already done (#109).
                let fb =
                    Feedback::Conflict { base: base.to_string(), base_sha, paths: paths.clone() };
                self.store.set_pending_feedback(
                    self.clock.as_ref(),
                    issue_id,
                    &serde_json::to_string(&fb)?,
                )?;
                Outcome::Blocked {
                    why: format!(
                        "rebase onto {base} conflicts in {} file(s): {}",
                        paths.len(),
                        paths.join(", ")
                    ),
                }
            }
            Verdict::Stuck { step, output } => {
                tracing::error!(issue_id, identifier, step, "worktree left mid-rebase; blocking");
                Outcome::Blocked { why: format!("the handoff gate is stuck at `{step}`: {output}") }
            }
            Verdict::Failed { step, output, on_base } => {
                self.gate_failure(issue_id, identifier, &step, &output, |n, max| {
                    // Only say where the work sits when the gate actually got that far. A
                    // base that would not resolve, or a rebase that was refused and aborted,
                    // leaves the branch exactly where the agent left it — telling it to "fix
                    // this on top of" a rebase that never happened describes a tree it will
                    // not find, and the usual result is a `Done` repeated unchanged.
                    let where_it_sits = if on_base {
                        "the branch now sits on the base, so fix this on top of it"
                    } else {
                        "the branch was not rebased and is where you left it, so fix this there"
                    };
                    format!(
                        "the handoff gate failed at `{step}` (failure {n} of {max}); \
                         {where_it_sits} and finish again. Output:\n{output}"
                    )
                })?
            }
        })
    }

    /// Charge one gate failure to the issue's streak: a `Continue` carrying `brief(n, max)` for
    /// the agent, or — once the streak reaches `gate.max_failures` — `Blocked` with `detail` for
    /// the human, who has no use for instructions addressed to the agent.
    fn gate_failure(
        &self,
        issue_id: &str,
        identifier: &str,
        step: &str,
        detail: &str,
        brief: impl FnOnce(u32, u32) -> String,
    ) -> anyhow::Result<Outcome> {
        let n = self.store.bump_gate_failures(issue_id)?;
        let max = self.cfg.gate.max_failures;
        if n >= max {
            tracing::warn!(
                issue_id,
                identifier,
                step,
                failures = n,
                "gate failed repeatedly; blocking"
            );
            return Ok(Outcome::Blocked {
                why: format!(
                    "the handoff gate failed {n} time(s) in a row; last at `{step}`:\n{detail}"
                ),
            });
        }
        tracing::info!(issue_id, identifier, step, failures = n, max, "gate failed; continuing");
        Ok(Outcome::Continue { why: brief(n, max) })
    }

    fn detect_stalls(&mut self) -> anyhow::Result<()> {
        let limit = self.cfg.agent.stall_timeout_ms;
        if limit == 0 {
            return Ok(()); // disabled
        }
        let now = self.clock.mono();

        // `observe_progress` has just refreshed `last_progress_at`, so anyone whose mark is
        // older than the limit has been quiet for that long.
        let stalled: Vec<String> = self
            .running
            .iter()
            .filter(|(_, r)| now.saturating_since(r.last_progress_at) > limit)
            .map(|(id, _)| id.clone())
            .collect();

        for id in stalled {
            tracing::warn!(issue_id = %id, limit_ms = limit, "stalled; terminating");
            self.terminate(&id, false)?;
            self.store.record_failure(
                self.clock.as_ref(),
                &id,
                ErrorClass::Stall,
                "no agent output within stall timeout",
                self.cfg.agent.quarantine_after_identical,
            )?;
            if self.store.get(&id)?.map(|s| s.is_quarantined()) != Some(true) {
                let attempt = self.store.get(&id)?.map(|s| s.attempt).unwrap_or(1);
                let delay = retry::backoff_ms(attempt, self.cfg.agent.max_retry_backoff_ms);
                let due = Wall(self.clock.wall().0 + delay as i64);
                self.store.schedule_retry(
                    self.clock.as_ref(),
                    &id,
                    due,
                    attempt,
                    "stalled",
                    None,
                )?;
            }
        }
        Ok(())
    }

    fn refresh_running(&mut self) -> anyhow::Result<()> {
        // A gating issue still holds its claim, so a ticket that closes mid-gate has to be seen
        // here too, or its worktree would be reclaimed only by the parked sweep, long after.
        let ids: Vec<String> = self.running.keys().chain(self.gating.keys()).cloned().collect();
        if ids.is_empty() {
            return Ok(());
        }

        let refreshed = match self.tracker.by_ids(&ids) {
            Ok(v) => v,
            Err(e) => {
                // Keep workers running; a tracker blip must not cancel real work.
                log_tracker_failure("running-state refresh failed; keeping workers", &e);
                self.last_error = Some(format!("refresh: {e}"));
                return Ok(());
            }
        };

        let mut returned = Vec::new();
        for issue in refreshed {
            returned.push(issue.id.clone());
            self.store.clear_miss(&issue.id)?;
            let key = issue.state_key();
            self.seen.insert(issue.id.clone(), issue.clone());

            if self.cfg.is_terminal(&key) {
                tracing::info!(issue_id = %issue.id, state = %issue.state, "terminal; stopping and cleaning");
                self.terminate(&issue.id, true)?;
                self.store.release(self.clock.as_ref(), &issue.id)?;
            } else if self.cfg.is_active(&key) && self.routable(&issue) {
                if let Some(r) = self.running.get_mut(&issue.id) {
                    r.issue = issue;
                } else if let Some(g) = self.gating.get_mut(&issue.id) {
                    g.run.issue = issue;
                }
            } else {
                tracing::info!(
                    issue_id = %issue.id, state = %issue.state,
                    "no longer active or routable; stopping without cleanup"
                );
                self.terminate(&issue.id, false)?;
                self.store.release(self.clock.as_ref(), &issue.id)?;
            }
        }

        // Omission is ambiguous: genuinely gone, or a filtered query that has not caught up.
        // The spec kills on the first miss; a grace count means one blip cannot destroy
        // in-flight work.
        for id in ids.iter().filter(|i| !returned.contains(i)) {
            let misses = self.store.bump_miss(id)?;
            if misses >= self.cfg.agent.refresh_miss_grace {
                tracing::info!(issue_id = %id, misses, "not visible for consecutive refreshes; stopping");
                self.terminate(id, false)?;
                self.store.release(self.clock.as_ref(), id)?;
            } else {
                tracing::debug!(issue_id = %id, misses, "not visible; within grace");
            }
        }
        Ok(())
    }

    /// Reclaim the worktrees of parked issues whose tickets have since closed.
    ///
    /// A run that ends `Done` or `Blocked` is released and parked, which takes it out of
    /// `running`; `refresh_running` never sees it again, it has no retry row for
    /// `dispatch_due_retries` to find, and once the ticket reaches a terminal state
    /// `by_states` over the active set stops returning it too. Every existing cleanup path is
    /// downstream of one of those three, so nothing ever matched — observed as a worktree, a
    /// branch and a rust-analyzer index per finished issue, left behind for good.
    ///
    /// Parked issues are not urgent, so this runs on its own, slower cadence
    /// (`parked_sweep_interval_ms`) rather than every tick, and asks about all of them in one
    /// `by_ids` batch. The cost per interval is therefore the number of issues still parked,
    /// not the number of ticks they have spent parked. The timestamp is taken before the call,
    /// so a tracker failure waits a full interval rather than retrying every tick.
    ///
    /// A cleaned issue is unparked. Left parked, it would be re-fetched on every sweep for the
    /// rest of the process's life, so the sweep's cost would grow with every ticket ever
    /// closed; unparked and in a terminal state it is inert, and should a human reopen it into
    /// an active state `dispatch_new` treats it like any issue with no park, which is what a
    /// state change means everywhere else here. An issue that has moved to a state that is
    /// neither active nor terminal keeps its warm worktree and stays parked, the same choice
    /// `refresh_running` makes for a running one. An issue the tracker no longer returns is
    /// left alone as well: a genuinely deleted ticket and an eventual-consistency blip look the
    /// same from here, and this path has no grace count. It costs one id in the batch.
    fn sweep_parked(&mut self) -> anyhow::Result<()> {
        let interval = self.cfg.agent.parked_sweep_interval_ms;
        if interval == 0 {
            return Ok(()); // disabled
        }
        let now = self.clock.mono();
        if let Some(last) = self.last_parked_sweep
            && now.saturating_since(last) < interval
        {
            return Ok(());
        }
        self.last_parked_sweep = Some(now);

        let parked: Vec<_> = self
            .store
            .parked()?
            .into_iter()
            .filter(|s| {
                !self.running.contains_key(&s.issue_id) && !self.gating.contains_key(&s.issue_id)
            })
            .collect();
        if parked.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = parked.iter().map(|s| s.issue_id.clone()).collect();

        let refreshed = match self.tracker.by_ids(&ids) {
            Ok(v) => v,
            Err(e) => {
                log_tracker_failure("parked-issue sweep failed", &e);
                self.last_error = Some(format!("parked sweep: {e}"));
                return Ok(());
            }
        };

        let mut reclaimed = 0usize;
        for issue in refreshed {
            let key = issue.state_key();
            self.seen.insert(issue.id.clone(), issue.clone());
            if !self.cfg.is_terminal(&key) {
                continue;
            }
            // The directory goes; the branch is `Workspace::remove`'s call, and it keeps one
            // that carries commits HEAD does not have. Closing a ticket is not a decision to
            // throw away the work done under it.
            match self.workspace.remove(&issue.id, &issue.identifier) {
                Ok(removed) => {
                    // The stored branch follows what cleanup actually did, here as at every
                    // other `remove` call site: a name that outlives its ref sends an operator
                    // to something that is no longer there. This path is the one a reviewer
                    // will forget, because the sweep is the only caller that reaches `remove`
                    // without a live run behind it.
                    if removed.branch_deleted {
                        self.store.set_branch(self.clock.as_ref(), &issue.id, None)?;
                    }
                    reclaimed += 1;
                }
                Err(e) => {
                    // Not fatal, and the issue stays parked so the next sweep tries again.
                    tracing::warn!(
                        issue_id = %issue.id, error = %e,
                        "parked workspace cleanup failed; will retry next sweep"
                    );
                    continue;
                }
            }
            self.store.unpark(self.clock.as_ref(), &issue.id)?;
            tracing::info!(
                issue_id = %issue.id, identifier = %issue.identifier, state = %issue.state,
                "parked issue reached a terminal state; workspace reclaimed"
            );
        }
        tracing::debug!(parked = ids.len(), reclaimed, "parked-issue sweep complete");
        Ok(())
    }

    /// Stop a run and, only once it is confirmed stopped, optionally remove its workspace.
    ///
    /// The ordering is the point. The spec says "terminate worker and clean workspace" with no
    /// constraint between the two, which deletes a directory out from under a process that may
    /// still be writing to it.
    fn terminate(&mut self, issue_id: &str, cleanup: bool) -> anyhow::Result<()> {
        let r = if let Some(r) = self.running.remove(issue_id) {
            let outcome = r.handle.kill(KILL_GRACE_MS);
            tracing::debug!(issue_id, ?outcome, "worker stopped");
            r
        } else if let Some(g) = self.gating.remove(issue_id) {
            // The agent is already gone; what is running is the gate, and it is stopped under
            // the same rule — confirmed dead before the worktree it runs in may be touched.
            let outcome = g.handle.kill(KILL_GRACE_MS);
            tracing::debug!(issue_id, ?outcome, "gate stopped");
            self.store.clear_gate_failures(issue_id)?;
            g.run
        } else {
            return Ok(());
        };

        // `p.tokens` is `None` here in every case but one: a run that had already reported its
        // result when the tracker moved the ticket, so the kill found nothing left to stop.
        // For the rest there is no total and none is invented.
        let p = r.handle.progress();
        self.store.finish_run(self.clock.as_ref(), &r.run_id, "killed", p.turns, p.tokens)?;
        self.store.add_turns(issue_id, p.turns)?;

        if cleanup {
            match self.workspace.remove(issue_id, &r.issue.identifier) {
                Ok(removed) if removed.branch_deleted => {
                    self.store.set_branch(self.clock.as_ref(), issue_id, None)?;
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(issue_id, error = %e, "workspace cleanup failed"),
            }
        }
        Ok(())
    }

    // ---- dispatch ------------------------------------------------------------

    fn routable(&self, issue: &Issue) -> bool {
        issue.dispatchable
            && self
                .cfg
                .tracker
                .required_labels
                .iter()
                .all(|want| issue.labels.iter().any(|l| l == want))
    }

    fn state_slots(&self, state_key: &str, reserved: &Reservations) -> usize {
        let used = self
            .running
            .values()
            .chain(self.gating.values().map(|g| &g.run))
            .map(|r| r.issue.state_key())
            .chain(reserved.values().map(|r| r.state.clone()))
            .filter(|k| k == state_key)
            .count();
        // Unset falls back to the workers' capacity rather than `Config::state_limit`'s, which
        // cannot see pools `set_workers` attached.
        let limit = self.cfg.agent.max_concurrent_by_state.get(state_key).copied();
        limit.unwrap_or_else(|| self.capacity()).saturating_sub(used)
    }

    /// Continuations holding a slot, read from the retry rows themselves so a reservation
    /// cannot outlive the row it belongs to: `release`, `clear_retry` and quarantine all end
    /// it by deleting that row. Each counts against the worker holding its session.
    fn reservations(&self) -> anyhow::Result<Reservations> {
        let pins = self.store.session_workers()?;
        Ok(self
            .store
            .all_retries()?
            .into_iter()
            .filter_map(|r| {
                let state = r.reserved_state?;
                let worker = self.reserving_worker(pins.get(&r.issue_id).map(String::as_str));
                Some((r.issue_id, Reservation { state, worker }))
            })
            .collect())
    }

    fn reserving_worker(&self, pin: Option<&str>) -> String {
        self.resolve_pin(pin).unwrap_or_else(|| self.default_worker())
    }

    /// Dispatch every retry that is due, and re-read every continuation still holding a slot.
    ///
    /// The re-read is what ends a reservation whose ticket went terminal or lost its label
    /// during the delay: without it the slot stays held until the delay runs out, which grows
    /// to the poll interval for a continuation that is not moving the ticket.
    fn dispatch_due_retries(&mut self) -> anyhow::Result<()> {
        let now = self.clock.wall().0;
        let pending: Vec<RetryEntry> = self
            .store
            .all_retries()?
            .into_iter()
            .filter(|r| r.due_at <= now || r.reserved_state.is_some())
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = pending.iter().map(|d| d.issue_id.clone()).collect();

        let refreshed = match self.tracker.by_ids(&ids) {
            Ok(v) => v,
            Err(e) => {
                log_tracker_failure("retry refresh failed", &e);
                self.last_error = Some(format!("retry refresh: {e}"));
                return Ok(());
            }
        };
        let by_id: HashMap<String, Issue> =
            refreshed.into_iter().map(|i| (i.id.clone(), i)).collect();
        let mut reserved = self.reservations()?;

        for (i, entry) in pending.iter().enumerate() {
            let Some(issue) = by_id.get(&entry.issue_id) else {
                // One omission is not proof the ticket is gone (`refresh_miss_grace` exists for
                // exactly that), so a reservation still waiting keeps its slot and its retry;
                // only the entry that is due acts on the omission, as it did before #86.
                if entry.due_at > now {
                    continue;
                }
                // Gone from the tracker: release rather than inventing a state for it.
                tracing::info!(issue_id = %entry.issue_id, "retry target not visible; releasing");
                reserved.remove(&entry.issue_id);
                self.store.clear_retry(&entry.issue_id)?;
                self.store.release(self.clock.as_ref(), &entry.issue_id)?;
                continue;
            };
            self.seen.insert(issue.id.clone(), issue.clone());
            let key = issue.state_key();

            if self.cfg.is_terminal(&key) {
                reserved.remove(&issue.id);
                self.store.clear_retry(&issue.id)?;
                match self.workspace.remove(&issue.id, &issue.identifier) {
                    Ok(removed) if removed.branch_deleted => {
                        self.store.set_branch(self.clock.as_ref(), &issue.id, None)?;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(issue_id = %issue.id, error = %e, "cleanup failed");
                    }
                }
                self.store.release(self.clock.as_ref(), &issue.id)?;
                continue;
            }
            if !self.cfg.is_active(&key) || !self.routable(issue) {
                reserved.remove(&issue.id);
                self.store.clear_retry(&issue.id)?;
                self.store.release(self.clock.as_ref(), &issue.id)?;
                continue;
            }
            if entry.due_at > now {
                // A reservation still waiting. It follows the ticket's state, so a move during
                // the delay does not leave the old state's limit charged for a run it will not
                // hold.
                if entry.reserved_state.as_deref() != Some(key.as_str()) {
                    self.store.schedule_retry(
                        self.clock.as_ref(),
                        &issue.id,
                        Wall(entry.due_at),
                        entry.attempt,
                        entry.reason.as_deref().unwrap_or_default(),
                        Some(&key),
                    )?;
                    let pin = self.store.session_worker(&issue.id)?;
                    let worker = self.reserving_worker(pin.as_deref());
                    reserved.insert(issue.id.clone(), Reservation { state: key, worker });
                }
                continue;
            }

            // Its own reservation is the slot it is about to take, so it does not count
            // against itself. Nor do the reservations queued behind it: when a state change
            // leaves more reservations than a limit allows, each counting the others would
            // defer every one of them forever, so the earliest due goes first. A failure
            // retry has no place in that queue and counts them all.
            let own = reserved.remove(&issue.id);
            let counted: Reservations = if own.is_some() {
                let ahead: Vec<&str> = pending[..i].iter().map(|e| e.issue_id.as_str()).collect();
                reserved
                    .iter()
                    .filter(|(id, _)| ahead.contains(&id.as_str()))
                    .map(|(id, k)| (id.clone(), k.clone()))
                    .collect()
            } else {
                reserved.clone()
            };
            // A continuation goes back to the worker holding its session, and waits for it when
            // that worker is full or paused rather than jumping provider (#119).
            let pin = self.store.session_worker(&issue.id)?;
            let worker = self.pick_worker(pin.as_deref(), &counted);
            let Some(worker) = worker.filter(|_| self.state_slots(&key, &counted) > 0) else {
                // Leave the entry in place; it is already due and will be retried next tick.
                tracing::debug!(issue_id = %issue.id, "no slots for retry; deferring");
                if let Some(k) = own {
                    reserved.insert(issue.id.clone(), k);
                }
                continue;
            };

            let issue = issue.clone();
            self.store.clear_retry(&issue.id)?;
            // The retry reason is the one thing the orchestrator knows about this attempt that
            // the agent cannot see from inside the worktree — for a gate-sent continuation, the
            // output of the suite that disagreed with its `Done`.
            self.launch(&issue, entry.attempt, entry.reason.as_deref(), &worker)?;
        }
        Ok(())
    }

    fn dispatch_new(&mut self) -> anyhow::Result<()> {
        let candidates = match self.tracker.by_states(&self.cfg.tracker.active_states) {
            Ok(v) => v,
            Err(e) => {
                log_tracker_failure("candidate fetch failed; skipping dispatch", &e);
                self.last_error = Some(format!("candidates: {e}"));
                return Ok(());
            }
        };

        let mut sorted = candidates;
        sorted.sort_by(|a, b| {
            // Providers disagree about what integer priorities mean, so rank only the presence
            // of a priority, then fall back to age. Adapters that want a different order
            // normalise before we see it.
            let ka = (a.priority.is_none(), a.priority.unwrap_or(i32::MAX));
            let kb = (b.priority.is_none(), b.priority.unwrap_or(i32::MAX));
            ka.cmp(&kb)
                .then_with(|| match (a.created_at, b.created_at) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.identifier.cmp(&b.identifier))
        });

        let reserved = self.reservations()?;
        for issue in sorted {
            // No worker with room for anyone: nothing further down the queue can go either.
            if self.pick_worker(None, &reserved).is_none() {
                break;
            }
            self.seen.insert(issue.id.clone(), issue.clone());

            let key = issue.state_key();
            if !self.cfg.is_active(&key) || self.cfg.is_terminal(&key) {
                continue;
            }
            if !self.routable(&issue) {
                continue;
            }
            if self.running.contains_key(&issue.id) {
                continue;
            }
            if self.state_slots(&key, &reserved) == 0 {
                // Per-state saturation skips this issue but not the rest of the queue.
                continue;
            }

            let st = self.store.ensure(
                self.clock.as_ref(),
                &issue.id,
                &issue.identifier,
                &worktree_key(&issue.id, &issue.identifier),
            )?;
            if st.is_quarantined() {
                continue;
            }
            if st.phase == Phase::RetryQueued {
                continue; // a timer owns it
            }
            if st.cumulative_turns >= self.cfg.agent.max_turns_per_issue {
                continue;
            }
            match st.parked_state.as_deref() {
                // Still sitting where we left it: nothing new to act on.
                Some(parked) if parked == key => continue,
                // The state moved, so the park is stale and the issue is live again.
                Some(_) => self.store.unpark(self.clock.as_ref(), &issue.id)?,
                None => {}
            }

            // `st.attempt`, not a literal 0. For a genuinely new issue these are the same,
            // but a rate-limit pause releases the claim with the attempt preserved
            // (`release_for_rate_limit`) and sends the issue back through here — so a hard 0
            // would tell the worker and the transcript this is a first attempt while the store
            // still says it is the Nth, which is exactly the disagreement the pause exists to
            // avoid.
            //
            // An issue holding a session — one a rate limit or a human's unblock sent back
            // through here — is pinned to its worker like a continuation, and skipped while
            // that worker has no room (#119).
            let pin = self.store.session_worker(&issue.id)?;
            let Some(worker) = self.pick_worker(pin.as_deref(), &reserved) else { continue };
            self.launch(&issue, st.attempt, None, &worker)?;
        }
        Ok(())
    }

    /// Claim, prepare, spawn — in that order.
    ///
    /// The claim commits before the worker exists. The spec spawns first and records the claim
    /// afterwards, leaving a window in which a fast-exiting worker reports against state that
    /// has not been written yet.
    fn launch(
        &mut self,
        issue: &Issue,
        attempt: u32,
        brief: Option<&str>,
        worker: &str,
    ) -> anyhow::Result<()> {
        let pool = self.pool(worker).expect("pick_worker only names a configured worker");
        let (pool_worker, pool_name) = (pool.worker.clone(), pool.name.clone());

        self.store.ensure(
            self.clock.as_ref(),
            &issue.id,
            &issue.identifier,
            &worktree_key(&issue.id, &issue.identifier),
        )?;

        if !self.store.claim(self.clock.as_ref(), &issue.id)? {
            tracing::debug!(issue_id = %issue.id, "already claimed; not dispatching");
            return Ok(());
        }

        let prepared = match self.workspace.prepare(&issue.id, &issue.identifier) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(issue_id = %issue.id, error = %e, "workspace preparation failed");
                let class = match e {
                    crate::workspace::WorkspaceError::OutsideRoot { .. } => {
                        ErrorClass::WorkspaceOutsideRoot
                    }
                    _ => ErrorClass::WorkspaceIo,
                };
                let quarantined = self.store.record_failure(
                    self.clock.as_ref(),
                    &issue.id,
                    class,
                    "workspace preparation failed",
                    self.cfg.agent.quarantine_after_identical,
                )?;
                if !quarantined {
                    let attempt = self.store.get(&issue.id)?.map(|s| s.attempt).unwrap_or(1);
                    let delay = retry::backoff_ms(attempt, self.cfg.agent.max_retry_backoff_ms);
                    let due = Wall(self.clock.wall().0 + delay as i64);
                    self.store.schedule_retry(
                        self.clock.as_ref(),
                        &issue.id,
                        due,
                        attempt,
                        "workspace error",
                        None,
                    )?;
                }
                return Ok(());
            }
        };

        // Persisted here, before the worker exists, for the same reason the claim above is:
        // this is the one moment the real name is known. Deriving it later from `identifier`
        // is what the finding on PR #31 flagged — `Store::ensure` can rename `identifier` after
        // this point, and `Workspace::remove` sometimes deletes the ref cleanup decided the run
        // did not need — so a recomputed name can point at a branch that was never this run's,
        // or at one that no longer exists. Written unconditionally, including on a resumed
        // attempt, so a stale value from a since-renamed identifier does not survive a retry.
        self.store.set_branch(self.clock.as_ref(), &issue.id, prepared.branch.as_deref())?;

        let run_id = format!("{}-{}", issue.id, self.clock.wall().0);

        // The conversation is named here, before the process exists, for the same reason the
        // claim is written first: a run that dies early must still leave behind a name its
        // continuation can resume, and the child cannot be the one to record it.
        //
        // A session held by another worker is not resumed: its id means nothing to this
        // provider (#119). `pick_worker` never sends a pinned issue elsewhere, so this is the
        // case of a worker removed from the config since the session began. A session no run
        // names a worker for predates v13: with one worker it can only be that worker's, and
        // with several, overflow may have sent it to another provider, so it starts fresh.
        let pin = self.store.session_worker(&issue.id)?;
        let stored = self.store.get(&issue.id)?.and_then(|s| s.session_id);
        let owned = match pin.as_deref() {
            Some(p) => p == pool_name,
            None => self.workers.len() == 1,
        };
        let session = match stored {
            Some(id) if owned => Session::Resume(id),
            _ => {
                let id = session_id(&issue.id, self.clock.wall().0);
                self.store.set_session(self.clock.as_ref(), &issue.id, Some(&id))?;
                Session::New(id)
            }
        };
        // A resumed session holds the body it was started with, and nothing newer: a decision
        // written into the description since then is otherwise never read (#109). An unknown
        // previous hash counts as changed — re-sending a body costs a few tokens, missing one
        // costs the attempt.
        let body_hash = blake3::hash(issue.body.as_deref().unwrap_or("").as_bytes()).to_hex();
        let seen = self.store.swap_session_body(self.clock.as_ref(), &issue.id, &body_hash)?;
        let body_changed = session.is_resume() && seen.as_deref() != Some(body_hash.as_str());
        // Opened before the process exists, like the claim and the session name, and for a
        // reason specific to this one: a run that dies in its first second is exactly the run
        // someone will want the bytes from, so the file and the record of where it went both
        // have to predate the thing that might die.
        let transcript = self.transcripts.as_ref().and_then(|t| t.open(&run_id));
        let transcript_path = transcript.as_ref().map(|t| t.path().to_path_buf());
        // Asked of the worker, not read from `cfg`: the worker is what builds the argv, so this
        // is the one source that cannot disagree with what the child was given.
        let model = pool_worker.model();
        self.store.start_run(
            self.clock.as_ref(),
            &RunStart {
                run_id: &run_id,
                issue_id: &issue.id,
                session_id: session.id(),
                transcript: transcript_path.as_deref(),
                model: &model,
                worker: &pool_name,
            },
        )?;

        // Opened before the worker exists, for the same reason the claim and the session name
        // are: the token has to be inside the config file the child reads at startup, so it
        // cannot be something the child reports back afterwards.
        let broker_session = self.broker.as_ref().and_then(|b| match b.open(issue, &run_id) {
            Ok(s) => Some(s),
            Err(e) => {
                // Degrade, never fail: the acceptance criterion for an unavailable broker is
                // an agent without tools, not a run that did not happen.
                tracing::warn!(
                    issue_id = %issue.id, error = %e,
                    "broker session could not be opened; dispatching without tracker tools"
                );
                None
            }
        });

        // Taken, not read: whatever delivery queued for this issue reaches exactly this run.
        // A feedback row that failed to parse is dropped with a warning rather than blocking
        // the dispatch — the pull request still shows the failure, and the agent still has
        // the issue. Delivery's word wins over the retry reason when both exist, because a
        // delivery hand-back writes both and the structured one carries more; the reason is
        // what is left when there is no row, or the row could not be read — for a gate-sent
        // continuation, the output of the suite that disagreed with the agent's `Done`.
        //
        // A queued conflict is taken first and wins over both: it is what stopped the most
        // recent run, and no CI result or review on the branch can be acted on until the branch
        // rebases again.
        let parse = |j: String, source: &str| match serde_json::from_str::<Feedback>(&j) {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!(issue_id = %issue.id, error = %e, source, "unreadable feedback dropped");
                None
            }
        };
        let conflict = self
            .store
            .take_pending_feedback(self.clock.as_ref(), &issue.id)?
            .and_then(|j| parse(j, "conflict"));
        let delivery = self
            .store
            .take_delivery_feedback(self.clock.as_ref(), &issue.id)?
            .and_then(|j| parse(j, "delivery"));
        let feedback: Option<Feedback> = conflict
            .or(delivery)
            .or_else(|| brief.map(|b| Feedback::Gate { output: b.to_string() }));

        let handle = pool_worker.spawn(Spawn {
            tools: broker_session.as_ref().map(|s| s.endpoint()),
            transcript,
            feedback: feedback.as_ref(),
            wip: &prepared.wip,
            body_changed,
            ..Spawn::new(issue, &prepared.path, attempt, &session)
        });
        // The stall clock starts here, after workspace preparation — not at dispatch. Hook or
        // setup time inside its own timeout must not eat the agent's stall budget.
        let now = self.clock.mono();

        // The branch is logged alongside the directory because it, not the directory, is what
        // a reviewer goes looking for once the run is over.
        tracing::info!(
            issue_id = %issue.id,
            identifier = %issue.identifier,
            attempt,
            workspace = %prepared.path.display(),
            branch = prepared.branch.as_deref().unwrap_or("-"),
            session = session.id(),
            resumed = session.is_resume(),
            worker = %pool_name,
            tools = broker_session.is_some(),
            model = model.model.as_deref().unwrap_or("-"),
            effort = model.effort.map(|e| e.as_str()).unwrap_or("-"),
            feedback = feedback.as_ref().map(|f| f.label()).unwrap_or("-"),
            wip = prepared.wip.len(),
            body_changed,
            transcript = transcript_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "-".into()),
            "dispatched"
        );

        self.running.insert(
            issue.id.clone(),
            Running {
                run_id,
                state_at_start: issue.state.clone(),
                worker: pool_name,
                issue: issue.clone(),
                handle,
                started: now,
                workspace: prepared.path,
                transcript: transcript_path,
                last_progress: Progress::default(),
                last_progress_at: now,
                verdicts: Vec::new(),
                _broker: broker_session,
            },
        );

        // After the insert, so the run just dispatched is among the protected paths. Retention
        // runs at dispatch rather than on a timer because that is the only moment a new file
        // appears, and a timer would be a second thing to reason about on the tick.
        if let Some(t) = &self.transcripts {
            let live: Vec<PathBuf> =
                self.running.values().filter_map(|r| r.transcript.clone()).collect();
            t.prune(&live);
        }
        Ok(())
    }

    // ---- observability -------------------------------------------------------

    pub fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let now_mono = self.clock.mono();
        let now_wall = self.clock.wall().0;
        let states = self.store.all()?;
        let retries: HashMap<String, RetryEntry> =
            self.store.all_retries()?.into_iter().map(|r| (r.issue_id.clone(), r)).collect();
        let totals = self.store.token_totals()?;

        // One query for every issue's history, not one per row: this runs on every tick, and
        // under `--tui` four times a second on top of that.
        let mut history: HashMap<String, Vec<RunRecord>> = HashMap::new();
        for run in self.store.recent_runs(RUNS_PER_ISSUE)? {
            history.entry(run.issue_id.clone()).or_default().push(run);
        }
        // For issues with no live run: the last transcript is what a post-mortem starts from.
        let transcripts = self.store.latest_transcripts()?;
        let mut deliveries = self.delivery_views()?;

        let mut rows = Vec::new();
        for st in &states {
            // A gating issue reads as its run does — same workspace, same age, same turns —
            // with the gate's current step where the agent's last event would be. The claim is
            // still held and the worktree is still busy, which is what an operator needs to see.
            let gate = self.gating.get(&st.issue_id);
            let run = self.running.get(&st.issue_id).or(gate.map(|g| &g.run));
            let issue = self.seen.get(&st.issue_id);
            let mut progress = run.map(|r| r.handle.progress()).unwrap_or_default();
            if let Some(g) = gate {
                progress.last_event = Some(format!("gate: {}", g.handle.step()));
            }

            let runs = history.remove(&st.issue_id).unwrap_or_default();
            let worker = run
                .map(|r| r.worker.clone())
                .or_else(|| runs.first().and_then(|r| r.worker.clone()));

            rows.push(Row {
                issue_id: st.issue_id.clone(),
                identifier: st.identifier.clone(),
                title: issue.map(|i| i.title.clone()).unwrap_or_default(),
                url: issue.and_then(|i| i.url.clone()),
                tracker_state: issue.map(|i| i.state.clone()).unwrap_or_default(),
                phase: st.phase,
                attempt: st.attempt,
                turns: if run.is_some() { progress.turns } else { st.cumulative_turns },
                tokens: progress.tokens,
                age_ms: run.map(|r| now_mono.saturating_since(r.started)).unwrap_or(0),
                retry_in_ms: retries.get(&st.issue_id).map(|r| r.due_at - now_wall),
                holds_slot: retries.get(&st.issue_id).is_some_and(|r| r.reserved_state.is_some()),
                quarantined: st.is_quarantined(),
                last_error: st.last_error.clone(),
                last_event: progress.last_event,
                workspace: run.map(|r| r.workspace.display().to_string()),
                // Read back rather than derived: `st.branch` is exactly what `prepare` returned
                // at the most recent dispatch, kept in step with reality by `set_branch` calls
                // at launch and at cleanup — not recomputed from `identifier`, which can have
                // been renamed since, or from run history, which says nothing about whether the
                // ref cleanup later deleted still exists.
                branch: st.branch.clone(),
                runs,
                transcript: run
                    .and_then(|r| r.transcript.as_ref().map(|p| p.display().to_string()))
                    .or_else(|| transcripts.get(&st.issue_id).cloned()),
                delivery: deliveries.remove(&st.issue_id),
                worker,
            });
        }

        rows.sort_by(|a, b| {
            // Running first, then anything waiting, then the inert rows.
            let rank = |p: Phase| match p {
                Phase::Running => 0,
                Phase::RetryQueued => 1,
                Phase::Queued => 2,
                Phase::Quarantined => 3,
                Phase::Released => 4,
            };
            rank(a.phase).cmp(&rank(b.phase)).then_with(|| a.identifier.cmp(&b.identifier))
        });

        Ok(Snapshot {
            generated_at: now_wall,
            running: self.running.len() + self.gating.len(),
            reserved: retries.values().filter(|r| r.reserved_state.is_some()).count(),
            limit: self.capacity(),
            retrying: retries.len(),
            quarantined: states.iter().filter(|s| s.is_quarantined()).count(),
            tokens: totals.counted,
            uncounted_runs: totals.uncounted_runs,
            ticks: self.ticks,
            last_tick_at: Some(now_wall),
            last_error: self.last_error.clone(),
            rate_limit_pauses: self.published_pauses(),
            rows,
        })
    }

    fn publish(&self) -> anyhow::Result<()> {
        let snap = self.snapshot()?;
        let projected: Vec<ProjectedIssue> = snap
            .rows
            .iter()
            .map(|r| ProjectedIssue {
                issue_id: r.issue_id.clone(),
                identifier: r.identifier.clone(),
                title: r.title.clone(),
                url: r.url.clone(),
                tracker_state: r.tracker_state.clone(),
                phase: r.phase,
                attempt: r.attempt,
                cumulative_turns: r.turns,
                tokens: r.tokens,
                workspace: r.workspace.clone(),
                retry_due_at: r.retry_in_ms.map(|d| snap.generated_at + d),
                quarantined: r.quarantined,
                last_error: r.last_error.clone(),
                blocked_by: self
                    .seen
                    .get(&r.issue_id)
                    .map(|i| i.blocked_by.clone())
                    .unwrap_or_default(),
            })
            .collect();

        // Best-effort by contract: a projection failure is a lost view, never a lost tick.
        if let Err(e) = self.projector.project(&projected) {
            tracing::warn!(error = %e, "projection failed; continuing");
        }
        Ok(())
    }

    /// Operator action: clear quarantine so the issue becomes dispatchable again.
    ///
    /// Reports whether a quarantine was actually cleared. An issue that was not quarantined is
    /// a no-op and says so — the caller wants to tell an operator "there was nothing to clear"
    /// rather than an error, and rather than the silent phase reset an unguarded version would
    /// perform on a live claim (see [`Store::unquarantine`]).
    pub fn unquarantine(&self, issue_id: &str) -> anyhow::Result<bool> {
        Ok(self.store.unquarantine(self.clock.as_ref(), issue_id)?)
    }

    /// Operator action: hand a parked issue back once whatever parked it has been resolved.
    ///
    /// Reports whether a park was lifted; anything live is a no-op that says so (see
    /// [`Store::unblock`]). The next `dispatch_new` sees the issue as live and dispatches it
    /// onto its existing branch.
    ///
    /// The ticket's state is read fresh first, and a park on one that is no longer active is
    /// kept. `dispatch_new` never sees such an issue, and `sweep_parked` — the only path that
    /// reclaims a closed ticket's worktree and branch — only walks parked rows, so lifting the
    /// park there would strand both. A tracker failure is an error, not a guess either way.
    ///
    /// The same holds for every other gate `dispatch_new` applies: an issue that has lost its
    /// dispatch marker or spent its per-issue turn budget would be unparked and never
    /// dispatched, invisible to the sweep, so the park is kept and the answer says no.
    pub fn unblock(&self, issue_id: &str) -> anyhow::Result<bool> {
        let fresh = self.tracker.by_ids(&[issue_id.to_string()])?;
        let Some(issue) = fresh.iter().find(|i| i.id == issue_id) else {
            return Ok(false);
        };
        let within_budget = self
            .store
            .get(issue_id)?
            .is_some_and(|st| st.cumulative_turns < self.cfg.agent.max_turns_per_issue);
        if !self.cfg.is_active(&issue.state_key()) || !self.routable(issue) || !within_budget {
            return Ok(false);
        }
        Ok(self.store.unblock(self.clock.as_ref(), issue_id)?)
    }

    /// Stop every in-flight run before the process exits.
    ///
    /// A worker `RunHandle` outlives the `Scheduler` that spawned it unless something kills it
    /// explicitly — dropping the handle does not stop the underlying process. Without this, a
    /// real worker process (and its supervising threads) becomes orphaned the moment the
    /// orchestrator exits with runs still in flight: caught empirically, not hypothetically —
    /// the first live end-to-end run of the real worker left a `claude` process running after
    /// `cargo run` returned, because nothing had ever called `terminate` on it. No cleanup: the
    /// issue is not terminal, so the workspace is preserved for reuse on the next start, the
    /// same as `detect_stalls`. The claim is released so the issue is dispatchable again too.
    pub fn shutdown(&mut self) -> anyhow::Result<()> {
        self.terminate_all_running()
    }

    fn terminate_all_running(&mut self) -> anyhow::Result<()> {
        let ids: Vec<String> = self.running.keys().chain(self.gating.keys()).cloned().collect();
        for id in ids {
            tracing::info!(issue_id = %id, "stopping in-flight run for shutdown");
            self.terminate(&id, false)?;
            self.store.release(self.clock.as_ref(), &id)?;
        }
        Ok(())
    }
}

impl Drop for Scheduler {
    /// Last-resort safety net for exit paths that skip the explicit `shutdown()` call — an
    /// early return via `?`, a panic unwinding past the normal loop. `drop` cannot propagate a
    /// `Result`, so a failure here is logged, not surfaced; the `is_empty` guard keeps this
    /// silent on the expected path, where `shutdown()` already emptied `running`.
    fn drop(&mut self) {
        if self.running.is_empty() && self.gating.is_empty() {
            return;
        }
        tracing::warn!("scheduler dropped with runs still in flight; terminating them now");
        if let Err(e) = self.terminate_all_running() {
            tracing::warn!(error = %e, "cleanup on drop failed");
        }
    }
}
