//! The handoff gate: what stands between an agent saying `Done` and a human being handed the
//! branch.
//!
//! "Agent said done" is not a merge signal. A dispatched run leaves commits on `crew/<key>`
//! green against the base it forked from and unknown against the base it will actually merge
//! into — and the three defects in issue #21 existed only in that combination, where neither
//! agent could have seen them. So a `Done` verdict is not applied until the run's branch has
//! been brought onto the configured base *and* the gate commands have passed on the resulting
//! tree, in that order, in the agent's own worktree. The base has to come in first because a
//! gate run against a stale base answers a question nobody asked. How it comes in depends on
//! the branch: one that already contains the base's tip is left as it is, one that merged an
//! older base has the new one merged in, and any other is rebased. A rebase drops a merge of
//! the base, and with it the conflict resolution an agent made that way (#122, #177).
//!
//! Like the worker, a gate is a process the scheduler supervises rather than a call it makes:
//! `cargo test` in a real worktree runs for minutes, and a tick that blocked on it would stall
//! stall-detection for every other run. [`Gate::start`] returns a handle the scheduler polls;
//! the claim stays held for as long as the handle is open, so nothing can dispatch a second
//! agent onto a worktree the gate is rebasing or merging.
//!
//! The verdict the scheduler derives from the gate is deliberately three-way. A conflict is a
//! human's problem — the branch is restored to where the agent left it and the issue parks
//! `Blocked` naming the paths — unless every conflicted path is one `gate.agent_resolvable`
//! names ([`agent_resolvable`]), where two branches appending to the same list is mechanical
//! and the agent that wrote one of them resolves it (#111). A failing command is the agent's —
//! the output goes back to it in the continuation prompt and the verdict is `Continue`. And a
//! run of consecutive failures, resolvable conflicts included, escalates to `Blocked`, because
//! a gate that can be failed forever is a runaway with a new name.

pub mod fake;
pub mod git;

use std::path::Path;
use std::sync::Arc;

use crate::model::Issue;
use crate::worker::KillResult;

pub use git::GitGate;

/// What the gate found. Produced once, when the handle finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The branch holds nothing the base does not. There is nothing to hand off, so there is
    /// nothing to rebase and nothing worth gating; the run's `Done` stands as it was.
    NoCommits,
    /// Brought onto the base — or already on it — and every command exited zero.
    Passed {
        /// True when a rebase moved commits or a merge brought the base in, false when the
        /// branch was already current. For the log; the scheduler treats both the same.
        base_updated: bool,
    },
    /// The rebase or merge stopped on conflicts and was aborted, so the branch is exactly where
    /// the agent left it: green against the old base, and safe for a human to pick up.
    Conflict {
        paths: Vec<String>,
        /// The commit the gate tried to bring the branch onto, resolved in `workspace.repo`.
        /// Carried because a brief that names the base by ref sends the agent to merge whatever
        /// that ref means *in its worktree* — and with `gate.base` unset that is `HEAD`, the
        /// agent's own branch, a no-op that leaves the conflict to recur.
        base_sha: String,
    },
    /// The rebase or merge stopped and could not be aborted: the worktree is still mid-way, so
    /// it is neither the branch the agent left nor a tree any brief describes. A human's,
    /// always — an agent resumed into it would build on gate state or trip over a second
    /// rebase or merge.
    Stuck { step: String, output: String },
    /// A step failed for a reason the agent can act on: a command exited non-zero, the rebase
    /// or merge was refused (a dirty tree, most likely), or a command could not be started.
    /// `step` names which, and `output` is what it said — bounded, tail-first, because a
    /// failing `cargo test` puts its summary at the end.
    Failed {
        step: String,
        output: String,
        /// Whether the branch is sitting on the base by the time this failed.
        ///
        /// False for every step that runs before the branch is brought onto the base —
        /// resolving it, counting commits — and for a rebase or merge that was refused or
        /// aborted, which puts the branch back where the agent left it. True once the branch
        /// is on the base: rebased onto it, merged onto it, or already containing its tip.
        /// That covers a command failing after a merge as well as after a rebase, and a dirty
        /// tree refused on a branch that already contained the base. A dirty tree refused
        /// before the merge that would get it there is false. The scheduler needs the
        /// distinction because it tells the agent where its work now sits: saying the branch
        /// is on the base when the rebase or merge never landed describes a tree the agent
        /// will not find, and an agent that cannot reconcile the instruction with what it
        /// sees tends to report `Done` again unchanged.
        ///
        /// Distinct from [`Verdict::Passed`]'s `base_updated`, which answers a different
        /// question — whether a rebase or merge *moved* anything. A branch already on the base
        /// is `base_updated: false` there and `on_base: true` here.
        on_base: bool,
    },
}

/// A gate in flight. Dropping the handle does not stop the work — call [`GateHandle::kill`].
pub trait GateHandle: Send + Sync {
    /// The verdict, once there is one.
    fn finished(&self) -> Option<Verdict>;
    /// Which step is running right now, for the dashboard and for the timeout message.
    fn step(&self) -> String;
    /// Stop whatever is running and wait, bounded, for it to actually stop. The same contract
    /// as [`crate::worker::RunHandle::kill`], for the same reason: the caller may delete the
    /// worktree next.
    fn kill(&self, grace_ms: u64) -> KillResult;
}

/// Whether every conflicted path matches one of `patterns`, so the conflict is the agent's.
///
/// False for an empty `paths`: a conflict that names nothing cannot be shown to be confined to
/// anything, and handing it back would hand an agent a brief with no file in it.
pub fn agent_resolvable(paths: &[String], patterns: &[String]) -> bool {
    !paths.is_empty() && paths.iter().all(|p| patterns.iter().any(|g| glob_matches(g, p)))
}

/// `*` within one `/`-separated segment, `**` across any number of them. Hand-rolled because it
/// is these two rules and nothing else, not worth a dependency.
fn glob_matches(pattern: &str, path: &str) -> bool {
    fn segs(pat: &[&str], path: &[&str]) -> bool {
        match pat.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|i| segs(rest, &path[i..])),
            Some((p, rest)) => {
                path.first().is_some_and(|s| seg(p.as_bytes(), s.as_bytes()))
                    && segs(rest, &path[1..])
            }
        }
    }
    fn seg(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (0..=s.len()).any(|i| seg(rest, &s[i..])),
            Some((c, rest)) => s.first() == Some(c) && seg(rest, &s[1..]),
        }
    }
    let pat: Vec<&str> = pattern.trim().split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    segs(&pat, &path)
}

pub trait Gate: Send + Sync {
    /// Begin gating the run whose worktree is at `workspace`. Never fails: a gate that cannot
    /// even start reports that as a [`Verdict::Failed`] through the handle, so the scheduler
    /// has one path to reason about.
    fn start(&self, issue: &Issue, workspace: &Path) -> Arc<dyn GateHandle>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_conflict_is_agent_resolvable_only_when_every_path_matches_a_pattern() {
        let pats = v(&["CLAUDE.md", "docs/**", "src/store/schema.rs", "src/*.toml"]);
        assert!(agent_resolvable(&v(&["CLAUDE.md"]), &pats));
        assert!(agent_resolvable(&v(&["docs/adr/0001-x.md", "docs/a.md"]), &pats));
        assert!(agent_resolvable(&v(&["src/store/schema.rs", "CLAUDE.md"]), &pats));
        assert!(agent_resolvable(&v(&["src/x.toml"]), &pats));
        assert!(!agent_resolvable(&v(&["CLAUDE.md", "src/sched/mod.rs"]), &pats));
        assert!(!agent_resolvable(&v(&["sub/CLAUDE.md"]), &pats), "a name is anchored at the root");
        assert!(!agent_resolvable(&v(&["docsx/a.md"]), &pats), "`docs/**` is the directory");
        assert!(!agent_resolvable(&v(&["src/a/x.toml"]), &pats), "`*` stays in its segment");
        assert!(!agent_resolvable(&[], &pats), "a conflict naming nothing stays a human's");
        assert!(!agent_resolvable(&v(&["CLAUDE.md"]), &[]), "no patterns, no hand-back");
    }
}
