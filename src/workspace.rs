//! Per-issue workspaces.
//!
//! [`DirWorkspace`] (slice 1) is plain directories under a root; [`GitWorktreeWorkspace`]
//! (slice 2) is a real `git worktree` per issue, behind the same trait. The containment
//! invariant belongs here rather than at the call sites, and it is checked before *deletion* as
//! well as before launch — deletion is the more dangerous of the two, and the spec only
//! mandates the check for launch.

use std::collections::HashSet;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::credentials::Credentials;
use crate::forge::{ForgeError, Published, Publisher, Synced};
use crate::model::{dispatch_suffix, looks_like_commit, worktree_key};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("refusing to operate on {path}: outside workspace root {root}")]
    OutsideRoot { path: PathBuf, root: PathBuf },
    #[error("{path} is not a git repository")]
    NotAGitRepo { path: PathBuf },
    #[error("git {args} failed: {stderr}")]
    Git { args: String, stderr: String },
    /// `workspace.repo` or `workspace.root` resolves inside a linked worktree of the
    /// repository — one orchestrator about to run inside another's checkout. Refused rather
    /// than redirected, because the worktrees such a run would create register in the shared
    /// `.git` of `main`, where the orchestrator that owns that checkout never recorded them and
    /// will never revisit them.
    #[error(
        "refusing to nest: {path} is inside the linked worktree {worktree} of {main}; \
         a run there would register worktrees and branches the orchestrator owning that checkout \
         never recorded. Point workspace.repo at a checkout that is not itself a worktree \
         (e.g. a throwaway clone) and workspace.root outside every worktree of it"
    )]
    Nested { path: PathBuf, worktree: PathBuf, main: PathBuf },
    /// The base a new branch starts from could not be fetched. Not answered by branching from
    /// `HEAD` instead: that is the operator's checkout, which can be any branch at any age (#170).
    #[error(
        "cannot fetch the base `{base}` from `{remote}`; not branching from HEAD, which may be \
         behind it: {reason}"
    )]
    BaseFetch { remote: String, base: String, reason: String },
}

#[derive(Debug)]
pub struct Prepared {
    pub path: PathBuf,
    /// True only when this call created the directory. Gates first-time setup.
    pub created_now: bool,
    /// The branch this run's commits land on, for impls that have one. It is the only durable
    /// artifact a dispatched run leaves behind — the worktree directory is scratch space — so
    /// it is reported upwards rather than kept private to the impl, and the caller persists it
    /// rather than recomputing it later: `identifier` can be renamed after this call returns,
    /// and a name derived from the *current* identifier would silently stop matching the ref
    /// this call actually checked out.
    pub branch: Option<String>,
    /// Uncommitted work earlier runs' removals saved for this issue, oldest first; empty when
    /// there is none. Reported rather than applied: a stale snapshot applied silently can
    /// conflict with work committed since, so the agent is told it exists and decides (#22).
    pub wip: Vec<WipSnapshot>,
    /// The commit the worktree sits on, for impls that have one. A session handed off to
    /// another worker is resumed only while this has not moved since the handoff (#165); `None`
    /// never matches, so a workspace that cannot say starts the returning worker fresh.
    pub head: Option<String>,
}

/// A side ref holding the uncommitted state of a worktree at the moment it was removed.
///
/// Kept off the run's branch on purpose: the handoff gate, delivery and `remove`'s merged check
/// all read the branch, and a snapshot commit there would be handed off as the agent's work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WipSnapshot {
    /// Full ref name, `refs/crew/wip/<issue key>/<sequence>-<commit>`.
    pub ref_name: String,
    /// `git diff --stat` of the snapshot against the branch head it was taken on.
    pub diffstat: String,
}

/// What `remove` did to the branch, as distinct from the worktree directory it always removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Removed {
    /// True only when this call deleted the branch along with the directory. `false` covers
    /// every case a caller must treat alike: the branch outlived cleanup because it holds
    /// commits the base does not (or, with no base configured, `repo`'s HEAD), there was never
    /// a branch for this issue, or this impl has no branches at all. A caller that persisted the branch at `prepare` time clears that
    /// record exactly when this is `true`, and leaves it alone otherwise.
    pub branch_deleted: bool,
}

pub trait Workspace: Send + Sync {
    fn prepare(&self, issue_id: &str, identifier: &str) -> Result<Prepared, WorkspaceError>;

    /// [`Workspace::prepare`], with the issue title and the branch already stored for it.
    ///
    /// A new git branch is named from the title. Passing the stored name is what keeps a later
    /// edit of that title from minting a second branch, which delivery would never push: it
    /// reads the stored one (#205). Impls with no branches ignore both.
    fn prepare_for(
        &self,
        issue_id: &str,
        identifier: &str,
        title: &str,
        stored_branch: Option<&str>,
    ) -> Result<Prepared, WorkspaceError> {
        let _ = (title, stored_branch);
        self.prepare(issue_id, identifier)
    }

    fn remove(&self, issue_id: &str, identifier: &str) -> Result<Removed, WorkspaceError>;
    fn path_for(&self, issue_id: &str, identifier: &str) -> PathBuf;

    /// The branch this issue's runs commit on, for impls that have one.
    ///
    /// The snapshot does not call this per row — it reads the branch [`Workspace::prepare`]
    /// stored — so a tick does not pay for a subprocess. It stays the naming `prepare` uses,
    /// so the two cannot spell the branch differently.
    fn branch_for(&self, issue_id: &str, identifier: &str) -> Option<String>;
}

/// Shared by every [`Workspace`] impl: refuse a path whose resolved parent is not the root
/// itself or a descendant of it. Compares the *parent*, not `path`, because the leaf may not
/// exist yet and a non-existent path cannot be canonicalised.
fn guard_within(root: &Path, path: &Path) -> Result<(), WorkspaceError> {
    let parent = path.parent().unwrap_or(path);
    let resolved = parent
        .canonicalize()
        .map_err(|source| WorkspaceError::Io { path: parent.to_path_buf(), source })?;
    if resolved != root && !resolved.starts_with(root) {
        return Err(WorkspaceError::OutsideRoot {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        });
    }
    Ok(())
}

pub struct DirWorkspace {
    root: PathBuf,
}

impl DirWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|source| WorkspaceError::Io { path: root.clone(), source })?;
        // Canonicalise once so later containment checks compare resolved paths, not the
        // symlink-laden strings the caller happened to pass in.
        let root = root
            .canonicalize()
            .map_err(|source| WorkspaceError::Io { path: root.clone(), source })?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every path leaving this type passes through here.
    fn guard(&self, path: &Path) -> Result<(), WorkspaceError> {
        guard_within(&self.root, path)
    }
}

impl Workspace for DirWorkspace {
    fn path_for(&self, issue_id: &str, identifier: &str) -> PathBuf {
        self.root.join(worktree_key(issue_id, identifier))
    }

    /// Plain directories, so there is no branch to report — not an unknown one.
    fn branch_for(&self, _issue_id: &str, _identifier: &str) -> Option<String> {
        None
    }

    fn prepare(&self, issue_id: &str, identifier: &str) -> Result<Prepared, WorkspaceError> {
        let path = self.path_for(issue_id, identifier);
        self.guard(&path)?;
        let created_now = !path.exists();
        std::fs::create_dir_all(&path)
            .map_err(|source| WorkspaceError::Io { path: path.clone(), source })?;
        Ok(Prepared { path, created_now, branch: None, wip: Vec::new(), head: None })
    }

    fn remove(&self, issue_id: &str, identifier: &str) -> Result<Removed, WorkspaceError> {
        let path = self.path_for(issue_id, identifier);
        self.guard(&path)?;
        if path.exists() {
            std::fs::remove_dir_all(&path)
                .map_err(|source| WorkspaceError::Io { path: path.clone(), source })?;
        }
        Ok(Removed::default())
    }
}

/// A real `git worktree` per issue, on its own branch. A new one starts from the base
/// [`GitWorktreeWorkspace::branching_from`] names — the remote's copy, after a fetch, when
/// delivery is on — and from `repo`'s HEAD only when that is unset (#170).
///
/// **Uncommitted work is snapshotted, not discarded, on removal.** A run killed mid-flight —
/// stall, turn budget, shutdown — leaves whatever it had not committed in the directory, and
/// `remove` force-removes that directory (#22). Refusing to remove a dirty worktree instead
/// would strand every issue that reaches a terminal state with so much as a scratch file. So
/// `remove` first commits the dirty tree under [`GitWorktreeWorkspace::wip_prefix`], a side ref the
/// branch never sees, and `prepare` reports it to the next run. Every removal path goes through
/// here, and every caller has already confirmed the run stopped, so this is the one place the
/// snapshot can sit in the window between `kill` and deletion without a caller forgetting it.
///
/// **Committed work survives it.** The branch outlives the worktree whenever it carries commits
/// `repo`'s HEAD or the base does not already have. Cleanup is driven by a ticket reaching a
/// terminal state, and closing a ticket is not a decision to discard the run's output — so the
/// directory goes and the branch stays.
pub struct GitWorktreeWorkspace {
    root: PathBuf,
    repo: PathBuf,
    push_auth: Option<PushAuth>,
    start: Option<StartPoint>,
}

/// Held across every fetch of the base into `repo`. The gate and `prepare` update the same
/// remote-tracking ref, and the one that loses `cannot lock ref` would be charged a failure (#161).
pub type FetchLock = Arc<parking_lot::Mutex<()>>;

/// Kills `pid`'s process group on drop, unless disarmed after `wait`. `Child`'s drop signals
/// only that pid, so a helper it spawned would survive an early return.
struct KillGroup {
    pid: i32,
    armed: bool,
}

impl Drop for KillGroup {
    fn drop(&mut self) {
        if self.armed {
            let _ = kill(Pid::from_raw(-self.pid), Signal::SIGKILL);
        }
    }
}

/// Flags for every fetch of the base: `--no-write-fetch-head` leaves `repo`'s existing
/// `FETCH_HEAD` untouched, so a merge the operator has pending still sees the fetch it made, and
/// `--no-auto-gc` stops the detached `git maintenance` that would otherwise lock `repo` under the
/// next gate or `prepare`.
pub(crate) const BASE_FETCH: [&str; 5] =
    ["fetch", "--quiet", "--no-tags", "--no-auto-gc", "--no-write-fetch-head"];

/// `prepare` runs on the scheduler's tick, so a wait here stops `harvest_gates` from timing out
/// the gate whose fetch holds the lock. Past it, `prepare` fails as a retryable error instead.
const PREPARE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Kills a `prepare` fetch that has not exited by then. `http.lowSpeedTime` never arms until
/// bytes are moving, so a connect that never answers holds the tick until the OS gives up,
/// which a blackholed route may never do (review on #194).
const PREPARE_FETCH_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Where a new branch starts: `base` in `repo`, or `remote`'s copy of it after a fetch. Unset,
/// a new branch starts from `repo`'s HEAD, the operator's checkout.
struct StartPoint {
    base: String,
    remote: Option<String>,
    fetch_lock: FetchLock,
    lock_wait: std::time::Duration,
    fetch_wait: std::time::Duration,
}

/// The credential `publish` pushes with when the orchestrator has an identity of its own (#64),
/// and the URL it pushes to. Unset, the push rides the operator's ambient git credential to the
/// remote's own push URL, as it always has.
///
/// The URL is explicit because the remote's is not trustworthy for this: an explicit `pushurl`
/// or an SSH host alias sends a push over SSH, authored by the operator's key whatever
/// credential this holds, and `pushInsteadOf` rewriting is skipped for exactly those.
struct PushAuth {
    creds: Arc<dyn Credentials>,
    url: String,
}

impl std::fmt::Debug for PushAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PushAuth(..)")
    }
}

/// A `git credential-store` file holding one token for one push, in a private directory outside
/// every worktree, deleted when dropped.
///
/// Each other route puts the token where the agent sharing this worktree can read it: a URL with
/// the token in it lands in `.git/config`, `-c http.extraheader=` lands in argv (`ps`), and an
/// environment variable is readable from the push's own process. The file is named in argv, not
/// its contents, and lives only for the length of the push.
///
/// This keeps the token out of the places an agent reads by accident, not out of reach of one
/// that goes looking: a worker runs as the same user, and the App's private key, from which any
/// number of tokens can be minted, is a file that user can read for the daemon's whole life.
/// Only a separate uid would change that, the same limit the broker's module doc records.
pub(crate) struct PushCredentialFile {
    dir: PathBuf,
    file: PathBuf,
}

impl PushCredentialFile {
    pub(crate) fn new(token: &str) -> std::io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("crewd-push-{}-{n}", std::process::id()));
        // A leftover from a crashed push of the same pid and counter holds nothing live.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let guard = Self { file: dir.join("credentials"), dir };
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&guard.file)?;
        writeln!(f, "https://x-access-token:{token}@github.com")?;
        Ok(guard)
    }

    /// Config for one `git` invocation. The empty `credential.helper` first clears every helper
    /// configured anywhere else — the operator's keychain included, which would otherwise be
    /// consulted first and, on success, be handed the installation token to store.
    pub(crate) fn git_config(&self) -> Vec<String> {
        [
            "credential.helper=".to_string(),
            format!("credential.helper=store --file={}", self.file.display()),
        ]
        .into_iter()
        .flat_map(|c| ["-c".to_string(), c])
        .collect()
    }
}

fn classify_push(remote: &str, branch: &str, text: &str) -> ForgeError {
    if text.contains("stale info") {
        // The lease failed: the remote branch carries something this repository never pushed.
        // A real conflict, and the one case forcing must not resolve.
        ForgeError::Permanent(format!(
            "{remote}/{branch} has moved since it was last fetched; refusing to force over work \
             this orchestrator did not push: {text}"
        ))
    } else if is_auth_refusal(text) {
        // Reached only after `retry_on_auth` has already tried a fresh token, so a transient
        // here would re-mint and re-push on every poll for a credential that stays refused.
        ForgeError::Permanent(format!("{remote} refused the push credential: {text}"))
    } else if text.contains("rejected") || text.contains("permission") || text.contains("denied") {
        // Any other rejection will be rejected again; the rest is the network.
        ForgeError::Permanent(text.to_string())
    } else {
        ForgeError::Transient(text.to_string())
    }
}

/// A failed `read_remote`: a refused credential is permanent for the reason it is in
/// [`classify_push`], and anything else is the network.
fn classify_read(what: String, e: &WorkspaceError) -> ForgeError {
    let text = e.to_string();
    if is_auth_refusal(&text) {
        ForgeError::Permanent(format!("{what}: the remote refused the credential: {text}"))
    } else {
        ForgeError::Transient(format!("{what}: {text}"))
    }
}

/// What git prints when the remote refused the credential it sent: `Authentication failed` is
/// git's own wording for a 401 over HTTPS, and `Invalid username or token` is GitHub's.
pub(crate) fn is_auth_refusal(stderr: &str) -> bool {
    stderr.contains("Authentication failed") || stderr.contains("Invalid username or token")
}

impl Drop for PushCredentialFile {
    fn drop(&mut self) {
        // Best-effort: a directory the OS will not let go of still holds a token that expires
        // within the hour.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl GitWorktreeWorkspace {
    /// `repo` is the git repository worktrees are created from — its HEAD is the branch point
    /// unless [`GitWorktreeWorkspace::branching_from`] names a base, and its `.git` directory is
    /// where every worktree's admin state lives.
    pub fn new(root: impl Into<PathBuf>, repo: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|source| WorkspaceError::Io { path: root.clone(), source })?;
        let root = root
            .canonicalize()
            .map_err(|source| WorkspaceError::Io { path: root.clone(), source })?;

        let repo = repo.into();
        let repo = repo
            .canonicalize()
            .map_err(|source| WorkspaceError::Io { path: repo.clone(), source })?;
        if Self::git(&repo, &["rev-parse", "--git-dir"]).is_err() {
            return Err(WorkspaceError::NotAGitRepo { path: repo });
        }

        // A process killed mid-run never reaches `remove`, so git's own admin entry for that
        // worktree can outlive the directory if something outside this type deleted it by
        // hand. Pruning at startup is the one point where that bookkeeping gets reconciled;
        // it is a no-op whenever every registered worktree's directory is still present, which
        // is the case after an ordinary kill-and-restart.
        let _ = Self::git(&repo, &["worktree", "prune"]);

        // Refuse to run inside another run's checkout. The first dispatched agent to exercise
        // the daemon did so from its own worktree with the default config, and `repo = "."`
        // plus a cwd-relative `root` put six demo worktrees one level down inside it. Nothing
        // under `.gitignore` was involved: a worktree registers in the *shared* `.git` of the
        // main checkout, so the top-level orchestrator — which never recorded those keys —
        // was left holding registrations and branches it will never revisit, and the merged
        // check that protects an agent's commits is exactly what keeps them alive.
        //
        // Refused rather than redirected to the top-level root: redirecting would move the
        // directories but still create registrations and branches the owning orchestrator does
        // not know about, which is the same litter made harder to see. Both paths are checked
        // because either alone reproduces it — `repo` inside a worktree nests the metadata,
        // `root` inside one nests the directories under something `remove` will later delete.
        // After the prune above, so a stale entry for a directory that is gone cannot refuse a
        // legitimate start.
        let linked = Self::registered_worktrees(&repo)?;
        let main = linked.first().map(|(p, _)| p.clone()).unwrap_or_else(|| repo.clone());
        for path in [&repo, &root] {
            if let Some((worktree, _)) = linked.iter().skip(1).find(|(wt, _)| path.starts_with(wt))
            {
                return Err(WorkspaceError::Nested {
                    path: path.clone(),
                    worktree: worktree.clone(),
                    main,
                });
            }
        }

        Ok(Self { root, repo, push_auth: None, start: None })
    }

    /// Start a new branch from `base` rather than from `repo`'s HEAD, the operator's checkout,
    /// which can be any branch at any age (#170). With `remote`, from
    /// its copy of `base`, fetched as the gate fetches it and under the gate's `fetch_lock`, so
    /// the branch starts where the gate will measure it and the pull request will merge it.
    pub fn branching_from(
        mut self,
        base: impl Into<String>,
        remote: Option<String>,
        fetch_lock: FetchLock,
    ) -> Self {
        self.start = Some(StartPoint {
            base: base.into(),
            remote,
            fetch_lock,
            lock_wait: PREPARE_LOCK_WAIT,
            fetch_wait: PREPARE_FETCH_WAIT,
        });
        self
    }

    /// The ref a new branch starts from; `None` for `repo`'s HEAD. A failed fetch is an error,
    /// never a fall back to the local ref, which lags until someone pulls (#134).
    fn start_ref(&self) -> Result<Option<String>, WorkspaceError> {
        let Some(StartPoint { base, remote, fetch_lock, lock_wait, fetch_wait }) = &self.start
        else {
            return Ok(None);
        };
        let Some(remote) = remote else {
            return Ok(Some(base.clone()));
        };
        let tracking = format!("refs/remotes/{remote}/{base}");
        let refspec = format!("+refs/heads/{base}:{tracking}");
        let reason = match fetch_lock.try_lock_for(*lock_wait) {
            None => format!("another fetch of the base held the lock for {lock_wait:?}"),
            Some(_hold) => {
                match self.read_remote(
                    &self.repo,
                    remote,
                    &BASE_FETCH,
                    &[&refspec],
                    Some(*fetch_wait),
                ) {
                    Ok(Ok(_)) => return Ok(Some(tracking)),
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                }
            }
        };
        Err(WorkspaceError::BaseFetch { remote: remote.clone(), base: base.clone(), reason })
    }

    /// The base as `repo` last saw it, without fetching: what `remove` measures a branch
    /// against, since a branch that started there and gained nothing holds nothing to keep.
    fn known_base(&self) -> Option<String> {
        let StartPoint { base, remote, .. } = self.start.as_ref()?;
        Some(match remote {
            Some(remote) => format!("refs/remotes/{remote}/{base}"),
            None => base.clone(),
        })
    }

    /// Pushes once, and once more on a fresh token if git reports the first refused for
    /// authentication — an installation token revoked before its expiry would otherwise be
    /// handed to every delivery poll until the refresh margin, most of an hour. Only with an App
    /// credential: a second push on the operator's ambient credential would fail the same way.
    fn retry_on_auth(
        &self,
        push: impl Fn() -> Result<Result<String, WorkspaceError>, ForgeError>,
    ) -> Result<Result<String, WorkspaceError>, ForgeError> {
        let first = push()?;
        let refused =
            matches!(&first, Err(WorkspaceError::Git { stderr, .. }) if is_auth_refusal(stderr));
        match &self.push_auth {
            Some(PushAuth { creds, .. }) if refused && creds.invalidate() => push(),
            _ => Ok(first),
        }
    }

    /// Push as the orchestrator's own identity rather than the operator's ambient one, to `url`
    /// — the repository's canonical HTTPS URL in `main` — never to whatever the remote resolves
    /// to for a push (see [`PushAuth`]).
    pub fn with_push_credentials(
        mut self,
        creds: Arc<dyn Credentials>,
        url: impl Into<String>,
    ) -> Self {
        self.push_auth = Some(PushAuth { creds, url: url.into() });
        self
    }

    /// The token-bearing file, when there is one, is created here and dropped by the caller
    /// once the push has returned.
    ///
    /// `lease` is spelled out on both paths, empty for "must not exist yet": a bare
    /// `--force-with-lease` takes it from `refs/remotes/<remote>/<branch>`, which a fetch in
    /// `workspace.repo` moves onto commits the worktree never took in, and the push then
    /// replaces them (#163). See [`GitWorktreeWorkspace::lease_ref`].
    fn push_args(
        &self,
        remote: &str,
        branch: &str,
        lease: &str,
    ) -> Result<(Vec<String>, Option<PushCredentialFile>), ForgeError> {
        let lease_arg = format!("--force-with-lease=refs/heads/{branch}:{lease}");
        let Some(PushAuth { creds, url }) = &self.push_auth else {
            let args = ["push", &lease_arg, "--set-upstream", remote, branch];
            return Ok((args.map(str::to_string).to_vec(), None));
        };
        let file = PushCredentialFile::new(&creds.token()?)
            .map_err(|e| ForgeError::Transient(format!("writing push credential: {e}")))?;
        let mut args = file.git_config();
        args.extend([
            "push".to_string(),
            lease_arg,
            url.clone(),
            format!("{branch}:refs/heads/{branch}"),
        ]);
        Ok((args, Some(file)))
    }

    /// A read on the ambient credential while the push uses the App's fails wherever only the App
    /// can reach the repository, and a sync that cannot read leaves the lease refusing a push
    /// whose missing commits the worktree never got to take in (#189). Every read of the remote
    /// goes through here: `git <cmd> <where> <refspecs>` in `worktree`, where `where` is the URL
    /// and credential `publish` pushes with when there is one, and `remote` otherwise. Retried on
    /// a fresh token as a push is.
    fn read_remote(
        &self,
        worktree: &Path,
        remote: &str,
        cmd: &[&str],
        refspecs: &[&str],
        limit: Option<std::time::Duration>,
    ) -> Result<Result<String, WorkspaceError>, ForgeError> {
        self.retry_on_auth(|| {
            let (mut args, file, target) = match &self.push_auth {
                None => (Vec::new(), None, remote.to_string()),
                Some(PushAuth { creds, url }) => {
                    let file = PushCredentialFile::new(&creds.token()?).map_err(|e| {
                        ForgeError::Transient(format!("writing the fetch credential: {e}"))
                    })?;
                    (file.git_config(), Some(file), url.clone())
                }
            };
            args.extend(cmd.iter().map(|c| c.to_string()));
            args.push(target);
            args.extend(refspecs.iter().map(|r| r.to_string()));
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = match limit {
                Some(limit) => Self::git_within(worktree, &args, limit),
                None => Self::git(worktree, &args),
            };
            drop(file);
            Ok(out)
        })
    }

    /// The remote head the worktree last took in, by `sync` or by its own push: the only value
    /// the push lease may hold. Outside `refs/remotes/` so no fetch moves it, and in the shared
    /// `.git` rather than `refs/worktree/` so it outlives a worktree removed and re-prepared.
    fn lease_ref(branch: &str) -> String {
        format!("refs/crew/lease/{branch}")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn guard(&self, path: &Path) -> Result<(), WorkspaceError> {
        guard_within(&self.root, path)
    }

    fn git(repo: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
        Self::git_env(repo, args, &[])
    }

    /// `http.lowSpeedTime` does not arm until bytes move, so a connect that never answers would
    /// hold `prepare`'s tick until the OS gives up. This kills the process group at `limit`.
    ///
    /// The `Child` stays on this thread and the pipes are drained on others, so the kill
    /// happens before `wait`: the pid is still this process's child and cannot have been reused.
    fn git_within(
        repo: &Path,
        args: &[&str],
        limit: std::time::Duration,
    ) -> Result<String, WorkspaceError> {
        let io = |source| WorkspaceError::Io { path: repo.to_path_buf(), source };
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().map_err(&io)?;
        let pid = child.id() as i32;
        // Declared after `child` so it drops first and the pid above is still unreaped.
        let mut kill_group = KillGroup { pid, armed: true };

        let mut stdout = child.stdout.take().ok_or_else(|| WorkspaceError::Git {
            args: args.join(" "),
            stderr: "stdout was not piped".to_string(),
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| WorkspaceError::Git {
            args: args.join(" "),
            stderr: "stderr was not piped".to_string(),
        })?;
        let (tx_out, rx_out) = std::sync::mpsc::channel();
        let (tx_err, rx_err) = std::sync::mpsc::channel();
        // `Builder::spawn`, not `thread::spawn`: an OS that refuses a thread is an error this
        // call returns, not a panic on the tick's thread (review on #194). The process group is
        // still armed for `kill_group`, so an early return here takes `git` down with it.
        std::thread::Builder::new()
            .spawn(move || {
                let mut buf = Vec::new();
                let _ = stdout.read_to_end(&mut buf);
                let _ = tx_out.send(buf);
            })
            .map_err(&io)?;
        std::thread::Builder::new()
            .spawn(move || {
                let mut buf = Vec::new();
                let _ = stderr.read_to_end(&mut buf);
                let _ = tx_err.send(buf);
            })
            .map_err(&io)?;

        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
        let (alarm_tx, alarm_rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .spawn(move || {
                let _ = cancel_rx.recv_timeout(limit);
                let _ = alarm_tx.send(());
            })
            .map_err(&io)?;

        let mut killed = false;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(&io)? {
                break status;
            }
            match alarm_rx.try_recv() {
                Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
                    killed = true;
                    break child.wait().map_err(&io)?;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::park_timeout(std::time::Duration::from_millis(20));
                }
            }
        };
        kill_group.armed = false;
        drop(cancel_tx);
        let stdout_bytes = rx_out.recv().unwrap_or_default();
        let stderr_bytes = rx_err.recv().unwrap_or_default();
        if !status.success() {
            let detail = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
            let stderr = if killed {
                match detail.is_empty() {
                    true => format!("fetch did not finish within {limit:?}"),
                    false => format!("fetch did not finish within {limit:?}: {detail}"),
                }
            } else {
                detail
            };
            return Err(WorkspaceError::Git { args: args.join(" "), stderr });
        }
        Ok(String::from_utf8_lossy(&stdout_bytes).trim().to_string())
    }

    fn git_env(
        repo: &Path,
        args: &[&str],
        env: &[(&str, &std::ffi::OsStr)],
    ) -> Result<String, WorkspaceError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .envs(env.iter().copied())
            .output()
            .map_err(|source| WorkspaceError::Io { path: repo.to_path_buf(), source })?;
        if !output.status.success() {
            return Err(WorkspaceError::Git {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// An uncut title is the unreadable branch name #205 replaces, and a cut in the middle of
    /// a word is just as hard to scan. Forty characters holds the title the operator quoted.
    const SLUG_MAX: usize = 40;

    /// The issue number as it appears in the branch: one leading `#` dropped, then anything
    /// git refuses in a ref turned into `_`. `#178` and `PROJ-123` stay recognisable; `..`
    /// and `a..b` do not reach the ref namespace as dots.
    fn issue_number(identifier: &str) -> String {
        let stripped = identifier.strip_prefix('#').unwrap_or(identifier);
        let sanitized: String = stripped
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        if sanitized.is_empty() { "issue".into() } else { sanitized }
    }

    /// Lowercase ASCII words of `title`, joined by `-`, cut at a word boundary.
    ///
    /// A word longer than [`Self::SLUG_MAX`] is skipped, not sliced: a partial token is the
    /// unreadable name the limit exists to avoid, and skipping it lets a later word still name
    /// the branch (#205). Once the slug holds a word, the next word that does not fit ends it.
    fn title_slug(title: &str) -> String {
        let mut slug = String::new();
        let mut word = String::new();
        for c in title.chars() {
            if c.is_ascii_alphanumeric() {
                word.push(c.to_ascii_lowercase());
                continue;
            }
            if !Self::push_slug_word(&mut slug, &word) && !slug.is_empty() {
                return slug;
            }
            word.clear();
        }
        if !Self::push_slug_word(&mut slug, &word) && !slug.is_empty() {
            return slug;
        }
        slug
    }

    /// Append `word` when the result still fits in [`Self::SLUG_MAX`].
    ///
    /// `false` means nothing was written. The word is never sliced: a cut in the middle of a
    /// token is the unreadable branch name the limit exists to avoid (#205).
    fn push_slug_word(slug: &mut String, word: &str) -> bool {
        if word.is_empty() {
            return true;
        }
        let sep = usize::from(!slug.is_empty());
        if slug.len() + sep + word.len() > Self::SLUG_MAX {
            return false;
        }
        if sep == 1 {
            slug.push('-');
        }
        slug.push_str(word);
        true
    }

    /// `crew/<number>-<slug>`, or `crew/<number>` when the title yields no words.
    fn pretty_branch(identifier: &str, title: &str) -> String {
        let number = Self::issue_number(identifier);
        let slug = Self::title_slug(title);
        match slug.is_empty() {
            true => format!("crew/{number}"),
            false => format!("crew/{number}-{slug}"),
        }
    }

    /// The name `branch_name` produced before #205: `crew/` plus the directory key with dots
    /// folded out. An issue already in flight has this ref, and renaming it would orphan the
    /// commits delivery is about to push.
    fn legacy_branch_name(issue_id: &str, identifier: &str) -> String {
        format!("crew/{}", Self::ref_key(issue_id, identifier))
    }

    /// `refs/crew/branch/<issue key>` → the branch this dispatch id already checked out.
    ///
    /// Keyed on the issue id alone, like [`Self::wip_prefix`]: `Store::ensure` may rename the
    /// identifier after the branch exists, and a record under the old identifier would make
    /// the next prepare mint a second branch. A symbolic ref, not a commit: the thing to
    /// remember is the name, which a title edit must not change (#205).
    fn branch_record_ref(issue_id: &str) -> String {
        format!("refs/crew/branch/{}", Self::ref_key(issue_id, issue_id))
    }

    fn ref_exists(repo: &Path, branch: &str) -> bool {
        let full = format!("refs/heads/{branch}");
        Self::git(repo, &["rev-parse", "--verify", "--quiet", &full]).is_ok()
    }

    fn checked_out_branch(path: &Path) -> Option<String> {
        Self::git(path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()
    }

    fn recorded_branch(repo: &Path, issue_id: &str) -> Option<String> {
        let raw = Self::git(repo, &["symbolic-ref", "--quiet", &Self::branch_record_ref(issue_id)])
            .ok()?;
        let name = raw.strip_prefix("refs/heads/")?;
        Self::ref_exists(repo, name).then(|| name.to_string())
    }

    fn record_branch(repo: &Path, issue_id: &str, branch: &str) -> Result<(), WorkspaceError> {
        let target = format!("refs/heads/{branch}");
        Self::git(repo, &["symbolic-ref", &Self::branch_record_ref(issue_id), &target])?;
        Ok(())
    }

    /// True when some other dispatch id's record already points at `branch`.
    ///
    /// An unowned name is not this: it is reused, because it is this issue's own branch from
    /// a prepare whose record did not land, and minting the hashed form would leave those
    /// commits on a branch nothing attaches to. A name recorded for a different dispatch id
    /// is the collision #205 requires a suffix for — sharing it would put two issues on one
    /// branch.
    fn pretty_taken_by_other(repo: &Path, issue_id: &str, branch: &str) -> bool {
        let mine = Self::branch_record_ref(issue_id);
        let target = format!("refs/heads/{branch}");
        let listed =
            Self::git(repo, &["for-each-ref", "--format=%(refname) %(symref)", "refs/crew/branch"])
                .unwrap_or_default();
        listed.lines().any(|line| {
            let mut parts = line.splitn(2, ' ');
            let name = parts.next().unwrap_or("");
            let sym = parts.next().unwrap_or("");
            name != mine && sym == target
        })
    }

    /// The branch `prepare` should check out. Order is the one that does not rename or share:
    /// the stored name, then the record, then a legacy ref, then the hashed form this issue
    /// already holds, then a free pretty name, then — only when another dispatch id holds
    /// the pretty name — the hashed form.
    fn resolve_branch(
        &self,
        issue_id: &str,
        identifier: &str,
        title: &str,
        stored: Option<&str>,
    ) -> String {
        if let Some(stored) = stored.filter(|b| Self::ref_exists(&self.repo, b)) {
            return stored.to_string();
        }
        if let Some(recorded) = Self::recorded_branch(&self.repo, issue_id) {
            return recorded;
        }
        let legacy = Self::legacy_branch_name(issue_id, identifier);
        if Self::ref_exists(&self.repo, &legacy) {
            return legacy;
        }
        let pretty = Self::pretty_branch(identifier, title);
        let hashed = format!("{pretty}-{}", dispatch_suffix(issue_id));
        if Self::ref_exists(&self.repo, &hashed) {
            return hashed;
        }
        if !Self::ref_exists(&self.repo, &pretty)
            || !Self::pretty_taken_by_other(&self.repo, issue_id, &pretty)
        {
            return pretty;
        }
        hashed
    }

    fn ref_key(issue_id: &str, identifier: &str) -> String {
        worktree_key(issue_id, identifier).chars().map(|c| if c == '.' { '_' } else { c }).collect()
    }

    /// Where this issue's snapshots live, one ref per snapshot beneath it.
    ///
    /// Keyed on the issue id alone: the identifier can be renamed after a snapshot is taken
    /// (`Store::ensure` allows it), and a ref named after the old one would be invisible to
    /// the next `prepare`. Outside `refs/heads/` so no branch listing, push or merged check
    /// ever sees it, and outside `refs/worktree/` so it lives in the shared `.git` and
    /// outlives the worktree.
    pub fn wip_prefix(issue_id: &str) -> String {
        format!("refs/crew/wip/{}", Self::ref_key(issue_id, issue_id))
    }

    /// Commit the worktree's tracked changes and untracked, non-ignored files to a new ref
    /// under `prefix`, parented on its HEAD, and return that ref when there was anything to
    /// save.
    ///
    /// One ref per snapshot rather than one per issue: two runs stopped in turn both snapshot
    /// off the branch head, so moving a single ref would orphan the first before the agent had
    /// decided anything about it.
    ///
    /// Built in a scratch index so the worktree's own index — which the agent may have staged
    /// into deliberately — is never touched, and compared against HEAD's tree so a clean
    /// worktree creates no ref and an ordinary finish gains no noise. Authored as `crewd`
    /// so it cannot be mistaken for the agent's own commit.
    fn snapshot(path: &Path, prefix: &str) -> Result<Option<String>, WorkspaceError> {
        let index = Self::git(path, &["rev-parse", "--git-path", "crew-wip-index"])?;
        let index = path.join(index);
        let _ = std::fs::remove_file(&index);
        let env: [(&str, &std::ffi::OsStr); 5] = [
            ("GIT_INDEX_FILE", index.as_os_str()),
            ("GIT_AUTHOR_NAME", "crewd".as_ref()),
            ("GIT_AUTHOR_EMAIL", "crewd@localhost".as_ref()),
            ("GIT_COMMITTER_NAME", "crewd".as_ref()),
            ("GIT_COMMITTER_EMAIL", "crewd@localhost".as_ref()),
        ];
        let result = (|| {
            // Seeded from the worktree's own index, not from HEAD: a path the agent staged past
            // `.gitignore` (`git add -f`) exists only there, and `add -A` over a HEAD-seeded
            // index would treat it as ignored and leave it out of the snapshot.
            let real = path.join(Self::git(path, &["rev-parse", "--git-path", "index"])?);
            if std::fs::copy(&real, &index).is_err() {
                Self::git_env(path, &["read-tree", "HEAD"], &env)?;
            }
            Self::git_env(path, &["add", "-A"], &env)?;
            let tree = Self::git_env(path, &["write-tree"], &env)?;
            if tree == Self::git(path, &["rev-parse", "HEAD^{tree}"])? {
                return Ok(None);
            }
            let msg = "crewd: uncommitted work at removal\n\nSnapshot of the worktree as \
                       its run left it, parented on the branch head. Not on any branch; apply \
                       with `git cherry-pick --no-commit <ref>`.";
            let commit =
                Self::git_env(path, &["commit-tree", &tree, "-p", "HEAD", "-m", msg], &env)?;
            let wip_ref = format!(
                "{prefix}/{:06}-{}",
                Self::next_wip_seq(path, prefix),
                &commit[..commit.len().min(12)]
            );
            Self::git(path, &["update-ref", &wip_ref, &commit])?;
            Ok(Some(wip_ref))
        })();
        // Best-effort: the scratch index lives in this worktree's admin directory, which the
        // `worktree remove` that follows deletes anyway.
        let _ = std::fs::remove_file(&index);
        result
    }

    /// One past the highest sequence number already under `prefix`, so `refname` order is
    /// creation order. Commit dates cannot give that: they have one-second resolution, and two
    /// removals of the same issue inside one second would list in arbitrary order.
    fn next_wip_seq(repo: &Path, prefix: &str) -> u32 {
        let refs = Self::git(repo, &["for-each-ref", "--format=%(refname:lstrip=-1)", prefix])
            .unwrap_or_default();
        refs.lines()
            .filter_map(|name| name.split('-').next()?.parse::<u32>().ok())
            .max()
            .map_or(1, |n| n + 1)
    }

    /// Every snapshot earlier removals left for this issue, oldest first.
    ///
    /// Best-effort: a listing git refuses reads as none, costing the next run a hint and never
    /// costing it the dispatch. The refs themselves are untouched either way.
    fn existing_wip(&self, issue_id: &str) -> Vec<WipSnapshot> {
        let prefix = Self::wip_prefix(issue_id);
        let args = ["for-each-ref", "--sort=refname", "--format=%(refname)", &prefix];
        let Ok(refs) = Self::git(&self.repo, &args) else { return Vec::new() };
        refs.lines()
            .map(|ref_name| {
                let range = format!("{ref_name}^..{ref_name}");
                let diffstat =
                    Self::git(&self.repo, &["diff", "--stat", &range]).unwrap_or_default();
                WipSnapshot { ref_name: ref_name.to_string(), diffstat }
            })
            .collect()
    }

    /// True when `branch` exists and is not an ancestor of `base`, or of `HEAD` when no base is
    /// configured. The checkout is the wrong measure once a base is set: it can contain the
    /// branch — the operator merged it locally — while the base the pull request merges into
    /// does not, and `-B` would then move the branch off those commits (review on #194).
    ///
    /// `merge-base --is-ancestor` exits non-zero both for a branch that is ahead and for one
    /// that does not exist, so existence is established first rather than inferred from it.
    fn branch_carries_work(repo: &Path, branch: &str, base: Option<&str>) -> bool {
        let full = format!("refs/heads/{branch}");
        if Self::git(repo, &["rev-parse", "--verify", "--quiet", &full]).is_err() {
            return false;
        }
        let into = base.unwrap_or("HEAD");
        Self::git(repo, &["merge-base", "--is-ancestor", branch, into]).is_err()
    }

    fn is_worktree_checkout(path: &Path) -> bool {
        path.join(".git").is_file()
    }

    /// Every worktree `repo`'s shared metadata knows about, main checkout first, each with the
    /// branch it has checked out when it has one. Paths are canonicalised where they still
    /// exist so they compare equal to the canonical `root` and `repo` this type holds; an
    /// entry whose directory is already gone keeps git's own spelling, which is enough to
    /// name it in a log line.
    fn registered_worktrees(repo: &Path) -> Result<Vec<(PathBuf, Option<String>)>, WorkspaceError> {
        let porcelain = Self::git(repo, &["worktree", "list", "--porcelain"])?;
        let mut out: Vec<(PathBuf, Option<String>)> = Vec::new();
        for line in porcelain.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                let path = PathBuf::from(p);
                out.push((path.canonicalize().unwrap_or(path), None));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/")
                && let Some(last) = out.last_mut()
            {
                last.1 = Some(b.to_string());
            }
        }
        Ok(out)
    }
}

impl Workspace for GitWorktreeWorkspace {
    fn path_for(&self, issue_id: &str, identifier: &str) -> PathBuf {
        self.root.join(worktree_key(issue_id, identifier))
    }

    fn branch_for(&self, issue_id: &str, identifier: &str) -> Option<String> {
        Some(self.resolve_branch(issue_id, identifier, "", None))
    }

    fn prepare(&self, issue_id: &str, identifier: &str) -> Result<Prepared, WorkspaceError> {
        self.prepare_for(issue_id, identifier, "", None)
    }

    fn prepare_for(
        &self,
        issue_id: &str,
        identifier: &str,
        title: &str,
        stored_branch: Option<&str>,
    ) -> Result<Prepared, WorkspaceError> {
        let path = self.path_for(issue_id, identifier);
        self.guard(&path)?;

        // A real worktree checkout has a `.git` *file* (pointing at the admin directory back
        // in `repo`), not a `.git` directory. Trusting bare `path.exists()` here would silently
        // treat a leftover plain directory — e.g. from a `DirWorkspace` deployment migrating to
        // this type, or any other stray write to the workspace root — as an already-prepared
        // worktree, when nothing ever registered it with git.
        //
        // The branch this issue owns, not whatever the worktree was switched onto. Git lets
        // this checkout move to another issue's retained ref once that issue's worktree is
        // gone; recording the checkout would store both issues against one branch, which
        // delivery then pushes (#205). An owned name the checkout is already on is left
        // there, so an in-flight legacy spelling is not overwritten with a new one.
        let wip = self.existing_wip(issue_id);
        if Self::is_worktree_checkout(&path) {
            let owned = self.resolve_branch(issue_id, identifier, title, stored_branch);
            if Self::checked_out_branch(&path).is_some_and(|current| current != owned) {
                Self::git(&path, &["switch", "--quiet", &owned])?;
            }
            Self::record_branch(&self.repo, issue_id, &owned)?;
            let head = Self::git(&path, &["rev-parse", "HEAD"]).ok();
            return Ok(Prepared { path, created_now: false, branch: Some(owned), wip, head });
        }

        let branch = self.resolve_branch(issue_id, identifier, title, stored_branch);
        let path_str = path.to_string_lossy().into_owned();
        let start = self.start_ref()?;
        // A branch left behind by an earlier run holds that run's commits, so this attaches to
        // it rather than resetting it — otherwise a re-dispatch after `Done`, or a crash
        // between `prepare` and `remove`, would throw the agent's work away. `-B` stays the
        // path for a branch carrying nothing the configured base does not already have — or
        // `HEAD`, when no base is configured. Judged against the checkout, a branch the
        // operator merged locally would be reset onto a base that lacks it (review on #194).
        // A run that crashed before committing anything still cannot turn every future
        // `prepare` for this issue into a permanent "branch already exists" failure. If `path`
        // exists but is not a worktree checkout, git itself refuses with a clear error rather
        // than this type guessing at what to do with foreign state.
        if Self::branch_carries_work(&self.repo, &branch, start.as_deref()) {
            Self::git(&self.repo, &["worktree", "add", &path_str, &branch])?;
        } else if let Some(start) = &start {
            // `--no-track`: a branch tracking `origin/<base>` would have an agent's bare
            // `git push` refused by `push.default=simple`, or aimed at the base itself.
            let args = ["worktree", "add", "--no-track", "-B", &branch, &path_str, start];
            Self::git(&self.repo, &args)?;
        } else {
            Self::git(&self.repo, &["worktree", "add", "-B", &branch, &path_str])?;
        }
        // Without this record the next prepare cannot tell this issue's branch from another
        // issue's identical `crew/<number>-<slug>`, and would either share it or leave these
        // commits on a name nothing attaches to (#205).
        Self::record_branch(&self.repo, issue_id, &branch)?;
        let head = Self::git(&path, &["rev-parse", "HEAD"]).ok();
        Ok(Prepared { path, created_now: true, branch: Some(branch), wip, head })
    }

    fn remove(&self, issue_id: &str, identifier: &str) -> Result<Removed, WorkspaceError> {
        let path = self.path_for(issue_id, identifier);
        self.guard(&path)?;

        if !path.exists() {
            return Ok(Removed::default());
        }

        // Read before `worktree remove` deletes the checkout. The branch to delete is the one
        // this issue owns — the record, else the name resolve would check out, which is how a
        // legacy ref is still found when `remove` is not handed the title. The checkout is
        // that branch only while the agent left it there. Deleting a checkout switched onto
        // another issue's retained ref removes their commits, and `branch -d` succeeds once
        // those commits are already in HEAD (#205).
        let checked_out =
            if Self::is_worktree_checkout(&path) { Self::checked_out_branch(&path) } else { None };
        let owned = Self::recorded_branch(&self.repo, issue_id)
            .unwrap_or_else(|| self.resolve_branch(issue_id, identifier, "", None));

        // Worktrees registered *beneath* this one — an orchestrator that ran inside this
        // checkout before `new` refused that, or anything else that nested a worktree here by
        // hand. Collected before removal because `worktree remove --force` deletes their
        // directories along with the parent's without knowing they were worktrees, and once
        // the directories are gone their branches can no longer be told from any other.
        let nested: Vec<(PathBuf, Option<String>)> = Self::registered_worktrees(&self.repo)
            .unwrap_or_default()
            .into_iter()
            .filter(|(wt, _)| wt != &path && wt.starts_with(&path))
            .collect();

        // Before the directory goes, and failing closed: a snapshot that could not be taken
        // leaves the worktree in place for the next cleanup to retry, rather than deleting the
        // only copy of the work. A plain directory at the path has nothing to snapshot, and
        // `worktree remove` below refuses it with git's own error.
        if Self::is_worktree_checkout(&path)
            && let Some(wip_ref) = Self::snapshot(&path, &Self::wip_prefix(issue_id))?
        {
            tracing::info!(
                worktree = %path.display(), wip = %wip_ref,
                "worktree held uncommitted work; snapshotted it before removal"
            );
        }

        let path_str = path.to_string_lossy().into_owned();
        Self::git(&self.repo, &["worktree", "remove", "--force", &path_str])?;

        // A branch carrying nothing the base does not already hold is deleted, so an issue that
        // produced no commit leaves no litter; one carrying commits outlives its worktree.
        // With a base configured (#170) the base alone decides, as in `branch_carries_work`,
        // and the delete is `-D` once `merge-base --is-ancestor` has shown the base holds it:
        // `-d` would ask the operator's checkout instead, and a checkout that merged the
        // agent's commits locally would let them go while the base still lacks them (review on
        // #194). With no base, `-d` against `HEAD` is the test, git's own merged check.
        //
        // Best-effort either way: a branch that was already deleted, or never created because
        // `prepare` failed before reaching it, must not turn a successful worktree removal into
        // an error — it just means `branch_deleted` reads `false`, same as "kept".
        let mismatched = checked_out.as_ref().is_some_and(|current| current != &owned);
        let branch = if mismatched { owned } else { checked_out.unwrap_or(owned) };
        let branch_deleted = match self.known_base() {
            Some(base) => {
                Self::git(&self.repo, &["merge-base", "--is-ancestor", &branch, &base]).is_ok()
                    && Self::git(&self.repo, &["branch", "-D", &branch]).is_ok()
            }
            None => Self::git(&self.repo, &["branch", "-d", &branch]).is_ok(),
        };
        if branch_deleted {
            // A record that still names this branch would, once some other issue created that
            // same pretty name, read as this issue's branch and check out theirs. `update-ref
            // -d` follows the symbolic ref and deletes the branch; `--delete` removes the
            // record alone (#205).
            let record = Self::branch_record_ref(issue_id);
            let points_here = Self::git(&self.repo, &["symbolic-ref", "--quiet", &record])
                .ok()
                .is_some_and(|raw| raw == format!("refs/heads/{branch}"));
            if points_here
                && let Err(e) = Self::git(&self.repo, &["symbolic-ref", "--delete", &record])
            {
                tracing::warn!(
                    issue_id,
                    branch,
                    error = %e,
                    "deleted the branch but not its name record"
                );
            }
        }

        // Reconcile what the removal just orphaned in the shared metadata. The registrations
        // now point at directories that no longer exist, which is precisely what `prune`
        // reclaims — and it has to run first, because `branch -d` refuses a branch a
        // registered worktree still has checked out, stale or not. The branches then get the
        // same `-d` as this run's own: one sitting on a commit HEAD already has goes, one
        // carrying anything else stays and is named here, because the rule that protects an
        // agent's commits is not weakened for litter. That is the case for a nested run
        // branched off a parent that had committed: its branches are kept until the parent
        // branch is merged, at which point `git branch -d` on them succeeds by hand.
        if !nested.is_empty() {
            let _ = Self::git(&self.repo, &["worktree", "prune"]);
            for (wt, nested_branch) in &nested {
                match nested_branch {
                    Some(b) if Self::git(&self.repo, &["branch", "-d", b]).is_ok() => {
                        tracing::info!(
                            worktree = %wt.display(), branch = %b,
                            "pruned a worktree nested inside the removed one, and its branch"
                        );
                    }
                    Some(b) => tracing::warn!(
                        worktree = %wt.display(), branch = %b,
                        "pruned a worktree nested inside the removed one; its branch carries \
                         commits HEAD does not and is kept — `git branch -d` it once they are merged"
                    ),
                    None => tracing::info!(
                        worktree = %wt.display(),
                        "pruned a detached worktree nested inside the removed one"
                    ),
                }
            }
        }
        Ok(Removed { branch_deleted })
    }
}

/// The git half of delivery, on the type that already owns every other git call.
///
/// `publish` pushes from the *worktree*, not from `repo`: the worktree's HEAD is the branch,
/// and pushing from `repo` would mean naming a ref `repo` may have checked out under a
/// different name. The commit list is taken over the remote's copy of `base` when the remote
/// has one, because that is the base the pull request will actually be opened against; a
/// local `base` that has fallen behind would list commits the remote already has.
impl Publisher for GitWorktreeWorkspace {
    fn sync(&self, worktree: &Path, branch: &str, remote: &str) -> Result<Synced, ForgeError> {
        self.guard(worktree).map_err(|e| ForgeError::Permanent(e.to_string()))?;
        // A merge into a worktree the agent left mid-rebase or mid-merge would land on the
        // rebase's detached head, or abort the agent's own merge on the way out; one into
        // tracked edits git lets through moves `HEAD` under them. The gate after the sync before
        // it (#269) would then never see the state it reports. Unreadable counts as unsafe, as
        // the gate's own checks do.
        let in_progress = ["rebase-merge", "rebase-apply", "MERGE_HEAD"].iter().any(|p| {
            Self::git(worktree, &["rev-parse", "--path-format=absolute", "--git-path", p])
                .map_or(true, |p| Path::new(&p).exists())
        });
        let dirty = Self::git(worktree, &["status", "--porcelain", "--untracked-files=no"])
            .map_or(true, |s| !s.is_empty());
        if in_progress || dirty {
            return Err(ForgeError::Permanent(format!(
                "{} is mid-rebase or mid-merge, or has uncommitted tracked changes; not syncing \
                 it with {remote}/{branch}",
                worktree.display()
            )));
        }
        // Asked of the remote, not read off `refs/remotes/`: that ref is what `workspace.repo`
        // last fetched, which is neither current nor anything this worktree took in.
        let full = format!("refs/heads/{branch}");
        let lease_ref = Self::lease_ref(branch);
        let listed = self
            .read_remote(worktree, remote, &["ls-remote", "--heads"], &[&full], None)?
            .map_err(|e| classify_read(format!("reading {remote}/{branch}"), &e))?;
        if listed.is_empty() {
            // A lease left from a branch since deleted (merged, or by hand) would make the push
            // that recreates it expect a head the remote no longer has, and be refused forever.
            Self::git(worktree, &["update-ref", "-d", &lease_ref])
                .map_err(|e| ForgeError::Transient(format!("clearing the lease: {e}")))?;
            return Ok(Synced::Absent);
        }
        // Into `FETCH_HEAD`, which is per worktree, rather than any shared ref a fetch in
        // `workspace.repo` could also move.
        self.read_remote(worktree, remote, &["fetch", "--quiet"], &[&full], None)?
            .map_err(|e| classify_read(format!("fetching {remote}/{branch}"), &e))?;
        let remote_head = Self::git(worktree, &["rev-parse", "FETCH_HEAD^{commit}"])
            .map_err(|e| ForgeError::Transient(format!("reading the fetched head: {e}")))?;
        let is_ancestor =
            |a: &str, b: &str| Self::git(worktree, &["merge-base", "--is-ancestor", a, b]).is_ok();
        // Folding an unchanged remote head back in after the gate rebases onto a moved base
        // merges commits the worktree already held — its own, or someone else's that an earlier
        // sync took in — into their rewritten copies. Where the two sides touched the same lines
        // that merge conflicts, and delivery reports it as someone else's push (#227, on #191).
        // The lease is that last incorporated head. Equality with the fetched head means the
        // remote has not moved since the last sync or publish, so the divergence is the rewrite
        // and the push replaces it. A head the lease does not name is still merged (#163).
        let lease = Self::git(worktree, &["rev-parse", "--verify", "--quiet", &lease_ref])
            .unwrap_or_default();
        let remote_unchanged = lease == remote_head;

        let synced = if is_ancestor(&remote_head, "HEAD") || remote_unchanged {
            Synced::Current { remote_head: remote_head.clone() }
        } else {
            let fast_forward = is_ancestor("HEAD", &remote_head);
            let msg = format!("Merge {remote}/{branch} into the agent's branch");
            let args: &[&str] = if fast_forward {
                &["merge", "--ff-only", "--quiet", &remote_head]
            } else {
                &["merge", "--no-ff", "--no-edit", "--quiet", "-m", &msg, &remote_head]
            };
            if let Err(e) = Self::git(worktree, args) {
                // Paths before the abort, which is what clears them; the abort is what leaves
                // the worktree as the agent had it, as the gate's rebase abort does.
                let paths: Vec<String> =
                    Self::git(worktree, &["diff", "--name-only", "--diff-filter=U"])
                        .map(|s| s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
                        .unwrap_or_default();
                let _ = Self::git(worktree, &["merge", "--abort"]);
                if paths.is_empty() {
                    // Refused before it began, a dirty tree most likely: asking again will not
                    // change the answer, and the lease still holds either way.
                    return Err(ForgeError::Permanent(format!(
                        "merging {remote}/{branch} ({remote_head}) into the worktree: {e}"
                    )));
                }
                return Ok(Synced::Conflict { remote_head, paths });
            }
            Synced::Advanced { remote_head: remote_head.clone(), merged: !fast_forward }
        };
        // Only now may the lease move to a head, and only once the worktree holds it. When the
        // fetched head is already the lease, this writes that same head back.
        Self::git(worktree, &["update-ref", &lease_ref, &remote_head])
            .map_err(|e| ForgeError::Transient(format!("recording the fetched head: {e}")))?;
        Ok(synced)
    }

    fn publish(
        &self,
        worktree: &Path,
        branch: &str,
        remote: &str,
        base: &str,
    ) -> Result<Published, ForgeError> {
        self.guard(worktree).map_err(|e| ForgeError::Permanent(e.to_string()))?;
        // With a lease, not plain: the gate rebases an already-published branch onto a newer
        // base before every re-delivery, and a rebase rewrites the history the remote holds, so
        // a plain push is rejected non-fast-forward in exactly the round the base moved and
        // delivery hands off instead of updating the pull request. Forcing is sanctioned
        // because the branch is the orchestrator's own; the lease is what keeps that apart from
        // forcing over somebody else's — it expects the remote ref to be the head the worktree
        // last took in, and a remote that has moved since is refused, not overwritten.
        let tracking = format!("refs/remotes/{remote}/{branch}");
        let lease_ref = Self::lease_ref(branch);
        let lease = Self::git(worktree, &["rev-parse", "--verify", "--quiet", &lease_ref])
            .unwrap_or_default();
        let push = || {
            let (args, credential) = self.push_args(remote, branch, &lease)?;
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let pushed = Self::git(worktree, &args);
            drop(credential);
            Ok(pushed)
        };
        self.retry_on_auth(push)?.map_err(|e| classify_push(remote, branch, &e.to_string()))?;
        let head_sha = Self::git(worktree, &["rev-parse", "HEAD"])
            .map_err(|e| ForgeError::Transient(e.to_string()))?;
        Self::git(worktree, &["update-ref", &lease_ref, &head_sha])
            .map_err(|e| ForgeError::Transient(format!("recording the pushed head: {e}")))?;
        if self.push_auth.is_some() {
            // A push to a URL moves no remote-tracking ref, and without one the branch has no
            // upstream for an operator's `git status` to compare against. Display only: the
            // lease is `lease_ref`, never this.
            let _ = Self::git(worktree, &["update-ref", &tracking, &head_sha]);
            let upstream = format!("{remote}/{branch}");
            let _ = Self::git(worktree, &["branch", "--set-upstream-to", &upstream, branch]);
        }

        // Best-effort: a fetch that fails leaves the remote-tracking `base`, else the local
        // one, which is right whenever the remote has nothing newer, and only over-lists commits
        // otherwise. Read off `FETCH_HEAD`, because a fetch from the App's URL moves no
        // remote-tracking ref.
        let fetched = self
            .read_remote(worktree, remote, &["fetch", "--quiet"], &[base], None)
            .ok()
            .and_then(Result::ok)
            .and_then(|_| Self::git(worktree, &["rev-parse", "FETCH_HEAD^{commit}"]).ok());
        let remote_base = format!("refs/remotes/{remote}/{base}");
        let base_ref = if let Some(sha) = fetched {
            sha
        } else if Self::git(worktree, &["rev-parse", "--verify", "--quiet", &remote_base]).is_ok() {
            remote_base
        } else {
            base.to_string()
        };
        let range = format!("{base_ref}..{branch}");
        let log = Self::git(worktree, &["log", "--format=%s", &range])
            .map_err(|e| ForgeError::Transient(e.to_string()))?;
        let commits = log.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
        Ok(Published { head_sha, commits })
    }

    fn stacked_on(
        &self,
        worktree: &Path,
        branch: &str,
        remote: &str,
        base: &str,
        candidates: &[String],
    ) -> Result<Option<String>, ForgeError> {
        // What the remote actually has, asked for directly rather than read off the
        // remote-tracking refs: those say what this repository last fetched, and a lower branch
        // pushed by another clone — or deleted after its merge — would be misread either way.
        // One round trip for every candidate at once; a network failure here is the same
        // transient the push after it would hit.
        let heads = self
            .read_remote(worktree, remote, &["ls-remote", "--heads"], &[], None)?
            .map_err(|e| classify_read(format!("listing {remote}'s branches"), &e))?;
        let on_remote: HashSet<&str> = heads
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter_map(|(_, r)| r.strip_prefix("refs/heads/"))
            .collect();

        // A candidate is "under" this branch when the remote has it, it is an ancestor of
        // this branch, and it carries commits `base` does not — a branch already merged into
        // `base` is not a stack, it is history.
        let under: Vec<&String> = candidates
            .iter()
            .filter(|c| *c != branch)
            .filter(|c| on_remote.contains(c.as_str()))
            .filter(|c| {
                Self::git(
                    worktree,
                    &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{c}")],
                )
                .is_ok()
            })
            .filter(|c| Self::git(worktree, &["merge-base", "--is-ancestor", c, branch]).is_ok())
            .filter(|c| Self::git(worktree, &["merge-base", "--is-ancestor", c, base]).is_err())
            .collect();
        // Of a stack of several, the pull request is based on the nearest: the one no other
        // candidate under this branch descends from.
        let nearest = under.iter().find(|c| {
            !under.iter().any(|o| {
                o != *c && Self::git(worktree, &["merge-base", "--is-ancestor", c, o]).is_ok()
            })
        });
        Ok(nearest.map(|s| s.to_string()))
    }

    fn carries(&self, worktree: &Path, branch: &str, sha: &str) -> Result<bool, ForgeError> {
        // Shape first, so a bare acknowledgement never reaches git as a revision expression —
        // `fixed` is not a ref, but `HEAD` or `@{-1}` would be, and an agent's text is input.
        if !looks_like_commit(sha) {
            return Ok(false);
        }
        self.guard(worktree).map_err(|e| ForgeError::Permanent(e.to_string()))?;
        // Existence before ancestry: `merge-base --is-ancestor` fails the same way for a commit
        // that is not an ancestor and for a name that resolves to nothing, and only the first
        // of those is a fact about the branch.
        let object = format!("{sha}^{{commit}}");
        if Self::git(worktree, &["rev-parse", "--verify", "--quiet", &object]).is_err() {
            return Ok(false);
        }
        Ok(Self::git(worktree, &["merge-base", "--is-ancestor", sha, branch]).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn tmp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "crew-ws-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn prepare_creates_once_then_reuses() {
        let root = tmp_root("reuse");
        let ws = DirWorkspace::new(&root).unwrap();

        let a = ws.prepare("id-1", "MT-1").unwrap();
        assert!(a.created_now);
        assert!(a.path.is_dir());

        let b = ws.prepare("id-1", "MT-1").unwrap();
        assert!(!b.created_now, "an existing workspace must be reused, not recreated");
        assert_eq!(a.path, b.path);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn workspaces_are_preserved_until_explicitly_removed() {
        let root = tmp_root("persist");
        let ws = DirWorkspace::new(&root).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        std::fs::write(p.join("artifact.txt"), b"warm").unwrap();

        // Re-preparing must not wipe accumulated state; that warmth is the point.
        ws.prepare("id-1", "MT-1").unwrap();
        assert!(p.join("artifact.txt").exists());

        ws.remove("id-1", "MT-1").unwrap();
        assert!(!p.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn removing_an_absent_workspace_is_not_an_error() {
        let root = tmp_root("absent");
        let ws = DirWorkspace::new(&root).unwrap();
        assert!(ws.remove("id-nope", "MT-nope").is_ok());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hostile_identifiers_stay_inside_the_root() {
        let root = tmp_root("escape");
        let ws = DirWorkspace::new(&root).unwrap();

        for bad in ["../../etc", "/etc/passwd", "..", "a/../../b"] {
            let p = ws.prepare("id-x", bad).unwrap();
            assert!(p.path.starts_with(ws.root()), "{bad} escaped the root: {}", p.path.display());
            assert_eq!(p.path.parent().unwrap(), ws.root(), "must be exactly one level deep");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    // ---- GitWorktreeWorkspace -------------------------------------------------

    /// A throwaway repo with one commit, so `git worktree add` has a HEAD to branch from.
    /// Config is set repo-local rather than relying on the machine having a global identity.
    fn tmp_repo(tag: &str) -> PathBuf {
        let p = tmp_root(&format!("repo-{tag}"));
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&p)
                .args(args)
                .output()
                .expect("git must be on PATH to run these tests");
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        p
    }

    fn is_registered_worktree(repo: &Path, path: &Path) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.strip_prefix("worktree ").is_some_and(|p| Path::new(p) == path))
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", "--quiet", branch])
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    fn constructing_over_a_non_repo_fails_clearly() {
        let root = tmp_root("not-a-repo-root");
        let not_a_repo = tmp_root("not-a-repo-target");
        assert!(matches!(
            GitWorktreeWorkspace::new(&root, &not_a_repo),
            Err(WorkspaceError::NotAGitRepo { .. })
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&not_a_repo).ok();
    }

    #[test]
    fn a_worktree_is_created_once_then_reused_across_attempts() {
        let root = tmp_root("wt-reuse");
        let repo = tmp_repo("wt-reuse");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let a = ws.prepare("id-1", "MT-1").unwrap();
        assert!(a.created_now);
        assert!(a.path.join(".git").is_file(), "a worktree checkout has a `.git` file, not dir");
        assert!(is_registered_worktree(&repo, &a.path));

        let b = ws.prepare("id-1", "MT-1").unwrap();
        assert!(!b.created_now, "an existing worktree must be reused, not recreated");
        assert_eq!(a.path, b.path);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The agent writes its checkpoint where `git rev-parse --git-path` points in a prepared
    /// worktree (#186). That path must stay out of `git status` and outside the worktree, or
    /// `git add -A` would commit it.
    #[test]
    fn the_checkpoint_path_is_outside_the_tracked_tree() {
        let root = tmp_root("wt-checkpoint");
        let repo = tmp_repo("wt-checkpoint");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let wt = ws.prepare("id-1", "MT-1").unwrap().path;
        let git = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&wt).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?} failed");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let rel = git(&["rev-parse", "--git-path", crate::worker::prompt::CHECKPOINT_GIT_PATH]);
        let checkpoint = wt.join(rel);
        std::fs::create_dir_all(checkpoint.parent().unwrap()).unwrap();
        std::fs::write(&checkpoint, "acceptance: open\nlast green commit: none\nnext: start\n")
            .unwrap();

        let status = git(&["status", "--porcelain", "--untracked-files=all"]);
        assert!(status.is_empty(), "the checkpoint must not change git status, got {status:?}");
        let checkpoint = checkpoint.canonicalize().unwrap();
        assert!(!checkpoint.starts_with(wt.canonicalize().unwrap()), "{}", checkpoint.display());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn two_issues_sharing_an_identifier_get_two_distinct_worktrees() {
        let root = tmp_root("wt-two-issues");
        let repo = tmp_repo("wt-two-issues");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let a = ws.prepare("id-a", "MT-1").unwrap();
        let b = ws.prepare("id-b", "MT-1").unwrap();

        assert_ne!(a.path, b.path);
        assert!(is_registered_worktree(&repo, &a.path));
        assert!(is_registered_worktree(&repo, &b.path));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_stale_plain_directory_at_the_target_path_is_not_silently_treated_as_a_worktree() {
        // Reproduces what a `DirWorkspace` deployment migrating to git worktrees would leave
        // behind: a plain, non-empty directory sitting exactly where a worktree would go,
        // because `worktree_key` is deterministic and both types compute the same leaf path.
        let root = tmp_root("wt-stale-plain-dir");
        let repo = tmp_repo("wt-stale-plain-dir");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let stale = ws.path_for("id-1", "MT-1");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("leftover.txt"), b"from a previous DirWorkspace run").unwrap();

        // Must not silently report this foreign directory as an already-prepared worktree.
        let err = ws.prepare("id-1", "MT-1").unwrap_err();
        assert!(matches!(err, WorkspaceError::Git { .. }), "expected a git error, got {err:?}");
        assert!(!is_registered_worktree(&repo, &stale), "must never register foreign state");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn removing_a_worktree_prunes_a_branch_that_carries_no_commits_even_from_a_dirty_tree() {
        let root = tmp_root("wt-remove");
        let repo = tmp_repo("wt-remove");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        std::fs::write(p.join("scratch.txt"), b"never committed").unwrap();
        let branch = GitWorktreeWorkspace::pretty_branch("MT-1", "");
        assert!(branch_exists(&repo, &branch));

        let removed = ws.remove("id-1", "MT-1").unwrap();

        assert!(!p.exists());
        assert!(!is_registered_worktree(&repo, &p));
        assert!(!branch_exists(&repo, &branch), "remove must prune the branch too");
        assert!(removed.branch_deleted, "and report having done so");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn removing_an_absent_worktree_is_not_an_error() {
        let root = tmp_root("wt-absent");
        let repo = tmp_repo("wt-absent");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        assert!(ws.remove("id-nope", "MT-nope").is_ok());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn hostile_identifiers_stay_inside_the_root_for_git_worktrees_too() {
        let root = tmp_root("wt-escape");
        let repo = tmp_repo("wt-escape");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        for (i, bad) in ["../../etc", "/etc/passwd", "..", "a/../../b"].iter().enumerate() {
            let p = ws.prepare(&format!("id-{i}"), bad).unwrap();
            assert!(p.path.starts_with(ws.root()), "{bad} escaped the root: {}", p.path.display());
            assert_eq!(p.path.parent().unwrap(), ws.root(), "must be exactly one level deep");
        }
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Commit a file inside a worktree, standing in for what a dispatched agent leaves behind.
    /// Identity comes from the repo-local config `tmp_repo` set; worktrees share it.
    fn commit_in(worktree: &Path, file: &str, msg: &str) {
        std::fs::write(worktree.join(file), msg.as_bytes()).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(worktree).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        git(&["add", file]);
        git(&["commit", "-q", "-m", msg]);
    }

    fn head_of(worktree: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn a_branch_holding_committed_work_outlives_the_worktree_it_is_removed_with() {
        // Cleanup fires when a ticket reaches a terminal state. Closing a ticket an agent
        // already worked must not be what destroys the commits that work produced.
        let root = tmp_root("wt-keep-branch");
        let repo = tmp_repo("wt-keep-branch");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        commit_in(&p, "work.txt", "the agent output");
        let branch = GitWorktreeWorkspace::pretty_branch("MT-1", "");

        let removed = ws.remove("id-1", "MT-1").unwrap();

        assert!(!p.exists(), "the directory is scratch space and still goes");
        assert!(branch_exists(&repo, &branch), "the run's commits must survive cleanup");
        assert!(!removed.branch_deleted, "and remove must say so, not just leave it alone");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    fn git_stdout(at: &Path, args: &[&str]) -> Option<String> {
        let out = Command::new("git").arg("-C").arg(at).args(args).output().unwrap();
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn wip_refs(repo: &Path, issue_id: &str) -> Vec<String> {
        let prefix = GitWorktreeWorkspace::wip_prefix(issue_id);
        let out = git_stdout(repo, &["for-each-ref", "--format=%(refname)", &prefix]).unwrap();
        out.lines().map(str::to_string).collect()
    }

    /// `Store::ensure` lets an identifier change under a live issue id. A snapshot keyed on the
    /// identifier it was taken under would be invisible to every `prepare` after the rename.
    #[test]
    fn a_snapshot_is_reported_after_its_issue_is_renamed() {
        let root = tmp_root("wt-wip-rename");
        let repo = tmp_repo("wt-wip-rename");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        std::fs::write(p.join("half.txt"), b"half-done").unwrap();
        ws.remove("id-1", "MT-1").unwrap();

        let renamed = ws.prepare("id-1", "MT-1-renamed").unwrap();
        assert_eq!(renamed.wip.len(), 1, "the snapshot must follow the issue, not its name");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Both snapshots are parented on the branch head, so neither is an ancestor of the other:
    /// moving one ref per issue would have left the first reachable from nothing.
    #[test]
    fn a_second_interrupted_run_does_not_overwrite_the_first_runs_snapshot() {
        let root = tmp_root("wt-wip-twice");
        let repo = tmp_repo("wt-wip-twice");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        std::fs::write(p.join("first.txt"), b"first run").unwrap();
        ws.remove("id-1", "MT-1").unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        std::fs::write(p.join("second.txt"), b"second run").unwrap();
        ws.remove("id-1", "MT-1").unwrap();

        let reported = ws.prepare("id-1", "MT-1").unwrap().wip;
        assert_eq!(reported.len(), 2, "both snapshots must be reported: {reported:?}");
        let show = |r: &str, f: &str| git_stdout(&repo, &["show", &format!("{r}:{f}")]);
        assert_eq!(show(&reported[0].ref_name, "first.txt").as_deref(), Some("first run"));
        assert_eq!(show(&reported[1].ref_name, "second.txt").as_deref(), Some("second run"));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// A path staged past `.gitignore` lives only in the worktree's own index; a scratch index
    /// rebuilt from HEAD would see it as ignored and drop it from the snapshot.
    #[test]
    fn a_file_force_staged_past_gitignore_is_kept_in_the_snapshot() {
        let root = tmp_root("wt-wip-force");
        let repo = tmp_repo("wt-wip-force");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        std::fs::write(p.join(".gitignore"), b"generated.txt\n").unwrap();
        std::fs::write(p.join("generated.txt"), b"staged on purpose").unwrap();
        let out = Command::new("git")
            .arg("-C")
            .arg(&p)
            .args(["add", "-f", "generated.txt"])
            .output()
            .unwrap();
        assert!(out.status.success());
        ws.remove("id-1", "MT-1").unwrap();

        let refs = wip_refs(&repo, "id-1");
        assert_eq!(refs.len(), 1, "exactly one snapshot: {refs:?}");
        let show = |f: &str| git_stdout(&repo, &["show", &format!("{}:{f}", refs[0])]);
        assert_eq!(show("generated.txt").as_deref(), Some("staged on purpose"));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The guard for #22: a run stopped mid-flight leaves its uncommitted work in the worktree,
    /// and `remove` is what deletes it. Without the snapshot step the ref does not exist.
    #[test]
    fn a_worktree_removed_with_uncommitted_changes_leaves_them_recoverable_from_its_wip_ref() {
        let root = tmp_root("wt-wip");
        let repo = tmp_repo("wt-wip");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        commit_in(&p, "work.txt", "committed");
        let branch_head = head_of(&p);
        std::fs::write(p.join("work.txt"), b"edited, not committed").unwrap();
        std::fs::write(p.join("new.txt"), b"never added").unwrap();
        ws.remove("id-1", "MT-1").unwrap();

        assert!(!p.exists());
        let refs = wip_refs(&repo, "id-1");
        assert_eq!(refs.len(), 1, "exactly one snapshot: {refs:?}");
        let wip = &refs[0];
        let show = |f: &str| git_stdout(&repo, &["show", &format!("{wip}:{f}")]);
        assert_eq!(show("work.txt").as_deref(), Some("edited, not committed"));
        assert_eq!(show("new.txt").as_deref(), Some("never added"));
        assert_eq!(
            git_stdout(&repo, &["rev-parse", &format!("{wip}^")]).as_deref(),
            Some(branch_head.as_str()),
            "the snapshot is parented on the branch head it was taken from"
        );
        let branch = GitWorktreeWorkspace::pretty_branch("MT-1", "");
        assert_eq!(
            git_stdout(&repo, &["rev-parse", &branch]).as_deref(),
            Some(branch_head.as_str()),
            "the run's branch must not move: it carries only the agent's own commits"
        );
        assert_eq!(
            git_stdout(&repo, &["log", "-1", "--format=%an", wip]).as_deref(),
            Some("crewd"),
            "and the snapshot must be distinguishable from the agent's commits"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_clean_worktree_is_removed_without_creating_a_wip_ref() {
        let root = tmp_root("wt-wip-clean");
        let repo = tmp_repo("wt-wip-clean");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        commit_in(&p, ".gitignore", "target\n");
        std::fs::create_dir(p.join("target")).unwrap();
        std::fs::write(p.join("target/build.o"), b"ignored output is not work").unwrap();

        ws.remove("id-1", "MT-1").unwrap();

        assert_eq!(wip_refs(&repo, "id-1"), Vec::<String>::new());
        assert!(ws.prepare("id-1", "MT-1").unwrap().wip.is_empty());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn the_next_prepare_reports_the_snapshot_with_its_diffstat_and_leaves_the_tree_clean() {
        let root = tmp_root("wt-wip-next");
        let repo = tmp_repo("wt-wip-next");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap().path;
        assert!(ws.prepare("id-1", "MT-1").unwrap().wip.is_empty());
        std::fs::write(p.join("half.txt"), b"half-done\n").unwrap();
        ws.remove("id-1", "MT-1").unwrap();

        let next = ws.prepare("id-1", "MT-1").unwrap();
        let [wip] = next.wip.as_slice() else { panic!("one snapshot reported: {:?}", next.wip) };
        assert_eq!(wip.ref_name, wip_refs(&repo, "id-1")[0]);
        assert!(wip.diffstat.contains("half.txt"), "diffstat: {}", wip.diffstat);
        assert!(!next.path.join("half.txt").exists(), "nothing is applied automatically");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn re_preparing_an_issue_does_not_reset_a_branch_that_holds_work() {
        // `-B` is what makes a crashed `prepare` recoverable, and it is also a force-reset:
        // applied to a branch a finished run already committed to, it discards that run.
        let root = tmp_root("wt-no-reset");
        let repo = tmp_repo("wt-no-reset");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let first = ws.prepare("id-1", "MT-1").unwrap().path;
        commit_in(&first, "work.txt", "the agent output");
        let committed = head_of(&first);
        ws.remove("id-1", "MT-1").unwrap();

        let again = ws.prepare("id-1", "MT-1").unwrap();
        assert!(again.path.join("work.txt").exists(), "the earlier run's file must come back");
        assert_eq!(head_of(&again.path), committed, "the branch must not have been reset");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #170: the operator's checkout is on another branch, behind the remote base. A new branch
    /// starts at the remote base the gate will rebase onto, and the checkout is not moved.
    #[test]
    fn a_new_branch_starts_from_the_remote_base_not_the_operators_checkout() {
        let root = tmp_root("wt-remote-base");
        let (repo, bare) = repo_with_remote("wt-remote-base");
        git_out(&repo, &["checkout", "-q", "-b", "probe/elsewhere"]).unwrap();
        let operators = head_of(&repo);
        let base = advance_remote(&bare, "wt-remote-base");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().branching_from(
            "main",
            Some("origin".into()),
            FetchLock::default(),
        );

        let p = ws.prepare("id-1", "MT-1").unwrap();

        assert_eq!(head_of(&p.path), base, "the branch starts at the fetched remote base");
        assert_eq!(p.head.as_deref(), Some(base.as_str()));
        assert_eq!(head_of(&repo), operators, "the operator's checkout is never moved");
        assert_eq!(
            git_out(&repo, &["symbolic-ref", "--short", "HEAD"]).unwrap(),
            "probe/elsewhere"
        );
        assert!(
            git_out(&p.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]).is_err(),
            "the branch does not track the base, so an agent's bare push cannot aim at it"
        );

        // Given nothing, it holds nothing to keep, though the lagging checkout lacks its base.
        let removed = ws.remove("id-1", "MT-1").unwrap();
        assert!(removed.branch_deleted);
        assert!(!branch_exists(&repo, p.branch.as_deref().unwrap()));

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Review on #194: the operator merged the agent's branch into their own checkout, but the
    /// base has not got it. `branch -d` would call it merged and delete it; the base decides.
    #[test]
    fn a_branch_the_checkout_merged_but_the_base_lacks_is_kept() {
        let root = tmp_root("wt-keep-unbased");
        let (repo, bare) = repo_with_remote("wt-keep-unbased");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().branching_from(
            "main",
            Some("origin".into()),
            FetchLock::default(),
        );
        let p = ws.prepare("id-1", "MT-1").unwrap();
        commit_in(&p.path, "work.txt", "the agent output");
        let branch = p.branch.clone().unwrap();
        git_out(&repo, &["merge", "-q", "--no-edit", &branch]).unwrap();

        let removed = ws.remove("id-1", "MT-1").unwrap();

        assert!(!removed.branch_deleted, "the base does not hold the agent's commits");
        assert!(branch_exists(&repo, &branch));
        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// With the base configured, a branch holding commits is attached where it is: neither
    /// reset to the base that moved since, nor rebased onto it here — that is the gate's job.
    #[test]
    fn a_branch_that_carries_work_is_attached_where_it_is() {
        let root = tmp_root("wt-attach-base");
        let (repo, bare) = repo_with_remote("wt-attach-base");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().branching_from(
            "main",
            Some("origin".into()),
            FetchLock::default(),
        );

        let first = ws.prepare("id-1", "MT-1").unwrap().path;
        commit_in(&first, "work.txt", "the agent output");
        let committed = head_of(&first);
        ws.remove("id-1", "MT-1").unwrap();
        advance_remote(&bare, "wt-attach-base");

        let again = ws.prepare("id-1", "MT-1").unwrap();
        assert_eq!(head_of(&again.path), committed, "the branch must not have been moved");

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// A fetch that fails leaves `prepare` failing, retried as any workspace error is, rather
    /// than quietly starting the branch from the operator's checkout.
    #[test]
    fn a_failed_base_fetch_fails_prepare_rather_than_branching_from_head() {
        let root = tmp_root("wt-base-fetch-fails");
        let (repo, bare) = repo_with_remote("wt-base-fetch-fails");
        std::fs::remove_dir_all(&bare).unwrap();
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().branching_from(
            "main",
            Some("origin".into()),
            FetchLock::default(),
        );

        let err = ws.prepare("id-1", "MT-1").unwrap_err();

        assert!(matches!(err, WorkspaceError::BaseFetch { .. }), "got {err:?}");
        let path = ws.path_for("id-1", "MT-1");
        assert!(!path.exists(), "no worktree is created on a guessed start point");
        assert!(!branch_exists(&repo, &GitWorktreeWorkspace::pretty_branch("MT-1", "")));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Copilot on #194: `prepare` runs on the tick, and a gate's hung fetch holding the lock
    /// would otherwise stop the tick from ever reaching the gate's timeout.
    #[test]
    fn a_base_fetch_lock_held_elsewhere_fails_prepare_rather_than_stalling_the_tick() {
        let root = tmp_root("wt-lock-held");
        let (repo, bare) = repo_with_remote("wt-lock-held");
        let lock = FetchLock::default();
        let mut ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().branching_from(
            "main",
            Some("origin".into()),
            lock.clone(),
        );
        ws.start.as_mut().unwrap().lock_wait = std::time::Duration::from_millis(50);

        let held = lock.lock();
        let err = ws.prepare("id-1", "MT-1").unwrap_err();
        drop(held);

        assert!(matches!(err, WorkspaceError::BaseFetch { .. }), "got {err:?}");
        assert!(!ws.path_for("id-1", "MT-1").exists());
        assert!(ws.prepare("id-1", "MT-1").is_ok(), "the retry goes through once it is free");

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Copilot on #194: the operator's checkout contains the branch (they merged it) and the
    /// base does not. `prepare` must attach it, not `-B` it onto that base.
    #[test]
    fn a_branch_the_checkout_contains_is_kept_when_the_base_does_not() {
        let root = tmp_root("wt-head-contains");
        let (repo, bare) = repo_with_remote("wt-head-contains");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().branching_from(
            "main",
            Some("origin".into()),
            FetchLock::default(),
        );

        let first = ws.prepare("id-1", "MT-1").unwrap().path;
        commit_in(&first, "work.txt", "the agent output");
        let committed = head_of(&first);
        let branch = GitWorktreeWorkspace::pretty_branch("MT-1", "");
        ws.remove("id-1", "MT-1").unwrap();
        git_out(&repo, &["merge", "--ff-only", &branch]).unwrap();

        let again = ws.prepare("id-1", "MT-1").unwrap();
        assert_eq!(head_of(&again.path), committed, "the branch must not have been reset");
        assert_eq!(head_of(&repo), committed, "the operator's checkout is never moved");

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Copilot on #194: a peer that accepts the fetch and never answers. `http.lowSpeedTime`
    /// would not be what ends it during connect, and `prepare` must not hold the tick either way.
    #[test]
    fn a_base_fetch_that_does_not_finish_fails_prepare_rather_than_stalling_the_tick() {
        let root = tmp_root("wt-fetch-hangs");
        let (repo, bare) = repo_with_remote("wt-fetch-hangs");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf);
                let _ = sock.read(&mut buf);
            }
        });
        git_out(&repo, &["remote", "set-url", "origin", &format!("http://127.0.0.1:{port}/r.git")])
            .unwrap();
        let mut ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().branching_from(
            "main",
            Some("origin".into()),
            FetchLock::default(),
        );
        ws.start.as_mut().unwrap().fetch_wait = std::time::Duration::from_millis(400);

        let err = ws.prepare("id-1", "MT-1").unwrap_err();

        assert!(
            matches!(&err, WorkspaceError::BaseFetch { reason, .. } if reason.contains("did not finish")),
            "got {err:?}"
        );
        assert!(!ws.path_for("id-1", "MT-1").exists());
        assert!(!branch_exists(&repo, &GitWorktreeWorkspace::pretty_branch("MT-1", "")));

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Without delivery there is no remote to fetch: the local base is the start point.
    #[test]
    fn with_no_remote_a_new_branch_starts_from_the_local_base() {
        let root = tmp_root("wt-local-base");
        let repo = tmp_repo("wt-local-base");
        let base = head_of(&repo);
        git_out(&repo, &["checkout", "-q", "-b", "probe/elsewhere"]).unwrap();
        commit_in(&repo, "probe.txt", "operator's own probe");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().branching_from(
            "main",
            None,
            FetchLock::default(),
        );

        let p = ws.prepare("id-1", "MT-1").unwrap();
        assert_eq!(head_of(&p.path), base);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Push a new commit onto the bare remote's `main` from a clone `repo` has never fetched,
    /// and return it.
    fn advance_remote(bare: &Path, tag: &str) -> String {
        let clone = tmp_root(&format!("clone-{tag}"));
        std::fs::remove_dir_all(&clone).ok();
        // `-b main`: the bare remote's HEAD names `init.defaultBranch`, `master` on CI, and a
        // clone of a HEAD that does not exist commits an orphan that cannot fast-forward `main`.
        let (bare, clone_str) = (bare.to_str().unwrap(), clone.to_str().unwrap());
        git_out(Path::new("."), &["clone", "-q", "-b", "main", bare, clone_str]).unwrap();
        git_out(&clone, &["config", "user.email", "test@example.com"]).unwrap();
        git_out(&clone, &["config", "user.name", "test"]).unwrap();
        commit_in(&clone, "upstream.txt", "landed on the base meanwhile");
        git_out(&clone, &["push", "-q", "origin", "HEAD:main"]).unwrap();
        let sha = head_of(&clone);
        std::fs::remove_dir_all(&clone).ok();
        sha
    }

    #[test]
    fn a_prepared_worktree_reports_the_branch_its_work_will_land_on() {
        let root = tmp_root("wt-report-branch");
        let repo = tmp_repo("wt-report-branch");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let fresh = ws.prepare("id-1", "MT-1").unwrap();
        let reused = ws.prepare("id-1", "MT-1").unwrap();
        let named = fresh.branch.as_deref().expect("a git worktree always has a branch");
        assert_eq!(named, "crew/MT-1");
        assert_eq!(fresh.branch, reused.branch, "reuse reports the same branch as creation");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn the_branch_the_snapshot_publishes_is_the_one_prepare_checks_out() {
        // `Workspace::branch_for` is what reaches an operator, through the snapshot and
        // `crewctl status`; `Prepared.branch` is what the run actually commits on. Letting
        // those two drift would send a reviewer looking for a ref that was never written —
        // the exact failure the published branch exists to prevent.
        let root = tmp_root("wt-published-branch");
        let repo = tmp_repo("wt-published-branch");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let prepared = ws.prepare("id-1", "MT-1").unwrap();
        assert_eq!(
            ws.branch_for("id-1", "MT-1"),
            prepared.branch,
            "the published branch must be the one the worktree is on"
        );
        // Answered from the name alone, so it survives the run it describes.
        ws.remove("id-1", "MT-1").unwrap();
        assert_eq!(ws.branch_for("id-1", "MT-1"), prepared.branch);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_plain_directory_workspace_reports_no_branch_rather_than_an_invented_one() {
        let root = tmp_root("dir-no-branch");
        let ws = DirWorkspace::new(&root).unwrap();
        assert_eq!(ws.branch_for("id-1", "MT-1"), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_new_branch_is_named_after_the_issue_number_and_title() {
        let root = tmp_root("wt-branch-slug");
        let repo = tmp_repo("wt-branch-slug");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let title = "A pull request is judged on the head crewd just pushed";

        let prepared = ws.prepare_for("id-178", "#178", title, None).unwrap();

        let branch = "crew/178-a-pull-request-is-judged-on-the-head";
        assert_eq!(prepared.branch.as_deref(), Some(branch));
        assert_eq!(ws.branch_for("id-178", "#178").as_deref(), Some(branch));
        assert!(branch_exists(&repo, branch));
        assert!(!branch.contains(crate::model::dispatch_suffix("id-178").as_str()));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn two_issues_whose_names_collide_still_get_distinct_branches() {
        let root = tmp_root("wt-branch-collide");
        let repo = tmp_repo("wt-branch-collide");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let title = "Judge the pushed head";

        let first = ws.prepare_for("id-a", "#178", title, None).unwrap();
        let second = ws.prepare_for("id-b", "#178", title, None).unwrap();

        let pretty = "crew/178-judge-the-pushed-head";
        let hashed = format!("{pretty}-{}", crate::model::dispatch_suffix("id-b"));
        assert_eq!(first.branch.as_deref(), Some(pretty));
        assert_eq!(second.branch.as_deref(), Some(hashed.as_str()));
        assert!(branch_exists(&repo, pretty));
        assert!(branch_exists(&repo, &hashed));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_branch_created_under_the_old_name_is_still_found() {
        let root = tmp_root("wt-branch-legacy");
        let repo = tmp_repo("wt-branch-legacy");
        let legacy = GitWorktreeWorkspace::legacy_branch_name("id-178", "#178");
        assert!(legacy.starts_with("crew/_178-"), "{legacy}");
        git_out(&repo, &["checkout", "-q", "-b", &legacy]).unwrap();
        std::fs::write(repo.join("kept.txt"), b"from the old branch\n").unwrap();
        git_out(&repo, &["add", "kept.txt"]).unwrap();
        git_out(&repo, &["commit", "-q", "-m", "old work"]).unwrap();
        git_out(&repo, &["checkout", "-q", "main"]).unwrap();
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let prepared = ws
            .prepare_for(
                "id-178",
                "#178",
                "A pull request is judged on the head crewd just pushed",
                None,
            )
            .unwrap();

        assert_eq!(prepared.branch.as_deref(), Some(legacy.as_str()));
        assert!(prepared.path.join("kept.txt").exists(), "attached to the old branch");
        assert!(!branch_exists(&repo, "crew/178-a-pull-request-is-judged-on-the-head"));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_title_edit_does_not_rename_an_existing_branch() {
        let root = tmp_root("wt-branch-retitle");
        let repo = tmp_repo("wt-branch-retitle");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let first = ws.prepare_for("id-1", "#178", "Judge the pushed head", None).unwrap();
        let branch = first.branch.clone().unwrap();
        commit_in(&first.path, "work.txt", "the agent output");
        ws.remove("id-1", "#178").unwrap();

        let again = ws.prepare_for("id-1", "#178", "A completely different title", None).unwrap();
        assert_eq!(again.branch.as_deref(), Some(branch.as_str()));
        assert!(again.path.join("work.txt").exists(), "the commits stay on the original branch");
        ws.remove("id-1", "#178").unwrap();

        // The store is the other copy of the name. With the git record gone, a title edit
        // still has to reattach to the branch delivery already has.
        let record = GitWorktreeWorkspace::branch_record_ref("id-1");
        git_out(&repo, &["symbolic-ref", "--delete", &record]).unwrap();
        let from_store =
            ws.prepare_for("id-1", "#178", "Yet another title", Some(&branch)).unwrap();
        assert_eq!(from_store.branch.as_deref(), Some(branch.as_str()));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_title_whose_first_word_exceeds_the_slug_is_cut_at_a_word_boundary() {
        // A token longer than the limit has no boundary inside it. Slicing it would put the
        // partial word #205 rejects into the branch name; skipping it leaves a later word, or
        // no slug at all.
        let long = "A".repeat(50);
        assert_eq!(GitWorktreeWorkspace::title_slug(&long), "");
        assert_eq!(GitWorktreeWorkspace::pretty_branch("#178", &long), "crew/178");
        let later = format!("{long} keeps the later words");
        assert_eq!(GitWorktreeWorkspace::title_slug(&later), "keeps-the-later-words");
        assert_eq!(
            GitWorktreeWorkspace::title_slug(
                "A pull request is judged on the head crewd just pushed"
            ),
            "a-pull-request-is-judged-on-the-head"
        );
    }

    #[test]
    fn a_warm_worktree_checked_out_on_another_issues_branch_is_not_adopted_or_deleted() {
        // Once an issue's worktree is gone, git lets another worktree check out the branch
        // that was kept. Adopting that checkout stores both issues against one ref, and
        // `branch -d` on remove then deletes it whenever the commits are already in HEAD.
        let root = tmp_root("wt-foreign-branch");
        let repo = tmp_repo("wt-foreign-branch");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let other = ws.prepare_for("id-a", "#178", "Judge the pushed head", None).unwrap();
        let other_branch = other.branch.clone().unwrap();
        commit_in(&other.path, "other.txt", "the other issue");
        git_out(&repo, &["merge", "--ff-only", &other_branch]).unwrap();
        git_out(&repo, &["worktree", "remove", "--force", other.path.to_str().unwrap()]).unwrap();
        assert!(branch_exists(&repo, &other_branch), "the retained branch outlives its worktree");

        let mine = ws.prepare_for("id-b", "#179", "A different change", None).unwrap();
        let my_branch = mine.branch.clone().unwrap();
        commit_in(&mine.path, "mine.txt", "this issue");
        git_out(&mine.path, &["switch", "--quiet", &other_branch]).unwrap();

        let removed = ws.remove("id-b", "#179").unwrap();
        assert!(
            branch_exists(&repo, &other_branch),
            "remove must not delete the other issue's branch"
        );
        assert!(branch_exists(&repo, &my_branch), "this issue's own commits still outlive cleanup");
        assert!(!removed.branch_deleted);

        let again = ws.prepare_for("id-b", "#179", "A different change", Some(&my_branch)).unwrap();
        git_out(&again.path, &["switch", "--quiet", &other_branch]).unwrap();
        let continued =
            ws.prepare_for("id-b", "#179", "A different change", Some(&my_branch)).unwrap();
        assert_eq!(continued.branch.as_deref(), Some(my_branch.as_str()));
        let head =
            git_out(&continued.path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).unwrap();
        assert_eq!(head, my_branch);
        assert!(branch_exists(&repo, &other_branch));
        let record = GitWorktreeWorkspace::branch_record_ref("id-b");
        assert_eq!(
            git_out(&repo, &["symbolic-ref", "--quiet", &record]).unwrap(),
            format!("refs/heads/{my_branch}")
        );
        let other_record = GitWorktreeWorkspace::branch_record_ref("id-a");
        assert_eq!(
            git_out(&repo, &["symbolic-ref", "--quiet", &other_record]).unwrap(),
            format!("refs/heads/{other_branch}")
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn a_hostile_identifier_cannot_produce_a_branch_git_refuses() {
        // The identifier reaches the ref namespace now, not just the filesystem, and git's
        // rules there are not the filesystem's: `..`, a trailing `.lock`, a bare `@`.
        let root = tmp_root("wt-branch-refs");
        let repo = tmp_repo("wt-branch-refs");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        for (i, bad) in ["..", "a..b", "x.lock", "@", "-dash", "a/../b"].iter().enumerate() {
            let id = format!("id-{i}");
            ws.prepare(&id, bad).expect("a hostile identifier must not fail preparation");
            let branch = GitWorktreeWorkspace::pretty_branch(bad, "");
            assert!(branch_exists(&repo, &branch), "git refused the branch name {branch}");
        }

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Run git inside an existing worktree, standing in for a second orchestrator started
    /// there with `repo = "."` — the shape a dispatched agent produces when it runs the daemon
    /// from its own checkout with the default config.
    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn registered_count(repo: &Path) -> usize {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.starts_with("worktree ")).count()
    }

    #[test]
    fn an_orchestrator_cannot_be_started_inside_another_runs_worktree() {
        // Reproduces #29: the agent for #24 ran `cargo run` from its own worktree, so
        // `workspace.repo = "."` was a linked worktree and `workspace.root` resolved inside it.
        // Both spellings of that mistake must be refused, and the ordinary shape — root beside
        // or inside the *main* checkout — must not be.
        let root = tmp_root("wt-no-nest");
        let repo = tmp_repo("wt-no-nest");
        let outer =
            GitWorktreeWorkspace::new(&root, &repo).unwrap().prepare("id-1", "MT-1").unwrap().path;

        // repo inside a linked worktree, root inside it too: the exact #29 configuration.
        let nested_root = outer.join(".crew/workspaces");
        let err = GitWorktreeWorkspace::new(&nested_root, &outer).err().expect("must be refused");
        assert!(
            matches!(err, WorkspaceError::Nested { .. }),
            "expected a nesting refusal, got {err:?}"
        );
        assert_eq!(registered_count(&repo), 2, "a refused start must register nothing");

        // Only the root inside a linked worktree, repo pointed at the main checkout: the
        // directories would still be deleted under a later `remove` of the outer worktree.
        let err = GitWorktreeWorkspace::new(&nested_root, &repo).err().expect("must be refused");
        assert!(
            matches!(err, WorkspaceError::Nested { .. }),
            "expected a nesting refusal, got {err:?}"
        );

        // Only the repo inside a linked worktree, root elsewhere: metadata still nests.
        let elsewhere = tmp_root("wt-no-nest-elsewhere");
        let err = GitWorktreeWorkspace::new(&elsewhere, &outer).err().expect("must be refused");
        assert!(
            matches!(err, WorkspaceError::Nested { .. }),
            "expected a nesting refusal, got {err:?}"
        );

        // The dogfooding shape — root under the main checkout — is not nesting.
        let in_main = repo.join(".crew/workspaces");
        GitWorktreeWorkspace::new(&in_main, &repo)
            .expect("a root inside the main checkout is fine");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&elsewhere).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn removing_a_worktree_reclaims_the_worktrees_nested_inside_it_from_shared_metadata() {
        // What #29 left behind predates the refusal above, so cleanup has to reconcile it: the
        // parent's `worktree remove --force` deletes the nested directories without knowing
        // they were worktrees, leaving registrations that point nowhere and branches those
        // registrations pin. Both must go by the same path that removed the parent — with the
        // merged check intact, so a nested branch that carries commits is kept exactly as the
        // parent's own would be.
        let root = tmp_root("wt-nested-cleanup");
        let repo = tmp_repo("wt-nested-cleanup");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let outer = ws.prepare("id-1", "MT-1").unwrap().path;

        let inner_root = outer.join(".crew/workspaces");
        std::fs::create_dir_all(&inner_root).unwrap();
        let empty = inner_root.join("MT-601");
        let with_work = inner_root.join("MT-602");
        git_in(&outer, &["worktree", "add", "-q", "-B", "crew/MT-601", empty.to_str().unwrap()]);
        git_in(
            &outer,
            &["worktree", "add", "-q", "-B", "crew/MT-602", with_work.to_str().unwrap()],
        );
        commit_in(&with_work, "work.txt", "a nested run's output");
        assert_eq!(registered_count(&repo), 4, "main, outer and two nested");

        ws.remove("id-1", "MT-1").unwrap();

        assert!(!outer.exists());
        assert_eq!(registered_count(&repo), 1, "no registration may outlive its directory");
        assert!(!branch_exists(&repo, "crew/MT-601"), "a nested branch holding nothing goes");
        assert!(branch_exists(&repo, "crew/MT-602"), "a nested branch holding commits stays");
        assert!(
            !branch_exists(&repo, &GitWorktreeWorkspace::pretty_branch("MT-1", "")),
            "the parent's own branch is treated exactly as before"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ---- Publisher -----------------------------------------------------------

    fn git_out(at: &Path, args: &[&str]) -> Result<String, String> {
        let out = Command::new("git").arg("-C").arg(at).args(args).output().unwrap();
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    /// A repo with a bare `origin` it has already pushed `main` to, so a publish has somewhere
    /// real to land and a remote-tracking base to be measured against.
    fn repo_with_remote(tag: &str) -> (PathBuf, PathBuf) {
        let repo = tmp_repo(tag);
        let bare = tmp_root(&format!("bare-{tag}"));
        git_out(&bare, &["init", "-q", "--bare"]).unwrap();
        git_out(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]).unwrap();
        git_out(&repo, &["push", "-q", "origin", "main"]).unwrap();
        (repo, bare)
    }

    #[test]
    fn publish_pushes_the_branch_to_the_remote_and_lists_its_commits_over_the_base() {
        let root = tmp_root("wt-publish");
        let (repo, bare) = repo_with_remote("wt-publish");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first change");
        commit_in(&p.path, "b.txt", "second change");

        let published = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_eq!(published.head_sha, head_of(&p.path));
        assert_eq!(published.commits, vec!["second change", "first change"], "newest first");
        assert_eq!(
            git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap(),
            published.head_sha,
            "the remote must hold exactly the head that was reported"
        );

        // Pushing again with nothing new is idempotent, which delivery relies on.
        let again = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_eq!(again, published);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    /// #64: the orchestrator's push token must not land anywhere the agent sharing the worktree
    /// reads — its `.git/config`, the argv of the push, or a file that outlives the push.
    #[test]
    fn a_push_credential_reaches_git_without_touching_config_argv_or_a_lasting_file() {
        const TOKEN: &str = "ghs_push_token_that_must_not_leak";
        let root = tmp_root("wt-push-cred");
        let (repo, bare) = repo_with_remote("wt-push-cred");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().with_push_credentials(
            Arc::new(crate::credentials::StaticToken::new(TOKEN)),
            bare.to_str().unwrap(),
        );

        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "a change");

        let (args, file) = ws.push_args("origin", &branch, "").unwrap();
        assert!(args.iter().all(|a| !a.contains(TOKEN)), "the token is in argv: {args:?}");
        // What git itself resolves through exactly those arguments is the token, ahead of any
        // helper the operator configured.
        let config: Vec<&str> =
            args.iter().take_while(|a| *a != "push").map(String::as_str).collect();
        let mut fill = Command::new("git")
            .arg("-C")
            .arg(&p.path)
            .args(&config)
            .args(["credential", "fill"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            fill.stdin.take().unwrap().write_all(b"protocol=https\nhost=github.com\n\n").unwrap();
        }
        let out = String::from_utf8(fill.wait_with_output().unwrap().stdout).unwrap();
        assert!(out.contains(&format!("password={TOKEN}")), "{out}");
        let file = file.unwrap();
        let (dir, path) = (file.dir.clone(), file.file.clone());
        assert!(!dir.starts_with(&root) && !dir.starts_with(&repo), "outside every worktree");
        drop(file);
        assert!(!path.exists() && !dir.exists(), "the file lives only as long as the push");

        let published = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_eq!(
            git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap(),
            published.head_sha
        );
        let common = git_out(&p.path, &["rev-parse", "--git-common-dir"]).unwrap();
        let common = p.path.join(common);
        for config in [common.join("config"), p.path.join(".git")] {
            let text = std::fs::read_to_string(&config).unwrap_or_default();
            assert!(!text.contains(TOKEN), "{} holds the token", config.display());
        }
        let leftover = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("crewd-push-"))
            .filter_map(|e| std::fs::read_to_string(e.path().join("credentials")).ok())
            .any(|text| text.contains(TOKEN));
        assert!(!leftover, "a credential file outlived its push");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    /// An explicit SSH `pushurl` (or an SSH host alias) is where `pushInsteadOf` rewriting stops
    /// applying, and the push would go out on the operator's key. With an App credential the
    /// push goes to the URL it was given and the remote's push URL is never consulted — here it
    /// names a host that does not exist, so taking it would fail the push outright. The lease
    /// still holds across a rewrite and still refuses someone else's work.
    #[test]
    fn an_app_push_ignores_the_remotes_push_url_and_keeps_its_lease() {
        let root = tmp_root("wt-push-url");
        let (repo, bare) = repo_with_remote("wt-push-url");
        git_out(&repo, &["config", "remote.origin.pushurl", "ssh://git@crew-test.invalid/o/r.git"])
            .unwrap();
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().with_push_credentials(
            Arc::new(crate::credentials::StaticToken::new("tok")),
            bare.to_str().unwrap(),
        );
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        let on_remote = |b: &str| git_out(&bare, &["rev-parse", &format!("refs/heads/{b}")]);

        commit_in(&p.path, "a.txt", "first");
        let first = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_eq!(on_remote(&branch).unwrap(), first.head_sha, "pushed to the given URL");

        // A rewrite of our own branch is forced over our own earlier push.
        git_out(&p.path, &["commit", "-q", "--amend", "-m", "first, reworded"]).unwrap();
        let second = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_ne!(second.head_sha, first.head_sha);
        assert_eq!(on_remote(&branch).unwrap(), second.head_sha);

        // Someone else moves the remote branch: the lease refuses rather than overwrites.
        let other = tmp_root("wt-push-url-other");
        git_out(&other, &["clone", "-q", bare.to_str().unwrap(), "."]).unwrap();
        git_out(&other, &["checkout", "-q", &branch]).unwrap();
        // Its own identity, as `tmp_repo` gives each repo: CI has no global one.
        git_out(&other, &["config", "user.name", "someone else"]).unwrap();
        git_out(&other, &["config", "user.email", "someone@example.invalid"]).unwrap();
        commit_in(&other, "theirs.txt", "not ours");
        git_out(&other, &["push", "-q", "origin", &branch]).unwrap();
        commit_in(&p.path, "b.txt", "second");
        let refused = ws.publish(&p.path, &branch, "origin", "main").unwrap_err();
        assert!(
            matches!(refused, ForgeError::Permanent(ref m) if m.contains("has moved")),
            "{refused:?}"
        );

        for d in [&root, &repo, &bare, &other] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Review on #174: with an App credential the sync read through the operator's remote,
    /// which a host with only the App's access cannot reach.
    #[test]
    fn an_app_credentialed_sync_reads_the_branch_where_the_push_goes() {
        let root = tmp_root("wt-sync-app");
        let (repo, bare) = repo_with_remote("wt-sync-app");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().with_push_credentials(
            Arc::new(crate::credentials::StaticToken::new("tok")),
            bare.to_str().unwrap(),
        );
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first");
        let first = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        git_out(&repo, &["remote", "set-url", "origin", "/nonexistent/crew-test.git"]).unwrap();

        assert_eq!(
            ws.sync(&p.path, &branch, "origin").unwrap(),
            Synced::Current { remote_head: first.head_sha }
        );

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// #189.
    #[test]
    fn stacked_on_lists_the_remote_with_the_push_credential() {
        let root = tmp_root("wt-stack-app");
        let (repo, bare) = repo_with_remote("wt-stack-app");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().with_push_credentials(
            Arc::new(crate::credentials::StaticToken::new("tok")),
            bare.to_str().unwrap(),
        );
        let lower = ws.prepare("id-1", "MT-1").unwrap();
        commit_in(&lower.path, "lower.txt", "lower");
        let lower_branch = lower.branch.clone().unwrap();
        ws.publish(&lower.path, &lower_branch, "origin", "main").unwrap();
        let upper = ws.prepare("id-2", "MT-2").unwrap();
        git_out(&upper.path, &["merge", "-q", "--ff-only", &lower_branch]).unwrap();
        commit_in(&upper.path, "upper.txt", "upper");
        let upper_branch = upper.branch.clone().unwrap();
        git_out(&repo, &["remote", "set-url", "origin", "/nonexistent/crew-test.git"]).unwrap();

        let candidates = vec![lower_branch.clone()];
        assert_eq!(
            ws.stacked_on(&upper.path, &upper_branch, "origin", "main", &candidates).unwrap(),
            Some(lower_branch)
        );

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// #189.
    #[test]
    fn publish_fetches_the_base_with_the_push_credential() {
        let root = tmp_root("wt-publish-base-app");
        let (repo, bare) = repo_with_remote("wt-publish-base-app");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap().with_push_credentials(
            Arc::new(crate::credentials::StaticToken::new("tok")),
            bare.to_str().unwrap(),
        );
        // The remote's `main` lands a commit the worktree is built on, while the local `main`
        // and its remote-tracking ref stay behind it.
        commit_in(&repo, "landed.txt", "landed on the remote's main");
        git_out(&repo, &["push", "-q", "origin", "main"]).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        git_out(&repo, &["reset", "-q", "--hard", "HEAD~1"]).unwrap();
        git_out(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]).unwrap();
        commit_in(&p.path, "a.txt", "the branch's own");
        git_out(&repo, &["remote", "set-url", "origin", "/nonexistent/crew-test.git"]).unwrap();

        let published = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_eq!(published.commits, vec!["the branch's own"]);

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Review on #174: a lease outliving its remote branch refused the push recreating it.
    #[test]
    fn a_branch_deleted_on_the_remote_can_be_pushed_again() {
        let root = tmp_root("wt-sync-deleted");
        let (repo, bare) = repo_with_remote("wt-sync-deleted");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first");
        ws.publish(&p.path, &branch, "origin", "main").unwrap();
        git_out(&bare, &["branch", "-D", &branch]).unwrap();
        commit_in(&p.path, "b.txt", "second");

        assert_eq!(ws.sync(&p.path, &branch, "origin").unwrap(), Synced::Absent);
        let again = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_eq!(
            git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap(),
            again.head_sha
        );

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn a_push_refused_for_authentication_is_retried_once_on_a_fresh_token() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct Rotating(AtomicU32);
        impl Credentials for Rotating {
            fn token(&self) -> Result<String, crate::credentials::CredentialError> {
                Ok(format!("tok-{}", self.0.load(Ordering::SeqCst)))
            }
            fn invalidate(&self) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                true
            }
        }

        let root = tmp_root("wt-push-reauth");
        let (repo, bare) = repo_with_remote("wt-push-reauth");
        let creds = Arc::new(Rotating(AtomicU32::new(0)));
        let ws = GitWorktreeWorkspace::new(&root, &repo)
            .unwrap()
            .with_push_credentials(creds.clone(), bare.to_str().unwrap());
        let refused = || WorkspaceError::Git {
            args: "push".into(),
            stderr: "fatal: Authentication failed for 'https://github.com/o/r.git/'".into(),
        };

        // Revoked early: the retry goes out on the re-minted token and succeeds.
        let attempts = AtomicU32::new(0);
        let pushed = ws.retry_on_auth(|| {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            let token = creds.token().unwrap();
            Ok(if token == "tok-1" { Ok(format!("pushed on attempt {n}")) } else { Err(refused()) })
        });
        assert_eq!(pushed.unwrap().unwrap(), "pushed on attempt 1");

        // Refused again: exactly one retry, and the refusal is what comes back.
        let attempts = AtomicU32::new(0);
        let pushed = ws.retry_on_auth(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Ok(Err(refused()))
        });
        assert!(pushed.unwrap().is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "one retry, not a loop");

        // Anything but an authentication refusal is not retried.
        let attempts = AtomicU32::new(0);
        let _ = ws.retry_on_auth(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Ok(Err(WorkspaceError::Git { args: "push".into(), stderr: "stale info".into() }))
        });
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    #[test]
    fn an_authentication_refusal_that_survived_its_retry_is_permanent() {
        let text = "git push failed: fatal: Authentication failed for 'https://github.com/o/r/'";
        assert!(matches!(classify_push("origin", "crew/x", text), ForgeError::Permanent(_)));
        let text = "remote: Invalid username or token. Password authentication is not supported";
        assert!(matches!(classify_push("origin", "crew/x", text), ForgeError::Permanent(_)));
        assert!(
            classify_push("origin", "crew/x", "Could not resolve host: github.com").retryable(),
            "the network is still transient"
        );
    }

    #[test]
    fn a_read_refused_for_authentication_after_its_retry_is_permanent() {
        let git =
            |stderr: &str| WorkspaceError::Git { args: "ls-remote".into(), stderr: stderr.into() };
        let refused = git("fatal: Authentication failed for 'https://github.com/o/r/'");
        assert!(matches!(classify_read("listing".into(), &refused), ForgeError::Permanent(_)));
        let offline = git("Could not resolve host: github.com");
        assert!(classify_read("listing".into(), &offline).retryable(), "the network is transient");
    }

    #[test]
    fn without_push_credentials_the_push_is_the_plain_one_it_always_was() {
        let root = tmp_root("wt-push-plain");
        let (repo, bare) = repo_with_remote("wt-push-plain");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let (args, file) = ws.push_args("origin", "crew/x", "").unwrap();
        assert!(file.is_none());
        assert_eq!(
            args,
            ["push", "--force-with-lease=refs/heads/crew/x:", "--set-upstream", "origin", "crew/x"]
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    #[test]
    fn a_branch_carrying_nothing_over_its_base_publishes_an_empty_commit_list() {
        let root = tmp_root("wt-publish-empty");
        let (repo, bare) = repo_with_remote("wt-publish-empty");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap();
        let published =
            ws.publish(&p.path, p.branch.as_deref().unwrap(), "origin", "main").unwrap();
        assert!(published.commits.is_empty(), "nothing to open a pull request over");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    /// Finding 6 on #47. The gate rebases a published branch before every re-delivery, so the
    /// second push of a branch is routinely a history rewrite; a plain push refuses it and the
    /// pull request never sees the fix. Forcing is right only because the branch is the
    /// orchestrator's — the second half of this test is what keeps that from becoming forcing
    /// over anyone's: a remote that moved under us is a refusal, with the reason, not a push.
    #[test]
    fn a_rebased_branch_is_pushed_over_its_own_history_but_never_over_someone_elses() {
        let root = tmp_root("wt-publish-lease");
        let (repo, bare) = repo_with_remote("wt-publish-lease");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first change");
        let first = ws.publish(&p.path, &branch, "origin", "main").unwrap();

        // The gate's rebase, in miniature: the same change under a rewritten commit.
        git_out(&p.path, &["commit", "--amend", "-q", "-m", "first change, rebased"]).unwrap();
        assert_ne!(head_of(&p.path), first.head_sha, "the history was rewritten");
        let second = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_eq!(second.head_sha, head_of(&p.path));
        assert_eq!(
            git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap(),
            second.head_sha,
            "the remote must follow the orchestrator's own rewrite"
        );
        assert_eq!(second.commits, vec!["first change, rebased"]);

        // Somebody else pushes to the branch from a clone this repository has never fetched.
        let other = tmp_root("wt-publish-lease-other");
        std::fs::remove_dir_all(&other).ok();
        git_out(
            &std::env::temp_dir(),
            &["clone", "-q", bare.to_str().unwrap(), other.to_str().unwrap()],
        )
        .unwrap();
        git_out(&other, &["config", "user.email", "test@example.com"]).unwrap();
        git_out(&other, &["config", "user.name", "test"]).unwrap();
        git_out(&other, &["checkout", "-q", &branch]).unwrap();
        commit_in(&other, "theirs.txt", "a reviewer's own commit");
        git_out(&other, &["push", "-q", "origin", &branch]).unwrap();
        let theirs = head_of(&other);

        commit_in(&p.path, "b.txt", "second change");
        let err = ws.publish(&p.path, &branch, "origin", "main").unwrap_err();
        assert!(
            matches!(err, ForgeError::Permanent(_)),
            "a lease failure is a real conflict: {err}"
        );
        assert!(err.to_string().contains("did not push"), "and says whose work stopped it: {err}");
        assert_eq!(
            git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap(),
            theirs,
            "their commit must still be on the remote"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&bare).ok();
        std::fs::remove_dir_all(&other).ok();
    }

    /// A clone of `bare` with `branch` checked out, standing in for the operator's session.
    fn someone_else(bare: &Path, branch: &str, tag: &str) -> PathBuf {
        let other = tmp_root(tag);
        std::fs::remove_dir_all(&other).ok();
        git_out(
            &std::env::temp_dir(),
            &["clone", "-q", bare.to_str().unwrap(), other.to_str().unwrap()],
        )
        .unwrap();
        git_out(&other, &["config", "user.email", "test@example.com"]).unwrap();
        git_out(&other, &["config", "user.name", "test"]).unwrap();
        git_out(&other, &["checkout", "-q", branch]).unwrap();
        other
    }

    /// #163: the bare lease came from `refs/remotes/origin/<branch>`, so once anything in
    /// `workspace.repo` had fetched, the push replaced the operator's commit instead of refusing.
    #[test]
    fn the_next_push_never_replaces_a_commit_the_worktree_has_not_seen() {
        let root = tmp_root("wt-lease-fetched");
        let (repo, bare) = repo_with_remote("wt-lease-fetched");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first change");
        ws.publish(&p.path, &branch, "origin", "main").unwrap();

        let other = someone_else(&bare, &branch, "wt-lease-fetched-other");
        commit_in(&other, "theirs.txt", "the operator's commit");
        git_out(&other, &["push", "-q", "origin", &branch]).unwrap();
        let theirs = head_of(&other);
        git_out(&repo, &["fetch", "-q", "origin"]).unwrap();

        commit_in(&p.path, "b.txt", "second change");
        let err = ws.publish(&p.path, &branch, "origin", "main").unwrap_err();
        assert!(matches!(err, ForgeError::Permanent(_)), "refused, not forced: {err}");
        let remote_head = |b: &str| git_out(&bare, &["rev-parse", &format!("refs/heads/{b}")]);
        assert_eq!(remote_head(&branch).unwrap(), theirs, "their commit is still the head");

        // Taken in, the same push carries it.
        assert!(matches!(
            ws.sync(&p.path, &branch, "origin").unwrap(),
            Synced::Advanced { merged: true, .. }
        ));
        ws.publish(&p.path, &branch, "origin", "main").unwrap();
        let pushed = remote_head(&branch).unwrap();
        assert!(git_out(&bare, &["merge-base", "--is-ancestor", &theirs, &pushed]).is_ok());

        for d in [&root, &repo, &bare, &other] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn a_branch_nobody_else_moved_syncs_as_absent_then_current() {
        let root = tmp_root("wt-sync-current");
        let (repo, bare) = repo_with_remote("wt-sync-current");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first change");

        assert_eq!(ws.sync(&p.path, &branch, "origin").unwrap(), Synced::Absent);
        let published = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        commit_in(&p.path, "b.txt", "a later, unpushed change");
        let head = head_of(&p.path);
        assert_eq!(
            ws.sync(&p.path, &branch, "origin").unwrap(),
            Synced::Current { remote_head: published.head_sha }
        );
        assert_eq!(head_of(&p.path), head, "the worktree is left alone");

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Fast-forward when the worktree has nothing of its own, so the operator's commit id — what
    /// a reviewer saw — is the branch's head, with no merge commit on top.
    #[test]
    fn a_worktree_with_nothing_of_its_own_fast_forwards_to_the_remote() {
        let root = tmp_root("wt-sync-ff");
        let (repo, bare) = repo_with_remote("wt-sync-ff");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first change");
        ws.publish(&p.path, &branch, "origin", "main").unwrap();

        let other = someone_else(&bare, &branch, "wt-sync-ff-other");
        commit_in(&other, "theirs.txt", "the operator's commit");
        git_out(&other, &["push", "-q", "origin", &branch]).unwrap();
        let theirs = head_of(&other);

        assert_eq!(
            ws.sync(&p.path, &branch, "origin").unwrap(),
            Synced::Advanced { remote_head: theirs.clone(), merged: false }
        );
        assert_eq!(head_of(&p.path), theirs);

        for d in [&root, &repo, &bare, &other] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// A conflict leaves the worktree as the agent had it, and the lease where it was, so the
    /// push after it still refuses.
    #[test]
    fn a_conflicting_remote_branch_is_aborted_named_and_never_pushed_over() {
        let root = tmp_root("wt-sync-conflict");
        let (repo, bare) = repo_with_remote("wt-sync-conflict");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first change");
        ws.publish(&p.path, &branch, "origin", "main").unwrap();

        let other = someone_else(&bare, &branch, "wt-sync-conflict-other");
        commit_in(&other, "a.txt", "their version");
        git_out(&other, &["push", "-q", "origin", &branch]).unwrap();
        let theirs = head_of(&other);
        commit_in(&p.path, "a.txt", "the agent's version");
        let mine = head_of(&p.path);

        assert_eq!(
            ws.sync(&p.path, &branch, "origin").unwrap(),
            Synced::Conflict { remote_head: theirs.clone(), paths: vec!["a.txt".into()] }
        );
        assert_eq!(head_of(&p.path), mine, "the branch is where the agent left it");
        assert!(
            git_out(&p.path, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_err(),
            "no merge is left in progress"
        );
        assert!(ws.publish(&p.path, &branch, "origin", "main").is_err());
        assert_eq!(
            git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap(),
            theirs
        );

        for d in [&root, &repo, &bare, &other] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// #227: the gate rebases a delivered branch onto a base that has moved, rewriting every
    /// commit. The remote still holds crewd's own pre-rebase push, the head the lease recorded,
    /// which is not an ancestor of the rewrite. `sync` leaves that branch alone, and the push
    /// replaces the earlier head. Merging those commits back in is the conflict #191 was handed
    /// with its own push.
    #[test]
    fn a_branch_the_gate_rebased_onto_a_moved_base_replaces_crewds_own_earlier_push() {
        let root = tmp_root("wt-rebase-own");
        let (repo, bare) = repo_with_remote("wt-rebase-own");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "the agent's change");
        let first = ws.publish(&p.path, &branch, "origin", "main").unwrap();

        commit_in(&repo, "base.txt", "base moved");
        git_out(&repo, &["push", "-q", "origin", "main"]).unwrap();
        git_out(&p.path, &["fetch", "-q", "origin", "main"]).unwrap();
        git_out(&p.path, &["rebase", "-q", "FETCH_HEAD"]).unwrap();
        let rebased = head_of(&p.path);
        assert_ne!(rebased, first.head_sha, "the gate rewrote the delivered commits");

        assert_eq!(
            ws.sync(&p.path, &branch, "origin").unwrap(),
            Synced::Current { remote_head: first.head_sha.clone() },
            "the remote head is crewd's own push, so there is nothing to merge"
        );
        assert_eq!(
            head_of(&p.path),
            rebased,
            "sync must leave the rebased branch as the gate left it"
        );

        let published = ws.publish(&p.path, &branch, "origin", "main").unwrap();
        assert_eq!(published.head_sha, rebased);
        let remote = git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap();
        assert_eq!(remote, rebased, "the push replaces crewd's own earlier head");
        assert!(
            git_out(&bare, &["merge-base", "--is-ancestor", &first.head_sha, &remote]).is_err(),
            "the earlier push was replaced, not merged back in"
        );
        let base = git_out(&bare, &["rev-parse", "refs/heads/main"]).unwrap();
        assert!(
            git_out(&bare, &["merge-base", "--is-ancestor", &base, &remote]).is_ok(),
            "the replaced branch still sits on the base the gate rebased onto"
        );
        let parents = git_out(&bare, &["rev-list", "--parents", "-n", "1", &remote]).unwrap();
        assert_eq!(parents.split_whitespace().count(), 2, "the replaced head is not a merge");

        for d in [&root, &repo, &bare] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// #269 syncs a worktree as the agent left it, before the gate has checked it. One left
    /// mid-merge keeps its merge, so the gate still finds it and reports it `Stuck`.
    #[test]
    fn a_worktree_left_mid_merge_is_not_synced_and_keeps_its_merge() {
        let root = tmp_root("wt-sync-mid-merge");
        let (repo, bare) = repo_with_remote("wt-sync-mid-merge");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first change");
        ws.publish(&p.path, &branch, "origin", "main").unwrap();

        let other = someone_else(&bare, &branch, "wt-sync-mid-merge-other");
        commit_in(&other, "theirs.txt", "a commit someone else pushed");
        git_out(&other, &["push", "-q", "origin", &branch]).unwrap();

        git_out(&p.path, &["checkout", "-q", "-b", "side", "HEAD~1"]).unwrap();
        commit_in(&p.path, "a.txt", "the side's version");
        git_out(&p.path, &["checkout", "-q", &branch]).unwrap();
        assert!(git_out(&p.path, &["merge", "-q", "side"]).is_err(), "the agent's merge stops");

        assert!(matches!(ws.sync(&p.path, &branch, "origin"), Err(ForgeError::Permanent(_))));
        assert!(
            git_out(&p.path, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_ok(),
            "the agent's merge is still in progress for the gate to find"
        );
        assert!(!p.path.join("theirs.txt").exists(), "nothing was merged over it");

        for d in [&root, &repo, &bare, &other] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// Review on #271: git merges a commit touching other files over tracked edits, moving
    /// `HEAD` before the gate has reported those edits. Refused, the worktree stays as left.
    #[test]
    fn a_worktree_with_uncommitted_tracked_edits_is_not_synced() {
        let root = tmp_root("wt-sync-dirty");
        let (repo, bare) = repo_with_remote("wt-sync-dirty");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "first change");
        ws.publish(&p.path, &branch, "origin", "main").unwrap();

        let other = someone_else(&bare, &branch, "wt-sync-dirty-other");
        commit_in(&other, "theirs.txt", "a commit someone else pushed");
        git_out(&other, &["push", "-q", "origin", &branch]).unwrap();

        std::fs::write(p.path.join("a.txt"), "an edit the agent never committed").unwrap();
        let head = head_of(&p.path);

        assert!(matches!(ws.sync(&p.path, &branch, "origin"), Err(ForgeError::Permanent(_))));
        assert_eq!(head_of(&p.path), head, "HEAD did not move under the edit");
        assert!(!p.path.join("theirs.txt").exists(), "nothing was merged under it");

        for d in [&root, &repo, &bare, &other] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// #227 stops at crewd's own head. A commit someone else pushed after that — a head the lease
    /// does not name — is still merged in, including once the gate has rewritten the worktree so
    /// ancestry alone can no longer tell the two apart (#163).
    #[test]
    fn a_commit_someone_else_pushed_after_crewd_is_still_merged_in() {
        let root = tmp_root("wt-rebase-theirs");
        let (repo, bare) = repo_with_remote("wt-rebase-theirs");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();
        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "the agent's change");
        ws.publish(&p.path, &branch, "origin", "main").unwrap();

        let other = someone_else(&bare, &branch, "wt-rebase-theirs-other");
        commit_in(&other, "theirs.txt", "a commit someone else pushed");
        git_out(&other, &["push", "-q", "origin", &branch]).unwrap();
        let theirs = head_of(&other);

        commit_in(&repo, "base.txt", "base moved");
        git_out(&repo, &["push", "-q", "origin", "main"]).unwrap();
        git_out(&p.path, &["fetch", "-q", "origin", "main"]).unwrap();
        git_out(&p.path, &["rebase", "-q", "FETCH_HEAD"]).unwrap();

        assert_eq!(
            ws.sync(&p.path, &branch, "origin").unwrap(),
            Synced::Advanced { remote_head: theirs.clone(), merged: true }
        );
        assert!(p.path.join("theirs.txt").exists(), "their commit was merged in");
        assert!(p.path.join("base.txt").exists(), "the rebase onto the moved base was kept");
        assert!(
            git_out(&p.path, &["merge-base", "--is-ancestor", &theirs, "HEAD"]).is_ok(),
            "their commit is an ancestor of the branch, not something the push will replace"
        );

        ws.publish(&p.path, &branch, "origin", "main").unwrap();
        let remote = git_out(&bare, &["rev-parse", &format!("refs/heads/{branch}")]).unwrap();
        assert!(git_out(&bare, &["merge-base", "--is-ancestor", &theirs, &remote]).is_ok());
        let base = git_out(&bare, &["rev-parse", "refs/heads/main"]).unwrap();
        assert!(git_out(&bare, &["merge-base", "--is-ancestor", &base, &remote]).is_ok());

        for d in [&root, &repo, &bare, &other] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn stacked_on_names_the_nearest_branch_under_the_work_and_ignores_ones_already_in_the_base() {
        let root = tmp_root("wt-stack");
        let (repo, bare) = repo_with_remote("wt-stack");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        // Lower carries a commit over main; upper is built on top of lower; sibling sits on
        // main by itself; merged is a branch main already contains. All of them are on the
        // remote, so this test is about ancestry alone.
        let lower = ws.prepare("id-1", "MT-1").unwrap();
        commit_in(&lower.path, "lower.txt", "lower");
        let lower_branch = lower.branch.clone().unwrap();
        ws.publish(&lower.path, &lower_branch, "origin", "main").unwrap();

        let upper = ws.prepare("id-2", "MT-2").unwrap();
        git_out(&upper.path, &["merge", "-q", "--ff-only", &lower_branch]).unwrap();
        commit_in(&upper.path, "upper.txt", "upper");
        let upper_branch = upper.branch.clone().unwrap();
        ws.publish(&upper.path, &upper_branch, "origin", "main").unwrap();

        let sibling = ws.prepare("id-3", "MT-3").unwrap();
        commit_in(&sibling.path, "sibling.txt", "sibling");
        let sibling_branch = sibling.branch.clone().unwrap();
        ws.publish(&sibling.path, &sibling_branch, "origin", "main").unwrap();

        let merged = ws.prepare("id-4", "MT-4").unwrap();
        let merged_branch = merged.branch.clone().unwrap();
        ws.publish(&merged.path, &merged_branch, "origin", "main").unwrap();

        let candidates = vec![lower_branch.clone(), sibling_branch.clone(), merged_branch];

        assert_eq!(
            ws.stacked_on(&upper.path, &upper_branch, "origin", "main", &candidates).unwrap(),
            Some(lower_branch.clone()),
            "upper's work sits on lower"
        );
        assert_eq!(
            ws.stacked_on(&sibling.path, &sibling_branch, "origin", "main", &candidates).unwrap(),
            None,
            "a branch straight off main is not stacked, even with merged-in candidates around"
        );

        // A third storey: the nearest branch wins, not the lowest.
        let top = ws.prepare("id-5", "MT-5").unwrap();
        git_out(&top.path, &["merge", "-q", "--ff-only", &upper_branch]).unwrap();
        commit_in(&top.path, "top.txt", "top");
        let all = vec![lower_branch, upper_branch.clone(), sibling_branch];
        assert_eq!(
            ws.stacked_on(&top.path, top.branch.as_deref().unwrap(), "origin", "main", &all)
                .unwrap(),
            Some(upper_branch)
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    /// Finding 2 on #47. The pull request is opened against the remote's branches, so a lower
    /// branch that exists only locally — still running, or finished and not yet pushed — is
    /// a `422` from the provider and a handoff for the upper one. Not a base, however plainly
    /// the work sits on it; and a base once it has been pushed.
    #[test]
    fn a_stack_candidate_the_remote_does_not_have_is_not_selected_as_a_base() {
        let root = tmp_root("wt-stack-unpushed");
        let (repo, bare) = repo_with_remote("wt-stack-unpushed");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let lower = ws.prepare("id-1", "MT-1").unwrap();
        commit_in(&lower.path, "lower.txt", "lower");
        let lower_branch = lower.branch.clone().unwrap();

        let upper = ws.prepare("id-2", "MT-2").unwrap();
        git_out(&upper.path, &["merge", "-q", "--ff-only", &lower_branch]).unwrap();
        commit_in(&upper.path, "upper.txt", "upper");
        let upper_branch = upper.branch.clone().unwrap();

        let candidates = vec![lower_branch.clone()];
        assert_eq!(
            ws.stacked_on(&upper.path, &upper_branch, "origin", "main", &candidates).unwrap(),
            None,
            "lower is under upper locally, but the remote has never seen it"
        );

        ws.publish(&lower.path, &lower_branch, "origin", "main").unwrap();
        assert_eq!(
            ws.stacked_on(&upper.path, &upper_branch, "origin", "main", &candidates).unwrap(),
            Some(lower_branch),
            "once pushed, the same branch is the base"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    /// Finding 5 on #47. An acceptance names the commit that resolved the comment, and the
    /// delivered branch is what that claim is checked against: a commit on the base only, a
    /// sha that resolves to nothing, or a word that is no sha at all is not an acceptance.
    #[test]
    fn an_acceptance_is_believed_only_for_a_commit_the_delivered_branch_carries() {
        let root = tmp_root("wt-carries");
        let repo = tmp_repo("wt-carries");
        let ws = GitWorktreeWorkspace::new(&root, &repo).unwrap();

        let p = ws.prepare("id-1", "MT-1").unwrap();
        let branch = p.branch.clone().unwrap();
        commit_in(&p.path, "a.txt", "the fix");
        let on_branch = head_of(&p.path);
        let abbreviated = on_branch[..7].to_string();
        // A real commit that is not on the branch: main moves on without it.
        std::fs::write(repo.join("main.txt"), b"elsewhere").unwrap();
        git_out(&repo, &["add", "main.txt"]).unwrap();
        git_out(&repo, &["commit", "-q", "-m", "on main only"]).unwrap();
        let on_main = git_out(&repo, &["rev-parse", "HEAD"]).unwrap();

        assert!(ws.carries(&p.path, &branch, &on_branch).unwrap());
        assert!(ws.carries(&p.path, &branch, &abbreviated).unwrap(), "abbreviated is still it");
        assert!(!ws.carries(&p.path, &branch, &on_main).unwrap(), "a commit the branch lacks");
        assert!(!ws.carries(&p.path, &branch, "deadbeefdeadbeef").unwrap(), "resolves to nothing");
        assert!(!ws.carries(&p.path, &branch, "fixed").unwrap(), "not a commit at all");
        assert!(!ws.carries(&p.path, &branch, "HEAD").unwrap(), "a ref is not a commit named");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&repo).ok();
    }
}
