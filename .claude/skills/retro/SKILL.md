---
name: retro
description: Retrospective over finished crewd runs. Reads their transcripts and proposes changes to the environment the agent ran in — CLAUDE.md, gate and CI checks, docs, tools — as candidate Inbox issues. Use after a delivery closes, after a quarantine or handoff, or weekly over the last N transcripts. Typed by the operator; never loaded into a dispatched agent.
disable-model-invocation: true
---

# Retro

You are improving the **environment** a dispatched agent ran in, so later runs on this repo
spend fewer turns and fail less. A retro changes no file. It ends with candidates the operator
can file.

Style for anything you draft: match the density of the file it would land in; point at a file
instead of restating it; say what to do, not what to avoid.

## Steps

1. **Pick the runs.** The operator names an issue, a run id, a transcript path, or a count.
   With none, take the newest transcript. The root is `[transcripts] root` in the daemon's
   config, default `.transcripts` under `workspace.root`; `crewctl status <issue>` prints each
   run's path. Done when every file you will read is listed.

2. **Read each run.** Pull the parts that carry lessons and skip the rest (`system` records are noise; `rate_limit_event` is read below because utilization can explain wasted turns):
   ```bash
   jq -c 'select(.type | endswith("_run_start") or endswith("_run_end") or . == "result")' <run>.jsonl
   jq -r 'select(.type=="assistant") | .message.content[]? | select(.type=="text") | .text' <run>.jsonl
   jq -c 'select(.type=="assistant") | .message.content[]? | select(.type=="tool_use") | {name, input: (.input | tostring | .[0:160])}' <run>.jsonl
   jq -c 'select(.type=="user") | .message.content[]? | select(.type=="tool_result" and .is_error==true) | .content' <run>.jsonl
   jq -c 'select(.type=="rate_limit_event") | .rate_limit_info | {status, utilization}' <run>.jsonl
   ```
   The last two lines are often empty. The `result` event carries its own `is_error` for the
   whole run; an empty fourth line means no single tool call failed. Then read the issue body
   the prompt was built from, and `gh pr view <n> --comments` for review rounds and CI when
   the run reached a PR. A run that ended before any commit has no PR; its ending is the
   finding. Done when you can name what the run spent its turns on, which turns were wasted,
   and why.

3. **Read the environment** as it is today: `CLAUDE.md`, `docs/coding-guidelines.md`,
   `[gate] commands` in the config, `.github/workflows/ci.yml`, `.claude/skills/`. These are
   the files a finding changes. For an old run, check the file at the run's commit before
   blaming a rule that did not exist yet; the finding still lands in today's file.

4. **Look for candidates** in these categories:

   - **Navigation.** Turns spent finding a file, a call site or a rule. A pointer in
     `CLAUDE.md` or a docs file would have saved them. *Use when* the run took several turns
     to find one fact.
   - **Automated checks.** A mistake the gate or CI could have caught, or did catch late. Read
     `[gate] commands` and `ci.yml` first: a check that exists but is unwired is the finding.
     A `Check: review` rule in `docs/coding-guidelines.md` that a lint could enforce is one
     too. *Use when* a gate or review round failed on something mechanical.
   - **Coding standards.** A rule `/code-review` or Copilot should have applied and did not,
     or one that misfired. Mechanical rule → a lint or CI step. Judgement call → a rule with
     `Check: review`. *Use when* a review round asked for a change no rule names.
   - **Steering files.** A line in `CLAUDE.md` the run ignored, contradicted, or did not
     need. A cache of something the environment already states. *Use when* the run acted
     against a written instruction, or the file is long.
   - **Tool economy.** Expensive or repeated tool calls; broker refusals; rust-analyzer
     answering empty on first load. *Use when* one tool ate a large share of the turns.
   - **Information access.** A fact the run needed that neither the issue body nor the
     environment gave it. An under-specified issue body is a triage finding; say so.
     *Use when* the run guessed, or asked a question nobody answered.
   - **Budget.** Turns against `agent.max_turns_per_session`, continuation rounds, gate
     `max_failures`. *Use when* a budget ended the run, or nearly did.

5. **Filter.** A candidate survives when all four hold: it cost turns in this run and will
   recur; it names the file to change; no open issue covers it
   (`gh issue list --state open --search "<keywords>"`, with two or three phrasings, since
   the search is literal); it fits an ADR 1 boundary, or is a docs/CI change with no
   boundary. Keep at most 2 per run. "Nothing to file" is a result.

6. **Present** the survivors in one table, most severe first: category, turns it cost, the
   file to change, the proposed title. Then stop and wait.

7. **File on the operator's say-so**, one issue per candidate, with the `issue-authoring`
   skill. `## Why` opens with the turns this cost and the run. When no ADR 1 boundary fits,
   the line is `**Boundary:** none, because a docs/CI change`. `## Triggered by` names the
   run id, the PR and the date.
