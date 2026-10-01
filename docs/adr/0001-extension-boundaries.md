# 1. Where new work lands: traits, external commands, hooks, or the core

- **Status:** Accepted
- **Date:** 2026-09-26
- **Issues:** #84 (crew-core), #92 and #63 (the size of what is loaded), #94 (the first case the
  rule decides)

## Context

crewd is about 22,000 lines, and the backlog grows it faster than it trims it. Nearly all of that
is the scheduler, the store and the adapters behind its traits (`workspace.rs`, `sched/`,
`store/`, `worker/claude.rs`, `tracker/github.rs`, `forge/github.rs`). No optional feature is
bolted on the side. Each new idea (memory, notifications, releases, other trackers) arrives
as a proposal to change that core, because nothing says where else it could go.

Three runtime plugin mechanisms were considered and rejected:

- **Dynamic libraries** (`libloading`, `abi_stable`). Rust has no stable ABI, so a plugin must be
  built with the daemon's exact compiler and dependencies. That means loading code through
  `unsafe` for no gain over a crate.
- **WASM** (wasmtime, extism). This adds around a hundred crates and a host API to design and
  version. It pays off only for untrusted third-party plugins, and crewd has none.
- **Splitting the daemon into cooperating processes.** "The scheduler is the only authority"
  is what makes the invariant table (`docs/invariants.md`, CLAUDE.md's until #92) possible. Spreading that authority across
  processes would turn every row into a distributed-systems problem.

## Decision

crewd stays **one daemon process with one authority**. It is extended at four boundaries, and
every new piece of work names the one it belongs to:

1. **A trait implementation.** `Tracker`, `TrackerWrites`, `Worker`, `Workspace`, `Forge`,
   `Gate`, `Store` and `Projector` are plugin interfaces, chosen at compile time. #84
   moves `Tracker`, `TrackerWrites`, `Worker`, `Workspace`, `Store`, `Projector` and their
   model types into `crew-core`, so that a new backend (a Linear tracker, another agent
   CLI) is a crate depending on `crew-core` rather than a patch to `crewd`. The scheduler
   stays in `crewd`.
2. **An external command.** `crewctl <name>` runs `crewctl-<name>` from `PATH` when `<name>`
   is not built in (git and cargo do the same). The ops API address is passed in the
   environment. Such a command talks to the published API only, so it gains no authority the
   API lacks. It can be written out of tree and in any language. Built-in subcommands are one
   module each (`crewctl/src/cmd/<name>.rs`, exposing `Args` and `run`), and `main.rs` holds only
   the clap enum and the dispatch.
3. **A hook.** A program the daemon runs on a lifecycle event (`[hooks] on_done`, `on_blocked`,
   `on_quarantine`, …):
   - run as argv with no shell, like the gate's commands;
   - handed the event as JSON on stdin;
   - bounded by a timeout.

   A hook's result is logged and never read back into a scheduling decision. A hook
   observes; it holds no authority. This is where notifications and metrics go.
4. **A core change.** This is allowed only when the work closes or protects an invariant, or
   needs the scheduler's authority to be correct: a claim, a budget, a bound, or a write made on
   an agent's behalf. A core change adds or changes a row in the invariant table, or says why
   it needs none.

Tools the *agent* calls are one more boundary, and the same line runs through it:
- A tool that writes on the agent's behalf belongs in the broker (4), where it is scoped to the
  run's token, budgeted and audited.
- A read-only tool can be an ordinary MCP server the operator configures, and needs no crewd
  code.

Work that fits none of the four goes to the Backlog, not into the core.

## Consequences

- An issue names its boundary when it is written
  (`.claude/skills/issue-authoring/SKILL.md`, "Boundary"). Triage confirms that line before
  the issue is placed.
- #84 lands after #57, because the async decision changes the trait signatures. Splitting first
  would mean doing that work twice.
- External subcommands (#95) and `[hooks]` (#96) are each small and independent of #57. Each is its own
  issue.
- Some future features cost more under this rule than as a direct edit. A notifier is a separate
  program rather than twenty lines in `delivery.rs`. That cost is the point: it is what keeps
  the core the size of its invariants.
- Anything an external command or a hook needs that the snapshot or the event lacks is added to
  the snapshot or the event. It is never met by handing out a `Store`, which is rule 3
  ("No observer reads the store") extended to the new boundaries.
