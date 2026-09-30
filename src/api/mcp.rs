//! The ops API as MCP tools, for the agent supervising this daemon.
//!
//! `crewctl status` closed the gap between the HTTP API and a person (#24). An agent
//! supervising the daemon — one driving a dogfooding session, a watchdog later — was still on
//! the wrong side of it: it had to spawn the CLI and parse a rendering that reuses `fmt_count`,
//! `fmt_ms` and `Phase::label` precisely so it reads well to a human, and so changes whenever
//! the dashboard does. Every fact it needs is in the published [`Snapshot`]; this is a way to
//! ask for it as data.
//!
//! ## What it is, exactly
//!
//! One tool per HTTP route and nothing else — `snapshot`, `issue`, `refresh`, `unquarantine`,
//! `unblock` — each answering with the same JSON body the corresponding route writes. That is not a
//! resemblance kept up by discipline: every tool runs the *same* [`Api`] method the router
//! runs, and frames the resulting [`Response`] as a tool result instead of an HTTP response. A
//! status below 400 is a plain result; anything else is the same body with `isError: true`, so
//! a supervising agent reads a 404 as "no such issue" rather than as a broken server. New
//! authority is a decision separate from new transport, and this module cannot express one: it
//! holds an `Api`, which holds a `watch::Receiver` and a [`Command`](super::Command) sender and
//! no `Store`, so rule 3 is enforced by the type here as it already is over HTTP.
//!
//! The transport is the broker's hand-rolled server, made generic over
//! [`McpService`](crate::broker::McpService) rather than copied; see
//! [`crate::broker::server`]'s module doc for what that cost.
//!
//! ## The constraint that must survive review
//!
//! **crewd must never hand this server to a dispatched agent.** The broker exists because a
//! worker gets authority scoped to one issue; this is scoped to the whole daemon. A worker that could call
//! `unquarantine` or `unblock` could clear its own quarantine or park and re-dispatch itself, defeating
//! `max_turns_per_issue`, the verdict and `parked_state` in one move — the three independent
//! brakes the invariant table says to keep all of.
//!
//! What the crate controls, it enforces by wiring rather than by a check on the tool:
//!
//! * The only MCP servers crewd hands a worker are named in one file, the `--mcp-config` the
//!   broker writes in [`Broker::open`](crate::broker::Broker::open). That file names the broker's own listener
//!   and nothing else, and no code path hands this type to the broker or the broker's address
//!   to this type. `a_dispatched_worker_is_not_handed_the_ops_tools` reads the file a real
//!   session produces and connects to what it names.
//! * This server binds **its own listener**, never the broker's. A shared listener routed by
//!   path prefix would put the operator tools at the exact `host:port` every worker is handed;
//!   two ports make the separation a property of the address rather than of a string compare.
//!   The path `/ops` is accepted and every other path is refused, so a worker's `/mcp/<token>`
//!   URL pointed here by mistake answers no tools.
//!
//! A worker never loads this server from the operator's `claude` configuration either: it runs
//! with `--strict-mcp-config`, so only the broker's `--mcp-config` starts (#191). Before that, a
//! dispatched run on 2026-09-26 listed `crew_ops` connected while it was registered only at
//! local scope, because a worktree is the same project to Claude Code. A same-user process can
//! still call the loopback HTTP API serving the same routes; only OS confinement (#135) closes
//! that. Off by default for the same reason the HTTP API is: a daemon must not grow a control plane by being upgraded.
//!
//! ## Exposure
//!
//! `api.mcp_enabled`, or `--mcp <addr>` for one run; loopback unless `api.allow_public` says
//! otherwise, through the same [`resolve_bind`](super::resolve_bind) the HTTP API uses, because
//! the two surfaces carry the same two write actions and one flag should govern both. A bind
//! failure stops crewd at startup naming the address, as [`super::bind`]'s does (#218).
//!
//! `allow_public` is what makes the transport's own bounds load-bearing here, and it is the
//! reason they exist: the broker's listener is always loopback and serves a handful of workers,
//! but this one is an address an operator picks, on a server that spends a thread per
//! connection. [`Limits`](crate::broker::server::Limits) is what the HTTP API's `READ_TIMEOUT`
//! is over there — a deadline on a request that has begun and never ends, split from the idle
//! wait so keep-alive still works, plus a cap on connections in flight.
//! `a_client_that_never_finishes_its_request_cannot_hold_a_connection_thread` holds it.

use std::net::TcpListener;

use anyhow::Context;
use serde_json::{Value, json};

use super::{Api, Response};
use crate::broker::McpService;
use crate::config::ApiConfig;

/// The MCP server name. Tools reach the supervising agent as `mcp__crew_ops__<tool>`.
/// Distinct from the broker's [`SERVER_NAME`](crate::broker::SERVER_NAME) so a transcript
/// shows at a glance which surface a call went to.
pub const SERVER_NAME: &str = "crew_ops";

/// The one path this server answers. Fixed rather than tokened: the authority here is
/// the address, which is why the address is loopback and its own port.
pub const PATH: &str = "/ops";

pub const TOOL_SNAPSHOT: &str = "snapshot";
pub const TOOL_ISSUE: &str = "issue";
pub const TOOL_REFRESH: &str = "refresh";
pub const TOOL_UNQUARANTINE: &str = "unquarantine";
pub const TOOL_UNBLOCK: &str = "unblock";

/// Every tool this server offers. One place, so the wiring test and `tools/list` cannot
/// disagree about what "an ops tool" is.
pub const TOOLS: &[&str] =
    &[TOOL_SNAPSHOT, TOOL_ISSUE, TOOL_REFRESH, TOOL_UNQUARANTINE, TOOL_UNBLOCK];

/// Bind the ops MCP listener, refusing an exposure nobody asked for.
///
/// Synchronous, unlike [`super::bind`], because the transport it feeds is the broker's
/// thread-per-connection server rather than a tokio task — which is also why a public bind
/// here is bounded by [`Limits`](crate::broker::server::Limits) rather than by the address
/// being loopback. Separate from serving for the same reason as the HTTP one: `main` binds at
/// startup, before anything else is opened, and exits naming the address on a failure (#218).
pub fn bind(cfg: &ApiConfig) -> anyhow::Result<TcpListener> {
    let addr = super::resolve_bind("api.mcp_bind", &cfg.mcp_bind, cfg.allow_public)?;
    TcpListener::bind(addr).with_context(|| format!("binding the ops MCP server to {addr}"))
}

/// The ops routes, as tools. See the module doc.
pub struct OpsMcp {
    api: Api,
}

impl OpsMcp {
    pub fn new(api: Api) -> Self {
        Self { api }
    }

    /// The tool list, as `tools/list` returns it.
    pub fn tools_json() -> Value {
        let key = json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The issue's dispatch id or tracker identifier. A dispatch \
                                    id resolves exactly; an identifier shared by two issues \
                                    answers with both ids to retry with."
                }
            },
            "required": ["key"],
            "additionalProperties": false
        });
        let none = json!({ "type": "object", "properties": {}, "additionalProperties": false });
        json!([
            {
                "name": TOOL_SNAPSHOT,
                "description": "The orchestrator's published snapshot: every issue it knows, \
                                with phase, attempt, turns, tokens, branch and run history, \
                                plus the running/limit/quarantined totals. The same JSON as \
                                GET /api/v1/snapshot.",
                "inputSchema": none
            },
            {
                "name": TOOL_ISSUE,
                "description": "One issue in full, by dispatch id or identifier. The same JSON \
                                as GET /api/v1/issues/:key; a 404 body if there is no such \
                                issue.",
                "inputSchema": key
            },
            {
                "name": TOOL_REFRESH,
                "description": "Run one scheduler tick now and return the snapshot it \
                                published. The same action as POST /api/v1/refresh and the \
                                dashboard's `r` key.",
                "inputSchema": none
            },
            {
                "name": TOOL_UNQUARANTINE,
                "description": "Clear an issue's quarantine so it is dispatchable again. \
                                Reports `cleared: false` rather than failing when the issue \
                                was not quarantined. The same action as POST \
                                /api/v1/unquarantine/:key and the dashboard's `u` key.",
                "inputSchema": key
            },
            {
                "name": TOOL_UNBLOCK,
                "description": "Lift the park on an issue a `Done` or `Blocked` verdict left \
                                waiting — a gate's rebase conflict, say — once its cause has \
                                been resolved, so the next tick dispatches it onto its \
                                existing branch. Write what changed into the issue's \
                                description first: that is the prompt the agent reads. \
                                Reports `cleared: false` rather than failing when the issue \
                                is not parked, is running, gating or waiting on a retry, its \
                                delivery still owns the branch, or its ticket is no \
                                longer in an active state. \
                                The same action as POST /api/v1/unblock/:key and the \
                                dashboard's `b` key.",
                "inputSchema": key
            }
        ])
    }

    /// Run one tool the way the router runs its route, on the calling thread.
    fn run(&self, tool: &str, args: &Value) -> Response {
        let key = match argument(tool, args) {
            Ok(k) => k,
            Err(refused) => return refused,
        };
        match (tool, key) {
            (TOOL_SNAPSHOT, None) => super::json_of(200, &self.api.latest()),
            (TOOL_ISSUE, Some(key)) => self.api.issue(&key),
            (TOOL_REFRESH, None) => self.api.refresh_blocking(),
            (TOOL_UNQUARANTINE, Some(key)) => self.api.unquarantine_blocking(&key),
            (TOOL_UNBLOCK, Some(key)) => self.api.unblock_blocking(&key),
            _ => Response::error(400, &format!("unknown tool: {tool}")),
        }
    }
}

/// Validate a call's arguments and return its `key`, when the tool takes one.
///
/// Unknown arguments are refused rather than ignored, as the broker refuses them: an argument
/// the schema does not name is a call the model built for a tool that does not exist here.
/// `_meta` is the one exception — MCP clients attach it as protocol plumbing.
fn argument(tool: &str, args: &Value) -> Result<Option<String>, Response> {
    let takes_key = match tool {
        TOOL_SNAPSHOT | TOOL_REFRESH => false,
        TOOL_ISSUE | TOOL_UNQUARANTINE | TOOL_UNBLOCK => true,
        other => return Err(Response::error(400, &format!("unknown tool: {other}"))),
    };
    let obj =
        args.as_object().ok_or_else(|| Response::error(400, "arguments must be an object"))?;

    let unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "_meta" && !(takes_key && *k == "key"))
        .collect();
    if !unknown.is_empty() {
        return Err(Response::error(
            400,
            &format!("unexpected argument(s) for {tool}: {}", unknown.join(", ")),
        ));
    }
    if !takes_key {
        return Ok(None);
    }
    let key = obj
        .get("key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            Response::error(400, &format!("{tool} requires a non-empty string `key`"))
        })?;
    Ok(Some(key.to_string()))
}

/// `/ops`, tolerating a trailing slash — the one normalisation a hand-typed URL needs.
fn is_ops_path(path: &str) -> bool {
    path.trim_end_matches('/') == PATH
}

impl McpService for OpsMcp {
    fn name(&self) -> &str {
        SERVER_NAME
    }

    fn tools(&self, path: &str) -> Value {
        // A client on the wrong path is most likely a worker whose `/mcp/<token>` URL was pointed
        // at this port by a misconfiguration. It gets nothing to call, not a list to try.
        if is_ops_path(path) { Self::tools_json() } else { json!([]) }
    }

    fn call(&self, path: &str, tool: &str, args: &Value) -> Result<String, String> {
        if !is_ops_path(path) {
            tracing::warn!(path, tool, "ops MCP call on an unknown path refused");
            return Err(format!("no tools at {path:?}; the ops tools are served at {PATH}"));
        }
        let response = self.run(tool, args);
        tracing::debug!(tool, status = response.status, "ops MCP call");
        let text = String::from_utf8_lossy(&response.body).into_owned();
        if response.status < 400 { Ok(text) } else { Err(text) }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::model::Phase;
    use crate::sched::{Row, Snapshot};

    fn row(issue_id: &str, identifier: &str) -> Row {
        Row {
            issue_id: issue_id.into(),
            identifier: identifier.into(),
            phase: Phase::Running,
            ..Default::default()
        }
    }

    fn ops(rows: Vec<Row>) -> (OpsMcp, mpsc::UnboundedReceiver<super::super::Command>) {
        let (_tx, rx) = watch::channel(Snapshot { rows, ticks: 7, ..Default::default() });
        // Leaked so the watch stays open for the test's duration; a dropped sender would still
        // serve the last value, but keeping it live is closer to the daemon.
        std::mem::forget(_tx);
        let (ctx, crx) = mpsc::unbounded_channel();
        (OpsMcp::new(Api::new(rx, ctx)), crx)
    }

    fn parsed(result: Result<String, String>) -> Value {
        serde_json::from_str(&result.expect("a plain result")).expect("the body is JSON")
    }

    #[test]
    fn a_read_tool_answers_from_the_snapshot_and_sends_the_scheduler_nothing() {
        // The property `run_status` holds by owning no `Store`: asking what the daemon is
        // doing cannot disturb it. Here the check is on the wire — a read produces no
        // `Command`, so there is nothing the scheduler loop could even be asked to do.
        let (svc, mut commands) = ops(vec![row("iss-a", "MT-1")]);

        let snap = parsed(svc.call(PATH, TOOL_SNAPSHOT, &json!({})));
        assert_eq!(snap["ticks"], 7);
        assert_eq!(snap["rows"][0]["identifier"], "MT-1");

        let issue = parsed(svc.call(PATH, TOOL_ISSUE, &json!({ "key": "MT-1" })));
        assert_eq!(issue["issue_id"], "iss-a");

        let by_id = parsed(svc.call(PATH, TOOL_ISSUE, &json!({ "key": "iss-a" })));
        assert_eq!(by_id["identifier"], "MT-1");

        assert!(
            matches!(commands.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "a read must not reach the scheduler"
        );
    }

    #[test]
    fn a_missing_issue_is_a_tool_error_carrying_the_routes_own_404_body() {
        let (svc, _c) = ops(vec![row("iss-a", "MT-1")]);
        let err = svc.call(PATH, TOOL_ISSUE, &json!({ "key": "MT-9" })).unwrap_err();
        let body: Value = serde_json::from_str(&err).unwrap();
        assert!(body["error"].as_str().unwrap().contains("MT-9"), "{err}");
    }

    #[test]
    fn a_write_tool_reports_a_gone_scheduler_rather_than_hanging() {
        // The same 503 the HTTP route gives: the command channel is closed, so the answer is
        // immediate. A blocking wait on a reply that can never come is the failure this guards.
        let (svc, commands) = ops(vec![row("iss-a", "MT-1")]);
        drop(commands);

        let err = svc.call(PATH, TOOL_REFRESH, &json!({})).unwrap_err();
        assert!(err.contains("no longer accepting commands"), "{err}");
        let err = svc.call(PATH, TOOL_UNQUARANTINE, &json!({ "key": "MT-1" })).unwrap_err();
        assert!(err.contains("no longer accepting commands"), "{err}");
        let err = svc.call(PATH, TOOL_UNBLOCK, &json!({ "key": "MT-1" })).unwrap_err();
        assert!(err.contains("no longer accepting commands"), "{err}");
    }

    #[test]
    fn the_unblock_tool_sends_the_resolved_issue_and_answers_with_the_schedulers_word() {
        // The tool resolves the key against the snapshot and hands the scheduler a dispatch
        // id, never the raw identifier; the store's answer, not the row, is what comes back.
        let (svc, mut commands) = ops(vec![row("iss-a", "MT-1")]);
        let scheduler = std::thread::spawn(move || match commands.blocking_recv() {
            Some(super::super::Command::Unblock { issue_id, reply }) => {
                let _ = reply.send(Ok(false));
                issue_id
            }
            other => panic!("expected an unblock, got {other:?}"),
        });

        let body = parsed(svc.call(PATH, TOOL_UNBLOCK, &json!({ "key": "MT-1" })));
        assert_eq!(scheduler.join().unwrap(), "iss-a");
        assert_eq!(body["cleared"], false);
        assert_eq!(body["identifier"], "MT-1");
    }

    #[test]
    fn the_only_path_that_serves_tools_is_the_ops_path() {
        // A worker's config points at `/mcp/<token>`. Aimed at this port by mistake, it must
        // find no tools and be refused on a call — not fall through to the ops routes.
        let (svc, _c) = ops(vec![row("iss-a", "MT-1")]);
        assert_eq!(svc.tools("/mcp/deadbeef"), json!([]));
        assert!(svc.call("/mcp/deadbeef", TOOL_SNAPSHOT, &json!({})).is_err());
        assert!(svc.call("/", TOOL_SNAPSHOT, &json!({})).is_err());

        assert_eq!(svc.tools(PATH).as_array().unwrap().len(), TOOLS.len());
        assert_eq!(svc.tools("/ops/").as_array().unwrap().len(), TOOLS.len());
        assert!(svc.call("/ops/", TOOL_SNAPSHOT, &json!({})).is_ok());
    }

    #[test]
    fn arguments_the_schema_does_not_name_are_refused_not_ignored() {
        let (svc, _c) = ops(vec![row("iss-a", "MT-1")]);
        let err = svc.call(PATH, TOOL_SNAPSHOT, &json!({ "key": "MT-1" })).unwrap_err();
        assert!(err.contains("unexpected argument"), "{err}");
        let err = svc.call(PATH, TOOL_ISSUE, &json!({})).unwrap_err();
        assert!(err.contains("requires"), "{err}");
        let err = svc.call(PATH, TOOL_ISSUE, &json!({ "key": "   " })).unwrap_err();
        assert!(err.contains("requires"), "{err}");
        let err = svc.call(PATH, "comment", &json!({ "body": "hi" })).unwrap_err();
        assert!(err.contains("unknown tool"), "{err}");

        // `_meta` is protocol plumbing and never a reason to refuse.
        assert!(svc.call(PATH, TOOL_SNAPSHOT, &json!({ "_meta": {} })).is_ok());
    }

    #[test]
    fn every_tool_is_a_route_that_exists_and_the_list_names_each_once() {
        // The scope rule from the issue: a tool per existing route, no more, and the
        // schemas close over their arguments.
        let tools = OpsMcp::tools_json();
        let names: Vec<&str> =
            tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOLS);
        for t in tools.as_array().unwrap() {
            assert_eq!(t.pointer("/inputSchema/additionalProperties"), Some(&json!(false)));
        }
    }

    #[test]
    fn a_non_loopback_bind_is_refused_unless_it_was_asked_for() {
        let cfg = ApiConfig { mcp_bind: "0.0.0.0:0".into(), ..Default::default() };
        let err = bind(&cfg).expect_err("0.0.0.0 must not bind by default").to_string();
        assert!(err.contains("allow_public") && err.contains("mcp_bind"), "{err}");

        let ok = ApiConfig { mcp_bind: "127.0.0.1:0".into(), ..Default::default() };
        assert!(bind(&ok).is_ok(), "loopback needs no ceremony");
    }
}
