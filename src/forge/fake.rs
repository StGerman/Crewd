//! In-memory [`Forge`] and [`Publisher`], scripted by the scheduler tests.
//!
//! Every operation is recorded in order, because several of the invariants delivery defends
//! are about *what was not done* — no second pull request for the same head, no reply on a
//! settled thread, no merge ever — and a fake that only remembered current state could not
//! show an absence.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use super::{
    CiFailure, CiStatus, Forge, ForgeError, PrState, Published, Publisher, PullRequest,
    PullRequestSpec, Review, ReviewComment,
};

/// One call the fake saw, for asserting on sequences and absences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Publish {
        branch: String,
        base: String,
    },
    OpenPr {
        head: String,
        base: String,
        title: String,
        body: String,
    },
    /// An open pull request found for the head was pointed at a different base and moved.
    Retarget {
        number: u64,
        from: String,
        to: String,
    },
    RequestReview {
        number: u64,
        reviewer: String,
    },
    Reply {
        number: u64,
        comment_id: String,
        body: String,
    },
    /// A comment on the pull request's own conversation, not on any thread.
    Comment {
        number: u64,
        body: String,
    },
    /// Recorded for every call, including one on a thread already resolved, so a test can
    /// count attempts as well as outcomes.
    Resolve {
        number: u64,
        comment_id: String,
    },
}

struct PrRecord {
    pr: PullRequest,
    spec: PullRequestSpec,
    reviews: Vec<Review>,
    comments: Vec<ReviewComment>,
}

#[derive(Default)]
struct Inner {
    prs: BTreeMap<u64, PrRecord>,
    next_number: u64,
    /// CI verdict per head sha; absent means [`CiStatus::Pending`].
    ci: HashMap<String, CiStatus>,
    /// What every head not scripted individually reports.
    ci_default: Option<CiStatus>,
    /// Whether `request_review` actually attaches the reviewer. `false` reproduces the silent
    /// `200` the provider gives for a bot login.
    attach_reviewers: bool,
    /// Commit subjects the next `publish` reports. Empty models a run that committed nothing.
    commits: Vec<String>,
    publishes: u32,
    /// Every branch `publish` has pushed. The fake stands in for the remote too, so this is
    /// what "exists on the remote" means to `stacked_on`.
    published: HashSet<String>,
    /// The commits the delivered branch carries, for `carries`. `None` — the default — answers
    /// yes to every sha, so a test that is not about acceptance can name any commit it likes;
    /// a test that is about it scripts the set.
    on_branch: Option<HashSet<String>>,
    fail: Option<ForgeError>,
    /// Makes `reply` and `comment` alone fail: the network dropping exactly the write that carries a verdict
    /// to its reviewer, while every read still answers.
    fail_reply: Option<ForgeError>,
    /// Makes `resolve_threads` alone fail.
    fail_resolve: Option<ForgeError>,
    /// `(number, comment_id)` of every thread resolved.
    resolved: HashSet<(u64, String)>,
    ops: Vec<Op>,
    /// Every `pull_request` read, which `ops` does not record: a handed-off row is polled with
    /// reads alone, and a test has to see that it is polled at all.
    pr_reads: u32,
    stacked_on: Option<String>,
    /// What a newly opened pull request reports as `mergeable`.
    mergeable: Option<bool>,
}

pub struct FakeForge {
    inner: Mutex<Inner>,
}

impl Default for FakeForge {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeForge {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_number: 100,
                ci_default: Some(CiStatus::Success),
                attach_reviewers: true,
                mergeable: Some(true),
                commits: vec!["do the work".into()],
                ..Default::default()
            }),
        }
    }

    pub fn ops(&self) -> Vec<Op> {
        self.inner.lock().unwrap().ops.clone()
    }

    pub fn pr_reads(&self) -> u32 {
        self.inner.lock().unwrap().pr_reads
    }

    pub fn open_prs(&self) -> Vec<PullRequest> {
        let g = self.inner.lock().unwrap();
        g.prs.values().filter(|r| r.pr.state == PrState::Open).map(|r| r.pr.clone()).collect()
    }

    pub fn pr(&self, number: u64) -> Option<PullRequest> {
        self.inner.lock().unwrap().prs.get(&number).map(|r| r.pr.clone())
    }

    pub fn spec_of(&self, number: u64) -> Option<PullRequestSpec> {
        self.inner.lock().unwrap().prs.get(&number).map(|r| r.spec.clone())
    }

    /// The head sha the most recent publish produced. Deterministic per publish count, so a
    /// test can script CI for a head before the push that creates it.
    pub fn head_after_publish(n: u32) -> String {
        format!("sha-{n:04}")
    }

    /// Script the CI verdict for every head not named individually.
    pub fn set_ci_default(&self, s: Option<CiStatus>) {
        self.inner.lock().unwrap().ci_default = s;
    }

    pub fn set_ci(&self, head_sha: &str, s: CiStatus) {
        self.inner.lock().unwrap().ci.insert(head_sha.to_string(), s);
    }

    pub fn red_ci(&self, head_sha: &str, detail: &str) {
        self.set_ci(
            head_sha,
            CiStatus::Failure {
                failures: vec![CiFailure {
                    name: "fmt + clippy + test".into(),
                    url: Some("https://ci.example/run/1".into()),
                    detail: detail.into(),
                }],
            },
        );
    }

    pub fn set_attach_reviewers(&self, attach: bool) {
        self.inner.lock().unwrap().attach_reviewers = attach;
    }

    pub fn set_commits(&self, commits: Vec<String>) {
        self.inner.lock().unwrap().commits = commits;
    }

    pub fn set_stacked_on(&self, branch: Option<String>) {
        self.inner.lock().unwrap().stacked_on = branch;
    }

    /// Make every call fail until cleared.
    pub fn fail_with(&self, e: Option<ForgeError>) {
        self.inner.lock().unwrap().fail = e;
    }

    /// Script which commits the delivered branch carries; `None` restores "all of them".
    pub fn set_commits_on_branch(&self, shas: Option<Vec<String>>) {
        self.inner.lock().unwrap().on_branch = shas.map(|v| v.into_iter().collect());
    }

    /// Make only `reply` fail until cleared.
    pub fn fail_reply_with(&self, e: Option<ForgeError>) {
        self.inner.lock().unwrap().fail_reply = e;
    }

    /// Make only `resolve_threads` fail until cleared.
    pub fn fail_resolve_with(&self, e: Option<ForgeError>) {
        self.inner.lock().unwrap().fail_resolve = e;
    }

    pub fn is_resolved(&self, number: u64, comment_id: &str) -> bool {
        self.inner.lock().unwrap().resolved.contains(&(number, comment_id.to_string()))
    }

    /// A reviewer leaves a comment. Returns its id.
    pub fn add_comment(&self, number: u64, author: &str, path: &str, body: &str) -> String {
        let mut g = self.inner.lock().unwrap();
        let rec = g.prs.get_mut(&number).expect("no such pull request");
        let id = format!("c-{}-{}", number, rec.comments.len() + 1);
        rec.comments.push(ReviewComment {
            id: id.clone(),
            author: author.into(),
            path: Some(path.into()),
            line: Some(1),
            body: body.into(),
            url: None,
        });
        id
    }

    /// A reviewer submits a review on the current head, which also clears their request.
    pub fn add_review(&self, number: u64, reviewer: &str, state: &str) {
        self.add_summary_review(number, reviewer, state, "");
    }

    /// The same, with a summary body. Returns the review's id.
    pub fn add_summary_review(
        &self,
        number: u64,
        reviewer: &str,
        state: &str,
        body: &str,
    ) -> String {
        let mut g = self.inner.lock().unwrap();
        let rec = g.prs.get_mut(&number).expect("no such pull request");
        let id = format!("r-{}-{}", number, rec.reviews.len() + 1);
        let sha = rec.pr.head_sha.clone();
        rec.reviews.push(Review {
            id: id.clone(),
            reviewer: reviewer.into(),
            commit_sha: sha,
            state: state.into(),
            body: body.into(),
            url: Some(format!("https://forge.example/pulls/{number}#{id}")),
        });
        rec.pr.requested_reviewers.retain(|r| r != reviewer);
        id
    }

    /// Someone other than the orchestrator moves the head — the operator merging the base in,
    /// or the provider's "Update branch" — so the pull request's head is one no `publish` made.
    pub fn push_head(&self, number: u64, head_sha: &str) {
        if let Some(rec) = self.inner.lock().unwrap().prs.get_mut(&number) {
            rec.pr.head_sha = head_sha.to_string();
        }
    }

    /// What every pull request opened from now on reports as `mergeable`.
    pub fn set_mergeable_default(&self, mergeable: Option<bool>) {
        self.inner.lock().unwrap().mergeable = mergeable;
    }

    /// The base moves under an open pull request, or the provider finishes computing.
    pub fn set_mergeable(&self, number: u64, mergeable: Option<bool>) {
        if let Some(rec) = self.inner.lock().unwrap().prs.get_mut(&number) {
            rec.pr.mergeable = mergeable;
        }
    }

    /// The operator closes or merges it outside the orchestrator.
    pub fn set_state(&self, number: u64, state: PrState) {
        if let Some(rec) = self.inner.lock().unwrap().prs.get_mut(&number) {
            rec.pr.state = state;
        }
    }

    pub fn replies_to(&self, number: u64, comment_id: &str) -> Vec<String> {
        self.ops()
            .into_iter()
            .filter_map(|op| match op {
                Op::Reply { number: n, comment_id: c, body } if n == number && c == comment_id => {
                    Some(body)
                }
                _ => None,
            })
            .collect()
    }

    /// Every comment posted on the pull request's own conversation.
    pub fn comments_on(&self, number: u64) -> Vec<String> {
        self.ops()
            .into_iter()
            .filter_map(|op| match op {
                Op::Comment { number: n, body } if n == number => Some(body),
                _ => None,
            })
            .collect()
    }

    fn gate(g: &Inner) -> Result<(), ForgeError> {
        match &g.fail {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}

impl Publisher for FakeForge {
    fn publish(
        &self,
        _worktree: &Path,
        branch: &str,
        _remote: &str,
        base: &str,
    ) -> Result<Published, ForgeError> {
        let mut g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        g.ops.push(Op::Publish { branch: branch.into(), base: base.into() });
        g.published.insert(branch.to_string());
        g.publishes += 1;
        let head_sha = Self::head_after_publish(g.publishes);
        // The pull request open for this branch moves with the push, as the real one does. A
        // push is the gate's rebased branch, so it merges again.
        for rec in g.prs.values_mut() {
            if rec.spec.head == branch && rec.pr.state == PrState::Open {
                rec.pr.head_sha = head_sha.clone();
                rec.pr.mergeable = Some(true);
            }
        }
        Ok(Published { head_sha, commits: g.commits.clone() })
    }

    fn stacked_on(
        &self,
        _worktree: &Path,
        _branch: &str,
        _remote: &str,
        _base: &str,
        candidates: &[String],
    ) -> Result<Option<String>, ForgeError> {
        let g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        // The scripted answer holds only for a candidate the scheduler offered *and* a branch
        // this fake has seen pushed — the same two conditions the real remote imposes.
        Ok(g.stacked_on.clone().filter(|b| candidates.contains(b) && g.published.contains(b)))
    }

    fn carries(&self, _worktree: &Path, _branch: &str, sha: &str) -> Result<bool, ForgeError> {
        let g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        Ok(g.on_branch.as_ref().is_none_or(|set| set.contains(sha)))
    }
}

impl Forge for FakeForge {
    fn open_pull_request(&self, spec: &PullRequestSpec) -> Result<PullRequest, ForgeError> {
        let mut g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        g.ops.push(Op::OpenPr {
            head: spec.head.clone(),
            base: spec.base.clone(),
            title: spec.title.clone(),
            body: spec.body.clone(),
        });
        if let Some(rec) =
            g.prs.values_mut().find(|r| r.spec.head == spec.head && r.pr.state == PrState::Open)
        {
            if rec.pr.base != spec.base {
                // The real forge retargets a pull request found pointing elsewhere, body
                // included; recorded as its own op so a test can assert it happened — or that
                // it did not.
                let retarget = Op::Retarget {
                    number: rec.pr.number,
                    from: rec.pr.base.clone(),
                    to: spec.base.clone(),
                };
                rec.pr.base = spec.base.clone();
                rec.spec.base = spec.base.clone();
                rec.spec.body = spec.body.clone();
                let pr = rec.pr.clone();
                g.ops.push(retarget);
                return Ok(pr);
            }
            return Ok(rec.pr.clone());
        }
        if g.commits.is_empty() {
            return Err(ForgeError::NothingToDeliver(format!(
                "no commits between {} and {}",
                spec.base, spec.head
            )));
        }
        let number = g.next_number;
        g.next_number += 1;
        let head_sha = Self::head_after_publish(g.publishes);
        let pr = PullRequest {
            number,
            url: format!("https://forge.example/pulls/{number}"),
            head_sha,
            base: spec.base.clone(),
            state: PrState::Open,
            requested_reviewers: vec![],
            mergeable: g.mergeable,
        };
        g.prs.insert(
            number,
            PrRecord { pr: pr.clone(), spec: spec.clone(), reviews: vec![], comments: vec![] },
        );
        Ok(pr)
    }

    fn pull_request(&self, number: u64) -> Result<PullRequest, ForgeError> {
        let mut g = self.inner.lock().unwrap();
        g.pr_reads += 1;
        Self::gate(&g)?;
        g.prs
            .get(&number)
            .map(|r| r.pr.clone())
            .ok_or_else(|| ForgeError::Permanent(format!("no pull request #{number}")))
    }

    fn request_review(&self, number: u64, reviewer: &str) -> Result<(), ForgeError> {
        let mut g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        g.ops.push(Op::RequestReview { number, reviewer: reviewer.into() });
        let attach = g.attach_reviewers;
        let rec = g
            .prs
            .get_mut(&number)
            .ok_or_else(|| ForgeError::Permanent(format!("no pull request #{number}")))?;
        // The silent success: the provider says yes and does nothing.
        if attach && !rec.pr.requested_reviewers.iter().any(|r| r == reviewer) {
            rec.pr.requested_reviewers.push(reviewer.into());
        }
        Ok(())
    }

    fn reviews(&self, number: u64) -> Result<Vec<Review>, ForgeError> {
        let g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        Ok(g.prs.get(&number).map(|r| r.reviews.clone()).unwrap_or_default())
    }

    fn ci_status(&self, head_sha: &str) -> Result<CiStatus, ForgeError> {
        let g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        Ok(g.ci
            .get(head_sha)
            .cloned()
            .or_else(|| g.ci_default.clone())
            .unwrap_or(CiStatus::Pending { running: vec![] }))
    }

    fn review_comments(&self, number: u64) -> Result<Vec<ReviewComment>, ForgeError> {
        let g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        Ok(g.prs.get(&number).map(|r| r.comments.clone()).unwrap_or_default())
    }

    fn reply(&self, number: u64, comment_id: &str, body: &str) -> Result<(), ForgeError> {
        let mut g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        if let Some(e) = &g.fail_reply {
            return Err(e.clone());
        }
        g.ops.push(Op::Reply { number, comment_id: comment_id.into(), body: body.into() });
        Ok(())
    }

    /// Fails with `reply`: both are the write that carries a verdict to its reviewer.
    fn comment(&self, number: u64, body: &str) -> Result<(), ForgeError> {
        let mut g = self.inner.lock().unwrap();
        Self::gate(&g)?;
        if let Some(e) = &g.fail_reply {
            return Err(e.clone());
        }
        g.ops.push(Op::Comment { number, body: body.into() });
        Ok(())
    }

    fn resolve_threads(&self, number: u64, comment_ids: &[String]) -> Vec<Result<(), ForgeError>> {
        let mut g = self.inner.lock().unwrap();
        comment_ids
            .iter()
            .map(|comment_id| {
                Self::gate(&g)?;
                g.ops.push(Op::Resolve { number, comment_id: comment_id.clone() });
                if let Some(e) = &g.fail_resolve {
                    return Err(e.clone());
                }
                g.resolved.insert((number, comment_id.clone()));
                Ok(())
            })
            .collect()
    }
}
