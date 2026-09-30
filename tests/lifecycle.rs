//! The daemon's process lifecycle, driven through the real `crewd` binary: what the scheduler
//! tests cannot reach, because a signal is delivered to a process, not to `Scheduler::tick()`.
//!
//! A tick is held open by a `post-checkout` hook in the repo worktrees are created from: it
//! touches a marker and then sleeps, so the test knows a tick is in progress when it signals.
//! Without the hook the fake tracker's tick lasts milliseconds and a signal would almost always
//! land between ticks, which is the case that worked before #215.
//!
//! `wait_for` calls `Instant::now` and `thread::sleep` because the thing it waits on is a real
//! process. This file is a named exception for that in `docs/coding-guidelines.md`; the
//! `disallowed_methods` allow on these calls lands with #52.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// Kills the daemon if an assertion fails first, so a regression costs a red test rather than a
/// `crewd` left running on the machine.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tmp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("crew-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.canonicalize().unwrap()
}

fn git(at: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(at).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

/// A repo whose every checkout, `git worktree add` included, marks `marker` and then blocks for
/// `hold`: the part of a tick that `prepare` spends in git.
fn slow_checkout_repo(at: &Path, marker: &Path, hold: Duration) {
    std::fs::create_dir_all(at).unwrap();
    git(at, &["init", "-q", "-b", "main"]);
    git(at, &["config", "user.email", "test@example.com"]);
    git(at, &["config", "user.name", "test"]);
    git(at, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let hook = at.join(".git/hooks/post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\nsleep {}\n", marker.display(), hold.as_secs()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "timed out after {limit:?} waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts `crewd` against the fake tracker and worker, sends `sig` once while a tick is blocked
/// in `git worktree add`, and returns the daemon's log once it has exited.
fn one_signal_during_a_tick(tag: &str, sig: Signal) -> String {
    let dir = tmp_dir(tag);
    let marker = dir.join("tick-in-progress");
    let repo = dir.join("repo");
    slow_checkout_repo(&repo, &marker, Duration::from_secs(2));
    let config = dir.join("crew.toml");
    std::fs::write(
        &config,
        format!(
            "[tracker]\nkind = \"fake\"\nactive_states = [\"In Progress\"]\n\
             terminal_states = [\"Done\"]\n\n[polling]\ninterval_ms = 200\n\n\
             [workspace]\nroot = '{}'\nrepo = '{}'\n\n[agent]\nmax_concurrent = 1\n",
            dir.join("workspaces").display(),
            repo.display(),
        ),
    )
    .unwrap();
    let log = dir.join("crewd.log");

    let mut daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_crewd"))
            .arg("--config")
            .arg(&config)
            .current_dir(&dir)
            .env("CREW_DB", dir.join("crew.db"))
            .env("CREW_TASKS_ROOT", dir.join("tasks"))
            .env("RUST_LOG", "crew=info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );

    wait_for("a tick to reach git worktree add", Duration::from_secs(30), || marker.exists());
    kill(Pid::from_raw(daemon.0.id() as i32), sig).unwrap();

    let mut status = None;
    wait_for("crewd to exit after one signal", Duration::from_secs(30), || {
        status = daemon.0.try_wait().unwrap();
        status.is_some()
    });
    let log = std::fs::read_to_string(&log).unwrap();
    assert!(status.unwrap().success(), "crewd exited with {status:?}; log:\n{log}");
    log
}

#[test]
fn one_sigint_during_a_tick_stops_the_daemon_through_shutdown() {
    let log = one_signal_during_a_tick("sigint-mid-tick", Signal::SIGINT);
    assert!(log.contains("interrupt received; shutting down"), "log:\n{log}");
}

#[test]
fn one_sigterm_during_a_tick_stops_the_daemon_through_shutdown() {
    let log = one_signal_during_a_tick("sigterm-mid-tick", Signal::SIGTERM);
    assert!(log.contains("interrupt received; shutting down"), "log:\n{log}");
}
