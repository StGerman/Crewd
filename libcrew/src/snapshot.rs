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
    /// The daemon's build string ([`crate::build()`]), so `status` names the build that is
    /// running rather than the one last installed. Empty from a daemon before #244.
    #[serde(default)]
    pub build: String,
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
    /// One entry per worker paused until the daemon restarts, in dispatch order: its binary
    /// could not be spawned (#216) or its account has no balance left (#237). Empty when every
    /// worker can run. The wire key predates the reason, so a client from before #237 still sees
    /// the pause; defaulted so a client still reads a daemon from before #216.
    #[serde(default, rename = "missing_binaries")]
    pub halted_workers: Vec<HaltedWorker>,
}

/// A worker that takes nothing until restart, published so `status` says why rather than
/// showing that worker as idle (#216). Unlike [`RateLimitPause`], nothing here lifts the pause:
/// the binary is not re-resolved and the balance is not probed while the daemon runs (#237).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaltedWorker {
    pub worker: String,
    /// The path the worker was spawned with.
    pub binary: String,
    /// Defaulted because a daemon from before #237 halted a worker only for its binary.
    #[serde(default)]
    pub reason: HaltReason,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    #[default]
    BinaryNotFound,
    /// The provider refused the account with HTTP 402: no balance, and no reset time (#237).
    AccountExhausted,
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

    /// A client newer than the daemon must not refuse a snapshot that predates the field (#216).
    #[test]
    fn a_snapshot_without_missing_binaries_reads_as_no_worker_paused_for_that() {
        let mut old = serde_json::to_value(Snapshot::default()).unwrap();
        old.as_object_mut().unwrap().remove("missing_binaries");
        let read: Snapshot = serde_json::from_value(old).unwrap();
        assert!(read.halted_workers.is_empty());
    }

    /// Every field set: each `Option` is `Some` and each list holds one entry, so the walk below
    /// reaches every key path a v1 client can read. `fully_populated_leaves_no_path_unpinned`
    /// refuses a `null` or an empty list, which is how a new field left at its default fails here
    /// instead of slipping past the shape snapshot.
    fn fully_populated() -> Snapshot {
        let run = RunRecord {
            run_id: "r".into(),
            issue_id: "i".into(),
            started_at: 1,
            ended_at: Some(2),
            outcome: Some("done".into()),
            session_id: Some("s".into()),
            turns: 3,
            in_tok: Some(4),
            out_tok: Some(5),
            transcript: Some("t.jsonl".into()),
            model: Some("opus".into()),
            effort: Some("high".into()),
            worker: Some("claude".into()),
        };
        let row = Row {
            issue_id: "i".into(),
            identifier: "#1".into(),
            title: "t".into(),
            url: Some("u".into()),
            tracker_state: "open".into(),
            phase: Phase::Running,
            attempt: 1,
            turns: 2,
            tokens: Some(TokenUsage { input: 1, output: 2 }),
            age_ms: 3,
            retry_in_ms: Some(4),
            holds_slot: true,
            quarantined: false,
            last_error: Some("e".into()),
            last_event: Some("ev".into()),
            workspace: Some("w".into()),
            branch: Some("b".into()),
            runs: vec![run],
            transcript: Some("t.jsonl".into()),
            delivery: Some(DeliveryView {
                stage: "awaiting".into(),
                pr_number: Some(1),
                pr_url: Some("p".into()),
                base: Some("master".into()),
                rounds_pr: 1,
                rounds_issue: 2,
                review_error: Some("r".into()),
                handoff_reason: Some("h".into()),
            }),
            worker: Some("claude".into()),
        };
        Snapshot {
            build: "0.1.0 (abc1234)".into(),
            generated_at: 1,
            rows: vec![row],
            running: 1,
            reserved: 1,
            limit: 2,
            retrying: 0,
            quarantined: 0,
            tokens: TokenUsage { input: 1, output: 2 },
            uncounted_runs: 0,
            ticks: 1,
            last_tick_at: Some(1),
            last_error: Some("e".into()),
            rate_limit_pauses: vec![pause("claude")],
            halted_workers: vec![HaltedWorker {
                worker: "grok".into(),
                binary: "/bin/grok".into(),
                reason: HaltReason::AccountExhausted,
            }],
        }
    }

    /// One line per key path, `path: type`, sorted; a list's element is the path plus `[]`.
    fn shape(path: &str, v: &serde_json::Value, out: &mut Vec<String>) {
        use serde_json::Value;
        let ty = match v {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(n) if n.is_f64() => "float",
            Value::Number(_) => "integer",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        if !path.is_empty() {
            out.push(format!("{path}: {ty}"));
        }
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    shape(&p, child, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    shape(&format!("{path}[]"), item, out);
                }
            }
            _ => {}
        }
    }

    fn populated_shape() -> Vec<String> {
        let mut out = Vec::new();
        shape("", &serde_json::to_value(fully_populated()).unwrap(), &mut out);
        out.sort();
        out.dedup();
        out
    }

    /// A v1 payload as the API wrote it when the promise was made (#245), with every optional
    /// field `null`. **Never edit it for an addition**: it stands for a daemon older than the
    /// addition, which a newer client still has to read.
    const FROZEN_V1: &str = include_str!("testdata/snapshot_v1_frozen.json");

    /// Key paths that may be `null`: those [`FROZEN_V1`] sends as `null`, and those the current
    /// types write back as `null` after reading it, which is how a new `Option` field shows up.
    fn nullable_paths() -> std::collections::BTreeSet<String> {
        let raw: serde_json::Value = serde_json::from_str(FROZEN_V1).unwrap();
        let read: Snapshot = serde_json::from_str(FROZEN_V1).unwrap();
        let mut lines = Vec::new();
        shape("", &raw, &mut lines);
        shape("", &serde_json::to_value(read).unwrap(), &mut lines);
        lines.iter().filter_map(|l| l.strip_suffix(": null")).map(String::from).collect()
    }

    /// [`populated_shape`] with `| null` on each path that may be `null`, so making a field
    /// required, or a new one optional, rewrites a line of the pinned snapshot.
    fn snapshot_shape() -> Vec<String> {
        let nullable = nullable_paths();
        populated_shape()
            .into_iter()
            .map(|l| {
                let path = l.split_once(": ").unwrap().0;
                if nullable.contains(path) { format!("{l} | null") } else { l }
            })
            .collect()
    }

    /// A newer client must read an older daemon (docs/api-v1.md, #245). A field added without
    /// `#[serde(default)]`, or an `Option` made required, fails to read this payload.
    #[test]
    fn a_v1_payload_from_before_any_addition_still_reads() {
        let read: Snapshot = serde_json::from_str(FROZEN_V1)
            .unwrap_or_else(|e| panic!("a v1 payload no longer reads: {e}"));
        assert_eq!(read.rows.len(), 2);
        assert_eq!(read.rows[1].runs.len(), 1);
    }

    /// Every `Phase`, in declaration order. `next` matches with no wildcard, so a new variant
    /// does not compile until it is given a place in this chain, and so in the pinned snapshot.
    fn every_phase() -> Vec<Phase> {
        fn next(p: Phase) -> Option<Phase> {
            match p {
                Phase::Queued => Some(Phase::Running),
                Phase::Running => Some(Phase::RetryQueued),
                Phase::RetryQueued => Some(Phase::Quarantined),
                Phase::Quarantined => Some(Phase::Released),
                Phase::Released => None,
            }
        }
        std::iter::successors(Some(Phase::Queued), |&p| next(p)).collect()
    }

    /// Every `HaltReason`, kept exhaustive the way [`every_phase`] is.
    fn every_halt_reason() -> Vec<HaltReason> {
        fn next(r: HaltReason) -> Option<HaltReason> {
            match r {
                HaltReason::BinaryNotFound => Some(HaltReason::AccountExhausted),
                HaltReason::AccountExhausted => None,
            }
        }
        std::iter::successors(Some(HaltReason::BinaryNotFound), |&r| next(r)).collect()
    }

    /// The v1 promise (docs/api-v1.md, #245): `/api/v1/snapshot` and `/api/v1/issues/:id` only
    /// grow. Renaming or removing a field, or changing its JSON type, rewrites a line here, and
    /// that diff is the review question "does this need `/api/v2`?". A new field adds a line,
    /// which is allowed only with `#[serde(default)]` and a row in docs/api-v1.md. The enum
    /// spellings are pinned too, because a client matches on them.
    #[test]
    fn the_v1_snapshot_shape_only_grows() {
        let mut lines = snapshot_shape();
        for p in every_phase() {
            lines.push(format!("enum phase: {}", serde_json::to_value(p).unwrap()));
        }
        for r in every_halt_reason() {
            lines.push(format!("enum reason: {}", serde_json::to_value(r).unwrap()));
        }
        insta::assert_snapshot!(lines.join("\n"));
    }

    /// A path the fixture leaves `null` or empty would be pinned as `null`, or not at all, so a
    /// later rename of it would pass. Every `Option` and list in [`fully_populated`] must be set.
    #[test]
    fn fully_populated_leaves_no_path_unpinned() {
        let lines = populated_shape();
        let unset: Vec<&String> = lines.iter().filter(|l| l.ends_with(": null")).collect();
        assert!(unset.is_empty(), "set these in fully_populated(): {unset:?}");
        let mut empty = Vec::new();
        fn empties(path: &str, v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Array(a) if a.is_empty() => out.push(path.into()),
                serde_json::Value::Array(a) => {
                    a.iter().for_each(|i| empties(&format!("{path}[]"), i, out))
                }
                serde_json::Value::Object(m) => {
                    m.iter().for_each(|(k, c)| empties(&format!("{path}.{k}"), c, out))
                }
                _ => {}
            }
        }
        empties("", &serde_json::to_value(fully_populated()).unwrap(), &mut empty);
        assert!(empty.is_empty(), "give these lists an entry in fully_populated(): {empty:?}");
    }

    /// The docs/api-v1.md section that documents the object a key path's last key sits in.
    /// Unknown parents fail rather than default, so a new nested object needs a section here.
    fn section_of(path: &str) -> &'static str {
        let parent = path.rsplit_once('.').map_or("", |(p, _)| p);
        match parent {
            "" => "Snapshot",
            "rows[]" => "Row",
            "rows[].runs[]" => "RunRecord",
            "rows[].delivery" => "DeliveryView",
            "tokens" | "rows[].tokens" => "TokenUsage",
            "rate_limit_pause" | "rate_limit_pauses[]" => "RateLimitPause",
            "missing_binaries[]" => "HaltedWorker",
            other => panic!("no docs/api-v1.md section for the object at {other:?}"),
        }
    }

    /// docs/api-v1.md is the contract a client reads, so a field the shape pins and the table
    /// leaves out is a promise nobody wrote down. Checked per section, since `issue_id`, `turns`,
    /// `transcript` and `worker` each appear in more than one object.
    #[test]
    fn api_v1_md_names_every_pinned_field() {
        let doc = include_str!("../../docs/api-v1.md");
        let section = |name: &str| -> &str {
            let start = doc
                .find(&format!("\n## {name}\n"))
                .unwrap_or_else(|| panic!("docs/api-v1.md has no section {name}"));
            let body = &doc[start + 1..];
            &body[..body[3..].find("\n## ").map_or(body.len(), |e| e + 3)]
        };
        let nullable = nullable_paths();
        let mut missing = Vec::new();
        let mut optionality = Vec::new();
        for path in populated_shape().iter().filter_map(|l| l.split(':').next()) {
            if path.ends_with("[]") {
                continue;
            }
            let key = path.rsplit('.').next().unwrap().trim_end_matches("[]");
            let row =
                section(section_of(path)).lines().find(|l| l.starts_with(&format!("| `{key}` |")));
            let Some(row) = row else {
                missing.push(format!("{path} in {}", section_of(path)));
                continue;
            };
            let documented = row.split('|').nth(2).unwrap().trim().starts_with("optional");
            if documented != nullable.contains(path) {
                optionality.push(path.to_string());
            }
        }
        assert!(missing.is_empty(), "add these to docs/api-v1.md: {missing:?}");
        assert!(
            optionality.is_empty(),
            "docs/api-v1.md says optional exactly where the wire may send null; these disagree: \
             {optionality:?}"
        );
    }

    /// A daemon from before #237 sends no `reason`; every worker it halted had a missing binary.
    #[test]
    fn a_halted_worker_without_a_reason_reads_as_a_missing_binary() {
        let old = serde_json::json!({"worker": "grok", "binary": "/no/such/grok"});
        let read: HaltedWorker = serde_json::from_value(old).unwrap();
        assert_eq!(read.reason, HaltReason::BinaryNotFound);
    }
}
