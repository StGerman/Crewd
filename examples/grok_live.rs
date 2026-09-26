//! Live probe of Grok Build's headless CLI (#118).
//!
//! `GrokWorker` (#120) parses this CLI's stream. The flags the docs name were checked here
//! against a real install rather than assumed, because a wrong guess is a worker that never
//! runs. The findings for the install this example was run against are recorded below.
//!
//! ```bash
//! cargo run --example grok_live
//! ```
//!
//! Spends subscription quota. Needs a working `grok login`. Writes nothing to GitHub. Like
//! every example it is `test = false`, and CI never runs it. The redacted fresh-session
//! stream is left at `tests/fixtures/grok/stream.jsonl`.
//!
//! ## Findings (`grok 1.0.41`, `4220f3b224a6`)
//!
//! The invocation that completed a fresh session and a resume of it:
//! `--cwd`, `--session-id` / `--resume` with an explicit UUID, `--prompt-file`,
//! `--output-format streaming-json`, `--always-approve`, `--verbatim`, `--no-auto-update`,
//! `--no-plan`, `--no-subagents`, `--disable-web-search`, `--trust`, `--model grok-4.7`,
//! `--effort medium`. Both sessions exited 0. `--prompt-file` is what carries the prompt;
//! it is not an argv element.
//!
//! A turn is one `usage` event. `end.num_turns` was 4 on each session, and each emitted 4
//! `usage` lines. `text` is a chunk (`"I'll"` was one line); joining the chunks produced
//! `CREW_OUTCOME: continue: probe-fresh` and, on the resume, `CREW_OUTCOME: blocked:
//! probe-resume`. `thought` is not a turn. `end.usage` is the total: `input_tokens` plus
//! `cache_read_input_tokens` plus `cache_creation_input_tokens` in, `output_tokens` out.
//! `reasoning_tokens` is reported beside that and is not part of `total_tokens`. The
//! per-response lines summed to the same figure on the fresh run; that is not the contract.
//!
//! `SIGTERM` of the process group before a turn: the process died on the signal, stdout held
//! only `available_commands`, and there was no `end`. An unknown `--resume` id exits 1 with
//! an empty stdout and a "not found" / 404 on stderr, and does not hang. An unknown
//! `--model` exits 1 after one `{"type":"error",... "unknown model id" ...}` line and no
//! `end`. An unknown `--effort` exits 1 naming `xhigh, high, medium, low`. No rate-limit
//! event appeared.
//!
//! `--trust` on a fresh worktree, with `AGENTS.md` a symlink to `CLAUDE.md`, followed the
//! symlink: the passphrase that exists only in `CLAUDE.md` was written to `notes.txt`.
//! `end.modelUsage` named `grok-4.7-build` for a run started with `--model grok-4.7`. The
//! run row records the flag.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const PASSPHRASE: &str = "CREW_PROBE_PHRASE_ORANGE";
const FRESH_BUDGET: Duration = Duration::from_secs(300);
const RESUME_BUDGET: Duration = Duration::from_secs(240);

fn main() -> anyhow::Result<()> {
    let version = Command::new("grok").arg("--version").output()?;
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    println!("grok version: {version}");

    let probe = temp_repo()?;
    let mut prompts = PromptFiles::default();
    println!("throwaway repo: {}", probe.display());

    let home = std::env::var("HOME").unwrap_or_default();
    let redact_from = vec![home, probe.display().to_string()];

    // Already observed: a resume of an id the CLI does not hold fails cleanly, with no stream
    // and no hang. Asserted because a hang here would pin a scheduler slot forever.
    let unknown_prompt = write_prompt(&mut prompts, "Say hi.")?;
    let unknown_args = vec![
        "--cwd".into(),
        probe.display().to_string(),
        "--resume".into(),
        "00000000-0000-4000-8000-000000000099".into(),
        "--always-approve".into(),
        "--verbatim".into(),
        "--no-auto-update".into(),
        "--trust".into(),
        "--output-format".into(),
        "streaming-json".into(),
        "--prompt-file".into(),
        unknown_prompt,
    ];
    let unknown = run(&probe, &unknown_args, Duration::from_secs(40), false)?;
    println!("unknown resume: exit={:?} stdout_bytes={}", unknown.code, unknown.stdout.len());
    anyhow::ensure!(unknown.code == Some(1), "unknown resume exit {:?}", unknown.code);
    anyhow::ensure!(
        unknown.stderr.contains("not found") || unknown.stderr.contains("404"),
        "unknown resume stderr: {}",
        unknown.stderr
    );
    anyhow::ensure!(unknown.stdout.is_empty(), "unknown resume produced a stream");

    let bad_model_prompt = write_prompt(&mut prompts, "Say hi.")?;
    let bad_model_id = crew::model::session_id("grok-live-bad-model", std::process::id() as i64);
    let bad_model_args = vec![
        "--cwd".into(),
        probe.display().to_string(),
        "--session-id".into(),
        bad_model_id,
        "--model".into(),
        "grok-does-not-exist".into(),
        "--always-approve".into(),
        "--verbatim".into(),
        "--no-auto-update".into(),
        "--trust".into(),
        "--output-format".into(),
        "streaming-json".into(),
        "--prompt-file".into(),
        bad_model_prompt,
    ];
    let bad_model = run(&probe, &bad_model_args, Duration::from_secs(60), false)?;
    println!("unknown model: exit={:?} {}", bad_model.code, bad_model.stdout.trim());
    anyhow::ensure!(bad_model.stdout.contains("unknown model id"), "{}", bad_model.stdout);

    let fresh_id = crew::model::session_id("grok-live-fresh", std::process::id() as i64 + 1);
    let fresh_prompt = write_prompt(
        &mut prompts,
        "The project instructions name a probe passphrase. Create notes.txt whose single line \
         is that passphrase. Commit it with the message \"probe\". Do nothing else. End your \
         final message with exactly this line:\nCREW_OUTCOME: continue: probe-fresh\n",
    )?;
    println!("fresh session {fresh_id}");
    let fresh = run(
        &probe,
        &session_args(&probe, "--session-id", &fresh_id, &fresh_prompt),
        FRESH_BUDGET,
        false,
    )?;
    report("fresh", &fresh);
    write_fixture("stream.jsonl", &redact(&fresh.stdout, &redact_from))?;
    anyhow::ensure!(fresh.code == Some(0), "fresh session exit {:?}\n{}", fresh.code, fresh.stderr);

    let resume_prompt = write_prompt(
        &mut prompts,
        "Append a second line `resumed` to notes.txt and commit it with the message \"probe \
         resume\". End your final message with exactly this line:\nCREW_OUTCOME: blocked: \
         probe-resume\n",
    )?;
    println!("resume {fresh_id}");
    let resumed = run(
        &probe,
        &session_args(&probe, "--resume", &fresh_id, &resume_prompt),
        RESUME_BUDGET,
        false,
    )?;
    report("resume", &resumed);
    write_fixture("resume.jsonl", &redact(&resumed.stdout, &redact_from))?;
    anyhow::ensure!(resumed.code == Some(0), "resume exit {:?}\n{}", resumed.code, resumed.stderr);

    let notes = fs::read_to_string(probe.join("notes.txt")).unwrap_or_default();
    println!("notes.txt:\n{notes}");
    println!(
        "passphrase in notes: {} (AGENTS.md is a symlink to CLAUDE.md)",
        notes.contains(PASSPHRASE)
    );

    // A separate session, stopped by this process. Records whether a SIGTERM mid-run still
    // emits a terminal event. The turn budget in #120 depends on that answer.
    let sig_id = crew::model::session_id("grok-live-sigterm", std::process::id() as i64 + 2);
    let sig_prompt = write_prompt(
        &mut prompts,
        "Read CLAUDE.md with your file tool, then write count.txt containing the number 1. \
         Take one tool call at a time.",
    )?;
    println!("sigterm session {sig_id}");
    let killed = run(
        &probe,
        &session_args(&probe, "--session-id", &sig_id, &sig_prompt),
        Duration::from_secs(45),
        true,
    )?;
    report("sigterm", &killed);
    write_fixture("sigterm.jsonl", &redact(&killed.stdout, &redact_from))?;

    let _ = fs::remove_dir_all(&probe);
    println!("OK: fresh and resumed sessions exited 0. Streams are under tests/fixtures/grok/.");
    Ok(())
}

fn session_args(probe: &Path, session_flag: &str, session_id: &str, prompt: &str) -> Vec<String> {
    [
        "--cwd",
        &probe.display().to_string(),
        session_flag,
        session_id,
        "--model",
        "grok-4.7",
        "--effort",
        "medium",
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
        prompt,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

struct Captured {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// `stop_early` sends SIGTERM to the process group once the first stdout line arrives, or at
/// the deadline if none has. Either way the child is reaped before this returns.
fn run(
    dir: &Path,
    args: &[String],
    budget: Duration,
    stop_early: bool,
) -> anyhow::Result<Captured> {
    let mut cmd = Command::new("grok");
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("spawning grok: {e}"))?;
    let pid = child.id() as i32;
    let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("stdout was not piped"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow::anyhow!("stderr was not piped"))?;
    let out: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let out_thread = {
        let out = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => out.lock().expect("stdout buffer").push_str(&line),
                    Err(_) => break,
                }
            }
        })
    };
    let err: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let err_thread = {
        let err = Arc::clone(&err);
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut buf);
            *err.lock().expect("stderr buffer") = buf;
        })
    };

    let started = Instant::now();
    let mut signalled = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let saw_line = out.lock().expect("stdout buffer").contains('\n');
        if stop_early && !signalled && (saw_line || started.elapsed() > Duration::from_secs(20)) {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
            signalled = true;
        }
        if started.elapsed() > budget {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
            let _ = child.wait();
            anyhow::bail!("grok did not exit within {}s", budget.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let _ = out_thread.join();
    let _ = err_thread.join();
    Ok(Captured {
        code: status.code(),
        stdout: out.lock().expect("stdout buffer").clone(),
        stderr: err.lock().expect("stderr buffer").clone(),
    })
}

fn report(label: &str, cap: &Captured) {
    println!("--- {label}: exit={:?}", cap.code);
    if !cap.stderr.trim().is_empty() {
        println!("--- {label} stderr:\n{}", cap.stderr.trim());
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut texts = Vec::new();
    let mut end_usage = false;
    let mut usage_lines = 0u32;
    for line in cap.stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            *counts.entry("(non-json)".into()).or_default() += 1;
            continue;
        };
        let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("(no type)").to_string();
        *counts.entry(kind.clone()).or_default() += 1;
        if kind == "text"
            && let Some(data) = v.get("data").and_then(|d| d.as_str())
        {
            texts.push(data.to_string());
        }
        if kind == "usage" {
            usage_lines += 1;
        }
        if kind == "end" {
            end_usage = v.get("usage").is_some();
            println!("--- {label} end: {}", serde_json::to_string(&v).unwrap_or_default());
        }
        if kind == "error" {
            println!("--- {label} error: {}", line);
        }
    }
    println!("--- {label} events: {counts:?} usage_lines={usage_lines} end_has_usage={end_usage}");
    let joined = texts.join("");
    println!(
        "--- {label} text chars={} marker_continue={} marker_blocked={}",
        joined.chars().count(),
        joined.contains("CREW_OUTCOME: continue:"),
        joined.contains("CREW_OUTCOME: blocked:")
    );
}

fn redact(raw: &str, paths: &[String]) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let mut line = line.to_string();
        for path in paths {
            if path.len() > 1 {
                line = line.replace(path, "/tmp/probe");
            }
        }
        if let Ok(user) = std::env::var("USER")
            && user.len() > 1
        {
            line = line.replace(&user, "operator");
        }
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&line) {
            redact_value(&mut v);
            if let Ok(s) = serde_json::to_string(&v) {
                line = s;
            }
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn redact_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(sig) = map.get_mut("signature") {
                *sig = serde_json::Value::String("redacted".into());
            }
            // A tool result's `output` is the same text again, as raw bytes, and it keeps a
            // username the string replace cannot see.
            let drop_bytes = matches!(
                map.get("output"),
                Some(serde_json::Value::Array(items))
                    if items.first().is_some_and(serde_json::Value::is_number)
            );
            if drop_bytes {
                map.remove("output");
            }
            for value in map.values_mut() {
                redact_value(value);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        _ => {}
    }
}

fn write_fixture(name: &str, body: &str) -> anyhow::Result<()> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok");
    fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    fs::write(&path, body)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Prompt files for the probe. Dropped when `main` returns, including on failure, so a
/// passphrase written for the agent does not stay in the shared temp directory.
#[derive(Default)]
struct PromptFiles(Vec<PathBuf>);

impl Drop for PromptFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
            if let Some(dir) = path.parent() {
                let _ = fs::remove_dir(dir);
            }
        }
    }
}

fn write_prompt(kept: &mut PromptFiles, text: &str) -> anyhow::Result<String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("crewd-grok-live-{}-{n}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let path = dir.join("prompt");
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    file.write_all(text.as_bytes())?;
    kept.0.push(path.clone());
    Ok(path.display().to_string())
}

fn temp_repo() -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("crewd-grok-live-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("CLAUDE.md"), format!("The probe passphrase is {PASSPHRASE}.\n"))?;
    std::os::unix::fs::symlink("CLAUDE.md", dir.join("AGENTS.md"))?;
    git(&dir, &["init", "-q"])?;
    git(&dir, &["add", "CLAUDE.md", "AGENTS.md"])?;
    git(&dir, &["commit", "-q", "-m", "init"])?;
    Ok(dir)
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").args(args).current_dir(dir).output()?;
    anyhow::ensure!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}
