//! New Muster repositories are created on `main`: fleet bundles carry
//! `refs/heads/main`, so the branch every repo starts on must line up with
//! what `git pull` integrates (`origin/<current-branch>`). Covers the repo
//! creation default and the pull alignment for repositories that predate it.

mod support;

use muster::git;
use std::path::{Path, PathBuf};
use std::process::Command;
use support::MusterEnv;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Commits everything in `dir` with a fixed identity (raw `git commit` has no
/// muster-managed identity to fall back on).
fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@localhost"])
        .args(["commit", "--quiet", "-m", message])
        .output()
        .expect("run git commit");
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn symbolic_head(dir: &Path) -> String {
    String::from_utf8_lossy(&git(dir, &["symbolic-ref", "HEAD"]).stdout)
        .trim()
        .to_owned()
}

fn head_commit(dir: &Path) -> String {
    String::from_utf8_lossy(&git(dir, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_owned()
}

/// A fleet-style repository on `main` with one commit. Returns (path, tip).
fn published_fleet(work: &Path) -> (PathBuf, String) {
    let published = work.join("published");
    std::fs::create_dir_all(&published).unwrap();
    git(&published, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(published.join("muster.toml"), "[fleet]\n").unwrap();
    commit_all(&published, "genesis");
    (published.clone(), head_commit(&published))
}

/// An old-style joining repository whose HEAD is unborn on the legacy
/// default branch (`master` on this git), with `origin` pointing at
/// `published`.
fn unborn_joiner(published: &Path, work: &Path) -> PathBuf {
    let joining = work.join("joining");
    std::fs::create_dir_all(&joining).unwrap();
    // Deterministically model a repo created before the `main` default.
    git(&joining, &["init", "--quiet", "--initial-branch=master"]);
    assert_ne!(
        symbolic_head(&joining),
        "refs/heads/main",
        "precondition: the joining repo does not start on main"
    );
    git(
        &joining,
        &[
            "remote",
            "add",
            "origin",
            &format!("{}", published.display()),
        ],
    );
    joining
}

/// `git::init_repo` (the join path) and `muster init` both start the config
/// repo on `main`, matching the `refs/heads/main` branch fleet bundles carry.
#[test]
fn new_repositories_start_on_main() {
    let env = MusterEnv::new();

    let joined = env.path().join("joined");
    git::init_repo(&joined).expect("init_repo");
    assert_eq!(symbolic_head(&joined), "refs/heads/main");

    env.fake_exe("claude", "#!/bin/sh\nexit 1\n");
    let init = env.run_json(&["--format", "json", "--yes", "init", "--machine", "laptop"]);
    assert!(init.1.ok(), "{}", init.1.raw);
    assert_eq!(symbolic_head(&env.config_dir()), "refs/heads/main");
}

/// A repository created before the default — unborn HEAD on another branch,
/// e.g. an old `master` — aligns itself with the remote's branch when it
/// joins a fleet that only publishes `main`. Silently reporting an empty
/// remote here made bundle joins look broken on such machines.
#[test]
fn pull_aligns_an_unborn_branch_with_the_remote_branch() {
    let work = tempfile::tempdir().expect("workdir");
    let (published, tip) = published_fleet(work.path());
    let joining = unborn_joiner(&published, work.path());

    let pulled = git::pull(&joining).expect("pull aligns the unborn branch");
    assert!(
        matches!(pulled, git::Pulled::Joined { .. }),
        "the unborn repo joined the fleet: {pulled:?}"
    );
    assert_eq!(symbolic_head(&joining), "refs/heads/main");
    assert_eq!(head_commit(&joining), tip);
}

/// A repository with its own history on another branch cannot align: the
/// refusal names both branches instead of reporting an empty remote.
#[test]
fn pull_with_history_on_another_branch_names_the_conflict() {
    let work = tempfile::tempdir().expect("workdir");
    let (published, _tip) = published_fleet(work.path());

    // A real (non-unborn) history on the legacy default branch.
    let joining = work.path().join("joining");
    std::fs::create_dir_all(&joining).unwrap();
    git(&joining, &["init", "--quiet", "--initial-branch=master"]);
    std::fs::write(joining.join("local.txt"), "scaffold\n").unwrap();
    commit_all(&joining, "local scaffold");
    let local_tip = head_commit(&joining);
    git(
        &joining,
        &[
            "remote",
            "add",
            "origin",
            &format!("{}", published.display()),
        ],
    );

    let err = git::pull(&joining).expect_err("divergent branches must not align silently");
    let message = err.to_string();
    assert!(message.contains("master"), "{message}");
    assert!(message.contains("main"), "{message}");
    assert_eq!(
        head_commit(&joining),
        local_tip,
        "the refused pull preserves local history"
    );
}
