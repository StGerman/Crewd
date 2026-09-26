//! `Worker` over `grok --output-format streaming-json` (#120).
//!
//! Confirmed against `grok 1.0.41` (`4220f3b224a6`), recorded by `examples/grok_live.rs`.
//! Guessing this contract is how a worker silently never runs:
//!
//! * **A turn is one `usage` event.** `end.num_turns` matched that count on both the fresh
//!   session and the resume. `text` and `thought` are not turns: `text` arrives as small
//!   chunks (`"I'll"` was one), and the `CREW_OUTCOME` line is only intact once they are
//!   joined. There is a `--max-turns` flag. It is not passed. The budget here sends
//!   `SIGTERM` and reports [`Outcome::Continue`] itself, the same contract as `ClaudeWorker`,
//!   and a CLI-enforced stop would exit without that verdict.
//! * **Totals come from `end.usage` only.** `input` is `input_tokens` plus the two cache
//!   fields; `output` is `output_tokens`. `reasoning_tokens` is not part of `total_tokens`
//!   on the event, so it is not added. A per-response `usage` line is a turn and a liveness
//!   bump, never a sum. The fresh recording happened to sum to the same total; the test
//!   where the lines disagree is what keeps the next one honest. A `SIGTERM` before `end`
//!   emitted no usage at all, so `tokens` stays `None`.
//! * **An unknown model is one `error` line** whose message contains `unknown model id`,
//!   then exit 1 and no `end`. That is [`ErrorClass::ModelNotFound`]. An unknown `--resume`
//!   id exits 1 with an empty stdout and does not hang. No rate-limit event was emitted;
//!   `rate_limit` stays empty until one is observed.
//! * **`--prompt-file`, not argv.** The prompt is the issue body plus feedback. `--cwd` and
//!   the process working directory are the same worktree. `--trust` was required for a fresh
//!   worktree to read `AGENTS.md`: the passphrase that lives only in `CLAUDE.md` was written
//!   out, and `AGENTS.md` is a symlink to it. `--always-approve` so a permission prompt
//!   cannot stall the run. The run row records the `--model` flag that was passed
//!   (`grok-4.7`); `end.modelUsage` named `grok-4.7-build` for that same run.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use parking_lot::{Condvar, Mutex};

use super::prompt::{build_continuation_prompt, build_prompt, extract_marker, extract_verdicts};
use super::{ModelChoice, RunHandle, Session, Spawn, Worker};
use crate::model::{ErrorClass, Outcome, ReviewVerdict};
use crate::transcript::TranscriptWriter;

use libcrew::TokenUsage;

const MAX_CAPTURED_BYTES: usize = 4096;
/// Joined `text` kept for the outcome marker and the review lines. The marker is at the end
/// of the final message, so the tail is what the parser needs.
const MAX_TEXT_BYTES: usize = 8192;

pub struct GrokWorker {
    bin: PathBuf,
    env_allowlist: Vec<String>,
    max_turns_per_session: u32,
    model: ModelChoice,
}

impl GrokWorker {
    pub fn new(
        bin: impl Into<PathBuf>,
        env_allowlist: Vec<String>,
        max_turns_per_session: u32,
    ) -> Self {
        Self {
            bin: bin.into(),
            env_allowlist,
            max_turns_per_session,
            model: ModelChoice::default(),
        }
    }

    pub fn with_model(mut self, model: ModelChoice) -> Self {
        self.model = model;
        self
    }
}

#[derive(Default)]
struct Inner {
    progress: super::Progress,
    outcome: Option<Outcome>,
    verdicts: Vec<ReviewVerdict>,
    reaped: bool,
}

type SharedState = Arc<(Mutex<Inner>, Condvar)>;

struct GrokRun {
    state: SharedState,
    pid: i32,
    sent_term: AtomicBool,
}

impl RunHandle for GrokRun {
    fn progress(&self) -> super::Progress {
        self.state.0.lock().progress.clone()
    }

    fn finished(&self) -> Option<Outcome> {
        self.state.0.lock().outcome.clone()
    }

    fn verdicts(&self) -> Vec<ReviewVerdict> {
        self.state.0.lock().verdicts.clone()
    }

    // No `rate_limit` field. grok 1.0.41 did not emit a rejection, and the trait default is
    // the report for a worker that has not shown one. A Claude-shaped parser here would be a
    // guess.

    fn kill(&self, grace_ms: u64) -> super::KillResult {
        let already_finished = self.state.0.lock().outcome.is_some();
        if self.state.0.lock().reaped {
            return super::KillResult::AlreadyDone;
        }
        if !already_finished && !self.sent_term.swap(true, Ordering::SeqCst) {
            let _ = kill(Pid::from_raw(-self.pid), Signal::SIGTERM);
        }
        let (lock, cvar) = &*self.state;
        let mut guard = lock.lock();
        let result =
            cvar.wait_while_for(&mut guard, |g| !g.reaped, Duration::from_millis(grace_ms));
        if !result.timed_out() {
            return if already_finished {
                super::KillResult::AlreadyDone
            } else {
                super::KillResult::Stopped
            };
        }
        drop(guard);
        let _ = kill(Pid::from_raw(-self.pid), Signal::SIGKILL);
        let mut guard = lock.lock();
        let _ = cvar.wait_while_for(&mut guard, |g| !g.reaped, Duration::from_secs(5));
        super::KillResult::Forced
    }
}

impl Worker for GrokWorker {
    fn uses_tools(&self) -> bool {
        false
    }

    fn model(&self) -> ModelChoice {
        self.model.clone()
    }

    fn spawn(&self, req: Spawn<'_>) -> Arc<dyn RunHandle> {
        let Spawn {
            issue,
            workspace,
            attempt,
            session,
            mut transcript,
            feedback,
            wip,
            body_changed,
            ..
        } = req;
        // `tools` is dropped. This worker has no `--mcp-config` arm, and `launch` does not
        // open a broker session for it. A caller that passes one anyway must not grow an argv
        // flag the probe never tested.
        let prompt = match session {
            Session::New(_) => build_prompt(issue, None, feedback, wip),
            Session::Resume(_) => {
                build_continuation_prompt(issue, None, feedback, wip, body_changed)
            }
        };
        let flag = match session {
            Session::New(_) => "--session-id",
            Session::Resume(_) => "--resume",
        };

        let prompt_path = match write_prompt_file(&prompt) {
            Ok(path) => path,
            Err(e) => {
                return finished(Outcome::Failed {
                    class: ErrorClass::AgentCrash,
                    msg: format!("writing the prompt: {e}"),
                });
            }
        };

        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(workspace)
            .env_clear()
            .envs(
                self.env_allowlist
                    .iter()
                    .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v))),
            )
            .args([
                "--cwd",
                &workspace.display().to_string(),
                flag,
                session.id(),
                "--always-approve",
                "--verbatim",
                "--no-auto-update",
                "--no-plan",
                "--no-subagents",
                "--disable-web-search",
                "--trust",
                "--output-format",
                "streaming-json",
                "--prompt-file",
            ])
            .arg(&prompt_path);
        if let Some(model) = &self.model.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = self.model.effort {
            cmd.args(["--effort", effort.as_str()]);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        if let Some(t) = transcript.as_mut() {
            t.write_line(
                &serde_json::json!({
                    "type": "crew_run_start",
                    "attempt": attempt,
                    "session": session.id(),
                    "resumed": session.is_resume(),
                    "workspace": workspace.display().to_string(),
                    "tools": false,
                    "model": self.model.model,
                    "effort": self.model.effort,
                })
                .to_string(),
            );
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                remove_prompt(&prompt_path);
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
                return finished(Outcome::Failed {
                    class: ErrorClass::AgentNotFound,
                    msg: format!("spawning {}: {e}", self.bin.display()),
                });
            }
        };

        let pid = child.id() as i32;
        let stdout = child.stdout.take().expect("piped at spawn");
        let stderr = child.stderr.take().expect("piped at spawn");
        let state: SharedState = Arc::new((Mutex::new(Inner::default()), Condvar::new()));
        let max_turns = self.max_turns_per_session;
        {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                run_reader(child, stdout, stderr, state, pid, max_turns, transcript, prompt_path)
            });
        }
        Arc::new(GrokRun { state, pid, sent_term: AtomicBool::new(false) })
    }
}

/// A private file for the prompt. `create_new` refuses a path that already exists, symlink
/// included, and the mode is set at creation so the umask cannot widen it. The directory is
/// `0700` and named with a counter, the same shape as the push-credential file.
fn write_prompt_file(contents: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("crewd-grok-{}-{n}", std::process::id()));
    let mut dirs = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dirs.mode(0o700);
    }
    dirs.create(&dir)?;
    let path = dir.join("prompt");
    let mut file = create_private_file(&path).inspect_err(|_| {
        let _ = std::fs::remove_dir(&dir);
    })?;
    if let Err(e) = file.write_all(contents.as_bytes()) {
        remove_prompt(&path);
        return Err(e);
    }
    Ok(path)
}

fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn remove_prompt(path: &Path) {
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

fn finished(outcome: Outcome) -> Arc<dyn RunHandle> {
    let state: SharedState = Arc::new((
        Mutex::new(Inner { outcome: Some(outcome), reaped: true, ..Default::default() }),
        Condvar::new(),
    ));
    Arc::new(GrokRun { state, pid: 0, sent_term: AtomicBool::new(true) })
}

#[allow(clippy::too_many_arguments)]
fn run_reader(
    mut child: Child,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    state: SharedState,
    pid: i32,
    max_turns_per_session: u32,
    mut transcript: Option<TranscriptWriter>,
    prompt_path: PathBuf,
) {
    let stderr_thread = std::thread::spawn(move || drain_capped(stderr));
    let mut turns = 0u32;
    let mut saw_any_line = false;
    let mut saw_valid_line = false;
    let mut text = String::new();
    let mut model_error: Option<String> = None;

    for line in BufReader::new(stdout).lines() {
        let Ok(raw) = line else { break };
        if let Some(t) = transcript.as_mut() {
            t.write_line(&raw);
        }
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        saw_any_line = true;
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, line, "malformed streaming-json line; skipping");
                continue;
            }
        };
        saw_valid_line = true;
        {
            let mut g = state.0.lock();
            g.progress.events += 1;
        }
        match value.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(data) = value.get("data").and_then(|d| d.as_str()) {
                    append_capped(&mut text, data, MAX_TEXT_BYTES);
                    let mut g = state.0.lock();
                    g.progress.last_event = Some(truncate(&text, 120));
                }
            }
            Some("usage") => {
                turns += 1;
                let mut g = state.0.lock();
                g.progress.turns = turns;
                drop(g);
                if max_turns_per_session > 0 && turns >= max_turns_per_session {
                    state.0.lock().outcome =
                        Some(Outcome::Continue { why: "session turn budget reached".into() });
                    let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
                    break;
                }
            }
            Some("error") => {
                let msg = value.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
                if msg.contains("unknown model id") {
                    state.0.lock().outcome = Some(Outcome::Failed {
                        class: ErrorClass::ModelNotFound,
                        msg: truncate(&msg, 500),
                    });
                } else {
                    model_error = Some(msg);
                }
            }
            Some("end") => {
                if matches!(
                    state.0.lock().outcome,
                    Some(Outcome::Failed { class: ErrorClass::ModelNotFound, .. })
                ) {
                    break;
                }
                let mut g = state.0.lock();
                g.progress.tokens = end_usage(&value);
                g.verdicts = extract_verdicts(&text);
                g.outcome = Some(outcome_from_text(&text, model_error.as_deref()));
                break;
            }
            _ => {}
        }
    }

    let status = child.wait();
    let stderr_tail = stderr_thread.join().unwrap_or_default();
    remove_prompt(&prompt_path);
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
    let mut g = state.0.lock();
    if g.outcome.is_none() {
        let msg = if !saw_any_line {
            "process produced no output before exiting".to_string()
        } else if !saw_valid_line {
            "process output was entirely unparseable".to_string()
        } else if let Some(err) = model_error {
            err
        } else {
            format!("process exited without an end event (status: {status:?})")
        };
        let msg =
            if stderr_tail.is_empty() { msg } else { format!("{msg}; stderr: {stderr_tail}") };
        g.outcome = Some(Outcome::Failed { class: ErrorClass::AgentCrash, msg });
    }
    g.reaped = true;
    drop(g);
    state.1.notify_all();
}

fn outcome_from_text(text: &str, error: Option<&str>) -> Outcome {
    if let Some(why) = extract_marker(text, "continue") {
        return Outcome::Continue { why };
    }
    if let Some(why) = extract_marker(text, "blocked") {
        return Outcome::Blocked { why };
    }
    if let Some(msg) = error {
        return Outcome::Failed { class: ErrorClass::AgentCrash, msg: truncate(msg, 500) };
    }
    Outcome::Done
}

/// Totals from the terminal `end` event. `None` when that event has no `usage` object.
fn end_usage(v: &serde_json::Value) -> Option<TokenUsage> {
    let usage = v.get("usage")?.as_object()?;
    let get = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    Some(TokenUsage {
        input: get("input_tokens")
            + get("cache_creation_input_tokens")
            + get("cache_read_input_tokens"),
        output: get("output_tokens"),
    })
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn append_capped(buf: &mut String, data: &str, max_bytes: usize) {
    buf.push_str(data);
    if buf.len() <= max_bytes {
        return;
    }
    let mut cut = buf.len() - max_bytes;
    while !buf.is_char_boundary(cut) {
        cut += 1;
    }
    buf.drain(..cut);
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::model::Issue;
    use crate::worker::{Session, Spawn, ToolEndpoint};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_grok").join(name)
    }

    fn tmp_workspace(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("crew-grok-worker-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn issue() -> Issue {
        Issue {
            id: "iss-1".into(),
            identifier: "MT-1".into(),
            title: "probe".into(),
            body: Some("do the thing".into()),
            state: "In Progress".into(),
            priority: None,
            url: None,
            labels: vec![],
            dispatchable: true,
            created_at: None,
            native_ref: None,
            blocked_by: vec![],
        }
    }

    fn fresh() -> Session {
        Session::New("a1b2c3d4-0000-4000-8000-000000000001".into())
    }

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
    fn a_recorded_grok_stream_reports_the_turns_outcome_and_terminal_usage() {
        let ws = tmp_workspace("recorded");
        let w = GrokWorker::new(fixture("replay_stream.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        assert_eq!(wait_for_finish(&h), Outcome::Continue { why: "probe-fresh".into() });
        let p = h.progress();
        assert_eq!(p.turns, 4, "a turn is a usage event; the recording has four");
        assert_eq!(p.tokens.map(|t| (t.input, t.output)), Some((94_726, 590)));
        assert!(h.rate_limit().is_none(), "the recording emitted no rate-limit event");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_grok_run_reports_the_end_events_usage_rather_than_a_sum_of_usage_lines() {
        let ws = tmp_workspace("sum");
        let w = GrokWorker::new(fixture("usage_disagrees.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        assert_eq!(wait_for_finish(&h), Outcome::Done);
        // The usage lines sum far above this. Only `end.usage` is 3+5+6 in and 4 out.
        assert_eq!(h.progress().tokens.map(|t| (t.input, t.output)), Some((14, 4)));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_grok_run_that_reaches_the_turn_budget_is_stopped_and_reports_continue() {
        let ws = tmp_workspace("budget");
        let w = GrokWorker::new(fixture("two_usage.sh"), vec!["PATH".into()], 1);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        assert_eq!(
            wait_for_finish(&h),
            Outcome::Continue { why: "session turn budget reached".into() }
        );
        assert_eq!(h.progress().tokens, None);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_grok_run_that_dies_before_its_end_event_reports_no_token_total() {
        let ws = tmp_workspace("noend");
        let w = GrokWorker::new(fixture("no_end.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        match wait_for_finish(&h) {
            Outcome::Failed { class: ErrorClass::AgentCrash, .. } => {}
            other => panic!("expected a crash with no total, got {other:?}"),
        }
        assert_eq!(h.progress().tokens, None);
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_marker_split_across_text_chunks_is_still_the_outcome() {
        let ws = tmp_workspace("split");
        let w = GrokWorker::new(fixture("split_marker.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        assert_eq!(wait_for_finish(&h), Outcome::Continue { why: "split marker".into() });
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn an_unknown_model_is_permanent_rather_than_a_crash() {
        let ws = tmp_workspace("model");
        let w = GrokWorker::new(fixture("unknown_model.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        match wait_for_finish(&h) {
            Outcome::Failed { class: ErrorClass::ModelNotFound, .. } => {}
            other => panic!("expected ModelNotFound, got {other:?}"),
        }
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    #[allow(unsafe_code)]
    fn a_grok_run_is_spawned_without_a_shell_and_with_only_the_allowlisted_environment() {
        let ws = tmp_workspace("argv");
        let w = GrokWorker::new(fixture("dump_argv.sh"), vec!["PATH".into()], 0).with_model(
            ModelChoice {
                model: Some("grok-4.7".into()),
                effort: Some(crate::worker::Effort::Medium),
            },
        );
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        wait_for_finish(&h);
        let dump = std::fs::read_to_string(ws.join("argv_dump.txt")).unwrap();
        let argv: Vec<&str> = dump.lines().collect();
        assert!(argv[0].ends_with("dump_argv.sh"), "the binary is exec'd directly: {argv:?}");
        assert!(!argv.contains(&"bash"));
        assert!(!argv.contains(&"-lc"));
        assert!(!argv.contains(&"--mcp-config"));
        assert!(!argv.contains(&"--max-turns"));
        let at = argv.iter().position(|a| *a == "--session-id").expect("session flag");
        assert_eq!(argv[at + 1], fresh().id());
        assert!(argv.contains(&"--prompt-file"));
        assert!(argv.contains(&"grok-4.7"));
        assert!(argv.contains(&"medium"));

        // A tool endpoint handed in anyway must not become an argv flag.
        let ws2 = tmp_workspace("tools");
        let endpoint = ToolEndpoint {
            server: "crew".into(),
            config_path: PathBuf::from("/tmp/should-not-appear.json"),
            tools: vec!["comment".into()],
        };
        let h = w.spawn(Spawn {
            tools: Some(&endpoint),
            ..Spawn::new(&issue(), &ws2, 0, &Session::Resume(fresh().id().to_string()))
        });
        wait_for_finish(&h);
        let dump = std::fs::read_to_string(ws2.join("argv_dump.txt")).unwrap();
        assert!(!dump.contains("mcp"), "{dump}");
        assert!(!dump.contains("should-not-appear"), "{dump}");
        assert!(dump.contains("--resume"));

        let ws3 = tmp_workspace("env");
        // SAFETY: a unique key nothing else reads or writes, scoped to this one test. It is
        // present in the parent and absent from the allowlist, so a leak would show in the dump.
        unsafe {
            std::env::set_var("UNRELATED_SECRET", "super-secret-value");
        }
        let w = GrokWorker::new(fixture("dump_env.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws3, 0, &fresh()));
        wait_for_finish(&h);
        let env = std::fs::read_to_string(ws3.join("env_dump.txt")).unwrap();
        assert!(env.lines().any(|l| l.starts_with("PATH=")), "{env}");
        assert!(!env.contains("GITHUB_TOKEN"), "{env}");
        assert!(!env.contains("ANTHROPIC_API_KEY"), "{env}");
        assert!(!env.contains("UNRELATED_SECRET"), "{env}");
        unsafe {
            std::env::remove_var("UNRELATED_SECRET");
        }

        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&ws2).ok();
        std::fs::remove_dir_all(&ws3).ok();
    }

    /// `fs::write` follows a symlink already at the path and creates the file at the umask.
    /// The prompt holds the issue body, so neither is acceptable.
    #[cfg(unix)]
    #[test]
    fn a_prompt_file_is_private_and_does_not_follow_a_planted_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tmp_workspace("prompt-priv");
        let path = dir.join("prompt");
        let mut file = create_private_file(&path).unwrap();
        use std::io::Write;
        file.write_all(b"secret").unwrap();
        drop(file);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the prompt must not be group- or world-readable");

        std::fs::remove_file(&path).unwrap();
        let stolen = dir.join("stolen");
        std::fs::write(&stolen, b"original").unwrap();
        symlink(&stolen, &path).unwrap();
        assert!(create_private_file(&path).is_err(), "create_new must refuse the symlink");
        assert_eq!(std::fs::read(&stolen).unwrap(), b"original");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_marker_past_the_text_cap_is_still_read() {
        let ws = tmp_workspace("cap");
        let w = GrokWorker::new(fixture("past_the_cap.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        assert_eq!(wait_for_finish(&h), Outcome::Continue { why: "past the cap".into() });
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn a_run_whose_stderr_fills_the_pipe_still_reaches_its_end_event() {
        let ws = tmp_workspace("stderr");
        let w = GrokWorker::new(fixture("chatty_stderr.sh"), vec!["PATH".into()], 0);
        let h = w.spawn(Spawn::new(&issue(), &ws, 0, &fresh()));
        assert_eq!(wait_for_finish(&h), Outcome::Done);
        assert_eq!(h.progress().tokens.map(|t| (t.input, t.output)), Some((1, 1)));
        std::fs::remove_dir_all(&ws).ok();
    }
}
