#!/usr/bin/env bash
# Stop hook: a dispatched agent sees a fmt or clippy failure before it reports
# done, rather than a continuation later from the handoff gate (#187).
#
# It acts only on a `crew/` branch, the prefix every dispatched branch has
# (`branch_name` in src/workspace.rs). An operator's session on such a branch is
# blocked too, on purpose: it is about to push to an agent's branch.
#
# It blocks one stop at a time. When Claude Code is already continuing because
# of a stop hook (`stop_hook_active`), the next stop is let through, so a
# failure the agent cannot fix costs one extra turn rather than a loop. The gate
# stays the check that decides; `cargo test` is left to it as too slow here.
set -uo pipefail

input=$(cat)

branch=$(git branch --show-current 2>/dev/null || true)
case "$branch" in
  crew/*) ;;
  *) exit 0 ;;
esac

if printf '%s' "$input" | grep -Eq '"stop_hook_active"[[:space:]]*:[[:space:]]*true'; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

failed=""
if ! out=$(cargo fmt --check -- --color never 2>&1); then
  failed="$failed
\$ cargo fmt --check
$(printf '%s\n' "$out" | tail -n 60)
"
fi
if ! out=$(cargo clippy --all-targets --quiet --color never -- -D warnings 2>&1); then
  failed="$failed
\$ cargo clippy --all-targets -- -D warnings
$(printf '%s\n' "$out" | tail -n 120)
"
fi

[ -z "$failed" ] && exit 0

# Exit 2 blocks the stop and hands stderr to the agent.
printf 'The commit gate would fail on this branch. Fix these before finishing:\n%s' "$failed" >&2
exit 2
