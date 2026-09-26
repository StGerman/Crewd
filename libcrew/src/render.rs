//! Plain text for [`Snapshot`] and [`Row`], for `crewctl status`.
//!
//! The acceptance criterion is "readable without `jq`", and the thing that makes a CLI fail it
//! is not missing data — it is a wall of it. So the table carries what an operator scans for
//! (what is running, how long, what broke) and the per-issue view carries everything else,
//! including run history.
//!
//! Formatting is borrowed from the dashboard rather than reinvented: [`fmt_count`], [`fmt_ms`]
//! and [`Phase::label`] are the same functions the TUI renders through, so `4m12s`, `82.1k`
//! and `quarantine` mean the same thing on both screens and in the database column. The
//! module doc on [`Phase`] makes that a rule; this is the third surface bound by it.
//!
//! Everything here is a pure function of the published types. No terminal, no colour, no
//! width probing — output that survives a pipe into `grep` is worth more here than output that
//! looks its best in a wide window.

use crate::fmt::{fmt_count, fmt_ms, timestamp};
use crate::{Phase, Row, RunRecord, Snapshot, TokenUsage};

/// What a missing value prints as, everywhere. The same mark the dashboard's table uses, and
/// never `0` — an unreported cost is unknown, not free.
const NONE: &str = "-";

/// The whole snapshot: a header an operator reads once, then one line per issue.
pub fn snapshot(snap: &Snapshot, addr: &str) -> String {
    let mut out = String::new();

    out.push_str(&format!("crewd @ {addr}  —  {} running", snap.running));
    if snap.reserved > 0 {
        out.push_str(&format!(" + {} reserved", snap.reserved));
    }
    out.push_str(&format!(" / {} limit", snap.limit));
    if snap.retrying > 0 {
        out.push_str(&format!(", {} retrying", snap.retrying));
    }
    if snap.quarantined > 0 {
        out.push_str(&format!(", {} quarantined", snap.quarantined));
    }
    out.push('\n');

    // Named so an idle daemon reads as "waiting on a limit" rather than as broken — the whole
    // point of publishing this at all (#37). Its own line, ahead of the tick line: an operator
    // deciding whether to restart the daemon needs this before anything else here. One line per
    // worker, because a pause on one leaves the others dispatching (#119).
    for p in &snap.rate_limit_pauses {
        out.push_str(&format!(
            "{} waiting on a {} limit until {}\n",
            if p.worker.is_empty() { "dispatch" } else { p.worker.as_str() },
            p.kind.replace('_', "-"),
            timestamp(p.resets_at)
        ));
    }

    out.push_str(&format!("tick {}", snap.ticks));
    if let Some(at) = snap.last_tick_at {
        out.push_str(&format!(", last at {}", timestamp(at)));
        // Against the snapshot's own clock, not this machine's: the answer stays right when
        // the daemon is on another host, and it needs no local clock to be trusted.
        out.push_str(&format!(
            " ({} ago)",
            fmt_ms(snap.generated_at.saturating_sub(at).max(0) as u64)
        ));
    }
    out.push('\n');

    // Never a bare total: the runs that reported nothing are the ones that were killed or
    // crashed, which are not the cheap ones, so the sum is always a floor and has to read like
    // one. All-uncounted is its own sentence rather than `0 in / 0 out`, which would say the
    // work was free when it means nothing was measured.
    match (snap.tokens, snap.uncounted_runs) {
        (t, 0) => out.push_str(&format!(
            "tokens {} in / {} out\n",
            fmt_count(t.input),
            fmt_count(t.output)
        )),
        (t, n) if t.input == 0 && t.output == 0 => {
            out.push_str(&format!("tokens not reported by any of {n} finished runs\n"))
        }
        (t, n) => out.push_str(&format!(
            "tokens at least {} in / {} out ({n} finished runs reported none)\n",
            fmt_count(t.input),
            fmt_count(t.output)
        )),
    }

    if let Some(err) = &snap.last_error {
        out.push_str(&format!("last error: {err}\n"));
    }

    if snap.rows.is_empty() {
        out.push_str("\nno issues tracked yet\n");
        return out;
    }

    out.push('\n');
    out.push_str(&table(&snap.rows));
    out
}

/// Rows as a table, sized to its widest cell so nothing is truncated.
///
/// Titles are the one exception: they go last and are clipped, because a tracker title has no
/// bound and one long one would push every column an operator came to read off the screen.
fn table(rows: &[Row]) -> String {
    const TITLE_MAX: usize = 44;

    let header =
        ["PHASE", "IDENTIFIER", "WORKER", "ATT", "TURNS", "AGE", "TOKENS", "STATE", "TITLE"];
    let mut cells: Vec<Vec<String>> = vec![header.iter().map(|h| h.to_string()).collect()];

    for r in rows {
        cells.push(vec![
            r.phase.label().to_string(),
            r.identifier.clone(),
            r.worker.clone().unwrap_or_else(|| NONE.into()),
            if r.attempt == 0 { NONE.into() } else { r.attempt.to_string() },
            r.turns.to_string(),
            age_of(r),
            tokens_of(r),
            if r.tracker_state.is_empty() { NONE.into() } else { r.tracker_state.clone() },
            clip(&r.title, TITLE_MAX),
        ]);
    }

    let widths: Vec<usize> = (0..header.len())
        .map(|i| cells.iter().map(|row| row[i].chars().count()).max().unwrap_or(0))
        .collect();

    let mut out = String::new();
    for row in &cells {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            // The last column is never padded; trailing whitespace is noise in a pipe.
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:<width$}  ", width = widths[i]));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }

    // Anything that needs a sentence goes under its row rather than into a column, so the
    // table keeps its shape when one issue has a long error and the rest do not.
    let mut notes = String::new();
    for r in rows {
        if let Some(due) = r.retry_in_ms {
            let when = if due > 0 { format!("in {}", fmt_ms(due as u64)) } else { "now".into() };
            let held = if r.holds_slot { ", holding its slot" } else { "" };
            notes.push_str(&format!("  {} retries {when}{held}\n", r.identifier));
        }
        if let Some(err) = &r.last_error {
            notes.push_str(&format!("  {} last error: {err}\n", r.identifier));
        }
    }
    if !notes.is_empty() {
        out.push('\n');
        out.push_str(&notes);
    }
    out
}

/// One issue in full — the acceptance criterion's "phase, attempt, turns, cost and the
/// branch", plus the run history that says whether this is the first try or the fourth.
pub fn issue(r: &Row) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}  {}\n", r.identifier, r.issue_id));
    if !r.title.is_empty() {
        out.push_str(&format!("{}\n", r.title));
    }
    out.push('\n');

    let mut field = |label: &str, value: String| {
        out.push_str(&format!("  {label:<11}{value}\n"));
    };

    field("phase", r.phase.label().to_string());
    field("worker", r.worker.clone().unwrap_or_else(|| NONE.into()));
    field("state", or_none(&r.tracker_state));
    field(
        "attempt",
        match r.attempt {
            0 => "first".into(),
            n => n.to_string(),
        },
    );
    field("turns", r.turns.to_string());
    field(
        "tokens",
        match effective_tokens(r) {
            Some(t) => format!("{} in / {} out", fmt_count(t.input), fmt_count(t.output)),
            // The dashboard's distinction, kept: a run still going has not reported yet, which
            // is not the same as one that ended without reporting at all.
            None if r.phase == Phase::Running => "not yet reported".into(),
            None => "not reported".into(),
        },
    );
    if r.age_ms > 0 {
        field("age", fmt_ms(r.age_ms));
    }
    // The branch outlives the worktree, so it is listed even when the directory is gone —
    // that is the state a reviewer most often finds an issue in.
    field("branch", r.branch.clone().unwrap_or_else(|| NONE.into()));
    // Listed for the same reason as the branch, and with the same always-present shape: the
    // transcript is what a post-mortem reads, and it is written beside the worktrees precisely
    // so it survives the directory being cleaned up. A row rendered without it would leave an
    // operator guessing whether transcripts are off or the run simply never wrote one.
    field("transcript", r.transcript.clone().unwrap_or_else(|| NONE.into()));
    // The pull request is the thing a finished run is *for*, so it is listed with its stage
    // and the round counts — the numbers that say how close the loop is to handing off.
    if let Some(d) = &r.delivery {
        let pr = match (&d.pr_url, d.pr_number) {
            (Some(url), _) => url.clone(),
            (None, Some(n)) => format!("#{n}"),
            (None, None) => NONE.into(),
        };
        field("pull request", pr);
        field(
            "delivery",
            format!("{}  (rounds {} on pr, {} on issue)", d.stage, d.rounds_pr, d.rounds_issue),
        );
        if let Some(b) = &d.base {
            field("base", b.clone());
        }
        if let Some(e) = &d.review_error {
            field("review", e.clone());
        }
        if let Some(why) = &d.handoff_reason {
            field("handed off", why.clone());
        }
    }
    if let Some(ws) = &r.workspace {
        field("workspace", ws.clone());
    }
    if let Some(url) = &r.url {
        field("url", url.clone());
    }
    if let Some(due) = r.retry_in_ms {
        let when = if due > 0 { format!("in {}", fmt_ms(due as u64)) } else { "due now".into() };
        let held = if r.holds_slot { ", holding its slot" } else { "" };
        field("retry", format!("{when}{held}"));
    }
    if let Some(ev) = &r.last_event {
        field("last event", ev.clone());
    }
    if let Some(err) = &r.last_error {
        field("error", err.clone());
    }
    if r.quarantined {
        field("quarantined", "yes — clear it with POST /api/v1/unquarantine".into());
    }

    if !r.runs.is_empty() {
        out.push_str("\n  runs (newest first)\n");
        for run in &r.runs {
            out.push_str(&format!("    {}\n", run_line(run)));
        }
    }
    out
}

fn run_line(run: &RunRecord) -> String {
    let ended = match run.ended_at {
        Some(t) => timestamp(t),
        // Distinguishable from a run that ended: still going, or killed with the orchestrator
        // and not yet closed by the next startup's recover().
        None => "still open".to_string(),
    };
    let cost = match (run.in_tok, run.out_tok) {
        (Some(i), Some(o)) => format!("{}/{}", fmt_count(i), fmt_count(o)),
        _ => NONE.to_string(),
    };
    format!(
        "{}  started {}  ended {}  turns {:<4}  outcome {:<9}  tokens {}  worker {}  model {}",
        run.run_id,
        timestamp(run.started_at),
        ended,
        run.turns,
        run.outcome.as_deref().unwrap_or(NONE),
        cost,
        run.worker.as_deref().unwrap_or(NONE),
        run.model_label()
    )
}

fn age_of(r: &Row) -> String {
    if r.age_ms == 0 { NONE.into() } else { fmt_ms(r.age_ms) }
}

fn tokens_of(r: &Row) -> String {
    match effective_tokens(r) {
        Some(t) => format!("{}/{}", fmt_count(t.input), fmt_count(t.output)),
        None => NONE.into(),
    }
}

/// The totals to show for a row. `Row.tokens` is live progress and is only ever populated for
/// the run currently in flight, so it is `None` for every row that is not `Running` even when
/// that issue's last run finished with a known cost sitting right there in `r.runs`. For any
/// other phase, fall back to the newest run that actually reported a `result` event.
///
/// Deliberately the newest reporting run, not a sum over all of them: `Row.tokens` has always
/// meant one run's cost, not a lifetime total for the issue (that lifetime figure is the
/// snapshot-level `tokens`/`uncounted_runs` pair, which already sums across finished runs on
/// purpose). Keeping this to one run preserves that meaning instead of quietly turning a
/// per-run figure into a different, larger number. A run that ended without a total (killed,
/// crashed, cut off by the turn budget) is skipped rather than treated as zero — `find_map`
/// only stops at a run that reported both halves of its usage.
fn effective_tokens(r: &Row) -> Option<TokenUsage> {
    if r.phase == Phase::Running {
        return r.tokens;
    }
    r.runs.iter().find_map(|run| match (run.in_tok, run.out_tok) {
        (Some(input), Some(output)) => Some(TokenUsage { input, output }),
        _ => None,
    })
}

fn or_none(s: &str) -> String {
    if s.is_empty() { NONE.into() } else { s.to_string() }
}

fn clip(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RateLimitPause;

    /// The whole point of a transcript is that it is readable after the run is over, so the
    /// surface an operator actually types has to name it — a path that only reaches `--json`
    /// is the `jq` recipe the status client exists to replace.
    #[test]
    fn the_detail_view_names_the_transcript_of_a_finished_run() {
        let mut r = row("MT-7", Phase::Released);
        r.transcript = Some("/tmp/.transcripts/MT-7-1770000000000-abc.jsonl".into());
        let out = issue(&r);
        assert!(
            out.contains("/tmp/.transcripts/MT-7-1770000000000-abc.jsonl"),
            "the detail view must name the transcript path:\n{out}"
        );

        let mut none = row("MT-8", Phase::Released);
        none.transcript = None;
        assert!(
            issue(&none).contains("transcript"),
            "a run without a transcript still says so, rather than omitting the field"
        );
    }

    fn row(identifier: &str, phase: Phase) -> Row {
        Row {
            issue_id: format!("iss-{identifier}"),
            identifier: identifier.into(),
            title: "do the thing".into(),
            tracker_state: "in progress".into(),
            phase,
            ..Default::default()
        }
    }

    #[test]
    fn a_dates_conversion_matches_known_instants() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00Z");
        // 2026-09-12T09:31:02Z, the kind of value a live snapshot carries.
        assert_eq!(timestamp(1_789_205_462_000), "2026-09-12 09:31:02Z");
        // A leap day, which an off-by-one in the era arithmetic would move.
        assert_eq!(timestamp(1_709_164_800_000), "2024-02-29 00:00:00Z");
    }

    #[test]
    fn the_summary_names_every_number_an_operator_would_otherwise_grep_for() {
        let snap = Snapshot {
            generated_at: 1_789_205_462_000,
            last_tick_at: Some(1_789_205_450_000),
            ticks: 47,
            running: 2,
            reserved: 0,
            limit: 3,
            retrying: 1,
            quarantined: 1,
            tokens: TokenUsage { input: 1_200_000, output: 48_300 },
            uncounted_runs: 2,
            rows: vec![row("MT-601", Phase::Running)],
            last_error: None,
            rate_limit_pauses: vec![],
        };

        let out = snapshot(&snap, "127.0.0.1:8787");
        assert!(out.contains("127.0.0.1:8787"), "{out}");
        assert!(out.contains("2 running / 3 limit"), "{out}");
        assert!(out.contains("1 retrying"), "{out}");
        assert!(out.contains("1 quarantined"), "{out}");
        assert!(out.contains("tick 47"), "{out}");
        assert!(out.contains("12s ago"), "age is measured against the snapshot's clock: {out}");
        assert!(out.contains("at least 1.2M in / 48.3k out"), "{out}");
        assert!(out.contains("2 finished runs reported none"), "a floor, not a total: {out}");
        assert!(out.contains("MT-601"), "{out}");
    }

    /// #86: a continuation holding its slot is capacity in use, so a full daemon with nothing
    /// running must say which issue holds the slot rather than read as idle.
    #[test]
    fn a_reserved_slot_reads_as_taken_and_names_the_issue_holding_it() {
        let mut held = row("MT-64", Phase::RetryQueued);
        held.retry_in_ms = Some(5_000);
        held.holds_slot = true;
        let snap = Snapshot { reserved: 1, limit: 1, rows: vec![held], ..Default::default() };

        let out = snapshot(&snap, "127.0.0.1:8787");
        assert!(out.contains("0 running + 1 reserved / 1 limit"), "{out}");
        assert!(out.contains("MT-64 retries in 5s, holding its slot"), "{out}");
    }

    /// #37: an idle daemon and a daemon waiting out an account-wide rate limit must not read
    /// the same to an operator deciding whether to restart it.
    #[test]
    fn a_rate_limit_pause_says_why_nothing_is_dispatching_and_when_that_ends() {
        let snap = Snapshot {
            rate_limit_pauses: vec![RateLimitPause {
                worker: "claude".into(),
                kind: "five_hour".into(),
                resets_at: 1_789_981_200_000,
            }],
            ..Default::default()
        };
        let out = snapshot(&snap, "x");
        assert!(out.contains("claude waiting on a five-hour limit until"), "{out}");
        assert!(out.contains(&timestamp(1_789_981_200_000)), "{out}");
    }

    #[test]
    fn a_cost_nothing_reported_does_not_print_as_a_free_run() {
        // What the live daemon showed: 28 finished runs, none of which reported a total.
        // `0 in / 0 out` there would claim the work cost nothing.
        let snap = Snapshot { uncounted_runs: 28, ..Default::default() };
        let out = snapshot(&snap, "x");
        assert!(out.contains("not reported by any of 28 finished runs"), "{out}");
        assert!(!out.contains("0 in / 0 out"), "{out}");
    }

    #[test]
    fn an_empty_snapshot_says_so_instead_of_printing_a_bare_header() {
        let out = snapshot(&Snapshot { limit: 3, ..Default::default() }, "127.0.0.1:8787");
        assert!(out.contains("no issues tracked yet"), "{out}");
    }

    #[test]
    fn the_table_columns_line_up_whatever_the_identifier_lengths_are() {
        // A column that drifts is the difference between scanning a table and reading it.
        let snap = Snapshot {
            limit: 3,
            rows: vec![row("MT-1", Phase::Running), row("a-very-long-identifier", Phase::Queued)],
            ..Default::default()
        };
        let out = snapshot(&snap, "x");

        // The identifier is the second column, so it starts wherever the widest phase ends.
        let offsets: Vec<usize> = out
            .lines()
            .filter(|l| l.contains("IDENTIFIER") || l.contains("MT-1") || l.contains("a-very-long"))
            .map(|l| {
                let second = l.split_whitespace().nth(1).expect("a second column");
                l.find(second).expect("the cell is on its line")
            })
            .collect();
        assert_eq!(offsets.len(), 3, "header and both rows: {out}");
        assert!(offsets.windows(2).all(|w| w[0] == w[1]), "columns drifted: {out}");
    }

    #[test]
    fn a_long_title_is_clipped_so_it_cannot_push_the_columns_off_screen() {
        let mut r = row("MT-1", Phase::Running);
        r.title = "x".repeat(200);
        let out = snapshot(&Snapshot { rows: vec![r], ..Default::default() }, "x");
        assert!(out.contains('…'), "a long title must be marked as clipped: {out}");
        assert!(out.lines().all(|l| l.chars().count() < 140), "a line escaped the layout: {out}");
    }

    #[test]
    fn the_detail_view_answers_phase_attempt_turns_cost_and_branch() {
        // The five fields issue #24 names, in one place, for one id.
        let r = Row {
            phase: Phase::Running,
            attempt: 2,
            turns: 12,
            tokens: Some(TokenUsage { input: 82_100, output: 3_400 }),
            branch: Some("crew/MT-601-a1b2c3d4e5f6".into()),
            workspace: Some("/tmp/ws/MT-601-a1b2c3d4e5f6".into()),
            age_ms: 252_000,
            ..row("MT-601", Phase::Running)
        };

        let out = issue(&r);
        assert!(out.contains("running"), "{out}");
        assert!(out.contains("attempt    2"), "{out}");
        assert!(out.contains("turns      12"), "{out}");
        assert!(out.contains("82.1k in / 3.4k out"), "{out}");
        assert!(out.contains("crew/MT-601-a1b2c3d4e5f6"), "{out}");
        assert!(out.contains("4m12s"), "{out}");
    }

    #[test]
    fn an_unreported_cost_never_reads_as_a_free_run() {
        // Zero would say "cheap"; these two runs are "unknown" and "unknown, and it is over".
        let running = Row { tokens: None, ..row("MT-1", Phase::Running) };
        assert!(issue(&running).contains("not yet reported"));

        let ended = Row { tokens: None, ..row("MT-2", Phase::Released) };
        let out = issue(&ended);
        assert!(out.contains("not reported") && !out.contains("not yet"), "{out}");
    }

    #[test]
    fn a_finished_rows_cost_falls_back_to_its_newest_run_that_reported_one() {
        // Row.tokens is live progress and goes back to None once the run stops — the cost did
        // not disappear, it is sitting in `runs`. The review finding: the header used to say
        // "not reported" here even though the run history right below it carried a real total.
        let r = Row {
            tokens: None,
            runs: vec![
                // Newest run first, and it reported nothing (killed/crashed/cut off) — it must
                // be skipped rather than read as a free run, so the older run's total is what
                // should surface.
                RunRecord {
                    run_id: "run-2".into(),
                    issue_id: "iss-MT-7".into(),
                    started_at: 1_789_205_400_000,
                    ended_at: Some(1_789_205_500_000),
                    outcome: Some("blocked".into()),
                    session_id: None,
                    turns: 3,
                    in_tok: None,
                    out_tok: None,
                    transcript: None,
                    model: None,
                    effort: None,
                    worker: None,
                },
                RunRecord {
                    run_id: "run-1".into(),
                    issue_id: "iss-MT-7".into(),
                    started_at: 1_789_200_000_000,
                    ended_at: Some(1_789_200_100_000),
                    outcome: Some("continue".into()),
                    session_id: None,
                    turns: 6,
                    in_tok: Some(9_000),
                    out_tok: Some(1_500),
                    transcript: None,
                    model: None,
                    effort: None,
                    worker: None,
                },
            ],
            ..row("MT-7", Phase::Released)
        };

        let out = issue(&r);
        assert!(out.contains("9.0k in / 1.5k out"), "{out}");
        assert!(!out.contains("not reported"), "{out}");

        // The table's TOKENS column is the same fallback, not a second, inconsistent answer.
        let table_out = snapshot(&Snapshot { rows: vec![r], ..Default::default() }, "x");
        assert!(table_out.contains("9.0k/1.5k"), "{table_out}");
    }

    #[test]
    fn a_finished_issue_still_reports_the_branch_its_work_is_on() {
        // The worktree is gone — cleanup deleted it — and the branch is the whole point.
        let r = Row {
            phase: Phase::Released,
            workspace: None,
            branch: Some("crew/MT-9-deadbeef".into()),
            ..row("MT-9", Phase::Released)
        };
        let out = issue(&r);
        assert!(out.contains("crew/MT-9-deadbeef"), "{out}");
        assert!(!out.contains("workspace"), "a deleted worktree must not be listed: {out}");
    }

    #[test]
    fn run_history_distinguishes_a_run_still_open_from_one_that_ended() {
        let r = Row {
            runs: vec![
                RunRecord {
                    run_id: "run-9".into(),
                    issue_id: "iss-MT-1".into(),
                    started_at: 1_789_205_400_000,
                    ended_at: None,
                    outcome: None,
                    session_id: None,
                    turns: 12,
                    in_tok: None,
                    out_tok: None,
                    transcript: None,
                    model: None,
                    effort: None,
                    worker: None,
                },
                RunRecord {
                    run_id: "run-8".into(),
                    issue_id: "iss-MT-1".into(),
                    started_at: 1_789_205_000_000,
                    ended_at: Some(1_789_205_200_000),
                    outcome: Some("continue".into()),
                    session_id: None,
                    turns: 20,
                    in_tok: Some(130_400),
                    out_tok: Some(5_110),
                    transcript: None,
                    model: None,
                    effort: None,
                    worker: None,
                },
            ],
            ..row("MT-1", Phase::Running)
        };
        let out = issue(&r);
        assert!(out.contains("still open"), "{out}");
        assert!(out.contains("outcome continue"), "{out}");
        assert!(out.contains("130.4k/5.1k"), "{out}");
        // The open run reported nothing, and that must not print as zero.
        assert!(out.contains("tokens -"), "{out}");
    }

    #[test]
    fn a_retry_and_an_error_appear_under_the_table_rather_than_inside_it() {
        let r = Row {
            retry_in_ms: Some(45_000),
            last_error: Some("agent_crash: exited unexpectedly".into()),
            ..row("MT-3", Phase::RetryQueued)
        };
        let out = snapshot(&Snapshot { rows: vec![r], ..Default::default() }, "x");
        assert!(out.contains("MT-3 retries in 45s"), "{out}");
        assert!(out.contains("last error: agent_crash"), "{out}");
    }

    /// #119: with two workers the table has to say who is working on what, and a pause has to
    /// name the worker it stops — the other one is still dispatching.
    #[test]
    fn the_table_names_each_rows_worker_and_each_pause_names_its_worker() {
        let mut claude = row("MT-1", Phase::Running);
        claude.worker = Some("claude".into());
        claude.attempt = 1;
        claude.turns = 4;
        claude.age_ms = 65_000;
        let mut grok = row("MT-2", Phase::Running);
        grok.worker = Some("grok".into());
        grok.attempt = 1;
        grok.turns = 2;
        grok.age_ms = 12_000;
        let never = row("MT-3", Phase::Queued);
        let snap = Snapshot {
            generated_at: 1_789_205_462_000,
            last_tick_at: Some(1_789_205_462_000),
            ticks: 9,
            running: 2,
            limit: 2,
            rows: vec![claude, grok, never],
            rate_limit_pauses: vec![RateLimitPause {
                worker: "claude".into(),
                kind: "five_hour".into(),
                resets_at: 1_789_981_200_000,
            }],
            ..Default::default()
        };
        insta::assert_snapshot!(snapshot(&snap, "127.0.0.1:8787"));
    }
}
