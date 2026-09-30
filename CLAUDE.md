# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**MUST follow: [docs/coding-guidelines.md](docs/coding-guidelines.md).** Read it before the
first edit. It is the authoritative statement of how code is written here, with a check named
for every rule. This file covers what the system is and why; that file covers how to change it.

Development here is done by agents, so this file is the working surface: every command, every
switch, every trap. [README.md](README.md) is for a person deciding whether to adopt this and
what it is for — intention, installation, contribution. Commands belong here, not there.

**Before changing `src/sched/`, `src/store/`, `src/broker/`, `src/gate/` or delivery, read
[docs/invariants.md](docs/invariants.md).** Every row there names a guard test that must still
fail without its mechanism; several fail only in the exact scenario they were written for, so a
green `cargo test` is not proof a row still holds. The reasoning behind each subsystem is in
[docs/architecture.md](docs/architecture.md): read its section for the code you are changing.

## What this is

`crewd` is a tracker-driven orchestrator for Claude Code agents: a daemon that polls an
issue tracker, opens a workspace per issue, runs a coding-agent session in it, and reconciles
what comes back. It is a Rust reimplementation of the coordination layer described in
[openai/symphony](https://github.com/openai/symphony)'s `SPEC.md`, written after a review that
found several concrete defects in that design. A lot of this code exists specifically in order
*not* to have those defects.

The workspace is split into packages (#45). `crewd`, at the root, is the daemon: its library is
`crew` (`use crew::`), its binary `crewd`. `crewctl/` is the client, a binary that links only
`libcrew/`, which holds what both need: the published `Snapshot`/`Row` types, the ops API's
address and marker, and the HTTP client and renderer. Anything both binaries need moves *down*
into `libcrew`, never re-described in the client. `crewctl`'s graph holds no `rusqlite`,
`ratatui`, `tokio` or `ureq` (CI checks it with `cargo tree`), so the client cannot open the
daemon's store.

Where new work lands is decided by [ADR 1](docs/adr/0001-extension-boundaries.md): a trait
implementation, an external `crewctl-<name>` command, a hook, or a core change that protects an
invariant.

## Commands

```bash
cargo test                                 # every package; the count is in its output, never here
cargo test --lib                           # unit only
cargo test --test scheduler                # scheduler integration only
cargo test --test api                      # ops API integration only
cargo test a_permanent_failure             # one test; the arg is a substring match

cargo clippy --all-targets -- -D warnings  # the standing bar is zero warnings
cargo fmt --check

cargo run -- --tui                         # dashboard against the fake tracker
cargo run -- --max-ticks 20                # headless smoke run, then exit
cargo run -- --api 127.0.0.1:8787          # headless, with the ops API on for this run
cargo run -p crewctl -- status             # what a running daemon is doing, read over that API
cargo run -p crewctl -- status MT-649      # one issue in full: phase, attempt, turns, cost, branch
cargo run -- --mcp 127.0.0.1:8788          # the ops API's routes as MCP tools, for a supervising agent
claude mcp add --scope local --transport http crew_ops http://127.0.0.1:8788/ops
                                           # ...and how that agent gets them; workers inherit it too
cargo run -- init                          # register your own GitHub App: two clicks, writes ~/.crewd/
cargo run --example dashboard_preview      # render the UI to stdout, no terminal needed
cargo run --example broker_live            # real `claude` against a real broker; spends tokens
```

`cargo test`, `cargo clippy` and `cargo fmt --check` are the commit gate, and
[.github/workflows/ci.yml](.github/workflows/ci.yml) runs each as a required check on every
pull request — delivery opens a pull request for every agent branch, so that is where a red
gate can still stop a merge. `default-members` covers every package, and `default-run` keeps a
bare `cargo run` meaning `crewd`. The workflow must never *run* an example: `broker_live`
spawns a real `claude` and spends tokens, and only a Cargo default (examples are
`test = false`) keeps `cargo test` from calling it. `rust-toolchain.toml` pins the compiler and
CI builds `--locked`, so a drifted `Cargo.lock` fails rather than being quietly rewritten.

On a `crew/` branch a `Stop` hook ([.claude/hooks/lint-before-stop.sh](.claude/hooks/lint-before-stop.sh))
runs `cargo fmt --check` and clippy when a Claude Code session tries to finish, and blocks the
stop with their output until they pass and the fix is committed (#187), so a lint failure costs
a turn rather than a gate round. A stop reporting `continue` or `blocked` is let through, and a
blocked agent repeats its `CREW_OUTCOME`/`CREW_REVIEW` lines, because crewd reads them from the
final message only. Its timeout stays under `agent.stall_timeout_ms`. Grok and other branches
never run it; the gate still decides.

**Before reporting done,** check `git diff <base>...HEAD` against the Failure paths section of
[docs/coding-guidelines.md](docs/coding-guidelines.md) and against
[docs/invariants.md](docs/invariants.md). `<base>` is the branch the pull request merges into
(`master` on this repo). Fix each finding, or answer it with the line and why the rule does not
apply.

- A Claude Code session runs the `crew-reviewer` subagent
  ([.claude/agents/crew-reviewer.md](.claude/agents/crew-reviewer.md)) and passes it that diff.
- A session that cannot run a subagent reads those two texts and checks the diff itself. Grok
  is started with `--no-subagents` ([src/worker/grok.rs](src/worker/grok.rs)).

The loop while editing, measured on this tree (an agent runs the command the docs name, so this
table *is* the loop):

| Editing | Command | Takes |
|---|---|---|
| anything | `cargo check` | ~0.2s, ~1.5s after an edit |
| shared types, the client | `cargo test -p libcrew` | ~0.2s |
| scheduler, store, broker | `cargo test -p crewd --lib -- --skip workspace::` | ~1s |
| `GitWorktreeWorkspace` | `cargo test -p crewd --lib workspace::` | ~3s |
| `crewctl` | `cargo test -p crewctl -p libcrew` | ~0.6s |
| before committing | `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check` | ~9s |

The `workspace::` tests shell out to real `git`; `--skip workspace::` keeps the loop fast.

**Side effects of a run.** Headless runs create real worktrees under `workspace.root` (default
`.crew/workspaces`) and real files under `~/.claude/tasks/`; `CREW_DB=/tmp/x.db` and
`CREW_TASKS_ROOT=/tmp/tasks` point them somewhere disposable. `RUST_LOG=crew=debug` raises the
log level; logs go to stderr, because under `--tui` the alternate screen owns stdout.

**Never run the daemon from inside a worktree**, which is a dispatched agent's cwd.
`GitWorktreeWorkspace::new` refuses it (#29); point `workspace.repo` at a throwaway clone and
`workspace.root` beside it (`CREW_DB` alone does not help).

**What a run did** is in its transcript, `<workspace.root>/.transcripts/<run>.jsonl`, named in
the `dispatched` log line: `jq -c 'select(.type=="assistant")'` for the turns, `grep
crew_run_end` for how it exited.

```bash
cargo run -- --config crew.github.toml --max-ticks 3
```

points the tracker at this repo's own Issues as the `crew-bot` GitHub App (#64, #98): every
write is authored by `crew-bot[bot]`, from `~/.crewd/github-app.toml` (`crewd init` writes it),
and a missing or incomplete file stops the daemon at startup by name. With `tracker.github_app`
commented out, `GITHUB_TOKEN=$(gh auth token)` is the fallback for a smoke run. The config
holds no token and sets `tracker.dispatch_label = "agent"`: the label alone makes an issue
dispatchable (`DispatchRule` in `src/tracker/github.rs`); token minting is in `docs/architecture.md`.

**`worker.kind = "claude"` spawns real agents** (`claude -p --permission-mode
bypassPermissions`, against real worktrees), so `--max-ticks` on such a config is not a dry run.
It is a separate switch from the tracker on purpose: a real tracker with the fake worker watches
real dispatch decisions without spawning anything.

```bash
cargo run -- --config ~/.crewd/<deployment>.toml --max-ticks 3
```

runs the Jira Cloud tracker (#99) from a filled-in copy of `crew.jira.toml`. That copy names a
real Jira site and repository, so it lives under `~/.crewd/` and never in this public repo. The
Jira credential is the file `tracker.jira.credentials` names (`email`, `api_token`), else
`JIRA_EMAIL`/`JIRA_API_TOKEN`; `Config::load` refuses a half-written file by name. `[forge]`
names the GitHub repository delivery pushes to, and each key falls back to `[tracker]`, which
is why `crew.github.toml` has no `[forge]` table. **A second deployment on this host needs its
own `CREW_DB`, `workspace.root` and `api.bind`/`api.mcp_bind`**: the default `crew.db` in the cwd
is the dogfooding daemon's, and both shipped configs bind `127.0.0.1:8787`/`:8788`.

## Architecture

**The scheduler is the only authority.** Everything external sits behind a trait, and each
trait has a fake: `Clock`, `Tracker`, `Worker`, `Workspace`, `Store`, `Projector`, `Gate`,
`Forge`. That is what lets [tests/scheduler.rs](tests/scheduler.rs) drive real
`Scheduler::tick()` calls with no sleeps and nothing to flake — time only moves when a test
moves it.

**Tick order is load-bearing** ([src/sched/mod.rs](src/sched/mod.rs)):

```
recover()                                        ← first tick only
                 ↓
observe_rate_limit_warnings → harvest_finished → observe_progress → harvest_gates
                 → detect_stalls → refresh_running
                 ↓
         advance_deliveries                          ← unconditional too
                 ↓
            cfg.preflight()                          ← gate: on failure, return here
                 ↓
sweep_parked → dispatch_due_retries → dispatch_new → publish
```

Reconciliation runs before `preflight()` so a broken config stops *new* dispatch without
stranding runs in flight; do not move it earlier. `sweep_parked` sits behind it on purpose (it
deletes workspaces); the rest of the reasoning is in [docs/architecture.md](docs/architecture.md).

**Rules that bind everywhere:**

1. The clock is injected. Nothing outside [src/clock.rs](src/clock.rs) may call
   `Instant::now` or `SystemTime::now`. Monotonic (`Mono`) for every interval — stall, backoff
   — so an NTP step cannot fire them early; wall (`Wall`) only for display and for
   `retry.due_at`, which has to survive a restart.
2. The claim commits before the worker exists: `ensure → claim → prepare → spawn`, in that
   order, in `launch()`. Spawning first leaves a window where a fast-exiting worker reports
   against state that was never written.
3. No observer reads the store. The scheduler publishes an immutable `Snapshot` over a
   `tokio::sync::watch` channel, and the TUI, the HTTP API and `crewctl status` render that
   and nothing else. Headless is the default and `--tui` opts in, which is what keeps the
   dashboard from becoming load-bearing. The rule cuts both ways: an observer that needs
   something the snapshot does not carry does not get a `Store`, it gets a new field on
   `Snapshot` — which is why run history lives on `Row`, and why `Row.branch` was added when
   the status client needed it rather than having the client ask git.
4. The projection is one-way ([src/project.rs](src/project.rs)). The orchestrator writes to
   `~/.claude/tasks` and never reads it back for a scheduling decision — it is Claude Code's
   internal store with no published schema, so a change there must cost a dashboard, not the
   scheduler. `TasksProjector` probes one existing task file's shape at startup and disables
   itself with a warning if the keys it depends on are missing; that probe is the one read this
   type performs, and its result only ever flips this type's own on/off switch.
5. Fail fast. Anything the config turns on either starts or crewd exits non-zero at startup,
   naming what failed: a listener that cannot bind, a worker binary that cannot be resolved, a
   store another live daemon holds. Never log it and keep scheduling: a daemon that is up but
   partial looks healthy to its operator and is not (#218). A failure confined to one run, such
   as its crash or one flaky API call, stays with that run and is retried; one that disables a
   whole worker pauses that worker (#216). Rule 4's projection is the one startup exception,
   because a change in Claude Code's internal store must cost a dashboard, not the scheduler.

**The store is a cache of judgment, not a system of record.** Losing `crew.db` degrades
to stateless re-polling, never to incorrect behaviour — the session id lives there too, so
losing it costs cold continuations rather than a wrong conversation. The claim is the one entry
that could invert that, because *keeping* it across a hard kill is what went wrong: an issue
marked `running` with nothing running is refused by `claim()` forever, is invisible to
`detect_stalls`, and has no retry row to bring it back. `Scheduler::recover` is what holds the
contract — it releases those claims at startup and reconciles their worktrees, so the worst a
surviving database can do is still cost a re-poll.

**Independent brakes on the same runaway** — the `Outcome` verdict, the per-issue turn budget,
`parked_state` and the gate's `max_failures` — cover different paths; removing one looks safe
because the others hold, so keep all of them. Likewise the broker's per-run *and* per-issue
budgets: a continuation opens a fresh run, so the per-run cap alone bounds nothing.

## Dogfooding

The backlog is GitHub Issues on this repository; an open issue labelled `agent` is work a real
agent will pick up, and the `issue-triage` skill decides which carry it.

**Filing an issue** is the `issue-authoring` skill. The issue body is the durable work
specification; tracker comments do not reach the prompt, so a decision, a scope cut and an
acceptance criterion go there. HTML comments in the body are stripped before dispatch. The
`issue-triage` skill places the issue.

`crew.github.toml` turns on every real seam: the GitHub tracker, the `claude` worker, the tool
broker, transcripts, the handoff gate (naming the commit-gate commands) and delivery. `crew.toml`,
the default config, stays on the fakes so the quickstart is unchanged. The broker is on by
default where the worker is not, because the switches mean opposite things: `worker.kind`
decides whether an agent runs at all, while the broker only decides whether a running agent has
a *scoped, logged* way to do what it could otherwise do ambiently. When you change scheduler
behaviour, ask whether the change would still be correct when the agent running it is working
on this repo.

`.claude/settings.json` enables Claude Code's `rust-analyzer-lsp` plugin (#192). The `LSP`
tool answers definition, references and hover, so whether a guard is still reached from both
call sites is a find-references question. On the #192 run a warm server published E0308 over
LSP about a second after the save, and the dispatched turn after the edit still contained no
diagnostic; that gap is #228, so an edit's compile result stays `cargo check`. The server is
the pinned toolchain's `rust-analyzer` component, so rustup installs it;
`.claude/skills/setup-rust-analyzer` covers a machine without rustup and a `SessionStart` hook
names any missing piece. Each worktree indexes its own copy: budget roughly 1-2 GB and one
`cargo check` per concurrent run. **Trap:** `references`, `definition` and `hover` answer from
whatever is indexed *so far*, so during the first load they come back empty — indistinguishable
from *no callers*. Ask again until an answer is non-empty before concluding anything from one.
The plugin passes the server no options, and a root `rust-analyzer.toml` cannot stand in:
rust-analyzer reads its flycheck and cargo settings with no source root, which skips that file.
The per-edit check is `cargo check` (not clippy), built in `target/`, where it can hold Cargo's
lock against the agent's own build for a moment.

## Skills

Reach for each at the moment named:

- **`engineering:architecture`**: a decision has to be recorded before code (an issue's "Open"
  section, a new seam or trait, reversing a line in this file). The ADR goes in `docs/adr/`
  (#63), and the issue and the code cite it in one line.
- **`mattpocock-skills:diagnosing-bugs`**: a run, a scheduling decision or a test does something
  nobody can explain. Start from the transcript, `crewctl status <issue>` and the daemon log,
  and finish with a guard test that fails without the fix.
- **`/code-review`**: every pull request before it merges, as well as Copilot's review. It
  catches a second implementation of something that already exists, as on #88.
- **`/retro`**: a delivery closed, a run was quarantined or handed off, or it is the weekly
  pass over the last transcripts. It reads the transcript and proposes environment changes as
  Inbox issues; the operator files them (#182).

Watching a running daemon is `/supervise-crewd`, typed by the operator's session. It is never
loaded into a dispatched agent.

## Constraints for the worker and broker

Decisions already taken that are expensive to rediscover. The first two are implemented in
[src/worker/claude.rs](src/worker/claude.rs), the third in [src/broker/](src/broker/):

- The worker's child environment is built from an explicit **allowlist**
  (`DEFAULT_ENV_ALLOWLIST`) — not inherit-and-scrub. A denylist is fragile, and one missed
  variable leaks a tracker credential into a coding agent. Notably absent by default: any
  tracker credential and any API key — an operator on API-key auth adds `ANTHROPIC_API_KEY`
  deliberately, it is not there by default.
- The worker execs the `claude` binary directly (`Command::new`, never a shell). **No `bash
  -lc`.** A login shell re-imports from the operator's dotfiles exactly the secrets that were
  just scrubbed.
- The MCP tool broker ([src/broker/](src/broker/)) executes tracker writes host-side while
  holding the credential; the worker receives results, never a raw token. **The broker is not
  an isolation boundary.** On macOS `gh` and `claude` authenticate from the login keychain even
  with `HOME` scrubbed, so a dispatched agent *can* still comment, push and close as the
  operator; only a real sandbox could stop that. The broker adds a path scoped to one issue,
  budgeted and logged. Never restate this as "the worker cannot reach a credential" (#4).

## Troubleshooting

If linking fails, or `git` refuses every invocation with *"You have not agreed to the Xcode
license agreements"*, the problem is which developer directory is active — not your code, and
not a missing toolchain:

```bash
xcode-select -p    # pointing at /Applications/Xcode.app means an unaccepted license
sudo xcode-select --switch /Library/Developer/CommandLineTools
```

`xcode-select --install` does **not** fix this. It installs the Command Line Tools; it does
not make them active, so the reported symptom is unchanged and the real cause stays hidden.

If every tracker or forge call fails with `invalid peer certificate: UnknownIssuer`, a TLS
proxy (Netskope, typically) is intercepting the connection with its own root. Set
`SSL_CERT_FILE` to the proxy's full CA bundle, the same variable curl, Python and Node already
read for the same problem: crewd trusts exactly the certificates that file names, replacing the
default roots rather than adding to them (#150).
