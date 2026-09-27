//! `Tracker` and `TrackerWrites` against Jira Cloud (#99).
//!
//! REST API v3 over Basic auth: the API token is the operator's own identity, so every write
//! this adapter makes is authored by them, the same way a GitHub PAT authors as its owner. The
//! issue key (`PROJ-123`) is the dispatch id — it changes only when an issue leaves its project,
//! and the adapter then omits it, which the scheduler already treats as "not visible" rather
//! than as an error.
//!
//! `tracker.dispatch_label` is the whole dispatch signal, as on GitHub after #64: a teammate
//! taking a ticket must not hand it to an agent, and a Jira service account cannot be an assignee
//! at all (#99). `assigned_to_me` narrows further and is checked per issue, in both `by_states`
//! and `by_ids`, because `refresh_running` stops a run the moment `by_ids` reports it as no
//! longer dispatchable — narrowing only the poll's own query would leave a reassigned issue
//! running past that point.
//!
//! `set_state` applies whichever transition's target status matches the requested state, without
//! regard to case; there is no operator-configured state-to-transition map, so a workflow with no
//! path to the requested status fails naming every status a transition can reach.
//!
//! Jira Cloud's rate limiting is cost-based with no budget header the way GitHub's is, so there
//! is no equivalent of `github.rs`'s per-hour arithmetic to size `interval_ms` against — it stays
//! an empirical knob, tightened only if a 429 (classified
//! [`crate::model::ErrorClass::RateLimited`]) starts showing up in the log.
//!
//! A status name the site does not know makes Jira reject the poll's JQL with a 400 naming it,
//! which fails every poll until the config is fixed; this is deliberate, a loud config error
//! rather than a silently empty poll.

mod adf;
mod issue;
mod jql;

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tracker, TrackerError};
use crate::broker::TrackerWrites;
use crate::credentials::{CredentialError, Credentials};
use crate::model::Issue;
use crate::tracker::github::{Http, HttpResponse, HttpTransportError};
use issue::{FIELDS, JiIssue, MapContext};

/// `search_all`'s ceiling on pages of `search_page`'s own 100-result page size. Jira Cloud
/// documents no bound on `nextPageToken`'s cardinality, so a search that never sets `isLast`
/// and never repeats a token would otherwise page forever on the tick thread.
const MAX_SEARCH_PAGES: usize = 100;

/// A construction with no credential set. `new` alone is never enough for a live call — every
/// site builds one with `with_credentials` right after, the way `tracker.jira.credentials` or
/// `JIRA_EMAIL`/`JIRA_API_TOKEN` is always resolved before `main.rs` hands one over.
struct NoCredentials;

impl Credentials for NoCredentials {
    fn token(&self) -> Result<String, CredentialError> {
        Err(CredentialError::Permanent("jira credentials not configured".into()))
    }
}

#[derive(Debug, Default, Deserialize)]
struct SearchPage {
    #[serde(default)]
    issues: Vec<JiIssue>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(default, rename = "isLast")]
    is_last: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct Transitions {
    transitions: Vec<Transition>,
}

#[derive(Debug, Deserialize)]
struct Transition {
    id: String,
    to: NamedStatus,
}

#[derive(Debug, Deserialize)]
struct NamedStatus {
    name: String,
}

#[derive(Debug, Deserialize)]
struct CurrentStatus {
    fields: StatusField,
}

#[derive(Debug, Deserialize)]
struct StatusField {
    status: NamedStatus,
}

pub struct JiraTracker<H: Http> {
    http: H,
    base_url: String,
    project: String,
    dispatch_label: String,
    assigned_to_me: bool,
    creds: Arc<dyn Credentials>,
    /// The token owner's `accountId`, resolved lazily the first time `assigned_to_me` needs it
    /// and cached for the tracker's life — nothing about which account a token authenticates as
    /// changes without a new token, and a new token means a new `JiraTracker`.
    me: OnceLock<String>,
}

impl<H: Http> JiraTracker<H> {
    pub fn new(http: H, base_url: &str, project: &str, dispatch_label: &str) -> Self {
        Self {
            http,
            base_url: base_url.to_string(),
            project: project.to_string(),
            dispatch_label: dispatch_label.to_string(),
            assigned_to_me: false,
            creds: Arc::new(NoCredentials),
            me: OnceLock::new(),
        }
    }

    pub fn with_credentials(mut self, creds: Arc<dyn Credentials>) -> Self {
        self.creds = creds;
        self
    }

    pub fn with_assigned_to_me(mut self, assigned_to_me: bool) -> Self {
        self.assigned_to_me = assigned_to_me;
        self
    }

    fn headers(&self) -> Result<Vec<(&'static str, String)>, TrackerError> {
        Ok(vec![
            ("Authorization", format!("Basic {}", self.creds.token()?)),
            ("Accept", "application/json".to_string()),
            ("User-Agent", "crewd".to_string()),
        ])
    }

    /// Sends once, and once more with a fresh token if the first was refused with a 401 — see
    /// `GithubTracker::authed` for why a refused request is always safe to repeat.
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

    fn request(&self, url: &str) -> Result<HttpResponse, TrackerError> {
        classify(self.authed(|h| self.http.get(url, h))?)
    }

    fn write(&self, method: &str, url: &str, body: &Value) -> Result<HttpResponse, TrackerError> {
        let payload =
            serde_json::to_vec(body).map_err(|e| TrackerError::Response(e.to_string()))?;
        classify(self.authed(|h| self.http.send_json(method, url, h, &payload))?)
    }

    /// The token owner's `accountId`; see the `me` field's own doc for the caching contract.
    fn me(&self) -> Result<String, TrackerError> {
        if let Some(id) = self.me.get() {
            return Ok(id.clone());
        }
        let url = format!("{}/rest/api/3/myself", self.base_url);
        let resp = self.request(&url)?;
        #[derive(Deserialize)]
        struct Myself {
            #[serde(rename = "accountId")]
            account_id: String,
        }
        let parsed: Myself = serde_json::from_slice(&resp.body)
            .map_err(|e| TrackerError::Response(e.to_string()))?;
        // Losing a `set` race just means another thread's identical answer wins — the token
        // authenticates as one account either way.
        let _ = self.me.set(parsed.account_id.clone());
        Ok(self.me.get().cloned().unwrap_or(parsed.account_id))
    }

    /// Well-formed (`^[A-Za-z][A-Za-z0-9_]*-[0-9]+$`) and in this adapter's own project, checked
    /// by hand rather than with a regex crate — one key shape, matched at most once per id.
    fn owns(&self, id: &str) -> bool {
        let Some((prefix, rest)) = id.split_once('-') else { return false };
        let prefix_ok = prefix.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let rest_ok = !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
        prefix_ok && rest_ok && prefix.eq_ignore_ascii_case(&self.project)
    }

    /// A moved issue keeps its old key resolvable under whatever project it landed in
    /// (`PROJ-12` -> `SECRET-3`), so the key-shape check in `owns` alone would let a write
    /// follow a stale key into another project; every write confirms the project live, right
    /// before it, with its own GET rather than trusting a cached or filtered issue. A
    /// malformed or foreign-prefixed id fails `owns` locally, before any request; a 404 on the
    /// live lookup is refused the same as a project mismatch.
    fn validate(&self, issue_id: &str) -> Result<(), TrackerError> {
        if !self.owns(issue_id) {
            return Err(TrackerError::Status(format!(
                "{issue_id} is not an issue in project {}",
                self.project
            )));
        }
        let url = format!("{}/rest/api/3/issue/{issue_id}?fields=project", self.base_url);
        let resp = self.request(&url)?;
        #[derive(Deserialize)]
        struct ProjectOnly {
            fields: ProjectField,
        }
        #[derive(Deserialize)]
        struct ProjectField {
            #[serde(default)]
            project: Option<issue::JiProject>,
        }
        let parsed: ProjectOnly = serde_json::from_slice(&resp.body)
            .map_err(|e| TrackerError::Response(e.to_string()))?;
        let project_key = parsed.fields.project.map(|p| p.key).unwrap_or_default();
        if project_key.eq_ignore_ascii_case(&self.project) {
            Ok(())
        } else {
            Err(TrackerError::Status(format!(
                "{issue_id} now belongs to project {project_key}, not {}",
                self.project
            )))
        }
    }

    /// `None` for a clean 404 or an issue now outside this project — both mean "not visible",
    /// the contract `by_ids` documents. Any other failure propagates.
    fn fetch_issue(&self, id: &str) -> Result<Option<JiIssue>, TrackerError> {
        if !self.owns(id) {
            return Ok(None);
        }
        let url = format!("{}/rest/api/3/issue/{id}?fields={}", self.base_url, FIELDS.join(","));
        let resp = self.authed(|h| self.http.get(&url, h))?;
        if resp.status == 404 {
            return Ok(None);
        }
        let resp = classify(resp)?;
        let raw: JiIssue = serde_json::from_slice(&resp.body)
            .map_err(|e| TrackerError::Response(e.to_string()))?;
        let project_key = raw.fields.project.as_ref().map(|p| p.key.as_str()).unwrap_or("");
        if !project_key.eq_ignore_ascii_case(&self.project) {
            return Ok(None); // the issue moved to another project
        }
        Ok(Some(raw))
    }

    /// The GET form of the search, not the POST one: `Http::send_json` is the seam's write half,
    /// and a poll that went through it would make the read kernel indistinguishable from a
    /// mutation at the seam.
    fn search_page(&self, jql: &str, token: Option<&str>) -> Result<SearchPage, TrackerError> {
        let mut url = format!(
            "{}/rest/api/3/search/jql?jql={}&fields={}&maxResults=100",
            self.base_url,
            query_escape(jql),
            FIELDS.join(",")
        );
        if let Some(t) = token {
            url.push_str(&format!("&nextPageToken={}", query_escape(t)));
        }
        let resp = self.request(&url)?;
        serde_json::from_slice(&resp.body).map_err(|e| TrackerError::Response(e.to_string()))
    }

    /// Pages while a token is present and `isLast` is not `true`. A page failing fails the whole
    /// call rather than returning what was gathered so far. Every token seen is remembered, not
    /// just the previous one, so a cycle (`A, B, A`) fails the same as an immediate repeat; a
    /// search that advances to a new token every time without ever setting `isLast` fails once
    /// it passes [`MAX_SEARCH_PAGES`] rather than paging forever on the tick thread.
    fn search_all(&self, jql: &str) -> Result<Vec<JiIssue>, TrackerError> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        let mut seen_tokens: HashSet<String> = HashSet::new();
        for _ in 0..MAX_SEARCH_PAGES {
            let page = self.search_page(jql, token.as_deref())?;
            all.extend(page.issues);
            if page.is_last.unwrap_or(false) {
                return Ok(all);
            }
            let Some(next) = page.next_page_token else { return Ok(all) };
            if !seen_tokens.insert(next.clone()) {
                return Err(TrackerError::Response(format!(
                    "page token {next} was seen twice; refusing to loop forever"
                )));
            }
            token = Some(next);
        }
        Err(TrackerError::Response(format!(
            "search did not finish within {MAX_SEARCH_PAGES} pages"
        )))
    }

    /// `filter_states`, when non-empty, narrows the mapped output to those state keys — the same
    /// defensive filter `GithubTracker::by_states` applies after its own labelled query. `me` is
    /// resolved at most once per call, and only when there is at least one issue to judge.
    fn map_issues(
        &self,
        raw: Vec<JiIssue>,
        filter_states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let me = if self.assigned_to_me && !raw.is_empty() { Some(self.me()?) } else { None };
        let want: Option<HashSet<&str>> =
            (!filter_states.is_empty()).then(|| filter_states.iter().map(String::as_str).collect());
        let ctx = MapContext {
            base_url: &self.base_url,
            project: &self.project,
            dispatch_label: &self.dispatch_label,
            me: me.as_deref(),
            assigned_to_me: self.assigned_to_me,
        };
        Ok(raw
            .into_iter()
            .map(|r| issue::to_issue(&ctx, r))
            .filter(|i| want.as_ref().is_none_or(|w| w.contains(i.state_key().as_str())))
            .collect())
    }

    /// No transition reaches `want`: succeeds if the issue is already there, else fails naming
    /// the current status and every status a transition can reach.
    fn confirm_already_in_state(
        &self,
        issue_id: &str,
        want: &str,
        transitions: &[Transition],
    ) -> Result<String, TrackerError> {
        let status_url = format!("{}/rest/api/3/issue/{issue_id}?fields=status", self.base_url);
        let resp = self.request(&status_url)?;
        let current: CurrentStatus = serde_json::from_slice(&resp.body)
            .map_err(|e| TrackerError::Response(e.to_string()))?;
        let current_name = current.fields.status.name;
        if current_name.trim().to_lowercase() == want.trim().to_lowercase() {
            return Ok(format!("{issue_id} is already {current_name}"));
        }
        let targets: Vec<&str> = transitions.iter().map(|t| t.to.name.as_str()).collect();
        Err(TrackerError::Status(format!(
            "{issue_id} is {current_name}; no transition reaches {want:?}; reachable: {}",
            targets.join(", ")
        )))
    }
}

impl<H: Http> Tracker for JiraTracker<H> {
    fn by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(vec![]);
        }
        let jql = jql::poll_query(&self.project, states, &self.dispatch_label, self.assigned_to_me);
        let raw = self.search_all(&jql)?;
        self.map_issues(raw, states)
    }

    fn by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut raw = Vec::new();
        for id in ids {
            if let Some(issue) = self.fetch_issue(id)? {
                raw.push(issue);
            }
        }
        self.map_issues(raw, &[])
    }
}

/// Ticket mutations, for the broker only — see the module doc for `set_state`'s transition rule.
impl<H: Http> TrackerWrites for JiraTracker<H> {
    fn comment(&self, issue_id: &str, body: &str) -> Result<String, TrackerError> {
        self.validate(issue_id)?;
        let url = format!("{}/rest/api/3/issue/{issue_id}/comment", self.base_url);
        let resp = self.write("POST", &url, &json!({ "body": adf::encode(body) }))?;
        let id = serde_json::from_slice::<Value>(&resp.body)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_string));
        Ok(match id {
            Some(id) => format!("{}/browse/{issue_id}?focusedCommentId={id}", self.base_url),
            None => format!("commented on {issue_id}"),
        })
    }

    fn set_state(&self, issue_id: &str, state: &str) -> Result<String, TrackerError> {
        self.validate(issue_id)?;
        let transitions_url = format!("{}/rest/api/3/issue/{issue_id}/transitions", self.base_url);
        let resp = self.request(&transitions_url)?;
        let parsed: Transitions = serde_json::from_slice(&resp.body)
            .map_err(|e| TrackerError::Response(e.to_string()))?;
        let want = state.trim();
        let want_lower = want.to_lowercase();
        // `eq_ignore_ascii_case` never matches a non-ASCII status (e.g. "Überprüfung") against
        // the broker's Unicode-lowercased argument, so both sides fold through `to_lowercase`.
        if let Some(t) =
            parsed.transitions.iter().find(|t| t.to.name.trim().to_lowercase() == want_lower)
        {
            self.write("POST", &transitions_url, &json!({ "transition": { "id": t.id } }))?;
            return Ok(format!("{issue_id} is now {}", t.to.name));
        }
        self.confirm_already_in_state(issue_id, want, &parsed.transitions)
    }

    fn link_pr(&self, issue_id: &str, url: &str) -> Result<String, TrackerError> {
        self.validate(issue_id)?;
        let remote_url = format!("{}/rest/api/3/issue/{issue_id}/remotelink", self.base_url);
        self.write(
            "POST",
            &remote_url,
            &json!({ "globalId": url, "object": { "url": url, "title": pr_title(url) } }),
        )?;
        Ok(format!("linked {url} to {issue_id}"))
    }
}

/// Maps a response onto [`TrackerError`], shared by the read and write paths — see
/// `github.rs`'s own `classify` for why that sharing matters.
fn classify(resp: HttpResponse) -> Result<HttpResponse, TrackerError> {
    if (200..300).contains(&resp.status) {
        return Ok(resp);
    }
    if resp.status == 401 {
        return Err(TrackerError::Auth(body_snippet(&resp)));
    }
    let rate_limited =
        resp.status == 429 || (resp.status == 503 && resp.header("retry-after").is_some());
    if rate_limited {
        return Err(TrackerError::RateLimited);
    }
    Err(TrackerError::Status(format!("{}: {}", resp.status, body_snippet(&resp))))
}

/// Prefers Jira's own `errorMessages`/`errors` shape over the raw body, so a validation failure
/// reads as the message Jira wrote rather than as JSON punctuation.
fn body_snippet(resp: &HttpResponse) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(&resp.body) {
        let mut parts: Vec<String> = Vec::new();
        if let Some(msgs) = v.get("errorMessages").and_then(Value::as_array) {
            parts.extend(msgs.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if let Some(errors) = v.get("errors").and_then(Value::as_object) {
            parts.extend(errors.iter().map(|(k, v)| format!("{k}: {v}")));
        }
        if !parts.is_empty() {
            return parts.join("; ").chars().take(200).collect();
        }
    }
    String::from_utf8_lossy(&resp.body).chars().take(200).collect()
}

/// Percent-encodes a query-string value. A JQL query carries spaces, quotes, `=` and `,`, and a
/// page token may carry `+`, `/` or `=`; any of them left raw changes what Jira reads.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `Pull request owner/repo#N` for a GitHub pull request URL, else the generic fallback. The
/// title is display only — `globalId` (the URL itself) is what makes a repeat call idempotent.
fn pr_title(url: &str) -> String {
    parse_github_pr(url)
        .map(|(owner, repo, n)| format!("Pull request {owner}/{repo}#{n}"))
        .unwrap_or_else(|| "Pull request".to_string())
}

fn parse_github_pr(url: &str) -> Option<(String, String, String)> {
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.splitn(4, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next()? != "pull" {
        return None;
    }
    let n: String = parts.next()?.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!n.is_empty()).then_some((owner.to_string(), repo.to_string(), n))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::credentials::StaticToken;
    use crate::tracker::test_http::{FakeHttp, ok, status, status_body};

    fn tracker(http: FakeHttp) -> JiraTracker<FakeHttp> {
        JiraTracker::new(http, "https://x.atlassian.net", "PROJ", "crewd")
            .with_credentials(Arc::new(StaticToken::new("dGVzdA==")))
    }

    fn ji(key: &str, labels: &[&str], status_name: &str, project: &str) -> Value {
        json!({
            "key": key,
            "fields": {
                "summary": format!("issue {key}"),
                "status": {"name": status_name},
                "labels": labels,
                "project": {"key": project},
            }
        })
    }

    fn assigned_issue(key: &str, account_id: &str) -> Value {
        json!({
            "key": key,
            "fields": {
                "summary": "s",
                "status": {"name": "Open"},
                "labels": ["crewd"],
                "project": {"key": "PROJ"},
                "assignee": {"accountId": account_id},
            }
        })
    }

    /// Every write now confirms the project live before it does anything else (#99); this
    /// is that check's response, scripted first in every write test below.
    fn project_ok() -> Result<HttpResponse, HttpTransportError> {
        ok(json!({"fields": {"project": {"key": "PROJ"}}}))
    }

    #[test]
    fn empty_queries_make_no_jira_request() {
        let http = FakeHttp::new();
        let t = tracker(http);
        assert!(t.by_states(&[]).unwrap().is_empty());
        assert!(t.by_ids(&[]).unwrap().is_empty());
        assert!(t.http.calls().is_empty());
        assert!(t.http.writes().is_empty());
    }

    #[test]
    fn a_full_page_is_followed_by_a_request_carrying_its_next_page_token() {
        let http = FakeHttp::new();
        http.push(ok(json!({
            "issues": [ji("PROJ-1", &["crewd"], "Open", "PROJ")],
            "nextPageToken": "tok-1",
            "isLast": false,
        })));
        http.push(ok(json!({
            "issues": [ji("PROJ-2", &["crewd"], "Open", "PROJ")],
            "isLast": true,
        })));
        let t = tracker(http);
        let got = t.by_states(&["open".to_string()]).unwrap();
        assert_eq!(got.len(), 2);
        let calls = t.http.calls();
        assert_eq!(calls.len(), 2, "a full page must be followed by a page-2 request");
        assert!(!calls[0].contains("nextPageToken"), "{}", calls[0]);
        assert!(calls[1].ends_with("&nextPageToken=tok-1"), "{}", calls[1]);
        assert!(t.http.writes().is_empty(), "a poll never goes through the write half");
    }

    #[test]
    fn the_poll_query_is_percent_encoded_into_the_search_url() {
        let http = FakeHttp::new();
        http.push(ok(json!({ "issues": [], "isLast": true })));
        let t = tracker(http);
        t.by_states(&["in development".to_string()]).unwrap();
        insta::assert_snapshot!(t.http.calls()[0]);
    }

    #[test]
    fn a_later_page_failure_fails_the_whole_poll_rather_than_returning_a_short_list() {
        let http = FakeHttp::new();
        http.push(ok(json!({
            "issues": [ji("PROJ-1", &["crewd"], "Open", "PROJ")],
            "nextPageToken": "tok-1",
            "isLast": false,
        })));
        http.push(status(500, &[]));
        let t = tracker(http);
        let err = t.by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Status(_)));
    }

    #[test]
    fn a_repeated_page_token_fails_the_call_rather_than_looping_forever() {
        let http = FakeHttp::new();
        http.push(ok(json!({
            "issues": [ji("PROJ-1", &["crewd"], "Open", "PROJ")],
            "nextPageToken": "tok-1",
            "isLast": false,
        })));
        http.push(ok(json!({ "issues": [], "nextPageToken": "tok-1", "isLast": false })));
        let t = tracker(http);
        let err = t.by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Response(_)));
    }

    #[test]
    fn a_page_token_cycle_fails_the_call_rather_than_looping_forever() {
        let http = FakeHttp::new();
        http.push(ok(json!({ "issues": [], "nextPageToken": "A", "isLast": false })));
        http.push(ok(json!({ "issues": [], "nextPageToken": "B", "isLast": false })));
        http.push(ok(json!({ "issues": [], "nextPageToken": "A", "isLast": false })));
        let t = tracker(http);
        let err = t.by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Response(_)), "A, B, A is a cycle, not a repeat");
    }

    #[test]
    fn a_search_that_never_ends_fails_after_the_page_cap() {
        let http = FakeHttp::new();
        for i in 0..MAX_SEARCH_PAGES {
            http.push(ok(
                json!({ "issues": [], "nextPageToken": format!("tok-{i}"), "isLast": false }),
            ));
        }
        let t = tracker(http);
        let err = t.by_states(&["open".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Response(_)));
        assert_eq!(t.http.calls().len(), MAX_SEARCH_PAGES, "stops at the cap, not past it");
    }

    #[test]
    fn a_jira_issue_missing_from_by_ids_is_omitted_but_a_server_error_fails_the_call() {
        let http = FakeHttp::new();
        http.push(status(404, &[]));
        let got = tracker(http).by_ids(&["PROJ-1".to_string()]).unwrap();
        assert!(got.is_empty());

        let http = FakeHttp::new();
        http.push(status(500, &[]));
        let err = tracker(http).by_ids(&["PROJ-1".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Status(_)));
    }

    #[test]
    fn an_issue_moved_out_of_the_project_is_omitted() {
        let http = FakeHttp::new();
        http.push(ok(ji("PROJ-1", &["crewd"], "Open", "OTHER")));
        let got = tracker(http).by_ids(&["PROJ-1".to_string()]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn an_id_from_another_project_is_omitted_without_a_request() {
        let http = FakeHttp::new();
        let t = tracker(http);
        let got = t.by_ids(&["OTHER-1".to_string()]).unwrap();
        assert!(got.is_empty());
        assert!(t.http.calls().is_empty());
    }

    /// #99: `refresh_running` stops a run once `by_ids` says not dispatchable, so a token
    /// owner's `/myself` must be cheap enough to fetch every reconciliation — cached, not
    /// re-fetched, across separate calls.
    #[test]
    fn a_reassigned_issue_stops_being_dispatchable_when_assigned_to_me_is_set() {
        let http = FakeHttp::new();
        http.push(ok(assigned_issue("PROJ-1", "me-1")));
        http.push(ok(json!({"accountId": "me-1"})));
        http.push(ok(assigned_issue("PROJ-1", "someone-else")));
        let t = tracker(http).with_assigned_to_me(true);

        let first = t.by_ids(&["PROJ-1".to_string()]).unwrap();
        assert!(first[0].dispatchable);

        let second = t.by_ids(&["PROJ-1".to_string()]).unwrap();
        assert!(!second[0].dispatchable, "reassigned away from the token owner");

        let myself_calls = t.http.calls().iter().filter(|u| u.ends_with("/myself")).count();
        assert_eq!(myself_calls, 1, "/myself is cached across calls");
    }

    #[test]
    fn a_401_is_permanent_and_a_429_is_rate_limited() {
        let http = FakeHttp::new();
        http.push(status(401, &[]));
        let err = tracker(http).by_ids(&["PROJ-1".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::Auth(_)));
        assert!(!err.class().retryable());

        let http = FakeHttp::new();
        http.push(status(429, &[]));
        let err = tracker(http).by_ids(&["PROJ-1".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::RateLimited));

        let http = FakeHttp::new();
        http.push(status(503, &[("retry-after", "10")]));
        let err = tracker(http).by_ids(&["PROJ-1".to_string()]).unwrap_err();
        assert!(matches!(err, TrackerError::RateLimited), "503 with retry-after too");

        let http = FakeHttp::new();
        http.push(status_body(400, json!({"errorMessages": ["bad jql"]})));
        let err = tracker(http).by_ids(&["PROJ-1".to_string()]).unwrap_err();
        match err {
            TrackerError::Status(m) => insta::assert_snapshot!(m),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn set_state_applies_the_transition_whose_target_matches_without_regard_to_case() {
        let http = FakeHttp::new();
        http.push(project_ok());
        http.push(ok(json!({
            "transitions": [
                {"id": "11", "to": {"name": "In Progress"}},
                {"id": "31", "to": {"name": "DONE"}},
            ]
        })));
        http.push(ok(json!({})));
        let t = tracker(http);
        let out = t.set_state("PROJ-1", "done").unwrap();
        insta::assert_snapshot!(out);
        let writes = t.http.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].2["transition"]["id"], "31");
    }

    #[test]
    fn set_state_matches_a_non_ascii_status_name_without_regard_to_case() {
        let http = FakeHttp::new();
        http.push(project_ok());
        http.push(ok(json!({ "transitions": [{"id": "11", "to": {"name": "Überprüfung"}}] })));
        http.push(ok(json!({})));
        let t = tracker(http);
        // The broker hands over a Unicode-lowercased state (#99): `eq_ignore_ascii_case`
        // never folds `Ü` to `ü`, so the match must go through `to_lowercase` on both sides.
        let out = t.set_state("PROJ-1", "überprüfung").unwrap();
        assert_eq!(t.http.writes().len(), 1, "the transition was posted, not just confirmed");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn set_state_names_the_reachable_targets_when_none_matches() {
        let http = FakeHttp::new();
        http.push(project_ok());
        http.push(ok(json!({ "transitions": [{"id": "11", "to": {"name": "In Progress"}}] })));
        http.push(ok(json!({"fields": {"status": {"name": "Open"}}})));
        let t = tracker(http);
        let err = t.set_state("PROJ-1", "done").unwrap_err();
        match err {
            TrackerError::Status(m) => insta::assert_snapshot!(m),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn set_state_to_the_current_status_succeeds_without_a_transition() {
        let http = FakeHttp::new();
        http.push(project_ok());
        http.push(ok(json!({"transitions": []})));
        http.push(ok(json!({"fields": {"status": {"name": "Done"}}})));
        let t = tracker(http);
        let out = t.set_state("PROJ-1", "done").unwrap();
        insta::assert_snapshot!(out);
        assert!(t.http.writes().is_empty(), "no transition was posted");
    }

    #[test]
    fn a_write_to_an_issue_outside_the_project_is_refused_without_a_request() {
        let t = tracker(FakeHttp::new());
        assert!(t.comment("OTHER-1", "hi").is_err());
        assert!(t.http.writes().is_empty());
        assert!(t.http.calls().is_empty());
    }

    #[test]
    fn a_write_to_an_issue_moved_out_of_the_project_is_refused() {
        let http = FakeHttp::new();
        http.push(ok(json!({"fields": {"project": {"key": "SECRET"}}})));
        let t = tracker(http);
        let err = t.comment("PROJ-1", "hi").unwrap_err();
        assert!(matches!(err, TrackerError::Status(_)));
        assert_eq!(t.http.calls().len(), 1, "only the project check, no comment request follows");
        assert!(t.http.writes().is_empty());

        let http = FakeHttp::new();
        http.push(status(404, &[]));
        let t = tracker(http);
        let err = t.set_state("PROJ-1", "done").unwrap_err();
        assert!(
            matches!(err, TrackerError::Status(_)),
            "a 404 on the project check is refused too"
        );
        assert!(t.http.writes().is_empty());
    }

    #[test]
    fn link_pr_posts_an_idempotent_remote_link() {
        let http = FakeHttp::new();
        http.push(project_ok());
        http.push(ok(json!({})));
        let t = tracker(http);
        let url = "https://github.com/o/r/pull/9";
        t.link_pr("PROJ-1", url).unwrap();
        let writes = t.http.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].2["globalId"], url);
        assert_eq!(writes[0].2["object"]["title"], "Pull request o/r#9");
    }

    #[test]
    fn a_comment_is_posted_as_adf() {
        let http = FakeHttp::new();
        http.push(project_ok());
        http.push(ok(json!({"id": "10001"})));
        let t = tracker(http);
        let out = t.comment("PROJ-1", "hello").unwrap();
        insta::assert_snapshot!(out);
        let writes = t.http.writes();
        assert_eq!(writes[0].2["body"]["type"], "doc");
    }
}
