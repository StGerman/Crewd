# Invariants

Moved verbatim from CLAUDE.md by #92 and grouped by area, so rows added by parallel pull
requests land in different places. **Read this before changing `src/sched/`, `src/store/`,
`src/broker/`, `src/gate/` or delivery.** Add a row in the area it belongs to, with the guard
test that fails without its mechanism.

Each of these closes a defect found in the original spec. The later rows came instead from
dogfooding this orchestrator against its own backlog: the first review, the operator surface
built over the same state, issue #1's acceptance criterion made executable, the first live
dispatch and the review that followed it, the client put in front of that operator surface,
making a finished run diagnosable, a closed ticket whose worktree outlived it, an
orchestrator that ran inside its own worktree, the agent supervising the daemon getting
the same surface as data, two branches that were each green alone and broken together, and
the review of the handoff path that found it handing off, retrying or settling in six cases
where it should not.

Every row has a test that fails without its mechanism. Several of those tests only fail in the
exact scenario they were written for, so a regression here can pass a casual `cargo test`
reading — check that the named test is still meaningful, not just still green.

## Scheduling and dispatch

| Invariant | Mechanism | Guard test |
|---|---|---|
| Backoff cannot overflow or collapse | cap the *exponent* (`EXP_CAP = 16`), not just the product | `backoff_never_overflows_or_collapses_at_any_attempt_count` |
| No 1s continuation respawn loop | explicit `Outcome` verdict + escalating delay + `max_turns_per_issue` | `continuation_backs_off_instead_of_respawning_every_second` |
| A finished issue is not re-dispatched | `parked_state`, cleared only when the ticket actually moves | `a_finished_issue_is_not_re_dispatched_while_its_state_is_unchanged` |
| Permanent failures stop | `ErrorClass::retryable()` → immediate quarantine | `a_permanent_failure_quarantines_immediately_rather_than_retrying_forever` |
| One tracker blip cannot kill a run | `refresh_miss_grace`, reset on reappearance | `one_invisible_refresh_is_survivable_but_two_are_not` |
| A dead session cannot strand an issue | drop the session name after a run with zero turns | `a_run_that_took_no_turns_is_not_retried_into_the_same_conversation` |
| A resumed session reads a description edited since it last saw one | `launch` swaps the body's `blake3` into `issue_state.session_body` (v12) and sets `Spawn::body_changed` on a resume whose hash differs or was never recorded; the continuation prompt then carries the body under a "changed since your last session" heading (#109) | `a_description_edited_after_the_session_started_reaches_the_resumed_session` (in `tests/scheduler.rs` and in the worker's snapshots) |
| A run resumed after a rebase conflict is told the conflict, not a turn budget | `gate_outcome` queues `Feedback::Conflict { base, base_sha, paths }` in `issue_state.pending_feedback`; `launch` takes it ahead of delivery's feedback and the retry reason, and the continuation prompt opens by saying the gate stopped the handoff (#109) | `a_resume_after_a_rebase_conflict_is_handed_the_conflict_rather_than_a_turn_budget`, `a_resume_after_a_rebase_conflict_names_the_conflict_rather_than_a_turn_budget` |
| A hard kill cannot strand a claim | startup `recover()`: a claim with no live run is stale, because `running` cannot cross a process boundary | `a_claim_stranded_by_a_hard_kill_is_recovered_at_the_next_startup` |
| A hard kill cannot zero an in-flight run's progress | `observe_progress` checkpoints `run.turns` once per tick when the count moved, and `close_open_runs` keeps it and charges it to `cumulative_turns` in one transaction | `a_run_interrupted_by_a_hard_kill_reports_its_last_known_turn_count_after_restart` |
| A continuation keeps its place in the milestone order | a retry queued by a `Continue` verdict — the agent's or the gate's — records the state whose slot it holds (`retry.reserved_state`, v10), and `global_slots`/`state_slots` count it for the length of its delay; a failure backoff, a delivery hand-back and a rate-limit release reserve nothing; the reservation lives on the retry row, so every path deleting the row ends it, and `dispatch_due_retries` re-reads a waiting reservation each tick so a ticket closed or unlabelled during the delay frees its slot at once — while an id the tracker merely omits keeps its reservation until due, one omission being the blip `refresh_miss_grace` exists for; a due reservation counts only the reservations ahead of it in due order, so ones oversubscribing a limit after a state change dispatch earliest-first instead of deferring each other forever; the snapshot publishes `reserved` and `Row::holds_slot` (#86) | `a_continuing_issue_keeps_its_slot_through_its_continuation_delay`, `a_continuation_holds_its_slot_in_its_own_state_limit`, `a_failure_backoff_does_not_hold_a_slot`, `a_continuation_whose_ticket_went_terminal_releases_its_reserved_slot`, `a_waiting_continuation_survives_one_tracker_omission_with_its_slot`, `reservations_oversubscribing_a_state_limit_dispatch_in_due_order_rather_than_deadlocking` |
| An account-wide rate limit is not any one issue's failure | a rejected `rate_limit_event` releases the claim (`Store::release_for_rate_limit`, not `release`) without charging an attempt or the identical-failure streak, and pauses dispatch itself until `resets_at` rather than scheduling a per-issue retry | `a_rate_limit_pauses_dispatch_rather_than_quarantining_the_issues_it_interrupted` |
| A rate limit cannot stop dispatch on a clock the host disagrees with | a `resets_at` that is missing or already behind the clock falls through to the ordinary `Failed` path instead of pausing on a value that would never lift | `a_rate_limit_with_no_usable_resets_at_degrades_to_ordinary_backoff` |
| A misspelled `tracker.kind` or `worker.kind` cannot silently run the fake | both parse into `TrackerKind`/`WorkerKind` in `preflight`, naming the value and the supported set, and `main.rs` matches on the enum with no `else` fallthrough; an empty `worker.kind` is `fake` on purpose | `a_misspelled_tracker_kind_is_rejected_rather_than_running_the_demo`, `a_misspelled_worker_kind_is_rejected_rather_than_running_the_fake` |

## Worker

| Invariant | Mechanism | Guard test |
|---|---|---|
| A killed run cannot record a fabricated cost | totals are read only from the `result` event; a run that never emits one stores NULL, not a per-event sum | `a_run_that_dies_before_its_result_event_reports_no_token_total` |
| A run names the model that did its work, not today's setting | the scheduler records `Worker::model()` — the value the worker builds `--model`/`--effort` from — on the run row at `start_run`, and never updates it | `a_run_records_the_model_it_was_dispatched_with_rather_than_the_current_default` |
| A model the CLI refuses is not silently replaced by the default | `model_not_found` on the stream is `ErrorClass::ModelNotFound`, permanent; an unknown `effort`, which the CLI would ignore with a warning, fails config load | `a_model_the_cli_refuses_quarantines_the_issue_instead_of_retrying_it`, `a_model_setting_the_cli_would_silently_ignore_is_refused_at_load` |

## Tracker and identity

| Invariant | Mechanism | Guard test |
|---|---|---|
| A label's casing cannot make an issue undispatchable | `GithubTracker::to_issue` trims, lowercases, drops blank and dedupes labels — the shape `Config::normalize` gives `required_labels` — so `routable`'s plain equality holds; `set_state` reads the raw labels instead, so writing them back never renames the operator's own (#70) | `labels_are_normalized_at_the_adapter_boundary`, `set_state_writes_labels_back_in_their_original_casing` |
| An installation token cannot expire under a long-running daemon | the tracker, forge and push ask a `Credentials` source per request; `GithubApp` caches against the injected clock and re-mints `REFRESH_MARGIN_MS` before expiry; a 401 on an App token — or a push git reports refused for authentication — re-mints and repeats the refused request exactly once, so an early revocation neither fails a poll nor hands delivery off (#64) | `a_daemon_up_past_the_installation_tokens_lifetime_keeps_polling_without_a_401`, `an_installation_token_is_re_minted_before_it_expires_rather_than_served_stale`, `a_token_revoked_before_its_expiry_is_replaced_and_the_pull_request_still_opens`, `a_credential_refused_twice_is_permanent_after_exactly_one_retry`, `a_push_refused_for_authentication_is_retried_once_on_a_fresh_token`, `an_authentication_refusal_that_survived_its_retry_is_permanent` |
| The push credential never reaches the agent sharing its worktree | `publish` hands git a `credential-store` file in a private temp dir, deleted after the push, behind an empty `credential.helper` that clears the operator's own — never a URL, `extraheader` or env var | `a_push_credential_reaches_git_without_touching_config_argv_or_a_lasting_file`, `the_default_allowlist_names_no_credential_variable` |
| A teammate's assignment does not hand an issue to an agent | with `dispatch_label` set, `DispatchRule` makes the label the whole signal; unset, any assignee, as before | `with_a_dispatch_label_only_the_label_makes_an_issue_dispatchable`, `without_a_dispatch_label_any_assignee_is_still_the_signal` |
| A half-configured App is refused by name, not met as a 401 | `Config::load` runs `check_github_app` once — never per tick — loading `tracker.github_app` and its key and naming each missing field or unreadable file | `a_half_configured_github_app_is_refused_naming_the_missing_piece` |

## Workspace and cleanup

| Invariant | Mechanism | Guard test |
|---|---|---|
| No workspace is deleted under a live agent | `kill(grace)` blocks until confirmed stopped, *then* `remove` | `a_ticket_moving_to_terminal_stops_the_run_and_cleans_up` |
| A workspace path cannot escape its root | `guard()` on **both** `prepare` and `remove` | `hostile_identifiers_stay_inside_the_root` |
| Cleanup cannot discard an agent's commits | `branch -d` (not `-D`) on remove; attach, not `-B`, on reuse | `a_branch_holding_committed_work_outlives_the_worktree_it_is_removed_with` |
| Cleanup cannot discard an agent's *uncommitted* work | `remove` snapshots a dirty tree to a new ref under `refs/crew/wip/<issue key>/` (never the branch, never overwriting an earlier snapshot) before deleting it, failing closed; `prepare` reports every such ref and the next prompt names them | `a_worktree_removed_with_uncommitted_changes_leaves_them_recoverable_from_its_wip_ref`, `a_run_killed_with_uncommitted_changes_has_them_recoverable_after_its_workspace_is_removed` |
| The branch an operator is sent to is the one git checked out | `Workspace::branch_for` is the same naming function `prepare` uses, not a second spelling of it | `the_branch_the_snapshot_publishes_is_the_one_prepare_checks_out` |
| The published branch never names a ref that is gone or was never this run's | `Store::set_branch` persists what `prepare` returned and is cleared exactly when `Removed::branch_deleted` says cleanup deleted it — never recomputed from `identifier`, which `Store::ensure` can rename after dispatch | `the_published_branch_is_the_one_prepare_recorded_not_one_recomputed_from_the_current_identifier` |
| A parked run's worktree is reclaimed once its ticket closes | `sweep_parked` re-reads parked ids on a bounded cadence, unparks what it cleans, and clears the published branch when cleanup deleted the ref | `a_parked_issue_that_is_later_closed_has_its_workspace_reclaimed_without_a_restart`, `sweeping_parked_issues_costs_tracker_traffic_bounded_by_the_interval_not_by_ticks`, `a_sweep_that_deletes_a_branch_clears_the_name_the_snapshot_publishes` |
| One orchestrator cannot nest its worktrees inside another's | `GitWorktreeWorkspace::new` refuses a `repo` or `root` inside a linked worktree of the repository; `remove` prunes registrations beneath the path it deletes, then `branch -d`s their branches with the merged check intact | `an_orchestrator_cannot_be_started_inside_another_runs_worktree`, `removing_a_worktree_reclaims_the_worktrees_nested_inside_it_from_shared_metadata` |

## Broker

| Invariant | Mechanism | Guard test |
|---|---|---|
| An agent cannot write to another ticket | no tool takes an issue id; the target comes from the per-run token | `a_call_naming_a_different_issue_is_refused_and_the_refusal_is_audited` |
| A looping agent cannot write without bound | per-run **and** per-issue budgets, charged on attempts not successes | `a_continuation_cannot_refresh_the_budget_by_opening_a_new_session` |
| A finished run keeps no write authority | the session is an RAII guard living in the `running` entry | `a_run_that_ends_takes_its_broker_authority_with_it` |

## Store

| Invariant | Mechanism | Guard test |
|---|---|---|
| A half-applied migration cannot stop the store opening | each migration and the `user_version` bump that records it commit in one transaction | `a_migration_that_fails_partway_leaves_no_trace_and_does_not_advance_the_version` |
| A released migration is never edited | `migrate` records only the number of the last migration a store applied, so the tests pin a `blake3` of every released entry in `RELEASED`: an edited entry fails naming its version, and a new one fails until its hash is appended (#81) | `a_released_migration_is_never_edited` |

## Operator surface: API, MCP, client, projection, transcripts

| Invariant | Mechanism | Guard test |
|---|---|---|
| Clearing a quarantine or a park cannot release a live claim | `Store::unquarantine` is guarded on `quarantined_at IS NOT NULL`, and `Store::unblock` on a park in phase `released` with no quarantine, no retry row and no delivery in a stage that pushes, hands back or has handed off — so a running, gating, retry-queued or delivering issue is untouched, and neither is one whose ticket `Scheduler::unblock` reads as no longer active — and both report what they did; unblock lifts the park and never takes or releases a claim (#108) | `clearing_a_quarantine_that_is_not_there_does_not_release_a_live_claim`, `unblocking_an_issue_that_is_not_parked_does_not_release_a_live_claim`, `unblocking_a_parked_blocked_issue_dispatches_it_onto_its_existing_branch`, `an_unblock_does_not_lift_a_park_a_live_delivery_still_owns`, `unblocking_an_issue_whose_ticket_closed_leaves_it_for_the_parked_sweep`, `unblocking_an_issue_dispatch_would_refuse_keeps_its_park`, `a_ticket_that_closes_between_an_unblock_and_the_next_tick_is_still_swept` |
| A slow HTTP client cannot delay a tick | one task per connection, a `oneshot` reply the scheduler never waits on, and a bounded read timeout | `a_client_that_never_finishes_its_request_cannot_delay_a_tick` |
| The projection cannot become load-bearing | `publish` logs a projector error and returns `Ok`; nothing written is ever read back | `the_scheduler_makes_the_same_decisions_whether_the_projector_writes_fails_or_is_off` |
| "No daemon" is never confused with "daemon said no" | `StatusError` splits a refused connection from a refused request, and names the address and its source in both | `a_closed_port_reads_as_no_daemon_rather_than_a_refused_request` |
| The client cannot open the daemon's store | `crewctl` is its own package over `libcrew`, whose graph holds no `rusqlite`, `ratatui`, `tokio` or `ureq`; CI fails if `cargo tree -p crewctl -e normal` ever shows one (#45) | `a_status_query_does_not_open_the_database_the_daemon_holds` |
| Retention cannot delete a live run's transcript | `prune` is handed the paths of runs still in `running` | `retention_bounds_the_transcript_directory_but_spares_a_stalled_runs_own_file` |
| crewd never hands a dispatched worker the ops tools | the ops MCP server has its own listener and its own path, and is never passed to `Broker`, whose `open` writes the only `--mcp-config` crewd gives a worker. Not unreachability: a worker inherits the operator's MCP config, local scope included, and that is accepted (see `src/api/mcp.rs`) | `a_dispatched_worker_is_not_handed_the_ops_tools` |
| An operator's question cannot disturb the daemon it asks about | `OpsMcp` holds an `Api` — a `watch::Receiver` and a `Command` sender, no `Store` — and a read sends no `Command` at all | `an_ops_read_cannot_disturb_the_daemon_it_asks_about` |
| A wedged or hostile client cannot exhaust the MCP transport | `broker::server::Limits`: a deadline on a request that has started, split from the idle wait so keep-alive survives, plus a per-listener cap on connections in flight, released by RAII | `a_client_that_never_finishes_its_request_cannot_hold_a_connection_thread`, `a_flood_of_connections_is_refused_rather_than_served_without_bound` |

## Handoff gate

| Invariant | Mechanism | Guard test |
|---|---|---|
| A branch is not handed off ungated against the base it will merge into | a `Done` moves the run into `gating` with its claim held; `GitGate` rebases first and runs the commands on the rebased tree — unless the branch already contains the base's tip (`merge-base --is-ancestor`), which is already against that base and whose merge of it a rebase would drop, re-raising a conflict the agent resolved by merging (#122); that path still refuses uncommitted tracked changes, as the rebase does, since delivery pushes `HEAD` alone, and a worktree paused mid-rebase is `Stuck` rather than passed | `a_done_verdict_is_gated_in_its_own_worktree_before_the_claim_is_released`, `a_dirty_worktree_already_on_the_base_fails_rather_than_passing_without_its_edit`, `a_worktree_paused_mid_rebase_on_the_base_is_stuck_rather_than_passed`, `the_branch_is_rebased_onto_the_base_before_the_gate_runs_on_the_rebased_tree`, `a_branch_behind_the_base_is_still_rebased`, `a_branch_that_already_merged_the_base_is_gated_without_a_rebase`, `a_conflict_resolved_by_merging_the_base_is_not_re_raised_by_the_gate` |
| A rebase conflict is a human's problem, not a silent failure — unless every conflicted path is one the operator listed as the agent's | the rebase is aborted first, whichever way it goes, and one left in progress after the abort is `Verdict::Stuck`, always `Blocked`; a conflict touching any path outside `gate.agent_resolvable` parks `Blocked` naming the paths, in `last_error`; one confined to it is a `Continue` whose `Feedback::Gate` brief names the base and paths, charged to `gate.max_failures` like a failing command (#111); `preflight` rejects a blank pattern | `a_rebase_conflict_parks_the_issue_blocked_naming_the_conflicted_paths`, `a_conflicting_rebase_is_aborted_and_names_the_conflicted_paths`, `a_conflict_confined_to_agent_resolvable_paths_is_handed_back_to_the_agent`, `a_conflict_touching_any_other_path_still_blocks_for_a_human`, `repeated_unresolved_docs_conflicts_escalate_to_blocked`, `a_blank_agent_resolvable_pattern_is_rejected`, `a_rebase_the_gate_could_not_abort_blocks_for_a_human_even_on_resolvable_paths`, `a_worktree_left_mid_rebase_is_detected_and_an_aborted_one_is_not` |
| The gate cannot become a runaway | a failing command is a `Continue` that carries its output, bounded by `gate.max_failures` consecutive failures → `Blocked`, and by `gate.timeout_ms` per gate | `a_failing_gate_continues_the_run_with_the_failing_output_in_hand`, `repeated_gate_failures_escalate_to_blocked_rather_than_looping`, `a_gate_that_hangs_is_killed_at_the_timeout_and_counts_as_a_failure` |
| A restart cannot forgive the gate's failure streak | the streak is `issue_state.gate_failures` (v7), bumped by `Store::bump_gate_failures` and cleared exactly where a pass or a verdict ending the line of work cleared the old in-memory map | `a_gate_failure_streak_is_not_forgiven_by_restarting_the_daemon` |
| A gate cannot outrun the concurrency limit | `global_slots` and `state_slots` count `gating` as well as `running`, because a gate is a build on this host and the claim is held across it | `a_gating_run_still_holds_its_concurrency_slot_so_gates_cannot_accumulate` |
| A gate cannot hide how far its run got | the turn count is checkpointed as the entry leaves `running`, since `observe_progress` walks only that map while the run row stays open for the whole gate | `a_run_entering_the_gate_checkpoints_its_turn_count_before_it_leaves_running` |
| A gate command that can never start is refused at startup | `preflight` rejects a blank program name as well as an empty argv, so a typo costs one error instead of every run of the issue | `a_gate_that_could_never_escalate_or_never_start_is_rejected` |
| The gate's own git subprocesses are killable | every `git` the gate runs goes through `spawn_tracked`, so `pgid` is set for the rebase and not only for a configured command; the rebase re-checks `killed` before it spawns, since a probe killed just before it reads as an ordinary failure | `killing_a_gate_during_the_rebase_step_stops_the_git_subprocess_instead_of_leaving_it_running`, `a_gate_stopped_before_its_rebase_starts_does_not_rebase` |
| A gate that cannot start reports rather than panics | a supervising thread the OS refuses becomes `Verdict::Failed`, which is what `Gate::start` promises; a panic here would strand the claim until the next startup | `a_gate_whose_supervising_thread_cannot_be_spawned_reports_failed_instead_of_panicking` |
| A brief describes the tree the agent will find | `Verdict::Failed` carries `on_base`, false for every step before the rebase and for a rebase that was aborted | `a_gate_that_failed_before_rebasing_does_not_tell_the_agent_its_branch_was_rebased` |

## Delivery

| Invariant | Mechanism | Guard test |
|---|---|---|
| A red gate cannot rest on `Done` | delivery reads CI after every push and hands a failure back through the retry path with the failure in the prompt | `a_red_ci_gate_re_dispatches_the_issue_with_the_failure_in_the_prompt_and_the_run_does_not_rest_on_done` |
| A check that re-runs on a ready pull request is not CI that reported nothing | the CI wait is timed from when this head was first seen pending, never from crewd's own push, and cleared whenever CI is not pending; it is measured on `Mono` (`Scheduler::ci_waits`) so a wall-clock step cannot cut it short, and `delivery.ci_pending_head`/`ci_pending_since` (v11) record its wall-clock start so a restart resumes the wait rather than forgiving it (#105) | `a_check_re_run_on_a_ready_pull_request_does_not_hand_it_off`, `a_wall_clock_step_does_not_cut_a_ci_wait_short`, `a_restart_does_not_forgive_the_ci_wait_already_spent` |
| A head the operator pushed gets its own CI wait | the pending clock is keyed on the head the provider reports, so a head crewd did not push restarts it rather than inheriting what was left of the last one (#105) | `a_head_the_operator_pushed_gets_its_own_ci_wait` |
| Delivery does not wait for CI on a pull request that cannot merge | GitHub runs no CI on a conflicting pull request, so `step_delivery` reads `PullRequest::mergeable` (`false`, or `mergeable_state` `dirty`) before the review request and the CI read; a conflict charges one delivery round and `regate` claims the issue and starts the gate on its worktree as if its `Done` were re-gated, so the gate's rebase and #111/#122 decide; with no gate the agent is handed it as `Feedback::Gate`. `null` is the provider still computing, and delivery keeps waiting (#159) | `a_pull_request_the_provider_reports_conflicting_is_re_gated_rather_than_awaiting_ci`, `a_pull_request_whose_mergeability_is_unknown_keeps_waiting`, `a_conflicting_pull_request_with_no_gate_is_handed_back_to_the_agent`, `mergeability_parses_dirty_as_a_conflict_and_null_as_unknown` |
| A review request that attached nobody is not a success | the request is followed by a read of `requested_reviewers` and `reviews`; a missing login hands off with the reason | `a_review_request_the_provider_accepts_without_attaching_a_reviewer_is_reported_as_a_failure` |
| A settled review comment is not re-argued | verdicts are recorded by comment id, first one stands, and open threads are computed against that table | `each_review_comment_ends_accepted_with_a_commit_or_rejected_with_a_reason_and_is_not_re_argued` |
| A finding left only in a review's summary is worked like an inline comment | `step_delivery` adds `summary_findings` — each review on the current head from a `summary_reviewers` login, or `CHANGES_REQUESTED` from anyone, whose body says more than "Findings: None" — to the open comments, keyed `review-<id>`, so the same verdict table settles it once, the same round bounds charge it, and an unsettled one holds or leaves `ready`; its verdict is a pull request comment quoting it, and it has no thread to resolve (#126) | `a_finding_only_in_a_review_summary_is_handed_back_to_the_agent`, `a_review_after_ready_with_a_summary_finding_reopens_the_round`, `a_settled_summary_finding_is_not_handed_back_again`, `a_summary_that_says_findings_none_and_nothing_else_hands_nothing_back`, `a_changes_requested_review_with_only_a_summary_is_handed_back` |
| The review-fix loop is bounded, and the bound survives a new run and a new pull request | `rounds_pr` resets only when the pull request number changes; `rounds_issue` never resets; both checked before charging | `fix_rounds_are_bounded_per_pull_request_and_per_issue_and_the_bound_survives_a_new_run_and_a_new_pull_request` |
| Nothing merges without a human | `Forge` has no merge method; a ready pull request is polled, never advanced | `nothing_merges_without_a_human` |
| Delivery never publishes an ungated branch | a `Done` enters `gating` first and only the gate's pass reaches the `Done` arm that queues delivery; a failing gate is a `Continue` the forge never hears about | `a_done_branch_is_gated_before_delivery_pushes_it_and_a_failing_gate_publishes_nothing` |
| A reused pull request targets the base the scheduler recorded | `open_pull_request` compares the base it finds with `spec.base` and retargets — body included — when they differ, instead of returning the pull request as found | `a_reused_pull_request_whose_desired_base_has_changed_is_retargeted`, `a_pull_request_whose_desired_base_has_changed_is_retargeted_and_the_snapshot_agrees` |
| A stack base the remote does not have is not a base | `Publisher::stacked_on` takes the remote and keeps only the candidates it has, asked directly with `ls-remote` rather than read off remote-tracking refs | `a_stack_candidate_the_remote_does_not_have_is_not_selected_as_a_base`, `a_base_that_is_not_published_is_not_selected_as_a_stack_base` |
| A verdict is settled only by a reply that landed | `apply_verdicts` replies first and records second; a failed reply leaves the verdict in `pending_verdicts` — which the push no longer clears — and returns the error, so the threads are neither read nor handed back until it lands | `a_verdict_is_not_settled_by_a_reply_that_did_not_land` |
| A settled review comment's thread is resolved, and resolution never re-opens the verdict | `resolve_settled_threads` runs after `apply_verdicts` as its own step, resolving every recorded verdict — accepted or rejected — whose `review_verdict.resolved_at` (v9) is NULL; a failed resolve is logged and retried next poll, costing no reply, no round and no readiness — but surfaced, a permanent refusal on the issue's row — which stays computed from the verdict table rather than the provider's `isResolved`; `GithubForge::resolve_threads` reads the pull request's threads over GraphQL once per pass, finds each by its root comment's `databaseId`, is a no-op on one already resolved, and counts a mutation answered without `isResolved: true` as a permanent refusal rather than a resolve (#89, #100) | `a_settled_review_comment_has_its_thread_resolved`, `a_failed_resolve_is_retried_and_never_re_replies`, `a_refused_resolve_is_reported_on_the_row_without_handing_off_a_ready_pull_request`, `resolve_threads_finds_the_thread_by_its_root_comment_and_resolves_it`, `a_resolve_the_provider_did_not_apply_is_not_recorded_as_resolved`, `resolving_several_comments_reads_the_threads_once` |
| A handed-off delivery does not outlive its merged pull request as a failure | `advance_deliveries` keeps polling a `HandedOff` row at `poll_interval_ms`, for the pull request's state alone (`watch_handoff`: no push, no review request, no `Feedback`); merged or closed moves it to `Closed` with how it ended and clears the issue's `last_error`, through the same `close_delivery` a `Ready` row takes (#101) | `a_handed_off_pull_request_the_operator_merged_is_reported_closed_not_failed`, `a_handed_off_pull_request_that_stays_open_is_polled_for_its_state_only` |
| A new head is not left unreviewed | `set_delivery_pr` resets `review_requested` when the head changes, not only when the pull request number does, so the request-then-verify runs again for every push | `a_new_head_on_the_same_pull_request_needs_its_review_requested_again`, `a_fix_round_re_requests_review_so_the_new_head_is_not_left_unreviewed` |
| An acceptance names a commit the branch carries, never a bare acknowledgement | `extract_verdicts` drops an `accepted:` whose detail is not commit-shaped; `verified_verdicts` checks the rest with `Publisher::carries` as the run reports `Done`, before the gate's rebase rewrites the shas | `an_acceptance_that_names_no_commit_leaves_its_comment_outstanding`, `an_acceptance_is_believed_only_for_a_commit_the_delivered_branch_carries`, `an_acceptance_naming_a_commit_the_branch_does_not_carry_leaves_the_comment_outstanding` |
| A rebased branch updates its pull request, and never overwrites someone else's work | `publish` pushes `--force-with-lease`; a lease failure is classified on its own as permanent, naming the remote branch that moved, so it stays distinct from a stale-base rejection | `a_rebased_branch_is_pushed_over_its_own_history_but_never_over_someone_elses` |

## crewd init

| Invariant | Mechanism | Guard test |
|---|---|---|
| `crewd init` cannot be driven by a page other than its own | a per-run 128-bit `state` nonce checked on the callback — a mismatch ends the run before the code is converted — and a listener that answers only its own loopback `Host` | `a_callback_whose_state_this_run_did_not_issue_is_refused_without_converting_the_code`, `a_request_under_another_host_name_is_not_shown_the_page_or_its_nonce` |
| The init listener does not outlive its one callback | `await_callback` stops and joins the accept thread before returning, whatever the callback's outcome | `init_ends_with_a_600_key_a_settings_file_naming_it_and_an_installed_app_after_two_clicks` |
| The App's key and client secret never reach a log | the key is held in a redacting `Pem`, the secrets are never deserialized, and no error quotes a successful conversion body | `neither_the_key_nor_the_client_secret_reaches_the_log_at_any_level` |
| `init` never overwrites a key | both files checked before GitHub is asked anything, then created with `create_new` at mode 600 | `an_existing_key_or_settings_file_is_refused_by_name_before_github_is_asked_anything` |
| The created App has exactly the permissions asked for, and is not offered for install otherwise | one `PERMISSIONS` table builds the manifest and is compared against `GET /app`, read with the App's own JWT, while the callback is still open — only a pass redirects the browser to *Install* | `an_app_created_with_other_permissions_than_the_manifest_fails_the_run_over_its_own_jwt` |
| The init listener cannot be made to hold threads without bound | `ConnSlot::take` against `Limits::max_connections` before a connection's thread exists; past the cap the socket is closed | `connections_past_the_cap_are_refused_rather_than_each_given_a_thread` |

## How the guards were checked

The delivery rows' bound is the same shape as the broker's, and each guard was checked the same
way: disable the mechanism — treat a CI failure as success, trust the provider's `200`, drop
the per-issue bound or reset it with the pull request, stop consulting the verdict table — and
the named test fails. The six rows from #47 were checked the same way — a plain push, then a
bare `--force`; the pull request returned as found; the remote filter dropped from both
`stacked_on`s; the verdict recorded before its reply; `review_requested` keyed on the number
alone; the commit check removed from the worker, from `carries` and from the scheduler in turn —
and each named test failed, with the file restored by `cp` rather than `mv`, because a preserved
mtime leaves cargo running the stale binary.

The three broker rows are one property in three places, and the middle one is the easy one to
lose: a reviewer who sees `max_calls_per_run` will read it as the bound and delete the
per-issue cap as redundant. It is not — re-read the continuation loop before touching it.

Three of these — the verdict, the per-issue turn budget and `parked_state` — are independent
brakes on the same runaway. Removing any one of them looks safe because the other two still
hold. They cover different paths; keep all three. The gate's `max_failures` is a fourth, for the
route the gate opened: a gate-sent `Continue` goes through the turn budget too, but at forty
turns a session the budget is a slow brake, and a suite an agent cannot make pass would spend
all of it rediscovering that.
