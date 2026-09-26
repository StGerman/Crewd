//! `Forge` against real GitHub pull requests, reviews, checks and review-comment threads.
//!
//! Reuses the `Http` seam [`crate::tracker::github`] already defined for the tracker adapter
//! rather than declaring a second one — a GET and an authenticated JSON write are the same
//! two primitives whether the caller is reading issues or pull requests, and a second trait
//! would just be a second place for the ureq-vs-fake split to drift. `GithubForge<H: Http>`
//! carries its own credential rather than borrowing `GithubTracker`'s, because a forge and a
//! tracker are different providers on every platform except this one, and this adapter should
//! not assume otherwise.
//!
//! ## Best-effort CI detail
//!
//! [`Forge::ci_status`] owes the scheduler more than pass/fail: a [`super::CiFailure`] lands in
//! an agent's prompt, so "check X failed" without why is a wasted turn spent re-discovering
//! what the API already knows. The failing check run's own `output.title`/`output.summary`
//! comes for free in the same response. Going further — which step inside a GitHub Actions job
//! failed, and the tail of that job's log — costs two more requests per failing check, and
//! both are best-effort: a job or a log that cannot be fetched leaves `detail` with whatever
//! was gathered already, never fails the call. `ci_status` itself must still answer even when
//! GitHub Actions is not the CI provider on this repo at all, which is why the job/log lookup
//! only fires for a `details_url` shaped like an Actions job link and is skipped otherwise.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    CiFailure, CiStatus, Forge, ForgeError, PrState, PullRequest, PullRequestSpec, Review,
    ReviewComment,
};
use crate::credentials::{Credentials, StaticToken};
use crate::tracker::github::{Http, HttpResponse, HttpTransportError};

const API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const PER_PAGE: u32 = 100;
/// Bound on [`CiFailure::detail`] — it lands in an agent's prompt, and a raw job log can run
/// to megabytes. Kept as the tail rather than the head, because a compiler error sits at the
/// end of a failing build's log, not the start.
const DETAIL_CAP_BYTES: usize = 8 * 1024;
/// How many trailing log lines to append after the failed step names. Wide enough to catch a
/// multi-line compiler diagnostic without pulling in an unrelated setup step.
const LOG_TAIL_LINES: usize = 120;

// ---- GitHub's REST shape, trimmed to what this adapter reads --------------

#[derive(Debug, Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhHeadRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GhBaseRef {
    #[serde(rename = "ref")]
    r#ref: String,
}

#[derive(Debug, Deserialize)]
struct GhPullRequest {
    number: u64,
    html_url: String,
    head: GhHeadRef,
    base: GhBaseRef,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    requested_reviewers: Vec<GhUser>,
    #[serde(default)]
    mergeable: Option<bool>,
    #[serde(default)]
    mergeable_state: Option<String>,
}

/// `merged`/`merged_at` win over `state`: GitHub reports a merged pull request's `state` as
/// `"closed"` too, and a caller that only checked `state` would read "merged" as "closed
/// without landing" — the exact distinction [`PrState`] exists to keep.
fn to_pull_request(gh: GhPullRequest) -> PullRequest {
    let state = if gh.merged || gh.merged_at.is_some() {
        PrState::Merged
    } else if gh.state == "closed" {
        PrState::Closed
    } else {
        PrState::Open
    };
    PullRequest {
        number: gh.number,
        url: gh.html_url,
        head_sha: gh.head.sha,
        base: gh.base.r#ref,
        state,
        requested_reviewers: gh.requested_reviewers.into_iter().map(|u| u.login).collect(),
        // `dirty` is GitHub's word for a merge conflict, and can arrive with `mergeable` still
        // unset; any other state leaves the boolean to speak.
        mergeable: if gh.mergeable_state.as_deref() == Some("dirty") {
            Some(false)
        } else {
            gh.mergeable
        },
    }
}

#[derive(Debug, Deserialize)]
struct GhReview {
    id: u64,
    user: GhUser,
    commit_id: String,
    state: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhCheckOutput {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhCheckRun {
    name: String,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    details_url: Option<String>,
    #[serde(default)]
    output: Option<GhCheckOutput>,
}

#[derive(Debug, Deserialize)]
struct GhCheckRunsPage {
    #[serde(default)]
    check_runs: Vec<GhCheckRun>,
}

#[derive(Debug, Deserialize)]
struct GhJobStep {
    name: String,
    #[serde(default)]
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhJob {
    #[serde(default)]
    steps: Vec<GhJobStep>,
}

#[derive(Debug, Deserialize)]
struct GhReviewComment {
    id: u64,
    user: GhUser,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    original_line: Option<u64>,
    body: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    in_reply_to_id: Option<u64>,
}

// ---- GitHub's GraphQL shape, for review threads alone ------------------

/// Review threads and their resolution exist only in GraphQL; REST has neither.
const GRAPHQL_URL: &str = "https://api.github.com/graphql";

const THREADS_QUERY: &str =
    "query($owner: String!, $repo: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes { id isResolved comments(first: 1) { nodes { databaseId } } }
      }
    }
  }
}";

const RESOLVE_MUTATION: &str = "mutation($id: ID!) {
  resolveReviewThread(input: { threadId: $id }) { thread { id isResolved } }
}";

/// GraphQL answers `200` for a query it refused, with the reason in `errors`.
#[derive(Debug, Deserialize)]
struct GqlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Debug, Deserialize)]
struct GqlError {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    message: String,
}

#[derive(Debug, Deserialize)]
struct GqlThreadsData {
    repository: Option<GqlRepository>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlRepository {
    pull_request: Option<GqlPullRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPullRequest {
    review_threads: GqlThreadPage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThreadPage {
    page_info: GqlPageInfo,
    nodes: Vec<GqlThread>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThread {
    id: String,
    is_resolved: bool,
    comments: GqlCommentPage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlResolveData {
    resolve_review_thread: Option<GqlResolvePayload>,
}

#[derive(Debug, Deserialize)]
struct GqlResolvePayload {
    thread: Option<GqlResolvedThread>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlResolvedThread {
    is_resolved: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GqlCommentPage {
    nodes: Vec<GqlComment>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlComment {
    database_id: Option<u64>,
}

// ---- the forge ---------------------------------------------------------

pub struct GithubForge<H: Http> {
    http: H,
    owner: String,
    repo: String,
    creds: Arc<dyn Credentials>,
}

impl<H: Http> GithubForge<H> {
    pub fn new(http: H, owner: &str, repo: &str, token: &str) -> Self {
        Self {
            http,
            owner: owner.to_string(),
            repo: repo.to_string(),
            creds: Arc::new(StaticToken::new(token)),
        }
    }

    /// The installation token the pull request is authored with, asked for per request because
    /// it expires within the hour (#64).
    pub fn with_credentials(mut self, creds: Arc<dyn Credentials>) -> Self {
        self.creds = creds;
        self
    }

    fn headers(&self) -> Result<Vec<(&'static str, String)>, ForgeError> {
        Ok(vec![
            ("Authorization", format!("Bearer {}", self.creds.token()?)),
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", API_VERSION.to_string()),
            ("User-Agent", "crewd".to_string()),
        ])
    }

    /// Sends once, and once more with a fresh token if the first was refused with a 401: an
    /// installation token revoked before its expiry would otherwise read as a permanent auth
    /// failure and hand delivery off, though the next mint would have worked. A refused request
    /// was not applied, so repeating it is safe; a second 401 is the credential really being
    /// wrong, and stands.
    fn authed(
        &self,
        send: impl Fn(&[(&str, String)]) -> Result<HttpResponse, HttpTransportError>,
    ) -> Result<HttpResponse, ForgeError> {
        let transport =
            |e: HttpTransportError| ForgeError::Transient(format!("transport error: {}", e.0));
        let resp = send(&self.headers()?).map_err(transport)?;
        if resp.status == 401 && self.creds.invalidate() {
            return send(&self.headers()?).map_err(transport);
        }
        Ok(resp)
    }

    /// One authenticated GET, classified onto [`ForgeError`].
    fn get(&self, url: &str) -> Result<HttpResponse, ForgeError> {
        classify(self.authed(|h| self.http.get(url, h))?)
    }

    /// One authenticated JSON write, classified the same way a read is — a bad credential on a
    /// write and one on a read must come back as the same variant, or the scheduler's
    /// retryable/permanent split stops meaning the same thing on both paths.
    fn send_json(&self, method: &str, url: &str, body: &Value) -> Result<HttpResponse, ForgeError> {
        let payload = serde_json::to_vec(body).map_err(|e| ForgeError::Permanent(e.to_string()))?;
        classify(self.authed(|h| self.http.send_json(method, url, h, &payload))?)
    }

    /// One GraphQL request over the same credential path as every REST call. A refusal GraphQL
    /// reports in `errors` is classified like the REST status it stands for: rate limiting is
    /// transient, anything else will not change on a retry.
    fn graphql<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T, ForgeError> {
        let resp = self.send_json(
            "POST",
            GRAPHQL_URL,
            &json!({ "query": query, "variables": variables }),
        )?;
        let gql: GqlResponse<T> = parse(&resp)?;
        if let Some(e) = gql.errors.first() {
            let msg = format!("graphql: {}", e.message);
            return Err(if e.kind.as_deref() == Some("RATE_LIMITED") {
                ForgeError::Transient(msg)
            } else {
                ForgeError::Permanent(msg)
            });
        }
        gql.data.ok_or_else(|| ForgeError::Permanent("graphql: a response with no data".into()))
    }

    /// Every review thread on the pull request, keyed by its root comment's `databaseId`.
    fn review_threads(&self, number: u64) -> Result<HashMap<u64, GqlThread>, ForgeError> {
        let mut threads = HashMap::new();
        let mut after: Option<String> = None;
        loop {
            let data: GqlThreadsData = self.graphql(
                THREADS_QUERY,
                json!({ "owner": self.owner, "repo": self.repo, "number": number, "after": after }),
            )?;
            let page = data
                .repository
                .and_then(|r| r.pull_request)
                .ok_or_else(|| ForgeError::Permanent(format!("no pull request #{number}")))?
                .review_threads;
            for t in page.nodes {
                if let Some(root) = t.comments.nodes.first().and_then(|c| c.database_id) {
                    threads.insert(root, t);
                }
            }
            match page.page_info.end_cursor {
                Some(cursor) if page.page_info.has_next_page => after = Some(cursor),
                _ => return Ok(threads),
            }
        }
    }

    /// A mutation GitHub answers without `isResolved: true` did not resolve the thread, and
    /// reading it as success would record the verdict resolved and never try again (#100). It
    /// is permanent: the same credential asking again gets the same answer. A payload with no
    /// thread is the exception: the thread was deleted after the listing read it, and a deleted
    /// thread has nothing left to resolve.
    fn resolve_review_thread(&self, thread_id: &str) -> Result<(), ForgeError> {
        let data: GqlResolveData = self.graphql(RESOLVE_MUTATION, json!({ "id": thread_id }))?;
        let not_applied = || {
            ForgeError::Permanent(format!(
                "graphql: review thread {thread_id} was not resolved by resolveReviewThread"
            ))
        };
        match data.resolve_review_thread.ok_or_else(not_applied)?.thread {
            None => Ok(()),
            Some(GqlResolvedThread { is_resolved: Some(true) }) => Ok(()),
            Some(_) => Err(not_applied()),
        }
    }

    fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ForgeError> {
        parse(&self.get(url)?)
    }

    /// A best-effort, unauthenticated-failure-tolerant GET: used only by the CI detail path,
    /// where a job or a log that cannot be fetched must leave `detail` with whatever was
    /// gathered so far rather than failing `ci_status` outright.
    fn get_best_effort(&self, url: &str) -> Option<HttpResponse> {
        let resp = self.authed(|h| self.http.get(url, h)).ok()?;
        if (200..300).contains(&resp.status) { Some(resp) } else { None }
    }

    /// Paginate a GET that returns a bare JSON array, stopping once a page comes back short —
    /// the same rule [`crate::tracker::github::GithubTracker::fetch_open_labelled`] uses, and
    /// for the same reason: a short page is the only signal GitHub gives that there is no next
    /// one.
    fn paginate<T: DeserializeOwned>(
        &self,
        url_for_page: impl Fn(u32) -> String,
    ) -> Result<Vec<T>, ForgeError> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let batch: Vec<T> = self.get_json(&url_for_page(page))?;
            let got = batch.len();
            all.extend(batch);
            if got < PER_PAGE as usize {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    /// The failed step names and trailing log lines for one failing check run's GitHub Actions
    /// job, when `details_url` names one. Every step here is best-effort: a repo whose CI is
    /// not GitHub Actions, or a job/log fetch that fails, leaves this returning less rather
    /// than erroring `ci_status` for every other check run in the same response.
    fn actions_detail(&self, details_url: &str) -> Option<String> {
        let job_id = actions_job_id(details_url)?;

        let mut parts = Vec::new();

        let job_url =
            format!("{API_BASE}/repos/{}/{}/actions/jobs/{job_id}", self.owner, self.repo);
        if let Some(resp) = self.get_best_effort(&job_url)
            && let Ok(job) = serde_json::from_slice::<GhJob>(&resp.body)
        {
            let failed: Vec<&str> = job
                .steps
                .iter()
                .filter(|s| s.conclusion.as_deref() == Some("failure"))
                .map(|s| s.name.as_str())
                .collect();
            if !failed.is_empty() {
                parts.push(format!("failed steps: {}", failed.join(", ")));
            }
        }

        let log_url =
            format!("{API_BASE}/repos/{}/{}/actions/jobs/{job_id}/logs", self.owner, self.repo);
        if let Some(resp) = self.get_best_effort(&log_url) {
            let log = String::from_utf8_lossy(&resp.body);
            let tail = tail_lines(&log, LOG_TAIL_LINES);
            if !tail.is_empty() {
                parts.push(tail);
            }
        }

        if parts.is_empty() { None } else { Some(parts.join("\n")) }
    }

    fn build_failure(&self, run: &GhCheckRun) -> CiFailure {
        let mut detail = String::new();
        if let Some(output) = &run.output {
            if let Some(title) = &output.title {
                detail.push_str(title);
            }
            if let Some(summary) = &output.summary {
                if !detail.is_empty() {
                    detail.push('\n');
                }
                detail.push_str(summary);
            }
        }
        if let Some(details_url) = &run.details_url
            && let Some(more) = self.actions_detail(details_url)
        {
            if !detail.is_empty() {
                detail.push('\n');
            }
            detail.push_str(&more);
        }
        CiFailure {
            name: run.name.clone(),
            url: run.html_url.clone(),
            detail: cap_tail(detail, DETAIL_CAP_BYTES),
        }
    }
}

impl<H: Http> Forge for GithubForge<H> {
    fn open_pull_request(&self, spec: &PullRequestSpec) -> Result<PullRequest, ForgeError> {
        // Idempotency first: the scheduler calls this after every run that reports done, so the
        // second and later calls must find the first call's pull request rather than trying —
        // and failing on a duplicate — to open a second one.
        let head_qualifier = format!("{}:{}", self.owner, spec.head);
        let list_url = format!(
            "{API_BASE}/repos/{}/{}/pulls?state=open&head={}&per_page=1",
            self.owner,
            self.repo,
            urlencode(&head_qualifier)
        );
        let existing: Vec<GhPullRequest> = self.get_json(&list_url)?;
        if let Some(gh) = existing.into_iter().next() {
            if gh.base.r#ref == spec.base {
                return Ok(to_pull_request(gh));
            }
            // Found, but pointed at a base the scheduler no longer wants — a lower branch of
            // the stack merged, most often. Returning it as is would leave the scheduler
            // recording the base it computed while the provider targets another, and the pull
            // request body claiming a stack that is over. Retargeted with the body, because the
            // body is where the old base is named; the title has no base in it and is left.
            let url = format!("{API_BASE}/repos/{}/{}/pulls/{}", self.owner, self.repo, gh.number);
            let resp = self.send_json(
                "PATCH",
                &url,
                &json!({
                    "base": spec.base,
                    "body": spec.body,
                }),
            )?;
            let gh: GhPullRequest = parse(&resp)?;
            return Ok(to_pull_request(gh));
        }

        let url = format!("{API_BASE}/repos/{}/{}/pulls", self.owner, self.repo);
        let payload = json!({
            "title": spec.title,
            "body": spec.body,
            "head": spec.head,
            "base": spec.base,
        });
        let raw = serde_json::to_vec(&payload).map_err(|e| ForgeError::Permanent(e.to_string()))?;
        let resp = self.authed(|h| self.http.send_json("POST", &url, h, &raw))?;
        // A run that committed nothing new is not a delivery failure — it is nothing to
        // deliver, and `ForgeError::NothingToDeliver` is what tells the scheduler the
        // difference. GitHub's only signal for that case is a 422 with this exact phrase in the
        // body; every other 422 (a bad branch name, a closed base) is a real, permanent refusal.
        if resp.status == 422 {
            let snippet = body_snippet(&resp);
            if snippet.contains("No commits between") {
                return Err(ForgeError::NothingToDeliver(snippet));
            }
        }
        let resp = classify(resp)?;
        let gh: GhPullRequest = parse(&resp)?;
        Ok(to_pull_request(gh))
    }

    fn pull_request(&self, number: u64) -> Result<PullRequest, ForgeError> {
        let url = format!("{API_BASE}/repos/{}/{}/pulls/{number}", self.owner, self.repo);
        let gh: GhPullRequest = self.get_json(&url)?;
        Ok(to_pull_request(gh))
    }

    fn request_review(&self, number: u64, reviewer: &str) -> Result<(), ForgeError> {
        let url = format!(
            "{API_BASE}/repos/{}/{}/pulls/{number}/requested_reviewers",
            self.owner, self.repo
        );
        self.send_json("POST", &url, &json!({ "reviewers": [reviewer] }))?;
        Ok(())
    }

    fn reviews(&self, number: u64) -> Result<Vec<Review>, ForgeError> {
        let owner = self.owner.clone();
        let repo = self.repo.clone();
        let raw: Vec<GhReview> = self.paginate(|page| {
            format!("{API_BASE}/repos/{owner}/{repo}/pulls/{number}/reviews?per_page={PER_PAGE}&page={page}")
        })?;
        Ok(raw
            .into_iter()
            .map(|r| Review {
                id: r.id.to_string(),
                reviewer: r.user.login,
                commit_sha: r.commit_id,
                state: r.state,
                body: r.body.unwrap_or_default(),
                url: r.html_url,
            })
            .collect())
    }

    fn ci_status(&self, head_sha: &str) -> Result<CiStatus, ForgeError> {
        let owner = self.owner.clone();
        let repo = self.repo.clone();
        let sha = head_sha.to_string();
        // Unlike every other list endpoint this adapter reads, check runs arrive wrapped in an
        // object (`{ "check_runs": [...] }`), so `paginate`'s bare-array rule does not apply
        // and the loop is spelled out here with the same short-page stop.
        let mut runs: Vec<GhCheckRun> = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!(
                "{API_BASE}/repos/{owner}/{repo}/commits/{sha}/check-runs?per_page={PER_PAGE}&page={page}"
            );
            let batch: GhCheckRunsPage = self.get_json(&url)?;
            let got = batch.check_runs.len();
            runs.extend(batch.check_runs);
            if got < PER_PAGE as usize {
                break;
            }
            page += 1;
        }

        // No check runs at all reads the same as "still running" — a repo with no CI configured
        // for this head and a repo mid-run are indistinguishable from here, and the module doc
        // says the scheduler, not this trait, bounds that with a timeout.
        if runs.is_empty() {
            return Ok(CiStatus::Pending { running: vec![] });
        }

        const FAILING: &[&str] =
            &["failure", "timed_out", "cancelled", "action_required", "startup_failure"];
        let failing: Vec<&GhCheckRun> = runs
            .iter()
            .filter(|r| r.conclusion.as_deref().is_some_and(|c| FAILING.contains(&c)))
            .collect();
        if !failing.is_empty() {
            let failures = failing.iter().map(|r| self.build_failure(r)).collect();
            return Ok(CiStatus::Failure { failures });
        }

        let running: Vec<String> =
            runs.iter().filter(|r| r.status != "completed").map(|r| r.name.clone()).collect();
        if !running.is_empty() {
            return Ok(CiStatus::Pending { running });
        }

        Ok(CiStatus::Success)
    }

    fn review_comments(&self, number: u64) -> Result<Vec<ReviewComment>, ForgeError> {
        let owner = self.owner.clone();
        let repo = self.repo.clone();
        let raw: Vec<GhReviewComment> = self.paginate(|page| {
            format!("{API_BASE}/repos/{owner}/{repo}/pulls/{number}/comments?per_page={PER_PAGE}&page={page}")
        })?;
        Ok(raw
            .into_iter()
            // Replies are not surfaced — see the module doc on `ReviewComment`: a thread is
            // settled or not by its root, and this crate only ever replies to roots.
            .filter(|c| c.in_reply_to_id.is_none())
            .map(|c| ReviewComment {
                id: c.id.to_string(),
                author: c.user.login,
                path: c.path,
                line: c.line.or(c.original_line),
                body: c.body,
                url: c.html_url,
            })
            .collect())
    }

    fn reply(&self, number: u64, comment_id: &str, body: &str) -> Result<(), ForgeError> {
        let id: u64 = comment_id.parse().map_err(|_| {
            ForgeError::Permanent(format!("comment id {comment_id} is not numeric"))
        })?;
        let url = format!(
            "{API_BASE}/repos/{}/{}/pulls/{number}/comments/{id}/replies",
            self.owner, self.repo
        );
        self.send_json("POST", &url, &json!({ "body": body }))?;
        Ok(())
    }

    /// A pull request's conversation is its issue's, so this is the issues endpoint.
    fn comment(&self, number: u64, body: &str) -> Result<(), ForgeError> {
        let url = format!("{API_BASE}/repos/{}/{}/issues/{number}/comments", self.owner, self.repo);
        self.send_json("POST", &url, &json!({ "body": body }))?;
        Ok(())
    }

    /// Reads the pull request's threads once, then resolves each comment's from that read — a
    /// thread has no REST id, only its root's `databaseId` joins the two APIs.
    fn resolve_threads(&self, number: u64, comment_ids: &[String]) -> Vec<Result<(), ForgeError>> {
        let mut threads = match self.review_threads(number) {
            Ok(t) => t,
            Err(e) => return comment_ids.iter().map(|_| Err(e.clone())).collect(),
        };
        comment_ids
            .iter()
            .map(|comment_id| {
                let id: u64 = comment_id.parse().map_err(|_| {
                    ForgeError::Permanent(format!("comment id {comment_id} is not numeric"))
                })?;
                match threads.remove(&id) {
                    Some(t) if !t.is_resolved => self.resolve_review_thread(&t.id),
                    // Already resolved, or its root comment is gone: nothing left to do either way.
                    _ => Ok(()),
                }
            })
            .collect()
    }
}

/// Maps a response onto [`ForgeError`]. 401/404/422 and a bare 403 will not resolve by
/// retrying; 429 and a 403 carrying a rate-limit signal will. 5xx is the provider's own
/// problem, not the request's, so it is transient too.
fn classify(resp: HttpResponse) -> Result<HttpResponse, ForgeError> {
    if (200..300).contains(&resp.status) {
        return Ok(resp);
    }
    let snippet = body_snippet(&resp);
    let rate_limit_signal = resp.header("retry-after").is_some()
        || resp.header("x-ratelimit-remaining").is_some_and(|v| v == "0");
    match resp.status {
        429 => Err(ForgeError::Transient(format!("rate limited: {snippet}"))),
        403 if rate_limit_signal => Err(ForgeError::Transient(format!("rate limited: {snippet}"))),
        401 | 403 | 404 | 422 => Err(ForgeError::Permanent(format!("{}: {snippet}", resp.status))),
        500..=599 => Err(ForgeError::Transient(format!("{}: {snippet}", resp.status))),
        other => Err(ForgeError::Transient(format!("{other}: {snippet}"))),
    }
}

fn parse<T: DeserializeOwned>(resp: &HttpResponse) -> Result<T, ForgeError> {
    serde_json::from_slice(&resp.body)
        .map_err(|e| ForgeError::Permanent(format!("parsing response: {e}")))
}

fn body_snippet(resp: &HttpResponse) -> String {
    let text = String::from_utf8_lossy(&resp.body);
    text.chars().take(200).collect()
}

/// Extracts the job id from a `details_url` shaped like
/// `https://github.com/o/r/actions/runs/<run_id>/job/<job_id>` — the only form the log/step
/// lookup in [`GithubForge::actions_detail`] knows how to follow. Checking for both the `runs`
/// and `job` segments, not just parsing the trailing number, is what keeps a non-Actions
/// `details_url` (a third-party CI's own dashboard link) from being treated as one by accident.
fn actions_job_id(details_url: &str) -> Option<u64> {
    let parts: Vec<&str> = details_url.split('/').collect();
    let runs_pos = parts.iter().position(|p| *p == "runs")?;
    let job_pos = parts.iter().position(|p| *p == "job")?;
    if job_pos <= runs_pos {
        return None;
    }
    parts.get(runs_pos + 1)?.parse::<u64>().ok()?;
    parts.get(job_pos + 1)?.parse().ok()
}

/// The last `n` lines of a job log, with each line's leading GitHub Actions timestamp
/// (`2024-01-15T10:30:00.1234567Z `) stripped — the timestamp is noise in an agent's prompt,
/// and stripping it is what keeps a 120-line tail from spending half its budget on repeated
/// prefixes.
fn tail_lines(log: &str, n: usize) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|l| strip_timestamp(l)).collect::<Vec<_>>().join("\n")
}

fn strip_timestamp(line: &str) -> &str {
    let Some(space) = line.find(' ') else { return line };
    let prefix = &line[..space];
    if prefix.contains('T') && prefix.ends_with('Z') { &line[space + 1..] } else { line }
}

/// Truncates `s` to at most `max_bytes`, keeping the *tail* — a compiler error sits at the end
/// of a failing build's output, not the start, so a head-truncated detail would routinely lose
/// exactly the line an agent needs.
fn cap_tail(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// The only characters this adapter ever puts in a query value: an owner/branch qualifier for
/// `head=`. Covers exactly that rather than pulling in a general-purpose URL crate.
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::tracker::github::HttpTransportError;

    struct FakeHttp {
        inner: Mutex<FakeHttpInner>,
    }

    #[derive(Default)]
    struct FakeHttpInner {
        responses: VecDeque<Result<HttpResponse, HttpTransportError>>,
        gets: Vec<String>,
        writes: Vec<(String, String, Value)>,
    }

    impl FakeHttp {
        fn new() -> Self {
            Self { inner: Mutex::new(FakeHttpInner::default()) }
        }

        fn push(&self, resp: Result<HttpResponse, HttpTransportError>) {
            self.inner.lock().unwrap().responses.push_back(resp);
        }

        fn gets(&self) -> Vec<String> {
            self.inner.lock().unwrap().gets.clone()
        }

        /// Method, URL and parsed JSON body of every write, in order.
        fn writes(&self) -> Vec<(String, String, Value)> {
            self.inner.lock().unwrap().writes.clone()
        }
    }

    impl Http for FakeHttp {
        fn get(
            &self,
            url: &str,
            _headers: &[(&str, String)],
        ) -> Result<HttpResponse, HttpTransportError> {
            let mut g = self.inner.lock().unwrap();
            g.gets.push(url.to_string());
            g.responses
                .pop_front()
                .unwrap_or_else(|| Err(HttpTransportError("no scripted response".into())))
        }

        fn send_json(
            &self,
            method: &str,
            url: &str,
            _headers: &[(&str, String)],
            body: &[u8],
        ) -> Result<HttpResponse, HttpTransportError> {
            let mut g = self.inner.lock().unwrap();
            let parsed = serde_json::from_slice(body).unwrap_or(Value::Null);
            g.writes.push((method.to_string(), url.to_string(), parsed));
            g.responses
                .pop_front()
                .unwrap_or_else(|| Err(HttpTransportError("no scripted response".into())))
        }
    }

    fn ok(body: Value) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status: 200,
            headers: HashMap::new(),
            body: body.to_string().into_bytes(),
        })
    }

    fn status(
        code: u16,
        headers: &[(&str, &str)],
        body: Value,
    ) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status: code,
            headers: headers.iter().map(|(k, v)| (k.to_lowercase(), v.to_string())).collect(),
            body: body.to_string().into_bytes(),
        })
    }

    fn forge(http: FakeHttp) -> GithubForge<FakeHttp> {
        GithubForge::new(http, "o", "r", "tok")
    }

    fn gh_pr(number: u64, head_sha: &str, base: &str, state: &str) -> Value {
        json!({
            "number": number,
            "html_url": format!("https://github.com/o/r/pull/{number}"),
            "head": { "sha": head_sha },
            "base": { "ref": base },
            "state": state,
            "requested_reviewers": [],
        })
    }

    #[test]
    fn an_existing_open_pull_request_for_the_head_is_reused_and_no_post_is_made() {
        let http = FakeHttp::new();
        http.push(ok(json!([gh_pr(7, "sha1", "master", "open")])));
        let f = forge(http);

        let spec = PullRequestSpec {
            title: "t".into(),
            body: "b".into(),
            head: "feature".into(),
            base: "master".into(),
        };
        let pr = f.open_pull_request(&spec).unwrap();

        assert_eq!(pr.number, 7);
        assert!(f.http.writes().is_empty(), "an existing pull request must not be re-opened");
        assert!(f.http.gets()[0].contains("head=o%3Afeature"));
    }

    /// Finding 1 on #47: #42 is stacked on #43, and when #43 merges, #42's base has to move to
    /// `master`. Reusing the open pull request as found dropped the new base on the floor, so
    /// the scheduler recorded one thing and the provider targeted another.
    #[test]
    fn a_reused_pull_request_whose_desired_base_has_changed_is_retargeted() {
        let http = FakeHttp::new();
        http.push(ok(json!([gh_pr(7, "sha1", "crew/lower", "open")])));
        http.push(ok(gh_pr(7, "sha1", "master", "open")));
        let f = forge(http);

        let spec = PullRequestSpec {
            title: "t".into(),
            body: "no longer stacked".into(),
            head: "feature".into(),
            base: "master".into(),
        };
        let pr = f.open_pull_request(&spec).unwrap();

        assert_eq!(pr.number, 7, "the same pull request, not a second one");
        assert_eq!(pr.base, "master", "and it now targets what the scheduler asked for");
        let writes = f.http.writes();
        assert_eq!(writes.len(), 1, "one retarget, no open: {writes:?}");
        let (method, url, body) = &writes[0];
        assert_eq!(method, "PATCH");
        assert!(url.ends_with("/repos/o/r/pulls/7"), "{url}");
        assert_eq!(body["base"], "master");
        assert_eq!(body["body"], "no longer stacked", "the body named the old base; it moves too");
        assert!(body.get("head").is_none(), "the head is the one thing a retarget never touches");
    }

    #[test]
    fn a_422_saying_no_commits_between_maps_to_nothing_to_deliver() {
        let http = FakeHttp::new();
        http.push(ok(json!([]))); // the idempotency lookup finds nothing
        http.push(status(422, &[], json!({ "message": "No commits between master and feature" })));
        let f = forge(http);

        let spec = PullRequestSpec {
            title: "t".into(),
            body: "b".into(),
            head: "feature".into(),
            base: "master".into(),
        };
        let err = f.open_pull_request(&spec).unwrap_err();
        assert!(matches!(err, ForgeError::NothingToDeliver(_)), "got {err:?}");
    }

    #[test]
    fn a_422_for_any_other_reason_is_permanent_not_nothing_to_deliver() {
        let http = FakeHttp::new();
        http.push(ok(json!([])));
        http.push(status(422, &[], json!({ "message": "Validation failed: head is invalid" })));
        let f = forge(http);

        let spec = PullRequestSpec {
            title: "t".into(),
            body: "b".into(),
            head: "feature".into(),
            base: "master".into(),
        };
        let err = f.open_pull_request(&spec).unwrap_err();
        assert!(matches!(err, ForgeError::Permanent(_)), "got {err:?}");
    }

    #[test]
    fn a_merged_pull_request_parses_as_merged_even_though_its_state_is_closed() {
        let http = FakeHttp::new();
        let mut pr = gh_pr(9, "sha2", "master", "closed");
        pr["merged"] = json!(true);
        http.push(ok(pr));
        let f = forge(http);

        let got = f.pull_request(9).unwrap();
        assert_eq!(got.state, PrState::Merged);
    }

    #[test]
    fn a_closed_unmerged_pull_request_parses_as_closed() {
        let http = FakeHttp::new();
        http.push(ok(gh_pr(9, "sha2", "master", "closed")));
        let f = forge(http);

        let got = f.pull_request(9).unwrap();
        assert_eq!(got.state, PrState::Closed);
    }

    /// #159: `dirty` is a conflict whatever `mergeable` says, and a `null` is still computing.
    #[test]
    fn mergeability_parses_dirty_as_a_conflict_and_null_as_unknown() {
        let read = |mergeable: Value, state: Value| {
            let http = FakeHttp::new();
            let mut pr = gh_pr(9, "sha2", "master", "open");
            pr["mergeable"] = mergeable;
            pr["mergeable_state"] = state;
            http.push(ok(pr));
            forge(http).pull_request(9).unwrap().mergeable
        };
        assert_eq!(read(json!(false), json!("dirty")), Some(false));
        assert_eq!(read(Value::Null, json!("dirty")), Some(false));
        assert_eq!(read(Value::Null, json!("unknown")), None);
        assert_eq!(read(json!(true), json!("blocked")), Some(true));
    }

    #[test]
    fn request_review_posts_exactly_the_reviewers_array_and_nothing_else() {
        let http = FakeHttp::new();
        http.push(ok(json!({})));
        let f = forge(http);

        f.request_review(7, "alice").unwrap();

        let w = f.http.writes();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, "POST");
        assert_eq!(w[0].1, "https://api.github.com/repos/o/r/pulls/7/requested_reviewers");
        assert_eq!(w[0].2, json!({ "reviewers": ["alice"] }));
    }

    fn gh_check_run(
        name: &str,
        status: &str,
        conclusion: Option<&str>,
        details_url: Option<&str>,
    ) -> Value {
        json!({
            "name": name,
            "status": status,
            "conclusion": conclusion,
            "html_url": "https://github.com/o/r/runs/1",
            "details_url": details_url,
        })
    }

    #[test]
    fn ci_status_with_an_empty_check_run_list_is_pending() {
        let http = FakeHttp::new();
        http.push(ok(json!({ "check_runs": [] })));
        let f = forge(http);
        assert_eq!(f.ci_status("sha").unwrap(), CiStatus::Pending { running: vec![] });
    }

    #[test]
    fn a_still_running_check_with_no_failure_is_pending_and_named() {
        let http = FakeHttp::new();
        http.push(ok(json!({ "check_runs": [
            gh_check_run("build", "in_progress", None, None),
            gh_check_run("lint", "completed", Some("success"), None),
        ] })));
        let f = forge(http);
        assert_eq!(
            f.ci_status("sha").unwrap(),
            CiStatus::Pending { running: vec!["build".into()] }
        );
    }

    #[test]
    fn a_completed_successful_check_is_success() {
        let http = FakeHttp::new();
        http.push(ok(json!({
            "check_runs": [gh_check_run("build", "completed", Some("success"), None)]
        })));
        let f = forge(http);
        assert_eq!(f.ci_status("sha").unwrap(), CiStatus::Success);
    }

    #[test]
    fn ci_status_maps_a_failed_check_run_to_failure_and_gathers_step_names_and_log_tail() {
        let http = FakeHttp::new();
        http.push(ok(json!({
            "check_runs": [gh_check_run(
                "fmt + clippy + test",
                "completed",
                Some("failure"),
                Some("https://github.com/o/r/actions/runs/55/job/66"),
            )]
        })));
        http.push(ok(json!({
            "steps": [
                { "name": "checkout", "conclusion": "success" },
                { "name": "test", "conclusion": "failure" },
            ]
        })));
        http.push(ok(json!("2024-01-15T10:30:00.1234567Z error: something broke\nnext line")));
        let f = forge(http);

        let status = f.ci_status("sha").unwrap();
        let CiStatus::Failure { failures } = status else { panic!("expected Failure") };
        assert_eq!(failures.len(), 1);
        assert!(failures[0].detail.contains("failed steps: test"), "{}", failures[0].detail);
        assert!(failures[0].detail.contains("error: something broke"), "{}", failures[0].detail);
        assert!(
            !failures[0].detail.contains("2024-01-15T10:30:00"),
            "the timestamp must be stripped"
        );
    }

    #[test]
    fn a_log_fetch_failure_still_returns_failure_with_the_summary_it_had() {
        let http = FakeHttp::new();
        let mut run = gh_check_run(
            "fmt + clippy + test",
            "completed",
            Some("failure"),
            Some("https://github.com/o/r/actions/runs/55/job/66"),
        );
        run["output"] =
            json!({ "title": "Process completed with exit code 1", "summary": "see log" });
        http.push(ok(json!({ "check_runs": [run] })));
        http.push(status(500, &[], json!({}))); // job fetch fails
        http.push(status(500, &[], json!({}))); // log fetch fails
        let f = forge(http);

        let status = f.ci_status("sha").unwrap();
        let CiStatus::Failure { failures } = status else { panic!("expected Failure") };
        assert_eq!(failures.len(), 1);
        assert!(failures[0].detail.contains("Process completed with exit code 1"));
        assert!(failures[0].detail.contains("see log"));
    }

    fn gh_review_comment(id: u64, in_reply_to: Option<u64>) -> Value {
        json!({
            "id": id,
            "user": { "login": "reviewer" },
            "path": "src/lib.rs",
            "line": 10,
            "body": "please fix",
            "html_url": format!("https://github.com/o/r/pull/1#discussion_r{id}"),
            "in_reply_to_id": in_reply_to,
        })
    }

    #[test]
    fn review_comments_drops_replies_and_keeps_roots() {
        let http = FakeHttp::new();
        http.push(ok(json!([gh_review_comment(1, None), gh_review_comment(2, Some(1))])));
        let f = forge(http);

        let got = f.review_comments(1).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "1");
    }

    #[test]
    fn reply_posts_to_the_replies_endpoint_under_the_root_comment() {
        let http = FakeHttp::new();
        http.push(ok(json!({})));
        let f = forge(http);

        f.reply(1, "42", "done").unwrap();

        let w = f.http.writes();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, "POST");
        assert_eq!(w[0].1, "https://api.github.com/repos/o/r/pulls/1/comments/42/replies");
        assert_eq!(w[0].2, json!({ "body": "done" }));
    }

    #[test]
    fn a_review_carries_its_id_summary_and_url() {
        let http = FakeHttp::new();
        http.push(ok(json!([{
            "id": 5325971434u64, "user": { "login": "copilot-pull-request-reviewer[bot]" },
            "commit_id": "b414d2c", "state": "COMMENTED", "body": "Correct the dirty-tree status.",
            "html_url": "https://github.com/o/r/pull/1#pullrequestreview-5325971434",
        }])));
        let f = forge(http);

        let got = f.reviews(1).unwrap();
        assert_eq!(got[0].id, "5325971434");
        assert_eq!(got[0].body, "Correct the dirty-tree status.");
        assert!(got[0].url.as_deref().unwrap().ends_with("pullrequestreview-5325971434"));
    }

    #[test]
    fn a_pull_request_comment_posts_to_its_issue_conversation() {
        let http = FakeHttp::new();
        http.push(ok(json!({})));
        let f = forge(http);

        f.comment(1, "**Rejected**").unwrap();

        let w = f.http.writes();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, "POST");
        assert_eq!(w[0].1, "https://api.github.com/repos/o/r/issues/1/comments");
        assert_eq!(w[0].2, json!({ "body": "**Rejected**" }));
    }

    #[test]
    fn a_reply_with_a_non_numeric_comment_id_is_refused_before_any_request() {
        let f = forge(FakeHttp::new());
        let err = f.reply(1, "not-a-number", "done").unwrap_err();
        assert!(matches!(err, ForgeError::Permanent(_)));
        assert!(f.http.writes().is_empty());
    }

    fn gh_threads(threads: &[(&str, bool, u64)], next: Option<&str>) -> Value {
        let nodes: Vec<Value> = threads
            .iter()
            .map(|(id, resolved, root)| {
                json!({
                    "id": id,
                    "isResolved": resolved,
                    "comments": { "nodes": [{ "databaseId": root }] },
                })
            })
            .collect();
        json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
            "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
            "nodes": nodes,
        } } } } })
    }

    fn resolve_one(f: &GithubForge<FakeHttp>, comment_id: &str) -> Result<(), ForgeError> {
        let mut results = f.resolve_threads(7, &[comment_id.to_string()]);
        assert_eq!(results.len(), 1);
        results.remove(0)
    }

    fn resolved(id: &str, is_resolved: Value) -> Value {
        json!({ "data": { "resolveReviewThread": {
            "thread": { "id": id, "isResolved": is_resolved } } } })
    }

    /// #100: the mutation's answer was discarded, so a thread GitHub declined to resolve was
    /// recorded resolved and never tried again. A permanent error is what puts it on the row.
    #[test]
    fn a_resolve_the_provider_did_not_apply_is_not_recorded_as_resolved() {
        for answer in [json!(false), Value::Null] {
            let http = FakeHttp::new();
            http.push(ok(gh_threads(&[("T_mine", false, 42)], None)));
            http.push(ok(resolved("T_mine", answer.clone())));
            let err = resolve_one(&forge(http), "42").unwrap_err();
            assert!(
                matches!(&err, ForgeError::Permanent(m) if m.contains("T_mine")),
                "isResolved {answer}: got {err:?}"
            );
        }
    }

    /// Review on #102: a thread deleted between the listing and the mutation comes back as
    /// `thread: null`, which is the idempotent "nothing left to resolve", not a refusal.
    #[test]
    fn a_thread_deleted_before_the_mutation_counts_as_resolved() {
        let http = FakeHttp::new();
        http.push(ok(gh_threads(&[("T_mine", false, 42)], None)));
        http.push(ok(json!({ "data": { "resolveReviewThread": { "thread": null } } })));
        resolve_one(&forge(http), "42").unwrap();
    }

    /// #100: each comment paged through every thread again, so the cost was verdicts × pages.
    #[test]
    fn resolving_several_comments_reads_the_threads_once() {
        let http = FakeHttp::new();
        http.push(ok(gh_threads(
            &[("T_a", false, 41), ("T_b", false, 42), ("T_c", false, 43)],
            None,
        )));
        for id in ["T_a", "T_b", "T_c"] {
            http.push(ok(resolved(id, json!(true))));
        }
        let f = forge(http);

        let ids: Vec<String> = ["41", "42", "43"].map(String::from).into();
        let results = f.resolve_threads(7, &ids);

        assert!(results.iter().all(Result::is_ok), "{results:?}");
        let w = f.http.writes();
        let queries =
            w.iter().filter(|(_, _, b)| b["query"].as_str().unwrap().contains("reviewThreads"));
        assert_eq!(queries.count(), 1, "one threads query: {w:?}");
        let mutated: Vec<&Value> = w[1..].iter().map(|(_, _, b)| &b["variables"]["id"]).collect();
        assert_eq!(mutated, [&json!("T_a"), &json!("T_b"), &json!("T_c")]);
    }

    #[test]
    fn a_failed_thread_read_fails_every_comment_in_the_batch() {
        let http = FakeHttp::new();
        http.push(status(502, &[], json!({})));
        let f = forge(http);
        let results = f.resolve_threads(7, &["41".to_string(), "42".to_string()]);
        assert_eq!(results.len(), 2);
        assert!(
            results.iter().all(|r| r.as_ref().is_err_and(ForgeError::retryable)),
            "{results:?}"
        );
        assert_eq!(f.http.writes().len(), 1, "no mutation without the threads");
    }

    #[test]
    fn resolve_threads_finds_the_thread_by_its_root_comment_and_resolves_it() {
        let http = FakeHttp::new();
        http.push(ok(gh_threads(&[("T_other", false, 41)], Some("cur1"))));
        http.push(ok(gh_threads(&[("T_mine", false, 42)], None)));
        http.push(ok(resolved("T_mine", json!(true))));
        let f = forge(http);

        resolve_one(&f, "42").unwrap();

        let w = f.http.writes();
        assert_eq!(w.len(), 3, "two pages of threads, then the mutation: {w:?}");
        assert!(w.iter().all(|(m, u, _)| m == "POST" && u == "https://api.github.com/graphql"));
        assert_eq!(
            w[0].2["variables"],
            json!({ "owner": "o", "repo": "r", "number": 7, "after": null })
        );
        assert_eq!(w[1].2["variables"]["after"], "cur1", "the second page follows the cursor");
        assert!(w[2].2["query"].as_str().unwrap().contains("resolveReviewThread"));
        assert_eq!(w[2].2["variables"], json!({ "id": "T_mine" }), "the matching thread only");
    }

    #[test]
    fn resolving_an_already_resolved_thread_is_ok_and_sends_no_mutation() {
        let http = FakeHttp::new();
        http.push(ok(gh_threads(&[("T_mine", true, 42)], None)));
        let f = forge(http);

        resolve_one(&f, "42").unwrap();

        assert_eq!(f.http.writes().len(), 1, "the query only: {:?}", f.http.writes());
    }

    #[test]
    fn a_graphql_error_in_a_200_is_classified_rather_than_read_as_success() {
        let http = FakeHttp::new();
        http.push(ok(json!({ "errors": [{ "type": "RATE_LIMITED", "message": "slow down" }] })));
        let err = resolve_one(&forge(http), "42").unwrap_err();
        assert!(err.retryable(), "got {err:?}");

        let http = FakeHttp::new();
        http.push(ok(gh_threads(&[("T_mine", false, 42)], None)));
        http.push(ok(
            json!({ "data": null, "errors": [{ "type": "FORBIDDEN", "message": "no" }] }),
        ));
        let err = resolve_one(&forge(http), "42").unwrap_err();
        assert!(matches!(err, ForgeError::Permanent(_)), "got {err:?}");
    }

    #[test]
    fn a_429_classifies_as_transient() {
        let http = FakeHttp::new();
        http.push(status(429, &[], json!({})));
        let f = forge(http);
        let err = f.pull_request(1).unwrap_err();
        assert!(err.retryable(), "got {err:?}");
    }

    #[test]
    fn a_401_classifies_as_permanent() {
        let http = FakeHttp::new();
        http.push(status(401, &[], json!({ "message": "Bad credentials" })));
        let f = forge(http);
        let err = f.pull_request(1).unwrap_err();
        assert!(!err.retryable(), "got {err:?}");
        assert!(matches!(err, ForgeError::Permanent(_)));
    }

    #[test]
    fn a_403_with_a_rate_limit_signal_is_transient_but_a_bare_403_is_permanent() {
        let http = FakeHttp::new();
        http.push(status(403, &[("retry-after", "30")], json!({})));
        let err = forge(http).pull_request(1).unwrap_err();
        assert!(err.retryable(), "got {err:?}");

        let http = FakeHttp::new();
        http.push(status(403, &[], json!({})));
        let err = forge(http).pull_request(1).unwrap_err();
        assert!(!err.retryable(), "a bare 403 is a permission error, not a rate limit: {err:?}");
    }

    #[test]
    fn a_5xx_classifies_as_transient() {
        let http = FakeHttp::new();
        http.push(status(503, &[], json!({})));
        let f = forge(http);
        let err = f.pull_request(1).unwrap_err();
        assert!(err.retryable(), "got {err:?}");
    }

    #[test]
    fn reviews_paginate_the_same_way_the_tracker_does() {
        let http = FakeHttp::new();
        let full_page: Vec<_> = (1..=PER_PAGE)
            .map(|_| json!({ "id": 1, "user": { "login": "r" }, "commit_id": "s", "state": "APPROVED" }))
            .collect();
        http.push(ok(Value::Array(full_page)));
        http.push(ok(json!([{
            "id": 2, "user": { "login": "r2" }, "commit_id": "s2", "state": "COMMENTED",
            "body": null,
        }])));
        let f = forge(http);

        let got = f.reviews(1).unwrap();
        assert_eq!(got.len(), PER_PAGE as usize + 1);
        assert!(f.http.gets()[0].contains("page=1"));
        assert!(f.http.gets()[1].contains("page=2"));
    }
}

/// #64: a 401 on an App token re-mints and retries once, on every path that sends.
#[cfg(test)]
mod reauth_tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicU32, Ordering};

    use parking_lot::Mutex;

    use super::*;
    use crate::credentials::CredentialError;

    /// Hands out `tok-N`, moving to the next only once invalidated, as `GithubApp` would.
    #[derive(Default)]
    struct Rotating(AtomicU32);

    impl Credentials for Rotating {
        fn token(&self) -> Result<String, CredentialError> {
            Ok(format!("tok-{}", self.0.load(Ordering::SeqCst)))
        }

        fn invalidate(&self) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    /// Answers 401 to any bearer but `accept`; otherwise the next scripted body.
    struct Github {
        accept: &'static str,
        bodies: Mutex<VecDeque<(u16, Value)>>,
        sent: Mutex<Vec<(String, String)>>,
    }

    impl Github {
        fn new(accept: &'static str, bodies: Vec<(u16, Value)>) -> Arc<Self> {
            Arc::new(Self { accept, bodies: Mutex::new(bodies.into()), sent: Mutex::default() })
        }

        fn answer(&self, what: String, headers: &[(&str, String)]) -> HttpResponse {
            let bearer = headers.iter().find(|(k, _)| *k == "Authorization").unwrap().1.clone();
            self.sent.lock().push((what, bearer.clone()));
            let (status, body) = if bearer == format!("Bearer {}", self.accept) {
                self.bodies.lock().pop_front().expect("scripted")
            } else {
                (401, json!({ "message": "Bad credentials" }))
            };
            HttpResponse { status, headers: HashMap::new(), body: body.to_string().into_bytes() }
        }
    }

    impl Http for Arc<Github> {
        fn get(
            &self,
            url: &str,
            headers: &[(&str, String)],
        ) -> Result<HttpResponse, HttpTransportError> {
            Ok(self.answer(format!("GET {url}"), headers))
        }

        fn send_json(
            &self,
            method: &str,
            url: &str,
            headers: &[(&str, String)],
            _body: &[u8],
        ) -> Result<HttpResponse, HttpTransportError> {
            Ok(self.answer(format!("{method} {url}"), headers))
        }
    }

    fn forge(gh: &Arc<Github>) -> GithubForge<Arc<Github>> {
        GithubForge::new(gh.clone(), "o", "r", "unused")
            .with_credentials(Arc::new(Rotating::default()))
    }

    #[test]
    fn a_token_revoked_before_its_expiry_is_replaced_and_the_pull_request_still_opens() {
        let pr = json!({
            "number": 9,
            "html_url": "https://github.com/o/r/pull/9",
            "head": { "sha": "abc" },
            "base": { "ref": "master" },
            "state": "open",
        });
        // `tok-0` was revoked; only the re-minted `tok-1` is accepted.
        let gh = Github::new("tok-1", vec![(200, json!([])), (201, pr)]);
        let spec = PullRequestSpec {
            title: "t".into(),
            body: "b".into(),
            head: "crew/x".into(),
            base: "master".into(),
        };

        let opened = forge(&gh).open_pull_request(&spec).expect("delivery is not handed off");
        assert_eq!(opened.number, 9);
        let sent = gh.sent.lock().clone();
        assert_eq!(
            sent.iter()
                .map(|(w, b)| (w.split(' ').next().unwrap(), b.as_str()))
                .collect::<Vec<_>>(),
            vec![("GET", "Bearer tok-0"), ("GET", "Bearer tok-1"), ("POST", "Bearer tok-1")],
            "one retry on the refused read, and the write goes out with the fresh token"
        );
    }

    #[test]
    fn a_credential_refused_twice_is_permanent_after_exactly_one_retry() {
        let gh = Github::new("never", vec![]);
        let err = forge(&gh).ci_status("abc").unwrap_err();
        assert!(matches!(err, ForgeError::Permanent(_)), "got {err:?}");
        assert_eq!(gh.sent.lock().len(), 2, "one retry, not a loop");
    }

    #[test]
    fn a_static_token_is_not_retried_since_a_second_try_would_send_the_same_one() {
        let gh = Github::new("never", vec![]);
        let f = GithubForge::new(gh.clone(), "o", "r", "tok");
        assert!(matches!(f.ci_status("abc"), Err(ForgeError::Permanent(_))));
        assert_eq!(gh.sent.lock().len(), 1);
    }
}
