//! Thin wrapper over the `git` binary for the config repo (commit, remote,
//! push, pull). Git is the spec's only external dependency.

use std::path::Path;
use std::process::{Command, Output};

use crate::errors::MusterError;

const REMOTE: &str = "origin";

/// Outcome of [`pull`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pulled {
    /// The remote has no branch yet; nothing to pull.
    Empty,
    /// Local history now contains the remote branch (fast-forward or rebase).
    Updated,
    /// Local history was unrelated to the remote (a machine joining an existing
    /// fleet): it was saved under `backup` and replaced by the remote branch.
    Joined { backup: &'static str },
}

const JOIN_BACKUP_REF: &str = "refs/muster/pre-join";

pub fn is_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Stages everything and commits when the config dir is a git repo with
/// changes. Returns whether a commit was created.
pub fn commit_if_configured(dir: &Path, message: &str) -> Result<bool, MusterError> {
    if !is_repo(dir) {
        return Ok(false);
    }
    check(dir, &["add", "-A"])?;
    if run(dir, &["diff", "--cached", "--quiet"])?.status.success() {
        return Ok(false);
    }
    let mut args: Vec<&str> = Vec::new();
    // Fresh machines often have no git identity; never fail a fleet operation on it.
    if !has_config(dir, "user.email") {
        args.extend([
            "-c",
            "user.name=Muster",
            "-c",
            "user.email=muster@localhost",
        ]);
    }
    args.extend(["commit", "--quiet", "-m", message]);
    check(dir, &args)?;
    Ok(true)
}

pub fn has_remote(dir: &Path) -> bool {
    is_repo(dir) && run(dir, &["remote", "get-url", REMOTE]).is_ok_and(|o| o.status.success())
}

/// Adds or updates the `origin` remote.
pub fn set_remote(dir: &Path, url: &str) -> Result<(), MusterError> {
    require_repo(dir)?;
    if has_remote(dir) {
        check(dir, &["remote", "set-url", REMOTE, url])
    } else {
        check(dir, &["remote", "add", REMOTE, url])
    }
}

/// Pushes the current branch to `origin` (no-op without local commits).
pub fn push(dir: &Path) -> Result<(), MusterError> {
    require_remote(dir)?;
    if !has_commits(dir) {
        return Ok(());
    }
    let branch = current_branch(dir)?;
    let refspec = format!("HEAD:refs/heads/{branch}");
    check(dir, &["push", "--quiet", "-u", REMOTE, &refspec])
}

/// Fetches `origin` and integrates its copy of the current branch.
pub fn pull(dir: &Path) -> Result<Pulled, MusterError> {
    require_remote(dir)?;
    check(dir, &["fetch", "--quiet", REMOTE])?;
    let branch = current_branch(dir)?;
    let upstream = format!("{REMOTE}/{branch}");
    let upstream_ref = format!("refs/remotes/{upstream}");
    if !run(dir, &["rev-parse", "--verify", "--quiet", &upstream_ref])?
        .status
        .success()
    {
        return Ok(Pulled::Empty);
    }
    if !has_commits(dir) {
        check(dir, &["reset", "--quiet", "--hard", &upstream])?;
        return Ok(Pulled::Joined {
            backup: JOIN_BACKUP_REF,
        });
    }
    let related = run(dir, &["merge-base", "HEAD", &upstream])?
        .status
        .success();
    if !related {
        check(dir, &["update-ref", JOIN_BACKUP_REF, "HEAD"])?;
        check(dir, &["reset", "--quiet", "--hard", &upstream])?;
        return Ok(Pulled::Joined {
            backup: JOIN_BACKUP_REF,
        });
    }
    let mut args: Vec<&str> = Vec::new();
    if !has_config(dir, "user.email") {
        args.extend([
            "-c",
            "user.name=Muster",
            "-c",
            "user.email=muster@localhost",
        ]);
    }
    args.extend(["rebase", "--quiet", &upstream]);
    if let Err(e) = check(dir, &args) {
        let _ = run(dir, &["rebase", "--abort"]);
        return Err(MusterError::Operational(format!(
            "local changes conflict with {upstream}; resolve manually: {e}"
        )));
    }
    Ok(Pulled::Updated)
}

/// Transports git may use for skill sources: https, ssh and local files only.
const SOURCE_TRANSPORT: [&str; 8] = [
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.https.allow=always",
    "-c",
    "protocol.ssh.allow=always",
    "-c",
    "protocol.file.allow=user",
];

/// Shallow copy of `url` at `git_ref` (default: the remote HEAD) in `dest`:
/// `git clone --depth 1` the first time, then `git fetch --depth 1` and a
/// forced checkout of `FETCH_HEAD`. Returns the checked-out commit.
pub fn shallow_fetch(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<String, MusterError> {
    if is_repo(dest) {
        check(dest, &["remote", "set-url", "--", REMOTE, url])?;
        let target = git_ref.unwrap_or("HEAD");
        let mut args = SOURCE_TRANSPORT.to_vec();
        args.extend(["fetch", "--quiet", "--depth", "1", "--", REMOTE, target]);
        check(dest, &args)?;
        check(dest, &["checkout", "--quiet", "--force", "FETCH_HEAD"])?;
    } else {
        if dest.exists() {
            std::fs::remove_dir_all(dest)
                .map_err(|e| MusterError::Operational(format!("{}: {e}", dest.display())))?;
        }
        let parent = dest.parent().unwrap_or(dest);
        std::fs::create_dir_all(parent)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", parent.display())))?;
        let dest_text = dest.to_string_lossy();
        let mut args = SOURCE_TRANSPORT.to_vec();
        args.extend(["clone", "--quiet", "--depth", "1"]);
        if let Some(r) = git_ref {
            args.extend(["--branch", r]);
        }
        args.extend(["--", url, &dest_text]);
        check(parent, &args)?;
    }
    head_commit(dest)
}

/// The commit `HEAD` points at.
pub fn head_commit(dir: &Path) -> Result<String, MusterError> {
    let out = run(dir, &["rev-parse", "HEAD"])?;
    if !out.status.success() {
        return Err(MusterError::Operational(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git --version`, or `None` when git cannot be run.
pub fn version() -> Option<String> {
    let out = Command::new("git").arg("--version").output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Local view of the config repo, without touching the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStatus {
    /// Paths with uncommitted changes (`git status --porcelain`).
    pub changed: usize,
    pub has_remote: bool,
    /// Commits ahead of / behind the upstream branch as of the last fetch;
    /// `None` when the branch has no upstream.
    pub ahead_behind: Option<(u64, u64)>,
}

pub fn status(dir: &Path) -> Result<RepoStatus, MusterError> {
    require_repo(dir)?;
    let porcelain = run(dir, &["status", "--porcelain"])?;
    if !porcelain.status.success() {
        return Err(MusterError::Operational(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&porcelain.stderr).trim()
        )));
    }
    let changed = String::from_utf8_lossy(&porcelain.stdout).lines().count();
    let counts = run(
        dir,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    )?;
    let ahead_behind = counts
        .status
        .success()
        .then(|| {
            let text = String::from_utf8_lossy(&counts.stdout).to_string();
            let mut parts = text.split_whitespace().map(|n| n.parse::<u64>().ok());
            Some((parts.next()??, parts.next()??))
        })
        .flatten();
    Ok(RepoStatus {
        changed,
        has_remote: has_remote(dir),
        ahead_behind,
    })
}

fn current_branch(dir: &Path) -> Result<String, MusterError> {
    let out = run(dir, &["symbolic-ref", "--short", "HEAD"])?;
    if !out.status.success() {
        return Err(MusterError::Operational(
            "config repo is on a detached HEAD; check out a branch first".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn has_commits(dir: &Path) -> bool {
    run(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok_and(|o| o.status.success())
}

fn has_config(dir: &Path, key: &str) -> bool {
    run(dir, &["config", "--get", key])
        .is_ok_and(|o| o.status.success() && !o.stdout.trim_ascii().is_empty())
}

fn require_repo(dir: &Path) -> Result<(), MusterError> {
    if is_repo(dir) {
        Ok(())
    } else {
        Err(MusterError::Operational(format!(
            "{} is not a git repository; run `muster init` first",
            dir.display()
        )))
    }
}

fn require_remote(dir: &Path) -> Result<(), MusterError> {
    require_repo(dir)?;
    if has_remote(dir) {
        Ok(())
    } else {
        Err(MusterError::Usage(
            "no remote configured; run `muster sync --remote <url>` first".into(),
        ))
    }
}

fn run(dir: &Path, args: &[&str]) -> Result<Output, MusterError> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| MusterError::Unavailable(format!("cannot run git: {e}")))
}

fn check(dir: &Path, args: &[&str]) -> Result<(), MusterError> {
    let out = run(dir, args)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(MusterError::Operational(format!(
            "git {} failed: {}",
            args.iter()
                .find(|a| !a.starts_with('-') && !a.contains('='))
                .unwrap_or(&"command"),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}
