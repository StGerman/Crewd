# Vision: a service manager for agent sessions

crewd is built to hold, for coding-agent sessions, the guarantees a service manager holds for
processes: what systemd is to services and containerd is to containers. It starts a session for
each issue the tracker marks ready, supervises it, bounds it, retries or stops it, and keeps its
log. Something above it (the tracker, an operator, a supervising agent) decides what work to run.

The [README](../README.md) is what crewd does for you today. This page is where it is headed,
and the "Not yet" rows below are the roadmap that stance implies.

## The model

| A service manager has | crewd has |
|---|---|
| One PID 1, one authority | One scheduler. A second `crewd` on the same store refuses to start |
| Restart policy, `StartLimitBurst` | Capped backoff, quarantine on a permanent failure, `gate.max_failures` |
| `WatchdogSec` | Stall detection, `agent.stall_timeout_ms` |
| A stop that waits for the process | A workspace is removed only after its worker is confirmed stopped |
| State that survives its own crash | Claims released and worktrees reconciled at the next startup |
| Resource limits | Per-session and per-issue budgets on turns, tool calls and review rounds that never reset. **Not yet:** dollars ([#26]), CPU and memory ([#143]) |
| journald | A transcript per run |
| `systemctl status` | `crewctl status`, the TUI, MCP tools for a supervising agent |
| A pluggable runtime (runc) | The `Worker` trait: Claude Code, Grok |
| A versioned API a higher layer drives (CRI) | The ops API at `/api/v1`. **Not yet:** a promise of what v1 keeps stable ([#90]) |
| Sandboxing | **Not yet** ([#135]). Agents run as you |
| Packaged, started by the init system | **Not yet** ([#90]). Built from source, run by hand |
| Drain and restart | **Not yet** ([#107]) |

One row has no counterpart. A service manager believes a process's exit status; crewd does not
believe an agent's "done".

## Where it sits

As of October 2026:

- **Hosted agents** (Codex cloud, Devin, Cursor's background agents, Rovo Dev in Jira) are
  vertically integrated: agent, runtime and interface in one product, sandboxed and run by the
  vendor.
- **Agent apps** (Agent Orchestrator, Cyrus, the Symphony ports) each bring their own runner and
  their own interface.
- **crewd is only the runner.** `crewctl`, the TUI and the ops API are its whole interface.

## Scope

In scope and built:

- **The session lifecycle:** claim, worktree, spawn, supervise, detect a stall, retry, stop,
  release.
- **Bounds** per session and per issue, which an agent cannot reset.
- **The handoff:** the gate, the pull request, and red CI or review comments sent back to the
  agent.
- **The operator surface:** transcripts, `crewctl`, the TUI, the ops API and its MCP tools.

In scope and not built yet:

- **Confining the agent** ([#135]).
- **Packaging, and running under launchd or systemd** ([#90]).
- **Draining for a restart** ([#107]), and **accounting** in dollars, CPU and memory ([#26],
  [#143]).

Out of scope:

- **Deciding what to work on.** The tracker, and whoever triages it, decides; crewd dispatches
  what is labelled.
- **Planning or splitting work.** The issue is the unit of work. crewd does not decompose a
  goal into tasks.
- **Merging.** The trait that talks to the forge has no merge method, and is not going to grow
  one.
- **Being an editor.** The TUI and `crewctl` show state; they do not change the work.

New work lands at one of the four boundaries [ADR 1](adr/0001-extension-boundaries.md) names: a
trait implementation, a `crewctl-<name>` command, a hook, or a core change that protects an
invariant.

[#26]: https://github.com/StGerman/crewd/issues/26
[#90]: https://github.com/StGerman/crewd/issues/90
[#107]: https://github.com/StGerman/crewd/issues/107
[#135]: https://github.com/StGerman/crewd/issues/135
[#143]: https://github.com/StGerman/crewd/issues/143
