//! The other end of `crewd`'s ops API, behind `crewctl status`.
//!
//! The API landed before this did, and in the window between the two it was not reached for
//! once — diagnosing a live run meant tailing a block-buffered log file hours behind the
//! daemon, which produced a wrong answer. `curl` would have been right and instant. Nobody
//! typed it because there was nothing to type: a port to remember and a `jq` filter to write
//! is enough friction to send an operator back to the logs. So this is deliberately a client
//! and nothing more — no new server surface, and no second way to reach the scheduler.
//!
//! Two properties it inherits rather than re-decides:
//!
//! * **It reads the published API only.** No `Store`, no `~/.claude/tasks`, no git. The same
//!   rule that keeps the TUI from becoming load-bearing, for the same reason — and the same
//!   consequence, which is that a question this cannot answer is a missing `Snapshot` field
//!   rather than a reason to open the database here. [`crate::Row::branch`] was added
//!   under exactly that rule.
//! * **It shares the wire types with the server.** [`Snapshot`] and [`Row`] are serialised by
//!   one and deserialised by the other, so a field renamed on the scheduler breaks this at
//!   compile time instead of silently rendering a blank column.
//!
//! What this module owns beyond the request is the part an operator actually feels: finding
//! the daemon without being told where it is ([`endpoint`]), and saying something useful when
//! it is not there ([`StatusError`]).

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::api::{
    API_MARKER_HEADER, API_MARKER_VERSION, ApiConfig, DEFAULT_API_BIND, normalize_bind,
};
use crate::{Row, Snapshot};

/// A status query is a question about right now, so it fails fast rather than hanging on a
/// daemon that accepted the connection and then wedged. Generous next to a loopback round
/// trip, short next to an operator's patience.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Where the address came from, so a failure can say which one it tried and why.
///
/// An operator who typoed `api.bind` and one who never enabled the API see the same
/// connection refusal; only this tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An explicit `--api`.
    Flag,
    /// An `[api] bind` in this config file.
    Config(PathBuf),
    /// Nothing named one, so [`DEFAULT_API_BIND`].
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Flag => write!(f, "from --api"),
            Source::Config(p) => write!(f, "from [api] bind in {}", p.display()),
            Source::Default => write!(f, "the default; no --api and no [api] bind in the config"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub addr: String,
    pub source: Source,
}

/// Find the daemon without making the operator remember a port.
///
/// `--api`, then the config's `[api] bind`, then [`DEFAULT_API_BIND`] — the same order of
/// precedence `main` uses when it decides where to *serve*, so the client looks where the
/// daemon was told to listen. Both of the first two are run through [`normalize_bind`],
/// the same trim the daemon's `api::bind` applies before parsing — a `--api` flag or a `[api] bind` with
/// incidental surrounding whitespace must not bind successfully on the server side and build an
/// unreachable URL on this one.
pub fn endpoint(explicit: Option<&str>, config_path: &Path) -> Endpoint {
    if let Some(addr) = explicit.map(normalize_bind).filter(|a| !a.is_empty()) {
        return Endpoint { addr: addr.to_string(), source: Source::Flag };
    }
    match bind_in(config_path) {
        Some(addr) => Endpoint { addr, source: Source::Config(config_path.to_path_buf()) },
        None => Endpoint { addr: DEFAULT_API_BIND.to_string(), source: Source::Default },
    }
}

/// Read `[api] bind` out of a config, forgivingly.
///
/// Deliberately not the daemon's `Config::load`: that runs `preflight`, which gates
/// *dispatch*. A missing `tracker.owner` is a real problem for the daemon and none at all for
/// a client asking what the daemon is doing — refusing to print status over it would be the
/// same class of mistake as validating `api.bind` in preflight. Anything unreadable falls
/// through to the default, where a wrong guess costs one clearly-labelled connection refusal.
///
/// The value is normalised with [`normalize_bind`] before it is handed back — the same
/// trim the daemon's `api::bind` applies server-side — so `bind = " 127.0.0.1:8787 "` binds the daemon
/// and reaches it from here, rather than binding the daemon while this builds an invalid URL.
fn bind_in(config_path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct JustApi {
        api: ApiConfig,
    }
    let text = std::fs::read_to_string(config_path).ok()?;
    let parsed: JustApi = toml::from_str(&text).ok()?;
    Some(normalize_bind(&parsed.api.bind).to_string())
}

/// Why a status query did not produce a snapshot.
///
/// The split that matters is the first two variants, and it is the one an operator cannot make
/// from a stack trace: nothing accepted the connection, versus something accepted it and said
/// no. They have different fixes and only one of them means "the daemon is down".
#[derive(Debug)]
pub enum StatusError {
    /// Nothing is listening. The daemon is not running — or it is, with its API off, which is
    /// the default and so the likelier of the two.
    NotListening { endpoint: Endpoint },
    /// A daemon answered and refused. Reachable, so this is a question about the request or
    /// the daemon's state, not about whether it is up.
    Refused { endpoint: Endpoint, status: u16, detail: String },
    /// Reachable in principle, but the connection did not complete in time.
    Unreachable { endpoint: Endpoint, why: String },
    /// Something answered that is not this API — a different service on the port, or a version
    /// that no longer speaks this shape.
    Unrecognised { endpoint: Endpoint, why: String },
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatusError::NotListening { endpoint: e } => write!(
                f,
                "nothing is listening on {} ({}).\n\n\
                 Either no daemon is running, or one is running with its ops API off — it is \
                 off by default. Turn it on with `[api] enabled = true` in the config, or \
                 start the daemon with `--api {}` for one run.",
                e.addr, e.source, e.addr
            ),
            StatusError::Refused { endpoint: e, status, detail } => write!(
                f,
                "the daemon on {} ({}) is running and refused the request: HTTP {status}, \
                 {detail}",
                e.addr, e.source
            ),
            StatusError::Unreachable { endpoint: e, why } => write!(
                f,
                "could not reach {} ({}): {why}.\n\n\
                 Something is in the way rather than absent — a firewall, or a daemon that \
                 accepted the connection and then stopped answering.",
                e.addr, e.source
            ),
            StatusError::Unrecognised { endpoint: e, why } => write!(
                f,
                "{} ({}) answered, but not as this ops API: {why}.\n\n\
                 Another service is probably on that port.",
                e.addr, e.source
            ),
        }
    }
}

impl std::error::Error for StatusError {}

/// The read routes this client uses, spelled once.
const SNAPSHOT: &str = "/api/v1/snapshot";

fn issue_path(key: &str) -> String {
    format!("/api/v1/issues/{}", encode_segment(key))
}

/// A daemon's published state, read over the API it publishes it on.
pub struct Client {
    endpoint: Endpoint,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn snapshot(&self) -> Result<Snapshot, StatusError> {
        self.get(SNAPSHOT)
    }

    /// One issue by dispatch id or identifier, resolved server-side — this client does not
    /// fetch the whole snapshot and filter, because a duplicated identifier is the API's
    /// `409` to answer and not this one's to guess at.
    pub fn issue(&self, key: &str) -> Result<Row, StatusError> {
        self.get(&issue_path(key))
    }

    /// The same routes, unparsed, for `--json`.
    ///
    /// They are separate methods rather than one taking a path because the first version took a
    /// path: `--json` built its own URL, skipped [`encode_segment`], and turned an identifier
    /// containing `/` into a 404 that read as a missing issue on exactly the input the typed
    /// call handles. Routes are constructed in one place now so the two cannot diverge again.
    pub fn raw_snapshot(&self) -> Result<String, StatusError> {
        self.fetch(SNAPSHOT).map(|(_, body)| body)
    }

    pub fn raw_issue(&self, key: &str) -> Result<String, StatusError> {
        self.fetch(&issue_path(key)).map(|(_, body)| body)
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, StatusError> {
        let (_, body) = self.fetch(path)?;
        serde_json::from_str(&body).map_err(|e| StatusError::Unrecognised {
            endpoint: self.endpoint.clone(),
            why: format!("the response is not a snapshot this version understands ({e})"),
        })
    }

    fn fetch(&self, path: &str) -> Result<(u16, String), StatusError> {
        let reply = self.exchange(path)?;
        let Some(response) = Response::parse(&reply) else {
            return Err(StatusError::Unrecognised {
                endpoint: self.endpoint.clone(),
                why: "its reply is not valid HTTP".into(),
            });
        };

        // Settled before either the status or the body is trusted: a status code and a JSON body
        // are exactly what an unrelated service on this port could also produce (a 404 from
        // nginx reads just like this API's own 404, and a 200 would otherwise be handed straight
        // through by the `--json` paths).
        if response.header(API_MARKER_HEADER) != Some(API_MARKER_VERSION) {
            return Err(StatusError::Unrecognised {
                endpoint: self.endpoint.clone(),
                why: "it answered without this API's marker header — probably a different \
                      service on that port"
                    .into(),
            });
        }

        let body = response.body.ok_or_else(|| StatusError::Unreachable {
            endpoint: self.endpoint.clone(),
            why: "the response head arrived but its body did not".into(),
        })?;
        if response.status == 200 {
            return Ok((response.status, body));
        }
        Err(StatusError::Refused {
            endpoint: self.endpoint.clone(),
            status: response.status,
            detail: message_in(&body),
        })
    }

    /// One GET, read to the end: the ops API answers every request with `Connection: close`,
    /// so end-of-stream is where its response ends.
    ///
    /// Hand-written over `std::net` rather than an HTTP crate so this client links no HTTP
    /// stack at all (#45). What it keeps from the crate it replaced is the one distinction that
    /// matters to an operator: a refused connection on a loopback address is "no daemon", never
    /// "busy daemon", so it is [`StatusError::NotListening`] and nothing vaguer.
    fn exchange(&self, path: &str) -> Result<Vec<u8>, StatusError> {
        let endpoint = || self.endpoint.clone();
        let addrs: Vec<SocketAddr> = match self.endpoint.addr.to_socket_addrs() {
            Ok(a) => a.collect(),
            Err(e) => {
                return Err(StatusError::Unreachable {
                    endpoint: endpoint(),
                    why: format!("that is not a usable address ({e})"),
                });
            }
        };
        if addrs.is_empty() {
            return Err(StatusError::Unreachable {
                endpoint: endpoint(),
                why: "the host does not resolve".into(),
            });
        }

        let mut last = None;
        let mut stream = None;
        for addr in &addrs {
            match TcpStream::connect_timeout(addr, TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let Some(mut stream) = stream else {
            return Err(match last {
                Some(e) if e.kind() == ErrorKind::ConnectionRefused => {
                    StatusError::NotListening { endpoint: endpoint() }
                }
                Some(e) if timed_out(&e) => StatusError::Unreachable {
                    endpoint: endpoint(),
                    why: "it did not answer in time".into(),
                },
                Some(e) => StatusError::Unreachable { endpoint: endpoint(), why: e.to_string() },
                None => StatusError::NotListening { endpoint: endpoint() },
            });
        };

        let io = |e: std::io::Error| StatusError::Unreachable {
            endpoint: endpoint(),
            why: if timed_out(&e) { "it did not answer in time".into() } else { e.to_string() },
        };
        stream.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\n\
             Connection: close\r\n\r\n",
            self.endpoint.addr
        );
        stream.write_all(request.as_bytes()).map_err(io)?;

        let mut reply = Vec::new();
        stream.take(MAX_REPLY_BYTES + 1).read_to_end(&mut reply).map_err(io)?;
        if reply.len() as u64 > MAX_REPLY_BYTES {
            return Err(StatusError::Unrecognised {
                endpoint: endpoint(),
                why: format!("its reply is larger than {MAX_REPLY_BYTES} bytes"),
            });
        }
        Ok(reply)
    }
}

/// A socket timeout surfaces as `WouldBlock` on some platforms and `TimedOut` on others.
fn timed_out(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

/// A snapshot of a busy daemon is tens of kilobytes; this only stops something that is not the
/// ops API from filling memory.
const MAX_REPLY_BYTES: u64 = 32 * 1024 * 1024;

/// A parsed HTTP/1.1 response. `body` is `None` when the head promised more bytes than arrived,
/// or the body is not UTF-8.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

impl Response {
    fn parse(reply: &[u8]) -> Option<Self> {
        let split = reply.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = std::str::from_utf8(&reply[..split]).ok()?;
        let mut lines = head.split("\r\n");
        let mut status_line = lines.next()?.splitn(3, ' ');
        if !status_line.next()?.starts_with("HTTP/1.") {
            return None;
        }
        let status = status_line.next()?.parse().ok()?;
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();

        let mut body = &reply[split + 4..];
        let declared = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse::<usize>().ok());
        let body = match declared {
            Some(n) if body.len() < n => None,
            Some(n) => {
                body = &body[..n];
                String::from_utf8(body.to_vec()).ok()
            }
            None => String::from_utf8(body.to_vec()).ok(),
        };
        Some(Self { status, headers, body })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// The API's `{"error": ...}` if the body carries one, and the body itself otherwise. A
/// non-JSON error body is still the most informative thing available, so it is shown rather
/// than replaced with a generic line.
fn message_in(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() { "no detail".into() } else { trimmed.to_string() }
        })
}

/// Percent-encode one path segment.
///
/// Identifiers are whatever the tracker hands over, and the server splits on `/` before it
/// decodes — so a `/` left raw here would arrive as a segment boundary and miss the route it
/// was aimed at. Encoding conservatively (anything outside unreserved) keeps `?`, `#` and
/// space out of the target too.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(name: &str, body: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("crew-client-{}-{name}.toml", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn the_address_comes_from_the_flag_then_the_config_then_the_default() {
        let cfg = write_config("precedence", "[api]\nbind = \"127.0.0.1:9001\"\n");

        let flag = endpoint(Some("127.0.0.1:9999"), &cfg);
        assert_eq!(flag.addr, "127.0.0.1:9999");
        assert_eq!(flag.source, Source::Flag);

        let from_cfg = endpoint(None, &cfg);
        assert_eq!(from_cfg.addr, "127.0.0.1:9001");
        assert_eq!(from_cfg.source, Source::Config(cfg.clone()));

        let missing = endpoint(None, Path::new("/nonexistent/crew.toml"));
        assert_eq!(missing.addr, DEFAULT_API_BIND);
        assert_eq!(missing.source, Source::Default);

        std::fs::remove_file(&cfg).ok();
    }

    #[test]
    fn an_unparseable_config_falls_back_rather_than_failing() {
        let cfg = write_config("garbage", "this is not toml {{{");
        let e = endpoint(None, &cfg);
        assert_eq!(e.addr, DEFAULT_API_BIND);
        assert_eq!(e.source, Source::Default);
        std::fs::remove_file(&cfg).ok();
    }

    #[test]
    fn a_config_without_an_api_table_reports_the_default_as_the_default() {
        // The address is the same either way; what must not happen is blaming a file that
        // never named one when the connection is refused.
        let cfg = write_config("no-api", "[tracker]\nkind = \"fake\"\n");
        let e = endpoint(None, &cfg);
        assert_eq!(e.addr, DEFAULT_API_BIND);
        assert_eq!(e.source, Source::Default);
        std::fs::remove_file(&cfg).ok();
    }

    #[test]
    fn an_identifier_with_a_separator_cannot_escape_its_path_segment() {
        // The server splits before it decodes, so a raw `/` here would route somewhere else.
        assert_eq!(encode_segment("MT-649"), "MT-649");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("a b?c#d"), "a%20b%3Fc%23d");
        assert_eq!(encode_segment("iss_1.2~3"), "iss_1.2~3");
    }

    #[test]
    fn an_error_body_is_quoted_back_whether_or_not_it_is_json() {
        assert_eq!(message_in(r#"{"error":"no such endpoint"}"#), "no such endpoint");
        assert_eq!(message_in("plain text failure"), "plain text failure");
        assert_eq!(message_in("   "), "no detail");
        assert_eq!(message_in(r#"{"other":1}"#), r#"{"other":1}"#);
    }

    #[test]
    fn a_closed_port_reads_as_no_daemon_rather_than_a_refused_request() {
        // The distinction issue #24 asks for, against a real closed port: bind one to learn a
        // number the OS just handed out, then drop it so nothing is there. That port is in the
        // ephemeral range, so Linux can pick it as the connecting socket's own source port and
        // the connect succeeds against itself (TCP self-connect), failing later with a reset. A
        // connection that got through says nothing about a closed port, so take another one; a
        // refusal reported as anything but NotListening still fails on the first try.
        let mut attempt = 0;
        let (addr, client) = loop {
            attempt += 1;
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = probe.local_addr().unwrap().to_string();
            drop(probe);

            let client = Client::new(Endpoint { addr: addr.clone(), source: Source::Flag });
            match client.snapshot() {
                Err(StatusError::NotListening { .. }) => break (addr, client),
                Err(StatusError::Unreachable { why, .. })
                    if attempt < 5 && !why.contains("refused") => {}
                other => panic!("a closed port must read as NotListening, got {other:?}"),
            }
        };

        // And the message has to be actionable, not just correct.
        let rendered = client.snapshot().unwrap_err().to_string();
        assert!(rendered.contains(&addr), "the message must name the address tried: {rendered}");
        assert!(rendered.contains("[api] enabled"), "it must name the likeliest fix: {rendered}");
    }
}
