//! Worker execution.
//!
//! A worker runs one attempt for one issue and reports an explicit [`Outcome`]. The spec infers
//! "maybe continue" from a clean process exit and re-dispatches on a 1s timer, which is how it
//! runs away; here the verdict is data, and only `Continue` earns another dispatch.

pub mod claude;
pub mod fake;
pub mod grok;
pub(crate) mod prompt;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{Feedback, Issue, Outcome, ReviewVerdict};
use crate::transcript::TranscriptWriter;
use crate::workspace::WipSnapshot;

/// Where this run's host-side tool broker is, when there is one.
///
/// Carries a path rather than a URL because the token inside it is a secret: a `--mcp-config`
/// file (mode 0600, written and owned by the broker) keeps it out of `argv`, where any process
/// on the host could read it out of `ps`. The worker never parses this — it hands the path to
/// the CLI and names the tools in its prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEndpoint {
    /// MCP server name. Tools reach the agent as `mcp__<server>__<tool>`.
    pub server: String,
    /// A `--mcp-config` file carrying this run's own endpoint and bearer token.
    pub config_path: PathBuf,
    /// Tool names, for the prompt. The agent will not use a tool it was not told about.
    pub tools: Vec<String>,
}

impl ToolEndpoint {
    /// The name the agent actually sees, which is not the bare tool name.
    pub fn qualified(&self, tool: &str) -> String {
        format!("mcp__{}__{}", self.server, tool)
    }
}

/// Progress reported while a run is in flight. Drives stall detection and the dashboard.
///
/// Two of these fields answer different questions and must not be merged back into one.
/// `events` is a liveness signal: it exists so that this struct compares unequal between two
/// scheduler ticks whenever the child did anything at all, which is exactly what
/// `detect_stalls` checks. `tokens` is a cost figure, and it is absent until the run's terminal
/// `result` event supplies one. The first version of this type had per-event token counters
/// serving both jobs, and they served the second badly (see [`TokenUsage`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    pub turns: u32,
    /// Stream events observed so far, of every type — a tool result counts as much as a turn.
    ///
    /// The bound on what this can prove: events arrive when the CLI *emits* one, and a tool
    /// result is emitted when the tool returns. A single command that runs longer than
    /// `stall_timeout_ms` therefore still reads as a stall, because that interval genuinely
    /// produces no output. What this fixes is the narrower case of an agent that is emitting
    /// steadily — tool results, system events — without producing `assistant` turns. The long
    /// single build is handled by sizing `stall_timeout_ms` above it, not by this counter.
    pub events: u64,
    /// The run's token totals, once it has reported them. `None` while the run is in flight and
    /// `None` forever for a run that ended without a `result` event: killed, crashed, or cut off
    /// by the session turn budget. An honest absence, not a zero.
    pub tokens: Option<TokenUsage>,
    pub last_event: Option<String>,
}

pub use libcrew::TokenUsage;

/// Which conversation an attempt runs in.
///
/// The scheduler names it before the process exists, the same ordering as claim-before-spawn:
/// a worker that reported its own id back afterwards would leave a window in which a run that
/// died early could never be resumed, because nothing outside it ever learned the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// Start a fresh conversation under this id.
    New(String),
    /// Continue the conversation already recorded under this id.
    Resume(String),
}

impl Session {
    pub fn id(&self) -> &str {
        match self {
            Self::New(id) | Self::Resume(id) => id,
        }
    }

    pub fn is_resume(&self) -> bool {
        matches!(self, Self::Resume(_))
    }
}

/// What a run's stream reported about an account-wide rate limit, read off a `rate_limit_event`
/// whose `status` is `"rejected"` (#37), or a `five_hour` `"allowed_warning"` past
/// `agent.rate_limit_warn_utilization` (#184). Orthogonal to [`Outcome`]: the CLI still reports its
/// ordinary verdict for a run cut short this way — typically [`Outcome::Failed`], since the
/// process exits with no explicit marker — and this is the separate signal that tells the
/// scheduler *why*, so it can treat the interruption as account-wide rather than as this issue's
/// own failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitSignal {
    /// Whatever the CLI named the window that rejected the request — `"five_hour"`,
    /// `"seven_day"`, or a name this crate has never seen. Kept as the raw string rather than
    /// parsed into an enum, so an unrecognised window still carries its own `resets_at` instead
    /// of being dropped as unrecognised.
    pub kind: String,
    /// Unix seconds the window resets at, when the event carried one that parsed as a number.
    /// `None` is the scheduler's cue to fall back to ordinary backoff rather than pause
    /// dispatch — the same degrade a `resets_at` already in the past gets, since a clock the
    /// host disagrees with must not be able to stop dispatch permanently.
    pub resets_at: Option<i64>,
}

/// The model and effort a worker passes its agent, and what the scheduler records on each run
/// it starts (#36). `None` in either field means no flag is passed and the agent runs on
/// whatever the operator's own CLI defaults to — which is also what gets recorded, as an
/// absence, rather than a guess at what that default was.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub model: Option<String>,
    pub effort: Option<Effort>,
}

/// `--effort`'s levels, as `claude --help` lists them. An enum rather than a string because the
/// CLI answers an unknown level with a warning on stderr and the default effort — a silent
/// fallback that would make the recorded value a lie — so a typo has to fail at config load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// A run in flight. Dropping the handle does not stop the work — call [`RunHandle::kill`].
pub trait RunHandle: Send + Sync {
    fn progress(&self) -> Progress;
    /// True once the run has produced a verdict.
    fn finished(&self) -> Option<Outcome>;
    /// The review verdicts the run reported, once it has finished. Empty for a run that gave
    /// none — which is every run not handed review feedback, and also a run that was handed
    /// it and said nothing, which the scheduler treats as "still outstanding" rather than as
    /// settled.
    fn verdicts(&self) -> Vec<ReviewVerdict> {
        Vec::new()
    }
    /// Set once the run's stream reported a rejected, account-wide rate limit. Defaulted to
    /// `None` rather than required alongside `finished()`: only [`crate::worker::claude`] has
    /// shown this event. The grok 1.0.41 probe did not emit one, so [`crate::worker::grok`]
    /// is correct reporting nothing until a rejection is recorded.
    fn rate_limit(&self) -> Option<RateLimitSignal> {
        None
    }
    /// Set, while the run is still going, once its stream reported a `five_hour` warning at or
    /// past `agent.rate_limit_warn_utilization` (#184): the window is about to close, so the scheduler stops
    /// *new* dispatch on this worker and lets this run finish. Answered mid-run, unlike
    /// `rate_limit`, because a warning read only at exit would pause nothing earlier than the
    /// rejection it is meant to get ahead of.
    fn rate_limit_warning(&self) -> Option<RateLimitSignal> {
        None
    }
    /// Request termination and wait, bounded, for the run to actually stop.
    ///
    /// Must not return until the run is confirmed stopped — the caller deletes the workspace
    /// next, and the spec's failure to order these is how a live agent gets its directory
    /// removed mid-write.
    fn kill(&self, grace_ms: u64) -> KillResult;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillResult {
    /// Stopped within the grace period.
    Stopped,
    /// Did not stop in time and was forced. Still safe to clean up after.
    Forced,
    /// Already finished before the request.
    AlreadyDone,
}

/// Everything one attempt is spawned with.
///
/// A struct rather than positional arguments because four of these are adjacent `Option`s that a
/// call site could swap without a type error. Build it with [`Spawn::new`] and set what applies.
pub struct Spawn<'a> {
    pub issue: &'a Issue,
    pub workspace: &'a Path,
    pub attempt: u32,
    pub session: &'a Session,
    /// `None` when the broker is unavailable. That is a degrade, not an error: the run proceeds
    /// without tracker tools rather than failing, so a broker that cannot bind costs the agent a
    /// capability and nothing else.
    pub tools: Option<&'a ToolEndpoint>,
    /// Owned rather than borrowed because the implementation that matters hands it to a reader
    /// thread that outlives the call; `None` means transcripts are off or the file could not be
    /// opened, and carries the same degrade-never-fail contract as `tools`. An implementation
    /// writes to it and never reads it back — where it points is already known to the
    /// scheduler, which is what records the path.
    pub transcript: Option<TranscriptWriter>,
    /// What the orchestrator knows about why this attempt exists that the agent cannot see from
    /// inside its worktree — the handoff gate's failing output, a red CI, review comments — in
    /// the order the agent should act on them, and empty on a first dispatch. More than one when
    /// a gate verdict interrupted a delivery hand-back, which rides along after it rather than
    /// costing a second round (#160). The worker renders it into the prompt; the scheduler does
    /// not, because the prompt's wording and the verdict marker the worker parses back are one
    /// convention and live in one module. It reaches the agent through the prompt and nothing
    /// else, so a worker that ignores it is degraded, not wrong.
    pub feedback: &'a [Feedback],
    /// Uncommitted work earlier runs of this issue left behind when their worktrees were
    /// removed, oldest first. Separate from `feedback` because the two are independent — a
    /// gate-sent continuation can also have a snapshot — and, like it, reaches the agent only
    /// through the prompt: the worktree it is handed is clean, and applying a snapshot is the
    /// agent's call.
    pub wip: &'a [WipSnapshot],
    /// On a resumed session, whether the issue body differs from the one the session was last
    /// handed (#109). The continuation prompt otherwise leaves the body out, as already held.
    pub body_changed: bool,
}

impl<'a> Spawn<'a> {
    pub fn new(issue: &'a Issue, workspace: &'a Path, attempt: u32, session: &'a Session) -> Self {
        Self {
            issue,
            workspace,
            attempt,
            session,
            tools: None,
            transcript: None,
            feedback: &[],
            wip: &[],
            body_changed: false,
        }
    }
}

pub trait Worker: Send + Sync {
    fn spawn(&self, req: Spawn<'_>) -> Arc<dyn RunHandle>;

    /// What every [`Worker::spawn`] on this worker passes the agent. The scheduler records it on
    /// the run row before spawning, so asking the worker — not the config — is what keeps the
    /// record equal to what the child was actually given. Defaulted for a worker that has no
    /// model to choose.
    fn model(&self) -> ModelChoice {
        ModelChoice::default()
    }

    /// Whether `launch` should open a broker session for this worker. Grok is not handed
    /// tracker tools (#120); the default keeps Claude's path unchanged.
    fn uses_tools(&self) -> bool {
        true
    }

    /// The last message this worker's agent wrote in a run, read back from that run's
    /// transcript, for the brief another worker is handed when it takes the issue over (#165).
    /// Each CLI streams its own shape, so only the worker that wrote a transcript can read it.
    /// Defaulted to `None`, which costs the brief its excerpt and nothing else.
    fn last_text(&self, _transcript: &str) -> Option<String> {
        None
    }
}
