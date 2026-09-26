//! Runtime configuration and dispatch preflight.
//!
//! TOML rather than the spec's YAML-front-matter-in-Markdown. The prompt body belongs in a
//! skill, not wedged into a config file, and a fourth config location in a Claude Code repo
//! is a cost with no payoff.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::credentials::GithubAppFile;
use crate::worker::{Effort, ModelChoice};

fn d_interval() -> u64 {
    30_000
}
fn d_max_agents() -> usize {
    10
}
fn d_turns_per_session() -> u32 {
    20
}
fn d_turns_per_issue() -> u32 {
    120
}
fn d_backoff_cap() -> u64 {
    300_000
}
fn d_stall() -> u64 {
    300_000
}
fn d_quarantine_after() -> u32 {
    3
}
fn d_miss_grace() -> u32 {
    2
}
pub use libcrew::api::{DEFAULT_API_BIND, DEFAULT_MCP_BIND};

fn d_parked_sweep() -> u64 {
    300_000
}
fn d_broker_enabled() -> bool {
    true
}
fn d_calls_per_run() -> u32 {
    20
}
fn d_calls_per_issue() -> u32 {
    100
}
fn d_transcripts_enabled() -> bool {
    true
}
fn d_max_bytes_per_run() -> u64 {
    8 * 1024 * 1024
}
fn d_keep_runs() -> usize {
    100
}
fn d_gate_enabled() -> bool {
    true
}
fn d_gate_max_failures() -> u32 {
    3
}
fn d_gate_timeout() -> u64 {
    1_800_000
}
fn d_delivery_base() -> String {
    "master".into()
}
fn d_delivery_remote() -> String {
    "origin".into()
}
fn d_summary_reviewers() -> Vec<String> {
    // The login the reviews endpoint reports for Copilot's automatic review.
    vec!["copilot-pull-request-reviewer[bot]".into()]
}
fn d_rounds_per_pr() -> u32 {
    3
}
fn d_rounds_per_issue() -> u32 {
    6
}
fn d_delivery_poll() -> u64 {
    120_000
}
fn d_ci_timeout() -> u64 {
    60 * 60 * 1000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub tracker: TrackerConfig,
    #[serde(default)]
    pub polling: PollingConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
    /// Several workers side by side, in dispatch order (#119). Empty means the one `[worker]`
    /// above with `agent.max_concurrent` slots, which is what keeps `crew.toml` unchanged; see
    /// [`Config::workers`].
    #[serde(default)]
    pub workers: Vec<WorkerConfig>,
    #[serde(default)]
    pub broker: BrokerConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub transcripts: TranscriptsConfig,
    #[serde(default)]
    pub gate: GateConfig,
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

/// The handoff gate (see [`crate::gate`]): rebase a `Done` run's branch onto `base`, then run
/// `commands` in its worktree, before the verdict is believed.
///
/// On by default with no commands, which makes the default a rebase and nothing else: the
/// rebase is what turns "green against the base it forked from" into "green against the base it
/// will merge into", and it costs nothing on a branch with no commits. The commands are the
/// repository's own bar and cannot be guessed here — a Rust crate wants `cargo test`, a
/// checked-in script wants itself — so they are the operator's to name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateConfig {
    #[serde(default = "d_gate_enabled")]
    pub enabled: bool,
    /// The ref a finished branch is rebased onto, resolved in `workspace.repo`. Unset means that
    /// repository's current HEAD — the same commit worktrees are branched from, only now.
    #[serde(default)]
    pub base: Option<String>,
    /// Each command is an argv, exec'd directly in the worktree with no shell: `["cargo",
    /// "test"]`, not `"cargo test"`. The first to exit non-zero ends the gate, and its output
    /// goes back to the agent. Empty means the rebase alone is the gate.
    #[serde(default)]
    pub commands: Vec<Vec<String>>,
    /// Consecutive gate failures before the issue parks `Blocked` instead of continuing. This
    /// is what keeps the gate from becoming a runaway of its own — an agent that cannot make
    /// the suite pass would otherwise be re-dispatched until the turn budget ran out.
    #[serde(default = "d_gate_max_failures")]
    pub max_failures: u32,
    /// How long one gate may run before it is killed and counted as a failure. Sized for a
    /// cold `cargo test`, not for a fake. `0` disables the timeout.
    #[serde(default = "d_gate_timeout")]
    pub timeout_ms: u64,
    /// Paths whose rebase conflicts go back to the agent instead of to a human (#111): when
    /// *every* conflicted path matches one of these, the verdict is `Continue`, counted against
    /// `max_failures` like a failing command. `*` matches within one path segment and `**` any
    /// number of them. Empty means every conflict is a human's, as before #111.
    #[serde(default)]
    pub agent_resolvable: Vec<String>,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            enabled: d_gate_enabled(),
            base: None,
            commands: Vec::new(),
            max_failures: d_gate_max_failures(),
            timeout_ms: d_gate_timeout(),
            agent_resolvable: Vec::new(),
        }
    }
}

/// Delivery: push, pull request, CI, review — the path from a run's `Done` to a pull request
/// an operator can merge (see [`crate::sched`]'s delivery section and [`crate::forge`]).
///
/// Off by default, on the same reasoning as `worker.kind`: it publishes branches and opens
/// pull requests under the operator's credentials, and that is a decision to make in the
/// config that names the real repository, not one to inherit by upgrading.
///
/// The two round bounds are the brakes on the loop this feature creates. A reviewer that
/// comments on every push, answered by an agent that pushes a fix, has no bound of its own;
/// each round also spends a reviewer's quota and an operator's attention, not only tokens.
/// `max_rounds_per_pr` alone is not enough — a pull request closed and reopened would start it
/// over — which is what `max_rounds_per_issue` is for, and why it never resets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The branch pull requests target unless the work is stacked on another issue's branch.
    #[serde(default = "d_delivery_base")]
    pub base: String,
    /// The git remote the branch is pushed to.
    #[serde(default = "d_delivery_remote")]
    pub remote: String,
    /// Logins to request a review from once the pull request is open. Each request is verified
    /// afterwards: a provider that accepts the request and attaches nobody is reported as a
    /// failure, not a success. Empty means no review is requested and none is waited for.
    #[serde(default)]
    pub reviewers: Vec<String>,
    /// Logins whose review *summary* is read for findings as well as their inline comments
    /// (#126): any of their reviews on the current head whose body says more than
    /// "Findings: None" is handed to an agent whole. A review in the `CHANGES_REQUESTED` state
    /// is read the same way whoever wrote it, so this names only the reviewers whose
    /// `COMMENTED` summaries count too. Defaults to the Copilot reviewer, which puts findings
    /// there that it leaves on no line.
    #[serde(default = "d_summary_reviewers")]
    pub summary_reviewers: Vec<String>,
    /// Times delivery may hand the *current pull request* back to an agent — for a red CI or
    /// for review comments — before handing it to the operator instead.
    #[serde(default = "d_rounds_per_pr")]
    pub max_rounds_per_pr: u32,
    /// The same bound over the issue's whole life. Survives a new run and a new pull request.
    #[serde(default = "d_rounds_per_issue")]
    pub max_rounds_per_issue: u32,
    /// How often an open delivery is polled for CI and review. Costs two or three provider
    /// requests per open pull request per poll, on top of the tracker's own budget.
    #[serde(default = "d_delivery_poll")]
    pub poll_interval_ms: u64,
    /// How long to wait for CI to report on a pushed head before handing off. A repository
    /// with no CI configured would otherwise wait forever, looking healthy.
    #[serde(default = "d_ci_timeout")]
    pub ci_timeout_ms: u64,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base: d_delivery_base(),
            remote: d_delivery_remote(),
            reviewers: vec![],
            summary_reviewers: d_summary_reviewers(),
            max_rounds_per_pr: d_rounds_per_pr(),
            max_rounds_per_issue: d_rounds_per_issue(),
            poll_interval_ms: d_delivery_poll(),
            ci_timeout_ms: d_ci_timeout(),
        }
    }
}

pub use libcrew::api::ApiConfig;

/// Per-run transcripts of the agent's raw event stream (see [`crate::transcript`]).
///
/// On by default, like the broker and for a related reason: what it adds is a record of what
/// already happened, so switching it off removes the evidence rather than the behaviour. The
/// bounds are the reason it can be a default at all — without them a long-lived daemon would
/// trade a disk for a debugging story.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptsConfig {
    #[serde(default = "d_transcripts_enabled")]
    pub enabled: bool,
    /// Defaults to `.transcripts` under the workspace root — see [`TranscriptsConfig::root_in`].
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Backstop against one pathological run, not a limit a normal session approaches.
    #[serde(default = "d_max_bytes_per_run")]
    pub max_bytes_per_run: u64,
    /// How many runs' transcripts to keep. Must exceed `agent.max_concurrent` to be useful,
    /// though a live run is never the one pruned even when it does not.
    #[serde(default = "d_keep_runs")]
    pub keep_runs: usize,
}

impl Default for TranscriptsConfig {
    fn default() -> Self {
        Self {
            enabled: d_transcripts_enabled(),
            root: None,
            max_bytes_per_run: d_max_bytes_per_run(),
            keep_runs: d_keep_runs(),
        }
    }
}

impl TranscriptsConfig {
    /// Where transcripts go, given the resolved workspace root.
    ///
    /// The default is a dot-directory *beside* the worktrees rather than inside one: a
    /// worktree is a git checkout the agent commits from, and it is deleted when the ticket
    /// goes terminal, which is the exact moment the transcript becomes worth reading. A leading
    /// dot cannot collide with a `worktree_key`, which always ends in `-<hash>`.
    pub fn root_in(&self, workspace_root: &Path) -> PathBuf {
        self.root.clone().unwrap_or_else(|| workspace_root.join(".transcripts"))
    }
}

/// The host-side tool broker (see [`crate::broker`]).
///
/// On by default, unlike `worker.kind`, because the blast radius is the other way round: the
/// broker's whole purpose is to give the agent a *scoped* way to do what it can already do
/// ambiently, so switching it off does not remove an authority, it removes the audited path to
/// one. An operator who wants no agent-initiated tracker writes at all wants a token without
/// write scope, not `enabled = false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerConfig {
    #[serde(default = "d_broker_enabled")]
    pub enabled: bool,
    /// Broker calls one dispatched run may make.
    #[serde(default = "d_calls_per_run")]
    pub max_calls_per_run: u32,
    /// Broker calls one issue may accumulate across all of its runs. The per-run cap does not
    /// bound the continuation loop on its own — each continuation is a fresh run with a fresh
    /// allowance — which is the same gap `max_turns_per_issue` closes for turns.
    #[serde(default = "d_calls_per_issue")]
    pub max_calls_per_issue: u32,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            enabled: d_broker_enabled(),
            max_calls_per_run: d_calls_per_run(),
            max_calls_per_issue: d_calls_per_issue(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// `fake` (default) or `claude`. Deliberately independent of `tracker.kind` — a real
    /// tracker with a fake worker is a safe way to watch real dispatch decisions without
    /// spawning real agents; making `claude` the default the moment a real tracker is
    /// configured would turn "point this at a real repo" into "start editing that repo" with
    /// no separate decision in between.
    #[serde(default)]
    pub kind: String,
    /// Only used when `kind = "claude"`. Defaults to `"claude"` — resolved via `PATH`.
    #[serde(default)]
    pub bin: Option<String>,
    /// Replaces
    /// [`DEFAULT_ENV_ALLOWLIST`](crate::worker::claude::DEFAULT_ENV_ALLOWLIST) wholesale when
    /// set — not merged with it, so an operator who names a list gets exactly that list and
    /// nothing arrives by inheritance.
    ///
    /// The reason to reach for it is usually to *add* something deliberate, like
    /// `ANTHROPIC_API_KEY` on an API-key install. Tightening it is a weaker lever than it
    /// looks: see [`crate::broker`] on why removing `HOME` does not take the agent's ambient
    /// credentials away.
    #[serde(default)]
    pub env_allowlist: Option<Vec<String>>,
    /// Passed as `--model`, an alias (`opus`) or a full name. Unset passes no flag and the agent
    /// runs on the operator's CLI default, as it did before this setting existed. Not checked
    /// against a list: the CLI is the authority on which names exist, and one it refuses fails
    /// the dispatch as [`ErrorClass::ModelNotFound`](crate::model::ErrorClass::ModelNotFound).
    #[serde(default)]
    pub model: Option<String>,
    /// Passed as `--effort`. Unset passes no flag.
    #[serde(default)]
    pub effort: Option<Effort>,
    /// `[[workers]]` only: what the run rows, the rate-limit pause and every observer call this
    /// worker. Defaults to the kind, so it needs setting only when two workers share one.
    #[serde(default)]
    pub name: Option<String>,
    /// `[[workers]]` only, and required there: this worker's own slots. The single `[worker]`
    /// takes `agent.max_concurrent` instead.
    #[serde(default)]
    pub max_concurrent: Option<usize>,
}

impl WorkerConfig {
    pub fn model_choice(&self) -> ModelChoice {
        ModelChoice { model: self.model.clone(), effort: self.effort }
    }

    /// A continuation is pinned to this name (#119), so it is the one identity a worker has.
    pub fn name(&self) -> String {
        match self.name.as_deref().map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => match self.kind.trim().to_ascii_lowercase().as_str() {
                "" => "fake".to_string(),
                k => k.to_string(),
            },
        }
    }
}

/// The worker a config selects. Parsed rather than compared as a string, so a misspelling is
/// a preflight error instead of an `else` branch that quietly runs the fake (#69).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerKind {
    Fake,
    Claude,
}

impl WorkerConfig {
    /// Empty reads as `fake`: `crew.toml` has no `[worker]` section, and a real agent must
    /// be a decision written down, never a default.
    pub fn kind(&self) -> Result<WorkerKind, ConfigError> {
        match self.kind.trim().to_ascii_lowercase().as_str() {
            "" | "fake" => Ok(WorkerKind::Fake),
            "claude" => Ok(WorkerKind::Claude),
            _ => Err(ConfigError::Invalid(format!(
                "unsupported worker.kind {:?}; expected one of \"fake\", \"claude\"",
                self.kind
            ))),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// Selects an adapter: `fake` or `github`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub active_states: Vec<String>,
    #[serde(default)]
    pub terminal_states: Vec<String>,
    #[serde(default)]
    pub required_labels: Vec<String>,
    /// `kind = "github"` only: the repository owner and name to poll. The credential comes from
    /// `github_app` when set and from `GITHUB_TOKEN` otherwise, never from this file.
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub repo: String,
    /// `kind = "github"` only: a file naming the App every write is authored by — `app_id`,
    /// `installation_id`, `private_key_path` (#64). A path to a file rather than the three keys
    /// inline, because it is the same file `crewd init` writes (#65) and it points at a key that
    /// must never be checked in beside this one.
    #[serde(default)]
    pub github_app: Option<PathBuf>,
    /// The label that marks an issue as ready for Crew. Set, carrying it is the whole signal;
    /// unset, any assignee is, as before this key existed. See `DispatchRule`.
    #[serde(default)]
    pub dispatch_label: Option<String>,
    /// An optional further narrowing to issues assigned to this login. Never required: a
    /// read-only marker account is only possible on an organization-owned repository.
    #[serde(default)]
    pub assignee: Option<String>,
}

/// The tracker a config selects; see [`WorkerKind`] for why this is parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerKind {
    Fake,
    Github,
}

impl TrackerConfig {
    pub fn kind(&self) -> Result<TrackerKind, ConfigError> {
        match self.kind.trim().to_ascii_lowercase().as_str() {
            "" => Err(ConfigError::Invalid("tracker.kind is required".into())),
            "fake" => Ok(TrackerKind::Fake),
            "github" => Ok(TrackerKind::Github),
            _ => Err(ConfigError::Invalid(format!(
                "unsupported tracker.kind {:?}; expected one of \"fake\", \"github\"",
                self.kind
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollingConfig {
    #[serde(default = "d_interval")]
    pub interval_ms: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self { interval_ms: d_interval() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// The git repository worktrees are created from. Defaults to the current directory, which
    /// is the shape dogfooding takes: crewd run from inside the repo it dispatches
    /// against.
    #[serde(default)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "d_max_agents")]
    pub max_concurrent: usize,
    /// Per-state override. Keys are normalised (trim + lowercase) on load.
    #[serde(default)]
    pub max_concurrent_by_state: std::collections::HashMap<String, usize>,
    /// Turns within one worker session. The spec stops here, which is why it runs away.
    #[serde(default = "d_turns_per_session")]
    pub max_turns_per_session: u32,
    /// Turns across *all* sessions for an issue. The global brake the spec lacks entirely.
    #[serde(default = "d_turns_per_issue")]
    pub max_turns_per_issue: u32,
    #[serde(default = "d_backoff_cap")]
    pub max_retry_backoff_ms: u64,
    #[serde(default = "d_stall")]
    pub stall_timeout_ms: u64,
    /// Consecutive identical retryable failures before quarantine.
    #[serde(default = "d_quarantine_after")]
    pub quarantine_after_identical: u32,
    /// Consecutive refresh misses before a running issue is treated as gone. The spec kills on
    /// the first miss, so one eventual-consistency blip destroys in-flight work.
    #[serde(default = "d_miss_grace")]
    pub refresh_miss_grace: u32,
    /// How often parked issues are re-read from the tracker to see whether they have since
    /// closed, so their worktrees can be reclaimed. A parked issue is one the scheduler has
    /// finished with and is no longer watching: it is out of `running`, has no retry row, and
    /// once its ticket closes the active-state poll never returns it again. Without this sweep
    /// its worktree and branch stay on disk for good. The sweep costs one `by_ids` batch per
    /// interval over every issue still parked, so this is deliberately slower than
    /// `polling.interval_ms` — a parked issue is not urgent. `0` disables the sweep.
    #[serde(default = "d_parked_sweep")]
    pub parked_sweep_interval_ms: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_concurrent: d_max_agents(),
            max_concurrent_by_state: Default::default(),
            max_turns_per_session: d_turns_per_session(),
            max_turns_per_issue: d_turns_per_issue(),
            max_retry_backoff_ms: d_backoff_cap(),
            stall_timeout_ms: d_stall(),
            quarantine_after_identical: d_quarantine_after(),
            refresh_miss_grace: d_miss_grace(),
            parked_sweep_interval_ms: d_parked_sweep(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config at {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("cannot parse config at {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    /// Parse and preflight a config, then check the host has what `tracker.github_app` names.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let cfg = Self::parse(path)?;
        cfg.check_github_app()?;
        Ok(cfg)
    }

    /// Everything [`Config::load`] checks that depends on the file alone, not on this host. The
    /// App settings file and key are the operator's (#98), so a checked-in config naming them
    /// must still parse on a machine, CI included, that has neither.
    pub fn parse(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Read { path: path.to_path_buf(), source })?;
        let mut cfg: Config = toml::from_str(&text)
            .map_err(|source| ConfigError::Parse { path: path.to_path_buf(), source })?;
        cfg.normalize();
        cfg.preflight()?;
        Ok(cfg)
    }

    /// Once, at load, and not in `preflight`: preflight runs every tick, and a key file briefly
    /// replaced on disk must not stop dispatch while the key `main` already loaded is still the
    /// one in use. A half-configured App would otherwise surface as a 401 on the first poll,
    /// naming none of the pieces actually missing. The fake tracker never reads the file.
    pub fn check_github_app(&self) -> Result<(), ConfigError> {
        let Some(path) = &self.tracker.github_app else { return Ok(()) };
        if self.tracker.kind()? != TrackerKind::Github {
            return Ok(());
        }
        GithubAppFile::load(path)
            .and_then(|file| file.load_key().map(drop))
            .map_err(|e| ConfigError::Invalid(format!("tracker.github_app: {e}")))
    }

    /// Lowercase every state used for comparison, so provider spelling never leaks into lookups.
    fn normalize(&mut self) {
        let norm = |v: &Vec<String>| -> Vec<String> {
            v.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
        };
        self.tracker.active_states = norm(&self.tracker.active_states);
        self.tracker.terminal_states = norm(&self.tracker.terminal_states);
        self.tracker.required_labels = norm(&self.tracker.required_labels);
        // The same shape the adapter gives an issue's labels, or `Agent` would never match.
        self.tracker.dispatch_label = self
            .tracker
            .dispatch_label
            .as_deref()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty());

        self.agent.max_concurrent_by_state = self
            .agent
            .max_concurrent_by_state
            .iter()
            .map(|(k, v)| (k.trim().to_lowercase(), *v))
            .filter(|(k, v)| !k.is_empty() && *v > 0)
            .collect();
    }

    /// Checks that must hold before the scheduling loop starts, and again before each dispatch.
    pub fn preflight(&self) -> Result<(), ConfigError> {
        self.check_workers()?;
        if self.tracker.kind()? == TrackerKind::Github
            && (self.tracker.owner.trim().is_empty() || self.tracker.repo.trim().is_empty())
        {
            return Err(ConfigError::Invalid(
                "tracker.owner and tracker.repo are required when tracker.kind = \"github\"".into(),
            ));
        }
        // The spec omits this, so a service with no active states polls forever, dispatches
        // nothing, logs nothing, and looks perfectly healthy.
        if self.tracker.active_states.is_empty() {
            return Err(ConfigError::Invalid("tracker.active_states must be non-empty".into()));
        }
        // With no terminal states nothing is ever terminal, so no worktree is ever reclaimed
        // and `sweep_parked` never fires — the same silent health as above.
        if self.tracker.terminal_states.is_empty() {
            return Err(ConfigError::Invalid("tracker.terminal_states must be non-empty".into()));
        }
        if self.workers.is_empty() && self.agent.max_concurrent == 0 {
            return Err(ConfigError::Invalid("agent.max_concurrent must be > 0".into()));
        }
        if self.agent.max_turns_per_session == 0 || self.agent.max_turns_per_issue == 0 {
            return Err(ConfigError::Invalid("turn budgets must be > 0".into()));
        }
        if self.polling.interval_ms == 0 {
            return Err(ConfigError::Invalid("polling.interval_ms must be > 0".into()));
        }
        // A zero cap would not mean "unlimited", it would refuse the agent's first call and
        // read as the broker being broken. Anyone wanting that wants `enabled = false`.
        if self.broker.enabled
            && (self.broker.max_calls_per_run == 0 || self.broker.max_calls_per_issue == 0)
        {
            return Err(ConfigError::Invalid(
                "broker call budgets must be > 0; use broker.enabled = false to disable tools"
                    .into(),
            ));
        }
        // Zero on either bound would not mean "unlimited", it would mean "write nothing" by a
        // route that looks like a bug at the call site rather than a decision here.
        if self.transcripts.enabled
            && (self.transcripts.max_bytes_per_run == 0 || self.transcripts.keep_runs == 0)
        {
            return Err(ConfigError::Invalid(
                "transcript bounds must be > 0; use transcripts.enabled = false to record none"
                    .into(),
            ));
        }
        // A zero here would not mean "never escalate", it would block on the first failure and
        // read as the gate being broken; an empty argv would fail every gate at spawn time.
        if self.gate.enabled {
            if self.gate.max_failures == 0 {
                return Err(ConfigError::Invalid(
                    "gate.max_failures must be > 0; use gate.enabled = false to skip the gate"
                        .into(),
                ));
            }
            // A blank program name is rejected here for the same reason an empty argv is, and it
            // is the easier one to write by accident: `[[""]]` is a non-empty command whose
            // program cannot be spawned, so it survives preflight and then fails every single
            // run. Each failure is a `Continue` that sends the agent back to work on a
            // configuration error it cannot see or fix, until `max_failures` blocks the issue.
            // Startup is the only place this is cheap to say.
            if let Some(i) = self
                .gate
                .commands
                .iter()
                .position(|c| c.first().is_none_or(|p| p.trim().is_empty()))
            {
                return Err(ConfigError::Invalid(format!(
                    "gate.commands[{i}] has no program to run; each command is an argv like \
                     [\"cargo\", \"test\"]"
                )));
            }
            // A blank pattern matches no path, so it reads as configured while handing nothing
            // back — the operator would learn it only from a conflict parked for them anyway.
            if let Some(i) = self.gate.agent_resolvable.iter().position(|p| p.trim().is_empty()) {
                return Err(ConfigError::Invalid(format!(
                    "gate.agent_resolvable[{i}] is blank; each entry is a path or a glob like \
                     \"docs/**\""
                )));
            }
        }
        // A zero round bound would refuse the first fix round and read as delivery being
        // broken; an empty base or remote would push nowhere. Same shape as the broker check.
        if self.delivery.enabled {
            if self.delivery.max_rounds_per_pr == 0 || self.delivery.max_rounds_per_issue == 0 {
                return Err(ConfigError::Invalid(
                    "delivery round bounds must be > 0; use delivery.enabled = false to \
                     deliver nothing"
                        .into(),
                ));
            }
            if self.delivery.base.trim().is_empty() || self.delivery.remote.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "delivery.base and delivery.remote are required".into(),
                ));
            }
            if self.delivery.poll_interval_ms == 0 || self.delivery.ci_timeout_ms == 0 {
                return Err(ConfigError::Invalid("delivery intervals must be > 0".into()));
            }
        }
        // Overlap makes startup cleanup delete a workspace for an issue about to be dispatched.
        let overlap: Vec<_> = self
            .tracker
            .active_states
            .iter()
            .filter(|s| self.tracker.terminal_states.contains(s))
            .cloned()
            .collect();
        if !overlap.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "states appear in both active and terminal: {}",
                overlap.join(", ")
            )));
        }
        Ok(())
    }

    pub fn is_active(&self, state_key: &str) -> bool {
        self.tracker.active_states.iter().any(|s| s == state_key)
    }

    pub fn is_terminal(&self, state_key: &str) -> bool {
        self.tracker.terminal_states.iter().any(|s| s == state_key)
    }

    /// Every state the operator has named, active or terminal. The broker accepts exactly
    /// these for `set_state`, so an agent cannot move its ticket somewhere the scheduler has
    /// no rule for.
    pub fn known_states(&self) -> Vec<String> {
        let mut v = self.tracker.active_states.clone();
        v.extend(self.tracker.terminal_states.iter().cloned());
        v.dedup();
        v
    }

    pub fn state_limit(&self, state_key: &str) -> usize {
        self.agent
            .max_concurrent_by_state
            .get(state_key)
            .copied()
            .unwrap_or_else(|| self.capacity())
    }

    /// Every worker in dispatch order, each with its name and `max_concurrent` resolved. A config
    /// with no `[[workers]]` is the one-element list of `[worker]` at `agent.max_concurrent`.
    pub fn workers(&self) -> Vec<WorkerConfig> {
        if self.workers.is_empty() {
            let mut w = self.worker.clone();
            w.max_concurrent = Some(self.agent.max_concurrent);
            w.name = Some(w.name());
            return vec![w];
        }
        self.workers.iter().map(|w| WorkerConfig { name: Some(w.name()), ..w.clone() }).collect()
    }

    /// Global capacity: the sum of every worker's slots.
    pub fn capacity(&self) -> usize {
        self.workers().iter().filter_map(|w| w.max_concurrent).sum()
    }

    fn check_workers(&self) -> Result<(), ConfigError> {
        // Two sources for one list would leave the operator guessing which one ran. Any field
        // counts, not only `kind`: `[worker] model = ...` beside `[[workers]]` would otherwise be
        // dropped without a word.
        if !self.workers.is_empty() && self.worker != WorkerConfig::default() {
            return Err(ConfigError::Invalid(
                "set either [worker] or [[workers]], not both".into(),
            ));
        }
        let mut names = std::collections::HashSet::new();
        for w in self.workers() {
            w.kind()?;
            let name = w.name();
            // A pause and a pinned session are both keyed by name, so a shared one would pause
            // the wrong worker and resume a session on a provider that never held it.
            if !names.insert(name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "two workers are named {name:?}; give each [[workers]] entry its own name"
                )));
            }
            if !self.workers.is_empty() && w.max_concurrent.unwrap_or(0) == 0 {
                return Err(ConfigError::Invalid(format!(
                    "worker {name:?} needs max_concurrent > 0"
                )));
            }
            // A blank model would reach the child as `--model ""`, which the CLI refuses on every
            // attempt — one quarantine per issue for a typo that belongs here.
            if w.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
                return Err(ConfigError::Invalid(
                    "worker.model must not be blank; leave it unset for the CLI default".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        let mut c = Config {
            tracker: TrackerConfig {
                kind: "fake".into(),
                active_states: vec!["In Progress".into()],
                terminal_states: vec!["Done".into()],
                required_labels: vec![],
                owner: String::new(),
                repo: String::new(),
                ..Default::default()
            },
            polling: Default::default(),
            workspace: Default::default(),
            agent: Default::default(),
            worker: Default::default(),
            workers: Default::default(),
            broker: Default::default(),
            api: Default::default(),
            transcripts: Default::default(),
            gate: Default::default(),
            delivery: Default::default(),
        };
        c.normalize();
        c
    }

    /// #64: each half-configured App names its missing piece instead of reaching the first poll
    /// as a 401.
    #[test]
    fn a_half_configured_github_app_is_refused_naming_the_missing_piece() {
        let dir = std::env::temp_dir().join(format!("crew-cfg-app-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let app = dir.join("github-app.toml");
        let mut c = base();
        c.tracker.kind = "github".into();
        c.tracker.owner = "o".into();
        c.tracker.repo = "r".into();
        let refused = |c: &Config| match c.check_github_app() {
            Err(ConfigError::Invalid(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        };

        c.tracker.github_app = Some(dir.join("absent.toml"));
        assert!(refused(&c).contains("cannot read"), "{}", refused(&c));

        c.tracker.github_app = Some(app.clone());
        std::fs::write(&app, "installation_id = 2\nprivate_key_path = \"k.pem\"").unwrap();
        assert!(refused(&c).ends_with("has no app_id"), "{}", refused(&c));
        std::fs::write(&app, "app_id = 1\nprivate_key_path = \"k.pem\"").unwrap();
        assert!(refused(&c).ends_with("has no installation_id"), "{}", refused(&c));
        std::fs::write(&app, "app_id = 1\ninstallation_id = 2").unwrap();
        assert!(refused(&c).ends_with("has no private_key_path"), "{}", refused(&c));

        std::fs::write(&app, "app_id = 1\ninstallation_id = 2\nprivate_key_path = \"k.pem\"")
            .unwrap();
        assert!(refused(&c).contains("not readable"), "{}", refused(&c));
        std::fs::write(dir.join("k.pem"), "garbage").unwrap();
        assert!(refused(&c).contains("not an RSA private key"), "{}", refused(&c));

        crate::credentials::tests::throwaway_key(&dir);
        std::fs::rename(dir.join("app.pem"), dir.join("k.pem")).unwrap();
        c.check_github_app().expect("a complete App passes");
        c.preflight().expect("and the per-tick preflight never reads the file");
        std::fs::remove_file(dir.join("k.pem")).unwrap();
        c.preflight().expect("a key replaced after load does not stop dispatch");
        c.tracker.kind = "fake".into();
        c.check_github_app().expect("the fake tracker never reads the App file");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dispatch_label_is_normalized_like_the_labels_it_is_compared_with() {
        let mut c = base();
        c.tracker.dispatch_label = Some(" Agent ".into());
        c.normalize();
        assert_eq!(c.tracker.dispatch_label.as_deref(), Some("agent"));
        c.tracker.dispatch_label = Some("  ".into());
        c.normalize();
        assert_eq!(c.tracker.dispatch_label, None, "blank is unset, not a label nothing carries");
    }

    #[test]
    fn a_gate_that_could_never_escalate_or_never_start_is_rejected() {
        let mut c = base();
        c.gate.max_failures = 0;
        assert!(c.preflight().is_err());
        c.gate.max_failures = 3;
        c.gate.commands = vec![vec!["cargo".into(), "test".into()], vec![]];
        assert!(c.preflight().is_err(), "an empty argv cannot be exec'd");
        // A command with a blank program is the same defect wearing a non-empty argv, and it is
        // the one an operator writes by accident. Caught here it costs a startup error; missed,
        // it costs every run of every issue until `max_failures` blocks each one.
        c.gate.commands = vec![vec![String::new()]];
        assert!(c.preflight().is_err(), "a blank program name cannot be exec'd either");
        c.gate.commands = vec![vec!["   ".into(), "test".into()]];
        assert!(c.preflight().is_err(), "nor can a program name that is only whitespace");
        c.gate.commands = vec![vec!["cargo".into(), "test".into()]];
        assert!(c.preflight().is_ok());
        // Turning the gate off is the supported way to skip it.
        c.gate.enabled = false;
        c.gate.max_failures = 0;
        assert!(c.preflight().is_ok());
    }

    #[test]
    fn a_blank_agent_resolvable_pattern_is_rejected() {
        let mut c = base();
        c.gate.agent_resolvable = vec!["CLAUDE.md".into(), "docs/**".into()];
        assert!(c.preflight().is_ok());
        c.gate.agent_resolvable.push("  ".into());
        let err = c.preflight().unwrap_err().to_string();
        assert!(err.contains("gate.agent_resolvable[2]"), "names the entry: {err}");
    }

    #[test]
    fn a_zero_delivery_round_bound_is_rejected_rather_than_refusing_every_fix() {
        let mut c = base();
        c.delivery.enabled = true;
        assert!(c.preflight().is_ok());
        c.delivery.max_rounds_per_issue = 0;
        assert!(c.preflight().is_err());
        c.delivery.max_rounds_per_issue = 6;
        c.delivery.max_rounds_per_pr = 0;
        assert!(c.preflight().is_err());
        // Off, the bounds are not consulted, so a stale zero cannot stop dispatch.
        c.delivery.enabled = false;
        assert!(c.preflight().is_ok());
    }

    #[test]
    fn a_config_written_before_delivery_existed_still_loads() {
        // The finding on PR #30, made structural: a new top-level section must default.
        let text = "[tracker]\nkind = \"fake\"\nactive_states = [\"open\"]\n";
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(!cfg.delivery.enabled);
        assert_eq!(cfg.delivery.base, "master");
    }

    #[test]
    fn a_model_setting_the_cli_would_silently_ignore_is_refused_at_load() {
        let with = |worker: &str| {
            toml::from_str::<Config>(&format!(
                "[tracker]\nkind = \"fake\"\nactive_states = [\"open\"]\nterminal_states = [\"closed\"]\n[worker]\n{worker}"
            ))
        };
        // The CLI answers an unknown `--effort` with a stderr warning and the default effort,
        // so a typo that got this far would run on a level the run row does not name.
        assert!(with("effort = \"meduim\"").is_err());
        let cfg = with("model = \"claude-opus-5-5\"\neffort = \"xhigh\"").unwrap();
        assert_eq!(cfg.worker.model_choice().effort, Some(Effort::Xhigh));
        assert!(cfg.preflight().is_ok());

        let unset = with("").unwrap();
        assert_eq!(unset.worker.model_choice(), ModelChoice::default());

        let mut blank = unset;
        blank.worker.model = Some("  ".into());
        assert!(blank.preflight().is_err(), "a blank name would reach the child as --model ''");
    }

    /// #119: a config with no `[[workers]]` is the one-element list of `[worker]` at
    /// `agent.max_concurrent`, which is what lets every scheduler test and `crew.toml` run
    /// unchanged.
    #[test]
    fn a_single_worker_config_behaves_exactly_as_before() {
        let mut c = base();
        c.agent.max_concurrent = 3;
        let ws = c.workers();
        assert_eq!(ws.len(), 1);
        assert_eq!((ws[0].name(), ws[0].max_concurrent), ("fake".to_string(), Some(3)));
        assert_eq!(c.capacity(), 3);
        assert_eq!(c.state_limit("anything"), 3);
        assert!(c.preflight().is_ok());
    }

    #[test]
    fn workers_are_listed_in_dispatch_order_and_capacity_is_their_sum() {
        let text = "[tracker]\nkind = \"fake\"\nactive_states = [\"open\"]\n\
                    terminal_states = [\"closed\"]\n\
                    [[workers]]\nkind = \"claude\"\nmax_concurrent = 1\neffort = \"high\"\n\
                    [[workers]]\nkind = \"fake\"\nname = \"grok\"\nmax_concurrent = 2\n";
        let c: Config = toml::from_str(text).unwrap();
        assert!(c.preflight().is_ok());
        let names: Vec<_> = c.workers().iter().map(|w| w.name()).collect();
        assert_eq!(names, ["claude", "grok"]);
        assert_eq!(c.capacity(), 3);
    }

    /// A pause and a pinned session are keyed by worker name, so two sources for the list, a
    /// shared name or a worker with no slots are each refused at load.
    #[test]
    fn an_ambiguous_worker_list_is_refused() {
        let pool = |name: &str, max: usize| WorkerConfig {
            kind: "fake".into(),
            name: Some(name.into()),
            max_concurrent: Some(max),
            ..Default::default()
        };
        let mut both = base();
        both.worker.kind = "claude".into();
        both.workers = vec![pool("a", 1)];
        assert!(both.preflight().is_err(), "[worker] and [[workers]] together");
        both.worker = WorkerConfig { model: Some("opus".into()), ..Default::default() };
        assert!(both.preflight().is_err(), "a [worker] with only a model is still a [worker]");

        let mut shared = base();
        shared.workers = vec![pool("a", 1), pool("a", 1)];
        assert!(shared.preflight().is_err(), "two workers named alike");

        let mut empty = base();
        empty.workers = vec![pool("a", 0)];
        assert!(empty.preflight().is_err(), "a worker with no slots");
    }

    #[test]
    fn states_are_normalized_for_comparison() {
        let c = base();
        assert!(c.is_active("in progress"));
        assert!(c.is_terminal("done"));
        assert!(!c.is_active("Done"));
    }

    #[test]
    fn empty_active_states_is_rejected() {
        let mut c = base();
        c.tracker.active_states.clear();
        assert!(c.preflight().is_err());
    }

    #[test]
    fn empty_terminal_states_is_rejected() {
        let mut c = base();
        c.tracker.terminal_states.clear();
        assert!(c.preflight().is_err());
    }

    #[test]
    fn a_misspelled_tracker_kind_is_rejected_rather_than_running_the_demo() {
        let mut c = base();
        c.tracker.kind = "gihub".into();
        insta::assert_snapshot!("misspelled_tracker_kind", c.preflight().unwrap_err());
    }

    #[test]
    fn a_misspelled_worker_kind_is_rejected_rather_than_running_the_fake() {
        let mut c = base();
        c.worker.kind = "cluade".into();
        insta::assert_snapshot!("misspelled_worker_kind", c.preflight().unwrap_err());
    }

    #[test]
    fn supported_kinds_are_accepted_in_any_case_and_an_empty_worker_kind_is_fake() {
        let mut c = base();
        c.tracker.owner = "o".into();
        c.tracker.repo = "r".into();
        for (kind, want) in [
            ("fake", TrackerKind::Fake),
            (" GitHub ", TrackerKind::Github),
            ("FAKE", TrackerKind::Fake),
        ] {
            c.tracker.kind = kind.into();
            assert_eq!(c.tracker.kind().unwrap(), want);
            assert!(c.preflight().is_ok());
        }
        for (kind, want) in [
            ("", WorkerKind::Fake),
            ("Fake", WorkerKind::Fake),
            ("claude", WorkerKind::Claude),
            (" CLAUDE", WorkerKind::Claude),
        ] {
            c.worker.kind = kind.into();
            assert_eq!(c.worker.kind().unwrap(), want);
            assert!(c.preflight().is_ok());
        }
    }

    #[test]
    fn the_checked_in_configs_load() {
        for name in ["crew.toml", "crew.github.toml"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
            if let Err(e) = Config::parse(&path) {
                panic!("{name}: {e}");
            }
        }
        // The dogfooding config writes as the App by default (#98); its settings file is the
        // host's, so only `load` checks it.
        let github = Config::parse(&Path::new(env!("CARGO_MANIFEST_DIR")).join("crew.github.toml"));
        assert!(github.unwrap().tracker.github_app.is_some());
    }

    #[test]
    fn overlapping_active_and_terminal_states_are_rejected() {
        let mut c = base();
        c.tracker.terminal_states.push("in progress".into());
        assert!(c.preflight().is_err());
    }

    #[test]
    fn a_github_tracker_without_owner_and_repo_is_rejected() {
        let mut c = base();
        c.tracker.kind = "github".into();
        assert!(c.preflight().is_err());
        c.tracker.owner = "o".into();
        assert!(c.preflight().is_err(), "repo is still missing");
        c.tracker.repo = "r".into();
        assert!(c.preflight().is_ok());
    }

    #[test]
    fn zero_transcript_bounds_are_rejected_rather_than_silently_recording_nothing() {
        let mut c = base();
        c.transcripts.keep_runs = 0;
        assert!(c.preflight().is_err());
        c.transcripts.keep_runs = 10;
        c.transcripts.max_bytes_per_run = 0;
        assert!(c.preflight().is_err());
        // Turning the feature off is the supported way to record nothing.
        c.transcripts.enabled = false;
        assert!(c.preflight().is_ok());
    }

    #[test]
    fn transcripts_default_beside_the_worktrees_never_inside_one() {
        let c = base();
        let root = c.transcripts.root_in(Path::new("/tmp/ws"));
        assert_eq!(root, Path::new("/tmp/ws/.transcripts"));
        // A worktree_key can never start with a dot followed by letters — it is a sanitised
        // identifier plus `-<hash>` — so the default cannot shadow one.
        assert!(root.file_name().unwrap().to_str().unwrap().starts_with('.'));
    }

    #[test]
    fn per_state_limit_falls_back_to_global_and_drops_invalid_entries() {
        let mut c = base();
        c.agent.max_concurrent = 7;
        c.agent.max_concurrent_by_state.insert("  In Progress ".into(), 2);
        c.agent.max_concurrent_by_state.insert("In Review".into(), 0); // invalid, dropped
        c.normalize();
        assert_eq!(c.state_limit("in progress"), 2);
        assert_eq!(c.state_limit("in review"), 7);
        assert_eq!(c.state_limit("anything"), 7);
    }

    /// The client reads `[api] bind` leniently (`libcrew::client`); this is the half of that
    /// contract only the daemon side can check.
    #[test]
    fn a_config_the_daemon_would_reject_still_yields_an_address() {
        // `Config::load` refuses this — no `tracker.kind`, no `active_states`. Preflight gates
        // dispatch, and a client asking what is running has no stake in it; failing here would
        // make an unrelated typo look like the daemon being unreachable.
        let cfg = std::env::temp_dir()
            .join(format!("crew-config-unloadable-{}.toml", std::process::id()));
        std::fs::write(&cfg, "[tracker]\nkind = \"\"\n[api]\nbind = \"127.0.0.1:9100\"\n").unwrap();
        assert!(Config::load(&cfg).is_err(), "the daemon must still reject this");

        let e = libcrew::client::endpoint(None, &cfg);
        assert_eq!(e.addr, "127.0.0.1:9100");
        std::fs::remove_file(&cfg).ok();
    }
}
