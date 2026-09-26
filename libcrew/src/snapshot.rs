//! The published view: what the daemon's scheduler writes into its snapshot and every observer
//! reads back. One definition shared by the daemon and `crewctl`, so a renamed field fails the
//! build on both sides rather than rendering a blank column on one of them (#45).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Row {
    pub issue_id: String,
    pub identifier: String,
    pub title: String,
    pub url: Option<String>,
    pub tracker_state: String,
    pub phase: Phase,
    pub attempt: u32,
    pub turns: u32,
    /// The current run's totals, once its `result` event has supplied them. `None` while it is
    /// in flight and for every row that is not running.
    pub tokens: Option<TokenUsage>,
    pub age_ms: u64,
    pub retry_in_ms: Option<i64>,
    /// A continuation waiting out its delay with its concurrency slot held (#86): counted in
    /// [`Snapshot::reserved`], and named here so a saturated daemon says who holds the slot.
    #[serde(default)]
    pub holds_slot: bool,
    pub quarantined: bool,
    pub last_error: Option<String>,
    pub last_event: Option<String>,
    pub workspace: Option<String>,
    /// The branch this issue's most recent dispatch actually checked out, recorded at that
    /// call rather than recomputed from `identifier` — see `Store::set_branch`.
    ///
    /// Outlives `workspace`, and deliberately: the worktree directory is scratch that cleanup
    /// deletes, while the branch is what a finished run leaves behind for a reviewer to find.
    /// `None` for an issue never dispatched — naming a branch that was never written would
    /// send that reviewer after nothing — for a `DirWorkspace` deployment,
    /// which has no branches at all, and once cleanup deletes a branch that turned out to carry
    /// nothing new: `None` here is always either of those, never a ref that is already gone.
    pub branch: Option<String>,
    /// This issue's most recent runs, newest first, at most the daemon's `RUNS_PER_ISSUE`.
    pub runs: Vec<RunRecord>,
    /// The most recent run's transcript, so "show me what this issue did" is one path away
    /// from the dashboard rather than a layout someone has to know.
    pub transcript: Option<String>,
    /// Where the branch is on its way to a mergeable pull request, once a run has reported
    /// done with delivery on. `None` before that, and always for a deployment without a forge.
    pub delivery: Option<DeliveryView>,
    /// The worker running this issue, or else the one that ran its latest run (#119). `None`
    /// for an issue never dispatched, or last dispatched by a daemon that did not record it.
    #[serde(default)]
    pub worker: Option<String>,
}

/// Immutable view published to observers. The TUI renders this and never touches the store,
/// and neither does the HTTP API ([`crate::api`]), which is what keeps either from becoming
/// load-bearing.
///
/// It follows that this type is the *whole* published view: an observer that needs something
/// it does not carry does not get a `Store`, it gets a new field here. That is why run history
/// lives on [`Row`] rather than being read back out of the database by whoever wants it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub generated_at: i64,
    pub rows: Vec<Row>,
    /// Runs holding a slot, gating ones included — the scheduler's own count, not the agents
    /// alive right now.
    pub running: usize,
    /// Slots held by continuations between sessions (#86). `running + reserved` is what the
    /// scheduler compares with `limit`; defaulted so a client reads an older daemon.
    #[serde(default)]
    pub reserved: usize,
    pub limit: usize,
    pub retrying: usize,
    pub quarantined: usize,
    /// Summed over every run that reported a total.
    pub tokens: TokenUsage,
    /// Finished runs that reported none — killed, crashed, or budget-cut. Shown next to the sum
    /// so it reads as the lower bound it is.
    pub uncounted_runs: u64,
    pub ticks: u64,
    pub last_tick_at: Option<i64>,
    pub last_error: Option<String>,
    /// One entry per worker whose dispatch is paused for an account-wide rate limit its agent
    /// CLI reported (#37), in dispatch order; a pause on one worker leaves the others
    /// dispatching (#119). Empty when nothing is paused for this reason — which is not the same
    /// as "nothing is wrong"; see `last_error` for an ordinary failure.
    ///
    /// On the wire it travels beside the singleton `rate_limit_pause` it replaced, so a client
    /// and a daemon on either side of #119 still see a pause while the API marker says `1`.
    #[serde(flatten, with = "pauses_wire")]
    pub rate_limit_pauses: Vec<RateLimitPause>,
}

/// The pause list plus the pre-#119 singleton. Without the singleton an older client reads no
/// `rate_limit_pause` and shows an idle daemon as healthy; without reading it back, a newer
/// client does the same against an older daemon.
mod pauses_wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::RateLimitPause;

    #[derive(Serialize, Deserialize)]
    struct Wire {
        #[serde(default)]
        rate_limit_pauses: Option<Vec<RateLimitPause>>,
        #[serde(default)]
        rate_limit_pause: Option<RateLimitPause>,
    }

    pub(super) fn serialize<S: Serializer>(v: &[RateLimitPause], s: S) -> Result<S::Ok, S::Error> {
        Wire { rate_limit_pauses: Some(v.to_vec()), rate_limit_pause: v.first().cloned() }
            .serialize(s)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<RateLimitPause>, D::Error> {
        let w = Wire::deserialize(d)?;
        Ok(w.rate_limit_pauses.unwrap_or_else(|| w.rate_limit_pause.into_iter().collect()))
    }
}

/// An account-wide dispatch pause, published so an operator sees *why* nothing is running
/// rather than an idle daemon with no explanation (#37).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitPause {
    /// The worker whose account is limited, and the only one that stops dispatching (#119).
    #[serde(default)]
    pub worker: String,
    /// Whatever the CLI named the exhausted window — `"five_hour"`, `"seven_day"`, or a name
    /// this crate has never seen.
    pub kind: String,
    /// Wall-clock milliseconds — the same units as [`Snapshot::generated_at`] — at which
    /// dispatch resumes.
    pub resets_at: i64,
}

/// What the published snapshot says about an issue's delivery. A view, so that observers get
/// the stage and the pull request without a `Store` — rule 3, same as every other `Row` field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryView {
    pub stage: String,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub base: Option<String>,
    pub rounds_pr: u32,
    pub rounds_issue: u32,
    pub review_error: Option<String>,
    pub handoff_reason: Option<String>,
}

/// One dispatched run, as the published snapshot carries it.
///
/// `ended_at` and `outcome` are `None` while the run is in flight — and stay `None` for a run
/// whose process was killed with the orchestrator, until the next startup's `recover()` closes
/// it. `turns` is checkpointed while the run is in flight (`Store::record_progress`, once per
/// tick) and made final by `finish_run`, so a run that died with its process reports what it
/// had reached at the last tick rather than zero. The token columns have no such checkpoint —
/// the CLI reports a total once, at the end, or never.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub issue_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub outcome: Option<String>,
    pub session_id: Option<String>,
    pub turns: u32,
    /// `None` when the run ended without the CLI reporting a total — killed,
    /// crashed, or cut off by the session turn budget. Not zero: unknown.
    pub in_tok: Option<u64>,
    pub out_tok: Option<u64>,
    /// Path to this run's raw event stream, when one was written. How an operator gets from
    /// "run X went wrong" to the bytes it produced, without knowing where transcripts are kept.
    pub transcript: Option<String>,
    /// The `--model` this run was dispatched with; `None` when none was passed. Strings rather
    /// than the daemon's `ModelChoice` because a recorded level must stay readable after the CLI, and so
    /// `Effort`, stops offering it.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The worker that ran it (#119), and so the one holding its session. `None` for a run
    /// recorded before workers were named.
    #[serde(default)]
    pub worker: Option<String>,
}

impl RunRecord {
    /// `model/effort`, for every observer that shows a run. A field dispatched with no flag reads
    /// `default` rather than naming today's default, which is exactly the value the run may not
    /// have had.
    pub fn model_label(&self) -> String {
        match (&self.model, &self.effort) {
            (None, None) => "cli default".into(),
            (m, e) => format!(
                "{}/{}",
                m.as_deref().unwrap_or("default"),
                e.as_deref().unwrap_or("default")
            ),
        }
    }
}

/// Token totals for one run, as reported by the agent CLI itself in its terminal `result` event.
///
/// Taken from there and nowhere else. Summing the `usage` block of each streamed `assistant`
/// event looked equivalent and was not, in both directions: the CLI emits one `assistant` event
/// per content block, each carrying the whole turn's usage, so a thinking-then-text turn is
/// counted twice; and the per-event `output_tokens` is a streaming placeholder that reads `1`
/// for a full paragraph. The first live dispatch recorded ten million input tokens and four
/// hundred output tokens over eighty-three turns that way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt-side tokens billed for the run: fresh input plus cache creation plus cache reads.
    /// One figure rather than three because the dashboard has one column; the split is in the
    /// CLI's own transcript if a cost breakdown is ever needed.
    pub input: u64,
    pub output: u64,
}

/// The orchestrator's claim state for an issue. Distinct from tracker state.
///
/// The serde spelling is pinned to [`Phase::label`], which is also what the store writes into
/// its `phase` column and what the dashboard prints. One word per phase everywhere it is
/// visible means an operator reading the HTTP API, the database and the TUI side by side never
/// has to translate between three vocabularies for the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Phase {
    #[serde(rename = "queued")]
    Queued,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "retry")]
    RetryQueued,
    #[serde(rename = "quarantine")]
    Quarantined,
    #[default]
    #[serde(rename = "released")]
    Released,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Queued => "queued",
            Phase::Running => "running",
            Phase::RetryQueued => "retry",
            Phase::Quarantined => "quarantine",
            Phase::Released => "released",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pause(worker: &str) -> RateLimitPause {
        RateLimitPause { worker: worker.into(), kind: "five_hour".into(), resets_at: 1 }
    }

    /// Copilot on #156: the list replaced a singleton while the API marker stayed `1`, so either
    /// side of the change must still see the other's pause.
    #[test]
    fn a_pause_survives_a_client_and_daemon_on_either_side_of_the_list() {
        let snap = Snapshot { rate_limit_pauses: vec![pause("claude")], ..Default::default() };
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["rate_limit_pause"]["kind"], "five_hour", "an older client reads this");
        let back: Snapshot = serde_json::from_value(json).unwrap();
        assert_eq!(back.rate_limit_pauses, vec![pause("claude")]);

        let mut old = serde_json::to_value(Snapshot::default()).unwrap();
        let map = old.as_object_mut().unwrap();
        map.remove("rate_limit_pauses");
        map.insert(
            "rate_limit_pause".into(),
            serde_json::json!({"kind": "five_hour", "resets_at": 1}),
        );
        let read: Snapshot = serde_json::from_value(old).unwrap();
        assert_eq!(read.rate_limit_pauses, vec![pause("")], "an older daemon's pause is read");
    }
}
