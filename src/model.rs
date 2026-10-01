//! Domain model.
//!
//! Deliberate deviations from the Symphony spec, each fixing a defect found in review:
//!
//! * [`Outcome`] replaces "the worker process exited normally, so maybe there is more to do".
//!   The spec re-dispatches on a 1s timer forever; here only [`Outcome::Continue`] re-dispatches.
//! * [`ErrorClass`] is partitioned into retryable and permanent. The spec has no permanent
//!   class, so a template typo retries every 5 minutes for eternity.
//! * [`worktree_key`] hashes the *dispatch id*, not the identifier. The spec hashes the
//!   identifier and only when sanitisation alters it, which does nothing for two distinct
//!   issues that genuinely share an identifier.

use serde::{Deserialize, Serialize};

/// A normalized work item. Adapters map provider payloads onto this; the scheduler never
/// inspects provider data directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Opaque dispatch identity. May be a project-item id rather than a ticket id.
    pub id: String,
    /// Human-readable key, e.g. `MT-649`. Used for display and workspace naming.
    pub identifier: String,
    pub title: String,
    /// Free-text description, when the provider has one. The only field of the task the
    /// worker's prompt has any real content from beyond the title — an adapter that can
    /// populate it and doesn't is handing the agent a ticket with no description.
    pub body: Option<String>,
    pub state: String,
    pub priority: Option<i32>,
    pub url: Option<String>,
    pub labels: Vec<String>,
    /// Adapter-derived eligibility for provider rules the scheduler cannot infer
    /// (assignment, board membership, blocker semantics).
    pub dispatchable: bool,
    pub created_at: Option<i64>,
    /// Non-secret provider identifiers, preserved opaquely for tool context.
    pub native_ref: Option<serde_json::Value>,
    /// Dispatch ids of issues blocking this one. Projected into the task store as `blockedBy`.
    pub blocked_by: Vec<String>,
}

impl Issue {
    /// Trimmed + lowercased state, for scheduler comparison only. Provider spelling is
    /// preserved in `state` for display.
    pub fn state_key(&self) -> String {
        self.state.trim().to_lowercase()
    }
}

pub use libcrew::Phase;

/// What a worker reports when its run ends. An explicit verdict, never inferred from exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Reached a handoff state. Release the claim.
    Done,
    /// Real work remains. Eligible for re-dispatch, subject to the turn budget.
    Continue {
        why: String,
    },
    /// Needs a human. Release the claim without retrying.
    Blocked {
        why: String,
    },
    Failed {
        class: ErrorClass,
        msg: String,
    },
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Continue { .. } => "continue",
            Outcome::Blocked { .. } => "blocked",
            Outcome::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorClass {
    // Retryable: transient conditions that a later attempt may survive.
    TrackerRequest,
    TrackerStatus,
    RateLimited,
    TurnTimeout,
    Stall,
    AgentCrash,
    WorkspaceIo,
    // Permanent: deterministic failures that will recur identically. Retrying is a hot loop.
    TemplateRender,
    ConfigInvalid,
    AgentNotFound,
    /// The CLI refused the configured `worker.model` (#36). Permanent rather than retried: every
    /// attempt passes the same name, and the alternative the CLI offers — dropping the flag —
    /// would run the issue on a model the run row does not name.
    ModelNotFound,
    WorkspaceOutsideRoot,
    AuthFailed,
}

impl ErrorClass {
    /// Exhaustive by construction: adding a variant without classifying it fails the build.
    pub fn retryable(self) -> bool {
        match self {
            ErrorClass::TrackerRequest
            | ErrorClass::TrackerStatus
            | ErrorClass::RateLimited
            | ErrorClass::TurnTimeout
            | ErrorClass::Stall
            | ErrorClass::AgentCrash
            | ErrorClass::WorkspaceIo => true,

            ErrorClass::TemplateRender
            | ErrorClass::ConfigInvalid
            | ErrorClass::AgentNotFound
            | ErrorClass::ModelNotFound
            | ErrorClass::WorkspaceOutsideRoot
            | ErrorClass::AuthFailed => false,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::TrackerRequest => "tracker_request",
            ErrorClass::TrackerStatus => "tracker_status",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::TurnTimeout => "turn_timeout",
            ErrorClass::Stall => "stall",
            ErrorClass::AgentCrash => "agent_crash",
            ErrorClass::WorkspaceIo => "workspace_io",
            ErrorClass::TemplateRender => "template_render",
            ErrorClass::ConfigInvalid => "config_invalid",
            ErrorClass::AgentNotFound => "agent_not_found",
            ErrorClass::ModelNotFound => "model_not_found",
            ErrorClass::WorkspaceOutsideRoot => "workspace_outside_root",
            ErrorClass::AuthFailed => "auth_failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "tracker_request" => ErrorClass::TrackerRequest,
            "tracker_status" => ErrorClass::TrackerStatus,
            "rate_limited" => ErrorClass::RateLimited,
            "turn_timeout" => ErrorClass::TurnTimeout,
            "stall" => ErrorClass::Stall,
            "agent_crash" => ErrorClass::AgentCrash,
            "workspace_io" => ErrorClass::WorkspaceIo,
            "template_render" => ErrorClass::TemplateRender,
            "config_invalid" => ErrorClass::ConfigInvalid,
            "agent_not_found" => ErrorClass::AgentNotFound,
            "model_not_found" => ErrorClass::ModelNotFound,
            "workspace_outside_root" => ErrorClass::WorkspaceOutsideRoot,
            "auth_failed" => ErrorClass::AuthFailed,
            _ => return None,
        })
    }
}

/// What the orchestrator knows about why this attempt exists that the agent cannot see from
/// inside its worktree. `None` on a first dispatch; otherwise exactly one of these, whichever
/// sent the issue back — the handoff gate, CI on the pull request, or a reviewer.
///
/// One channel on purpose. The gate (#21) and delivery (#32) each arrived with their own
/// parameter on `Worker::spawn` for the same idea, and two channels would have meant two
/// prompt conventions for one question. Structured rather than pre-rendered text where the
/// structure earns its place, because the two sides have different jobs: the scheduler knows
/// *what* went wrong (which check, which comments) and the worker knows how to ask its agent to
/// act on it (the prompt wording, the verdict marker it will parse back). Keeping the marker in
/// one module is what stops the two from drifting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Feedback {
    /// CI went red on the pull request. The run is told to make it green.
    Ci { pr_url: String, failures: Vec<crate::forge::CiFailure> },
    /// Review comments are outstanding. The run is told to settle each one, with a verdict.
    Review {
        pr_url: String,
        comments: Vec<crate::forge::ReviewComment>,
        /// Ids among `comments` that were handed to an earlier round and came back with no
        /// verdict. Named so the agent knows silence was noticed.
        unanswered_before: Vec<String>,
    },
    /// The retry reason, verbatim — for a run the handoff gate sent back, the step that failed,
    /// how many tries are left and the failing output, as `gate_outcome` composed them.
    ///
    /// One string rather than a reason and an output apart, because one string is what the
    /// retry row stores and what `gate_outcome` writes: it already says which step failed and
    /// where the output starts, so splitting it at render time would mean parsing back text
    /// this crate wrote. The day the gate records its output on its own column, this grows a
    /// field; until then a second field would be a second spelling of the same string.
    Gate { output: String },
    /// The handoff gate could not bring the branch onto `base`: the rebase or merge stopped on
    /// conflicts in `paths` and was aborted, and the issue parked `Blocked` (#109). Carried to
    /// the run after the one that was blocked, so it is told the truth about why it exists —
    /// not that a turn budget ran out — once a human has unblocked it. `base_sha` is the commit
    /// the gate tried, which the agent is told to merge: `base` resolves in its worktree,
    /// where an unset base's `HEAD` is its own branch.
    Conflict { base: String, base_sha: String, paths: Vec<String> },
    /// Another worker ran this issue last and stopped (#165): `from` names it, `why` says how
    /// its run ended, and `last_text` is its agent's last message, read off the run's
    /// transcript and truncated. A session never crosses providers, so this is all the new one
    /// learns of the old conversation besides the commits. First in the list when present, so
    /// whatever else this attempt was sent back with reads as that worker's unfinished business.
    Handoff { from: String, why: String, last_text: Option<String> },
}

impl Feedback {
    pub fn label(&self) -> &'static str {
        match self {
            Feedback::Ci { .. } => "ci",
            Feedback::Review { .. } => "review",
            Feedback::Gate { .. } => "gate",
            Feedback::Conflict { .. } => "conflict",
            Feedback::Handoff { .. } => "handoff",
        }
    }
}

/// Whether `s` is shaped like a git commit: an abbreviated or full hex object name, and nothing
/// else. The test an acceptance's detail has to pass before anything treats it as the commit it
/// claims to be — `fixed`, `see above` and `commit abc1234` all fail it. Shape only; whether
/// the commit exists, and is on the branch delivered, is git's to answer.
pub fn looks_like_commit(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The agent's settlement of one review comment. Either a fix, named by the commit that
/// carries it, or a refusal, named by its reason — never a bare acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub comment_id: String,
    pub verdict: Verdict,
    /// The resolving commit for `Accepted`; the reason for `Rejected`.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    #[serde(rename = "accepted")]
    Accepted,
    #[serde(rename = "rejected")]
    Rejected,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Accepted => "accepted",
            Verdict::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "accepted" => Some(Verdict::Accepted),
            "rejected" => Some(Verdict::Rejected),
            _ => None,
        }
    }
}

/// Directory name for an issue's workspace.
///
/// Sanitises the identifier for display value, then appends a hash of the *dispatch id* so two
/// issues can never share a directory — whether because their identifiers sanitise to the same
/// text, or because the adapter handed us a duplicate identifier outright.
pub fn worktree_key(issue_id: &str, identifier: &str) -> String {
    let sanitized: String = identifier
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();

    let stem = if sanitized.is_empty() { "issue" } else { sanitized.as_str() };
    format!("{stem}-{}", dispatch_suffix(issue_id))
}

/// Twelve hex digits of the dispatch id.
///
/// Two issues whose identifiers — or whose issue number and title slug — sanitise to the same
/// text would otherwise share a directory or a branch. The same length everywhere it is
/// appended, so the branch collision form cannot be weaker than the directory name (#205).
pub(crate) fn dispatch_suffix(issue_id: &str) -> String {
    blake3::hash(issue_id.as_bytes()).to_hex().chars().take(12).collect()
}

/// A UUID-shaped name for a `claude` conversation, derived rather than random so this crate
/// keeps its dependency surface: blake3 is already here for [`worktree_key`], and the CLI only
/// needs this to parse as a UUID.
///
/// `stamp` is what makes it unique per dispatch rather than per issue. An issue can be worked
/// more than once, and reusing a name the CLI still holds a conversation under would collide
/// with it rather than start something new.
pub fn session_id(issue_id: &str, stamp: i64) -> String {
    let mut b = *blake3::hash(format!("{issue_id}:{stamp}").as_bytes()).as_bytes();
    // Version 4 and the RFC 4122 variant: the two fields a UUID parser actually checks.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b[..16].iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_is_uuid_shaped_and_never_repeats_across_dispatches() {
        let a = session_id("iss-1", 1_000);
        assert_eq!(a.len(), 36);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{a} is not hex");
        assert!(parts[2].starts_with('4'), "{a} is not version 4");
        assert!(
            matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
            "{a} does not carry the RFC 4122 variant"
        );

        // A second dispatch of the same issue must not land on the name the first one is
        // already using.
        assert_ne!(a, session_id("iss-1", 1_001));
        assert_ne!(a, session_id("iss-2", 1_000));
    }

    #[test]
    fn a_phase_serialises_to_the_one_word_the_store_and_the_dashboard_use() {
        for p in
            [Phase::Queued, Phase::Running, Phase::RetryQueued, Phase::Quarantined, Phase::Released]
        {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.label()), "serde and label disagree for {p:?}");
            assert_eq!(serde_json::from_str::<Phase>(&json).unwrap(), p);
        }
    }

    #[test]
    fn every_error_class_is_classified_and_round_trips() {
        let all = [
            ErrorClass::TrackerRequest,
            ErrorClass::TrackerStatus,
            ErrorClass::RateLimited,
            ErrorClass::TurnTimeout,
            ErrorClass::Stall,
            ErrorClass::AgentCrash,
            ErrorClass::WorkspaceIo,
            ErrorClass::TemplateRender,
            ErrorClass::ConfigInvalid,
            ErrorClass::AgentNotFound,
            ErrorClass::ModelNotFound,
            ErrorClass::WorkspaceOutsideRoot,
            ErrorClass::AuthFailed,
        ];
        for c in all {
            let _ = c.retryable();
            assert_eq!(ErrorClass::parse(c.as_str()), Some(c), "round trip {}", c.as_str());
        }
        assert!(!ErrorClass::TemplateRender.retryable());
        assert!(ErrorClass::RateLimited.retryable());
    }

    #[test]
    fn identifiers_sharing_sanitized_text_get_distinct_keys() {
        // `MT/649` and `MT_649` both sanitise to `MT_649`.
        let a = worktree_key("id-a", "MT/649");
        let b = worktree_key("id-b", "MT_649");
        assert_ne!(a, b);
        assert!(a.starts_with("MT_649-"));
        assert!(b.starts_with("MT_649-"));
    }

    #[test]
    fn duplicate_identifiers_still_get_distinct_keys() {
        // The case the spec's identifier-hash cannot catch: same identifier, different issues.
        assert_ne!(worktree_key("id-a", "MT-649"), worktree_key("id-b", "MT-649"));
    }

    #[test]
    fn key_is_stable_and_contains_only_safe_characters() {
        let k = worktree_key("id-a", "MT-649");
        assert_eq!(k, worktree_key("id-a", "MT-649"));
        assert!(
            k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "unsafe characters in {k}"
        );
    }

    #[test]
    fn traversal_attempts_are_neutralised() {
        // Sanitisation yields `.._.._etc_passwd`: dots survive, separators do not. A dot run is
        // harmless inside a single path component — traversal needs a separator — so the
        // invariants that matter are "no separator" and "not exactly . or ..".
        for identifier in ["../../etc/passwd", "..", ".", "a\\b", "with space/slash"] {
            let k = worktree_key("id-a", identifier);
            assert!(!k.contains('/'), "separator survived in {k}");
            assert!(!k.contains('\\'), "separator survived in {k}");
            assert!(k != "." && k != "..", "resolved to a relative directory: {k}");
        }
    }
}
