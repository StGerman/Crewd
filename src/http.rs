//! The HTTP seam every tracker and forge adapter shares (#181).
//!
//! One trait, one client, one percent-encoder, one authenticated-request helper. The trait's
//! shape is the read/write split a poll relies on: `get` is a read and `send_json` is a
//! mutation, so a poll cannot be mistaken for a write at the seam. `UreqHttp` is the
//! process-wide client `main` builds once (#150); adapters take it as `H: Http` and tests
//! substitute a fake.
//!
//! Encoding a non-ASCII query value as its Unicode scalar (`é` as `%E9`) would send a
//! different string than the label the operator wrote. [`percent_encode`] encodes UTF-8 bytes.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ureq::tls::{Certificate, PemItem, RootCerts, TlsConfig, parse_pem};

use crate::credentials::{CredentialError, Credentials};

/// Dropping the dated API version is a silent upgrade to whatever GitHub currently calls
/// current, so every GitHub caller sends this one constant.
const GITHUB_API_VERSION: &str = "2022-11-28";

/// Spelling these three headers at each GitHub call site lets the media type or the API
/// version drift. Every GitHub REST call sends this set besides `Authorization`.
pub(crate) fn github_rest_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Accept", "application/vnd.github+json".to_string()),
        ("X-GitHub-Api-Version", GITHUB_API_VERSION.to_string()),
        ("User-Agent", "crewd".to_string()),
    ]
}

/// How `Authorization` is spelled. GitHub sends `Bearer`; Jira Cloud sends `Basic` over an
/// already-encoded token (#99).
#[derive(Clone, Copy)]
pub(crate) enum AuthScheme {
    Bearer,
    Basic,
}

impl AuthScheme {
    fn value(self, token: &str) -> String {
        match self {
            AuthScheme::Bearer => format!("Bearer {token}"),
            AuthScheme::Basic => format!("Basic {token}"),
        }
    }
}

/// A second message wrapped around a credential or transport failure would hide the text
/// callers already classify on. Displaying this is displaying the inner error; callers still
/// map the variant onto their own type.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AuthError {
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Transport(#[from] HttpTransportError),
}

/// Sends once, and once more with a fresh token when the first answer is 401 and
/// [`Credentials::invalidate`] says the next token may differ.
///
/// A refused request was not applied, so repeating it is safe; a second 401 is the credential
/// really being wrong and is returned for the caller to classify. `extra` is every header
/// besides `Authorization` and is sent unchanged on both attempts.
pub(crate) fn authed(
    creds: &dyn Credentials,
    scheme: AuthScheme,
    extra: &[(&'static str, String)],
    send: impl Fn(&[(&str, String)]) -> Result<HttpResponse, HttpTransportError>,
) -> Result<HttpResponse, AuthError> {
    let resp = send(&with_authorization(creds, scheme, extra)?)?;
    if resp.status == 401 && creds.invalidate() {
        send(&with_authorization(creds, scheme, extra)?).map_err(Into::into)
    } else {
        Ok(resp)
    }
}

fn with_authorization(
    creds: &dyn Credentials,
    scheme: AuthScheme,
    extra: &[(&'static str, String)],
) -> Result<Vec<(&'static str, String)>, AuthError> {
    let token = creds.token()?;
    let mut headers = Vec::with_capacity(1 + extra.len());
    headers.push(("Authorization", scheme.value(&token)));
    headers.extend(extra.iter().map(|(name, value)| (*name, value.clone())));
    Ok(headers)
}

/// Encoding a non-ASCII character as its Unicode scalar (`é` as `%E9`, `😀` as `%1F600`) sends
/// a different query than the label the operator wrote (#181). Unreserved ASCII stays literal;
/// every other octet, including each UTF-8 byte of a non-ASCII character, is `%HH`.
pub(crate) fn percent_encode(s: &str) -> String {
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

/// One read and one JSON write.
///
/// Separate methods rather than one general `request` keep a poll from being able to mutate:
/// a fake can answer reads and refuse writes, and a call site shows which one it is.
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
///
/// `Clone` is cheap: `ureq::Agent` is an `Arc`-backed handle to its connection pool, so cloning
/// this shares one pool and one root-certificate set rather than opening a second (#150) — which
/// is what lets `main` build one from the environment and hand every tracker, forge and
/// `GithubApp` a clone instead of each reading `SSL_CERT_FILE` on its own.
#[derive(Clone)]
pub struct UreqHttp {
    agent: ureq::Agent,
}

/// How long a connection may take to open. GitHub answers in well under a second.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The whole call, connect to last byte of the body. Search and GraphQL answers take seconds;
/// this is only the ceiling that turns a dead connection into an error.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The variable a Netskope-style TLS-intercepting proxy already makes an operator export for
/// curl, Python and Node — reused rather than inventing a crewd-specific name (#150).
const SSL_CERT_FILE: &str = "SSL_CERT_FILE";

/// What can go wrong reading the bundle `SSL_CERT_FILE` names. Kept distinct from
/// [`HttpTransportError`], which is a request failure: this one is a startup failure, so `main`
/// can refuse to run rather than hand every tracker poll a client that was never built.
#[derive(Debug, thiserror::Error)]
pub enum CaBundleError {
    #[error("{SSL_CERT_FILE} names {path}, which could not be read: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("{SSL_CERT_FILE} names {path}, which holds no certificate")]
    NoCertificates { path: PathBuf },
    #[error("{SSL_CERT_FILE} names {path}, which could not be parsed as a PEM bundle: {source}")]
    Parse { path: PathBuf, source: ureq::Error },
}

/// The whole trust store, read from `value` — the standard meaning `SSL_CERT_FILE` already has
/// for curl, OpenSSL and Python, so `RootCerts::Specific` (which *replaces* ureq's default
/// WebPki roots rather than adding to them) is the correct behaviour, not a shortcut. Non-
/// certificate PEM items (a stray private key, a CRL) are ignored rather than refused, the same
/// tolerance `openssl` extends to a bundle file. Takes the variable's value rather than reading
/// the environment itself, so a test never has to mutate process-global state to exercise every
/// branch.
fn roots_from(value: Option<OsString>) -> Result<Option<Vec<Certificate<'static>>>, CaBundleError> {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    let bytes = std::fs::read(&path)
        .map_err(|source| CaBundleError::Read { path: path.clone(), source })?;
    let mut certs = Vec::new();
    for item in parse_pem(&bytes) {
        if let PemItem::Certificate(cert) =
            item.map_err(|source| CaBundleError::Parse { path: path.clone(), source })?
        {
            certs.push(cert);
        }
    }
    if certs.is_empty() {
        return Err(CaBundleError::NoCertificates { path });
    }
    Ok(Some(certs))
}

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
        Self::build(connect, call, None)
    }

    /// The one client `main` builds for the whole process (#150): today's default-rooted client
    /// when `SSL_CERT_FILE` is unset or empty, or one trusting exactly that bundle's certificates
    /// when it names one — never both, since `ureq` 3's `RootCerts::Specific` replaces the
    /// default roots rather than extending them. An operator who named a bundle that cannot be
    /// read, or that holds no certificate, gets that refusal here, at startup, rather than as a
    /// transport error on the first poll.
    pub fn from_env() -> Result<Self, CaBundleError> {
        let value = std::env::var_os(SSL_CERT_FILE);
        let path = value.clone().map(PathBuf::from);
        let roots = roots_from(value)?;
        if let (Some(certs), Some(path)) = (&roots, &path) {
            // Never the certificates themselves — only the count and the path the operator
            // already knows they named.
            tracing::info!(
                path = %path.display(),
                certificates = certs.len(),
                "{SSL_CERT_FILE} set: trusting exactly its certificates instead of the default roots"
            );
        }
        Ok(Self::build(CONNECT_TIMEOUT, CALL_TIMEOUT, roots))
    }

    fn build(connect: Duration, call: Duration, roots: Option<Vec<Certificate<'static>>>) -> Self {
        // ureq's default turns a non-2xx status into an `Err` that drops the response body and
        // headers — exactly the rate-limit header and body snippet a caller needs to classify
        // the failure. Disabling it is what makes every status code, not just 2xx, arrive as an
        // ordinary `HttpResponse`.
        //
        // A renamed repo makes every call 301 to `/repositories/<id>/...`; ureq's own default
        // (`RedirectAuthHeaders::Never`) never forwards `Authorization` on that redirect, so the
        // retry lands anonymous and burns the 60/hour IP-keyed limit in minutes (#68). `SameHost`
        // keeps the header only when the redirect stays on the same host under HTTPS, which is
        // this case, without weakening the cross-host protection the default exists for.
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            .timeout_connect(Some(connect))
            .timeout_global(Some(call));
        // Left untouched (the `TlsConfig` default, `RootCerts::WebPki`) when `roots` is `None`,
        // so an unset `SSL_CERT_FILE` really does build the client exactly as before (#150).
        if let Some(certs) = roots {
            config = config.tls_config(
                TlsConfig::builder().root_certs(RootCerts::Specific(Arc::new(certs))).build(),
            );
        }
        Self { agent: ureq::Agent::new_with_config(config.build()) }
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

/// A `FakeHttp` test is structurally blind to ureq's default turning a non-2xx response into
/// an `Err` that discards the body and headers adapters classify on: it compiled, and every
/// other test stayed green, while a live 404 came back as a transport error instead of a
/// response. These tests talk to a raw `TcpListener` rather than a real server so they still
/// run with no network.
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

/// `roots_from` is what `SSL_CERT_FILE` actually drives (#150); it takes the variable's value
/// as a parameter rather than reading `std::env` itself, exactly so these tests never mutate the
/// process environment shared with every other test in the binary.
#[cfg(test)]
mod ca_bundle_tests {
    use super::*;

    /// A directory under the OS temp root, unique to this process and this test's thread, so
    /// concurrently running tests never race on the same path — no `tempfile` dependency needed
    /// for two throwaway files.
    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crewd-ca-bundle-test-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// insta's `filters` feature is not enabled, so the temp directory — different on every
    /// run — is swapped for a fixed placeholder before the message is snapshotted.
    fn redact_tmp(message: &str, dir: &std::path::Path) -> String {
        message.replace(&dir.display().to_string(), "<tmp>")
    }

    #[test]
    fn an_unreadable_ca_bundle_refuses_startup_and_names_the_path() {
        let dir = unique_temp_dir("missing");
        let missing = dir.join("bundle.pem");
        let err = roots_from(Some(missing.into_os_string())).unwrap_err();
        insta::assert_snapshot!(
            "a_missing_ca_bundle_refuses_startup_and_names_the_path",
            redact_tmp(&err.to_string(), &dir)
        );

        let dir = unique_temp_dir("empty");
        let empty = dir.join("bundle.pem");
        std::fs::write(&empty, b"not a certificate\n").unwrap();
        let err = roots_from(Some(empty.into_os_string())).unwrap_err();
        insta::assert_snapshot!(
            "a_ca_bundle_with_no_certificate_refuses_startup_and_names_the_path",
            redact_tmp(&err.to_string(), &dir)
        );
    }

    #[test]
    fn an_unset_ssl_cert_file_leaves_the_default_roots() {
        assert!(roots_from(None).unwrap().is_none(), "unset must add no root");
        assert!(
            roots_from(Some(OsString::new())).unwrap().is_none(),
            "empty must add no root, the same as unset"
        );

        // `roots_from(None)` alone proves nothing about `UreqHttp::build`, which is the type
        // that decides whether `TlsConfig::builder().root_certs(..)` is ever called at all — so
        // this pins that a `None` really does leave the agent on the untouched `TlsConfig`
        // default, `RootCerts::WebPki`, rather than an equivalent-looking `Specific([])`.
        let http = UreqHttp::build(CONNECT_TIMEOUT, CALL_TIMEOUT, None);
        assert!(
            matches!(http.agent.config().tls_config().root_certs(), RootCerts::WebPki),
            "an unset SSL_CERT_FILE must leave the client builder unchanged"
        );
    }
}

/// A real TLS handshake against a self-signed test CA is what proves `RootCerts::Specific`
/// actually replaces `ureq`'s default roots rather than merely compiling — a `FakeHttp` test
/// cannot reach this, the same gap `ureq_http_tests` closes for plain HTTP. Fixtures under
/// `src/tracker/testdata/tls/` are throwaway test material: an EC P-256 CA (~100-year validity,
/// key discarded right after signing) and a `localhost`/`127.0.0.1` server certificate it signed,
/// generated once with the `openssl` CLI and never used outside this test.
#[cfg(test)]
mod tls_bundle_tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use rustls::ServerConfig;
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};

    use super::*;

    const CA_CERT_PEM: &[u8] = include_bytes!("tracker/testdata/tls/ca-cert.pem");
    const SERVER_CERT_PEM: &[u8] = include_bytes!("tracker/testdata/tls/server-cert.pem");
    const SERVER_KEY_PEM: &[u8] = include_bytes!("tracker/testdata/tls/server-key.pem");

    /// Accepts exactly one TLS connection over `127.0.0.1`, answers a fixed HTTP/1.1 response
    /// once the handshake completes, then exits — the TLS analogue of `ureq_http_tests::
    /// serve_once`. A client that refuses the handshake (an untrusted issuer) never completes it,
    /// so `write_all`'s error there is expected and ignored rather than unwrapped.
    fn serve_tls_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let cert = CertificateDer::from_pem_slice(SERVER_CERT_PEM).unwrap();
            let key = PrivateKeyDer::from_pem_slice(SERVER_KEY_PEM).unwrap();
            // `ring`, never `aws-lc-rs`, which needs a C toolchain (#150); passed explicitly
            // rather than installed process-wide, so this test never races another test over
            // which provider `rustls::crypto::CryptoProvider::install_default` won.
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .unwrap();
            let conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let (sock, _) = listener.accept().unwrap();
            let mut tls = rustls::StreamOwned::new(conn, sock);
            let mut buf = [0u8; 1024];
            let _ = tls.read(&mut buf); // drives the handshake and drains the request
            let _ = tls.write_all(response.as_bytes());
        });
        port
    }

    #[test]
    fn a_server_cert_signed_by_an_extra_ca_is_accepted_only_when_the_bundle_is_set() {
        const RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";

        let port = serve_tls_once(RESPONSE);
        let unbundled = UreqHttp::default().get(&format!("https://localhost:{port}/"), &[]);
        assert!(
            unbundled.is_err(),
            "the default roots must not trust a certificate the test CA signed, got {unbundled:?}"
        );

        let port = serve_tls_once(RESPONSE);
        let ca = Certificate::from_pem(CA_CERT_PEM).unwrap();
        let bundled = UreqHttp::build(CONNECT_TIMEOUT, CALL_TIMEOUT, Some(vec![ca]));
        let resp = bundled.get(&format!("https://localhost:{port}/"), &[]).unwrap();
        assert_eq!(resp.status, 200, "the bundle naming the signing CA must be trusted");
    }
}
