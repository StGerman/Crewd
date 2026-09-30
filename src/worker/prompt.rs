//! The prompt both workers send, and the markers they read back.
//!
//! Claude and Grok have no structured verdict channel, so the outcome and the review
//! replies are lines in the agent's own text. One rendering site keeps the two workers
//! from drifting (#120). `feedback_help` is that site for everything the orchestrator
//! knows and the worktree does not.
//!
//! HTML comments are stripped from the issue body here (#157). Left in, a template hint
//! is dispatched as the task. A marker inside a fence or backticks is the issue quoting
//! one, and stays. Everything else is copied through — a markdown crate would re-emit the
//! description — except the blank-line run that touches a removed comment. A body with no
//! comment must reach the agent unchanged.

use super::{RateLimitSignal, TokenUsage, ToolEndpoint};
use crate::model::{Feedback, Issue, ReviewVerdict, Verdict, looks_like_commit};
use crate::workspace::WipSnapshot;

pub(crate) const OUTCOME_MARKER: &str = "CREW_OUTCOME:";
/// `CREW_REVIEW: <comment-id>: accepted: <commit>` or `...: rejected: <reason>`, one per
/// review comment the run was handed. The same kind of soft convention as the outcome marker,
/// and for the same reason: the CLI has no structured channel for it. A comment the agent
/// gives no line for simply stays outstanding — see the delivery section of `sched`.
pub(crate) const REVIEW_MARKER: &str = "CREW_REVIEW:";

/// Looks for a `CREW_OUTCOME: <kind>: <reason>` line anywhere in the agent's final text —
/// see the module doc for why this is a text convention rather than a structured signal.
pub(crate) fn extract_marker(text: &str, kind: &str) -> Option<String> {
    let prefix = format!("{OUTCOME_MARKER} {kind}:");
    text.lines().find_map(|l| {
        l.trim().strip_prefix(&prefix).map(|rest| {
            let rest = rest.trim();
            if rest.is_empty() { format!("agent reported {kind}") } else { rest.to_string() }
        })
    })
}

/// Every well-formed `CREW_REVIEW: <id>: <accepted|rejected>: <detail>` line in the final
/// text. Malformed lines are skipped rather than failing the run: the run's own outcome does not
/// depend on this, and a comment left unsettled stays outstanding, which is the safe reading.
///
/// An acceptance is well-formed only when its detail is shaped like a commit. `accepted: fixed`
/// is a bare acknowledgement wearing the accepted form, and recording it would post "resolved
/// in fixed" to a reviewer; whether the commit named is actually on the branch is the
/// scheduler's to check, with the worktree in hand, when the run ends.
pub(crate) fn extract_verdicts(text: &str) -> Vec<ReviewVerdict> {
    text.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix(REVIEW_MARKER)?.trim();
            let (id, rest) = rest.split_once(':')?;
            let (kind, detail) = rest.trim().split_once(':')?;
            let verdict = Verdict::parse(kind)?;
            let id = id.trim();
            let detail = detail.trim();
            if id.is_empty() || detail.is_empty() {
                return None;
            }
            if verdict == Verdict::Accepted && !looks_like_commit(detail) {
                return None;
            }
            Some(ReviewVerdict { comment_id: id.to_string(), verdict, detail: detail.to_string() })
        })
        .collect()
}

/// Reads the totals off a `result` event's top-level `usage` block. Only that block: the
/// per-event `message.usage` on `assistant` events is what this replaced, and `modelUsage` on
/// the same `result` carries the same totals keyed by model, which would only matter if the
/// split were wanted.
///
/// `None` when the block is missing rather than a zeroed total — a `result` without `usage` is
/// an unknown cost, and the dashboard must not show it as a free run.
pub(crate) fn extract_usage(v: &serde_json::Value) -> Option<TokenUsage> {
    let usage = v.get("usage")?.as_object()?;
    let get = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    Some(TokenUsage {
        input: get("input_tokens")
            + get("cache_creation_input_tokens")
            + get("cache_read_input_tokens"),
        output: get("output_tokens"),
    })
}

/// Reads `rate_limit_info` off a `rate_limit_event` line, when its `status` is `"rejected"`.
/// An `allowed_warning` is [`parse_rate_limit_warning`]'s; every other status is the CLI
/// reporting where it stands, not that it stopped.
///
/// `rateLimitType` is kept as whatever string the CLI sent rather than matched against a known
/// set: a window name this crate has never seen must still carry its own `resetsAt` forward
/// instead of being dropped as unrecognised. `resetsAt` itself is left as `Option` — a missing
/// or unparseable value is the scheduler's cue to fall back to ordinary backoff rather than
/// guess a pause length.
pub(crate) fn parse_rate_limit_event(v: &serde_json::Value) -> Option<RateLimitSignal> {
    let info = v.get("rate_limit_info")?;
    if info.get("status").and_then(|s| s.as_str()) != Some("rejected") {
        return None;
    }
    let kind = info.get("rateLimitType").and_then(|s| s.as_str()).unwrap_or("unknown").to_string();
    let resets_at = info.get("resetsAt").and_then(|r| r.as_i64());
    Some(RateLimitSignal { kind, resets_at })
}

/// Reads an `allowed_warning` off a `rate_limit_event` line, when it is a `five_hour` warning
/// whose top-level `utilization` is at or above `threshold` (`agent.rate_limit_warn_utilization`,
/// #184). Any other window yields `None`: `seven_day` warns from 0.75 and resets days away, so
/// pausing on it would stop dispatch for days, and its rejection still pauses as before. The
/// event's own `surpassedThreshold` is not read, since that is the 0.75. A warning without
/// `utilization` or `resetsAt` is not one this crate can judge, and yields `None`.
pub(crate) fn parse_rate_limit_warning(
    v: &serde_json::Value,
    threshold: f64,
) -> Option<RateLimitSignal> {
    let info = v.get("rate_limit_info")?;
    if info.get("status").and_then(|s| s.as_str()) != Some("allowed_warning")
        || info.get("rateLimitType").and_then(|s| s.as_str()) != Some("five_hour")
    {
        return None;
    }
    let utilization = info.get("utilization").and_then(|u| u.as_f64())?;
    let resets_at = info.get("resetsAt").and_then(|r| r.as_i64())?;
    (utilization >= threshold)
        .then(|| RateLimitSignal { kind: "five_hour".into(), resets_at: Some(resets_at) })
}

pub(crate) fn extract_text(v: &serde_json::Value) -> Option<String> {
    v.pointer("/message/content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| blocks.iter().find_map(|b| b.get("text").and_then(|t| t.as_str())))
        .map(|s| truncate(s, 120))
}

pub(crate) fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Issue body as the prompt carries it.
///
/// A `<!-- -->` left in the body is dispatched as the task (#157). Spans are
/// non-nested and dropped only outside fenced blocks and backticks. A body with
/// none is returned unchanged — this is not a rewrite of the text `session_body`
/// hashes (#109). Only the blank-line run touching a removed comment is collapsed;
/// a run elsewhere in the body is the author's and stays.
fn prompt_body(body: &str) -> String {
    let (stripped, removed) = strip_html_comments(body);
    if !removed { body.to_string() } else { stripped }
}

/// `(rewritten, removed a comment)`. An unclosed `<!--` is left in place: eating
/// the rest of the body would hide the task.
fn strip_html_comments(input: &str) -> (String, bool) {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut line_start = true;
    let mut removed = false;
    while i < input.len() {
        if let Some((end, next_line)) = skip_code(input, i, line_start) {
            out.push_str(&input[i..end]);
            i = end;
            line_start = next_line;
            continue;
        }
        if let Some(end) = comment_end(input, i) {
            removed = true;
            if comment_owns_its_lines(&out, input, end) {
                // The comment was the whole line. Drop that line, then fold only the
                // blank run that met it — a run further up the body was not this comment.
                trim_indent_before_comment(&mut out);
                let j = skip_owned_line_ending(input, end);
                i = shrink_blank_junction(&mut out, input, j);
                line_start = true;
            } else {
                i = end;
                line_start = false;
            }
            continue;
        }
        let Some(ch) = input[i..].chars().next() else { break };
        out.push(ch);
        i += ch.len_utf8();
        line_start = ch == '\n';
    }
    (out, removed)
}

/// End offset of a comment that starts at `i`.
///
/// `None` unless `i` is `<!--` and a `-->` closes it outside a fence or backticks.
/// A closer inside code is the sample quoting one, not the end of the comment; an
/// unclosed marker stays, because dropping through to EOF would hide the task.
fn comment_end(input: &str, i: usize) -> Option<usize> {
    let rest = input.get(i..)?;
    rest.strip_prefix("<!--")?;
    let mut j = i + 4;
    let mut line_start = false;
    while j < input.len() {
        if let Some((end, next_line)) = skip_code(input, j, line_start) {
            j = end;
            line_start = next_line;
            continue;
        }
        if input[j..].starts_with("-->") {
            return Some(j + 3);
        }
        let ch = input[j..].chars().next()?;
        j += ch.len_utf8();
        line_start = ch == '\n';
    }
    None
}

/// The comment occupies its own lines: only whitespace sits before it on its first
/// line and after `-->` on its last. An inline comment is not a blank-line residue.
fn comment_owns_its_lines(out: &str, input: &str, end: usize) -> bool {
    let before = match out.rfind('\n') {
        Some(i) => &out[i + 1..],
        None => out,
    };
    if !before.trim().is_empty() {
        return false;
    }
    let after = &input[end..];
    let rest = match after.find('\n') {
        Some(i) => &after[..i],
        None => after,
    };
    rest.trim().is_empty()
}

fn trim_indent_before_comment(out: &mut String) {
    let bytes = out.as_bytes();
    let mut t = bytes.len();
    while t > 0 && (bytes[t - 1] == b' ' || bytes[t - 1] == b'\t') {
        t -= 1;
    }
    out.truncate(t);
}

/// Past the newline that ended a comment's last line. The caller has already
/// checked that the rest of that line is whitespace.
fn skip_owned_line_ending(input: &str, end: usize) -> usize {
    let bytes = input.as_bytes();
    let mut j = end;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\r') {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'\n' {
        j += 1;
    }
    j
}

/// Fold the blank-line run that now meets at `i` down to one blank line.
///
/// A run of two newlines or fewer is the author's and is left byte for byte. A
/// longer run is the comment's line joining the blanks around it; only that run
/// is reduced, and only when prose sits on both sides — a leading or trailing
/// pile is residue and goes away rather than becoming a blank paragraph.
fn shrink_blank_junction(out: &mut String, input: &str, i: usize) -> usize {
    let out_bytes = out.as_bytes();
    let mut t = out.len();
    let mut trail = 0;
    while t > 0 && (out_bytes[t - 1] == b'\n' || out_bytes[t - 1] == b'\r') {
        if out_bytes[t - 1] == b'\n' {
            trail += 1;
        }
        t -= 1;
    }
    let in_bytes = input.as_bytes();
    let mut j = i;
    let mut lead = 0;
    while j < in_bytes.len() && (in_bytes[j] == b'\n' || in_bytes[j] == b'\r') {
        if in_bytes[j] == b'\n' {
            lead += 1;
        }
        j += 1;
    }
    if trail + lead <= 2 {
        return i;
    }
    let content_before = t > 0;
    let content_after = j < in_bytes.len();
    out.truncate(t);
    if content_before && content_after {
        out.push_str("\n\n");
    }
    j
}

/// A fence or a closed backtick span at `i`, and whether the byte after it is a
/// line start.
///
/// `None` when `i` is ordinary text. Missing this is how a `<!--` inside code
/// becomes a comment (#157).
fn skip_code(input: &str, i: usize, line_start: bool) -> Option<(usize, bool)> {
    if line_start && let Some(end) = consume_fence(input, i) {
        return Some((end, true));
    }
    if i < input.len()
        && input.as_bytes()[i] == b'`'
        && let Some(end) = inline_span_end(input, i)
    {
        let at_line_start = end > 0 && input.as_bytes()[end - 1] == b'\n';
        return Some((end, at_line_start));
    }
    None
}

/// Offset just past a fence that opens at `i`, which is a line start.
///
/// `None` when the line is not a fence. An unclosed fence runs to EOF, so the
/// rest is code and a later `<!--` stays — resuming the strip would treat a
/// quoted marker as a comment.
fn consume_fence(input: &str, i: usize) -> Option<usize> {
    let (content, next) = line_end(input, i);
    let (ch, len) = fence_open(&input[i..content])?;
    let mut j = next;
    while j < input.len() {
        let (cend, nnext) = line_end(input, j);
        if fence_close(&input[j..cend], ch, len) {
            return Some(nnext);
        }
        j = nnext;
    }
    Some(input.len())
}

fn line_end(input: &str, i: usize) -> (usize, usize) {
    match input[i..].find('\n') {
        Some(off) => {
            let nl = i + off;
            let content = if nl > i && input.as_bytes()[nl - 1] == b'\r' { nl - 1 } else { nl };
            (content, nl + 1)
        }
        None => (input.len(), input.len()),
    }
}

/// `(fence char, run length)` when `line` opens a fence. Indent of four or more
/// is an indented code block, not a fence, so a marker there is not protected.
fn fence_open(line: &str) -> Option<(char, usize)> {
    let rest = line.trim_start_matches(' ');
    let indent = line.len() - rest.len();
    if indent > 3 {
        return None;
    }
    let (ch, n, info) = fence_run(rest)?;
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((ch, n))
}

fn fence_close(line: &str, ch: char, len: usize) -> bool {
    let rest = line.trim_start_matches(' ');
    let indent = line.len() - rest.len();
    if indent > 3 {
        return false;
    }
    let Some((got, n, info)) = fence_run(rest) else { return false };
    got == ch && n >= len && info.trim().is_empty()
}

fn fence_run(rest: &str) -> Option<(char, usize, &str)> {
    let ch = rest.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let n = rest.bytes().take_while(|b| *b == ch as u8).count();
    if n < 3 {
        return None;
    }
    Some((ch, n, &rest[n..]))
}

/// Offset just past the closing backtick run of the same length as the one at `i`.
/// `None` when the run never closes, so the backticks stay ordinary text.
fn inline_span_end(input: &str, i: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut n = 0;
    while i + n < bytes.len() && bytes[i + n] == b'`' {
        n += 1;
    }
    if n == 0 {
        return None;
    }
    let mut j = i + n;
    while j < bytes.len() {
        if bytes[j] == b'`' {
            let mut m = 0;
            while j + m < bytes.len() && bytes[j + m] == b'`' {
                m += 1;
            }
            if m == n {
                return Some(j + m);
            }
            j += m;
            continue;
        }
        j += 1;
    }
    None
}

/// The prompt for an attempt that resumes an existing conversation.
///
/// It omits the issue body when the session already holds the current one, and most of the
/// contract always: re-sending them spends the turn budget on what the agent is about to re-read
/// anyway. A body edited since the session last saw it is sent again (#109) — it is where the
/// operator writes decisions, and a session told only to continue acts on the old text. What
/// the prompt adds is what the agent cannot see from inside: why the previous session ended,
/// which is a rebase conflict when the gate blocked it and an unfinished session otherwise.
pub(crate) fn build_continuation_prompt(
    issue: &Issue,
    tools: Option<&ToolEndpoint>,
    feedback: &[Feedback],
    wip: &[WipSnapshot],
    body_changed: bool,
) -> String {
    let why = match feedback.first() {
        Some(Feedback::Conflict { .. }) => {
            "Your previous session on this issue reported its work finished, but the \
             orchestrator could not hand the branch off; what stopped it is below."
        }
        Some(Feedback::Handoff { .. }) => {
            "Since your previous session on this issue, another agent held it; who, and why \
             it stopped, is below."
        }
        _ => {
            "Your previous session on this issue ended before the work was finished — either \
             you asked for another turn, or the orchestrator's per-session turn budget stopped \
             you."
        }
    };
    let mut p = format!(
        "Continue working on {}. {why} The working directory is the same worktree, with \
         whatever you committed still in it. Pick up where you left off.\n\n\
         The same rules apply: commit as you go, and when the work is fully complete, simply \
         stop. If you need another turn, end your final message with a line reading \
         exactly:\n\
         CREW_OUTCOME: continue: <one-sentence reason>\n\n\
         If you are stuck and need a human to unblock you, end with:\n\
         CREW_OUTCOME: blocked: <one-sentence reason>\n",
        issue.identifier
    );
    if body_changed {
        p.push_str(
            "\nThe issue description has changed since your last session. Re-read it: it may \
             carry new decisions that override what you did before.\n\nDescription:\n",
        );
        let rendered = issue.body.as_deref().map(prompt_body);
        p.push_str(rendered.as_deref().unwrap_or("(now empty)"));
        p.push('\n');
    }
    p.push_str(&feedback_help(feedback));
    p.push_str(&wip_help(wip));
    p.push_str(&tool_help(tools));
    p
}

pub(crate) fn build_prompt(
    issue: &Issue,
    tools: Option<&ToolEndpoint>,
    feedback: &[Feedback],
    wip: &[WipSnapshot],
) -> String {
    let mut p = format!("You are working on issue {}: {}\n\n", issue.identifier, issue.title);
    if let Some(url) = &issue.url {
        p.push_str(&format!("Tracker URL: {url}\n\n"));
    }
    if let Some(body) = &issue.body {
        p.push_str("Description:\n");
        p.push_str(&prompt_body(body));
        p.push_str("\n\n");
    }
    p.push_str(
        "Investigate and resolve this issue in the current working directory, committing your \
         changes as you go. When you have fully completed the work, simply stop.\n\n\
         If you have made real progress but need another turn to finish, end your final \
         message with a line reading exactly:\n\
         CREW_OUTCOME: continue: <one-sentence reason>\n\n\
         If you are stuck and need a human to unblock you, end your final message with:\n\
         CREW_OUTCOME: blocked: <one-sentence reason>\n",
    );
    p.push_str(&feedback_help(feedback));
    p.push_str(&wip_help(wip));
    p.push_str(&tool_help(tools));
    p
}

/// What the orchestrator found wrong with the previous run's output, as work.
///
/// The handoff gate's word is handed over as the retry reason it composed — which step, how
/// many tries are left, the failing output — because an agent that said `Done` and is not told
/// why it is back would re-run the same suite to rediscover the same failure, or say `Done`
/// again. A red CI is handed over as the failing check and its detail, with the instruction
/// that the job is to make it green — not to explain it. Review comments are handed over one by
/// one with their ids, and the run is asked for a verdict on each in a form this module can
/// parse back: a fix names the commit that carries it, a refusal names its reason. Comments
/// that came back unanswered from an earlier round are called out, so silence reads as noticed
/// rather than accepted.
///
/// Each item is rendered in turn, so a conflict brief that interrupted a review hand-back comes
/// first and the comments follow it in the same prompt (#160): the branch has to be brought
/// onto the base before anything else on it can land, and splitting the two cost a review
/// round on a conflict.
pub(crate) fn feedback_help(feedback: &[Feedback]) -> String {
    let mut s = String::new();
    for fb in feedback {
        match fb {
            Feedback::Gate { output } if output.trim().is_empty() => {}
            Feedback::Gate { output } => {
                s.push_str(&format!(
                    "\nFrom the orchestrator, on why this attempt was dispatched:\n{}\n",
                    output.trim_end()
                ));
            }
            Feedback::Conflict { base, base_sha, paths } => {
                s.push_str(&format!(
                    "\nThe orchestrator could not bring your branch onto {base} ({base_sha}): \
                 conflicts in {}. It was aborted, so your branch is exactly as you left it. Your \
                 job this run is to resolve that yourself — `git merge {base_sha}`, resolve each \
                 conflict (the description may say how), commit the merge — then run the \
                 project's own gate. Do not rebase: the orchestrator merges the base into a \
                 branch that has merged it, so a merge keeps your resolution. Do not report \
                 done while the branch still conflicts with {base}.\n",
                    paths.join(", ")
                ));
            }
            Feedback::Handoff { from, why, last_text } => {
                s.push_str(&format!(
                    "\nThe previous run on this issue was on another agent, `{from}`, which \
                     stopped: {why}. You are taking over from it. Whatever it committed is in \
                     this worktree; read `git log` and the diff against the base for what it \
                     did.\n"
                ));
                if let Some(t) = last_text.as_deref().filter(|t| !t.trim().is_empty()) {
                    s.push_str(&format!("\nIts last message:\n{}\n", t.trim_end()));
                }
            }
            Feedback::Ci { pr_url, failures } => {
                s.push_str(&format!(
                "\nCI is red on the pull request for this work ({pr_url}). Your job this run is \
                 to make it green: reproduce the failure locally, fix it, run the project's \
                 own gate, and commit. Do not report done while the cause below is unfixed.\n"
            ));
                for f in failures {
                    s.push_str(&format!("\n### {}", f.name));
                    if let Some(u) = &f.url {
                        s.push_str(&format!(" ({u})"));
                    }
                    s.push('\n');
                    if !f.detail.is_empty() {
                        s.push_str(&f.detail);
                        s.push('\n');
                    }
                }
            }
            Feedback::Review { pr_url, comments, unanswered_before } => {
                s.push_str(&format!(
                    "\nThe pull request for this work ({pr_url}) has review comments that need a \
                 verdict each. For every comment below, either fix what it raises and commit, \
                 or decide it should not change and say why. Do not merely acknowledge one. \
                 Then end your final message with one line per comment, exactly:\n\
                 CREW_REVIEW: <comment-id>: accepted: <commit sha that resolved it>\n\
                 CREW_REVIEW: <comment-id>: rejected: <one-sentence reason>\n"
                ));
                if !unanswered_before.is_empty() {
                    s.push_str(&format!(
                        "\nThese were handed to a previous run and came back without a verdict; \
                     they are still open: {}\n",
                        unanswered_before.join(", ")
                    ));
                }
                for c in comments {
                    let at = match (&c.path, c.line) {
                        (Some(p), Some(l)) => format!("{p}:{l}"),
                        (Some(p), None) => p.clone(),
                        _ if crate::forge::summary_review_id(&c.id).is_some() => {
                            "(review summary)".into()
                        }
                        _ => "(general)".into(),
                    };
                    s.push_str(&format!("\n[{}] {} — {}\n{}\n", c.id, at, c.author, c.body.trim()));
                }
            }
        }
    }
    s
}

/// Names every snapshot earlier removals took of this issue's uncommitted work (#22).
///
/// Without this the snapshots are saved and never found: the worktree the agent is handed is
/// clean, and nothing else in it points at a ref outside `refs/heads/`. Told, not applied,
/// because a snapshot may predate commits made since and only the agent can judge a conflict.
pub(crate) fn wip_help(wip: &[WipSnapshot]) -> String {
    if wip.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\nEarlier runs on this issue were stopped with uncommitted work, and the orchestrator \
         saved it to the refs below, oldest first — each a commit on top of the branch head it \
         was taken from, not on your branch. Decide whether each is still useful. To apply one: \
         `git cherry-pick --no-commit <ref>`. Once you have applied or discarded it, delete it \
         with `git update-ref -d <ref>` so the next run is not told about it again.\n",
    );
    for w in wip {
        s.push_str(&format!("\n`{}`:\n{}\n", w.ref_name, w.diffstat.trim_end()));
    }
    s
}

/// Names the broker's tools in the prompt.
///
/// Without this the tools are wired up and never called: they arrive in the tool list as
/// `mcp__crew__*` among everything else the operator's config provides, with nothing to say
/// they are the sanctioned way to touch the ticket. Saying so is also the only lever there is
/// against the agent reaching for ambient `gh` instead — see [`crate::broker`] on why that
/// remains possible and why this is persuasion rather than enforcement.
pub(crate) fn tool_help(tools: Option<&ToolEndpoint>) -> String {
    let Some(t) = tools else { return String::new() };

    let mut s = String::from(
        "\nTracker tools are available for this issue. The orchestrator performs each write and \
         holds the credential, so prefer these over `gh` or any other tracker CLI:\n",
    );
    for tool in &t.tools {
        let what = match tool.as_str() {
            "comment" => "post a comment on this issue",
            "set_state" => {
                "move this issue to another workflow state (it ends your run if the \
                            state is terminal, so do it last)"
            }
            "link_pr" => "link a pull request to this issue",
            _ => continue,
        };
        s.push_str(&format!("  {} — {what}\n", t.qualified(tool)));
    }
    s.push_str(
        "They act on the issue you were dispatched for and take no issue id. If one fails, the \
         failure is yours to work around, not a reason to stop.\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Issue;

    fn issue(body: &str) -> Issue {
        Issue {
            id: "iss-1".into(),
            identifier: "MT-1".into(),
            title: "t".into(),
            body: Some(body.into()),
            state: "Todo".into(),
            priority: None,
            url: None,
            labels: vec![],
            dispatchable: true,
            created_at: None,
            native_ref: None,
            blocked_by: vec![],
        }
    }

    /// Both prompts that carry the body. A continuation whose session already holds
    /// it does not, which is #109 and not this rule.
    fn prompts_carrying_the_body(body: &str) -> [String; 2] {
        let issue = issue(body);
        [
            build_prompt(&issue, None, &[], &[]),
            build_continuation_prompt(&issue, None, &[], &[], true),
        ]
    }

    #[test]
    fn html_comments_in_an_issue_body_do_not_reach_the_prompt() {
        // The gap above "Keep this gap." is the author's, not a comment's residue.
        let body = "\
Do the work.\n\
\n\
\n\
\n\
Keep this gap.\n\
\n\
<!-- single-line hint: delete me -->\n\
\n\
<!--\n\
multi-line hint: also delete me\n\
-->\n\
\n\
Then stop.\n";
        let [new, continuation] = prompts_carrying_the_body(body);
        insta::assert_snapshot!("html_comments_stripped_new", new);
        insta::assert_snapshot!("html_comments_stripped_continuation", continuation);
    }

    #[test]
    fn a_comment_marker_inside_a_code_block_is_kept() {
        let body = "\
Before.\n\
\n\
<!-- author hint -->\n\
\n\
```html\n\
<!-- fenced -->\n\
\n\
\n\
kept blank above\n\
```\n\
\n\
Inline `<!-- inline -->` and ``<!-- ticks -->``.\n";
        let [new, continuation] = prompts_carrying_the_body(body);
        insta::assert_snapshot!("comment_marker_in_code_new", new);
        insta::assert_snapshot!("comment_marker_in_code_continuation", continuation);
    }
}
