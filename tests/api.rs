//! The ops HTTP API, end to end over a real socket against a real `Scheduler`.
//!
//! The harness below is `main`'s loop with the timer taken out: the same command channel, the
//! same "publish, then answer" ordering, a real `Api` on a real loopback port, and a scheduler
//! whose clock only moves when a test moves it. Nothing here fakes the HTTP layer, because the
//! things most likely to break — framing, closing the connection, a request that never
//! finishes — are invisible to a fake.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crew::api::client::{Client, Endpoint, Source, StatusError};
use crew::api::mcp::{self, OpsMcp};
use crew::api::{Api, Command};
use crew::broker::fake::FakeWrites;
use crew::broker::{self, Broker, BrokerLimits, TrackerWrites};
use crew::clock::FakeClock;
use crew::config::{AgentConfig, Config, PollingConfig, TrackerConfig, WorkspaceConfig};
use crew::model::{ErrorClass, Issue, Outcome};
use crew::project::NoopProjector;
use crew::sched::{Scheduler, Snapshot};
use crew::store::Store;
use crew::tracker::fake::FakeTracker;
use crew::worker::fake::{FakeWorker, Script};
use crew::workspace::DirWorkspace;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

struct Harness {
    addr: SocketAddr,
    /// The ops MCP server, on its own listener — as `main` wires it, never the HTTP API's port
    /// and never the broker's.
    mcp_addr: SocketAddr,
    sched: Scheduler,
    clock: Arc<FakeClock>,
    worker: Arc<FakeWorker>,
    snap_tx: watch::Sender<Snapshot>,
    commands: mpsc::UnboundedReceiver<Command>,
    root: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn issue(n: u32, state: &str) -> Issue {
    Issue {
        id: format!("iss-{n}"),
        identifier: format!("MT-{n}"),
        title: format!("issue {n}"),
        body: None,
        state: state.to_string(),
        priority: Some(1),
        url: Some(format!("https://example.invalid/{n}")),
        labels: vec!["agent".into()],
        dispatchable: true,
        created_at: Some(1_000 + n as i64),
        native_ref: None,
        blocked_by: vec![],
    }
}

impl Harness {
    async fn new(issues: Vec<Issue>) -> Self {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("crew-api-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

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
            workspace: WorkspaceConfig { root: Some(root.clone()), repo: None },
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
        let worker = Arc::new(FakeWorker::new(clock.clone()));
        let sched = Scheduler::new(
            cfg,
            clock.clone(),
            Store::open_in_memory().unwrap(),
            Arc::new(FakeTracker::new(issues)),
            worker.clone(),
            Arc::new(DirWorkspace::new(&root).unwrap()),
            Arc::new(NoopProjector),
        );

        let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
        let (cmd_tx, commands) = mpsc::unbounded_channel();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(Api::new(snap_rx.clone(), cmd_tx.clone()).serve(listener));

        // The same `Api` (same watch receiver, same command sender) behind the MCP surface, so
        // the two are provably one view rather than two that happen to agree today.
        let mcp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mcp_addr = mcp_listener.local_addr().unwrap();
        broker::server::serve(Arc::new(OpsMcp::new(Api::new(snap_rx, cmd_tx))), mcp_listener);

        Harness { addr, mcp_addr, sched, clock, worker, snap_tx, commands, root }
    }

    /// One MCP `tools/call` against the ops server, pumping commands the way `main`'s loop
    /// does. Returns the tool result: `(isError, parsed text body)`.
    async fn tool(&mut self, tool: &str, args: Value) -> (bool, Value) {
        let addr = self.mcp_addr;
        let tool = tool.to_string();
        let client = tokio::task::spawn_blocking(move || {
            let mut c = McpConn::open(addr, mcp::PATH);
            c.tool_call(&tool, args)
        });
        self.pump(client).await
    }

    /// Drive a blocking client to completion while applying every command it sends.
    async fn pump<T>(&mut self, mut client: tokio::task::JoinHandle<T>) -> T {
        loop {
            tokio::select! {
                Some(cmd) = self.commands.recv() => self.apply(cmd),
                done = &mut client => return done.unwrap(),
            }
        }
    }

    /// One scheduler tick, published the way the loop in `main` publishes it.
    fn tick(&mut self) {
        self.sched.tick().unwrap();
        self.snap_tx.send(self.sched.snapshot().unwrap()).unwrap();
    }

    /// Serve one request, pumping commands the way `main`'s loop does — so the write endpoints
    /// reach a real scheduler rather than a stub that agrees with them.
    async fn request(&mut self, method: &str, path: &str) -> (u16, Value) {
        let addr = self.addr;
        let raw =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n");
        let client = tokio::spawn(async move { send(addr, &raw).await });
        self.pump(client).await
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::Tick(reply) => {
                self.sched.tick().unwrap();
                let snap = self.sched.snapshot();
                if let Ok(s) = &snap {
                    self.snap_tx.send(s.clone()).unwrap();
                }
                let _ = reply.send(snap);
            }
            Command::Unquarantine { issue_id, reply } => {
                let cleared = self.sched.unquarantine(&issue_id);
                self.snap_tx.send(self.sched.snapshot().unwrap()).unwrap();
                let _ = reply.send(cleared);
            }
            Command::Unblock { issue_id, reply } => {
                let cleared = self.sched.unblock(&issue_id);
                self.snap_tx.send(self.sched.snapshot().unwrap()).unwrap();
                let _ = reply.send(cleared);
            }
        }
    }
}

/// Write one request and read until the server closes — which it always does, so a response
/// that forgot `Connection: close` would hang this rather than passing quietly.
async fn send(addr: SocketAddr, request: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8(raw).expect("responses are JSON over ASCII headers");

    let (head, body) = text.split_once("\r\n\r\n").expect("a response has a head and a body");
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .expect("a status line");

    assert!(head.contains("Content-Type: application/json"), "head was: {head}");
    assert_eq!(
        head.lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .and_then(|v| v.trim().parse::<usize>().ok()),
        Some(body.len()),
        "Content-Length must describe the body actually sent"
    );

    (status, serde_json::from_str(body).expect("every response body is JSON"))
}

/// A blocking MCP client over a real socket, for the ops server — the same keep-alive request
/// sequence the broker's transport tests drive, because the framing is the part a fake would
/// hide. It speaks JSON-RPC and nothing more: `initialize`, `tools/list`, `tools/call`.
struct McpConn {
    stream: std::net::TcpStream,
    reader: std::io::BufReader<std::net::TcpStream>,
    path: String,
    next_id: u64,
}

impl McpConn {
    fn open(addr: SocketAddr, path: &str) -> Self {
        let stream = std::net::TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        let reader = std::io::BufReader::new(stream.try_clone().unwrap());
        Self { stream, reader, path: path.to_string(), next_id: 1 }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        use std::io::{BufRead, Read, Write};
        let id = self.next_id;
        self.next_id += 1;
        let body =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        let req = format!(
            "POST {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            self.path,
            body.len()
        );
        self.stream.write_all(req.as_bytes()).unwrap();
        self.stream.flush().unwrap();

        let mut status_line = String::new();
        self.reader.read_line(&mut status_line).unwrap();
        assert!(status_line.contains(" 200 "), "status was {status_line:?}");
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse().unwrap();
            }
        }
        let mut buf = vec![0u8; len];
        self.reader.read_exact(&mut buf).unwrap();
        let v: Value = serde_json::from_slice(&buf).expect("a JSON-RPC response");
        assert_eq!(v["id"], id);
        v
    }

    fn tool_names(&mut self) -> Vec<String> {
        let list = self.rpc("tools/list", json!({}));
        list["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("no tools array in {list}"))
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// `(isError, the text content parsed as JSON — or the raw text if it is not JSON)`.
    fn tool_call(&mut self, name: &str, args: Value) -> (bool, Value) {
        let call = self.rpc("tools/call", json!({ "name": name, "arguments": args }));
        assert!(call.get("error").is_none(), "a tool failure must not be a JSON-RPC error: {call}");
        let is_error = call["result"]["isError"].as_bool().unwrap();
        let text = call["result"]["content"][0]["text"].as_str().unwrap();
        (is_error, serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into())))
    }
}

fn rows(snapshot: &Value) -> &Vec<Value> {
    snapshot["rows"].as_array().expect("rows is an array")
}

fn row<'a>(snapshot: &'a Value, identifier: &str) -> &'a Value {
    rows(snapshot)
        .iter()
        .find(|r| r["identifier"] == identifier)
        .unwrap_or_else(|| panic!("no row for {identifier}"))
}

/// Render a snapshot through the dashboard, without a terminal.
fn as_dashboard(snap: &Snapshot) -> String {
    let mut term = Terminal::new(TestBackend::new(110, 26)).unwrap();
    term.draw(|f| crew::tui::render_snapshot(f, snap, 0)).unwrap();
    term.backend().buffer().content().iter().map(|c| c.symbol()).collect()
}

#[tokio::test]
async fn the_snapshot_endpoint_serves_what_the_dashboard_renders() {
    let mut h = Harness::new(vec![issue(1, "In Progress"), issue(2, "In Progress")]).await;
    h.tick();

    let (status, body) = h.request("GET", "/api/v1/snapshot").await;
    assert_eq!(status, 200);

    // The same published snapshot reaches both surfaces, so anything the operator can read off
    // the dashboard has to be answerable from the JSON.
    let published = h.sched.snapshot().unwrap();
    let screen = as_dashboard(&published);

    assert_eq!(rows(&body).len(), published.rows.len());
    for r in &published.rows {
        assert!(screen.contains(&r.identifier), "the dashboard shows {}", r.identifier);
        let json = row(&body, &r.identifier);
        assert_eq!(json["phase"], r.phase.label());
        assert_eq!(json["tracker_state"], r.tracker_state);
        assert_eq!(json["title"], r.title);
    }
    assert_eq!(body["running"], published.running);
    assert_eq!(body["limit"], published.limit);
    assert_eq!(body["quarantined"], published.quarantined);
    assert_eq!(body["ticks"], published.ticks);
}

#[tokio::test]
async fn a_forced_refresh_advances_the_tick_count_by_exactly_one() {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();

    let (_, before) = h.request("GET", "/api/v1/snapshot").await;
    let (status, after) = h.request("POST", "/api/v1/refresh").await;
    assert_eq!(status, 200);
    assert_eq!(
        after["ticks"].as_u64().unwrap(),
        before["ticks"].as_u64().unwrap() + 1,
        "exactly one tick, and the response describes the one it ran"
    );

    // A read must not be a write: the tick count is unchanged by asking for it.
    let (_, again) = h.request("GET", "/api/v1/snapshot").await;
    assert_eq!(again["ticks"], after["ticks"]);
}

#[tokio::test]
async fn an_issue_response_carries_its_state_and_its_run_history() {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.worker.set_default(Script::succeeds_in(1_000));
    h.tick();

    let (status, running) = h.request("GET", "/api/v1/issues/MT-1").await;
    assert_eq!(status, 200);
    assert_eq!(running["issue_id"], "iss-1");
    assert_eq!(running["phase"], "running");
    assert_eq!(running["url"], "https://example.invalid/1");
    let in_flight = running["runs"].as_array().unwrap();
    assert_eq!(in_flight.len(), 1, "the run in flight is already history");
    assert!(in_flight[0]["ended_at"].is_null(), "and is not finished yet");

    // Let it finish, and the same endpoint shows the verdict.
    h.clock.advance_ms(1_000);
    h.tick();
    let (_, done) = h.request("GET", "/api/v1/issues/MT-1").await;
    let runs = done["runs"].as_array().unwrap();
    assert_eq!(runs[0]["outcome"], "done");
    assert!(runs[0]["ended_at"].is_i64());

    // A dispatch id is accepted in the same slot, for the case two issues share an identifier.
    let (status, by_id) = h.request("GET", "/api/v1/issues/iss-1").await;
    assert_eq!(status, 200);
    assert_eq!(by_id["identifier"], "MT-1");

    let (status, missing) = h.request("GET", "/api/v1/issues/MT-404").await;
    assert_eq!(status, 404, "an unknown issue is a 404, not a 500 and not an empty 200");
    assert!(missing["error"].as_str().unwrap().contains("MT-404"));
}

#[tokio::test]
async fn clearing_a_quarantine_that_is_not_there_is_a_no_op_with_a_plain_answer() {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.worker.set_default(Script::succeeds_in(60_000));
    h.tick();
    assert_eq!(h.sched.running_count(), 1, "the issue is running, not quarantined");

    let (status, body) = h.request("POST", "/api/v1/unquarantine/MT-1").await;
    assert_eq!(status, 200, "a no-op is not an error");
    assert_eq!(body["cleared"], false);
    assert_eq!(body["detail"], "not quarantined; nothing to clear");

    // And it really was a no-op: an unguarded version would have released the live claim here,
    // leaving the next tick free to dispatch a second agent onto the same worktree.
    h.tick();
    assert_eq!(h.sched.running_count(), 1, "the live run is untouched");
    let (_, snap) = h.request("GET", "/api/v1/snapshot").await;
    assert_eq!(row(&snap, "MT-1")["phase"], "running");
}

#[tokio::test]
async fn clearing_a_quarantine_returns_the_issue_to_service() {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.worker.set_default(
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::AuthFailed, msg: "401".into() }),
    );
    h.tick();
    h.clock.advance_ms(1_000);
    h.tick();

    let (_, snap) = h.request("GET", "/api/v1/snapshot").await;
    assert_eq!(row(&snap, "MT-1")["quarantined"], true);
    assert_eq!(snap["quarantined"], 1);

    h.worker.set_default(Script::succeeds_in(1_000));
    let (status, body) = h.request("POST", "/api/v1/unquarantine/MT-1").await;
    assert_eq!(status, 200);
    assert_eq!(body["cleared"], true, "there was a quarantine, and it was cleared");
    assert_eq!(body["issue_id"], "iss-1");

    h.tick();
    assert_eq!(h.sched.running_count(), 1, "cleared issues become dispatchable again");

    let (status, unknown) = h.request("POST", "/api/v1/unquarantine/MT-404").await;
    assert_eq!(status, 404, "an issue that does not exist is not a silent success");
    assert!(unknown["error"].is_string());
}

#[tokio::test]
async fn a_client_that_never_finishes_its_request_cannot_delay_a_tick() {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();

    // A request head that never terminates. The connection stays open for the whole test:
    // served from the accept loop, this alone would wedge every endpoint until the read
    // timeout, and `POST /refresh` would be answered minutes after it was asked.
    let mut stuck = TcpStream::connect(h.addr).await.unwrap();
    stuck.write_all(b"GET /api/v1/snapshot HTTP/1.1\r\nHost: localhost\r\n").await.unwrap();
    stuck.flush().await.unwrap();

    // Deadlined deliberately: a server that accepted connections one at a time would still
    // answer this — after the stuck client's read timeout expired, minutes into a real
    // incident. "Eventually" is the bug, so the assertion is on the wait, not just the answer.
    let deadline = std::time::Duration::from_secs(1);
    let (status, body) = tokio::time::timeout(deadline, h.request("POST", "/api/v1/refresh"))
        .await
        .expect("a healthy client must not wait behind a stuck one");
    assert_eq!(status, 200);
    assert_eq!(body["ticks"].as_u64().unwrap(), 2, "and the tick it asked for really ran");

    let (status, _) = tokio::time::timeout(deadline, h.request("GET", "/api/v1/snapshot"))
        .await
        .expect("nor does the next one");
    assert_eq!(status, 200);

    drop(stuck);
}

#[tokio::test]
async fn the_wrong_method_on_a_real_endpoint_says_which_one_to_use() {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();

    let (status, body) = h.request("GET", "/api/v1/refresh").await;
    assert_eq!(status, 405);
    assert!(body["error"].as_str().unwrap().contains("POST"));

    let (status, _) = h.request("DELETE", "/api/v1/snapshot").await;
    assert_eq!(status, 405, "the read endpoints are read-only, including to an odd verb");

    let (status, _) = h.request("GET", "/metrics").await;
    assert_eq!(status, 404);
}

// ---- the status client ------------------------------------------------------
//
// The client lives in the same crate as the server and shares its wire types, so most of what
// could go wrong between them is a compile error. What is left is what only a socket shows:
// that the address the client builds reaches the route the server matches, and that an error
// status becomes a message rather than a panic. These drive the real `Client` against the real
// `Api` for that reason — the same argument the rest of this file is built on.

/// Run a blocking client call without stalling the runtime the server is accepting on.
async fn via_client<T, F>(addr: SocketAddr, call: F) -> Result<T, StatusError>
where
    T: Send + 'static,
    F: FnOnce(Client) -> Result<T, StatusError> + Send + 'static,
{
    let endpoint = Endpoint { addr: addr.to_string(), source: Source::Flag };
    tokio::task::spawn_blocking(move || call(Client::new(endpoint))).await.unwrap()
}

/// Accepts one connection, writes a fixed raw HTTP/1.1 response with none of this API's own
/// headers, and closes — standing in for "some other service happens to be on this port," the
/// same raw-`TcpListener` pattern `tracker::github`'s `ureq_http_tests` uses so the test proves
/// something about the real `Client` rather than about a fake that was told what to answer.
fn serve_once_unmarked(response: &'static str) -> SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // drain the request so the client isn't left hanging
            let _ = stream.write_all(response.as_bytes());
        }
    });
    addr
}

#[tokio::test]
async fn the_status_client_renders_the_same_snapshot_the_api_publishes() {
    let mut h = Harness::new(vec![issue(1, "In Progress"), issue(2, "In Progress")]).await;
    h.tick();

    let snap = via_client(h.addr, |c| c.snapshot()).await.expect("a running daemon answers");
    let text = crew::api::render::snapshot(&snap, "127.0.0.1:8787");

    // The numbers an operator opens this for, against a scheduler that really dispatched.
    assert!(text.contains("2 running"), "{text}");
    assert!(text.contains("MT-1") && text.contains("MT-2"), "{text}");
    assert!(text.contains("running"), "{text}");
    // And it is a rendering, not a JSON dump.
    assert!(!text.contains("issue_id\":"), "{text}");
}

#[tokio::test]
async fn the_status_client_answers_phase_attempt_turns_cost_and_branch_for_one_issue() {
    // Issue #24's second acceptance criterion, end to end: the five fields have to survive the
    // scheduler, the snapshot, JSON and the renderer. `branch` is `None` here because the test
    // workspace is a `DirWorkspace`, which has no branches — that the *field* arrives is what
    // this proves; that its value is the branch git checked out is
    // `the_branch_the_snapshot_publishes_is_the_one_prepare_checks_out`.
    let mut h = Harness::new(vec![issue(7, "In Progress")]).await;
    h.tick();

    let row = via_client(h.addr, |c| c.issue("MT-7")).await.expect("the issue resolves");
    let text = crew::api::render::issue(&row);

    for field in ["phase", "attempt", "turns", "tokens", "branch"] {
        assert!(text.contains(field), "the detail view dropped {field}:\n{text}");
    }
    assert!(text.contains("MT-7") && text.contains("iss-7"), "{text}");

    // The dispatch id resolves to the same row, so an operator who has either one is served.
    let by_id = via_client(h.addr, |c| c.issue("iss-7")).await.expect("the dispatch id resolves");
    assert_eq!(by_id.issue_id, row.issue_id);
}

#[tokio::test]
async fn a_daemon_that_answers_no_is_reported_as_running_rather_than_absent() {
    // The distinction issue #24 asks for, from the other side: the closed-port case is covered
    // by a unit test, and this is the one that needs a server to produce. Getting these two
    // confused is what sends an operator restarting a daemon that was never down.
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();

    let err = via_client(h.addr, |c| c.issue("MT-nope")).await.expect_err("no such issue");
    assert!(matches!(err, StatusError::Refused { status: 404, .. }), "got {err:?}");

    let message = err.to_string();
    assert!(message.contains("is running and refused"), "{message}");
    assert!(message.contains("MT-nope"), "the API's own reason must survive: {message}");
}

#[tokio::test]
async fn an_identifier_that_needs_encoding_still_reaches_its_route() {
    // The server splits on `/` before it decodes, so a client that sent this raw would ask for
    // a four-segment path and get a 404 that looks like a missing issue.
    let mut odd = issue(1, "In Progress");
    odd.identifier = "team/MT-1".into();
    let mut h = Harness::new(vec![odd]).await;
    h.tick();

    let row = via_client(h.addr, |c| c.issue("team/MT-1")).await.expect("the slash survives");
    assert_eq!(row.identifier, "team/MT-1");

    // `--json` goes down a second path, and the first version of it built its own URL and
    // skipped the encoding — a 404 that reads as a missing issue, on the one input the typed
    // call gets right.
    let raw = via_client(h.addr, |c| c.raw_issue("team/MT-1")).await.expect("so does --json");
    let parsed: Value = serde_json::from_str(&raw).expect("the body is JSON");
    assert_eq!(parsed["identifier"], "team/MT-1");
}

#[tokio::test]
async fn a_padded_bind_address_in_the_config_reaches_the_daemon_the_way_a_trimmed_one_does() {
    // `api::bind` (mod.rs) trims `cfg.bind` before parsing it, so a config written with
    // incidental surrounding whitespace binds the daemon successfully. The client has to trim
    // the same string the same way, or the exact address the daemon is listening on turns into
    // an invalid URL only on this side of the connection.
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();

    let cfg_path =
        std::env::temp_dir().join(format!("crew-api-test-padded-bind-{}.toml", std::process::id()));
    std::fs::write(&cfg_path, format!("[api]\nbind = \"  {}  \"\n", h.addr)).unwrap();

    let ep = crew::api::client::endpoint(None, &cfg_path);
    assert_eq!(ep.addr, h.addr.to_string(), "the padding must not survive into the address used");

    let snap = tokio::task::spawn_blocking(move || Client::new(ep).snapshot())
        .await
        .unwrap()
        .expect("a client built from the padded config must still reach the daemon");
    assert_eq!(snap.rows.len(), 1);

    std::fs::remove_file(&cfg_path).ok();
}

#[tokio::test]
async fn every_response_the_ops_api_writes_carries_its_marker_header() {
    // Both a 200 and this API's own 404 have to carry it: the client trusts the header before
    // it trusts anything else in the response, including a status code this router produced
    // itself, so an error path that forgot the header would make itself indistinguishable from
    // a different service refusing the request.
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();

    for path in ["/api/v1/snapshot", "/api/v1/issues/MT-404"] {
        let mut stream = TcpStream::connect(h.addr).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("X-Crew-Ops-Api"), "missing the marker header for {path}:\n{text}");
    }
}

#[tokio::test]
async fn a_response_missing_the_marker_is_treated_as_a_different_service_not_the_daemon() {
    // The exact case issue #31's review flagged: a reachable port that answers is not
    // necessarily this daemon. Without the check, the 200 below would have satisfied
    // `snapshot()` outright, and the same body would have been handed back verbatim by the
    // `--json` path — and a 404 would have read as "the daemon said no" rather than as nothing
    // to do with the daemon at all.
    let addr = serve_once_unmarked(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    );
    let err = via_client(addr, |c| c.snapshot()).await.expect_err("no marker, no daemon");
    assert!(matches!(err, StatusError::Unrecognised { .. }), "got {err:?}");

    // `--json` is not exempt: a raw body from a service that is not this API must not reach an
    // operator's `jq` looking like a snapshot.
    let addr = serve_once_unmarked(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    );
    let err = via_client(addr, |c| c.raw_snapshot()).await.expect_err("--json is not exempt");
    assert!(matches!(err, StatusError::Unrecognised { .. }), "got {err:?}");

    let addr = serve_once_unmarked("HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found");
    let err = via_client(addr, |c| c.issue("MT-1")).await.expect_err("no marker, no daemon");
    assert!(matches!(err, StatusError::Unrecognised { .. }), "got {err:?}");
}

// ---- the ops MCP server -----------------------------------------------------
//
// The supervising agent's half of the harness (#34). These drive the real transport against the
// same `Api` the HTTP tests drive, over a real socket, for the reason the rest of this file
// gives: the tool result framing and the "publish, then answer" ordering are what a fake would
// get right by construction.

#[tokio::test]
async fn the_ops_tools_answer_the_same_snapshot_the_http_api_serves() {
    let mut h = Harness::new(vec![issue(1, "In Progress"), issue(2, "In Progress")]).await;
    h.tick();

    let (_, over_http) = h.request("GET", "/api/v1/snapshot").await;
    let (is_error, over_mcp) = h.tool("snapshot", json!({})).await;
    assert!(!is_error);
    // Byte-for-byte the same JSON: the tool runs the route's method and frames its body, so an
    // agent reading the one can be handed documentation written for the other.
    assert_eq!(over_mcp, over_http);

    let (_, http_row) = h.request("GET", "/api/v1/issues/MT-2").await;
    let (is_error, mcp_row) = h.tool("issue", json!({ "key": "MT-2" })).await;
    assert!(!is_error);
    assert_eq!(mcp_row, http_row);
    assert_eq!(mcp_row["issue_id"], "iss-2");

    // And the list is exactly the routes, no more.
    let addr = h.mcp_addr;
    let names = tokio::task::spawn_blocking(move || McpConn::open(addr, mcp::PATH).tool_names())
        .await
        .unwrap();
    assert_eq!(names, mcp::TOOLS);
}

#[tokio::test]
async fn an_ops_read_cannot_disturb_the_daemon_it_asks_about() {
    // The property `run_status` holds by owning no `Store`, worktree or credential, shown from
    // the daemon's side: the scheduler loop is *not pumped* while these reads are served, so
    // if a read needed anything from it the call would hang past the deadline. And the tick
    // count is unchanged afterwards — asking was not acting.
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();
    let before = h.sched.snapshot().unwrap().ticks;

    let addr = h.mcp_addr;
    let deadline = std::time::Duration::from_secs(2);
    let (snap, row) = tokio::time::timeout(
        deadline,
        tokio::task::spawn_blocking(move || {
            let mut c = McpConn::open(addr, mcp::PATH);
            (c.tool_call("snapshot", json!({})), c.tool_call("issue", json!({ "key": "MT-1" })))
        }),
    )
    .await
    .expect("a read must be answered without the scheduler's participation")
    .unwrap();
    assert!(!snap.0 && !row.0);
    assert_eq!(snap.1["ticks"].as_u64().unwrap(), before);
    assert_eq!(row.1["phase"], "running");

    assert!(
        matches!(h.commands.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "a read sent the scheduler a command"
    );
    assert_eq!(h.sched.snapshot().unwrap().ticks, before, "a read must not tick");
}

#[tokio::test]
async fn an_ops_refresh_runs_exactly_one_tick_and_answers_with_it() {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();
    let (_, before) = h.request("GET", "/api/v1/snapshot").await;

    let (is_error, after) = h.tool("refresh", json!({})).await;
    assert!(!is_error);
    assert_eq!(after["ticks"].as_u64().unwrap(), before["ticks"].as_u64().unwrap() + 1);

    // Published before it was answered, so the HTTP surface agrees the instant the tool returns.
    let (_, again) = h.request("GET", "/api/v1/snapshot").await;
    assert_eq!(again["ticks"], after["ticks"]);
}

#[tokio::test]
async fn clearing_a_quarantine_over_mcp_that_is_not_there_reports_a_no_op_not_a_tool_error() {
    // The shape `Store::unquarantine` and the HTTP route already have, carried through to the
    // tool: `cleared: false` in a plain result, so a supervising agent that clears
    // speculatively is told what happened rather than handed a failure to work around.
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.worker.set_default(Script::succeeds_in(60_000));
    h.tick();
    assert_eq!(h.sched.running_count(), 1);

    let (is_error, body) = h.tool("unquarantine", json!({ "key": "MT-1" })).await;
    assert!(!is_error, "a no-op is not an error: {body}");
    assert_eq!(body["cleared"], false);
    assert_eq!(body["detail"], "not quarantined; nothing to clear");

    // And the live claim was not released under the running agent.
    h.tick();
    assert_eq!(h.sched.running_count(), 1);

    // An issue that does not exist, by contrast, is a tool error carrying the route's 404 body.
    let (is_error, body) = h.tool("unquarantine", json!({ "key": "MT-404" })).await;
    assert!(is_error);
    assert!(body["error"].as_str().unwrap().contains("MT-404"), "{body}");
}

#[tokio::test]
async fn clearing_a_quarantine_over_mcp_returns_the_issue_to_service() {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.worker.set_default(
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::AuthFailed, msg: "401".into() }),
    );
    h.tick();
    h.clock.advance_ms(1_000);
    h.tick();
    let (_, row) = h.tool("issue", json!({ "key": "MT-1" })).await;
    assert_eq!(row["quarantined"], true);

    h.worker.set_default(Script::succeeds_in(1_000));
    let (is_error, body) = h.tool("unquarantine", json!({ "key": "iss-1" })).await;
    assert!(!is_error);
    assert_eq!(body["cleared"], true);

    h.tick();
    assert_eq!(h.sched.running_count(), 1, "cleared issues become dispatchable again");
}

#[tokio::test]
async fn a_dispatched_worker_is_not_handed_the_ops_tools() {
    // The constraint issue #34 says must survive review, tested on the wiring rather than on
    // the tool. The MCP servers crewd hands a worker are named in exactly one file the crate
    // writes — the `--mcp-config` from `Broker::open` — so the test is: run both servers the
    // way `main` does, open a real broker session, read the file a worker would be handed, and
    // connect to whatever it names. A version that only asked the ops server to refuse an
    // unknown caller would pass just as well if crewd handed the worker the ops address after
    // all. What a worker inherits from the operator's own MCP config is outside this test, and
    // accepted (see `api::mcp`).
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.tick();

    let writes: Arc<dyn TrackerWrites> = Arc::new(FakeWrites::new());
    let listener = broker::server::bind().unwrap();
    let broker_addr = listener.local_addr().unwrap();
    let dir = h.root.join("mcp-configs");
    let broker = Arc::new(
        Broker::new(
            writes,
            h.clock.clone(),
            BrokerLimits::default(),
            vec!["in progress".into(), "done".into()],
            broker_addr,
            &dir,
        )
        .unwrap(),
    );
    broker::server::serve(Arc::clone(&broker), listener);
    assert_ne!(broker_addr, h.mcp_addr, "the two servers are two listeners");

    let session = broker.open(&issue(1, "In Progress"), "run-1").unwrap();
    let endpoint = session.endpoint().clone();

    // 1. The prompt half: the tool names the worker is told about are the broker's only.
    for ops_tool in mcp::TOOLS {
        assert!(!endpoint.tools.iter().any(|t| t == ops_tool), "prompt names {ops_tool}");
    }

    // 2. The config half: the file names exactly one server, and that server is the broker.
    let config: Value =
        serde_json::from_slice(&std::fs::read(&endpoint.config_path).unwrap()).unwrap();
    let servers = config["mcpServers"].as_object().unwrap();
    assert_eq!(servers.len(), 1, "one server, and only one: {config}");
    let url = servers.values().next().unwrap()["url"].as_str().unwrap().to_string();
    let host_port: SocketAddr =
        url.trim_start_matches("http://").split('/').next().unwrap().parse().unwrap();
    assert_eq!(host_port, broker_addr, "the worker is pointed at the broker");
    assert_ne!(host_port, h.mcp_addr, "and never at the ops server");
    let path = format!("/{}", url.trim_start_matches("http://").split_once('/').unwrap().1);

    // 3. The wire half: what the worker's URL actually serves. Connect as the worker would,
    //    list the tools, and try the one call that would defeat the brakes.
    let ops_addr = h.mcp_addr;
    let (worker_sees, worker_call, misaimed_list, misaimed_call) =
        tokio::task::spawn_blocking(move || {
            let mut as_worker = McpConn::open(broker_addr, &path);
            let names = as_worker.tool_names();
            let call = as_worker.tool_call("unquarantine", json!({ "key": "MT-1" }));
            // The worker's own path, aimed at the ops port by a misconfiguration.
            let mut misaimed = McpConn::open(ops_addr, &path);
            let list = misaimed.tool_names();
            let call2 = misaimed.tool_call("unquarantine", json!({ "key": "MT-1" }));
            (names, call, list, call2)
        })
        .await
        .unwrap();

    for ops_tool in mcp::TOOLS {
        assert!(!worker_sees.iter().any(|t| t == ops_tool), "worker sees {ops_tool}");
    }
    assert!(worker_sees.contains(&"comment".to_string()), "it does see its own tools");
    assert!(worker_call.0, "unquarantine at the worker's URL must be refused: {:?}", worker_call);
    assert!(misaimed_list.is_empty(), "the ops port serves no tools at a worker path");
    assert!(misaimed_call.0, "nor does it act on one: {:?}", misaimed_call);

    // And nothing above touched the scheduler: the issue is still running, not re-dispatched.
    assert!(
        matches!(h.commands.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "a refused call must not become a command"
    );
    drop(session);
}

/// A harness whose one issue has already been parked by a `Blocked` verdict.
async fn blocked_harness() -> Harness {
    let mut h = Harness::new(vec![issue(1, "In Progress")]).await;
    h.worker.set_default(
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Blocked { why: "rebase onto master conflicts".into() }),
    );
    h.tick();
    h.clock.advance_ms(1_000);
    h.tick();
    h.clock.advance_ms(60_000);
    h.tick();
    assert_eq!(h.sched.running_count(), 0, "a blocked issue stays parked");
    h.worker.set_default(Script::succeeds_in(60_000));
    h
}

#[tokio::test]
async fn unblocking_a_parked_issue_over_the_api_returns_it_to_service() {
    let mut h = blocked_harness().await;
    let (_, snap) = h.request("GET", "/api/v1/snapshot").await;
    assert_eq!(row(&snap, "MT-1")["last_error"], "rebase onto master conflicts");

    let (status, body) = h.request("POST", "/api/v1/unblock/MT-1").await;
    assert_eq!(status, 200);
    assert_eq!(body["cleared"], true, "there was a park, and it was lifted");
    assert_eq!(body["issue_id"], "iss-1");
    let (_, snap) = h.request("GET", "/api/v1/snapshot").await;
    assert_eq!(row(&snap, "MT-1")["last_error"], Value::Null, "published before answering");

    h.tick();
    assert_eq!(h.sched.running_count(), 1, "the next tick dispatches it");

    // Now live: the same request is a plain no-op, and the claim stands.
    let (status, body) = h.request("POST", "/api/v1/unblock/MT-1").await;
    assert_eq!(status, 200, "a no-op is not an error");
    assert_eq!(body["cleared"], false);
    assert_eq!(
        body["detail"],
        "nothing to unblock: not parked, still live, owned by its delivery, or its ticket is not active"
    );
    h.tick();
    assert_eq!(h.sched.running_count(), 1, "the live run is untouched");

    let (status, unknown) = h.request("POST", "/api/v1/unblock/MT-404").await;
    assert_eq!(status, 404, "an issue that does not exist is not a silent success");
    assert!(unknown["error"].is_string());

    let (status, _) = h.request("GET", "/api/v1/unblock/MT-1").await;
    assert_eq!(status, 405, "an action is a POST");
}

#[tokio::test]
async fn an_unblock_naming_an_identifier_two_issues_share_is_refused_with_both_ids() {
    // Identifiers are not unique, so an unblock sent by one must not pick an issue and hope:
    // the wrong pick re-dispatches an issue nobody resolved.
    let mut twin = issue(2, "In Progress");
    twin.identifier = "MT-1".into();
    let mut h = Harness::new(vec![issue(1, "In Progress"), twin]).await;
    h.tick();

    let (status, body) = h.request("POST", "/api/v1/unblock/MT-1").await;
    assert_eq!(status, 409);
    let mut ids: Vec<&str> =
        body["issue_ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, ["iss-1", "iss-2"], "both candidates, to retry with");

    let (status, body) = h.request("POST", "/api/v1/unblock/iss-2").await;
    assert_eq!(status, 200, "a dispatch id resolves exactly");
    assert_eq!(body["issue_id"], "iss-2");
}

#[tokio::test]
async fn unblocking_over_mcp_returns_a_parked_issue_to_service_and_reports_a_no_op_plainly() {
    let mut h = blocked_harness().await;

    let (is_error, body) = h.tool("unblock", json!({ "key": "MT-1" })).await;
    assert!(!is_error, "{body}");
    assert_eq!(body["cleared"], true);
    h.tick();
    assert_eq!(h.sched.running_count(), 1, "unblocked issues are dispatched again");

    let (is_error, body) = h.tool("unblock", json!({ "key": "MT-1" })).await;
    assert!(!is_error, "a no-op is not an error: {body}");
    assert_eq!(body["cleared"], false);
    h.tick();
    assert_eq!(h.sched.running_count(), 1);

    let (is_error, body) = h.tool("unblock", json!({ "key": "MT-404" })).await;
    assert!(is_error);
    assert!(body["error"].as_str().unwrap().contains("MT-404"), "{body}");
}
