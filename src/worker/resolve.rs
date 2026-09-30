//! Resolving a worker's `bin` at startup (#218).
//!
//! Without it a daemon started with no `grok` on its `PATH` came up healthy and quarantined every
//! issue its Grok slot took, one spawn failure at a time. A name with a `/` is a path, anything
//! else is searched on `PATH`, and the worker execs the absolute path found here rather than
//! repeating the lookup at spawn. A binary that disappears while running is #216's.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("{0} is not an executable file")]
    NotExecutable(PathBuf),
    #[error("{0:?} was not found on PATH")]
    NotOnPath(String),
}

/// The absolute path `bin` names, given the `PATH` and cwd the daemon was started with.
///
/// Absolute because the worker spawns it from a worktree, with an allowlisted environment that
/// may carry no `PATH`: a relative or bare name would resolve differently there than here.
pub fn resolve_bin(bin: &str, path: Option<&OsStr>) -> Result<PathBuf, ResolveError> {
    let found = if bin.contains('/') {
        let p = PathBuf::from(bin);
        if !is_executable(&p) {
            return Err(ResolveError::NotExecutable(p));
        }
        p
    } else {
        path.into_iter()
            .flat_map(std::env::split_paths)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(bin))
            .find(|p| is_executable(p))
            .ok_or_else(|| ResolveError::NotOnPath(bin.to_string()))?
    };
    std::path::absolute(&found).map_err(|_| ResolveError::NotExecutable(found))
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn a_bare_name_is_found_on_path() {
        let path = std::env::join_paths(["/nonexistent", "/bin"]).unwrap();
        assert_eq!(resolve_bin("sh", Some(&path)), Ok(PathBuf::from("/bin/sh")));
    }

    #[test]
    fn a_bare_name_missing_from_path_is_refused() {
        let path = std::env::join_paths(["/bin"]).unwrap();
        assert_eq!(
            resolve_bin("crew-no-such-binary-218", Some(&path)),
            Err(ResolveError::NotOnPath("crew-no-such-binary-218".into()))
        );
        assert!(resolve_bin("sh", None).is_err());
    }

    #[test]
    fn a_relative_bin_resolves_to_an_absolute_path() {
        let rel = "tests/fixtures/fake_grok/dump_argv.sh";
        assert!(std::fs::metadata(rel).is_ok(), "cargo runs unit tests from the package root");
        assert_eq!(resolve_bin(&format!("./{rel}"), None), Ok(std::path::absolute(rel).unwrap()));
        let path = std::env::join_paths(["tests/fixtures/fake_grok"]).unwrap();
        assert_eq!(resolve_bin("dump_argv.sh", Some(&path)), Ok(std::path::absolute(rel).unwrap()));
    }

    #[test]
    fn a_path_must_name_an_executable_file() {
        let dir = fixtures();
        assert_eq!(resolve_bin(dir.to_str().unwrap(), None), Err(ResolveError::NotExecutable(dir)));
        assert!(resolve_bin("/nonexistent/grok", None).is_err());
        assert_eq!(resolve_bin("/bin/sh", None), Ok(PathBuf::from("/bin/sh")));
    }
}
