//! One suite of scenarios, each run against [`FakeForge`] and against the real implementation
//! (#190).
//!
//! Every delivery test in `tests/scheduler.rs` runs against the fake, so a fake that answers
//! differently from the real forge makes those tests pass for code that fails against GitHub,
//! as the per-branch head on #174 did until a reviewer read it. A case here is written once
//! against the trait, and the backends only do what the trait cannot: make a commit, or give
//! the provider's side of the exchange. The `Publisher` half runs against a local bare
//! repository, the `Forge` half against the scripted HTTP fake the tracker adapters share.
//!
//! When the fake fails a case, the fix goes in the fake; a case is never skipped for one
//! backend. Each divergence review finds from here on adds a case.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::json;

use super::fake::FakeForge;
use super::github::GithubForge;
use super::{Forge, PrState, Publisher, PullRequest, PullRequestSpec, Synced};
use crate::tracker::test_http::{FakeHttp, ok};
use crate::workspace::{GitWorktreeWorkspace, Workspace};

// ---- Publisher -------------------------------------------------------------

/// A [`Publisher`] under test, with the setup the trait does not offer.
trait PublisherBackend {
    fn name(&self) -> &'static str;
    fn publisher(&self) -> &dyn Publisher;
    /// A worktree whose branch carries one commit the base does not, and that branch's name.
    fn branch_with_work(&self, tag: &str) -> (PathBuf, String);
}

struct FakePublisher(FakeForge);

impl PublisherBackend for FakePublisher {
    fn name(&self) -> &'static str {
        "FakeForge"
    }

    fn publisher(&self) -> &dyn Publisher {
        &self.0
    }

    fn branch_with_work(&self, tag: &str) -> (PathBuf, String) {
        (PathBuf::from("/nowhere").join(tag), format!("crew/{tag}"))
    }
}

/// [`GitWorktreeWorkspace`] over a throwaway repository with a bare `origin`.
struct GitPublisher {
    dir: PathBuf,
    ws: GitWorktreeWorkspace,
}

impl GitPublisher {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "crew-contract-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let (repo, bare) = (dir.join("repo"), dir.join("origin.git"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare"]);
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "test"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&repo, &["push", "-q", "origin", "main"]);
        let ws = GitWorktreeWorkspace::new(dir.join("worktrees"), &repo).unwrap();
        Self { dir, ws }
    }
}

impl Drop for GitPublisher {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl PublisherBackend for GitPublisher {
    fn name(&self) -> &'static str {
        "GitWorktreeWorkspace"
    }

    fn publisher(&self) -> &dyn Publisher {
        &self.ws
    }

    fn branch_with_work(&self, tag: &str) -> (PathBuf, String) {
        let p = self.ws.prepare(&format!("id-{tag}"), tag).unwrap();
        std::fs::write(p.path.join(format!("{tag}.txt")), tag).unwrap();
        git(&p.path, &["add", "."]);
        git(&p.path, &["commit", "-q", "-m", tag]);
        (p.path, p.branch.unwrap())
    }
}

fn git(at: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(at).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Review on #174: the fake answered a sync with the last head *any* branch published, so a
/// second issue's push read as the first one's branch moving.
fn a_sync_reports_the_head_its_own_branch_last_published(b: &dyn PublisherBackend) {
    let p = b.publisher();
    let (at_a, a) = b.branch_with_work("MT-1");
    let (at_b, other) = b.branch_with_work("MT-2");
    let (at_c, never) = b.branch_with_work("MT-3");
    let first = p.publish(&at_a, &a, "origin", "main").unwrap().head_sha;
    p.publish(&at_b, &other, "origin", "main").unwrap();
    assert_eq!(
        p.sync(&at_a, &a, "origin").unwrap(),
        Synced::Current { remote_head: first },
        "{}",
        b.name()
    );
    assert_eq!(p.sync(&at_c, &never, "origin").unwrap(), Synced::Absent, "{}", b.name());
}

const PUBLISHER_CASES: &[fn(&dyn PublisherBackend)] =
    &[a_sync_reports_the_head_its_own_branch_last_published];

// ---- Forge -----------------------------------------------------------------

/// A [`Forge`] under test, with the provider's side of each exchange. The fake holds that side
/// as its own state, so its hooks do nothing; the real forge is handed the answers GitHub gives.
trait ForgeBackend {
    fn name(&self) -> &'static str;
    fn forge(&self) -> &dyn Forge;
    /// The provider holds no open pull request for `spec.head`, and opens one when asked.
    fn will_open(&self, spec: &PullRequestSpec);
    /// The provider holds `open` for `spec.head`, and moves it to `spec.base` when asked.
    fn holds_open(&self, open: &PullRequest, spec: &PullRequestSpec);
}

struct FakeForgeBackend(FakeForge);

impl ForgeBackend for FakeForgeBackend {
    fn name(&self) -> &'static str {
        "FakeForge"
    }

    fn forge(&self) -> &dyn Forge {
        &self.0
    }

    fn will_open(&self, _spec: &PullRequestSpec) {}

    fn holds_open(&self, _open: &PullRequest, _spec: &PullRequestSpec) {}
}

struct GithubBackend {
    http: Arc<FakeHttp>,
    forge: GithubForge<Arc<FakeHttp>>,
}

impl GithubBackend {
    fn new() -> Self {
        let http = Arc::new(FakeHttp::new());
        Self { forge: GithubForge::new(http.clone(), "o", "r", "t"), http }
    }

    fn pull(number: u64, head_sha: &str, base: &str) -> serde_json::Value {
        json!({
            "number": number,
            "html_url": format!("https://github.com/o/r/pull/{number}"),
            "head": { "sha": head_sha },
            "base": { "ref": base },
            "state": "open",
            "merged": false,
            "requested_reviewers": [],
            "mergeable": true,
        })
    }
}

impl ForgeBackend for GithubBackend {
    fn name(&self) -> &'static str {
        "GithubForge"
    }

    fn forge(&self) -> &dyn Forge {
        &self.forge
    }

    fn will_open(&self, spec: &PullRequestSpec) {
        self.http.push(ok(json!([])));
        self.http.push(ok(Self::pull(7, "abc123", &spec.base)));
    }

    fn holds_open(&self, open: &PullRequest, spec: &PullRequestSpec) {
        self.http.push(ok(json!([Self::pull(open.number, &open.head_sha, &open.base)])));
        if open.base != spec.base {
            self.http.push(ok(Self::pull(open.number, &open.head_sha, &spec.base)));
        }
    }
}

/// The trait's idempotency contract: the scheduler opens after every run that reports done, and
/// records `spec.base` as the truth about the pull request it gets back.
fn a_second_open_for_a_head_finds_the_first_and_points_it_at_the_base_asked_for(
    b: &dyn ForgeBackend,
) {
    let f = b.forge();
    let spec = |base: &str| PullRequestSpec {
        title: "Do the work".into(),
        body: format!("Stacked on {base}."),
        head: "crew/MT-1".into(),
        base: base.into(),
    };
    b.will_open(&spec("main"));
    let first = f.open_pull_request(&spec("main")).unwrap();
    assert_eq!((first.state, first.base.as_str()), (PrState::Open, "main"), "{}", b.name());

    b.holds_open(&first, &spec("crew/MT-0"));
    let moved = f.open_pull_request(&spec("crew/MT-0")).unwrap();
    assert_eq!((moved.number, moved.base.as_str()), (first.number, "crew/MT-0"), "{}", b.name());

    b.holds_open(&moved, &spec("crew/MT-0"));
    let again = f.open_pull_request(&spec("crew/MT-0")).unwrap();
    assert_eq!(again, moved, "{}", b.name());
}

const FORGE_CASES: &[fn(&dyn ForgeBackend)] =
    &[a_second_open_for_a_head_finds_the_first_and_points_it_at_the_base_asked_for];

/// Fresh backends per case, so no case can pass on state another left behind.
#[test]
fn each_forge_contract_case_passes_against_the_fake_and_the_real_forge() {
    for case in PUBLISHER_CASES {
        case(&FakePublisher(FakeForge::new()));
        case(&GitPublisher::new("publisher"));
    }
    for case in FORGE_CASES {
        case(&FakeForgeBackend(FakeForge::new()));
        case(&GithubBackend::new());
    }
}
