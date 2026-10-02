//! A small read-mostly HTTP surface, so a running orchestrator can be inspected and nudged
//! without a terminal attached.
//!
//! The routes, all under `/api/v1`:
//!
//! | Route | Method | Answer |
//! |---|---|---|
//! | `/snapshot` | `GET` | the published [`Snapshot`], as JSON |
//! | `/issues/:identifier` | `GET` | one [`Row`], run history included |
//! | `/refresh` | `POST` | the snapshot the forced tick published |
//! | `/unquarantine/:identifier` | `POST` | whether a quarantine was actually cleared |
//! | `/unblock/:identifier` | `POST` | whether a park or a handoff was lifted, and which (#108, #262) |
//!
//! Four properties decide the shape of everything below:
//!
//! * **The snapshot is the whole view.** This type holds a [`watch::Receiver`] and a command
//!   channel, and no `Store` — so an endpoint that wanted state the snapshot does not carry
//!   could not reach it even by accident. That is the same rule that keeps the TUI from
//!   becoming load-bearing, and it cuts the same way: if the API cannot answer something, the
//!   snapshot is incomplete and *that* is the bug to fix. Run history is on [`Row`] for
//!   exactly this reason.
//! * **A slow client cannot delay a tick.** Every connection is served by its own task; the
//!   scheduler is reached only by sending a [`Command`] on an unbounded channel, and it never
//!   waits on this side — its reply goes to a `oneshot` whose receiver may already be gone. A
//!   request that stops mid-header is dropped by [`READ_TIMEOUT`] rather than holding anything
//!   the scheduler needs.
//! * **Loopback unless an operator says otherwise.** The `POST` routes control agent
//!   execution, so [`bind`] refuses a non-loopback address unless `api.allow_public` is set.
//! * **Every response proves it came from here.** A status code and a JSON body are exactly
//!   what an unrelated service on the same port could also produce, so every response this
//!   module writes — success or error — carries [`API_MARKER_HEADER`], and [`client::Client`]
//!   refuses to trust anything else in a response that lacks it.
//!
//! There is no framework here on purpose. Plain routes, no query parameters, no content
//! negotiation and one response type do not pay for a server stack; `ureq` covers the client
//! side of this crate's HTTP needs and this covers the server side, both deliberately small.
//!
//! The seam this module needs for tests is not a trait: it is the pair of channels above.
//! `tests/api.rs` drives a real `Scheduler` through the same command loop `main` runs, over a
//! real socket, so there is no fake here to drift out of step with the real thing.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};

pub use libcrew::client;
pub mod mcp;
pub use libcrew::render;

use crate::config::ApiConfig;
use crate::model::Unblocked;
use crate::sched::{Row, Snapshot};

/// How long a connection has to deliver a complete request head. A client that opens a socket
/// and goes quiet costs one parked task until this expires, and nothing else — but "nothing
/// else" only stays true if it is bounded.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Caps on what one request may make this process hold. Neither limit is reachable by the
/// endpoints here — no route takes a body at all — so anything near them is a client that has
/// lost the plot or is probing.
const MAX_HEAD_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;

use libcrew::api::{API_MARKER_HEADER, API_MARKER_VERSION, normalize_bind};

/// Work only the scheduler loop may do, with a channel to answer on.
///
/// Every variant is an operator action the dashboard already offers; this module deliberately
/// cannot express anything the `r`, `u` and `b` keys cannot.
#[derive(Debug)]
pub enum Command {
    /// Run one tick now, and answer with the snapshot that tick published.
    Tick(oneshot::Sender<anyhow::Result<Snapshot>>),
    /// Clear a quarantine, answering whether there was one to clear.
    Unquarantine { issue_id: String, reply: oneshot::Sender<anyhow::Result<bool>> },
    /// Lift a park or a delivery's handoff, answering which was lifted (#262).
    Unblock { issue_id: String, reply: oneshot::Sender<anyhow::Result<Unblocked>> },
}

/// Bind the API's listener, refusing an exposure nobody asked for.
///
/// Separate from [`Api::serve`] so `main` can bind at startup, before the store or any worktree
/// is opened, and exit naming the address on a failure (a port already in use, an address that
/// does not parse) rather than schedule with no ops API (#218).
pub async fn bind(cfg: &ApiConfig) -> anyhow::Result<TcpListener> {
    let addr = resolve_bind("api.bind", &cfg.bind, cfg.allow_public)?;
    TcpListener::bind(addr).await.with_context(|| format!("binding the ops API to {addr}"))
}

/// Parse a configured bind address and apply the exposure rule.
///
/// Shared with [`mcp::bind`], so the two operator surfaces cannot drift on what "loopback" or
/// "public" means: both carry the same write actions, so one `allow_public` governs both.
/// `field` names the config key in the error, because an operator fixing a typo needs to know
/// which of the two addresses to look at.
pub(crate) fn resolve_bind(
    field: &str,
    raw: &str,
    allow_public: bool,
) -> anyhow::Result<SocketAddr> {
    let addr: SocketAddr = normalize_bind(raw)
        .parse()
        .with_context(|| format!("{field} = {raw:?} is not a host:port address"))?;

    if !addr.ip().is_loopback() && !allow_public {
        anyhow::bail!(
            "{field} = {addr} is not loopback and api.allow_public is not set; the write \
             endpoints control agent execution, so this is refused rather than exposed"
        );
    }
    Ok(addr)
}

#[derive(Clone)]
pub struct Api {
    snapshots: watch::Receiver<Snapshot>,
    commands: mpsc::UnboundedSender<Command>,
}

impl Api {
    pub fn new(
        snapshots: watch::Receiver<Snapshot>,
        commands: mpsc::UnboundedSender<Command>,
    ) -> Self {
        Self { snapshots, commands }
    }

    /// Accept connections until the process ends, one task per connection.
    pub async fn serve(self, listener: TcpListener) {
        match listener.local_addr() {
            Ok(addr) => tracing::info!(%addr, "ops API listening"),
            Err(e) => tracing::warn!(error = %e, "ops API listening on an unknown address"),
        }

        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let api = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = api.serve_connection(stream).await {
                            tracing::debug!(%peer, error = %e, "ops API connection ended early");
                        }
                    });
                }
                Err(e) => {
                    // Descriptor exhaustion is the realistic cause, and it clears as open
                    // connections close. A bare `continue` would spin a core until it does.
                    tracing::warn!(error = %e, "ops API accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    async fn serve_connection(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let response = match tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await {
            Ok(Ok(req)) => {
                let response = self.route(&req).await;
                tracing::debug!(
                    method = %req.method, path = %req.path, status = response.status,
                    "ops API request"
                );
                response
            }
            Ok(Err(rejected)) => rejected,
            Err(_elapsed) => Response::error(408, "request head did not arrive in time"),
        };
        response.write(&mut stream).await
    }

    async fn route(&self, req: &Request) -> Response {
        // Decoding happens after the split, so a `%2F` in an identifier cannot introduce a
        // segment boundary that changes which route matches.
        let decoded: Vec<String> = match req.path.trim_matches('/') {
            "" => Vec::new(),
            path => path.split('/').map(percent_decode).collect(),
        };
        let segments: Vec<&str> = decoded.iter().map(String::as_str).collect();

        match (req.method.as_str(), segments.as_slice()) {
            ("GET", ["api", "v1", "snapshot"]) => json_of(200, &self.latest()),
            ("GET", ["api", "v1", "issues", key]) => self.issue(key),
            ("POST", ["api", "v1", "refresh"]) => self.refresh().await,
            ("POST", ["api", "v1", "unquarantine", key]) => self.unquarantine(key).await,
            ("POST", ["api", "v1", "unblock", key]) => self.unblock(key).await,

            (_, ["api", "v1", "snapshot"] | ["api", "v1", "issues", _]) => Response::allow("GET"),
            (
                _,
                ["api", "v1", "refresh"]
                | ["api", "v1", "unquarantine", _]
                | ["api", "v1", "unblock", _],
            ) => Response::allow("POST"),
            _ => Response::error(404, "no such endpoint"),
        }
    }

    /// The latest published snapshot. Cloned rather than borrowed: a `watch` read guard held
    /// across an await would block the scheduler's next publish on whatever this client does.
    pub(crate) fn latest(&self) -> Snapshot {
        self.snapshots.borrow().clone()
    }

    pub(crate) fn issue(&self, key: &str) -> Response {
        let snap = self.latest();
        match resolve(&snap, key) {
            Resolved::One(row) => json_of(200, row),
            Resolved::Unknown => Response::error(404, &format!("no issue matching {key:?}")),
            Resolved::Ambiguous(ids) => ambiguous(key, &ids),
        }
    }

    async fn refresh(&self) -> Response {
        match self.request_tick() {
            Ok(rx) => tick_reply(rx.await),
            Err(refused) => refused,
        }
    }

    /// [`Api::refresh`] for a caller on a plain thread — the MCP transport, which is not async.
    ///
    /// The wait is the same one-shot the async path awaits, so the property that matters — the
    /// scheduler answers into a channel and never waits on this side — holds identically.
    pub(crate) fn refresh_blocking(&self) -> Response {
        match self.request_tick() {
            Ok(rx) => tick_reply(rx.blocking_recv()),
            Err(refused) => refused,
        }
    }

    fn request_tick(&self) -> Result<oneshot::Receiver<anyhow::Result<Snapshot>>, Response> {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::Tick(tx)).is_err() {
            return Err(Response::error(503, "the scheduler is no longer accepting commands"));
        }
        Ok(rx)
    }

    async fn unquarantine(&self, key: &str) -> Response {
        self.act(Action::Unquarantine, key).await
    }

    async fn unblock(&self, key: &str) -> Response {
        self.act(Action::Unblock, key).await
    }

    async fn act(&self, action: Action, key: &str) -> Response {
        match self.request_action(action, key) {
            Ok((target, rx)) => target.reply(rx.recv().await),
            Err(refused) => refused,
        }
    }

    /// [`Api::unquarantine`] for a caller on a plain thread; see [`Api::refresh_blocking`].
    pub(crate) fn unquarantine_blocking(&self, key: &str) -> Response {
        self.act_blocking(Action::Unquarantine, key)
    }

    /// [`Api::unblock`] for a caller on a plain thread; see [`Api::refresh_blocking`].
    pub(crate) fn unblock_blocking(&self, key: &str) -> Response {
        self.act_blocking(Action::Unblock, key)
    }

    fn act_blocking(&self, action: Action, key: &str) -> Response {
        match self.request_action(action, key) {
            Ok((target, rx)) => target.reply(rx.blocking_recv()),
            Err(refused) => refused,
        }
    }

    fn request_action(&self, action: Action, key: &str) -> Result<(Target, ActionReply), Response> {
        let snap = self.latest();
        let (issue_id, identifier) = match resolve(&snap, key) {
            Resolved::One(row) => (row.issue_id.clone(), row.identifier.clone()),
            Resolved::Unknown => {
                return Err(Response::error(404, &format!("no issue matching {key:?}")));
            }
            Resolved::Ambiguous(ids) => return Err(ambiguous(key, &ids)),
        };

        // Sent even when this snapshot says there is nothing to clear: the snapshot is as old
        // as the last tick, and the store decides the question without a race anyway. Its
        // answer, not this row, is what the operator is told.
        let (cmd, rx) = match action {
            Action::Unquarantine => {
                let (tx, rx) = oneshot::channel();
                let cmd = Command::Unquarantine { issue_id: issue_id.clone(), reply: tx };
                (cmd, ActionReply::Unquarantine(rx))
            }
            Action::Unblock => {
                let (tx, rx) = oneshot::channel();
                (
                    Command::Unblock { issue_id: issue_id.clone(), reply: tx },
                    ActionReply::Unblock(rx),
                )
            }
        };
        if self.commands.send(cmd).is_err() {
            return Err(Response::error(503, "the scheduler is no longer accepting commands"));
        }
        Ok((Target { action, issue_id, identifier }, rx))
    }
}

fn tick_reply(answer: Result<anyhow::Result<Snapshot>, oneshot::error::RecvError>) -> Response {
    match answer {
        Ok(Ok(snap)) => json_of(200, &snap),
        Ok(Err(e)) => Response::error(500, &format!("the tick failed: {e}")),
        Err(_) => Response::error(503, "the scheduler stopped before the tick completed"),
    }
}

/// The scheduler's answer to one action, on the channel that action's command carries.
enum ActionReply {
    Unquarantine(oneshot::Receiver<anyhow::Result<bool>>),
    Unblock(oneshot::Receiver<anyhow::Result<Unblocked>>),
}

type Answered = Result<anyhow::Result<Answer>, oneshot::error::RecvError>;

impl ActionReply {
    async fn recv(self) -> Answered {
        Ok(match self {
            ActionReply::Unquarantine(rx) => rx.await?.map(Answer::unquarantine),
            ActionReply::Unblock(rx) => rx.await?.map(Answer::unblock),
        })
    }

    fn blocking_recv(self) -> Answered {
        Ok(match self {
            ActionReply::Unquarantine(rx) => rx.blocking_recv()?.map(Answer::unquarantine),
            ActionReply::Unblock(rx) => rx.blocking_recv()?.map(Answer::unblock),
        })
    }
}

/// What an action did, as the operator is told it.
struct Answer {
    cleared: bool,
    detail: String,
}

impl Answer {
    fn unquarantine(cleared: bool) -> Self {
        let detail = if cleared {
            "quarantine cleared; the issue is dispatchable again"
        } else {
            "not quarantined; nothing to clear"
        };
        Answer { cleared, detail: detail.to_string() }
    }

    /// Names which of the two an unblock lifted, since a park and a handoff hand the issue back
    /// to different paths (#262).
    fn unblock(done: Unblocked) -> Self {
        let detail = match &done {
            Unblocked::Park => {
                "park lifted; the next tick dispatches the issue onto its existing branch".into()
            }
            Unblocked::Delivery { pr_url: Some(url) } => {
                format!("delivery resumed on {url}; the next poll reads its CI and review")
            }
            Unblocked::Delivery { pr_url: None } => "delivery resumed before its pull request; \
                 the next poll pushes the branch and opens or adopts one"
                .into(),
            Unblocked::Nothing => "nothing to unblock: not parked, still live, owned by its \
                 delivery, or its ticket is not active"
                .into(),
        };
        Answer { cleared: done.cleared(), detail }
    }
}

/// The guarded operator actions on one issue. One type, so the routes and the MCP tools
/// share a resolution, a `409` and an answer shape rather than each growing its own.
#[derive(Debug, Clone, Copy)]
enum Action {
    Unquarantine,
    Unblock,
}

impl Action {
    fn failed(self) -> &'static str {
        match self {
            Action::Unquarantine => "clearing the quarantine failed",
            Action::Unblock => "unblocking failed",
        }
    }
}

/// The issue an action was sent for, kept so the answer can name it.
struct Target {
    action: Action,
    issue_id: String,
    identifier: String,
}

impl Target {
    fn reply(self, answer: Answered) -> Response {
        match answer {
            Ok(Ok(Answer { cleared, detail })) => Response::json(
                200,
                json!({
                    "issue_id": self.issue_id,
                    "identifier": self.identifier,
                    "cleared": cleared,
                    "detail": detail,
                }),
            ),
            Ok(Err(e)) => Response::error(500, &format!("{}: {e}", self.action.failed())),
            Err(_) => Response::error(503, "the scheduler stopped before the action completed"),
        }
    }
}

enum Resolved<'a> {
    One(&'a Row),
    Unknown,
    Ambiguous(Vec<String>),
}

/// Find the row an `:identifier` path segment names.
///
/// Identifiers are what an operator has to hand, but nothing guarantees they are unique — an
/// adapter can hand two distinct issues the same one, which is why workspaces are keyed on the
/// dispatch id (see [`crate::model::worktree_key`]). So a dispatch id resolves first and
/// exactly, and a duplicated identifier answers with the ids to retry with rather than picking
/// one of them and hoping.
fn resolve<'a>(snap: &'a Snapshot, key: &str) -> Resolved<'a> {
    if let Some(row) = snap.rows.iter().find(|r| r.issue_id == key) {
        return Resolved::One(row);
    }
    let hits: Vec<&Row> = snap.rows.iter().filter(|r| r.identifier == key).collect();
    match hits.as_slice() {
        [] => Resolved::Unknown,
        [row] => Resolved::One(row),
        many => Resolved::Ambiguous(many.iter().map(|r| r.issue_id.clone()).collect()),
    }
}

fn ambiguous(key: &str, ids: &[String]) -> Response {
    Response::json(
        409,
        json!({
            "error": format!("{key:?} names more than one issue; retry with a dispatch id"),
            "issue_ids": ids,
        }),
    )
}

pub(crate) fn json_of<T: Serialize>(status: u16, value: &T) -> Response {
    match serde_json::to_value(value) {
        Ok(v) => Response::json(status, v),
        // Unreachable for the types served here, and cheaper to answer than to panic in a task
        // whose death would be invisible.
        Err(e) => Response::error(500, &format!("serialising the response failed: {e}")),
    }
}

// ---- HTTP -------------------------------------------------------------------

struct Request {
    method: String,
    /// Path only: the query string and fragment are stripped, because no route reads them.
    path: String,
}

/// One answer from the ops surface, before it is put on a wire.
///
/// Built by the routes and written by either transport: the HTTP server below frames it as a
/// response, and [`mcp`] frames the same status and body as a tool result. That is what keeps
/// the two surfaces from answering the same question differently.
pub(crate) struct Response {
    pub(crate) status: u16,
    headers: Vec<(&'static str, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    fn json(status: u16, body: Value) -> Self {
        Self { status, headers: Vec::new(), body: body.to_string().into_bytes() }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::json(status, json!({ "error": message }))
    }

    fn allow(methods: &str) -> Self {
        let mut r = Self::error(405, &format!("method not allowed; use {methods}"));
        r.headers.push(("Allow", methods.to_string()));
        r
    }

    async fn write(self, stream: &mut TcpStream) -> std::io::Result<()> {
        // Unconditional, on every status this writes — the marker is what lets a client trust
        // a status code at all, so it cannot itself be gated on one.
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n{API_MARKER_HEADER}: {API_MARKER_VERSION}\r\n",
            self.status,
            reason(self.status),
            self.body.len()
        );
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");

        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&self.body).await?;
        stream.flush().await
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// Read one request head, then drain whatever body followed it.
///
/// Every connection is answered and closed — no keep-alive. Routes that an operator hits
/// by hand or from a script do not need connection reuse, and a parser that never has to find
/// the next request on the same socket is a parser with far less to get wrong.
async fn read_request(stream: &mut TcpStream) -> Result<Request, Response> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(Response::error(431, "request head too large"));
        }
        let mut chunk = [0u8; 1024];
        match stream.read(&mut chunk).await {
            Ok(0) => return Err(Response::error(400, "connection closed mid-request")),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(Response::error(400, &format!("read failed: {e}"))),
        }
    };

    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| Response::error(400, "request head is not valid UTF-8"))?;
    let mut lines = head.split("\r\n");

    let mut start = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target)) = (start.next(), start.next()) else {
        return Err(Response::error(400, "malformed request line"));
    };

    let declared: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    if declared > MAX_BODY_BYTES {
        return Err(Response::error(413, "request body too large"));
    }

    // No route reads a body, but leaving one unread would have the client see a connection
    // reset where its response should have been.
    let mut remaining = declared.saturating_sub(buf.len() - (head_end + 4));
    let mut scratch = [0u8; 1024];
    while remaining > 0 {
        match stream.read(&mut scratch).await {
            Ok(0) => break,
            Ok(n) => remaining = remaining.saturating_sub(n),
            Err(e) => return Err(Response::error(400, &format!("read failed: {e}"))),
        }
    }

    Ok(Request { method: method.to_ascii_uppercase(), path: path_of(target).to_string() })
}

/// The path part of a request target. The query string and fragment are dropped here, once, so
/// that routing only ever sees a path — no route reads either, and a `?` reaching the matcher
/// would silently turn a known endpoint into a 404.
fn path_of(target: &str) -> &str {
    target.split(['?', '#']).next().unwrap_or("/")
}

/// Decode `%XX` escapes in one path segment. Invalid escapes are left as written, which is what
/// makes this total: a hostile identifier becomes a lookup that finds nothing, never an error
/// path of its own.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Phase;

    fn row(issue_id: &str, identifier: &str) -> Row {
        Row {
            issue_id: issue_id.into(),
            identifier: identifier.into(),
            phase: Phase::Running,
            ..Default::default()
        }
    }

    fn snapshot(rows: Vec<Row>) -> Snapshot {
        Snapshot { rows, ..Default::default() }
    }

    #[test]
    fn an_unblock_answer_says_whether_it_lifted_a_park_or_resumed_a_delivery() {
        // #262: the two hand the issue to different paths, so the operator is told which.
        let park = Answer::unblock(Unblocked::Park);
        assert!(park.cleared);
        insta::assert_snapshot!(park.detail, @"park lifted; the next tick dispatches the issue onto its existing branch");
        let url = "https://github.com/o/r/pull/7".to_string();
        let resumed = Answer::unblock(Unblocked::Delivery { pr_url: Some(url) });
        assert!(resumed.cleared);
        insta::assert_snapshot!(resumed.detail, @"delivery resumed on https://github.com/o/r/pull/7; the next poll reads its CI and review");
        let before = Answer::unblock(Unblocked::Delivery { pr_url: None });
        assert!(before.cleared);
        insta::assert_snapshot!(before.detail, @"delivery resumed before its pull request; the next poll pushes the branch and opens or adopts one");
        assert!(!Answer::unblock(Unblocked::Nothing).cleared);
    }

    #[test]
    fn a_dispatch_id_resolves_even_when_two_issues_share_an_identifier() {
        // The case `worktree_key` exists for: distinct issues, one identifier. Answering with
        // whichever row sorted first would point an operator action at the wrong agent.
        let snap = snapshot(vec![row("iss-a", "MT-649"), row("iss-b", "MT-649")]);

        assert!(matches!(resolve(&snap, "MT-649"), Resolved::Ambiguous(ids) if ids.len() == 2));
        assert!(matches!(resolve(&snap, "iss-b"), Resolved::One(r) if r.issue_id == "iss-b"));
        assert!(matches!(resolve(&snap, "MT-650"), Resolved::Unknown));
    }

    #[test]
    fn an_identifier_resolves_to_its_one_row() {
        let snap = snapshot(vec![row("iss-a", "MT-1"), row("iss-b", "MT-2")]);
        assert!(matches!(resolve(&snap, "MT-2"), Resolved::One(r) if r.issue_id == "iss-b"));
    }

    #[test]
    fn a_query_string_does_not_reach_the_route_matcher() {
        // `/api/v1/snapshot?pretty=1` is a request for the snapshot, not a 404.
        assert_eq!(path_of("/api/v1/snapshot?pretty=1"), "/api/v1/snapshot");
        assert_eq!(path_of("/api/v1/snapshot#frag"), "/api/v1/snapshot");
        assert_eq!(path_of("/api/v1/snapshot"), "/api/v1/snapshot");
    }

    #[test]
    fn an_encoded_separator_cannot_invent_a_path_segment() {
        // Decoding before the split would turn `/issues/a%2Fb` into four segments and route it
        // somewhere its author did not name.
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("MT%2D649"), "MT-649");
        assert_eq!(percent_decode("100%"), "100%", "a trailing escape is left as written");
        assert_eq!(percent_decode("%zz"), "%zz", "an invalid escape is left as written");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[tokio::test]
    async fn a_non_loopback_bind_is_refused_unless_it_was_asked_for() {
        let cfg = ApiConfig { enabled: true, bind: "0.0.0.0:0".into(), ..Default::default() };
        let err = bind(&cfg).await.expect_err("0.0.0.0 must not bind by default").to_string();
        assert!(err.contains("allow_public"), "the refusal must name the way out: {err}");

        let bad = ApiConfig { enabled: true, bind: "not-an-address".into(), ..Default::default() };
        assert!(bind(&bad).await.is_err());

        let ok = ApiConfig { enabled: true, bind: "127.0.0.1:0".into(), ..Default::default() };
        assert!(bind(&ok).await.is_ok(), "loopback needs no ceremony");
    }

    #[tokio::test]
    async fn a_request_for_an_unknown_route_is_a_404_and_a_wrong_method_is_a_405() {
        let (_tx, rx) = watch::channel(snapshot(vec![row("iss-a", "MT-1")]));
        let (ctx, _crx) = mpsc::unbounded_channel();
        let api = Api::new(rx, ctx);

        let req = |m: &str, p: &str| Request { method: m.into(), path: p.into() };

        assert_eq!(api.route(&req("GET", "/api/v1/nope")).await.status, 404);
        assert_eq!(api.route(&req("GET", "/")).await.status, 404);
        assert_eq!(api.route(&req("POST", "/api/v1/snapshot")).await.status, 405);
        assert_eq!(api.route(&req("GET", "/api/v1/refresh")).await.status, 405);
        assert_eq!(api.route(&req("GET", "/api/v1/snapshot")).await.status, 200);
        assert_eq!(api.route(&req("GET", "/api/v1/issues/MT-1")).await.status, 200);
        assert_eq!(api.route(&req("GET", "/api/v1/issues/MT-9")).await.status, 404);
    }

    #[tokio::test]
    async fn an_action_answers_503_rather_than_hanging_when_the_scheduler_is_gone() {
        let (_tx, rx) = watch::channel(snapshot(vec![row("iss-a", "MT-1")]));
        let (ctx, crx) = mpsc::unbounded_channel();
        drop(crx); // the scheduler loop has exited

        let api = Api::new(rx, ctx);
        assert_eq!(api.refresh().await.status, 503);
        assert_eq!(api.unquarantine("MT-1").await.status, 503);
        assert_eq!(api.unblock("MT-1").await.status, 503);
    }
}
