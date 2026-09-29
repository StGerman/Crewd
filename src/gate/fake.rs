//! Simulated gate, for the scheduler tests.
//!
//! A verdict is a pure function of elapsed fake-clock time, the same way [`FakeWorker`]'s
//! progress is: a test advances the clock and the gate "finishes". Every `start` is recorded
//! with the worktree it was given, because whether the scheduler gated a run at all — and on
//! which directory — is invisible from outside otherwise, and it is exactly what the guard
//! test for issue #21 has to see.
//!
//! [`FakeWorker`]: crate::worker::fake::FakeWorker

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::{Gate, GateHandle, Verdict};
use crate::clock::{Clock, Mono};
use crate::model::Issue;
use crate::worker::KillResult;

#[derive(Debug, Clone)]
pub struct GateScript {
    pub duration_ms: u64,
    pub verdict: Verdict,
}

impl GateScript {
    pub fn passes_in(ms: u64) -> Self {
        Self { duration_ms: ms, verdict: Verdict::Passed { base_updated: true } }
    }

    pub fn with_verdict(mut self, v: Verdict) -> Self {
        self.verdict = v;
        self
    }

    /// Never finishes on its own — a `cargo test` that hangs.
    pub fn hangs() -> Self {
        Self { duration_ms: u64::MAX, verdict: Verdict::Passed { base_updated: false } }
    }
}

impl Default for GateScript {
    fn default() -> Self {
        Self::passes_in(1_000)
    }
}

pub struct FakeGate {
    clock: Arc<dyn Clock>,
    scripts: Mutex<HashMap<String, GateScript>>,
    default_script: Mutex<GateScript>,
    /// Every workspace each issue was gated in, in order.
    starts: Mutex<HashMap<String, Vec<PathBuf>>>,
}

impl FakeGate {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            scripts: Mutex::new(HashMap::new()),
            default_script: Mutex::new(GateScript::default()),
            starts: Mutex::new(HashMap::new()),
        }
    }

    /// Script a specific issue. Later gates for the same issue reuse it unless replaced.
    pub fn script(&self, issue_id: &str, s: GateScript) {
        self.scripts.lock().unwrap().insert(issue_id.to_string(), s);
    }

    pub fn set_default(&self, s: GateScript) {
        *self.default_script.lock().unwrap() = s;
    }

    /// The worktrees this issue's gates were started in, oldest first. Empty means the
    /// scheduler never gated it.
    pub fn starts_for(&self, issue_id: &str) -> Vec<PathBuf> {
        self.starts.lock().unwrap().get(issue_id).cloned().unwrap_or_default()
    }
}

impl Gate for FakeGate {
    fn start(&self, issue: &Issue, workspace: &Path) -> Arc<dyn GateHandle> {
        self.starts
            .lock()
            .unwrap()
            .entry(issue.id.clone())
            .or_default()
            .push(workspace.to_path_buf());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(&issue.id)
            .cloned()
            .unwrap_or_else(|| self.default_script.lock().unwrap().clone());
        Arc::new(FakeGateRun {
            clock: Arc::clone(&self.clock),
            started: self.clock.mono(),
            script,
            killed: AtomicBool::new(false),
        })
    }
}

struct FakeGateRun {
    clock: Arc<dyn Clock>,
    started: Mono,
    script: GateScript,
    killed: AtomicBool,
}

impl GateHandle for FakeGateRun {
    fn finished(&self) -> Option<Verdict> {
        if self.killed.load(Ordering::SeqCst) {
            return Some(Verdict::Failed {
                step: "fake".into(),
                output: "stopped by the orchestrator".into(),
                on_base: false,
            });
        }
        let elapsed = self.clock.mono().saturating_since(self.started);
        (elapsed >= self.script.duration_ms).then(|| self.script.verdict.clone())
    }

    fn step(&self) -> String {
        "fake gate".into()
    }

    fn kill(&self, _grace_ms: u64) -> KillResult {
        if self.finished().is_some() {
            return KillResult::AlreadyDone;
        }
        self.killed.store(true, Ordering::SeqCst);
        KillResult::Stopped
    }
}
