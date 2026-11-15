//! Git storage for workspaces: one bare repository per workspace under
//! `MUSTER_REPO_ROOT`.
//!
//! Durability order (see the implementation contract): an upload's objects
//! and an immutable ref `refs/muster/uploads/<id>` are written **before** the
//! PostgreSQL transaction that may accept it. PostgreSQL is the only
//! authority on the accepted head; `refs/heads/main` is a derived ref that is
//! repaired to the PostgreSQL head before it is served. Upload refs are never
//! deleted, so retained proposals (and every accepted state) survive `git gc`.
//! Ref and database are not updated atomically together; a crash between them
//! leaves an unreferenced-by-PG upload ref, which is harmless.
//!
//! All functions block (they run `git`); call them from `spawn_blocking`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use axum::http::StatusCode;

use crate::error::AppError;
use crate::protocol::{error_codes, SYNC_REF};

pub struct RepoStore {
    root: PathBuf,
}

/// Ref under which upload `id` is kept forever.
pub fn upload_ref(id: uuid::Uuid) -> String {
    format!("refs/muster/uploads/{id}")
}

fn invalid(reason: &str, message: impl Into<String>) -> AppError {
    AppError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        error_codes::INVALID_HISTORY,
        message,
    )
    .with_details(serde_json::json!({ "reason": reason }))
}

fn git(dir: &Path, args: &[&str]) -> Result<Output, AppError> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| AppError::internal(format!("cannot run git: {e}")))
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<String, AppError> {
    let out = git(dir, args)?;
    if !out.status.success() {
        return Err(AppError::internal(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl RepoStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn repo(&self, ws: uuid::Uuid) -> PathBuf {
        self.root.join(format!("{ws}.git"))
    }

    fn scratch(&self) -> Result<tempfile::TempDir, AppError> {
        let dir = self.root.join("tmp");
        fs::create_dir_all(&dir).map_err(AppError::internal)?;
        tempfile::tempdir_in(dir).map_err(AppError::internal)
    }

    fn ensure(&self, ws: uuid::Uuid) -> Result<PathBuf, AppError> {
        let path = self.repo(ws);
        if !path.join("HEAD").exists() {
            fs::create_dir_all(&path).map_err(AppError::internal)?;
            git_ok(&path, &["init", "--quiet", "--bare"])?;
            git_ok(&path, &["config", "transfer.fsckObjects", "true"])?;
            git_ok(&path, &["config", "gc.auto", "0"])?;
        }
        Ok(path)
    }

    /// Bytes the workspace repository occupies on disk: every object the
    /// service keeps (accepted, retained proposals, stored-but-refused
    /// uploads) plus refs. Zero before the first upload.
    pub fn usage(&self, ws: uuid::Uuid) -> Result<u64, AppError> {
        fn walk(dir: &Path) -> std::io::Result<u64> {
            let mut total = 0;
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                if meta.is_dir() {
                    total += walk(&entry.path())?;
                } else {
                    total += meta.len();
                }
            }
            Ok(total)
        }
        let repo = self.repo(ws);
        if !repo.exists() {
            return Ok(0);
        }
        walk(&repo).map_err(AppError::internal)
    }

    /// Stores the bundle's objects and pins its tip under
    /// [`upload_ref`]`(id)`. The bundle must carry exactly one ref,
    /// `refs/heads/main`, and its prerequisites must already be stored.
    /// Returns the tip commit.
    pub fn ingest(
        &self,
        ws: uuid::Uuid,
        bundle: &[u8],
        id: uuid::Uuid,
    ) -> Result<String, AppError> {
        let repo = self.ensure(ws)?;
        let scratch = self.scratch()?;
        let file = scratch.path().join("upload.bundle");
        fs::write(&file, bundle).map_err(AppError::internal)?;
        let file_str = file.to_str().ok_or_else(|| AppError::internal("path"))?;

        let heads = git(&repo, &["bundle", "list-heads", file_str])?;
        if !heads.status.success() {
            return Err(invalid("bad_bundle", "body is not a readable git bundle"));
        }
        let heads = String::from_utf8_lossy(&heads.stdout).into_owned();
        let lines: Vec<&str> = heads.lines().collect();
        let tip = match lines.as_slice() {
            [line] => match line.split_once(' ') {
                Some((sha, name)) if name == SYNC_REF && is_sha(sha) => sha.to_string(),
                _ => {
                    return Err(invalid(
                        "bad_bundle_refs",
                        "bundle must carry only refs/heads/main",
                    ))
                }
            },
            _ => {
                return Err(invalid(
                    "bad_bundle_refs",
                    "bundle must carry only refs/heads/main",
                ))
            }
        };
        let verify = git(&repo, &["bundle", "verify", "--quiet", file_str])?;
        if !verify.status.success() {
            return Err(invalid(
                "missing_prerequisites",
                "bundle prerequisites are not in the workspace history",
            ));
        }
        let refspec = format!("{SYNC_REF}:{}", upload_ref(id));
        let fetched = git(
            &repo,
            &["fetch", "--quiet", "--no-tags", file_str, &refspec],
        )?;
        if !fetched.status.success() {
            return Err(invalid(
                "bad_bundle",
                format!(
                    "bundle objects refused: {}",
                    String::from_utf8_lossy(&fetched.stderr).trim()
                ),
            ));
        }
        Ok(tip)
    }

    /// A private checkout of `rev` (which must be reachable from a stored
    /// upload ref) with its complete history, for the trust verifier.
    pub fn checkout(
        &self,
        ws: uuid::Uuid,
        upload: uuid::Uuid,
    ) -> Result<tempfile::TempDir, AppError> {
        let repo = self.repo(ws);
        let dir = self.scratch()?;
        let path = dir.path();
        let repo_str = repo.to_str().ok_or_else(|| AppError::internal("path"))?;
        git_ok(path, &["init", "--quiet"])?;
        git_ok(
            path,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                repo_str,
                &upload_ref(upload),
            ],
        )?;
        git_ok(path, &["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;
        Ok(dir)
    }

    /// Whether `ancestor` is an ancestor of (or equal to) `rev`.
    pub fn is_ancestor(&self, ws: uuid::Uuid, ancestor: &str, rev: &str) -> Result<bool, AppError> {
        if !is_sha(ancestor) || !is_sha(rev) {
            return Ok(false);
        }
        let out = git(
            &self.repo(ws),
            &["merge-base", "--is-ancestor", ancestor, rev],
        )?;
        Ok(out.status.success())
    }

    /// Repairs the derived `refs/heads/main` to the PostgreSQL head.
    pub fn set_main(&self, ws: uuid::Uuid, head: &str) -> Result<(), AppError> {
        git_ok(&self.repo(ws), &["update-ref", SYNC_REF, head]).map(|_| ())
    }

    /// Bundle of `refs/heads/main` at `head` (repaired first): the complete
    /// history, or `since..head` when `since` is a stored ancestor of head.
    pub fn bundle(
        &self,
        ws: uuid::Uuid,
        head: &str,
        since: Option<&str>,
    ) -> Result<Vec<u8>, AppError> {
        self.set_main(ws, head)?;
        let repo = self.repo(ws);
        let scratch = self.scratch()?;
        let file = scratch.path().join("out.bundle");
        let file_str = file.to_str().ok_or_else(|| AppError::internal("path"))?;
        let range = match since {
            Some(since) if since != head && self.is_ancestor(ws, since, head)? => {
                format!("{since}..{SYNC_REF}")
            }
            _ => SYNC_REF.to_string(),
        };
        git_ok(&repo, &["bundle", "create", "--quiet", file_str, &range])?;
        fs::read(&file).map_err(AppError::internal)
    }

    /// Self-contained bundle of upload `id` (complete history), exposing its
    /// tip as `refs/heads/main` like every sync bundle.
    pub fn upload_bundle(&self, ws: uuid::Uuid, id: uuid::Uuid) -> Result<Vec<u8>, AppError> {
        let repo = self.repo(ws);
        let scratch = self.scratch()?;
        let tmp = scratch.path().join("view.git");
        let tmp_str = tmp.to_str().ok_or_else(|| AppError::internal("path"))?;
        let repo_str = repo.to_str().ok_or_else(|| AppError::internal("path"))?;
        git_ok(scratch.path(), &["init", "--quiet", "--bare", tmp_str])?;
        let refspec = format!("{}:{SYNC_REF}", upload_ref(id));
        git_ok(&tmp, &["fetch", "--quiet", "--no-tags", repo_str, &refspec])?;
        let file = scratch.path().join("out.bundle");
        let file_str = file.to_str().ok_or_else(|| AppError::internal("path"))?;
        git_ok(&tmp, &["bundle", "create", "--quiet", file_str, SYNC_REF])?;
        fs::read(&file).map_err(AppError::internal)
    }

    /// Subject line of commit `rev`.
    pub fn subject(&self, ws: uuid::Uuid, rev: &str) -> Result<String, AppError> {
        if !is_sha(rev) {
            return Err(AppError::not_found());
        }
        git_ok(&self.repo(ws), &["log", "-1", "--format=%s", rev])
    }

    /// Per-file changes from `base` (or the empty tree) to `rev`. Vault
    /// ciphertext (`*.age`) is reported without content.
    pub fn diff(
        &self,
        ws: uuid::Uuid,
        base: Option<&str>,
        rev: &str,
    ) -> Result<Vec<crate::protocol::DiffFile>, AppError> {
        use crate::protocol::{DiffChange, DiffFile};
        if !is_sha(rev) || base.is_some_and(|b| !is_sha(b)) {
            return Err(AppError::not_found());
        }
        let repo = self.repo(ws);
        let from = match base {
            Some(b) => b.to_string(),
            None => git_ok(&repo, &["hash-object", "-t", "tree", "/dev/null"])?,
        };
        let names = git_ok(
            &repo,
            &["diff", "--no-renames", "--name-status", "-z", &from, rev],
        )?;
        let mut files = Vec::new();
        let mut parts = names.split('\0').filter(|p| !p.is_empty());
        while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
            let change = match status.chars().next() {
                Some('A') => DiffChange::Added,
                Some('D') => DiffChange::Deleted,
                _ => DiffChange::Modified,
            };
            let is_vault = path.ends_with(".age");
            let patch = if is_vault {
                "[ciphertext changed]".to_string()
            } else {
                git_ok(
                    &repo,
                    &["diff", "--no-renames", "--no-color", &from, rev, "--", path],
                )?
            };
            files.push(DiffFile {
                path: path.to_string(),
                change,
                is_vault,
                patch,
            });
        }
        Ok(files)
    }
}
