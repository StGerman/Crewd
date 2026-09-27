//! Where a GitHub or Jira credential comes from: asked for per request, never captured once.
//!
//! Jira's is the simpler of the two: a Cloud API token does not expire on a schedule this
//! daemon needs to track, so [`JiraCredentialsFile::basic_token`] is Basic-encoded once and
//! handed to [`StaticToken`] unchanged, with no mint-and-refresh loop of its own (#99).
//!
//! An installation token expires an hour after it is minted and this daemon runs for days, so a
//! tracker or forge that took its token as a `String` at construction would start failing on
//! hour two (#64). Every GitHub call therefore asks a [`Credentials`] for the token it is about to
//! send. [`StaticToken`] is the `GITHUB_TOKEN` path, unchanged; [`GithubApp`] mints a JWT from the
//! App's private key, exchanges it for an installation token, and caches that until
//! [`REFRESH_MARGIN_MS`] before it expires — on the injected [`Clock`], so the refresh is driven
//! by a test stepping a fake clock rather than by waiting an hour.
//!
//! Neither the key nor a minted token ever enters this process's environment, so the worker's
//! environment allowlist has nothing new to exclude; the push reaches the token through a
//! credential file that `GitWorktreeWorkspace::publish` creates and deletes around one `git
//! push`, outside the worktree, never through argv or `.git/config`.
//!
//! RS256 is signed with `ring` and the key parsed with `rustls-pki-types`, both already in the
//! tree under `ureq`'s `rustls`, rather than with `jsonwebtoken`, whose crypto backends would add
//! a second RSA implementation for one signature an hour.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject;
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::clock::{Clock, Wall};
use crate::forge::ForgeError;
use crate::tracker::TrackerError;
use crate::tracker::github::{Http, HttpResponse};

const API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
/// A token this close to expiry is re-minted: one handed out with seconds left would expire in
/// flight on a paginated poll and reach the scheduler as a 401.
pub const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;
/// GitHub rejects a JWT whose `iat` is in its future, so it is backdated past a skewed host clock.
const JWT_BACKDATE_S: i64 = 60;
/// GitHub's ceiling on a JWT's lifetime is ten minutes; it is only ever used for one exchange.
const JWT_LIFETIME_S: i64 = 9 * 60;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CredentialError {
    /// Will not resolve on its own: a key that does not parse, an App or installation GitHub
    /// does not recognise.
    #[error("github credential unusable: {0}")]
    Permanent(String),
    /// A network failure or a provider hiccup while minting.
    #[error("minting a github installation token failed: {0}")]
    Transient(String),
}

impl From<CredentialError> for TrackerError {
    fn from(e: CredentialError) -> Self {
        match e {
            CredentialError::Permanent(m) => TrackerError::Auth(m),
            CredentialError::Transient(m) => TrackerError::Request(m),
        }
    }
}

impl From<CredentialError> for ForgeError {
    fn from(e: CredentialError) -> Self {
        match e {
            CredentialError::Permanent(m) => ForgeError::Permanent(m),
            CredentialError::Transient(m) => ForgeError::Transient(m),
        }
    }
}

pub trait Credentials: Send + Sync {
    /// The bearer token for the next request.
    fn token(&self) -> Result<String, CredentialError>;

    /// The provider answered 401 to the last token: a revoked installation token would otherwise
    /// be served from the cache until its own expiry. `true` when the next `token` may differ,
    /// which is what makes retrying the refused request worth one more call.
    fn invalidate(&self) -> bool {
        false
    }
}

/// A personal or fine-grained token from `GITHUB_TOKEN`, which does not expire on any scale this
/// daemon cares about.
pub struct StaticToken(String);

impl StaticToken {
    pub fn new(token: &str) -> Self {
        Self(token.to_string())
    }
}

impl Credentials for StaticToken {
    fn token(&self) -> Result<String, CredentialError> {
        Ok(self.0.clone())
    }
}

// ---- the App file ------------------------------------------------------------

/// What `tracker.github_app` names: the one file a hand-registered App and one created by
/// `crewd init` (#65) are both read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubAppFile {
    pub app_id: u64,
    pub installation_id: u64,
    pub private_key_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum AppFileError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("cannot parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path} has no {field}")]
    Missing { path: PathBuf, field: &'static str },
    /// A placeholder left in a template: GitHub ids start at 1, so a zero would pass startup and
    /// fail only at the first mint.
    #[error("{path} has {field} = 0; GitHub ids are positive")]
    Zero { path: PathBuf, field: &'static str },
    #[error("private key {path} is not readable: {source}")]
    KeyRead { path: PathBuf, source: std::io::Error },
    #[error("private key {path} is not an RSA private key: {message}")]
    Key { path: PathBuf, message: String },
}

#[derive(Deserialize)]
struct RawAppFile {
    app_id: Option<u64>,
    installation_id: Option<u64>,
    private_key_path: Option<String>,
}

impl GithubAppFile {
    /// Every missing field is named on its own, so a half-written file is one edit away from
    /// working rather than a 401 on the first poll.
    pub fn load(path: &Path) -> Result<Self, AppFileError> {
        let path = expand_home(path);
        let text = std::fs::read_to_string(&path)
            .map_err(|source| AppFileError::Read { path: path.clone(), source })?;
        let raw: RawAppFile = toml::from_str(&text)
            .map_err(|e| AppFileError::Parse { path: path.clone(), message: e.to_string() })?;
        let positive = |value: Option<u64>, field| match value {
            None => Err(AppFileError::Missing { path: path.clone(), field }),
            Some(0) => Err(AppFileError::Zero { path: path.clone(), field }),
            Some(v) => Ok(v),
        };
        let app_id = positive(raw.app_id, "app_id")?;
        let installation_id = positive(raw.installation_id, "installation_id")?;
        let key = raw.private_key_path.filter(|p| !p.trim().is_empty()).ok_or_else(|| {
            AppFileError::Missing { path: path.clone(), field: "private_key_path" }
        })?;
        let key = expand_home(Path::new(&key));
        // Relative to the file that names it, not to wherever the daemon happened to start.
        let private_key_path = match path.parent() {
            Some(dir) if key.is_relative() => dir.join(key),
            _ => key,
        };
        Ok(Self { app_id, installation_id, private_key_path })
    }

    pub fn load_key(&self) -> Result<RsaKeyPair, AppFileError> {
        let path = &self.private_key_path;
        let pem = std::fs::read(path)
            .map_err(|source| AppFileError::KeyRead { path: path.clone(), source })?;
        parse_app_key(&pem).map_err(|message| AppFileError::Key { path: path.clone(), message })
    }
}

/// A GitHub App's private key, from the PEM GitHub issues. The one key parser: the token source
/// below reads it from the settings file and `crewd init` from the manifest conversion (#65).
pub fn parse_app_key(pem: &[u8]) -> Result<RsaKeyPair, String> {
    match PrivateKeyDer::from_pem_slice(pem).map_err(|e| e.to_string())? {
        PrivateKeyDer::Pkcs1(der) => RsaKeyPair::from_der(der.secret_pkcs1_der()),
        PrivateKeyDer::Pkcs8(der) => RsaKeyPair::from_pkcs8(der.secret_pkcs8_der()),
        _ => return Err("GitHub App keys are RSA".into()),
    }
    .map_err(|e| e.to_string())
}

/// What an App authenticates as itself with: its id and key, signing the short-lived RS256 JWT
/// GitHub accepts from an App. One implementation, shared by the installation-token source below
/// and `crewd init`'s read-back of the App it just created (#65), so the claims and the
/// signature cannot drift apart between the two.
pub struct AppSigner {
    app_id: u64,
    key: RsaKeyPair,
    rng: SystemRandom,
}

impl AppSigner {
    pub fn new(app_id: u64, key: RsaKeyPair) -> Self {
        Self { app_id, key, rng: SystemRandom::new() }
    }

    /// `iat` is backdated by [`JWT_BACKDATE_S`] for clock drift against GitHub's, and both
    /// times come from `now`, the injected clock.
    pub fn jwt(&self, now: Wall) -> Result<String, String> {
        let now_s = now.0.div_euclid(1000);
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = json!({
            "iat": now_s - JWT_BACKDATE_S,
            "exp": now_s + JWT_LIFETIME_S,
            "iss": self.app_id.to_string(),
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let mut sig = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &self.rng, signing_input.as_bytes(), &mut sig)
            .map_err(|_| "signing the App's JWT failed".to_string())?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig)))
    }
}

// ---- Jira Basic auth --------------------------------------------------------

/// What `tracker.jira.credentials` names: the operator's own Atlassian account email and API
/// token. Both are read together because Jira Cloud's Basic auth needs the pair — the token
/// alone does not identify an account the way a GitHub PAT does.
#[derive(Clone)]
pub struct JiraCredentialsFile {
    pub(crate) email: String,
    pub(crate) api_token: String,
}

impl std::fmt::Debug for JiraCredentialsFile {
    /// Hand-written so the token can never reach a log line or a panic message through a
    /// derived `Debug`, the way `GithubAppFile`'s key path (never its contents) is the only
    /// thing that type prints either.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JiraCredentialsFile")
            .field("email", &self.email)
            .field("api_token", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JiraCredentialsError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("cannot parse {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path} has no {field}")]
    Missing { path: PathBuf, field: &'static str },
    /// The `JIRA_EMAIL`/`JIRA_API_TOKEN` fallback names the variable, not a path, so this stays
    /// a separate variant rather than reusing [`JiraCredentialsError::Missing`].
    #[error("{0} is not set")]
    MissingEnv(&'static str),
}

#[derive(Deserialize)]
struct RawJiraCredentials {
    email: Option<String>,
    api_token: Option<String>,
}

impl JiraCredentialsFile {
    /// Each missing or blank field is named on its own, mirroring [`GithubAppFile::load`].
    pub fn load(path: &Path) -> Result<Self, JiraCredentialsError> {
        let path = expand_home(path);
        let text = std::fs::read_to_string(&path)
            .map_err(|source| JiraCredentialsError::Read { path: path.clone(), source })?;
        let raw: RawJiraCredentials =
            toml::from_str(&text).map_err(|e| JiraCredentialsError::Parse {
                path: path.clone(),
                message: jira_toml_error_message(&text, &e),
            })?;
        let field = |value: Option<String>, name| match value.as_deref().map(str::trim) {
            Some(v) if !v.is_empty() => Ok(v.to_string()),
            _ => Err(JiraCredentialsError::Missing { path: path.clone(), field: name }),
        };
        Ok(Self {
            email: field(raw.email, "email")?,
            api_token: field(raw.api_token, "api_token")?,
        })
    }

    /// `JIRA_EMAIL` and `JIRA_API_TOKEN`: the fallback when `tracker.jira.credentials` is unset.
    pub fn from_env() -> Result<Self, JiraCredentialsError> {
        let var = |name: &'static str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or(JiraCredentialsError::MissingEnv(name))
        };
        Ok(Self { email: var("JIRA_EMAIL")?, api_token: var("JIRA_API_TOKEN")? })
    }

    /// Basic auth's `base64(email:api_token)`, sent in every request's `Authorization` header —
    /// Jira Cloud's REST v3 accepts the API token this way, with no token exchange to cache.
    pub fn basic_token(&self) -> String {
        base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.email, self.api_token))
    }
}

/// `e.message()` and a line number from `e.span()`, never `toml::de::Error`'s `Display`: the
/// latter prints the offending source line, which for `api_token = "...` is the secret itself.
fn jira_toml_error_message(text: &str, e: &toml::de::Error) -> String {
    match e.span() {
        Some(span) => {
            let upto = &text.as_bytes()[..span.start.min(text.len())];
            let line = upto.iter().filter(|&&b| b == b'\n').count() + 1;
            format!("line {line}: {}", e.message())
        }
        None => e.message().to_string(),
    }
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

// ---- the App ---------------------------------------------------------------

struct Cached {
    token: String,
    expires_at: Wall,
}

pub struct GithubApp<H: Http> {
    http: H,
    installation_id: u64,
    signer: AppSigner,
    clock: Arc<dyn Clock>,
    /// Held across a mint, so a tracker poll and a forge call arriving together mint once.
    cached: Mutex<Option<Cached>>,
}

impl<H: Http> GithubApp<H> {
    pub fn new(http: H, file: &GithubAppFile, clock: Arc<dyn Clock>) -> Result<Self, AppFileError> {
        Ok(Self {
            http,
            installation_id: file.installation_id,
            signer: AppSigner::new(file.app_id, file.load_key()?),
            clock,
            cached: Mutex::new(None),
        })
    }

    fn jwt(&self, now: Wall) -> Result<String, CredentialError> {
        self.signer.jwt(now).map_err(CredentialError::Permanent)
    }

    fn mint(&self, now: Wall) -> Result<Cached, CredentialError> {
        let url = format!("{API_BASE}/app/installations/{}/access_tokens", self.installation_id);
        let headers = [
            ("Authorization", format!("Bearer {}", self.jwt(now)?)),
            ("Accept", "application/vnd.github+json".to_string()),
            ("X-GitHub-Api-Version", API_VERSION.to_string()),
            ("User-Agent", "crewd".to_string()),
        ];
        let resp = self
            .http
            .send_json("POST", &url, &headers, b"{}")
            .map_err(|e| CredentialError::Transient(e.0))?;
        classify_mint(&resp, self.installation_id)?;

        #[derive(Deserialize)]
        struct Minted {
            token: String,
            expires_at: String,
        }
        let minted: Minted = serde_json::from_slice(&resp.body)
            .map_err(|e| CredentialError::Transient(format!("malformed token response: {e}")))?;
        let expires_at = OffsetDateTime::parse(&minted.expires_at, &Rfc3339)
            .map_err(|e| CredentialError::Transient(format!("malformed expires_at: {e}")))?;
        let expires_at = Wall((expires_at.unix_timestamp_nanos() / 1_000_000) as i64);
        Ok(Cached { token: minted.token, expires_at })
    }
}

/// A 401, 403 or 404 on the exchange is the App or its installation being wrong — a key that does
/// not match, an uninstalled App — and will not fix itself; rate limits and 5xx will.
fn classify_mint(resp: &HttpResponse, installation_id: u64) -> Result<(), CredentialError> {
    if (200..300).contains(&resp.status) {
        return Ok(());
    }
    let snippet: String = String::from_utf8_lossy(&resp.body).chars().take(200).collect();
    let rate_limited = resp.status == 429
        || (resp.status == 403
            && (resp.header("retry-after").is_some()
                || resp.header("x-ratelimit-remaining").is_some_and(|v| v == "0")));
    let message = format!("installation {installation_id}: {}: {snippet}", resp.status);
    if !rate_limited && matches!(resp.status, 401 | 403 | 404) {
        Err(CredentialError::Permanent(message))
    } else {
        Err(CredentialError::Transient(message))
    }
}

impl<H: Http> Credentials for GithubApp<H> {
    fn token(&self) -> Result<String, CredentialError> {
        let now = self.clock.wall();
        let mut cached = self.cached.lock();
        if let Some(c) = cached.as_ref()
            && now.0 < c.expires_at.0 - REFRESH_MARGIN_MS
        {
            return Ok(c.token.clone());
        }
        let fresh = self.mint(now)?;
        let token = fresh.token.clone();
        *cached = Some(fresh);
        Ok(token)
    }

    fn invalidate(&self) -> bool {
        *self.cached.lock() = None;
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::process::Command;

    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::*;
    use crate::clock::FakeClock;
    use crate::tracker::github::HttpTransportError;

    /// A throwaway key generated for the test, never the operator's.
    pub(crate) fn throwaway_key(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
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
        path
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("crew-app-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[derive(Default)]
    struct MintHttp {
        responses: parking_lot::Mutex<VecDeque<HttpResponse>>,
        calls: parking_lot::Mutex<Vec<(String, String)>>,
    }

    impl MintHttp {
        fn push(&self, status: u16, body: serde_json::Value) {
            self.responses.lock().push_back(HttpResponse {
                status,
                headers: HashMap::new(),
                body: body.to_string().into_bytes(),
            });
        }
    }

    impl Http for Arc<MintHttp> {
        fn get(&self, _: &str, _: &[(&str, String)]) -> Result<HttpResponse, HttpTransportError> {
            Err(HttpTransportError("the credential source never reads".into()))
        }

        fn send_json(
            &self,
            _method: &str,
            url: &str,
            headers: &[(&str, String)],
            _body: &[u8],
        ) -> Result<HttpResponse, HttpTransportError> {
            let auth = headers.iter().find(|(k, _)| *k == "Authorization").unwrap().1.clone();
            self.calls.lock().push((url.to_string(), auth));
            self.responses.lock().pop_front().ok_or(HttpTransportError("unscripted".into()))
        }
    }

    fn minted(token: &str, expires_at: Wall) -> serde_json::Value {
        let at = OffsetDateTime::from_unix_timestamp(expires_at.0 / 1000).unwrap();
        let at = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            at.year(),
            u8::from(at.month()),
            at.day(),
            at.hour(),
            at.minute(),
            at.second()
        );
        json!({ "token": token, "expires_at": at })
    }

    fn app(tag: &str, clock: Arc<FakeClock>) -> (GithubApp<Arc<MintHttp>>, Arc<MintHttp>) {
        let dir = tmp(tag);
        let file =
            GithubAppFile { app_id: 42, installation_id: 7, private_key_path: throwaway_key(&dir) };
        let http = Arc::new(MintHttp::default());
        (GithubApp::new(http.clone(), &file, clock).unwrap(), http)
    }

    #[test]
    fn an_installation_token_is_re_minted_before_it_expires_rather_than_served_stale() {
        let clock = Arc::new(FakeClock::new());
        let (app, http) = app("refresh", clock.clone());
        let hour = 60 * 60 * 1000;
        http.push(201, minted("ghs_first", Wall(clock.wall().0 + hour)));

        assert_eq!(app.token().unwrap(), "ghs_first");
        clock.advance_ms(30 * 60 * 1000);
        assert_eq!(app.token().unwrap(), "ghs_first", "a token with time left is reused");
        assert_eq!(http.calls.lock().len(), 1);

        // Inside the margin of the first token's hour: a second mint, and the caller never sees
        // the first token again.
        clock.advance_ms((hour - 30 * 60 * 1000 - REFRESH_MARGIN_MS) as u64);
        http.push(201, minted("ghs_second", Wall(clock.wall().0 + hour)));
        assert_eq!(app.token().unwrap(), "ghs_second");
        let calls = http.calls.lock();
        assert_eq!(calls.len(), 2, "the refresh is exercised, not just the first mint");
        assert_eq!(calls[1].0, "https://api.github.com/app/installations/7/access_tokens");
    }

    #[test]
    fn a_revoked_token_is_re_minted_on_the_next_request_after_a_401() {
        let clock = Arc::new(FakeClock::new());
        let (app, http) = app("revoked", clock.clone());
        http.push(201, minted("ghs_first", Wall(clock.wall().0 + 3_600_000)));
        http.push(201, minted("ghs_second", Wall(clock.wall().0 + 3_600_000)));
        assert_eq!(app.token().unwrap(), "ghs_first");
        app.invalidate();
        assert_eq!(app.token().unwrap(), "ghs_second");
    }

    #[test]
    fn the_exchange_is_authorized_by_an_rs256_jwt_the_apps_public_key_verifies() {
        let clock = Arc::new(FakeClock::new());
        let (app, http) = app("jwt", clock.clone());
        http.push(201, minted("ghs_x", Wall(clock.wall().0 + 3_600_000)));
        app.token().unwrap();

        let auth = http.calls.lock()[0].1.clone();
        let jwt = auth.strip_prefix("Bearer ").unwrap();
        let (signing_input, sig) = jwt.rsplit_once('.').unwrap();
        let public =
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, app.signer.key.public().as_ref());
        public.verify(signing_input.as_bytes(), &URL_SAFE_NO_PAD.decode(sig).unwrap()).unwrap();

        let claims = signing_input.split_once('.').unwrap().1;
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
        let now_s = clock.wall().0 / 1000;
        assert_eq!(claims["iss"], "42");
        assert_eq!(claims["iat"], now_s - JWT_BACKDATE_S, "iat comes from the injected clock");
        assert!(claims["exp"].as_i64().unwrap() - now_s <= 600, "GitHub's ten-minute ceiling");
    }

    #[test]
    fn an_app_github_does_not_recognise_is_permanent_and_a_5xx_is_not() {
        let clock = Arc::new(FakeClock::new());
        let (app, http) = app("classify", clock);
        http.push(404, json!({ "message": "Not Found" }));
        assert!(matches!(app.token(), Err(CredentialError::Permanent(_))));
        http.push(502, json!({}));
        assert!(matches!(app.token(), Err(CredentialError::Transient(_))));
    }

    #[test]
    fn a_jira_api_token_never_appears_in_debug_output() {
        let creds =
            JiraCredentialsFile { email: "a@b.com".into(), api_token: "super-secret".into() };
        let dump = format!("{creds:?}");
        assert!(!dump.contains("super-secret"), "{dump}");
        assert!(dump.contains("a@b.com"));
    }

    #[test]
    fn a_malformed_jira_credentials_file_never_echoes_the_token() {
        let dir = tmp("jira-malformed");
        let path = dir.join("jira.toml");
        let message = |text: &str| {
            std::fs::write(&path, text).unwrap();
            match JiraCredentialsFile::load(&path).unwrap_err() {
                JiraCredentialsError::Parse { message, .. } => message,
                other => panic!("expected Parse, got {other:?}"),
            }
        };

        let unterminated = message("email = \"a@b.com\"\napi_token = \"SECRET-TOKEN-123");
        assert!(!unterminated.contains("SECRET-TOKEN-123"), "{unterminated}");
        insta::assert_snapshot!("unterminated_string", unterminated);

        let unquoted = message("email = \"a@b.com\"\napi_token = SECRET-TOKEN-123");
        assert!(!unquoted.contains("SECRET-TOKEN-123"), "{unquoted}");
        insta::assert_snapshot!("unquoted_value", unquoted);
    }

    #[test]
    fn each_missing_piece_of_the_app_file_is_named() {
        let dir = tmp("file");
        let path = dir.join("github-app.toml");
        let err = |text: &str| {
            std::fs::write(&path, text).unwrap();
            GithubAppFile::load(&path).unwrap_err().to_string()
        };
        assert!(err("installation_id = 1\nprivate_key_path = \"k\"").ends_with("has no app_id"));
        assert!(err("app_id = 1\nprivate_key_path = \"k\"").ends_with("has no installation_id"));
        assert!(err("app_id = 1\ninstallation_id = 2").ends_with("has no private_key_path"));
        let zero = "app_id = 0\ninstallation_id = 2\nprivate_key_path = \"k\"";
        assert!(err(zero).ends_with("has app_id = 0; GitHub ids are positive"));
        let zero = "app_id = 1\ninstallation_id = 0\nprivate_key_path = \"k\"";
        assert!(err(zero).ends_with("has installation_id = 0; GitHub ids are positive"));

        std::fs::write(&path, "app_id = 1\ninstallation_id = 2\nprivate_key_path = \"k.pem\"")
            .unwrap();
        let file = GithubAppFile::load(&path).unwrap();
        assert_eq!(file.private_key_path, dir.join("k.pem"), "relative to the file naming it");
        assert!(matches!(file.load_key(), Err(AppFileError::KeyRead { .. })));
        std::fs::write(dir.join("k.pem"), "not a key").unwrap();
        assert!(matches!(file.load_key(), Err(AppFileError::Key { .. })));
    }
}
