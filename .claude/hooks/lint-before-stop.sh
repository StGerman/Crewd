#!/usr/bin/env bash
# Stop hook: a dispatched agent sees a fmt or clippy failure before it reports
# done, rather than a continuation later from the handoff gate (#187).
#
# It acts only on a `crew/` branch, the prefix every dispatched branch has
# (`pretty_branch` in src/workspace.rs). An operator's session on such a branch is
# blocked too, on purpose: it is about to push to an agent's branch.
#
# It checks every stop, including the one after its own block, so a fix is
# verified before the session ends. Claude Code lets a turn end after 8
# consecutive blocks (CLAUDE_CODE_STOP_HOOK_BLOCK_CAP), and crewd's turn budget
# bounds the same loop, so a failure the agent cannot fix still ends the run.
#
# crewd reads a run's outcome from its final message only (`interpret_result` in
# src/worker/claude.rs), and a block makes the agent write another one. So a
# stop whose message reports `continue` or `blocked`, which is not headed for the
# gate, is let through, and the block tells the agent to repeat its marker lines.
#
# The timeout in .claude/settings.json stays under `agent.stall_timeout_ms` in
# the shipped configs (300 s): a hook that outlives it gets a finished run killed
# as stalled. The gate stays the check that decides; `cargo test` is left to it
# as too slow here.
set -uo pipefail

input=$(cat)

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

branch=$(git branch --show-current 2>/dev/null || true)
case "$branch" in
  crew/*) ;;
  *) exit 0 ;;
esac

# Matched on the raw JSON: a marker quoted mid-line only lets a stop through.
if printf '%s' "$input" | grep -Eq 'CREW_OUTCOME: (continue|blocked):'; then
  exit 0
fi

failed=""
if ! out=$(cargo fmt --check -- --color never 2>&1); then
  failed="$failed
\$ cargo fmt --check
$(printf '%s\n' "$out" | head -n 60)
"
fi
if ! out=$(cargo clippy --all-targets --quiet --color never -- -D warnings 2>&1); then
  failed="$failed
\$ cargo clippy --all-targets -- -D warnings
$(printf '%s\n' "$out" | head -n 150)
"
fi

# After a block, a fix left uncommitted would still fail the gate, which hands
# off only what is committed.
if [ -z "$failed" ] && printf '%s' "$input" | grep -Eq '"stop_hook_active"[[:space:]]*:[[:space:]]*true' \
  && [ -n "$(git status --porcelain --untracked-files=no 2>/dev/null)" ]; then
  failed="
\$ git status --porcelain --untracked-files=no
$(git status --porcelain --untracked-files=no | head -n 40)
"
fi

[ -z "$failed" ] && exit 0

# Exit 2 blocks the stop and hands stderr to the agent.
printf '%s\n%s\n%s\n' \
  'The commit gate would fail on this branch. Run `cargo fmt`, fix what clippy reports (the first errors are shown; re-run it for the rest), and commit the fix.' \
  'Then end with your final message again, repeating every CREW_OUTCOME and CREW_REVIEW line from it word for word: crewd reads them from your last message only.' \
  "$failed" >&2
exit 2
