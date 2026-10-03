//! The code-hosting seam: pull requests, CI verdicts, review requests and review threads.
//!
//! [`Tracker`](crate::tracker::Tracker) reads tickets and [`TrackerWrites`](crate::broker::TrackerWrites)
//! lets the agent write to its own ticket. Neither knows what a pull request is, and neither
//! should: a ticket is where work is *asked for*, a pull request is where it is *delivered*,
//! and on every provider except GitHub those are different systems with different
//! credentials. This trait is the delivery half, and the scheduler drives it directly — the
//! agent never holds it. Opening a pull request, reading CI and recording verdicts on review
//! comments are orchestrator decisions about the agent's output, not things the agent does to
//! its own ticket.
//!
//! **There is no `merge` method, and there must not be one.** Merging stays with a human; the
//! trait's shape is what enforces that, rather than a config flag somebody can flip.
//!
//! Two things a caller must know about the contract:
//!
//! * `request_review` performs the request and nothing else. Whether a reviewer actually
//!   attached is answered by [`Forge::review_requests`] and [`Forge::reviews`] afterwards, and
//!   the scheduler is what asks — because the provider's own answer cannot be trusted here:
//!   GitHub answers a REST request for a bot reviewer with `200` and attaches nobody
//!   (GETT-174120), and the only way to notice is to look.
//! * `ci_status` reports [`CiStatus::Pending`] for a head with no check runs yet as well as for
//!   one still running. A repository with no CI at all therefore reads as pending forever; the
//!   scheduler bounds that with a timeout rather than this trait guessing.

#[cfg(test)]
mod contract;
pub mod fake;
pub mod github;

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ForgeError {
    /// Transient: try again later. A network failure, a 5xx, a rate limit.
    #[error("forge request failed: {0}")]
    Transient(String),
    /// Will not resolve on its own: a bad credential, a 422 on an input that will not change.
    #[error("forge refused: {0}")]
    Permanent(String),
    /// The branch holds nothing the base does not already have — there is no pull request to
    /// open. Distinct from an error so a run that committed nothing reads as "nothing to
    /// deliver" rather than "delivery failed".
    #[error("nothing to deliver: {0}")]
    NothingToDeliver(String),
}

impl ForgeError {
    pub fn retryable(&self) -> bool {
        matches!(self, ForgeError::Transient(_))
    }
}

impl From<crate::http::AuthError> for ForgeError {
    fn from(e: crate::http::AuthError) -> Self {
        match e {
            crate::http::AuthError::Credential(c) => c.into(),
            crate::http::AuthError::Transport(t) => {
                ForgeError::Transient(format!("transport error: {}", t.0))
            }
        }
    }
}

/// Everything needed to open a pull request. The body is composed by the scheduler from the
/// run record — never by the agent — which is what keeps it honest about what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSpec {
    pub title: String,
    pub body: String,
    /// The branch the work is on.
    pub head: String,
    /// The branch to merge into. `master` normally; another issue's branch for a stacked
    /// pull request.
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub head_sha: String,
    pub base: String,
    /// `open`, `closed` or `merged`. A merged pull request also reads as not open.
    pub state: PrState,
    /// Whether the pull request can merge into its base: `Some(false)` for a conflict, `None`
    /// while the provider is still computing it — which GitHub does after every push, and which
    /// is not a conflict. A list endpoint never carries it, so only [`Forge::pull_request`] does.
    #[serde(default)]
    pub mergeable: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// One submitted review: enough to tell whether a requested reviewer has answered and for
/// which commit, and the summary it was submitted with, which can carry a finding no inline
/// comment does (#126).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// Provider id, stable across polls.
    pub id: String,
    pub reviewer: String,
    pub commit_sha: String,
    /// The provider's own word for it: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`.
    pub state: String,
    /// The review's summary text. Empty for a review that is only its inline comments.
    pub body: String,
    pub url: Option<String>,
}

/// The prefix that marks a [`ReviewComment`] id as a review's summary rather than an inline
/// comment. A summary has no thread: its verdict is posted as a comment on the pull request,
/// and there is nothing to resolve. Review and comment ids are separate id spaces on the
/// provider, so the prefix is also what keeps the two from colliding in `review_verdict`.
pub const SUMMARY_PREFIX: &str = "review-";

/// The review id a summary finding's key names, or `None` for an inline comment's id.
pub fn summary_review_id(comment_id: &str) -> Option<&str> {
    comment_id.strip_prefix(SUMMARY_PREFIX)
}

/// The prefix that marks a [`ReviewComment`] id as a comment on the pull request's conversation
/// (#263). Like a summary it has no thread, so its verdict is posted as a comment quoting it;
/// and conversation comments are a third id space on the provider, so the prefix keeps them
/// from colliding with inline comments and reviews in `review_verdict`.
pub const CONVERSATION_PREFIX: &str = "conversation-";

/// The conversation comment id a finding's key names, or `None` for any other kind.
pub fn conversation_comment_id(comment_id: &str) -> Option<&str> {
    comment_id.strip_prefix(CONVERSATION_PREFIX)
}

/// Which of the pull request's comment kinds a finding's key names: what the agent's prompt
/// calls it, and whether settling it resolves a thread or posts a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    Inline,
    Summary,
    Conversation,
}

impl CommentKind {
    pub fn of(comment_id: &str) -> Self {
        if summary_review_id(comment_id).is_some() {
            CommentKind::Summary
        } else if conversation_comment_id(comment_id).is_some() {
            CommentKind::Conversation
        } else {
            CommentKind::Inline
        }
    }
}

/// The CI verdict for one head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    /// No check has failed yet and at least one has not finished — or none has started.
    /// `running` names the checks not yet completed, empty when none has started, so a handoff
    /// for silence can say what it was waiting on (#105).
    Pending {
        running: Vec<String>,
    },
    Success,
    /// At least one check failed. `failures` is what the agent will be handed.
    Failure {
        failures: Vec<CiFailure>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiFailure {
    /// The check's name, e.g. `fmt + clippy + test`.
    pub name: String,
    pub url: Option<String>,
    /// Whatever the provider can say about *why*: the failed step, a log tail. Bounded by the
    /// implementation, because this lands in a prompt.
    pub detail: String,
}

/// One top-level review comment on a pull request. Replies are not surfaced — a thread is
/// settled or not by its root, and the verdicts this project records are replies to roots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// Provider id, stable across polls. The key verdicts are recorded under.
    pub id: String,
    pub author: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub body: String,
    pub url: Option<String>,
}

pub trait Forge: Send + Sync {
    /// Open a pull request for `spec.head` against `spec.base`, or return the one already open
    /// for that head — retargeted to `spec.base` if it was pointing elsewhere. Idempotent by
    /// contract, because the scheduler calls it after every run that reports done: the second
    /// and later calls must find the first call's result, and must leave it targeting what the
    /// caller asked for, since the caller records `spec.base` as the truth about it.
    fn open_pull_request(&self, spec: &PullRequestSpec) -> Result<PullRequest, ForgeError>;

    /// The pull request as it is now: head, state, outstanding review requests.
    fn pull_request(&self, number: u64) -> Result<PullRequest, ForgeError>;

    /// Ask `reviewer` to review. Performs the request only — see the module doc for why the
    /// caller, not this method, decides whether it worked.
    fn request_review(&self, number: u64, reviewer: &str) -> Result<(), ForgeError>;

    /// Logins with a review request outstanding, bots included and spelled as
    /// [`Forge::reviews`] spells them (`<name>[bot]`). Not a field of [`PullRequest`]: GitHub's
    /// REST read of a pull request omits a requested bot (#222), so a request verified there
    /// reads as attaching nobody whether it took or not. A reviewer who has already posted a
    /// review is no longer listed — check [`Forge::reviews`] too before concluding nobody
    /// attached.
    fn review_requests(&self, number: u64) -> Result<Vec<String>, ForgeError>;

    fn reviews(&self, number: u64) -> Result<Vec<Review>, ForgeError>;

    fn ci_status(&self, head_sha: &str) -> Result<CiStatus, ForgeError>;

    /// The login what this forge posts is attributed to, spelled as [`Forge::reviews`] and the
    /// comment reads spell an author: `<slug>[bot]` for a GitHub App. Read once, when delivery is
    /// attached, so its own replies, summaries and verdict comments are never handed back to an
    /// agent as a reviewer's (#263).
    fn login(&self) -> Result<String, ForgeError>;

    /// Every comment on the pull request's own conversation, oldest first, with the provider's
    /// ids unprefixed. Not cut at the head: GitHub records no time a head was pushed, and a
    /// commit's own date is not one (#265), so a cutoff would drop a comment nobody answered.
    fn conversation_comments(&self, number: u64) -> Result<Vec<ReviewComment>, ForgeError>;

    /// Top-level review comments, oldest first.
    fn review_comments(&self, number: u64) -> Result<Vec<ReviewComment>, ForgeError>;

    /// Reply on the thread rooted at `comment_id`. This is how a verdict becomes visible to
    /// the reviewer who left the comment.
    fn reply(&self, number: u64, comment_id: &str, body: &str) -> Result<(), ForgeError>;

    /// Post `body` on the pull request's own conversation. How a verdict on a review's summary
    /// reaches its reviewer, since a summary has no thread to reply on.
    fn comment(&self, number: u64, body: &str) -> Result<(), ForgeError>;

    /// Resolve the thread rooted at each of `comment_ids`, so a settled comment reads as done to
    /// the person merging, answering one result per id in the same order. A batch because
    /// finding a thread means reading every thread on the pull request, and one pass per
    /// comment would spend that read again for each (#100). Idempotent: a thread already
    /// resolved is `Ok`, and so is one that no longer exists — a deleted comment has nothing
    /// left to resolve.
    fn resolve_threads(&self, number: u64, comment_ids: &[String]) -> Vec<Result<(), ForgeError>>;
}

/// What a run leaves on its branch after `Workspace::publish` has pushed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub head_sha: String,
    /// Commit subjects on `head` that `base` does not have, newest first. Empty means the run
    /// committed nothing new — and nothing to open a pull request over.
    pub commits: Vec<String>,
}

/// What [`Publisher::sync`] found on the remote's copy of a branch, and what it did about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Synced {
    /// The remote has no such branch: nothing has been pushed to it yet.
    Absent,
    /// Nothing on the remote to take in. The worktree already holds every commit the remote's
    /// branch does, or the fetched head equals the lease: it has not moved since the last sync
    /// or publish, whoever pushed it. A rebase since then rewrote commits the worktree already
    /// held, and the next push replaces that head (#227).
    Current { remote_head: String },
    /// The worktree took the remote's commits: fast-forwarded when it had none of its own,
    /// merged when it had (`merged`). Never rebased, which would rewrite the agent's commits and
    /// drop a merge someone else pushed, so the pull request's head would change under review.
    Advanced { remote_head: String, merged: bool },
    /// Both sides moved and merging the remote's branch in conflicted in `paths`. The merge was
    /// aborted, so the worktree is where it was, and the push lease still names the last head it
    /// took in: a push now is refused rather than replacing the commits it has not seen.
    Conflict { remote_head: String, paths: Vec<String> },
}

impl Synced {
    /// The remote branch's head as fetched, or `None` when there is no remote branch.
    pub fn remote_head(&self) -> Option<&str> {
        match self {
            Synced::Absent => None,
            Synced::Current { remote_head }
            | Synced::Advanced { remote_head, .. }
            | Synced::Conflict { remote_head, .. } => Some(remote_head),
        }
    }
}

/// The git half of delivery: push the branch and describe what is on it. On the
/// [`Workspace`](crate::workspace::Workspace) rather than on [`Forge`] because it is a git
/// operation in the worktree, not a provider API call, and because the plain-directory
/// workspace has to be able to say "no branch here" rather than fake one.
pub trait Publisher: Send + Sync {
    /// Bring into `worktree` whatever someone else pushed to `branch` on `remote` (#163): fetch
    /// it, fast-forward or merge a head the lease does not name, and record the head taken in
    /// as the lease the next [`Publisher::publish`] pushes against. A fetched head the lease
    /// already names has not moved since that sync or publish. The worktree's divergence is a
    /// local rewrite of commits it already held — the gate rebasing onto a base that moved
    /// (#227) — and is left for that push to replace. Called before every agent run, every
    /// re-gate and every delivery push, so the branch those build on is the one the pull request
    /// shows, and before the gate of a run that reported `Done`, so a head the agent pushed
    /// itself is the lease before the gate's rebase rewrites it (#269). A fetched head the
    /// branch's own reflog reaches was held too, and is treated as the lease is. A worktree mid-rebase,
    /// mid-merge or with uncommitted tracked changes is refused, not synced.
    fn sync(&self, worktree: &Path, branch: &str, remote: &str) -> Result<Synced, ForgeError>;

    /// Push `branch` from `worktree` to `remote`, and list its commits over `base`.
    ///
    /// Forced, against a lease naming the last head [`Publisher::sync`] took in or this method
    /// pushed — never a remote-tracking ref, which any `git fetch` in `workspace.repo` moves
    /// without the worktree having seen what it fetched. A remote branch that moved since is
    /// refused, not overwritten.
    fn publish(
        &self,
        worktree: &Path,
        branch: &str,
        remote: &str,
        base: &str,
    ) -> Result<Published, ForgeError>;

    /// Which of `candidates` this branch is stacked on, if any: the candidate that is an
    /// ancestor of `branch`, carries commits `base` does not, is not itself an ancestor of
    /// another such candidate — **and exists on `remote`**. `None` when the work sits directly
    /// on `base`.
    ///
    /// The remote is what the pull request is opened against, so a candidate the remote does
    /// not have is not a base, whatever the local repository says: a lower branch that has not
    /// finished, or finished and not yet been pushed, would be a `422` there — permanent, and
    /// so a handoff — for a pull request that had nothing wrong with it but its timing.
    fn stacked_on(
        &self,
        worktree: &Path,
        branch: &str,
        remote: &str,
        base: &str,
        candidates: &[String],
    ) -> Result<Option<String>, ForgeError>;

    /// Whether `sha` names a commit that `branch` carries. What an accepted review verdict is
    /// checked against before it is believed: an acceptance names the commit that resolved the
    /// comment, and one naming a commit the branch does not have — invented, or from somewhere
    /// else — is a bare acknowledgement dressed up, so its comment stays outstanding. `false`
    /// for anything that is not a commit at all; an error only for a repository that could not
    /// be asked.
    fn carries(&self, worktree: &Path, branch: &str, sha: &str) -> Result<bool, ForgeError>;
}
