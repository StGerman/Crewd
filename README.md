# crewd

**Point it at your backlog. It works the tickets and hands you pull requests.**

crewd is a daemon you run on your own machine. Label an issue in GitHub Issues or Jira, and
crewd opens a git worktree for it, runs Claude Code there (or Grok, as a second worker on its
own account), and opens a pull request only after the branch has been brought onto today's base
and has passed your own checks (the gate, on by default). You review pull requests. Nothing it
does merges.

It ships as two binaries: `crewd`, the daemon, and `crewctl`, which asks a running daemon what
it is doing without being able to touch its state.

## Who it is for

Teams whose tickets live in GitHub Issues or Jira Cloud and whose code lives on GitHub, who
already use Claude Code, and who want it working the backlog while nobody is watching: on their
own machine, under their own logins, behind their own test suite. A Grok account adds a second
worker with its own slots, so work overflows to it when Claude's are full.

Something else fits better, as of October 2026, if:

- **None of it should run on your machine.** Hosted agents (Codex cloud, Devin, Cursor's
  background agents, Rovo Dev in Jira) run the agent in the vendor's sandbox.
- **Your tracker is Linear.** Linear hands issues to Codex, Cursor, Devin or Copilot natively,
  and Cyrus runs Claude Code from it. crewd has no Linear tracker.
- **Your code is on Bitbucket or GitLab.** crewd opens pull requests on GitHub only.
- **You want a dozen agent CLIs side by side.** crewd runs Claude Code and Grok, each held to
  the same gate and budgets.

## Why it doesn't trust "done"

Running one coding agent is easy. Running several, unattended, against a real backlog is where
it falls apart, and it falls apart in specific, repeatable ways. Each one has a brake here:

- **An agent that says "done" is not done.** Two branches cut from the same base can each pass
  the test suite alone and fail together. crewd's gate brings a finished branch onto the
  current base, fetched fresh (a rebase, or a merge when the branch already merged an older
  base), and runs your checks in it *before* it believes the verdict. A failure goes back to the
  agent with the output in hand; a conflict it cannot hand back, or `gate.max_failures` failures
  in a row, parks the issue for a human. The gate is on by default; with `gate.enabled = false`
  a "done" is believed as reported.
- **A loop with no brake spends your whole budget.** Each budget has a narrow bound and an
  issue-wide one that never resets: turns per session and per issue, broker calls per run and
  per issue, review round-trips per pull request and per issue. An agent cannot buy itself a
  fresh budget by starting a new session or opening a new pull request. A run that keeps failing
  the same way is quarantined rather than retried forever. The bounds count turns and calls, not
  dollars ([#26]).
- **A crash should cost a poll, not a wedged queue.** The database is a cache of judgment, not
  a system of record. Losing it degrades to re-polling the tracker. A hard kill releases its
  claims at the next startup rather than leaving an issue marked running forever. A second
  `crewd` pointed at the same database refuses to start while the first is alive.
- **You should be able to see what happened.** Every run writes its full event stream to a
  transcript on disk (on by default), and a running daemon answers questions over an HTTP API, and as MCP tools
  for a supervising agent, without being disturbed.

The orchestrator is the only authority, and every external thing (tracker, agent, git, clock)
sits behind a seam with a fake behind it. That is why the test suite has no sleeps in it, and
why "would this still be correct if the agent were working on this repo?" is a question the
code can actually answer. The question is not hypothetical: crewd's own backlog is worked by
crewd.

## What it will not do

- **Merge.** The trait that talks to the forge has no merge method, and is not going to grow
  one. A red CI or a review comment goes back to the agent; a green, reviewed pull request
  waits for you.
- **Decide what to work on.** It dispatches what is labelled. Whoever triages the tracker
  decides what that is.
- **Contain the agent, yet** ([#135]). A real worker runs on your machine, as you: `claude -p
  --permission-mode bypassPermissions`, or `grok --always-approve`, in the issue's worktree.
  Its environment is built from an allowlist, and Claude's tracker writes go through a scoped,
  logged broker (Grok gets no tracker tools), but on macOS `gh` and `claude` read the login
  keychain, so a dispatched agent can still push, comment and close as you. Run it where that
  is acceptable.

## Installation

You need `git` and the [Claude Code CLI](https://claude.com/claude-code) logged in. Releases
are built for Apple Silicon macOS and x86_64 Linux. With Homebrew:

```bash
brew install stgerman/tap/crewd stgerman/tap/crewctl
```

Without it, each binary has a shell installer on the
[latest release](https://github.com/StGerman/crewd/releases/latest), or `cargo install crewd
crewctl` builds both from crates.io. Then:

```bash
crewd --tui
```

From source, with Rust installed:

```bash
git clone https://github.com/StGerman/crewd.git
cd crewd
cargo build
cargo run -- --tui
```

`crewd --tui` (or `cargo run -- --tui`) runs against fake demo data: no tracker is contacted and no agent is
spawned, so it is safe to explore. `q` quits.

Run headless with the ops API on, and ask it what it is doing from another terminal:

```bash
cargo run -- --api 127.0.0.1:8787
cargo run -p crewctl -- status
```

`crewctl` reads the same `/api/v1` a program of your own can drive; what v1 keeps stable is
[docs/api-v1.md](docs/api-v1.md).

### Quickstart

Install the binary once (`cargo install --locked --path .` in the clone above), then, inside a
clone of your own repository whose `origin` is on GitHub:

```bash
crewd init
crewd service install --config ~/.crewd/<owner>-<repo>/crewd.toml
```

and label an issue `agent`. The second command runs the deployment as a login service
(below); `crewd --config ~/.crewd/<owner>-<repo>/crewd.toml` runs it in the foreground instead.

`crewd init` registers a GitHub App for crewd (below; once per machine, two clicks), checks it
is installed on the repository, and writes `~/.crewd/<owner>-<repo>/crewd.toml`. It asks two
questions, both defaulting to yes: whether agents work issues (`--work`/`--no-work`) and whether
it opens pull requests (`--deliver`/`--no-deliver`). It suggests the gate's check command from
the clone (`--checks` answers that). Then it creates the `agent` label and every `state:` label
the config names, as the App. It never overwrites a config or a label, so running it again is
how you see what it kept and which switches are still off, with the one line that turns each on.

### Pointing it at your own repository by hand

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
"work my backlog" should be a decision you make twice; `crewd init` asks each one separately.

### Running it as a service

Once a config does what you want, let launchd (macOS) or systemd (Linux) run it rather than a
terminal:

```bash
cargo install --path .
crewd service install --config ~/.crewd/acme-api/crewd.toml
```

The service starts at login, restarts after a crash, and stays down after a clean shutdown. It
runs in the config's directory, which names it (`dev.crewd.acme-api`), so a second deployment
gets its own service and its own `crew.db`; give it its own `api.bind` and `api.mcp_bind` too,
because both shipped configs use the same ports. Your shell's `PATH` and `SSL_CERT_FILE` are
copied into it, because neither service manager reads your shell; reinstall after changing them.
Nothing else is: the install refuses a config whose credential would come from `GITHUB_TOKEN` or
`JIRA_*`, so name a credentials file instead, and it needs `crewd` on that `PATH`, which `cargo
install` provides. The install prints where the logs go: `crewd.log` beside the config on macOS,
the user journal on Linux, where it also tells you if `loginctl enable-linger` is needed to keep
the service up after you log out. On Linux every path and value is quoted and escaped in the
unit, so a deployment directory with a space works; only one ending in a space or a backslash,
or holding a line break, is refused by name, because systemd's `WorkingDirectory=` cannot hold it.
Install and uninstall report success only when the service manager agrees: a running service
that will not stop fails the install, and an uninstall interrupted after removing the unit
finishes when run again. `crewd service uninstall --config <same path>` removes it.

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

`crewd init` registers that App for you in two clicks and writes the file below. To do
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

A release is a `v` tag cut by hand when a milestone closes. It publishes the binaries, the
Homebrew formulae and the crates, and versions stay `0.x` until the v1 promise and the store
schema are declared stable.

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
[docs/symphony.md](docs/symphony.md) quotes the spec's passage behind each of them, the failure
it causes, and the test that proves crewd does not have it.

Where it is headed: crewd is built to hold the guarantees a service manager holds, applied to
coding-agent sessions, the way systemd runs services and containerd runs containers.
[docs/vision.md](docs/vision.md) maps each guarantee to crewd and marks what is not built yet.

[#26]: https://github.com/StGerman/crewd/issues/26
[#135]: https://github.com/StGerman/crewd/issues/135
