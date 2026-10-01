# crewd

**A system daemon for coding-agent sessions: what systemd is to services and containerd is to
containers.**

crewd starts an agent session for each issue your tracker marks ready, supervises it, bounds it,
retries or stops it, and keeps its log. It does not decide what work matters; the tracker does.
And unlike a service manager, it does not trust the exit status: a branch becomes a pull request
only after it is rebased onto today's base and passes your checks. Nothing it does merges.

Work comes from GitHub Issues or Jira Cloud, pull requests go to GitHub, and the agent is Claude
Code, with Grok as a second worker on its own account. It ships as two binaries: `crewd`, the
daemon, and `crewctl`, which asks a running daemon what it is doing without being able to touch
its state.

## The model

crewd is built to hold the guarantees a service manager holds, applied to coding-agent
sessions. Where it does not hold one yet, the table says so.

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

[#26]: https://github.com/StGerman/crewd/issues/26
[#90]: https://github.com/StGerman/crewd/issues/90
[#107]: https://github.com/StGerman/crewd/issues/107
[#135]: https://github.com/StGerman/crewd/issues/135
[#143]: https://github.com/StGerman/crewd/issues/143

## Why it doesn't trust "done"

Running one coding agent is easy. Running several unattended against a real backlog fails in
specific, repeatable ways, and each one has a brake here:

- **An agent that says "done" is not done.** Two branches cut from the same base can each pass
  the test suite alone and fail together. crewd rebases a finished branch onto the current
  base, fetched fresh, and runs your checks in it *before* it believes the verdict. A failure
  goes back to the agent with the output in hand; a rebase conflict it cannot hand back, or
  `gate.max_failures` failures in a row, parks the issue for a human.
- **A loop with no brake spends your whole budget.** Turns, retries, review round-trips and the
  agent's tool calls are each bounded per session *and* per issue, and the per-issue bounds
  never reset. An agent cannot buy itself a fresh budget by starting a new session or opening a
  new pull request. The bounds count turns and calls, not dollars ([#26]).
- **A crash should cost a poll, not a wedged queue.** The database is a cache of judgment, not
  a system of record. Losing it degrades to re-polling the tracker. A hard kill releases its
  claims at the next startup rather than leaving an issue marked running forever.
- **You should be able to see what happened.** Every run writes its full event stream to a
  transcript on disk, and a running daemon answers questions over an HTTP API, and as MCP tools
  for a supervising agent, without being disturbed.

The orchestrator is the only authority, and every external thing (tracker, agent, git, clock)
sits behind a seam with a fake behind it. That is why the test suite has no sleeps in it, and
why "would this still be correct if the agent were working on this repo?" is a question the
code can actually answer. The question is not hypothetical: crewd's own backlog is worked by
crewd.

## Who it is for

Operators who want coding agents run as infrastructure: started from the tracker, supervised
and bounded like any other service, and observed without being disturbed, on their own machine,
under their own logins, behind their own test suite. A Grok account adds a second worker with
its own slots, so work overflows to it when Claude's are full.

Where it sits among the alternatives, as of October 2026:

- **Hosted agents** (Codex cloud, Devin, Cursor's background agents, Rovo Dev in Jira) are
  vertically integrated: agent, runtime and interface in one product, sandboxed and run by the
  vendor. Choose one of them if none of it should run on your machine.
- **Agent apps** (Agent Orchestrator, Cyrus, the Symphony ports) each bring their own runner and
  their own interface. crewd is only the runner, and `crewctl`, the TUI and the ops API are its
  whole interface.
- **Not covered yet:** a Linear tracker (Linear delegates to agents natively, and Cyrus runs
  Claude Code from it), pull requests on Bitbucket or GitLab, and a sandboxed agent ([#135]).

## Scope

In scope and built:

- **The session lifecycle:** claim, worktree, spawn, supervise, detect a stall, retry, stop,
  release.
- **Bounds** per session and per issue, which an agent cannot reset.
- **The handoff:** the gate, the pull request, and red CI or review comments sent back to the
  agent.
- **The operator surface:** transcripts, `crewctl`, the TUI, the ops API and its MCP tools.

In scope and not built yet:

- **Confining the agent** ([#135]). Today a real worker runs on your machine, as you: `claude -p
  --permission-mode bypassPermissions`, or `grok --always-approve`, in the issue's worktree.
  Its environment is built from an allowlist and tracker writes go through a scoped, logged
  broker, but on macOS `gh` and `claude` read the login keychain, so a dispatched agent can
  still push, comment and close as you. Run it where that is acceptable.
- **Packaging, and running under launchd or systemd** ([#90]).
- **Draining for a restart** ([#107]), and **accounting** in dollars, CPU and memory ([#26],
  [#143]).

Out of scope:

- **Deciding what to work on.** The tracker, and whoever triages it, decides; crewd dispatches
  what is labelled.
- **Planning or splitting work.** The issue is the unit of work. crewd does not decompose a
  goal into tasks.
- **Merging.** The trait that talks to the forge has no merge method, and is not going to grow
  one. A green, reviewed pull request waits for you.
- **Being an editor.** The TUI and `crewctl` show state; they do not change the work.

New work lands at one of the four boundaries [ADR 1](docs/adr/0001-extension-boundaries.md)
names: a trait implementation, a `crewctl-<name>` command, a hook, or a core change that
protects an invariant.

## Installation

A system daemon should arrive as a package and be started by launchd or systemd. crewd has
neither yet ([#90]): you build it from source and run it by hand. You need Rust, `git`, and the
[Claude Code CLI](https://claude.com/claude-code) logged in.

```bash
git clone https://github.com/StGerman/crewd.git
cd crewd
cargo build
cargo run -- --tui
```

That last command runs against fake demo data: no tracker is contacted and no agent is
spawned, so it is safe to explore. `q` quits.

Run headless with the ops API on, and ask it what it is doing from another terminal:

```bash
cargo run -- --api 127.0.0.1:8787
cargo run -p crewctl -- status
```

### Pointing it at your own repository

1. **Copy `crew.github.toml`** and set `tracker.owner` / `tracker.repo` to yours. Both
   shipped configs are heavily commented; the comments explain why each number is what it is,
   which is usually more useful than the number.
2. **Label the issues you want worked.** `tracker.dispatch_label` (`agent` in
   `crew.github.toml`) is what marks an issue as ready; an issue without it is never
   dispatched, whoever it is assigned to. Unset, any assignee marks it instead.
3. **Give it a credential.** A GitHub App (below), or `GITHUB_TOKEN` in the environment;
   never a token in the file.
4. **Decide whether it acts.** `tracker.kind = "github"` decides what it *looks at* (`"jira"`
   is the alternative, below); `worker.kind = "claude"` decides whether it *works*. Leaving the
   worker fake is a safe way to watch real dispatch decisions before you let anything edit code.
   To run Claude and Grok together, list both under `[[workers]]`: each entry is its own account
   and its own slots, Claude first, and overflow fills whichever slot is free.
5. **Decide whether it publishes.** `[delivery]` pushes branches and opens pull requests under
   your credentials. Off by default.

Steps 4 and 5 are separate switches on purpose. Turning this from "watch my backlog" into
"work my backlog" should be a decision you make twice.

### Using Jira

`tracker.kind = "jira"` polls a Jira Cloud project instead of GitHub Issues: Jira Cloud only,
not Data Center. Copy `crew.jira.toml` and fill in `[tracker.jira]`: `base_url`
(`https://your-domain.atlassian.net`), `project` (the key, e.g. `PROJ`), and `assigned_to_me`
to narrow dispatch further to issues assigned to the token's own account. `dispatch_label` is
required for a Jira tracker and is the whole dispatch signal, the same way it works for GitHub.
A Jira service account cannot be an assignee either, so there is no assignee fallback.

Credentials are a TOML file named by `[tracker.jira] credentials` (`~/.crewd/jira.toml` in the
shipped config), holding `email` and `api_token`: an Atlassian API token from
[id.atlassian.com](https://id.atlassian.com/manage-profile/security/api-tokens), not the account
password. Unset, it falls back to `JIRA_EMAIL` and `JIRA_API_TOKEN` in the environment. Every
write (a comment, a transition, the pull request's remote link) is authored by that token's
own account, the same way a GitHub PAT authors as its owner.

A Jira-tracked issue is not a GitHub issue, so delivery still needs a repository to push to and
open a pull request against: `[forge]` names it (`owner`, `repo`). Set `tracker.kind = "github"`
and the two repositories are the same one, so a GitHub-tracker config needs no `[forge]` table
at all.

### Giving it its own identity

By default every comment, label and branch is authored by *you*, because the token is yours. A
GitHub App gives the daemon its own identity, so its writes are distinguishable from yours and
it can hold `contents: write` while being denied merge entirely.

`cargo run -- init` registers that App for you in two clicks and writes the file below. To do
it by hand instead, register an App with `contents`, `issues` and `pull_requests` set to
**write** and `checks` set to **read** (delivery reads CI through the check-runs API, and
without it every delivery is handed off on a 403 right after its pull request opens). Add
`actions` **read** to have a red job's failing steps and log tail included in what the agent is
sent back. Install it on the repository, and write a small file naming it:

```toml
# ~/.crewd/github-app.toml
app_id = 123456
installation_id = 7890123
private_key_path = "~/.crewd/crew.private-key.pem"   # relative paths resolve beside this file
```

Then point `tracker.github_app` at that file. Every comment, label change, pull request and
branch push is then authored by the App; the daemon mints a one-hour installation token and
refreshes it itself, and neither the key nor a token ever reaches a dispatched agent's
environment, argv or `.git/config`. `crewd` refuses to start with a half-written file and names
the missing piece. `GITHUB_TOKEN` stays supported for anyone not running an App.

Behind a TLS-intercepting corporate proxy, set `SSL_CERT_FILE` to the proxy's CA bundle (the
same variable curl, Python and Node already read), and every tracker and forge call trusts
exactly that bundle instead of the default roots.

Two findings from setting one up by hand, since they shape how dispatch works: an App's
`[bot]` account **cannot be an issue assignee** outside GitHub's partner agent program, and a
separate marker account does not help because a collaborator on a personal repository cannot be
given read-only access. That is why the issues crewd picks up are marked by a **label** rather
than by assignment.

### Requirements, in full

| | |
|---|---|
| Rust | pinned in `rust-toolchain.toml`; only rustup reads it |
| `git` | real worktrees are created on disk |
| `claude` | logged in; needed only once `worker.kind = "claude"` |
| `grok` | logged in; needed only for a `kind = "grok"` worker |
| Memory | budget 1–2 GB per concurrent agent |
| Tokens | the real limit. Five concurrent agents can exhaust a five-hour window in minutes |

## Contributing

**Work is picked up from GitHub Issues on this repository, labelled `agent`**, not from a plan
file. That is the same mechanism described above, pointed at itself: crewd's own backlog is
worked by crewd.

Because of that, most development here is done by an agent rather than by a person at a
keyboard, and the documentation is split to match:

- **[CLAUDE.md](CLAUDE.md)** is the working surface: every command, the core rules, and the
  traps that are expensive to rediscover. It points to
  **[docs/invariants.md](docs/invariants.md)**, read before changing `src/sched/`, `src/store/`,
  `src/broker/`, `src/gate/` or delivery, and **[docs/architecture.md](docs/architecture.md)**,
  the reasoning behind each subsystem.
- **[docs/coding-guidelines.md](docs/coding-guidelines.md)** is how code is written here, with
  a named check for every rule. Read it before your first edit.

The commit gate is `cargo test`, `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check`. CI runs all three on every push and pull request.

Open an issue with the `issue-authoring` skill. Triage adds `agent`
when the daemon should pick it up. The issue needs enough context to act on. The invariant
table in `docs/invariants.md` is the standard the codebase holds itself to, and an issue that
names which invariant is at stake is one an agent can finish.

## Status and origin

Working today: a deterministic core with a fake behind every seam, real git worktrees, GitHub
Issues and Jira Cloud trackers, Claude Code and Grok workers, a host-side tool broker that lets
an agent write to its own ticket without holding the tracker credential, an ops API with a
status client and an MCP server over it, the handoff gate, and delivery through to pull
requests and review round-trips.

crewd began as a reimplementation of the coordination layer in
[openai/symphony](https://github.com/openai/symphony)'s `SPEC.md`, written after a review found
concrete defects in that design: a continuation loop that could respawn every second, a backoff
that could overflow, permanent failures retried forever, a workspace deleted under a live
worker. Each is a row in [docs/invariants.md](docs/invariants.md) with the test that fails
without its fix; the later rows came from crewd working its own backlog.
