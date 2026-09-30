---
name: issue-authoring
description: File a GitHub issue on this repo. Use when creating an issue or splitting follow-up work into one.
---

# Issue authoring

Write the issue from
[.github/ISSUE_TEMPLATE/work.md](../../../.github/ISSUE_TEMPLATE/work.md). The
`issue-triage` skill places it.

## Steps

1. **Search.** The search is literal, so try two or three phrasings:
   ```bash
   gh issue list --state open --search "<words>"
   ```
   Done when one open issue states this outcome, or the searches show that none does.

2. **Write the body** in a file, from the template's sections.
   - The title is one sentence naming the outcome. The first line of Why restates it.
   - What states the work. Boundary is one line from [Boundary](#boundary). Out of scope
     is `none` when nothing is fenced off.
   - Acceptance criteria are guard tests named as sentences
     (`a_thing_holds_under_the_condition`). Each fails on the base commit. A
     behavior-preserving change names the existing tests that stay green. More than three
     criteria is two issues.
   - Keep Open only while a decision is still open. Triggered by names what someone was
     doing, and the date.
   - Triage appends the Decisions section. Leave it out of this file.
   Done when Why, What, and Triggered by each contain that content, Out of scope is
   filled, Open is absent or one open decision, and no HTML comment remains.

3. **Choose labels** from [Areas](#areas). Leave the milestone unset. Leave `agent` for
   triage.
   Done when what you will pass is at most `bug` plus one area, with the milestone unset
   and `agent` absent.

4. **Create and check.** Pass `--label` once for each label step 3 chose: none, an area,
   `bug`, or `bug` and an area.
   ```bash
   gh issue create --title "<outcome>" --body-file <file> [--label <area>] [--label bug]
   gh issue view <n> --json title,milestone,labels,body
   ```
   The create command prints the URL. The view is the check.
   Done when the view shows no milestone, no `agent`, the sections from step 2, and the
   labels from step 3.

## Areas

Set one area when the change lands in a single row. A change that spans rows, and a change
that lands in none, carries no area label.

| Area | Lands in |
|---|---|
| `delivery` | The handoff: the gate, the forge, rebase, the pull request, review sent back |
| `scheduler` | Dispatch, claims, the store, startup and shutdown, config |
| `worker` | The session: spawn, prompt, resume, budgets, the model, confinement, the broker |
| `ops` | `crewctl`, the ops API, the TUI, logs, traces, readiness |
| `workspace` | Worktrees and branch preparation |
| `guidelines` | Lints, CI, and `docs/coding-guidelines.md` |
| `tracker` | The GitHub or Jira adapter: poll, the dispatch rule, transitions |
| `onboarding` | `crewd init`, installing, building a release, publishing to crates.io |

A pull request stays `delivery` when delivery also writes the issue.

Set `bug` when shipped behavior is false. Triage sets the milestone.

## Boundary

One line under What, naming which of the four boundaries in
[ADR 1](../../../docs/adr/0001-extension-boundaries.md) the work belongs to:

`**Boundary:** <name>, because <reason>`.

A core change also names the row in [docs/invariants.md](../../../docs/invariants.md) it adds
or protects, or says why it needs none.

When none of the four fits, the line is `**Boundary:** none, because <reason>`.
