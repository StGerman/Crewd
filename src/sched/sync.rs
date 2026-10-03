//! Taking in what someone else pushed to an agent's branch before anything builds on it (#163).
//!
//! The worktree used to never read its own remote branch: a continuation resumed on the branch
//! as it was left, the gate rebased that, and the push, leased against a remote-tracking ref any
//! fetch in `workspace.repo` moves, replaced the operator's commit. So an agent run, a re-gate,
//! a push and the gate after an agent's `Done` (#269) each start from
//! [`Publisher::sync`](crate::forge::Publisher::sync). A head the
//! lease does not name is fast-forwarded or merged, never rebased. A head the lease already
//! names has not moved since the last sync or publish: the worktree already held it, whoever
//! pushed it, the gate rewrote it (#227), and merging it back is the conflict delivery was
//! reporting, so the push replaces it instead. A merge conflict is a gate
//! conflict by #111's rule: a brief to the agent when every path is agent-resolvable, `Blocked`
//! naming the paths otherwise.
//!
//! A sync that fails outright does not withhold a run: the lease names only heads the worktree
//! took in, so the push after it refuses rather than overwrites, and the next sync tries again.

use std::path::Path;

use super::Scheduler;
use crate::forge::{ForgeError, Synced};
use crate::gate;
use crate::model::{Feedback, Issue, Outcome};
use crate::workspace::Prepared;

/// Whether `launch` goes on to spawn the agent, and with which brief.
pub(super) enum BeforeRun {
    Proceed(Option<Feedback>),
    Blocked,
}

/// The brief for an agent whose branch could not take in the remote's commits. It says the
/// merge was aborted for the reason the gate's brief does, and forbids a rebase because the
/// commits it would rewrite are the ones the pull request's reviewers have already seen.
fn sync_brief(
    remote: &str,
    branch: &str,
    head: &str,
    paths: &[String],
    n: Option<(u32, u32)>,
) -> String {
    let tries = n.map(|(n, max)| format!(" (failure {n} of {max})")).unwrap_or_default();
    format!(
        "someone else pushed to {remote}/{branch}, whose head is now {head}, and merging it into \
         your branch conflicted in {}{tries}. The merge was aborted, so the branch is where you \
         left it. Take their commits in yourself: `git merge {head}`, keeping both sides' changes \
         in each conflicted file, and commit. Do not rebase onto it, which rewrites commits a \
         reviewer has seen. Check the result still builds, and finish again.",
        paths.join(", ")
    )
}

impl Scheduler {
    /// Sync `worktree` with the remote's copy of `branch`; `None` when delivery is off, since
    /// without it nothing is pushed and there is no remote branch to take anything in from.
    pub(super) fn sync_branch(
        &self,
        worktree: &Path,
        branch: &str,
    ) -> Result<Option<Synced>, ForgeError> {
        if !self.delivery_on() {
            return Ok(None);
        }
        let publisher = self.publisher.clone().expect("checked by delivery_on");
        publisher.sync(worktree, branch, &self.cfg.delivery.remote).map(Some)
    }

    /// Sync the worktree of a run that reported `Done`, before the gate rebases it, so a head
    /// the agent pushed itself is the lease before the rebase rewrites it (#269). A conflict or
    /// a failure leaves the lease where it was for the sync before the next run or push.
    pub(super) fn sync_before_gate(&self, issue_id: &str, worktree: &Path) {
        let branch = match self.store.get(issue_id) {
            Ok(st) => st.and_then(|s| s.branch),
            Err(e) => {
                tracing::warn!(issue_id, error = %e, "could not read the branch to sync before the gate");
                return;
            }
        };
        let Some(branch) = branch else { return };
        match self.sync_branch(worktree, &branch) {
            Ok(Some(Synced::Advanced { remote_head, merged })) => tracing::info!(
                issue_id, branch, head = %remote_head, merged,
                "took in commits someone else pushed to the branch before gating it"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                issue_id, branch, error = %e,
                "could not sync the branch before the gate; the push lease still holds"
            ),
        }
    }

    /// Sync the worktree `launch` just prepared, before the agent is spawned into it.
    ///
    /// `queued` is whether a human's unblock carried feedback to this run: a conflict that
    /// blocked once and was then handed back is the agent's to resolve, not a reason to block
    /// again before the agent has seen it (#109).
    pub(super) fn sync_before_run(
        &mut self,
        issue: &Issue,
        prepared: &Prepared,
        queued: bool,
    ) -> anyhow::Result<BeforeRun> {
        let Some(branch) = prepared.branch.as_deref() else { return Ok(BeforeRun::Proceed(None)) };
        let (head, paths) = match self.sync_branch(&prepared.path, branch) {
            Ok(Some(Synced::Conflict { remote_head, paths })) => (remote_head, paths),
            Ok(Some(Synced::Advanced { remote_head, merged })) => {
                tracing::info!(
                    issue_id = %issue.id, branch, head = %remote_head, merged,
                    "took in commits someone else pushed to the branch"
                );
                return Ok(BeforeRun::Proceed(None));
            }
            Ok(_) => return Ok(BeforeRun::Proceed(None)),
            Err(e) => {
                tracing::warn!(
                    issue_id = %issue.id, branch, error = %e,
                    "could not sync the branch with its remote; the push lease still holds"
                );
                return Ok(BeforeRun::Proceed(None));
            }
        };
        let remote = self.cfg.delivery.remote.clone();
        let detail = format!("conflicts in {}", paths.join(", "));
        let outcome = if gate::agent_resolvable(&paths, &self.cfg.gate.agent_resolvable) {
            let step = format!("merge {remote}/{branch}");
            self.gate_failure(&issue.id, &issue.identifier, &step, &detail, |n, max| {
                sync_brief(&remote, branch, &head, &paths, Some((n, max)))
            })?
        } else if queued {
            Outcome::Continue { why: sync_brief(&remote, branch, &head, &paths, None) }
        } else {
            Outcome::Blocked {
                why: format!(
                    "merging {remote}/{branch} ({head}) conflicts in {} file(s): {}",
                    paths.len(),
                    paths.join(", ")
                ),
            }
        };
        let why = match outcome {
            Outcome::Continue { why } => {
                return Ok(BeforeRun::Proceed(Some(Feedback::Gate { output: why })));
            }
            Outcome::Blocked { why } => why,
            _ => detail,
        };
        tracing::warn!(issue_id = %issue.id, ?paths, "remote branch conflicts; blocking for a human");
        // Queued for the run a human's unblock dispatches next, as a gate conflict's is (#109).
        let fb = Feedback::Gate { output: sync_brief(&remote, branch, &head, &paths, None) };
        let clock = self.clock.clone();
        self.store.set_pending_feedback(clock.as_ref(), &issue.id, &serde_json::to_string(&fb)?)?;
        self.store.set_note(clock.as_ref(), &issue.id, &why)?;
        self.store.clear_gate_failures(&issue.id)?;
        self.no_progress.remove(&issue.id);
        self.store.release(clock.as_ref(), &issue.id)?;
        let state =
            self.seen.get(&issue.id).map(|i| i.state_key()).unwrap_or_else(|| issue.state_key());
        self.store.park(clock.as_ref(), &issue.id, &state)?;
        Ok(BeforeRun::Blocked)
    }
}
