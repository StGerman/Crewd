---
name: issue-triage
description: Triage this repo's GitHub Issues into the milestone and agent states the daemon dispatches from. Use when running the weekly triage, clearing the inbox, placing an issue, or planning the next milestone. Filing an issue is the issue-authoring skill.
---

# Issue triage

The backlog is this repository's GitHub Issues. The daemon reads it directly: an open issue
labelled `agent` is work a real agent will pick up and spend tokens on. The label is
`tracker.dispatch_label` in `crew.github.toml`, and carrying it is the whole rule — an assignee
is neither needed nor sufficient (`DispatchRule` in `src/tracker/github.rs`; re-read it if the
rule may have moved). Triage
is therefore the decision point between "someone wrote this down" and "an agent works this" —
treat it as the gate it is.

## States

Every open issue is in exactly one state. Check the rows top-down; the first match is its
state.

| State | On GitHub | Meaning |
|---|---|---|
| **Inbox** | no milestone | Nobody has looked at it yet. The default for every new issue. |
| **Backlog** | milestone **"Worth doing, not scheduled"** | Triaged and accepted, with no date. |
| **Dispatchable** | current milestone, labelled `agent` | The daemon will pick it up. |
| **Planned** | any other `M<n>` milestone | Committed. The milestone *is* the priority. |

A current-milestone issue without `agent` is **Planned**, not
Dispatchable: report it that way, because the daemon will never run it. An issue triage decides
against leaves the set: it is closed as *not planned* with a one-line reason.

Keeping Backlog as a real milestone is what makes "no milestone" mean *untriaged* and nothing
else, so `is:open no:milestone` is the complete inbox with no extra label to maintain.

Milestones are titled **`M<n> - <outcome>`**: a sequence number, then a human-readable phrase
that says what is true once it closes ("M1 - The daemon can be trusted with its own backlog").
The `M<n>` is the order milestones are worked in — it is the sequence in the title, not
GitHub's internal milestone number, which only records creation. Backlog has no `M<n>`, because
it is not in the sequence. Read the current
set from GitHub rather than from memory; it changes:

```bash
gh api repos/{owner}/{repo}/milestones --jq '.[] | "\(.number)\t\(.title)\t\(.open_issues) open"'
```

## Making an issue dispatchable is a human decision

Adding `agent` is what turns an issue into a spawned `claude -p` process with
`bypassPermissions` against a real worktree. It is added only in triage, only by the operator or
on their explicit say-so, and only to an issue that is:

- **in the current milestone** — `agent` is how the milestone order reaches the daemon, so it
  sits on the current milestone's issues and nowhere else;
- **well-specified** — the issue states the failure and what done looks like, so an agent does
  not spend its turn budget discovering the question;
- **free of an open design choice** — anything that needs a decision (an ADR, "option 1 or 2",
  an "Open" section in the body) goes to the operator first, and the answer is written into the
  description (see below) before the issue is made dispatchable.

## Decisions go into the description

A dispatched agent's prompt is built from the issue **body** alone. Comments never reach it
(`src/worker/claude.rs`, where the prompt is assembled). So every decision made in triage or
grooming that changes how the work is done — an approach chosen, a default picked, scope cut,
an open question settled, work folded into another issue — is written into that issue's
description the moment it is made, not left in a comment or in this conversation:

- Append a `## Decisions (<where>, <YYYY-MM-DD>)` section, e.g. `(backlog grooming, 2026-09-25)`,
  opening with one line saying it overrides anything above that contradicts it.
- Strike through (`~~...~~`) each acceptance criterion or paragraph a decision supersedes,
  pointing to its replacement, and write replacement criteria in the Decisions section. Two
  live, conflicting criteria lists leave the agent to guess which one holds.
- Scope cut out of an issue becomes a new Inbox issue, filed with the `issue-authoring`
  skill and linked from the Decisions section.
- Edit with `gh issue view <n> --json body --jq .body > <file>`, change the file, then
  `gh issue edit <n> --body-file <file>`, so the rest of the body survives byte for byte.

This holds for an issue that already has a live session too. A resumed session's prompt
leaves the body out while it is unchanged, and sends it again, under a heading saying it
changed, whenever its hash differs from the body the session last saw (#109). Editing the
description is therefore the way to reach a running or parked issue's agent. A comment still
does not reach it.

An issue whose body still has an unanswered question is not ready to be made dispatchable,
whatever its milestone.

## Scope check

Before placing an issue, confirm its `**Boundary:**` line is present and matches
[ADR 1](../../../docs/adr/0001-extension-boundaries.md). An issue that needs a core change
only because no hook or command exists yet keeps that boundary; the missing hook or command
becomes its own issue, linked from the description. An issue that fits none of the four goes
to Backlog, whatever its urgency sounds like.

## Placing an issue

Apply this rule top-down; the first match wins.

1. **It breaks dogfooding or an invariant** — a wrong scheduling decision, lost work, a
   stranded claim, credentials or rate limits that stop the daemon, anything that would fail a
   row of the invariant table in docs/invariants.md. → the **current** milestone. Dispatchable if it meets
   the bar above.
2. **It unblocks the current milestone.** → the current milestone.
3. **It fits a future milestone's outcome.** → that milestone.
4. **Otherwise** → Backlog. Backlog items are promoted only when a milestone is being planned,
   never one at a time between plannings — that is what keeps the current milestone finishable.
5. **Duplicate, already fixed, or not worth doing** → close as *not planned* with the reason
   (link the duplicate or the PR that fixed it).

"Current milestone" is the open milestone with the lowest `M<n>`.

Revisit the area against [Labels](#labels). Record the choice in the confirmation table
with the milestone.

## Priority is the milestone

There are no priority labels. Milestones are worked one at a time in `M<n>` order, and
that sequence is the whole priority order. Inside a milestone, issues are peers: the daemon
takes them oldest first, and nothing tries to steer that.

So when order inside a milestone *matters* — one issue must land before another, or a fix is
more urgent than the rest — the milestone is too big: **split it**. The part that goes first keeps
its `M<n>`; the rest moves to a new milestone right after it, and every later milestone's `M<n>`
is shifted up by one so the titles still read as the sequence. A milestone whose
issues could land in any order is the right size.

The milestone description lists its issues with one clause each on why they belong to that
outcome. Large work is split into small stacked PRs, each its own issue, stacked by the value
each adds to the outcome rather than by when it was written. The stack itself carries that
order — each PR is based on the one before — so its issues can share a milestone.

## Labels

`agent` stays the dispatch rule above. Area and `bug` are the Areas table in the
`issue-authoring` skill. The author may have set them. Keep each or replace it.

#181, #84, #153 and #182 carry no area. The report says why for any newer issue that
Areas leaves unlabeled.

Done when the confirmation table names one area, or names the issue unlabeled and why.

## Weekly triage

Run through these in order. Triage is done when every step's criterion holds.

1. **Inbox to zero.** List it:
   ```bash
   gh issue list --state open --search 'no:milestone' --json number,title,labels,createdAt
   ```
   Read each issue in full (`gh issue view <n> --comments`) and place it with the rule above.
   Done when the inbox list is empty.
2. **One area each.** Revisit every open issue's area per [Labels](#labels). The names in
   the query are the Areas table in `issue-authoring`.
   ```bash
   gh issue list --state open --limit 200 --json number,labels --jq '.[] | {n:.number, a:[.labels[].name | select(test("^(delivery|scheduler|worker|ops|workspace|guidelines|tracker|onboarding)$"))]} | select(.a|length != 1) | .n'
   ```
   Done when that list is only the issues Labels names, plus any newer one the report explains.
3. **Stale dispatch.** Every open `agent` issue should be in the current milestone; fix any
   that is not. Then check each has a linked PR or recent progress:
   ```bash
   gh issue list --state open --label agent --json number,title,milestone,assignees,updatedAt
   gh pr list --state open --json number,title,headRefName
   ```
   An `agent` issue open more than 7 days with no PR is either under-specified (tighten it)
   or blocked (remove `agent`, say why). Done when each one has a PR, a fresh update, or a
   comment explaining the hold.
4. **Resolved but open.** An issue whose fix has merged (check the invariant table and recent
   PRs) is closed with a comment naming the PR. Done when no merged fix is left open.
5. **Report.** Summarise what moved: issues placed per milestone, labels added or removed,
   issues closed, and anything that needs the operator's decision.

## Planning the next milestone

When a milestone closes:

1. The milestone with the next `M<n>` becomes current. Check its name still states the outcome.
2. Promote from Backlog whatever serves that outcome; leave the rest. Write each decision taken
   while planning into its issue's description. If the result has issues
   that must land in a particular order, split it before going on.
3. Update the description's list.
4. Add `agent` to its issues that meet the bar — this is the moment they
   become dispatchable.

## Acting versus proposing

Moving issues, labelling and closing are writes to a shared tracker under the operator's
credential. Present the full set of proposed changes as one table — issue, from-state,
to-state, labels, reason — and apply it only after the operator confirms. When confirmed,
apply with `gh issue edit <n> --milestone "<title>" --add-label agent` and `gh issue close
<n> --reason "not planned" --comment "<why>"`, and batch the calls: the GitHub API budget is
shared with the running daemon.
