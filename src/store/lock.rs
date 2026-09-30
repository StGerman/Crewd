//! One live daemon per store (#217).
//!
//! [`Scheduler::recover`](crate::sched::Scheduler::recover) releases every `running` claim it
//! finds. That is correct only while a single process has the store: a second daemon would
//! take the first one's live claim for a hard kill, remove its worktree and dispatch the issue
//! again. This module is the premise that makes the recovery rule true.
//!
//! The lock is an exclusive `flock` on `<store>.lock`, beside the database, held until this
//! process exits. The kernel releases it when the process dies, including under `SIGKILL`, so
//! a lock whose holder is gone does not block the next start. The database file itself is the
//! wrong target: SQLite locks it with `fcntl`, and on macOS `flock` is implemented as `fcntl`,
//! so a lock on the database would be dropped the next time SQLite closed a connection to it.
//!
//! The held object is the open inode, and the descriptor is close-on-exec. A worker child must
//! not inherit a handle that would keep the lock after this process is gone, and replacing the
//! directory entry (delete the file, create it again) names a different inode than the one
//! held here. Nothing in this module reopens the path after acquire.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::Pid;

/// Exclusive hold on one store. Dropping it releases the hold; a hard kill never runs the
/// destructor, and the kernel releases the underlying `flock` anyway.
#[must_use = "dropping the lock releases the store"]
pub struct StoreLock {
    _guard: Flock<File>,
}

/// Why a second daemon could not take the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreLockError {
    /// Another live process holds the lock. `pid` is the one recorded in the lock file after
    /// that process acquired it.
    #[error("store {store} is held by pid {pid}")]
    Held { store: PathBuf, pid: u32 },
    /// The lock is held, but the file never yielded a pid belonging to a live process. Startup
    /// still stops: proceeding would let two daemons recover each other's claims.
    #[error("store {store} is held, and the holder's pid could not be read")]
    HeldUnreadable { store: PathBuf },
    /// The lock file could not be created or the pid line could not be written.
    #[error("locking store {store}: {source}")]
    Io {
        store: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl StoreLock {
    /// Take the exclusive hold for `store`, or fail naming that path and the pid that has it.
    pub fn acquire(store: &Path) -> Result<Self, StoreLockError> {
        let path = lock_path(store)?;
        // Truncate only after the flock is held. Truncating here would erase the holder's pid
        // in the window before this process learns it lost.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| StoreLockError::Io { store: store.to_path_buf(), source })?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(guard) => held(store, guard),
            Err((file, err)) if contended(err) => lost(store, file),
            Err((_, err)) => Err(io_lock(store, err)),
        }
    }
}

/// The pid is written only after the `flock` succeeds, and a holder can die between a failed
/// attempt and the read. Retry the lock until a live pid is recorded or the attempt succeeds;
/// giving up while the file still names a dead process would refuse a start the kernel has
/// already freed.
fn lost(store: &Path, mut file: File) -> Result<StoreLock, StoreLockError> {
    for _ in 0..64 {
        if let Some(pid) = read_pid(&mut file)
            .map_err(|source| StoreLockError::Io { store: store.to_path_buf(), source })?
            && pid_alive(pid)
        {
            return Err(StoreLockError::Held { store: store.to_path_buf(), pid });
        }
        std::thread::yield_now();
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(guard) => return held(store, guard),
            Err((returned, err)) if contended(err) => file = returned,
            Err((_, err)) => return Err(io_lock(store, err)),
        }
    }
    Err(StoreLockError::HeldUnreadable { store: store.to_path_buf() })
}

fn held(store: &Path, mut guard: Flock<File>) -> Result<StoreLock, StoreLockError> {
    write_pid(&mut guard)
        .map_err(|source| StoreLockError::Io { store: store.to_path_buf(), source })?;
    Ok(StoreLock { _guard: guard })
}

fn io_lock(store: &Path, err: Errno) -> StoreLockError {
    StoreLockError::Io {
        store: store.to_path_buf(),
        source: std::io::Error::from_raw_os_error(err as i32),
    }
}

fn lock_path(store: &Path) -> Result<PathBuf, StoreLockError> {
    let Some(name) = store.file_name() else {
        return Err(StoreLockError::Io {
            store: store.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "store path has no file name",
            ),
        });
    };
    let mut locked = name.to_os_string();
    locked.push(".lock");
    Ok(store.with_file_name(locked))
}

fn contended(err: Errno) -> bool {
    err == Errno::EAGAIN || err == Errno::EWOULDBLOCK
}

fn write_pid(file: &mut File) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    let line = format!("{}\n", std::process::id());
    file.write_all(line.as_bytes())?;
    file.sync_data()?;
    Ok(())
}

fn read_pid(file: &mut File) -> std::io::Result<Option<u32>> {
    let mut buf = [0u8; 32];
    file.seek(SeekFrom::Start(0))?;
    let n = file.read(&mut buf)?;
    let text = std::str::from_utf8(&buf[..n]).unwrap_or("").trim();
    Ok(text.parse().ok().filter(|pid: &u32| *pid > 0))
}

fn pid_alive(pid: u32) -> bool {
    let pid = Pid::from_raw(pid as i32);
    matches!(kill(pid, None), Ok(()) | Err(Errno::EPERM))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::StoreLock;
    use super::lock_path;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::{Pid, write};

    const PROBE: &str = "CREW_STORE_LOCK_PROBE";
    const DB: &str = "CREW_STORE_LOCK_DB";

    #[test]
    fn a_second_daemon_on_the_same_store_refuses_to_start() {
        if std::env::var(PROBE).as_deref() == Ok("refuse") {
            probe_refuse(Path::new(&std::env::var(DB).unwrap()));
        }

        let dir = temp_dir("second");
        let db = dir.join("crew.db");
        let _held = StoreLock::acquire(&db).unwrap();

        let out = reexec(
            "store::lock::tests::a_second_daemon_on_the_same_store_refuses_to_start",
            "refuse",
            &db,
        )
        .output()
        .unwrap();

        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "a second daemon on {db:?} started: {err}");
        let expected = format!("store {} is held by pid {}", db.display(), std::process::id());
        assert!(err.contains(&expected), "stderr: {err}");

        drop(_held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lock_left_by_a_hard_kill_does_not_block_the_next_start() {
        if std::env::var(PROBE).as_deref() == Ok("hold") {
            probe_hold(Path::new(&std::env::var(DB).unwrap()));
        }

        let dir = temp_dir("killed");
        let db = dir.join("crew.db");
        let mut child = reexec(
            "store::lock::tests::a_lock_left_by_a_hard_kill_does_not_block_the_next_start",
            "hold",
            &db,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

        let mut stdout = child.stdout.take().unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = stdout.read(&mut buf).unwrap();
            assert!(n > 0, "holder exited before announcing the lock: {got:?}");
            got.extend_from_slice(&buf[..n]);
            if got.windows(5).any(|w| w == b"held\n") {
                break;
            }
        }

        let holder = child.id();
        kill(Pid::from_raw(holder as i32), Signal::SIGKILL).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "SIGKILL must not look like a clean exit");

        // The directory entry and the dead pid survive. The kernel has already dropped the
        // `flock`, which is the only thing the next start consults.
        let left = std::fs::read_to_string(lock_path(&db).unwrap()).unwrap();
        assert_eq!(left.trim(), holder.to_string(), "the killed process left its pid behind");
        let _next = StoreLock::acquire(&db).unwrap();

        drop(_next);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn probe_refuse(db: &Path) -> ! {
        match StoreLock::acquire(db) {
            Err(e) => {
                let msg = format!("{e}\n");
                emit(&std::io::stderr(), msg.as_bytes());
                std::process::exit(2);
            }
            Ok(_lock) => {
                emit(&std::io::stderr(), b"acquired\n");
                std::process::exit(0);
            }
        }
    }

    fn probe_hold(db: &Path) -> ! {
        let _lock = match StoreLock::acquire(db) {
            Ok(lock) => lock,
            Err(e) => {
                let msg = format!("{e}\n");
                emit(&std::io::stdout(), msg.as_bytes());
                std::process::exit(2);
            }
        };
        emit(&std::io::stdout(), b"held\n");
        loop {
            std::thread::park();
        }
    }

    fn emit(fd: &impl std::os::fd::AsFd, mut buf: &[u8]) {
        while !buf.is_empty() {
            match write(fd, buf) {
                Ok(0) => panic!("writing the probe result returned no bytes"),
                Ok(n) => buf = &buf[n..],
                Err(nix::errno::Errno::EINTR) => {}
                Err(e) => panic!("writing the probe result: {e}"),
            }
        }
    }

    fn reexec(test: &str, probe: &str, db: &Path) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.arg("--exact").arg(test).env(PROBE, probe).env(DB, db);
        cmd
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("crew-store-lock-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
