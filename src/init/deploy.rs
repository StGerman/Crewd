//! The deployment `crewd init` leaves once the App is registered (#247): a directory under
//! `~/.crewd/` holding a config that loads, and the labels that config dispatches on.
//!
//! The config is written from [`TEMPLATE`], shipped in the binary, rather than copied from
//! `crew.github.toml`: that file is this repository's own deployment, and a stranger's first run
//! would inherit its gate commands, its reviewers and its two workers. Both switches that let
//! the daemon act, the real worker and delivery, are asked for and never defaulted from a
//! script: with no terminal, a missing answer stops init naming its flag.
//!
//! Nothing here overwrites: a config that exists is kept and asked nothing about, and a label
//! that exists is kept. Re-running init is how an operator finds out what is still off.

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{ApiConfig, Config, WorkerKind};
use crate::tracker::{LabelOutcome, RepoLabels};

use super::{InitError, write_new};

pub const CONFIG_FILE: &str = "crewd.toml";
const TEMPLATE: &str = include_str!("deployment.toml");
/// The ports `crew.github.toml` binds, so the first deployment on a host matches the docs.
const FIRST_PORT: u16 = 8787;
/// Pairs tried before giving up: a host with fifty deployments has a problem a port won't fix.
const PORT_PAIRS: u16 = 50;

/// The git clone init was run in, and the GitHub repository its `origin` names.
#[derive(Debug, Clone)]
pub struct Clone {
    pub path: PathBuf,
    pub owner: String,
    pub repo: String,
    /// `origin`'s default branch, which the gate and pull requests target.
    pub base: String,
}

impl Clone {
    pub fn at(dir: &Path) -> Result<Self, InitError> {
        let top = git(dir, &["rev-parse", "--show-toplevel"])
            .ok_or_else(|| InitError::NotAClone { dir: dir.to_path_buf() })?;
        let path = PathBuf::from(top);
        let url = git(&path, &["remote", "get-url", "origin"])
            .ok_or_else(|| InitError::NotGithub { url: "no `origin` remote".into() })?;
        // A linked worktree passes `--show-toplevel` too, but the daemon refuses to start over
        // one (#29), so init would leave a deployment that cannot run.
        let git_dir = git(&path, &["rev-parse", "--path-format=absolute", "--git-dir"]);
        let common = git(&path, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
        if let (Some(git_dir), Some(common)) = (git_dir, common)
            && git_dir != common
        {
            let main = Path::new(&common).parent().map(Path::to_path_buf).unwrap_or_default();
            return Err(InitError::LinkedWorktree { path, main });
        }
        let (owner, repo) = parse_github_remote(&url)
            .ok_or_else(|| InitError::NotGithub { url: without_userinfo(&url) })?;
        // `origin/HEAD` is set by `git clone` and absent after `git remote add`; the branch
        // checked out is the best remaining guess, and the config names it for the operator.
        let base = git(&path, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])
            .and_then(|r| r.strip_prefix("origin/").map(str::to_string))
            .or_else(|| git(&path, &["symbolic-ref", "--quiet", "--short", "HEAD"]))
            .unwrap_or_else(|| "main".into());
        Ok(Self { path, owner, repo, base })
    }
}

/// Only git's terminating newline is removed: `--show-toplevel` of a directory whose name ends
/// in a space is that name, and trimming it would run the next command somewhere else.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).strip_suffix('\n')?.to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// A URL's `user:token@` removed, so a refused remote can be named in an error without printing
/// the token an HTTPS origin may carry.
fn without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// `owner` and `repo` from an HTTPS, scp-style or `ssh://` GitHub remote. The host is matched
/// whole, so another host with `github.com/` in its path does not pass for GitHub.
pub fn parse_github_remote(url: &str) -> Option<(String, String)> {
    let rest = [
        "https://github.com/",
        "http://github.com/",
        "git@github.com:",
        "ssh://git@github.com/",
        "ssh://github.com/",
    ]
    .iter()
    .find_map(|p| url.strip_prefix(p))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let valid = |s: &str| !s.is_empty() && !s.contains('/');
    (valid(owner) && valid(repo)).then(|| (owner.to_string(), repo.to_string()))
}

/// The check command a clone's own build file implies, if any.
pub fn suggest_checks(clone: &Path) -> Option<&'static str> {
    let read = |f: &str| std::fs::read_to_string(clone.join(f)).ok();
    if clone.join("Cargo.toml").is_file() {
        return Some("cargo test");
    }
    if let Some(pkg) = read("package.json")
        && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&pkg)
        && pkg["scripts"]["test"].is_string()
    {
        return Some("npm test");
    }
    if clone.join("go.mod").is_file() {
        return Some("go test ./...");
    }
    let makefile = read("Makefile").or_else(|| read("makefile"))?;
    makefile.lines().any(|l| l.starts_with("test:")).then_some("make test")
}

/// `a b && c d` as two argvs. No shell runs them, so quoting is not understood.
pub fn parse_checks(line: &str) -> Vec<Vec<String>> {
    line.split("&&")
        .map(|c| c.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Asks the operator at a terminal. Init holds none when stdin is not one.
pub trait Prompt {
    /// A Y/n question whose empty answer is yes.
    fn yes_no(&mut self, question: &str) -> std::io::Result<bool>;
    fn line(&mut self, question: &str) -> std::io::Result<String>;
}

/// What the flags already answered; `None` is asked.
#[derive(Debug, Clone, Default)]
pub struct Answers {
    pub work: Option<bool>,
    pub deliver: Option<bool>,
    /// The gate's commands as one line, `&&` between two; empty is none.
    pub checks: Option<String>,
}

pub struct Options<'a> {
    /// `~/.crewd`, where the App's settings file is and deployments go.
    pub dir: PathBuf,
    /// The deployment's directory name; `<owner>-<repo>` unless given.
    pub name: Option<String>,
    pub app_file: PathBuf,
    pub answers: Answers,
    /// Whether nothing on this host listens on a loopback port. Injected so a test does not
    /// depend on what the machine running it has bound.
    pub port_free: &'a dyn Fn(u16) -> bool,
}

#[derive(Debug)]
pub struct Written {
    pub dir: PathBuf,
    pub config: PathBuf,
    /// The config was already there and is unchanged.
    pub kept: bool,
}

/// `<root>/<name>`, `<owner>-<repo>` unless named.
pub fn deployment_dir(
    root: &Path,
    name: Option<&str>,
    clone: &Clone,
) -> Result<PathBuf, InitError> {
    let name =
        name.map(str::to_string).unwrap_or_else(|| format!("{}-{}", clone.owner, clone.repo));
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(InitError::BadName { name });
    }
    Ok(root.join(name))
}

/// What [`write`] would refuse for want of an answer, checked before registration starts: that
/// waits on a browser, so a script missing a flag would hang there instead of hearing which.
/// A deployment whose config exists asks nothing and needs no flags.
pub fn check_answers(clone: &Clone, opts: &Options, has_terminal: bool) -> Result<(), InitError> {
    let dir = deployment_dir(&opts.dir, opts.name.as_deref(), clone)?;
    if has_terminal || dir.join(CONFIG_FILE).symlink_metadata().is_ok() {
        return Ok(());
    }
    unanswered(&opts.answers).map_or(Ok(()), Err)
}

fn unanswered(answers: &Answers) -> Option<InitError> {
    let missing: Vec<&str> = [
        (answers.work.is_none(), "--work or --no-work"),
        (answers.deliver.is_none(), "--deliver or --no-deliver"),
        (answers.checks.is_none(), "--checks <commands> (\"\" for none)"),
    ]
    .into_iter()
    .filter_map(|(m, f)| m.then_some(f))
    .collect();
    (!missing.is_empty()).then(|| InitError::NoTerminal { flags: missing.join(", ") })
}

/// Steps 3 to 6 of #247: the directory, the questions, the config.
pub fn write(
    clone: &Clone,
    opts: &Options,
    prompt: Option<&mut dyn Prompt>,
) -> Result<Written, InitError> {
    let dir = deployment_dir(&opts.dir, opts.name.as_deref(), clone)?;
    let io = |source| InitError::Io { path: dir.clone(), source };
    match std::fs::create_dir(&dir) {
        Ok(()) => {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io(e)),
    }
    let config = dir.join(CONFIG_FILE);
    if config.symlink_metadata().is_ok() {
        return Ok(Written { dir, config, kept: true });
    }

    let (work, deliver, checks) = ask(clone, &opts.answers, prompt)?;
    let (api, mcp) = free_pair(&opts.dir, opts.port_free)?;
    let string = |s: &str| toml::Value::String(s.to_string()).to_string();
    let text = render(&[
        ("owner", string(&clone.owner)),
        ("repo", string(&clone.repo)),
        ("github_app", string(&opts.app_file.display().to_string())),
        ("workspace_repo", string(&clone.path.display().to_string())),
        ("base", string(&clone.base)),
        ("worker_kind", string(if work { "claude" } else { "fake" })),
        ("api_bind", string(&format!("127.0.0.1:{api}"))),
        ("mcp_bind", string(&format!("127.0.0.1:{mcp}"))),
        ("delivery_enabled", deliver.to_string()),
        ("gate_commands", toml_array(&checks)),
    ])?;
    write_new(&config, text.as_bytes())?;
    Ok(Written { dir, config, kept: false })
}

fn ask(
    clone: &Clone,
    answers: &Answers,
    prompt: Option<&mut dyn Prompt>,
) -> Result<(bool, bool, Vec<Vec<String>>), InitError> {
    let Some(prompt) = prompt else {
        return match (answers.work, answers.deliver, &answers.checks) {
            (Some(w), Some(d), Some(c)) => Ok((w, d, parse_checks(c))),
            _ => Err(unanswered(answers).expect("one of the three is None")),
        };
    };
    let io = |source| InitError::Io { path: PathBuf::from("<stdin>"), source };
    let work = match answers.work {
        Some(w) => w,
        None => prompt.yes_no("Let agents work issues?").map_err(io)?,
    };
    let deliver = match answers.deliver {
        Some(d) => d,
        None => prompt.yes_no("Open pull requests?").map_err(io)?,
    };
    // The suggestion is shown, never taken on Enter: an empty answer is no commands (#247).
    let checks = match (&answers.checks, suggest_checks(&clone.path)) {
        (Some(c), _) => c.clone(),
        (None, suggestion) => {
            let hint =
                suggestion.map(|s| format!(" This clone suggests `{s}`.")).unwrap_or_default();
            prompt
                .line(&format!(
                    "The gate's check commands, `&&` between two, empty for none.{hint}"
                ))
                .map_err(io)?
        }
    };
    Ok((work, deliver, parse_checks(&checks)))
}

/// One pass over [`TEMPLATE`], each `{{key}}` replaced by its already-encoded TOML value. Never a
/// `replace` per key: that rescans what earlier keys inserted, so a clone at
/// `/repos/{{delivery_enabled}}/api` would be written as `/repos/true/api`. Strings arrive
/// quoted and escaped, so a path with a quote in it cannot end the string and become a key.
fn render(values: &[(&str, String)]) -> Result<String, InitError> {
    let mut out = String::with_capacity(TEMPLATE.len());
    let mut rest = TEMPLATE;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| InitError::Template("an unclosed `{{`".into()))?;
        let key = &after[..end];
        let value = values
            .iter()
            .find(|(k, _)| *k == key)
            .ok_or_else(|| InitError::Template(format!("no value for `{{{{{key}}}}}`")))?;
        out.push_str(&value.1);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn toml_array(commands: &[Vec<String>]) -> String {
    let argv = |c: &Vec<String>| {
        toml::Value::Array(c.iter().cloned().map(toml::Value::String).collect()).to_string()
    };
    format!("[{}]", commands.iter().map(argv).collect::<Vec<_>>().join(", "))
}

/// The first loopback pair that is free now and that no other deployment's config names: a
/// deployment that is stopped today still binds its pair when it starts.
fn free_pair(root: &Path, port_free: &dyn Fn(u16) -> bool) -> Result<(u16, u16), InitError> {
    let named = named_ports(root);
    let usable = |p: u16| port_free(p) && !named.contains(&p);
    (0..PORT_PAIRS)
        .map(|i| (FIRST_PORT + 2 * i, FIRST_PORT + 2 * i + 1))
        .find(|&(a, b)| usable(a) && usable(b))
        .ok_or(InitError::NoFreePorts { from: FIRST_PORT })
}

/// Ports in sibling deployments' configs, read through [`ApiConfig`] so an omitted `bind` counts
/// as the default it binds. Both are reserved even with a listener off: turning it on later is
/// one line, and that line must not collide. One that does not parse names none: it cannot
/// start either, and init is not the place to report it.
fn named_ports(root: &Path) -> Vec<u16> {
    let Ok(entries) = std::fs::read_dir(root) else { return vec![] };
    let mut ports = Vec::new();
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path().join(CONFIG_FILE)) else { continue };
        let Ok(value) = text.parse::<toml::Table>() else { continue };
        let api = match value.get("api").cloned() {
            Some(table) => match table.try_into::<ApiConfig>() {
                Ok(api) => api,
                Err(_) => continue,
            },
            None => ApiConfig::default(),
        };
        for addr in [&api.bind, &api.mcp_bind] {
            ports.extend(addr.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()));
        }
    }
    ports
}

/// The dispatch label and a `state:<name>` label for every state the config names. `open` and
/// `closed` are the issue's own flag and have no label (`GithubTracker::set_state`).
pub fn labels_for(cfg: &Config) -> Vec<String> {
    let states = cfg.known_states().into_iter().filter(|s| s != "open" && s != "closed");
    cfg.tracker
        .dispatch_label
        .clone()
        .into_iter()
        .chain(states.map(|s| format!("state:{s}")))
        .collect()
}

/// A kept config may name another repository, when `--name` reuses a deployment made from
/// another clone: its labels would land there although this clone's installation was checked.
pub fn check_kept(config: &Path, cfg: &Config, clone: &Clone) -> Result<(), InitError> {
    let same = cfg.tracker.owner.eq_ignore_ascii_case(&clone.owner)
        && cfg.tracker.repo.eq_ignore_ascii_case(&clone.repo);
    if same {
        return Ok(());
    }
    Err(InitError::OtherRepo {
        config: config.to_path_buf(),
        named: format!("{}/{}", cfg.tracker.owner, cfg.tracker.repo),
        clone: format!("{}/{}", clone.owner, clone.repo),
    })
}

/// The deployment's config when it already exists, loaded and checked against this clone. Read
/// before the App is checked, because a kept config names the App its labels are written as,
/// which need not be the default settings file.
pub fn kept(clone: &Clone, opts: &Options) -> Result<Option<Config>, InitError> {
    let config = deployment_dir(&opts.dir, opts.name.as_deref(), clone)?.join(CONFIG_FILE);
    if config.symlink_metadata().is_err() {
        return Ok(None);
    }
    let cfg = Config::load(&config)
        .map_err(|e| InitError::KeptConfig { config: config.clone(), detail: e.to_string() })?;
    check_kept(&config, &cfg, clone)?;
    Ok(Some(cfg))
}

/// Step 7 of #247, through the configured credential. The first failure stops it: the labels
/// before it are made and a re-run keeps them.
pub fn create_labels(
    labels: &dyn RepoLabels,
    cfg: &Config,
) -> Result<Vec<(String, LabelOutcome)>, InitError> {
    labels_for(cfg)
        .into_iter()
        .map(|name| match labels.ensure_label(&name) {
            Ok(outcome) => Ok((name, outcome)),
            Err(e) => Err(InitError::Label { name, detail: e.to_string() }),
        })
        .collect()
}

/// The command that runs this deployment as a login service (#246), which also starts it.
pub fn next_command(dir: &Path) -> String {
    format!(
        "crewd service install --config {}",
        shell_quote(&dir.join(CONFIG_FILE).display().to_string())
    )
}

/// Single-quoted for a POSIX shell, so a space or a `;` in `--name` stays part of the path.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Step 8 of #247: what was kept, the switches still off with the line that turns each on, the
/// ports, and the command to run next. Read from the config on disk, so a kept config is
/// described as it is rather than as this run would have written it.
pub fn summary(written: &Written, cfg: &Config, labels: &[(String, LabelOutcome)]) -> String {
    let mut out = format!("Deployment {}\n", written.dir.display());
    let kept = |k: bool| if k { "kept: it already existed" } else { "written" };
    out += &format!("  config: {} ({})\n", written.config.display(), kept(written.kept));
    for (name, outcome) in labels {
        let what = match outcome {
            LabelOutcome::Created => "created",
            LabelOutcome::Kept => "kept: it already existed",
        };
        out += &format!("  label:  {name} ({what})\n");
    }
    out += &format!("  ops API: {}, MCP: {}\n", cfg.api.bind, cfg.api.mcp_bind);

    let real_worker =
        cfg.workers().iter().any(|w| matches!(w.kind(), Ok(WorkerKind::Claude | WorkerKind::Grok)));
    // The table to edit and the line to put in it, never `[table] key = value`: that is not
    // TOML, and a second header for a table the file already has does not load either.
    let mut off = Vec::new();
    // `[worker]` beside `[[workers]]` does not load, so a kept config with the list is pointed
    // at its entries.
    if !real_worker {
        let table = if cfg.workers.is_empty() { "[worker]" } else { "a [[workers]] entry" };
        off.push(("Agents do not work issues", table, "kind = \"claude\""));
    }
    if !cfg.delivery.enabled {
        off.push(("No pull requests are opened", "[delivery]", "enabled = true"));
    }
    if cfg.gate.enabled && cfg.gate.commands.is_empty() {
        off.push((
            "The gate only brings branches onto the base",
            "[gate]",
            "commands = [[\"<program>\", \"<arg>\"]]   # this repository's own checks",
        ));
    }
    if !off.is_empty() {
        out += &format!("\nStill off, each turned on by one line in {CONFIG_FILE}:\n");
        for (what, table, line) in off {
            out += &format!("  {what}: under {table}, set\n    {line}\n");
        }
    }
    out += &format!("\nNext:\n  {}\n", next_command(&written.dir));
    if let Some(label) = &cfg.tracker.dispatch_label {
        out += &format!("then label an issue `{label}`.\n");
    }
    out
}
