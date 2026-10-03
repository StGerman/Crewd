//! The release workflow's trigger and gates (#248). `dist generate` rewrites
//! `.github/workflows/v-release.yml` from `dist-workspace.toml`, so a changed setting or a newer
//! dist can make a pull request build, host or publish, and nothing else would notice before a
//! release went out from an unmerged branch.
//!
//! `Cargo.toml` excludes this file from the published crate, which ships no `.github` (#272).
//!
//! The workflow is read as text, line by line, rather than through a YAML crate: dist's output
//! is regular, and one test does not justify a dependency (docs/coding-guidelines.md).

use std::collections::BTreeMap;
use std::path::Path;

const WORKFLOWS: &str = ".github/workflows";
const RELEASE: &str = ".github/workflows/v-release.yml";

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

/// The lines of the top-level `key:` block, comments and blank lines dropped.
fn block<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    text.lines()
        .skip_while(|l| *l != key)
        .skip(1)
        .take_while(|l| l.is_empty() || l.starts_with(' ') || l.starts_with('#'))
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect()
}

/// The lines of one job in the release workflow, its name line excluded.
fn job<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let head = format!("  {name}:");
    block(text, "jobs:")
        .into_iter()
        .skip_while(|l| *l != head)
        .skip(1)
        .take_while(|l| l.starts_with("   "))
        .collect()
}

struct Job {
    needs: Vec<String>,
    cond: String,
}

fn jobs(text: &str) -> BTreeMap<String, Job> {
    let mut jobs = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut in_needs = false;
    for line in block(text, "jobs:") {
        if let Some(name) = line.strip_prefix("  ").and_then(|l| l.strip_suffix(':'))
            && !name.starts_with(' ')
        {
            jobs.insert(name.to_string(), Job { needs: vec![], cond: String::new() });
            current = Some(name.to_string());
            in_needs = false;
            continue;
        }
        let Some(job) = current.as_ref().and_then(|n| jobs.get_mut(n)) else { continue };
        if line == "    needs:" {
            in_needs = true;
        } else if in_needs && let Some(need) = line.strip_prefix("      - ") {
            job.needs.push(need.trim().to_string());
        } else {
            in_needs = false;
            if let Some(cond) = line.strip_prefix("    if: ") {
                job.cond = cond.to_string();
            }
        }
    }
    jobs
}

/// Whether a job runs only when the plan job says this is a publishing run, that is a tag push.
/// A job with no `always()` is skipped when a job it needs was skipped, so it inherits the gate;
/// one with `always()` inherits it only from a need it requires to have succeeded.
fn gated(name: &str, jobs: &BTreeMap<String, Job>) -> bool {
    let job = &jobs[name];
    if job.cond.contains("needs.plan.outputs.publishing == 'true'") {
        return true;
    }
    let inherited = |need: &String| gated(need, jobs);
    if job.cond.contains("always()") {
        job.needs
            .iter()
            .filter(|n| {
                let required = format!("needs.{n}.result == 'success'");
                let skippable = format!("needs.{n}.result == 'skipped'");
                job.cond.contains(&required) && !job.cond.contains(&skippable)
            })
            .any(inherited)
    } else {
        job.needs.iter().any(inherited)
    }
}

#[test]
fn the_release_workflow_builds_and_publishes_only_on_a_version_tag_and_never_runs_an_example() {
    let release = read(RELEASE);
    assert_eq!(
        block(&release, "on:"),
        ["  pull_request:", "  push:", "    tags:", "      - 'v**[0-9]+.[0-9]+.[0-9]+*'"],
        "the release runs on a `v` version tag, and on a pull request only to plan"
    );
    assert!(
        release.contains("      publishing: ${{ !github.event.pull_request }}"),
        "the plan job must mark a pull request as not publishing"
    );

    let jobs = jobs(&release);
    assert!(jobs.contains_key("plan") && jobs["plan"].needs.is_empty());
    let ungated: Vec<&String> = jobs.keys().filter(|n| *n != "plan" && !gated(n, &jobs)).collect();
    assert!(ungated.is_empty(), "jobs that would run on a pull request: {ungated:?}");

    let dist: toml::Table = toml::from_str(&read("dist-workspace.toml")).expect("dist config");
    let dist = dist["dist"].as_table().expect("[dist]");
    // `upload` would build on a pull request: `build-local-artifacts` is gated on it too.
    assert_eq!(dist.get("pr-run-mode").and_then(|v| v.as_str()), Some("plan"));

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(WORKFLOWS);
    for entry in std::fs::read_dir(&dir).expect("workflows") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("workflow");
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            assert!(
                !line.contains("--example") && !line.contains("cargo run"),
                "{} runs an example or a binary: {line}",
                path.display()
            );
        }
    }
}

#[test]
fn a_release_ships_two_targets_a_shell_installer_and_a_homebrew_formula_per_binary() {
    let dist: toml::Table = toml::from_str(&read("dist-workspace.toml")).expect("dist config");
    let dist = dist["dist"].as_table().expect("[dist]");
    let strings = |key: &str| -> Vec<&str> {
        dist[key].as_array().expect(key).iter().filter_map(|v| v.as_str()).collect()
    };
    assert_eq!(strings("targets"), ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]);
    assert_eq!(strings("installers"), ["shell", "homebrew"]);
    assert_eq!(strings("publish-jobs"), ["homebrew", "./publish-crates"]);
    assert_eq!(dist["tap"].as_str(), Some("StGerman/homebrew-tap"));
}

#[test]
fn the_crate_publish_job_can_check_out_the_repository() {
    let release = read(RELEASE);
    let job = job(&release, "custom-publish-crates");
    assert!(job.contains(&"    permissions:"), "the job's permissions: {job:?}");
    assert!(job.contains(&"      \"contents\": \"read\""), "the job's permissions: {job:?}");
}

#[test]
fn the_pull_request_plan_job_has_no_write_token() {
    let release = read(RELEASE);
    assert!(jobs(&release)["plan"].cond.is_empty(), "the plan job runs on every pull request");
    let plan = job(&release, "plan");
    let scopes: Vec<&str> = plan
        .iter()
        .skip_while(|l| **l != "    permissions:")
        .skip(1)
        .take_while(|l| l.starts_with("      "))
        .copied()
        .collect();
    assert_eq!(scopes, ["      \"contents\": \"read\""], "the plan job's permissions: {plan:?}");
}

#[test]
fn a_rerun_of_the_homebrew_publish_skips_the_commit_when_the_formulae_are_unchanged() {
    let release = read(RELEASE);
    let commits: Vec<&str> = job(&release, "publish-homebrew-formula")
        .into_iter()
        .map(str::trim)
        .filter(|l| l.contains("git commit"))
        .collect();
    assert_eq!(commits, ["git diff --cached --quiet || git commit -m \"${name} ${version}\""]);
}

#[test]
fn the_published_crewd_crate_ships_no_test_that_reads_repository_only_files() {
    let out = std::process::Command::new(env!("CARGO"))
        .args(["package", "--list", "-p", "crewd", "--locked", "--allow-dirty"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo package --list");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let listed = String::from_utf8(out.stdout).expect("utf-8 file list");

    let manifest: toml::Table = toml::from_str(&read("Cargo.toml")).expect("Cargo.toml");
    let excluded: Vec<&str> = manifest["package"]["exclude"]
        .as_array()
        .expect("package.exclude")
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|p| !p.ends_with(".rs"))
        .collect();
    assert!(excluded.contains(&".github"), "the crate excludes: {excluded:?}");

    let readers: Vec<&str> = listed
        .lines()
        .filter(|f| f.starts_with("tests/") && f.ends_with(".rs"))
        .filter(|f| excluded.iter().any(|dir| read(f).contains(&format!("{dir}/"))))
        .collect();
    assert!(readers.is_empty(), "packaged tests that read excluded files: {readers:?}");
}

/// Needs dist at the version `dist-workspace.toml` pins on `PATH`; the `release-plan` job in
/// `ci.yml` installs it and runs this.
#[test]
#[ignore = "needs dist on PATH; ci.yml's release-plan job runs it"]
fn the_release_plan_lists_both_binaries_both_formulae_and_the_shell_installer() {
    let dist: toml::Table = toml::from_str(&read("dist-workspace.toml")).expect("dist config");
    let pinned = dist["dist"]["cargo-dist-version"].as_str().expect("cargo-dist-version");
    let run = |args: &[&str]| {
        let out = std::process::Command::new("dist")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap_or_else(|e| panic!("dist {args:?}: {e}"));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).expect("utf-8")
    };
    assert_eq!(run(&["--version"]).split_whitespace().last(), Some(pinned));

    let plan: serde_json::Value =
        serde_json::from_str(&run(&["plan", "--output-format=json"])).expect("plan json");
    let apps: Vec<&str> = plan["releases"]
        .as_array()
        .expect("releases")
        .iter()
        .filter_map(|r| r["app_name"].as_str())
        .collect();
    assert_eq!(apps, ["crewctl", "crewd"]);

    let kinds: BTreeMap<&str, &str> = plan["artifacts"]
        .as_object()
        .expect("artifacts")
        .iter()
        .map(|(name, a)| (name.as_str(), a["kind"].as_str().unwrap_or_default()))
        .collect();
    for app in apps {
        for (artifact, kind) in [
            (format!("{app}-aarch64-apple-darwin.tar.xz"), "executable-zip"),
            (format!("{app}-x86_64-unknown-linux-gnu.tar.xz"), "executable-zip"),
            (format!("{app}-installer.sh"), "installer"),
            (format!("{app}.rb"), "installer"),
        ] {
            assert_eq!(kinds.get(artifact.as_str()), Some(&kind), "{artifact} in {kinds:?}");
        }
    }
}
