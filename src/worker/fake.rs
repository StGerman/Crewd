//! Simulated worker.
//!
//! Progress is a pure function of elapsed clock time, so a test advances [`FakeClock`] and the
//! simulation advances with it — no threads, no sleeps, no flake. Under `SystemClock` the same
//! code animates the dashboard in real time.
//!
//! [`FakeClock`]: crate::clock::FakeClock

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::{
    KillResult, ModelChoice, Progress, RateLimitSignal, RunHandle, Session, Spawn, TokenUsage,
    ToolEndpoint, Worker,
};
use crate::clock::{Clock, Mono};
use crate::model::{ErrorClass, Feedback, Outcome, ReviewVerdict};
use crate::workspace::WipSnapshot;

#[derive(Debug, Clone)]
pub struct Script {
    pub duration_ms: u64,
    pub turns: u32,
    /// Reported once the run completes on its own, the way the real worker only learns its
    /// totals from the CLI's terminal event. A run that stalls or is killed never reports them.
    pub tokens: TokenUsage,
    pub outcome: Outcome,
    /// Stop emitting progress at this point while never finishing — a stalled agent.
    pub silent_after_ms: Option<u64>,
    /// Review verdicts reported at completion, the way the real worker parses them off the
    /// final message. Empty by default: most scripted runs were never handed a review.
    pub verdicts: Vec<ReviewVerdict>,
    /// Reported at completion like `verdicts`, the way the real worker's `rate_limit()` only
    /// ever answers once the process has exited. `None` by default: most scripted runs were
    /// never handed a rate limit.
    pub rate_limit: Option<RateLimitSignal>,
}

impl Script {
    pub fn succeeds_in(ms: u64) -> Self {
        Self {
            duration_ms: ms,
            turns: 3,
            tokens: TokenUsage { input: 1_200, output: 750 },
            outcome: Outcome::Done,
            silent_after_ms: None,
            verdicts: vec![],
            rate_limit: None,
        }
    }

    pub fn with_verdicts(mut self, v: Vec<ReviewVerdict>) -> Self {
        self.verdicts = v;
        self
    }

    pub fn with_outcome(mut self, o: Outcome) -> Self {
        self.outcome = o;
        self
    }

    /// Simulates the CLI's own account-wide rate limit rejecting this run (#37). Orthogonal to
    /// `outcome`, the same as the real worker: the CLI still reports its ordinary verdict — set
    /// `with_outcome` alongside this the way a real rejection reads as `Failed`.
    pub fn with_rate_limit(mut self, sig: RateLimitSignal) -> Self {
        self.rate_limit = Some(sig);
        self
    }

    pub fn stalls_after(ms: u64) -> Self {
        Self {
            duration_ms: u64::MAX, // never completes on its own
            turns: 2,
            tokens: TokenUsage { input: 600, output: 240 },
            outcome: Outcome::Failed { class: ErrorClass::Stall, msg: "no output".into() },
            silent_after_ms: Some(ms),
            verdicts: vec![],
            rate_limit: None,
        }
    }
}

impl Default for Script {
    fn default() -> Self {
        Self::succeeds_in(4_000)
    }
}

pub struct FakeWorker {
    clock: Arc<dyn Clock>,
    scripts: Mutex<HashMap<String, Script>>,
    default_script: Mutex<Script>,
    /// Every session each issue was spawned under, in order. Whether a continuation resumed the
    /// conversation or started a new one is invisible from outside the worker otherwise, and it
    /// is exactly what the scheduler tests need to assert.
    sessions: Mutex<HashMap<String, Vec<Session>>>,
    /// Whether each spawn was handed a broker endpoint. The scheduler decides this — a broker
    /// that failed to open a session hands `None` — so it is the scheduler's tests that need
    /// to see it, and nothing else in the run reveals it.
    endpoints: Mutex<HashMap<String, Vec<Option<ToolEndpoint>>>>,
    /// What delivery told each spawn about the previous run's output. The scheduler decides
    /// this, and whether a red CI or a review actually reached the next run is exactly what
    /// its tests need to see.
    feedback: Mutex<HashMap<String, Vec<Vec<Feedback>>>>,
    /// The work-in-progress snapshot each spawn was told about, for the same reason.
    wips: Mutex<HashMap<String, Vec<Vec<WipSnapshot>>>>,
    body_changed: Mutex<HashMap<String, Vec<bool>>>,
    /// Mutable so a test can change it between dispatches, which is how a config change looks
    /// to the scheduler across a restart.
    model: Mutex<ModelChoice>,
}

impl FakeWorker {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            scripts: Mutex::new(HashMap::new()),
            default_script: Mutex::new(Script::default()),
            sessions: Mutex::new(HashMap::new()),
            endpoints: Mutex::new(HashMap::new()),
            feedback: Mutex::new(HashMap::new()),
            wips: Mutex::new(HashMap::new()),
            body_changed: Mutex::new(HashMap::new()),
            model: Mutex::new(ModelChoice::default()),
        }
    }

    /// Script a specific issue. Later spawns for the same issue reuse it unless replaced.
    pub fn script(&self, issue_id: &str, s: Script) {
        self.scripts.lock().unwrap().insert(issue_id.to_string(), s);
    }

    pub fn set_model(&self, m: ModelChoice) {
        *self.model.lock().unwrap() = m;
    }

    pub fn set_default(&self, s: Script) {
        *self.default_script.lock().unwrap() = s;
    }

    /// The sessions this issue has been spawned under, oldest first.
    pub fn sessions_for(&self, issue_id: &str) -> Vec<Session> {
        self.sessions.lock().unwrap().get(issue_id).cloned().unwrap_or_default()
    }

    /// The broker endpoint each spawn for this issue was handed, oldest first.
    pub fn endpoints_for(&self, issue_id: &str) -> Vec<Option<ToolEndpoint>> {
        self.endpoints.lock().unwrap().get(issue_id).cloned().unwrap_or_default()
    }

    /// The delivery feedback each spawn for this issue was handed, oldest first.
    pub fn feedback_for(&self, issue_id: &str) -> Vec<Vec<Feedback>> {
        self.feedback.lock().unwrap().get(issue_id).cloned().unwrap_or_default()
    }

    /// The work-in-progress snapshot each spawn for this issue was handed, oldest first.
    pub fn wips_for(&self, issue_id: &str) -> Vec<Vec<WipSnapshot>> {
        self.wips.lock().unwrap().get(issue_id).cloned().unwrap_or_default()
    }

    /// Whether each spawn for this issue was told its body changed since the session saw it.
    pub fn body_changed_for(&self, issue_id: &str) -> Vec<bool> {
        self.body_changed.lock().unwrap().get(issue_id).cloned().unwrap_or_default()
    }
}

impl Worker for FakeWorker {
    fn spawn(&self, req: Spawn<'_>) -> Arc<dyn RunHandle> {
        let Spawn { issue, session, tools, transcript, feedback, wip, body_changed, .. } = req;
        self.sessions.lock().unwrap().entry(issue.id.clone()).or_default().push(session.clone());
        self.endpoints.lock().unwrap().entry(issue.id.clone()).or_default().push(tools.cloned());
        self.feedback.lock().unwrap().entry(issue.id.clone()).or_default().push(feedback.to_vec());
        self.wips.lock().unwrap().entry(issue.id.clone()).or_default().push(wip.to_vec());
        self.body_changed.lock().unwrap().entry(issue.id.clone()).or_default().push(body_changed);

        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(&issue.id)
            .cloned()
            .unwrap_or_else(|| self.default_script.lock().unwrap().clone());

        // The fake honours the transcript contract at a coarser grain: it has no reader thread
        // to stream from, so it writes the whole scripted run up front. That is enough to keep
        // the seam exercised by the scheduler's own tests, which is the point — a transcript
        // only the real worker ever produces is a transcript the scheduler can silently stop
        // wiring up.
        if let Some(mut t) = transcript {
            t.write_line(
                &serde_json::json!({
                    "type": "crew_run_start",
                    "issue": issue.identifier,
                    "issue_id": issue.id,
                    "session": session.id(),
                    "resumed": session.is_resume(),
                    "tools": tools.is_some(),
                })
                .to_string(),
            );
            for turn in 1..=script.turns {
                t.write_line(
                    &serde_json::json!({
                        "type": "assistant",
                        "message": {
                            "content": [{"type": "text", "text": format!("fake turn {turn}")}],
                        },
                    })
                    .to_string(),
                );
            }
            t.write_line(
                &serde_json::json!({
                    "type": "crew_run_end",
                    "scripted_outcome": script.outcome.label(),
                    // Totals arrive once, on the terminal event, the way the real CLI reports
                    // them — a fixture that put them on every turn would model the very
                    // double-count the nullable columns exist to undo.
                    "usage": {
                        "input_tokens": script.tokens.input,
                        "output_tokens": script.tokens.output,
                    },
                })
                .to_string(),
            );
        }

        Arc::new(FakeRun {
            clock: Arc::clone(&self.clock),
            started: self.clock.mono(),
            script,
            killed: AtomicBool::new(false),
            forced: AtomicBool::new(false),
        })
    }

    fn model(&self) -> ModelChoice {
        self.model.lock().unwrap().clone()
    }
}

struct FakeRun {
    clock: Arc<dyn Clock>,
    started: Mono,
    script: Script,
    killed: AtomicBool,
    forced: AtomicBool,
}

impl FakeRun {
    /// Elapsed time the simulation can observe. Past `silent_after_ms` the clock keeps moving
    /// but the run stops noticing, which is exactly what a stalled agent looks like from outside.
    fn visible_elapsed(&self) -> u64 {
        let raw = self.clock.mono().saturating_since(self.started);
        match self.script.silent_after_ms {
            Some(cut) => raw.min(cut),
            None => raw,
        }
    }

    /// Ran to its scripted end under its own power. A killed run is finished too, but the way
    /// a real one is finished when SIGTERM lands: with no terminal event and no totals.
    fn completed(&self) -> bool {
        !self.killed.load(Ordering::SeqCst)
            && self.clock.mono().saturating_since(self.started) >= self.script.duration_ms
    }
}

impl RunHandle for FakeRun {
    fn progress(&self) -> Progress {
        let elapsed = self.visible_elapsed();
        let turns = if self.script.duration_ms == u64::MAX {
            // Stalling script: ramp turns over the silent window, then hold.
            let window = self.script.silent_after_ms.unwrap_or(1).max(1);
            ((elapsed * self.script.turns as u64) / window).min(self.script.turns as u64) as u32
        } else {
            let d = self.script.duration_ms.max(1);
            (((elapsed * self.script.turns as u64) / d) + 1).min(self.script.turns as u64) as u32
        };

        Progress {
            turns,
            events: turns as u64,
            tokens: self.completed().then_some(self.script.tokens),
            last_event: Some(if turns == 0 { "starting" } else { "turn_completed" }.into()),
        }
    }

    /// Reported the way the real worker reports them: only once the run has completed under
    /// its own power. A killed run's verdicts, like its totals, never arrive.
    fn verdicts(&self) -> Vec<ReviewVerdict> {
        if self.completed() { self.script.verdicts.clone() } else { Vec::new() }
    }

    /// Reported the same way as `verdicts`: only once the run has completed under its own
    /// power, mirroring the real worker's stream-driven signal.
    fn rate_limit(&self) -> Option<RateLimitSignal> {
        if self.completed() { self.script.rate_limit.clone() } else { None }
    }

    fn finished(&self) -> Option<Outcome> {
        if self.killed.load(Ordering::SeqCst) {
            return Some(Outcome::Failed {
                class: ErrorClass::AgentCrash,
                msg: if self.forced.load(Ordering::SeqCst) {
                    "terminated (forced)".into()
                } else {
                    "terminated".into()
                },
            });
        }
        let elapsed = self.clock.mono().saturating_since(self.started);
        (elapsed >= self.script.duration_ms).then(|| self.script.outcome.clone())
    }

    fn kill(&self, _grace_ms: u64) -> KillResult {
        if self.finished().is_some() && !self.killed.load(Ordering::SeqCst) {
            return KillResult::AlreadyDone;
        }
        if self.killed.swap(true, Ordering::SeqCst) {
            return KillResult::AlreadyDone;
        }
        // A stalling run ignores the polite request, mirroring a wedged process.
        if self.script.silent_after_ms.is_some() {
            self.forced.store(true, Ordering::SeqCst);
            KillResult::Forced
        } else {
            KillResult::Stopped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::clock::FakeClock;
    use crate::model::Issue;

    fn issue() -> Issue {
        Issue {
            id: "iss-1".into(),
            identifier: "MT-1".into(),
            title: "t".into(),
            body: None,
            state: "In Progress".into(),
            priority: Some(1),
            url: None,
            labels: vec![],
            dispatchable: true,
            created_at: None,
            native_ref: None,
            blocked_by: vec![],
        }
    }

    #[test]
    fn a_run_finishes_only_once_the_clock_reaches_its_duration() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        let h = w.spawn(Spawn::new(&issue(), Path::new("/tmp"), 0, &Session::New("s-1".into())));

        assert!(h.finished().is_none());
        c.advance_ms(3_999);
        assert!(h.finished().is_none());
        c.advance_ms(1);
        assert_eq!(h.finished(), Some(Outcome::Done));
    }

    #[test]
    fn progress_accumulates_turns_as_time_passes_and_totals_appear_only_at_completion() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        let h = w.spawn(Spawn::new(&issue(), Path::new("/tmp"), 0, &Session::New("s-1".into())));

        let p0 = h.progress();
        c.advance_ms(3_999);
        let p1 = h.progress();
        assert!(p1.turns > p0.turns);
        assert_eq!(p1.tokens, None, "in flight: nothing authoritative to report yet");

        c.advance_ms(1);
        assert_eq!(h.progress().tokens, Some(TokenUsage { input: 1_200, output: 750 }));
    }

    #[test]
    fn a_killed_run_reports_no_token_total() {
        // Mirrors the real worker: SIGTERM lands before the CLI's result event, so there is no
        // total, and the fake must not hand the scheduler one the real thing never would.
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        let h = w.spawn(Spawn::new(&issue(), Path::new("/tmp"), 0, &Session::New("s-1".into())));

        c.advance_ms(2_000);
        assert_eq!(h.kill(1_000), KillResult::Stopped);
        c.advance_ms(10_000);
        assert_eq!(h.progress().tokens, None, "even once the scripted duration has elapsed");
    }

    #[test]
    fn a_stalling_run_freezes_its_progress_but_never_finishes() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        w.script("iss-1", Script::stalls_after(1_000));
        let h = w.spawn(Spawn::new(&issue(), Path::new("/tmp"), 0, &Session::New("s-1".into())));

        c.advance_ms(1_000);
        let frozen = h.progress();
        c.advance_ms(600_000);
        assert_eq!(h.progress(), frozen, "a stalled run must stop reporting progress");
        assert!(h.finished().is_none(), "and must never complete on its own");
    }

    #[test]
    fn killing_a_wedged_run_reports_that_force_was_needed() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        w.script("iss-1", Script::stalls_after(1_000));
        let h = w.spawn(Spawn::new(&issue(), Path::new("/tmp"), 0, &Session::New("s-1".into())));

        c.advance_ms(5_000);
        assert_eq!(h.kill(1_000), KillResult::Forced);
        assert!(matches!(h.finished(), Some(Outcome::Failed { .. })));
    }

    #[test]
    fn killing_an_already_finished_run_is_a_no_op() {
        let c = Arc::new(FakeClock::new());
        let w = FakeWorker::new(c.clone());
        let h = w.spawn(Spawn::new(&issue(), Path::new("/tmp"), 0, &Session::New("s-1".into())));
        c.advance_ms(4_000);
        assert_eq!(h.kill(1_000), KillResult::AlreadyDone);
        assert_eq!(h.finished(), Some(Outcome::Done), "verdict must not be rewritten");
    }
}
