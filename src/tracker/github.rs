//! `Tracker` against GitHub Issues.
//!
//! ## The state-mapping problem
//!
//! GitHub Issues have exactly two workflow states: open and closed. This project's scheduler
//! wants an arbitrary set of `active_states` and `terminal_states` — enough to express, for
//! example, a busy "in review" state with its own `max_concurrent_by_state` cap, which
//! `crew.toml`'s demo config already relies on. Open/closed alone cannot express that.
//!
//! Three ways to add states on top of open/closed: a `state:<name>` label convention, a
//! Projects v2 board field, or open/closed plus assignee as a two-value stand-in. This adapter
//! uses **labels** — `state:in-progress`, `state:in-review`, and so on. Reasons:
//!
//! * It needs nothing beyond the REST Issues API this adapter already calls for
//!   `required_labels`. A Projects v2 field means a second API (GraphQL), a project number to
//!   configure, and a second pagination scheme.
//! * It supports as many states as the operator wants, unlike open/closed+assignee, which
//!   caps out at two.
//! * The label text *is* the state string — `active_states`/`terminal_states` in `crew.toml`
//!   read the same vocabulary an operator already sees on the issue.
//!
//! `closed` always wins over any `state:*` label when deriving an issue's state — an operator
//! who closes an issue without remembering to swap its label must still see it as terminal, or
//! reconciliation could keep a workspace open under a ticket nobody is tracking anymore. An
//! issue with no `state:*` label reports as `"open"`, which is enough on its own for a repo
//! that has not adopted the label convention: `active_states = ["open"]` matches every open
//! issue with `required_labels`, same as this crate's own backlog today.
//!
//! ## Rate-limit budget
//!
//! One poll costs one paginated fetch for `by_states` (one request per 100 open, labelled
//! issues) plus one request per currently-running issue for `by_ids`. At the authenticated
//! primary limit of 5000 requests/hour, `interval_ms` should keep
//! `(1 + agent.max_concurrent) * (3_600_000 / interval_ms)` comfortably under that — a 30s
//! interval at `max_concurrent = 2` is `3 * 120 = 360/hour`, nowhere near the ceiling.
//! `crew.github.toml` uses that combination. A 403/429 with a rate-limit signal still
//! classifies as [`crate::model::ErrorClass::RateLimited`] and backs off rather than escalates,
//! but a tight interval against a real repo will find that path often enough to be worth
//! avoiding up front.
//!
//! With `tracker.github_app` set, that 5000 is the App installation's own pool rather than the
//! operator's, so the poll no longer competes with whatever else their personal token is doing.
//! A mint costs one more request an hour.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{Tracker, TrackerError};
use crate::broker::TrackerWrites;
use crate::credentials::{Credentials, StaticToken};
use crate::model::Issue;

const API_BASE: &str = "https://api.github.com";
const PER_PAGE: u32 = 100;
const API_VERSION: &str = "2022-11-28";

// ---- the HTTP seam ---------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    /// Lowercased header names, so lookups don't depend on what casing the caller used.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(String::as_str)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("http transport error: {0}")]
pub struct HttpTransportError(pub String);

/// Everything `GithubTracker` needs from the network: one authenticated read, one
/// authenticated write.
///
/// The write half arrived with the tool broker and is used only by this type's
/// [`TrackerWrites`] impl — never by [`Tracker`], which stays a read kernel. Keeping them as
/// separate methods rather than one general `request` is what makes that split visible at the
/// seam: a fake can answer reads and refuse writes, and a reader can see at a glance which
/// call sites can mutate a ticket.
pub trait Http: Send + Sync {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<HttpResponse, HttpTransportError>;

    /// `method` is `POST`, `PATCH` or `PUT`; `body` is a JSON document.
    fn send_json(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, HttpTransportError>;
}

/// The real implementation, over `ureq`. Picked for its blocking API — `Tracker` methods are
/// synchronous, so an async client would need a runtime handle threaded through for no benefit
/// — and its default TLS backend is pure-Rust `rustls`, which needs no C toolchain to link.
pub struct UreqHttp {
    agent: ureq::Agent,
}

/// How long a connection may take to open. GitHub answers in well under a second.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The whole call, connect to last byte of the body. Search and GraphQL answers take seconds;
/// this is only the ceiling that turns a dead connection into an error.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

impl Default for UreqHttp {
    fn default() -> Self {
        Self::with_timeouts(CONNECT_TIMEOUT, CALL_TIMEOUT)
    }
}

impl UreqHttp {
    /// An unbounded call stops the scheduler, because every call runs on the tick's own thread
    /// and ureq sets no timeout by default (#176). A call past either bound fails as an
    /// `HttpTransportError`, which the tracker classes as retryable.
    pub fn with_timeouts(connect: Duration, call: Duration) -> Self {
        // ureq's default turns a non-2xx status into an `Err` that drops the response body and
        // headers — exactly the rate-limit header and body snippet `request()` needs to
        // classify the failure. Disabling it is what makes every status code, not just 2xx,
        // arrive as an ordinary `HttpResponse` for this adapter to read.
        //
        // A renamed repo makes every call 301 to `/repositories/<id>/...`; ureq's own default
        // (`RedirectAuthHeaders::Never`) never forwards `Authorization` on that redirect, so the
        // retry lands anonymous and burns the 60/hour IP-keyed limit in minutes (#68). `SameHost`
        // keeps the header only when the redirect stays on the same host under HTTPS, which is
        // this case, without weakening the cross-host protection the default exists for.
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            .timeout_connect(Some(connect))
            .timeout_global(Some(call))
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Http for UreqHttp {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<HttpResponse, HttpTransportError> {
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let mut resp = req.call().map_err(|e| HttpTransportError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| HttpTransportError(format!("reading response body: {e}")))?;
        Ok(HttpResponse { status, headers, body })
    }

    fn send_json(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, HttpTransportError> {
        let mut req = match method {
            "POST" => self.agent.post(url),
            "PATCH" => self.agent.patch(url),
            "PUT" => self.agent.put(url),
            other => return Err(HttpTransportError(format!("unsupported method {other}"))),
        };
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let mut resp = req
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|e| HttpTransportError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_lowercase(), v.to_str().unwrap_or_default().to_string()))
            .collect();
        let body = resp
            .body_mut()
            .read_to_vec()
            .map_err(|e| HttpTransportError(format!("reading response body: {e}")))?;
        Ok(HttpResponse { status, headers, body })
    }
}

// ---- GitHub's REST shape, trimmed to what this adapter reads --------------

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    node_id: String,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    html_url: String,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    assignees: Vec<GhUser>,
    created_at: String,
    /// Present (any shape) only on pull requests — GitHub's Issues API returns PRs too. Its
    /// presence, not its content, is the signal.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhUser {
    login: String,
}

/// `closed` overrides any `state:*` label; see the module doc for why. An issue with neither a
/// label nor a closed flag reports `"open"`.
fn derive_state(closed: bool, labels: &[String]) -> String {
    if closed {
        return "closed".into();
    }
    labels
        .iter()
        .find_map(|l| l.strip_prefix("state:"))
        .map(str::to_string)
        .unwrap_or_else(|| "open".into())
}

fn parse_created_at(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc3339).ok().map(|t| t.unix_timestamp() * 1000)
}

/// Without this an issue labelled `Agent` never matches a configured `agent` and is never
/// dispatched (#70): `routable` compares with plain equality against `required_labels`, which
/// `Config::normalize` has already given this same shape.
fn normalize_labels(labels: Vec<GhLabel>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for label in labels {
        let name = label.name.trim().to_lowercase();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Which issues are for Crew (#64). With `label` set, carrying it is the whole signal — an
/// assignee is neither needed nor sufficient, because a teammate taking a ticket must not hand it
/// to an agent. Unset, any assignee makes an issue dispatchable, as before the label existed.
/// `assignee`, when set, narrows either rule to one login; it is never required.
#[derive(Debug, Clone, Default)]
pub struct DispatchRule {
    pub label: Option<String>,
    pub assignee: Option<String>,
}

impl DispatchRule {
    fn dispatchable(&self, labels: &[String], assignees: &[GhUser]) -> bool {
        let marked = match &self.label {
            Some(label) => labels.iter().any(|l| l == label),
            None => !assignees.is_empty(),
        };
        let narrowed = self
            .assignee
            .as_ref()
            .is_none_or(|want| assignees.iter().any(|a| a.login.eq_ignore_ascii_case(want)));
        marked && narrowed
    }
}

fn to_issue(owner: &str, repo: &str, rule: &DispatchRule, gh: GhIssue) -> Issue {
    let labels = normalize_labels(gh.labels);
    let dispatchable = rule.dispatchable(&labels, &gh.assignees);
    let closed = gh.state == "closed";
    Issue {
        id: format!("{owner}/{repo}#{}", gh.number),
        identifier: format!("#{}", gh.number),
        title: gh.title,
        body: gh.body,
        state: derive_state(closed, &labels),
        priority: None, // GitHub has no native priority; an adapter over Projects v2 could add one.
        url: Some(gh.html_url),
        labels,
        dispatchable,
        created_at: parse_created_at(&gh.created_at),
        native_ref: Some(serde_json::json!({ "number": gh.number, "node_id": gh.node_id })),
        blocked_by: vec![], // GitHub's native issue-dependency graph is not read by this slice.
    }
}

// ---- the tracker ------------------------------------------------------------

pub struct GithubTracker<H: Http> {
    http: H,
    owner: String,
    repo: String,
    creds: Arc<dyn Credentials>,
    required_labels: Vec<String>,
    rule: DispatchRule,
}

impl<H: Http> GithubTracker<H> {
    /// A static token and the any-assignee rule: the `GITHUB_TOKEN` deployment, unchanged.
    pub fn new(http: H, owner: &str, repo: &str, token: &str, required_labels: &[String]) -> Self {
        Self {
            http,
            owner: owner.to_string(),
            repo: repo.to_string(),
            creds: Arc::new(StaticToken::new(token)),
            required_labels: required_labels.to_vec(),
            rule: DispatchRule::default(),
        }
    }

    pub fn with_credentials(mut self, creds: Arc<dyn Credentials>) -> Self {
        self.creds = creds;
        self
    }

    pub fn with_dispatch_rule(mut self, rule: DispatchRule) -> Self {
        self.rule = rule;
        self
    }

    fn headers(&self) -> Result<Vec<(&'static str, String)>, TrackerError> {
        Ok(vec![
            ("Authorization", format!("Bearer {}", self.creds.token()?)),
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", API_VERSION.to_string()),
            ("User-Agent", "crewd".to_string()),
        ])
    }

    /// Sends once, and once more with a fresh token if the first was refused with a 401: an
    /// installation token revoked before its expiry would otherwise fail a poll and a broker
    /// write that the next mint would have served. A refused request was not applied, so
    /// repeating it is safe; a second 401 is the credential really being wrong, and stands.
    fn authed(
        &self,
        send: impl Fn(&[(&str, String)]) -> Result<HttpResponse, HttpTransportError>,
    ) -> Result<HttpResponse, TrackerError> {
        let resp = send(&self.headers()?).map_err(|e| TrackerError::Request(e.0))?;
        if resp.status == 401 && self.creds.invalidate() {
            return send(&self.headers()?).map_err(|e| TrackerError::Request(e.0));
        }
        Ok(resp)
    }

    /// One authenticated GET, classified onto [`TrackerError`]. The only branch a caller needs
    /// to handle specially beyond this is "404 on a single-issue fetch", which `by_ids` treats
    /// as absence rather than as an error.
    fn request(&self, url: &str) -> Result<HttpResponse, TrackerError> {
        classify(self.authed(|h| self.http.get(url, h))?)
    }

    /// One authenticated write, classified the same way a read is — so a broker tool failing
    /// on a bad credential and a poll failing on one produce the same `TrackerError` variant
    /// and the same log level.
    fn write(&self, method: &str, url: &str, body: &Value) -> Result<HttpResponse, TrackerError> {
        let payload =
            serde_json::to_vec(body).map_err(|e| TrackerError::Response(e.to_string()))?;
        classify(self.authed(|h| self.http.send_json(method, url, h, &payload))?)
    }

    fn number_for(&self, issue_id: &str) -> Result<u64, TrackerError> {
        parse_dispatch_id(issue_id, &self.owner, &self.repo).ok_or_else(|| {
            TrackerError::Status(format!(
                "{issue_id} is not an issue in {}/{}",
                self.owner, self.repo
            ))
        })
    }

    /// Labels normalized here would be renamed on GitHub by the next `set_state`, which writes
    /// them back (#70), so this returns GitHub's own names and `by_ids` normalizes them itself.
    fn fetch_issue(&self, id: &str) -> Result<Option<GhIssue>, TrackerError> {
        // An id this tracker never issued (wrong owner/repo, or malformed) cannot be
        // "visible" to it — omit rather than error, same as a clean 404 below.
        let Some(number) = parse_dispatch_id(id, &self.owner, &self.repo) else {
            return Ok(None);
        };

        let url = format!("{API_BASE}/repos/{}/{}/issues/{number}", self.owner, self.repo);
        let resp = self.authed(|h| self.http.get(&url, h))?;
        if resp.status == 404 {
            return Ok(None); // genuinely gone: the caller's grace-count path handles this
        }
        if !(200..300).contains(&resp.status) {
            // Not "gone" — a real fetch failure. Surfacing it as an error rather than an
            // omission is the whole point of by_ids's "partial success is an error" rule:
            // the scheduler cannot otherwise tell a failed check apart from a clean miss.
            if resp.status == 401 {
                return Err(TrackerError::Auth(body_snippet(&resp)));
            }
            let rate_limited = resp.status == 403 || resp.status == 429;
            let rate_limit_signal = resp.header("retry-after").is_some()
                || resp.header("x-ratelimit-remaining").is_some_and(|v| v == "0");
            if rate_limited && rate_limit_signal {
                return Err(TrackerError::RateLimited);
            }
            return Err(TrackerError::Status(format!("{}: {}", resp.status, body_snippet(&resp))));
        }
        let gh = Self::parse_issue(&resp)?;
        if gh.pull_request.is_some() {
            return Ok(None); // a PR sharing the issue numbering space; never dispatchable
        }
        Ok(Some(gh))
    }

    fn parse_issues(resp: &HttpResponse) -> Result<Vec<GhIssue>, TrackerError> {
        serde_json::from_slice(&resp.body).map_err(|e| TrackerError::Response(e.to_string()))
    }

    fn parse_issue(resp: &HttpResponse) -> Result<GhIssue, TrackerError> {
        serde_json::from_slice(&resp.body).map_err(|e| TrackerError::Response(e.to_string()))
    }

    /// Only issues with `required_labels` and `state=open`: a repo's closed-issue history can
    /// be arbitrarily large and is never active, so pulling it every poll would burn the rate
    /// budget on rows this call always discards. `by_ids` still sees closed issues — that is
    /// how reconciliation notices a running issue's ticket got closed.
    ///
    /// The dispatch label joins the query too, so the poll's cost follows the marked issues
    /// rather than the repository's whole open backlog.
    fn fetch_open_labelled(&self) -> Result<Vec<GhIssue>, TrackerError> {
        let mut labels = self.required_labels.clone();
        if let Some(l) = &self.rule.label
            && !labels.contains(l)
        {
            labels.push(l.clone());
        }
        let labels = labels.join(",");
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!(
                "{API_BASE}/repos/{}/{}/issues?state=open&labels={}&per_page={PER_PAGE}&page={page}",
                self.owner,
                self.repo,
                urlencode(&labels),
            );
            // Fail the whole call on any page's error rather than returning what has been
            // gathered so far — a short list here is indistinguishable from "this is really
            // all of them," and the scheduler has no way to tell the difference back out.
            let resp = self.request(&url)?;
            let batch = Self::parse_issues(&resp)?;
            let got = batch.len();
            all.extend(batch);
            if got < PER_PAGE as usize {
                break;
            }
            page += 1;
        }
        Ok(all)
    }
}

impl<H: Http> Tracker for GithubTracker<H> {
    fn by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(vec![]);
        }
        let want: std::collections::HashSet<&str> = states.iter().map(|s| s.as_str()).collect();
        Ok(self
            .fetch_open_labelled()?
            .into_iter()
            .filter(|gh| gh.pull_request.is_none())
            .map(|gh| to_issue(&self.owner, &self.repo, &self.rule, gh))
            .filter(|issue| want.contains(issue.state_key().as_str()))
            .collect())
    }

    fn by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for id in ids {
            if let Some(gh) = self.fetch_issue(id)? {
                out.push(to_issue(&self.owner, &self.repo, &self.rule, gh));
            }
        }
        Ok(out)
    }
}

/// Maps a response onto [`TrackerError`]. Shared by the read and write paths so they cannot
/// drift — a 401 must mean `Auth` for both, or the scheduler's retryable/permanent split stops
/// being trustworthy for half the calls.
fn classify(resp: HttpResponse) -> Result<HttpResponse, TrackerError> {
    if (200..300).contains(&resp.status) {
        return Ok(resp);
    }
    if resp.status == 401 {
        return Err(TrackerError::Auth(body_snippet(&resp)));
    }
    let rate_limited = resp.status == 403 || resp.status == 429;
    let rate_limit_signal = resp.header("retry-after").is_some()
        || resp.header("x-ratelimit-remaining").is_some_and(|v| v == "0");
    if rate_limited && rate_limit_signal {
        return Err(TrackerError::RateLimited);
    }
    Err(TrackerError::Status(format!("{}: {}", resp.status, body_snippet(&resp))))
}

/// Ticket mutations, for the broker only.
///
/// The state mapping is the read path's run backwards: `by_states` derives a state from a
/// `state:<name>` label with `closed` overriding it, so moving an issue means rewriting that
/// label *and* the open/closed flag together. Doing only one of the two would produce an issue
/// whose state depends on which of the two signals you looked at — precisely the ambiguity the
/// "closed always wins" rule in the module doc exists to resolve.
///
/// `set_state` costs three requests (read labels, replace labels, set open/closed) rather than
/// one. That is the price of the label convention and it is charged only when an agent moves
/// its own ticket, which is bounded by the broker's own call budget — see the rate-limit budget
/// note in the module doc before assuming it is free.
impl<H: Http> TrackerWrites for GithubTracker<H> {
    fn comment(&self, issue_id: &str, body: &str) -> Result<String, TrackerError> {
        let number = self.number_for(issue_id)?;
        let url = format!("{API_BASE}/repos/{}/{}/issues/{number}/comments", self.owner, self.repo);
        let resp = self.write("POST", &url, &json!({ "body": body }))?;
        Ok(created_url(&resp).unwrap_or_else(|| format!("commented on {issue_id}")))
    }

    fn set_state(&self, issue_id: &str, state: &str) -> Result<String, TrackerError> {
        let number = self.number_for(issue_id)?;

        // Read-modify-write, because GitHub's label endpoint replaces the whole set: anything
        // not sent back is removed, and `required_labels` (the `agent` label this repo
        // dispatches on) living in that set means a blind write would make the issue
        // undispatchable.
        // Through `by_ids`, this would rename the operator's labels on every state change (#70).
        let current = self
            .fetch_issue(issue_id)?
            .ok_or_else(|| TrackerError::Status(format!("{issue_id} is not visible")))?;

        let closed = state == "closed";
        let mut labels: Vec<String> = current
            .labels
            .into_iter()
            .map(|l| l.name)
            .filter(|l| !l.trim().to_lowercase().starts_with("state:"))
            .collect();
        // `open` and `closed` are carried by the flag itself, so they get no label of their own
        // — a `state:open` label would be a second, disagreeing source of truth.
        if !closed && state != "open" {
            labels.push(format!("state:{state}"));
        }

        let labels_url =
            format!("{API_BASE}/repos/{}/{}/issues/{number}/labels", self.owner, self.repo);
        self.write("PUT", &labels_url, &json!({ "labels": labels }))?;

        let issue_url = format!("{API_BASE}/repos/{}/{}/issues/{number}", self.owner, self.repo);
        let want = if closed { "closed" } else { "open" };
        self.write("PATCH", &issue_url, &json!({ "state": want }))?;

        Ok(format!("{issue_id} is now {state}"))
    }

    fn link_pr(&self, issue_id: &str, url: &str) -> Result<String, TrackerError> {
        // GitHub has no "linked PR" field on an issue that is writable from the Issues API —
        // the real association is the cross-reference GitHub creates when the URL is mentioned,
        // and a comment is what creates one. Saying so plainly beats a `linked_prs` field that
        // silently means "we left a comment".
        let number = self.number_for(issue_id)?;
        let comments =
            format!("{API_BASE}/repos/{}/{}/issues/{number}/comments", self.owner, self.repo);
        let body = format!("Pull request: {url}");
        let resp = self.write("POST", &comments, &json!({ "body": body }))?;
        Ok(created_url(&resp).unwrap_or_else(|| format!("linked {url} to {issue_id}")))
    }
}

/// The `html_url` of whatever was just created, when the response carries one — a comment URL
/// is the most useful thing the agent can be handed back.
fn created_url(resp: &HttpResponse) -> Option<String> {
    serde_json::from_slice::<Value>(&resp.body).ok()?.get("html_url")?.as_str().map(str::to_string)
}

fn parse_dispatch_id(id: &str, owner: &str, repo: &str) -> Option<u64> {
    let rest = id.strip_prefix(owner)?.strip_prefix('/')?.strip_prefix(repo)?.strip_prefix('#')?;
    rest.parse().ok()
}

fn body_snippet(resp: &HttpResponse) -> String {
    let text = String::from_utf8_lossy(&resp.body);
    text.chars().take(200).collect()
}

fn urlencode(s: &str) -> String {
    // The only characters this adapter ever puts in a query value are label names and commas;
    // this covers exactly those without pulling in a general-purpose URL crate.
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ',' => "%2C".to_string(),
            ' ' => "%20".to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    struct FakeHttp {
        inner: Mutex<FakeHttpInner>,
    }

    #[derive(Default)]
    struct FakeHttpInner {
        responses: VecDeque<Result<HttpResponse, HttpTransportError>>,
        calls: Vec<String>,
        writes: Vec<(String, String, Value)>,
    }

    impl FakeHttp {
        fn new() -> Self {
            Self { inner: Mutex::new(FakeHttpInner::default()) }
        }

        fn push(&self, resp: Result<HttpResponse, HttpTransportError>) {
            self.inner.lock().unwrap().responses.push_back(resp);
        }

        fn calls(&self) -> Vec<String> {
            self.inner.lock().unwrap().calls.clone()
        }

        /// Method, URL and parsed body of every write, in order.
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
            g.calls.push(url.to_string());
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

    fn ok(body: serde_json::Value) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status: 200,
            headers: HashMap::new(),
            body: body.to_string().into_bytes(),
        })
    }

    fn status(code: u16, headers: &[(&str, &str)]) -> Result<HttpResponse, HttpTransportError> {
        Ok(HttpResponse {
            status: code,
            headers: headers.iter().map(|(k, v)| (k.to_lowercase(), v.to_string())).collect(),
            body: b"{}".to_vec(),
        })
    }

    fn gh_issue(number: u64, labels: &[&str], state: &str, assigned: bool) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "node_id": format!("node-{number}"),
            "title": format!("issue {number}"),
            "state": state,
            "html_url": format!("https://github.com/o/r/issues/{number}"),
            "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
            "assignees": if assigned { vec![serde_json::json!({"login": "someone"})] } else { vec![] },
            "created_at": "2024-01-15T10:30:00Z",
        })
    }

    fn tracker(http: FakeHttp) -> GithubTracker<FakeHttp> {
        GithubTracker::new(http, "o", "r", "tok", &["agent".to_string()])
    }

    #[test]
    fn empty_queries_make_no_request() {
        let http = FakeHttp::new();
        let t = tracker(http);
        assert!(t.by_states(&[]).unwrap().is_empty());
        assert!(t.by_ids(&[]).unwrap().is_empty());
        assert!(t.http.calls().is_empty());
    }

    #[test]
    fn a_full_page_triggers_a_second_request_a_short_page_does_not() {
        let http = FakeHttp::new();
        let full_page: Vec<_> =
            (1..=PER_PAGE as u64).map(|n| gh_issue(n, &["state:open"], "open", true)).collect();
        http.push(ok(serde_json::Value::Array(full_page)));
        http.push(ok(serde_json::json!([gh_issue(
            PER_PAGE as u64 + 1,
            &["state:open"],
            "open",
            true
        )])));

        let t = tracker(http);
        let got = t.by_states(&["open".to_string()]).unwrap();

        assert_eq!(got.len(), PER_PAGE as usize + 1);
        assert_eq!(t.http.calls().len(), 2, "a full page must be followed by a page-2 request");
        assert!(t.http.calls()[0].contains("page=1"));
        assert!(t.http.calls()[1].contains("page=2"));
    }

    #[test]
    fn a_failure_on_the_second_page_fails_the_whole_call_not_a_short_list() {
        let http = FakeHttp::new();
        let full_page: Vec<_> =
            (1..=PER_PAGE as u64).map(|n| gh_issue(n, &["state:open"], "open", true)).collect();
        http.push(ok(serde_json::Value::Array(full_page)));
        http.push(status(500, &[]));

        let t = tracker(http);
        let err = t.by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Status(_)));
    }

    #[test]
    fn rate_limit_signals_classify_distinctly_from_a_bare_403() {
        let http = FakeHttp::new();
        http.push(status(403, &[("retry-after", "60")]));
        let err = tracker(http).by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::RateLimited));

        let http = FakeHttp::new();
        http.push(status(403, &[]));
        let err = tracker(http).by_states(&["open".to_string()]).unwrap_err();
        assert!(
            matches!(err, TrackerError::Status(_)),
            "a bare 403 is a permission error, not a rate limit"
        );
    }

    #[test]
    fn an_expired_token_classifies_as_auth_not_status() {
        let http = FakeHttp::new();
        http.push(status(401, &[]));
        let err = tracker(http).by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Auth(_)));
    }

    /// A timed-out call classed as permanent would be logged as a config fault that "will not
    /// resolve on its own", when the next poll clears it (#176).
    #[test]
    fn a_transport_timeout_is_a_transient_tracker_error() {
        let http = FakeHttp::new();
        http.push(Err(HttpTransportError("timeout: global".into())));
        let err = tracker(http).by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Request(_)), "got {err:?}");
        assert!(err.class().retryable());
    }

    #[test]
    fn pull_requests_are_excluded_from_by_states() {
        let http = FakeHttp::new();
        let mut pr = gh_issue(2, &["state:open"], "open", true);
        pr["pull_request"] = serde_json::json!({});
        http.push(ok(serde_json::json!([gh_issue(1, &["state:open"], "open", true), pr])));

        let got = tracker(http).by_states(&["open".to_string()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].identifier, "#1");
    }

    #[test]
    fn closed_overrides_a_stale_state_label() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!(gh_issue(5, &["state:in-progress"], "closed", true))));
        let got = tracker(http).by_ids(&["o/r#5".to_string()]).unwrap();
        assert_eq!(got[0].state, "closed");
    }

    #[test]
    fn a_404_on_by_ids_is_omitted_not_an_error() {
        let http = FakeHttp::new();
        http.push(status(404, &[]));
        let got = tracker(http).by_ids(&["o/r#9".to_string()]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn a_non_404_failure_on_by_ids_fails_the_whole_call() {
        let http = FakeHttp::new();
        http.push(status(500, &[]));
        let err = tracker(http).by_ids(&["o/r#9".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Status(_)));
    }

    #[test]
    fn an_id_from_a_different_repo_is_omitted_without_a_request() {
        let http = FakeHttp::new();
        let got = tracker(http).by_ids(&["other/repo#1".to_string()]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn dispatchability_follows_assignment() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!([gh_issue(1, &["state:open"], "open", false)])));
        let got = tracker(http).by_states(&["open".to_string()]).unwrap();
        assert!(!got[0].dispatchable, "an unassigned issue is not yet ready for an agent");
    }

    #[test]
    fn the_dispatch_id_is_stable_and_never_the_title() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!([gh_issue(42, &["state:open"], "open", true)])));
        let got = tracker(http).by_states(&["open".to_string()]).unwrap();
        assert_eq!(got[0].id, "o/r#42");
    }

    #[test]
    fn derive_state_prefers_closed_then_label_then_open() {
        assert_eq!(derive_state(true, &["state:in-progress".to_string()]), "closed");
        assert_eq!(derive_state(false, &["state:in-progress".to_string()]), "in-progress");
        assert_eq!(derive_state(false, &[]), "open");
    }

    /// Guards dispatch against label casing (#70).
    #[test]
    fn labels_are_normalized_at_the_adapter_boundary() {
        let http = FakeHttp::new();
        let labels = &["Agent", " agent ", "", "State:In-Progress", "Bug"];
        http.push(ok(serde_json::json!([gh_issue(3, labels, "open", true)])));
        let got = tracker(http).by_states(&["in-progress".to_string()]).unwrap();

        assert_eq!(got[0].labels, vec!["agent", "state:in-progress", "bug"]);
        assert_eq!(
            got[0].state, "in-progress",
            "a capitalised `State:` prefix still names the state"
        );
    }

    /// Guards dispatch against label casing (#70), by the equality `routable` uses.
    #[test]
    fn a_mixed_case_required_label_satisfies_the_normalized_config() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!(gh_issue(4, &["AGENT"], "open", true))));
        let got = tracker(http).by_ids(&["o/r#4".to_string()]).unwrap();

        let required = ["agent".to_string()];
        assert!(required.iter().all(|want| got[0].labels.iter().any(|l| l == want)));
    }

    // ---- the write path (broker tools) -------------------------------------

    #[test]
    fn a_comment_is_a_single_post_to_the_issue_it_was_scoped_to() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!({
            "html_url": "https://github.com/o/r/issues/7#issuecomment-1"
        })));
        let t = tracker(http);

        let out = t.comment("o/r#7", "an update").unwrap();

        let w = t.http.writes();
        assert_eq!(w.len(), 1, "one comment must cost one write");
        assert_eq!(w[0].0, "POST");
        assert_eq!(w[0].1, "https://api.github.com/repos/o/r/issues/7/comments");
        assert_eq!(w[0].2["body"], "an update");
        assert!(out.contains("issuecomment-1"), "the agent gets a reference back: {out}");
    }

    #[test]
    fn an_id_from_another_repository_is_refused_before_any_request() {
        // Defence in depth behind the broker's own scoping: even handed a foreign id directly,
        // this adapter must not construct a URL for a repo it was not configured with.
        let t = tracker(FakeHttp::new());
        assert!(t.comment("other/repo#7", "hi").is_err());
        assert!(t.http.writes().is_empty(), "no request may leave for a foreign repo");
    }

    #[test]
    fn set_state_preserves_labels_it_does_not_own() {
        // GitHub's label endpoint replaces the whole set, and `required_labels` lives in that
        // set — a blind write would strip `agent` and make the issue undispatchable, which is
        // a failure that would only show up as the issue silently never being picked up again.
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!(gh_issue(
            7,
            &["agent", "state:in-progress", "bug"],
            "open",
            true
        ))));
        http.push(ok(serde_json::json!({}))); // PUT labels
        http.push(ok(serde_json::json!({}))); // PATCH state
        let t = tracker(http);

        t.set_state("o/r#7", "in-review").unwrap();

        let w = t.http.writes();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].0, "PUT");
        assert_eq!(w[0].1, "https://api.github.com/repos/o/r/issues/7/labels");
        let mut labels: Vec<String> = w[0].2["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        labels.sort();
        assert_eq!(labels, vec!["agent", "bug", "state:in-review"]);
        assert!(!labels.contains(&"state:in-progress".to_string()), "the old state must go");
    }

    /// Guards the operator's labels against being renamed by a state change (#70), and a
    /// capitalised `State:` label against surviving as a second state.
    #[test]
    fn set_state_writes_labels_back_in_their_original_casing() {
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!(gh_issue(
            8,
            &["Agent", "State:In-Progress", "Bug"],
            "open",
            true
        ))));
        http.push(ok(serde_json::json!({}))); // PUT labels
        http.push(ok(serde_json::json!({}))); // PATCH state
        let t = tracker(http);

        t.set_state("o/r#8", "in-review").unwrap();

        let w = t.http.writes();
        let labels: Vec<&str> =
            w[0].2["labels"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        assert_eq!(labels, vec!["Agent", "Bug", "state:in-review"]);
    }

    #[test]
    fn closing_an_issue_sets_the_flag_rather_than_only_a_label() {
        // `closed` always wins over a `state:*` label when reading, so writing only the label
        // would produce an issue that reads as closed to nobody.
        let http = FakeHttp::new();
        http.push(ok(serde_json::json!(gh_issue(7, &["agent", "state:in-review"], "open", true))));
        http.push(ok(serde_json::json!({})));
        http.push(ok(serde_json::json!({})));
        let t = tracker(http);

        t.set_state("o/r#7", "closed").unwrap();

        let w = t.http.writes();
        let labels = w[0].2["labels"].as_array().unwrap();
        assert!(
            labels.iter().all(|l| l.as_str().unwrap() != "state:closed"),
            "closed is carried by the flag, not by a second disagreeing label"
        );
        assert_eq!(w[1].0, "PATCH");
        assert_eq!(w[1].1, "https://api.github.com/repos/o/r/issues/7");
        assert_eq!(w[1].2["state"], "closed");
    }

    #[test]
    fn a_write_classifies_failures_the_same_way_a_read_does() {
        // The scheduler's retryable/permanent split has to mean the same thing on both paths,
        // or a bad credential on a tool call reads as a transient blip.
        let http = FakeHttp::new();
        http.push(Ok(HttpResponse {
            status: 401,
            headers: HashMap::new(),
            body: b"Bad credentials".to_vec(),
        }));
        let t = tracker(http);

        let e = t.comment("o/r#7", "hi").unwrap_err();
        assert!(matches!(e, TrackerError::Auth(_)), "got {e:?}");
        assert!(!e.class().retryable());
    }
}

/// The seam above `FakeHttp` (#64): which credential a request carries, and which issues are
/// marked for Crew. Kept apart from the tests above so those stay exactly as they were.
#[cfg(test)]
mod app_tests {
    use std::collections::VecDeque;

    use parking_lot::Mutex;

    use super::*;
    use crate::clock::{Clock, FakeClock};
    use crate::credentials::tests::throwaway_key;
    use crate::credentials::{GithubApp, GithubAppFile, REFRESH_MARGIN_MS};

    /// Answers every mint with a fresh token valid for an hour of the fake clock, and every read
    /// with 401 unless it carries the token minted last — which is what GitHub does to an
    /// expired installation token.
    struct Github {
        clock: Arc<FakeClock>,
        issues: Value,
        state: Mutex<(u32, Option<String>, VecDeque<String>)>,
    }

    impl Github {
        fn new(clock: Arc<FakeClock>, issues: Value) -> Arc<Self> {
            Arc::new(Self { clock, issues, state: Mutex::new((0, None, VecDeque::new())) })
        }

        fn mints(&self) -> u32 {
            self.state.lock().0
        }

        /// GitHub revoking the live token before its expiry.
        fn revoke(&self) {
            self.state.lock().1 = None;
        }

        fn queries(&self) -> Vec<String> {
            self.state.lock().2.iter().cloned().collect()
        }
    }

    fn respond(status: u16, body: Value) -> HttpResponse {
        HttpResponse { status, headers: HashMap::new(), body: body.to_string().into_bytes() }
    }

    impl Http for Arc<Github> {
        fn get(
            &self,
            url: &str,
            headers: &[(&str, String)],
        ) -> Result<HttpResponse, HttpTransportError> {
            let mut g = self.state.lock();
            g.2.push_back(url.to_string());
            let sent = headers.iter().find(|(k, _)| *k == "Authorization").map(|(_, v)| v.clone());
            let live = g.1.as_ref().map(|t| format!("Bearer {t}"));
            if sent.is_none() || sent != live {
                return Ok(respond(401, json!({ "message": "Bad credentials" })));
            }
            Ok(respond(200, self.issues.clone()))
        }

        fn send_json(
            &self,
            _method: &str,
            url: &str,
            _headers: &[(&str, String)],
            _body: &[u8],
        ) -> Result<HttpResponse, HttpTransportError> {
            assert!(url.ends_with("/app/installations/7/access_tokens"), "{url}");
            let mut g = self.state.lock();
            g.0 += 1;
            let token = format!("ghs_{}", g.0);
            g.1 = Some(token.clone());
            let expires =
                OffsetDateTime::from_unix_timestamp(self.clock.wall().0 / 1000 + 3600).unwrap();
            let expires_at = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                expires.year(),
                u8::from(expires.month()),
                expires.day(),
                expires.hour(),
                expires.minute(),
                expires.second()
            );
            Ok(respond(201, json!({ "token": token, "expires_at": expires_at })))
        }
    }

    fn issue(number: u64, labels: &[&str], assignees: &[&str]) -> Value {
        json!({
            "number": number,
            "node_id": format!("node-{number}"),
            "title": format!("issue {number}"),
            "state": "open",
            "html_url": format!("https://github.com/o/r/issues/{number}"),
            "labels": labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(),
            "assignees": assignees.iter().map(|a| json!({ "login": a })).collect::<Vec<_>>(),
            "created_at": "2024-01-15T10:30:00Z",
        })
    }

    fn app_tracker(clock: Arc<FakeClock>, gh: &Arc<Github>) -> GithubTracker<Arc<Github>> {
        // Per thread: the tests below run in parallel, and each generates its own key.
        let dir = std::env::temp_dir().join(format!(
            "crew-tracker-app-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let file =
            GithubAppFile { app_id: 42, installation_id: 7, private_key_path: throwaway_key(&dir) };
        let app = GithubApp::new(gh.clone(), &file, clock).unwrap();
        GithubTracker::new(gh.clone(), "o", "r", "unused", &[]).with_credentials(Arc::new(app))
    }

    #[test]
    fn a_daemon_up_past_the_installation_tokens_lifetime_keeps_polling_without_a_401() {
        let clock = Arc::new(FakeClock::new());
        let gh = Github::new(clock.clone(), json!([issue(1, &[], &["someone"])]));
        let t = app_tracker(clock.clone(), &gh);
        let open = ["open".to_string()];

        assert_eq!(t.by_states(&open).unwrap().len(), 1);
        assert_eq!(gh.mints(), 1);

        // Every 30s poll for a day and a half: the token is refreshed, never served stale.
        for _ in 0..(36 * 120) {
            clock.advance_ms(30_000);
            t.by_states(&open).expect("no 401 may reach the scheduler");
        }
        assert!(gh.mints() >= 36, "one mint per hour, not one per process: {}", gh.mints());
        let per_token_ms = (36 * 3_600_000) / i64::from(gh.mints() - 1);
        assert!(per_token_ms >= 3_600_000 - REFRESH_MARGIN_MS - 30_000, "not minted per poll");
    }

    #[test]
    fn a_token_revoked_before_its_expiry_is_re_minted_within_the_same_poll() {
        let clock = Arc::new(FakeClock::new());
        let gh = Github::new(clock.clone(), json!([issue(1, &[], &["someone"])]));
        let t = app_tracker(clock, &gh);
        let open = ["open".to_string()];
        t.by_states(&open).unwrap();

        gh.revoke();
        t.by_states(&open).expect("a revoked token costs one retry, not a failed poll");
        assert_eq!(gh.mints(), 2);
        assert_eq!(gh.queries().len(), 3, "the refused request is repeated exactly once");
    }

    fn by_label(label: Option<&str>) -> DispatchRule {
        DispatchRule { label: label.map(str::to_string), assignee: None }
    }

    #[test]
    fn with_a_dispatch_label_only_the_label_makes_an_issue_dispatchable() {
        let clock = Arc::new(FakeClock::new());
        let issues = json!([
            issue(1, &["agent"], &[]),
            issue(2, &[], &["someone"]),
            issue(3, &["agent"], &["someone"]),
        ]);
        let gh = Github::new(clock.clone(), issues);
        let t = app_tracker(clock, &gh).with_dispatch_rule(by_label(Some("agent")));

        let got = t.by_states(&["open".to_string()]).unwrap();
        let dispatchable: Vec<_> =
            got.iter().map(|i| (i.identifier.as_str(), i.dispatchable)).collect();
        assert_eq!(
            dispatchable,
            vec![("#1", true), ("#2", false), ("#3", true)],
            "an assignee is neither needed nor sufficient; the unlabelled issue is reported, not \
             dropped"
        );
        assert!(gh.queries()[0].contains("labels=agent&"), "the poll filters on the label");
    }

    #[test]
    fn without_a_dispatch_label_any_assignee_is_still_the_signal() {
        let clock = Arc::new(FakeClock::new());
        let gh =
            Github::new(clock.clone(), json!([issue(1, &["agent"], &[]), issue(2, &[], &["a"])]));
        let t = app_tracker(clock, &gh).with_dispatch_rule(by_label(None));

        let got = t.by_states(&["open".to_string()]).unwrap();
        assert_eq!(got.iter().map(|i| i.dispatchable).collect::<Vec<_>>(), vec![false, true]);
    }

    #[test]
    fn an_assignee_narrows_the_label_rule_and_is_never_required_by_it() {
        let rule = DispatchRule { label: Some("agent".into()), assignee: Some("Crew-Bot".into()) };
        let users = |names: &[&str]| -> Vec<GhUser> {
            names.iter().map(|n| GhUser { login: n.to_string() }).collect()
        };
        let agent = ["agent".to_string()];
        assert!(rule.dispatchable(&agent, &users(&["crew-bot"])));
        assert!(!rule.dispatchable(&agent, &users(&["someone"])));
        assert!(!rule.dispatchable(&[], &users(&["crew-bot"])));
        assert!(by_label(Some("agent")).dispatchable(&agent, &[]));
    }
}

/// `FakeHttp` above proves `GithubTracker`'s classification logic is correct given a properly
/// shaped `HttpResponse` — it says nothing about whether `UreqHttp` actually produces one.
/// ureq's default (`http_status_as_error: true`) turns a non-2xx response into an `Err` that
/// discards the body and headers this adapter classifies on, which every `FakeHttp` test is
/// structurally blind to: it compiled, and every other test stayed green, while a live 404
/// came back as `TrackerError::Request` instead of an empty result. These tests talk to a raw
/// `TcpListener` rather than a real server so they still run with no network.
#[cfg(test)]
mod ureq_http_tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// Accepts exactly one connection, writes a fixed raw HTTP/1.1 response, and hands back the
    /// port it bound so a test can point `UreqHttp` at it.
    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // drain the request so the client isn't left hanging
            stream.write_all(response.as_bytes()).unwrap();
        });
        port
    }

    #[test]
    fn a_404_arrives_as_a_response_not_an_error() {
        let port = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}");
        let http = UreqHttp::default();
        let resp = http.get(&format!("http://127.0.0.1:{port}/"), &[]).unwrap();
        assert_eq!(resp.status, 404, "a 404 must reach the caller as a response, not Err(_)");
    }

    #[test]
    fn rate_limit_headers_survive_a_non_2xx_response() {
        let port =
            serve_once("HTTP/1.1 403 Forbidden\r\nRetry-After: 30\r\nContent-Length: 2\r\n\r\n{}");
        let http = UreqHttp::default();
        let resp = http.get(&format!("http://127.0.0.1:{port}/"), &[]).unwrap();
        assert_eq!(resp.status, 403);
        assert_eq!(resp.header("retry-after"), Some("30"));
    }

    #[test]
    fn a_2xx_body_still_parses_normally() {
        let port = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"ok\": true}\n");
        let http = UreqHttp::default();
        let resp = http.get(&format!("http://127.0.0.1:{port}/"), &[]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"{\"ok\": true}\n");
    }

    /// Answers one request with `200` and reports the `Authorization` header it carried, if any.
    fn capture_authorization() -> (u16, std::sync::mpsc::Receiver<Option<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let auth = request
                .lines()
                .find_map(|l| {
                    l.strip_prefix("authorization: ").or(l.strip_prefix("Authorization: "))
                })
                .map(str::to_owned);
            tx.send(auth).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        });
        (port, rx)
    }

    /// A `301` to `location`, closing the connection so the redirected request opens a new one.
    fn redirect_once(location: String) -> u16 {
        let response = format!(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        serve_once(Box::leak(response.into_boxed_str()))
    }

    #[test]
    fn a_same_host_redirect_keeps_the_authorization_header() {
        // #68: a renamed repo 301s every call; losing the token on that hop sends the retry to
        // the anonymous 60/hour limit.
        let (target, auth) = capture_authorization();
        let port = redirect_once(format!("http://127.0.0.1:{target}/repositories/1/issues"));
        let resp = UreqHttp::default()
            .get(
                &format!("http://127.0.0.1:{port}/repos/o/old-name/issues"),
                &[("Authorization", "Bearer t0ken".to_string())],
            )
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(auth.recv().unwrap().as_deref(), Some("Bearer t0ken"));
    }

    #[test]
    fn a_cross_host_redirect_still_drops_the_authorization_header() {
        // `localhost` and `127.0.0.1` reach the same socket but are different hosts to ureq,
        // which is exactly the comparison that keeps the token off a third-party redirect.
        let (target, auth) = capture_authorization();
        let port = redirect_once(format!("http://localhost:{target}/elsewhere"));
        let resp = UreqHttp::default()
            .get(
                &format!("http://127.0.0.1:{port}/repos/o/r/issues"),
                &[("Authorization", "Bearer t0ken".to_string())],
            )
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(auth.recv().unwrap(), None, "the token must not follow a redirect off-host");
    }

    /// Without a bound, a call to a server that accepts and never answers blocks forever, and
    /// the tick with it (#176). The server really accepts, rather than leaving the handshake to
    /// the listen backlog, so the bound this exercises is the wait for a response.
    #[test]
    fn a_server_that_accepts_and_never_answers_fails_the_call_within_its_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut held, _) = listener.accept().unwrap();
            // Reading to EOF holds the connection open, unanswered, until the client hangs up.
            let _ = held.read_to_end(&mut Vec::new());
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let http = UreqHttp::with_timeouts(Duration::from_secs(1), Duration::from_secs(1));
            let _ = tx.send(http.get(&format!("http://127.0.0.1:{port}/repos/o/r/issues"), &[]));
        });

        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the call is still blocked: no timeout bounded it");
        assert!(got.is_err(), "a server that never answers returned {got:?}");
    }
}
