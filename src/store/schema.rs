//! Schema migrations, applied in order at open.

use rusqlite::Connection;

/// Append-only. A store records only the number of the last migration it applied, so an entry
/// edited after it shipped is skipped for good by every store that already ran it (#81). Change
/// the schema with a new entry, and pin its hash in `RELEASED` in the tests below.
pub(super) const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE IF NOT EXISTS issue_state (
      issue_id          TEXT PRIMARY KEY,
      identifier        TEXT NOT NULL,
      worktree_key      TEXT NOT NULL,
      phase             TEXT NOT NULL DEFAULT 'released',
      attempt           INTEGER NOT NULL DEFAULT 0,
      consecutive_fail  INTEGER NOT NULL DEFAULT 0,
      last_fail_class   TEXT,
      cumulative_turns  INTEGER NOT NULL DEFAULT 0,
      miss_count        INTEGER NOT NULL DEFAULT 0,
      -- Tracker state at the moment a run reported Done or Blocked. While the issue still sits
      -- in that state there is nothing new to act on, so it is not re-dispatched. Cleared as
      -- soon as the state moves.
      parked_state      TEXT,
      quarantined_at    INTEGER,
      last_error_class  TEXT,
      last_error        TEXT,
      task_ref          TEXT,
      updated_at        INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS run (
      run_id     TEXT PRIMARY KEY,
      issue_id   TEXT NOT NULL REFERENCES issue_state(issue_id),
      started_at INTEGER NOT NULL,
      ended_at   INTEGER,
      outcome    TEXT,
      session_id TEXT,
      turns      INTEGER NOT NULL DEFAULT 0,
      in_tok     INTEGER NOT NULL DEFAULT 0,
      out_tok    INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS run_by_issue ON run(issue_id, started_at DESC);

    CREATE TABLE IF NOT EXISTS retry (
      issue_id TEXT PRIMARY KEY REFERENCES issue_state(issue_id),
      due_at   INTEGER NOT NULL,
      attempt  INTEGER NOT NULL,
      reason   TEXT
    );
    "#,
    // v2
    r#"
    -- The conversation an issue's work is happening in. A continuation resumes it instead of
    -- starting cold. Nullable in both directions that matter: an issue that has never been
    -- dispatched has no session yet, and a run that proves its session unresumable clears the
    -- column rather than retrying into the same dead id.
    ALTER TABLE issue_state ADD COLUMN session_id TEXT;
    "#,
    // v3
    r#"
    -- Token totals become nullable, and every total recorded so far is dropped rather than
    -- carried over. Until this version they were sums of per-event stream usage, which
    -- double-counts input by the turn count and undercounts output by orders of magnitude
    -- (issue #9). Those figures are not a rough version of the truth; they are unrelated to it,
    -- and a NULL is the honest replacement. From here on NULL means "this run ended without the
    -- CLI reporting a total" — killed, crashed, or cut off by the turn budget — and the sums the
    -- dashboard shows cover only the runs that did report. SQLite cannot drop a NOT NULL, so the
    -- table is rebuilt; nothing references `run`, so the rebuild has no cascade to worry about.
    CREATE TABLE run_v3 (
      run_id     TEXT PRIMARY KEY,
      issue_id   TEXT NOT NULL REFERENCES issue_state(issue_id),
      started_at INTEGER NOT NULL,
      ended_at   INTEGER,
      outcome    TEXT,
      session_id TEXT,
      turns      INTEGER NOT NULL DEFAULT 0,
      in_tok     INTEGER,
      out_tok    INTEGER
    );
    INSERT INTO run_v3 (run_id, issue_id, started_at, ended_at, outcome, session_id, turns)
      SELECT run_id, issue_id, started_at, ended_at, outcome, session_id, turns FROM run;
    DROP TABLE run;
    ALTER TABLE run_v3 RENAME TO run;
    CREATE INDEX IF NOT EXISTS run_by_issue ON run(issue_id, started_at DESC);
    "#,
    // v4
    r#"
    -- The branch a dispatched run actually checked out, recorded once at `prepare` time rather
    -- than recomputed from `identifier` on every read. A recomputed name can drift two ways:
    -- `Store::ensure` may rename `identifier` after launch, changing what the recompute would
    -- produce, and `Workspace::remove` deletes the branch whenever git's merged check says it
    -- holds nothing new, so a name that still resolves says nothing about whether the ref still
    -- exists. NULL means what it means everywhere else in this table: nothing to report, either
    -- because the issue has never been dispatched or because cleanup deleted the ref.
    ALTER TABLE issue_state ADD COLUMN branch TEXT;
    "#,
    // v5
    r#"
    -- Where this run's raw event stream was written. Nullable because a transcript is
    -- best-effort: a run dispatched with transcripts off, or whose file could not be opened,
    -- still gets a row. Recording the path rather than deriving it is what makes "show me what
    -- run X did" answerable from the run record alone, without knowing the layout on disk.
    ALTER TABLE run ADD COLUMN transcript TEXT;
    "#,
    // v6
    r#"
    -- Delivery: what happened to an issue's branch after a run reported done. One row per
    -- issue, because one issue has one branch and therefore at most one open pull request at
    -- a time. The row is a cache of judgment like everything else here — losing it costs a
    -- re-push and a re-read of the pull request, both idempotent — with one exception that is
    -- the reason two of these counters exist: `rounds_issue` is the bound on how many times
    -- delivery may hand an issue back to an agent over its whole life, and a bound that reset
    -- with every fresh run (or every fresh pull request, which is what `rounds_pr` tracks)
    -- would bound nothing, for the same reason `max_calls_per_run` alone bounds no broker.
    CREATE TABLE IF NOT EXISTS delivery (
      issue_id          TEXT PRIMARY KEY REFERENCES issue_state(issue_id),
      stage             TEXT NOT NULL,
      pr_number         INTEGER,
      pr_url            TEXT,
      base              TEXT,
      head_sha          TEXT,
      head_pushed_at    INTEGER,
      review_requested  INTEGER NOT NULL DEFAULT 0,
      review_error      TEXT,
      rounds_pr         INTEGER NOT NULL DEFAULT 0,
      rounds_issue      INTEGER NOT NULL DEFAULT 0,
      pending_feedback  TEXT,
      pending_verdicts  TEXT,
      -- Comment ids handed to the most recent fix round, so the next one can name the threads
      -- a run was told about and left unanswered, rather than presenting them as new.
      handed_comments   TEXT,
      handoff_reason    TEXT,
      updated_at        INTEGER NOT NULL
    );

    -- Every review comment that has been settled, and how. Keyed by the provider's comment id
    -- so a later round can be handed only the threads still open, and so nobody re-argues one
    -- already accepted or rejected. `detail` is the commit that resolved it or the reason it
    -- was declined — never empty, because a verdict without its grounds is just a label.
    CREATE TABLE IF NOT EXISTS review_verdict (
      issue_id     TEXT NOT NULL REFERENCES issue_state(issue_id),
      comment_id   TEXT NOT NULL,
      pr_number    INTEGER NOT NULL,
      verdict      TEXT NOT NULL,
      detail       TEXT NOT NULL,
      recorded_at  INTEGER NOT NULL,
      PRIMARY KEY (issue_id, comment_id)
    );
    "#,
    // v7
    r#"
    -- Consecutive handoff-gate failures (#46). Held only in memory, a restart forgave the
    -- streak while nothing about the issue had changed, so `gate.max_failures` bounded failures
    -- per process rather than per line of work — and a suite the agent cannot make pass is the
    -- case both most in need of the bound and most likely to outlive a restart.
    ALTER TABLE issue_state ADD COLUMN gate_failures INTEGER NOT NULL DEFAULT 0;
    "#,
    // v8
    r#"
    -- The model and effort a run was dispatched with (#36), written when the run starts and
    -- never updated: they are history, not a view of the current config, so a run keeps naming
    -- what did its work after `worker.model` changes. NULL means no flag was passed and the run
    -- got whatever the operator's CLI defaulted to — recorded as unknown rather than guessed.
    ALTER TABLE run ADD COLUMN model TEXT;
    ALTER TABLE run ADD COLUMN effort TEXT;
    "#,
    // v9
    r#"
    -- When the thread a verdict answers was resolved on the provider (#89). NULL until then, so
    -- a resolve that failed is retried on the next poll; set once and never cleared, so a
    -- human re-opening the thread is their conversation, not a reason to resolve it again.
    ALTER TABLE review_verdict ADD COLUMN resolved_at INTEGER;
    "#,
    // v10
    r#"
    -- The state whose slot a continuation holds while it waits out its delay (#86). Without it
    -- the slot freed by a `Continue` went to whatever `dispatch_new` found first, so a
    -- continuing issue lost its place in the milestone order at every session boundary. Kept on
    -- the retry row rather than beside it, so a reservation cannot outlive the retry it belongs
    -- to: every path that deletes the row ends the reservation with it. NULL for a failure
    -- backoff, which must not hold capacity hostage.
    ALTER TABLE retry ADD COLUMN reserved_state TEXT;
    "#,
    // v11
    r#"
    -- The head CI was last seen pending on, and when it was first seen so (#105). Timing the
    -- wait from `head_pushed_at` handed off a ready pull request the moment a check re-ran on
    -- it, or the operator pushed a head of their own, because that clock had started hours
    -- earlier on crewd's own push. NULL whenever CI is not pending, so a re-run starts afresh.
    ALTER TABLE delivery ADD COLUMN ci_pending_head TEXT;
    ALTER TABLE delivery ADD COLUMN ci_pending_since INTEGER;
    "#,
    // v12
    r#"
    -- What a resumed session has not seen (#109). `session_body` is the hash of the issue body
    -- the session was started or last resumed with; a continuation whose current body hashes
    -- differently is handed the body again, because the decisions an operator writes into a
    -- description after the first attempt otherwise never reach the conversation. NULL means
    -- unknown, which a resume reads as changed. `pending_feedback` is why the last run ended
    -- when nothing else records it in a form the next prompt can use — a rebase conflict that
    -- parked the issue `Blocked` — taken by the next launch like delivery's column of the same
    -- name, which exists only while delivery is on.
    ALTER TABLE issue_state ADD COLUMN session_body TEXT;
    ALTER TABLE issue_state ADD COLUMN pending_feedback TEXT;
    "#,
    // v13
    r#"
    -- The worker a run was dispatched to (#119). A session belongs to one provider, so the run
    -- that holds it is what pins a continuation: a Claude session id means nothing to another
    -- worker. NULL for a run recorded before workers were named, which pins nothing.
    ALTER TABLE run ADD COLUMN worker TEXT;
    "#,
];

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version > current {
            apply(conn, sql, version)?;
        }
    }
    Ok(())
}

/// One migration and the version bump that records it, in a single transaction.
///
/// Both halves or neither. `execute_batch` runs in autocommit, so a rebuild like v3 — several
/// statements, one of them a `DROP TABLE` — could be interrupted between them and leave a
/// half-built schema still labelled with the *old* version. The next startup would then retry
/// that migration against a database it had already half-changed, fail on the first `CREATE`,
/// and return an error out of `migrate`, so the store would refuse to open at all. That breaks
/// the contract this store is built on: losing it may cost re-polling, but it must never stop
/// the process from starting. `PRAGMA user_version` is a header write and rolls back with
/// everything else, which is what makes the version safe to set inside the same transaction.
fn apply(conn: &Connection, sql: &str, version: i64) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(sql)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_migration_that_fails_partway_leaves_no_trace_and_does_not_advance_the_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let before: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();

        // Shaped like v3: build something, then fail before the end of the batch.
        let half = "CREATE TABLE half_done (x INTEGER); INSERT INTO nonexistent VALUES (1);";
        assert!(apply(&conn, half, before + 1).is_err());

        let after: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(after, before, "a migration that failed must not claim to have applied");

        let leftover: i64 = conn
            .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'half_done'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(leftover, 0, "nor leave the half it did finish behind for the next startup");
    }

    /// A released migration is never edited: a store that already applied it records only the
    /// version number, so anything added to that entry afterwards is skipped for good. v7 is
    /// written out as it shipped (#46) rather than read from `MIGRATIONS`, so the test still
    /// fails if a later change is folded back into it.
    #[test]
    fn a_store_already_at_v7_gains_the_run_model_columns() {
        let conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..6] {
            conn.execute_batch(sql).unwrap();
        }
        conn.execute_batch(
            "ALTER TABLE issue_state ADD COLUMN gate_failures INTEGER NOT NULL DEFAULT 0;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 7).unwrap();

        migrate(&conn).unwrap();

        conn.prepare("SELECT model, effort FROM run").expect("v8 must add the model columns");
    }

    /// The `blake3` of each migration as it shipped, v1 first. A mismatch means a released
    /// migration was edited; a missing entry means a new one was added without saying so.
    const RELEASED: &[&str] = &[
        "14e82b747db56f527afe0a5a6a0b2f94770fa7baa0d7e02aadd09f8c4d16840e", // v1
        "2da644fc2cdfbab221193b636f6dec6914c592f7944d97e9d3e958dbc54bdc85", // v2
        "dbba94e1a4136623100f45830f210aeb9ebae032d782965b7f19e34648af8c85", // v3
        "9633dfbe2976eea22c539e1a43fb9c280517455d7c7840d33f1180f2382a8363", // v4
        "67c7d584e3911b0ef33d0419694f1e73dab08a885061e58e99d25ae4a8b96c2d", // v5
        "078f6e37cb5db33df1653342b32b1364e50d4e18662c5288f502267c24f2fdf6", // v6
        "0fe2a0bfac334b97916fbd74b2bbe95bc849095d7c114180d8bf6700819d88b6", // v7
        "ed7f6925c75fac094be75380ad40f558d6ba9be90f69924168a970c358393593", // v8
        "c0f411b768e640dee5cfa9e4703d8af27c23a7e322bcac4c3e5585fdf083fca7", // v9
        "6a9665ba56edbe4ceccb6d168c317ca59b6b7f100e03790a66323dda541c5caf", // v10
        "3a74fa83ec2e66ebeb9a2231b6086493884f40fdabed03608e3608f2a9cd0a5d", // v11
        "1b15b60789f8f4d2dedd3427f09829947ef902234e31abcc5b9de075e8a7502e", // v12
        "d102caf59988230bdea97ad90b36339238e95b23246d5132fe9ab356a66f449f", // v13
    ];

    #[test]
    fn a_released_migration_is_never_edited() {
        let hash = |sql: &str| blake3::hash(sql.as_bytes()).to_hex().to_string();
        for (i, pinned) in RELEASED.iter().enumerate() {
            assert_eq!(
                hash(MIGRATIONS[i]),
                *pinned,
                "v{} has changed since it shipped. Stores that already applied it will never \
                 see the edit; put the change in a new migration instead",
                i + 1
            );
        }
        if let Some(sql) = MIGRATIONS.get(RELEASED.len()) {
            panic!(
                "v{} has no pinned hash. Once it is final, append \"{}\" to RELEASED: from then \
                 on it is released and never edited",
                RELEASED.len() + 1,
                hash(sql)
            );
        }
    }
}
