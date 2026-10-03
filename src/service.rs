//! `crewd service install|uninstall`: run one deployment as a per-user launchd agent or systemd
//! `--user` unit that starts at login and restarts after a crash (#246).
//!
//! This is an external command beside `init` (ADR 1): it runs before the daemon and decides
//! nothing about scheduling. The definition is built as plain data by [`plan`], so a test reads
//! exactly what launchd or systemd would be told, and every effect goes through the
//! `service-manager` crate's [`ServiceManager`] trait, which the tests fake.
//!
//! Choices a reader would otherwise re-derive:
//! - The program is `crewd` where `PATH` finds it, never `current_exe()`: under Homebrew that
//!   resolves into `Cellar/<version>/`, which `brew upgrade` deletes.
//! - A tracker credential read from the environment is refused rather than captured: the
//!   definition is a plain file, and a token in it outlives its rotation.
//! - `PATH` and `SSL_CERT_FILE` are captured at install time: neither manager reads the
//!   operator's shell, and without them the service cannot resolve `claude` or `git`, or trusts
//!   the wrong roots behind a TLS proxy (#150).
//! - The working directory is the config's directory, the deployment (#251): `crew.db`,
//!   `workspace.root` and the log resolve against it. Its name names the service, so two
//!   deployments on one host do not collide.
//! - Restart on failure only. A clean shutdown stays down; M11's drain exit (75) is a failure,
//!   so a new binary comes up.
//! - On macOS the plist is written here rather than by the crate, which has no key for
//!   `StandardOutPath`/`StandardErrorPath` and would add `Disabled` to a `KeepAlive` job.
//! - On Linux the unit is written here too: the crate's template leaves `ExecStart` and
//!   `WorkingDirectory` unquoted, so a deployment path with a space would split (#276).
//! - Install and uninstall ask the manager whether the service is loaded rather than trusting
//!   the definition file, which outlives or predeceases what the manager holds (#276).

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceManager, ServiceStartCtx, ServiceStatus,
    ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};

use crate::config::{Config, ConfigError};
use crate::worker::resolve::resolve_bin;

/// Prefix of every service's label; the deployment's directory name follows it.
pub const LABEL_PREFIX: &str = "dev.crewd.";

/// The log file a launchd service writes, in the deployment directory.
pub const LOG_FILE: &str = "crewd.log";

/// launchd throttles a respawn to ten seconds; the same here keeps systemd under its default
/// start limit, which would otherwise give up after five quick failures.
const RESTART_DELAY_SECS: u32 = 10;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("the config does not load, so no service was installed: {0}")]
    Config(#[from] ConfigError),
    #[error("{path}: {source}")]
    Path { path: PathBuf, source: io::Error },
    #[error("PATH is not set; the service needs it to find the worker binaries and git")]
    NoPath,
    #[error(
        "no crewd on this PATH; install it where PATH finds it (`cargo install --path .`, \
         Homebrew) so the service survives an upgrade of the binary"
    )]
    NoCrewdOnPath,
    #[error(
        "the config reads {} from the environment, which the service does not have and must not \
         hold in its definition; name a credentials file in the config instead \
         (tracker.github_app, tracker.jira.credentials)",
        .0.join(", ")
    )]
    CredentialFromEnv(Vec<&'static str>),
    #[error("HOME is not set; it names where the service definition goes")]
    NoHome,
    #[error("per-user services are supported on macOS (launchd) and Linux (systemd) only")]
    Unsupported,
    #[error(
        "the deployment directory {value:?} {what} {ch:?}, which systemd's WorkingDirectory= \
         cannot hold"
    )]
    Unquotable { what: &'static str, value: String, ch: char },
    #[error("writing the launchd plist: {0}")]
    Plist(#[from] plist::Error),
    #[error("{action} {label}: {source}")]
    Manager { action: &'static str, label: String, source: io::Error },
}

/// The service manager this host runs per-user services under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Launchd,
    Systemd,
}

impl Platform {
    pub fn native() -> Result<Self, ServiceError> {
        if cfg!(target_os = "macos") {
            Ok(Self::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Self::Systemd)
        } else {
            Err(ServiceError::Unsupported)
        }
    }

    /// The crate's manager for this platform, at user level.
    pub fn manager(self, host: &Host) -> Result<Box<dyn ServiceManager>, ServiceError> {
        Ok(match self {
            Self::Launchd => Box::new(Launchd {
                inner: service_manager::LaunchdServiceManager::user(),
                launchctl: Box::new(|args| Command::new("launchctl").args(args).output()),
                domain: format!("gui/{}", nix::unistd::getuid()),
                agents_dir: host.home.as_ref().ok_or(ServiceError::NoHome)?.join(AGENTS_DIR),
            }),
            Self::Systemd => Box::new(Reloading {
                inner: service_manager::SystemdServiceManager::user(),
                reload: Box::new(daemon_reload),
                unit_dir: host.systemd_user_dir.clone().ok_or(ServiceError::NoHome)?,
            }),
        })
    }
}

/// A systemd manager that reloads the user manager after writing a unit. The crate does not,
/// and a reinstall over a loaded unit would otherwise start it with the cached `ExecStart` and
/// environment, not the ones just written.
pub struct Reloading<M> {
    pub inner: M,
    pub reload: Box<dyn Fn() -> io::Result<()>>,
    /// Where `inner` writes its units, the same directory [`Host::systemd_user_dir`] names.
    pub unit_dir: PathBuf,
}

impl<M: ServiceManager> ServiceManager for Reloading<M> {
    fn available(&self) -> io::Result<bool> {
        self.inner.available()
    }
    fn install(&self, ctx: ServiceInstallCtx) -> io::Result<()> {
        self.inner.install(ctx)?;
        (self.reload)()
    }
    fn uninstall(&self, ctx: ServiceUninstallCtx) -> io::Result<()> {
        // A retry after a failed reload finds the unit already disabled and its file gone, which
        // the crate's `disable` refuses; the reload is all that is left, and without it systemd
        // keeps the cached unit (#276).
        if self.unit_dir.join(format!("{}.service", ctx.label.to_script_name())).exists() {
            self.inner.uninstall(ctx)?;
        }
        (self.reload)()
    }
    fn start(&self, ctx: ServiceStartCtx) -> io::Result<()> {
        self.inner.start(ctx)
    }
    fn stop(&self, ctx: ServiceStopCtx) -> io::Result<()> {
        self.inner.stop(ctx)
    }
    fn level(&self) -> service_manager::ServiceLevel {
        self.inner.level()
    }
    fn set_level(&mut self, level: service_manager::ServiceLevel) -> io::Result<()> {
        self.inner.set_level(level)
    }
    fn status(
        &self,
        ctx: service_manager::ServiceStatusCtx,
    ) -> io::Result<service_manager::ServiceStatus> {
        self.inner.status(ctx)
    }
}

/// Where a per-user launchd agent's plist lives, under `HOME`.
const AGENTS_DIR: &str = "Library/LaunchAgents";

/// Runs `launchctl` with these arguments; a test answers in its place.
pub type Launchctl = dyn Fn(&[&str]) -> io::Result<Output>;

/// `launchctl print` exits with this for a target its domain does not hold.
const LAUNCHCTL_NOT_FOUND: i32 = 113;
/// `launchctl bootout` exits with this (ESRCH) for a job that is not loaded.
const LAUNCHCTL_NO_SUCH_PROCESS: i32 = 3;

/// A launchd manager whose `status` and `uninstall` are exact (#276). The crate's `status`
/// matches `launchctl print` suggestions by substring, so `dev.crewd.acme` can read as
/// `dev.crewd.acme-api`'s state; its `uninstall` discards a failed `launchctl remove` and reports
/// success with the job still loaded; and its `install` unloads only a job whose plist is on
/// disk, so a loaded job whose plist was deleted would keep running the old definition.
pub struct Launchd<M> {
    pub inner: M,
    pub launchctl: Box<Launchctl>,
    /// `gui/<uid>`, the domain a per-user agent is loaded in.
    pub domain: String,
    pub agents_dir: PathBuf,
}

impl<M> Launchd<M> {
    fn target(&self, label: &ServiceLabel) -> String {
        format!("{}/{}", self.domain, label.to_qualified_name())
    }

    /// Unload the job if it is loaded, and fail unless launchd then agrees it is gone.
    fn bootout(&self, label: &ServiceLabel) -> io::Result<()> {
        if self.state(label)? == ServiceStatus::NotInstalled {
            return Ok(());
        }
        let target = self.target(label);
        let out = (self.launchctl)(&["bootout", &target])?;
        if !out.status.success() && out.status.code() != Some(LAUNCHCTL_NO_SUCH_PROCESS) {
            return Err(launchctl_failed("bootout", &out));
        }
        match self.state(label)? {
            ServiceStatus::NotInstalled => Ok(()),
            _ => Err(io::Error::other(format!("launchctl bootout {target}: still loaded"))),
        }
    }

    fn state(&self, label: &ServiceLabel) -> io::Result<ServiceStatus> {
        let out = (self.launchctl)(&["print", &self.target(label)])?;
        match out.status.code() {
            Some(0) => {
                let running = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .any(|l| l.trim() == "state = running");
                Ok(if running { ServiceStatus::Running } else { ServiceStatus::Stopped(None) })
            }
            Some(LAUNCHCTL_NOT_FOUND) => Ok(ServiceStatus::NotInstalled),
            _ => Err(launchctl_failed("print", &out)),
        }
    }
}

fn launchctl_failed(cmd: &str, out: &Output) -> io::Error {
    io::Error::other(format!(
        "launchctl {cmd} exited {}: {}",
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

impl<M: ServiceManager> ServiceManager for Launchd<M> {
    fn available(&self) -> io::Result<bool> {
        self.inner.available()
    }
    fn install(&self, ctx: ServiceInstallCtx) -> io::Result<()> {
        self.bootout(&ctx.label)?;
        self.inner.install(ctx)
    }
    fn uninstall(&self, ctx: ServiceUninstallCtx) -> io::Result<()> {
        // The plist goes only after the job is verified unloaded, so a failed bootout leaves
        // what a retry needs to find.
        self.bootout(&ctx.label)?;
        let plist = self.agents_dir.join(format!("{}.plist", ctx.label.to_qualified_name()));
        match std::fs::remove_file(plist) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
    fn start(&self, ctx: ServiceStartCtx) -> io::Result<()> {
        self.inner.start(ctx)
    }
    fn stop(&self, ctx: ServiceStopCtx) -> io::Result<()> {
        self.inner.stop(ctx)
    }
    fn level(&self) -> service_manager::ServiceLevel {
        self.inner.level()
    }
    fn set_level(&mut self, level: service_manager::ServiceLevel) -> io::Result<()> {
        self.inner.set_level(level)
    }
    fn status(&self, ctx: ServiceStatusCtx) -> io::Result<ServiceStatus> {
        self.state(&ctx.label)
    }
}

fn daemon_reload() -> io::Result<()> {
    let out = Command::new("systemctl").args(["--user", "daemon-reload"]).output()?;
    if out.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "systemctl --user daemon-reload: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

/// What the installing shell had that the service will not: read once, so a test can pass its
/// own.
#[derive(Debug, Clone)]
pub struct Host {
    pub path: Option<OsString>,
    pub ssl_cert_file: Option<OsString>,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    /// systemd's `--user` unit directory, which honours `XDG_CONFIG_HOME`.
    pub systemd_user_dir: Option<PathBuf>,
}

impl Host {
    pub fn from_env() -> io::Result<Self> {
        Ok(Self {
            path: std::env::var_os("PATH"),
            ssl_cert_file: std::env::var_os("SSL_CERT_FILE"),
            cwd: std::env::current_dir()?,
            home: std::env::var_os("HOME").map(PathBuf::from),
            systemd_user_dir: service_manager::systemd_user_dir_path().ok(),
        })
    }
}

/// One deployment: its config and the directory that holds it, by absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub config: PathBuf,
    pub dir: PathBuf,
    pub label: String,
}

impl Deployment {
    /// The directory is canonicalized and the file name kept as given, so a config that is
    /// itself a symlink still names the deployment it sits in, and install and uninstall agree
    /// on the label however the path was spelled.
    pub fn locate(config: &Path, cwd: &Path) -> Result<Self, ServiceError> {
        let given = cwd.join(config);
        let parent = given.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(cwd);
        let dir = std::fs::canonicalize(parent)
            .map_err(|source| ServiceError::Path { path: parent.to_path_buf(), source })?;
        let file = given.file_name().ok_or_else(|| ServiceError::Path {
            path: given.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "names no config file"),
        })?;
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Self {
            config: dir.join(file),
            label: format!("{LABEL_PREFIX}{}", unit_safe(&name)),
            dir,
        })
    }

    fn service_label(&self) -> ServiceLabel {
        // The whole label as the application, so the systemd unit is named `dev.crewd.<name>`
        // too: with a qualifier and organization the crate names it `crewd-<name>`.
        ServiceLabel { qualifier: None, organization: None, application: self.label.clone() }
    }

    /// Where the service's definition file lives on this platform.
    pub fn definition(&self, platform: Platform, host: &Host) -> Result<PathBuf, ServiceError> {
        Ok(match platform {
            Platform::Launchd => host
                .home
                .as_ref()
                .ok_or(ServiceError::NoHome)?
                .join(AGENTS_DIR)
                .join(format!("{}.plist", self.label)),
            Platform::Systemd => host
                .systemd_user_dir
                .as_ref()
                .ok_or(ServiceError::NoHome)?
                .join(format!("{}.service", self.label)),
        })
    }
}

/// A launchd label and a systemd unit name both accept this set; anything else in a directory's
/// name becomes `-`. A name that had to change gets a digest of the original, or `acme api` and
/// `acme-api` would share one service and installing either would replace the other.
fn unit_safe(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' })
        .collect();
    if safe == name && !safe.trim_matches('.').is_empty() && safe.len() <= NAME_MAX {
        return safe;
    }
    let digest: String = blake3::hash(name.as_bytes()).to_hex().chars().take(8).collect();
    // `safe` is ASCII, so any byte index is a char boundary.
    let kept = &safe.trim_matches('.')[..safe.trim_matches('.').len().min(NAME_MAX - 9)];
    format!("{kept}-{digest}")
}

/// The longest name [`unit_safe`] returns: systemd refuses a unit name over 255 bytes, and the
/// definition's file name, `dev.crewd.<name>.service`, has to fit the same component limit
/// most filesystems set, so a valid 255-byte directory name must not make it uncreatable.
const NAME_MAX: usize = 255 - LABEL_PREFIX.len() - ".service".len();

/// Everything the service definition says, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub platform: Platform,
    pub deployment: Deployment,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub environment: Vec<(String, String)>,
}

impl Plan {
    /// Where the daemon's stderr ends up, for the operator to read.
    pub fn logs(&self) -> String {
        match self.platform {
            Platform::Launchd => self.deployment.dir.join(LOG_FILE).display().to_string(),
            Platform::Systemd => format!("journalctl --user -u {}", self.deployment.label),
        }
    }

    pub fn install_ctx(&self) -> Result<ServiceInstallCtx, ServiceError> {
        let contents = match self.platform {
            Platform::Launchd => self.plist()?,
            Platform::Systemd => self.unit()?,
        };
        Ok(ServiceInstallCtx {
            label: self.deployment.service_label(),
            program: self.program.clone(),
            args: self.args.clone(),
            contents: Some(contents),
            username: None,
            working_directory: Some(self.deployment.dir.clone()),
            environment: Some(self.environment.clone()),
            autostart: true,
            restart_policy: RestartPolicy::OnFailure {
                delay_secs: Some(RESTART_DELAY_SECS),
                max_retries: None,
                reset_after_secs: None,
            },
        })
    }

    fn plist(&self) -> Result<String, ServiceError> {
        use plist::{Dictionary, Value};
        let s = |p: &OsStr| Value::String(p.to_string_lossy().into_owned());
        let log = s(self.deployment.dir.join(LOG_FILE).as_os_str());
        let argv = std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str));
        let mut keep_alive = Dictionary::new();
        keep_alive.insert("SuccessfulExit".into(), Value::Boolean(false));
        let mut dict = Dictionary::new();
        dict.insert("Label".into(), Value::String(self.deployment.label.clone()));
        dict.insert("ProgramArguments".into(), Value::Array(argv.map(s).collect()));
        dict.insert("WorkingDirectory".into(), s(self.deployment.dir.as_os_str()));
        dict.insert(
            "EnvironmentVariables".into(),
            Value::Dictionary(
                self.environment
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            ),
        );
        dict.insert("RunAtLoad".into(), Value::Boolean(true));
        dict.insert("KeepAlive".into(), Value::Dictionary(keep_alive));
        dict.insert("StandardOutPath".into(), log.clone());
        dict.insert("StandardErrorPath".into(), log);
        let mut out = Vec::new();
        Value::Dictionary(dict).to_writer_xml(&mut out)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// The `--user` unit, with every value quoted and escaped for the directive it is in.
    fn unit(&self) -> Result<String, ServiceError> {
        let exec: Vec<_> = std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(|a| systemd_quoted(&a.to_string_lossy(), true))
            .collect();
        let mut unit = format!(
            "[Unit]\nDescription={label}\n\n[Service]\nWorkingDirectory={dir}\n",
            label = self.deployment.label,
            dir = systemd_path(&self.deployment.dir)?,
        );
        for (k, v) in &self.environment {
            unit.push_str(&format!("Environment={}\n", systemd_quoted(&format!("{k}={v}"), false)));
        }
        unit.push_str(&format!(
            "ExecStart={}\nRestart=on-failure\nRestartSec={RESTART_DELAY_SECS}\n\n\
             [Install]\nWantedBy=default.target\n",
            exec.join(" ")
        ));
        Ok(unit)
    }
}

/// One double-quoted word of `ExecStart=` or `Environment=`, both of which unquote and C-unescape
/// their value. Without the escapes a `%` would expand as a specifier, a quote or backslash would
/// end or bend the word, a line break would start a new directive from the rest of the value, and
/// in `ExecStart=` (`dollar`) a `$` would expand as a variable.
fn systemd_quoted(value: &str, dollar: bool) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            '$' if dollar => out.push_str("$$"),
            c if c.is_ascii_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `WorkingDirectory=` takes the rest of the line as is: it expands specifiers but neither
/// unquotes nor unescapes, so a space needs nothing and a `%` is doubled. A line break, an edge
/// space that systemd strips and a trailing backslash that continues the line cannot be written.
fn systemd_path(dir: &Path) -> Result<String, ServiceError> {
    let value = dir.to_string_lossy();
    let refuse =
        |what, ch| Err(ServiceError::Unquotable { what, value: value.clone().into_owned(), ch });
    if let Some(ch) = value.chars().find(|c| c.is_control()) {
        return refuse("contains", ch);
    }
    if let Some(ch) = value.chars().last().filter(|c| c.is_whitespace() || *c == '\\') {
        return refuse("ends in", ch);
    }
    Ok(value.replace('%', "%%"))
}

/// The service definition for the config at `config`, which must load: a service started on a
/// config `Config::load` refuses would only restart into the same error. Its relative paths
/// resolve against its own directory (#251), which is the service's working directory too.
pub fn plan(config: &Path, host: &Host, platform: Platform) -> Result<Plan, ServiceError> {
    let deployment = Deployment::locate(config, &host.cwd)?;
    let cfg = Config::load(&deployment.config)?;
    // The same reason as a broken config: it passes here from a shell holding the token, then
    // exits at every restart under a manager that has none.
    let from_env = cfg.credentials_from_env()?;
    if !from_env.is_empty() {
        return Err(ServiceError::CredentialFromEnv(from_env));
    }
    let path = host.path.as_ref().filter(|p| !p.is_empty()).ok_or(ServiceError::NoPath)?;
    let path = anchor_path(path, &host.cwd);
    // No fallback to `current_exe()`: on Linux it is the resolved path, under Homebrew the
    // `Cellar/<version>/` one an upgrade deletes.
    let program = resolve_bin("crewd", Some(&path)).map_err(|_| ServiceError::NoCrewdOnPath)?;
    let mut environment = vec![("PATH".to_string(), path.to_string_lossy().into_owned())];
    if let Some(cert) = host.ssl_cert_file.as_ref().filter(|c| !c.is_empty()) {
        // Against the installing shell's cwd: the service starts in the deployment directory.
        let cert = host.cwd.join(cert);
        environment.push(("SSL_CERT_FILE".into(), cert.to_string_lossy().into_owned()));
    }
    let args = vec![OsString::from("--config"), deployment.config.clone().into_os_string()];
    Ok(Plan { platform, deployment, program, args, environment })
}

/// `PATH` with every relative entry, and an empty one (which means the cwd), joined onto the
/// installing shell's cwd: the service runs in the deployment directory, where the same entry
/// would name somewhere else, and a binary found here would then be missing at its startup.
fn anchor_path(path: &OsStr, cwd: &Path) -> OsString {
    let entries = std::env::split_paths(path).map(|e| cwd.join(e));
    // An entry that came out of `split_paths` holds no separator, so joining cannot fail.
    std::env::join_paths(entries).unwrap_or_else(|_| path.to_os_string())
}

/// What [`install`] did, for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub plan: Plan,
    pub definition: PathBuf,
}

/// Load `config`, write the service definition, and load and start it. A service already
/// running for this deployment is stopped first, so the new definition is the one running.
pub fn install(
    manager: &dyn ServiceManager,
    config: &Path,
    host: &Host,
    platform: Platform,
) -> Result<Installed, ServiceError> {
    let plan = plan(config, host, platform)?;
    let definition = plan.deployment.definition(platform, host)?;
    let ctx = plan.install_ctx()?;
    let label = plan.deployment.service_label();
    let fail = |action| {
        let label = label.to_qualified_name();
        move |source| ServiceError::Manager { action, label, source }
    };
    // Asked of the manager, not read off the disk: a loaded service outlives its deleted file,
    // and `start` on one still running would leave the old argv and environment in place, so a
    // stop that fails ends the install (#276). One not running is rewritten and started fresh.
    let status = manager
        .status(ServiceStatusCtx { label: label.clone() })
        .map_err(fail("reading the state of"))?;
    if status == ServiceStatus::Running {
        manager.stop(ServiceStopCtx { label: label.clone() }).map_err(fail("stopping"))?;
    }
    manager.install(ctx).map_err(fail("installing"))?;
    manager.start(ServiceStartCtx { label: label.clone() }).map_err(fail("starting"))?;
    Ok(Installed { plan, definition })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uninstalled {
    Removed { label: String, definition: PathBuf },
    NeverInstalled { label: String, definition: PathBuf },
}

/// Stop and remove the service for the deployment `config` sits in. The config need not load:
/// a broken one is a reason to uninstall, not a reason uninstall cannot run.
pub fn uninstall(
    manager: &dyn ServiceManager,
    config: &Path,
    host: &Host,
    platform: Platform,
) -> Result<Uninstalled, ServiceError> {
    let deployment = Deployment::locate(config, &host.cwd)?;
    let definition = deployment.definition(platform, host)?;
    let label = deployment.label.clone();
    let fail = |action| {
        let label = label.clone();
        move |source| ServiceError::Manager { action, label, source }
    };
    let ctx = deployment.service_label();
    // Both the manager and the disk have to agree it is gone: a unit whose file was removed by
    // an uninstall that failed to reload is still held by systemd, and a retry has to finish it.
    let status = manager
        .status(ServiceStatusCtx { label: ctx.clone() })
        .map_err(fail("reading the state of"))?;
    if status == ServiceStatus::NotInstalled && !definition.exists() {
        return Ok(Uninstalled::NeverInstalled { label, definition });
    }
    // systemd's `disable` leaves a running unit running after its file is gone, so a stop that
    // fails ends the uninstall rather than reporting a removal that is not one; stopping an
    // inactive unit succeeds. On launchd the crate's `launchctl remove` stops the job itself.
    if platform == Platform::Systemd {
        manager.stop(ServiceStopCtx { label: ctx.clone() }).map_err(fail("stopping"))?;
    }
    manager.uninstall(ServiceUninstallCtx { label: ctx }).map_err(fail("uninstalling"))?;
    Ok(Uninstalled::Removed { label, definition })
}

/// The advice to print when systemd will stop this user's units at logout, or `None`. Only
/// printed: enabling lingering is a host-wide decision for the operator, not for crewd.
pub fn linger_advice() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let out = Command::new("loginctl").args(["show-user", &user, "-p", "Linger"]).output().ok()?;
    linger_off(&String::from_utf8_lossy(&out.stdout)).then(|| {
        format!(
            "Lingering is off for {user}, so systemd stops this service when {user} logs out \
             and starts it again only at the next login. To keep it running, run:\n\n  \
             loginctl enable-linger {user}"
        )
    })
}

/// `loginctl show-user <user> -p Linger` prints `Linger=yes` or `Linger=no`. Anything else, a
/// host without logind included, gives no advice rather than wrong advice.
fn linger_off(show_user: &str) -> bool {
    show_user.lines().any(|l| l.trim() == "Linger=no")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::rc::Rc;

    /// Records what would have reached `launchctl` or `systemctl`.
    struct FakeManager {
        calls: Rc<RefCell<Vec<String>>>,
        fail_stop: bool,
        status: ServiceStatus,
        /// The definition the last install was given.
        contents: RefCell<Option<String>>,
        /// The file uninstall removes, as the crate's does.
        definition: Option<PathBuf>,
    }

    impl Default for FakeManager {
        fn default() -> Self {
            Self {
                calls: Rc::default(),
                fail_stop: false,
                status: ServiceStatus::NotInstalled,
                contents: RefCell::default(),
                definition: None,
            }
        }
    }

    impl FakeManager {
        fn record(&self, call: &str, label: &ServiceLabel) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("{call} {label}"));
            Ok(())
        }
    }

    impl ServiceManager for FakeManager {
        fn available(&self) -> io::Result<bool> {
            Ok(true)
        }
        fn install(&self, ctx: ServiceInstallCtx) -> io::Result<()> {
            *self.contents.borrow_mut() = ctx.contents.clone();
            self.record("install", &ctx.label)
        }
        fn uninstall(&self, ctx: ServiceUninstallCtx) -> io::Result<()> {
            if let Some(definition) = &self.definition {
                std::fs::remove_file(definition)?;
            }
            self.record("uninstall", &ctx.label)
        }
        fn start(&self, ctx: ServiceStartCtx) -> io::Result<()> {
            self.record("start", &ctx.label)
        }
        fn stop(&self, ctx: ServiceStopCtx) -> io::Result<()> {
            self.record("stop", &ctx.label)?;
            if self.fail_stop { Err(io::Error::other("Failed to stop")) } else { Ok(()) }
        }
        fn level(&self) -> service_manager::ServiceLevel {
            service_manager::ServiceLevel::User
        }
        fn set_level(&mut self, _: service_manager::ServiceLevel) -> io::Result<()> {
            Ok(())
        }
        fn status(&self, ctx: ServiceStatusCtx) -> io::Result<ServiceStatus> {
            self.record("status", &ctx.label)?;
            Ok(self.status.clone())
        }
    }

    /// `<tmp>/crew-svc-<pid>-<tag>/` holding `acme-api/crew.toml` (a copy of the checked-in
    /// fake config), an executable `bin/crewd` and a `home/`, canonicalized so a macOS `/var`
    /// symlink does not differ from what `Deployment::locate` returns.
    fn sandbox(tag: &str) -> (PathBuf, Host) {
        let root = std::env::temp_dir().join(format!("crew-svc-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["acme-api", "bin", "home"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("crew.toml"),
            root.join("acme-api/crew.toml"),
        )
        .unwrap();
        let crewd = root.join("bin/crewd");
        std::fs::write(&crewd, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&crewd, std::fs::Permissions::from_mode(0o755)).unwrap();
        let host = Host {
            path: Some(std::env::join_paths([root.join("bin"), "/usr/bin".into()]).unwrap()),
            ssl_cert_file: None,
            cwd: root.clone(),
            home: Some(root.join("home")),
            systemd_user_dir: Some(root.join("home/.config/systemd/user")),
        };
        (root, host)
    }

    #[test]
    fn the_service_runs_this_binary_with_an_absolute_config_its_directory_and_the_installing_path()
    {
        let (root, mut host) = sandbox("plan");
        host.ssl_cert_file = Some("certs/proxy.pem".into());
        let dir = root.join("acme-api");
        let path = host.path.clone().unwrap().to_string_lossy().into_owned();
        for platform in [Platform::Launchd, Platform::Systemd] {
            let ctx = plan(Path::new("acme-api/crew.toml"), &host, platform)
                .unwrap()
                .install_ctx()
                .unwrap();
            assert_eq!(ctx.label.to_qualified_name(), "dev.crewd.acme-api");
            assert_eq!(ctx.label.to_script_name(), "dev.crewd.acme-api", "the systemd unit's name");
            assert_eq!(ctx.program, root.join("bin/crewd"), "found on PATH, not current_exe");
            assert_eq!(ctx.args, [OsString::from("--config"), dir.join("crew.toml").into()]);
            assert_eq!(ctx.working_directory.as_deref(), Some(dir.as_path()));
            assert_eq!(
                ctx.environment,
                Some(vec![
                    ("PATH".to_string(), path.clone()),
                    (
                        "SSL_CERT_FILE".to_string(),
                        root.join("certs/proxy.pem").display().to_string()
                    ),
                ])
            );
            assert!(ctx.autostart);
            assert!(matches!(ctx.restart_policy, RestartPolicy::OnFailure { .. }));
            assert!(ctx.contents.is_some(), "written here, not by the crate's template");
        }

        let plan = plan(&dir.join("crew.toml"), &host, Platform::Launchd).unwrap();
        let plist = plist::Value::from_reader_xml(plan.plist().unwrap().as_bytes()).unwrap();
        let plist = plist.as_dictionary().unwrap();
        let log = dir.join(LOG_FILE).display().to_string();
        assert_eq!(plist["StandardErrorPath"].as_string(), Some(log.as_str()));
        assert_eq!(plist["StandardOutPath"].as_string(), Some(log.as_str()));
        assert_eq!(plist["RunAtLoad"].as_boolean(), Some(true));
        let keep_alive = plist["KeepAlive"].as_dictionary().unwrap();
        assert_eq!(keep_alive["SuccessfulExit"].as_boolean(), Some(false), "restart on failure");
        assert!(!plist.contains_key("Disabled"), "loading the agent must start it");
        let argv: Vec<_> =
            plist["ProgramArguments"].as_array().unwrap().iter().map(|v| v.as_string()).collect();
        let config = dir.join("crew.toml").display().to_string();
        let program = root.join("bin/crewd").display().to_string();
        assert_eq!(argv, [Some(program.as_str()), Some("--config"), Some(config.as_str())]);

        host.path = Some("/usr/bin".into());
        let err = super::plan(&dir.join("crew.toml"), &host, Platform::Launchd).unwrap_err();
        assert!(matches!(err, ServiceError::NoCrewdOnPath), "never the running binary: {err}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_refuses_a_config_that_does_not_load() {
        let (root, host) = sandbox("broken");
        std::fs::write(root.join("acme-api/crew.toml"), "[tracker\nkind = ").unwrap();
        let manager = FakeManager::default();
        let err = install(&manager, Path::new("acme-api/crew.toml"), &host, Platform::Launchd)
            .unwrap_err();
        assert!(matches!(err, ServiceError::Config(ConfigError::Parse { .. })), "{err}");
        assert!(err.to_string().contains("crew.toml"), "names the file: {err}");
        assert!(manager.calls.borrow().is_empty(), "nothing reached launchctl");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_stops_any_running_copy_then_loads_and_starts_it() {
        let (root, host) = sandbox("install");
        let manager = FakeManager { status: ServiceStatus::Running, ..FakeManager::default() };
        let done =
            install(&manager, Path::new("acme-api/crew.toml"), &host, Platform::Launchd).unwrap();
        assert_eq!(
            done.definition,
            root.join("home/Library/LaunchAgents/dev.crewd.acme-api.plist")
        );
        assert_eq!(
            *manager.calls.borrow(),
            [
                "status dev.crewd.acme-api",
                "stop dev.crewd.acme-api",
                "install dev.crewd.acme-api",
                "start dev.crewd.acme-api"
            ],
            "stopped even with no definition on disk: a loaded service outlives its file"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_fails_when_a_running_service_cannot_be_stopped() {
        let (root, host) = sandbox("install-stuck");
        let config = Path::new("acme-api/crew.toml");
        for platform in [Platform::Launchd, Platform::Systemd] {
            let manager = FakeManager {
                status: ServiceStatus::Running,
                fail_stop: true,
                ..FakeManager::default()
            };
            let err = install(&manager, config, &host, platform).unwrap_err();
            assert!(matches!(err, ServiceError::Manager { action: "stopping", .. }), "{err}");
            assert_eq!(
                *manager.calls.borrow(),
                ["status dev.crewd.acme-api", "stop dev.crewd.acme-api"],
                "the old argv keeps running, so nothing is rewritten or started"
            );
        }
        // Only a manager that says nothing is loaded makes a stop unnecessary.
        for status in [ServiceStatus::NotInstalled, ServiceStatus::Stopped(None)] {
            let manager = FakeManager { status, fail_stop: true, ..FakeManager::default() };
            install(&manager, config, &host, Platform::Systemd).unwrap();
            assert!(!manager.calls.borrow().iter().any(|c| c.starts_with("stop")));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_on_systemd_stops_before_removing_and_a_failed_stop_removes_nothing() {
        let (root, host) = sandbox("uninstall-stop");
        let unit = root.join("home/.config/systemd/user/dev.crewd.acme-api.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "").unwrap();
        let config = Path::new("acme-api/crew.toml");
        let manager = FakeManager {
            status: ServiceStatus::Running,
            fail_stop: true,
            ..FakeManager::default()
        };
        let err = uninstall(&manager, config, &host, Platform::Systemd).unwrap_err();
        assert!(matches!(err, ServiceError::Manager { action: "stopping", .. }), "{err}");
        assert_eq!(
            *manager.calls.borrow(),
            ["status dev.crewd.acme-api", "stop dev.crewd.acme-api"],
            "never disabled"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_relative_path_entry_is_captured_against_the_installing_cwd() {
        let (root, mut host) = sandbox("relpath");
        host.path = Some(std::env::join_paths(["bin", "/usr/bin"]).unwrap());
        let plan = plan(Path::new("acme-api/crew.toml"), &host, Platform::Launchd).unwrap();
        assert_eq!(plan.program, root.join("bin/crewd"));
        let expected = std::env::join_paths([root.join("bin"), "/usr/bin".into()]).unwrap();
        assert_eq!(plan.environment[0], ("PATH".into(), expected.to_string_lossy().into_owned()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_of_a_service_never_installed_says_so_and_succeeds() {
        let (root, host) = sandbox("uninstall");
        let manager = FakeManager::default();
        let config = Path::new("acme-api/crew.toml");
        for platform in [Platform::Launchd, Platform::Systemd] {
            let got = uninstall(&manager, config, &host, platform).unwrap();
            let definition =
                Deployment::locate(config, &host.cwd).unwrap().definition(platform, &host).unwrap();
            assert_eq!(
                got,
                Uninstalled::NeverInstalled { label: "dev.crewd.acme-api".into(), definition }
            );
        }
        assert_eq!(
            *manager.calls.borrow(),
            ["status dev.crewd.acme-api", "status dev.crewd.acme-api"],
            "systemctl disable would fail on no unit"
        );
        manager.calls.borrow_mut().clear();

        let unit = root.join("home/.config/systemd/user/dev.crewd.acme-api.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "").unwrap();
        let got = uninstall(&manager, config, &host, Platform::Systemd).unwrap();
        assert!(matches!(got, Uninstalled::Removed { .. }));
        assert_eq!(
            *manager.calls.borrow(),
            [
                "status dev.crewd.acme-api",
                "stop dev.crewd.acme-api",
                "uninstall dev.crewd.acme-api"
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_deployment_path_with_a_space_installs_on_systemd() {
        let (root, mut host) = sandbox("quoting");
        let spaced = root.join("acme api");
        std::fs::create_dir_all(&spaced).unwrap();
        std::fs::copy(root.join("acme-api/crew.toml"), spaced.join("crew.toml")).unwrap();
        let config = spaced.join("crew.toml");
        let manager = FakeManager::default();
        let done = install(&manager, &config, &host, Platform::Systemd).unwrap();
        let label = &done.plan.deployment.label;
        assert!(label.starts_with("dev.crewd.acme-api-"), "made unit-safe: {label}");
        assert_ne!(
            label, "dev.crewd.acme-api",
            "and distinct from the deployment it now resembles"
        );
        let unit = manager.contents.borrow().clone().unwrap();
        let lines: Vec<_> = unit.lines().collect();
        let dir = format!("WorkingDirectory={}", spaced.display());
        assert!(lines.contains(&dir.as_str()), "{unit}");
        let exec = format!(
            "ExecStart=\"{}\" \"--config\" \"{}\"",
            root.join("bin/crewd").display(),
            config.display()
        );
        assert!(lines.contains(&exec.as_str()), "{unit}");

        // A line break in a captured value is escaped, never a directive of its own.
        host.path =
            Some(format!("{}:/opt\nExecStartPre=/bin/false", root.join("bin").display()).into());
        let manager = FakeManager::default();
        install(&manager, &config, &host, Platform::Systemd).unwrap();
        let unit = manager.contents.borrow().clone().unwrap();
        assert!(!unit.lines().any(|l| l.starts_with("ExecStartPre")), "{unit}");
        assert!(unit.contains(r"/opt\x0aExecStartPre=/bin/false"), "{unit}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_working_directory_systemd_would_trim_is_refused_by_name() {
        let (root, host) = sandbox("trailing");
        let trailing = root.join("acme ");
        std::fs::create_dir_all(&trailing).unwrap();
        std::fs::copy(root.join("acme-api/crew.toml"), trailing.join("crew.toml")).unwrap();
        let manager = FakeManager::default();
        let err =
            install(&manager, &trailing.join("crew.toml"), &host, Platform::Systemd).unwrap_err();
        assert!(matches!(err, ServiceError::Unquotable { what: "ends in", ch: ' ', .. }), "{err}");
        assert!(manager.calls.borrow().is_empty(), "nothing reached systemctl");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_systemd_unit_escapes_every_value_for_its_directive() {
        let dir = PathBuf::from("/home/op/.crewd/acme api 100%");
        let plan = Plan {
            platform: Platform::Systemd,
            deployment: Deployment {
                config: dir.join("crew.toml"),
                label: "dev.crewd.acme-api-100--1a2b3c4d".into(),
                dir,
            },
            program: "/opt/my $tools/crewd".into(),
            args: vec!["--config".into(), "/home/op/.crewd/acme api 100%/crew.toml".into()],
            environment: vec![
                ("PATH".into(), "/opt/my tools:/a\"b\\c:$HOME/bin".into()),
                ("SSL_CERT_FILE".into(), "/etc/ca%h.pem".into()),
            ],
        };
        insta::assert_snapshot!(plan.unit().unwrap());
    }

    #[test]
    fn uninstall_after_a_failed_reload_finishes_the_reload() {
        let (root, host) = sandbox("half-uninstall");
        let unit_dir = host.systemd_user_dir.clone().unwrap();
        let unit = unit_dir.join("dev.crewd.acme-api.service");
        std::fs::create_dir_all(&unit_dir).unwrap();
        std::fs::write(&unit, "").unwrap();
        // systemd still holds the unit after its file is gone, until a reload succeeds.
        let inner = FakeManager {
            status: ServiceStatus::Stopped(None),
            definition: Some(unit.clone()),
            ..FakeManager::default()
        };
        let calls = inner.calls.clone();
        let reloads = calls.clone();
        let manager = Reloading {
            inner,
            reload: Box::new(move || {
                let mut calls = reloads.borrow_mut();
                calls.push("daemon-reload".into());
                match calls.iter().filter(|c| *c == "daemon-reload").count() {
                    1 => Err(io::Error::other("Failed to reload daemon")),
                    _ => Ok(()),
                }
            }),
            unit_dir,
        };
        let config = Path::new("acme-api/crew.toml");
        let err = uninstall(&manager, config, &host, Platform::Systemd).unwrap_err();
        assert!(matches!(err, ServiceError::Manager { action: "uninstalling", .. }), "{err}");
        assert!(!unit.exists(), "the crate removed the file before the reload failed");

        let got = uninstall(&manager, config, &host, Platform::Systemd).unwrap();
        assert!(matches!(got, Uninstalled::Removed { .. }), "not NeverInstalled: {got:?}");
        assert_eq!(
            calls.borrow()[4..],
            ["status dev.crewd.acme-api", "stop dev.crewd.acme-api", "daemon-reload"],
            "the retry reloads, and does not disable a unit file that is gone"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A GitHub tracker over `tracker.github_app = "app.toml"`, written into the deployment.
    fn github_config(root: &Path, app: Option<&str>) {
        let app = app.map(|a| format!("github_app = {a:?}\n")).unwrap_or_default();
        let text = format!(
            "[tracker]\nkind = \"github\"\nowner = \"o\"\nrepo = \"r\"\n{app}\
             active_states = [\"open\"]\nterminal_states = [\"closed\"]\n"
        );
        std::fs::write(root.join("acme-api/crew.toml"), text).unwrap();
    }

    #[test]
    fn install_refuses_a_config_whose_credential_would_come_from_the_shell() {
        let (root, host) = sandbox("env-cred");
        github_config(&root, None);
        let err = plan(Path::new("acme-api/crew.toml"), &host, Platform::Launchd).unwrap_err();
        assert!(matches!(&err, ServiceError::CredentialFromEnv(v) if v == &["GITHUB_TOKEN"]));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_config_is_checked_against_its_own_directory_not_the_installing_shells() {
        let (root, host) = sandbox("cred-dir");
        github_config(&root, Some("app.toml"));
        // Present where the shell stands, absent where the service will run: refused, naming
        // the file the service would have read.
        std::fs::write(root.join("app.toml"), "").unwrap();
        let err = plan(Path::new("acme-api/crew.toml"), &host, Platform::Launchd).unwrap_err();
        let expected = root.join("acme-api/app.toml").display().to_string();
        assert!(err.to_string().contains(&expected), "{err}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// What `launchctl` holds in `gui/501`: a loaded label maps to whether it is running, and
    /// `bootout` unloads it unless `stuck`. Every argv is recorded.
    #[derive(Default)]
    struct FakeLaunchctl {
        loaded: RefCell<std::collections::HashMap<String, bool>>,
        stuck: bool,
        calls: RefCell<Vec<String>>,
    }

    impl FakeLaunchctl {
        fn run(&self, args: &[&str]) -> io::Result<Output> {
            self.calls.borrow_mut().push(args.join(" "));
            let label = args[1].strip_prefix("gui/501/").expect("a gui/501 target");
            let exit = |code: i32, stdout: &str| Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            };
            let loaded = self.loaded.borrow().get(label).copied();
            Ok(match (args[0], loaded) {
                ("print", Some(true)) => exit(0, "\tstate = running\n"),
                ("print", Some(false)) => exit(0, "\tstate = not running\n"),
                ("print", None) => exit(LAUNCHCTL_NOT_FOUND, ""),
                ("bootout", None) => exit(LAUNCHCTL_NO_SUCH_PROCESS, ""),
                ("bootout", Some(_)) => {
                    if !self.stuck {
                        self.loaded.borrow_mut().remove(label);
                    }
                    exit(0, "")
                }
                _ => unreachable!("only print and bootout are run"),
            })
        }
    }

    fn launchd(fake: Rc<FakeLaunchctl>, host: &Host) -> Launchd<FakeManager> {
        Launchd {
            inner: FakeManager::default(),
            launchctl: Box::new(move |args| fake.run(args)),
            domain: "gui/501".into(),
            agents_dir: host.home.clone().unwrap().join(AGENTS_DIR),
        }
    }

    #[test]
    fn launchd_status_is_the_exact_label_not_a_neighbour_it_prefixes() {
        let (root, host) = sandbox("launchd-exact");
        let acme = root.join("acme");
        std::fs::create_dir_all(&acme).unwrap();
        std::fs::copy(root.join("acme-api/crew.toml"), acme.join("crew.toml")).unwrap();
        let fake = Rc::new(FakeLaunchctl::default());
        fake.loaded.borrow_mut().insert("dev.crewd.acme-api".into(), true);
        let manager = launchd(fake.clone(), &host);
        install(&manager, &acme.join("crew.toml"), &host, Platform::Launchd).unwrap();
        assert_eq!(
            *manager.inner.calls.borrow(),
            ["install dev.crewd.acme", "start dev.crewd.acme"],
            "a running dev.crewd.acme-api is not dev.crewd.acme's to stop"
        );
        assert!(fake.calls.borrow().iter().all(|c| c.ends_with("/dev.crewd.acme")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_launchd_uninstall_reports_removed_only_once_the_job_is_unloaded() {
        let (root, host) = sandbox("launchd-uninstall");
        let config = Path::new("acme-api/crew.toml");
        let plist = Deployment::locate(config, &host.cwd)
            .unwrap()
            .definition(Platform::Launchd, &host)
            .unwrap();
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "").unwrap();
        let fake = Rc::new(FakeLaunchctl { stuck: true, ..FakeLaunchctl::default() });
        fake.loaded.borrow_mut().insert("dev.crewd.acme-api".into(), true);
        let err =
            uninstall(&launchd(fake.clone(), &host), config, &host, Platform::Launchd).unwrap_err();
        assert!(matches!(err, ServiceError::Manager { action: "uninstalling", .. }), "{err}");
        assert!(plist.exists(), "kept for the retry that finds the job still loaded");

        let fake = Rc::new(FakeLaunchctl::default());
        fake.loaded.borrow_mut().insert("dev.crewd.acme-api".into(), true);
        let got = uninstall(&launchd(fake.clone(), &host), config, &host, Platform::Launchd);
        assert!(matches!(got, Ok(Uninstalled::Removed { .. })), "{got:?}");
        assert!(!plist.exists());
        assert!(fake.loaded.borrow().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_launchd_reinstall_unloads_a_job_whose_plist_is_gone() {
        let (root, host) = sandbox("launchd-orphan");
        let fake = Rc::new(FakeLaunchctl::default());
        fake.loaded.borrow_mut().insert("dev.crewd.acme-api".into(), false);
        let manager = launchd(fake.clone(), &host);
        install(&manager, Path::new("acme-api/crew.toml"), &host, Platform::Launchd).unwrap();
        assert!(fake.loaded.borrow().is_empty(), "unloaded before the new plist is loaded");
        assert!(fake.calls.borrow().contains(&"bootout gui/501/dev.crewd.acme-api".to_string()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_systemd_reinstall_reloads_the_unit_before_starting_it() {
        let (root, host) = sandbox("reload");
        let inner = FakeManager { status: ServiceStatus::Running, ..FakeManager::default() };
        let calls = inner.calls.clone();
        let reloads = calls.clone();
        let manager = Reloading {
            inner,
            reload: Box::new(move || {
                reloads.borrow_mut().push("daemon-reload".into());
                Ok(())
            }),
            unit_dir: host.systemd_user_dir.clone().unwrap(),
        };
        install(&manager, Path::new("acme-api/crew.toml"), &host, Platform::Systemd).unwrap();
        assert_eq!(
            *calls.borrow(),
            [
                "status dev.crewd.acme-api",
                "stop dev.crewd.acme-api",
                "install dev.crewd.acme-api",
                "daemon-reload",
                "start dev.crewd.acme-api"
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_name_too_long_for_a_unit_file_is_cut_and_keeps_its_digest() {
        let long = "a".repeat(255);
        let name = unit_safe(&long);
        let file = format!("{LABEL_PREFIX}{name}.service");
        assert_eq!(file.len(), 255, "{file}");
        assert_ne!(unit_safe(&"a".repeat(254)), name, "two long names stay distinct");
        assert_eq!(unit_safe("acme-api"), "acme-api");
    }

    #[test]
    fn lingering_is_read_from_loginctl_output() {
        assert!(linger_off("Linger=no\n"));
        assert!(!linger_off("Linger=yes\n"));
        assert!(!linger_off(""), "no loginctl: no advice");
    }
}
