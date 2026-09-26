//! Scheduler behaviour, end to end, on a fake clock.
//!
//! Every test drives real `Scheduler::tick` against fake tracker/worker/workspace. Time only
//! moves when a test moves it, so there are no sleeps and nothing to flake. Each test names the
//! invariant it defends; several of them correspond directly to defects found in the spec.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crew::broker::fake::FakeWrites;
use crew::broker::{Broker, BrokerLimits, TrackerWrites};
use crew::clock::{Clock, FakeClock};
use crew::config::{AgentConfig, Config, PollingConfig, TrackerConfig, WorkspaceConfig};
use crew::gate::Verdict as GateVerdict;
use crew::gate::fake::{FakeGate, GateScript};
use crew::gate::{Gate, GateHandle, GitGate};
use crew::model::{ErrorClass, Issue, Outcome, Phase};
use crew::project::{NoopProjector, Projector, TasksProjector};
use crew::sched::Scheduler;
use crew::store::Store;
use crew::tracker::TrackerError;
use crew::tracker::fake::FakeTracker;
use crew::transcript::Transcripts;
use crew::worker::fake::{FakeWorker, Script};
use crew::worker::{Effort, ModelChoice, RateLimitSignal, Session};
use crew::workspace::{DirWorkspace, GitWorktreeWorkspace, Workspace};

struct Harness {
    sched: Scheduler,
    clock: Arc<FakeClock>,
    tracker: Arc<FakeTracker>,
    worker: Arc<FakeWorker>,
    root: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn issue(n: u32, state: &str, prio: Option<i32>) -> Issue {
    Issue {
        id: format!("iss-{n}"),
        identifier: format!("MT-{n}"),
        title: format!("issue {n}"),
        body: None,
        state: state.to_string(),
        priority: prio,
        url: None,
        labels: vec!["agent".into()],
        dispatchable: true,
        created_at: Some(1_000 + n as i64),
        native_ref: None,
        blocked_by: vec![],
    }
}

fn harness(issues: Vec<Issue>, tune: impl FnOnce(&mut Config)) -> Harness {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!("crew-sched-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let workspace = Arc::new(DirWorkspace::new(&root).unwrap());
    harness_over(issues, root, Store::open_in_memory().unwrap(), workspace, tune)
}

/// A harness whose durable state the caller names, so a second one can be built over the same
/// database and workspace root — which is all a restart is.
fn harness_over(
    issues: Vec<Issue>,
    root: PathBuf,
    store: Store,
    workspace: Arc<dyn Workspace>,
    tune: impl FnOnce(&mut Config),
) -> Harness {
    harness_full(issues, root, store, workspace, Arc::new(NoopProjector), tune)
}

/// The fully explicit builder. Every other harness reaches the scheduler through here, so a
/// test that needs to swap one seam names it and inherits the rest.
fn harness_full(
    issues: Vec<Issue>,
    root: PathBuf,
    store: Store,
    workspace: Arc<dyn Workspace>,
    projector: Arc<dyn Projector>,
    tune: impl FnOnce(&mut Config),
) -> Harness {
    let mut cfg = Config {
        tracker: TrackerConfig {
            kind: "fake".into(),
            active_states: vec!["in progress".into()],
            terminal_states: vec!["done".into()],
            required_labels: vec![],
            owner: String::new(),
            repo: String::new(),
            ..Default::default()
        },
        polling: PollingConfig { interval_ms: 30_000 },
        workspace: WorkspaceConfig { root: Some(root.clone()), repo: None },
        agent: AgentConfig::default(),
        worker: Default::default(),
        broker: Default::default(),
        api: Default::default(),
        transcripts: Default::default(),
        gate: Default::default(),
        delivery: Default::default(),
    };
    tune(&mut cfg);
    cfg.preflight().expect("test config must be valid");

    let clock = Arc::new(FakeClock::new());
    let tracker = Arc::new(FakeTracker::new(issues));
    let worker = Arc::new(FakeWorker::new(clock.clone()));

    let sched = Scheduler::new(
        cfg,
        clock.clone(),
        store,
        tracker.clone(),
        worker.clone(),
        workspace,
        projector,
    );

    Harness { sched, clock, tracker, worker, root }
}

/// A broker with nothing real behind it, for asserting on what the scheduler hands a run.
fn test_broker(clock: Arc<FakeClock>) -> (Arc<Broker>, Arc<FakeWrites>) {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("crew-sched-mcp-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let writes = Arc::new(FakeWrites::new());
    let w: Arc<dyn TrackerWrites> = writes.clone();
    let b = Broker::new(
        w,
        clock,
        BrokerLimits::default(),
        vec!["in progress".into(), "done".into()],
        "127.0.0.1:1".parse().unwrap(),
        dir,
    )
    .unwrap();
    (Arc::new(b), writes)
}

// ---- tool broker ------------------------------------------------------------

#[test]
fn a_dispatched_run_is_handed_a_broker_endpoint_scoped_to_its_own_issue() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    let (broker, writes) = test_broker(h.clock.clone());
    h.sched.set_broker(Some(broker.clone()));

    h.sched.tick().unwrap();

    let endpoints = h.worker.endpoints_for("iss-1");
    assert_eq!(endpoints.len(), 1);
    let ep = endpoints[0].as_ref().expect("a broker was attached, so the run gets tools");
    assert_eq!(ep.server, "crew");
    assert!(ep.config_path.exists(), "the worker needs a file to pass to --mcp-config");
    assert_eq!(ep.qualified("comment"), "mcp__crew__comment");

    // The endpoint is only useful if the token inside it reaches this issue and no other. Read
    // it back the way the agent would, rather than trusting the scheduler's bookkeeping.
    let cfg: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ep.config_path).unwrap()).unwrap();
    let url = cfg["mcpServers"]["crew"]["url"].as_str().unwrap();
    let token = url.rsplit('/').next().unwrap();

    broker.call(token, "comment", &serde_json::json!({ "body": "from the agent" })).unwrap();
    assert_eq!(writes.count(), 1);
    assert_eq!(writes.writes()[0].issue_id(), "iss-1");
}

#[test]
fn dispatch_without_a_broker_still_runs_the_agent_just_without_tools() {
    // The degrade the whole design turns on: a broker that could not start costs the agent a
    // capability, never a dispatch.
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});

    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 1, "the run happens regardless");
    assert_eq!(h.worker.endpoints_for("iss-1"), vec![None]);
}

#[test]
fn a_run_that_ends_takes_its_broker_authority_with_it() {
    // A token that outlived its run would let an orphaned agent keep writing to a ticket the
    // orchestrator has already released.
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    let (broker, writes) = test_broker(h.clock.clone());
    h.sched.set_broker(Some(broker.clone()));

    h.sched.tick().unwrap();
    let ep = h.worker.endpoints_for("iss-1")[0].clone().unwrap();
    let cfg: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ep.config_path).unwrap()).unwrap();
    let url = cfg["mcpServers"]["crew"]["url"].as_str().unwrap().to_string();
    let token = url.rsplit('/').next().unwrap().to_string();
    assert_eq!(broker.open_sessions(), 1);

    // Let the scripted run finish and be harvested.
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    assert_eq!(
        h.sched.running_count(),
        0,
        "the run must actually be over for this to mean anything"
    );

    assert_eq!(broker.open_sessions(), 0, "the session died with the run");
    assert!(!ep.config_path.exists(), "and so did the file carrying its token");
    assert!(broker.call(&token, "comment", &serde_json::json!({ "body": "late" })).is_err());
    assert_eq!(writes.count(), 0);
}

// ---- dispatch ---------------------------------------------------------------

#[test]
fn dispatch_respects_the_global_concurrency_limit() {
    let issues = (1..=5).map(|n| issue(n, "In Progress", Some(1))).collect();
    let mut h = harness(issues, |c| c.agent.max_concurrent = 2);

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2, "must not exceed max_concurrent");

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2, "and must not creep upward on later ticks");
}

#[test]
fn a_saturated_state_cap_skips_that_state_without_stalling_the_queue() {
    let mut issues: Vec<Issue> = (1..=3).map(|n| issue(n, "In Progress", Some(1))).collect();
    issues.push(issue(9, "In Review", Some(1)));

    let mut h = harness(issues, |c| {
        c.tracker.active_states = vec!["in progress".into(), "in review".into()];
        c.agent.max_concurrent = 10;
        c.agent.max_concurrent_by_state.insert("in progress".into(), 1);
    });

    h.sched.tick().unwrap();
    // One "In Progress" plus the unconstrained "In Review": the per-state cap must not act as
    // a barrier for the rest of the queue.
    assert_eq!(h.sched.running_count(), 2);
}

#[test]
fn undispatchable_and_unlabelled_issues_are_never_picked_up() {
    let mut a = issue(1, "In Progress", Some(1));
    a.dispatchable = false;
    let mut b = issue(2, "In Progress", Some(1));
    b.labels = vec!["other".into()];
    let c = issue(3, "In Progress", Some(1));

    let mut h = harness(vec![a, b, c], |cfg| {
        cfg.tracker.required_labels = vec!["agent".into()];
    });

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "only the routable, correctly-labelled issue runs");
}

#[test]
fn dispatch_order_is_priority_then_age() {
    let issues = vec![
        issue(1, "In Progress", None), // no priority: last
        issue(2, "In Progress", Some(3)),
        issue(3, "In Progress", Some(1)), // highest
    ];
    let mut h = harness(issues, |c| c.agent.max_concurrent = 1);

    h.sched.tick().unwrap();
    let snap = h.sched.snapshot().unwrap();
    let running: Vec<_> = snap.rows.iter().filter(|r| r.phase == Phase::Running).collect();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].identifier, "MT-3");
}

// ---- outcomes ---------------------------------------------------------------

#[test]
fn a_done_verdict_releases_the_claim_and_keeps_the_workspace() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    let ws = h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap();

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(!st.is_quarantined());
    assert_eq!(st.attempt, 0, "a clean finish must not leave failure state behind");
    assert!(
        PathBuf::from(&ws).exists(),
        "workspaces are preserved across runs; that warmth is the point"
    );
}

/// Releasing the claim is not enough on its own: the ticket usually stays in an active state
/// after a run finishes, so without parking the next tick picks it straight back up — the
/// continuation runaway arriving by another route.
#[test]
fn a_finished_issue_is_not_re_dispatched_while_its_state_is_unchanged() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0);

    for _ in 0..10 {
        h.clock.advance_ms(60_000);
        h.sched.tick().unwrap();
        assert_eq!(h.sched.running_count(), 0, "parked issues must stay parked");
    }
    assert_eq!(
        h.sched.store().get("iss-1").unwrap().unwrap().parked_state.as_deref(),
        Some("in progress")
    );
}

#[test]
fn moving_the_ticket_makes_a_parked_issue_live_again() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.tracker.active_states = vec!["in progress".into(), "in review".into()];
    });
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "parked");

    // A human moves it on: there is something new to act on.
    h.tracker.set_state("iss-1", "In Review");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "a state change lifts the park");
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state.is_none());
}

/// Bring a fresh harness to the point where `iss-1` has finished `Done` and is parked, and
/// return its workspace path. The first tick dispatches; the second harvests and parks.
fn park_one(h: &mut Harness) -> PathBuf {
    h.worker.set_default(Script::succeeds_in(1_000));
    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "parked");
    assert!(ws.exists(), "a parked issue keeps its warm workspace");
    ws
}

/// The leak this closes: a `Done` run is released and parked, which takes it out of `running`,
/// and once the ticket closes the active-state poll never returns it again. Every cleanup path
/// before this one hung off `running` or a retry row, so the worktree stayed on disk for good.
#[test]
fn a_parked_issue_that_is_later_closed_has_its_workspace_reclaimed_without_a_restart() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.parked_sweep_interval_ms = 300_000;
    });
    let ws = park_one(&mut h);

    // While the ticket stays where the run left it, sweeps find nothing to reclaim.
    h.clock.advance_ms(300_000);
    h.sched.tick().unwrap();
    assert!(ws.exists(), "a parked issue in a non-terminal state keeps its workspace");

    // A human closes it. The next sweep is what notices — no restart, no re-dispatch.
    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(300_000);
    h.sched.tick().unwrap();

    assert!(!ws.exists(), "the closed issue's workspace must be reclaimed");
    assert_eq!(h.sched.running_count(), 0, "cleanup must not dispatch anything");
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(
        st.parked_state.is_none(),
        "a reclaimed issue leaves the parked set, or every later sweep would pay for it"
    );
    assert_eq!(st.phase, Phase::Released);
}

/// The sweep is the one path that reaches `Workspace::remove` with no live run behind it, so
/// it is the one a reviewer forgets when `remove`'s contract changes. It did: the branch a run
/// leaves behind is published from the store, and cleanup deletes that ref whenever git's
/// merged check says it carries nothing new — so a sweep that reclaims a worktree without
/// clearing the stored name leaves `status` pointing at a branch that is gone.
#[test]
fn a_sweep_that_deletes_a_branch_clears_the_name_the_snapshot_publishes() {
    let dir = tmp_dir("sweep-clears-branch");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));

    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |c| c.agent.parked_sweep_interval_ms = 300_000,
    );

    // A run that finishes without committing: cleanup will delete its branch, because the
    // merged check finds nothing on it that HEAD does not already have.
    h.worker.set_default(Script::succeeds_in(1_000));
    h.sched.tick().unwrap();
    // `Row.workspace` describes a live run, so it has to be read while the run is still in
    // flight. `Row.branch` comes from the store and outlives it — which is the point.
    let ws = PathBuf::from(
        h.sched.snapshot().unwrap().rows[0]
            .workspace
            .clone()
            .expect("a running row names its worktree"),
    );
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "the run is parked, not running");

    let parked = h.sched.snapshot().unwrap();
    let row = parked.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    assert!(row.branch.is_some(), "a parked run still publishes the branch it checked out");
    assert!(ws.exists(), "a parked issue keeps its warm workspace");

    // The ticket closes later, so the sweep is what reclaims it — no restart, no re-dispatch.
    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(300_000);
    h.sched.tick().unwrap();

    assert!(!ws.exists(), "the sweep must reclaim the closed issue's workspace");
    let after = h.sched.snapshot().unwrap();
    let row = after.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    assert_eq!(
        row.branch, None,
        "the sweep deleted the ref, so the published name must go with it rather than \
         sending an operator to a branch that no longer exists"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The sweep is a poll over ids the scheduler is otherwise not watching, so its cost has to be
/// set by the interval and the number of parked issues — not by how many ticks an issue spends
/// parked. An implementation that swept every tick would pass the test above and fail this one.
#[test]
fn sweeping_parked_issues_costs_tracker_traffic_bounded_by_the_interval_not_by_ticks() {
    let interval = 300_000u64;
    let poll = 30_000u64;
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.parked_sweep_interval_ms = interval;
    });
    park_one(&mut h);
    let (_, by_ids_before) = h.tracker.call_counts();

    let ticks = 20u64;
    for _ in 0..ticks {
        h.clock.advance_ms(poll);
        h.sched.tick().unwrap();
    }

    let (_, by_ids_after) = h.tracker.call_counts();
    let sweeps = (by_ids_after - by_ids_before) as u64;
    let elapsed = ticks * poll;
    assert!(sweeps >= 1, "the sweep has to run at all for the bound to mean anything");
    assert!(
        sweeps <= elapsed / interval + 1,
        "{sweeps} by_ids calls over {elapsed}ms with a {interval}ms interval: the sweep is \
         running more often than its cadence allows"
    );
    assert!(sweeps < ticks, "and it certainly must not run once per tick");
}

/// Reclaiming a parked worktree must keep the branch-survival contract that every other cleanup
/// path keeps: the directory is scratch, the commits are the deliverable. Closing the ticket is
/// not a decision to throw the work away.
#[test]
fn a_parked_issues_committed_work_survives_the_sweep_that_reclaims_its_worktree() {
    let dir = tmp_dir("parked-sweep-git");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));
    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |c| c.agent.parked_sweep_interval_ms = 300_000,
    );
    let ws = park_one(&mut h);
    commit_in(&ws, "work.txt", "what the agent delivered");
    std::fs::write(ws.join("scratch.txt"), b"never committed").unwrap();

    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(300_000);
    h.sched.tick().unwrap();
    assert!(!ws.exists(), "the worktree itself must be reclaimed");

    // Reopening is how the branch is proven to have survived: the next dispatch attaches to
    // it, so the committed file comes back while the uncommitted one stays gone.
    h.tracker.set_state("iss-1", "In Progress");
    h.worker.set_default(Script::succeeds_in(600_000));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "a reopened issue is dispatchable again");
    assert!(ws.join("work.txt").exists(), "the commits must outlive the worktree they were in");
    assert!(
        !ws.join("scratch.txt").exists(),
        "which is only meaningful if the directory was rebuilt"
    );

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_blocked_verdict_releases_without_scheduling_a_retry() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Blocked { why: "needs a human".into() }),
    );

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    assert!(h.sched.store().all_retries().unwrap().is_empty(), "blocked must not retry");
}

/// The spec's central defect: a clean exit means "maybe continue", re-dispatched on a flat 1s
/// timer forever. Here the delay escalates while the tracker state does not move.
#[test]
fn continuation_backs_off_instead_of_respawning_every_second() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    // First continuation: 5s.
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let due = h.sched.store().all_retries().unwrap()[0].due_at;
    assert_eq!(due - h.clock.wall().0, 5_000);

    // Second, with the state still unmoved: 30s.
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let due = h.sched.store().all_retries().unwrap()[0].due_at;
    assert_eq!(due - h.clock.wall().0, 30_000);
}

/// #109: a Decisions section written into a description after the session started never
/// reached the resumed session, because the continuation prompt leaves the body out. The
/// worker is told exactly when the body differs from what the session last saw.
#[test]
fn a_description_edited_after_the_session_started_reaches_the_resumed_session() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.tracker.set_body("iss-1", Some("do the thing"));
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );
    let attempt = |h: &mut Harness| {
        h.sched.tick().unwrap();
        h.clock.advance_ms(1_000);
        h.sched.tick().unwrap();
        h.clock.advance_ms(60_000);
    };

    attempt(&mut h);
    attempt(&mut h);
    h.tracker.set_body("iss-1", Some("do the thing\n\n## Decisions\n\nthis way"));
    attempt(&mut h);
    attempt(&mut h);
    h.sched.tick().unwrap();

    assert_eq!(
        h.worker.body_changed_for("iss-1"),
        vec![false, false, true, false, false],
        "a new session has the body in its first prompt; a resume gets it again only once \
         after it changes"
    );
}

/// The milestone order is the priority (#86). A continuation's delay slows one issue's loop; it
/// must not also hand that issue's slot to whatever became eligible in the same tick, or at
/// `max_concurrent = 1` the milestone is worked round-robin by session.
#[test]
fn a_continuing_issue_keeps_its_slot_through_its_continuation_delay() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1)), issue(2, "Todo", Some(2))], |c| {
        c.agent.max_concurrent = 1;
        // B in another state, so only the global count stands between it and A's slot.
        c.tracker.active_states.push("todo".into());
    });
    h.tracker.set_dispatchable("iss-2", false);
    h.worker.script(
        "iss-1",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    h.sched.tick().unwrap();
    h.tracker.set_dispatchable("iss-2", true);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // A continues, 5s out; B is eligible in this same tick
    assert!(h.worker.sessions_for("iss-2").is_empty(), "B must not take A's slot");

    let snap = h.sched.snapshot().unwrap();
    assert_eq!((snap.running, snap.reserved, snap.limit), (0, 1, 1), "the slot shows as taken");
    let a = snap.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    assert!(a.holds_slot, "and names the issue holding it");
    let b = snap.rows.iter().find(|r| r.issue_id == "iss-2");
    assert!(b.is_none_or(|b| !b.holds_slot));

    h.worker.script("iss-1", Script::succeeds_in(1_000));
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "A is dispatched again first");
    assert!(h.worker.sessions_for("iss-2").is_empty(), "and B still waits while A runs");
    assert_eq!(h.sched.snapshot().unwrap().reserved, 0, "a dispatched retry reserves nothing");

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // A ends done, and only then does B get the slot
    assert_eq!(h.worker.sessions_for("iss-2").len(), 1);
}

/// The per-state limit is capacity too: a continuation's slot in its own state is held just as
/// its global one is, or a second issue in that state takes it while global room remains (#86).
#[test]
fn a_continuation_holds_its_slot_in_its_own_state_limit() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |c| {
            c.agent.max_concurrent = 2;
            c.agent.max_concurrent_by_state.insert("in progress".into(), 1);
        });
    h.tracker.set_dispatchable("iss-2", false);
    h.worker.script(
        "iss-1",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    h.sched.tick().unwrap();
    h.tracker.set_dispatchable("iss-2", true);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert!(h.worker.sessions_for("iss-2").is_empty(), "B must not take A's state slot");

    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "A continues in the slot it held");
}

/// Re-reading a waiting reservation must not turn one tracker omission into a cancelled
/// continuation: an id `by_ids` briefly misses is the blip `refresh_miss_grace` exists for.
#[test]
fn a_waiting_continuation_survives_one_tracker_omission_with_its_slot() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |c| {
            c.agent.max_concurrent = 1;
        });
    h.tracker.set_dispatchable("iss-2", false);
    h.worker.script(
        "iss-1",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    h.sched.tick().unwrap();
    h.tracker.set_dispatchable("iss-2", true);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    h.tracker.hide("iss-1");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.store().all_retries().unwrap().len(), 1, "the continuation is kept");
    assert!(h.worker.sessions_for("iss-2").is_empty(), "and so is its slot");

    h.tracker.unhide("iss-1");
    h.clock.advance_ms(4_000);
    h.sched.tick().unwrap();
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "A continues once it is visible again");
    assert!(h.worker.sessions_for("iss-2").is_empty());
}

/// Reservations follow their ticket's state, so two can land in a state whose limit holds one.
/// Each counting the other would defer both forever; the earliest due goes first instead.
#[test]
fn reservations_oversubscribing_a_state_limit_dispatch_in_due_order_rather_than_deadlocking() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |c| {
            c.agent.max_concurrent = 2;
            c.tracker.active_states.push("review".into());
            c.agent.max_concurrent_by_state.insert("review".into(), 1);
        });
    let cont = |ms| Script::succeeds_in(ms).with_outcome(Outcome::Continue { why: "more".into() });
    h.worker.script("iss-1", cont(1_000));
    h.worker.script("iss-2", cont(2_000));

    h.sched.tick().unwrap(); // both dispatched
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // A continues, due at 6s
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // B continues, due at 7s

    h.tracker.set_state("iss-1", "Review");
    h.tracker.set_state("iss-2", "Review");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // both reservations now count against review's one slot

    h.clock.advance_ms(3_000);
    h.sched.tick().unwrap();
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "the earliest due reservation launches");
    assert_eq!(h.worker.sessions_for("iss-2").len(), 1, "the later one waits behind it");
}

/// A failure backoff grows to minutes, so a reservation there would let one failing issue hold
/// capacity hostage (#86): only a continuation keeps its slot.
#[test]
fn a_failure_backoff_does_not_hold_a_slot() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |c| {
            c.agent.max_concurrent = 1;
        });
    h.tracker.set_dispatchable("iss-2", false);
    h.worker.script(
        "iss-1",
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::AgentCrash, msg: "boom".into() }),
    );

    h.sched.tick().unwrap();
    h.tracker.set_dispatchable("iss-2", true);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.store().all_retries().unwrap().len(), 1, "A waits on its backoff");
    assert_eq!(h.worker.sessions_for("iss-2").len(), 1, "B dispatches in the same tick");
}

/// A reservation that outlives its retry row is a slot lost for good (#86). A ticket closed
/// during the delay ends the reservation at the next tick, not when the delay runs out.
#[test]
fn a_continuation_whose_ticket_went_terminal_releases_its_reserved_slot() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |c| {
            c.agent.max_concurrent = 1;
        });
    h.tracker.set_dispatchable("iss-2", false);
    h.worker.script(
        "iss-1",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    h.sched.tick().unwrap();
    h.tracker.set_dispatchable("iss-2", true);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert!(h.worker.sessions_for("iss-2").is_empty(), "held while A is continuing");

    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(1_000); // still 4s short of A's due time
    h.sched.tick().unwrap();

    assert!(h.sched.store().all_retries().unwrap().is_empty(), "the retry row is gone");
    assert_eq!(h.sched.snapshot().unwrap().reserved, 0);
    assert_eq!(h.worker.sessions_for("iss-2").len(), 1, "and its slot went to B");
    assert_eq!(h.worker.sessions_for("iss-1").len(), 1, "A is not dispatched again");
}

/// A continuation is only worth having if it continues something. Respawning `claude -p` cold
/// makes the agent re-read the issue and re-explore the tree on every turn the budget buys, so
/// the budget and the restart spend the same turns twice.
#[test]
fn a_continuation_resumes_the_conversation_the_first_attempt_started() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // harvest the Continue, queue the retry
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap(); // the continuation itself

    let seen = h.worker.sessions_for("iss-1");
    assert_eq!(seen.len(), 2, "expected a first attempt and a continuation, got {seen:?}");
    assert!(matches!(seen[0], Session::New(_)), "the first attempt names a fresh conversation");
    assert_eq!(
        seen[1],
        Session::Resume(seen[0].id().to_string()),
        "the continuation must resume the conversation the first attempt started"
    );
}

/// The degradation path. A session the CLI no longer holds is answered with "No conversation
/// found with session ID" and no turns at all, so resuming the same name again fails identically
/// every time — straight into quarantine, for an issue with nothing wrong with it.
#[test]
fn a_run_that_took_no_turns_is_not_retried_into_the_same_conversation() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script {
        turns: 0,
        outcome: Outcome::Failed {
            class: ErrorClass::AgentCrash,
            msg: "no conversation found with session id".into(),
        },
        ..Script::succeeds_in(1_000)
    });

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // harvest the failure, queue the retry
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap(); // the retry

    let seen = h.worker.sessions_for("iss-1");
    assert_eq!(seen.len(), 2, "expected a first attempt and a retry, got {seen:?}");
    assert!(
        matches!(seen[1], Session::New(_)),
        "a conversation that produced nothing must not be resumed into, got {seen:?}"
    );
    assert_ne!(seen[0].id(), seen[1].id(), "the retry needs a name of its own");
}

/// The other half of the brake: even a well-behaved `Continue` cannot run forever.
#[test]
fn the_per_issue_turn_budget_stops_an_endless_continuation() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.max_turns_per_issue = 5; // the fake burns 3 turns per run
    });
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "forever".into() }),
    );

    for _ in 0..12 {
        h.sched.tick().unwrap();
        h.clock.advance_ms(31_000);
    }

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(st.is_quarantined(), "must stop once the cross-session turn budget is spent");
    assert!(
        st.cumulative_turns >= 5,
        "budget should have been reached, saw {}",
        st.cumulative_turns
    );
}

// ---- account-wide rate limit (#37) -------------------------------------------

/// The CLI's own account-wide rate limit is not any one issue's failure. Every issue it
/// interrupts must resume at the attempt it was already on rather than being quarantined, and
/// it is dispatch itself — not any one issue's retry timer — that pauses until the window
/// resets.
#[test]
fn a_rate_limit_pauses_dispatch_rather_than_quarantining_the_issues_it_interrupted() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |_| {});
    let resets_at_secs = h.clock.wall().0 / 1_000 + 60;
    h.worker.set_default(
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed {
                class: ErrorClass::AgentCrash,
                msg: "session limit".into(),
            })
            .with_rate_limit(RateLimitSignal {
                kind: "five_hour".into(),
                resets_at: Some(resets_at_secs),
            }),
    );

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2);
    let first_sessions: Vec<_> =
        ["iss-1", "iss-2"].iter().map(|id| h.worker.sessions_for(id)[0].clone()).collect();

    // Both issues arrive at the limit with history behind them. Without this the assertions
    // below start and end at zero, so a scheduler that used the ordinary `release()` — which
    // sets `attempt = 0, consecutive_fail = 0` — would satisfy them exactly as well as one that
    // preserved the claim. The invariant is that the counters are *untouched*, and only a
    // nonzero starting value can tell "untouched" apart from "reset".
    for id in ["iss-1", "iss-2"] {
        for _ in 0..2 {
            h.sched
                .store()
                .record_failure(h.clock.as_ref(), id, ErrorClass::AgentCrash, "earlier", 99)
                .unwrap();
        }
        let st = h.sched.store().get(id).unwrap().unwrap();
        assert_eq!((st.attempt, st.consecutive_fail), (2, 2), "seeded history for {id}");
    }

    // The interruption itself: both runs end on the rejected limit.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0);

    for id in ["iss-1", "iss-2"] {
        let st = h.sched.store().get(id).unwrap().unwrap();
        assert_eq!(st.attempt, 2, "{id} must keep the attempt it was on, not be charged another");
        assert_eq!(st.consecutive_fail, 2, "{id} must keep the identical-failure streak it had");
        assert!(!st.is_quarantined(), "{id} must not be quarantined");
        assert_eq!(st.phase, Phase::Released, "{id} must be dispatchable again, not parked");
    }
    assert!(
        h.sched.store().all_retries().unwrap().is_empty(),
        "no per-issue retry timer owns this — dispatch itself is what pauses"
    );

    let pause = h.sched.snapshot().unwrap().rate_limit_pause.expect("the pause must be published");
    assert_eq!(pause.kind, "five_hour");

    // Before the reset: dispatch stays paused, account-wide.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "still paused before the reset");

    // Past the reset: both issues are dispatchable again, resuming their sessions.
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2, "dispatch resumes once the window resets");
    assert!(h.sched.snapshot().unwrap().rate_limit_pause.is_none());

    for (id, first) in [("iss-1", &first_sessions[0]), ("iss-2", &first_sessions[1])] {
        let seen = h.worker.sessions_for(id);
        assert_eq!(seen.len(), 2, "{id}: {seen:?}");
        assert_eq!(
            seen[1],
            Session::Resume(first.id().to_string()),
            "{id} resumes the conversation the interrupted attempt started"
        );
    }
}

/// What is throttled is the agent's own CLI, not this orchestrator's other work: killing a run
/// that is mid-edit to react to a limit one *other* issue hit would cost work for nothing.
#[test]
fn a_rate_limit_pause_does_not_disturb_runs_already_in_flight() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |_| {});
    h.worker.script(
        "iss-1",
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed {
                class: ErrorClass::AgentCrash,
                msg: "session limit".into(),
            })
            .with_rate_limit(RateLimitSignal {
                kind: "five_hour".into(),
                resets_at: Some(h.clock.wall().0 / 1_000 + 60),
            }),
    );
    // Still running when iss-1 is interrupted, and finishing well inside the 60s pause — which
    // is the point: the run ends first and the pause is still in force afterwards, so what the
    // assertion below proves is that a pause outlives the runs it did not interrupt.
    h.worker.script("iss-2", Script::succeeds_in(10_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2);

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // iss-1 is interrupted and pauses dispatch
    assert_eq!(h.sched.running_count(), 1, "the run already in flight must survive the pause");
    assert_eq!(h.sched.store().get("iss-2").unwrap().unwrap().phase, Phase::Running);

    // iss-2 finishes on its own power while dispatch is still paused — reconciliation is not
    // what the pause gates, only new dispatch is.
    h.clock.advance_ms(9_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0);
    assert_eq!(h.sched.store().get("iss-2").unwrap().unwrap().phase, Phase::Released);
    assert!(h.sched.snapshot().unwrap().rate_limit_pause.is_some(), "the pause itself outlives it");
}

/// A `resets_at` the scheduler cannot trust — missing, or already behind the clock — must not
/// be able to stop dispatch permanently. A clock the host disagrees with is exactly the case
/// this has to survive, so both degrade to the ordinary failure path instead of pausing on a
/// value that would never lift.
#[test]
fn a_rate_limit_with_no_usable_resets_at_degrades_to_ordinary_backoff() {
    for resets_at in [None, Some(1)] {
        let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
        h.worker.set_default(
            Script::succeeds_in(1_000)
                .with_outcome(Outcome::Failed {
                    class: ErrorClass::AgentCrash,
                    msg: "session limit".into(),
                })
                .with_rate_limit(RateLimitSignal { kind: "five_hour".into(), resets_at }),
        );

        h.sched.tick().unwrap();
        h.clock.advance_ms(1_000);
        h.sched.tick().unwrap();

        assert!(
            h.sched.snapshot().unwrap().rate_limit_pause.is_none(),
            "resets_at {resets_at:?} must not pause anything"
        );
        let st = h.sched.store().get("iss-1").unwrap().unwrap();
        assert_eq!(st.attempt, 1, "resets_at {resets_at:?} falls back to charging the attempt");
        assert!(!h.sched.store().all_retries().unwrap().is_empty(), "an ordinary retry is queued");
    }
}

// ---- handoff gate -----------------------------------------------------------

/// A harness with a fake gate attached, and the run scripted to finish `Done` after one second.
fn gated_harness(tune: impl FnOnce(&mut Config)) -> (Harness, Arc<FakeGate>) {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], tune);
    let gate = Arc::new(FakeGate::new(h.clock.clone()));
    h.sched.set_gate(Some(gate.clone()));
    h.worker.set_default(Script::succeeds_in(1_000));
    (h, gate)
}

/// Dispatch and let the agent finish, so the next tick is the one that starts the gate.
fn dispatch_and_finish(h: &mut Harness) -> PathBuf {
    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    ws
}

/// A gate is work on this machine, so it must occupy the slot its run occupied. Counting only
/// `running` frees the slot the instant the agent exits, and a fast worker in front of a slow
/// gate then starts another agent while the last one's suite is still compiling — `cargo test`
/// processes accumulate with nothing bounding them, and `max_concurrent` stops describing what
/// the host is actually running.
#[test]
fn a_gating_run_still_holds_its_concurrency_slot_so_gates_cannot_accumulate() {
    let issues = vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(1))];
    let mut h = harness(issues, |c| c.agent.max_concurrent = 1);
    let gate = Arc::new(FakeGate::new(h.clock.clone()));
    h.sched.set_gate(Some(gate.clone()));
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "one slot, one agent");

    // The same tick that moves the finished run into the gate also reaches `dispatch_new`.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 1, "the gate has taken the run over");
    assert_eq!(h.sched.running_count(), 0, "and the second issue must not have taken the slot");

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "still held, tick after tick");

    // Once the gate is done the slot is genuinely free, or this would be a deadlock rather
    // than a bound.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 0, "gate finished");
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "the freed slot is reused");
}

/// `observe_progress` only walks `running`, and the run row stays open for the whole gate — the
/// long part of the run. Without a checkpoint taken as the entry leaves `running`, a hard kill
/// during a gate has `recover()` close the row on the previous tick's figure, undercounting the
/// attempt and charging the per-issue turn budget less than the agent actually spent.
#[test]
fn a_run_entering_the_gate_checkpoints_its_turn_count_before_it_leaves_running() {
    let (mut h, _gate) = gated_harness(|_| {});
    dispatch_and_finish(&mut h);
    assert_eq!(h.sched.gating_count(), 1, "the gate has taken over");

    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].ended_at.is_none(), "the run row is still open for the gate");
    assert_eq!(
        runs[0].turns, 3,
        "the durable count must be what the agent finished on, not the last tick that saw it \
         in `running`"
    );
}

/// The guard for issue #21 at the scheduler: a `Done` is not applied until the gate has run in
/// the run's own worktree, and the claim is held for the whole of that. Skip the gate and the
/// fake records no start; release early and the phase reads `Released` while the gate is still
/// running, which is the window in which a second agent could be dispatched onto the worktree.
#[test]
fn a_done_verdict_is_gated_in_its_own_worktree_before_the_claim_is_released() {
    let (mut h, gate) = gated_harness(|_| {});
    let ws = dispatch_and_finish(&mut h);

    assert_eq!(h.sched.running_count(), 0, "the agent is finished");
    assert_eq!(h.sched.gating_count(), 1, "and the gate must have taken over from it");
    assert_eq!(gate.starts_for("iss-1"), vec![ws.clone()], "gated where the agent worked");
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Running, "the claim is held until the gate has spoken");
    assert!(st.parked_state.is_none(), "and the issue is not yet parked");
    let row = &h.sched.snapshot().unwrap().rows[0];
    assert!(
        row.last_event.as_deref().is_some_and(|e| e.starts_with("gate:")),
        "an operator must be able to see the gate running, got {:?}",
        row.last_event
    );
    assert_eq!(row.workspace.as_deref(), Some(ws.to_str().unwrap()), "on which worktree");

    // Ticks while the gate runs must not touch the issue.
    h.clock.advance_ms(500);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 1);
    assert_eq!(h.sched.running_count(), 0, "nothing may be dispatched onto a gating worktree");

    h.clock.advance_ms(500);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 0);
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released);
    assert_eq!(st.parked_state.as_deref(), Some("in progress"), "a passed gate is a real Done");
    assert_eq!(st.attempt, 0, "and not a failure of any kind");
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome.as_deref(), Some("done"));
    assert_eq!(runs[0].turns, 3, "the run's own counters survive the wait");
}

#[test]
fn a_branch_with_nothing_to_hand_off_passes_straight_through() {
    let (mut h, gate) = gated_harness(|_| {});
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::NoCommits));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released);
    assert_eq!(st.parked_state.as_deref(), Some("in progress"));
    assert!(h.sched.store().all_retries().unwrap().is_empty());
}

/// A conflict is a human's problem, not a failure to retry and not something to bury in a log
/// line: the issue parks `Blocked`, the reason names the paths, and nothing is re-dispatched.
#[test]
fn a_rebase_conflict_parks_the_issue_blocked_naming_the_conflicted_paths() {
    let (mut h, gate) = gated_harness(|c| c.gate.base = Some("master".into()));
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Conflict {
        paths: vec!["src/sched/mod.rs".into(), "CLAUDE.md".into()],
        base_sha: BASE.into(),
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released, "blocked releases the claim");
    assert_eq!(st.parked_state.as_deref(), Some("in progress"), "and parks, like any Blocked");
    assert!(h.sched.store().all_retries().unwrap().is_empty(), "a conflict is not retried");
    assert!(!st.is_quarantined(), "and is not a failure");
    let note = st.last_error.expect("the reason must reach the dashboard");
    assert!(note.contains("master"), "which base: {note}");
    assert!(note.contains("src/sched/mod.rs") && note.contains("CLAUDE.md"), "which files: {note}");
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs[0].outcome.as_deref(), Some("blocked"), "the run row says what happened");

    for _ in 0..5 {
        h.clock.advance_ms(60_000);
        h.sched.tick().unwrap();
        assert_eq!(h.sched.running_count() + h.sched.gating_count(), 0, "blocked stays parked");
    }
}

/// #109: #105 was parked on a conflict, unblocked, and resumed with a prompt saying it had run
/// out of turns — so it reported its work already done and hit the same conflict a minute later.
/// The run after the unblocking must be handed the conflict itself, once.
#[test]
fn a_resume_after_a_rebase_conflict_is_handed_the_conflict_rather_than_a_turn_budget() {
    let (mut h, gate) = gated_harness(|c| {
        c.gate.base = Some("master".into());
        c.tracker.active_states.push("todo".into());
    });
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Conflict {
        paths: vec!["src/store/schema.rs".into()],
        base_sha: BASE.into(),
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count() + h.sched.gating_count(), 0, "parked Blocked");

    // The operator's unblocking: the ticket moves, which lifts the park.
    gate.set_default(GateScript::passes_in(1_000));
    h.tracker.set_state("iss-1", "Todo");
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "the unblocked issue is dispatched again");
    assert!(matches!(h.worker.sessions_for("iss-1")[1], Session::Resume(_)));
    assert_eq!(
        h.worker.feedback_for("iss-1")[1],
        Some(Feedback::Conflict {
            base: "master".into(),
            base_sha: BASE.into(),
            paths: vec!["src/store/schema.rs".into()]
        }),
        "the resumed run is told what blocked the last one"
    );

    // Told once: the run after that one has nothing queued.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.tracker.set_state("iss-1", "In Progress");
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    let feedback = h.worker.feedback_for("iss-1");
    assert_eq!(feedback.len(), 3, "dispatched a third time: {feedback:?}");
    assert_eq!(feedback[2], None, "the conflict was taken by the run it was queued for");
}

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

fn resolvable(c: &mut Config) {
    c.gate.base = Some("master".into());
    c.gate.agent_resolvable =
        vec!["CLAUDE.md".into(), "docs/**".into(), "src/store/schema.rs".into()];
}

/// Two branches appending to the same invariant table is not a human's problem (#111): the
/// agent that wrote one of them is handed the conflict back as a continuation, told the base,
/// the paths and that the rebase was aborted — `GitGate` aborts before any verdict, which is
/// what `a_conflicting_rebase_is_aborted_and_names_the_conflicted_paths` guards, so the branch
/// is the one the agent committed.
#[test]
fn a_conflict_confined_to_agent_resolvable_paths_is_handed_back_to_the_agent() {
    let (mut h, gate) = gated_harness(resolvable);
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Conflict {
        paths: vec!["CLAUDE.md".into()],
        base_sha: BASE.into(),
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::RetryQueued, "a resolvable conflict is a continuation");
    assert_eq!(st.parked_state, None, "and does not park");
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs[0].outcome.as_deref(), Some("continue"));

    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "the continuation is dispatched");
    assert!(matches!(h.worker.sessions_for("iss-1")[1], Session::Resume(_)));
    let Some(Feedback::Gate { output }) = &h.worker.feedback_for("iss-1")[1] else {
        panic!("the continuation must be told about the conflict");
    };
    assert!(output.contains("CLAUDE.md"), "which paths: {output}");
    assert!(output.contains("master"), "onto which base: {output}");
    assert!(output.contains(&format!("git rebase {BASE}")), "by commit, not ref: {output}");
    assert!(output.contains("aborted") && output.contains("where you left it"), "{output}");
    assert!(output.contains("1 of 3"), "and how many tries are left: {output}");
    assert!(!output.contains("RELEASED"), "the migration rule only when schema.rs conflicts");
}

/// A real `GitGate` whose `start` returns only once its verdict is in, so the scheduler sees it
/// on the next tick without the test waiting on the gate's thread against the wall clock.
struct SettledGate(GitGate);

impl Gate for SettledGate {
    fn start(&self, issue: &Issue, workspace: &Path) -> Arc<dyn GateHandle> {
        let h = self.0.start(issue, workspace);
        while h.finished().is_none() {
            std::thread::yield_now();
        }
        h
    }
}

/// #122 end to end: a conflict handed back (#111), resolved the way an agent told to bring its
/// branch up to date commonly does — by merging the base in — must pass the gate. Rebasing that
/// branch replays only the agent's own commit, drops the merge that held the resolution, and
/// parks the issue on the same conflict again, forever.
#[test]
fn a_conflict_resolved_by_merging_the_base_is_not_re_raised_by_the_gate() {
    let dir = tmp_dir("merge-resolves-conflict");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));
    commit_in(&repo, "CLAUDE.md", "base\n");
    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |c| {
            resolvable(c);
            c.gate.base = Some("main".into());
        },
    );
    h.sched.set_gate(Some(Arc::new(SettledGate(GitGate::new(&repo, Some("main".into()), vec![])))));
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    commit_in(&ws, "CLAUDE.md", "the agent's line\n");
    commit_in(&repo, "CLAUDE.md", "another branch's line\n");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.sched.tick().unwrap();
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::RetryQueued, "the conflict is handed back to the agent");

    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "the continuation is dispatched");
    let Some(Feedback::Gate { output }) = &h.worker.feedback_for("iss-1")[1] else {
        panic!("the continuation must be told about the conflict");
    };
    assert!(output.contains("git merge "), "a merge is offered as a resolution: {output}");

    assert!(git_out(&ws, &["merge", "-q", "main"]).is_none(), "the merge must conflict");
    std::fs::write(ws.join("CLAUDE.md"), "the agent's line\nanother branch's line\n").unwrap();
    git(&ws, &["add", "CLAUDE.md"]);
    git(&ws, &["commit", "-q", "--no-edit"]);
    let merged = git_out(&ws, &["rev-parse", "HEAD"]).unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released);
    assert_eq!(st.parked_state.as_deref(), Some("in progress"), "parked Done, not Blocked");
    assert!(h.sched.store().all_retries().unwrap().is_empty(), "and not handed back again");
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs[0].outcome.as_deref(), Some("done"), "the latest run is a Done: {runs:?}");
    assert_eq!(git_out(&ws, &["rev-parse", "HEAD"]), Some(merged), "the merge commit survives");

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `gate.base` unset the gate rebased onto `workspace.repo`'s HEAD; in the agent's own
/// worktree `HEAD` is its branch, so a brief saying `git rebase HEAD` is a no-op that leaves
/// the conflict to recur. The brief names the commit the gate actually tried.
#[test]
fn a_conflict_brief_with_no_configured_base_names_the_commit_not_head() {
    let (mut h, gate) = gated_harness(|c| {
        resolvable(c);
        c.gate.base = None;
    });
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Conflict {
        paths: vec!["CLAUDE.md".into()],
        base_sha: BASE.into(),
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    let Some(Feedback::Gate { output }) = &h.worker.feedback_for("iss-1")[1] else {
        panic!("the continuation must be told about the conflict");
    };
    assert!(output.contains(&format!("git rebase {BASE}")), "{output}");
    assert!(!output.contains("git rebase HEAD"), "{output}");
}

/// A rebase the gate could not abort leaves the worktree mid-rebase — not the branch the agent
/// left, and not a tree any brief describes — so it is a human's even when every conflicted
/// path is on the agent's list.
#[test]
fn a_rebase_the_gate_could_not_abort_blocks_for_a_human_even_on_resolvable_paths() {
    let (mut h, gate) = gated_harness(resolvable);
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Stuck {
        step: "rebase onto master".into(),
        output: "`git rebase --abort` left it in progress: index.lock exists".into(),
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released);
    assert_eq!(st.parked_state.as_deref(), Some("in progress"), "parked Blocked");
    assert!(h.sched.store().all_retries().unwrap().is_empty(), "no continuation into it");
    let note = st.last_error.expect("the reason must reach the dashboard");
    assert!(note.contains("index.lock"), "{note}");
}

/// A renumbering is mechanical, but a migration is a one-way door: the brief states the rule
/// the gate's own `a_released_migration_is_never_edited` would otherwise teach by failing.
#[test]
fn a_schema_conflict_brief_names_the_released_migration_rule() {
    let (mut h, gate) = gated_harness(resolvable);
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Conflict {
        paths: vec!["src/store/schema.rs".into(), "docs/adr/0001-x.md".into()],
        base_sha: BASE.into(),
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    let Some(Feedback::Gate { output }) = &h.worker.feedback_for("iss-1")[1] else {
        panic!("the continuation must be told about the conflict");
    };
    assert!(output.contains("never be edited") && output.contains("RELEASED"), "{output}");
}

/// The list narrows which conflicts are the agent's; it does not make a partly-listed one so.
#[test]
fn a_conflict_touching_any_other_path_still_blocks_for_a_human() {
    let (mut h, gate) = gated_harness(resolvable);
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Conflict {
        paths: vec!["CLAUDE.md".into(), "src/sched/mod.rs".into()],
        base_sha: BASE.into(),
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released);
    assert_eq!(st.parked_state.as_deref(), Some("in progress"), "parked Blocked, as before #111");
    assert!(h.sched.store().all_retries().unwrap().is_empty());
    let note = st.last_error.expect("the reason must reach the dashboard");
    assert!(note.contains("src/sched/mod.rs") && note.contains("CLAUDE.md"), "{note}");
}

/// A resolvable conflict the agent keeps failing to resolve is a gate failure like any other,
/// so `max_failures` hands it to a human instead of re-dispatching it forever.
#[test]
fn repeated_unresolved_docs_conflicts_escalate_to_blocked() {
    let (mut h, gate) = gated_harness(|c| {
        resolvable(c);
        c.gate.max_failures = 2;
    });
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Conflict {
        paths: vec!["docs/coding-guidelines.md".into()],
        base_sha: BASE.into(),
    }));

    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::RetryQueued);

    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 1);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released, "the second unresolved conflict blocks");
    assert!(h.sched.store().all_retries().unwrap().is_empty(), "no third try");
    let note = st.last_error.expect("the escalation must say why");
    assert!(note.contains("2 time(s)") && note.contains("docs/coding-guidelines.md"), "{note}");
    assert!(!note.contains("git rebase --continue"), "the human is not sent the agent's brief");
    assert_eq!(gate.starts_for("iss-1").len(), 2);
}

/// A failing command is the agent's problem: the verdict is `Continue`, and the continuation
/// is handed the output — otherwise it would re-run the suite to rediscover the same failure,
/// or say `Done` again.
#[test]
fn a_failing_gate_continues_the_run_with_the_failing_output_in_hand() {
    let (mut h, gate) = gated_harness(|_| {});
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Failed {
        step: "cargo test".into(),
        output: "test a_thing ... FAILED\nassertion `left == right` failed".into(),
        on_base: true,
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::RetryQueued, "a gate failure is a continuation");
    assert_eq!(st.attempt, 0, "not a failure: the agent did what it was asked");
    let retries = h.sched.store().all_retries().unwrap();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].due_at - h.clock.wall().0, 5_000, "the continuation delay applies");
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs[0].outcome.as_deref(), Some("continue"), "the row records the gate's word");

    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "the continuation is dispatched");
    let sessions = h.worker.sessions_for("iss-1");
    assert!(matches!(sessions[1], Session::Resume(_)), "into the same conversation");
    let feedback = h.worker.feedback_for("iss-1");
    assert_eq!(feedback[0], None, "a first dispatch has nothing to explain");
    let Some(Feedback::Gate { output }) = &feedback[1] else {
        panic!("the continuation must be told why it exists, by the gate: {:?}", feedback[1]);
    };
    assert!(output.contains("cargo test"), "which step: {output}");
    assert!(output.contains("a_thing ... FAILED"), "and what it said: {output}");
    assert!(output.contains("1 of 3"), "and how many tries are left: {output}");
    assert!(output.contains("sits on the base"), "and where the work now is: {output}");
}

/// The brief must describe the tree the agent will actually find. A base that will not
/// resolve, and a rebase refused and therefore aborted, both leave the branch exactly where the
/// agent left it — so a brief that says "the branch now sits on the base, fix this on top of it"
/// sends it looking for a state that does not exist. An agent that cannot reconcile the
/// instruction with what it sees reports `Done` again unchanged, and the gate fails it again,
/// which spends the `max_failures` allowance on a sentence rather than on the work.
#[test]
fn a_gate_that_failed_before_rebasing_does_not_tell_the_agent_its_branch_was_rebased() {
    let (mut h, gate) = gated_harness(|_| {});
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Failed {
        step: "resolve base release-1.2".into(),
        output: "cannot resolve rebase base `release-1.2`".into(),
        on_base: false,
    }));
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();

    // Delivery folded the gate's brief into `Feedback::Gate`; the fact under test is the same.
    let brief = match &h.worker.feedback_for("iss-1")[1] {
        Some(Feedback::Gate { output }) => output.clone(),
        other => panic!("the continuation must be told why it exists, got {other:?}"),
    };
    assert!(
        !brief.contains("sits on the base"),
        "nothing was rebased, so the brief must not claim the branch is on the base: {brief}"
    );
    assert!(
        brief.contains("where you left it"),
        "and must say where the work actually is: {brief}"
    );
    assert!(brief.contains("resolve base release-1.2"), "which step failed: {brief}");
}

/// The gate cannot be a runaway of its own: an agent that cannot make the suite pass gets
/// `max_failures` consecutive tries and is then handed to a human, well inside the turn budget.
#[test]
fn repeated_gate_failures_escalate_to_blocked_rather_than_looping() {
    let (mut h, gate) = gated_harness(|c| c.gate.max_failures = 2);
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Failed {
        step: "cargo clippy".into(),
        output: "error: unused variable".into(),
        on_base: true,
    }));

    // First failure: a continuation.
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::RetryQueued);

    // The continuation runs, says Done again, and fails the gate again.
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 1);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released, "the second failure blocks");
    assert!(h.sched.store().all_retries().unwrap().is_empty(), "no third try");
    assert!(!st.is_quarantined(), "blocked, not quarantined: the agent did nothing wrong");
    let note = st.last_error.expect("the escalation must say why");
    assert!(note.contains("2 time(s)") && note.contains("cargo clippy"), "{note}");
    assert_eq!(gate.starts_for("iss-1").len(), 2, "exactly max_failures gates were run");

    for _ in 0..5 {
        h.clock.advance_ms(60_000);
        h.sched.tick().unwrap();
        assert_eq!(h.sched.running_count() + h.sched.gating_count(), 0);
    }
}

/// A restart changes nothing about an issue whose suite keeps failing, so it must not forgive
/// the streak: held in memory, every restart handed the agent a fresh `max_failures` (#46).
#[test]
fn a_gate_failure_streak_is_not_forgiven_by_restarting_the_daemon() {
    let dir = tmp_dir("gate-streak-restart");
    let db = dir.join("crew.db");
    let root = dir.join("workspaces");
    let failing = GateScript::passes_in(1_000).with_verdict(GateVerdict::Failed {
        step: "cargo test".into(),
        output: "boom".into(),
        on_base: true,
    });
    let start = |failing: &GateScript| {
        let mut h = harness_over(
            vec![issue(1, "In Progress", Some(1))],
            root.clone(),
            Store::open(&db).unwrap(),
            Arc::new(DirWorkspace::new(&root).unwrap()),
            |c| c.gate.max_failures = 2,
        );
        let gate = Arc::new(FakeGate::new(h.clock.clone()));
        gate.set_default(failing.clone());
        h.sched.set_gate(Some(gate.clone()));
        h.worker.set_default(Script::succeeds_in(1_000));
        (h, gate)
    };

    {
        let (mut h, _gate) = start(&failing);
        dispatch_and_finish(&mut h);
        h.clock.advance_ms(1_000);
        h.sched.tick().unwrap();
        assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::RetryQueued);
    }

    let (mut h, gate) = start(&failing);
    // The new process's clock starts where the old one did, so the retry's wall-clock due time
    // is still ahead of it by the time the first process spent.
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "the continuation survives the restart");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 1);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released, "failure 2 of 2 blocks, restart or not");
    assert!(h.sched.store().all_retries().unwrap().is_empty(), "no third try");
    let note = st.last_error.expect("the escalation must say why");
    assert!(note.contains("2 time(s)"), "{note}");
    assert_eq!(gate.starts_for("iss-1").len(), 1, "one gate in this process, one before it");

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A pass resets the streak: two failures separated by a pass are not an escalation.
#[test]
fn a_passing_gate_resets_the_failure_streak() {
    let (mut h, gate) = gated_harness(|c| {
        c.gate.max_failures = 2;
        c.tracker.active_states = vec!["in progress".into(), "in review".into()];
    });
    let failing = GateScript::passes_in(1_000).with_verdict(GateVerdict::Failed {
        step: "cargo test".into(),
        output: "boom".into(),
        on_base: true,
    });

    gate.set_default(failing.clone());
    dispatch_and_finish(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // failure 1 → continue

    gate.set_default(GateScript::passes_in(1_000));
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap(); // continuation dispatched
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // done → gate
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap(); // pass → parked
    assert_eq!(
        h.sched.store().get("iss-1").unwrap().unwrap().parked_state.as_deref(),
        Some("in progress")
    );

    // A human moves the ticket; the next run fails its gate once. That is failure 1 again.
    gate.set_default(failing);
    h.tracker.set_state("iss-1", "In Review");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(
        st.phase,
        Phase::RetryQueued,
        "one failure after a pass is a continuation, not a block"
    );
}

/// A gate that hangs — a wedged `cargo test` — is bounded by the timeout, and the timeout is
/// a failure the agent hears about, not a silent kill.
#[test]
fn a_gate_that_hangs_is_killed_at_the_timeout_and_counts_as_a_failure() {
    let (mut h, gate) = gated_harness(|c| c.gate.timeout_ms = 10_000);
    gate.set_default(GateScript::hangs());
    dispatch_and_finish(&mut h);

    h.clock.advance_ms(10_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 1, "at the limit, not past it");

    h.clock.advance_ms(1);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 0, "past it, the gate is stopped");
    let retries = h.sched.store().all_retries().unwrap();
    assert_eq!(retries.len(), 1, "and the run continues");
    let reason = retries[0].reason.clone().unwrap_or_default();
    assert!(reason.contains("still running after 10000 ms"), "the agent is told why: {reason}");
}

/// The same rule as for a running agent: a ticket that closes takes the worktree with it, but
/// only once whatever is running in that worktree is confirmed stopped.
#[test]
fn a_ticket_closed_while_its_gate_runs_stops_the_gate_and_reclaims_the_worktree() {
    let (mut h, gate) = gated_harness(|_| {});
    gate.set_default(GateScript::hangs());
    let ws = dispatch_and_finish(&mut h);
    assert_eq!(h.sched.gating_count(), 1);

    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.gating_count(), 0, "the gate is stopped");
    assert!(!ws.exists(), "and the worktree reclaimed");
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released);
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs[0].outcome.as_deref(), Some("killed"), "the open run row is closed");
}

#[test]
fn shutdown_stops_a_gate_in_flight_and_releases_its_claim() {
    let (mut h, gate) = gated_harness(|_| {});
    gate.set_default(GateScript::hangs());
    let ws = dispatch_and_finish(&mut h);

    h.sched.shutdown().unwrap();

    assert_eq!(h.sched.gating_count(), 0);
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::Released);
    assert!(ws.exists(), "the worktree is kept for the next start, like a stopped run's");
}

/// The scheduler with no gate attached is the scheduler as it was: `Done` is applied as
/// reported. Every earlier test in this file runs that way, so this only pins the fact.
#[test]
fn without_a_gate_a_done_verdict_is_applied_as_the_agent_reported_it() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000));
    dispatch_and_finish(&mut h);
    assert_eq!(h.sched.gating_count(), 0);
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::Released);
}

// ---- failures ---------------------------------------------------------------

#[test]
fn a_permanent_failure_quarantines_immediately_rather_than_retrying_forever() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000).with_outcome(Outcome::Failed {
        class: ErrorClass::TemplateRender,
        msg: "unknown variable".into(),
    }));

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(st.is_quarantined());
    assert!(h.sched.store().all_retries().unwrap().is_empty());

    // And it stays out of the rotation on later ticks.
    h.clock.advance_ms(600_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0);
}

/// The model a run names is history (#36): changing the setting under a run in flight, or before
/// the next one, must not rewrite what the earlier run reports it was dispatched with.
#[test]
fn a_run_records_the_model_it_was_dispatched_with_rather_than_the_current_default() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    let pinned =
        ModelChoice { model: Some("claude-opus-5-5".into()), effort: Some(Effort::Medium) };
    h.worker.set_model(pinned);
    h.worker.set_default(
        Script::succeeds_in(10_000).with_outcome(Outcome::Continue { why: "more".into() }),
    );

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    // The operator unpins while the first run is still going.
    h.worker.set_model(ModelChoice::default());
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let runs = h.sched.snapshot().unwrap().rows[0].runs.clone();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(runs[0].effort.as_deref(), Some("medium"));

    // Finish it, and let the continuation dispatch under the new (absent) setting.
    for _ in 0..6 {
        h.clock.advance_ms(31_000);
        h.sched.tick().unwrap();
    }
    let runs = h.sched.snapshot().unwrap().rows[0].runs.clone();
    assert!(runs.len() >= 2, "expected the continuation to have dispatched: {runs:?}");
    let (newest, first) = (&runs[0], runs.last().unwrap());
    assert_eq!((newest.model.as_deref(), newest.effort.as_deref()), (None, None));
    assert_eq!(first.model.as_deref(), Some("claude-opus-5-5"), "history, not current config");
    assert_eq!(first.effort.as_deref(), Some("medium"));
}

/// A silent fallback to the default would make the recorded model a lie; the refusal has to
/// take the permanent path instead (#36).
#[test]
fn a_model_the_cli_refuses_quarantines_the_issue_instead_of_retrying_it() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000).with_outcome(Outcome::Failed {
        class: ErrorClass::ModelNotFound,
        msg: "There's an issue with the selected model".into(),
    }));

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert!(h.sched.store().get("iss-1").unwrap().unwrap().is_quarantined());
    assert!(h.sched.store().all_retries().unwrap().is_empty());
}

#[test]
fn a_quarantined_issue_returns_to_service_once_an_operator_clears_it() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::AuthFailed, msg: "401".into() }),
    );

    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().is_quarantined());

    h.worker.set_default(Script::succeeds_in(1_000));
    h.sched.unquarantine("iss-1").unwrap();
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "cleared issues become dispatchable again");
}

#[test]
fn repeated_transient_failures_escalate_then_quarantine() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.quarantine_after_identical = 3;
    });
    h.worker.set_default(
        Script::succeeds_in(500)
            .with_outcome(Outcome::Failed { class: ErrorClass::AgentCrash, msg: "boom".into() }),
    );

    let mut delays = Vec::new();
    for _ in 0..3 {
        h.sched.tick().unwrap();
        h.clock.advance_ms(500);
        h.sched.tick().unwrap();
        if let Some(r) = h.sched.store().all_retries().unwrap().first() {
            delays.push(r.due_at - h.clock.wall().0);
        }
        h.clock.advance_ms(400_000); // past any scheduled backoff
    }

    assert_eq!(delays, vec![10_000, 20_000], "backoff doubles between attempts");
    assert!(
        h.sched.store().get("iss-1").unwrap().unwrap().is_quarantined(),
        "an identical failure three times running is not transient"
    );
}

#[test]
fn a_stalled_run_is_terminated_and_retried() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.stall_timeout_ms = 10_000;
    });
    h.worker.set_default(Script::stalls_after(1_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    h.clock.advance_ms(2_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "still producing output; not yet stalled");

    h.clock.advance_ms(30_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "silence past the timeout must terminate the run");
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.last_error_class, Some(ErrorClass::Stall));
}

// ---- reconciliation ---------------------------------------------------------

#[test]
fn a_ticket_moving_to_terminal_stops_the_run_and_cleans_up() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    let ws = h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap();
    assert!(PathBuf::from(&ws).exists());

    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    assert!(!PathBuf::from(&ws).exists(), "terminal issues have their workspace removed");
}

#[test]
fn a_ticket_leaving_the_active_set_stops_the_run_but_keeps_the_workspace() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    let ws = h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap();

    h.tracker.set_state("iss-1", "Backlog"); // neither active nor terminal
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    assert!(PathBuf::from(&ws).exists(), "non-terminal exits must not discard warm state");
}

/// A `RunHandle` outlives the `Scheduler` unless something kills it explicitly — found
/// empirically the first time a real worker process was left running after `cargo run`
/// returned with a run still in flight, because nothing had ever called `terminate` on it.
#[test]
fn shutdown_stops_every_in_flight_run_without_discarding_its_workspace() {
    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(1))], |c| {
            c.agent.max_concurrent = 2;
        });
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2);
    let workspaces: Vec<PathBuf> = h
        .sched
        .snapshot()
        .unwrap()
        .rows
        .iter()
        .map(|r| PathBuf::from(r.workspace.clone().unwrap()))
        .collect();

    h.sched.shutdown().unwrap();

    assert_eq!(h.sched.running_count(), 0);
    for ws in &workspaces {
        assert!(ws.exists(), "shutdown must not discard warm state, only stop the run");
    }

    // The claim must be released too, or the issue would never be dispatchable again.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 2, "a released issue is picked back up on the next tick");
}

/// The spec kills on the first refresh miss, so a single eventual-consistency blip destroys
/// in-flight work.
#[test]
fn one_invisible_refresh_is_survivable_but_two_are_not() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.refresh_miss_grace = 2;
    });
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    h.tracker.hide("iss-1");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "one miss is within grace");

    h.tracker.unhide("iss-1");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "reappearing resets the grace count");

    h.tracker.hide("iss-1");
    for _ in 0..2 {
        h.clock.advance_ms(1_000);
        h.sched.tick().unwrap();
    }
    assert_eq!(h.sched.running_count(), 0, "two consecutive misses is gone");
}

#[test]
fn a_tracker_outage_keeps_existing_workers_alive() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    h.tracker.fail_by_ids(Some(TrackerError::RateLimited));
    h.tracker.fail_by_states(Some(TrackerError::RateLimited));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 1, "an outage must not cancel real work");
    assert!(h.sched.snapshot().unwrap().last_error.is_some(), "but it must be visible");
}

// ---- config gating ----------------------------------------------------------

#[test]
fn a_broken_config_stops_dispatch_but_still_reconciles() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);

    // Break it the way a bad hot-reload would.
    h.sched.cfg.tracker.active_states.clear();

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    // The finished run was still harvested despite dispatch being gated off.
    assert_eq!(h.sched.running_count(), 0);
    let snap = h.sched.snapshot().unwrap();
    assert!(snap.last_error.unwrap().contains("preflight"));
}

#[test]
fn nothing_is_dispatched_twice_across_repeated_ticks() {
    let issues = (1..=3).map(|n| issue(n, "In Progress", Some(1))).collect();
    let mut h = harness(issues, |c| c.agent.max_concurrent = 5);
    h.worker.set_default(Script::succeeds_in(600_000));

    for _ in 0..5 {
        h.sched.tick().unwrap();
        h.clock.advance_ms(100);
    }

    assert_eq!(h.sched.running_count(), 3, "claims must make re-dispatch impossible");
    let snap = h.sched.snapshot().unwrap();
    assert_eq!(snap.rows.iter().filter(|r| r.phase == Phase::Running).count(), 3);
}

// ---- startup recovery -------------------------------------------------------

fn tmp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("crew-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn git(at: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").arg("-C").arg(at).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

/// A throwaway repo with one commit, so `git worktree add` has a HEAD to branch from. Identity
/// is set repo-local rather than relying on the machine having a global one.
fn git_repo(at: &Path) -> PathBuf {
    std::fs::create_dir_all(at).unwrap();
    git(at, &["init", "-q", "-b", "main"]);
    git(at, &["config", "user.email", "test@example.com"]);
    git(at, &["config", "user.name", "test"]);
    git(at, &["commit", "-q", "--allow-empty", "-m", "init"]);
    at.canonicalize().unwrap()
}

/// Commit inside a worktree, standing in for work a dispatched agent finished before the kill.
fn commit_in(worktree: &Path, file: &str, msg: &str) {
    std::fs::write(worktree.join(file), msg.as_bytes()).unwrap();
    git(worktree, &["add", file]);
    git(worktree, &["commit", "-q", "-m", msg]);
}

fn git_out(at: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(at).args(args).output().unwrap();
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Issue #22. A run stopped mid-flight by its ticket closing is killed and then has its worktree
/// removed, and whatever it had not committed used to go with the directory. It must land on the
/// side ref instead, leave the run's branch alone, and be named to the next run of the issue.
#[test]
fn a_run_killed_with_uncommitted_changes_has_them_recoverable_after_its_workspace_is_removed() {
    let dir = tmp_dir("killed-wip");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));
    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |_| {},
    );
    h.worker.set_default(Script::succeeds_in(600_000));
    h.sched.tick().unwrap();

    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    commit_in(&ws, "work.txt", "committed before the kill");
    let head = git_out(&ws, &["rev-parse", "HEAD"]).unwrap();
    let branch = git_out(&ws, &["symbolic-ref", "--short", "HEAD"]).unwrap();
    std::fs::write(ws.join("half.txt"), b"not yet committed").unwrap();

    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "the run is killed");
    assert!(!ws.exists(), "and its worktree removed");

    let prefix = GitWorktreeWorkspace::wip_prefix("iss-1");
    let refs = git_out(&repo, &["for-each-ref", "--format=%(refname)", &prefix]).unwrap();
    let wip = refs.lines().next().expect("a snapshot ref must exist").to_string();
    assert_eq!(
        git_out(&repo, &["show", &format!("{wip}:half.txt")]).as_deref(),
        Some("not yet committed"),
        "the uncommitted work must be recoverable from the snapshot ref"
    );
    assert_eq!(git_out(&repo, &["rev-parse", &branch]), Some(head), "the branch is untouched");

    h.tracker.set_state("iss-1", "In Progress");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);
    assert!(!ws.join("half.txt").exists(), "nothing is applied to the new worktree");
    let told = h.worker.wips_for("iss-1");
    assert_eq!(told.first(), Some(&vec![]), "the first run had no snapshot to be told about");
    let [last] = told.last().unwrap().as_slice() else { panic!("the next run must be told") };
    assert_eq!(last.ref_name, wip);
    assert!(last.diffstat.contains("half.txt"), "diffstat: {}", last.diffstat);

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A restarted process gets a `FakeClock` that starts where the dead one's did, which no real
/// restart does. Run ids are `issue_id`-plus-wall-millisecond, so without this the re-dispatch
/// collides with the row the interrupted run left behind.
fn restart_took_time(h: &Harness) {
    h.clock.advance_ms(1_000);
}

/// A `SIGKILL` runs no destructor. `shutdown()`, `Drop for Scheduler` and the interrupt arm in
/// `main` all release in-flight claims; a hard kill, an OOM kill and a host reboot reach none of
/// them. `mem::forget` is the faithful simulation: the store keeps the claim, the disk keeps the
/// worktree, and the in-memory `running` map dies with the process that owned it.
#[test]
fn a_claim_stranded_by_a_hard_kill_is_recovered_at_the_next_startup() {
    let dir = tmp_dir("hard-kill");
    let db = dir.join("crew.db");
    let root = dir.join("workspaces");

    let ws = {
        let mut h = harness_over(
            vec![issue(1, "In Progress", Some(1))],
            root.clone(),
            Store::open(&db).unwrap(),
            Arc::new(DirWorkspace::new(&root).unwrap()),
            |_| {},
        );
        h.worker.set_default(Script::succeeds_in(600_000));
        h.sched.tick().unwrap();
        assert_eq!(h.sched.running_count(), 1);

        let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
        // Stands in for whatever the kill catches an agent half-way through writing.
        std::fs::write(ws.join("half-written.txt"), b"caught mid-run").unwrap();

        std::mem::forget(h); // the kill: no shutdown, no Drop, nothing released
        ws
    };

    // What the dead process left behind: a claim no live run can ever match again.
    let phase = Store::open(&db).unwrap().get("iss-1").unwrap().unwrap().phase;
    assert_eq!(phase, Phase::Running, "the claim outlives the process that took it");

    // The restart. Recovery runs inside the first tick, so this is the ordinary startup path
    // rather than a call some second entry point has to remember to make.
    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open(&db).unwrap(),
        Arc::new(DirWorkspace::new(&root).unwrap()),
        |_| {},
    );
    h.worker.set_default(Script::succeeds_in(600_000));
    restart_took_time(&h);
    h.sched.tick().unwrap();

    // Being dispatchable at all is the point: without recovery `claim()` refuses forever.
    assert_eq!(h.sched.running_count(), 1, "a stranded issue must be dispatched again");
    assert!(
        !ws.join("half-written.txt").exists(),
        "the orphaned worktree must be reconciled, not handed on as the kill left it"
    );

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The cleanup that frees a stranded issue must not be what discards what the killed run had
/// already committed. The directory is scratch space; the branch is the deliverable.
#[test]
fn a_hard_killed_runs_commits_survive_the_recovery_that_frees_its_issue() {
    let dir = tmp_dir("hard-kill-git");
    let db = dir.join("crew.db");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));

    let ws = {
        let mut h = harness_over(
            vec![issue(1, "In Progress", Some(1))],
            root.clone(),
            Store::open(&db).unwrap(),
            Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
            |_| {},
        );
        h.worker.set_default(Script::succeeds_in(600_000));
        h.sched.tick().unwrap();

        let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
        commit_in(&ws, "work.txt", "what the agent finished before the kill");
        std::fs::write(ws.join("scratch.txt"), b"never committed").unwrap();

        std::mem::forget(h);
        ws
    };

    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open(&db).unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |_| {},
    );
    h.worker.set_default(Script::succeeds_in(600_000));
    restart_took_time(&h);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 1, "the stranded issue must be dispatchable again");
    // The uncommitted file proves the worktree really was removed and rebuilt, so the committed
    // one coming back proves the branch it was removed with outlived the reconciliation.
    assert!(!ws.join("scratch.txt").exists(), "the orphaned worktree itself must be reconciled");
    assert!(ws.join("work.txt").exists(), "but the commits it held must survive that");

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Issue #25. `run.turns` used to be written once, by `finish_run`, so an in-flight run's row
/// read `0` and the live count existed only in the `RunHandle` inside `running`. That map dies
/// with the process, so after a hard kill the interrupted run reported zero turns forever and
/// the conversation `recover()` resumes had a recorded cost of nothing. The count must be
/// checkpointed as the run advances, survive the kill, and be charged to the issue's budget when
/// recovery closes the row — without the fake worker ever finishing, which is the only way to
/// prove `finish_run` is not what wrote it.
#[test]
fn a_run_interrupted_by_a_hard_kill_reports_its_last_known_turn_count_after_restart() {
    let dir = tmp_dir("hard-kill-turns");
    let db = dir.join("crew.db");
    let root = dir.join("workspaces");

    // Ten turns over 600s: one every minute, so the count is unambiguous at any tick.
    let script = Script { duration_ms: 600_000, turns: 10, ..Script::succeeds_in(600_000) };

    let (run_id, expected) = {
        let mut h = harness_over(
            vec![issue(1, "In Progress", Some(1))],
            root.clone(),
            Store::open(&db).unwrap(),
            Arc::new(DirWorkspace::new(&root).unwrap()),
            |_| {},
        );
        h.worker.set_default(script.clone());
        h.sched.tick().unwrap();
        assert_eq!(h.sched.running_count(), 1);

        // Three ticks of progress. Each one may write at most once, whatever the agent did in
        // between: the fake's count moves on every clock advance, and the store must see only
        // what the tick observed.
        for _ in 0..3 {
            h.clock.advance_ms(120_000);
            h.sched.tick().unwrap();
        }
        let snap = h.sched.snapshot().unwrap();
        let row = &snap.rows[0];
        let expected = row.turns;
        assert!(expected > 1, "the run must actually have advanced for this test to mean anything");
        assert!(row.runs[0].ended_at.is_none(), "and must still be in flight");
        assert_eq!(
            row.runs[0].turns, expected,
            "the published run record tracks the live count while the run is in flight"
        );

        // More progress the scheduler never gets to observe — the kill lands before the tick.
        h.clock.advance_ms(60_000);
        assert!(h.sched.running_count() == 1);
        let run_id = row.runs[0].run_id.clone();
        std::mem::forget(h); // no shutdown, no Drop: nothing gets a final `finish_run`
        (run_id, expected)
    };

    // The row the kill left behind: still open, but carrying the last checkpoint, not zero.
    let orphan = Store::open(&db).unwrap().run(&run_id).unwrap().unwrap();
    assert_eq!(orphan.ended_at, None);
    assert_eq!(orphan.turns, expected, "the count must have been written before the kill");

    // The restart. Recovery closes the row and charges the issue for what the attempt reached.
    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open(&db).unwrap(),
        Arc::new(DirWorkspace::new(&root).unwrap()),
        |_| {},
    );
    h.worker.set_default(script);
    restart_took_time(&h);
    h.sched.tick().unwrap();

    let closed = h.sched.store().run(&run_id).unwrap().unwrap();
    assert_eq!(closed.outcome.as_deref(), Some("orphaned"));
    assert_eq!(closed.turns, expected, "closing the orphaned row must not reset its count");
    assert_eq!(closed.in_tok, None, "and must not invent a total the run never reported");

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(
        st.cumulative_turns, expected,
        "the interrupted attempt counts against the per-issue budget it consumed"
    );
    assert_eq!(h.sched.running_count(), 1, "and the issue is dispatched again regardless");

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The filter that makes recovery safe to run at all: a claim is stale only when *this* process
/// has no run for it. Drop the check and a recovery pass reconciles away the live work it was
/// added to rescue.
#[test]
fn recovery_leaves_the_claims_of_runs_this_process_is_still_executing_alone() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());

    h.sched.recover().unwrap();

    assert_eq!(h.sched.running_count(), 1, "a live run must not be reconciled away");
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::Running);
    assert!(ws.exists(), "nor its workspace removed out from under it");
}

// ---- published branch ---------------------------------------------------------

/// Review finding on PR #31 (`src/sched/mod.rs:895`): the published branch used to be
/// recomputed on every snapshot from `st.identifier` and "has this issue ever produced a run",
/// neither of which proves the recomputed name is the branch `prepare` actually checked out.
/// Two things break it independently: `Store::ensure` can rename `identifier` after dispatch,
/// changing what a recompute produces, and `Workspace::remove` deletes a branch that turns out
/// to carry no commits while the run history that gated the old derivation never clears. This
/// drives both: one issue's branch survives cleanup because it holds a commit, the other's does
/// not, and the surviving one's identifier is renamed after dispatch. The recorded value must
/// track what `prepare` returned and what cleanup actually did, not what a recompute would say.
#[test]
fn the_published_branch_is_the_one_prepare_recorded_not_one_recomputed_from_the_current_identifier()
{
    let dir = tmp_dir("branch-recorded-not-derived");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));

    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |c| c.agent.max_concurrent = 2,
    );
    h.worker.set_default(Script::succeeds_in(600_000));

    h.sched.tick().unwrap();
    let snap = h.sched.snapshot().unwrap();
    let row_1 = snap.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    let row_2 = snap.rows.iter().find(|r| r.issue_id == "iss-2").unwrap();
    assert!(row_1.branch.is_some(), "a dispatched run reports the branch it checked out");
    let branch_2 = row_2.branch.clone().expect("same for the second issue");
    let ws_2 = PathBuf::from(row_2.workspace.clone().unwrap());

    // iss-2's run leaves a commit, so cleanup keeps its branch; iss-1's leaves none, so cleanup
    // deletes it (git's own merged check, exercised by `GitWorktreeWorkspace::remove`).
    commit_in(&ws_2, "work.txt", "the agent's output");

    // A tracker-side rename after dispatch — the failure mode a recomputed name cannot survive.
    // `worktree_key` is fixed at the first `ensure` and ignored on conflict, so this only
    // changes what `identifier` displays as, never which worktree or branch this issue owns.
    h.sched.store().ensure(h.clock.as_ref(), "iss-2", "MT-2-renamed", "irrelevant").unwrap();

    h.tracker.set_state("iss-1", "Done");
    h.tracker.set_state("iss-2", "Done");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let snap = h.sched.snapshot().unwrap();
    let row_1 = snap.rows.iter().find(|r| r.issue_id == "iss-1").unwrap();
    let row_2 = snap.rows.iter().find(|r| r.issue_id == "iss-2").unwrap();

    assert_eq!(row_2.identifier, "MT-2-renamed", "the rename really did take hold in the store");
    assert_eq!(
        row_1.branch, None,
        "cleanup deleted the ref this run actually used; the stored value must follow, not \
         keep reporting a name that no longer exists just because the issue has run history"
    );
    assert_eq!(
        row_2.branch,
        Some(branch_2),
        "cleanup kept this branch because it holds commits, so the recorded name must survive \
         unchanged — not get recomputed from the identifier that was renamed underneath it"
    );

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- projection -------------------------------------------------------------

/// The projector every real deployment is one disk error away from.
struct FailingProjector;

impl Projector for FailingProjector {
    fn project(&self, _issues: &[crew::project::ProjectedIssue]) -> anyhow::Result<()> {
        anyhow::bail!("disk full")
    }
}

/// Everything the scheduler decides, as one comparable value per tick: the store is its
/// judgment, the retry table its timing, the snapshot what it tells the world. The snapshot
/// has no `PartialEq`, so its `Debug` form stands in — every field in it is deterministic on a
/// fake clock, including the timestamps. The one thing that legitimately differs between two
/// otherwise identical lifetimes is where on disk each one lived, so the workspace root is
/// masked out rather than letting it hide a real divergence behind an expected one.
fn decision_trace(h: &Harness, root: &Path) -> String {
    let store = h.sched.store();
    format!(
        "running={}\nstates={:?}\nretries={:?}\nsnapshot={:?}",
        h.sched.running_count(),
        store.all().unwrap(),
        store.all_retries().unwrap(),
        h.sched.snapshot().unwrap(),
    )
    .replace(&*root.to_string_lossy(), "<root>")
}

/// One scripted lifetime exercising every path `publish` runs after: a clean finish, a
/// retryable failure with its retry coming due, an immediate quarantine, a continuation, and
/// a ticket reaching a terminal state with the cleanup that triggers. Returns the trace after
/// every tick, so a divergence names the tick it happened on.
fn run_scripted_lifetime(projector: Arc<dyn Projector>, root: PathBuf) -> Vec<String> {
    let workspace = Arc::new(DirWorkspace::new(&root).unwrap());
    // `DirWorkspace` canonicalises, so the paths in the snapshot are the resolved form, not
    // the one this function was handed — on macOS that is `/private/var/...` for `/var/...`.
    let canonical_root = workspace.root().to_path_buf();
    let mut h = harness_full(
        vec![
            issue(1, "In Progress", Some(1)),
            issue(2, "In Progress", Some(2)),
            issue(3, "In Progress", Some(3)),
            issue(4, "In Progress", Some(4)),
        ],
        root,
        Store::open_in_memory().unwrap(),
        workspace,
        projector,
        |c| c.agent.max_concurrent = 4,
    );
    h.worker.script("iss-1", Script::succeeds_in(1_000));
    h.worker.script(
        "iss-2",
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::Stall, msg: "hung".into() }),
    );
    h.worker.script(
        "iss-3",
        Script::succeeds_in(1_000)
            .with_outcome(Outcome::Failed { class: ErrorClass::AuthFailed, msg: "401".into() }),
    );
    h.worker.script(
        "iss-4",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more".into() }),
    );

    let mut trace = Vec::new();
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    // Every run finishes; verdicts land: done, retry scheduled, quarantined, continuation.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    // A human closes the finished one; cleanup fires.
    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    // Far enough for both the continuation and the first backoff to come due.
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    trace.push(decision_trace(&h, &canonical_root));

    trace
}

/// The projection is a view. Whether it writes real task files, fails on every call, or is
/// switched off entirely, the scheduler must reach the same decisions on the same ticks —
/// otherwise the dashboard has become load-bearing, which is the failure this invariant
/// exists to make impossible. Proved by trace, not by inspection: the same scripted lifetime
/// runs three times and the traces are required to be identical.
#[test]
fn the_scheduler_makes_the_same_decisions_whether_the_projector_writes_fails_or_is_off() {
    let dir = tmp_dir("projector-bit-identical");

    let off = run_scripted_lifetime(Arc::new(NoopProjector), dir.join("ws-off"));

    let failing = run_scripted_lifetime(Arc::new(FailingProjector), dir.join("ws-failing"));
    assert_eq!(off, failing, "a projector that fails on every call must not change one decision");

    let tasks_root = dir.join("tasks");
    let real = TasksProjector::new(&tasks_root, "sess").unwrap();
    let written = run_scripted_lifetime(Arc::new(real), dir.join("ws-real"));
    assert_eq!(off, written, "a projector that writes real files must not change one decision");

    // Sanity: the real one did write — otherwise the third leg tested the same thing as the
    // first. Four issues, none ever dropped from the store, so four files.
    assert_eq!(
        std::fs::read_dir(tasks_root.join("sess")).unwrap().count(),
        4,
        "the real projector must have projected, or this test proves nothing about it"
    );

    // Each tick must also have differed from the last, or the trace is too coarse to catch a
    // divergence that happened to land on a quiet tick.
    for w in off.windows(2) {
        assert_ne!(w[0], w[1], "every scripted tick should move the scheduler somewhere new");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- transcripts ------------------------------------------------------------

/// A transcript root under the harness's own directory, so it is cleaned up with it.
fn transcript_root(h: &Harness) -> PathBuf {
    h.root.join(".transcripts")
}

#[test]
fn a_completed_run_leaves_a_readable_transcript_reachable_from_its_run_record() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    let root = transcript_root(&h);
    h.sched.set_transcripts(Some(Transcripts::new(&root, 1 << 20, 10).unwrap()));

    h.sched.tick().unwrap();
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::Released);

    // The run record is the entry point — nobody should have to know the layout on disk.
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome.as_deref(), Some("done"));
    let path = PathBuf::from(runs[0].transcript.clone().expect("the run recorded a transcript"));

    let text = std::fs::read_to_string(&path).expect("and the transcript is readable");
    assert!(text.contains("crew_run_start"), "got: {text}");
    assert!(text.contains("iss-1"));
    assert!(text.lines().count() > 1, "a transcript with only a header records nothing");
}

#[test]
fn a_transcript_root_that_cannot_be_written_costs_the_record_and_not_the_dispatch() {
    let h0 = harness(vec![], |_| {});
    // A *file* where the root should be: `create_dir_all` fails, and so would every open under
    // it. The degrade has to be silent enough that dispatch never notices.
    std::fs::create_dir_all(&h0.root).unwrap();
    let blocked = h0.root.join("not-a-directory");
    std::fs::write(&blocked, b"").unwrap();
    assert!(Transcripts::new(&blocked, 1 << 20, 10).is_err());
    drop(h0);

    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |_| {});
    h.sched.set_transcripts(None);

    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "a missing transcript must never cost a dispatch");

    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    let runs = h.sched.store().runs_for("iss-1").unwrap();
    assert_eq!(runs[0].outcome.as_deref(), Some("done"));
    assert_eq!(runs[0].transcript, None, "and the row says plainly that there is none");
}

/// Retention has to bound the directory without ever unlinking a file a live run is writing
/// to — and a stalled run is both live and, by definition, the oldest file there.
#[test]
fn retention_bounds_the_transcript_directory_but_spares_a_stalled_runs_own_file() {
    const KEEP: usize = 2;

    let mut h =
        harness(vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))], |c| {
            c.agent.stall_timeout_ms = 3_600_000
        });
    let root = transcript_root(&h);
    // Deliberately far below the number of runs this test produces, so pruning has to fire.
    h.sched.set_transcripts(Some(Transcripts::new(&root, 1 << 20, KEEP).unwrap()));

    // iss-1 goes quiet at once and never finishes — live throughout, and its file never gets a
    // second write, so it ages to the back of the directory while the process behind it runs.
    h.worker.script("iss-1", Script::stalls_after(0));
    // iss-2 keeps asking for another turn, so every retry opens a fresh run and a fresh file.
    h.worker.script(
        "iss-2",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );

    h.sched.tick().unwrap();
    let stalled =
        PathBuf::from(h.sched.store().runs_for("iss-1").unwrap()[0].transcript.clone().unwrap());
    assert!(stalled.exists());

    // Past the escalating continuation backoff each time, so each pass really re-dispatches.
    for _ in 0..6 {
        h.clock.advance_ms(600_000);
        h.sched.tick().unwrap();
    }

    let created = h.sched.store().runs_for("iss-2").unwrap().len();
    assert!(created > KEEP + 1, "the test needs more runs than the bound: {created}");

    let kept = std::fs::read_dir(&root).unwrap().count();
    assert!(kept < created, "retention never fired: {kept} files for {created} runs");
    // The bound, plus the one file retention is not allowed to touch.
    assert!(kept <= KEEP + 1, "retention is not bounding the directory: {kept} files");
    assert!(
        stalled.exists(),
        "a live run's transcript must survive retention, however old the file looks"
    );
}

// ---- delivery ------------------------------------------------------------------

use crew::forge::fake::{FakeForge, Op};
use crew::forge::{CiStatus, ForgeError, PrState, Publisher};
use crew::model::{Feedback, ReviewVerdict, Verdict};
use crew::workspace::{Prepared, Removed, WorkspaceError};

/// A plain-directory workspace that names a branch, the way a git one would.
///
/// Delivery is keyed on the branch `prepare` recorded, and `DirWorkspace` honestly reports
/// none. These tests are about the scheduler's decisions, not git's, so the wrapper gives the
/// scheduler a name to deliver without paying for a real repository per test.
struct NamedBranches(DirWorkspace);

impl Workspace for NamedBranches {
    fn prepare(&self, issue_id: &str, identifier: &str) -> Result<Prepared, WorkspaceError> {
        let p = self.0.prepare(issue_id, identifier)?;
        Ok(Prepared { branch: self.branch_for(issue_id, identifier), ..p })
    }
    fn remove(&self, issue_id: &str, identifier: &str) -> Result<Removed, WorkspaceError> {
        self.0.remove(issue_id, identifier)
    }
    fn path_for(&self, issue_id: &str, identifier: &str) -> PathBuf {
        self.0.path_for(issue_id, identifier)
    }
    fn branch_for(&self, issue_id: &str, identifier: &str) -> Option<String> {
        Some(format!("crew/{}", crew::model::worktree_key(issue_id, identifier)))
    }
}

/// A harness with delivery on: a fake forge that reports green CI and attaches reviewers
/// unless told otherwise, polled every second so a test can advance the clock past it cheaply.
fn delivery_harness(
    issues: Vec<Issue>,
    store: Store,
    tune: impl FnOnce(&mut Config),
) -> (Harness, Arc<FakeForge>) {
    delivery_harness_with(issues, store, Arc::new(FakeForge::new()), tune)
}

/// The same, over a forge the caller already holds — what a restart looks like from the
/// provider's side: the pull requests it has are still there, only the process is new.
fn delivery_harness_with(
    issues: Vec<Issue>,
    store: Store,
    forge: Arc<FakeForge>,
    tune: impl FnOnce(&mut Config),
) -> (Harness, Arc<FakeForge>) {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!("crew-deliver-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let workspace = Arc::new(NamedBranches(DirWorkspace::new(&root).unwrap()));

    let mut h = harness_over(issues, root, store, workspace, |c| {
        c.delivery.enabled = true;
        c.delivery.poll_interval_ms = 1_000;
        tune(c);
    });
    let publisher: Arc<dyn Publisher> = forge.clone();
    h.sched.set_delivery(Some(forge.clone()), Some(publisher));
    h.worker.set_default(Script::succeeds_in(1_000));
    (h, forge)
}

fn delivery_of(h: &Harness, id: &str) -> crew::store::DeliveryRecord {
    h.sched.store().delivery(id).unwrap().expect("a delivery row")
}

/// Run one attempt to completion and harvest it: dispatch, let the fake finish, tick again.
fn run_once(h: &mut Harness) {
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
}

#[test]
fn a_run_that_finishes_leaves_an_open_pull_request_not_only_a_branch() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );

    run_once(&mut h);

    let prs = forge.open_prs();
    assert_eq!(prs.len(), 1, "a done run must leave a pull request, got {:?}", forge.ops());
    let spec = forge.spec_of(prs[0].number).unwrap();
    assert!(spec.head.starts_with("crew/MT-1-"), "opened from the run's branch: {}", spec.head);
    assert_eq!(spec.base, "master");
    assert!(spec.title.contains("MT-1"), "{}", spec.title);
    assert!(
        spec.body.contains("do the work"),
        "the body lists the branch's commits: {}",
        spec.body
    );
    assert!(spec.body.contains("crewd"), "and says who opened it: {}", spec.body);

    // The push comes before the pull request, and the snapshot carries both.
    assert!(matches!(forge.ops()[0], Op::Publish { .. }));
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::Ready, "green CI, nothing outstanding");
    let row = &h.sched.snapshot().unwrap().rows[0];
    assert_eq!(row.delivery.as_ref().unwrap().pr_url.as_deref(), Some(prs[0].url.as_str()));
    // Delivery does not disturb the parked issue: it is still released, still parked.
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::Released);
}

/// The done bar this project states: a red gate is a `Continue`, never a `Done`. The run said
/// done; CI disagreed; the issue must go back to an agent with the failure in its prompt rather
/// than rest as finished.
#[test]
fn a_red_ci_gate_re_dispatches_the_issue_with_the_failure_in_the_prompt_and_the_run_does_not_rest_on_done()
 {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    forge.red_ci(
        &FakeForge::head_after_publish(1),
        "error[E0308]: mismatched types\n --> src/x.rs:4:5",
    );

    run_once(&mut h);

    // Not resting: the retry delivery queued was due at once, so by the end of the tick that
    // harvested the "done" the fix round is already running — into the same conversation,
    // with the failure in hand.
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Running, "a red gate must send the issue back, not park it");
    assert!(st.parked_state.is_none(), "and lift the park so the retry is not refused");
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Redispatched);
    let fb = h.worker.feedback_for("iss-1");
    assert_eq!(fb.len(), 2, "one first run, one fix round: {fb:?}");
    assert!(fb[0].is_none(), "the first run had nothing to be told");
    match &fb[1] {
        Some(Feedback::Ci { failures, .. }) => {
            assert!(
                failures[0].detail.contains("E0308"),
                "the cause reaches the agent: {failures:?}"
            );
        }
        other => panic!("the fix round must be handed the CI failure, got {other:?}"),
    }
    let sessions = h.worker.sessions_for("iss-1");
    assert!(
        matches!(sessions[1], Session::Resume(_)),
        "a fix round resumes, it does not re-orient"
    );

    // The fix lands green, and only then does the issue come to rest.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);
    assert_eq!(forge.open_prs().len(), 1, "the same pull request, updated, not a second one");
    assert_eq!(delivery_of(&h, "iss-1").rounds_pr, 1);
}

/// A delivery harness with a gate that passes in a second, and CI that never reports, so any
/// CI wait delivery starts is visible on the row.
fn gated_delivery_harness() -> (Harness, Arc<FakeForge>, Arc<FakeGate>) {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    let gate = Arc::new(FakeGate::new(h.clock.clone()));
    h.sched.set_gate(Some(gate.clone()));
    forge.set_ci_default(None);
    (h, forge, gate)
}

/// #159: GitHub runs no CI on a pull request that cannot merge, so waiting for it ended at the
/// CI timeout with "CI reported nothing". The conflict is the gate's to answer, at once, for one
/// round, and a re-gate that passes is delivered again without an agent turn being spent.
#[test]
fn a_pull_request_the_provider_reports_conflicting_is_re_gated_rather_than_awaiting_ci() {
    let (mut h, forge, gate) = gated_delivery_harness();
    forge.set_mergeable_default(Some(false));

    // Dispatch, the agent's `Done` goes to the gate, the gate passes and delivery opens a pull
    // request the provider says cannot merge.
    run_once(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let d = delivery_of(&h, "iss-1");
    assert_eq!(gate.starts_for("iss-1").len(), 2, "the conflicting branch is gated again");
    assert_eq!(h.sched.gating_count(), 1);
    assert_eq!(d.stage, crew::store::DeliveryStage::Redispatched);
    assert_eq!((d.rounds_pr, d.rounds_issue), (1, 1), "one delivery round, charged once");
    assert_eq!(d.ci_pending, None, "no CI wait starts on a pull request that cannot merge");
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().phase, Phase::Running);

    // The re-gate passes: the rebased branch is pushed to the same pull request, and only now
    // is CI waited on — for the new head.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::Awaiting);
    assert_eq!(d.ci_pending.map(|(head, _)| head), Some(FakeForge::head_after_publish(2)));
    assert_eq!(forge.open_prs().len(), 1, "the same pull request, updated");
    assert_eq!(h.worker.feedback_for("iss-1").len(), 1, "no agent run was spent on it");
    assert_eq!(d.rounds_issue, 1);
}

/// #159: `mergeable: null` is the provider still computing after a push, not a conflict.
#[test]
fn a_pull_request_whose_mergeability_is_unknown_keeps_waiting() {
    let (mut h, forge, gate) = gated_delivery_harness();
    forge.set_mergeable_default(None);

    run_once(&mut h);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let reads = forge.pr_reads();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let d = delivery_of(&h, "iss-1");
    assert!(forge.pr_reads() > reads, "asked again on the next poll");
    assert_eq!(d.stage, crew::store::DeliveryStage::Awaiting, "still waiting on CI");
    assert!(d.ci_pending.is_some(), "the CI wait runs as for any mergeable pull request");
    assert_eq!((d.rounds_pr, d.rounds_issue), (0, 0));
    assert_eq!(gate.starts_for("iss-1").len(), 1, "not re-gated");
    assert_eq!(h.sched.gating_count() + h.sched.running_count(), 0);
}

/// With no gate to re-run, the conflict goes to the agent as a round of its own rather than
/// waiting on CI that will not come.
#[test]
fn a_conflicting_pull_request_with_no_gate_is_handed_back_to_the_agent() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    forge.set_ci_default(None);
    forge.set_mergeable_default(Some(false));

    run_once(&mut h);

    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::Redispatched);
    assert_eq!(d.ci_pending, None);
    assert!(
        matches!(&h.worker.feedback_for("iss-1")[1], Some(Feedback::Gate { output }) if output.contains("cannot merge")),
        "{:?}",
        h.worker.feedback_for("iss-1")
    );
}

const HOUR_MS: u64 = 60 * 60 * 1_000;

fn copilot_running() -> CiStatus {
    CiStatus::Pending { running: vec!["copilot-pull-request-reviewer".into()] }
}

/// #105: a ready pull request was handed off for "CI reported nothing" a minute into a check
/// re-run, because the wait was timed from crewd's push hours earlier. The wait belongs to the
/// re-run, and a re-run that finishes green stops it, so the next one starts from zero too.
#[test]
fn a_check_re_run_on_a_ready_pull_request_does_not_hand_it_off() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);
    let head = FakeForge::head_after_publish(1);

    h.clock.advance_ms(6 * HOUR_MS);
    forge.set_ci(&head, copilot_running());
    h.sched.tick().unwrap();
    assert_eq!(
        delivery_of(&h, "iss-1").stage,
        crew::store::DeliveryStage::Ready,
        "a check that starts again is not CI that reported nothing"
    );

    // Green again, then a second re-run: the first re-run's wait does not carry over.
    h.clock.advance_ms(HOUR_MS * 3 / 4);
    h.sched.tick().unwrap();
    forge.set_ci(&head, CiStatus::Success);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    forge.set_ci(&head, copilot_running());
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(HOUR_MS * 3 / 4);
    h.sched.tick().unwrap();
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);

    h.clock.advance_ms(HOUR_MS / 4 + 1_000);
    h.sched.tick().unwrap();
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::HandedOff, "the timeout still holds");
    let why = d.handoff_reason.expect("a reason");
    assert!(why.contains("no check run completed"), "says what was observed: {why}");
    assert!(why.contains("copilot-pull-request-reviewer"), "names the pending run: {why}");
}

/// #105: a head the operator pushed — a merge of the base to clear a conflict — was handed
/// off ninety seconds into its CI run, timed from crewd's own earlier push of another head.
/// Here crewd's head is already most of the way through its wait when the operator's lands.
#[test]
fn a_head_the_operator_pushed_gets_its_own_ci_wait() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    forge.set_ci(&FakeForge::head_after_publish(1), CiStatus::Pending { running: vec![] });
    run_once(&mut h);
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Awaiting);
    let number = forge.open_prs()[0].number;

    h.clock.advance_ms(HOUR_MS * 3 / 4);
    h.sched.tick().unwrap();
    forge.push_head(number, "operator-merge");
    forge.set_ci("operator-merge", CiStatus::Pending { running: vec!["ci".into()] });
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(HOUR_MS - 1_000);
    h.sched.tick().unwrap();
    assert_eq!(
        delivery_of(&h, "iss-1").stage,
        crew::store::DeliveryStage::Awaiting,
        "the operator's head is waited on for the full timeout, not what was left of crewd's"
    );

    h.clock.advance_ms(2_000);
    h.sched.tick().unwrap();
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::HandedOff);
    assert!(d.handoff_reason.unwrap().contains("operator-merge"), "names the head it waited on");
}

/// Review on #121: the CI wait is an interval, so it is timed on the monotonic clock — a wall
/// clock stepped forward by NTP must not hand off a pull request whose checks are still running.
#[test]
fn a_wall_clock_step_does_not_cut_a_ci_wait_short() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    forge.set_ci(&FakeForge::head_after_publish(1), copilot_running());
    run_once(&mut h);
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Awaiting);

    h.clock.step_wall_ms(2 * HOUR_MS as i64);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(
        delivery_of(&h, "iss-1").stage,
        crew::store::DeliveryStage::Awaiting,
        "a second of waiting is a second, whatever the wall clock says"
    );
}

/// `Mono` does not cross a restart, so the wait is rebuilt from the store's wall-clock record
/// of it: a daemon restarted every half hour must still hand off CI that never reports.
#[test]
fn a_restart_does_not_forgive_the_ci_wait_already_spent() {
    let dir = tmp_dir("ci-wait-restart");
    let db = dir.join("crew.db");
    let forge = Arc::new(FakeForge::new());
    forge.set_ci(&FakeForge::head_after_publish(1), copilot_running());
    {
        let (mut h, _) = delivery_harness_with(
            vec![issue(1, "In Progress", Some(1))],
            Store::open(&db).unwrap(),
            forge.clone(),
            |_| {},
        );
        run_once(&mut h);
        h.clock.advance_ms(HOUR_MS * 3 / 4);
        h.sched.tick().unwrap();
        assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Awaiting);
    }

    let (mut h, _) = delivery_harness_with(
        vec![issue(1, "In Progress", Some(1))],
        Store::open(&db).unwrap(),
        forge.clone(),
        |_| {},
    );
    // The new process's clock starts where the old one did; this is the time that has passed.
    h.clock.advance_ms(HOUR_MS * 3 / 4 + 5_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(HOUR_MS / 4);
    h.sched.tick().unwrap();
    assert_eq!(
        delivery_of(&h, "iss-1").stage,
        crew::store::DeliveryStage::HandedOff,
        "the wait spent before the restart still counts"
    );

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// GETT-174120: requesting a bot reviewer over REST returns success and adds nobody. The
/// provider's answer is not evidence; the pull request's own state is, and a request that did
/// not take must be reported as the failure it is.
#[test]
fn a_review_request_the_provider_accepts_without_attaching_a_reviewer_is_reported_as_a_failure() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |c| {
            c.delivery.reviewers = vec!["copilot-pull-request-reviewer[bot]".into()];
        },
    );
    forge.set_attach_reviewers(false);

    run_once(&mut h);

    assert!(
        forge.ops().iter().any(|o| matches!(o, Op::RequestReview { .. })),
        "the request was made: {:?}",
        forge.ops()
    );
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::HandedOff, "not a success");
    let why = d.review_error.expect("the failure is recorded on the delivery");
    assert!(why.contains("attached nobody"), "{why}");
    assert!(why.contains("copilot-pull-request-reviewer[bot]"), "and names who: {why}");
    let row = &h.sched.snapshot().unwrap().rows[0];
    assert!(
        row.last_error.as_deref().is_some_and(|e| e.contains("attached nobody")),
        "surfaced to the operator: {row:?}"
    );

    // The same request, verifiably attached, is a success — so the check is about attachment,
    // not about requesting bots.
    let (mut h2, forge2) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |c| {
            c.delivery.reviewers = vec!["copilot-pull-request-reviewer[bot]".into()];
        },
    );
    run_once(&mut h2);
    assert_eq!(delivery_of(&h2, "iss-1").stage, crew::store::DeliveryStage::Ready);
    assert!(delivery_of(&h2, "iss-1").review_error.is_none());
    drop(forge2);
}

#[test]
fn each_review_comment_ends_accepted_with_a_commit_or_rejected_with_a_reason_and_is_not_re_argued()
{
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;

    // A reviewer leaves two comments after the pull request opens.
    let fix_me = forge.add_comment(pr, "Copilot", "src/config.rs", "missing #[serde(default)]");
    let no = forge.add_comment(pr, "Copilot", "src/transcript.rs", "umask concern");
    // The fix round will settle one and decline the other.
    h.worker.set_default(Script::succeeds_in(1_000).with_verdicts(vec![
        ReviewVerdict {
            comment_id: fix_me.clone(),
            verdict: Verdict::Accepted,
            detail: "abc1234".into(),
        },
        ReviewVerdict {
            comment_id: no.clone(),
            verdict: Verdict::Rejected,
            detail: "restrict() sets the mode two lines below".into(),
        },
    ]));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    // Handed to a run as work, not as text: the round is dispatched with both comments.
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Redispatched);
    match &h.worker.feedback_for("iss-1")[1] {
        Some(Feedback::Review { comments, .. }) => {
            assert_eq!(comments.len(), 2, "{comments:?}");
        }
        other => panic!("expected review feedback, got {other:?}"),
    }

    // The run settles both; delivery records each verdict and replies on its thread.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let verdicts = h.sched.store().verdicts_for("iss-1").unwrap();
    assert_eq!(
        verdicts[&fix_me],
        (Verdict::Accepted, "abc1234".to_string()),
        "accepted, with the commit"
    );
    assert_eq!(verdicts[&no].0, Verdict::Rejected, "rejected, with the reason");
    assert!(forge.replies_to(pr, &fix_me)[0].contains("abc1234"));
    assert!(forge.replies_to(pr, &no)[0].contains("restrict()"));
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);

    // Settled threads are not handed out again, however many times the pull request is polled.
    for _ in 0..5 {
        h.clock.advance_ms(2_000);
        h.sched.tick().unwrap();
    }
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "no further round over settled comments");
    assert_eq!(forge.replies_to(pr, &fix_me).len(), 1, "one reply per verdict, ever");
    assert_eq!(delivery_of(&h, "iss-1").rounds_pr, 1);
}

/// Finding 3 on #47. The verdict was recorded — durably settled, excluded from every later
/// poll — and *then* the reply was attempted, with a failure merely logged. One transient error
/// on that write hid the verdict from the reviewer for good while delivery advanced to `Ready`.
/// The ordering is the bug: a verdict is settled by a reply that landed, and by nothing else.
#[test]
fn a_verdict_is_not_settled_by_a_reply_that_did_not_land() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    let c = forge.add_comment(pr, "Copilot", "src/config.rs", "missing #[serde(default)]");
    h.worker.set_default(Script::succeeds_in(1_000).with_verdicts(vec![ReviewVerdict {
        comment_id: c.clone(),
        verdict: Verdict::Rejected,
        detail: "the default is set two lines below".into(),
    }]));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Redispatched);

    // The run settles the comment; the network drops the reply that would say so.
    forge.fail_reply_with(Some(ForgeError::Transient("connection reset".into())));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert!(
        !h.sched.store().verdicts_for("iss-1").unwrap().contains_key(&c),
        "a verdict whose reply did not land is not settled"
    );
    let d = delivery_of(&h, "iss-1");
    assert_ne!(d.stage, crew::store::DeliveryStage::Ready, "and delivery cannot rest on it");
    assert!(d.pending_verdicts.is_some(), "the verdict is still queued, not dropped: {d:?}");
    assert!(
        h.sched.snapshot().unwrap().last_error.as_deref().unwrap().contains("connection reset"),
        "the failure is visible"
    );
    // Not handed back to an agent either: the thread is unsettled, but the run already spoke.
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "no round is opened over a failed reply");
    assert_eq!(d.rounds_pr, 1);

    // The network comes back: the same reply is retried, the verdict recorded, and only then
    // does the pull request read as ready.
    forge.fail_reply_with(None);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(forge.replies_to(pr, &c).len(), 1, "one reply, once it could land");
    assert_eq!(h.sched.store().verdicts_for("iss-1").unwrap()[&c].0, Verdict::Rejected);
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::Ready);
    assert!(d.pending_verdicts.is_none(), "the queue is empty once every reply has landed");
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "and it cost no agent run");
}

/// #89: a verdict replied to and recorded left its thread open on the provider, so a pull
/// request delivery called ready still read as unfinished work to the person merging it.
#[test]
fn a_settled_review_comment_has_its_thread_resolved() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    let fixed = forge.add_comment(pr, "Copilot", "src/config.rs", "missing #[serde(default)]");
    let declined = forge.add_comment(pr, "Copilot", "src/store/mod.rs", "rename this");
    let ignored = forge.add_comment(pr, "Copilot", "src/main.rs", "and this");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert!(!forge.is_resolved(pr, &fixed), "nothing is resolved before it is settled");

    h.worker.set_default(Script::succeeds_in(1_000).with_verdicts(vec![
        ReviewVerdict {
            comment_id: fixed.clone(),
            verdict: Verdict::Accepted,
            detail: "abc1234".into(),
        },
        ReviewVerdict {
            comment_id: declined.clone(),
            verdict: Verdict::Rejected,
            detail: "the name is the one the spec uses".into(),
        },
    ]));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(forge.replies_to(pr, &fixed).len(), 1);
    assert_eq!(forge.replies_to(pr, &declined).len(), 1);
    assert!(forge.is_resolved(pr, &fixed), "an accepted comment's thread is resolved");
    assert!(forge.is_resolved(pr, &declined), "and so is a rejected one's");
    assert!(!forge.is_resolved(pr, &ignored), "a comment with no verdict is never resolved");
    assert!(
        forge
            .ops()
            .iter()
            .all(|o| !matches!(o, Op::Resolve { comment_id, .. } if *comment_id == ignored)),
        "not even attempted: {:?}",
        forge.ops()
    );

    // Resolved once: a later poll does not ask again.
    let resolves =
        |forge: &FakeForge| forge.ops().iter().filter(|o| matches!(o, Op::Resolve { .. })).count();
    let before = resolves(&forge);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(resolves(&forge), before);
}

/// #89: resolving is its own idempotent step, so a resolve the provider refuses costs a poll —
/// never the verdict, never a second reply, and never the pull request's readiness.
#[test]
fn a_failed_resolve_is_retried_and_never_re_replies() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    let c = forge.add_comment(pr, "Copilot", "src/config.rs", "missing #[serde(default)]");
    h.worker.set_default(Script::succeeds_in(1_000).with_verdicts(vec![ReviewVerdict {
        comment_id: c.clone(),
        verdict: Verdict::Rejected,
        detail: "the default is set two lines below".into(),
    }]));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    forge.fail_resolve_with(Some(ForgeError::Transient("connection reset".into())));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(forge.replies_to(pr, &c).len(), 1);
    assert!(h.sched.store().verdicts_for("iss-1").unwrap().contains_key(&c), "the verdict stands");
    assert!(!forge.is_resolved(pr, &c));
    assert_eq!(
        delivery_of(&h, "iss-1").stage,
        crew::store::DeliveryStage::Ready,
        "readiness is the verdict table's, not the provider's isResolved"
    );
    assert!(
        h.sched.snapshot().unwrap().last_error.as_deref().unwrap().contains("connection reset"),
        "the failure is visible"
    );

    forge.fail_resolve_with(None);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert!(forge.is_resolved(pr, &c), "the next poll resolves it");
    assert_eq!(forge.replies_to(pr, &c).len(), 1, "without replying a second time");
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "and resolving opened no round");
    assert_eq!(delivery_of(&h, "iss-1").rounds_pr, 1);
}

/// Review on #93: a resolve the provider refuses outright — a credential without the
/// permission — must be visible on the issue's row, and still must not hand off a pull request
/// that is otherwise ready over a write that only tidies its threads.
#[test]
fn a_refused_resolve_is_reported_on_the_row_without_handing_off_a_ready_pull_request() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    let c = forge.add_comment(pr, "Copilot", "src/config.rs", "missing #[serde(default)]");
    h.worker.set_default(Script::succeeds_in(1_000).with_verdicts(vec![ReviewVerdict {
        comment_id: c.clone(),
        verdict: Verdict::Rejected,
        detail: "the default is set two lines below".into(),
    }]));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    forge.fail_resolve_with(Some(ForgeError::Permanent("403: Resource not accessible".into())));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);
    let row = h.sched.store().get("iss-1").unwrap().unwrap();
    assert!(
        row.last_error.as_deref().is_some_and(|e| e.contains("Resource not accessible")),
        "the refusal is on the row: {:?}",
        row.last_error
    );
    assert_eq!(forge.replies_to(pr, &c).len(), 1);
}

const COPILOT: &str = "copilot-pull-request-reviewer[bot]";

/// The review ids a round was handed, in order.
fn handed_ids(h: &Harness, id: &str, run: usize) -> Vec<String> {
    match &h.worker.feedback_for(id)[run] {
        Some(Feedback::Review { comments, .. }) => comments.iter().map(|c| c.id.clone()).collect(),
        other => panic!("expected review feedback, got {other:?}"),
    }
}

/// #126: Copilot put a real finding in its review's summary, with no inline comment, and
/// delivery — reading only threads — never saw it.
#[test]
fn a_finding_only_in_a_review_summary_is_handed_back_to_the_agent() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    let review = forge.add_summary_review(
        pr,
        COPILOT,
        "COMMENTED",
        "## Copilot review overview\n\nPreviously missed: src/gate/git.rs:325 reports \
         `on_base: false` on the no-rebase path.\n\n**Findings:** None",
    );
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Redispatched);
    match &h.worker.feedback_for("iss-1")[1] {
        Some(Feedback::Review { comments, .. }) => {
            assert_eq!(comments.len(), 1, "{comments:?}");
            assert_eq!(comments[0].id, format!("review-{review}"), "keyed by the review");
            assert!(comments[0].body.contains("Previously missed"), "handed over whole");
        }
        other => panic!("expected review feedback, got {other:?}"),
    }
}

/// #126, the case as it happened: the pull request was `ready` when the review arrived.
#[test]
fn a_review_after_ready_with_a_summary_finding_reopens_the_round() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);

    forge.add_summary_review(pr, COPILOT, "COMMENTED", "The dirty-tree failure is misreported.");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let d = delivery_of(&h, "iss-1");
    assert_ne!(d.stage, crew::store::DeliveryStage::Ready, "the finding holds ready: {d:?}");
    assert_eq!(d.rounds_pr, 1, "and costs a round like any comment");
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2);
}

/// #126: a summary finding has the inline comment's lifecycle — a verdict keyed by the review,
/// posted on the pull request since there is no thread, the first verdict standing, and one
/// round charged however often the same review is read again.
#[test]
fn a_settled_summary_finding_is_not_handed_back_again() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    let reviewed_head = forge.pr(pr).unwrap().head_sha;
    let review = forge.add_summary_review(pr, COPILOT, "COMMENTED", "Consider renaming `x`.");
    let key = format!("review-{review}");
    h.worker.set_default(Script::succeeds_in(1_000).with_verdicts(vec![ReviewVerdict {
        comment_id: key.clone(),
        verdict: Verdict::Rejected,
        detail: "the name is the one the spec uses".into(),
    }]));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(handed_ids(&h, "iss-1", 1), std::slice::from_ref(&key));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let verdicts = h.sched.store().verdicts_for("iss-1").unwrap();
    assert_eq!(verdicts[&key].0, Verdict::Rejected);
    let posted = forge.comments_on(pr);
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert!(posted[0].contains("> Consider renaming `x`."), "quotes the finding: {}", posted[0]);
    assert!(posted[0].contains("**Rejected** — the name is the one the spec uses"));
    assert!(forge.replies_to(pr, &key).is_empty(), "a summary has no thread to reply on");
    assert!(
        forge.ops().iter().all(|o| !matches!(o, Op::Resolve { .. })),
        "nor one to resolve: {:?}",
        forge.ops()
    );
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);

    // The same review read on the head again — settled by its id, not re-argued; and a later
    // run's different verdict on it does not replace the first.
    forge.push_head(pr, &reviewed_head);
    for _ in 0..5 {
        h.clock.advance_ms(2_000);
        h.sched.tick().unwrap();
    }
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "no further round over a settled summary");
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::Ready);
    assert_eq!(d.rounds_pr, 1);
    assert_eq!(d.rounds_issue, 1);
    assert_eq!(forge.comments_on(pr).len(), 1, "one verdict comment, ever");
}

/// #126: Copilot's summary saying there is nothing is not a finding.
#[test]
fn a_summary_that_says_findings_none_and_nothing_else_hands_nothing_back() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    forge.add_summary_review(
        pr,
        COPILOT,
        "COMMENTED",
        "<!-- ccr-overview-v2 -->\n\n## Copilot review overview\n\n**Findings:** None\n",
    );
    for _ in 0..3 {
        h.clock.advance_ms(1_000);
        h.sched.tick().unwrap();
    }
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);
    assert_eq!(h.worker.sessions_for("iss-1").len(), 1);
    assert_eq!(delivery_of(&h, "iss-1").rounds_pr, 0);
}

/// #126: the same gap for a human who requested changes in the summary alone.
#[test]
fn a_changes_requested_review_with_only_a_summary_is_handed_back() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    // A human's plain comment summary is not a finding; their request for changes is.
    forge.add_summary_review(pr, "alice", "COMMENTED", "Nice work overall.");
    let review =
        forge.add_summary_review(pr, "alice", "CHANGES_REQUESTED", "Split the migration out.");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Redispatched);
    assert_eq!(handed_ids(&h, "iss-1", 1), [format!("review-{review}")]);
}

/// Finding 4 on #47, end to end: the reviewer approves the first head, CI sends the issue round,
/// and the fix lands as a second head on the same pull request. Nobody had asked the reviewer
/// again, so `Ready` was reached on a head no one had looked at.
#[test]
fn a_fix_round_re_requests_review_so_the_new_head_is_not_left_unreviewed() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |c| {
            c.delivery.reviewers = vec!["reviewer".into()];
        },
    );
    forge.red_ci(&FakeForge::head_after_publish(1), "error[E0308]: mismatched types");

    // First head: review requested and verified, then CI sends the issue back.
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    let requests = |forge: &FakeForge| {
        forge.ops().iter().filter(|o| matches!(o, Op::RequestReview { .. })).count()
    };
    assert_eq!(requests(&forge), 1);
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Redispatched);
    // The reviewer answers on that head while the fix is being written.
    forge.add_review(pr, "reviewer", "APPROVED");

    // Second head, same pull request: the reviewer is asked again before it can read as ready.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.head_sha.as_deref(), Some(FakeForge::head_after_publish(2).as_str()));
    assert_eq!(requests(&forge), 2, "a new head is a new request: {:?}", forge.ops());
    assert!(
        forge.pr(pr).unwrap().requested_reviewers.contains(&"reviewer".to_string()),
        "and it verifiably attached"
    );
    assert_eq!(d.stage, crew::store::DeliveryStage::Ready);
}

/// Finding 5 on #47, the half the parser cannot do: `deadbee` is shaped like a commit, and only
/// the branch can say it is not one of its. An acceptance naming it is not recorded and not
/// posted; the comment stays open and goes round again, named as one the agent left unanswered.
#[test]
fn an_acceptance_naming_a_commit_the_branch_does_not_carry_leaves_the_comment_outstanding() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    let real = forge.add_comment(pr, "Copilot", "src/a.rs", "handle the empty case");
    let bogus = forge.add_comment(pr, "Copilot", "src/b.rs", "this leaks the handle");
    forge.set_commits_on_branch(Some(vec!["abc1234".into()]));
    h.worker.set_default(Script::succeeds_in(1_000).with_verdicts(vec![
        ReviewVerdict {
            comment_id: real.clone(),
            verdict: Verdict::Accepted,
            detail: "abc1234".into(),
        },
        ReviewVerdict {
            comment_id: bogus.clone(),
            verdict: Verdict::Accepted,
            detail: "deadbee".into(),
        },
    ]));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let verdicts = h.sched.store().verdicts_for("iss-1").unwrap();
    assert_eq!(verdicts[&real].0, Verdict::Accepted, "a commit the branch carries is believed");
    assert!(!verdicts.contains_key(&bogus), "one it does not is not: {verdicts:?}");
    assert_eq!(forge.replies_to(pr, &real).len(), 1);
    assert!(forge.replies_to(pr, &bogus).is_empty(), "and nobody is told it was resolved");

    // The comment is still open, so it goes back to an agent — as one it already left hanging.
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::Redispatched, "{d:?}");
    assert_eq!(d.rounds_pr, 2);
    match &h.worker.feedback_for("iss-1")[2] {
        Some(Feedback::Review { comments, unanswered_before, .. }) => {
            let ids: Vec<&str> = comments.iter().map(|c| c.id.as_str()).collect();
            assert_eq!(ids, vec![bogus.as_str()], "only the unsettled comment is handed back");
            assert_eq!(unanswered_before, &vec![bogus.clone()], "named as left unanswered");
        }
        other => panic!("expected review feedback, got {other:?}"),
    }
}

/// The new runaway, bounded. A reviewer that comments on every push, answered by an agent
/// that pushes, would loop forever; every hand-back is a round and the two bounds hold across
/// a restart and across a fresh pull request.
#[test]
fn fix_rounds_are_bounded_per_pull_request_and_per_issue_and_the_bound_survives_a_new_run_and_a_new_pull_request()
 {
    let dir = tmp_dir("delivery-rounds");
    let db = dir.join("crew.db");
    let tune = |c: &mut Config| {
        c.delivery.max_rounds_per_pr = 2;
        c.delivery.max_rounds_per_issue = 3;
        c.tracker.active_states = vec!["in progress".into(), "in review".into()];
    };

    let (mut h, forge) =
        delivery_harness(vec![issue(1, "In Progress", Some(1))], Store::open(&db).unwrap(), tune);
    // Every head is red, every run says done: the loop with no bound of its own.
    forge.set_ci_default(Some(CiStatus::Failure {
        failures: vec![crew::forge::CiFailure {
            name: "gate".into(),
            url: None,
            detail: "still red".into(),
        }],
    }));

    for _ in 0..12 {
        h.sched.tick().unwrap();
        h.clock.advance_ms(1_000);
    }
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::HandedOff, "the per-PR bound must stop it");
    assert_eq!((d.rounds_pr, d.rounds_issue), (2, 2));
    assert!(
        d.handoff_reason.as_deref().unwrap().contains("gate"),
        "the outstanding item is named: {d:?}"
    );
    assert_eq!(h.worker.sessions_for("iss-1").len(), 3, "first run plus exactly two rounds");
    let pr1 = forge.open_prs()[0].number;

    // A restart, a closed pull request and a ticket moved on: the per-PR count starts over
    // with the new pull request, the per-issue count does not.
    drop(h);
    forge.set_state(pr1, PrState::Closed);
    let (mut h, _same) = delivery_harness_with(
        vec![issue(1, "In Review", Some(1))],
        Store::open(&db).unwrap(),
        forge.clone(),
        tune,
    );
    // Past everything the first process did: run ids are wall-clock stamped.
    h.clock.advance_ms(100_000);
    for _ in 0..12 {
        h.sched.tick().unwrap();
        h.clock.advance_ms(1_000);
    }
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::HandedOff);
    assert_eq!(d.rounds_issue, 3, "the issue-wide bound is what stopped the second pull request");
    assert_eq!(d.rounds_pr, 1, "with the new pull request's own count nowhere near its bound");
    assert_eq!(h.worker.sessions_for("iss-1").len(), 2, "one fresh run plus the single round left");

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nothing_merges_without_a_human() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |c| {
            c.delivery.reviewers = vec!["reviewer".into()];
        },
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    forge.add_review(pr, "reviewer", "APPROVED");

    // Green, approved, nothing outstanding, polled for a long time: it stays open.
    for _ in 0..20 {
        h.clock.advance_ms(60_000);
        h.sched.tick().unwrap();
    }
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);
    assert_eq!(
        forge.pr(pr).unwrap().state,
        PrState::Open,
        "ready to merge is where the orchestrator stops"
    );

    // And once the human merges, delivery notices and stops polling.
    forge.set_state(pr, PrState::Merged);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Closed);
    let before = forge.ops().len();
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    assert_eq!(forge.ops().len(), before, "a closed delivery makes no further forge calls");
}

#[test]
fn a_transient_forge_failure_retries_delivery_without_re_running_the_agent() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    forge.fail_with(Some(ForgeError::Transient("connection reset".into())));

    run_once(&mut h);
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Pending);
    assert!(forge.open_prs().is_empty());
    assert!(
        h.sched.snapshot().unwrap().last_error.as_deref().unwrap().contains("connection reset")
    );

    // Still failing: still pending, still no second run.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(
        h.worker.sessions_for("iss-1").len(),
        1,
        "a network blip must not cost an agent run"
    );

    forge.fail_with(None);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(forge.open_prs().len(), 1, "and the next poll finishes the job");
    assert_eq!(h.worker.sessions_for("iss-1").len(), 1);
}

#[test]
fn a_permanent_forge_failure_hands_off_rather_than_retrying_forever() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    forge.fail_with(Some(ForgeError::Permanent("401 bad credentials".into())));
    run_once(&mut h);
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::HandedOff);
    assert!(d.handoff_reason.unwrap().contains("bad credentials"));
    let calls = forge.ops().len();
    for _ in 0..5 {
        h.clock.advance_ms(5_000);
        h.sched.tick().unwrap();
    }
    assert_eq!(forge.ops().len(), calls, "a handed-off delivery is not polled again");
}

#[test]
fn a_run_that_committed_nothing_delivers_nothing_and_stays_parked() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    forge.set_commits(vec![]);
    run_once(&mut h);
    assert!(!forge.ops().iter().any(|o| matches!(o, Op::OpenPr { .. })), "{:?}", forge.ops());
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Closed);
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::Released);
    assert_eq!(st.parked_state.as_deref(), Some("in progress"));
}

#[test]
fn delivery_is_inert_unless_the_config_turns_it_on() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |c| {
            c.delivery.enabled = false;
        },
    );
    run_once(&mut h);
    assert!(forge.ops().is_empty(), "a forge that is attached but not enabled must not be used");
    assert!(h.sched.store().delivery("iss-1").unwrap().is_none());
}

#[test]
fn a_stacked_branch_opens_its_pull_request_against_the_branch_it_sits_on() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    // The lower branch finishes, and is pushed, before the one on top of it.
    h.worker.script("iss-2", Script::succeeds_in(2_000));
    h.sched.tick().unwrap();
    let under = h.sched.store().get("iss-1").unwrap().unwrap().branch.unwrap();
    // The publisher reports iss-2's work as sitting on iss-1's branch.
    forge.set_stacked_on(Some(under.clone()));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let bases: Vec<(String, String)> = forge
        .open_prs()
        .iter()
        .map(|p| (forge.spec_of(p.number).unwrap().head, p.base.clone()))
        .collect();
    let of =
        |n: &str| bases.iter().find(|(head, _)| head.contains(n)).map(|(_, b)| b.clone()).unwrap();
    assert_eq!(of("MT-1-"), "master", "the branch under the stack targets the trunk");
    assert_eq!(of("MT-2-"), under, "the branch on top targets the one it sits on");
    let top = forge.open_prs().iter().find(|p| p.base == under).unwrap().number;
    assert!(forge.spec_of(top).unwrap().body.contains("Stacked on"), "and the body says so");
}

/// Finding 1 on #47, and the situation #42 is in as this is written: stacked on #43, whose merge
/// moves #42's base to `master`. A reused pull request was returned as found, so the scheduler
/// recorded the base it had computed while the provider still targeted the merged branch —
/// and the pull request body claimed a stack that was over. Today that is a person clicking.
#[test]
fn a_pull_request_whose_desired_base_has_changed_is_retargeted_and_the_snapshot_agrees() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    h.worker.script("iss-2", Script::succeeds_in(2_000));
    h.sched.tick().unwrap();
    let under = h.sched.store().get("iss-1").unwrap().unwrap().branch.unwrap();
    forge.set_stacked_on(Some(under.clone()));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let top = forge.open_prs().iter().find(|p| p.base == under).expect("stacked").number;
    assert_eq!(delivery_of(&h, "iss-2").base.as_deref(), Some(under.as_str()));

    // The lower branch merges: iss-2's work now sits directly on the trunk. A review comment
    // then sends iss-2 round once more, and the push after that round recomputes the base.
    forge.set_stacked_on(None);
    let c = forge.add_comment(top, "reviewer", "src/x.rs", "nit");
    h.worker.script(
        "iss-2",
        Script::succeeds_in(1_000).with_verdicts(vec![ReviewVerdict {
            comment_id: c,
            verdict: Verdict::Rejected,
            detail: "intended".into(),
        }]),
    );
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let pr = forge.pr(top).unwrap();
    assert_eq!(pr.base, "master", "the provider targets the new base: {:?}", forge.ops());
    assert!(
        forge.ops().iter().any(
            |o| matches!(o, Op::Retarget { number, to, .. } if *number == top && to == "master")
        ),
        "moved, not reopened: {:?}",
        forge.ops()
    );
    assert_eq!(forge.open_prs().len(), 2, "still one pull request per issue");
    assert_eq!(delivery_of(&h, "iss-2").base.as_deref(), Some("master"), "the store agrees");
    let row = h.sched.snapshot().unwrap().rows.into_iter().find(|r| r.issue_id == "iss-2").unwrap();
    assert_eq!(row.delivery.unwrap().base.as_deref(), Some("master"), "and so does the snapshot");
    assert!(
        !forge.spec_of(top).unwrap().body.contains("Stacked on"),
        "the body no longer claims a stack that is over"
    );
    assert_eq!(delivery_of(&h, "iss-2").stage, crew::store::DeliveryStage::Ready);
}

/// Finding 2 on #47. Every issue's branch was a stack candidate, pushed or not; when the upper
/// one finished first the pull request named a base the remote had never seen, and the
/// provider's 422 read as permanent — a handoff for a branch with nothing wrong but its timing.
#[test]
fn a_base_that_is_not_published_is_not_selected_as_a_stack_base() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1)), issue(2, "In Progress", Some(2))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    // The branch on top finishes first; the one under it is still running.
    h.worker.script("iss-1", Script::succeeds_in(5_000));
    h.sched.tick().unwrap();
    let under = h.sched.store().get("iss-1").unwrap().unwrap().branch.unwrap();
    forge.set_stacked_on(Some(under.clone()));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert!(
        !forge.ops().iter().any(|o| matches!(o, Op::Publish { branch, .. } if *branch == under)),
        "the lower branch has not been pushed: {:?}",
        forge.ops()
    );
    let prs = forge.open_prs();
    assert_eq!(prs.len(), 1, "the upper branch delivered: {:?}", forge.ops());
    assert_eq!(prs[0].base, "master", "against the trunk, not a base the remote does not have");
    assert_eq!(delivery_of(&h, "iss-2").stage, crew::store::DeliveryStage::Ready);
    assert_eq!(delivery_of(&h, "iss-2").base.as_deref(), Some("master"));
}

/// Merging is the operator's, and it closes the ticket; `sweep_parked` then reclaims the
/// worktree and may delete the branch. The delivery row polled after that must read the merged
/// pull request as the end of the story, not the missing branch as a failure.
#[test]
fn a_merged_pull_request_whose_branch_cleanup_deleted_closes_delivery_rather_than_failing_it() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    run_once(&mut h);
    let pr = forge.open_prs()[0].number;
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::Ready);

    // The human merges; the ticket closes; cleanup deletes the now-merged branch.
    forge.set_state(pr, PrState::Merged);
    h.sched.store().set_branch(h.clock.as_ref(), "iss-1", None).unwrap();

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::Closed, "{d:?}");
    assert_eq!(d.handoff_reason.as_deref(), Some("merged"));
    assert!(
        h.sched.store().get("iss-1").unwrap().unwrap().last_error.is_none(),
        "a merged pull request is not an error to report"
    );
}

/// A handed-off pull request is the operator's, and the operator merging it is the end of the
/// story. A row never polled again kept publishing the handoff as a live failure for an issue
/// that was merged and closed (#101).
#[test]
fn a_handed_off_pull_request_the_operator_merged_is_reported_closed_not_failed() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |c| c.delivery.reviewers = vec!["copilot-pull-request-reviewer[bot]".into()],
    );
    forge.set_attach_reviewers(false);
    run_once(&mut h);
    assert_eq!(delivery_of(&h, "iss-1").stage, crew::store::DeliveryStage::HandedOff);
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().last_error.is_some());
    let sessions = h.worker.sessions_for("iss-1").len();

    forge.set_state(forge.open_prs()[0].number, PrState::Merged);
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::Closed, "{d:?}");
    assert_eq!(d.handoff_reason.as_deref(), Some("merged"));
    let row = &h.sched.snapshot().unwrap().rows[0];
    assert!(row.last_error.is_none(), "a merged pull request is not a failure: {row:?}");
    assert_eq!(h.worker.sessions_for("iss-1").len(), sessions, "and no agent is dispatched");
}

/// The handoff gave the branch to the operator, so a handed-off row that stays open is read and
/// nothing else: no push, no review request, no hand-back to an agent.
#[test]
fn a_handed_off_pull_request_that_stays_open_is_polled_for_its_state_only() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |c| c.delivery.reviewers = vec!["copilot-pull-request-reviewer[bot]".into()],
    );
    forge.set_attach_reviewers(false);
    run_once(&mut h);
    let d = delivery_of(&h, "iss-1");
    assert_eq!(d.stage, crew::store::DeliveryStage::HandedOff);
    let ops = forge.ops();
    let reads = forge.pr_reads();
    let feedback = h.worker.feedback_for("iss-1");

    for _ in 0..3 {
        h.clock.advance_ms(1_000);
        h.sched.tick().unwrap();
    }

    assert!(forge.pr_reads() > reads, "the handed-off row is still polled");
    assert_eq!(forge.ops(), ops, "and polling it writes nothing");
    assert_eq!(h.worker.feedback_for("iss-1"), feedback, "nor hands it back to an agent");
    let after = delivery_of(&h, "iss-1");
    assert_eq!(after.stage, crew::store::DeliveryStage::HandedOff);
    assert_eq!(after.handoff_reason, d.handoff_reason, "the operator's reason is kept");
}

/// The stack is gate first, delivery second (#44). A `Done` with both attached goes to the gate
/// before anything is pushed: a failing gate sends the issue back to an agent and the forge sees
/// nothing at all, and only the gate's pass hands the branch — rebased and re-gated — to
/// delivery. Push on the agent's `Done` instead and the first op below lands before the gate
/// has spoken, which is this stack built upside down.
#[test]
fn a_done_branch_is_gated_before_delivery_pushes_it_and_a_failing_gate_publishes_nothing() {
    let (mut h, forge) = delivery_harness(
        vec![issue(1, "In Progress", Some(1))],
        Store::open_in_memory().unwrap(),
        |_| {},
    );
    let gate = Arc::new(FakeGate::new(h.clock.clone()));
    h.sched.set_gate(Some(gate.clone()));
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Failed {
        step: "cargo test".into(),
        output: "test a_thing ... FAILED".into(),
        on_base: true,
    }));

    // The agent finishes; the gate takes over; nothing has been pushed.
    run_once(&mut h);
    assert_eq!(h.sched.gating_count(), 1, "the gate must have the branch first");
    assert!(forge.ops().is_empty(), "nothing is pushed while the gate runs: {:?}", forge.ops());
    assert!(h.sched.store().delivery("iss-1").unwrap().is_none(), "and no delivery is queued");

    // The gate fails: a continuation, still nothing on the forge.
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.phase, Phase::RetryQueued, "a failing gate is a continuation");
    assert!(forge.ops().is_empty(), "an ungated branch is never published: {:?}", forge.ops());
    assert!(h.sched.store().delivery("iss-1").unwrap().is_none());

    // The continuation is told why by the gate, through the one channel delivery also uses.
    h.clock.advance_ms(5_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1);
    assert!(
        matches!(&h.worker.feedback_for("iss-1")[1], Some(Feedback::Gate { output }) if output.contains("cargo test")),
        "{:?}",
        h.worker.feedback_for("iss-1")
    );

    // This time the gate passes, and only then does delivery push and open the pull request.
    gate.set_default(GateScript::passes_in(1_000));
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.gating_count(), 1, "gated again, on the rebased tree");
    assert!(forge.ops().is_empty());
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(gate.starts_for("iss-1").len(), 2, "one gate per Done");
    let ops = forge.ops();
    assert!(matches!(ops.first(), Some(Op::Publish { .. })), "pushed after the pass: {ops:?}");
    assert!(ops.iter().any(|o| matches!(o, Op::OpenPr { .. })), "and opened: {ops:?}");
    assert_eq!(forge.open_prs().len(), 1);
}

/// #108. A gate conflict is a human's problem, and once the human has resolved it the issue has
/// to be handed back without closing and reopening its ticket. The unblock lifts the park and
/// nothing else, so the next tick dispatches through the ordinary path — and `prepare` attaches
/// to the branch the blocked run committed to rather than resetting it to the base.
#[test]
fn unblocking_a_parked_blocked_issue_dispatches_it_onto_its_existing_branch() {
    let dir = tmp_dir("unblock-attaches");
    let root = dir.join("workspaces");
    let repo = git_repo(&dir.join("repo"));
    let mut h = harness_over(
        vec![issue(1, "In Progress", Some(1))],
        root.clone(),
        Store::open_in_memory().unwrap(),
        Arc::new(GitWorktreeWorkspace::new(&root, &repo).unwrap()),
        |_| {},
    );
    let gate = Arc::new(FakeGate::new(h.clock.clone()));
    h.sched.set_gate(Some(gate.clone()));
    gate.set_default(GateScript::passes_in(1_000).with_verdict(GateVerdict::Conflict {
        paths: vec!["src/store/schema.rs".into()],
        base_sha: BASE.into(),
    }));
    h.worker.set_default(Script::succeeds_in(1_000));

    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    commit_in(&ws, "work.txt", "the blocked run's work");
    let head = git_out(&ws, &["rev-parse", "HEAD"]).unwrap();
    let branch = git_out(&ws, &["symbolic-ref", "--short", "HEAD"]).unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.parked_state.as_deref(), Some("in progress"), "the conflict parks the issue");
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 0, "and it stays parked while nothing has changed");

    gate.set_default(GateScript::passes_in(1_000));
    assert!(h.sched.unblock("iss-1").unwrap(), "a parked issue is unblocked, and says so");
    let st = h.sched.store().get("iss-1").unwrap().unwrap();
    assert_eq!(st.parked_state.as_deref(), Some(crew::store::UNBLOCKED));
    assert_eq!(st.phase, Phase::Released, "unblock lifts the park; it takes no claim");
    assert_eq!(st.last_error, None, "the parked note names a problem now resolved");

    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "the next tick dispatches it");
    let ws2 = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    assert_eq!(git_out(&ws2, &["symbolic-ref", "--short", "HEAD"]), Some(branch));
    assert_eq!(
        git_out(&ws2, &["rev-parse", "HEAD"]),
        Some(head),
        "prepare attached to the branch the blocked run committed to, not a reset one"
    );

    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// #108, the invariant "Clearing a quarantine cannot release a live claim" widened to parks. An
/// unblock names an issue from a snapshot as old as the last tick, so it can land on a run, a
/// gate or a waiting continuation; each must be a no-op that reports it, or the next tick would
/// dispatch a second agent onto a worktree already in use.
#[test]
fn unblocking_an_issue_that_is_not_parked_does_not_release_a_live_claim() {
    let issues = vec![
        issue(1, "In Progress", Some(1)),
        issue(2, "In Progress", Some(1)),
        issue(3, "In Progress", Some(1)),
    ];
    let mut h = harness(issues, |_| {});
    let gate = Arc::new(FakeGate::new(h.clock.clone()));
    h.sched.set_gate(Some(gate.clone()));
    gate.set_default(GateScript::passes_in(600_000));
    h.worker.script("iss-1", Script::succeeds_in(600_000));
    h.worker.script("iss-2", Script::succeeds_in(1_000));
    h.worker.script(
        "iss-3",
        Script::succeeds_in(1_000).with_outcome(Outcome::Continue { why: "more to do".into() }),
    );
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    let phase = |h: &Harness, id: &str| h.sched.store().get(id).unwrap().unwrap().phase;
    assert_eq!(h.sched.running_count(), 1, "iss-1 is running");
    assert_eq!(h.sched.gating_count(), 1, "iss-2 is gating");
    assert_eq!(phase(&h, "iss-3"), Phase::RetryQueued, "iss-3 waits on its continuation");

    // Parked as well, so only the live-claim half of the guard stands between each of these
    // and an unpark. Parking alongside a live phase is not a path the scheduler takes today;
    // the guard must not rely on that.
    for id in ["iss-1", "iss-2", "iss-3"] {
        h.sched.store().park(h.clock.as_ref(), id, "in progress").unwrap();
        assert!(!h.sched.unblock(id).unwrap(), "{id}: nothing to unblock, and it says so");
        let st = h.sched.store().get(id).unwrap().unwrap();
        assert_eq!(st.parked_state.as_deref(), Some("in progress"), "{id}: untouched");
    }
    assert!(!h.sched.unblock("iss-404").unwrap(), "an unknown id is a no-op too");

    assert_eq!(phase(&h, "iss-1"), Phase::Running, "the claim must stand");
    assert_eq!(phase(&h, "iss-2"), Phase::Running, "a gate holds its claim");
    assert_eq!(phase(&h, "iss-3"), Phase::RetryQueued);
    assert_eq!(h.sched.store().all_retries().unwrap().len(), 1, "the continuation is kept");
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.running_count(), 1, "no second agent is dispatched onto iss-1");
    assert_eq!(h.sched.gating_count(), 1);
}

/// #108. `sweep_parked` only walks parked rows, and `dispatch_new` never sees a closed ticket,
/// so a park lifted from an issue whose ticket has since closed would strand its worktree and
/// branch for good. The unblock reads the ticket fresh and leaves such a park to the sweep.
#[test]
fn unblocking_an_issue_whose_ticket_closed_leaves_it_for_the_parked_sweep() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.parked_sweep_interval_ms = 60_000;
    });
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Blocked { why: "conflict".into() }),
    );
    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state.is_some());

    h.tracker.fail_by_ids(Some(TrackerError::Request("down".into())));
    assert!(h.sched.unblock("iss-1").is_err(), "an unread ticket is not a guess either way");
    h.tracker.fail_by_ids(None);

    h.tracker.set_state("iss-1", "Done");
    assert!(!h.sched.unblock("iss-1").unwrap(), "a closed ticket's park is kept, and says so");
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state.is_some());

    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state, None);
    assert!(!ws.exists(), "the sweep reclaimed the worktree the park kept in view");
}

/// Review on #112: an unblock that lifts a park `dispatch_new` would then refuse — the issue
/// lost its dispatch marker, or spent its turn budget — reports success and leaves the issue
/// neither parked nor dispatchable. Every gate dispatch applies is checked before lifting.
#[test]
fn unblocking_an_issue_dispatch_would_refuse_keeps_its_park() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.max_turns_per_issue = 5; // the fake burns 3 turns per run
    });
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Blocked { why: "conflict".into() }),
    );
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state.is_some());

    h.tracker.set_dispatchable("iss-1", false);
    assert!(!h.sched.unblock("iss-1").unwrap(), "no dispatch marker, no unblock");
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state.is_some());
    h.tracker.set_dispatchable("iss-1", true);

    assert!(h.sched.unblock("iss-1").unwrap(), "eligible again: the park lifts");
    h.sched.tick().unwrap();
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().cumulative_turns >= 5);
    assert!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state.is_some());
    assert!(!h.sched.unblock("iss-1").unwrap(), "a spent turn budget keeps the park");
}

/// Review on #112: the unblock reads the ticket, then lifts the park, and the next dispatch is
/// up to a tick later. A ticket that closed in between was neither dispatched nor swept, which
/// stranded its worktree and branch. The park is re-pointed rather than cleared, so the sweep
/// still sees it.
#[test]
fn a_ticket_that_closes_between_an_unblock_and_the_next_tick_is_still_swept() {
    let mut h = harness(vec![issue(1, "In Progress", Some(1))], |c| {
        c.agent.parked_sweep_interval_ms = 60_000;
    });
    h.worker.set_default(
        Script::succeeds_in(1_000).with_outcome(Outcome::Blocked { why: "conflict".into() }),
    );
    h.sched.tick().unwrap();
    let ws = PathBuf::from(h.sched.snapshot().unwrap().rows[0].workspace.clone().unwrap());
    h.clock.advance_ms(1_000);
    h.sched.tick().unwrap();

    assert!(h.sched.unblock("iss-1").unwrap());
    h.tracker.set_state("iss-1", "Done");
    h.clock.advance_ms(60_000);
    h.sched.tick().unwrap();

    assert_eq!(h.sched.running_count(), 0, "a closed ticket is not dispatched");
    assert!(!ws.exists(), "and the sweep still reclaimed the worktree the unblock left parked");
    assert_eq!(h.sched.store().get("iss-1").unwrap().unwrap().parked_state, None);
}
