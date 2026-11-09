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

/// URL of the `origin` remote.
pub fn remote_url(dir: &Path) -> Result<String, MusterError> {
    require_repo(dir)?;
    let out = run(dir, &["remote", "get-url", REMOTE])?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(MusterError::Usage(
            "no remote configured; run `muster sync --remote <url>` first".into(),
        ))
    }
}

/// Creates `dir` and a git repository in it when there is none yet.
pub fn init_repo(dir: &Path) -> Result<(), MusterError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| MusterError::Operational(format!("{}: {e}", dir.display())))?;
    if is_repo(dir) {
        return Ok(());
    }
    check(dir, &["init", "--quiet"])
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

/// Whether `rel_path` (relative to the repo root) has uncommitted local
/// changes: untracked, modified or staged. Used to tell a request that never
/// reached a commit apart from one that is at least recorded locally.
pub fn path_dirty(dir: &Path, rel_path: &str) -> Result<bool, MusterError> {
    require_repo(dir)?;
    let out = run(dir, &["status", "--porcelain", "--", rel_path])?;
    if !out.status.success() {
        return Err(MusterError::Operational(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Whether `rel_path` exists in the last-known state of the fleet remote:
/// the current branch's tracking ref (`origin/<branch>`, updated by every
/// fetch, pull and push), or `@{upstream}` when only that is configured.
/// `Some(true)` = published as of the last sync, `Some(false)` = committed
/// but not on the remote yet, `None` = no remote state is known locally, so
/// publication cannot be determined without the network.
pub fn path_on_upstream(dir: &Path, rel_path: &str) -> Result<Option<bool>, MusterError> {
    require_repo(dir)?;
    // The last-known state of the fleet remote: the current branch's
    // tracking ref (updated by every fetch, pull and push), falling back to
    // `@{upstream}` (set by `git push -u`).
    let mut candidates = Vec::new();
    if let Ok(branch) = current_branch(dir) {
        candidates.push(format!("refs/remotes/{REMOTE}/{branch}"));
    }
    candidates.push("@{upstream}".to_owned());
    let Some(target) = candidates.iter().find(|rev| {
        run(dir, &["rev-parse", "--verify", "--quiet", rev]).is_ok_and(|out| out.status.success())
    }) else {
        return Ok(None);
    };
    let check = run(dir, &["cat-file", "-e", &format!("{target}:{rel_path}")])?;
    Ok(Some(check.status.success()))
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

/// Undoes a pull that produced a state this machine refuses: back to `previous`,
/// or, when the repo had no commits before the pull, back to an empty repo
/// (the files the pull checked out are removed).
pub fn restore_after_pull(dir: &Path, previous: Option<&str>) -> Result<(), MusterError> {
    match previous {
        Some(commit) => check(dir, &["reset", "--quiet", "--hard", commit]),
        None => {
            let listed = run(dir, &["ls-files", "-z"])?;
            for rel in String::from_utf8_lossy(&listed.stdout).split('\0') {
                if !rel.is_empty() {
                    let _ = std::fs::remove_file(dir.join(rel));
                }
            }
            check(dir, &["update-ref", "-d", "HEAD"])?;
            check(
                dir,
                &["rm", "-r", "--cached", "--quiet", "--ignore-unmatch", "."],
            )
        }
    }
}

/// The commit `HEAD` points at, or `None` for a repo without commits.
pub fn head_commit_opt(dir: &Path) -> Option<String> {
    if has_commits(dir) {
        head_commit(dir).ok()
    } else {
        None
    }
}

/// `path` as of `commit` (`git show <commit>:<path>`), if it existed.
pub fn file_at(dir: &Path, commit: &str, path: &str) -> Option<String> {
    let out = run(dir, &["show", &format!("{commit}:{path}")]).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every commit touching `path`, oldest first.
pub fn commits_touching_oldest_first(dir: &Path, path: &str) -> Result<Vec<String>, MusterError> {
    let out = run(dir, &["log", "--reverse", "--format=%H", "--", path])?;
    if !out.status.success() {
        return Err(MusterError::Security(format!(
            "cannot read the history of {path}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Up to `limit` newest commits touching `path`.
pub fn commits_touching(dir: &Path, path: &str, limit: usize) -> Vec<String> {
    let limit = format!("-n{limit}");
    run(dir, &["log", "--format=%H", &limit, "--", path])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The commit whose `fleet.state.json` has payload hash `payload_hash`
/// (newest first among the last 100 commits touching it).
pub fn commit_with_state(dir: &Path, payload_hash: &str) -> Option<String> {
    commits_touching(dir, crate::fleet_state::STATE_FILE, 100)
        .into_iter()
        .find(|commit| {
            file_at(dir, commit, crate::fleet_state::STATE_FILE)
                .and_then(|text| serde_json::from_str::<crate::fleet_state::FleetState>(&text).ok())
                .and_then(|state| state.payload_hash().ok())
                .as_deref()
                == Some(payload_hash)
        })
}

/// Makes `paths` (index and working tree) match `commit` exactly, deleting
/// tracked files that `commit` does not have. Non-existent paths in `commit`
/// are cleanly removed from the index and working tree.
pub fn restore_paths(dir: &Path, commit: &str, paths: &[&str]) -> Result<(), MusterError> {
    let source = format!("--source={commit}");
    for path in paths {
        // If path exists in the commit, restore it. Otherwise remove it if present.
        let has_path = run(dir, &["cat-file", "-e", &format!("{commit}:{path}")])
            .is_ok_and(|o| o.status.success());
        if has_path {
            check(
                dir,
                &[
                    "restore",
                    source.as_str(),
                    "--staged",
                    "--worktree",
                    "--",
                    path,
                ],
            )?;
        } else if dir.join(path).exists() {
            let _ = check(dir, &["rm", "-rf", "--quiet", "--ignore-unmatch", path]);
        }
    }
    Ok(())
}

/// Up to `limit` newest commits in the config repository as JSON values for
/// audit history inspection. Git author and `Muster-Actor` trailers are
/// explicitly marked `verified: false` so they are never confused with
/// cryptographically verified fleet state signatures (`muster trust show`).
pub fn recent_commits(dir: &Path, limit: usize) -> Vec<serde_json::Value> {
    if !is_repo(dir) || !has_commits(dir) {
        return Vec::new();
    }
    let limit_arg = format!("-n{limit}");
    let format_arg =
        "--format=%H%x1f%cI%x1f%an <%ae>%x1f%s%x1f%(trailers:key=Muster-Actor,valueonly,separator=; )%x1e";
    let Ok(out) = run(dir, &["log", "--no-show-signature", &limit_arg, format_arg]) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .split('\x1e')
        .filter_map(|record| {
            let record = record.trim();
            if record.is_empty() {
                return None;
            }
            let mut parts = record.splitn(5, '\x1f');
            let hash = parts.next()?.trim();
            if hash.is_empty() {
                return None;
            }
            let timestamp = parts.next().unwrap_or("").trim();
            let git_author = parts.next().unwrap_or("").trim();
            let subject = parts.next().unwrap_or("").trim();
            let claimed_actor = parts
                .next()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let short_hash: String = hash.chars().take(7).collect();
            Some(serde_json::json!({
                "commit": hash,
                "short_commit": short_hash,
                "hash": hash,
                "short_hash": short_hash,
                "committed_at": timestamp,
                "timestamp": timestamp,
                "subject": subject,
                "git_author": git_author,
                "muster_actor": claimed_actor,
                "claimed_actor": claimed_actor,
                "verified": false,
            }))
        })
        .collect()
}
