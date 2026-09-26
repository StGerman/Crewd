//! `crewctl status` as a real process, against a real `Api` over a real socket, while the
//! daemon's database is held (#45).
//!
//! A fake here would hide the exact failure the split exists to prevent: a second handle on a
//! database the daemon holds. So the test takes an exclusive SQLite lock on the daemon's file,
//! points `crewctl` at that file every way it could look for one — the environment variable
//! and the working directory — and requires the query to succeed anyway. It can only succeed
//! because nothing in `crewctl` opens a store.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use crew::api::Api;
use crew::clock::FakeClock;
use crew::config::{AgentConfig, Config, PollingConfig, TrackerConfig, WorkspaceConfig};
use crew::model::Issue;
use crew::project::NoopProjector;
use crew::sched::{Scheduler, Snapshot};
use crew::store::Store;
use crew::tracker::fake::FakeTracker;
use crew::worker::fake::FakeWorker;
use crew::workspace::DirWorkspace;
use tokio::sync::{mpsc, watch};

fn issue(n: u32) -> Issue {
    Issue {
        id: format!("iss-{n}"),
        identifier: format!("MT-{n}"),
        title: format!("issue {n}"),
        body: None,
        state: "in progress".into(),
        priority: Some(1),
        url: None,
        labels: vec!["agent".into()],
        dispatchable: true,
        created_at: Some(1_000 + n as i64),
        native_ref: None,
        blocked_by: vec![],
    }
}

fn scheduler(root: &Path, db: &Path) -> Scheduler {
    let cfg = Config {
        tracker: TrackerConfig {
            kind: "fake".into(),
            active_states: vec!["in progress".into()],
            terminal_states: vec!["done".into()],
            required_labels: vec![],
            owner: String::new(),
            repo: String::new(),
            ..Default::default()
        },
        polling: PollingConfig { interval_ms: 30_000 },
        workspace: WorkspaceConfig { root: Some(root.join("workspaces")), repo: None },
        broker: Default::default(),
        agent: AgentConfig::default(),
        worker: Default::default(),
        workers: Default::default(),
        api: Default::default(),
        transcripts: Default::default(),
        gate: Default::default(),
        delivery: Default::default(),
    };
    let clock = Arc::new(FakeClock::new());
    Scheduler::new(
        cfg,
        clock.clone(),
        Store::open(db).unwrap(),
        Arc::new(FakeTracker::new(vec![issue(7)])),
        Arc::new(FakeWorker::new(clock)),
        Arc::new(DirWorkspace::new(root.join("workspaces")).unwrap()),
        Arc::new(NoopProjector),
    )
}

async fn crewctl(cwd: &Path, db: &Path, args: &[&str]) -> Output {
    let (cwd, db) = (cwd.to_path_buf(), db.to_path_buf());
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_crewctl"))
            .args(&args)
            .current_dir(&cwd)
            .env("CREW_DB", &db)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

/// What a person reads, with the parts that differ per run — the port the OS handed out and the
/// temporary root, in both its given and canonical spelling — named rather than printed.
/// Everything else comes from the fake clock and is stable.
fn stdout(out: &Output, addr: &str, root: &Path) -> String {
    let mut text = String::from_utf8_lossy(&out.stdout).replace(addr, "[ADDR]");
    if let Ok(canonical) = root.canonicalize() {
        text = text.replace(&canonical.display().to_string(), "[ROOT]");
    }
    text.replace(&root.display().to_string(), "[ROOT]")
}

#[tokio::test]
async fn a_status_query_does_not_open_the_database_the_daemon_holds() {
    let root: PathBuf = std::env::temp_dir().join(format!("crewctl-status-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("crew.db");

    // The daemon's side: a scheduler on a real database file, one tick, one published snapshot.
    let mut sched = scheduler(&root, &db);
    sched.tick().unwrap();
    let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
    snap_tx.send(sched.snapshot().unwrap()).unwrap();
    let (cmd_tx, _commands) = mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(Api::new(snap_rx, cmd_tx).serve(listener));

    // The published snapshot now lives in the watch channel, so the scheduler's own connection
    // closes and an exclusive lock takes its place: stricter than the daemon's hold, and any
    // process that opens this file from here on gets SQLITE_BUSY.
    drop(sched);
    let lock = rusqlite::Connection::open(&db).unwrap();
    lock.execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE;").unwrap();
    let intruder = rusqlite::Connection::open(&db).unwrap();
    intruder.busy_timeout(std::time::Duration::ZERO).unwrap();
    assert!(
        intruder.query_row("SELECT count(*) FROM issue_state", [], |r| r.get::<_, i64>(0)).is_err(),
        "the lock must refuse a second reader, or this test proves nothing"
    );
    drop(intruder);

    let all = crewctl(&root, &db, &["status", "--api", &addr]).await;
    assert!(all.status.success(), "stderr: {}", String::from_utf8_lossy(&all.stderr));
    insta::assert_snapshot!("status_all", stdout(&all, &addr, &root));

    let one = crewctl(&root, &db, &["status", "--api", &addr, "MT-7"]).await;
    assert!(one.status.success(), "stderr: {}", String::from_utf8_lossy(&one.stderr));
    insta::assert_snapshot!("status_one", stdout(&one, &addr, &root));

    drop(lock);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_closed_port_reads_as_no_daemon_from_the_binary_too() {
    // `a_closed_port_reads_as_no_daemon_rather_than_a_refused_request` holds for the library;
    // this is the same distinction as an operator meets it, through the process and its exit.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap().to_string();
    drop(probe);

    let dir = std::env::temp_dir();
    let out = crewctl(&dir, &dir.join("absent.db"), &["status", "--api", &addr]).await;
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr).replace(&addr, "[ADDR]");
    insta::assert_snapshot!("status_no_daemon", err);
}
