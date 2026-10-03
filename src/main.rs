//! crewd entry point.
//!
//! Headless is the default; `--tui` opts into the dashboard. That asymmetry is deliberate —
//! it keeps the UI a client of the same snapshot an operator could curl, rather than a
//! privileged view that correctness quietly depends on. `--api` makes the curl literal: the
//! same published snapshot, over HTTP, with no second path to the store.
//!
//! The loop below is the one place that owns the `Scheduler`, which is why both operator
//! surfaces reach it the same way — a message on a channel, never a handle. The dashboard's
//! messages are fire-and-forget; the API's carry a `oneshot` to answer on, because an HTTP
//! client is owed a response and a keypress is not.
//!
//! This binary has no `status`: asking a running daemon what it is doing is `crewctl`, a separate
//! package that links no store, worktree or tracker code (#45), because an operator asking what
//! is running must not be able to disturb it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use crew::api::mcp::OpsMcp;
use crew::api::{Api, Command};
use crew::broker::fake::FakeWrites;
use crew::broker::{self, Broker, BrokerLimits, TrackerWrites};
use crew::clock::{Clock, SystemClock};
use crew::config::{Config, TrackerKind, WorkerConfig, WorkerKind};
use crew::credentials::{Credentials, GithubApp, GithubAppFile, JiraCredentialsFile, StaticToken};
use crew::forge::fake::FakeForge;
use crew::forge::github::GithubForge;
use crew::forge::{Forge, Publisher};
use crew::gate::{Gate, GitGate};
use crew::http::UreqHttp;
use crew::init;
use crew::project::{NoopProjector, Projector, TasksProjector, derive_session_id};
use crew::sched::{Scheduler, Snapshot, WorkerPool};
use crew::service;
use crew::store::{Store, StoreLock};
use crew::tracker::Tracker;
use crew::tracker::fake::FakeTracker;
use crew::tracker::github::{DispatchRule, GithubTracker};
use crew::tracker::jira::JiraTracker;
use crew::transcript::Transcripts;
use crew::tui::{Ui, UiAction};
use crew::worker::Worker;
use crew::worker::claude::{ClaudeWorker, DEFAULT_ENV_ALLOWLIST};
use crew::worker::fake::{FakeWorker, Script};
use crew::worker::grok::GrokWorker;
use crew::worker::resolve::resolve_bin;
use crew::workspace::{FetchLock, GitWorktreeWorkspace};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

#[derive(Parser, Debug)]
#[command(name = "crewd", about = "Tracker-driven orchestrator for coding agents")]
struct Args {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Path to the TOML config.
    #[arg(short, long, default_value = "crew.toml")]
    config: PathBuf,

    /// Show the terminal dashboard. Without it the service runs headless and logs.
    #[arg(long)]
    tui: bool,

    /// Stop after this many ticks. Useful for smoke tests in CI.
    #[arg(long)]
    max_ticks: Option<u64>,

    /// Serve the ops HTTP API on this address, overriding `[api]` in the config. A
    /// non-loopback address still needs `api.allow_public`.
    #[arg(long, value_name = "ADDR")]
    api: Option<String>,

    /// Serve the ops API as MCP tools on this address, for the agent supervising this daemon,
    /// overriding `[api] mcp_bind`. Loopback unless `api.allow_public`. Never hand this
    /// address to a dispatched worker — see `api::mcp`.
    #[arg(long, value_name = "ADDR")]
    mcp: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Register this operator's own GitHub App and write ~/.crewd/github-app.toml naming it.
    /// Two clicks in a browser — create, install — and nothing typed.
    Init {
        /// The App's name. Defaults to `crew-<your GitHub login>`; GitHub requires it to be
        /// unique across all of GitHub.
        #[arg(long)]
        app_name: Option<String>,
        /// Register the App under this organization rather than your own account.
        #[arg(long)]
        org: Option<String>,
        /// Where to write the settings file and key. Defaults to `~/.crewd`.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
    /// Run one deployment as a per-user service (launchd on macOS, systemd `--user` on Linux)
    /// that starts at login and restarts after a crash.
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
}

#[derive(Subcommand, Debug)]
enum ServiceCmd {
    /// Write the service definition for the deployment this config sits in, then load and
    /// start it. The config must load; the service runs in its directory.
    Install {
        /// The deployment's config. Its directory names the service and is its working
        /// directory.
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Stop and remove the service for the deployment this config sits in.
    Uninstall {
        #[arg(short, long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // In TUI mode the alternate screen owns stdout, so logs go to stderr only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "crew=info".into()),
        )
        .init();

    // Before the config is loaded: `init` is what produces the file a config names, so it must
    // not need one to exist.
    if let Some(Cmd::Init { app_name, org, dir }) = args.command {
        return tokio::task::spawn_blocking(move || run_init(app_name, org, dir)).await?;
    }
    if let Some(Cmd::Service { action }) = args.command {
        return tokio::task::spawn_blocking(move || run_service(action)).await?;
    }

    let cfg = Config::load(&args.config)
        .with_context(|| format!("loading config from {}", args.config.display()))?;

    // A component the config turns on that cannot start stops startup by name: a daemon up with
    // part of itself missing looks healthy to its operator (#218). The workers and the ops
    // listeners are checked here, before the store or any worktree is opened; only the task
    // projection below keeps its degrade.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    // `Config::load` ran preflight, which parses it; matching on the enum rather than a string
    // comparison is what keeps a new kind from falling through to the fake (#69).
    let tracker_kind = cfg.tracker.kind()?;
    let mut pools = Vec::new();
    for w in cfg.workers() {
        let kind = w.kind()?;
        pools.push(WorkerPool {
            name: w.name(),
            worker: build_worker(&w, kind, &cfg, &clock, tracker_kind)?,
            max_concurrent: w.max_concurrent.unwrap_or(cfg.agent.max_concurrent),
        });
    }
    // `--api` and `--mcp` override `[api]` rather than being a second source of truth.
    let mut api_cfg = cfg.api.clone();
    if let Some(addr) = args.api.clone() {
        api_cfg.enabled = true;
        api_cfg.bind = addr;
    }
    if let Some(addr) = args.mcp.clone() {
        api_cfg.mcp_enabled = true;
        api_cfg.mcp_bind = addr;
    }
    let api_listener = if api_cfg.enabled { Some(crew::api::bind(&api_cfg).await?) } else { None };
    let mcp_listener =
        if api_cfg.mcp_enabled { Some(crew::api::mcp::bind(&api_cfg)?) } else { None };

    // One client for the whole process (#150): `SSL_CERT_FILE` is read once, here, so a bundle
    // that cannot be read stops startup instead of failing the first poll as an opaque transport
    // error, and the tracker, the forge and the GitHub App below share one connection pool and
    // one root-certificate set rather than each reading the environment for their own.
    let http = UreqHttp::from_env().context("building the HTTPS client")?;

    let db_path = crew::config::store_path(&args.config, std::env::var_os("CREW_DB"));
    // Before the store is opened and before the first tick's `recover`. A second process on
    // this file must exit here rather than release claims a live daemon still holds (#217).
    let _store_lock = StoreLock::acquire(&db_path)?;
    let store = Store::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    let ws_root =
        cfg.workspace.root.clone().unwrap_or_else(|| std::env::temp_dir().join("crew_workspaces"));
    let repo = cfg.workspace.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    // A Jira dry run with delivery off must not need a GitHub App on disk: delivery is the
    // forge's only consumer. A GitHub tracker needs this credential for its own reads (#99).
    let needs_forge_credential = tracker_kind == TrackerKind::Github
        || (tracker_kind != TrackerKind::Fake && cfg.delivery.enabled);
    // One source for a GitHub tracker, the forge and the push, so they mint one installation
    // token between them rather than one each. `None` is the `GITHUB_TOKEN` path, and there the
    // push rides the operator's ambient git credential exactly as before. It is the forge's
    // credential, not the tracker's: a Jira tracker delivers to GitHub only while `[delivery]`
    // is on (#99).
    let app: Option<Arc<dyn Credentials>> = match cfg.forge_github_app() {
        Some(path) if needs_forge_credential => {
            let key = if cfg.forge.github_app.is_some() {
                "forge.github_app"
            } else {
                "tracker.github_app"
            };
            let file = GithubAppFile::load(path)
                .with_context(|| format!("reading {key} {}", path.display()))?;
            tracing::info!(
                app_id = file.app_id,
                installation_id = file.installation_id,
                "writes are authored by the GitHub App, not the operator"
            );
            Some(Arc::new(GithubApp::new(http.clone(), &file, clock.clone())?))
        }
        _ => None,
    };
    // The repository's canonical HTTPS URL, not the remote's: a `pushurl` or an SSH alias there
    // would send the push out on the operator's key (#64). The gate fetches its base from it too.
    let app_url = format!("https://github.com/{}/{}.git", cfg.forge_owner(), cfg.forge_repo());
    let mut workspace = GitWorktreeWorkspace::new(&ws_root, &repo)?;
    if let Some(app) = &app {
        workspace = workspace.with_push_credentials(app.clone(), app_url.clone());
    }
    // A new branch starts where the gate will measure it (#170), so both fetch under one lock.
    let fetch_lock = FetchLock::default();
    if let Some(base) = cfg.gate_base() {
        let remote = cfg.delivery.enabled.then(|| cfg.delivery.remote.clone());
        workspace = workspace.branching_from(base, remote, fetch_lock.clone());
    }
    let workspace = Arc::new(workspace);

    let tasks_root = std::env::var_os("CREW_TASKS_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/tasks")))
        .unwrap_or_else(|| PathBuf::from(".claude/tasks"));
    // The session id is derived from the canonical workspace root rather than generated fresh,
    // so a restart of the same deployment updates one directory instead of littering a new one
    // on every process start.
    let session_id = derive_session_id(&workspace.root().display().to_string());
    let projector: Arc<dyn Projector> = match TasksProjector::new(&tasks_root, &session_id) {
        Ok(p) => Arc::new(p),
        Err(e) => {
            tracing::warn!(error = %e, "task projection setup failed; running without it");
            Arc::new(NoopProjector)
        }
    };

    let real_worker = cfg
        .workers()
        .iter()
        .any(|w| matches!(w.kind().ok(), Some(WorkerKind::Claude | WorkerKind::Grok)));

    // One adapter, two traits: a real tracker reads for the scheduler and writes for the broker
    // over the same credential, which never leaves this process either way. A non-GitHub
    // tracker with delivery off builds no forge (#99).
    let (tracker, writes, forge): TrackerSet = match tracker_kind {
        TrackerKind::Github => {
            let creds = github_credentials(&app)?;
            let rule = DispatchRule {
                label: cfg.tracker.dispatch_label.clone(),
                assignee: cfg.tracker.assignee.clone(),
            };
            let gh = Arc::new(
                GithubTracker::new(
                    http.clone(),
                    &cfg.tracker.owner,
                    &cfg.tracker.repo,
                    "",
                    &cfg.tracker.required_labels,
                )
                .with_credentials(creds.clone())
                .with_dispatch_rule(rule),
            );
            // `preflight` refuses a `[forge]` that names anything else when `tracker.kind =
            // "github"`, so the tracker and the forge always share this one credential.
            let forge: Arc<dyn Forge> = Arc::new(
                GithubForge::new(http.clone(), cfg.forge_owner(), cfg.forge_repo(), "")
                    .with_credentials(creds),
            );
            (gh.clone(), gh, Some(forge))
        }
        TrackerKind::Jira => build_jira(&cfg, &app, &http)?,
        TrackerKind::Fake => {
            // The demo tracker has nothing to write to, so broker calls are recorded and
            // dropped. That still exercises the whole path — scoping, budgets, audit —
            // without a network. The fake forge likewise: green CI, reviewers that attach.
            let forge: Arc<dyn Forge> = Arc::new(FakeForge::new());
            (Arc::new(FakeTracker::demo()), Arc::new(FakeWrites::new()), Some(forge))
        }
    };

    // A root that cannot be created stops startup (#218): a daemon whose runs leave no record is
    // missing what its config turned on. Defaults beside the worktrees rather than inside one —
    // see `TranscriptsConfig`.
    let transcripts = if cfg.transcripts.enabled {
        let root = cfg.transcripts.root_in(&ws_root);
        let t =
            Transcripts::new(&root, cfg.transcripts.max_bytes_per_run, cfg.transcripts.keep_runs)
                .with_context(|| format!("creating the transcript root {}", root.display()))?;
        tracing::info!(root = %root.display(), keep = cfg.transcripts.keep_runs, "recording run transcripts");
        Some(t)
    } else {
        tracing::info!("transcripts disabled by config; runs will leave no record on disk");
        None
    };

    let broker = if cfg.broker.enabled {
        Some(start_broker(&cfg, writes, clock.clone())?)
    } else {
        tracing::info!("broker disabled by config; agents run without tracker tools");
        None
    };

    if real_worker && tracker_kind != TrackerKind::Fake {
        tracing::warn!(
            "real tracker + real worker: this run will dispatch actual coding agents against \
             real issues and let them commit to real worktrees"
        );
    }

    tracing::info!(
        config = %args.config.display(),
        db = %db_path.display(),
        workspaces = %ws_root.display(),
        tracker = %cfg.tracker.kind,
        limit = cfg.capacity(),
        workers = ?pools.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        "starting"
    );

    let interval_ms = cfg.polling.interval_ms;

    // The handoff gate runs in the run's worktree against `repo`'s base, so it is built over
    // the same repository the worktrees come from. With no gate
    // a `Done` is handed to a human exactly as the agent left it, which is issue #21.
    let gate: Option<Arc<dyn Gate>> = if cfg.gate.enabled {
        let repo = repo.canonicalize().with_context(|| format!("resolving {}", repo.display()))?;
        tracing::info!(
            base = cfg.gate_base().as_deref().unwrap_or("HEAD"),
            commands = cfg.gate.commands.len(),
            "handoff gate on: done runs are brought onto the base and re-gated before release"
        );
        let mut gate = GitGate::new(repo, cfg.gate_base(), cfg.gate.commands.clone())
            .with_fetch_lock(fetch_lock.clone());
        // With delivery on, the base is the remote's: the one the pull request merges into. It
        // is fetched as the push is made, so a host with no ambient credential can do both.
        if cfg.delivery.enabled {
            gate = gate.with_remote(cfg.delivery.remote.clone());
            if let Some(app) = &app {
                gate = gate.with_fetch_credentials(app.clone(), app_url.clone());
            }
        }
        Some(Arc::new(gate))
    } else {
        tracing::warn!("handoff gate off: done runs are released as the agent left them");
        None
    };
    // Attached whenever the config asks, and the real git worktree is always the publisher:
    // there is no fake half here, because the branch that gets pushed is a real one. Every
    // tracker arm above builds a forge whenever delivery is on.
    let delivery = forge.filter(|_| cfg.delivery.enabled).map(|forge| {
        tracing::info!(
            base = %cfg.delivery.base, remote = %cfg.delivery.remote, reviewers = ?cfg.delivery.reviewers,
            rounds_per_pr = cfg.delivery.max_rounds_per_pr, rounds_per_issue = cfg.delivery.max_rounds_per_issue,
            "delivery on: finished runs will be pushed and opened as pull requests"
        );
        let publisher: Arc<dyn Publisher> = workspace.clone();
        (forge, publisher)
    });

    let first = pools[0].worker.clone();
    let mut sched = Scheduler::new(cfg, clock.clone(), store, tracker, first, workspace, projector);
    sched.set_workers(pools);
    sched.set_broker(broker);
    sched.set_transcripts(transcripts);
    sched.set_gate(gate);
    if let Some((forge, publisher)) = delivery {
        sched
            .set_delivery(Some(forge), Some(publisher))
            .context("delivery on: could not learn the login the forge posts as")?;
    }

    let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
    let (act_tx, mut act_rx) = mpsc::unbounded_channel::<UiAction>();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();

    // Publish once before anything can observe, so the window between start and the first tick
    // shows the scheduler's real state rather than `Snapshot::default()` — whose `running 0/0`
    // reports a concurrency limit this process never had.
    let _ = snap_tx.send(sched.snapshot()?);

    if let Some(listener) = api_listener {
        tokio::spawn(Api::new(snap_rx.clone(), cmd_tx.clone()).serve(listener));
    }

    // The same surface as tools, on its own listener, bound at startup like the HTTP API's. It
    // is served by the broker's transport but is deliberately *not* the broker — nothing here
    // passes it to `Broker`, and the `--mcp-config` crewd gives a worker is written by
    // `Broker::open` alone, so crewd never hands a dispatched agent this address
    // (`a_dispatched_worker_is_not_handed_the_ops_tools`). `--strict-mcp-config` keeps the
    // operator's own registration from loading too (#191); the loopback HTTP API remains
    // (#135). See `api::mcp`.
    if let Some(listener) = mcp_listener {
        let addr = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
        let ops = Arc::new(OpsMcp::new(Api::new(snap_rx.clone(), cmd_tx.clone())));
        broker::server::serve(ops, listener).context("starting the ops MCP server")?;
        tracing::info!(%addr, path = crew::api::mcp::PATH, "ops MCP server listening");
    }

    let ui = args.tui.then(|| {
        let rx = snap_rx.clone();
        let tx = act_tx.clone();
        std::thread::spawn(move || Ui::new(rx, tx).run())
    });

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // While the dashboard is up, refresh it faster than the poll interval so in-flight
    // progress animates. This drives rendering only; dispatch still happens on the tick.
    let mut repaint = tokio::time::interval(std::time::Duration::from_millis(250));

    // Created once, before the loop: a listener built per pass exists only while `select!` is
    // parked, so an interrupt that lands during a tick was dropped (#215). SIGTERM takes the same
    // path so `kill` and a supervisor's stop reach `sched.shutdown()` rather than orphan workers.
    let mut sigint = signal(SignalKind::interrupt()).context("listening for SIGINT")?;
    let mut sigterm = signal(SignalKind::terminate()).context("listening for SIGTERM")?;

    let mut ticks = 0u64;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Err(e) = sched.tick() {
                    tracing::error!(error = %e, "tick failed");
                }
                ticks += 1;
                let _ = snap_tx.send(sched.snapshot()?);
                if args.max_ticks.is_some_and(|m| ticks >= m) {
                    tracing::info!(ticks, "max ticks reached; shutting down");
                    break;
                }
            }
            _ = repaint.tick(), if args.tui => {
                let _ = snap_tx.send(sched.snapshot()?);
            }
            Some(action) = act_rx.recv() => {
                match action {
                    UiAction::Quit => break,
                    UiAction::ForceTick => {
                        if let Err(e) = sched.tick() { tracing::error!(error = %e, "forced tick failed"); }
                        let _ = snap_tx.send(sched.snapshot()?);
                    }
                    UiAction::Unquarantine(id) => {
                        let cleared = sched.unquarantine(&id)?;
                        tracing::info!(issue_id = %id, cleared, "operator cleared quarantine");
                        let _ = snap_tx.send(sched.snapshot()?);
                    }
                    // Unlike unquarantine, an unblock reads the tracker, so it can fail on an
                    // outage; a keypress must not take the daemon down with it.
                    UiAction::Unblock(id) => {
                        match sched.unblock(&id) {
                            Ok(done) => tracing::info!(issue_id = %id, unblocked = ?done, "operator unblocked"),
                            Err(e) => tracing::error!(issue_id = %id, error = %e, "unblock failed; the park or handoff is kept"),
                        }
                        let _ = snap_tx.send(sched.snapshot()?);
                    }
                }
            }
            Some(command) = cmd_rx.recv() => {
                // Every arm publishes before it answers, so a client that reads
                // `GET /snapshot` the instant its POST returns sees the effect it asked for.
                // A dropped reply channel is an ordinary outcome, not an error: it means the
                // client went away, and nothing here waits to find out.
                match command {
                    Command::Tick(reply) => {
                        if let Err(e) = sched.tick() { tracing::error!(error = %e, "api tick failed"); }
                        let snap = sched.snapshot();
                        if let Ok(s) = &snap { let _ = snap_tx.send(s.clone()); }
                        let _ = reply.send(snap);
                    }
                    Command::Unquarantine { issue_id, reply } => {
                        let cleared = sched.unquarantine(&issue_id);
                        if let Ok(c) = &cleared {
                            tracing::info!(issue_id = %issue_id, cleared = c, "api cleared quarantine");
                        }
                        if let Ok(s) = sched.snapshot() { let _ = snap_tx.send(s); }
                        let _ = reply.send(cleared);
                    }
                    Command::Unblock { issue_id, reply } => {
                        let cleared = sched.unblock(&issue_id);
                        if let Ok(done) = &cleared {
                            tracing::info!(issue_id = %issue_id, unblocked = ?done, "api unblocked");
                        }
                        if let Ok(s) = sched.snapshot() { let _ = snap_tx.send(s); }
                        let _ = reply.send(cleared);
                    }
                }
            }
            _ = sigint.recv() => {
                tracing::info!(signal = "SIGINT", "interrupt received; shutting down");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!(signal = "SIGTERM", "interrupt received; shutting down");
                break;
            }
        }
    }

    // A RunHandle outlives the Scheduler unless something kills it explicitly; exiting with
    // runs still in flight would otherwise orphan real worker processes.
    if let Err(e) = sched.shutdown() {
        tracing::error!(error = %e, "shutdown cleanup failed");
    }

    if let Some(h) = ui {
        let _ = h.join();
    }
    Ok(())
}

/// The forge credential: the GitHub App already minted above, or a `GITHUB_TOKEN` static token.
///
/// Split out because a Jira tracker needs the same forge credential for its own delivery path,
/// not a second way to build one.
fn github_credentials(app: &Option<Arc<dyn Credentials>>) -> anyhow::Result<Arc<dyn Credentials>> {
    match app {
        Some(app) => Ok(app.clone()),
        None => Ok(Arc::new(StaticToken::new(&std::env::var("GITHUB_TOKEN").context(
            "GITHUB_TOKEN must be set when tracker.kind is not \"fake\" and no forge.github_app \
             (or tracker.github_app) is configured",
        )?))),
    }
}

/// What every `TrackerKind` arm in `main` builds: the tracker, its broker writes, and the forge
/// (`None` for a non-GitHub tracker with delivery off).
type TrackerSet = (Arc<dyn Tracker>, Arc<dyn TrackerWrites>, Option<Arc<dyn Forge>>);

/// The Jira tracker and, only while `[delivery]` is on, the GitHub forge it delivers through,
/// which `[forge]` names (#99). With delivery off nothing here reads `[forge]`, so a dry run with
/// the fake worker needs no GitHub App, token or repository. `http` is the one client `main`
/// built from the environment (#150), shared rather than rebuilt per tracker.
fn build_jira(
    cfg: &Config,
    app: &Option<Arc<dyn Credentials>>,
    http: &UreqHttp,
) -> anyhow::Result<TrackerSet> {
    let jira = cfg
        .tracker
        .jira
        .as_ref()
        .context("[tracker.jira] is required when tracker.kind = \"jira\"")?;
    let creds = match &jira.credentials {
        Some(path) => JiraCredentialsFile::load(path)
            .with_context(|| format!("reading tracker.jira.credentials {}", path.display()))?,
        None => JiraCredentialsFile::from_env().context(
            "set tracker.jira.credentials to a file naming email/api_token, or JIRA_EMAIL and \
             JIRA_API_TOKEN in the environment",
        )?,
    };
    let token: Arc<dyn Credentials> = Arc::new(StaticToken::new(&creds.basic_token()));
    tracing::info!(
        project = %jira.project, base_url = %jira.base_url,
        "polling Jira Cloud; writes are authored by the token's own account"
    );
    // Preflight already refused a Jira tracker with no dispatch_label, so this is always set.
    let dispatch_label = cfg.tracker.dispatch_label.clone().unwrap_or_default();
    let jira_tracker = Arc::new(
        JiraTracker::new(http.clone(), &jira.base_url, &jira.project, &dispatch_label)
            .with_credentials(token)
            .with_assigned_to_me(jira.assigned_to_me),
    );
    let forge = cfg
        .delivery
        .enabled
        .then(|| {
            anyhow::Ok(Arc::new(
                GithubForge::new(http.clone(), cfg.forge_owner(), cfg.forge_repo(), "")
                    .with_credentials(github_credentials(app)?),
            ) as Arc<dyn Forge>)
        })
        .transpose()?;
    Ok((jira_tracker.clone(), jira_tracker, forge))
}

/// Bind the broker's loopback listener and start serving.
///
/// Every failure here stops startup: with `broker.enabled` on, agents running without tracker
/// tools is a daemon with part of itself missing (#218).
fn start_broker(
    cfg: &Config,
    writes: Arc<dyn TrackerWrites>,
    clock: Arc<dyn Clock>,
) -> anyhow::Result<Arc<Broker>> {
    let listener = broker::server::bind().context("binding the tool broker")?;
    let addr = listener.local_addr().context("reading the tool broker's address")?;

    // Per-process, so two orchestrators on one host cannot collide or read each other's tokens.
    let config_dir = std::env::temp_dir().join(format!("crew-mcp-{}", std::process::id()));
    let limits = BrokerLimits {
        max_calls_per_run: cfg.broker.max_calls_per_run,
        max_calls_per_issue: cfg.broker.max_calls_per_issue,
    };

    let broker = Arc::new(
        Broker::new(writes, clock, limits, cfg.known_states(), addr, &config_dir)
            .context("setting up the tool broker")?,
    );
    broker::server::serve(Arc::clone(&broker), listener).context("starting the tool broker")?;
    tracing::info!(%addr, states = ?cfg.known_states(), "tool broker listening");
    Ok(broker)
}

/// The absolute path a real worker execs, resolved once at startup (#218). The worker is handed
/// this path rather than the configured name, because the child's environment is the allowlist
/// alone: a list without `PATH`, or a relative `bin` spawned from a worktree, would otherwise
/// pass this check and fail every dispatch.
fn worker_bin(w: &WorkerConfig, default: &str) -> anyhow::Result<PathBuf> {
    let bin = w.bin.as_deref().unwrap_or(default);
    resolve_bin(bin, std::env::var_os("PATH").as_deref()).with_context(|| {
        format!("worker {:?} cannot start: its bin {bin:?} does not resolve", w.name())
    })
}

/// One configured worker. Deliberately independent of the tracker: see `WorkerConfig`'s doc for
/// why a real tracker does not imply a real worker.
fn build_worker(
    w: &WorkerConfig,
    kind: WorkerKind,
    cfg: &Config,
    clock: &Arc<dyn Clock>,
    tracker_kind: TrackerKind,
) -> anyhow::Result<Arc<dyn Worker>> {
    let worker: Arc<dyn Worker> =
        match kind {
            WorkerKind::Claude => {
                let bin = worker_bin(w, "claude")?;
                // An operator-supplied list replaces the default outright rather than extending it,
                // so what reaches the child is exactly what the config says.
                let env_allowlist = w.env_allowlist.clone().unwrap_or_else(|| {
                    DEFAULT_ENV_ALLOWLIST.iter().map(|s| s.to_string()).collect()
                });
                Arc::new(
                    ClaudeWorker::new(bin, env_allowlist, cfg.agent.max_turns_per_session)
                        .with_model(w.model_choice())
                        .with_rate_limit_warn_utilization(cfg.agent.rate_limit_warn_utilization),
                )
            }
            WorkerKind::Grok => {
                let bin = worker_bin(w, "grok")?;
                // The same list Claude gets. Nothing is added: Grok's login lives in the keychain
                // the way Claude's does, and a tracker credential is still not a worker's to hold.
                let env_allowlist = w.env_allowlist.clone().unwrap_or_else(|| {
                    DEFAULT_ENV_ALLOWLIST.iter().map(|s| s.to_string()).collect()
                });
                Arc::new(
                    GrokWorker::new(bin, env_allowlist, cfg.agent.max_turns_per_session)
                        .with_model(w.model_choice()),
                )
            }
            WorkerKind::Fake => {
                let fake = Arc::new(FakeWorker::new(clock.clone()));
                // The demo scripts are keyed to FakeTracker::demo()'s own issue ids; pointless (and
                // silently ignored, since none of those ids would ever be dispatched) against a
                // real tracker's real ids.
                if tracker_kind == TrackerKind::Fake {
                    seed_demo_scripts(&fake);
                }
                fake
            }
        };
    Ok(worker)
}

/// Give the demo tracker a spread of behaviours so the dashboard shows every state worth
/// recognising: clean completions, work that continues, a hard failure, and a wedged agent.
fn seed_demo_scripts(w: &FakeWorker) {
    use crew::model::{ErrorClass, Outcome};

    w.set_default(Script::succeeds_in(12_000));
    w.script(
        "iss-002",
        Script::succeeds_in(20_000)
            .with_outcome(Outcome::Continue { why: "tests still failing".into() }),
    );
    w.script(
        "iss-003",
        Script::succeeds_in(9_000).with_outcome(Outcome::Failed {
            class: ErrorClass::AgentCrash,
            msg: "agent exited unexpectedly".into(),
        }),
    );
    w.script("iss-004", Script::succeeds_in(30_000));
    w.script(
        "iss-005",
        Script::succeeds_in(15_000)
            .with_outcome(Outcome::Blocked { why: "needs product decision".into() }),
    );
    w.script("iss-006", Script::stalls_after(6_000));
}

fn run_init(
    app_name: Option<String>,
    org: Option<String>,
    dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let dir = match dir {
        Some(d) => d,
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set; pass --dir")?)
            .join(".crewd"),
    };
    let app_name = match app_name {
        Some(n) => n,
        None => {
            init::manifest::default_app_name(github_login().as_deref(), &init::random_suffix()?)
        }
    };
    let opts = init::Options {
        dir,
        app_name,
        org,
        limits: broker::server::Limits::default(),
        // Ten minutes at three seconds a read: long enough to choose repositories, short enough
        // that an abandoned run ends.
        install_polls: 200,
    };
    // `init` runs before a config is loaded and on its own thread (#150), so it builds its own
    // client from the environment rather than sharing the daemon's.
    let http = UreqHttp::from_env().context("building the HTTPS client")?;
    let registered = init::run(&http, &SystemClock::new(), &mut TerminalOperator, &opts)?;
    println!(
        "\nApp {} (id {}) is installed (installation {}).\n  key:      {}\n  settings: {}\n\n\
         Name the settings from the daemon's config:\n\n  [tracker]\n  github_app = \"{}\"",
        registered.slug,
        registered.app_id,
        registered.installation_id,
        registered.key.display(),
        registered.settings.display(),
        registered.settings.display(),
    );
    Ok(())
}

fn run_service(action: ServiceCmd) -> anyhow::Result<()> {
    let platform = service::Platform::native()?;
    let manager = platform.manager();
    let host = service::Host::from_env().context("reading the installing environment")?;
    match action {
        ServiceCmd::Install { config } => {
            let done = service::install(manager.as_ref(), &config, &host, platform)?;
            let plan = &done.plan;
            let argv: Vec<_> = std::iter::once(plan.program.as_os_str())
                .chain(plan.args.iter().map(|a| a.as_os_str()))
                .map(|a| a.to_string_lossy())
                .collect();
            let env: Vec<_> = plan.environment.iter().map(|(k, _)| k.as_str()).collect();
            println!(
                "Installed and started {}.\n  definition: {}\n  runs:       {}\n  in:         {}\n  \
                 captured:   {} (from this shell; reinstall after changing them)\n  logs:       {}\n\n\
                 It starts at login and restarts after a crash; a clean shutdown stays down.\n\
                 A worker binary the captured PATH cannot resolve stops the daemon at startup, \
                 naming it, in the logs above.",
                plan.deployment.label,
                done.definition.display(),
                argv.join(" "),
                plan.deployment.dir.display(),
                env.join(", "),
                plan.logs(),
            );
            if platform == service::Platform::Systemd
                && let Some(advice) = service::linger_advice()
            {
                println!("\n{advice}");
            }
        }
        ServiceCmd::Uninstall { config } => {
            match service::uninstall(manager.as_ref(), &config, &host, platform)? {
                service::Uninstalled::Removed { label, definition } => {
                    println!("Stopped and removed {label} ({}).", definition.display());
                }
                service::Uninstalled::NeverInstalled { label, definition } => {
                    println!(
                        "{label} is not installed ({} does not exist); nothing to do.",
                        definition.display()
                    );
                }
            }
        }
    }
    Ok(())
}

/// The operator's login for the default App name, from `gh` if it is there. Nothing else is
/// asked for: the point of `init` is that a machine with no prior setup still types nothing.
fn github_login() -> Option<String> {
    let out = std::process::Command::new("gh").args(["api", "user", "--jq", ".login"]).output();
    let out = out.ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|l| !l.is_empty())
}

struct TerminalOperator;

impl init::Operator for TerminalOperator {
    fn show(&mut self, what: &str, url: &str) {
        println!("{what}:\n\n  {url}\n");
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        // Best-effort: the URL is printed either way, and a headless host has no browser.
        let _ = std::process::Command::new(opener)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    fn wait(&mut self) {
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}
