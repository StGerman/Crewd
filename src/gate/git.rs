//! [`Gate`] over real `git rebase` or `git merge` and real subprocesses, in the run's own worktree.
//!
//! One supervising thread per gate, the same shape as the real worker: the scheduler polls a
//! handle, and the thread does the blocking. Commands are exec'd directly — argv, no shell —
//! for the reason the worker never uses `bash -lc`: a shell re-imports the operator's dotfiles,
//! and a gate command is the one subprocess here that inherits the operator's environment, so it
//! is also the one that must not be handed a shell to widen that further. A command that needs a
//! pipe belongs in a script the repository checks in.
//!
//! The environment *is* inherited, unlike the worker's allowlist: this is the operator's own
//! test suite running in the operator's own repository with no agent involved, and `cargo`
//! needs `PATH`, `CARGO_HOME`, `RUSTUP_HOME` and whatever else the machine's toolchain relies on.
//! Colour is switched off because the output's destination is a prompt, not a terminal.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::{Gate, GateHandle, Verdict};
use crate::credentials::Credentials;
use crate::model::Issue;
use crate::worker::KillResult;
use crate::workspace::PushCredentialFile;

/// How much of a failing command's output travels back to the agent. The tail, not the head:
/// `cargo test` prints its `failures:` section and summary last, and a compiler stops at the
/// last error. 16 KiB is a few screens — enough to act on, small enough not to crowd the
/// conversation it lands in.
const OUTPUT_CAP: usize = 16 * 1024;

pub struct GitGate {
    repo: PathBuf,
    /// The ref to rebase onto, resolved in `repo`. `None` means `repo`'s own HEAD — the same
    /// commit `GitWorktreeWorkspace::prepare` branches from, now rather than then.
    base: Option<String>,
    /// The remote `base` is fetched from before it is resolved, when delivery names one. `repo`'s
    /// own `base` moves only when someone pulls, and a gate that resolved it passed #50 "on the
    /// base" five seconds before its pull request opened conflicting (#134).
    remote: Option<String>,
    fetch_auth: Option<FetchAuth>,
    /// Held across the fetch. Two gates share `repo` and both update the same remote-tracking
    /// ref; the one that loses `cannot lock ref` would be charged a gate failure.
    fetch_lock: Arc<parking_lot::Mutex<()>>,
    commands: Vec<Vec<String>>,
}

/// The credential and URL the base is fetched with, when the push has them: the same pair
/// `GitWorktreeWorkspace::with_push_credentials` pushes with, so a host whose only credential
/// is the orchestrator's can gate what it can deliver.
#[derive(Clone)]
struct FetchAuth {
    creds: Arc<dyn Credentials>,
    url: String,
}

impl GitGate {
    pub fn new(repo: impl Into<PathBuf>, base: Option<String>, commands: Vec<Vec<String>>) -> Self {
        Self {
            repo: repo.into(),
            base,
            remote: None,
            fetch_auth: None,
            fetch_lock: Arc::new(parking_lot::Mutex::new(())),
            commands,
        }
    }

    /// Fetch from `url` with `creds` instead of from the remote's own URL on the operator's
    /// ambient credential. The fetched commit still lands in `refs/remotes/<remote>/<base>`.
    /// A refused token is retried once: the cache would keep serving it until its refresh
    /// margin, which is how a push already treats the same credential (#64).
    pub fn with_fetch_credentials(
        mut self,
        creds: Arc<dyn Credentials>,
        url: impl Into<String>,
    ) -> Self {
        self.fetch_auth = Some(FetchAuth { creds, url: url.into() });
        self
    }

    /// Gate against `remote`'s copy of the base rather than `repo`'s. Ignored with no base set:
    /// `HEAD` names nothing to fetch.
    pub fn with_remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = Some(remote.into());
        self
    }
}

#[derive(Default)]
struct Inner {
    step: String,
    verdict: Option<Verdict>,
    /// Process group of whatever subprocess is running right now, so `kill` can reach it — and
    /// every grandchild it forks, `cargo test`'s test binaries included — from another thread.
    /// Set by [`GateRun::spawn_tracked`] for *every* subprocess the gate spawns: the `git`
    /// steps that resolve the base, count commits and rebase are tracked here exactly like a
    /// configured `gate.commands` entry, not just the latter. A `pgid` that only covered gate
    /// commands left `kill` unable to reach a `git rebase` in flight — it would signal nothing,
    /// wait out its grace period, and return while `git` kept writing to the worktree the
    /// scheduler was about to delete.
    pgid: Option<i32>,
    killed: bool,
}

type Shared = Arc<(Mutex<Inner>, Condvar)>;

struct GitGateRun {
    state: Shared,
}

impl GateHandle for GitGateRun {
    fn finished(&self) -> Option<Verdict> {
        self.state.0.lock().unwrap().verdict.clone()
    }

    fn step(&self) -> String {
        self.state.0.lock().unwrap().step.clone()
    }

    fn kill(&self, grace_ms: u64) -> KillResult {
        let (lock, cvar) = &*self.state;
        let pgid = {
            let mut g = lock.lock().unwrap();
            if g.verdict.is_some() {
                return KillResult::AlreadyDone;
            }
            g.killed = true;
            g.pgid
        };
        // `pgid` names whichever subprocess is running right now, git step or gate command
        // alike — see the field doc on `Inner::pgid`. The thread also checks `killed` between
        // steps, which is what stops a *next* step from starting, but the signal below is what
        // stops the one already in flight instead of leaving it to run to completion unwatched.
        if let Some(pgid) = pgid {
            let _ = kill(Pid::from_raw(-pgid), Signal::SIGTERM);
        }
        let g = lock.lock().unwrap();
        let (g, timeout) = cvar
            .wait_timeout_while(g, Duration::from_millis(grace_ms), |g| g.verdict.is_none())
            .unwrap();
        if !timeout.timed_out() {
            return KillResult::Stopped;
        }
        let pgid = g.pgid;
        drop(g);
        if let Some(pgid) = pgid {
            let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
        }
        let g = lock.lock().unwrap();
        let _ = cvar.wait_timeout_while(g, Duration::from_secs(5), |g| g.verdict.is_none());
        KillResult::Forced
    }
}

impl Gate for GitGate {
    fn start(&self, issue: &Issue, workspace: &Path) -> Arc<dyn GateHandle> {
        let state: Shared = Arc::new((Mutex::new(Inner::default()), Condvar::new()));
        let run = GateRun {
            state: state.clone(),
            repo: self.repo.clone(),
            base: self.base.clone(),
            remote: self.remote.clone(),
            fetch_auth: self.fetch_auth.clone(),
            fetch_lock: self.fetch_lock.clone(),
            commands: self.commands.clone(),
            workspace: workspace.to_path_buf(),
            identifier: issue.identifier.clone(),
        };
        let builder = std::thread::Builder::new().name(format!("gate-{}", issue.identifier));
        spawn_gate_thread(builder, &state, run);
        Arc::new(GitGateRun { state })
    }
}

/// Hand `run` to `builder`, or — if the OS refuses to hand back a thread — record why directly
/// into `state` as a `Verdict::Failed` instead of panicking.
///
/// `Gate::start` promises never to fail outright: a gate that cannot even begin still answers
/// through the handle, so the scheduler has exactly one path to reason about. An `.expect()`
/// here used to break that promise — an OS that briefly refuses a new thread (an exhausted
/// `ulimit`, most likely) panicked the whole daemon instead, taking every other run down with
/// it and leaving this issue's claim for `recover()` to find stranded at the next startup: the
/// very failure mode `recover()` exists to bound, reached by a route that skips it entirely.
///
/// Split out from `start` so a test can drive this path directly — with a `Builder` configured
/// to fail deterministically (an oversized `stack_size`) — instead of exhausting a real,
/// process-wide thread limit that every other test sharing this binary would also feel.
fn spawn_gate_thread(builder: std::thread::Builder, state: &Shared, run: GateRun) {
    if let Err(e) = builder.spawn(move || run.run()) {
        state.0.lock().unwrap().verdict = Some(Verdict::Failed {
            on_base: false,
            step: "start gate".into(),
            output: format!("could not spawn the gate's supervising thread: {e}"),
        });
    }
}

/// Everything the supervising thread owns.
struct GateRun {
    state: Shared,
    repo: PathBuf,
    base: Option<String>,
    remote: Option<String>,
    fetch_auth: Option<FetchAuth>,
    fetch_lock: Arc<parking_lot::Mutex<()>>,
    commands: Vec<Vec<String>>,
    workspace: PathBuf,
    identifier: String,
}

impl GateRun {
    fn set_step(&self, step: impl Into<String>) {
        self.state.0.lock().unwrap().step = step.into();
    }

    fn killed(&self) -> bool {
        self.state.0.lock().unwrap().killed
    }

    fn finish(&self, verdict: Verdict) {
        let (lock, cvar) = &*self.state;
        lock.lock().unwrap().verdict = Some(verdict);
        cvar.notify_all();
    }

    fn run(self) {
        let verdict = self.gate();
        self.finish(verdict);
    }

    fn gate(&self) -> Verdict {
        let ws = &self.workspace;
        let base_label = match (self.base.as_deref(), self.remote.as_deref()) {
            (Some(base), Some(remote)) => match self.fetch_base(remote, base) {
                Ok(tracking) => tracking,
                Err(verdict) => return verdict,
            },
            (base, _) => base.unwrap_or("HEAD").to_string(),
        };
        let base_label = base_label.as_str();

        // Resolved in `repo`, not in the worktree: the worktree's HEAD is the run's own branch,
        // and a bare `HEAD` there would rebase the branch onto itself and call it current.
        self.set_step(format!("resolve base {base_label}"));
        let base_sha = match self.git(
            &self.repo,
            &["rev-parse", "--verify", "--quiet", &format!("{base_label}^{{commit}}")],
        ) {
            Ok(sha) => sha,
            Err(e) => {
                return Verdict::Failed {
                    on_base: false,
                    step: format!("resolve base {base_label}"),
                    output: format!(
                        "cannot resolve rebase base `{base_label}` in {}: {e}",
                        self.repo.display()
                    ),
                };
            }
        };

        let ahead = self
            .git(ws, &["rev-list", "--count", &format!("{base_sha}..HEAD")])
            .ok()
            .and_then(|n| n.parse::<u64>().ok());
        match ahead {
            Some(0) => return Verdict::NoCommits,
            Some(_) => {}
            None => {
                return Verdict::Failed {
                    on_base: false,
                    step: "count commits".into(),
                    output: format!("{} is not a git checkout the gate can read", ws.display()),
                };
            }
        }
        if self.killed() {
            return stopped("count commits", false);
        }

        // A branch that already contains the base's tip is on the base, and rebasing it would
        // replay only its own commits and drop any merge of the base — which is where an agent
        // that resolved a conflict by merging put the resolution, so the conflict came back
        // (#122). A failed probe falls through to the rebase, which is what ran before.
        let contains_base =
            self.git(ws, &["merge-base", "--is-ancestor", &base_sha, "HEAD"]).is_ok();
        if self.killed() {
            // A probe killed mid-flight reads as "not an ancestor"; without this the stop
            // request would be answered by starting a rebase.
            return stopped("check the branch against the base", false);
        }
        // A rebase or merge the agent left unfinished is neither the branch it committed nor one
        // the gate may start another on: a rebase paused cleanly leaves `HEAD` a replayed commit
        // on the base, handed off with the rest never replayed, and one this gate started and
        // aborted would abort the agent's too, reporting its unresolved paths as a new conflict.
        for (what, in_progress) in
            [("rebase", self.rebase_in_progress(ws)), ("merge", self.merge_in_progress(ws))]
        {
            if in_progress {
                return Verdict::Stuck {
                    step: "check the branch against the base".into(),
                    output: format!(
                        "the worktree is in the middle of a {what} that was never finished"
                    ),
                };
            }
        }
        let base_updated = if contains_base {
            // Skipping the rebase skips its refusal of a dirty tree too, and delivery pushes
            // `HEAD` alone: a tracked edit left uncommitted would pass the gate and never ship.
            if let Some(verdict) = self.uncommitted(ws, true) {
                return verdict;
            }
            false
        } else if self.merged_the_base(ws, &base_sha) {
            if self.killed() {
                return stopped("check the branch for a merge of the base", false);
            }
            // A merge leaves uncommitted edits to files it does not touch where they are, and
            // delivery pushes `HEAD` alone.
            if let Some(verdict) = self.uncommitted(ws, false) {
                return verdict;
            }
            match self.merge(ws, &base_sha, format!("merge {base_label}")) {
                Ok(()) => true,
                Err(verdict) => return verdict,
            }
        } else {
            match self.rebase(ws, &base_sha, format!("rebase onto {base_label}")) {
                Ok(rebased) => rebased,
                Err(verdict) => return verdict,
            }
        };
        tracing::debug!(issue = %self.identifier, base = base_label, base_updated, "branch is on the base");

        for argv in &self.commands {
            if self.killed() {
                return stopped(argv.join(" "), true);
            }
            let step = argv.join(" ");
            self.set_step(&step);
            if let Err(output) = self.run_command(argv) {
                return Verdict::Failed { step, output, on_base: true };
            }
        }
        Verdict::Passed { base_updated }
    }

    /// Fetch `base` into `refs/remotes/<remote>/<base>`. Resolving the local ref instead passed
    /// a branch on a stale base (#134), so a failed fetch ends the gate and never falls back.
    /// The refspec names that one ref — the operator's branch is not moved — and
    /// `--no-write-fetch-head` leaves the `FETCH_HEAD` a merge of theirs is waiting on.
    /// `--no-auto-gc` stops the detached `git maintenance` that would otherwise lock this
    /// repository under the next gate. [`GitGate::fetch_lock`] is held for the fetch: two gates
    /// share `repo`, and the one that loses `cannot lock ref` would be charged a gate failure.
    fn fetch_base(&self, remote: &str, base: &str) -> Result<String, Verdict> {
        let step = format!("fetch {remote}/{base}");
        self.set_step(&step);
        let Some(_hold) = self.hold_fetch_lock() else {
            return Err(stopped(step, false));
        };
        let refspec = format!("+refs/heads/{base}:refs/remotes/{remote}/{base}");
        let fetched = match &self.fetch_auth {
            None => self.git_fetch(remote, &refspec),
            Some(auth) => self.fetch_with_credential(auth, &refspec),
        };
        match fetched {
            Ok(_) if self.killed() => Err(stopped(step, false)),
            Ok(_) => Ok(format!("refs/remotes/{remote}/{base}")),
            Err(e) => Err(Verdict::Failed {
                output: format!(
                    "cannot fetch the base `{base}` from `{remote}` in {}; the gate does not \
                     fall back to the local ref, which may be behind the pull request's base: {e}",
                    self.repo.display()
                ),
                step,
                on_base: false,
            }),
        }
    }

    /// Wait for [`GitGate::fetch_lock`] only while the gate has not been stopped (review on
    /// #161). A gate waiting here has no subprocess, so `kill` has no `pgid` to signal and
    /// returns once its grace runs out; a plain `lock()` would then take the lock later and
    /// start a fetch after the scheduler had treated the run as stopped. `None` once `killed`
    /// is set, including when it was set while the lock was being acquired.
    fn hold_fetch_lock(&self) -> Option<parking_lot::MutexGuard<'_, ()>> {
        loop {
            if self.killed() {
                return None;
            }
            if let Some(hold) = self.fetch_lock.try_lock_for(Duration::from_millis(50)) {
                return (!self.killed()).then_some(hold);
            }
        }
    }

    /// One fetch with the push's credential. A refusal is retried once, on a fresh token, as
    /// the push is (#64): an installation token revoked before its expiry stays cached until
    /// the refresh margin, most of an hour, and every later gate would fail on it.
    fn fetch_with_credential(&self, auth: &FetchAuth, refspec: &str) -> Result<String, String> {
        retry_auth_refusal(
            || self.killed(),
            auth.creds.as_ref(),
            || self.spawn_authed_fetch(auth, refspec),
        )
    }

    /// Dropping the token file any earlier deletes it out from under `git`.
    fn spawn_authed_fetch(&self, auth: &FetchAuth, refspec: &str) -> Result<String, String> {
        let token = auth.creds.token().map_err(|e| e.to_string())?;
        let file = PushCredentialFile::new(&token).map_err(|e| e.to_string())?;
        let args = authed_fetch_args(&file, &auth.url, refspec);
        let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git(&self.repo, &argrefs)
    }

    fn git_fetch(&self, repository: &str, refspec: &str) -> Result<String, String> {
        let args = fetch_args(repository, refspec);
        let argrefs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git(&self.repo, &argrefs)
    }

    /// Rebase the worktree onto `base_sha`: whether that moved any commits, or the verdict a
    /// rebase that stopped ends the gate with.
    fn rebase(&self, ws: &Path, base_sha: &str, step: String) -> Result<bool, Verdict> {
        if self.killed() {
            return Err(stopped(step, false));
        }
        self.set_step(&step);
        let before = self.git(ws, &["rev-parse", "HEAD"]).unwrap_or_default();
        if let Err(stderr) = self.git(ws, &["rebase", base_sha]) {
            // Conflicted paths are read *before* the abort, which is what clears them. The
            // abort itself is what makes a conflict safe to report: the branch goes back to
            // exactly the commits the agent made, so `Workspace::remove`'s merged check still
            // sees work and keeps it, and a human inherits a clean, if stale, branch.
            let paths: Vec<String> = self
                .git(ws, &["diff", "--name-only", "--diff-filter=U"])
                .map(|s| s.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
                .unwrap_or_default();
            // The abort's own exit code cannot say whether it worked: it also fails, harmlessly,
            // after a rebase that was refused before it began. Whether a rebase is still in
            // progress afterwards is the question every verdict below depends on.
            let abort = self.git(ws, &["rebase", "--abort"]);
            if self.rebase_in_progress(ws) {
                return Err(Verdict::Stuck {
                    step,
                    output: format!(
                        "the rebase stopped ({stderr}) and `git rebase --abort` left it in \
                         progress: {}",
                        abort.err().unwrap_or_default()
                    ),
                });
            }
            if !paths.is_empty() {
                return Err(Verdict::Conflict { paths, base_sha: base_sha.to_string() });
            }
            return Err(Verdict::Failed { step, output: stderr, on_base: false });
        }
        let after = self.git(ws, &["rev-parse", "HEAD"]).unwrap_or_default();
        Ok(before != after)
    }

    /// Whether the branch already took the base in by a merge: a commit in `base_sha..HEAD` with
    /// a parent outside that range, which is therefore an ancestor of the base. Rebasing such a
    /// branch drops the merge and replays the commits under it into the conflict the merge
    /// resolved (#177). A failed probe reads as no, which rebases as before.
    fn merged_the_base(&self, ws: &Path, base_sha: &str) -> bool {
        let Ok(out) = self.git(ws, &["rev-list", "--parents", &format!("{base_sha}..HEAD")]) else {
            return false;
        };
        let lines: Vec<Vec<&str>> = out.lines().map(|l| l.split(' ').collect()).collect();
        let own: std::collections::HashSet<&str> = lines.iter().map(|l| l[0]).collect();
        lines.iter().filter(|l| l.len() > 2).any(|l| l[1..].iter().any(|p| !own.contains(p)))
    }

    /// Merge `base_sha` into the worktree's branch, or the verdict a merge that stopped ends the
    /// gate with — the same verdicts, for the same reasons, as [`GateRun::rebase`].
    fn merge(&self, ws: &Path, base_sha: &str, step: String) -> Result<(), Verdict> {
        if self.killed() {
            return Err(stopped(step, false));
        }
        self.set_step(&step);
        let Err(stderr) = self.git(ws, &["merge", "--no-ff", "--no-edit", base_sha]) else {
            return Ok(());
        };
        let paths: Vec<String> = self
            .git(ws, &["diff", "--name-only", "--diff-filter=U"])
            .map(|s| s.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default();
        let abort = self.git(ws, &["merge", "--abort"]);
        if self.merge_in_progress(ws) {
            return Err(Verdict::Stuck {
                step,
                output: format!(
                    "the merge stopped ({stderr}) and `git merge --abort` left it in progress: {}",
                    abort.err().unwrap_or_default()
                ),
            });
        }
        if !paths.is_empty() {
            return Err(Verdict::Conflict { paths, base_sha: base_sha.to_string() });
        }
        Err(Verdict::Failed { step, output: stderr, on_base: false })
    }

    /// The failure for a worktree holding uncommitted changes to tracked files — the policy
    /// `git rebase` enforces by refusing, stated for the paths that do not rebase. Untracked
    /// files pass, as they do for the rebase. `on_base` is whether the branch already contains
    /// the base: the brief must not tell the agent its branch is off the base when it is on it
    /// (review on #123), nor on it before the merge that would put it there.
    fn uncommitted(&self, ws: &Path, on_base: bool) -> Option<Verdict> {
        let step = "check the worktree is clean".to_string();
        let output = match self.git(ws, &["status", "--porcelain", "--untracked-files=no"]) {
            Ok(changes) if changes.is_empty() => return None,
            Ok(changes) => format!(
                "uncommitted changes to tracked files would not be handed off; commit or \
                 discard them:\n{changes}"
            ),
            Err(e) => format!("cannot read the worktree's status: {e}"),
        };
        Some(Verdict::Failed { step, output, on_base })
    }

    /// Spawn `cmd` in its own process group and record its pid as `pgid` for as long as it is
    /// alive, then clear it. Every subprocess the gate starts goes through this one function —
    /// the `git` calls in [`GateRun::git`] as much as a configured `gate.commands` entry in
    /// [`GateRun::run_command`] — because `kill` can only ever reach what `pgid` names. Its own
    /// process group (not just its own pid) is what lets `kill` reach the test binaries `cargo
    /// test` forks, and every child a `git` hook spawns, and not just the immediate process.
    fn spawn_tracked(&self, mut cmd: Command) -> Result<Output, String> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
        let pid = child.id() as i32;
        self.state.0.lock().unwrap().pgid = Some(pid);

        // Reads both pipes without either deadlocking the other; blocks until exit, which is
        // what the supervising thread is for.
        let out = child.wait_with_output();
        self.state.0.lock().unwrap().pgid = None;
        out.map_err(|e| format!("wait: {e}"))
    }

    /// `git` at `at`, tracked the same way as [`GateRun::run_command`] — see
    /// [`GateRun::spawn_tracked`] — so resolving the base, counting commits and rebasing are all
    /// reachable from `kill` and not just the configured gate commands. Same contract as the
    /// free `git` helper the tests use: stdout trimmed on success, stderr trimmed on failure.
    fn git(&self, at: &Path, args: &[&str]) -> Result<String, String> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(at)
            .args(args)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = self.spawn_tracked(cmd).map_err(|e| format!("git {}: {e}", args.join(" ")))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// Whether `ws` is mid-rebase. A `git-path` that cannot be read counts as yes: this answers
    /// whether it is safe to tell an agent the branch is where it left it, and not knowing is
    /// not safe.
    fn rebase_in_progress(&self, ws: &Path) -> bool {
        ["rebase-merge", "rebase-apply"].iter().any(|d| {
            self.git(ws, &["rev-parse", "--path-format=absolute", "--git-path", d])
                .map_or(true, |p| Path::new(&p).exists())
        })
    }

    /// Whether `ws` is mid-merge. Unreadable counts as yes, as in [`GateRun::rebase_in_progress`].
    fn merge_in_progress(&self, ws: &Path) -> bool {
        self.git(ws, &["rev-parse", "--path-format=absolute", "--git-path", "MERGE_HEAD"])
            .map_or(true, |p| Path::new(&p).exists())
    }

    /// One gate command, to completion. `Err` carries what the agent should see.
    fn run_command(&self, argv: &[String]) -> Result<(), String> {
        let (bin, args) = argv.split_first().ok_or_else(|| "empty command".to_string())?;
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(&self.workspace)
            .env("NO_COLOR", "1")
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = self.spawn_tracked(cmd).map_err(|e| format!("could not run `{bin}`: {e}"))?;

        if out.status.success() {
            return Ok(());
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut combined = format!("{} exited with {}\n", argv.join(" "), out.status);
        if !stdout.trim().is_empty() {
            combined.push_str("--- stdout ---\n");
            combined.push_str(stdout.trim_end());
            combined.push('\n');
        }
        if !stderr.trim().is_empty() {
            combined.push_str("--- stderr ---\n");
            combined.push_str(stderr.trim_end());
            combined.push('\n');
        }
        Err(tail(&combined, OUTPUT_CAP))
    }
}

/// `git fetch` rewrites `FETCH_HEAD` and then detaches `git maintenance run --auto`, which
/// locks this same repository under whatever gate or rebase runs next.
fn fetch_args(repository: &str, refspec: &str) -> Vec<String> {
    ["fetch", "--quiet", "--no-tags", "--no-auto-gc", "--no-write-fetch-head", repository, refspec]
        .map(str::to_string)
        .to_vec()
}

fn authed_fetch_args(file: &PushCredentialFile, url: &str, refspec: &str) -> Vec<String> {
    let mut args = file.git_config();
    args.extend(fetch_args(url, refspec));
    args
}

/// One more attempt after an authentication refusal, and only then. `killed` is read after the
/// first attempt: a gate already stopped must not start another fetch.
fn retry_auth_refusal(
    killed: impl Fn() -> bool,
    creds: &dyn Credentials,
    mut attempt: impl FnMut() -> Result<String, String>,
) -> Result<String, String> {
    match attempt() {
        Err(e) if !killed() && crate::workspace::is_auth_refusal(&e) && creds.invalidate() => {
            attempt()
        }
        other => other,
    }
}

fn stopped(step: impl Into<String>, on_base: bool) -> Verdict {
    Verdict::Failed { step: step.into(), output: "stopped by the orchestrator".into(), on_base }
}

/// The last `cap` bytes of `s`, cut on a char boundary, with a note of what was dropped.
fn tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[... {} bytes elided ...]\n{}", start, &s[start..])
}

/// Run `git` at `at`; stdout trimmed on success, stderr trimmed on failure. Test-only: the gate
/// itself spawns git through [`GateRun::git`] instead, which is tracked the same way a
/// configured gate command is (see `spawn_tracked`) so `kill` can reach it. This one is untracked
/// on purpose — it is what the tests use to set up and inspect fixtures, not something `kill`
/// ever needs to reach.
#[cfg(test)]
fn git(at: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(at)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{GitWorktreeWorkspace, Workspace};
    use std::time::Instant;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "crew-gate-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sh_git(at: &Path, args: &[&str]) -> String {
        match git(at, args) {
            Ok(s) => s,
            Err(e) => panic!("git {args:?} failed: {e}"),
        }
    }

    /// A repo on `master` with one commit, plus a worktree for `iss-1` branched from it.
    fn repo_and_worktree(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = tmp(tag);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        sh_git(&repo, &["init", "-q", "-b", "master"]);
        sh_git(&repo, &["config", "user.email", "test@example.com"]);
        sh_git(&repo, &["config", "user.name", "test"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        sh_git(&repo, &["add", "base.txt"]);
        sh_git(&repo, &["commit", "-q", "-m", "init"]);
        let repo = repo.canonicalize().unwrap();

        let ws = GitWorktreeWorkspace::new(dir.join("workspaces"), &repo).unwrap();
        let wt = ws.prepare("iss-1", "MT-1").unwrap().path;
        (dir, repo, wt)
    }

    fn commit(at: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(at.join(file), content).unwrap();
        sh_git(at, &["add", file]);
        sh_git(at, &["commit", "-q", "-m", msg]);
    }

    fn issue() -> Issue {
        Issue {
            id: "iss-1".into(),
            identifier: "MT-1".into(),
            title: "t".into(),
            body: None,
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

    fn wait(h: &Arc<dyn GateHandle>) -> Verdict {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(v) = h.finished() {
                return v;
            }
            assert!(Instant::now() < deadline, "gate did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// The guard for issue #21 in one test: `master` moves after the fork, the branch carries a
    /// commit, and the gate command can only pass on a tree that has master's new file. Drop
    /// the rebase and the command fails; skip the command and the assertion on its side effect
    /// fails; run the command before the rebase and it fails too.
    #[test]
    fn the_branch_is_rebased_onto_the_base_before_the_gate_runs_on_the_rebased_tree() {
        let (dir, repo, wt) = repo_and_worktree("rebase-then-gate");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");

        let gate = GitGate::new(
            &repo,
            Some("master".into()),
            vec![
                argv(&["git", "cat-file", "-e", "HEAD:from_master.txt"]),
                argv(&["touch", "gate-ran"]),
            ],
        );
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::Passed { base_updated: true });
        assert!(wt.join("from_master.txt").exists(), "the worktree must now sit on master's tip");
        assert!(wt.join("agent.txt").exists(), "with the agent's commit replayed on top");
        assert!(wt.join("gate-ran").exists(), "and the gate must actually have run");
        let master = sh_git(&repo, &["rev-parse", "master"]);
        assert!(
            git(&wt, &["merge-base", "--is-ancestor", &master, "HEAD"]).is_ok(),
            "master must be an ancestor of the rebased branch"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guard for #122: the agent resolved a conflict by merging the base into its branch.
    /// A rebase replays the branch's own commits and drops that merge, so the same conflict
    /// comes back; a branch that already contains the base's tip is gated as it stands.
    #[test]
    fn a_branch_that_already_merged_the_base_is_gated_without_a_rebase() {
        let (dir, repo, wt) = repo_and_worktree("merged-base");
        commit(&wt, "base.txt", "agent's version\n", "agent edits base");
        commit(&repo, "base.txt", "master's version\n", "master edits base");
        assert!(git(&wt, &["merge", "-q", "master"]).is_err(), "the merge must conflict");
        std::fs::write(wt.join("base.txt"), "both versions\n").unwrap();
        sh_git(&wt, &["add", "base.txt"]);
        sh_git(&wt, &["commit", "-q", "--no-edit"]);
        let merged = sh_git(&wt, &["rev-parse", "HEAD"]);

        let gate = GitGate::new(
            &repo,
            Some("master".into()),
            vec![argv(&["git", "cat-file", "-e", "HEAD^2"]), argv(&["touch", "gate-ran"])],
        );
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::Passed { base_updated: false });
        assert_eq!(sh_git(&wt, &["rev-parse", "HEAD"]), merged, "the merge commit must survive");
        assert_eq!(std::fs::read_to_string(wt.join("base.txt")).unwrap(), "both versions\n");
        assert!(wt.join("gate-ran").exists(), "the commands still run on the merged branch");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `repo_and_worktree`, with `repo` cloned from an `upstream` that stands in for the remote
    /// its pull requests merge into. Returns `(dir, upstream, repo, worktree)`.
    fn clone_and_worktree(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let (dir, upstream, _) = repo_and_worktree(tag);
        let repo = dir.join("clone");
        sh_git(&dir, &["clone", "-q", upstream.to_str().unwrap(), repo.to_str().unwrap()]);
        sh_git(&repo, &["config", "user.email", "test@example.com"]);
        sh_git(&repo, &["config", "user.name", "test"]);
        let repo = repo.canonicalize().unwrap();
        let ws = GitWorktreeWorkspace::new(dir.join("clone-workspaces"), &repo).unwrap();
        let wt = ws.prepare("iss-1", "MT-1").unwrap().path;
        (dir, upstream, repo, wt)
    }

    /// The guard for #134: the daemon's own `master` lagged the remote's by a merged pull
    /// request, the gate passed on it, and the pull request opened conflicting. Resolve the
    /// local ref instead of fetching and this passes.
    #[test]
    fn the_gate_rebases_onto_the_remote_base_not_a_stale_local_ref() {
        let (dir, upstream, repo, wt) = clone_and_worktree("remote-base");
        commit(&wt, "base.txt", "agent's version\n", "agent edits base");
        commit(&upstream, "base.txt", "merged meanwhile\n", "another pull request merged");
        let local_master = sh_git(&repo, &["rev-parse", "master"]);

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])])
            .with_remote("origin");
        let verdict = wait(&gate.start(&issue(), &wt));

        let remote_master = sh_git(&upstream, &["rev-parse", "master"]);
        assert_eq!(
            verdict,
            Verdict::Conflict { paths: vec!["base.txt".into()], base_sha: remote_master }
        );
        assert!(!wt.join("gate-ran").exists(), "a conflicted branch is not gated");
        assert_eq!(
            sh_git(&repo, &["rev-parse", "master"]),
            local_master,
            "the operator's local master is fetched past, never moved"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review on #161: delivery pushes with the App's token to the canonical URL, so a host
    /// with no ambient credential for the remote could deliver and yet fail every gate fetch.
    /// The remote's own URL here leads nowhere; only the push URL reaches the base.
    #[test]
    fn the_base_is_fetched_from_the_push_url_with_the_push_credential() {
        let (dir, upstream, repo, wt) = clone_and_worktree("fetch-auth");
        commit(&wt, "base.txt", "agent's version\n", "agent edits base");
        commit(&upstream, "base.txt", "merged meanwhile\n", "another pull request merged");
        sh_git(&repo, &["remote", "set-url", "origin", dir.join("nowhere").to_str().unwrap()]);

        let gate = GitGate::new(&repo, Some("master".into()), vec![])
            .with_remote("origin")
            .with_fetch_credentials(
                Arc::new(crate::credentials::StaticToken::new("ghs_token")),
                upstream.to_str().unwrap(),
            );
        let verdict = wait(&gate.start(&issue(), &wt));

        let remote_master = sh_git(&upstream, &["rev-parse", "master"]);
        assert_eq!(
            verdict,
            Verdict::Conflict { paths: vec!["base.txt".into()], base_sha: remote_master.clone() }
        );
        assert_eq!(sh_git(&repo, &["rev-parse", "origin/master"]), remote_master);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review on #161: the fetch runs in the operator's checkout, and `git fetch` rewrites
    /// `FETCH_HEAD` — the commit a merge they have not finished is waiting on.
    #[test]
    fn fetching_the_base_leaves_the_operators_fetch_head() {
        let (dir, upstream, repo, wt) = clone_and_worktree("fetch-head");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&upstream, "remote.txt", "moved\n", "master moved");
        let sentinel = "operator FETCH_HEAD\n";
        std::fs::write(repo.join(".git/FETCH_HEAD"), sentinel).unwrap();

        let gate = GitGate::new(&repo, Some("master".into()), vec![]).with_remote("origin");
        let verdict = wait(&gate.start(&issue(), &wt));

        assert!(matches!(verdict, Verdict::Passed { base_updated: true }), "{verdict:?}");
        assert_eq!(std::fs::read_to_string(repo.join(".git/FETCH_HEAD")).unwrap(), sentinel);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review on #161: what `git credential fill` answers for the fetch's own config is the
    /// push token, ahead of a helper configured on the checkout. The token is not in argv.
    #[test]
    fn the_gate_fetch_resolves_the_push_token_ahead_of_an_ambient_helper() {
        const TOKEN: &str = "ghs_gate_token_that_must_not_leak";
        let (dir, repo, _wt) = repo_and_worktree("fetch-token");
        sh_git(&repo, &["config", "credential.helper", "!printf 'password=from-the-operator\\n'"]);
        let file = PushCredentialFile::new(TOKEN).unwrap();
        let args = authed_fetch_args(
            &file,
            "https://github.com/o/r.git",
            "+refs/heads/master:refs/remotes/origin/master",
        );
        assert!(args.iter().all(|a| !a.contains(TOKEN)), "the token is in argv: {args:?}");
        let config: Vec<&str> =
            args.iter().take_while(|a| a.as_str() != "fetch").map(String::as_str).collect();
        let mut fill = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(&config)
            .args(["credential", "fill"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            fill.stdin.take().unwrap().write_all(b"protocol=https\nhost=github.com\n\n").unwrap();
        }
        let out = fill.wait_with_output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            stdout.contains(&format!("password={TOKEN}")),
            "stdout={stdout} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!stdout.contains("from-the-operator"), "{stdout}");
        drop(file);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review on #161: a revoked installation token is what the push retries once. The same
    /// refusal on the fetch has to do the same, or every gate until the refresh margin fails
    /// on a credential that will not change. Anything else, and a gate already stopped, is not
    /// a reason to fetch again.
    #[test]
    fn a_fetch_refused_for_authentication_is_retried_once_on_a_fresh_token() {
        use std::sync::atomic::{AtomicU32, Ordering};

        use crate::credentials::{CredentialError, Credentials};

        struct Rotating(AtomicU32);
        impl Credentials for Rotating {
            fn token(&self) -> Result<String, CredentialError> {
                Ok(format!("tok-{}", self.0.load(Ordering::SeqCst)))
            }
            fn invalidate(&self) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                true
            }
        }

        let refused =
            || Err("fatal: Authentication failed for 'https://github.com/o/r.git/'".into());
        let creds = Rotating(AtomicU32::new(0));
        let attempts = AtomicU32::new(0);
        let fetched = retry_auth_refusal(
            || false,
            &creds,
            || {
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                if creds.token().unwrap() == "tok-1" {
                    Ok(format!("fetched on attempt {n}"))
                } else {
                    refused()
                }
            },
        );
        assert_eq!(fetched.unwrap(), "fetched on attempt 1");

        let attempts = AtomicU32::new(0);
        let fetched = retry_auth_refusal(
            || false,
            &creds,
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                refused()
            },
        );
        assert!(fetched.is_err(), "a second refusal is the answer");
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "one retry, not a loop");

        let attempts = AtomicU32::new(0);
        let _ = retry_auth_refusal(
            || false,
            &creds,
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err("cannot lock ref".into())
            },
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "only an authentication refusal is retried");

        let attempts = AtomicU32::new(0);
        let _ = retry_auth_refusal(
            || true,
            &creds,
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                refused()
            },
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "a stopped gate does not fetch again");

        let attempts = AtomicU32::new(0);
        let _ = retry_auth_refusal(
            || false,
            &crate::credentials::StaticToken::new("t"),
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                refused()
            },
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a token that cannot refresh is not retried"
        );
    }

    /// Review on #161: a gate stopped while another gate holds the fetch lock stops within its
    /// grace, and never fetches once the lock frees. Before, `kill` found no `pgid`, returned
    /// `Forced`, and the waiting thread went on to fetch after the run was written off.
    #[test]
    fn a_gate_stopped_while_waiting_for_the_fetch_lock_never_fetches() {
        let (dir, upstream, repo, wt) = clone_and_worktree("fetch-lock-kill");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        let before = sh_git(&repo, &["rev-parse", "refs/remotes/origin/master"]);
        commit(&upstream, "remote.txt", "moved\n", "master moved");

        let gate = GitGate::new(&repo, Some("master".into()), vec![]).with_remote("origin");
        let other_gate = gate.fetch_lock.lock();
        let h = gate.start(&issue(), &wt);
        while h.step() != "fetch origin/master" {
            std::thread::sleep(Duration::from_millis(10));
        }

        let result = h.kill(500);
        drop(other_gate);

        assert!(matches!(result, KillResult::Stopped), "the stop was not clean: {result:?}");
        assert!(
            matches!(h.finished(), Some(Verdict::Failed { .. })),
            "a stopped gate did not pass"
        );
        std::thread::sleep(Duration::from_millis(300));
        let after = sh_git(&repo, &["rev-parse", "refs/remotes/origin/master"]);
        assert_eq!(after, before, "a stopped gate fetched once the lock freed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_fetch_of_the_base_fails_the_gate_rather_than_passing_on_the_stale_ref() {
        let (dir, upstream, repo, wt) = clone_and_worktree("fetch-fails");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        std::fs::remove_dir_all(&upstream).unwrap();

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])])
            .with_remote("origin");
        let verdict = wait(&gate.start(&issue(), &wt));

        match verdict {
            Verdict::Failed { step, output, on_base } => {
                assert_eq!(step, "fetch origin/master");
                assert!(output.contains("cannot fetch the base `master`"), "{output}");
                assert!(!on_base, "nothing was rebased");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!wt.join("gate-ran").exists(), "the commands do not run on a stale base");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Skipping the rebase must not skip its refusal of a dirty tree: delivery pushes `HEAD`,
    /// so an uncommitted edit that passed the gate would be dropped from the handoff unseen.
    #[test]
    fn a_dirty_worktree_already_on_the_base_fails_rather_than_passing_without_its_edit() {
        let (dir, repo, wt) = repo_and_worktree("dirty-on-base");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        std::fs::write(wt.join("agent.txt"), "uncommitted edit\n").unwrap();
        std::fs::write(wt.join("scratch.txt"), "untracked\n").unwrap();

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])]);
        let verdict = wait(&gate.start(&issue(), &wt));

        match verdict {
            Verdict::Failed { step, output, on_base } => {
                assert_eq!(step, "check the worktree is clean");
                assert!(output.contains("agent.txt"), "names the edit: {output}");
                assert!(!output.contains("scratch.txt"), "untracked files pass: {output}");
                assert!(on_base, "the branch already contains the base, and the brief says so");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!wt.join("gate-ran").exists(), "the commands do not run on an unshippable tree");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worktree paused mid-rebase can already sit on the base with a clean status; the
    /// no-rebase path must not run the gate on it and pass what is half a branch.
    #[test]
    fn a_worktree_paused_mid_rebase_on_the_base_is_stuck_rather_than_passed() {
        let (dir, repo, wt) = repo_and_worktree("paused-rebase");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&wt, "more.txt", "more\n", "more of the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");
        assert!(git(&wt, &["rebase", "-q", "-x", "false", "master"]).is_err(), "must pause");

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert!(matches!(verdict, Verdict::Stuck { .. }), "got {verdict:?}");
        assert!(!wt.join("gate-ran").exists(), "an unfinished rebase is not gated");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `kill` may land while an earlier probe runs, which reads as the probe failing; the rebase
    /// must still see the stop and not begin rewriting a worktree cleanup is about to remove.
    #[test]
    fn a_gate_stopped_before_its_rebase_starts_does_not_rebase() {
        let (dir, repo, wt) = repo_and_worktree("stopped-before-rebase");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");
        let before = sh_git(&wt, &["rev-parse", "HEAD"]);
        let run = GateRun {
            state: Arc::new((Mutex::new(Inner::default()), Condvar::new())),
            repo: repo.clone(),
            base: None,
            remote: None,
            fetch_auth: None,
            fetch_lock: Arc::new(parking_lot::Mutex::new(())),
            commands: Vec::new(),
            workspace: wt.clone(),
            identifier: "MT-1".into(),
        };
        run.state.0.lock().unwrap().killed = true;

        let base = sh_git(&repo, &["rev-parse", "master"]);
        let result = run.rebase(&wt, &base, "rebase onto master".into());

        assert!(matches!(result, Err(Verdict::Failed { on_base: false, .. })), "got {result:?}");
        assert_eq!(sh_git(&wt, &["rev-parse", "HEAD"]), before, "the branch must not move");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guard for #177: the agent resolved a conflict by merging the base, and the base then
    /// moved on in a file nobody touched. A rebase would drop that merge and replay the agent's
    /// commit into the same conflict; the gate merges the new base instead.
    #[test]
    fn a_conflict_resolved_by_a_merge_is_not_replayed_when_the_base_moves_again() {
        let (dir, repo, wt) = repo_and_worktree("merged-stale-base");
        commit(&wt, "base.txt", "agent's version\n", "agent edits base");
        commit(&repo, "base.txt", "master's version\n", "master edits base");
        assert!(git(&wt, &["merge", "-q", "master"]).is_err(), "the merge must conflict");
        std::fs::write(wt.join("base.txt"), "both versions\n").unwrap();
        sh_git(&wt, &["add", "base.txt"]);
        sh_git(&wt, &["commit", "-q", "--no-edit"]);
        let resolved = sh_git(&wt, &["rev-parse", "HEAD"]);
        commit(&repo, "second.txt", "second\n", "master moves again, elsewhere");

        let gate = GitGate::new(&repo, Some("master".into()), vec![]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::Passed { base_updated: true });
        assert!(wt.join("second.txt").exists(), "the branch must now sit on master's tip");
        assert_eq!(std::fs::read_to_string(wt.join("base.txt")).unwrap(), "both versions\n");
        assert!(
            git(&wt, &["merge-base", "--is-ancestor", &resolved, "HEAD"]).is_ok(),
            "the resolving merge must survive, not be replayed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The merge path is only for a branch that already merged the base: one that never did is
    /// rebased as before, and keeps a linear history.
    #[test]
    fn a_branch_without_a_merge_is_still_rebased() {
        let (dir, repo, wt) = repo_and_worktree("no-merge");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "moved.txt", "moved\n", "master moves");

        let gate = GitGate::new(&repo, Some("master".into()), vec![]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::Passed { base_updated: true });
        let master = sh_git(&repo, &["rev-parse", "master"]);
        assert_eq!(sh_git(&wt, &["rev-parse", "HEAD^"]), master, "replayed onto master's tip");
        assert_eq!(sh_git(&wt, &["rev-list", "--merges", "HEAD"]), "", "and no merge made");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worktree the agent left mid-merge is not the gate's to abort: aborting it would throw
    /// away the agent's merge and report its unresolved paths as a conflict the gate raised.
    #[test]
    fn a_worktree_left_mid_merge_is_stuck_rather_than_aborted() {
        let (dir, repo, wt) = repo_and_worktree("left-mid-merge");
        commit(&wt, "base.txt", "agent's version\n", "agent edits base");
        commit(&repo, "base.txt", "master's version\n", "master edits base");
        assert!(git(&wt, &["merge", "-q", "master"]).is_err(), "the merge must conflict");

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert!(matches!(verdict, Verdict::Stuck { .. }), "{verdict:?}");
        assert!(
            git(&wt, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_ok(),
            "the agent's merge must be left as it was"
        );
        assert!(!wt.join("gate-ran").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A merge that conflicts is handled as a rebase that conflicts: aborted, the branch left
    /// where the agent made it, and the paths named.
    #[test]
    fn a_conflicting_merge_of_the_base_is_aborted_and_names_the_conflicted_paths() {
        let (dir, repo, wt) = repo_and_worktree("merge-conflict");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "first.txt", "first\n", "master moves once");
        sh_git(&wt, &["merge", "-q", "--no-edit", "master"]);
        commit(&wt, "first.txt", "agent's first\n", "agent edits first");
        commit(&repo, "first.txt", "master's first\n", "master edits first");
        let before = sh_git(&wt, &["rev-parse", "HEAD"]);

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(
            verdict,
            Verdict::Conflict {
                paths: vec!["first.txt".into()],
                base_sha: sh_git(&repo, &["rev-parse", "master"]),
            }
        );
        assert_eq!(sh_git(&wt, &["rev-parse", "HEAD"]), before, "the branch is left as it was");
        assert!(
            git(&wt, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_err(),
            "no merge may be left in progress"
        );
        assert!(!wt.join("gate-ran").exists(), "a conflict stops everything downstream");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_conflicting_rebase_is_aborted_and_names_the_conflicted_paths() {
        let (dir, repo, wt) = repo_and_worktree("conflict");
        commit(&wt, "base.txt", "agent's version\n", "agent edits base");
        commit(&repo, "base.txt", "master's version\n", "master edits base");
        let before = sh_git(&wt, &["rev-parse", "HEAD"]);

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(
            verdict,
            Verdict::Conflict {
                paths: vec!["base.txt".into()],
                base_sha: sh_git(&repo, &["rev-parse", "master"]),
            }
        );
        assert_eq!(
            sh_git(&wt, &["rev-parse", "HEAD"]),
            before,
            "the branch must be left as the agent made it"
        );
        assert!(
            git(&wt, &["rev-parse", "--verify", "--quiet", "REBASE_HEAD"]).is_err(),
            "no rebase may be left in progress"
        );
        assert!(!wt.join("gate-ran").exists(), "a conflict stops everything downstream");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe `Verdict::Stuck` rests on: it must see a rebase stopped on a conflict, and
    /// stop seeing it once the rebase is aborted — otherwise every conflict would read as stuck,
    /// or a stuck one as a clean hand-back.
    #[test]
    fn a_worktree_left_mid_rebase_is_detected_and_an_aborted_one_is_not() {
        let (dir, repo, wt) = repo_and_worktree("mid-rebase");
        commit(&wt, "base.txt", "agent's version\n", "agent edits base");
        commit(&repo, "base.txt", "master's version\n", "master edits base");
        let run = GateRun {
            state: Arc::new((Mutex::new(Inner::default()), Condvar::new())),
            repo: repo.clone(),
            base: None,
            remote: None,
            fetch_auth: None,
            fetch_lock: Arc::new(parking_lot::Mutex::new(())),
            commands: Vec::new(),
            workspace: wt.clone(),
            identifier: "MT-1".into(),
        };
        assert!(!run.rebase_in_progress(&wt), "a clean worktree is not mid-rebase");
        assert!(git(&wt, &["rebase", "master"]).is_err(), "the rebase must stop on the conflict");
        assert!(run.rebase_in_progress(&wt), "a stopped rebase is in progress");
        git(&wt, &["rebase", "--abort"]).unwrap();
        assert!(!run.rebase_in_progress(&wt), "an aborted one is not");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_command_reports_its_output_and_stops_the_sequence() {
        let (dir, repo, wt) = repo_and_worktree("failing-command");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");

        let gate = GitGate::new(
            &repo,
            Some("master".into()),
            vec![
                argv(&["git", "rev-parse", "--verify", "no-such-ref-anywhere"]),
                argv(&["touch", "second-ran"]),
            ],
        );
        let verdict = wait(&gate.start(&issue(), &wt));

        match verdict {
            Verdict::Failed { step, output, on_base } => {
                assert_eq!(step, "git rev-parse --verify no-such-ref-anywhere");
                // A configured command, despite the name: commands run only after the rebase
                // has completed, so this failure really is one the agent fixes on the rebased
                // tree. The base-resolution case is the one that reads `false`.
                assert!(on_base, "a command failure happens on the rebased tree");
                assert!(
                    output.contains("fatal"),
                    "git's own stderr must reach the agent, got: {output}"
                );
                assert!(output.contains("exited with"), "and so must the exit status: {output}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!wt.join("second-ran").exists(), "the first failure ends the gate");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_branch_with_nothing_beyond_the_base_is_not_gated() {
        let (dir, repo, wt) = repo_and_worktree("no-commits");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["touch", "gate-ran"])]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::NoCommits);
        assert!(!wt.join("gate-ran").exists(), "there is nothing to hand off, so nothing to check");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dirty_worktree_is_a_failure_the_agent_can_fix_not_a_conflict() {
        let (dir, repo, wt) = repo_and_worktree("dirty");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");
        std::fs::write(wt.join("agent.txt"), "uncommitted edit\n").unwrap();

        let gate = GitGate::new(&repo, Some("master".into()), vec![]);
        let verdict = wait(&gate.start(&issue(), &wt));

        match verdict {
            Verdict::Failed { step, .. } => assert_eq!(step, "rebase onto master"),
            other => panic!("expected Failed at the rebase, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unresolvable_base_fails_rather_than_rebasing_onto_nothing() {
        let (dir, repo, wt) = repo_and_worktree("bad-base");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");

        let gate = GitGate::new(&repo, Some("no-such-branch".into()), vec![]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert!(
            matches!(&verdict, Verdict::Failed { step, .. } if step.starts_with("resolve base")),
            "got {verdict:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_base_means_the_repositorys_own_head() {
        let (dir, repo, wt) = repo_and_worktree("head-base");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");

        let gate = GitGate::new(&repo, None, vec![]);
        let verdict = wait(&gate.start(&issue(), &wt));

        assert_eq!(verdict, Verdict::Passed { base_updated: true });
        assert!(wt.join("from_master.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn killing_a_running_gate_stops_its_command_and_reports_a_failure() {
        let (dir, repo, wt) = repo_and_worktree("kill");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");

        let gate = GitGate::new(&repo, Some("master".into()), vec![argv(&["sleep", "60"])]);
        let h = gate.start(&issue(), &wt);
        let deadline = Instant::now() + Duration::from_secs(30);
        while h.step() != "sleep 60" {
            assert!(Instant::now() < deadline, "the gate never reached its command");
            std::thread::sleep(Duration::from_millis(10));
        }

        let started = Instant::now();
        let result = h.kill(5_000);
        assert!(started.elapsed() < Duration::from_secs(10), "kill must not wait the sleep out");
        assert!(matches!(result, KillResult::Stopped | KillResult::Forced), "got {result:?}");
        assert!(matches!(h.finished(), Some(Verdict::Failed { .. })), "a killed gate did not pass");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guard for the review finding that `pgid` only ever named a configured gate command:
    /// before this, `kill` had nothing to signal while base resolution, commit counting or the
    /// rebase itself was the thing actually running, so it would wait out its grace period and
    /// return having stopped nothing — free to keep mutating the worktree the scheduler was
    /// about to delete out from under it. A `pre-rebase` hook runs synchronously *inside* `git
    /// rebase`, which is what lets this block that one subprocess for a long time without
    /// changing what `GateRun` itself invokes.
    #[test]
    fn killing_a_gate_during_the_rebase_step_stops_the_git_subprocess_instead_of_leaving_it_running()
     {
        let (dir, repo, wt) = repo_and_worktree("kill-rebase");
        commit(&wt, "agent.txt", "agent\n", "the agent's work");
        commit(&repo, "from_master.txt", "moved\n", "master moved on");

        let hooks = dir.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let pid_file = dir.join("prerebase.pid");
        let done_file = dir.join("prerebase.done");
        std::fs::write(
            hooks.join("pre-rebase"),
            format!(
                "#!/bin/sh\necho $$ > '{}'\nsleep 60\ntouch '{}'\n",
                pid_file.display(),
                done_file.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                hooks.join("pre-rebase"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        // `core.hooksPath` is repo-wide config, shared with the worktree rather than scoped to
        // it, so setting it once here reaches the rebase the gate runs in `wt`.
        sh_git(&repo, &["config", "core.hooksPath", &hooks.display().to_string()]);

        let gate = GitGate::new(&repo, Some("master".into()), vec![]);
        let h = gate.start(&issue(), &wt);
        let deadline = Instant::now() + Duration::from_secs(30);
        while h.step() != "rebase onto master" {
            assert!(Instant::now() < deadline, "the gate never reached the rebase step");
            std::thread::sleep(Duration::from_millis(10));
        }
        while !pid_file.exists() {
            assert!(Instant::now() < deadline, "the pre-rebase hook never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        let hook_pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(
            kill(Pid::from_raw(hook_pid), None).is_ok(),
            "the hook must actually be alive to prove anything by killing it"
        );

        let started = Instant::now();
        let result = h.kill(5_000);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "kill must not wait the hook's sleep out"
        );
        assert!(matches!(result, KillResult::Stopped | KillResult::Forced), "got {result:?}");
        assert!(matches!(h.finished(), Some(Verdict::Failed { .. })), "a killed gate did not pass");
        assert!(
            !kill(Pid::from_raw(hook_pid), None).is_ok(),
            "the git subprocess and its hook must actually be gone, not merely abandoned"
        );
        assert!(!done_file.exists(), "the hook must have been stopped before it could finish");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guard for the review finding that `Gate::start` broke its own doc comment: an
    /// `.expect()` on the supervising thread's spawn turned "the OS briefly refused a thread"
    /// into a panicked daemon and a claim stranded for `recover()`, instead of the
    /// `Verdict::Failed` the trait promises. `stack_size(usize::MAX)` asks for a stack no
    /// allocator can satisfy, which fails deterministically without touching a process-wide
    /// thread limit — this binary runs many tests concurrently, and exhausting a real limit
    /// would flake whichever of them happened to be spawning a thread at the same moment.
    #[test]
    fn a_gate_whose_supervising_thread_cannot_be_spawned_reports_failed_instead_of_panicking() {
        let state: Shared = Arc::new((Mutex::new(Inner::default()), Condvar::new()));
        let run = GateRun {
            state: state.clone(),
            repo: PathBuf::from("/nonexistent"),
            base: None,
            remote: None,
            fetch_auth: None,
            fetch_lock: Arc::new(parking_lot::Mutex::new(())),
            commands: vec![],
            workspace: PathBuf::from("/nonexistent"),
            identifier: "MT-1".into(),
        };
        let builder = std::thread::Builder::new().name("gate-test".into()).stack_size(usize::MAX);

        spawn_gate_thread(builder, &state, run);

        let verdict = state.0.lock().unwrap().verdict.clone();
        assert!(
            matches!(&verdict, Some(Verdict::Failed { step, .. }) if step == "start gate"),
            "got {verdict:?}"
        );
    }

    #[test]
    fn long_output_keeps_its_tail_where_the_summary_is() {
        let long = format!("{}\nTHE END", "x".repeat(OUTPUT_CAP * 2));
        let t = tail(&long, OUTPUT_CAP);
        assert!(t.ends_with("THE END"));
        assert!(t.starts_with("[... "), "must say what was dropped");
        assert!(t.len() < OUTPUT_CAP + 64);
    }
}
