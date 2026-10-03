# Ops API v1

For: a program that drives crewd over HTTP and needs to know what it can rely on. This is the
promise `/api/v1` makes. The reasoning behind the API's design (why it holds no `Store`, why the
HTTP is hand-rolled, how `:identifier` resolves) is in
[architecture.md](architecture.md) (the ops API paragraphs under Subsystems); this file is the contract.

## The rule

v1 changes only by addition.

- A new field may appear in any object below. It is optional on the wire and carries
  `#[serde(default)]`, so a client built against an older daemon, or a daemon older than the
  client, still parses. A client must ignore keys it does not know.
- A field is never removed, renamed, given another JSON type, or given another meaning. Nor is
  a spelling of `phase` or `reason`. A change that needs any of those opens `/api/v2` beside
  v1, and v1 keeps answering as it did.
- The spellings of `phase` and `reason` are closed sets. A new value is not an addition: the
  `Snapshot` and `Row` types in `libcrew`, which `crewctl` reads with, refuse a spelling they do
  not know, so a new one opens `/api/v2` like a rename does.

`the_v1_snapshot_shape_only_grows` in [libcrew/src/snapshot.rs](../libcrew/src/snapshot.rs)
enforces the field half of this: it pins every key path of a fully populated `Snapshot`, with its
JSON type, and every enum spelling, as an `insta` snapshot. Renaming or removing a field fails it;
adding one changes the snapshot visibly, and that diff is where a reviewer asks whether the change
is an addition. The meanings below are kept by review, not by that test.

Out of this promise: the store schema, the MCP tools' argument shapes beyond what these routes
already promise, the text of an `error` or `detail` message, and the order of `rows`.

## The marker

Every response, on every status, carries `X-Crew-Ops-Api: 1`. A response without it did not come
from this API, whatever its status and body say; `crewctl` refuses it. The value is the API's
major version: `/api/v2` would answer with `2`.

## Routes

Off unless `[api] enabled` or `--api <addr>`, and loopback unless `api.allow_public`.

| Route | Answers |
|---|---|
| `GET /api/v1/snapshot` | `200` with a `Snapshot` |
| `GET /api/v1/issues/:identifier` | `200` with one `Row`; `404` when nothing matches; `409` when the identifier names more than one issue |
| `POST /api/v1/refresh` | runs one tick, then `200` with the `Snapshot` after it; `500` when the tick failed, `503` when the scheduler stopped first |
| `POST /api/v1/unquarantine/:identifier` | `200` with an action answer; `404`/`409` as for `issues`; `500` when the action failed, `503` when the scheduler stopped first |
| `POST /api/v1/unblock/:identifier` | `200` with an action answer; `404`/`409` as for `issues`; `500` when the action failed, `503` when the scheduler stopped first |

`:identifier` is a dispatch id (`issue_id`), tried first and exactly, else a tracker identifier.
A wrong method on a known path is `405` with an `Allow` header; any other path is `404`.

Every error body is `{"error": string}`. A `409` adds `issue_ids`, an array of strings: the
dispatch ids to retry with.

An action answer is `{"issue_id", "identifier", "cleared", "detail"}`: the issue it resolved to,
`cleared` a bool saying whether the quarantine or park was there to clear, and `detail` a
sentence for a person. A `200` with `cleared: false` is not an error: there was nothing to clear.

## Snapshot

Times are integers: wall-clock milliseconds since the Unix epoch where named `_at`, durations in
milliseconds where named `_ms`. "Optional" means the key is always present and may be `null`.

| Field | Type | Meaning |
|---|---|---|
| `generated_at` | integer | When the scheduler published this snapshot |
| `rows` | array of `Row` | Every issue the scheduler holds a view of |
| `running` | integer | Runs holding a slot, gating ones included |
| `reserved` | integer | Slots held by continuations waiting out their delay; `running + reserved` is what is compared with `limit` |
| `limit` | integer | The concurrency limit |
| `retrying` | integer | Issues waiting on a retry |
| `quarantined` | integer | Issues quarantined |
| `tokens` | `TokenUsage` | Summed over every run that reported a total |
| `uncounted_runs` | integer | Finished runs that reported no total, so `tokens` is a lower bound |
| `ticks` | integer | Ticks run since startup |
| `last_tick_at` | optional integer | When the last tick started; `null` before the first, since a snapshot is published before it |
| `last_error` | optional string | A failure the last tick recorded and carried on past, such as a failed preflight or tracker read; a tick that stopped on an error leaves it unset, and `POST /refresh` answers that one with `500` |
| `rate_limit_pauses` | array of `RateLimitPause` | Workers paused for an account-wide rate limit, in dispatch order |
| `rate_limit_pause` | optional `RateLimitPause` | The first of `rate_limit_pauses`, kept for clients from before the list |
| `missing_binaries` | array of `HaltedWorker` | Workers that take nothing until the daemon restarts; the key predates `reason` |

## Row

| Field | Type | Meaning |
|---|---|---|
| `issue_id` | string | The dispatch id, unique |
| `identifier` | string | The tracker's identifier, not guaranteed unique |
| `title` | string | The issue's title; empty until the daemon has read the issue since it started, as in the snapshot published before the first tick |
| `url` | optional string | The issue in the tracker; `null` on the same terms as an empty `title` |
| `tracker_state` | string | The tracker's state for it; empty on the same terms as `title` |
| `phase` | string | The claim state: `queued`, `running`, `retry`, `quarantine` or `released` |
| `attempt` | integer | Attempts charged to it |
| `turns` | integer | Turns of the running run; with none running, the issue's total across its runs |
| `tokens` | optional `TokenUsage` | The current run's totals, once it reported them |
| `age_ms` | integer | Time since its current run started; `0` when none is running |
| `retry_in_ms` | optional integer | Time until its retry is due; negative once overdue |
| `holds_slot` | bool | A continuation holding its concurrency slot through its delay |
| `quarantined` | bool | Whether it is quarantined |
| `last_error` | optional string | Its last failure, or why it is parked |
| `last_event` | optional string | The last event its run reported; while the run is gating, `gate: <step>` naming the gate's current step |
| `workspace` | optional string | Its worktree path while a run or gate holds it; `null` otherwise, even if the worktree is still on disk |
| `branch` | optional string | The branch its latest dispatch checked out; `null` once cleanup deleted a branch that carried nothing |
| `runs` | array of `RunRecord` | Its most recent runs, newest first |
| `transcript` | optional string | The latest run's transcript path |
| `delivery` | optional `DeliveryView` | Its pull request's progress, once a run reported done with delivery on |
| `worker` | optional string | The worker running it, else the one that ran its latest run |

## RunRecord

| Field | Type | Meaning |
|---|---|---|
| `run_id` | string | The run's id |
| `issue_id` | string | The issue it ran for |
| `started_at` | integer | When it started |
| `ended_at` | optional integer | When it ended; `null` while in flight |
| `outcome` | optional string | How it ended; `null` while in flight |
| `session_id` | optional string | The agent session it ran in |
| `turns` | integer | Turns taken, checkpointed while in flight |
| `in_tok` | optional integer | Prompt-side tokens; `null` when the CLI reported no total, which is unknown, not zero |
| `out_tok` | optional integer | Output tokens, `null` on the same terms |
| `transcript` | optional string | Its raw event stream's path |
| `model` | optional string | The `--model` it was dispatched with |
| `effort` | optional string | The effort it was dispatched with |
| `worker` | optional string | The worker that ran it |

## DeliveryView

| Field | Type | Meaning |
|---|---|---|
| `stage` | string | Where the branch is on its way to a mergeable pull request |
| `pr_number` | optional integer | The pull request's number |
| `pr_url` | optional string | The pull request's URL |
| `base` | optional string | The branch it merges into |
| `rounds_pr` | integer | Fix rounds handed back to an agent on this pull request |
| `rounds_issue` | integer | Fix rounds handed back on this issue, across its pull requests |
| `review_error` | optional string | Why the review could not be read |
| `handoff_reason` | optional string | Why it was handed to a human |

## TokenUsage

| Field | Type | Meaning |
|---|---|---|
| `input` | integer | Prompt-side tokens: fresh input, cache creation and cache reads |
| `output` | integer | Output tokens |

## RateLimitPause

| Field | Type | Meaning |
|---|---|---|
| `worker` | string | The paused worker; empty from a daemon older than per-worker pauses |
| `kind` | string | The window the CLI named, such as `five_hour`; any string |
| `resets_at` | integer | When dispatch to that worker resumes |

## HaltedWorker

| Field | Type | Meaning |
|---|---|---|
| `worker` | string | The halted worker |
| `binary` | string | The path it was spawned with |
| `reason` | string | `binary_not_found` or `account_exhausted` |
