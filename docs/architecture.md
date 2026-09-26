# Architecture

Moved verbatim from CLAUDE.md by #92. CLAUDE.md keeps the parts every session needs: the
scheduler as the only authority, the tick order, the four rules and the store contract. This
file holds the reasoning behind each subsystem. Read the section for the subsystem you are
changing before you change it. #63 will move each design note into its module's `//!` doc
and each incident story into an ADR under `docs/adr/`, leaving one-line pointers here.

## Decisions recorded as ADRs

A decision that shapes the architecture is recorded in [`docs/adr/`](adr/) before the code
(the `engineering:architecture` skill), and the issue and the code cite it in one line.
This file keeps the reasoning; an ADR records the decision and what it ruled out.

| ADR | Decision |
|---|---|
| [1. Where new work lands](adr/0001-extension-boundaries.md) | crewd stays one daemon with one authority; new work is a trait implementation, an external `crewctl-<name>` command, a hook, or a core change that protects an invariant |

The async-versus-sync decision (#57) will be ADR 2. The incident stories under **Subsystems**
move into ADRs under #63.

## Tick order: why each step sits where it does

Reconciliation runs before the gate so that a broken config stops *new* dispatch without also
stranding the runs already in flight. Do not move the `preflight()` call earlier. `sweep_parked`
is the one reconciliation step deliberately *behind* it: it deletes workspaces on the strength
of `is_terminal`, and an active/terminal overlap — one of the things the gate rejects — is
exactly what would make it delete the workspace of an issue about to be dispatched. It also
runs on its own cadence (`agent.parked_sweep_interval_ms`, default 5 min) rather than every
tick, because parked issues are not urgent and each sweep is one `by_ids` read per issue still
parked.

`advance_deliveries` is reconciliation too — it reads the outside world (CI, review) about runs
already over and may queue a retry the gate below then decides whether to dispatch — and only
touches issues nothing else owns. It comes *after* `harvest_gates` in the tick and, more to the
point, after it in the life of a `Done`: with a gate attached, a run's `Done` enters `gating`
and only the gate's pass reaches the `Done` arm of `apply_outcome` that queues delivery, so the
branch delivery pushes is the rebased, re-gated one. That order is the stack #43/#42 were built
as — gate, then delivery — and
`a_done_branch_is_gated_before_delivery_pushes_it_and_a_failing_gate_publishes_nothing` is
what fails if it is ever inverted.

`harvest_gates` is where a `Done` becomes a verdict. With a handoff gate attached (see below), a
run whose agent reports `Done` leaves `running` for a `gating` map instead of being released: its
claim stays held, its run row stays open, and the gate — rebase onto the base, then the configured
commands, in the run's own worktree — runs on a thread the scheduler polls. This step turns the
gate's answer into `Done`, `Blocked` or `Continue` and hands it to the same `apply_outcome` an
agent's verdict goes through, which is what makes a gate-sent continuation subject to the same
turn budget and the same escalating delay. It sits with the rest of reconciliation, ahead of
`preflight`, because a claim held mid-rebase must not stay held behind a config typo.

`recover()` is startup reconciliation, and it runs ahead of the gate for the same reason: a
claim stranded by the last process must not stay stranded behind a config typo. It lives inside
`tick()` rather than in `main.rs` on purpose — recovery a second entry point can forget to call
is recovery that silently does not happen, which is the exact failure it exists to fix. It is
callable directly (`Scheduler::recover`) and idempotent, so a caller that wants it eagerly can
have it.

## Subsystems

**Worth knowing:** reconciliation lives in `sched/mod.rs` rather than its own module — it
mutates the same `running` map as dispatch, so splitting it meant threading the whole
scheduler through a free function. The `Tracker` trait is deliberately a two-method read
kernel (`by_states`, `by_ids`); ticket *mutations* belong to the agent through host-executed
tools, not to this trait. `Workspace` has two implementations behind the trait:
`DirWorkspace` (plain directories, what the scheduler tests use — real git is slower and adds
nothing to a test that fakes the worker too) and `GitWorktreeWorkspace` (real, what `main.rs`
wires by default). Reuse in the latter checks for a `.git` *file* at the target path, not
bare existence — a plain directory there, e.g. left by a prior `DirWorkspace` run against the
same root, must surface through git's own "already exists" error rather than being silently
trusted as an already-prepared worktree. The branch, not the directory, is what a run leaves
behind: `remove` deletes the worktree but only deletes the branch when git's own merged check
says it carries nothing `repo`'s HEAD does not already have, and `prepare` attaches to an
existing branch that does carry commits rather than `-B`-resetting it. What the agent had *not* committed when
its run was stopped is snapshotted by `remove` to a new ref under `refs/crew/wip/<issue key>/`
— keyed on the issue id, not the renameable identifier, one ref per snapshot so a second stop
cannot orphan the first, and outside `refs/heads/` so the gate, delivery and the merged check
see only the agent's own commits — and the next run is told every such ref and its diffstat
rather than handed them applied (#22). Every removal path goes through
`remove` after `kill` has confirmed the stop; `shutdown()` removes nothing, and the worktree it
leaves is snapshotted whenever a later cleanup finally removes it. Cleanup is triggered by
a ticket reaching a terminal state, and closing a ticket is not a decision to throw away the
work done under it. That same merged check is what makes nesting expensive: an orchestrator
started inside another run's worktree (the agent for #24 did, to exercise the API) creates
worktrees whose branches sit on the *parent's* commit and so are never merged into `master`,
and `branch -d` keeps every one of them. `new` therefore refuses when `repo` or `root`
resolves inside a linked worktree of the repository — refused rather than redirected to the
top-level root, because redirecting would still leave registrations and branches the owning
orchestrator does not know about — and `remove` reconciles what earlier binaries left: it
collects the worktrees registered beneath the path before `worktree remove --force` deletes
their directories, prunes the stale registrations (first — `-d` refuses a branch a registered
worktree still pins), then gives each nested branch the same `-d` the parent's own gets. A
nested branch carrying commits is kept and named in a `warn` log line, with what to run once
its parent is merged; the merged check is not weakened for litter. `Prepared.branch` reports that name upwards so it reaches the dispatch log, and
`Workspace::branch_for` — pure naming, like `path_for` — answers the same question for the
snapshot. Naming rather than probing is what lets a *finished* run still report its branch,
which is when a reviewer wants it; asking git per row per tick would put a subprocess on the
snapshot path. `Row.branch` is `None` until an issue has been dispatched at least once, because
before that the name is a prediction and pointing an operator at a ref nobody wrote is worse
than saying nothing.

`Tracker` gets its third implementation in [src/tracker/github.rs](../src/tracker/github.rs):
`GithubTracker<H: Http>`, generic over a small `Http` seam (`FakeHttp` in tests, `UreqHttp` —
over `ureq` with `rustls`, no C toolchain needed — in `main.rs`). GitHub has no workflow
states beyond open/closed; the module doc there is the write-up of that mapping (a
`state:<name>` label convention) and should be read before touching it. Two contract details
worth knowing before changing either `Tracker` impl: `by_ids` must fail the whole call on
anything other than a clean 404 — a transport error silently dropped from the result would be
indistinguishable from the id having genuinely disappeared, which is exactly the ambiguity
`refresh_miss_grace` exists to bound, and bounding it needs the *real* miss count, not one
deflated by swallowed errors. And `ureq`'s default turns a non-2xx response into an `Err` that
discards the headers and body this adapter classifies on (rate-limit header, error message) —
`UreqHttp::default()` disables that (`http_status_as_error(false)`) so every status code
arrives as an ordinary response. `FakeHttp`-based tests are structurally blind to that class
of bug — they hand `GithubTracker` an already-correct `HttpResponse` — which is why
`ureq_http_tests` in the same file talks to a raw `TcpListener` instead.

A tracker failure has no issue to quarantine against — `by_states`/`by_ids` are batch calls,
not scoped to one ticket — so `TrackerError::class()` ([src/tracker/mod.rs](../src/tracker/mod.rs))
reuses `ErrorClass::retryable()` only to pick a log level: `dispatch_new`, `dispatch_due_retries`
and `refresh_running` all skip the tick and try again either way, but a bad credential now logs
at `error` with a "will not resolve on its own" hint instead of blending into the same `warn` a
rate limit gets. That is the honest version of "an auth failure stops trying and gets loud" at
this scope; a literal per-issue quarantine here would be quarantining tickets a bad token had
nothing to do with.

`Worker` gets its real implementation in [src/worker/claude.rs](../src/worker/claude.rs):
`ClaudeWorker`, over `claude -p --output-format stream-json`. Two things there were confirmed
against a real install rather than assumed, because guessing wrong would have meant a worker
that silently never worked: there is no `--max-turns` flag, so the per-session turn budget is
self-enforced — the reader thread counts `assistant` events and sends `SIGTERM` once the count
reaches `max_turns_per_session`, reporting `Outcome::Continue` itself; and `--bare` needs
`ANTHROPIC_API_KEY`, which an OAuth-authenticated operator (this dev machine included) does not
have, so it is not passed by default — the worker inherits whatever hooks and MCP servers the
operator's own `claude` config has until a dedicated API key changes that trade-off. The model is
not inherited that way: `worker.model` and `worker.effort` become `--model` and `--effort` on
every attempt, a resumed one included, and each run row records what it was given (#36). Both
unset passes neither flag, which is the old behaviour exactly; `crew.github.toml` pins them.
`--fallback-model` is deliberately never passed — it would make the recorded model possibly
wrong. `Outcome`
beyond done/failed — `Continue`, `Blocked` — has no structural signal from the CLI to key off,
so the worker's prompt asks the agent to end its final message with `CREW_OUTCOME:
continue: <reason>` or `CREW_OUTCOME: blocked: <reason>`; the module doc has the reasoning,
and it is a soft convention by design — an agent that forgets it just reads as `Done`.

Token totals come from the terminal `result` event and nowhere else (`Progress::tokens`, an
`Option`). The first live dispatch (#7) summed the `usage` block of every streamed `assistant`
event instead and recorded ten million input tokens and four hundred output tokens over 83
turns: the CLI emits one `assistant` event per content block, each carrying the whole turn's
usage, and the per-event `output_tokens` is a streaming placeholder. Two things follow. Stall
detection no longer has token counters to watch, so `Progress::events` — a count of every
parsed stream event, tool results included — is the liveness signal, and it is a better one: an
agent an hour into a long tool call was previously indistinguishable from a silent one. And a
run that ends without a `result` — killed, crashed, or cut off by the turn budget, which on a
real install produces no `result` at all — reports `None`, stored as NULL, and lands in the
dashboard's `(+N uncounted)` tally rather than as a zero or an estimate. Schema v3 dropped the
totals recorded before this; they were unrelated to the real cost, not a rough version of it.

A rejected `rate_limit_event` is the one other thing `run_reader` parses off the stream, added
for #29's sibling defect (#37): three ordinary-looking dispatches interrupted by the same
account-wide limit each quarantined on their own, after three retries inside ninety seconds
against a five-hour reset nineteen minutes away — the exact "quarantine a ticket a bad token had
nothing to do with" mistake `TrackerError::class()`'s doc already warns against, on the worker
side of the process instead of the tracker side. `ClaudeWorker::rate_limit()` surfaces the
signal independently of `Outcome` — the CLI still reports its own verdict, ordinarily `Failed`,
since the process exits with no explicit marker — and `harvest_finished` checks it *before*
`apply_outcome` even sees the outcome: a run interrupted this way charges no attempt and no
quarantine streak, and releases the claim with `Store::release_for_rate_limit` rather than
`release`, which is what lets the issue resume at the attempt and session it was already on
rather than looking like a fresh start. What pauses is dispatch itself — `Scheduler::rate_limited`
checked once per tick, between `sweep_parked` and the two dispatch steps — until the CLI's own
`resetsAt`, published on `Snapshot::rate_limit_pause` so `status` reads "waiting on a five-hour
limit until 09:00Z" instead of showing an idle daemon with no explanation. A `resetsAt` the
scheduler cannot trust — missing, or already behind the clock — degrades to the ordinary
`Failed` path rather than risking a pause nothing ever lifts, the same failure mode a clock skew
would otherwise turn into a silent, permanent stop.

**Every run leaves a transcript** ([src/transcript.rs](../src/transcript.rs)). The reader copies
each `stream-json` line to a per-run file *before* deciding whether the parser has a use for it
— so the `system` and tool-call lines it drops, and the lines it could not parse at all, are
still there afterwards — then appends how the process exited and what it said on stderr. The
path is on the run row (`Store::run`, `runs_for`) and in the dispatch log line and the TUI
detail pane, so "show me what run X did" needs no knowledge of the layout. Three
things there are load-bearing and each looks removable: writes are **unbuffered, one per line**,
because a block-buffered transcript reproduces the exact defect that caused the wrong diagnosis
this exists to prevent; the root sits **beside** the worktrees rather than inside one, because a
worktree is a git checkout the agent commits from *and* is deleted when its ticket goes
terminal, which is the moment the transcript becomes worth reading; and `prune` is handed the
paths of runs still in `running`, because a **stalled** run stops writing by definition, so its
file ages past the newest `keep_runs` while the process behind it is still alive — pruning it
would lose the transcript of the run most likely to need one and leave the writer on an
orphaned inode. Best-effort like the projector: a transcript that cannot be opened costs a
post-mortem, never a dispatch.

`Tracker` stays a read kernel; the *write* half lives on a separate trait,
`TrackerWrites` ([src/broker/writes.rs](../src/broker/writes.rs)), whose only caller is the broker
([src/broker/](../src/broker/)). `GithubTracker` implements both over one credential that never
leaves the process. The broker hands each dispatched run an MCP server over loopback HTTP, with
a per-run bearer token in the URL path, wired in with `claude -p --mcp-config`. **No tool takes
an issue id** — the target is resolved from the token, which is the whole security property; a
call that passes one anyway is refused and audited rather than ignored, because a silent drop
would make the attempt indistinguishable from a well-formed call in exactly the log a reviewer
would read. Budgets are per-run *and* per-issue, and charge attempts rather than successes: a
per-run cap alone bounds nothing, because the continuation loop opens a fresh run each time —
the same gap `max_turns_per_issue` closes for turns — and a budget only spent on successes
would leave a loop of failing writes free. A session is an RAII guard held inside the run's own
record, so the token is revoked and its config file deleted on every path that ends a run,
including ones not yet written. A broker that cannot bind, or a session that cannot open,
degrades to an agent without tools and never to a failed dispatch.

The transport is hand-rolled rather than built on `rmcp`, and
[src/broker/server.rs](../src/broker/server.rs)'s module doc is the write-up — the short version
is that `rmcp`'s streamable-HTTP server is a `tower::Service` with no listener, so it adds ~35
crates and still needs axum on top, and it is async where `Tracker`, `TrackerWrites` and
`Scheduler::tick` are all deliberately blocking. What it would wrap is four methods. The wire
details there were recorded from a live `claude 2.1.278` handshake, not read off a spec: the
first request is `server/discover` (answer `-32601` and the client falls through), the
initialized notification must get `202` with no body, and the SSE `GET` can be refused with
`405` — which is what makes a plain JSON response to each POST the entire transport.

A continuation resumes rather than restarts: the scheduler names the conversation with
`--session-id` before the first attempt and passes `--resume <id>` for every attempt after, so
the turn budget is not spent twice over on the same re-orientation. The name is written to the
store before the process exists, for the same reason the claim is — the child cannot be what
records it. A run that takes no turns drops the name, which is how a session the CLI no longer
holds degrades to a cold start instead of failing every retry identically into quarantine.
A resume leaves the issue body out, as already held, unless it changed: `launch` swaps the
body's hash into `issue_state.session_body` (v12), and a resume whose hash differs sends the
body again under a "changed since your last session" heading, because the description is where
decisions are written (#109). A rebase conflict that parked the issue `Blocked` is queued in
`issue_state.pending_feedback` as `Feedback::Conflict` and taken by the next launch, so the
run a human's unblocking dispatches is told the base and the paths, not that it ran out of
turns.

The first live end-to-end run of `ClaudeWorker` (real agent, real worktree, cut off mid-run by
`--max-ticks`) left an orphaned `claude` process running after `cargo run` had already
returned. `RunHandle` says plainly that dropping a handle does not stop the work, and nothing
before this had ever exercised the "exit with a run still in flight" path — the scheduler's own
tick loop calls `harvest_finished`/`terminate` for every state transition except "the process
just quit." `Scheduler::shutdown()` (called from `main.rs` after the loop) and a `Drop for
Scheduler` safety net (for a panic or an early `?` return that skips the explicit call) both
terminate every run still in `self.running` before letting the process end. If you add another
place `main.rs` can exit, check that this still runs.

**A `Done` is a claim until the handoff gate agrees** ([src/gate/](../src/gate/)). Issue #21: two
branches dispatched off the same base each passed `cargo test` alone, and their combination did
not compile — three defects that existed only in the merge, which neither agent could have seen.
Nothing rebased a finished branch onto the current base, and nothing re-ran the checks after. So
`Gate` is the seam between an agent saying `Done` and a human being handed the branch: `GitGate`
resolves `gate.base` in `workspace.repo` (not in the worktree, whose HEAD is the run's own
branch), skips a branch with no commits beyond it, rebases — unless the branch already contains the
base's tip, since a rebase would drop an agent's merge of the base and its conflict resolution
with it (#122) — and on a clean rebase execs each
`gate.commands` argv directly in the worktree — no shell, for the same reason the worker has
none, and this one inherits the operator's environment because it is the operator's own suite
with no agent involved. The verdict is deliberately three-way and the split is the point: a
**conflict** is a human's problem, so the rebase is aborted (the branch goes back to exactly what
the agent committed, which is what keeps `remove`'s merged check on its side) and the issue parks
`Blocked` naming the paths — unless every path is in `gate.agent_resolvable` (`CLAUDE.md`,
`docs/**` and `src/store/schema.rs` in `crew.github.toml`), where two branches appended to the
same list and the verdict is a `Continue` carrying the conflict as its brief (#111); a **failing command** is the agent's, so the verdict is `Continue` and
the output rides into the next spawn as `Feedback::Gate` on `Worker::spawn` — the same
parameter delivery's CI and review feedback travel on, so there is one channel and one rendering
site for "why this attempt exists" — where the real worker puts it in the prompt; and `gate.max_failures` **consecutive** failures escalate to
`Blocked`, because a gate that can be failed forever is the continuation runaway wearing a new
name. A `Blocked` reason now also lands in the store's `last_error`, so the dashboard shows why
an issue is parked instead of only the log. Setting no gate is a decision, not a degrade — unlike
the broker or the projector, a scheduler without one hands a `Done` to a human exactly as the
agent left it, so `main.rs` attaches one whenever `gate.enabled` is true (the default, with an
empty command list, which makes the default a rebase and nothing more) and the scheduler tests
attach `FakeGate` explicitly. `crew.github.toml` sets the commit-gate commands from **Commands**
above; the fake worker never commits, so under `crew.toml` every gate finds nothing to hand
off.

The ops API ([src/api/mod.rs](../src/api/mod.rs)) is the second observer of that same published
snapshot: `GET /api/v1/snapshot`, `GET /api/v1/issues/:identifier`, `POST /api/v1/refresh`,
`POST /api/v1/unquarantine/:identifier`, `POST /api/v1/unblock/:identifier`. `Api` holds a
`watch::Receiver` and a command sender and no `Store`, so rule 3 is enforced by the type rather
than by discipline — and the `POST`s can express nothing the dashboard's `r`, `u` and `b`
keys cannot. Off by default (`[api]
enabled`, or `--api <addr>` for one run) and loopback unless `api.allow_public` says otherwise,
because the `POST` routes control agent execution. Deliberately *not* validated in
`Config::preflight`: preflight gates dispatch, so a typo in an address the scheduler never uses
must not be what stops it — `api::bind` parses it once, and a failure there is logged and
costs the API alone. The write path goes `HTTP task → Command → the loop in main.rs → oneshot`,
which is what keeps a hung client off the tick: the scheduler answers into a channel whose
receiver may already be gone and never waits to find out. The HTTP is hand-rolled (~200 lines,
no keep-alive, one response type) for the same reason the rest of this crate is small; the
tests in [tests/api.rs](../tests/api.rs) drive it over a real loopback socket against a real
`Scheduler`, because framing and connection-close bugs are exactly what a fake would hide.

Two notes for anyone adding an endpoint. `:identifier` resolves the dispatch id first and an
identifier second, and answers `409` with the candidate ids when one identifier names two
issues — identifiers are not unique, which is the same fact `worktree_key` exists for. And
`POST /unquarantine` on an issue that is not quarantined is a `200` saying so, not an error:
the guard lives in `Store::unquarantine`'s `WHERE` clause, because an unconditional version
would reset a *running* issue's phase to `released` and let the next tick dispatch a second
agent onto its worktree. `POST /unblock` (#108) is the same shape for a park: it is how a
`Blocked` issue — a gate's rebase conflict, typically — is handed back once a human has resolved
it, since with `active_states = ["open"]` the tracker has no state to move it through. It clears
`parked_state` and the parked note and nothing else, so the next `dispatch_new` claims it the
ordinary way and `prepare` attaches to its branch; `Store::unblock`'s `WHERE` refuses anything
running, gating (a held claim is phase `running`), retry-queued, quarantined, or parked under a
delivery still `pending`, `awaiting` or `ready` — which would push or hand back the branch in the
tick an agent is dispatched onto it — or `handed_off`, whose branch is the operator's. `Scheduler::unblock`
also reads the ticket fresh and keeps a park whose ticket is no longer active, since
`sweep_parked` — which reclaims a closed ticket's worktree — only walks parked rows. Write what
changed into the issue's description first: that is the prompt the next run reads.

`crewctl status` ([crewctl/src/main.rs](../crewctl/src/main.rs), over
[libcrew/src/client.rs](../libcrew/src/client.rs) and [libcrew/src/render.rs](../libcrew/src/render.rs))
is the other end, and it exists because the API on its own was not enough: it had been able to
answer for hours at the moment diagnosis instead went to a block-buffered log file and got the
wrong answer (#24). Nothing was missing server-side — what was missing was something to type.
So it is a client and nothing else, in its own binary: an operator asking what is running must
not be able to disturb it, and a second process on `crew.db` while the daemon holds it would be
exactly that. `crewctl` links no `Store`, worktree or tracker code at all — see the package
split under **What this is** — and its HTTP is a single `std::net` GET rather than an HTTP
crate, so its graph stays that small. It shares `Snapshot` and `Row` with the server through
`libcrew` instead of re-describing them, so a renamed field fails the build rather than
rendering a blank column, and it uses the same `fmt_count`/`fmt_ms`/`Phase::label` as the
dashboard so a duration means the same thing on all three surfaces.

Two behaviours there are load-bearing and easy to "simplify" away. It finds the daemon itself —
`--api`, then `[api] bind`, then `DEFAULT_API_BIND` — and reads the config *leniently* rather
than through `Config::load`, because that runs `preflight`, and preflight gates dispatch: an
unset `tracker.owner` is a real problem for the daemon and none at all for a client asking what
the daemon is doing. And `StatusError` keeps apart the two failures that a stack trace makes
look identical: nothing listening (no daemon — or one with its API off, which is the default
and so the likelier reading) versus a daemon that answered and refused. Collapsing those is
what has somebody restart a daemon that was never down, so each message names the address
tried, where that address came from, and the way out.

The ops MCP server ([src/api/mcp.rs](../src/api/mcp.rs)) is the ops API's routes for an *agent*
supervising the daemon — the one driving a dogfooding session, a watchdog later — which the
`status` client left on the wrong side of the gap it closed: an agent had to spawn the CLI and
parse a rendering built to read well to a person. One tool per route (`snapshot`, `issue`,
`refresh`, `unquarantine`, `unblock`) and nothing else; each runs the *same* `Api` method the HTTP router
runs and frames the same `Response` as a tool result, a status of 400 or more becoming
`isError: true`. So it holds an `Api` and no `Store`, and it cannot express an authority the
HTTP API lacks — new authority is a separate decision from new transport, and this module has
nowhere to put one. `api.mcp_enabled` or `--mcp <addr>`, off by default, loopback unless
`api.allow_public`, through the same `resolve_bind` the HTTP API uses. The transport is the
broker's hand-rolled server made generic over `McpService` rather than copied or replaced with
`rmcp`; `src/broker/server.rs`'s module doc records what that cost.

That transport spends a thread per connection, and this is the first listener on it whose
address an operator chooses — `allow_public` can put it on a routable interface, where a client
that connects and then says nothing would hold a thread for free. `broker::server::Limits` is
the HTTP API's `READ_TIMEOUT` arriving here: a deadline on a request that has begun and never
ends, deliberately *split* from the idle wait between requests so keep-alive still works (the
real client depends on it), and a cap on connections in flight, because a client that
reconnects rather than dribbles pays nothing for a deadline. The broker's own listener gets
both for free and keeps a separate count, so a flood at the public address cannot starve a
dispatched run of its tools.

**crewd never hands this server to a dispatched agent** — but a dispatched agent on the
operator's machine can still reach it, and that is an accepted trade, not an oversight. The
broker gives a worker authority scoped to one issue; this is scoped to the whole daemon, and a
worker that calls `unquarantine` or `unblock` can clear its own quarantine or park and
re-dispatch itself, defeating the verdict, `max_turns_per_issue` and `parked_state` together.
What crewd controls it enforces by wiring: the ops server binds **its own listener** (never the
broker's — a shared one routed by prefix would put these tools at the exact `host:port` every
worker is handed), answers only at `/ops`, and is never passed to `Broker`, whose `open` writes
the only `--mcp-config` crewd gives a worker. `a_dispatched_worker_is_not_handed_the_ops_tools`
reads that file from a real session and connects to what it names.

What wiring cannot control is the operator's own `claude` config. The worker runs without
`--strict-mcp-config` on purpose, so it inherits the operator's MCP servers — and **local scope
does not keep this one out**: a worker's cwd is a worktree of the same repository, which Claude
Code treats as the same project. On 2026-09-26 a dispatched run's `init` event listed `crew_ops`
connected while it was registered only at local scope. The operator accepted this (2026-09-25,
"option 2"): workers run as the same user and are trusted as that user, and the ops tools add
nothing such a process cannot already do by sending a request to the loopback HTTP API the same
routes live on. The way to actually withhold them is `--strict-mcp-config` with the worker's
servers passed explicitly (rust-analyzer from `.mcp.json` among them); it was not taken.


**`crewd init`** ([src/init/](../src/init/)) registers the operator's own GitHub App through the
App Manifest flow (#65): a loopback page posts the manifest to GitHub, the operator clicks
*Create*, GitHub redirects back with a single-use code, and `init` exchanges it for the App's
id and private key, then sends the browser on to *Install* and reads the installation id back
over the App's own JWT. It writes `~/.crewd/github-app.pem` (600) and `~/.crewd/github-app.toml`
(`app_id`, `installation_id`, `private_key_path`) in a 700 directory — the file #64's
`tracker.github_app` names — and never edits the daemon's config or overwrites either file.
Each operator registers their own App because the key is the App owner's; `GITHUB_TOKEN` and
a hand-registered App written into the same file stay supported. The listener reuses the broker
transport's `read_request`, `Limits` and `ConnSlot` cap, not its MCP service: it binds loopback, answers only
its own `Host`, and is joined shut once a callback carrying a code arrives. The GitHub calls go
over the tracker's `Http` seam, so the whole flow is tested against a fake GitHub over a real
socket; the real two-click run is the operator's. The conversion response is the one place in
the crate that carries a private key and a client secret, and the types are what keep it out of
the logs: the key sits in a `Pem` whose `Debug` redacts, and the secrets have no field at all.

**Delivery** ([src/sched/delivery.rs](../src/sched/delivery.rs), behind the `Forge` and
`Publisher` traits in [src/forge/](../src/forge/)) is what happens after a run reports `Done`,
when `[delivery] enabled` is on. `Done` still releases the claim and parks the issue exactly as
before; delivery is then a row in the store advanced on the tick — after reconciliation,
before the dispatch gate, at `delivery.poll_interval_ms` — through: push the branch
(`Publisher`, implemented by `GitWorktreeWorkspace`, from the worktree), open or find the pull
request (`Forge`, `GithubForge` over the tracker's `Http` seam), request the configured
reviewers *and read back whether they attached*, read CI, read the review threads — and the
reviews' summaries, since a reviewer can leave a finding on no line (#126). A summary on the
current head from a `delivery.summary_reviewers` login (Copilot by default), or in the
`CHANGES_REQUESTED` state from anyone, that says more than "Findings: None" is handed back whole
as one more comment keyed `review-<id>`: no parser for its sections, whose format is nobody's
contract, and noise costs one `rejected` verdict, posted as a pull request comment since a
summary has no thread. A red CI or
an open comment sends the issue back to an agent by the same path a `Continue` takes — a retry
due now, the session resumed, and the failure in the prompt as `Feedback::Ci` or
`Feedback::Review` — which is the literal form of "a red gate is a `Continue`, never a `Done`".
A pull request the provider reports unable to merge is not waited on at all, since GitHub runs no
CI on it: delivery charges a round and re-gates the branch as if its `Done` were new, so the
gate's rebase and its conflict rule decide what follows (#159).
The handoff gate's failing output travels the same way, as `Feedback::Gate`: `launch` builds one
`Feedback` from delivery's structured row when there is one and from the retry reason otherwise,
so `Worker::spawn` has a single parameter for the question and `feedback_help` in the worker is
the single place its wording lives. And delivery only ever sees a branch the gate has passed —
see the tick order above. The pull request body is derived
from the run record (commits, runs, turns, tokens), never composed by the agent. Verdicts on
review comments come back as `CREW_REVIEW: <id>: accepted: <commit>` or `rejected:
<reason>` lines in the agent's final text, the same soft convention as `CREW_OUTCOME`; the
orchestrator replies on the thread and, once that reply has landed, records the verdict in
`review_verdict`, and a settled thread is never handed out again. A later step of the same
poll resolves that thread on the provider (#89) — GraphQL, since REST cannot — and a resolve
that fails is retried next poll without a second reply. A comment the agent gives no
line for stays open — and so does one whose acceptance names no commit, or a commit the branch
does not carry: the worker drops an `accepted:` whose detail is not shaped like a commit, and
the scheduler checks the rest against the branch (`Publisher::carries`) at the moment the run
reports `Done`, before the gate's rebase rewrites the shas the agent named.

Four things there are load-bearing. Every hand-back is a *round*, bounded per pull request
(`max_rounds_per_pr`) and per issue (`max_rounds_per_issue`), and the per-issue count never
resets — not for a new run and not for a new pull request — because a reviewer that comments
on every push, answered by an agent that pushes, is a loop with no bound of its own, and one
that reset with the pull request would bound nothing (the same gap `max_calls_per_issue`
closes for the broker). At either bound the pull request is handed to the operator with the
outstanding items named. A review request is followed by a read of the pull request and its
reviews, because GitHub answers a request for a bot reviewer with `200` and attaches nobody
(GETT-174120); a request that verifiably attached nobody is a handoff with that reason on the
issue's row, not a success. `Forge` has no `merge` method, and must not grow one — merging is
the operator's, and the trait's shape is what enforces it. And a delivery step only runs for an
issue nothing else owns (phase `released`, no live run), so a push cannot land under a running
agent and a hand-back cannot race a dispatch. A branch whose work sits on another issue's branch
gets its pull request based on that branch (`Publisher::stacked_on`), so the two stay
reviewable apart — provided the remote has that branch. A lower branch still running, or done
and not yet pushed, is not a base a pull request can be opened against, so the upper one opens
against the trunk rather than handing off on the provider's 422; and when a later push computes
a different base than the open pull request targets — the lower branch merged — `open_pull_request`
retargets it, body included, so the store, the snapshot and the provider agree.

The review on PR #42 found the handoff itself wrong in six places (#47), and two of them are
worth knowing the shape of before touching `publish` or `apply_verdicts`. The push is
`--force-with-lease`, because the gate rebases an already-published branch before every
re-delivery and a plain push then fails non-fast-forward in exactly the round the base moved;
forcing is sanctioned because the branch is the orchestrator's own, and the lease is what keeps
that apart from forcing over someone else's — a lease failure is classified on its own,
permanent, naming the remote branch that moved. And a verdict is settled by its reply landing
and by nothing else: `apply_verdicts` replies first and records second, a failed reply leaves
the verdict queued on the row and the step returns the error, so the threads are not read — or
handed back to an agent — until it lands. Recording first, as it did, hid a verdict from its
reviewer for good over one transient error while delivery went on to `Ready`.

## Examples that talk to real services

`broker_live` is the counterpart for the tool broker, and it exists because nothing inside
this crate can prove the real CLI agrees with it: the transport tests drive a socket this crate
also wrote, and the tool tests drive a fake tracker. It spawns an actual `claude` process
against an actual broker and asserts the orchestrator performed exactly one write. It needs a
working login and spends tokens; it writes to no tracker.

`dashboard_preview` is the fastest way to see a layout change: it renders a canned `Snapshot`
through ratatui's `TestBackend`, so there is no terminal and no scheduler involved.

## The GitHub App identity and the push credential

`tracker.github_app` replaces `GITHUB_TOKEN` with a GitHub App identity (#64): it names a file
holding `app_id`, `installation_id` and `private_key_path`, and every tracker write, forge call
and branch push is then authored by the App. The token is a *source*, not a `String`
(`src/credentials.rs`): an installation token expires hourly, so `GithubApp` mints on the
injected clock and re-mints `REFRESH_MARGIN_MS` before expiry. The push reaches it through a
`git credential-store` file `publish` creates in a private temp directory and deletes after one
push (`PushCredentialFile` in `src/workspace.rs`) — never a URL (lands in `.git/config`), an
`http.extraheader` (lands in argv) or an environment variable. That keeps the token out of what an agent
reads by accident, not out of reach of one that goes looking: it runs as the same user as the
key file, which is the limit **Constraints for the worker and broker** already records. `Config::load` loads the file
and the key once (`check_github_app`, not the per-tick `preflight`), so a half-configured App is
refused by name at startup. It is commented out in `crew.github.toml`
until `~/.crewd/github-app.toml` exists on the host; an uncommented key with no file there
stops the daemon from starting.

## Why the worker switch is separate from the tracker

`worker.kind = "claude"` is the other half — and it is a separate switch from the tracker on
purpose (see `WorkerConfig`'s doc in [src/config.rs](../src/config.rs)): a real tracker with the
fake worker is a safe way to watch real dispatch decisions without spawning real agents,
turning "point this at a real repo" into "start editing that repo" only when both are flipped
deliberately. With it on, `cargo run` spawns real `claude -p` processes with
`--permission-mode bypassPermissions` — no human answers a tool-use prompt in a headless
dispatch — against real git worktrees. Treat `--max-ticks` on a config with `worker.kind =
"claude"` as spawning real, tool-using agent processes, not a dry run.

## Why the broker is not an isolation boundary

The MCP tool broker ([src/broker/](../src/broker/)) executes tracker writes host-side while
holding the credential. The worker receives results, never a raw token — and that is the
exact extent of the claim. **The broker is not an isolation boundary, and the module doc says
so.** Dropping `HOME` from the allowlist to close off `gh`'s stored token was tried and
abandoned on evidence: on macOS `gh auth token` succeeds with `HOME` unset, because the token
lives in the login keychain, keyed to the user session rather than to a path under `$HOME`.
`claude`'s own OAuth credential behaves the same way — this dev machine has no
`~/.claude/.credentials.json` at all and authenticates fine with `HOME` scrubbed. An
environment allowlist cannot take away a credential that was never in the environment or the
home directory; only a real sandbox (separate uid, container, seatbelt profile) could, and
that is a different and much larger change. So a dispatched agent here *can* still comment,
push and close as the operator through the ambient keychain. What the broker adds is a
sanctioned path that is scoped to one issue, budgeted and logged — so the agent has no reason
to reach for the ambient one, and every write it makes through the front door is auditable.
Do not restate this as "the worker cannot reach a credential"; it is the weaker of the two
options issue #4 offered, and it was chosen because the stronger one is not true on this
platform.

## Running the daemon inside a worktree

One thing the overrides do not cover: running the daemon from *inside* a worktree — which is
what a dispatched agent's cwd is. `GitWorktreeWorkspace::new` refuses that at startup, because
`workspace.repo = "."` there is a linked worktree and the worktrees a run would create register
in the top-level checkout's shared `.git`, where the orchestrator owning it never recorded them
(#29). The error names the way out: point `workspace.repo` at a throwaway clone and
`workspace.root` beside it. Setting `CREW_DB` alone does not help — the litter was never
in the store.
