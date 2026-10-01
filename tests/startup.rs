//! Startup, end to end through the `crewd` binary (#218).
//!
//! A component the config turns on either starts or crewd exits non-zero naming it; these run
//! the real binary because the defect was in `main`'s wiring, which no scheduler test reaches.
//! Each test runs in its own scratch git repository, never this checkout: `GitWorktreeWorkspace`
//! refuses a daemon started inside a worktree.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const BASE: &str = r#"
[tracker]
kind = "fake"
active_states = ["In Progress"]
terminal_states = ["Done"]

[polling]
interval_ms = 50

[workspace]
root = "ws"
"#;

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(config: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crewd-startup-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "master"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
        ] {
            let ok = Command::new("git").args(args).current_dir(&dir).status().unwrap().success();
            assert!(ok, "git {args:?}");
        }
        std::fs::write(dir.join("crew.toml"), config).unwrap();
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_crewd"));
        cmd.args(["--config", "crew.toml", "--max-ticks", "1"])
            .args(args)
            .current_dir(&self.dir)
            .env("CREW_DB", self.dir.join("crew.db"))
            .env("CREW_TASKS_ROOT", self.dir.join("tasks"))
            .env("RUST_LOG", "crew=warn");
        cmd
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Stderr with what differs per run and per OS taken out: the ephemeral address, and the errno
/// `EADDRINUSE` is on this platform.
fn diagnostic(out: &Output, addr: &str) -> String {
    let err = stderr(out).replace(addr, "[ADDR]");
    match err.find(" (os error") {
        Some(i) => format!("{}{}", &err[..i], err[i..].split_once(')').map_or("", |(_, t)| t)),
        None => err,
    }
}

#[test]
fn a_taken_ops_api_port_stops_startup_naming_it() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let scratch = Scratch::new(BASE);

    let out = scratch.run(&["--api", &addr]);

    assert!(!out.status.success(), "crewd started with its ops API port taken");
    insta::assert_snapshot!(diagnostic(&out, &addr));
    assert!(!scratch.dir.join("crew.db").exists(), "the store was opened before the bind");
}

#[test]
fn a_taken_ops_mcp_port_stops_startup_naming_it() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let scratch = Scratch::new(BASE);

    let out = scratch.run(&["--mcp", &addr]);

    assert!(!out.status.success(), "crewd started with its ops MCP port taken");
    insta::assert_snapshot!(diagnostic(&out, &addr));
}

#[test]
fn a_worker_binary_that_cannot_be_resolved_stops_startup() {
    let scratch = Scratch::new(&format!(
        "{BASE}\n[[workers]]\nname = \"slow\"\nkind = \"grok\"\nbin = \"crew-no-such-binary-218\"\nmax_concurrent = 1\n"
    ));

    let out = scratch.run(&[]);

    assert!(!out.status.success(), "crewd started with a worker binary it cannot resolve");
    insta::assert_snapshot!(stderr(&out));
}

#[test]
fn a_component_the_config_leaves_off_is_not_required_to_start() {
    // 8787 is `api.bind`'s default and may already be the dogfooding daemon's; either way it is
    // taken for the length of this test.
    let _held = TcpListener::bind("127.0.0.1:8787");
    let scratch = Scratch::new(&format!(
        "{BASE}\n[api]\nenabled = false\nbind = \"127.0.0.1:8787\"\nmcp_enabled = false\n"
    ));

    let out = scratch.run(&[]);

    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn a_transcript_root_that_cannot_be_created_stops_startup_naming_it() {
    let scratch = Scratch::new(&format!(
        "{BASE}\n[transcripts]\nenabled = true\nroot = \"blocker/transcripts\"\n"
    ));
    std::fs::write(scratch.dir.join("blocker"), "a file where a directory has to go").unwrap();

    let out = scratch.run(&[]);

    assert!(!out.status.success(), "crewd started with no transcript root");
    // Up to the OS's own wording, which differs between macOS and Linux for this errno.
    let err = stderr(&out);
    insta::assert_snapshot!(err.split("\n\nCaused by").next().unwrap_or(&err));
}

#[test]
fn a_running_daemons_startup_holds_the_store_lock_a_second_crewd_refuses() {
    let scratch = Scratch::new(BASE);
    let db = scratch.dir.join("crew.db");
    let holder_err = scratch.dir.join("holder.err");
    let mut holder = Command::new(env!("CARGO_BIN_EXE_crewd"))
        .args(["--config", "crew.toml", "--max-ticks", "1000"])
        .current_dir(&scratch.dir)
        .env("CREW_DB", &db)
        .env("CREW_TASKS_ROOT", scratch.dir.join("tasks"))
        .env("RUST_LOG", "crew=error")
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&holder_err).unwrap()))
        .spawn()
        .unwrap();
    let mut holder = KillOnDrop(&mut holder);
    let pid = holder.child().id();

    // The pid line is written only after `flock` succeeds. A second start before that can take
    // the lock itself and make this assertion race.
    wait_until_held(&db, pid, &holder_err);
    let out = scratch.command(&[]).env("CREW_DB", &db).output().unwrap();
    let refused = stderr(&out);
    let canonical = db.parent().unwrap().canonicalize().unwrap().join("crew.db");
    let expected = format!("store {} is held by pid {pid}", canonical.display());
    assert!(
        !out.status.success() && refused.contains(&expected),
        "startup did not refuse the second daemon with the holder's pid:\n{refused}\nholder:\n{}",
        std::fs::read_to_string(&holder_err).unwrap_or_default()
    );

    drop(holder);
}

fn wait_until_held(db: &Path, holder: u32, holder_err: &Path) {
    let lock = db.with_file_name("crew.db.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::fs::read_to_string(&lock).unwrap_or_default().trim() == holder.to_string() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "holder {holder} never recorded its pid in {}; holder stderr:\n{}",
        lock.display(),
        std::fs::read_to_string(holder_err).unwrap_or_default()
    );
}

struct KillOnDrop<'a>(&'a mut Child);

impl KillOnDrop<'_> {
    fn child(&mut self) -> &mut Child {
        self.0
    }
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_worker_binary_in_the_cwd_is_found_through_an_empty_path_entry() {
    // Trailing `:` is the cwd to `execvp`, so a check that dropped the empty entry would refuse
    // a binary every shell finds.
    let scratch = Scratch::new(&format!(
        "{BASE}\n[[workers]]\nname = \"here\"\nkind = \"grok\"\nbin = \"crew-tool-218\"\nmax_concurrent = 1\n"
    ));
    let tool = scratch.dir.join("crew-tool-218");
    std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:", std::env::var("PATH").unwrap_or_default());

    let out = scratch.command(&[]).env("PATH", path).output().unwrap();

    assert!(out.status.success(), "{}", stderr(&out));
}
