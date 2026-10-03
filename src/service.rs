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

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceManager, ServiceStartCtx,
    ServiceStopCtx, ServiceUninstallCtx,
};

use crate::config::{Config, ConfigError};
use crate::worker::resolve::resolve_bin;

/// Prefix of every service's label; the deployment's directory name follows it.
pub const LABEL_PREFIX: &str = "dev.crewd.";

/// The log file a launchd service writes, in the deployment directory.
pub const LOG_FILE: &str = "crewd.log";

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
    #[error("{what} {value:?} contains {ch:?}, which a systemd unit would not pass through as is")]
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
    pub fn manager(self) -> Box<dyn ServiceManager> {
        match self {
            Self::Launchd => Box::new(service_manager::LaunchdServiceManager::user()),
            Self::Systemd => Box::new(Reloading {
                inner: service_manager::SystemdServiceManager::user(),
                reload: Box::new(daemon_reload),
            }),
        }
    }
}

/// A systemd manager that reloads the user manager after writing a unit. The crate does not,
/// and a reinstall over a loaded unit would otherwise start it with the cached `ExecStart` and
/// environment, not the ones just written.
pub struct Reloading<M> {
    pub inner: M,
    pub reload: Box<dyn Fn() -> io::Result<()>>,
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
        self.inner.uninstall(ctx)?;
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
                .join("Library/LaunchAgents")
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
    if safe == name && !safe.trim_matches('.').is_empty() {
        return safe;
    }
    let digest: String = blake3::hash(name.as_bytes()).to_hex().chars().take(8).collect();
    format!("{}-{digest}", safe.trim_matches('.'))
}

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
            Platform::Launchd => Some(self.plist()?),
            Platform::Systemd => None,
        };
        Ok(ServiceInstallCtx {
            label: self.deployment.service_label(),
            program: self.program.clone(),
            args: self.args.clone(),
            contents,
            username: None,
            working_directory: Some(self.deployment.dir.clone()),
            environment: Some(self.environment.clone()),
            autostart: true,
            // launchd throttles a respawn to ten seconds; the same here keeps systemd under its
            // default start limit, which would otherwise give up after five quick failures.
            restart_policy: RestartPolicy::OnFailure {
                delay_secs: Some(10),
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
    let path = host.path.clone().filter(|p| !p.is_empty()).ok_or(ServiceError::NoPath)?;
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
    let plan = Plan { platform, deployment, program, args, environment };
    if platform == Platform::Systemd {
        check_systemd_quoting(&plan)?;
    }
    Ok(plan)
}

/// The crate writes `ExecStart=` unquoted and `Environment="K=V"`, so a space, a quote, a `$`
/// or a `%` specifier there would reach the daemon as something else. Refused by name instead.
fn check_systemd_quoting(plan: &Plan) -> Result<(), ServiceError> {
    let in_exec = |c: char| c.is_whitespace() || "\"'\\%$;".contains(c);
    // Inside the crate's quotes a space survives; a quote, a backslash or a `%` does not, and a
    // line break ends the directive, starting a new one from the rest of the value.
    let in_env = |c: char| c.is_control() || "\"\\%".contains(c);
    let check = |what, value: &OsStr, bad: &dyn Fn(char) -> bool| {
        let value = value.to_string_lossy();
        match value.chars().find(|c| bad(*c)) {
            Some(ch) => Err(ServiceError::Unquotable { what, value: value.into_owned(), ch }),
            None => Ok(()),
        }
    };
    check("the program", plan.program.as_os_str(), &in_exec)?;
    check("the deployment directory", plan.deployment.dir.as_os_str(), &in_exec)?;
    for a in &plan.args {
        check("an argument", a, &in_exec)?;
    }
    for (_, v) in &plan.environment {
        check("an environment value", OsStr::new(v), &in_env)?;
    }
    Ok(())
}

/// What [`install`] did, for the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub plan: Plan,
    pub definition: PathBuf,
}

/// Load `config`, write the service definition, and load and start it. A service already
/// installed for this deployment is stopped first, so the new definition is the one running.
pub fn install(
    manager: &dyn ServiceManager,
    config: &Path,
    host: &Host,
    platform: Platform,
) -> Result<Installed, ServiceError> {
    let plan = plan(config, host, platform)?;
    let definition = plan.deployment.definition(platform, host)?;
    let label = plan.deployment.service_label();
    let fail = |action| {
        let label = label.to_qualified_name();
        move |source| ServiceError::Manager { action, label, source }
    };
    if definition.exists() {
        // Best-effort: a service that crashed or was stopped by hand is not running, and
        // `install` below replaces its definition either way.
        let _ = manager.stop(ServiceStopCtx { label: label.clone() });
    }
    manager.install(plan.install_ctx()?).map_err(fail("installing"))?;
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
    // The crate's systemd uninstall fails on a unit it never wrote, and its launchd one
    // succeeds silently; checking the file first gives both the same answer.
    if !definition.exists() {
        return Ok(Uninstalled::NeverInstalled { label, definition });
    }
    // Best-effort, as in `install`; it matters on systemd, whose `disable` leaves a running unit
    // running after its file is gone.
    let _ = manager.stop(ServiceStopCtx { label: deployment.service_label() });
    manager.uninstall(ServiceUninstallCtx { label: deployment.service_label() }).map_err(
        |source| ServiceError::Manager { action: "uninstalling", label: label.clone(), source },
    )?;
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
    use std::rc::Rc;

    /// Records what would have reached `launchctl` or `systemctl`.
    #[derive(Default)]
    struct FakeManager {
        calls: Rc<RefCell<Vec<String>>>,
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
            self.record("install", &ctx.label)
        }
        fn uninstall(&self, ctx: ServiceUninstallCtx) -> io::Result<()> {
            self.record("uninstall", &ctx.label)
        }
        fn start(&self, ctx: ServiceStartCtx) -> io::Result<()> {
            self.record("start", &ctx.label)
        }
        fn stop(&self, ctx: ServiceStopCtx) -> io::Result<()> {
            self.record("stop", &ctx.label)
        }
        fn level(&self) -> service_manager::ServiceLevel {
            service_manager::ServiceLevel::User
        }
        fn set_level(&mut self, _: service_manager::ServiceLevel) -> io::Result<()> {
            Ok(())
        }
        fn status(
            &self,
            _: service_manager::ServiceStatusCtx,
        ) -> io::Result<service_manager::ServiceStatus> {
            Ok(service_manager::ServiceStatus::NotInstalled)
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
            assert_eq!(ctx.contents.is_some(), platform == Platform::Launchd);
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
    fn install_loads_and_starts_and_a_reinstall_stops_the_old_one_first() {
        let (root, host) = sandbox("install");
        let manager = FakeManager::default();
        let config = Path::new("acme-api/crew.toml");
        let done = install(&manager, config, &host, Platform::Launchd).unwrap();
        assert_eq!(
            done.definition,
            root.join("home/Library/LaunchAgents/dev.crewd.acme-api.plist")
        );
        assert_eq!(
            *manager.calls.borrow(),
            ["install dev.crewd.acme-api", "start dev.crewd.acme-api"]
        );

        std::fs::create_dir_all(done.definition.parent().unwrap()).unwrap();
        std::fs::write(&done.definition, "").unwrap();
        manager.calls.borrow_mut().clear();
        install(&manager, config, &host, Platform::Launchd).unwrap();
        assert_eq!(manager.calls.borrow()[0], "stop dev.crewd.acme-api");
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
        assert!(manager.calls.borrow().is_empty(), "systemctl disable would fail on no unit");

        let unit = root.join("home/.config/systemd/user/dev.crewd.acme-api.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "").unwrap();
        let got = uninstall(&manager, config, &host, Platform::Systemd).unwrap();
        assert!(matches!(got, Uninstalled::Removed { .. }));
        assert_eq!(
            *manager.calls.borrow(),
            ["stop dev.crewd.acme-api", "uninstall dev.crewd.acme-api"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_systemd_unit_refuses_a_path_its_execstart_would_split() {
        let (root, host) = sandbox("quoting");
        let spaced = root.join("acme api");
        std::fs::create_dir_all(&spaced).unwrap();
        std::fs::copy(root.join("acme-api/crew.toml"), spaced.join("crew.toml")).unwrap();
        let config = spaced.join("crew.toml");
        let err = plan(&config, &host, Platform::Systemd).unwrap_err();
        assert!(matches!(err, ServiceError::Unquotable { ch: ' ', .. }), "{err}");
        let launchd = plan(&config, &host, Platform::Launchd).unwrap();
        let label = &launchd.deployment.label;
        assert!(label.starts_with("dev.crewd.acme-api-"), "made unit-safe: {label}");
        assert_ne!(
            label, "dev.crewd.acme-api",
            "and distinct from the deployment it now resembles"
        );
        let mut broken = host.clone();
        broken.path =
            Some(format!("{}:/opt\nExecStartPre=/bin/false", root.join("bin").display()).into());
        let config = root.join("acme-api/crew.toml");
        let err = plan(&config, &broken, Platform::Systemd).unwrap_err();
        assert!(matches!(err, ServiceError::Unquotable { ch: '\n', .. }), "{err}");
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

    #[test]
    fn a_systemd_reinstall_reloads_the_unit_before_starting_it() {
        let (root, host) = sandbox("reload");
        let inner = FakeManager::default();
        let calls = inner.calls.clone();
        let reloads = calls.clone();
        let manager = Reloading {
            inner,
            reload: Box::new(move || {
                reloads.borrow_mut().push("daemon-reload".into());
                Ok(())
            }),
        };
        install(&manager, Path::new("acme-api/crew.toml"), &host, Platform::Systemd).unwrap();
        assert_eq!(
            *calls.borrow(),
            ["install dev.crewd.acme-api", "daemon-reload", "start dev.crewd.acme-api"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lingering_is_read_from_loginctl_output() {
        assert!(linger_off("Linger=no\n"));
        assert!(!linger_off("Linger=yes\n"));
        assert!(!linger_off(""), "no loginctl: no advice");
    }
}
