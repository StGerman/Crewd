//! The ops API's address and wire constants, shared by the daemon that serves it and the client
//! that finds it, so the two can never disagree about the default port or the marker header.

use serde::{Deserialize, Serialize};

/// Where the ops API listens unless told otherwise, and equally where
/// [`crate::client`] looks for it when neither a flag nor a config names an address. One
/// constant so those two can never disagree about what "the default" is.
pub const DEFAULT_API_BIND: &str = "127.0.0.1:8787";

fn d_api_bind() -> String {
    DEFAULT_API_BIND.to_string()
}
/// Where the ops MCP server (`crewd`'s `api::mcp`) listens unless told otherwise. A different
/// port from [`DEFAULT_API_BIND`] because it is a different listener by design — see that
/// module on why the two operator surfaces, and the broker, never share one.
pub const DEFAULT_MCP_BIND: &str = "127.0.0.1:8788";

fn d_mcp_bind() -> String {
    DEFAULT_MCP_BIND.to_string()
}

/// The ops HTTP surface ([`crate::api`]).
///
/// Off by default, and loopback when on: `POST /refresh`, `POST /unquarantine` and `POST /unblock` control
/// agent execution, so an orchestrator that grows a control plane merely by being upgraded, or
/// that binds `0.0.0.0` because a field was left at a convenient default, is not something an
/// operator asked for.
///
/// Deliberately absent from `crewd`'s `Config::preflight`, which runs before every dispatch: the
/// address is used once, at startup, so it is parsed there instead, by `crewd`'s `api::bind` and
/// only when the API is on. A failure there stops startup naming the address (#218), and a
/// config with the API off is never refused over it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default)]
    pub enabled: bool,
    /// `host:port`. Refused unless the host is loopback or `allow_public` is set.
    #[serde(default = "d_api_bind")]
    pub bind: String,
    /// Permit a non-loopback bind — a deliberate decision to expose the write endpoints to
    /// whatever can reach that interface, which is why it is a separate flag rather than an
    /// inference from the address. Governs `bind` and `mcp_bind` alike: they carry the same
    /// write actions.
    #[serde(default)]
    pub allow_public: bool,
    /// The ops API's routes as MCP tools, for a supervising agent (`crewd`'s `api::mcp`). Off
    /// by default like `enabled`, and independent of it: a daemon watched by a person needs the
    /// HTTP API and a daemon watched by an agent needs this, and neither should have to carry
    /// the other. **Never reachable by a dispatched worker** — see that module's doc for what
    /// enforces it and for the one operator-side rule that has to hold.
    #[serde(default)]
    pub mcp_enabled: bool,
    /// `host:port` for the MCP server. Its own listener, never the HTTP API's and never the
    /// broker's.
    #[serde(default = "d_mcp_bind")]
    pub mcp_bind: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: d_api_bind(),
            allow_public: false,
            mcp_enabled: false,
            mcp_bind: d_mcp_bind(),
        }
    }
}

/// Every response this API writes carries this header, so [`crate::client::Client`] can tell "the ops
/// API answered" apart from "something on this port answered" before it trusts anything else in
/// the response. Without it, a 404 from an unrelated service on the same port reads exactly like
/// this router's own 404, and `--json` would pass either straight through. A header rather than
/// a body field: the body is [`crate::Snapshot`]/[`crate::Row`] JSON, handed back to an operator verbatim by
/// the client's `--json` paths, and a marker key inside it would leak into output meant to be
/// piped into `jq`.
pub const API_MARKER_HEADER: &str = "X-Crew-Ops-Api";
/// A version rather than a bare flag, so a wire-incompatible future change has somewhere to say
/// so. Today the client only checks that this equals what it expects.
pub const API_MARKER_VERSION: &str = "1";

/// The one normalization `api.bind` gets before it is treated as an address. Shared with
/// `client::bind_in` (and, through it, [`crate::client::endpoint`]'s `--api` handling) so a client
/// built from the same config file looks for the daemon at the address the daemon actually
/// bound — a value with incidental surrounding whitespace must not bind here and build an
/// unreachable URL there.
pub fn normalize_bind(raw: &str) -> &str {
    raw.trim()
}
