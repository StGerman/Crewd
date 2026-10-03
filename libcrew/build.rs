//! Emits the commit the build came from, for the build string in `src/build.rs` (#244).
//!
//! A failure here never fails the build: a source tree with no git, which is every `cargo install`
//! from crates.io, gets no `VERGEN_GIT_*` variables and its build string is the version alone.

use std::path::Path;
use std::process::Command;

use vergen_gitcl::{Emitter, Gitcl};

fn main() {
    let git = Gitcl::builder().sha(true).dirty(false).build();
    if let Err(e) = Emitter::default().add_instructions(&git).and_then(|e| e.emit()) {
        println!("cargo:warning=no commit in the build string: {e}");
    }
    rerun_on_commit();
}

/// vergen watches only `HEAD`, which on a branch holds the branch's name and does not change on
/// a commit or a pull, so the build would keep reporting the sha it was first built at. The
/// branch's ref, `packed-refs` and the index do change. An unstaged edit changes none of them,
/// so every tracked file is watched too, or `-dirty` would lag until the next stage or commit.
/// Tracked files only: `target/` is never among them, and an untracked file does not make the
/// build dirty.
fn rerun_on_commit() {
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let mut watched = vec!["index".to_string(), "packed-refs".to_string()];
    if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(head);
    }
    for name in watched {
        // A path that does not exist reruns the script on every build, so only existing ones.
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", &name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    // A deleted tracked file is kept: it reruns the script on every build while it is missing,
    // which is what keeps `-dirty` true until it is restored or the deletion committed.
    if let Some(top) = git(&["rev-parse", "--show-toplevel"])
        && let Some(files) = git(&["-C", &top, "ls-files"])
    {
        for file in files.lines() {
            println!("cargo:rerun-if-changed={}", Path::new(&top).join(file).display());
        }
    }
}
