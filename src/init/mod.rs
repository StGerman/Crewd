//! `crewd init`: register this operator's own GitHub App through the Manifest flow (#65).
//!
//! Every operator registers their own App because the private key belongs to whoever owns it, and
//! a self-hosted daemon has to hold that key to mint tokens; there is no one App to ship. What this
//! module decides is only how much of that is the human's work: two clicks — *Create* on GitHub's
//! manifest page, *Install* on the App's — and nothing typed. The page, the listener and the four
//! GitHub calls are in the submodules; this file is the order they happen in and the files they
//! leave.
//!
//! The order is what makes a failure recoverable. The key is written the moment the conversion
//! returns it, before anything else can fail, because the code that produced it is single-use:
//! a key held only in memory across the install wait is a key a Ctrl-C throws away along with the
//! App it belongs to. The settings file is written last, and only with an installation id read
//! back over the App's own JWT, so a file that exists is a file #64's `tracker.github_app` can use.
//!
//! Output is `<dir>/github-app.toml` (`app_id`, `installation_id`, `private_key_path`) and
//! `<dir>/github-app.pem` at mode 600, in a directory at mode 700. `init` never edits the daemon's
//! config — that names the settings file with one line — and never overwrites either file, so
//! re-running it is safe and a second App is a deliberate act.
//!
//! This is not the only path: `GITHUB_TOKEN` and a hand-registered App written into the same
//! settings file are untouched by it.

pub mod callback;
pub mod github;
pub mod manifest;

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ring::rand::{SecureRandom, SystemRandom};

use crate::broker::server::Limits;
use crate::clock::Clock;
use crate::http::Http;

pub const SETTINGS_FILE: &str = "github-app.toml";
pub const KEY_FILE: &str = "github-app.pem";

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error(
        "{} already exists, and crewd init never overwrites it. Move it aside to register a new \
         App, or name the existing settings from the daemon's config",
        path.display()
    )]
    Exists { path: PathBuf },
    #[error(
        "GitHub already has an App named `{name}`. Choose another with \
         `crewd init --app-name <name>`"
    )]
    NameTaken { name: String },
    #[error("the local callback listener failed: {0}")]
    Listen(String),
    #[error(
        "refused a callback whose state this run did not issue, without converting its code: \
         something other than this run's page reached the listener. Run `crewd init` again"
    )]
    StateMismatch,
    #[error(
        "the callback carried no code, so GitHub did not create the App. Run `crewd init` again"
    )]
    NoCode,
    #[error(
        "exchanging GitHub's code for the App's key failed: {0}. The code is single-use and \
         expires within the hour, so this cannot be retried: run `crewd init` again, deleting \
         the App at https://github.com/settings/apps first if GitHub shows it was created"
    )]
    Conversion(String),
    #[error("could not reach GitHub: {0}")]
    Transport(String),
    #[error("{0}")]
    Key(String),
    #[error("reading {what} as the new App failed: {detail}")]
    Api { what: String, detail: String },
    #[error(
        "the App GitHub created does not have the permissions crewd asked for: wanted {want}; \
         got {got}. Delete it at https://github.com/settings/apps and run `crewd init` again"
    )]
    Permissions { want: String, got: String },
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    #[error(
        "no installation of App {app_id} appeared on {owner}'s account in time. Its key is at {}. \
         Install it at {install_url}, then write {} by hand with app_id = {app_id}, the \
         installation_id from the URL GitHub shows after installing, and private_key_path = \
         \"{}\"",
        key.display(), settings.display(), key.display()
    )]
    NotInstalled {
        app_id: u64,
        owner: String,
        install_url: String,
        key: PathBuf,
        settings: PathBuf,
    },
}

pub struct Options {
    /// `~/.crewd` in the binary; a scratch directory in tests.
    pub dir: PathBuf,
    pub app_name: String,
    /// Register under this organization instead of the operator's own account.
    pub org: Option<String>,
    pub limits: Limits,
    /// How many times to read the installation list, with [`Operator::wait`] between reads,
    /// before handing the rest to the operator.
    pub install_polls: u32,
}

/// The human half of the flow. The binary prints and opens; tests play the browser.
pub trait Operator {
    /// Put `url` in front of the operator.
    fn show(&mut self, what: &str, url: &str);
    /// Called between two reads of the installation list.
    fn wait(&mut self);
}

#[derive(Debug)]
pub struct Registered {
    pub app_id: u64,
    pub installation_id: u64,
    pub slug: String,
    pub settings: PathBuf,
    pub key: PathBuf,
}

pub fn run(
    http: &dyn Http,
    clock: &dyn Clock,
    operator: &mut dyn Operator,
    opts: &Options,
) -> Result<Registered, InitError> {
    let settings = opts.dir.join(SETTINGS_FILE);
    let key = opts.dir.join(KEY_FILE);
    for path in [&settings, &key] {
        if path.symlink_metadata().is_ok() {
            return Err(InitError::Exists { path: path.clone() });
        }
    }
    if github::name_taken(http, &manifest::slugify(&opts.app_name))? {
        return Err(InitError::NameTaken { name: opts.app_name.clone() });
    }

    let listener = callback::Listener::bind()?;
    let nonce = nonce()?;
    let create_url = match &opts.org {
        Some(org) => format!("https://github.com/organizations/{org}/settings/apps/new"),
        None => "https://github.com/settings/apps/new".to_string(),
    };
    let redirect = format!("{}/callback", listener.base_url());
    let page = manifest::page(
        &format!("{create_url}?state={nonce}"),
        &manifest::manifest(&opts.app_name, &redirect),
    );
    operator.show("Open this to create the App on GitHub", &format!("{}/", listener.base_url()));

    let cb = callback::await_callback(listener, &nonce, &page, opts.limits)?;
    let created = match github::convert(http, &cb.code) {
        Ok(c) => c,
        Err(e) => {
            cb.fail("crewd could not exchange GitHub's code. See the terminal.");
            return Err(e);
        }
    };
    if let Err(e) = write_key(&opts.dir, &key, &created.pem) {
        cb.fail("crewd could not save the App's key. See the terminal.");
        return Err(e);
    }
    tracing::info!(app_id = created.app_id, slug = %created.slug, key = %key.display(), "app created");

    // Verified while the browser is still waiting on the callback: redirecting first would put
    // the install button in front of the operator for an App this run is about to reject.
    let verified = github::AppAuth::new(created.app_id, &created.pem)
        .and_then(|auth| auth.verify(http, clock.wall()).map(|()| auth));
    let auth = match verified {
        Ok(auth) => auth,
        Err(e) => {
            cb.fail("The App GitHub created is not the one crewd asked for. See the terminal.");
            return Err(e);
        }
    };
    let owner = opts.org.clone().unwrap_or_else(|| created.owner.clone());
    let install_url = format!("https://github.com/apps/{}/installations/new", created.slug);
    cb.redirect(&install_url, &manifest::installing(&install_url));
    operator.show("Install the App on the repositories crewd should work on", &install_url);

    let mut installation_id = None;
    for poll in 0..opts.install_polls {
        if poll > 0 {
            operator.wait();
        }
        installation_id = auth.installation(http, &owner, clock.wall())?;
        if installation_id.is_some() {
            break;
        }
    }
    let Some(installation_id) = installation_id else {
        return Err(InitError::NotInstalled {
            app_id: created.app_id,
            owner,
            install_url,
            key,
            settings,
        });
    };

    write_new(&settings, settings_toml(created.app_id, installation_id, &key).as_bytes())?;
    Ok(Registered { app_id: created.app_id, installation_id, slug: created.slug, settings, key })
}

/// A suffix for the default App name when there is no login to put there: unique enough that two
/// operators without `gh` do not collide on `crew-`.
pub fn random_suffix() -> Result<String, InitError> {
    Ok(nonce()?[..6].to_string())
}

/// 128 bits from the OS, per run: the only thing standing between a page the operator happens to
/// have open and the conversion of a code into a key.
fn nonce() -> Result<String, InitError> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| InitError::Listen("the OS gave no randomness for the state nonce".into()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_key(dir: &Path, key: &Path, pem: &github::Pem) -> Result<(), InitError> {
    let io = |source| InitError::Io { path: dir.to_path_buf(), source };
    std::fs::create_dir_all(dir).map_err(io)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    write_new(key, pem.as_str().as_bytes())
}

/// `create_new` rather than the existence check at the top of [`run`] alone: that check is minutes
/// earlier, and the mode is set at creation so the key is never readable to anyone else, even
/// briefly.
fn write_new(path: &Path, contents: &[u8]) -> Result<(), InitError> {
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(
            |source| match source.kind() {
                std::io::ErrorKind::AlreadyExists => InitError::Exists { path: path.to_path_buf() },
                _ => InitError::Io { path: path.to_path_buf(), source },
            },
        )?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|source| InitError::Io { path: path.to_path_buf(), source })
}

fn settings_toml(app_id: u64, installation_id: u64, key: &Path) -> String {
    let key = toml::Value::String(key.display().to_string());
    format!(
        "# Written by `crewd init`. Name this file from the daemon's config:\n\
         #   [tracker]\n\
         #   github_app = \"<this file>\"\n\
         app_id = {app_id}\n\
         installation_id = {installation_id}\n\
         private_key_path = {key}\n"
    )
}

#[cfg(test)]
mod tests;
