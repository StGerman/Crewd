//! Scripted in-memory tracker.
//!
//! Tests drive it directly; `--tracker fake` uses [`FakeTracker::demo`] to give the TUI
//! something to render. It can also be told to fail or to hide issues, so the scheduler's
//! error and grace paths are exercisable without a network.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};

use super::{LabelOutcome, RepoLabels, Tracker, TrackerError};
use crate::model::Issue;

pub struct FakeTracker {
    inner: Mutex<Inner>,
}

struct Inner {
    issues: HashMap<String, Issue>,
    /// Ids omitted from `by_ids` results, simulating provider invisibility.
    hidden: HashSet<String>,
    fail_states: Option<TrackerError>,
    fail_ids: Option<TrackerError>,
    /// The repository's labels, which [`RepoLabels`] creates; never read by the poll.
    repo_labels: Vec<String>,
    pub calls_by_states: u32,
    pub calls_by_ids: u32,
}

impl FakeTracker {
    pub fn new(issues: Vec<Issue>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                issues: issues.into_iter().map(|i| (i.id.clone(), i)).collect(),
                hidden: HashSet::new(),
                fail_states: None,
                fail_ids: None,
                repo_labels: Vec::new(),
                calls_by_states: 0,
                calls_by_ids: 0,
            }),
        }
    }

    /// A spread of issues that exercises every visible outcome in the dashboard.
    pub fn demo() -> Self {
        let mk = |n: u32, title: &str, state: &str, prio: Option<i32>, disp: bool| Issue {
            id: format!("iss-{n:03}"),
            identifier: format!("MT-{}", 600 + n),
            title: title.to_string(),
            body: Some(format!("Demo body text for {title}.")),
            state: state.to_string(),
            priority: prio,
            url: Some(format!("https://tracker.example/issues/MT-{}", 600 + n)),
            labels: vec!["agent".into()],
            dispatchable: disp,
            created_at: Some(1_770_000_000_000 + (n as i64) * 60_000),
            native_ref: None,
            blocked_by: vec![],
        };
        Self::new(vec![
            mk(1, "Flaky retry on token refresh", "In Progress", Some(1), true),
            mk(2, "Add pagination to search results", "In Progress", Some(2), true),
            mk(3, "Migrate settings to new schema", "In Progress", Some(2), true),
            mk(4, "Document the webhook contract", "In Progress", Some(3), true),
            mk(5, "Drop the legacy export path", "In Progress", Some(4), true),
            mk(6, "Investigate slow cold start", "In Progress", None, true),
            // Not dispatchable: adapter-level routing says no. Should never be picked up.
            mk(7, "Unassigned triage item", "In Progress", Some(1), false),
            // Terminal: should be reconciled away rather than dispatched.
            mk(8, "Already shipped", "Done", Some(1), true),
        ])
    }

    pub fn set_state(&self, id: &str, state: &str) {
        if let Some(i) = self.inner.lock().issues.get_mut(id) {
            i.state = state.to_string();
        }
    }

    pub fn set_body(&self, id: &str, body: Option<&str>) {
        if let Some(i) = self.inner.lock().issues.get_mut(id) {
            i.body = body.map(str::to_string);
        }
    }

    pub fn set_dispatchable(&self, id: &str, v: bool) {
        if let Some(i) = self.inner.lock().issues.get_mut(id) {
            i.dispatchable = v;
        }
    }

    /// Hide from `by_ids` without deleting — the eventual-consistency blip the grace count exists for.
    pub fn hide(&self, id: &str) {
        self.inner.lock().hidden.insert(id.to_string());
    }

    pub fn unhide(&self, id: &str) {
        self.inner.lock().hidden.remove(id);
    }

    pub fn fail_by_states(&self, e: Option<TrackerError>) {
        self.inner.lock().fail_states = e;
    }

    pub fn fail_by_ids(&self, e: Option<TrackerError>) {
        self.inner.lock().fail_ids = e;
    }

    /// The repository's labels, in the order they were created.
    pub fn repo_labels(&self) -> Vec<String> {
        self.inner.lock().repo_labels.clone()
    }

    pub fn call_counts(&self) -> (u32, u32) {
        let g = self.inner.lock();
        (g.calls_by_states, g.calls_by_ids)
    }
}

impl Tracker for FakeTracker {
    fn by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(vec![]); // no provider request for an empty query
        }
        let mut g = self.inner.lock();
        g.calls_by_states += 1;
        if let Some(e) = &g.fail_states {
            return Err(e.clone());
        }
        let want: HashSet<&str> = states.iter().map(|s| s.as_str()).collect();
        Ok(g.issues
            .values()
            // Invisibility is global: a provider that stops returning an issue by id stops
            // returning it by state too.
            .filter(|i| !g.hidden.contains(&i.id))
            .filter(|i| want.contains(i.state.trim().to_lowercase().as_str()))
            .cloned()
            .collect())
    }

    fn by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut g = self.inner.lock();
        g.calls_by_ids += 1;
        if let Some(e) = &g.fail_ids {
            return Err(e.clone());
        }
        Ok(ids
            .iter()
            .filter(|id| !g.hidden.contains(*id))
            .filter_map(|id| g.issues.get(id).cloned())
            .collect())
    }
}

/// GitHub's label names compare without case, so `Agent` is the existing `agent`.
impl RepoLabels for FakeTracker {
    fn ensure_label(&self, name: &str) -> Result<LabelOutcome, TrackerError> {
        let mut g = self.inner.lock();
        if g.repo_labels.iter().any(|l| l.eq_ignore_ascii_case(name)) {
            return Ok(LabelOutcome::Kept);
        }
        g.repo_labels.push(name.to_string());
        Ok(LabelOutcome::Created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queries_short_circuit_without_a_provider_call() {
        let t = FakeTracker::demo();
        assert!(t.by_states(&[]).unwrap().is_empty());
        assert!(t.by_ids(&[]).unwrap().is_empty());
        assert_eq!(t.call_counts(), (0, 0), "neither should have hit the provider");
    }

    #[test]
    fn state_matching_ignores_case_and_surrounding_whitespace() {
        let t = FakeTracker::demo();
        let got = t.by_states(&["in progress".to_string()]).unwrap();
        assert_eq!(got.len(), 7);
        assert!(got.iter().all(|i| i.state == "In Progress"), "provider spelling is preserved");
    }

    #[test]
    fn hidden_ids_are_omitted_rather_than_erroring() {
        let t = FakeTracker::demo();
        t.hide("iss-001");
        let got = t.by_ids(&["iss-001".into(), "iss-002".into()]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "iss-002");
    }
}
