//! Live end-to-end check of the tool broker against a real `claude` process.
//!
//! Everything in the broker's own test suite stops at this crate's edge: the transport tests
//! drive a socket this crate also wrote, and the tool tests drive a fake tracker. Neither can
//! tell you that the *real* CLI parses the `--mcp-config` this crate writes, negotiates the
//! handshake this crate answers, and calls a tool by the name this crate registers. That is
//! exactly the class of bug slice 4 hit twice (`--max-turns` does not exist, `--bare` needs an
//! API key), so it gets an executable check rather than an assumption.
//!
//! ```bash
//! cargo run --example broker_live
//! ```
//!
//! Spends real tokens and needs a working `claude` login. Writes nothing to any tracker — the
//! broker is wired to [`FakeWrites`], so the only thing it proves is that the call arrived.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crew::broker::fake::FakeWrites;
use crew::broker::{self, Broker, BrokerLimits, TrackerWrites};
use crew::clock::{Clock, SystemClock};
use crew::model::Issue;
use crew::worker::claude::{ClaudeWorker, DEFAULT_ENV_ALLOWLIST};
use crew::worker::{Session, Spawn, Worker};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter("crew=debug").init();

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let writes = Arc::new(FakeWrites::new());
    let w: Arc<dyn TrackerWrites> = writes.clone();

    let listener = broker::server::bind()?;
    let addr = listener.local_addr()?;
    let dir = std::env::temp_dir().join(format!("crew-mcp-live-{}", std::process::id()));
    let broker = Arc::new(Broker::new(
        w,
        clock,
        BrokerLimits::default(),
        vec!["in progress".into(), "done".into()],
        addr,
        &dir,
    )?);
    broker::server::serve(Arc::clone(&broker), listener)?;
    println!("broker listening on {addr}");

    // The prompt is the issue, so the instruction to call the tool goes in the body the worker
    // builds its prompt from — same path a dispatched issue takes.
    let issue = Issue {
        id: "live/check#1".into(),
        identifier: "LIVE-1".into(),
        title: "Broker smoke test".into(),
        body: Some(
            "Do not read or modify any files, and do not run any commands. Call the \
             mcp__crew__comment tool exactly once with the body 'live broker check', then \
             stop and report what it returned."
                .into(),
        ),
        state: "In Progress".into(),
        priority: None,
        url: None,
        labels: vec![],
        dispatchable: true,
        created_at: None,
        native_ref: None,
        blocked_by: vec![],
    };

    let workspace = std::env::temp_dir().join(format!("crew-live-ws-{}", std::process::id()));
    std::fs::create_dir_all(&workspace)?;

    let session = broker.open(&issue, "live-run")?;
    println!("mcp config: {}", session.endpoint().config_path.display());

    let worker = ClaudeWorker::new(
        "claude",
        DEFAULT_ENV_ALLOWLIST.iter().map(|s| s.to_string()).collect(),
        10,
    );
    // A real transcript for a real run: this is the one place in the crate where the stream
    // comes from the actual CLI, so it is also the best place to see what a transcript of one
    // looks like.
    let transcripts = crew::transcript::Transcripts::new(
        &std::env::temp_dir().join("crew-live-transcripts"),
        8 << 20,
        10,
    )?;
    let transcript = transcripts.open("broker-live");
    if let Some(t) = &transcript {
        println!("transcript: {}", t.path().display());
    }

    let handle = worker.spawn(Spawn {
        tools: Some(session.endpoint()),
        transcript,
        ..Spawn::new(&issue, &workspace, 0, &Session::New(crew::model::session_id("live", 1)))
    });

    let deadline = Instant::now() + Duration::from_secs(180);
    let outcome = loop {
        if let Some(o) = handle.finished() {
            break o;
        }
        if Instant::now() > deadline {
            handle.kill(5_000);
            anyhow::bail!("no verdict within 180s");
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    println!("\n--- outcome: {outcome:?}");
    println!("--- tracker writes: {:#?}", writes.writes());
    for e in broker.audit() {
        println!("--- audit: {} {} {} {}", e.tool, e.outcome.as_str(), e.issue_id, e.detail);
    }

    let ok = writes.count() == 1;
    drop(session);
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&dir);

    anyhow::ensure!(ok, "expected exactly one tracker write, got {}", writes.count());
    println!("\nOK: the real CLI reached the broker and the orchestrator performed the write.");
    Ok(())
}
