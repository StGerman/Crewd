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
use crew::credentials::{Credentials, GithubApp, GithubAppFile, StaticToken};
use crew::forge::fake::FakeForge;
use crew::forge::github::GithubForge;
use crew::forge::{Forge, Publisher};
use crew::gate::{Gate, GitGate};
use crew::init;
use crew::project::{NoopProjector, Projector, TasksProjector, derive_session_id};
use crew::sched::{Scheduler, Snapshot, WorkerPool};
use crew::store::Store;
use crew::tracker::Tracker;
use crew::tracker::fake::FakeTracker;
use crew::tracker::github::{DispatchRule, GithubTracker, UreqHttp};
use crew::transcript::Transcripts;
use crew::tui::{Ui, UiAction};
use crew::worker::Worker;
use crew::worker::claude::{ClaudeWorker, DEFAULT_ENV_ALLOWLIST};
use crew::worker::fake::{FakeWorker, Script};
use crew::workspace::GitWorktreeWorkspace;
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

    let cfg = Config::load(&args.config)
        .with_context(|| format!("loading config from {}", args.config.display()))?;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());

    let db_path =
        std::env::var("CREW_DB").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("crew.db"));
    let store = Store::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    let ws_root =
        cfg.workspace.root.clone().unwrap_or_else(|| std::env::temp_dir().join("crew_workspaces"));
    let repo = cfg.workspace.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    // One source for the tracker, the forge and the push, so they mint one installation token
    // between them rather than one each. `None` is the `GITHUB_TOKEN` path, and there the push
    // rides the operator's ambient git credential exactly as before.
    let app: Option<Arc<dyn Credentials>> = match &cfg.tracker.github_app {
        Some(path) if cfg.tracker.kind()? == TrackerKind::Github => {
            let file = GithubAppFile::load(path)
                .with_context(|| format!("reading tracker.github_app {}", path.display()))?;
            tracing::info!(
                app_id = file.app_id,
                installation_id = file.installation_id,
                "writes are authored by the GitHub App, not the operator"
            );
            Some(Arc::new(GithubApp::new(UreqHttp::default(), &file, clock.clone())?))
        }
        _ => None,
    };
    let mut workspace = GitWorktreeWorkspace::new(&ws_root, &repo)?;
    if let Some(app) = &app {
        // The repository's canonical HTTPS URL, not the remote's: a `pushurl` or an SSH alias
        // there would send the push out on the operator's key (#64).
        let url = format!("https://github.com/{}/{}.git", cfg.tracker.owner, cfg.tracker.repo);
        workspace = workspace.with_push_credentials(app.clone(), url);
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

    // `Config::load` ran preflight, which parses both; matching on the enums rather than a
    // string comparison is what keeps a new kind from falling through to the fake (#69).
    let tracker_kind = cfg.tracker.kind()?;
    let mut pools = Vec::new();
    for w in cfg.workers() {
        let kind = w.kind()?;
        pools.push(WorkerPool {
            name: w.name(),
            worker: build_worker(&w, kind, &cfg, &clock, tracker_kind),
            max_concurrent: w.max_concurrent.unwrap_or(cfg.agent.max_concurrent),
        });
    }
    let real_worker = cfg.workers().iter().any(|w| w.kind().ok() == Some(WorkerKind::Claude));

    // One adapter, two traits: the GitHub tracker reads for the scheduler and writes for the
    // broker over the same credential, which never leaves this process either way.
    let (tracker, writes, forge): (Arc<dyn Tracker>, Arc<dyn TrackerWrites>, Arc<dyn Forge>) =
        match tracker_kind {
            TrackerKind::Github => {
                let creds: Arc<dyn Credentials> = match &app {
                    Some(app) => app.clone(),
                    None => Arc::new(StaticToken::new(&std::env::var("GITHUB_TOKEN").context(
                        "GITHUB_TOKEN must be set when tracker.kind = \"github\" and no \
                         tracker.github_app is configured",
                    )?)),
                };
                let rule = DispatchRule {
                    label: cfg.tracker.dispatch_label.clone(),
                    assignee: cfg.tracker.assignee.clone(),
                };
                let gh = Arc::new(
                    GithubTracker::new(
                        UreqHttp::default(),
                        &cfg.tracker.owner,
                        &cfg.tracker.repo,
                        "",
                        &cfg.tracker.required_labels,
                    )
                    .with_credentials(creds.clone())
                    .with_dispatch_rule(rule),
                );
                // The forge is the same repository on GitHub, so the same credential; on any
                // other provider it would be a separate adapter with its own.
                let forge = Arc::new(
                    GithubForge::new(
                        UreqHttp::default(),
                        &cfg.tracker.owner,
                        &cfg.tracker.repo,
                        "",
                    )
                    .with_credentials(creds),
                );
                (gh.clone(), gh, forge)
            }
            TrackerKind::Fake => {
                // The demo tracker has nothing to write to, so broker calls are recorded and
                // dropped. That still exercises the whole path — scoping, budgets, audit —
                // without a network. The fake forge likewise: green CI, reviewers that attach.
                (
                    Arc::new(FakeTracker::demo()),
                    Arc::new(FakeWrites::new()),
                    Arc::new(FakeForge::new()),
                )
            }
        };

    // Best-effort, like the projector: a root that cannot be created costs post-mortems, not
    // dispatch. Defaults beside the worktrees rather than inside one — see `TranscriptsConfig`.
    let transcripts = if cfg.transcripts.enabled {
        let root = cfg.transcripts.root_in(&ws_root);
        match Transcripts::new(&root, cfg.transcripts.max_bytes_per_run, cfg.transcripts.keep_runs)
        {
            Ok(t) => {
                tracing::info!(root = %root.display(), keep = cfg.transcripts.keep_runs, "recording run transcripts");
                Some(t)
            }
            Err(e) => {
                tracing::warn!(root = %root.display(), error = %e, "transcript root unavailable; runs will leave no record on disk");
                None
            }
        }
    } else {
        tracing::info!("transcripts disabled by config; runs will leave no record on disk");
        None
    };

    let broker = if cfg.broker.enabled {
        start_broker(&cfg, writes, clock.clone())
    } else {
        tracing::info!("broker disabled by config; agents run without tracker tools");
        None
    };

    if real_worker && tracker_kind == TrackerKind::Github {
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

    // Read before the config moves into the scheduler; `--api` is an override of it, not a
    // second source of truth.
    let mut api_cfg = cfg.api.clone();
    if let Some(addr) = args.api.clone() {
        api_cfg.enabled = true;
        api_cfg.bind = addr;
    }
    if let Some(addr) = args.mcp.clone() {
        api_cfg.mcp_enabled = true;
        api_cfg.mcp_bind = addr;
    }

    // The handoff gate runs in the run's worktree against `repo`'s base, so it is built over
    // the same repository the worktrees come from. Not a degrade like the broker: with no gate
    // a `Done` is handed to a human exactly as the agent left it, which is issue #21.
    let gate: Option<Arc<dyn Gate>> = if cfg.gate.enabled {
        let repo = repo.canonicalize().with_context(|| format!("resolving {}", repo.display()))?;
        tracing::info!(
            base = cfg.gate.base.as_deref().unwrap_or("HEAD"),
            commands = cfg.gate.commands.len(),
            "handoff gate on: done runs are rebased and re-gated before release"
        );
        Some(Arc::new(GitGate::new(repo, cfg.gate.base.clone(), cfg.gate.commands.clone())))
    } else {
        tracing::warn!("handoff gate off: done runs are released as the agent left them");
        None
    };
    // Attached whenever the config asks, and the real git worktree is always the publisher:
    // there is no fake half here, because the branch that gets pushed is a real one.
    let delivery = cfg.delivery.enabled.then(|| {
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
        sched.set_delivery(Some(forge), Some(publisher));
    }

    let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
    let (act_tx, mut act_rx) = mpsc::unbounded_channel::<UiAction>();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();

    // Publish once before anything can observe, so the window between start and the first tick
    // shows the scheduler's real state rather than `Snapshot::default()` — whose `running 0/0`
    // reports a concurrency limit this process never had.
    let _ = snap_tx.send(sched.snapshot()?);

    // Best-effort by contract: the API failing to start costs the API. Dispatch is not the
    // scheduler's opinion of whether a port was free.
    if api_cfg.enabled {
        match crew::api::bind(&api_cfg).await {
            Ok(listener) => {
                tokio::spawn(Api::new(snap_rx.clone(), cmd_tx.clone()).serve(listener));
            }
            Err(e) => tracing::error!(error = %e, "ops API not started; scheduling continues"),
        }
    }

    // The same surface as tools, on its own listener. Same contract as the HTTP API above: a
    // bind failure costs this server, never a dispatch. It is served by the broker's transport
    // but is deliberately *not* the broker — nothing here passes it to `Broker`, and the
    // `--mcp-config` crewd gives a worker is written by `Broker::open` alone, so crewd never
    // hands a dispatched agent this address (`a_dispatched_worker_is_not_handed_the_ops_tools`).
    // A worker can still inherit it from the operator's own MCP config; see `api::mcp`.
    if api_cfg.mcp_enabled {
        match crew::api::mcp::bind(&api_cfg) {
            Ok(listener) => {
                let addr = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
                let ops = Arc::new(OpsMcp::new(Api::new(snap_rx.clone(), cmd_tx.clone())));
                broker::server::serve(ops, listener);
                tracing::info!(%addr, path = crew::api::mcp::PATH, "ops MCP server listening");
            }
            Err(e) => {
                tracing::error!(error = %e, "ops MCP server not started; scheduling continues")
            }
        }
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
                            Ok(cleared) => tracing::info!(issue_id = %id, cleared, "operator lifted a park"),
                            Err(e) => tracing::error!(issue_id = %id, error = %e, "unblock failed; the park is kept"),
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
                        if let Ok(c) = &cleared {
                            tracing::info!(issue_id = %issue_id, cleared = c, "api lifted a park");
                        }
                        if let Ok(s) = sched.snapshot() { let _ = snap_tx.send(s); }
                        let _ = reply.send(cleared);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupt received; shutting down");
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

/// Bind the broker's loopback listener and start serving.
///
/// Every failure here returns `None` rather than propagating: a broker that cannot start is an
/// agent without tracker tools, which is a degrade the whole design allows for. Failing startup
/// over it would turn an optional capability into a required one.
fn start_broker(
    cfg: &Config,
    writes: Arc<dyn TrackerWrites>,
    clock: Arc<dyn Clock>,
) -> Option<Arc<Broker>> {
    let listener = match broker::server::bind() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "broker could not bind; agents run without tracker tools");
            return None;
        }
    };
    let addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "broker listener has no address; running without tools");
            return None;
        }
    };

    // Per-process, so two orchestrators on one host cannot collide or read each other's tokens.
    let config_dir = std::env::temp_dir().join(format!("crew-mcp-{}", std::process::id()));
    let limits = BrokerLimits {
        max_calls_per_run: cfg.broker.max_calls_per_run,
        max_calls_per_issue: cfg.broker.max_calls_per_issue,
    };

    let broker = match Broker::new(writes, clock, limits, cfg.known_states(), addr, &config_dir) {
        Ok(b) => Arc::new(b),
        Err(e) => {
            tracing::warn!(error = %e, "broker setup failed; agents run without tracker tools");
            return None;
        }
    };
    broker::server::serve(Arc::clone(&broker), listener);
    tracing::info!(%addr, states = ?cfg.known_states(), "tool broker listening");
    Some(broker)
}

/// Give the demo tracker a spread of behaviours so the dashboard shows every state worth
/// recognising: clean completions, work that continues, a hard failure, and a wedged agent.
/// One configured worker. Deliberately independent of the tracker: see `WorkerConfig`'s doc for
/// why a real tracker does not imply a real worker.
fn build_worker(
    w: &WorkerConfig,
    kind: WorkerKind,
    cfg: &Config,
    clock: &Arc<dyn Clock>,
    tracker_kind: TrackerKind,
) -> Arc<dyn Worker> {
    match kind {
        WorkerKind::Claude => {
            let bin = w.bin.clone().unwrap_or_else(|| "claude".to_string());
            // An operator-supplied list replaces the default outright rather than extending it,
            // so what reaches the child is exactly what the config says.
            let env_allowlist = w
                .env_allowlist
                .clone()
                .unwrap_or_else(|| DEFAULT_ENV_ALLOWLIST.iter().map(|s| s.to_string()).collect());
            Arc::new(
                ClaudeWorker::new(bin, env_allowlist, cfg.agent.max_turns_per_session)
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
    }
}

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
    let registered =
        init::run(&UreqHttp::default(), &SystemClock::new(), &mut TerminalOperator, &opts)?;
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
