# The defects crewd fixed in Symphony's SPEC.md

crewd began as a reimplementation of the coordination layer in
[openai/symphony](https://github.com/openai/symphony)'s `SPEC.md`. A review of that spec before
the first line of crewd found six defects, and the first commit (d4a6b10) closed each of them
with a test that fails without the fix. This page puts each one in front of a reader who runs a
port of the spec: the spec's text, what it does to a running system, what crewd does instead,
and the test that holds it.

Every quote is from `SPEC.md` at
[`8001b52`](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md)
("Draft v1", 2026-08-12), the newest revision of the file when this page was written. Each
passage reads the same in every revision of `SPEC.md` back to the first,
[`fa75ec6`](https://github.com/openai/symphony/blob/fa75ec68c23f/SPEC.md), so no defect here has
been fixed upstream. A section says so, with the commit, if one is.

The guard tests are rows of [invariants.md](invariants.md). Run one with
`cargo test <name>`.

## 1. Backoff that can overflow

**The spec**, §8.4 Retry and Backoff
([L799-L801](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L799-L801)):

> - Failure-driven retries use `delay = min(10000 * 2^(attempt - 1), agent.max_retry_backoff_ms)`.
> - Power is capped by the configured max retry backoff (default `300000` / 5m).

**The failure.** The cap applies to the product, after the power is computed; the exponent is
never bounded. `attempt` only climbs on the failure path, and §16.6 schedules each failed retry
at `retry_entry.attempt + 1`. At the 5-minute cap an issue that keeps failing reaches attempt 51
in about four hours, and `10000 * 2^50` no longer fits in a signed 64-bit integer; `2^63` itself
does not fit at attempt 64. A port in a language with fixed-width integers either panics there
or, in a release build, wraps: the longest backoff turns into the shortest, or into zero, and a
failing issue is retried in a tight loop exactly when something is already wrong. A port on
arbitrary-precision integers (the reference Elixir one) does not overflow, which is why the
formula looks safe when read against it.

**crewd.** `backoff_ms` in [src/sched/retry.rs](../src/sched/retry.rs) caps the *exponent*
(`EXP_CAP = 16`) before shifting, then multiplies with saturating arithmetic and applies the
configured cap. The delay is the cap for every attempt from the sixth to `u32::MAX`.

**Invariant:** "Backoff cannot overflow or collapse". **Guard test:**
`backoff_never_overflows_or_collapses_at_any_attempt_count`, which walks attempts 0 to 64 and the
`u32` boundaries and asserts the delay is never above the cap and never below it after the sixth.

## 2. The 1s continuation respawn loop

**The spec**, §7.1
([L672-L674](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L672-L674))
and §8.4
([L799](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L799)):

> - Once the worker exits normally, the orchestrator still schedules a short continuation retry
>   (about 1 second) so it can re-check whether the issue remains active and needs another worker
>   session.

> - Normal continuation retries after a clean worker exit use a short fixed delay of `1000` ms.

**The failure.** A clean exit means "maybe continue", and the only thing that ends the cycle is
the tracker state leaving the active set. Nothing in the run itself can say it is finished:
an agent that did all it could, or one that is stuck and needs a human, exits cleanly, and the
issue is still `In Progress`, so a new session starts one second later, forever. The spec's
`agent.max_turns` bounds the turns inside one worker session (L453), not across sessions, so it
bounds nothing here. At ten concurrent issues the refresh before each respawn alone is about 600
tracker requests a minute, enough to rate-limit the orchestrator off most providers, and every
respawn spends model tokens.

**crewd** closes the three paths into that loop separately, because each holds while the others
fail:

- The run returns an explicit verdict, `Outcome` in [src/model.rs](../src/model.rs): `Done`,
  `Continue`, `Blocked` or `Failed`. Only `Continue` asks for another session.
- A `Continue` waits `continuation_delay_ms` ([src/sched/retry.rs](../src/sched/retry.rs)): 5s,
  then 30s, then the poll interval, while nothing observable changes, resetting when something
  does.
- `agent.max_turns_per_issue` counts turns across every session of the issue and quarantines it
  when the budget is spent, so even a well-behaved `Continue` cannot run forever.
- A run that ends `Done` or `Blocked` parks the issue at its current state (`parked_state`), so
  the ticket still reading `In Progress` does not pick it straight back up. The park lifts only
  when the ticket moves.

**Invariants:** "No 1s continuation respawn loop" and "A finished issue is not re-dispatched".
**Guard tests:** `continuation_backs_off_instead_of_respawning_every_second` (the row's guard:
the second unmoved continuation waits 30s, not 1s), `the_per_issue_turn_budget_stops_an_endless_continuation`
and `a_finished_issue_is_not_re_dispatched_while_its_state_is_unchanged`, all in
[tests/scheduler.rs](../tests/scheduler.rs).

## 3. Permanent failures retried forever

**The spec**, §14.2 Recovery Behavior
([L1676-L1677](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L1676-L1677))
and §11.4
([L1301-L1302](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L1301-L1302)):

> - Worker failures:
>   - Convert to retries with exponential backoff.

> Adapters MAY add `retryable`, `retry_after_ms`, provider status, and
> provider-specific detail, but the orchestrator only relies on success vs. failure.

**The failure.** Every worker failure is retried, and nothing distinguishes one that can succeed
next time from one that cannot. A prompt template naming an unknown variable, a model name the
agent CLI refuses, a workspace path outside the root, revoked credentials: each fails the same
way on every attempt. With no attempt limit in §8.4 the issue is retried every 5 minutes for as
long as the service runs, each attempt preparing a workspace, spawning an agent and often
spending tokens, and the failure looks to the operator like a flaky retry rather than a broken
issue.

**crewd.** `ErrorClass` ([src/model.rs](../src/model.rs)) partitions every failure the
scheduler can see, and `ErrorClass::retryable()` is an exhaustive `match`, so a new class does
not compile until it is placed on one side. A non-retryable class such as `TemplateRender`,
`ConfigInvalid`, `ModelNotFound`, `WorkspaceOutsideRoot` or `AuthFailed` quarantines the issue
on its first occurrence, with no retry row, and it stays out of rotation until an operator
clears it. A failure that disables a whole worker rather than one issue, a missing binary or an
exhausted account, pauses that worker instead and charges the issue nothing.

**Invariant:** "Permanent failures stop". **Guard test:**
`a_permanent_failure_quarantines_immediately_rather_than_retrying_forever`
([tests/scheduler.rs](../tests/scheduler.rs)), which fails a run with `TemplateRender` and
asserts the issue is quarantined, has no retry, and is still not running ten minutes later.

## 4. A workspace removed before its worker is confirmed stopped

**The spec**, §8.5 Active Run Reconciliation
([L835](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L835)):

> - If tracker state is terminal: terminate worker and clean workspace.

**The failure.** The two steps carry no ordering constraint between them, and terminating a
process is a request, not an event: the signal is sent and the call returns. A port that issues
the kill and then deletes the directory removes the workspace from under an agent that is still
running in it, mid-write. The agent's next tool call fails against a vanished cwd, or recreates
part of the tree on its way out, leaving a half-populated directory that the next dispatch
reuses as if it were a warm workspace. A `before_remove` hook runs at the same moment, against a
tree still being written.

**crewd.** `Worker::kill(grace)` ([src/worker/mod.rs](../src/worker/mod.rs)) is specified not to
return until the run is confirmed stopped: the Claude worker sends `SIGTERM` to the process
group, waits up to the grace period for the process to be reaped, then sends `SIGKILL` and waits
again. `terminate` in
[src/sched/mod.rs](../src/sched/mod.rs) calls it, then records the run, and only after that
calls `Workspace::remove`. The same rule covers a handoff gate running in the worktree.

**Invariant:** "No workspace is deleted under a live agent". **Guard test:**
`a_ticket_moving_to_terminal_stops_the_run_and_cleans_up`
([tests/scheduler.rs](../tests/scheduler.rs)), which moves a running issue to `Done` and asserts
the run is stopped and its workspace is gone after the same tick.
`killing_a_process_that_ignores_sigterm_forces_it_and_it_is_actually_gone`
([src/worker/claude.rs](../src/worker/claude.rs)) holds the other half against a real process
that traps `SIGTERM`: `kill` does not return until that process is gone.

## 5. One tracker refresh miss destroying in-flight work

**The spec**, §16.3 Reconcile Active Runs
([L1887-L1888](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L1887-L1888)):

> ```text
>   for missing_id in running_ids - returned_ids:
>     state = terminate_running_issue(state, missing_id, cleanup_workspace=false)
> ```

**The failure.** The spec does guard a refresh that *fails* ("If state refresh fails, keep
workers running"), but treats a refresh that succeeds and omits an issue as proof the issue is
gone. Omission is ambiguous: tracker search indexes are eventually consistent, a filtered query
can lag an edit, and a page boundary can shift under a concurrent update. On the first such
blip the running agent is killed mid-turn. Its workspace survives, but the turn in progress and
any uncommitted state in the session are lost, and the next poll, which sees the issue again,
dispatches it from scratch.

**crewd.** `refresh_running` ([src/sched/mod.rs](../src/sched/mod.rs)) counts consecutive
omissions per issue (`Store::bump_miss`) and stops a run only once the count reaches
`agent.refresh_miss_grace` (default 2). An issue that reappears resets its count.

**Invariant:** "One tracker blip cannot kill a run". **Guard test:**
`one_invisible_refresh_is_survivable_but_two_are_not`
([tests/scheduler.rs](../tests/scheduler.rs)), which hides a running issue for one refresh, shows
it, then hides it for two, and asserts the run survives the first and stops after the second.

## 6. Workspace root containment checked only at launch

**The spec**, §9.5 Safety Invariants
([L937-L941](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L937-L941)),
§17.2
([L2098-L2099](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L2098-L2099))
and §8.6
([L846](https://github.com/openai/symphony/blob/8001b52e3062495a16e520e4ceaf8f9de868c4d0/SPEC.md#L846)):

> Invariant 2: Workspace path MUST stay inside workspace root.
>
> - Normalize both paths to absolute.
> - Require `workspace_path` to have `workspace_root` as a prefix directory.
> - Reject any path outside the workspace root.

> - Workspace path sanitization, stable original-identifier-hash collision resistance, and root
>   containment invariants are enforced before agent launch

> 2. For each returned issue identifier, remove the corresponding workspace directory.

**The failure.** The only place the spec says *when* containment is checked is before launch.
Removal, at startup cleanup (§8.6), on a terminal transition (§8.5) and on a retry that observes
one (§8.4), computes the same path from a tracker-supplied identifier and deletes it, with no
check named. Removal is the destructive operation: launching in the wrong directory runs an
agent somewhere unexpected, while removing the wrong directory recursively deletes it. Any path
the launch check would have refused (an identifier that sanitization mishandles, a symlink
inside the root, a root changed by a config reload between launch and cleanup) reaches
`rm -rf` unchecked.

**crewd.** Every path leaving a `Workspace` implementation passes `guard_within`
([src/workspace.rs](../src/workspace.rs)), which canonicalises the parent and refuses anything
not under the canonical root. `prepare` and `remove` both call it, in `DirWorkspace` and in
`GitWorktreeWorkspace`, and so do delivery's operations on a worktree. A refused path is
`ErrorClass::WorkspaceOutsideRoot`, which is permanent (section 3).

**Invariant:** "A workspace path cannot escape its root". **Guard test:**
`hostile_identifiers_stay_inside_the_root` ([src/workspace.rs](../src/workspace.rs)), with
`hostile_identifiers_stay_inside_the_root_for_git_worktrees_too` for git worktrees, which feed
`../../etc`, `/etc/passwd`, `..` and `a/../../b` as identifiers and assert each workspace lands
exactly one level under the root. Both drive `prepare`; `remove` is held by sharing the same
`guard_within`, not by a test of its own.
