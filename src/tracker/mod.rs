//! The tracker boundary: a read kernel, nothing more.
//!
//! Two operations. Deliberately no generic comment/state/attachment CRUD — those lose provider
//! semantics and the scheduler never needs them. Ticket mutations belong to the agent via
//! host-executed tools (slice 5), not to this trait.

pub mod fake;
pub mod github;
pub mod jira;
#[cfg(test)]
pub(crate) mod test_http;

use crate::http::AuthError;
use crate::model::{ErrorClass, Issue};

#[derive(Debug, Clone, thiserror::Error)]
pub enum TrackerError {
    #[error("transport failure: {0}")]
    Request(String),
    #[error("non-success response: {0}")]
    Status(String),
    #[error("rate limited")]
    RateLimited,
    #[error("malformed payload: {0}")]
    Response(String),
    #[error("auth failed: {0}")]
    Auth(String),
}

impl TrackerError {
    /// Reuses the scheduler's own retryable/permanent split rather than a second one, so a
    /// tracker failure and a workspace or run failure read the same way in a log line. A
    /// malformed payload is treated as retryable alongside a bad status: in practice it is
    /// almost always a transient truncation or provider hiccup, not a permanent schema break
    /// worth escalating identically to a bad credential.
    pub fn class(&self) -> ErrorClass {
        match self {
            TrackerError::Request(_) => ErrorClass::TrackerRequest,
            TrackerError::Status(_) | TrackerError::Response(_) => ErrorClass::TrackerStatus,
            TrackerError::RateLimited => ErrorClass::RateLimited,
            TrackerError::Auth(_) => ErrorClass::AuthFailed,
        }
    }
}

impl From<AuthError> for TrackerError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Credential(c) => c.into(),
            AuthError::Transport(t) => TrackerError::Request(t.0),
        }
    }
}

/// Whether [`RepoLabels::ensure_label`] made the label or found it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelOutcome {
    Created,
    Kept,
}

/// Creating a repository's labels, for `crewd init` only (#247): a label that does not exist
/// cannot be put on an issue, so a dispatch label nobody created is a backlog nothing reaches.
/// Never part of [`Tracker`], which the scheduler polls and which stays read-only.
pub trait RepoLabels {
    /// Creates `name` unless the repository already has it, and never edits one that exists.
    fn ensure_label(&self, name: &str) -> Result<LabelOutcome, TrackerError>;
}

/// Without this an issue labelled `Agent` never matches a configured `agent` and is never
/// dispatched (#70): `routable` compares with plain equality against `required_labels`, which
/// `Config::normalize` has already given this same shape.
pub(crate) fn normalize_labels(labels: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for label in labels {
        let name = label.trim().to_lowercase();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

pub trait Tracker: Send + Sync {
    /// Issues currently in any of the given normalized states.
    ///
    /// Includes issues with `dispatchable == false` — the scheduler owns that final filter.
    /// An empty `states` returns empty without a provider request.
    fn by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError>;

    /// Current snapshots for specific dispatch ids, used for reconciliation.
    ///
    /// Full snapshots rather than bare states: labels, routing and dispatchability can all
    /// change while a run is active. Ids no longer visible are omitted — the scheduler treats
    /// omission as "not visible", and applies a grace count before acting on it. A successful
    /// result is complete for that call; partial success must surface as an error instead.
    fn by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_auth_failure_is_permanent() {
        assert!(TrackerError::Request(String::new()).class().retryable());
        assert!(TrackerError::Status(String::new()).class().retryable());
        assert!(TrackerError::Response(String::new()).class().retryable());
        assert!(TrackerError::RateLimited.class().retryable());
        assert!(
            !TrackerError::Auth(String::new()).class().retryable(),
            "a bad credential will not fix itself on retry"
        );
    }
}
