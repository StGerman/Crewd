//! Durable scheduler state.
//!
//! This is a cache of *judgment*, not a system of record. The tracker owns what work exists and
//! the filesystem owns the workspaces; SQLite holds only what cannot be re-derived — attempt
//! counts, quarantine flags, turn budgets. Delete the file and the service degrades to stateless
//! re-polling, which is dumber but still correct. That property is what makes adding a database
//! safe here, where the spec left the retry queue unpersisted (§18.2, listed as an open TODO).

pub mod schema;

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::clock::{Clock, Wall};
use crate::model::{ErrorClass, Phase, Verdict};
use crate::worker::{ModelChoice, TokenUsage};

/// The park an operator's unblock leaves behind: a state no tracker ever reports, since
/// `state_key` is a trimmed, lowercased tracker state and never carries parentheses. It differs
/// from every real key, so the next `dispatch_new` treats the park as stale and launches, while
/// the row stays parked for `sweep_parked` until then (see [`Store::unblock`]).
pub const UNBLOCKED: &str = "(unblocked)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueState {
    pub issue_id: String,
    pub identifier: String,
    pub worktree_key: String,
    pub phase: Phase,
    pub attempt: u32,
    pub consecutive_fail: u32,
    pub last_fail_class: Option<ErrorClass>,
    pub cumulative_turns: u32,
    pub miss_count: u32,
    /// Tracker state this issue was parked in after a `Done`/`Blocked` verdict, if any.
    pub parked_state: Option<String>,
    pub quarantined_at: Option<i64>,
    pub last_error_class: Option<ErrorClass>,
    pub last_error: Option<String>,
    pub task_ref: Option<String>,
    /// The conversation continuations for this issue resume into, once one has been named.
    pub session_id: Option<String>,
    /// The branch `Workspace::prepare` actually checked out for this issue's most recent
    /// dispatch, recorded at that call rather than recomputed later from `identifier`. `None`
    /// before the first dispatch, and cleared back to `None` when cleanup deletes the ref —
    /// so a `Some` here always names a branch that still exists.
    pub branch: Option<String>,
}

impl IssueState {
    pub fn is_quarantined(&self) -> bool {
        self.quarantined_at.is_some()
    }
}

pub use libcrew::RunRecord;

/// What [`Store::start_run`] records about a run before its worker exists.
pub struct RunStart<'a> {
    pub run_id: &'a str,
    pub issue_id: &'a str,
    pub session_id: &'a str,
    pub transcript: Option<&'a Path>,
    pub model: &'a ModelChoice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryEntry {
    pub issue_id: String,
    pub due_at: i64,
    pub attempt: u32,
    pub reason: Option<String>,
    /// The state key whose concurrency slot this retry holds until it is dispatched, or `None`
    /// when it holds none. See [`Store::schedule_retry`].
    pub reserved_state: Option<String>,
}

fn retry_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<RetryEntry> {
    Ok(RetryEntry {
        issue_id: r.get(0)?,
        due_at: r.get(1)?,
        attempt: r.get::<_, i64>(2)? as u32,
        reason: r.get(3)?,
        reserved_state: r.get(4)?,
    })
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        schema::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Wraps a connection the caller has already migrated. For tests that need to stage a
    /// database at an older schema version before letting `migrate` run on it.
    #[cfg(test)]
    pub(crate) fn from_connection(conn: Connection) -> Self {
        Self { conn: Mutex::new(conn) }
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Insert-if-absent. Returns the current row either way.
    pub fn ensure(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        identifier: &str,
        worktree_key: &str,
    ) -> rusqlite::Result<IssueState> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO issue_state (issue_id, identifier, worktree_key, phase, updated_at)
             VALUES (?1, ?2, ?3, 'released', ?4)
             ON CONFLICT(issue_id) DO UPDATE SET identifier = ?2, updated_at = ?4",
            params![issue_id, identifier, worktree_key, clock.wall().0],
        )?;
        Self::get_locked(&conn, issue_id).map(|o| o.expect("row inserted above"))
    }

    pub fn get(&self, issue_id: &str) -> rusqlite::Result<Option<IssueState>> {
        let conn = self.conn.lock().unwrap();
        Self::get_locked(&conn, issue_id)
    }

    fn get_locked(conn: &Connection, issue_id: &str) -> rusqlite::Result<Option<IssueState>> {
        conn.query_row(
            "SELECT issue_id, identifier, worktree_key, phase, attempt, consecutive_fail,
                    last_fail_class, cumulative_turns, miss_count, parked_state, quarantined_at,
                    last_error_class, last_error, task_ref, session_id, branch
             FROM issue_state WHERE issue_id = ?1",
            params![issue_id],
            Self::row_to_state,
        )
        .optional()
    }

    fn row_to_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<IssueState> {
        let phase: String = row.get(3)?;
        let last_fail: Option<String> = row.get(6)?;
        let last_err_class: Option<String> = row.get(11)?;
        Ok(IssueState {
            issue_id: row.get(0)?,
            identifier: row.get(1)?,
            worktree_key: row.get(2)?,
            phase: match phase.as_str() {
                "queued" => Phase::Queued,
                "running" => Phase::Running,
                "retry" => Phase::RetryQueued,
                "quarantine" => Phase::Quarantined,
                _ => Phase::Released,
            },
            attempt: row.get::<_, i64>(4)? as u32,
            consecutive_fail: row.get::<_, i64>(5)? as u32,
            last_fail_class: last_fail.as_deref().and_then(ErrorClass::parse),
            cumulative_turns: row.get::<_, i64>(7)? as u32,
            miss_count: row.get::<_, i64>(8)? as u32,
            parked_state: row.get(9)?,
            quarantined_at: row.get(10)?,
            last_error_class: last_err_class.as_deref().and_then(ErrorClass::parse),
            last_error: row.get(12)?,
            task_ref: row.get(13)?,
            session_id: row.get(14)?,
            branch: row.get(15)?,
        })
    }

    pub fn all(&self) -> rusqlite::Result<Vec<IssueState>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT issue_id, identifier, worktree_key, phase, attempt, consecutive_fail,
                    last_fail_class, cumulative_turns, miss_count, parked_state, quarantined_at,
                    last_error_class, last_error, task_ref, session_id, branch
             FROM issue_state ORDER BY identifier",
        )?;
        let rows = stmt.query_map([], Self::row_to_state)?;
        rows.collect()
    }

    /// Every issue the store currently holds a claim for.
    ///
    /// At startup this is the set of runs the *previous* process was executing, because
    /// nothing else can write the claim: a scheduler releases every claim it holds on the way
    /// out. Filtering in Rust rather than SQL keeps the column list in one place, and this
    /// table carries one row per issue ever seen — it is read whole on every tick already.
    pub fn claimed(&self) -> rusqlite::Result<Vec<IssueState>> {
        Ok(self.all()?.into_iter().filter(|s| s.phase == Phase::Running).collect())
    }

    /// Issues parked after a `Done`/`Blocked` verdict and not currently running.
    ///
    /// These are the rows the scheduler has stopped watching: not in `running`, not waiting on
    /// a retry, and invisible to the active-state poll once the ticket closes. The parked-issue
    /// sweep is the only reader. A running issue cannot also be parked — `unpark` precedes
    /// every launch — so the phase filter is belt-and-braces against a future path that forgets
    /// that, rather than a case that occurs today.
    pub fn parked(&self) -> rusqlite::Result<Vec<IssueState>> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|s| s.parked_state.is_some() && s.phase != Phase::Running)
            .collect())
    }

    /// Close every still-open run row for an issue, returning how many there were.
    ///
    /// A run row opens before the worker exists and closes in `finish_run`. A process killed
    /// mid-run closes nothing, so the row reads as in-flight forever. Recovery closes it with
    /// what is actually known: the turn count stays at its last checkpoint
    /// ([`Store::record_progress`]), and the token columns stay NULL, because the total that
    /// run would have reported died with the process that was waiting for it. It lands in
    /// `token_totals`'s uncounted tally, same as a killed run.
    ///
    /// The checkpointed turns are folded into the issue's `cumulative_turns` here, in the same
    /// transaction that closes the rows. Every other path that ends a run calls `add_turns`
    /// with the handle's final count; this is the one path with no handle, and without the fold
    /// the interrupted attempt would cost the per-issue budget nothing — which is wrong by
    /// exactly the work most worth accounting for, since it is the work the resumed
    /// conversation is about to build on. One transaction, so a second kill between the two
    /// statements cannot count the same turns twice at the next startup.
    pub fn close_open_runs(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        outcome: &str,
    ) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE issue_state
             SET cumulative_turns = cumulative_turns
                 + (SELECT COALESCE(SUM(turns), 0) FROM run
                    WHERE issue_id = ?1 AND ended_at IS NULL)
             WHERE issue_id = ?1",
            params![issue_id],
        )?;
        let closed = tx.execute(
            "UPDATE run SET ended_at = ?2, outcome = ?3 WHERE issue_id = ?1 AND ended_at IS NULL",
            params![issue_id, clock.wall().0, outcome],
        )?;
        tx.commit()?;
        Ok(closed)
    }

    pub fn set_phase(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        phase: Phase,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET phase = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, phase.label(), clock.wall().0],
        )?;
        Ok(())
    }

    /// Reserve an issue. Returns false if it is already claimed — this is the guard that makes
    /// duplicate dispatch impossible, and it must commit *before* a worker is spawned.
    pub fn claim(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE issue_state SET phase = 'running', updated_at = ?2
             WHERE issue_id = ?1 AND phase NOT IN ('running', 'quarantine')",
            params![issue_id, clock.wall().0],
        )?;
        Ok(n == 1)
    }

    pub fn release(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state
             SET phase = 'released', attempt = 0, consecutive_fail = 0, last_fail_class = NULL,
                 miss_count = 0, updated_at = ?2
             WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        conn.execute("DELETE FROM retry WHERE issue_id = ?1", params![issue_id])?;
        Ok(())
    }

    /// Release a claim exactly as an account-wide rate limit found it: `phase` back to
    /// `released`, but `attempt`, `consecutive_fail` and `last_fail_class` all left alone.
    ///
    /// [`Store::release`] zeroes those columns, which is right after a `Done` or `Blocked`
    /// verdict but wrong here — the run this interrupted did not fail on its own account, so
    /// the next attempt must not look like a fresh start (#37). No retry row to clear either:
    /// this is only ever called on an issue the claim just found `running`, which cannot also
    /// hold one.
    pub fn release_for_rate_limit(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET phase = 'released', updated_at = ?2 WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        Ok(())
    }

    /// Record a failure and decide whether it quarantines the issue.
    ///
    /// Permanent classes quarantine immediately. Retryable classes quarantine after
    /// `quarantine_after` consecutive failures *of the same class* — a deterministic fault
    /// wearing a transient mask still gets caught. A different class resets the streak.
    pub fn record_failure(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        class: ErrorClass,
        msg: &str,
        quarantine_after: u32,
    ) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = clock.wall().0;

        let prev: Option<String> = conn
            .query_row(
                "SELECT last_fail_class FROM issue_state WHERE issue_id = ?1",
                params![issue_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let same_class = prev.as_deref().and_then(ErrorClass::parse) == Some(class);
        let streak_sql = if same_class { "consecutive_fail + 1" } else { "1" };

        conn.execute(
            &format!(
                "UPDATE issue_state
                 SET attempt = attempt + 1,
                     consecutive_fail = {streak_sql},
                     last_fail_class = ?2,
                     last_error_class = ?2,
                     last_error = ?3,
                     updated_at = ?4
                 WHERE issue_id = ?1"
            ),
            params![issue_id, class.as_str(), msg, now],
        )?;

        let streak: u32 = conn.query_row(
            "SELECT consecutive_fail FROM issue_state WHERE issue_id = ?1",
            params![issue_id],
            |r| r.get::<_, i64>(0).map(|v| v as u32),
        )?;

        let quarantine = !class.retryable() || streak >= quarantine_after;
        if quarantine {
            conn.execute(
                "UPDATE issue_state SET quarantined_at = ?2, phase = 'quarantine', updated_at = ?2
                 WHERE issue_id = ?1",
                params![issue_id, now],
            )?;
            conn.execute("DELETE FROM retry WHERE issue_id = ?1", params![issue_id])?;
        }
        Ok(quarantine)
    }

    /// Clear a quarantine, reporting whether there was one to clear.
    ///
    /// Guarded on `quarantined_at IS NOT NULL` rather than applied unconditionally, because
    /// this also resets `phase` to `released`: run it against an issue that is *not*
    /// quarantined and it drops a live claim out from under a running agent, and the next tick
    /// dispatches a second one onto the same worktree. An operator action that names the wrong
    /// issue must be a no-op, and the only place that can be decided without a race is here,
    /// in the same statement that would have done the damage.
    pub fn unquarantine(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE issue_state
             SET quarantined_at = NULL, phase = 'released', attempt = 0, consecutive_fail = 0,
                 last_fail_class = NULL, updated_at = ?2
             WHERE issue_id = ?1 AND quarantined_at IS NOT NULL",
            params![issue_id, clock.wall().0],
        )?;
        Ok(n == 1)
    }

    /// Lift a park an operator has resolved, reporting whether there was one to lift (#108).
    ///
    /// Guarded for the same reason as [`Store::unquarantine`], and in the same statement: only
    /// a parked issue in phase `released`, unquarantined and with no retry row, is unparked.
    /// A running or gating issue holds phase `running`, and a retry-queued one holds a retry
    /// row, so neither matches; clearing a park on anything live would let the next tick
    /// dispatch a second agent onto a worktree already in use. A delivery still `pending`,
    /// `awaiting` or `ready` owns the parked `Done` too: lifting its park would let delivery push
    /// the branch, or hand it back, in the same tick `dispatch_new` puts an agent on it. So does
    /// a `handed_off` one, which gave the branch to the operator: a dispatch there would end in a
    /// `Done` that restarts delivery, stepping around `max_rounds_per_issue` and the handoff
    /// itself. Only `redispatched` and `closed` are left out, since neither pushes nor hands
    /// back, and a re-dispatched run that ends `Blocked` keeps its row `redispatched`. The
    /// claim is never touched — the next `dispatch_new` takes it the ordinary way. The parked
    /// note goes too, since it names the problem the operator has just resolved.
    ///
    /// The park is not cleared but re-pointed at [`UNBLOCKED`], a state no ticket is ever in.
    /// Clearing it opened a window (review on #112): a ticket that closed between the
    /// operator's unblock and the next tick was neither dispatched — `dispatch_new` only sees
    /// active tickets — nor swept, since `sweep_parked` walks parked rows only, stranding its
    /// worktree and branch. Re-pointed, the row stays in the sweep's view, and `dispatch_new`'s
    /// ordinary "the state moved" branch unparks and launches it once it is still eligible.
    pub fn unblock(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE issue_state
             SET parked_state = ?3, last_error = NULL, last_error_class = NULL, updated_at = ?2
             WHERE issue_id = ?1 AND parked_state IS NOT NULL AND parked_state <> ?3
               AND phase = 'released'
               AND quarantined_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM retry WHERE retry.issue_id = ?1)
               AND NOT EXISTS (SELECT 1 FROM delivery WHERE delivery.issue_id = ?1
                               AND delivery.stage IN ('pending', 'awaiting', 'ready', 'handed_off'))",
            params![issue_id, clock.wall().0, UNBLOCKED],
        )?;
        Ok(n == 1)
    }

    /// Park an issue in its current tracker state after a `Done` or `Blocked` verdict.
    ///
    /// Releasing the claim alone is not enough: the ticket is usually still sitting in an
    /// active state, so the very next tick would pick it straight back up. That is the spec's
    /// continuation runaway arriving by a different route. Parking says "nothing has changed
    /// since we last finished with this", and lifts the moment the state moves.
    pub fn park(&self, clock: &dyn Clock, issue_id: &str, state_key: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET parked_state = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, state_key, clock.wall().0],
        )?;
        Ok(())
    }

    pub fn unpark(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET parked_state = NULL, updated_at = ?2 WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        Ok(())
    }

    /// Name the conversation this issue's continuations resume into, or clear it.
    ///
    /// Clearing is the degradation path: a resume target the CLI no longer knows about fails
    /// every attempt identically, so the run that discovers it drops the id and the next
    /// attempt starts fresh instead of retrying its way into quarantine.
    pub fn set_session(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        session_id: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET session_id = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, session_id, clock.wall().0],
        )?;
        Ok(())
    }

    /// Record `body_hash` as what this issue's session has now been shown, and return what it
    /// had been shown before — `None` when nothing was recorded (#109).
    ///
    /// One call rather than a read and a write, so the comparison and the update cannot be
    /// split by another launch of the same issue.
    pub fn swap_session_body(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        body_hash: &str,
    ) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let before: Option<String> = conn
            .query_row(
                "SELECT session_body FROM issue_state WHERE issue_id = ?1",
                params![issue_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        conn.execute(
            "UPDATE issue_state SET session_body = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, body_hash, clock.wall().0],
        )?;
        Ok(before)
    }

    /// Queue `feedback_json` for this issue's next run: why the last one ended, when neither a
    /// retry reason nor delivery carries it — a rebase conflict that parked the issue (#109).
    pub fn set_pending_feedback(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        feedback_json: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET pending_feedback = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, feedback_json, clock.wall().0],
        )?;
        Ok(())
    }

    /// Read what `set_pending_feedback` queued and clear it, in one step, so exactly one run is
    /// told about it.
    pub fn take_pending_feedback(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
    ) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let fb: Option<String> = conn
            .query_row(
                "SELECT pending_feedback FROM issue_state WHERE issue_id = ?1",
                params![issue_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if fb.is_some() {
            conn.execute(
                "UPDATE issue_state SET pending_feedback = NULL, updated_at = ?2
                 WHERE issue_id = ?1",
                params![issue_id, clock.wall().0],
            )?;
        }
        Ok(fb)
    }

    /// Record the branch `Workspace::prepare` actually checked out, or clear it once cleanup
    /// deletes that ref.
    ///
    /// Written before the worker exists, the same category as the claim and the session id:
    /// the real name only exists once `prepare` has returned, and the child cannot be what
    /// records it. Recomputing it later from `identifier` was the bug this replaces —
    /// `Store::ensure` can rename `identifier` after this call, and `Workspace::remove` deletes
    /// the branch whenever git's merged check says it carries nothing new, so a name that still
    /// resolves is not proof the ref still exists. Callers clear it (`None`) exactly when
    /// `Removed::branch_deleted` says so, and leave it alone otherwise.
    pub fn set_branch(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        branch: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET branch = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, branch, clock.wall().0],
        )?;
        Ok(())
    }

    /// Record why an issue parked `Blocked`, in the same column a failure would use.
    ///
    /// `Blocked` is not a failure — `attempt` and the quarantine streak are left alone — but it
    /// is the one verdict that ends with a human needing to act, and until this the reason went
    /// to the log and nowhere an operator reading the dashboard could see it. A rebase conflict
    /// naming its paths is the case that made that cost real.
    pub fn set_note(&self, clock: &dyn Clock, issue_id: &str, note: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET last_error = ?2, last_error_class = NULL, updated_at = ?3
             WHERE issue_id = ?1",
            params![issue_id, note, clock.wall().0],
        )?;
        Ok(())
    }

    /// Count one more consecutive handoff-gate failure and return the streak including it.
    ///
    /// Persisted rather than kept on the scheduler, because a streak that lives in memory is
    /// reset by every restart and `gate.max_failures` then never escalates an issue whose
    /// suite keeps failing across restarts (#46). Deliberately not on the published `Snapshot`:
    /// mid-streak there is nothing for an operator to do, and the `Blocked` note that ends one
    /// already names the count.
    pub fn bump_gate_failures(&self, issue_id: &str) -> rusqlite::Result<u32> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "UPDATE issue_state SET gate_failures = gate_failures + 1 WHERE issue_id = ?1
             RETURNING gate_failures",
            params![issue_id],
            |r| r.get::<_, i64>(0).map(|v| v as u32),
        )
    }

    pub fn clear_gate_failures(&self, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET gate_failures = 0 WHERE issue_id = ?1",
            params![issue_id],
        )?;
        Ok(())
    }

    pub fn add_turns(&self, issue_id: &str, turns: u32) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET cumulative_turns = cumulative_turns + ?2 WHERE issue_id = ?1",
            params![issue_id, turns as i64],
        )?;
        Ok(())
    }

    /// Returns the miss count after incrementing.
    pub fn bump_miss(&self, issue_id: &str) -> rusqlite::Result<u32> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET miss_count = miss_count + 1 WHERE issue_id = ?1",
            params![issue_id],
        )?;
        conn.query_row(
            "SELECT miss_count FROM issue_state WHERE issue_id = ?1",
            params![issue_id],
            |r| r.get::<_, i64>(0).map(|v| v as u32),
        )
    }

    pub fn clear_miss(&self, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET miss_count = 0 WHERE issue_id = ?1",
            params![issue_id],
        )?;
        Ok(())
    }

    /// `reserve` names the state whose slot the retry holds while it waits, and is `Some` only
    /// for a continuation (#86): a slot handed to a new dispatch during the delay is the
    /// continuing issue's place in the queue, lost. A failure backoff passes `None`, or one
    /// failing issue could hold capacity for as long as its backoff grows.
    pub fn schedule_retry(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        due_at: Wall,
        attempt: u32,
        reason: &str,
        reserve: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO retry (issue_id, due_at, attempt, reason, reserved_state)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(issue_id) DO UPDATE
             SET due_at = ?2, attempt = ?3, reason = ?4, reserved_state = ?5",
            params![issue_id, due_at.0, attempt as i64, reason, reserve],
        )?;
        conn.execute(
            "UPDATE issue_state SET phase = 'retry', updated_at = ?2 WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        Ok(())
    }

    pub fn due_retries(&self, now: Wall) -> rusqlite::Result<Vec<RetryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT issue_id, due_at, attempt, reason, reserved_state FROM retry
             WHERE due_at <= ?1 ORDER BY due_at",
        )?;
        let rows = stmt.query_map(params![now.0], retry_entry)?;
        rows.collect()
    }

    pub fn all_retries(&self) -> rusqlite::Result<Vec<RetryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT issue_id, due_at, attempt, reason, reserved_state FROM retry ORDER BY due_at",
        )?;
        let rows = stmt.query_map([], retry_entry)?;
        rows.collect()
    }

    pub fn clear_retry(&self, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM retry WHERE issue_id = ?1", params![issue_id])?;
        Ok(())
    }

    /// `transcript` is recorded here, before the worker exists, for the same reason the claim
    /// and the session name are: a run that dies in its first second is exactly the one someone
    /// will want the bytes from, and the child cannot be what records where they went.
    pub fn start_run(&self, clock: &dyn Clock, run: &RunStart) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO run (run_id, issue_id, started_at, session_id, transcript, model, effort)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.run_id,
                run.issue_id,
                clock.wall().0,
                run.session_id,
                run.transcript.map(|p| p.display().to_string()),
                run.model.model,
                run.model.effort.map(|e| e.as_str()),
            ],
        )?;
        Ok(())
    }

    /// One run by id — the "show me what run X did" lookup.
    pub fn run(&self, run_id: &str) -> rusqlite::Result<Option<RunRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT run_id, issue_id, started_at, ended_at, outcome, session_id, turns, in_tok,
                    out_tok, transcript, model, effort
             FROM run WHERE run_id = ?1",
            params![run_id],
            run_record,
        )
        .optional()
    }

    /// An issue's runs, newest first.
    pub fn runs_for(&self, issue_id: &str) -> rusqlite::Result<Vec<RunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, issue_id, started_at, ended_at, outcome, session_id, turns, in_tok,
                    out_tok, transcript, model, effort
             FROM run WHERE issue_id = ?1 ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map(params![issue_id], run_record)?;
        rows.collect()
    }

    /// The most recent transcript path per issue, for the dashboard.
    ///
    /// Ordered oldest-first so the newest run for each issue is the last write into the map —
    /// one scan rather than a correlated subquery, which at this scale is the cheaper of the
    /// two to read.
    pub fn latest_transcripts(
        &self,
    ) -> rusqlite::Result<std::collections::HashMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT issue_id, transcript FROM run
             WHERE transcript IS NOT NULL ORDER BY started_at ASC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect()
    }

    /// Checkpoint an in-flight run's turn count.
    ///
    /// Until this existed `turns` was written once, by `finish_run`, and the live count existed
    /// only in the `RunHandle` inside the scheduler's `running` map — which cannot cross a
    /// process boundary, so a hard kill lost how far every in-flight run had got (issue #25).
    /// The scheduler calls this once per tick per run, and only when the count has moved, so
    /// the write rate is bounded by the poll interval rather than by the agent's event rate.
    /// Guarded on `ended_at IS NULL`: a checkpoint that races a finish must not overwrite the
    /// final figure with an older one.
    pub fn record_progress(&self, run_id: &str, turns: u32) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE run SET turns = ?2 WHERE run_id = ?1 AND ended_at IS NULL",
            params![run_id, turns as i64],
        )?;
        Ok(())
    }

    /// `tokens` is `None` for a run that ended without reporting a total — killed, crashed, or
    /// cut off by the turn budget. It is stored as NULL, never as zero: a zero would read as a
    /// free run in every sum, and a run that was killed mid-stream is not free, it is uncounted.
    pub fn finish_run(
        &self,
        clock: &dyn Clock,
        run_id: &str,
        outcome: &str,
        turns: u32,
        tokens: Option<TokenUsage>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE run SET ended_at = ?2, outcome = ?3, turns = ?4, in_tok = ?5, out_tok = ?6
             WHERE run_id = ?1",
            params![
                run_id,
                clock.wall().0,
                outcome,
                turns as i64,
                tokens.map(|t| t.input as i64),
                tokens.map(|t| t.output as i64),
            ],
        )?;
        Ok(())
    }

    /// The latest runs of every issue, newest first, at most `per_issue` each.
    ///
    /// Bounded per issue rather than by a global `LIMIT`: one issue that has been retried
    /// fifty times would otherwise crowd every other issue's history out of a view that is
    /// published on every tick.
    pub fn recent_runs(&self, per_issue: usize) -> rusqlite::Result<Vec<RunRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, issue_id, started_at, ended_at, outcome, session_id,
                    turns, in_tok, out_tok, transcript, model, effort
             FROM (SELECT *, ROW_NUMBER() OVER (
                       PARTITION BY issue_id ORDER BY started_at DESC, run_id DESC) AS rn
                   FROM run)
             WHERE rn <= ?1
             ORDER BY issue_id, started_at DESC, run_id DESC",
        )?;
        let rows = stmt.query_map(params![per_issue as i64], |r| {
            Ok(RunRecord {
                run_id: r.get(0)?,
                issue_id: r.get(1)?,
                started_at: r.get(2)?,
                ended_at: r.get(3)?,
                outcome: r.get(4)?,
                session_id: r.get(5)?,
                turns: r.get::<_, i64>(6)? as u32,
                in_tok: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                out_tok: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                transcript: r.get(9)?,
                model: r.get(10)?,
                effort: r.get(11)?,
            })
        })?;
        rows.collect()
    }

    /// Sums over the runs that reported a total, and counts the finished ones that did not. The
    /// count travels with the sum because the sum is only meaningful alongside it: "12k tokens
    /// across 3 runs, 2 more uncounted" is a cost figure; "12k tokens" alone is a lower bound
    /// dressed up as a total.
    pub fn token_totals(&self) -> rusqlite::Result<TokenTotals> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(in_tok),0), COALESCE(SUM(out_tok),0),
                    COUNT(*) FILTER (WHERE ended_at IS NOT NULL AND in_tok IS NULL)
             FROM run",
            [],
            |r| {
                Ok(TokenTotals {
                    counted: TokenUsage {
                        input: r.get::<_, i64>(0)? as u64,
                        output: r.get::<_, i64>(1)? as u64,
                    },
                    uncounted_runs: r.get::<_, i64>(2)? as u64,
                })
            },
        )
    }
}

/// What the store can say about cost across every run it has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTotals {
    /// Summed over the runs that reported a total.
    pub counted: TokenUsage,
    /// Finished runs that reported none. Each is real spend the sum does not include.
    pub uncounted_runs: u64,
}

fn run_record(r: &rusqlite::Row) -> rusqlite::Result<RunRecord> {
    Ok(RunRecord {
        run_id: r.get(0)?,
        issue_id: r.get(1)?,
        started_at: r.get(2)?,
        ended_at: r.get(3)?,
        outcome: r.get(4)?,
        session_id: r.get(5)?,
        turns: r.get::<_, i64>(6)? as u32,
        in_tok: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        out_tok: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        transcript: r.get(9)?,
        model: r.get(10)?,
        effort: r.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    fn setup() -> (Store, FakeClock) {
        let s = Store::open_in_memory().unwrap();
        let c = FakeClock::new();
        s.ensure(&c, "id-1", "MT-1", "MT-1-abc").unwrap();
        (s, c)
    }

    fn start(
        s: &Store,
        c: &FakeClock,
        run_id: &str,
        issue_id: &str,
        session_id: &str,
        transcript: Option<&Path>,
    ) {
        let model = ModelChoice::default();
        s.start_run(c, &RunStart { run_id, issue_id, session_id, transcript, model: &model })
            .unwrap();
    }

    #[test]
    fn claim_is_exclusive() {
        let (s, c) = setup();
        assert!(s.claim(&c, "id-1").unwrap());
        assert!(!s.claim(&c, "id-1").unwrap(), "second claim must fail");
        s.release(&c, "id-1").unwrap();
        assert!(s.claim(&c, "id-1").unwrap());
    }

    /// #37: unlike `release`, this must not zero `attempt`/`consecutive_fail` — an account-wide
    /// rate limit is not this issue's failure, so the issue must resume at the attempt it was
    /// already on rather than looking like a fresh start.
    #[test]
    fn releasing_for_a_rate_limit_leaves_the_attempt_and_failure_streak_untouched() {
        let (s, c) = setup();
        s.record_failure(&c, "id-1", ErrorClass::AgentCrash, "boom", 5).unwrap();
        assert!(s.claim(&c, "id-1").unwrap());

        s.release_for_rate_limit(&c, "id-1").unwrap();

        let st = s.get("id-1").unwrap().unwrap();
        assert_eq!(st.phase, Phase::Released, "the claim is released");
        assert_eq!(st.attempt, 1, "the attempt this issue was already on survives");
        assert_eq!(st.consecutive_fail, 1);
        assert_eq!(st.last_fail_class, Some(ErrorClass::AgentCrash));
        assert!(s.claim(&c, "id-1").unwrap(), "and the issue is dispatchable again");
    }

    #[test]
    fn permanent_failure_quarantines_on_first_occurrence() {
        let (s, c) = setup();
        let q = s.record_failure(&c, "id-1", ErrorClass::TemplateRender, "bad var", 3).unwrap();
        assert!(q);
        assert!(s.get("id-1").unwrap().unwrap().is_quarantined());
    }

    #[test]
    fn retryable_failures_quarantine_only_after_the_configured_streak() {
        let (s, c) = setup();
        for i in 1..=2 {
            let q = s.record_failure(&c, "id-1", ErrorClass::Stall, "quiet", 3).unwrap();
            assert!(!q, "quarantined early at {i}");
        }
        assert!(s.record_failure(&c, "id-1", ErrorClass::Stall, "quiet", 3).unwrap());
    }

    #[test]
    fn a_different_failure_class_resets_the_streak() {
        let (s, c) = setup();
        s.record_failure(&c, "id-1", ErrorClass::Stall, "a", 3).unwrap();
        s.record_failure(&c, "id-1", ErrorClass::Stall, "b", 3).unwrap();
        // Different class: streak restarts at 1 rather than tipping into quarantine.
        let q = s.record_failure(&c, "id-1", ErrorClass::TurnTimeout, "c", 3).unwrap();
        assert!(!q);
        let st = s.get("id-1").unwrap().unwrap();
        assert_eq!(st.consecutive_fail, 1);
        assert_eq!(st.attempt, 3, "attempt counts every failure regardless of class");
    }

    #[test]
    fn quarantined_issues_cannot_be_claimed_until_cleared() {
        let (s, c) = setup();
        s.record_failure(&c, "id-1", ErrorClass::AuthFailed, "401", 3).unwrap();
        assert!(!s.claim(&c, "id-1").unwrap());
        assert!(s.unquarantine(&c, "id-1").unwrap(), "clearing a real quarantine reports it");
        assert!(s.claim(&c, "id-1").unwrap());
    }

    #[test]
    fn clearing_a_quarantine_that_is_not_there_does_not_release_a_live_claim() {
        // The operator action reachable from the dashboard and the HTTP API. Pointed at an
        // issue that is merely running, an unguarded version would reset its phase to
        // `released` — and the next tick would dispatch a second agent onto the same worktree.
        let (s, c) = setup();
        assert!(s.claim(&c, "id-1").unwrap());

        assert!(!s.unquarantine(&c, "id-1").unwrap(), "nothing was quarantined; nothing to clear");

        assert_eq!(s.get("id-1").unwrap().unwrap().phase, Phase::Running, "the claim must stand");
        assert!(!s.claim(&c, "id-1").unwrap(), "and must still be exclusive");
    }

    #[test]
    fn an_unblock_does_not_lift_a_park_a_live_delivery_still_owns() {
        // A `Done` parks and queues delivery. Lifting that park would put an agent on the
        // branch in the same tick delivery pushes it or hands it back.
        let (s, c) = setup();
        s.release(&c, "id-1").unwrap();
        s.park(&c, "id-1", "in progress").unwrap();
        for stage in [DeliveryStage::Pending, DeliveryStage::Awaiting, DeliveryStage::Ready] {
            s.begin_delivery(&c, "id-1", None).unwrap();
            s.set_delivery_stage(&c, "id-1", stage, None).unwrap();
            assert!(!s.unblock(&c, "id-1").unwrap(), "{stage:?} delivery owns the park");
            assert!(s.get("id-1").unwrap().unwrap().parked_state.is_some());
        }

        // A handoff gave the branch to the operator; an agent dispatched onto it would restart
        // delivery with its `Done` and step around the bound that handed it off.
        s.set_delivery_stage(&c, "id-1", DeliveryStage::HandedOff, Some("round bound")).unwrap();
        assert!(!s.unblock(&c, "id-1").unwrap(), "a handoff owns the park");

        // One that pushes nothing and hands nothing back does not stand in the way.
        s.set_delivery_stage(&c, "id-1", DeliveryStage::Redispatched, None).unwrap();
        assert!(s.unblock(&c, "id-1").unwrap());
        assert_eq!(s.get("id-1").unwrap().unwrap().parked_state.as_deref(), Some(UNBLOCKED));
        assert!(!s.unblock(&c, "id-1").unwrap(), "a second unblock has nothing left to lift");
    }

    #[test]
    fn run_history_is_newest_first_and_bounded_per_issue() {
        let (s, c) = setup();
        s.ensure(&c, "id-2", "MT-2", "MT-2-def").unwrap();
        for n in 1..=4 {
            c.advance_ms(1_000);
            start(&s, &c, &format!("run-1-{n}"), "id-1", "sess-1", None);
            let tok = TokenUsage { input: 10 * n as u64, output: n as u64 };
            s.finish_run(&c, &format!("run-1-{n}"), "done", n, Some(tok)).unwrap();
        }
        start(&s, &c, "run-2-1", "id-2", "sess-2", None);

        let runs = s.recent_runs(2).unwrap();
        let for_1: Vec<_> = runs.iter().filter(|r| r.issue_id == "id-1").collect();
        assert_eq!(
            for_1.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
            vec!["run-1-4", "run-1-3"],
            "the two newest, newest first"
        );
        assert_eq!(for_1[0].turns, 4);

        // A busy issue must not crowd out a quiet one's history.
        let for_2: Vec<_> = runs.iter().filter(|r| r.issue_id == "id-2").collect();
        assert_eq!(for_2.len(), 1);
        assert!(for_2[0].ended_at.is_none(), "a run still in flight has no end");
        assert_eq!(for_2[0].outcome, None);
    }

    /// Schema v3 made the token columns nullable; `recent_runs` reads them on every tick, via
    /// `snapshot`. SQLite hands a NULL to a non-null integer read as an error, not a zero — so
    /// one budget-cut run would have turned every later tick into a failed one.
    #[test]
    fn a_run_that_reported_no_total_still_appears_in_history() {
        let (s, c) = setup();
        start(&s, &c, "run-a", "id-1", "sess-1", None);
        s.finish_run(&c, "run-a", "killed", 2, None).unwrap();

        let runs = s.recent_runs(5).unwrap();
        let run = runs.iter().find(|r| r.run_id == "run-a").expect("it is still history");
        assert_eq!(run.in_tok, None, "unknown, which is not the same as zero");
        assert_eq!(run.out_tok, None);
    }

    #[test]
    fn retries_become_due_only_once_the_clock_reaches_them() {
        let (s, c) = setup();
        let due = Wall(c.wall().0 + 10_000);
        s.schedule_retry(&c, "id-1", due, 1, "backoff", None).unwrap();
        assert!(s.due_retries(c.wall()).unwrap().is_empty());
        c.advance_ms(10_000);
        assert_eq!(s.due_retries(c.wall()).unwrap().len(), 1);
    }

    #[test]
    fn only_claimed_issues_are_reported_as_claimed() {
        let (s, c) = setup();
        s.ensure(&c, "id-2", "MT-2", "MT-2-def").unwrap();
        assert!(s.claimed().unwrap().is_empty(), "nothing is claimed before anything claims it");

        s.claim(&c, "id-1").unwrap();
        let held: Vec<String> = s.claimed().unwrap().into_iter().map(|x| x.issue_id).collect();
        assert_eq!(held, vec!["id-1"], "a claim is visible; an untouched issue is not");

        s.release(&c, "id-1").unwrap();
        assert!(s.claimed().unwrap().is_empty(), "and it stops being visible once released");
    }

    #[test]
    fn a_retrying_or_quarantined_issue_is_not_mistaken_for_a_claimed_one() {
        // Recovery releases every claim it finds, so anything it can see that is merely *queued*
        // behind a timer would have its retry row deleted out from under it.
        let (s, c) = setup();
        s.ensure(&c, "id-2", "MT-2", "MT-2-def").unwrap();

        s.schedule_retry(&c, "id-1", Wall(c.wall().0 + 10_000), 1, "backoff", None).unwrap();
        s.record_failure(&c, "id-2", ErrorClass::AuthFailed, "401", 3).unwrap();

        assert!(s.claimed().unwrap().is_empty());
    }

    #[test]
    fn closing_open_runs_touches_only_the_ones_still_in_flight() {
        let (s, c) = setup();
        start(&s, &c, "run-a", "id-1", "sess-a", None);
        s.finish_run(&c, "run-a", "done", 3, Some(TokenUsage { input: 10, output: 20 })).unwrap();
        start(&s, &c, "run-b", "id-1", "sess-b", None);

        // Only the run the kill interrupted; the one that reported for itself keeps its verdict.
        assert_eq!(s.close_open_runs(&c, "id-1", "orphaned").unwrap(), 1);
        assert_eq!(s.close_open_runs(&c, "id-1", "orphaned").unwrap(), 0, "and it is idempotent");
        assert_eq!(
            s.token_totals().unwrap(),
            TokenTotals { counted: TokenUsage { input: 10, output: 20 }, uncounted_runs: 1 },
            "a run nobody counted contributes nothing to the sum and one to the uncounted tally"
        );
    }

    #[test]
    fn an_in_flight_runs_checkpointed_turns_survive_into_the_orphaned_row_and_the_issue_budget() {
        let (s, c) = setup();
        s.ensure(&c, "id-1", "MT-1", "MT-1-abc").unwrap();
        start(&s, &c, "run-a", "id-1", "sess-a", None);
        s.record_progress("run-a", 2).unwrap();
        s.record_progress("run-a", 7).unwrap();
        assert_eq!(s.run("run-a").unwrap().unwrap().turns, 7, "the latest checkpoint wins");
        assert_eq!(
            s.get("id-1").unwrap().unwrap().cumulative_turns,
            0,
            "a checkpoint is not yet charged to the issue; the run may still finish and report"
        );

        // The kill, then the next startup's recovery.
        assert_eq!(s.close_open_runs(&c, "id-1", "orphaned").unwrap(), 1);
        let rec = s.run("run-a").unwrap().unwrap();
        assert_eq!(rec.outcome.as_deref(), Some("orphaned"));
        assert_eq!(rec.turns, 7, "closing the row must keep the last known count, not zero it");
        assert_eq!(rec.in_tok, None, "and must not invent a total the CLI never reported");
        assert_eq!(
            s.get("id-1").unwrap().unwrap().cumulative_turns,
            7,
            "the interrupted attempt is charged to the issue exactly once"
        );
        s.close_open_runs(&c, "id-1", "orphaned").unwrap();
        assert_eq!(s.get("id-1").unwrap().unwrap().cumulative_turns, 7, "and not again");
    }

    #[test]
    fn a_late_checkpoint_cannot_overwrite_a_finished_runs_final_count() {
        let (s, c) = setup();
        start(&s, &c, "run-a", "id-1", "sess-a", None);
        s.finish_run(&c, "run-a", "done", 5, None).unwrap();
        s.record_progress("run-a", 3).unwrap();
        assert_eq!(s.run("run-a").unwrap().unwrap().turns, 5);
    }

    #[test]
    fn a_run_that_ended_without_a_total_is_uncounted_rather_than_free() {
        // Three runs end: one reports, one is killed before reporting, one is still open. The
        // sum must cover exactly the first, and the uncounted tally exactly the second — an
        // in-flight run is not yet anything.
        let (s, c) = setup();
        start(&s, &c, "run-a", "id-1", "sess-a", None);
        s.finish_run(&c, "run-a", "done", 4, Some(TokenUsage { input: 300, output: 40 })).unwrap();
        start(&s, &c, "run-b", "id-1", "sess-b", None);
        s.finish_run(&c, "run-b", "killed", 2, None).unwrap();
        start(&s, &c, "run-c", "id-1", "sess-c", None);

        assert_eq!(
            s.token_totals().unwrap(),
            TokenTotals { counted: TokenUsage { input: 300, output: 40 }, uncounted_runs: 1 }
        );
    }

    #[test]
    fn migrating_a_v2_database_drops_the_totals_it_recorded_and_keeps_everything_else() {
        // A store from before v3 holds per-event sums that are wrong by the turn count. The
        // migration must not carry them forward as if they were totals — and must not lose the
        // run rows themselves, which are what `close_open_runs` and the turn budget read.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for sql in &schema::MIGRATIONS[..2] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute_batch(
            "INSERT INTO issue_state (issue_id, identifier, worktree_key, updated_at)
               VALUES ('id-1', 'MT-1', 'MT-1-abc', 0);
             INSERT INTO run (run_id, issue_id, started_at, ended_at, outcome, turns, in_tok, out_tok)
               VALUES ('run-old', 'id-1', 1, 2, 'done', 83, 10231371, 441);
             INSERT INTO run (run_id, issue_id, started_at, turns)
               VALUES ('run-open', 'id-1', 3, 1);",
        )
        .unwrap();

        schema::migrate(&conn).unwrap();
        let s = Store::from_connection(conn);

        assert_eq!(
            s.token_totals().unwrap(),
            TokenTotals { counted: TokenUsage::default(), uncounted_runs: 1 },
            "the inflated figures are gone; the finished run now reads as uncounted"
        );
        let c = FakeClock::new();
        assert_eq!(s.close_open_runs(&c, "id-1", "orphaned").unwrap(), 1, "run rows survived");
    }

    #[test]
    fn a_runs_transcript_path_is_reachable_from_its_run_record() {
        let (s, c) = setup();
        s.ensure(&c, "id-1", "MT-1", "MT-1-abc").unwrap();
        let log = Path::new("/tmp/transcripts/id-1-1700-abc.jsonl");

        start(&s, &c, "run-a", "id-1", "sess-a", Some(log));
        s.finish_run(&c, "run-a", "done", 3, Some(TokenUsage { input: 10, output: 20 })).unwrap();
        c.advance_ms(1_000);
        // A run dispatched with transcripts off still gets a row, with nothing to point at.
        start(&s, &c, "run-b", "id-1", "sess-b", None);

        let rec = s.run("run-a").unwrap().expect("the run was recorded");
        assert_eq!(rec.transcript.as_deref(), Some("/tmp/transcripts/id-1-1700-abc.jsonl"));
        assert_eq!(rec.outcome.as_deref(), Some("done"));
        assert_eq!(rec.turns, 3);
        assert_eq!(s.run("run-b").unwrap().unwrap().transcript, None);
        assert_eq!(s.run("no-such-run").unwrap(), None);

        let runs = s.runs_for("id-1").unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].run_id, "run-b", "newest first");

        // The dashboard wants the latest per issue, not every run ever.
        assert_eq!(
            s.latest_transcripts().unwrap().get("id-1").map(String::as_str),
            Some(log.to_str().unwrap())
        );
    }

    #[test]
    fn state_survives_reopening_the_same_database() {
        let dir = std::env::temp_dir().join(format!("crew-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.db");
        let _ = std::fs::remove_file(&path);

        let c = FakeClock::new();
        {
            let s = Store::open(&path).unwrap();
            s.ensure(&c, "id-1", "MT-1", "MT-1-abc").unwrap();
            s.record_failure(&c, "id-1", ErrorClass::Stall, "quiet", 3).unwrap();
        }
        {
            let s = Store::open(&path).unwrap();
            let st = s.get("id-1").unwrap().unwrap();
            assert_eq!(st.attempt, 1, "attempt counter must survive restart");
            assert_eq!(st.last_fail_class, Some(ErrorClass::Stall));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---- delivery ------------------------------------------------------------------

/// Where an issue's branch is on its way to a mergeable pull request.
///
/// The serde spelling is the store's `stage` column and what the snapshot publishes, one word
/// per stage, for the same reason [`Phase`] is spelled once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStage {
    /// A run reported done; the push and the pull request have not happened yet, or the last
    /// attempt at them failed transiently. Retried each poll.
    #[serde(rename = "pending")]
    Pending,
    /// The pull request is open and the orchestrator is waiting on CI or on review.
    #[serde(rename = "awaiting")]
    Awaiting,
    /// CI or review sent the issue back to an agent. Nothing is polled until that run ends.
    #[serde(rename = "redispatched")]
    Redispatched,
    /// Green CI, review attached, no comment outstanding. The operator's to merge.
    #[serde(rename = "ready")]
    Ready,
    /// The orchestrator stopped working it and says why in `handoff_reason`: a round bound
    /// reached, a review request that attached nobody, a permanent forge error.
    #[serde(rename = "handed_off")]
    HandedOff,
    /// Merged or closed outside the orchestrator. Nothing more to do.
    #[serde(rename = "closed")]
    Closed,
}

impl DeliveryStage {
    pub fn label(self) -> &'static str {
        match self {
            DeliveryStage::Pending => "pending",
            DeliveryStage::Awaiting => "awaiting",
            DeliveryStage::Redispatched => "redispatched",
            DeliveryStage::Ready => "ready",
            DeliveryStage::HandedOff => "handed_off",
            DeliveryStage::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => DeliveryStage::Pending,
            "awaiting" => DeliveryStage::Awaiting,
            "redispatched" => DeliveryStage::Redispatched,
            "ready" => DeliveryStage::Ready,
            "handed_off" => DeliveryStage::HandedOff,
            "closed" => DeliveryStage::Closed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryRecord {
    pub issue_id: String,
    pub stage: DeliveryStage,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub base: Option<String>,
    pub head_sha: Option<String>,
    pub head_pushed_at: Option<i64>,
    pub review_requested: bool,
    pub review_error: Option<String>,
    /// Fix rounds against the current pull request. Reset when a new one is opened.
    pub rounds_pr: u32,
    /// Fix rounds over the issue's whole life. Never reset by the orchestrator.
    pub rounds_issue: u32,
    /// Serialised [`crate::model::Feedback`] for the next run, until that run is launched.
    pub pending_feedback: Option<String>,
    /// Serialised `Vec<ReviewVerdict>` the last run reported, until they are applied to the
    /// pull request.
    pub pending_verdicts: Option<String>,
    /// Serialised `Vec<String>` of the comment ids the most recent fix round was handed.
    pub handed_comments: Option<String>,
    pub handoff_reason: Option<String>,
    pub updated_at: i64,
    /// The head CI is pending on and when that was first observed, on the injected clock
    /// rather than any provider timestamp; `None` while CI is not pending. See
    /// [`Store::set_ci_pending`].
    pub ci_pending: Option<(String, i64)>,
}

impl Store {
    pub fn delivery(&self, issue_id: &str) -> rusqlite::Result<Option<DeliveryRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!("{DELIVERY_SELECT} WHERE issue_id = ?1"),
            params![issue_id],
            delivery_record,
        )
        .optional()
    }

    pub fn deliveries(&self) -> rusqlite::Result<Vec<DeliveryRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{DELIVERY_SELECT} ORDER BY issue_id"))?;
        let rows = stmt.query_map([], delivery_record)?;
        rows.collect()
    }

    /// A run reported done: queue its branch for delivery, carrying whatever verdicts it gave.
    ///
    /// An upsert that leaves the pull request fields and the round counters alone, because the
    /// second and later deliveries of an issue — after a CI or review round — are pushes to a
    /// pull request that already exists, and the counters are exactly what must survive them.
    pub fn begin_delivery(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        verdicts_json: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO delivery (issue_id, stage, pending_verdicts, updated_at)
             VALUES (?1, 'pending', ?2, ?3)
             ON CONFLICT(issue_id) DO UPDATE SET
               stage = 'pending', pending_verdicts = ?2, pending_feedback = NULL,
               handoff_reason = NULL, updated_at = ?3",
            params![issue_id, verdicts_json, clock.wall().0],
        )?;
        Ok(())
    }

    /// The push landed and the pull request is known. A *different* pull request number than
    /// before resets the per-PR round count; the per-issue count is untouched either way. A
    /// different *head* resets `review_requested`, whatever the number: a reviewer verified
    /// against the old head has not seen the new one, and a pull request reaching `Ready` with
    /// its current head unreviewed is exactly what the verification after each request exists
    /// to rule out. A re-push of the same head — the idempotent case — leaves both alone.
    ///
    /// `pending_verdicts` is deliberately left alone: the verdicts a run reported are applied
    /// after this, one reply at a time, and each leaves the queue only once its reply has
    /// landed — see [`Store::set_pending_verdicts`]. Clearing them here would drop the verdicts
    /// of a run whose replies then failed, and the threads would come back round as new.
    pub fn set_delivery_pr(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        pr_number: u64,
        pr_url: &str,
        base: &str,
        head_sha: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = clock.wall().0;
        conn.execute(
            "UPDATE delivery SET
               rounds_pr = CASE WHEN pr_number IS ?2 THEN rounds_pr ELSE 0 END,
               review_requested = CASE WHEN pr_number IS ?2 AND head_sha IS ?5
                                       THEN review_requested ELSE 0 END,
               review_error = CASE WHEN pr_number IS ?2 AND head_sha IS ?5
                                   THEN review_error ELSE NULL END,
               pr_number = ?2, pr_url = ?3, base = ?4, head_sha = ?5, head_pushed_at = ?6,
               stage = 'awaiting', updated_at = ?6
             WHERE issue_id = ?1",
            params![issue_id, pr_number as i64, pr_url, base, head_sha, now],
        )?;
        Ok(())
    }

    /// Start, keep or stop the CI wait. `Some(head)` starts the clock at now unless it is
    /// already running for that same head; `None` stops it. Keyed on the head the provider
    /// reports rather than the one crewd pushed, so a head the operator pushed gets its own
    /// full wait, and stopped whenever CI is not pending, so a check re-run hours after a
    /// green result is timed from the re-run and not from the push (#105).
    pub fn set_ci_pending(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        head: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE delivery SET
               ci_pending_since = CASE WHEN ?2 IS NULL THEN NULL
                                       WHEN ci_pending_head IS ?2 THEN ci_pending_since
                                       ELSE ?3 END,
               ci_pending_head = ?2
             WHERE issue_id = ?1",
            params![issue_id, head, clock.wall().0],
        )?;
        Ok(())
    }

    /// What is still waiting to be applied to the pull request: the verdicts whose replies have
    /// not landed yet, or `None` once every one has. The queue is written back after each
    /// pass rather than cleared at the start of it, so a reply that failed is retried on the
    /// next poll instead of being forgotten.
    pub fn set_pending_verdicts(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        verdicts_json: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE delivery SET pending_verdicts = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, verdicts_json, clock.wall().0],
        )?;
        Ok(())
    }

    pub fn set_delivery_stage(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        stage: DeliveryStage,
        handoff_reason: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE delivery SET stage = ?2, handoff_reason = ?3, updated_at = ?4
             WHERE issue_id = ?1",
            params![issue_id, stage.label(), handoff_reason, clock.wall().0],
        )?;
        Ok(())
    }

    /// `error` is the verification's finding when the provider accepted the request and
    /// attached nobody; `None` records a request that verifiably took.
    pub fn set_review_requested(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        error: Option<&str>,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE delivery SET review_requested = 1, review_error = ?2, updated_at = ?3
             WHERE issue_id = ?1",
            params![issue_id, error, clock.wall().0],
        )?;
        Ok(())
    }

    /// Hand the issue back to an agent with `feedback_json`, charging one round against both
    /// bounds. `None` for a round the gate answers rather than delivery (#159), whose brief
    /// travels on the retry reason and must not be outranked by a delivery row. Returns the counts *after* charging, so the caller compares them to the limits
    /// it holds — the store does not know the limits, and should not: they are config.
    pub fn open_delivery_round(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        feedback_json: Option<&str>,
        handed_comments_json: Option<&str>,
    ) -> rusqlite::Result<(u32, u32)> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE delivery SET rounds_pr = rounds_pr + 1, rounds_issue = rounds_issue + 1,
               pending_feedback = ?2, handed_comments = ?3, stage = 'redispatched',
               updated_at = ?4
             WHERE issue_id = ?1",
            params![issue_id, feedback_json, handed_comments_json, clock.wall().0],
        )?;
        conn.query_row(
            "SELECT rounds_pr, rounds_issue FROM delivery WHERE issue_id = ?1",
            params![issue_id],
            |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32)),
        )
    }

    /// Read the feedback queued for this issue's next run and clear it, in one step, so a run
    /// that is launched is the one and only run told about it.
    pub fn take_delivery_feedback(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
    ) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let fb: Option<String> = conn
            .query_row(
                "SELECT pending_feedback FROM delivery WHERE issue_id = ?1",
                params![issue_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if fb.is_some() {
            conn.execute(
                "UPDATE delivery SET pending_feedback = NULL, updated_at = ?2 WHERE issue_id = ?1",
                params![issue_id, clock.wall().0],
            )?;
        }
        Ok(fb)
    }

    /// Surface a problem on the issue's row without touching its failure streak or phase: a
    /// delivery that stopped is something an operator must see, and not something the retry
    /// machinery should act on — the work is done; what failed is the handoff.
    pub fn note_error(&self, clock: &dyn Clock, issue_id: &str, msg: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET last_error = ?2, updated_at = ?3 WHERE issue_id = ?1",
            params![issue_id, msg, clock.wall().0],
        )?;
        Ok(())
    }

    /// Clear what `note_error` or `set_note` left on the issue's row, once what it reported is
    /// over — a delivery whose pull request the operator merged or closed.
    pub fn clear_note(&self, clock: &dyn Clock, issue_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE issue_state SET last_error = NULL, last_error_class = NULL, updated_at = ?2
             WHERE issue_id = ?1",
            params![issue_id, clock.wall().0],
        )?;
        Ok(())
    }

    /// Settle one review comment. Insert-or-ignore: the first verdict stands, and a later run
    /// re-arguing a settled thread changes nothing here — that is the point of recording it.
    pub fn record_verdict(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        pr_number: u64,
        comment_id: &str,
        verdict: Verdict,
        detail: &str,
    ) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT OR IGNORE INTO review_verdict
               (issue_id, comment_id, pr_number, verdict, detail, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                issue_id,
                comment_id,
                pr_number as i64,
                verdict.as_str(),
                detail,
                clock.wall().0
            ],
        )?;
        Ok(n == 1)
    }

    /// Every settled comment for an issue: id → (verdict, detail).
    pub fn verdicts_for(
        &self,
        issue_id: &str,
    ) -> rusqlite::Result<std::collections::HashMap<String, (Verdict, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT comment_id, verdict, detail FROM review_verdict WHERE issue_id = ?1",
        )?;
        let rows = stmt.query_map(params![issue_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    Verdict::parse(&r.get::<_, String>(1)?).unwrap_or(Verdict::Rejected),
                    r.get::<_, String>(2)?,
                ),
            ))
        })?;
        rows.collect()
    }

    /// The comments on pull request `pr_number` whose verdict is recorded and whose thread has
    /// not yet been resolved on the provider, oldest verdict first.
    pub fn unresolved_verdicts(
        &self,
        issue_id: &str,
        pr_number: u64,
    ) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT comment_id FROM review_verdict
             WHERE issue_id = ?1 AND pr_number = ?2 AND resolved_at IS NULL
             ORDER BY recorded_at, comment_id",
        )?;
        let rows = stmt.query_map(params![issue_id, pr_number as i64], |r| r.get(0))?;
        rows.collect()
    }

    /// Record that the thread a verdict answers is resolved. Touches nothing about the verdict
    /// itself: resolution follows a settled comment and never re-opens or re-decides one.
    pub fn mark_thread_resolved(
        &self,
        clock: &dyn Clock,
        issue_id: &str,
        comment_id: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE review_verdict SET resolved_at = ?3
             WHERE issue_id = ?1 AND comment_id = ?2 AND resolved_at IS NULL",
            params![issue_id, comment_id, clock.wall().0],
        )?;
        Ok(())
    }
}

const DELIVERY_SELECT: &str = "SELECT issue_id, stage, pr_number, pr_url, base, head_sha,
    head_pushed_at, review_requested, review_error, rounds_pr, rounds_issue, pending_feedback,
    pending_verdicts, handed_comments, handoff_reason, updated_at, ci_pending_head,
    ci_pending_since FROM delivery";

fn delivery_record(r: &rusqlite::Row) -> rusqlite::Result<DeliveryRecord> {
    Ok(DeliveryRecord {
        issue_id: r.get(0)?,
        stage: DeliveryStage::parse(&r.get::<_, String>(1)?).unwrap_or(DeliveryStage::Pending),
        pr_number: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
        pr_url: r.get(3)?,
        base: r.get(4)?,
        head_sha: r.get(5)?,
        head_pushed_at: r.get(6)?,
        review_requested: r.get::<_, i64>(7)? != 0,
        review_error: r.get(8)?,
        rounds_pr: r.get::<_, i64>(9)? as u32,
        rounds_issue: r.get::<_, i64>(10)? as u32,
        pending_feedback: r.get(11)?,
        pending_verdicts: r.get(12)?,
        handed_comments: r.get(13)?,
        handoff_reason: r.get(14)?,
        updated_at: r.get(15)?,
        ci_pending: match (r.get::<_, Option<String>>(16)?, r.get::<_, Option<i64>>(17)?) {
            (Some(head), Some(since)) => Some((head, since)),
            _ => None,
        },
    })
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use crate::clock::FakeClock;

    fn store_with(issue: &str) -> (Store, FakeClock) {
        let s = Store::open_in_memory().unwrap();
        let c = FakeClock::new();
        s.ensure(&c, issue, "MT-1", "MT-1-abc").unwrap();
        (s, c)
    }

    #[test]
    fn a_new_pull_request_resets_the_per_pr_round_count_but_never_the_per_issue_one() {
        let (s, c) = store_with("iss-1");
        s.begin_delivery(&c, "iss-1", None).unwrap();
        s.set_delivery_pr(&c, "iss-1", 7, "u", "master", "aaa").unwrap();
        assert_eq!(s.open_delivery_round(&c, "iss-1", Some("{}"), None).unwrap(), (1, 1));
        assert_eq!(s.open_delivery_round(&c, "iss-1", Some("{}"), None).unwrap(), (2, 2));

        // The same pull request, pushed again: both counts stand.
        s.begin_delivery(&c, "iss-1", None).unwrap();
        s.set_delivery_pr(&c, "iss-1", 7, "u", "master", "bbb").unwrap();
        let d = s.delivery("iss-1").unwrap().unwrap();
        assert_eq!((d.rounds_pr, d.rounds_issue), (2, 2));

        // A different pull request: only the per-PR count starts over.
        s.begin_delivery(&c, "iss-1", None).unwrap();
        s.set_delivery_pr(&c, "iss-1", 8, "u", "master", "ccc").unwrap();
        let d = s.delivery("iss-1").unwrap().unwrap();
        assert_eq!((d.rounds_pr, d.rounds_issue), (0, 2), "the issue-wide bound must survive");
    }

    /// Finding 4 on #47. A fix round pushes a new head to the same pull request; a reviewer
    /// verified against the old one has not seen it, and a `review_requested` that survived the
    /// push meant nobody was asked again — the pull request could reach `Ready` with its current
    /// head unreviewed, which is the gap the verification after each request was built to close.
    #[test]
    fn a_new_head_on_the_same_pull_request_needs_its_review_requested_again() {
        let (s, c) = store_with("iss-1");
        s.begin_delivery(&c, "iss-1", None).unwrap();
        s.set_delivery_pr(&c, "iss-1", 7, "u", "master", "aaa").unwrap();
        s.set_review_requested(&c, "iss-1", None).unwrap();
        assert!(s.delivery("iss-1").unwrap().unwrap().review_requested);

        // The same head pushed again — idempotent — keeps the request.
        s.set_delivery_pr(&c, "iss-1", 7, "u", "master", "aaa").unwrap();
        assert!(s.delivery("iss-1").unwrap().unwrap().review_requested, "nothing changed");

        // A new head on the same pull request does not.
        s.set_delivery_pr(&c, "iss-1", 7, "u", "master", "bbb").unwrap();
        let d = s.delivery("iss-1").unwrap().unwrap();
        assert!(!d.review_requested, "the reviewer verified against `aaa`, not `bbb`");
        assert!(d.review_error.is_none());
    }

    #[test]
    fn feedback_is_handed_to_exactly_one_launch() {
        let (s, c) = store_with("iss-1");
        s.begin_delivery(&c, "iss-1", None).unwrap();
        s.open_delivery_round(&c, "iss-1", Some("\"ci\""), None).unwrap();
        assert_eq!(s.take_delivery_feedback(&c, "iss-1").unwrap().as_deref(), Some("\"ci\""));
        assert_eq!(s.take_delivery_feedback(&c, "iss-1").unwrap(), None);
    }

    #[test]
    fn the_first_verdict_on_a_comment_stands() {
        let (s, c) = store_with("iss-1");
        assert!(s.record_verdict(&c, "iss-1", 7, "c-1", Verdict::Rejected, "not a bug").unwrap());
        assert!(!s.record_verdict(&c, "iss-1", 7, "c-1", Verdict::Accepted, "abc123").unwrap());
        let v = s.verdicts_for("iss-1").unwrap();
        assert_eq!(v["c-1"], (Verdict::Rejected, "not a bug".to_string()));
    }
}
