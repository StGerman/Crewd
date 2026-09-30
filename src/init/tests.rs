//! The Manifest flow end to end, over a real loopback socket and a fake GitHub.
//!
//! The browser is a thread speaking raw HTTP to the listener, so the nonce, the `Host` check and
//! the listener closing are exercised on the wire; GitHub is a fake over the `Http` seam that
//! refuses to answer `/app` unless the request carries a JWT the created key verifies.

use std::collections::VecDeque;
use std::io::{Read, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaKeyPair, UnparsedPublicKey};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;
use serde_json::{Value, json};

use super::*;
use crate::clock::FakeClock;
use crate::http::{HttpResponse, HttpTransportError};

const CLIENT_SECRET: &str = "client-secret-7c1d0f3e9a";
const WEBHOOK_SECRET: &str = "webhook-secret-51be2a";

/// A throwaway key generated once per test binary, never the operator's.
fn throwaway_pem() -> &'static str {
    static PEM: OnceLock<String> = OnceLock::new();
    PEM.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("crew-init-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.pem");
        let out = Command::new("openssl")
            .args(["genrsa", "-traditional", "-out"])
            .arg(&path)
            .arg("2048")
            .output()
            .unwrap();
        if !out.status.success() {
            // LibreSSL has no `-traditional` and writes PKCS#1 already.
            let out =
                Command::new("openssl").args(["genrsa", "-out"]).arg(&path).arg("2048").output();
            assert!(out.unwrap().status.success(), "openssl genrsa failed");
        }
        std::fs::read_to_string(&path).unwrap()
    })
}

fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("crew-init-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.join("crewd")
}

// ---- GitHub ----------------------------------------------------------------

struct FakeGithub {
    name_taken: bool,
    conversion_status: u16,
    permissions: Value,
    /// One answer per read of `/app/installations`; empty once exhausted.
    installations: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<String>>,
}

impl Default for FakeGithub {
    fn default() -> Self {
        Self {
            name_taken: false,
            conversion_status: 201,
            permissions: manifest::permissions(),
            installations: Mutex::new(VecDeque::from([
                json!([]),
                json!([
                    { "id": 5, "account": { "login": "someone-else" } },
                    { "id": 99, "account": { "login": "Octo" } },
                ]),
            ])),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl FakeGithub {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn converted(&self) -> bool {
        self.calls().iter().any(|c| c.contains("/conversions"))
    }

    /// A JWT the throwaway key signed, naming App 42 — or a 401, as GitHub would answer.
    fn authorized(headers: &[(&str, String)]) -> bool {
        let Some((_, auth)) = headers.iter().find(|(k, _)| *k == "Authorization") else {
            return false;
        };
        let Some(jwt) = auth.strip_prefix("Bearer ") else { return false };
        let Some((input, sig)) = jwt.rsplit_once('.') else { return false };
        let der = PrivateKeyDer::from_pem_slice(throwaway_pem().as_bytes()).unwrap();
        let PrivateKeyDer::Pkcs1(der) = der else { panic!("the throwaway key is PKCS#1") };
        let key = RsaKeyPair::from_der(der.secret_pkcs1_der()).unwrap();
        let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref());
        let Ok(sig) = URL_SAFE_NO_PAD.decode(sig) else { return false };
        if public.verify(input.as_bytes(), &sig).is_err() {
            return false;
        }
        let claims = input.split_once('.').unwrap().1;
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
        claims["iss"] == "42"
    }
}

fn respond(status: u16, body: Value) -> Result<HttpResponse, HttpTransportError> {
    Ok(HttpResponse { status, headers: Default::default(), body: body.to_string().into_bytes() })
}

impl Http for Arc<FakeGithub> {
    fn get(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<HttpResponse, HttpTransportError> {
        self.calls.lock().unwrap().push(format!("GET {url}"));
        let path = url.strip_prefix(github::API_BASE).unwrap();
        if path.starts_with("/apps/") {
            return respond(if self.name_taken { 200 } else { 404 }, json!({}));
        }
        if !FakeGithub::authorized(headers) {
            return respond(401, json!({ "message": "A JSON web token could not be decoded" }));
        }
        match path {
            "/app" => respond(200, json!({ "permissions": self.permissions, "events": [] })),
            "/app/installations" => {
                let next = self.installations.lock().unwrap().pop_front();
                respond(200, next.unwrap_or(json!([])))
            }
            other => panic!("unscripted GET {other}"),
        }
    }

    fn send_json(
        &self,
        method: &str,
        url: &str,
        _headers: &[(&str, String)],
        _body: &[u8],
    ) -> Result<HttpResponse, HttpTransportError> {
        self.calls.lock().unwrap().push(format!("{method} {url}"));
        assert_eq!(url, format!("{}/app-manifests/c0de/conversions", github::API_BASE));
        if self.conversion_status != 201 {
            return respond(self.conversion_status, json!({ "message": "Not Found" }));
        }
        respond(
            201,
            json!({
                "id": 42,
                "slug": "crew-octo",
                "node_id": "A_kwDO",
                "owner": { "login": "octo" },
                "client_id": "Iv1.0123",
                "client_secret": CLIENT_SECRET,
                "webhook_secret": WEBHOOK_SECRET,
                "pem": throwaway_pem(),
            }),
        )
    }
}

// ---- the browser -----------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct Script {
    /// Send the callback with a state this run did not issue.
    forge_state: bool,
    /// Ask for the page under another name first, as a rebinding page would.
    foreign_host_first: bool,
}

#[derive(Default)]
struct Seen {
    addr: Option<SocketAddr>,
    shown: Vec<String>,
    /// (status line, `Location` or body) per request the browser made.
    answers: Vec<(String, String)>,
    waits: u32,
    /// The browser has read the callback's answer.
    finished: bool,
    /// The state this run's page carried.
    nonce: String,
}

struct Browser {
    script: Script,
    seen: Arc<Mutex<Seen>>,
}

impl Browser {
    fn new(script: Script) -> (Self, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        (Self { script, seen: Arc::clone(&seen) }, seen)
    }
}

fn get(addr: SocketAddr, path: &str, host: &str) -> (String, String, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.lines().next().unwrap().to_string();
    let location =
        head.lines().find_map(|l| l.strip_prefix("Location: ")).unwrap_or_default().to_string();
    (status, location, body.to_string())
}

impl Operator for Browser {
    fn show(&mut self, _what: &str, url: &str) {
        let mut seen = self.seen.lock().unwrap();
        seen.shown.push(url.to_string());
        if seen.shown.len() > 1 {
            return;
        }
        let addr: SocketAddr =
            url.strip_prefix("http://").unwrap().trim_end_matches('/').parse().unwrap();
        seen.addr = Some(addr);
        let (script, record) = (self.script, Arc::clone(&self.seen));
        std::thread::spawn(move || {
            let host = addr.to_string();
            // A browser opens sockets it never writes to; the callback must not wait on one.
            let _preconnect = TcpStream::connect(addr).unwrap();
            if script.foreign_host_first {
                let (status, _, body) = get(addr, "/", &format!("evil.example:{}", addr.port()));
                record.lock().unwrap().answers.push((status, body));
            }
            let (status, _, page) = get(addr, "/", &host);
            record.lock().unwrap().answers.push((status, page.clone()));
            let state = page.split("state=").nth(1).unwrap().split('"').next().unwrap();
            record.lock().unwrap().nonce = state.to_string();
            let state = if script.forge_state { "not-this-runs" } else { state };
            let (status, location, body) =
                get(addr, &format!("/callback?code=c0de&state={state}"), &host);
            let detail = if location.is_empty() { body } else { location };
            let mut record = record.lock().unwrap();
            record.answers.push((status, detail));
            record.finished = true;
        });
    }

    fn wait(&mut self) {
        self.seen.lock().unwrap().waits += 1;
    }
}

fn opts(dir: PathBuf) -> Options {
    Options {
        dir,
        app_name: "crew-octo".into(),
        org: None,
        limits: Limits {
            idle: Duration::from_secs(2),
            request: Duration::from_secs(2),
            max_connections: 8,
        },
        install_polls: 3,
    }
}

fn init(
    github: FakeGithub,
    script: Script,
    dir: PathBuf,
) -> (Result<Registered, InitError>, Arc<FakeGithub>, Arc<Mutex<Seen>>) {
    let github = Arc::new(github);
    let (mut browser, seen) = Browser::new(script);
    let result = run(&github, &FakeClock::new(), &mut browser, &opts(dir));
    // The browser thread records the callback's answer after the server has written it.
    for _ in 0..500 {
        let seen = seen.lock().unwrap();
        if seen.finished || seen.addr.is_none() {
            break;
        }
        drop(seen);
        std::thread::sleep(Duration::from_millis(10));
    }
    (result, github, seen)
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Refused, or answered by something that is not this run's listener: parallel tests bind
/// ephemeral ports too, and one may be handed this port the moment it is freed.
fn assert_closed(seen: &Arc<Mutex<Seen>>) {
    let (addr, nonce) = {
        let seen = seen.lock().unwrap();
        (seen.addr.unwrap(), seen.nonce.clone())
    };
    assert!(addr.ip().is_loopback(), "bound to loopback, not {addr}");
    assert!(!nonce.is_empty());
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else { return };
    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let _ = write!(s, "GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    assert!(
        !String::from_utf8_lossy(&raw).contains(&nonce),
        "this run's listener is still serving at {addr} after the callback was handled"
    );
}

// ---- the tests -------------------------------------------------------------

#[test]
fn init_ends_with_a_600_key_a_settings_file_naming_it_and_an_installed_app_after_two_clicks() {
    let dir = tmp("happy");
    let (result, github, seen) = init(FakeGithub::default(), Script::default(), dir.clone());
    let registered = result.unwrap();

    assert_eq!((registered.app_id, registered.installation_id), (42, 99));
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&registered.key), 0o600);
    assert_eq!(std::fs::read_to_string(&registered.key).unwrap(), throwaway_pem());
    assert_eq!(mode(&registered.settings), 0o600);

    // The keys #64's `tracker.github_app` reads.
    #[derive(serde::Deserialize)]
    struct Settings {
        app_id: u64,
        installation_id: u64,
        private_key_path: PathBuf,
    }
    let s: Settings =
        toml::from_str(&std::fs::read_to_string(&registered.settings).unwrap()).unwrap();
    assert_eq!((s.app_id, s.installation_id), (42, 99));
    assert_eq!(s.private_key_path, dir.join(KEY_FILE));

    assert_closed(&seen);
    let seen = seen.lock().unwrap();
    let install = "https://github.com/apps/crew-octo/installations/new";
    // The callback sends the browser straight on to the second click.
    assert_eq!(seen.answers.last().unwrap(), &("HTTP/1.1 302 Found".into(), install.into()));
    assert_eq!(seen.shown[1], install);
    assert_eq!(seen.waits, 1, "installation read until it appeared, no further");
    assert!(github.converted());
}

#[test]
fn the_manifest_asks_for_exactly_the_permissions_the_read_back_checks_and_no_webhook() {
    let m = manifest::manifest("crew-octo", "http://127.0.0.1:1/callback");
    assert_eq!(m["default_permissions"], manifest::permissions());
    assert_eq!(m["default_events"], json!([]));
    assert_eq!(m["hook_attributes"]["active"], false);
    assert_eq!(m["public"], false);
    insta::assert_snapshot!(manifest::permissions().to_string());
}

#[test]
fn an_app_created_with_other_permissions_than_the_manifest_fails_the_run_over_its_own_jwt() {
    let dir = tmp("perms");
    let mut perms = manifest::permissions();
    perms["administration"] = json!("write");
    let github = FakeGithub { permissions: perms, ..Default::default() };
    let (result, github, seen) = init(github, Script::default(), dir.clone());

    let err = result.unwrap_err();
    assert!(matches!(err, InitError::Permissions { .. }), "{err}");
    let seen = seen.lock().unwrap();
    // The browser is never sent on to install an App this run rejected.
    assert_eq!(seen.answers.last().unwrap().0, "HTTP/1.1 400 Bad Request");
    assert_eq!(seen.shown.len(), 1, "the install URL was shown: {:?}", seen.shown);
    insta::assert_snapshot!(err.to_string());
    assert!(github.calls().contains(&format!("GET {}/app", github::API_BASE)));
    assert!(dir.join(KEY_FILE).exists(), "the key outlives a failed check: its code is spent");
    assert!(!dir.join(SETTINGS_FILE).exists());
}

#[test]
fn a_callback_whose_state_this_run_did_not_issue_is_refused_without_converting_the_code() {
    let dir = tmp("state");
    let script = Script { forge_state: true, ..Default::default() };
    let (result, github, seen) = init(FakeGithub::default(), script, dir.clone());

    assert!(matches!(result, Err(InitError::StateMismatch)), "{result:?}");
    assert!(!github.converted(), "the code was converted: {:?}", github.calls());
    assert!(!dir.exists());
    assert_eq!(seen.lock().unwrap().answers.last().unwrap().0, "HTTP/1.1 400 Bad Request");
    assert_closed(&seen);
}

#[test]
fn a_request_under_another_host_name_is_not_shown_the_page_or_its_nonce() {
    let script = Script { foreign_host_first: true, ..Default::default() };
    let (result, _, seen) = init(FakeGithub::default(), script, tmp("host"));

    result.unwrap();
    let seen = seen.lock().unwrap();
    let (status, body) = &seen.answers[0];
    assert_eq!(status, "HTTP/1.1 421 Misdirected Request");
    assert!(body.is_empty());
}

#[test]
fn neither_the_key_nor_the_client_secret_reaches_the_log_at_any_level() {
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // The process-wide default rather than `with_default`: tracing caches a callsite's interest
    // the first time any thread reaches it, so a thread-scoped subscriber misses every event some
    // other test got to first. Other tests' events land here too, which only widens the check.
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    let capture = CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        tracing::callsite::rebuild_interest_cache();
        capture
    });

    let (result, _, _) = init(FakeGithub::default(), Script::default(), tmp("logs"));
    result.unwrap();

    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("manifest conversion answered"), "debug output was captured: {log}");
    let key_body = throwaway_pem().lines().nth(5).unwrap().to_string();
    for secret in [key_body.as_str(), "PRIVATE KEY", CLIENT_SECRET, WEBHOOK_SECRET] {
        assert!(!log.contains(secret), "{secret} reached the log:\n{log}");
    }
}

#[test]
fn an_existing_key_or_settings_file_is_refused_by_name_before_github_is_asked_anything() {
    for file in [KEY_FILE, SETTINGS_FILE] {
        let dir = tmp(&format!("exists-{file}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), "mine").unwrap();
        let (result, github, seen) = init(FakeGithub::default(), Script::default(), dir.clone());

        match result {
            Err(InitError::Exists { path }) => assert_eq!(path, dir.join(file)),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read_to_string(dir.join(file)).unwrap(), "mine");
        assert!(github.calls().is_empty());
        assert!(seen.lock().unwrap().shown.is_empty());
    }
    let err = InitError::Exists { path: "/home/op/.crewd/github-app.pem".into() };
    insta::assert_snapshot!(err.to_string());
}

#[test]
fn a_taken_app_name_is_reported_with_the_flag_that_chooses_another() {
    let github = FakeGithub { name_taken: true, ..Default::default() };
    let (result, github, seen) = init(github, Script::default(), tmp("taken"));

    let err = result.unwrap_err();
    insta::assert_snapshot!(err.to_string());
    assert_eq!(github.calls(), [format!("GET {}/apps/crew-octo", github::API_BASE)]);
    assert!(seen.lock().unwrap().shown.is_empty());
}

#[test]
fn a_failed_exchange_says_to_start_over_rather_than_retry() {
    let dir = tmp("expired");
    let github = FakeGithub { conversion_status: 404, ..Default::default() };
    let (result, _, seen) = init(github, Script::default(), dir.clone());

    let err = result.unwrap_err();
    insta::assert_snapshot!(err.to_string());
    assert!(!dir.exists());
    assert_eq!(seen.lock().unwrap().answers.last().unwrap().0, "HTTP/1.1 400 Bad Request");
}

#[test]
fn an_app_never_installed_leaves_its_key_and_says_how_to_finish_by_hand() {
    let dir = tmp("uninstalled");
    let github = FakeGithub { installations: Mutex::new(VecDeque::new()), ..Default::default() };
    let (result, _, seen) = init(github, Script::default(), dir.clone());

    let err = result.unwrap_err();
    assert!(matches!(err, InitError::NotInstalled { app_id: 42, .. }), "{err}");
    assert!(dir.join(KEY_FILE).exists());
    assert!(!dir.join(SETTINGS_FILE).exists());
    assert_eq!(seen.lock().unwrap().waits, 2, "three reads, a wait between each");
}
