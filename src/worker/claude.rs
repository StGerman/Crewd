//! `Worker` over `claude -p --output-format stream-json`.
//!
//! ## Invocation
//!
//! `-p --output-format stream-json --verbose --permission-mode bypassPermissions`, prompt
//! piped on stdin — never as an argv element, so there is no argv length limit and no shell
//! quoting to get wrong. `--permission-mode bypassPermissions` because there is no human on
//! the other end of a permission prompt in a headless dispatch; a prompt nobody answers just
//! hangs the run until the stall timeout kills it anyway, which is strictly worse than the
//! bypass.
//!
//! `--model` and `--effort` follow when [`ModelChoice`] sets them, and nothing when it does not,
//! so an unset `worker.model` is the command line from before it existed. `--fallback-model` is
//! never passed: a run the CLI moved to another model would contradict its own run row (#36).
//!
//! Three things confirmed against a real install (`claude 2.1.268`, and the session handling
//! below against `2.1.278`) rather than assumed, because guessing them wrong would have meant a
//! worker that silently never worked:
//!
//! * **There is no `--max-turns` flag.** [`AgentConfig::max_turns_per_session`]'s per-session
//!   turn budget is enforced by this module instead — it counts `assistant` events as they
//!   stream and sends `SIGTERM` once the count reaches the budget, reporting
//!   [`Outcome::Continue`] itself rather than waiting for the CLI to enforce a limit it does
//!   not have.
//! * **`--bare` requires `ANTHROPIC_API_KEY`.** An operator authenticated via OAuth (the
//!   default interactive login, and what this project's own dev machine uses) has no such key,
//!   and `--bare` fails outright without one. This worker does not pass `--bare`; the agent's
//!   configuration below says what it passes instead.
//! * **`--session-id <uuid>` names a conversation and `--resume <id>` continues it**, both
//!   working with the prompt on stdin and with `-p`. That is what lets a continuation pick up
//!   where the last one stopped instead of re-reading the issue from scratch. Three details
//!   decided the shape of [`Session`]: a resumed session is *not* scoped to the directory it
//!   was created in, so a worktree moving underneath it is survivable; an id the CLI no longer
//!   holds is answered with `No conversation found with session ID` and a `result` event
//!   carrying `is_error: true` and no turns at all, rather than a hang — which is the signal
//!   the scheduler degrades on; and `--resume` must always be passed *with* an id, because
//!   bare it opens an interactive picker and there is no human here to answer it.
//!
//! [`AgentConfig::max_turns_per_session`]: crate::config::AgentConfig::max_turns_per_session
//! [`Session`]: crate::worker::Session
//!
//! ## Tracker tools
//!
//! When the scheduler has a broker session for the run, this worker adds `--mcp-config <path>`
//! pointing at the file the broker wrote, and names the resulting tools in the prompt — an
//! agent does not use a tool nobody told it about. The flag goes *last* on the command line on
//! purpose: the CLI declares `--mcp-config <configs...>` as variadic, so anything non-flag
//! following it is swallowed as a second config path. (Found the direct way: passing the prompt
//! as an argument after it made the CLI try to open the prompt text as a file.) The prompt goes
//! on stdin regardless, so nothing needs to follow it.
//!
//! ## The agent's configuration
//!
//! A dispatched agent's Claude Code configuration comes from the repository and the broker,
//! and nothing from the operator's account (#191):
//!
//! * `--setting-sources project` loads the worktree's `.claude/settings.json` only; `local` is
//!   left out because a worktree has no `settings.local.json` of its own.
//! * `--strict-mcp-config` loads MCP servers only from `--mcp-config`, which drops user, local,
//!   plugin and claude.ai servers alike — and the repository's `.mcp.json` too, on purpose:
//!   #192 replaces that rust-analyzer bridge.
//! * `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` is applied after the allowlist. Auto memory survives
//!   both flags, and an allowlisted operator value of that name would turn it back on.
//!
//! None of this closes the keychain hole (#135).
//!
//! ## The outcome convention
//!
//! `claude -p`'s own terminal `result` event gives exactly one structural verdict: `is_error`.
//! That is enough to distinguish [`Outcome::Done`] from [`Outcome::Failed`], but this project
//! also needs [`Outcome::Continue`] (real progress, wants another turn) and
//! [`Outcome::Blocked`] (stuck, wants a human) — verdicts the CLI has no concept of. The only
//! channel available is the agent's own final text, so the prompt this worker sends asks the
//! agent to end that text with:
//!
//! ```text
//! CREW_OUTCOME: continue: <reason>
//! CREW_OUTCOME: blocked: <reason>
//! ```
//!
//! Absence of either line, with `is_error: false`, means `Done`. This is a text convention and
//! therefore soft — an agent that forgets the marker just reads as `Done`, which is the safe
//! default. A malformed line, a crash mid-stream, or a process that exits without ever
//! producing a `result` event all fall through to `Failed { class: AgentCrash }`: absent an
//! explicit verdict, nothing here infers `Continue` from a clean exit, which is the exact
//! spec defect this whole project exists to not have.
//!
//! ## The transcript
//!
//! [`run_reader`] parses a handful of things out of the stream and drops the rest. Everything
//! it drops — `system`, every tool call the agent made — is what a post-mortem actually wants,
//! so the same loop copies each line verbatim to this run's [`TranscriptWriter`] *before*
//! deciding whether the parser has a use for it. Lines that fail to parse are written too: a
//! stream the parser choked on is the single most interesting one to still have afterwards. See
//! [`crate::transcript`] for the retention bounds and for why the file does not live in the
//! worktree.
//!
//! `rate_limit_event` is the one exception: a rejected one is not a per-run detail but the
//! scheduler's cue that the whole account is throttled, so [`parse_rate_limit_event`] reads it
//! here rather than leaving it for a post-mortem (#37). A `five_hour` warning past
//! `agent.rate_limit_warn_utilization` is read the same way, so dispatch can pause before the
//! rejection arrives (#184). It is still copied to the transcript like every other line.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::{
    KillResult, ModelChoice, Progress, RateLimitSignal, RunHandle, Session, Spawn, Worker,
};
use crate::model::{ErrorClass, Outcome, ReviewVerdict};
use crate::transcript::TranscriptWriter;

use super::prompt::{
    build_continuation_prompt, build_prompt, extract_marker, extract_text, extract_usage,
    extract_verdicts, parse_rate_limit_event, parse_rate_limit_warning, truncate,
};

/// Env vars passed through to the child, explicitly — never inherit-and-scrub. Notably absent:
/// any tracker credential and any API key. An operator on API-key auth adds `ANTHROPIC_API_KEY`
/// to their own allowlist deliberately; it is not here by default.
///
/// `HOME` is here, and dropping it was considered and rejected on evidence rather than taste.
/// The worry was that it hands the agent ambient credentials — `gh`'s token under
/// `~/.config/gh`, git's credential helper via `~/.gitconfig`. It does not, because on macOS
/// those credentials are not under `$HOME` at all: `gh auth token` succeeds with `HOME` unset,
/// reading the login keychain, which is keyed to the user session. Removing `HOME` would cost
/// the agent its git identity and its own config while closing off nothing. See
/// [`crate::broker`]'s module doc for what that means for the broker's security story — the
/// short version is that the broker is a sanctioned, audited write path, not a sandbox.
///
/// Overridable via [`WorkerConfig::env_allowlist`](crate::config::WorkerConfig::env_allowlist)
/// for an operator who wants a tighter environment anyway.
pub const DEFAULT_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "TZ",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

/// Bounded by design: the last thing read from a crashing or malicious child should not become
/// an unbounded log line or error message.
const MAX_CAPTURED_BYTES: usize = 4096;

pub struct ClaudeWorker {
    bin: PathBuf,
    env_allowlist: Vec<String>,
    max_turns_per_session: u32,
    rate_limit_warn_utilization: f64,
    model: ModelChoice,
    #[cfg(test)]
    parent_env: Option<Vec<(String, String)>>,
}

impl ClaudeWorker {
    pub fn new(
        bin: impl Into<PathBuf>,
        env_allowlist: Vec<String>,
        max_turns_per_session: u32,
    ) -> Self {
        Self {
            bin: bin.into(),
            env_allowlist,
            max_turns_per_session,
            rate_limit_warn_utilization: crate::config::d_rate_limit_warn_utilization(),
            model: ModelChoice::default(),
            #[cfg(test)]
            parent_env: None,
        }
    }

    /// `agent.rate_limit_warn_utilization`: the `five_hour` utilization whose warning the run
    /// reports through [`RunHandle::rate_limit_warning`] (#184).
    pub fn with_rate_limit_warn_utilization(mut self, threshold: f64) -> Self {
        self.rate_limit_warn_utilization = threshold;
        self
    }

    /// Unset fields pass no flag, which is exactly the behaviour before `worker.model` existed.
    pub fn with_model(mut self, model: ModelChoice) -> Self {
        self.model = model;
        self
    }

    /// When set, [`Worker::spawn`] reads this instead of the process environment, so a test
    /// can supply a conflicting value without a process-global write (#191).
    #[cfg(test)]
    fn with_parent_env(mut self, parent: &[(&str, &str)]) -> Self {
        self.parent_env =
            Some(parent.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect());
        self
    }

    fn parent_value(&self, key: &str) -> Option<String> {
        #[cfg(test)]
        if let Some(parent) = &self.parent_env {
            return parent.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        }
        std::env::var(key).ok()
    }
}

/// Allowlisted parent variables, then auto memory forced off.
///
/// `Command` keeps the last value written for a key. The forced `1` has to come after the
/// allowlist copy: an operator value of `CLAUDE_CODE_DISABLE_AUTO_MEMORY` on the allowlist
/// would otherwise turn auto memory back on (#191).
fn apply_child_env(
    cmd: &mut Command,
    allowlist: &[String],
    lookup: impl Fn(&str) -> Option<String>,
) {
    cmd.envs(allowlist.iter().filter_map(|k| lookup(k).map(|v| (k.clone(), v))));
    cmd.env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
}

#[derive(Default)]
struct Inner {
    progress: Progress,
    outcome: Option<Outcome>,
    verdicts: Vec<ReviewVerdict>,
    /// Set when the stream carried a rejected `rate_limit_event` — see [`parse_rate_limit_event`].
    /// Independent of `outcome`: the CLI still reports its own verdict (ordinarily `Failed`,
    /// since the process exits with no explicit marker) alongside this.
    rate_limit: Option<RateLimitSignal>,
    /// The latest `five_hour` warning past the worker's threshold — see
    /// [`parse_rate_limit_warning`].
    rate_limit_warning: Option<RateLimitSignal>,
    /// Set only once the child has been reaped (`Child::wait` returned). `kill` must not
    /// return before this is true — the caller deletes the workspace next.
    reaped: bool,
}

type SharedState = Arc<(Mutex<Inner>, Condvar)>;

struct ClaudeRun {
    state: SharedState,
    pid: i32,
    /// Guards against sending SIGTERM twice on a double `kill()` call; does not by itself mean
    /// the run is finished — `state.reaped` is the source of truth for that.
    sent_term: AtomicBool,
}

impl ClaudeRun {
    fn already_finished(&self) -> Option<Outcome> {
        self.state.0.lock().unwrap().outcome.clone()
    }
}

impl RunHandle for ClaudeRun {
    fn progress(&self) -> Progress {
        self.state.0.lock().unwrap().progress.clone()
    }

    fn finished(&self) -> Option<Outcome> {
        self.state.0.lock().unwrap().outcome.clone()
    }

    fn verdicts(&self) -> Vec<ReviewVerdict> {
        self.state.0.lock().unwrap().verdicts.clone()
    }

    fn rate_limit(&self) -> Option<RateLimitSignal> {
        self.state.0.lock().unwrap().rate_limit.clone()
    }

    fn rate_limit_warning(&self) -> Option<RateLimitSignal> {
        self.state.0.lock().unwrap().rate_limit_warning.clone()
    }

    fn kill(&self, grace_ms: u64) -> KillResult {
        let (lock, cvar) = &*self.state;
        let already_finished = self.already_finished().is_some();

        {
            let g = lock.lock().unwrap();
            if g.reaped {
                return KillResult::AlreadyDone;
            }
        }

        // A run that already has a verdict is exiting on its own; do not signal it, just wait
        // for the reader thread to reap it. One still in flight gets SIGTERM, once.
        if !already_finished && !self.sent_term.swap(true, Ordering::SeqCst) {
            let _ = kill(Pid::from_raw(-self.pid), Signal::SIGTERM);
        }

        let g = lock.lock().unwrap();
        let (g, timeout) =
            cvar.wait_timeout_while(g, Duration::from_millis(grace_ms), |g| !g.reaped).unwrap();
        if !timeout.timed_out() {
            return if already_finished { KillResult::AlreadyDone } else { KillResult::Stopped };
        }
        drop(g);

        // Still not reaped after the grace period: escalate. SIGKILL cannot be caught, ignored
        // or blocked, so the reader thread's `wait()` should return promptly; this second wait
        // is a bound against something having gone very wrong, not an expected path.
        let _ = kill(Pid::from_raw(-self.pid), Signal::SIGKILL);
        let g = lock.lock().unwrap();
        let _ = cvar.wait_timeout_while(g, Duration::from_secs(5), |g| !g.reaped).unwrap();
        KillResult::Forced
    }
}

impl Worker for ClaudeWorker {
    fn spawn(&self, req: Spawn<'_>) -> Arc<dyn RunHandle> {
        let Spawn {
            issue,
            workspace,
            attempt,
            session,
            tools,
            mut transcript,
            feedback,
            wip,
            body_changed,
        } = req;
        // `--resume` is passed with an explicit id, never bare: bare opens an interactive
        // picker, and there is no human here to answer it.
        let (prompt, flag) = match session {
            Session::New(_) => (build_prompt(issue, tools, feedback, wip), "--session-id"),
            Session::Resume(_) => {
                (build_continuation_prompt(issue, tools, feedback, wip, body_changed), "--resume")
            }
        };

        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(workspace).env_clear();
        apply_child_env(&mut cmd, &self.env_allowlist, |k| self.parent_value(k));
        cmd.args([
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "bypassPermissions",
            "--setting-sources",
            "project",
            "--strict-mcp-config",
        ])
        .args([flag, session.id()]);

        // Passed on `--resume` as well, so a continuation runs on the model its run row records
        // whether or not the CLI would have carried the session's model over on its own.
        if let Some(m) = &self.model.model {
            cmd.args(["--model", m]);
        }
        if let Some(e) = self.model.effort {
            cmd.args(["--effort", e.as_str()]);
        }

        // Last, and nothing non-flag may follow it: `--mcp-config` is variadic.
        if let Some(t) = tools {
            cmd.args([std::ffi::OsStr::new("--mcp-config"), t.config_path.as_os_str()]);
        }

        // A header, so the file explains itself without a second lookup into the store. These
        // are the dispatch facts that vary per attempt; the prompt itself is omitted because it
        // is derived from the issue and would otherwise dominate the transcript of a short run.
        if let Some(t) = transcript.as_mut() {
            t.write_line(
                &serde_json::json!({
                    "type": "crew_run_start",
                    "issue": issue.identifier,
                    "issue_id": issue.id,
                    "attempt": attempt,
                    "session": session.id(),
                    "resumed": session.is_resume(),
                    "workspace": workspace.display().to_string(),
                    "tools": tools.is_some(),
                    "model": self.model.model,
                    "effort": self.model.effort,
                })
                .to_string(),
            );
        }

        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        // A new process group, rooted at this child, so `kill` can signal every descendant
        // `claude` spawns — a wedged grandchild would otherwise hold the worktree open after
        // the parent is gone.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                // The one failure that never reaches `run_reader`, so it has to say so here —
                // a transcript that stops after the header looks like a hung agent rather than
                // a binary that was never there.
                if let Some(t) = transcript.as_mut() {
                    t.write_line(
                        &serde_json::json!({
                            "type": "crew_run_end",
                            "exit": "spawn failed",
                            "stderr": e.to_string(),
                        })
                        .to_string(),
                    );
                }
                let state: SharedState = Arc::new((
                    Mutex::new(Inner {
                        outcome: Some(Outcome::Failed {
                            class: ErrorClass::AgentNotFound,
                            msg: format!("spawning {}: {e}", self.bin.display()),
                        }),
                        reaped: true,
                        ..Default::default()
                    }),
                    Condvar::new(),
                ));
                return Arc::new(ClaudeRun { state, pid: 0, sent_term: AtomicBool::new(true) });
            }
        };

        let pid = child.id() as i32;
        let mut stdin = child.stdin.take().expect("piped at spawn");
        // Best-effort: a write failure here means the child is already gone or refusing input,
        // which the reader thread will observe directly and turn into a Failed verdict.
        let _ = stdin.write_all(prompt.as_bytes());
        drop(stdin);

        let stdout = child.stdout.take().expect("piped at spawn");
        let stderr = child.stderr.take().expect("piped at spawn");

        let state: SharedState = Arc::new((Mutex::new(Inner::default()), Condvar::new()));
        let limits = ReaderLimits {
            max_turns_per_session: self.max_turns_per_session,
            rate_limit_warn_utilization: self.rate_limit_warn_utilization,
        };

        {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                run_reader(child, stdout, stderr, state, pid, limits, transcript)
            });
        }

        Arc::new(ClaudeRun { state, pid, sent_term: AtomicBool::new(false) })
    }

    fn model(&self) -> ModelChoice {
        self.model.clone()
    }

    fn last_text(&self, transcript: &str) -> Option<String> {
        last_assistant_text(transcript)
    }
}

/// Every text block of the last `assistant` event in a `stream-json` transcript that has any.
/// Read from the end, because the transcript of a long run is mostly tool traffic before it.
pub(crate) fn last_assistant_text(transcript: &str) -> Option<String> {
    transcript.lines().rev().find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        if v.get("type")?.as_str()? != "assistant" {
            return None;
        }
        let blocks = v.pointer("/message/content")?.as_array()?;
        let text: Vec<&str> =
            blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect();
        (!text.is_empty()).then(|| text.join("\n"))
    })
}

/// The worker's settings the reader applies to the stream, as one argument.
#[derive(Clone, Copy)]
struct ReaderLimits {
    max_turns_per_session: u32,
    rate_limit_warn_utilization: f64,
}

/// Runs on its own thread for the life of one attempt. Reads stdout to its end, copies every
/// line to the transcript, updates shared progress as events arrive, drains stderr on a second
/// thread so a chatty child cannot deadlock on a full pipe, then reaps the process. The verdict
/// — the last `result`'s (#214), the budget's, or a crash — is published only after `wait`
/// returns: a verdict published on `result` lets the scheduler reuse the worktree while the
/// child is still in it (#169).
fn run_reader(
    mut child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    state: SharedState,
    pid: i32,
    limits: ReaderLimits,
    mut transcript: Option<TranscriptWriter>,
) {
    let max_turns_per_session = limits.max_turns_per_session;
    let stderr_thread = std::thread::spawn(move || drain_capped(stderr));

    let mut turns = 0u32;
    let mut saw_any_line = false;
    let mut saw_valid_line = false;
    // The CLI's own classification of a failed request, which it puts on the synthetic
    // `assistant` event and not on `result` — the only place an unknown `--model` is named.
    let mut api_error: Option<String> = None;
    // The last `result`'s verdict, held until the child is reaped, like the budget's `Continue`.
    // `harvest_finished` treats a published outcome as the run being over (#169). Not the
    // first: a resumed session can emit an empty `result` before the turn it was resumed for
    // (#214), so the stream is read to its end and each `result` replaces the one before.
    let mut terminal: Option<Outcome> = None;
    // Set at the budget, applied only after `wait`. Publishing `Continue` earlier lets the
    // scheduler reuse the worktree while this process is still in it.
    let mut budget_hit = false;

    for line in BufReader::new(stdout).lines() {
        let Ok(raw) = line else { break };
        // Before the parse and before the trim: a line this module cannot read is exactly the
        // one someone will want to look at later, and so is the whitespace it arrived with.
        if let Some(t) = transcript.as_mut() {
            t.write_line(&raw);
        }
        if budget_hit {
            continue;
        }

        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        saw_any_line = true;

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                // Malformed JSON on a line is logged and skipped, not fatal — only a stream
                // that is entirely unparseable ends in Failed.
                tracing::warn!(error = %e, line, "malformed stream-json line; skipping");
                continue;
            }
        };
        saw_valid_line = true;

        // Every parsed event, whatever its type, is evidence the child is alive. Bumping this
        // before the type dispatch is what lets a tool result or a rate-limit notice count as
        // progress to `detect_stalls` — an agent an hour into a long `cargo test` is working,
        // not stalled, and its only output in that hour is `user` events carrying tool results.
        state.0.lock().unwrap().progress.events += 1;

        match value.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                // A failed request's synthetic turn is not a turn: counted, it could trip the
                // session budget below and read as `Continue` before the error `result` that
                // follows it is ever seen — a refused model retried instead of quarantined.
                if let Some(e) = value.get("error").and_then(|e| e.as_str()) {
                    api_error = Some(e.to_string());
                    continue;
                }
                turns += 1;
                let last_event = extract_text(&value);
                let mut g = state.0.lock().unwrap();
                g.progress.turns = turns;
                if let Some(t) = last_event {
                    g.progress.last_event = Some(t);
                }
                // A turn after a `result` makes that `result` provisional (#214). If this turn
                // ends in a crash or a budget cut instead of another `result`, the earlier
                // verdict, totals and review verdicts must not stand in for it.
                if terminal.take().is_some() {
                    g.progress.tokens = None;
                    g.verdicts.clear();
                }
                drop(g);

                if max_turns_per_session > 0 && turns >= max_turns_per_session {
                    // Do not publish `Continue` yet, and do not stop reading. The verdict
                    // waits until the child is reaped; the bytes after this turn are discarded
                    // below so a late `result` cannot invent a token total and the pipe cannot fill.
                    budget_hit = true;
                    let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
                }
            }
            Some("rate_limit_event") => {
                if let Some(sig) = parse_rate_limit_event(&value) {
                    state.0.lock().unwrap().rate_limit = Some(sig);
                } else if let Some(sig) =
                    parse_rate_limit_warning(&value, limits.rate_limit_warn_utilization)
                {
                    state.0.lock().unwrap().rate_limit_warning = Some(sig);
                }
            }
            Some("result") => {
                let outcome = interpret_result(&value, api_error.as_deref());
                let mut g = state.0.lock().unwrap();
                // The one place totals come from. A budget cut or a kill never reaches here —
                // confirmed on a real install: SIGTERM mid-run ends the stream with no `result`
                // — so `tokens` stays `None` for those, which is the intended report.
                g.progress.tokens = extract_usage(&value);
                g.verdicts = extract_verdicts(
                    value.get("result").and_then(|x| x.as_str()).unwrap_or_default(),
                );
                drop(g);
                terminal = Some(outcome);
            }
            _ => {} // system/etc: nothing this module needs
        }
    }

    let status = child.wait();
    let stderr_tail = stderr_thread.join().unwrap_or_default();

    // Two things the stream itself never carries, appended in its own shape so a reader can
    // parse the whole file uniformly: how the process actually exited, and whatever it said on
    // stderr — which on a crash is usually the only explanation there is, and which until now
    // reached nothing but a log line that had already scrolled away.
    if let Some(t) = transcript.as_mut() {
        let exit = match &status {
            Ok(s) => s.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            Err(e) => format!("wait failed: {e}"),
        };
        t.write_line(
            &serde_json::json!({
                "type": "crew_run_end",
                "exit": exit,
                "turns": turns,
                "stderr": stderr_tail,
            })
            .to_string(),
        );
    }

    let mut g = state.0.lock().unwrap();
    if budget_hit {
        g.outcome = Some(Outcome::Continue { why: "session turn budget reached".into() });
    } else if let Some(outcome) = terminal {
        g.outcome = Some(outcome);
    } else {
        let msg = if !saw_any_line {
            "process produced no output before exiting".to_string()
        } else if !saw_valid_line {
            "process output was entirely unparseable".to_string()
        } else {
            format!("process exited without a result event (status: {status:?})")
        };
        let msg =
            if stderr_tail.is_empty() { msg } else { format!("{msg}; stderr: {stderr_tail}") };
        g.outcome = Some(Outcome::Failed { class: ErrorClass::AgentCrash, msg });
    }
    g.reaped = true;
    drop(g);
    state.1.notify_all();
}

/// Reads until the pipe closes. Bytes past the cap are discarded, not left unread: stopping
/// while the handle is still open fills the pipe and the child blocks in its next write.
fn drain_capped(mut r: impl Read) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_CAPTURED_BYTES.saturating_sub(buf.len());
                if room > 0 {
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).trim().to_string()
}

fn interpret_result(v: &serde_json::Value, api_error: Option<&str>) -> Outcome {
    let is_error = v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(true);
    let text = v.get("result").and_then(|x| x.as_str()).unwrap_or_default();

    if is_error {
        // Confirmed against `claude 2.1.282`: an unknown `--model` is not refused at startup.
        // The process runs, reports `model_not_found` on a synthetic turn and exits 0 with an
        // error `result` — so read as a crash, it would be retried identically until the
        // identical-failure streak caught it.
        let class = match api_error {
            Some("model_not_found") => ErrorClass::ModelNotFound,
            _ => ErrorClass::AgentCrash,
        };
        return Outcome::Failed { class, msg: truncate(text, 500) };
    }
    if let Some(why) = extract_marker(text, "continue") {
        return Outcome::Continue { why };
    }
    if let Some(why) = extract_marker(text, "blocked") {
        return Outcome::Blocked { why };
    }
    Outcome::Done
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use super::*;
    use crate::model::{Feedback, Issue, Verdict, looks_like_commit};
    use crate::worker::prompt::REVIEW_MARKER;
    use crate::worker::{TokenUsage, ToolEndpoint};
    use crate::workspace::WipSnapshot;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude").join(name)
    }

    fn tmp_workspace(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "crew-claude-worker-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A fresh conversation name. These tests drive fixture scripts standing in for `claude`,
    /// which ignore the flag; what matters is that the call shape is the real one.
    fn fresh_session() -> Session {
        Session::New("11111111-1111-4111-8111-111111111111".into())
    }

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

    /// Polls rather than blocking on a channel: the point under test is the worker's own
    /// behaviour, and a bounded poll fails loudly (panics) instead of hanging the suite if that
    /// behaviour regresses to "never reports a verdict".
    fn wait_for_finish(h: &Arc<dyn RunHandle>) -> Outcome {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(o) = h.finished() {
                return o;
            }
            if Instant::now() > deadline {
                panic!("run did not report a verdict within 10s");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_completed_run_records_the_totals_the_result_event_reports_not_a_sum_of_events() {
        // The fixture's assistant events sum to 18 in / 8 out; its result event says 312 / 60.
        // The two disagree on purpose, so this test can only pass by reading the right one.
        let ws = tmp_workspace("done");
        let w = ClaudeWorker::new(fixture("clean_done.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        assert_eq!(wait_for_finish(&h), Outcome::Done);
        let p = h.progress();
        assert_eq!(p.turns, 2);
        assert_eq!(
            p.tokens,
            Some(TokenUsage { input: 312, output: 60 }),
            "input is the result's input + cache creation + cache read; output is its own"
        );

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_run_whose_stderr_fills_the_pipe_still_reaches_its_result_event() {
        let ws = tmp_workspace("stderr");
        let w = ClaudeWorker::new(fixture("chatty_stderr.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));
        assert_eq!(wait_for_finish(&h), Outcome::Done);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn liveness_moves_on_every_stream_event_not_only_on_turns() {
        // clean_done.sh emits system, assistant, user (a tool result), assistant, result. Two of
        // those are turns; all five are proof of life. Stall detection compares Progress between
        // ticks, so the count it sees has to move on the tool result too, or an agent inside a
        // long tool call reads as silent.
        let ws = tmp_workspace("events");
        let w = ClaudeWorker::new(fixture("clean_done.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        wait_for_finish(&h);
        let p = h.progress();
        assert_eq!(p.turns, 2);
        assert_eq!(p.events, 5, "every parsed event counts, not just the assistant ones");

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_run_that_dies_before_its_result_event_reports_no_token_total() {
        // The stream carried one assistant event with a usage block. Summing it would give a
        // number; the number would be wrong by the turn count, so the honest report is none.
        let ws = tmp_workspace("no-total");
        let w = ClaudeWorker::new(fixture("crash_mid_stream.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        wait_for_finish(&h);
        let p = h.progress();
        assert!(p.turns > 0, "there was usage on the stream to be tempted by");
        assert_eq!(p.tokens, None, "and it must not have been used");

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_crew_outcome_continue_marker_is_parsed_from_the_final_text() {
        let ws = tmp_workspace("continue");
        let w = ClaudeWorker::new(fixture("explicit_continue.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        assert_eq!(
            wait_for_finish(&h),
            Outcome::Continue { why: "need another turn to finish tests".into() }
        );

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn review_verdicts_are_parsed_off_the_final_text_and_a_malformed_line_leaves_its_comment_open()
    {
        let ws = tmp_workspace("verdicts");
        let w = ClaudeWorker::new(fixture("review_verdicts.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        assert_eq!(wait_for_finish(&h), Outcome::Done);
        let v = h.verdicts();
        assert_eq!(v.len(), 2, "two well-formed lines, one malformed, one bare acceptance: {v:?}");
        assert_eq!(v[0].comment_id, "4059939692");
        assert_eq!(v[0].verdict, Verdict::Accepted);
        assert_eq!(v[0].detail, "a1b2c3d", "an accepted verdict names the resolving commit");
        assert_eq!(v[1].verdict, Verdict::Rejected);
        assert!(v[1].detail.starts_with("the umask concern"), "a rejection names its reason");
        assert!(
            !v.iter().any(|x| x.comment_id == "4059939694"),
            "a line with no verdict must leave its comment outstanding, not invent one"
        );
        assert!(
            !v.iter().any(|x| x.comment_id == "4059939695"),
            "an acceptance that names no commit is not an acceptance"
        );

        std::fs::remove_dir_all(&ws).ok();
    }

    /// Finding 5 on #47. An acceptance needed only a non-empty detail, so `accepted: fixed`
    /// was stored and posted as though it named the commit carrying the fix. The module's own
    /// invariant is that acceptance records a commit and never a bare acknowledgement.
    #[test]
    fn an_acceptance_that_names_no_commit_leaves_its_comment_outstanding() {
        let v = extract_verdicts(
            "CREW_REVIEW: 1: accepted: fixed\n\
             CREW_REVIEW: 2: accepted: a1b2c3d\n\
             CREW_REVIEW: 3: accepted: see commit a1b2c3d\n\
             CREW_REVIEW: 4: accepted: 0123456789abcdef0123456789abcdef01234567\n\
             CREW_REVIEW: 5: rejected: fixed\n",
        );
        let ids: Vec<&str> = v.iter().map(|x| x.comment_id.as_str()).collect();
        assert_eq!(ids, vec!["2", "4", "5"], "{v:?}");
        assert!(v.iter().all(|x| x.verdict != Verdict::Accepted || looks_like_commit(&x.detail)));
        // A rejection's detail is a reason, and "fixed" is a poor one but not a forged commit.
        assert_eq!(v[2].verdict, Verdict::Rejected);
    }

    #[test]
    fn a_run_handed_no_review_reports_no_verdicts() {
        let ws = tmp_workspace("no-verdicts");
        let w = ClaudeWorker::new(fixture("clean_done.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));
        wait_for_finish(&h);
        assert!(h.verdicts().is_empty());
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_red_ci_reaches_the_prompt_as_the_failing_check_and_its_detail() {
        let fb = Feedback::Ci {
            pr_url: "https://github.com/o/r/pull/9".into(),
            failures: vec![crate::forge::CiFailure {
                name: "fmt + clippy + test".into(),
                url: Some("https://ci/run/1".into()),
                detail: "error[E0308]: mismatched types\n --> src/x.rs:4:5".into(),
            }],
        };
        for prompt in [
            build_prompt(&issue(), None, std::slice::from_ref(&fb), &[]),
            build_continuation_prompt(&issue(), None, std::slice::from_ref(&fb), &[], false),
        ] {
            assert!(prompt.contains("CI is red"), "{prompt}");
            assert!(prompt.contains("fmt + clippy + test"));
            assert!(
                prompt.contains("error[E0308]: mismatched types"),
                "the cause must reach the agent"
            );
            assert!(prompt.contains("https://github.com/o/r/pull/9"));
        }
        assert!(!build_prompt(&issue(), None, &[], &[]).contains("CI is red"));
    }

    #[test]
    fn snapshots_of_uncommitted_work_are_named_in_both_prompts_with_their_diffstats() {
        let wip = [
            WipSnapshot {
                ref_name: "refs/crew/wip/iss-1-abc/000001-0123456789ab".into(),
                diffstat: " half.txt | 1 +\n 1 file changed, 1 insertion(+)".into(),
            },
            WipSnapshot {
                ref_name: "refs/crew/wip/iss-1-abc/000002-ba9876543210".into(),
                diffstat: " src/lib.rs | 4 ++--\n 1 file changed, 2 insertions(+), 2 deletions(-)"
                    .into(),
            },
        ];
        insta::assert_snapshot!("new_prompt_with_wip", build_prompt(&issue(), None, &[], &wip));
        insta::assert_snapshot!(
            "continuation_prompt_with_wip",
            build_continuation_prompt(&issue(), None, &[], &wip, false)
        );
        insta::assert_snapshot!("new_prompt_without_wip", build_prompt(&issue(), None, &[], &[]));
    }

    /// #109: a Decisions section written into #105's description after its session started was
    /// never read, because the continuation prompt leaves the body out as already held.
    #[test]
    fn a_description_edited_after_the_session_started_reaches_the_resumed_session() {
        let edited = Issue {
            body: Some("Fix the thing.\n\n## Decisions\n\nKeep #86's v10; make yours v11.".into()),
            ..issue()
        };
        insta::assert_snapshot!(
            "continuation_prompt_body_changed",
            build_continuation_prompt(&edited, None, &[], &[], true)
        );
        insta::assert_snapshot!(
            "continuation_prompt_body_unchanged",
            build_continuation_prompt(&edited, None, &[], &[], false)
        );
    }

    /// #109: a resume after the gate parked the issue on a rebase conflict told the agent its
    /// session had run out of turns, and it reported the work already done.
    #[test]
    fn a_resume_after_a_rebase_conflict_names_the_conflict_rather_than_a_turn_budget() {
        let fb = Feedback::Conflict {
            base: "master".into(),
            base_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            paths: vec!["src/store/schema.rs".into(), "src/store/mod.rs".into()],
        };
        insta::assert_snapshot!(
            "continuation_prompt_after_conflict",
            build_continuation_prompt(&issue(), None, std::slice::from_ref(&fb), &[], false)
        );
    }

    /// #160: a conflict brief that interrupted a review hand-back once went out alone, and the
    /// comments came back as a second round.
    #[test]
    fn a_conflict_brief_and_the_review_it_interrupted_share_one_prompt() {
        let fb = [
            Feedback::Gate {
                output: "the handoff gate's rebase onto master conflicted in CLAUDE.md.".into(),
            },
            Feedback::Review {
                pr_url: "https://github.com/o/r/pull/9".into(),
                comments: vec![crate::forge::ReviewComment {
                    id: "4059939692".into(),
                    author: "Copilot".into(),
                    path: Some("src/config.rs".into()),
                    line: Some(79),
                    body: "This field is missing `#[serde(default)]`".into(),
                    url: None,
                }],
                unanswered_before: vec![],
            },
        ];
        insta::assert_snapshot!(
            "continuation_prompt_conflict_then_review",
            build_continuation_prompt(&issue(), None, &fb, &[], false)
        );
    }

    /// #165: the worker taking over is told who stopped, why and what it last said, and the
    /// feedback the attempt was sent back with still follows.
    #[test]
    fn a_handoff_names_the_worker_that_stopped_and_keeps_the_feedback_after_it() {
        let fb = [
            Feedback::Handoff {
                from: "claude".into(),
                why: "its account hit a rate limit (five_hour), and its window has not reset"
                    .into(),
                last_text: Some("Migration written; the store tests are next.".into()),
            },
            Feedback::Gate { output: "continuation: more to do".into() },
        ];
        insta::assert_snapshot!("new_prompt_after_handoff", build_prompt(&issue(), None, &fb, &[]));
        insta::assert_snapshot!(
            "continuation_prompt_after_handoff",
            build_continuation_prompt(&issue(), None, &fb, &[], false)
        );
    }

    /// The brief carries the last message's text, not the tool traffic that followed it.
    #[test]
    fn the_last_assistant_text_is_read_past_later_tool_events() {
        let t = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"first"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"},{"type":"tool_use","name":"Bash"},{"type":"text","text":"b"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read"}]}}"#,
            r#"{"type":"crew_run_end"}"#,
        ]
        .join("\n");
        assert_eq!(last_assistant_text(&t).as_deref(), Some("a\nb"));
        assert_eq!(last_assistant_text("not json"), None);
    }

    #[test]
    fn review_comments_reach_the_prompt_with_their_ids_and_the_verdict_convention() {
        let fb = Feedback::Review {
            pr_url: "https://github.com/o/r/pull/9".into(),
            comments: vec![crate::forge::ReviewComment {
                id: "4059939692".into(),
                author: "Copilot".into(),
                path: Some("src/config.rs".into()),
                line: Some(79),
                body: "This field is missing `#[serde(default)]`".into(),
                url: None,
            }],
            unanswered_before: vec!["4059939600".into()],
        };
        let prompt = build_prompt(&issue(), None, std::slice::from_ref(&fb), &[]);
        assert!(prompt.contains("[4059939692] src/config.rs:79 — Copilot"), "{prompt}");
        assert!(prompt.contains("missing `#[serde(default)]`"));
        assert!(prompt.contains(REVIEW_MARKER), "the agent must be told the marker to answer with");
        assert!(prompt.contains("accepted: <commit sha"), "an acceptance must name its commit");
        assert!(prompt.contains("rejected: <one-sentence reason>"));
        assert!(
            prompt.contains("4059939600"),
            "an earlier round's silence is named, not forgotten"
        );
    }

    #[test]
    fn a_crash_with_no_result_event_fails_rather_than_hanging_or_inferring_done() {
        let ws = tmp_workspace("crash");
        let w = ClaudeWorker::new(fixture("crash_mid_stream.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        let outcome = wait_for_finish(&h);
        assert!(
            matches!(outcome, Outcome::Failed { class: ErrorClass::AgentCrash, .. }),
            "got {outcome:?}"
        );

        std::fs::remove_dir_all(&ws).ok();
    }

    /// #37: a rejected rate limit is a separate signal from the CLI's own verdict, which stays
    /// `Failed` here exactly as an ordinary crash would — the scheduler is what tells the two
    /// apart, using `rate_limit()`.
    #[test]
    fn a_rejected_rate_limit_is_reported_alongside_the_crash_it_causes() {
        let ws = tmp_workspace("rate-limited");
        let w = ClaudeWorker::new(fixture("rate_limited.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        let outcome = wait_for_finish(&h);
        assert!(matches!(outcome, Outcome::Failed { class: ErrorClass::AgentCrash, .. }));
        assert_eq!(
            h.rate_limit(),
            Some(RateLimitSignal { kind: "five_hour".into(), resets_at: Some(1_789_981_200) })
        );

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn an_allowed_rate_limit_event_is_not_mistaken_for_a_rejected_one() {
        let allowed = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {"status": "allowed", "rateLimitType": "five_hour", "resetsAt": 1_789_981_200_i64},
        });
        assert_eq!(parse_rate_limit_event(&allowed), None);
    }

    #[test]
    fn an_unrecognised_rate_limit_window_still_pauses_on_its_own_resets_at() {
        let v = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {"status": "rejected", "rateLimitType": "brand_new_window", "resetsAt": 42},
        });
        assert_eq!(
            parse_rate_limit_event(&v),
            Some(RateLimitSignal { kind: "brand_new_window".into(), resets_at: Some(42) })
        );
    }

    #[test]
    fn a_rejected_rate_limit_with_no_resets_at_reports_none_rather_than_guessing() {
        let v = serde_json::json!({
            "type": "rate_limit_event",
            "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour"},
        });
        assert_eq!(
            parse_rate_limit_event(&v),
            Some(RateLimitSignal { kind: "five_hour".into(), resets_at: None })
        );
    }

    /// #184: a warning past its threshold is reported alongside a run that still finishes on
    /// its own, and is never mistaken for the rejection that `rate_limit()` carries.
    #[test]
    fn a_rate_limit_warning_is_reported_without_ending_the_run() {
        let ws = tmp_workspace("rate-limit-warning");
        let w = ClaudeWorker::new(fixture("rate_limit_warning.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        let outcome = wait_for_finish(&h);
        assert_eq!(outcome, Outcome::Done);
        assert_eq!(h.rate_limit(), None);
        assert_eq!(
            h.rate_limit_warning(),
            Some(RateLimitSignal { kind: "five_hour".into(), resets_at: Some(1_789_981_200) })
        );

        std::fs::remove_dir_all(&ws).ok();
    }

    fn warning(info: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type": "rate_limit_event", "rate_limit_info": info})
    }

    fn five_hour_warning(utilization: f64) -> serde_json::Value {
        warning(serde_json::json!({
            "status": "allowed_warning", "rateLimitType": "five_hour", "resetsAt": 42,
            "utilization": utilization,
        }))
    }

    #[test]
    fn a_warning_below_threshold_changes_nothing() {
        let v = five_hour_warning(0.79);
        assert_eq!(parse_rate_limit_warning(&v, 0.80), None);
        assert_eq!(parse_rate_limit_event(&v), None);
    }

    /// The pause names the event's `rateLimitType` and lasts until its `resetsAt`, as a
    /// rejection's does. `surpassedThreshold` is not consulted: one sampled `five_hour` warning
    /// carried none.
    #[test]
    fn a_five_hour_warning_at_threshold_names_its_window_and_reset() {
        assert_eq!(
            parse_rate_limit_warning(&five_hour_warning(0.80), 0.80),
            Some(RateLimitSignal { kind: "five_hour".into(), resets_at: Some(42) })
        );
    }

    /// 46 of the 54 warnings in this repository's transcripts were `seven_day` at 0.82-0.89
    /// against a `surpassedThreshold` of 0.75; pausing on them would stop dispatch for days.
    #[test]
    fn a_seven_day_warning_changes_nothing() {
        let v = warning(serde_json::json!({
            "status": "allowed_warning", "rateLimitType": "seven_day", "resetsAt": 42,
            "utilization": 0.99, "surpassedThreshold": 0.75,
        }));
        assert_eq!(parse_rate_limit_warning(&v, 0.80), None);
    }

    /// Only the top-level `utilization` and `resetsAt` are read; `unifiedWindows` is not a
    /// fallback for either.
    #[test]
    fn a_warning_without_utilization_or_resets_at_changes_nothing() {
        let no_utilization = warning(serde_json::json!({
            "status": "allowed_warning", "rateLimitType": "five_hour", "resetsAt": 42,
            "unifiedWindows": {"five_hour": {"utilization": 0.98}},
        }));
        assert_eq!(parse_rate_limit_warning(&no_utilization, 0.80), None);
        let no_reset = warning(serde_json::json!({
            "status": "allowed_warning", "rateLimitType": "five_hour", "utilization": 0.99,
        }));
        assert_eq!(parse_rate_limit_warning(&no_reset, 0.80), None);
    }

    #[test]
    fn a_partial_trailing_line_is_skipped_not_fatal_to_the_supervisor() {
        let ws = tmp_workspace("partial");
        let w = ClaudeWorker::new(fixture("partial_trailing_line.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        // The point under test is that a malformed final line does not panic or hang the
        // reader thread — it still reaches a verdict (Failed, since no result event arrived).
        let outcome = wait_for_finish(&h);
        assert!(matches!(outcome, Outcome::Failed { class: ErrorClass::AgentCrash, .. }));

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn killing_a_process_that_ignores_sigterm_forces_it_and_it_is_actually_gone() {
        let ws = tmp_workspace("silence");
        let w = ClaudeWorker::new(fixture("silence.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        // Give the script time to install its SIGTERM trap and write its own pid before we
        // try to kill it.
        let pid_file = ws.join("pid.txt");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_file.exists() {
            if Instant::now() > deadline {
                panic!("fixture never wrote its pid");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50)); // let the trap install before we signal

        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();

        assert_eq!(h.kill(300), KillResult::Forced);

        // Assert on the pid, not just the return value: signal 0 checks existence without
        // sending a real one.
        let alive = kill(Pid::from_raw(pid), None).is_ok();
        assert!(!alive, "the process must actually be gone after a forced kill");

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_fresh_session_is_named_on_the_command_line_and_a_continuation_resumes_it_by_id() {
        // The two halves are one decision: `--session-id` is what makes the conversation
        // findable later, and `--resume <id>` is the only reason naming it was worth doing.
        for (session, flag) in [
            (Session::New("a1b2c3d4-0000-4000-8000-000000000001".into()), "--session-id"),
            (Session::Resume("a1b2c3d4-0000-4000-8000-000000000002".into()), "--resume"),
        ] {
            let ws = tmp_workspace(flag.trim_start_matches('-'));
            let w = ClaudeWorker::new(fixture("dump_argv.sh"), vec!["PATH".into()], 0);
            let h = w.spawn(Spawn::new(&issue(), &ws, 0, &session));
            wait_for_finish(&h);

            let dump = std::fs::read_to_string(ws.join("argv_dump.txt")).unwrap();
            let argv: Vec<&str> = dump.lines().collect();
            let at = argv
                .iter()
                .position(|a| *a == flag)
                .unwrap_or_else(|| panic!("{flag} missing from {argv:?}"));
            assert_eq!(
                argv.get(at + 1).copied(),
                Some(session.id()),
                "{flag} must carry the id explicitly — bare `--resume` opens an interactive \
                 picker, and there is no human here to answer it"
            );
            std::fs::remove_dir_all(&ws).ok();
        }
    }

    fn argv_of(w: &ClaudeWorker, session: &Session, tag: &str) -> Vec<String> {
        let ws = tmp_workspace(tag);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, session));
        wait_for_finish(&h);
        let dump = std::fs::read_to_string(ws.join("argv_dump.txt")).unwrap();
        std::fs::remove_dir_all(&ws).ok();
        dump.lines().map(str::to_string).collect()
    }

    #[test]
    fn the_configured_model_and_effort_reach_every_attempt_and_unset_passes_neither_flag() {
        let pinned = ClaudeWorker::new(fixture("dump_argv.sh"), vec!["PATH".into()], 0).with_model(
            ModelChoice {
                model: Some("claude-opus-5-5".into()),
                effort: Some(crate::worker::Effort::Medium),
            },
        );
        // A continuation included: a flag passed only on the first attempt would leave every
        // resumed one on whatever the CLI chose, under a run row naming the pinned model.
        for session in [Session::New("s-new".into()), Session::Resume("s-old".into())] {
            let argv = argv_of(&pinned, &session, "model-pinned");
            let after = |flag: &str| {
                argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1)).cloned()
            };
            assert_eq!(after("--model").as_deref(), Some("claude-opus-5-5"), "{argv:?}");
            assert_eq!(after("--effort").as_deref(), Some("medium"), "{argv:?}");
        }

        let unset = ClaudeWorker::new(fixture("dump_argv.sh"), vec!["PATH".into()], 0);
        let argv = argv_of(&unset, &Session::New("s-new".into()), "model-unset");
        assert!(
            !argv.iter().any(|a| a == "--model" || a == "--effort"),
            "an operator who sets neither must get exactly the old command line: {argv:?}"
        );
    }

    /// Project settings and the broker's `--mcp-config`, nothing from the operator's account (#191).
    #[test]
    fn the_worker_argv_loads_only_project_settings_and_the_broker() {
        let w = ClaudeWorker::new(fixture("dump_argv.sh"), vec!["PATH".into()], 0);
        let broker = ToolEndpoint {
            server: "crew".into(),
            config_path: "/broker/run.json".into(),
            tools: vec!["comment".into()],
        };
        let ws = tmp_workspace("setting-sources");
        let session = Session::New("s-new".into());
        let h = w.spawn(Spawn { tools: Some(&broker), ..Spawn::new(&issue(), &ws, 0, &session) });
        wait_for_finish(&h);
        let dump = std::fs::read_to_string(ws.join("argv_dump.txt")).unwrap();
        std::fs::remove_dir_all(&ws).ok();
        insta::assert_snapshot!("worker_argv_with_broker", dump);
    }

    /// The parent value is `0`, and the name is on the allowlist. `Command` keeps the last
    /// write, so moving the forced `1` ahead of that copy would hand the child `0` (#191).
    #[test]
    fn auto_memory_is_off_in_the_child_even_when_the_allowlist_passes_it_through() {
        let ws = tmp_workspace("auto-memory");
        let w = ClaudeWorker::new(
            fixture("dump_env.sh"),
            vec!["PATH".into(), "CLAUDE_CODE_DISABLE_AUTO_MEMORY".into()],
            0,
        )
        .with_parent_env(&[("PATH", "/usr/bin"), ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "0")]);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));
        wait_for_finish(&h);
        let dump = std::fs::read_to_string(ws.join("env_dump.txt")).unwrap();
        std::fs::remove_dir_all(&ws).ok();
        let set: Vec<&str> =
            dump.lines().filter(|l| l.starts_with("CLAUDE_CODE_DISABLE_AUTO_MEMORY=")).collect();
        assert_eq!(set, ["CLAUDE_CODE_DISABLE_AUTO_MEMORY=1"]);
        assert!(
            dump.lines().any(|l| l == "PATH=/usr/bin"),
            "the allowlist copy must still reach the child: {dump}"
        );
    }

    #[test]
    fn an_unknown_model_fails_on_a_permanent_class_rather_than_reading_as_a_crash() {
        let ws = tmp_workspace("unknown-model");
        // A budget of one: the smallest valid one, and the one that would cut the run off on the
        // synthetic turn if it counted.
        let w = ClaudeWorker::new(fixture("unknown_model.sh"), vec!["PATH".into()], 1);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));
        let outcome = wait_for_finish(&h);
        match outcome {
            Outcome::Failed { class, .. } => {
                assert_eq!(class, ErrorClass::ModelNotFound);
                assert!(!class.retryable(), "every retry would pass the same name");
            }
            other => panic!("expected a permanent failure, got {other:?}"),
        }
        std::fs::remove_dir_all(&ws).ok();
    }

    /// #64: the App's key and its installation tokens never enter this process's environment at
    /// all, so what keeps them from the child is that the allowlist names no credential either.
    #[test]
    fn the_default_allowlist_names_no_credential_variable() {
        for name in DEFAULT_ENV_ALLOWLIST {
            let upper = name.to_ascii_uppercase();
            for marker in ["TOKEN", "KEY", "SECRET", "GITHUB", "GH_", "CREW", "JIRA", "ATLASSIAN"] {
                assert!(!upper.contains(marker), "{name} could carry a credential");
            }
        }
    }

    #[test]
    #[allow(unsafe_code)]
    fn no_tracker_credential_reaches_the_child_environment() {
        let ws = tmp_workspace("env-leak");
        // SAFETY: a unique key nothing else reads or writes, scoped to this one test.
        unsafe {
            std::env::set_var("CREW_TEST_TRACKER_TOKEN_MUST_NOT_LEAK", "super-secret-value");
        }

        let w = ClaudeWorker::new(fixture("dump_env.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));
        wait_for_finish(&h);

        let dump = std::fs::read_to_string(ws.join("env_dump.txt")).unwrap();
        assert!(!dump.contains("CREW_TEST_TRACKER_TOKEN_MUST_NOT_LEAK"));
        assert!(!dump.contains("super-secret-value"));

        // SAFETY: cleaning up the same unique key set above.
        unsafe {
            std::env::remove_var("CREW_TEST_TRACKER_TOKEN_MUST_NOT_LEAK");
        }
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn the_session_turn_budget_is_self_enforced_and_reports_continue() {
        // clean_done.sh emits two assistant turns; a budget of 1 must cut it off after the
        // first and report Continue rather than waiting for (or trusting) the CLI's own exit.
        let ws = tmp_workspace("budget");
        let w = ClaudeWorker::new(fixture("clean_done.sh"), vec!["PATH".into()], 1);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        assert_eq!(
            wait_for_finish(&h),
            Outcome::Continue { why: "session turn budget reached".into() }
        );
        // The cut happens before the CLI's result event, so there is no total to report. This
        // is the common way a run ends without one; it must read as unknown, not as zero.
        assert_eq!(h.progress().tokens, None);

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_budget_cut_drains_the_rest_of_stdout_and_reports_continue_only_when_the_child_is_gone() {
        let ws = tmp_workspace("flood");
        let w = ClaudeWorker::new(fixture("budget_then_flood.sh"), vec!["PATH".into()], 1);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));
        assert_eq!(
            wait_for_finish(&h),
            Outcome::Continue { why: "session turn budget reached".into() }
        );
        assert_eq!(h.progress().tokens, None);
        std::fs::remove_dir_all(&ws).ok();
    }

    /// #169: a `result` is not the process's exit. Published on the line, the verdict let the
    /// scheduler reclaim a workspace a live `claude` was still flushing into.
    #[test]
    fn a_claude_run_is_reported_finished_only_once_the_child_has_exited() {
        let ws = tmp_workspace("result-lingers");
        let w = ClaudeWorker::new(fixture("result_then_lingers.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));
        assert_eq!(wait_for_finish(&h), Outcome::Done);
        assert!(ws.join("exited").exists(), "the verdict came while the child was still running");
        assert_eq!(h.progress().tokens.map(|t| (t.input, t.output)), Some((14, 4)));
        std::fs::remove_dir_all(&ws).ok();
    }

    /// #214: a resumed session can emit an empty `result` before the turn it was resumed for.
    /// Judged on that one, the run read as `Done` with no turns while the agent was asking for a
    /// decision, and the rest of its stream never reached the transcript.
    #[test]
    fn a_run_that_emits_two_results_is_judged_on_the_last() {
        let ws = tmp_workspace("two-results");
        let t = crate::transcript::Transcripts::new(&ws.join("transcripts"), 1 << 20, 10).unwrap();
        let log = t.open("run-1").unwrap();
        let path = log.path().to_path_buf();

        let w = ClaudeWorker::new(fixture("two_results.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn {
            transcript: Some(log),
            ..Spawn::new(&issue(), &ws, 0, &fresh_session())
        });

        assert_eq!(
            wait_for_finish(&h),
            Outcome::Blocked { why: "two criteria need an operator decision".into() }
        );
        let p = h.progress();
        assert_eq!(p.turns, 1);
        assert_eq!(p.tokens.map(|t| t.output), Some(5), "totals come from the last result");
        let v = h.verdicts();
        assert_eq!(v.len(), 1, "only the last result's verdicts: {v:?}");
        assert_eq!((v[0].comment_id.as_str(), v[0].verdict), ("222", Verdict::Rejected));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches(r#""type":"result""#).count(), 2, "{text}");

        std::fs::remove_dir_all(&ws).ok();
    }

    /// A turn after an early `result` that dies without its own is a crash, not the early
    /// `result`'s `Done`: that would hand unfinished work to the gate (#214).
    #[test]
    fn a_turn_after_an_early_result_that_ends_without_one_reads_as_a_crash() {
        let ws = tmp_workspace("result-then-crash");
        let w = ClaudeWorker::new(fixture("result_then_crash.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        let outcome = wait_for_finish(&h);
        assert!(
            matches!(outcome, Outcome::Failed { class: ErrorClass::AgentCrash, .. }),
            "{outcome:?}"
        );
        assert_eq!(h.progress().tokens, None);
        assert!(h.verdicts().is_empty(), "the early result's verdicts must not survive");

        std::fs::remove_dir_all(&ws).ok();
    }

    /// The same turn cut by the session budget reads as the budget's `Continue`, and its total
    /// is unknown rather than the early `result`'s zero (#214).
    #[test]
    fn a_budget_cut_after_an_early_result_reports_no_token_total() {
        let ws = tmp_workspace("result-then-budget");
        let w = ClaudeWorker::new(fixture("result_then_budget.sh"), vec!["PATH".into()], 1);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        assert_eq!(
            wait_for_finish(&h),
            Outcome::Continue { why: "session turn budget reached".into() }
        );
        assert_eq!(h.progress().tokens, None);

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_completed_run_leaves_a_readable_transcript_of_everything_the_parser_dropped() {
        let ws = tmp_workspace("transcript");
        let root = ws.join("transcripts");
        let t = crate::transcript::Transcripts::new(&root, 1 << 20, 10).unwrap();
        let log = t.open("run-1").unwrap();
        let path = log.path().to_path_buf();

        let w = ClaudeWorker::new(fixture("chatty_done.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn {
            transcript: Some(log),
            ..Spawn::new(&issue(), &ws, 2, &fresh_session())
        });
        assert_eq!(wait_for_finish(&h), Outcome::Done);

        // The reader thread owns the writer, so the file is only certainly complete once the
        // run is reaped — which `wait_for_finish` plus the run-end line below establishes.
        let text = std::fs::read_to_string(&path).expect("the transcript must be readable");

        // Everything the parser has no use for, which is the whole point.
        assert!(text.contains(r#""subtype":"init""#), "system event missing");
        assert!(text.contains("cargo test"), "tool call missing");
        assert!(text.contains("114 passed"), "tool result missing");
        assert!(text.contains("rate_limit_event"), "rate limit event missing");
        assert!(
            text.contains("this line is not JSON at all"),
            "an unparseable line is the one most worth still having"
        );
        // And the two facts the stream never carries at all.
        assert!(text.contains("crew_run_start"), "dispatch header missing");
        assert!(text.contains(r#""attempt":2"#), "the header must name the attempt");
        assert!(text.contains("crew_run_end"), "exit status missing");

        // Every line but the deliberately broken one must still parse, so a reader can treat
        // the file as JSONL rather than guessing.
        for line in text.lines().filter(|l| l.starts_with('{')) {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("transcript line is not JSON: {line} ({e})"));
        }

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_run_with_no_transcript_behaves_exactly_as_it_did_before() {
        // The degrade path: transcripts off, or a file that could not be opened. It must cost
        // the record and nothing else.
        let ws = tmp_workspace("no-transcript");
        let w = ClaudeWorker::new(fixture("chatty_done.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        assert_eq!(wait_for_finish(&h), Outcome::Done);
        assert_eq!(h.progress().turns, 2);

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_missing_binary_reports_agent_not_found_immediately() {
        let ws = tmp_workspace("missing-bin");
        let w = ClaudeWorker::new("/definitely/not/a/real/claude/binary", vec![], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh_session()));

        let outcome = wait_for_finish(&h);
        assert!(matches!(outcome, Outcome::Failed { class: ErrorClass::AgentNotFound, .. }));
        assert_eq!(h.kill(100), KillResult::AlreadyDone);

        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_run_that_never_started_still_says_so_in_its_transcript() {
        // `run_reader` never gets a thread on this path, so without an explicit line here the
        // transcript would stop after the header and read as a hung agent.
        let ws = tmp_workspace("spawn-fail");
        let t = crate::transcript::Transcripts::new(&ws.join("transcripts"), 1 << 20, 10).unwrap();
        let log = t.open("run-1").unwrap();
        let path = log.path().to_path_buf();

        let w = ClaudeWorker::new("/definitely/not/a/real/claude/binary", vec![], 0);
        let h = w.spawn(Spawn {
            transcript: Some(log),
            ..Spawn::new(&issue(), &ws, 0, &fresh_session())
        });
        assert!(matches!(
            wait_for_finish(&h),
            Outcome::Failed { class: ErrorClass::AgentNotFound, .. }
        ));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("crew_run_start"));
        assert!(text.contains("spawn failed"), "got: {text}");

        std::fs::remove_dir_all(&ws).ok();
    }
}
