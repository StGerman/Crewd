//! The build string, `<version> (<short sha>[-dirty])`: the one string every surface reports
//! for the running build, so a restart that kept the old binary is visible (#244).
//!
//! It lives here because both binaries print it, and `crewctl` links only this crate. The
//! commit comes from this crate's build script, which builds from the same tree as the daemon.

use std::sync::LazyLock;

static BUILD: LazyLock<String> = LazyLock::new(|| {
    build_string(
        env!("CARGO_PKG_VERSION"),
        option_env!("VERGEN_GIT_SHA"),
        option_env!("VERGEN_GIT_DIRTY") == Some("true"),
    )
});

/// The build string of this binary.
pub fn build() -> &'static str {
    &BUILD
}

/// `None` is a build git reported nothing for, such as one from crates.io: a published version
/// maps to exactly one tag, so the version alone still names the build.
pub fn build_string(version: &str, sha: Option<&str>, dirty: bool) -> String {
    match sha.filter(|s| !s.is_empty()) {
        Some(sha) if dirty => format!("{version} ({sha}-dirty)"),
        Some(sha) => format!("{version} ({sha})"),
        None => version.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_string_is_the_package_version_and_the_commit() {
        assert_eq!(build_string("0.2.0", Some("be41670"), false), "0.2.0 (be41670)");
        assert_eq!(build_string("0.2.0", Some("be41670"), true), "0.2.0 (be41670-dirty)");
        assert_eq!(build_string("0.2.0", None, false), "0.2.0");
        assert_eq!(build_string("0.2.0", None, true), "0.2.0", "dirty means nothing without a sha");
        assert!(build().starts_with(env!("CARGO_PKG_VERSION")), "{}", build());
    }

    /// The wiring, not the format: a build script whose git step failed would still build and
    /// report the version alone, which the test above accepts. In a checkout, which CI is, the
    /// build has to name the commit and the dirty state git reports for this same tree.
    #[test]
    fn a_build_from_a_checkout_names_its_commit_and_dirty_state() {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .output()
                .ok()
                .filter(|o| o.status.success())?;
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        // A source tree with no git, such as a crates.io tarball, has nothing to compare with.
        let Some(sha) = git(&["rev-parse", "--short", "HEAD"]) else { return };
        let dirty = !git(&["status", "--porcelain", "--untracked-files=no"]).unwrap().is_empty();
        assert_eq!(build(), build_string(env!("CARGO_PKG_VERSION"), Some(&sha), dirty));
    }
}
