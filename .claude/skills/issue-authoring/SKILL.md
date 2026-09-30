---
name: issue-authoring
description: File a GitHub issue on this repo from the work template. Use when creating an issue or splitting follow-up work into one.
---

# Issue authoring

The issue body is what a dispatched agent receives. Tracker comments do not reach that
prompt. Write the issue from
[.github/ISSUE_TEMPLATE/work.md](../../../.github/ISSUE_TEMPLATE/work.md). The
`issue-triage` skill places it and is the only place the `agent` label is added.

## Steps

1. **Search.** The search is literal, so try two or three phrasings:
   ```bash
   gh issue list --state open --search "<words>"
   ```
   Done when an open issue already states this outcome, or none does.

2. **Write the body** in a file, from the template sections, without the HTML comments.
   - The title is one sentence naming the outcome. The first line of Why restates it.
   - What states the work. Boundary is one line from [Boundary](#boundary). Out of scope
     is `none` when nothing is fenced off.
   - Acceptance criteria are guard tests named as sentences
     (`a_thing_holds_under_the_condition`). Each must fail on the base commit. A
     behavior-preserving change names the existing tests that stay green. More than three
     criteria means two issues.
   - Delete Open when nothing is undecided. Triggered by names what someone was doing,
     and the date.
   - Leave the Decisions section out. Triage appends one.
   Done when every kept section is filled and the file contains no HTML comment.

3. **Choose labels** from [Areas](#areas). Leave the milestone unset, and leave `agent`
   off. Set one area when the change has a single home, and `bug` when shipped behavior
   is false. An issue that names more than one area, or none, gets no area label.
   Done when the labels you will pass are at most `bug` plus one area.

4. **Create and check.** Pass `--label` once per label, and omit it when there is neither
   an area nor `bug`.
   ```bash
   gh issue create --title "<outcome>" --body-file <file> --label <area>
   gh issue view <n> --json title,milestone,labels,body
   ```
   `gh issue create` prints the URL and does not accept `--json`. The view is the check:
   no milestone, no `agent`, the sections present, and the labels from step 3.
   Done when that view matches.

## Areas

One area, where the change lands.

| Area | Lands in |
|---|---|
| `delivery` | The handoff: the gate, the forge, rebase, the pull request, review sent back |
| `scheduler` | Dispatch, claims, the store, startup and shutdown, config, `crewd init`, a release |
| `worker` | The session: spawn, prompt, resume, budgets, the model, confinement, the broker |
| `ops` | `crewctl`, the ops API, the TUI, logs, traces, readiness |
| `workspace` | Worktrees and branch preparation |
| `guidelines` | Lints, CI, and `docs/coding-guidelines.md` |
| `tracker` | The GitHub or Jira adapter: poll, the dispatch rule, transitions |

A pull request stays `delivery` when delivery also writes the issue. The broker stays
`worker`. The store stays `scheduler`.

`bug` means shipped behavior is false. The milestone is the priority, and triage sets the
milestone.

## Boundary

Name where the work lands
([ADR 1](../../../docs/adr/0001-extension-boundaries.md)):

| Boundary | Fits when |
|---|---|
| **trait implementation** | a new backend behind `Tracker`, `TrackerWrites`, `Worker`, `Workspace`, `Forge`, `Gate`, `Store` or `Projector` |
| **external command** | an operator tool that needs only the ops API: a `crewctl-<name>` program on `PATH` |
| **hook** | a reaction to a lifecycle event that decides nothing: notifications, metrics |
| **core change** | it closes or protects an invariant, or needs the scheduler's authority: a claim, a budget, a bound, a write on an agent's behalf |

One line under What: `**Boundary:** <name>, because <reason>`. A core change names the
invariant row in `docs/invariants.md` it adds or protects, or says why it needs none. An
issue that fits none of the four is still filed; triage sends it to Backlog.
