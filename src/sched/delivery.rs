//! Delivery: from a run's `Done` to a pull request an operator can merge.
//!
//! `Done` used to mean "the process exited". Every branch that reached `master` in the week
//! this was written took the rest of the path by hand — rebase, gate, push, open the pull
//! request, request review, read the comments, decide, fix, push again — and none of what that
//! path found reached the agent that wrote the code (issue #32). This module walks the part of
//! that path after the branch exists, and it stops at ready-to-merge: there is no merge here,
//! and [`Forge`] has no method for one.
//!
//! ## Shape
//!
//! A `Done` verdict still releases the claim and parks the issue, exactly as before; delivery
//! is then a row in the store advanced by [`Scheduler::advance_deliveries`] on the tick, after
//! reconciliation and before the dispatch gate, at `delivery.poll_interval_ms`. Every step is
//! idempotent — a push of a branch already pushed, an open of a pull request already open — so
//! a transient forge failure costs a poll and nothing else, and a restart resumes from the row.
//!
//! When CI is red, or a review comment is outstanding, the issue is handed back to an agent by
//! the *same* path a `Continue` takes: a retry row due now, a resumed session, and the failure
//! in the prompt as [`Feedback`]. That is the literal form of "a red gate is a `Continue`, never
//! a `Done`": the run said done, and the orchestrator said not yet.
//!
//! ## The runaway this creates, and its brake
//!
//! A reviewer that comments on every push, answered by an agent that pushes a fix, is a loop
//! with no bound of its own — and each turn of it spends a reviewer's quota and an operator's
//! attention as well as tokens. So every hand-back is a *round*, counted against the current
//! pull request and against the issue's whole life, and the per-issue count never resets:
//! not for a new run, not for a new pull request. When either bound is reached the pull request
//! is handed to the operator with the outstanding items named, and nothing more is dispatched
//! for it. The per-issue turn budget is a fourth brake on the same loop and stays in force.
//!
//! ## Verification is not optional
//!
//! A review request is followed by a read. GitHub answers a request for a bot reviewer with
//! `200` and attaches nobody (GETT-174120), so the provider's own answer is not evidence; the
//! pull request's outstanding requests and its posted reviews are. A request that verifiably
//! attached nobody is a handoff with that reason, reported on the issue's row — never a quiet
//! success that leaves a pull request nobody will look at.
//!
//! And it is per head. A fix round pushes a new head to the same pull request, and a reviewer
//! verified against the old one has not seen it; so the push resets `review_requested`, and
//! the request-then-read runs again before the pull request can read as ready (#47).

use std::collections::HashMap;
use std::sync::Arc;

use super::review_summary::{summary_findings, verdict_comment};
use super::{Gating, Running, Scheduler};
use crate::clock::{Mono, Wall};
use crate::forge::{
    CiStatus, Forge, ForgeError, PrState, PullRequest, PullRequestSpec, Review, summary_review_id,
};
use crate::model::{Feedback, Outcome, ReviewVerdict, Verdict};
use crate::store::{DeliveryRecord, DeliveryStage, IssueState};
use crate::worker::{KillResult, Progress, RunHandle};

pub use libcrew::DeliveryView;
use libcrew::fmt::{fmt_ms, timestamp};

impl From<&DeliveryRecord> for DeliveryView {
    fn from(d: &DeliveryRecord) -> Self {
        Self {
            stage: d.stage.label().to_string(),
            pr_number: d.pr_number,
            pr_url: d.pr_url.clone(),
            base: d.base.clone(),
            rounds_pr: d.rounds_pr,
            rounds_issue: d.rounds_issue,
            review_error: d.review_error.clone(),
            handoff_reason: d.handoff_reason.clone(),
        }
    }
}

/// The run a re-gate stands in for. Its agent said `Done` before the branch was delivered, and
/// the gate is what turns that into a verdict again; there is no process, no turn and no run row
/// of its own, so `finish_run` on its id touches nothing and it adds no turns.
struct Regated;

impl RunHandle for Regated {
    fn progress(&self) -> Progress {
        Progress::default()
    }
    fn finished(&self) -> Option<Outcome> {
        Some(Outcome::Done)
    }
    fn kill(&self, _grace_ms: u64) -> KillResult {
        KillResult::AlreadyDone
    }
}

/// One step's failure. Forge failures are classified and never stop the tick; store failures
/// propagate like every other store failure in the scheduler.
enum StepError {
    Forge(ForgeError),
    Other(anyhow::Error),
}

impl From<ForgeError> for StepError {
    fn from(e: ForgeError) -> Self {
        StepError::Forge(e)
    }
}

impl From<rusqlite::Error> for StepError {
    fn from(e: rusqlite::Error) -> Self {
        StepError::Other(e.into())
    }
}

impl From<serde_json::Error> for StepError {
    fn from(e: serde_json::Error) -> Self {
        StepError::Other(e.into())
    }
}

impl Scheduler {
    /// Attach the forge and the publisher. A setter, like the broker's: optional by nature,
    /// and every caller without it — the scheduler's whole existing test suite — is correct
    /// without it. `delivery.enabled` in the config is what turns the attached pair on.
    pub fn set_delivery(
        &mut self,
        forge: Option<std::sync::Arc<dyn Forge>>,
        publisher: Option<std::sync::Arc<dyn crate::forge::Publisher>>,
    ) {
        self.forge = forge;
        self.publisher = publisher;
    }

    fn delivery_on(&self) -> bool {
        self.cfg.delivery.enabled && self.forge.is_some() && self.publisher.is_some()
    }

    /// A run reported `Done`: queue its branch for delivery and try the first step at once.
    ///
    /// Called after the claim is released and the issue parked, so the issue is in exactly
    /// the state `advance_deliveries` requires; the immediate step is what makes the pull
    /// request appear at the moment the run ends rather than a poll interval later.
    pub(super) fn queue_delivery(
        &mut self,
        issue_id: &str,
        verdicts: Vec<ReviewVerdict>,
    ) -> anyhow::Result<()> {
        if !self.delivery_on() {
            return Ok(());
        }
        let Some(st) = self.store.get(issue_id)? else { return Ok(()) };
        if st.branch.is_none() {
            // A plain-directory workspace, or a prepare that never named one. Not an error:
            // there is no branch to push, so there is nothing to deliver.
            tracing::info!(issue_id, "run done with no branch to deliver; leaving it parked");
            return Ok(());
        }
        let json = if verdicts.is_empty() { None } else { Some(serde_json::to_string(&verdicts)?) };
        self.store.begin_delivery(self.clock.as_ref(), issue_id, json.as_deref())?;
        self.delivery_polled.insert(issue_id.to_string(), self.clock.mono());
        self.advance_delivery(issue_id)
    }

    /// Poll every delivery that is waiting on the outside world.
    ///
    /// A handed-off row is polled too, for its pull request's state alone: the operator owns
    /// it from there, but a row never polled again keeps publishing the handoff as a live
    /// failure after the operator has merged it (#101).
    ///
    /// Skips issues that anything else owns — a live run, a claim, a retry timer — because a
    /// push from under a running agent, or a hand-back racing a dispatch, is exactly the kind
    /// of double-ownership the claim exists to rule out. Each delivery is polled at
    /// `poll_interval_ms`, measured on the monotonic clock so a wall step cannot fire it early.
    pub(super) fn advance_deliveries(&mut self) -> anyhow::Result<()> {
        if !self.delivery_on() {
            return Ok(());
        }
        let now = self.clock.mono();
        let interval = self.cfg.delivery.poll_interval_ms;
        for d in self.store.deliveries()? {
            if !matches!(
                d.stage,
                DeliveryStage::Pending
                    | DeliveryStage::Awaiting
                    | DeliveryStage::Ready
                    | DeliveryStage::HandedOff
            ) {
                continue;
            }
            if self.running.contains_key(&d.issue_id) {
                continue;
            }
            if self.store.get(&d.issue_id)?.map(|s| s.phase) != Some(crate::model::Phase::Released)
            {
                continue;
            }
            let due = self
                .delivery_polled
                .get(&d.issue_id)
                .is_none_or(|t| now.saturating_since(*t) >= interval);
            if !due {
                continue;
            }
            self.delivery_polled.insert(d.issue_id.clone(), now);
            self.advance_delivery(&d.issue_id)?;
        }
        Ok(())
    }

    fn advance_delivery(&mut self, issue_id: &str) -> anyhow::Result<()> {
        let (Some(d), Some(st)) = (self.store.delivery(issue_id)?, self.store.get(issue_id)?)
        else {
            return Ok(());
        };
        if d.stage == DeliveryStage::HandedOff {
            return self.watch_handoff(issue_id, &d);
        }
        match self.step_delivery(issue_id, &st, &d) {
            Ok(()) => Ok(()),
            Err(StepError::Forge(e)) if e.retryable() => {
                // The row stays where it is and the next poll tries the same step again;
                // nothing here re-runs the agent over a network blip.
                tracing::warn!(issue_id, error = %e, "delivery step failed; will retry next poll");
                self.last_error = Some(format!("delivery {}: {e}", st.identifier));
                Ok(())
            }
            Err(StepError::Forge(e)) => {
                let reason = format!("delivery stopped: {e}");
                tracing::error!(issue_id, error = %e, "delivery stopped; handing the branch to the operator");
                self.hand_off(issue_id, &reason)?;
                Ok(())
            }
            Err(StepError::Other(e)) => Err(e),
        }
    }

    fn hand_off(&mut self, issue_id: &str, reason: &str) -> anyhow::Result<()> {
        self.store.set_delivery_stage(
            self.clock.as_ref(),
            issue_id,
            DeliveryStage::HandedOff,
            Some(reason),
        )?;
        self.store.note_error(self.clock.as_ref(), issue_id, reason)?;
        Ok(())
    }

    /// Read a handed-off pull request's state and nothing else: no push, no review request, no
    /// hand-back to an agent, because the handoff gave the branch to the operator. A failed read
    /// is logged and tried again next poll rather than handed off a second time, which would
    /// overwrite the reason the operator was handed it with one about a status read.
    fn watch_handoff(&mut self, issue_id: &str, d: &DeliveryRecord) -> anyhow::Result<()> {
        let Some(number) = d.pr_number else { return Ok(()) };
        let forge = self.forge.clone().expect("checked by delivery_on");
        match forge.pull_request(number) {
            Ok(p) if p.state != PrState::Open => self.close_delivery(issue_id, &p),
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::warn!(issue_id, pr = number, error = %e, "handed-off pull request unreadable; will retry next poll");
                Ok(())
            }
        }
    }

    /// The operator merged or closed the pull request, which ends the delivery however it got
    /// there. The issue's error is cleared with it: a handoff reason left on the row would keep
    /// a merged issue on every observer as a live failure (#101).
    fn close_delivery(&mut self, issue_id: &str, p: &PullRequest) -> anyhow::Result<()> {
        let how = if p.state == PrState::Merged { "merged" } else { "closed" };
        tracing::info!(
            issue_id,
            pr = p.number,
            how,
            "pull request is no longer open; delivery over"
        );
        self.store.set_delivery_stage(
            self.clock.as_ref(),
            issue_id,
            DeliveryStage::Closed,
            Some(how),
        )?;
        self.store.clear_note(self.clock.as_ref(), issue_id)?;
        Ok(())
    }

    /// One pass over the delivery state machine, from whatever stage the row is in to the
    /// furthest stage the outside world allows right now.
    fn step_delivery(
        &mut self,
        issue_id: &str,
        st: &IssueState,
        d: &DeliveryRecord,
    ) -> Result<(), StepError> {
        let forge = self.forge.clone().expect("checked by delivery_on");
        let publisher = self.publisher.clone().expect("checked by delivery_on");
        let clock = self.clock.clone();
        let mut d = d.clone();

        // The pull request's own state comes first, before anything touches the branch. Once
        // the operator merges, the ticket closes and `sweep_parked` reclaims the worktree and
        // may delete the branch — so a delivery row polled after that has no branch to speak
        // of, and reading that as "the issue has no branch" would hand a finished, merged
        // piece of work to the operator as a failure.
        //
        // Only for a row that is *waiting* on that pull request. A row a fresh run just set
        // back to pending is about to push again, and a previous pull request the operator
        // closed without merging is then exactly why a new one gets opened, not a reason to
        // stop.
        let mut pr = match d.pr_number {
            Some(number) if d.stage != DeliveryStage::Pending => Some(forge.pull_request(number)?),
            _ => None,
        };
        if let Some(p) = &pr
            && p.state != PrState::Open
        {
            return self.close_delivery(issue_id, p).map_err(StepError::Other);
        }

        if d.stage == DeliveryStage::Pending {
            let branch = st
                .branch
                .clone()
                .ok_or_else(|| ForgeError::Permanent("the issue has no branch".into()))?;
            let worktree = self.workspace.path_for(issue_id, &st.identifier);
            if !worktree.exists() {
                // Retrying a push from a directory that is gone would fail identically on
                // every poll; the branch, if it still exists, is on the row for the operator.
                return Err(ForgeError::Permanent(format!(
                    "worktree {} is gone before the branch was pushed",
                    worktree.display()
                ))
                .into());
            }
            // Every other issue's branch is a candidate base: a pull request whose work sits
            // on another issue's branch is based on that branch, so the two stay reviewable
            // apart. The publisher keeps only the candidates the remote has — a lower branch
            // still running, or done and not yet pushed, is not a base a pull request can be
            // opened against, and this branch finishing first is not a reason to hand it off.
            let candidates: Vec<String> = self
                .store
                .all()?
                .into_iter()
                .filter(|o| o.issue_id != issue_id)
                .filter_map(|o| o.branch)
                .collect();
            let default_base = self.cfg.delivery.base.clone();
            let remote = self.cfg.delivery.remote.clone();
            let base = publisher
                .stacked_on(&worktree, &branch, &remote, &default_base, &candidates)?
                .unwrap_or(default_base);
            let published = publisher.publish(&worktree, &branch, &remote, &base)?;
            if published.commits.is_empty() {
                tracing::info!(
                    issue_id,
                    branch,
                    "branch carries nothing over its base; nothing to deliver"
                );
                self.store.set_delivery_stage(
                    clock.as_ref(),
                    issue_id,
                    DeliveryStage::Closed,
                    Some("no commits to deliver"),
                )?;
                return Ok(());
            }

            let spec = self.pr_spec(issue_id, st, &branch, &base, &published.commits)?;
            let opened = match forge.open_pull_request(&spec) {
                Err(ForgeError::NothingToDeliver(why)) => {
                    tracing::info!(issue_id, why, "nothing to deliver");
                    self.store.set_delivery_stage(
                        clock.as_ref(),
                        issue_id,
                        DeliveryStage::Closed,
                        Some(&why),
                    )?;
                    return Ok(());
                }
                other => other?,
            };
            let fresh = d.pr_number != Some(opened.number);
            self.store.set_delivery_pr(
                clock.as_ref(),
                issue_id,
                opened.number,
                &opened.url,
                &base,
                &published.head_sha,
            )?;
            tracing::info!(
                issue_id, identifier = %st.identifier, pr = opened.number, url = %opened.url, base, head = %published.head_sha,
                fresh, "pull request {}", if fresh { "opened" } else { "updated" }
            );

            d = self.store.delivery(issue_id)?.ok_or_else(|| {
                StepError::Other(anyhow::anyhow!("delivery row vanished mid-step"))
            })?;
            // Re-read rather than trusting `opened`: the head the push just moved is what CI
            // and review are judged against from here on.
            pr = Some(forge.pull_request(opened.number)?);
        }

        let pr = pr.ok_or_else(|| ForgeError::Permanent("no pull request".into()))?;
        let number = pr.number;

        // The last run's verdicts, before anything reads the threads: a verdict whose reply
        // has not landed is not settled, and a thread not settled would otherwise be read as
        // open below and handed straight back to an agent. A reply that fails leaves the step
        // here — the poll retries it, and the threads are read only once every reply is in.
        if let Some(json) = &d.pending_verdicts {
            let verdicts: Vec<ReviewVerdict> = serde_json::from_str(json)?;
            self.apply_verdicts(issue_id, &pr, &verdicts)?;
            d = self.store.delivery(issue_id)?.ok_or_else(|| {
                StepError::Other(anyhow::anyhow!("delivery row vanished mid-step"))
            })?;
        }

        self.resolve_settled_threads(issue_id, number)?;

        // GitHub runs no CI on a pull request that cannot merge, so a CI wait here could only
        // end at the timeout with a reason that is false (#159). Ahead of the review request
        // too: the re-gate rebases, and a reviewer asked now would be asked about a stale head.
        if pr.mergeable == Some(false) {
            return self.regate(issue_id, st, &d, &pr);
        }

        if !d.review_requested && !self.cfg.delivery.reviewers.is_empty() {
            for r in &self.cfg.delivery.reviewers {
                forge.request_review(number, r)?;
            }
            // The read that makes the request mean something. Attached is either still
            // requested, or already answered on this head.
            let after = forge.pull_request(number)?;
            let reviews = forge.reviews(number)?;
            let missing: Vec<&str> = self
                .cfg
                .delivery
                .reviewers
                .iter()
                .filter(|r| {
                    !after.requested_reviewers.iter().any(|x| x == *r)
                        && !reviews
                            .iter()
                            .any(|rv| rv.reviewer == **r && rv.commit_sha == after.head_sha)
                })
                .map(String::as_str)
                .collect();
            if missing.is_empty() {
                tracing::info!(issue_id, pr = number, reviewers = ?self.cfg.delivery.reviewers, "review requested and verified attached");
                self.store.set_review_requested(clock.as_ref(), issue_id, None)?;
            } else {
                let reason = format!(
                    "review request accepted by the provider but attached nobody: {}",
                    missing.join(", ")
                );
                tracing::error!(issue_id, pr = number, missing = ?missing, "review request did not attach; handing off");
                self.store.set_review_requested(clock.as_ref(), issue_id, Some(&reason))?;
                self.hand_off(issue_id, &reason).map_err(StepError::Other)?;
                return Ok(());
            }
        }

        let ci = forge.ci_status(&pr.head_sha)?;
        if !matches!(ci, CiStatus::Pending { .. }) {
            self.ci_waits.remove(issue_id);
            if d.ci_pending.is_some() {
                self.store.set_ci_pending(clock.as_ref(), issue_id, None)?;
            }
        }
        match ci {
            CiStatus::Pending { running } => {
                return self.await_ci(issue_id, &d, &pr, &running);
            }
            CiStatus::Failure { failures } => {
                let named: Vec<String> = failures.iter().map(|f| f.name.clone()).collect();
                let fb = Feedback::Ci { pr_url: pr.url.clone(), failures };
                return self.open_round(
                    issue_id,
                    &d,
                    fb,
                    None,
                    &format!("CI red: {}", named.join(", ")),
                );
            }
            CiStatus::Success => {}
        }

        // Inline comments, then the findings reviewers left only in a review's summary (#126):
        // both are settled by the same verdict table, so a summary is handed back once and a
        // review arriving after `ready` takes the pull request out of it.
        let settled = self.store.verdicts_for(issue_id)?;
        let mut open = forge.review_comments(number)?;
        open.extend(summary_findings(
            &forge.reviews(number)?,
            &pr.head_sha,
            &self.cfg.delivery.summary_reviewers,
        ));
        open.retain(|c| !settled.contains_key(&c.id));
        if !open.is_empty() {
            let handed_before: Vec<String> = d
                .handed_comments
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_default();
            let unanswered_before: Vec<String> = open
                .iter()
                .filter(|c| handed_before.contains(&c.id))
                .map(|c| c.id.clone())
                .collect();
            let ids: Vec<String> = open.iter().map(|c| c.id.clone()).collect();
            let named: Vec<String> = open
                .iter()
                .map(|c| match &c.path {
                    Some(p) => format!("{} ({p})", c.id),
                    None if summary_review_id(&c.id).is_some() => format!("{} (summary)", c.id),
                    None => c.id.clone(),
                })
                .collect();
            let fb = Feedback::Review { pr_url: pr.url.clone(), comments: open, unanswered_before };
            return self.open_round(
                issue_id,
                &d,
                fb,
                Some(ids),
                &format!("review comments outstanding: {}", named.join(", ")),
            );
        }

        if d.stage != DeliveryStage::Ready {
            tracing::info!(issue_id, pr = number, url = %pr.url, "ready to merge; the rest is the operator's");
            self.store.set_delivery_stage(clock.as_ref(), issue_id, DeliveryStage::Ready, None)?;
        }
        Ok(())
    }

    /// CI is pending on `pr`'s head: wait, or hand off once nothing has completed for
    /// `ci_timeout_ms`. The wait is timed from when this head was first seen pending, never
    /// from crewd's own push — that clock handed off a ready pull request the moment a check
    /// re-ran on it, or the operator pushed a head of their own (#105).
    fn await_ci(
        &mut self,
        issue_id: &str,
        d: &DeliveryRecord,
        pr: &PullRequest,
        running: &[String],
    ) -> Result<(), StepError> {
        let clock = self.clock.clone();
        let now = clock.mono();
        let since = match self.ci_waits.get(issue_id) {
            Some((head, since)) if *head == pr.head_sha => *since,
            _ => {
                // A record for this head that is not in memory is one a restart left: wall time
                // is all that crosses a restart, so it alone decides how much of the wait is
                // already spent — the same trust `retry.due_at` places in it.
                let spent = match &d.ci_pending {
                    Some((head, at)) if *head == pr.head_sha => {
                        clock.wall().0.saturating_sub(*at).max(0) as u64
                    }
                    _ => {
                        self.store.set_ci_pending(clock.as_ref(), issue_id, Some(&pr.head_sha))?;
                        0
                    }
                };
                let since = Mono(now.0.saturating_sub(spent));
                self.ci_waits.insert(issue_id.to_string(), (pr.head_sha.clone(), since));
                since
            }
        };
        let timeout = self.cfg.delivery.ci_timeout_ms;
        let waited = now.saturating_since(since);
        if waited <= timeout {
            tracing::debug!(issue_id, pr = pr.number, head = %pr.head_sha, "awaiting CI");
            return Ok(());
        }
        let what = if running.is_empty() {
            "none has started".to_string()
        } else {
            format!("still running: {}", running.join(", "))
        };
        let reason = format!(
            "no check run completed on {} within {} of {}; {what}",
            pr.head_sha,
            fmt_ms(timeout),
            timestamp(clock.wall().0 - waited as i64)
        );
        tracing::warn!(issue_id, pr = pr.number, "{reason}; handing off");
        self.hand_off(issue_id, &reason).map_err(StepError::Other)
    }

    /// Hand the issue back to an agent with `feedback`, or to the operator if the rounds are
    /// spent. The bound is checked *before* charging so `max_rounds_per_pr = 3` means three
    /// rounds, and `what` names the outstanding items in the handoff so the operator is told
    /// what is left rather than only that something is.
    fn open_round(
        &mut self,
        issue_id: &str,
        d: &DeliveryRecord,
        feedback: Feedback,
        handed_comments: Option<Vec<String>>,
        what: &str,
    ) -> Result<(), StepError> {
        if self.rounds_spent(issue_id, d, what)? {
            return Ok(());
        }
        let json = serde_json::to_string(&feedback)?;
        let handed = handed_comments.map(|v| serde_json::to_string(&v)).transpose()?;
        let (rp, ri) = self.store.open_delivery_round(
            self.clock.as_ref(),
            issue_id,
            Some(&json),
            handed.as_deref(),
        )?;
        // The same path a `Continue` takes: unparked, a retry due now, the session resumed.
        self.store.unpark(self.clock.as_ref(), issue_id)?;
        self.store.schedule_retry(
            self.clock.as_ref(),
            issue_id,
            Wall(self.clock.wall().0),
            0,
            &format!("delivery: {what}"),
            // Due now, so there is no delay for a reservation to bridge.
            None,
        )?;
        tracing::info!(
            issue_id, pr = ?d.pr_number, kind = feedback.label(), rounds_pr = rp, rounds_issue = ri,
            "handing back to an agent: {what}"
        );
        Ok(())
    }

    /// Whether either round bound is reached, handing the pull request to the operator if so.
    fn rounds_spent(
        &mut self,
        issue_id: &str,
        d: &DeliveryRecord,
        what: &str,
    ) -> Result<bool, StepError> {
        let cfg = &self.cfg.delivery;
        if d.rounds_pr < cfg.max_rounds_per_pr && d.rounds_issue < cfg.max_rounds_per_issue {
            return Ok(false);
        }
        let reason = format!(
            "fix rounds exhausted ({} on this pull request, {} on this issue); {what}",
            d.rounds_pr, d.rounds_issue
        );
        tracing::warn!(issue_id, pr = ?d.pr_number, "{reason}; handing off");
        self.hand_off(issue_id, &reason).map_err(StepError::Other)?;
        Ok(true)
    }

    /// The pull request cannot merge into its base: send the branch back through the gate as if
    /// the run's `Done` were re-gated (#159), so the gate's rebase decides what happens next —
    /// a pass is delivered again, a conflict goes to the agent or a human by #111's rule, and a
    /// branch that already merged the base is not rebased (#122). One round, like any
    /// hand-back, so a base that keeps moving is bounded by the same `max_rounds_*`.
    ///
    /// The re-gate claims the issue and holds a slot, as every gate does; with none free it
    /// waits for the next poll uncharged. With no gate at all the agent is told instead.
    fn regate(
        &mut self,
        issue_id: &str,
        st: &IssueState,
        d: &DeliveryRecord,
        pr: &PullRequest,
    ) -> Result<(), StepError> {
        let what = format!("pull request #{} cannot merge into {}", pr.number, pr.base);
        let Some(gate) = self.gate.clone() else {
            let output = format!(
                "{what}: the base moved under it and the two conflict. Merge or rebase onto {} \
                 and finish again.",
                pr.base
            );
            return self.open_round(issue_id, d, Feedback::Gate { output }, None, &what);
        };
        let Some(issue) = self.seen.get(issue_id).cloned() else {
            tracing::debug!(issue_id, "{what}; issue not seen yet, re-gating next poll");
            return Ok(());
        };
        let reserved = self.reservations().map_err(StepError::Other)?;
        if self.global_slots(&reserved) == 0 || self.state_slots(&issue.state_key(), &reserved) == 0
        {
            tracing::debug!(issue_id, "{what}; no slot to gate in, re-gating next poll");
            return Ok(());
        }
        if self.rounds_spent(issue_id, d, &what)? {
            return Ok(());
        }
        let (rp, ri) = self.store.open_delivery_round(self.clock.as_ref(), issue_id, None, None)?;
        self.store.unpark(self.clock.as_ref(), issue_id)?;
        if !self.store.claim(self.clock.as_ref(), issue_id)? {
            return Err(StepError::Other(anyhow::anyhow!(
                "{issue_id}: released issue refused a claim"
            )));
        }
        let workspace = self.workspace.path_for(issue_id, &st.identifier);
        let handle = gate.start(&issue, &workspace);
        let now = self.clock.mono();
        tracing::info!(
            issue_id, pr = pr.number, rounds_pr = rp, rounds_issue = ri,
            workspace = %workspace.display(), "{what}; re-gating instead of awaiting CI"
        );
        let run = Running {
            run_id: format!("{issue_id}-regate-{}", self.clock.wall().0),
            state_at_start: issue.state.clone(),
            issue,
            handle: Arc::new(Regated),
            started: now,
            workspace,
            transcript: None,
            last_progress: Progress::default(),
            last_progress_at: now,
            verdicts: Vec::new(),
            _broker: None,
        };
        self.gating.insert(issue_id.to_string(), Gating { run, handle, started: now });
        Ok(())
    }

    /// Reply on each verdict's thread, and record the verdict once the reply has landed.
    ///
    /// In that order, because the record is what excludes the thread from every later poll: a
    /// verdict recorded before its reply landed would be hidden from the reviewer for good over
    /// one failed request, while delivery went on to `Ready` as if they had been told. So a
    /// reply that fails leaves its verdict in the queue and the thread unsettled, the rest of
    /// the queue is still attempted, and the first failure is returned so the poll retries —
    /// or, if it will not resolve, hands off with the verdicts still on the row for the
    /// operator. The one write that can now happen twice is a reply whose record then failed
    /// to commit, and a duplicate reply is the cheaper mistake.
    fn apply_verdicts(
        &mut self,
        issue_id: &str,
        pr: &PullRequest,
        verdicts: &[ReviewVerdict],
    ) -> Result<(), StepError> {
        let forge = self.forge.clone().expect("checked by delivery_on");
        let settled = self.store.verdicts_for(issue_id)?;
        let mut unapplied: Vec<ReviewVerdict> = Vec::new();
        let mut failure: Option<ForgeError> = None;
        // Read once, and only if a summary is answered: its verdict quotes the finding.
        let mut reviews: Option<Result<Vec<Review>, ForgeError>> = None;
        for v in verdicts {
            if settled.contains_key(&v.comment_id) {
                // Settled by an earlier round; the first verdict stands and is not re-argued.
                continue;
            }
            let body = match v.verdict {
                Verdict::Accepted => format!("**Accepted** — resolved in {}.", v.detail),
                Verdict::Rejected => format!("**Rejected** — {}", v.detail),
            };
            let posted = match summary_review_id(&v.comment_id) {
                // A summary has no thread to reply on, so the verdict goes on the pull request.
                Some(review_id) => match reviews.get_or_insert_with(|| forge.reviews(pr.number)) {
                    Ok(all) => {
                        let r = all.iter().find(|r| r.id == review_id);
                        forge.comment(pr.number, &verdict_comment(&v.comment_id, r, &body))
                    }
                    Err(e) => Err(e.clone()),
                },
                None => forge.reply(pr.number, &v.comment_id, &body),
            };
            match posted {
                Ok(()) => {
                    self.store.record_verdict(
                        self.clock.as_ref(),
                        issue_id,
                        pr.number,
                        &v.comment_id,
                        v.verdict,
                        &v.detail,
                    )?;
                    tracing::info!(issue_id, pr = pr.number, comment = %v.comment_id, verdict = v.verdict.as_str(), detail = %v.detail, "review comment settled");
                }
                Err(e) => {
                    tracing::warn!(issue_id, comment = %v.comment_id, error = %e, "reply failed; the thread stays unsettled and the reply is retried");
                    unapplied.push(v.clone());
                    failure.get_or_insert(e);
                }
            }
        }
        let remaining =
            if unapplied.is_empty() { None } else { Some(serde_json::to_string(&unapplied)?) };
        self.store.set_pending_verdicts(self.clock.as_ref(), issue_id, remaining.as_deref())?;
        match failure {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    /// Resolve the thread of every comment on this pull request whose verdict is recorded and
    /// whose thread is not yet resolved (#89) — accepted and rejected alike, since a rejection's
    /// reason stays visible on a resolved thread and an open one reads as unfinished work.
    ///
    /// A step of its own, after `apply_verdicts` rather than inside it, so a verdict is still
    /// settled by its reply alone: a resolve that fails is logged and retried on the next poll,
    /// and costs no second reply, no round and no hold on readiness, which stays computed from
    /// the verdict table and never from the provider's `isResolved`. So the failure is not
    /// returned — that would stop the step before CI and the threads are read, and hand a
    /// ready pull request off over a cosmetic write — but it is surfaced as `advance_delivery`
    /// surfaces one: a transient failure in `last_error`, a permanent one on the issue's row,
    /// logged at `error`, since it will keep failing until the operator fixes the credential.
    fn resolve_settled_threads(&mut self, issue_id: &str, number: u64) -> Result<(), StepError> {
        let forge = self.forge.clone().expect("checked by delivery_on");
        let (summaries, comments): (Vec<String>, Vec<String>) = self
            .store
            .unresolved_verdicts(issue_id, number)?
            .into_iter()
            .partition(|c| summary_review_id(c).is_some());
        // A review's summary has no thread; its verdict's comment is all there is to see.
        for s in summaries {
            self.store.mark_thread_resolved(self.clock.as_ref(), issue_id, &s)?;
        }
        if comments.is_empty() {
            return Ok(());
        }
        let results = forge.resolve_threads(number, &comments);
        for (comment, result) in comments.into_iter().zip(results) {
            match result {
                Ok(()) => {
                    self.store.mark_thread_resolved(self.clock.as_ref(), issue_id, &comment)?;
                    tracing::info!(issue_id, pr = number, comment, "review thread resolved");
                }
                Err(e) if e.retryable() => {
                    tracing::warn!(issue_id, pr = number, comment, error = %e, "resolving a settled thread failed; retried next poll");
                    self.last_error = Some(format!("resolving review thread {comment}: {e}"));
                }
                Err(e) => {
                    tracing::error!(issue_id, pr = number, comment, error = %e, "resolving a settled thread was refused; retried next poll, but will not succeed on its own");
                    let msg = format!("resolving review thread {comment} refused: {e}");
                    self.store.note_error(self.clock.as_ref(), issue_id, &msg)?;
                }
            }
        }
        Ok(())
    }

    /// The run's verdicts as delivery will apply them: every acceptance that names a commit the
    /// branch does not carry is dropped, and its comment stays outstanding.
    ///
    /// An acceptance's detail is the commit that resolved the comment — that is what the reply
    /// on the thread will say — so a detail that is no commit of this branch's is a bare
    /// acknowledgement in the accepted form, and recording it would tell the reviewer a fix
    /// exists where none does. Called from `harvest_finished` at the moment the run ends,
    /// before the gate's rebase rewrites the shas; a repository that cannot be asked leaves
    /// the comment outstanding too, because "could not check" is not "on the branch".
    pub(super) fn verified_verdicts(
        &self,
        issue_id: &str,
        worktree: &std::path::Path,
        verdicts: Vec<ReviewVerdict>,
    ) -> Vec<ReviewVerdict> {
        if !self.delivery_on() || !verdicts.iter().any(|v| v.verdict == Verdict::Accepted) {
            return verdicts;
        }
        let branch = match self.store.get(issue_id) {
            Ok(st) => st.and_then(|s| s.branch),
            Err(e) => {
                tracing::warn!(issue_id, error = %e, "could not read the branch to check verdicts against");
                None
            }
        };
        // No branch means nothing will be delivered, so nothing here will be applied either.
        let Some(branch) = branch else { return verdicts };
        let publisher = self.publisher.clone().expect("checked by delivery_on");
        verdicts
            .into_iter()
            .filter(|v| {
                if v.verdict != Verdict::Accepted {
                    return true;
                }
                match publisher.carries(worktree, &branch, &v.detail) {
                    Ok(true) => true,
                    Ok(false) => {
                        tracing::warn!(
                            issue_id, comment = %v.comment_id, detail = %v.detail, branch,
                            "acceptance names a commit the branch does not carry; the comment stays outstanding"
                        );
                        false
                    }
                    Err(e) => {
                        tracing::warn!(
                            issue_id, comment = %v.comment_id, detail = %v.detail, error = %e,
                            "could not check the commit an acceptance names; the comment stays outstanding"
                        );
                        false
                    }
                }
            })
            .collect()
    }

    /// The pull request's title and body, derived from the run record — never composed by the
    /// agent. What a reviewer needs first is what the issue asked for and what the branch
    /// actually contains; both are facts the orchestrator holds and the agent could only
    /// restate.
    fn pr_spec(
        &self,
        issue_id: &str,
        st: &IssueState,
        branch: &str,
        base: &str,
        commits: &[String],
    ) -> Result<PullRequestSpec, StepError> {
        let issue = self.seen.get(issue_id);
        let title = match issue {
            Some(i) if !i.title.is_empty() => format!("{}: {}", i.identifier, i.title),
            _ => format!("{}: {}", st.identifier, branch),
        };

        let mut body = String::new();
        match issue.and_then(|i| i.url.as_deref()) {
            Some(url) => body.push_str(&format!("Closes {url}\n\n")),
            None => body.push_str(&format!("Issue: {}\n\n", st.identifier)),
        }
        if base != self.cfg.delivery.base {
            body.push_str(&format!(
                "Stacked on `{base}`: this work sits on another issue's branch and is reviewable \
                 apart from it. Merge that pull request first.\n\n"
            ));
        }
        body.push_str("## Commits\n\n");
        for c in commits {
            body.push_str(&format!("- {c}\n"));
        }

        let runs = self.store.runs_for(issue_id)?;
        let turns: u32 = runs.iter().map(|r| r.turns).sum();
        let (input, output, uncounted) =
            runs.iter().fold((0u64, 0u64, 0u32), |acc, r| match (r.in_tok, r.out_tok) {
                (Some(i), Some(o)) => (acc.0 + i, acc.1 + o, acc.2),
                _ if r.ended_at.is_some() => (acc.0, acc.1, acc.2 + 1),
                _ => acc,
            });
        body.push_str(&format!(
            "\n## Provenance\n\nOpened by crewd from {} run{} of {} ({} turns, {} tokens in / {} \
             out{}). The verdict on each review comment is recorded as a reply on its thread; \
             merging is left to a human.\n",
            runs.len(),
            if runs.len() == 1 { "" } else { "s" },
            st.identifier,
            turns,
            input,
            output,
            if uncounted > 0 { format!(", {uncounted} run(s) uncounted") } else { String::new() },
        ));

        Ok(PullRequestSpec { title, body, head: branch.to_string(), base: base.to_string() })
    }

    /// Delivery state for the snapshot, keyed by issue id. One query per tick, like run history.
    pub(super) fn delivery_views(&self) -> anyhow::Result<HashMap<String, DeliveryView>> {
        if self.forge.is_none() {
            return Ok(HashMap::new());
        }
        Ok(self
            .store
            .deliveries()?
            .iter()
            .map(|d| (d.issue_id.clone(), DeliveryView::from(d)))
            .collect())
    }
}
