//! Per-run transcripts of the agent's raw event stream.
//!
//! The worker parses `stream-json` for a turn count, a verdict and a rejected rate limit
//! (#37), and drops everything else — `system`, every tool call. Those dropped lines are
//! precisely what a post-mortem wants and the parser does not, so once the terminal scrollback
//! is gone a run that went wrong cannot be examined at all. This module is the other half of
//! the reader: every line it sees goes to a file keyed by run id, whether or not the parser had
//! a use for it.
//!
//! Three properties, each closing something that went wrong in practice:
//!
//! * **Unbuffered, one write per line.** The diagnosis this exists to prevent was made by
//!   tailing the daemon's redirected stdout, which was block-buffered and hours behind; reading
//!   a stale tail as live state produced a confident and completely wrong conclusion. A
//!   transcript that is itself block-buffered reproduces that defect exactly, so
//!   [`TranscriptWriter`] holds a bare `File` and issues one `write_all` per line. `tail -f` on
//!   it is live.
//! * **Written outside the worktree.** The obvious home is the run's own workspace, and it is
//!   the wrong one twice over: the worktree is a git checkout the agent runs `git add` in, and
//!   it is deleted when the ticket reaches a terminal state — which would destroy the record at
//!   exactly the moment a reviewer starts looking for it. Transcripts live in their own root,
//!   which nothing in [`crate::workspace`] touches.
//! * **Bounded on both axes.** A per-run byte cap and a count of runs kept, because a daemon
//!   that runs for months otherwise fills the disk with the thing that was supposed to make it
//!   debuggable.
//!
//! Best-effort per run: a transcript that cannot be opened or written is logged and swallowed,
//! and costs a post-mortem, never a dispatch. A root that [`Transcripts::new`] cannot create is
//! the exception, and stops crewd at startup (#218).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Written as the final line when a run exceeds its byte cap, so a truncated transcript says so
/// rather than just ending. Shaped as an event of its own, since everything else in the file is
/// one and a reader is already parsing per-line JSON.
const TRUNCATION_MARKER: &str = "crew_transcript_truncated";

/// A root directory of per-run transcripts, plus the retention policy over it.
pub struct Transcripts {
    root: PathBuf,
    max_bytes_per_run: u64,
    keep_runs: usize,
}

impl Transcripts {
    /// Creates the root if it does not exist. The error is the caller's cue to run without
    /// transcripts, never to fail startup.
    pub fn new(root: &Path, max_bytes_per_run: u64, keep_runs: usize) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        // A transcript is the agent's raw tool inputs and results, so it can hold anything the
        // run touched. Fail closed: if the mode cannot be tightened, the caller's cue is to run
        // without transcripts, not to write them where any local user can read them.
        restrict(root, 0o700)?;
        Ok(Self { root: root.to_path_buf(), max_bytes_per_run, keep_runs })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a given run's transcript lives.
    ///
    /// Sanitised for display value, then suffixed with a hash of the full run id — the same
    /// construction as [`worktree_key`](crate::model::worktree_key) and for the same reason: two
    /// run ids that sanitise to the same text would otherwise interleave into one file, and an
    /// interleaved transcript is worse than no transcript, because nothing in it says it is two
    /// runs. The suffix also makes the name traversal-proof without a separate guard — no
    /// separator survives sanitisation, and the result can never be `.` or `..`.
    pub fn path_for(&self, run_id: &str) -> PathBuf {
        let sanitized: String = run_id
            .chars()
            .take(64)
            .map(
                |c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' },
            )
            .collect();
        let digest = blake3::hash(run_id.as_bytes());
        let suffix: String = digest.to_hex().chars().take(12).collect();
        let stem = if sanitized.is_empty() { "run" } else { sanitized.as_str() };
        self.root.join(format!("{stem}-{suffix}.jsonl"))
    }

    /// Opens this run's transcript, or `None` if it cannot be written.
    pub fn open(&self, run_id: &str) -> Option<TranscriptWriter> {
        let path = self.path_for(run_id);
        // Append rather than truncate: `path_for` makes a collision impossible, so the only way
        // an existing file is hit is a genuine re-open of the same run, where discarding what
        // was already recorded would be the wrong half to keep.
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        // Set at creation rather than after: a chmod that follows the open leaves a window in
        // which the file exists at the umask's mode with content already in it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(file) => {
                // The marker's bytes are reserved up front, not spent when truncation happens:
                // charging them at the end is what let the file finish larger than its own cap.
                // Its length is fixed here because it reports the configured cap, not what was
                // left over.
                let marker = format!(
                    "{{\"type\":\"{TRUNCATION_MARKER}\",\"limit_bytes\":{}}}\n",
                    self.max_bytes_per_run
                );
                let remaining = self.max_bytes_per_run.saturating_sub(marker.len() as u64);
                Some(TranscriptWriter { path, file: Some(file), remaining, marker })
            }
            Err(e) => {
                tracing::warn!(
                    run_id, path = %path.display(), error = %e,
                    "cannot open a transcript for this run; it will leave no record on disk"
                );
                None
            }
        }
    }

    /// Delete transcripts beyond the retention count, newest first.
    ///
    /// `protect` names the transcripts of runs still in flight and is not optional bookkeeping:
    /// a *stalled* run stops writing by definition, so its file ages past the newest `keep_runs`
    /// while the process behind it is still very much alive. Unlinking it would leave that run
    /// writing to an orphaned inode and the path recorded against it answering nothing — losing
    /// the transcript of the one run most likely to need one. Protected files still occupy a
    /// retention slot; they are simply never the ones deleted.
    pub fn prune(&self, protect: &[PathBuf]) {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return };

        let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .filter_map(|e| e.metadata().and_then(|m| m.modified()).ok().map(|t| (t, e.path())))
            .collect();
        if files.len() <= self.keep_runs {
            return;
        }
        files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

        for (_, path) in files.into_iter().skip(self.keep_runs) {
            if protect.contains(&path) {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), error = %e, "cannot prune a transcript");
            }
        }
    }
}

/// One run's transcript file. Writes are unbuffered and capped; see the module doc for why.
pub struct TranscriptWriter {
    path: PathBuf,
    /// The truncation line, built at open so its cost can be reserved from `remaining`.
    marker: String,
    /// Dropped once the byte cap is spent, which is what stops further writes.
    file: Option<File>,
    remaining: u64,
}

impl TranscriptWriter {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line. Silent on every failure — a run must not die because its log did.
    pub fn write_line(&mut self, line: &str) {
        if self.file.is_none() {
            return;
        }

        let cost = line.len() as u64 + 1;
        if cost > self.remaining {
            // Keep the head and say so. The head is the half that explains how a run was set up
            // and what it did first; how it *ended* is already in the store's run row, so the
            // cheap policy loses less than it looks like. The cap is a backstop against a
            // pathological run, not a path a 20-turn session is expected to reach.
            let marker = std::mem::take(&mut self.marker);
            if let Some(file) = self.file.as_mut() {
                let _ = file.write_all(marker.as_bytes());
            }
            self.file = None;
            tracing::warn!(
                path = %self.path.display(),
                "transcript hit its byte cap and was truncated"
            );
            return;
        }

        let Some(file) = self.file.as_mut() else { return };
        if file.write_all(line.as_bytes()).and_then(|()| file.write_all(b"\n")).is_err() {
            self.file = None;
            return;
        }
        self.remaining -= cost;
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "crew-transcript-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn a_run_leaves_every_line_it_was_handed_in_a_file_named_after_it() {
        let root = tmp("roundtrip");
        let t = Transcripts::new(&root, 1 << 20, 10).unwrap();

        let mut w = t.open("iss-1-1700").unwrap();
        w.write_line(r#"{"type":"system","subtype":"init"}"#);
        w.write_line(r#"{"type":"assistant"}"#);
        drop(w);

        let text = std::fs::read_to_string(t.path_for("iss-1-1700")).unwrap();
        assert_eq!(text.lines().count(), 2);
        // The `system` line is one the parser drops; a transcript that dropped it too would be
        // no use for the post-mortem this exists for.
        assert!(text.contains(r#""subtype":"init""#));

        std::fs::remove_dir_all(&root).ok();
    }

    /// Transcripts carry the agent's raw tool inputs and results, so a default umask that
    /// leaves them group- or world-readable is a disclosure, not an inconvenience.
    #[cfg(unix)]
    #[test]
    fn a_transcript_is_readable_only_by_the_operator_who_ran_it() {
        use std::os::unix::fs::PermissionsExt;

        let root = tmp("modes");
        let t = Transcripts::new(&root, 1 << 20, 10).unwrap();
        let mut w = t.open("private").unwrap();
        w.write_line("{\"secret\":\"in the tool result\"}");
        let path = w.path().to_path_buf();
        drop(w);

        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "the transcript root must not be readable by other users");
        assert_eq!(file_mode, 0o600, "a transcript must not be readable by other users");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_run_that_never_stops_talking_cannot_grow_past_its_cap() {
        let root = tmp("cap");
        const CAP: u64 = 256;
        let t = Transcripts::new(&root, CAP, 10).unwrap();

        let mut w = t.open("noisy").unwrap();
        let path = w.path().to_path_buf();
        for _ in 0..200 {
            w.write_line(&"x".repeat(50));
        }
        drop(w);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.len() as u64 <= CAP,
            "the cap is a hard bound, marker included: {} bytes for a {CAP}-byte cap",
            text.len()
        );
        assert!(text.contains("xxx"), "the head of the run must survive truncation");
        assert!(
            text.contains(TRUNCATION_MARKER),
            "a truncated transcript must say so rather than just ending"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn retention_drops_the_oldest_transcripts_but_never_a_live_one() {
        let root = tmp("prune");
        let t = Transcripts::new(&root, 1 << 20, 2).unwrap();

        // Written oldest-first, with distinct mtimes so the ordering under test is real.
        let mut paths = Vec::new();
        for i in 0..5 {
            let mut w = t.open(&format!("run-{i}")).unwrap();
            w.write_line("{}");
            let p = w.path().to_path_buf();
            filetime_backdate(&p, 5 - i);
            paths.push(p);
        }

        // `run-0` is the oldest and would be pruned first — and is exactly the shape of a
        // stalled run: still live, but silent long enough to have aged out.
        t.prune(&[paths[0].clone()]);

        assert!(paths[0].exists(), "a live run's transcript must survive retention");
        assert!(paths[4].exists(), "the newest must survive");
        assert!(paths[3].exists());
        assert!(!paths[1].exists(), "an old, unprotected transcript must be pruned");
        assert!(!paths[2].exists());

        std::fs::remove_dir_all(&root).ok();
    }

    /// Set a file's mtime `secs` seconds into the past, so ordering does not depend on the test
    /// being slow enough for the filesystem clock to tick between writes.
    fn filetime_backdate(path: &Path, secs: u64) {
        // A fixed point, not the host clock: only relative ordering matters here, and
        // `clock.rs` owns every real `SystemTime::now` in this crate.
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 - secs);
        let f = File::options().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }
}
