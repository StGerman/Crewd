//! MCP over HTTP, on loopback, hand-rolled.
//!
//! ## Why not `rmcp`
//!
//! The issue proposed the official Rust SDK. It was tried and set aside for two concrete
//! reasons, neither of them stylistic:
//!
//! * `rmcp`'s `transport-streamable-http-server` is a `tower::Service`, not a server. It ships
//!   no listener, so it does not remove the need for an HTTP stack — it adds ~35 crates *and*
//!   still requires axum or hyper on top of them. The dependency footprint for the broker's tools
//!   would roughly double this crate's tree.
//! * It is async, and everything it would sit between is not. [`Tracker`] and
//!   [`TrackerWrites`] are blocking by deliberate choice (see [`crate::tracker::github`]'s note
//!   on `ureq`), and `Scheduler::tick` is a synchronous function. Bridging them would mean a
//!   runtime handle threaded through the broker and `spawn_blocking` around every write, for a
//!   protocol surface of four methods.
//!
//! Four methods is what this actually is: `initialize`, `notifications/initialized`,
//! `tools/list`, `tools/call`. The rest of MCP's streamable-HTTP transport is optional for a
//! server that never initiates a message.
//!
//! [`Tracker`]: crate::tracker::Tracker
//! [`TrackerWrites`]: super::TrackerWrites
//!
//! ## Two servers, one transport
//!
//! The same four methods later had to serve a second, unrelated set of tools: the operator's
//! [ops tools](crate::api::mcp), which answer from the published snapshot rather than writing to
//! a tracker. Rather than a second copy of this file or `rmcp` after all, the transport is
//! generic over [`McpService`] — the part that differs between the two is which tools exist
//! and what a call does, and that is the whole trait. What the decision cost: the transport
//! knows nothing about authority, so each service does its own scoping from the request path
//! (the broker reads a per-run token out of it; the ops service accepts one fixed path and
//! nothing else), and the two **never share a listener**. They could have — one accept loop,
//! routed by prefix — and it was not done, because the per-run token path and the operator path
//! would then answer at the same address, which is exactly the address every dispatched worker
//! is handed. Keeping them on separate ports is what makes "crewd never hands a worker the ops
//! tools" a property of the wiring rather than of a prefix check. It does not make them
//! unreachable: a worker inherits the operator's MCP config, and that is accepted (see
//! [`crate::api::mcp`]).
//!
//! ## What the real client does
//!
//! Confirmed by pointing `claude 2.1.278` at a recording server rather than read off a spec,
//! because the parts that would have been guessed wrong are exactly the parts that are not in
//! the spec's happy path:
//!
//! * The first request is **`server/discover`**, not `initialize`, carrying
//!   `mcp-protocol-version: 2026-07-28`. It is not in the revision this server implements, and
//!   answering it with a plain JSON-RPC `-32601` is accepted — the client falls through to the
//!   normal handshake. Nothing here needs to know what it is.
//! * `initialize` then negotiates `2025-11-25`. The server echoes whatever version it is
//!   offered rather than asserting one.
//! * `notifications/initialized` arrives with **no `id`**. A JSON-RPC notification takes no
//!   response body at all: `202 Accepted`, empty. Answering it like a request breaks the
//!   handshake.
//! * The client opens a **`GET` for an SSE stream**. Refusing it with `405` is fine and costs
//!   nothing — this server has no server-initiated messages to push, so a plain
//!   `application/json` response to each POST is the entire transport. That single fact is
//!   what makes this file short enough to be worth owning.
//! * Requests are keep-alive on one connection, so the read loop must serve many requests per
//!   accept rather than closing after the first.
//!
//! ## Exposure
//!
//! The broker's own listener is bound to `127.0.0.1` on an ephemeral port — never a routable
//! interface. Authority there is the per-run token in the path, checked in
//! [`Broker::call`](super::Broker::call). A tool failure is reported as an MCP tool *result*
//! with `isError: true`, never as a JSON-RPC error: the former is something the agent can read
//! and work around, the latter reads as a broken server and is the shape that would turn a
//! refused write into a failed run.
//!
//! The *ops* listener is why [`Limits`] exists. This is a thread per connection, and that
//! address is the operator's to choose: `api.allow_public` can put it on a routable interface,
//! where a client that connects and then says nothing costs a thread for as long as it cares
//! to hold one. The HTTP ops API bounds exactly that with its own `READ_TIMEOUT`; [`Limits`]
//! is the same rule arriving at the other operator surface, and the broker gets it for free.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

/// What the transport needs from a set of tools, and nothing more.
///
/// A service sees the request path so it can do its own scoping — the transport does not know
/// what a token is. `call` answers `Err` for anything the agent should read as a failed tool
/// rather than a broken server; the transport turns it into a result with `isError: true`, so
/// no implementation can accidentally produce the JSON-RPC error shape that fails a run.
pub trait McpService: Send + Sync + 'static {
    /// Reported in `initialize` as `serverInfo.name`. Tools reach the agent as
    /// `mcp__<name>__<tool>`.
    fn name(&self) -> &str;
    /// The `tools/list` answer for a client that connected at `path`.
    fn tools(&self, path: &str) -> Value;
    /// Perform one call for a client that connected at `path`.
    fn call(&self, path: &str, tool: &str, args: &Value) -> Result<String, String>;
}

/// The revision this server implements when the client does not name one.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";

/// Caps on one request, so a hostile or wedged client cannot turn a connection into a memory
/// problem. A comment body is bounded well below this by the broker's own argument limit.
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// What one client may cost this server in threads and in time.
///
/// A thread-per-connection server has two ways to be exhausted and a read deadline alone
/// closes one of them. The split between [`idle`](Self::idle) and [`request`](Self::request)
/// is what lets a deadline exist here at all without breaking keep-alive, which the real
/// client depends on: a connection sitting between two tool calls is ordinary and may wait a
/// long time, but once a request has *started* arriving the rest of it must arrive promptly —
/// the peer is a program, not a person. [`max_connections`](Self::max_connections) closes the
/// other: a client that dribbles a byte inside every deadline, or simply opens sockets and
/// never writes at all, pays nothing for a deadline and everything for a cap.
///
/// The count is per listener, not per process, so the broker's budget and the operator's are
/// separate — a flood at the public one cannot starve a dispatched run of its own tools.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// How long a connection may sit between requests before it is closed. Generous: hanging
    /// up on a live agent costs it a reconnect, and this exists to bound abandoned sockets,
    /// not to ration a slow conversation.
    pub idle: Duration,
    /// How long the rest of a request may take once its first byte has arrived — and how long
    /// a response may take to write, so a client that stops reading cannot wedge the thread
    /// in `write_all` instead.
    pub request: Duration,
    /// How many connections may be in flight at once. Past it a new one is closed rather than
    /// queued: refusal is something a client can retry, a thread it is holding is not.
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        // Room for every concurrent run's worker and an operator's session many times over.
        // The number is here to bound a flood, not to ration ordinary use.
        Self {
            idle: Duration::from_secs(300),
            request: Duration::from_secs(10),
            max_connections: 64,
        }
    }
}

/// Bind the broker's listener on loopback, letting the OS pick the port.
///
/// Separate from [`serve`] so the caller can learn the address *before* constructing the
/// [`Broker`](super::Broker) that has to embed it in every session URL.
pub fn bind() -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
}

/// Start serving on a background thread, under [`Limits::default`]. Returns immediately.
///
/// There is no shutdown handle: the listener lives as long as the process, and every session
/// it could serve is revoked independently when its [`BrokerSession`](super::BrokerSession) is
/// dropped. Stopping the listener would add a second thing to get right for no property the
/// token lifetime does not already provide.
///
/// An accept thread that cannot be created is returned rather than panicking, so a caller at
/// startup can stop naming what failed (#218).
pub fn serve<S: McpService>(service: Arc<S>, listener: TcpListener) -> std::io::Result<()> {
    serve_with(service, listener, Limits::default())
}

/// [`serve`], with the bounds named. Tests set deadlines they can wait out; nothing else needs
/// to — the defaults are the policy and a caller that wanted looser ones would be removing the
/// property, not configuring it.
pub fn serve_with<S: McpService>(
    service: Arc<S>,
    listener: TcpListener,
    limits: Limits,
) -> std::io::Result<()> {
    let live = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new().name("crew-broker-accept".into()).spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    // Refused *before* the thread exists: accepting and then spawning is the
                    // step that costs something, so the cap has to be checked on this side of
                    // it. Dropping the stream closes it, which a client reads as a hangup.
                    let Some(slot) = ConnSlot::take(&live, limits.max_connections) else {
                        tracing::warn!(
                            max = limits.max_connections,
                            "refusing a connection: too many already in flight"
                        );
                        drop(s);
                        continue;
                    };
                    let broker = Arc::clone(&service);
                    // `Builder::spawn` rather than `thread::spawn`: the latter panics when the
                    // process is out of threads, and a panic on this thread would take the
                    // whole orchestrator down over a connection it could simply have refused.
                    let spawned = std::thread::Builder::new()
                        .name("crew-broker-conn".into())
                        .spawn(move || {
                            // Held for the life of the connection, released however it ends.
                            let _slot = slot;
                            if let Err(e) = handle_conn(broker.as_ref(), s, limits) {
                                tracing::debug!(error = %e, "broker connection ended");
                            }
                        });
                    if let Err(e) = spawned {
                        // The slot went into the closure and comes back with it.
                        tracing::warn!(error = %e, "broker could not serve a connection");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "broker accept failed"),
            }
        }
    })?;
    Ok(())
}

/// One connection's place in [`Limits::max_connections`], returned by [`Drop`].
///
/// RAII rather than a decrement at the end of `handle_conn`, for the same reason
/// [`BrokerSession`](super::BrokerSession) is: a path that ends the connection without
/// releasing the slot — an early `?`, a panic in a tool — leaks a slot permanently, and a
/// server that has leaked every slot refuses everyone while doing nothing.
pub(crate) struct ConnSlot(Arc<AtomicUsize>);

impl ConnSlot {
    pub(crate) fn take(live: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        live.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1))
            .ok()?;
        Some(Self(Arc::clone(live)))
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One request as read off the wire. `crewd init`'s callback listener reads with the same
/// function, so it inherits the same caps and the same idle/request deadline split.
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    /// The `Host` header, which the callback listener checks so a page that rebinds its own
    /// name to `127.0.0.1` cannot read the init page's nonce. The MCP transport ignores it.
    pub(crate) host: Option<String>,
    pub(crate) body: Vec<u8>,
}

fn handle_conn<S: McpService>(
    service: &S,
    stream: TcpStream,
    limits: Limits,
) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    // The write side of the same bound: a client that asks and then stops reading would
    // otherwise hold this thread inside `write_all` for as long as it liked.
    stream.set_write_timeout(Some(limits.request))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    // Keep-alive: one accept serves the whole run's traffic.
    while let Some(req) = read_request(&mut reader, limits)? {
        match req.method.as_str() {
            // The SSE stream. Nothing here ever pushes, so decline it.
            "GET" => respond(&mut writer, 405, "Method Not Allowed", None)?,
            // The client's session teardown.
            "DELETE" => respond(&mut writer, 200, "OK", None)?,
            "POST" => {
                let parsed: Result<Value, _> = serde_json::from_slice(&req.body);
                let Ok(rpc) = parsed else {
                    let body = serde_json::to_vec(&error_body(Value::Null, -32700, "parse error"))
                        .unwrap_or_default();
                    respond(&mut writer, 200, "OK", Some(&body))?;
                    continue;
                };
                match handle_rpc(service, &req.path, &rpc) {
                    // A notification: acknowledged, never answered.
                    None => respond(&mut writer, 202, "Accepted", None)?,
                    Some(v) => {
                        let body = serde_json::to_vec(&v).unwrap_or_default();
                        respond(&mut writer, 200, "OK", Some(&body))?;
                    }
                }
            }
            _ => respond(&mut writer, 405, "Method Not Allowed", None)?,
        }
    }
    Ok(())
}

/// `None` means the message was a notification and takes no reply.
fn handle_rpc<S: McpService>(service: &S, path: &str, rpc: &Value) -> Option<Value> {
    let method = rpc.get("method").and_then(Value::as_str).unwrap_or_default();
    let id = rpc.get("id").cloned();

    // No id at all: a notification. `notifications/initialized` is the one that matters, but
    // the rule is general — anything without an id gets no response body.
    let id = id?;

    let result = match method {
        "initialize" => {
            let version = rpc
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": service.name(), "version": env!("CARGO_PKG_VERSION") }
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": service.tools(path) }),
        "tools/call" => {
            let name = rpc.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
            let empty = json!({});
            let args = rpc.pointer("/params/arguments").unwrap_or(&empty);

            // A refusal is a tool *result*, not a transport error: the agent is meant to read
            // it and pick something else, which is the "a tool failure is not a run failure"
            // rule from the issue.
            match service.call(path, name, args) {
                Ok(text) => json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false
                }),
                Err(text) => json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": true
                }),
            }
        }
        // Everything else, `server/discover` included. See the module doc: the real client
        // handles this and continues.
        other => {
            return Some(error_body(id, -32601, &format!("method not found: {other}")));
        }
    };

    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error_body(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// `Ok(None)` on a clean EOF — the client closed, which is how every run ends.
///
/// Two deadlines rather than one, and the split is the whole point: waiting for the *first*
/// byte of the next request is an idle keep-alive connection, which is ordinary and may be
/// long, while waiting for the *rest* of a request that has already begun is a client
/// dribbling, which is not. A single deadline would have to be the generous one to keep
/// keep-alive working, and a generous deadline on a half-sent request is no deadline at all.
/// A timeout surfaces as an ordinary read error and ends the connection.
pub(crate) fn read_request(
    reader: &mut BufReader<TcpStream>,
    limits: Limits,
) -> std::io::Result<Option<Request>> {
    reader.get_ref().set_read_timeout(Some(limits.idle))?;
    let mut start = String::new();
    if reader.read_line(&mut start)? == 0 {
        return Ok(None);
    }
    // Tolerate a stray blank line between keep-alive requests.
    while start.trim().is_empty() {
        start.clear();
        if reader.read_line(&mut start)? == 0 {
            return Ok(None);
        }
    }

    reader.get_ref().set_read_timeout(Some(limits.request))?;

    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut host = None;
    let mut header_bytes = start.len();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(std::io::Error::other("request headers too large"));
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if k.trim().eq_ignore_ascii_case("host") {
                host = Some(v.trim().to_string());
            }
        }
    }

    if content_length > MAX_BODY_BYTES {
        return Err(std::io::Error::other("request body too large"));
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some(Request { method, path, host, body }))
}

fn respond(
    w: &mut TcpStream,
    status: u16,
    reason: &str,
    body: Option<&[u8]>,
) -> std::io::Result<()> {
    let body = body.unwrap_or(b"");
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n",
        body.len()
    );
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Transport tests over a real socket.
///
/// Deliberately not driven through a client library: the bugs this file can have are framing
/// bugs — a missing `Content-Length`, a notification answered with a body, a keep-alive
/// connection closed after one request — and every one of them is invisible to a test that
/// hands the server a pre-parsed request. Same reasoning as `ureq_http_tests` in
/// [`crate::tracker::github`]. The request sequence below is the one `claude 2.1.278` actually
/// sends, recorded from a live handshake.
#[cfg(test)]
mod tests {
    use std::io::{BufRead, ErrorKind};
    use std::sync::Arc;

    use super::*;
    use crate::broker::fake::FakeWrites;
    use crate::broker::{Broker, BrokerLimits, BrokerSession, TrackerWrites};
    use crate::clock::FakeClock;
    use crate::model::Issue;

    fn issue(id: &str) -> Issue {
        Issue {
            id: id.into(),
            identifier: format!("#{id}"),
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

    /// A live broker, serving on a real loopback port, with one open session.
    fn serving(tag: &str) -> (Arc<Broker>, Arc<FakeWrites>, BrokerSession, SocketAddr) {
        serving_with(tag, Limits::default())
    }

    /// [`serving`] under bounds a test can wait out. The defaults are minutes; a test that
    /// proves a deadline fires has to be able to reach it.
    fn serving_with(
        tag: &str,
        limits: Limits,
    ) -> (Arc<Broker>, Arc<FakeWrites>, BrokerSession, SocketAddr) {
        let writes = Arc::new(FakeWrites::new());
        let w: Arc<dyn TrackerWrites> = writes.clone();
        let dir = std::env::temp_dir().join(format!(
            "crew-broker-srv-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let listener = bind().unwrap();
        let addr = listener.local_addr().unwrap();
        let broker = Arc::new(
            Broker::new(
                w,
                Arc::new(FakeClock::new()),
                BrokerLimits::default(),
                vec!["in progress".into(), "done".into()],
                addr,
                dir,
            )
            .unwrap(),
        );
        serve_with(Arc::clone(&broker), listener, limits).unwrap();
        let session = broker.open(&issue("o/r#1"), "run-1").unwrap();
        (broker, writes, session, addr)
    }

    struct Conn {
        stream: TcpStream,
        reader: BufReader<TcpStream>,
    }

    impl Conn {
        fn open(addr: SocketAddr) -> Self {
            let stream = TcpStream::connect(addr).unwrap();
            let reader = BufReader::new(stream.try_clone().unwrap());
            Self { stream, reader }
        }

        /// Returns the status code and body. Panics rather than erroring: a framing failure
        /// here is the bug under test, and a panic names the line.
        fn send(&mut self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
            let body = body.unwrap_or("");
            let req = format!(
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: \
                 application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            self.stream.write_all(req.as_bytes()).unwrap();
            self.stream.flush().unwrap();

            let mut status_line = String::new();
            self.reader.read_line(&mut status_line).unwrap();
            let status: u16 = status_line.split_whitespace().nth(1).unwrap_or("0").parse().unwrap();

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
            if len > 0 {
                self.reader.read_exact(&mut buf).unwrap();
            }
            (status, String::from_utf8_lossy(&buf).to_string())
        }

        fn rpc(&mut self, path: &str, body: &str) -> Value {
            let (status, text) = self.send("POST", path, Some(body));
            assert_eq!(status, 200, "body was {text}");
            serde_json::from_str(&text).expect("a JSON-RPC response")
        }
    }

    #[test]
    fn the_handshake_the_real_client_performs_is_answered_end_to_end() {
        let (_b, writes, session, addr) = serving("handshake");
        let path = format!("/mcp/{}", session.token);
        let mut c = Conn::open(addr);

        // 1. `server/discover`, which this revision does not implement. A plain -32601 is
        //    what the real client falls through on.
        let discover =
            c.rpc(&path, r#"{"jsonrpc":"2.0","id":"d1","method":"server/discover","params":{}}"#);
        assert_eq!(discover["error"]["code"], -32601);

        // 2. initialize, echoing the version offered rather than asserting one.
        let init = c.rpc(
            &path,
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        assert!(init["result"]["capabilities"]["tools"].is_object());

        // 3. The initialized notification: no id, so no response body at all.
        let (status, body) = c.send(
            "POST",
            &path,
            Some(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        );
        assert_eq!(status, 202, "a notification takes no reply");
        assert!(body.is_empty(), "answering a notification breaks the handshake, got {body}");

        // 4. The SSE stream this server does not offer.
        let (status, _) = c.send("GET", &path, None);
        assert_eq!(status, 405);

        // 5. tools/list, on the same keep-alive connection as everything above.
        let list = c.rpc(&path, r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);

        // 6. A call that does real work.
        let call = c.rpc(
            &path,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"comment","arguments":{"body":"from the wire"},"_meta":{"progressToken":2}}}"#,
        );
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(writes.count(), 1, "exactly one tracker write reached the orchestrator");
    }

    #[test]
    fn a_refused_call_comes_back_as_a_tool_error_not_a_transport_error() {
        // The shape matters: a JSON-RPC error reads to the client as a broken server, which is
        // how a refused write turns into a failed run. `isError` on the result is something
        // the agent can read and route around.
        let (b, writes, session, addr) = serving("refusal");
        let mut c = Conn::open(addr);

        let call = c.rpc(
            &format!("/mcp/{}", session.token),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"comment","arguments":{"body":"hi","issue_id":"o/r#99"}}}"#,
        );
        assert!(call.get("error").is_none(), "must not be a JSON-RPC error: {call}");
        assert_eq!(call["result"]["isError"], true);
        assert!(
            call["result"]["content"][0]["text"].as_str().unwrap().contains("issue_id"),
            "the agent is told what was wrong: {call}"
        );
        assert_eq!(writes.count(), 0);
        assert_eq!(b.audit().len(), 1);
    }

    #[test]
    fn a_forged_token_reaches_no_issue() {
        let (_b, writes, _session, addr) = serving("forged");
        let mut c = Conn::open(addr);

        let call = c.rpc(
            "/mcp/0000000000000000000000000000000000000000000000000000000000000000",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"comment","arguments":{"body":"hi"}}}"#,
        );
        assert_eq!(call["result"]["isError"], true);
        assert_eq!(writes.count(), 0);
    }

    #[test]
    fn a_malformed_body_does_not_take_the_connection_down_with_it() {
        let (_b, _w, session, addr) = serving("garbage");
        let path = format!("/mcp/{}", session.token);
        let mut c = Conn::open(addr);

        let (status, text) = c.send("POST", &path, Some("{not json"));
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["error"]["code"], -32700);

        // The connection must still be usable: one bad frame from a confused client should not
        // cost the run every tool call after it.
        let list = c.rpc(&path, r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#);
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);
    }

    /// Blocks until the server hangs up, or reports that it did not. The client-side deadline
    /// is the test's own safety net: without it the failure mode of every assertion below is a
    /// hung test rather than a red one.
    ///
    /// A reset counts as a hangup. On macOS, closing a socket with bytes still unread sends an
    /// RST rather than a FIN, so a probe that *writes* before reading cannot tell a refusal
    /// from a failure — which is why the ones below write nothing.
    fn hung_up_on(stream: &mut TcpStream) -> bool {
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 64];
        match stream.read(&mut buf) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => matches!(e.kind(), ErrorKind::ConnectionReset | ErrorKind::BrokenPipe),
        }
    }

    #[test]
    fn a_client_that_never_finishes_its_request_cannot_hold_a_connection_thread() {
        // Slowloris, aimed at the operator's listener rather than the broker's: `api.allow_public`
        // can put this transport on a routable interface, and a thread per connection means a
        // client that connects and then goes quiet holds one for free. The deadline is what gives
        // it a price. Note what is *not* asserted here — that a connection survives sitting idle
        // between requests is `the_handshake_the_real_client_performs_is_answered_end_to_end`,
        // and one deadline could not satisfy both tests.
        let limits = Limits {
            idle: Duration::from_millis(200),
            request: Duration::from_millis(200),
            max_connections: 4,
        };
        let (_b, _w, session, addr) = serving_with("slowloris", limits);

        let mut stream = TcpStream::connect(addr).unwrap();
        // A request that begins and never ends: no blank line, so the header loop has nothing to
        // finish on and the body is never reached. Every byte written is consumed by the server,
        // so its hangup arrives as a clean EOF.
        let head = format!("POST /mcp/{} HTTP/1.1\r\nHost: localhost\r\n", session.token);
        stream.write_all(head.as_bytes()).unwrap();
        stream.flush().unwrap();

        assert!(
            hung_up_on(&mut stream),
            "the server must hang up on a half-sent request rather than wait out the client"
        );
    }

    #[test]
    fn a_flood_of_connections_is_refused_rather_than_served_without_bound() {
        // The deadline alone does not close this: a client that reconnects, or that opens
        // sockets and writes nothing at all, spends a thread per socket for as long as the
        // deadline allows it to.
        let limits = Limits {
            idle: Duration::from_secs(5),
            request: Duration::from_secs(5),
            max_connections: 2,
        };
        let (_b, _w, session, addr) = serving_with("flood", limits);
        let path = format!("/mcp/{}", session.token);

        // Two connections that have each *completed* a request are two the server is certainly
        // holding. Merely connecting proves nothing — the accept loop is another thread.
        let mut held = Vec::new();
        for _ in 0..2 {
            let mut c = Conn::open(addr);
            let pong = c.rpc(&path, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
            assert!(pong["result"].is_object());
            held.push(c);
        }

        let mut third = TcpStream::connect(addr).unwrap();
        assert!(
            hung_up_on(&mut third),
            "over the cap, a connection is closed rather than queued or served"
        );

        // And the slot comes back when the connection holding it ends, so the cap bounds how
        // many clients are in flight rather than how many the server will ever see. Polled
        // because the slot is released on the connection's own thread, which the test does not
        // synchronise with; without `ConnSlot` every probe below is refused and this never ends
        // in anything but a failure.
        drop(held.pop());
        let mut served = false;
        for _ in 0..100 {
            let mut probe = TcpStream::connect(addr).unwrap();
            probe.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
            let mut buf = [0u8; 1];
            // An immediate EOF is a refusal. Silence is the server waiting for a request on a
            // connection it accepted, which is the whole assertion.
            match probe.read(&mut buf) {
                Ok(0) => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    served = true;
                    break;
                }
            }
        }
        assert!(served, "a finished connection must give its slot back");
    }
}
