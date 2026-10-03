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
| [3. A worker runs in the sandbox its config names](adr/0003-sandbox-modes.md) (Proposed) | `[sandbox] mode = "off" \| "wrapper" \| "microvm"`, default `off`; the whole worker and the gate run inside it, with egress through a crewd allowlist proxy; `container`, `remote` and `kubernetes` are reserved and stop startup |

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
branch delivery pushes is the one brought onto the base and re-gated. That order is the stack #43/#42 were built
as — gate, then delivery — and
`a_done_branch_is_gated_before_delivery_pushes_it_and_a_failing_gate_publishes_nothing` is
what fails if it is ever inverted.

`harvest_gates` is where a `Done` becomes a verdict. With a handoff gate attached (see below), a
run whose agent reports `Done` leaves `running` for a `gating` map instead of being released: its
claim stays held, its run row stays open, and the gate — bring the branch onto the base, then the configured
commands, in the run's own worktree — runs on a thread the scheduler polls. This step turns the
gate's answer into `Done`, `Blocked` or `Continue` and hands it to the same `apply_outcome` an
agent's verdict goes through, which is what makes a gate-sent continuation subject to the same
turn budget and the same escalating delay. It sits with the rest of reconciliation, ahead of
`preflight`, because a claim held mid-rebase or mid-merge must not stay held behind a config typo.

`recover()` is startup reconciliation, and it runs ahead of the gate for the same reason: a
claim stranded by the last process must not stay stranded behind a config typo. It lives inside
`tick()` rather than in `main.rs` on purpose — recovery a second entry point can forget to call
is recovery that silently does not happen, which is the exact failure it exists to fix. It is
callable directly (`Scheduler::recover`) and idempotent, so a caller that wants it eagerly can
have it. It releases every unmatched claim, so it is safe only while one process has the store.
`main` takes that premise before the first tick: an exclusive OS lock beside the canonical path
of the database, held until the process dies and released by the kernel on a hard kill (#217).
A symlink and a `./crew.db` spelling of one file share that lock. A second daemon on the same
store exits at startup, naming the store and the pid that holds it.

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
says it carries nothing `repo`'s HEAD (or the base) does not already have, and `prepare` attaches
to an existing branch that does carry commits rather than `-B`-resetting it.

A new branch is named `crew/<number>-<slug>`: the identifier with one leading `#` dropped, then
a slug of the title cut at a word boundary. The name is fixed when the branch is created.
`issue_state.branch` is what delivery pushes, and a symbolic ref `refs/crew/branch/<issue key>`
is what the next `prepare` finds when the store does not have the name yet — including after a
title edit, which must not mint a second branch. A pretty name already recorded for another
dispatch id gains the same dispatch-id suffix as the worktree directory. A branch created under
the old `crew/<sanitised key>` name is reattached, not renamed (#205).

A new branch starts from
the base the gate will rebase onto — `<delivery.remote>/<base>` fetched under the gate's lock and
credential, else the local base — not from `repo`'s HEAD, the operator's checkout, which can be
any branch at any age. A failed fetch fails `prepare` as a retryable workspace error, and so
does a lock still held after a bounded wait, or a fetch that has not finished within its own —
it is killed, rather than left to hold the tick (#170).

An existing branch is attached where it is when it holds commits the base does not. The checkout
is that measure only when no base is configured: a checkout that already contains the branch
would otherwise send `prepare` down `-B` and move the branch onto a base that lacks it.

What the agent had *not* committed when its run was stopped is snapshotted by `remove` to a new ref under `refs/crew/wip/<issue key>/`
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
its parent is merged; the merged check is not weakened for litter. `Prepared.branch` reports that name upwards so it reaches the dispatch log, and the scheduler
stores it on `issue_state.branch`, which is what the snapshot publishes as `Row.branch`.
`Workspace::branch_for` is the naming `prepare` uses and does look at git — the owner ref, a
legacy branch, a collision — so a title edit does not mint a second name (#205). The snapshot
does not call it. A finished run reports the stored branch, which is why that name is still
there to read after the worktree is gone, and why a tick does not spawn git per row.
`Row.branch` is `None` until an issue has been dispatched at least once, because before that
no run has written a ref and pointing an operator at a name nobody created is worse than
saying nothing.

`Tracker` gets its third implementation in [src/tracker/github.rs](../src/tracker/github.rs):
`GithubTracker<H: Http>`, generic over the `Http` seam in [src/http.rs](../src/http.rs)
(`FakeHttp` in tests, `UreqHttp` — over `ureq` with `rustls`, no C toolchain needed — in
`main.rs`). The same seam, percent-encoder and authenticated-request helper serve the Jira
tracker and the GitHub forge (#181). GitHub has no workflow
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
`ureq_http_tests` in [src/http.rs](../src/http.rs) talks to a raw `TcpListener` instead.

A tracker failure has no issue to quarantine against — `by_states`/`by_ids` are batch calls,
not scoped to one ticket — so `TrackerError::class()` ([src/tracker/mod.rs](../src/tracker/mod.rs))
reuses `ErrorClass::retryable()` only to pick a log level: `dispatch_new`, `dispatch_due_retries`
and `refresh_running` all skip the tick and try again either way, but a bad credential now logs
at `error` with a "will not resolve on its own" hint instead of blending into the same `warn` a
rate limit gets. That is the honest version of "an auth failure stops trying and gets loud" at
this scope; a literal per-issue quarantine here would be quarantining tickets a bad token had
nothing to do with.

`Tracker`'s second real implementation is [src/tracker/jira/mod.rs](../src/tracker/jira/mod.rs):
`JiraTracker<H: Http>`, over Jira Cloud's REST API v3 (#99) — Data Center and OAuth 3LO are out
of scope, a design-review decision recorded in the module doc and in
[#99](https://github.com/StGerman/crewd/issues/99)'s Decisions section. The issue key
(`PROJ-123`) is the dispatch id: it changes only when an issue leaves its project, and the
adapter then omits it, which the scheduler already treats as "not visible" rather than as an
error — the same contract `by_ids` documents for a 404. Reads split the same way GitHub's do:
`by_states` is `GET /rest/api/3/search/jql`, paged by `nextPageToken` rather than `POST`,
because `Http::send_json` is the seam's write half and a poll going through it would make the
read kernel indistinguishable from a mutation at the seam; `search_all` fails the whole call on
a later page's error rather than returning a short list, and on a page token equal to the one
that produced it rather than looping forever. `by_ids` makes one `GET` per key: a clean 404 or
an issue now in another project is omitted, and anything else fails the whole call, the same
"omit only on an unambiguous absence" rule `github.rs`'s own `by_ids` holds to. `by_states`
quotes `active_states`/`terminal_states` verbatim as status names in its JQL, so a status this
site's workflow does not have fails every poll with Jira's own 400 naming it — deliberately
loud, since the alternative would be a poll that came back empty and looked like a healthy
backlog with nothing ready.

Dispatch needs
`tracker.dispatch_label`, which is the whole signal — a Jira service account cannot be an
assignee at all, the same finding #64 made for GitHub — and `tracker.jira.assigned_to_me`
narrows further, checked per issue in *both* `by_states` and `by_ids`, because `refresh_running`
stops a run the moment `by_ids` reports it as no longer dispatchable; narrowing only the poll's
own JQL would leave a reassigned issue running past that point.

The description is Atlassian
Document Format, and the adapter renders it to Markdown in its own module
([src/tracker/jira/adf.rs](../src/tracker/jira/adf.rs)) rather than pulling in a crate: `jc-adf`
0.2 and `atlassian-markdown-converter` 0.1 are early 0.x releases from single maintainers.
Priority ranks by the priority's *name* (`priority_rank` in `src/tracker/jira/issue.rs`), never by
Jira's own priority id — a site's ids are creation order, not severity order; GETT's own
"Trivial" is id 10000.

`set_state` applies whichever transition's target status matches the
requested state, case-insensitively, with no operator-configured state-to-transition map; a
workflow with no path to the requested status fails naming every status a transition can reach,
and a transition whose screen requires a field Jira does not accept from this call fails with
Jira's own message. `link_pr` posts a remote link with `globalId` set to the PR URL, so a repeat
call updates the link in place instead of adding a second one.

Authentication is Basic auth —
`JiraCredentialsFile::basic_token` base64-encodes `email:api_token` once, and the result is
handed to the same `StaticToken` the `GITHUB_TOKEN` path uses, since a Cloud API token does not
expire on a schedule this daemon needs to track; the file itself is read once at
`Config::load` (`check_jira_credentials`), so a half-configured file is refused by name at
startup rather than met as a 401 on the first poll. Jira Cloud's rate limiting is cost-based
with no budget header the way GitHub's is, so there is no equivalent of `github.rs`'s per-hour
arithmetic to size `interval_ms` against — it stays an empirical knob, tightened only if a 429
starts showing up in the log.

A Jira-tracked issue is not a GitHub issue, so delivery still needs a GitHub repository to push
to and open a pull request against. `[forge]` (`ForgeConfig`) names one — `owner`, `repo`,
`github_app` — and each key falls back to the matching `[tracker]` key when unset
(`Config::forge_owner`/`forge_repo`/`forge_github_app`), so a GitHub-tracker config, where the
two repositories are the same one, needs no `[forge]` table at all and `crew.github.toml` works
unchanged. A GitHub tracker refuses a separate `[forge]` instead (`preflight`): naming another
owner, repo or App there would silently move every tracker write and every push onto a different
credential, with `tracker.github_app` never consulted again, so `[forge]` is for a tracker that
is not GitHub. Delivery is `[forge]`'s only consumer, and `main.rs` builds neither the forge nor
its credential for a non-GitHub tracker until `delivery.enabled` is true — `preflight`'s
owner/repo check and `check_github_app` are gated the same way — so a Jira dry run, the "real
tracker with the fake worker" shape `CLAUDE.md` recommends for watching real dispatch decisions,
needs no GitHub App, token or repository on disk at all. The pull request body writes `Closes
<url>` only when `url` is a GitHub issue's own permalink; otherwise, as for a Jira key, it writes
`Issue: [PROJ-123](url)`, since Jira has no GitHub-recognised closing keyword. Nothing here moves
the Jira ticket once that pull request merges (#180): a person or a Jira automation transitions
it, and `refresh_running`'s cleanup reclaims its worktree once the ticket itself reaches a
terminal status.

`Worker` gets its real implementation in [src/worker/claude.rs](../src/worker/claude.rs):
`ClaudeWorker`, over `claude -p --output-format stream-json`. Two things there were confirmed
against a real install rather than assumed, because guessing wrong would have meant a worker
that silently never worked: there is no `--max-turns` flag, so the per-session turn budget is
self-enforced — the reader thread counts `assistant` events and sends `SIGTERM` once the count
reaches `max_turns_per_session`, reporting `Outcome::Continue` itself; and `--bare` needs
`ANTHROPIC_API_KEY`, which an OAuth-authenticated operator (this dev machine included) does not
have, so it is not passed. Instead `--setting-sources project` and `--strict-mcp-config` keep
the operator's user- and local-scope settings, plugins and MCP servers out, and
`CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` keeps out their auto memory (#191). The model is
not inherited that way: `worker.model` and `worker.effort` become `--model` and `--effort` on
every attempt, a resumed one included, and each run row records what it was given (#36). Both
unset passes neither flag, which is the old behaviour exactly; `crew.github.toml` pins them.
`--fallback-model` is deliberately never passed — it would make the recorded model possibly
wrong. [src/worker/grok.rs](../src/worker/grok.rs) is the same contract over
`grok --output-format streaming-json` (#120), checked against `grok 1.0.41`: a turn is a
`usage` event, not an `assistant` event, totals come only from `end`, and a Grok run is
spawned with no broker tools. `Outcome`
beyond done/failed — `Continue`, `Blocked` — has no structural signal from the CLI to key off,
so the worker's prompt asks the agent to end its final message with `CREW_OUTCOME:
continue: <reason>` or `CREW_OUTCOME: blocked: <reason>`; the module doc has the reasoning,
and it is a soft convention by design — an agent that forgets it just reads as `Done`. The
marker is read from the *last* `result` in the stream, which is read to its end: a resumed
session whose previous run was killed with a background task running emits an empty `result`
before the turn it was resumed for, and judging that one read an agent asking for a decision as
`Done` (#214).

Token totals come from the last `result` event and nowhere else (`Progress::tokens`, an
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
rather than looking like a fresh start. What pauses is dispatch to the run's own worker — the
limit is that provider's account, so with several workers ([src/sched/workers.rs](../src/sched/workers.rs),
#119) the others keep dispatching and dispatch as a whole stops only when every worker is paused,
checked once per tick between `sweep_parked` and the two dispatch steps — until the CLI's own
`resetsAt`, published on `Snapshot::rate_limit_pauses` so `status` reads "claude waiting on a
five-hour limit until 09:00Z" instead of showing an idle daemon with no explanation. An issue the
pause released moves to another worker with a free slot rather than waiting out the window
(#165): the worktree already holds the commits, and a session id means nothing to the new
provider, so it starts or resumes its *own* session and is handed a brief — who stopped, why, and
that run's last message as the stopped worker reads its own transcript, truncated to its last
4 KiB. The paused worker's session is parked in `worker_session` with the worktree's head rather
than overwritten, and resumed when that worker next takes the issue only if the head has not
moved: once the other worker has committed, the old conversation describes a tree that is gone.
A worker that is merely full keeps its issue, since a slot frees within a run. A `resetsAt` the
scheduler cannot trust — missing, or already behind the clock — degrades to the ordinary
`Failed` path rather than risking a pause nothing ever lifts, the same failure mode a clock skew
would otherwise turn into a silent, permanent stop.

A `five_hour` warning whose top-level `utilization` is at or above
`agent.rate_limit_warn_utilization` (default 0.80) pauses the same worker one window earlier
(#184): runs dispatched into a 0.98 window spent their turns on reconnaissance and then died on
the rejection with nothing edited. The event's own `surpassedThreshold` is not the threshold,
because the `seven_day` window warns from 0.75 and resets days away; a `seven_day` warning
changes nothing, and its rejection still pauses as before. `RunHandle::rate_limit_warning`
answers mid-run rather than at exit, and `observe_rate_limit_warnings` reads it at the top of
every tick, before `harvest_finished` can remove a run that warned and ended since the last one;
the warned run is left to finish, since slowing a running run is out of scope and killing it
would cost the work it is doing.

A spawn that fails `agent_not_found` is the same kind of mistake on the worker (#216): the
binary went missing while the daemon was running, and treating it as the issue's permanent
failure quarantines a ticket nothing is wrong with. `harvest_finished` releases that claim with
`Store::release_for_rate_limit` and pauses only that worker. The pause is not lifted on a later
tick and the binary is not re-resolved; a restart is what #218's startup check is for.
`Snapshot::halted_workers` (on the wire, still `missing_binaries`) names the worker, the path
and the reason, beside
`rate_limit_pauses`, so `status` reads "grok paused: binary not found (/path/to/grok)" rather
than an idle worker. `model_not_found` stays on the ordinary per-run path. Only
`ErrorKind::NotFound` is classified `agent_not_found`: a permission or resource error on spawn
stays on that ordinary path, because a pause that lasts until restart is for a binary that is
gone. A `Session::New` written before the failed spawn is cleared — the CLI never created the
conversation — and a session the run was resuming is kept. Feedback `launch` had already taken
is put back for the next run. A sync brief is left for the next `sync_before_run`, which
writes it again.

A Grok `error` event reporting HTTP 402 is the same pause for an account with no balance
(#237): `ErrorClass::AccountExhausted`, released uncharged, that worker halted until restart
with reason `account_exhausted`. There is no reset time to wait for, and the balance is not
probed while running: a restart is the operator saying it was topped up. The session and the
feedback are given back only when the run took no turn; a run the 402 cut short later keeps
both, and the handoff carries where it stopped.

**Every run leaves a transcript** ([src/transcript.rs](../src/transcript.rs)). The reader copies
each `stream-json` line to a per-run file *before* deciding whether the parser has a use for it
— so the `system` and tool-call lines it drops, and the lines it could not parse at all, are
still there afterwards — then appends how the process exited and what it said on stderr. A
Grok `tool_call_update` is stored without the repeated `in_progress` output: writing each prefix
fills the cap before `end` (#172). The latest of those updates is written when the stream ends
before `completed`. `text`, `usage`, `end` and `error` stay the bytes that got parsed. The
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
including ones not yet written. A session that cannot open degrades to an agent without tools
and never to a failed dispatch; a broker that `broker.enabled` turns on and that cannot bind or
set up stops startup instead (#218).

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
That cold start, and a resume whose context was compacted, lose what the last run learned, so
both prompts carry the same fixed instruction: keep a checkpoint at the path `git rev-parse
--git-path crew/checkpoint.md` prints, read it first if it exists, and update it after each
commit. Git answers with the worktree's own git directory, which is untracked and dies with the
worktree. The scheduler never reads the file, and neither prompt checks that it exists (#186).

A resume leaves the issue body out, as already held, unless it changed: `launch` swaps the
body's hash into `issue_state.session_body` (v12), and a resume whose hash differs sends the
body again under a "changed since your last session" heading, because the description is where
decisions are written (#109). A gate conflict that parked the issue `Blocked` is queued in
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
branch) — with delivery on, `delivery.base` when `gate.base` is unset, fetched from
`delivery.remote` with the push's credential (retried once if the remote refuses the token) and resolved as `<remote>/<base>`, because the local branch lags until someone pulls and a fetch that fails
fails the gate rather than passing on it (#134) — skips a branch with no commits beyond it,
brings the rest onto the base, and once the branch is on it execs each
`gate.commands` argv directly in the worktree — no shell, for the same reason the worker has
none, and this one inherits the operator's environment because it is the operator's own suite
with no agent involved. How the base comes in depends on the branch: one that already contains
the base's tip is left as it is, one that merged an older base has the new one merged in, and
any other is rebased. A rebase replays a branch's own commits and drops its merges, so rebasing
a branch whose agent resolved a conflict by merging the base drops the resolution and raises the
same conflict again (#122, #177); both conflict briefs therefore tell the agent to merge. The verdict is deliberately three-way and the split is the point: a
**conflict** is a human's problem, so the rebase or merge is aborted (the branch goes back to exactly what
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
the projector, a scheduler without one hands a `Done` to a human exactly as the
agent left it, so `main.rs` attaches one whenever `gate.enabled` is true (the default, with an
empty command list, which makes the default bringing the branch onto the base and nothing more) and the scheduler tests
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
because the `POST` routes control agent execution. The address is validated once, at startup,
rather than in `Config::preflight`, which runs before every dispatch: `api::bind` parses and
binds it before anything else is opened, only when the API is on, and any failure there, a
taken port included, exits crewd naming the address (#218). A daemon scheduling with no ops API
looks healthy and cannot be queried or unquarantined. What `/api/v1` keeps stable, field by
field, and the rule that a breaking change opens `/api/v2` instead, is [api-v1.md](api-v1.md) (#245).
The write path goes `HTTP task → Command → the loop in main.rs → oneshot`,
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
`Blocked` issue — a gate's conflict with the base, typically — is handed back once a human has resolved
it, since with `active_states = ["open"]` the tracker has no state to move it through. It clears
`parked_state` and the parked note and nothing else, so the next `dispatch_new` claims it the
ordinary way and `prepare` attaches to its branch; `Store::unblock`'s `WHERE` refuses anything
running, gating (a held claim is phase `running`), retry-queued, quarantined, or parked under a
delivery still `pending`, `awaiting` or `ready` — which would push or hand back the branch in the
tick an agent is dispatched onto it — or `handed_off`, whose branch is the operator's. A
`handed_off` delivery is what the same unblock hands back instead (#262): `Store::resume_delivery`
moves it back to `awaiting` when it was handed off waiting on its pull request, and otherwise
to `pending`, so a fix run whose push was refused is pushed rather than judged on the old head.
It forgets the review request and the CI wait so neither is timed from before the handoff, and
keeps both round counts, so a pull request at `max_rounds_per_pr` is handed off again on its next
round; the turn budget is checked before any round opens, so one past `max_turns_per_issue` is
handed off rather than dispatched. The
answer says which it did: `park lifted`, or `delivery resumed on <pr url>`. Only the unblock
resumes it: `dispatch_new` keeps a handed-off delivery's park even when its ticket moves between
active states, since a run dispatched there would restart delivery with its `Done`. It keeps a
`pending`, `awaiting` or `ready` delivery's park the same way while delivery is on, which is
what a resumed delivery is, since an ordinary run beside it would work without its review
feedback and outside the round bounds (#266); with delivery off nothing advances such a row, so
the ticket's move lifts the park as it would any other. `Scheduler::unblock`
also reads the ticket fresh and keeps a park, or a handoff, whose ticket is no longer active or
routable, since
`sweep_parked` — which reclaims a closed ticket's worktree — only walks parked rows. Write what
changed into the issue's description first: that is the prompt the next run reads.

`crewctl status` ([crewctl/src/main.rs](../crewctl/src/main.rs), over
[libcrew/src/client.rs](../libcrew/src/client.rs) and [libcrew/src/render.rs](../libcrew/src/render.rs))
is the other end, and it exists because the API on its own was not enough: it had been able to
answer for hours at the moment diagnosis instead went to a block-buffered log file and got the
wrong answer (#24). Nothing was missing server-side — what was missing was something to type.
So it is a client and nothing else, in its own binary: an operator asking what is running must
not be able to disturb it, and a second process on `crew.db` while the daemon holds it would be
exactly that — the daemon now refuses that second process at startup (#217). `crewctl` links no
`Store`, worktree or tracker code at all — see the package split under **What this is** — and
its HTTP is a single `std::net` GET rather than an HTTP crate, so its graph stays that small.
It shares `Snapshot` and `Row` with the server through
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

The operator's own `claude` config no longer reaches a worker either (#191): the worker passes
`--setting-sources project --strict-mcp-config`, so it loads MCP servers only from the broker's
`--mcp-config`, and user, local, plugin and claude.ai servers — this one among them — never
start. A live init event with those flags lists no MCP server of source `user`, `local`,
`plugin` or `claudeai`. What remains is the loopback HTTP API serving the same routes, which a
same-user process can still call; only OS confinement (#135) closes that.

**The build string** is `<version> (<short sha>[-dirty])`, the one string every surface reports
for the build that is running (#244): `crewd --version`, `crewctl --version`, the daemon's
`starting` log line, `Snapshot.build` and so the `crewctl status` header, and
`serverInfo.version` from both MCP servers (set once, in `src/broker/server.rs`). It lives in
`libcrew::build` because both binaries print it. The version is the workspace's single
`[workspace.package] version`; the commit comes from `vergen-gitcl` in `libcrew/build.rs`, and a
build with no git, such as a `cargo install` from crates.io, reports the version alone, which
names exactly one tag. Without it, a restart that silently kept the old binary took a process
listing and a log dig to find.

**A release** is a `v<version>` tag cut by hand when a milestone closes (#248). Versions stay
`0.x` until the v1 promise (#245) and the store schema are declared stable. dist builds it from
`dist-workspace.toml` into `.github/workflows/v-release.yml`, which is generated: change the
config and run `dist generate`. The two edits dist cannot express, marked `Hand-edited (#272)`,
are why the config sets `allow-dirty = ["ci"]`: a pull request's plan job runs with a read-only
token (#259), and a rerun of the Homebrew publish skips an empty commit. On a pull
request that workflow only plans; on a tag it builds `aarch64-apple-darwin` and
`x86_64-unknown-linux-gnu`, attaches the archives and a shell installer per binary to a GitHub
release, pushes one Homebrew formula per binary to `StGerman/homebrew-tap`, runs `cargo publish
--workspace` (`publish-crates.yml`, since dist has no crates.io publisher), and puts GitHub's
generated notes above the install instructions (`release-notes.yml`), so there is no
CHANGELOG.md. `tests/release.rs` fails if a pull request could reach any job past the plan, if either hand
edit is lost, or if `dist plan` stops listing both binaries' archives, shell installers and
formulae (CI's `release-plan` job, which installs dist), and
the CI `package` job runs `cargo package --workspace`, so missing crates.io metadata fails a pull
request rather than a tag. The `crew` library is published only because `crewd` is, and makes
no API promise until `crew-core` (#84).

**`crewd init`** ([src/init/](../src/init/)) registers the operator's own GitHub App through the
App Manifest flow (#65): a loopback page posts the manifest to GitHub, the operator clicks
*Create*, GitHub redirects back with a single-use code, and `init` exchanges it for the App's
id and private key, then sends the browser on to *Install* and reads the installation id back
over the App's own JWT. It writes `~/.crewd/github-app.pem` (600) and `~/.crewd/github-app.toml`
(`app_id`, `installation_id`, `private_key_path`) in a 700 directory — the file #64's
`tracker.github_app` names — and never overwrites either file; with the settings file present,
a re-run skips registration. Run inside a git clone, it then leaves a deployment (#247,
[src/init/deploy.rs](../src/init/deploy.rs)): it checks the App's installation covers the
`origin` repository, asks whether agents work issues and whether it opens pull requests (flags
answer both, and with no terminal an unanswered one stops it), and writes
`~/.crewd/<owner>-<repo>/crewd.toml` from a template in the binary, not from `crew.github.toml`.
Then it creates the dispatch label and each `state:` label that config names through the App's
own `Credentials`. A config or label that exists is kept and reported, never rewritten.
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
reviewers on the current head *and read back whether they attached*, read CI, read the review threads — and the
reviews' summaries and the conversation, since a reviewer can leave a finding on no line (#126,
#263). Every comment is worked whoever wrote it, except crewd itself: its verdict comments land
in the same conversation, so the login the forge posts as is learned when delivery is attached
(`GET /app`'s slug as `<slug>[bot]` for an App, `GET /user` for a token), and startup fails if it
cannot be. A summary on the current head in the `COMMENTED` or `CHANGES_REQUESTED` state that
says more than "Findings: None" (or "0") and Copilot's template (headings, tags, the "Review
effort" line, section labels; #201), or than the overview sentence under Copilot's status
heading, whatever its emoji, before that count (#234, #280), is handed back whole as one more
comment keyed `review-<id>`:
no parser for its sections, whose format is nobody's contract, and noise costs one `rejected`
verdict. A conversation comment is handed back keyed `conversation-<id>`, whenever it was written:
GitHub records no time a head was pushed, a commit's date is not one, and a cutoff would drop a
comment nobody answered (#265). Neither has a thread, so a verdict on either is a pull request comment
quoting it. The round prompt names each comment's kind and author. A red CI or
an open comment sends the issue back to an agent by the same path a `Continue` takes — a retry
due now, the session resumed, and the failure in the prompt as `Feedback::Ci` or
`Feedback::Review` — which is the literal form of "a red gate is a `Continue`, never a `Done`".
A pull request the provider reports unable to merge is not waited on at all, since GitHub runs no
CI on it: delivery charges a round and re-gates the branch as if its `Done` were new, so the
gate's rebase or merge and its conflict rule decide what follows (#159).
A pull request is ready only once each of `delivery.reviewers` has reviewed its current head
(#222): green CI and no open comment say nothing about one nobody has looked at, and GitHub's
automatic Copilot review never runs on a pull request the app opens. A head the last request
was not made on is requested again, a `[bot]` login through GraphQL `requestReviewsByLogin`
and verified through `reviewRequests`, since REST drops a bot and answers success
(GETT-174120). A review that has not arrived within `delivery.review_timeout_ms` is handed off
naming the reviewer, timed like the CI wait: on `Mono`, resumed across a restart from the
wall-clock start on the row.
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
reports `Done`, before a rebase by the gate rewrites the shas the agent named.

Four things there are load-bearing. Every hand-back is a *round*, bounded per pull request
(`max_rounds_per_pr`) and per issue (`max_rounds_per_issue`), and the per-issue count never
resets — not for a new run and not for a new pull request — because a reviewer that comments
on every push, answered by an agent that pushes, is a loop with no bound of its own, and one
that reset with the pull request would bound nothing (the same gap `max_calls_per_issue`
closes for the broker). At either bound the pull request is handed to the operator with the
outstanding items named. A review request is followed by a read of the outstanding review
requests (`Forge::review_requests`, GraphQL `reviewRequests` on GitHub) and the reviews, because
GitHub answers a REST request for a bot reviewer with `200` and attaches nobody (GETT-174120); a person or team request that verifiably attached nobody is a handoff with that
reason on the issue's row, not a success. A bot's request is waited for like an attached one, with the
issue's row telling the operator to request it: an App has no Copilot seat, so crew-bot's
request for Copilot is accepted and attaches nobody, and only a person can make it (#252). `Forge` has no `merge` method, and must not grow one — merging is
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

The lease is a ref crewd owns, `refs/crew/lease/<branch>`, written by `Publisher::sync` and by
`publish` and by nothing else (#163). The bare `--force-with-lease` took it from
`refs/remotes/<remote>/<branch>`, which any fetch in `workspace.repo` moves onto commits the
worktree never had, so the push replaced an operator's merge instead of refusing. `sync` runs
before every agent run (`launch`), every re-gate and every delivery push, and before the gate of
a run that reported `Done`, because an agent can push its own branch and the lease would not
name that head once the gate rebased it (#269); a fetched head the branch's own reflog
reaches was held by the worktree too, so a pre-gate sync that failed does not bring the defect
back. It fetches the branch.
When the fetched head is the one the lease already names, the remote has not moved since the
last sync or publish. A divergence is a local rewrite of commits the worktree already held —
the gate rebasing onto a base that moved — so the worktree
is left alone and the push replaces that head (#227). Any other head is fast-forwarded, or
merged when the worktree has commits of its own. It never
rebases, which would rewrite commits a reviewer saw and drop the operator's merge. A conflict is
aborted and treated like a gate conflict (#111): a brief when every path is agent-resolvable,
`Blocked` naming the paths otherwise, with the brief queued for the run an unblock dispatches.
A re-gate also compares the fetched head with the one the unmergeable read was taken at, and
reads the pull request again when they differ, because the push that moved it may be the one
that resolved the conflict.

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

## Deployments

A **deployment** is one directory holding a config, its `crew.db`, its log and its `workspaces/`,
pointing at the repository's clone by absolute path. Everything a running crewd owns is in that
directory, so two deployments on one host share nothing but the clone (and must still pick their
own `api.bind`/`api.mcp_bind`). Writing one per deployment under `~/.crewd/` is #247; `crewd
init` today writes only the GitHub App files.

What makes the directory the unit rather than the shell that started crewd is `Config::parse`
(#251): every path key (`workspace.repo`, `workspace.root`, `transcripts.root`,
`tracker.github_app`, `forge.github_app`, `tracker.jira.credentials`) has `~` expanded and is then
resolved against the config file's directory, and the store defaults to `crew.db` beside the
config (`config::store_path`). Resolved against the current directory instead, `crewd --config
~/.crewd/acme-api/crewd.toml` started from `$HOME` opened a fresh, empty store there and took
`$HOME` for the clone. `CREW_DB` still overrides the store and, like any path given in the
environment or on the command line, means what it says relative to the shell that set it. This
repository's own run is unchanged: `crew.github.toml` sits at the root it is started from.

## Running the daemon inside a worktree

One thing the overrides do not cover: running the daemon from *inside* a worktree — which is
what a dispatched agent's cwd is. `GitWorktreeWorkspace::new` refuses that at startup, because
`workspace.repo = "."` there is a linked worktree and the worktrees a run would create register
in the top-level checkout's shared `.git`, where the orchestrator owning it never recorded them
(#29). The error names the way out: point `workspace.repo` at a throwaway clone and
`workspace.root` beside it. Setting `CREW_DB` alone does not help — the litter was never
in the store.
