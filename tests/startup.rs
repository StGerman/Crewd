//! Startup, end to end through the `crewd` binary (#218).
//!
//! A component the config turns on either starts or crewd exits non-zero naming it; these run
//! the real binary because the defect was in `main`'s wiring, which no scheduler test reaches.
//! Each test runs in its own scratch git repository, never this checkout: `GitWorktreeWorkspace`
//! refuses a daemon started inside a worktree.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

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
        Command::new(env!("CARGO_BIN_EXE_crewd"))
            .args(["--config", "crew.toml", "--max-ticks", "1"])
            .args(args)
            .current_dir(&self.dir)
            .env("CREW_DB", self.dir.join("crew.db"))
            .env("CREW_TASKS_ROOT", self.dir.join("tasks"))
            .env("RUST_LOG", "crew=warn")
            .output()
            .unwrap()
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

#[test]
fn a_taken_ops_api_port_stops_startup_naming_it() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let scratch = Scratch::new(BASE);

    let out = scratch.run(&["--api", &addr]);

    assert!(!out.status.success(), "crewd started with its ops API port taken");
    assert!(stderr(&out).contains(&format!("binding the ops API to {addr}")), "{}", stderr(&out));
    assert!(!scratch.dir.join("crew.db").exists(), "the store was opened before the bind");
}

#[test]
fn a_taken_ops_mcp_port_stops_startup_naming_it() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let scratch = Scratch::new(BASE);

    let out = scratch.run(&["--mcp", &addr]);

    assert!(!out.status.success(), "crewd started with its ops MCP port taken");
    assert!(stderr(&out).contains(&format!("binding the ops MCP server to {addr}")));
}

#[test]
fn a_worker_binary_that_cannot_be_resolved_stops_startup() {
    let scratch = Scratch::new(&format!(
        "{BASE}\n[[workers]]\nname = \"slow\"\nkind = \"grok\"\nbin = \"crew-no-such-binary-218\"\nmax_concurrent = 1\n"
    ));

    let out = scratch.run(&[]);

    assert!(!out.status.success(), "crewd started with a worker binary it cannot resolve");
    let err = stderr(&out);
    assert!(err.contains("\"slow\"") && err.contains("crew-no-such-binary-218"), "{err}");
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
