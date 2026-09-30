//! Shared `FakeHttp` for tracker adapters' tests (#99).
//!
//! `GithubTracker` and `JiraTracker` both talk to the same [`crate::http::Http`] seam, so one
//! scripted queue of responses serves both test modules instead of each declaring its own.

use std::collections::VecDeque;

use parking_lot::Mutex;
use serde_json::Value;

use crate::http::{Http, HttpResponse, HttpTransportError};

pub(crate) struct FakeHttp {
    inner: Mutex<FakeHttpInner>,
}

#[derive(Default)]
struct FakeHttpInner {
    responses: VecDeque<Result<HttpResponse, HttpTransportError>>,
    calls: Vec<String>,
    writes: Vec<(String, String, Value)>,
}

impl FakeHttp {
    pub(crate) fn new() -> Self {
        Self { inner: Mutex::new(FakeHttpInner::default()) }
    }

    pub(crate) fn push(&self, resp: Result<HttpResponse, HttpTransportError>) {
        self.inner.lock().responses.push_back(resp);
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        self.inner.lock().calls.clone()
    }

    /// Method, URL and parsed body of every write, in order.
    pub(crate) fn writes(&self) -> Vec<(String, String, Value)> {
        self.inner.lock().writes.clone()
    }
}

impl Http for FakeHttp {
    fn get(
        &self,
        url: &str,
        _headers: &[(&str, String)],
    ) -> Result<HttpResponse, HttpTransportError> {
        let mut g = self.inner.lock();
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
        let mut g = self.inner.lock();
        let parsed = serde_json::from_slice(body).unwrap_or(Value::Null);
        g.writes.push((method.to_string(), url.to_string(), parsed));
        g.responses
            .pop_front()
            .unwrap_or_else(|| Err(HttpTransportError("no scripted response".into())))
    }
}

pub(crate) fn ok(body: Value) -> Result<HttpResponse, HttpTransportError> {
    Ok(HttpResponse {
        status: 200,
        headers: Default::default(),
        body: body.to_string().into_bytes(),
    })
}

pub(crate) fn status(
    code: u16,
    headers: &[(&str, &str)],
) -> Result<HttpResponse, HttpTransportError> {
    Ok(HttpResponse {
        status: code,
        headers: headers.iter().map(|(k, v)| (k.to_lowercase(), v.to_string())).collect(),
        body: b"{}".to_vec(),
    })
}

/// A non-2xx response with a real JSON body, for classification tests that read a provider's
/// own error shape (Jira's `errorMessages`/`errors`, GitHub's `message`).
pub(crate) fn status_body(code: u16, body: Value) -> Result<HttpResponse, HttpTransportError> {
    Ok(HttpResponse {
        status: code,
        headers: Default::default(),
        body: body.to_string().into_bytes(),
    })
}

/// Shared with the forge it scripts, so the test can keep pushing answers after handing it over.
impl Http for std::sync::Arc<FakeHttp> {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<HttpResponse, HttpTransportError> {
        (**self).get(url, headers)
    }

    fn send_json(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, HttpTransportError> {
        (**self).send_json(method, url, headers, body)
    }
}
