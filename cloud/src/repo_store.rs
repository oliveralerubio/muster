//! Git storage for workspaces: one bare repository per workspace under
//! `MUSTER_REPO_ROOT`.
//!
//! Durability order (see the implementation contract): an upload is first
//! unpacked in private staging ([`RepoStore::stage`]), measured and
//! verified there; only an upload the service keeps (accepted, or retained
//! as a proposal) has its objects and an immutable ref
//! `refs/muster/uploads/<id>` moved into the workspace repository, and that
//! happens **before** the PostgreSQL transaction that records it commits.
//! Refused uploads leave nothing behind. PostgreSQL is the only
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
        // No automatic gc/maintenance: it runs detached after a fetch and
        // would repack staged objects while they are measured and moved.
        .args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"])
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

/// Size of a loose ref file (40 hex digits and a newline). Bare
/// repositories keep no reflog by default.
pub const REF_FILE_BYTES: u64 = 41;

/// An upload unpacked in private staging, not yet part of the workspace.
/// Dropping it discards every byte it holds.
pub struct Staged {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    id: uuid::Uuid,
    pub tip: String,
}

impl Staged {
    pub fn path(&self) -> &Path {
        &self.repo
    }

    /// `(path relative to objects/, size)` of every staged object file
    /// (packs, indexes, loose objects; not `info/` or `.keep` markers).
    fn object_files(&self) -> Result<Vec<(PathBuf, u64)>, AppError> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, u64)>) -> std::io::Result<()> {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                let meta = entry.metadata()?;
                if meta.is_dir() {
                    if path != base.join("info") {
                        walk(base, &path, out)?;
                    }
                } else if path.extension().is_none_or(|e| e != "keep") {
                    let rel = path.strip_prefix(base).map_err(std::io::Error::other)?;
                    out.push((rel.to_path_buf(), meta.len()));
                }
            }
            Ok(())
        }
        let base = self.repo.join("objects");
        let mut out = Vec::new();
        walk(&base, &base, &mut out)
            .map_err(|e| AppError::internal(format!("listing staged objects: {e}")))?;
        Ok(out)
    }
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

    /// Unpacks an upload into a private staging repository that borrows the
    /// workspace's objects (`objects/info/alternates`) for the bundle's
    /// prerequisites, so only the upload's **new** objects land in staging,
    /// as one pack. Nothing is written to the workspace repository: dropping
    /// the returned [`Staged`] discards the upload completely. The bundle
    /// must carry exactly one ref, `refs/heads/main`.
    pub fn stage(&self, ws: uuid::Uuid, bundle: &[u8], id: uuid::Uuid) -> Result<Staged, AppError> {
        let repo = self.ensure(ws)?;
        let dir = self.scratch()?;
        let stage = dir.path().join("stage.git");
        let stage_str = stage.to_str().ok_or_else(|| AppError::internal("path"))?;
        git_ok(
            dir.path(),
            &["init", "--quiet", "--bare", "--template=", stage_str],
        )?;
        let objects = repo.join("objects");
        let objects = objects
            .canonicalize()
            .map_err(|e| AppError::internal(format!("workspace objects: {e}")))?;
        fs::create_dir_all(stage.join("objects/info")).map_err(AppError::internal)?;
        fs::write(
            stage.join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )
        .map_err(AppError::internal)?;

        let file = dir.path().join("upload.bundle");
        fs::write(&file, bundle).map_err(AppError::internal)?;
        let file_str = file.to_str().ok_or_else(|| AppError::internal("path"))?;

        let heads = git(&stage, &["bundle", "list-heads", file_str])?;
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
        let verify = git(&stage, &["bundle", "verify", "--quiet", file_str])?;
        if !verify.status.success() {
            return Err(invalid(
                "missing_prerequisites",
                "bundle prerequisites are not in the workspace history",
            ));
        }
        let refspec = format!("{SYNC_REF}:{}", upload_ref(id));
        let fetched = git(
            &stage,
            &[
                "-c",
                "transfer.fsckObjects=true",
                "-c",
                "transfer.unpackLimit=1",
                "-c",
                "fetch.unpackLimit=1",
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                file_str,
                &refspec,
            ],
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
        fs::remove_file(&file).map_err(AppError::internal)?;
        Ok(Staged {
            _dir: dir,
            repo: stage,
            id,
            tip,
        })
    }

    /// Bytes committing `staged` adds to the workspace repository: its new
    /// object files plus the refs it will create (the upload ref, and
    /// `refs/heads/main` when `creates_main`).
    pub fn added_bytes(&self, staged: &Staged, creates_main: bool) -> Result<u64, AppError> {
        let mut total = 0;
        for (_, meta_len) in staged.object_files()? {
            total += meta_len;
        }
        let refs = 1 + u64::from(creates_main);
        Ok(total + refs * REF_FILE_BYTES)
    }

    /// Moves the staged objects into the workspace repository and pins the
    /// tip under [`upload_ref`]. Pack data is moved before its index, so Git
    /// never sees an index without its pack; files already present (same
    /// name, same content) are left alone. Nothing existing is deleted.
    pub fn commit_staged(&self, ws: uuid::Uuid, staged: Staged) -> Result<(), AppError> {
        let repo = self.ensure(ws)?;
        let target = repo.join("objects");
        let mut files = staged.object_files()?;
        files.sort_by_key(|(rel, _)| rel.extension().is_some_and(|e| e == "idx"));
        let durable = |e: std::io::Error| AppError::internal(format!("syncing objects: {e}"));
        let mut dirs = std::collections::BTreeSet::new();
        for (rel, _) in files {
            let dest = target.join(&rel);
            if dest.exists() {
                continue;
            }
            let src = staged.repo.join("objects").join(&rel);
            // Content on disk before it becomes visible in the workspace.
            fs::File::open(&src)
                .and_then(|f| f.sync_all())
                .map_err(durable)?;
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(AppError::internal)?;
                dirs.insert(parent.to_path_buf());
            }
            fs::rename(&src, &dest)
                .map_err(|e| AppError::internal(format!("moving staged {}: {e}", rel.display())))?;
        }
        // The new directory entries too, before the PostgreSQL commit that
        // will refer to these objects.
        for dir in dirs {
            fs::File::open(&dir)
                .and_then(|f| f.sync_all())
                .map_err(durable)?;
        }
        git_ok(&repo, &["update-ref", &upload_ref(staged.id), &staged.tip]).map(|_| ())
    }

    /// A private checkout of the staged upload with its complete history,
    /// for the trust verifier.
    pub fn checkout(&self, staged: &Staged) -> Result<tempfile::TempDir, AppError> {
        let dir = self.scratch()?;
        let path = dir.path();
        let repo_str = staged
            .repo
            .to_str()
            .ok_or_else(|| AppError::internal("path"))?;
        git_ok(path, &["init", "--quiet"])?;
        git_ok(
            path,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                repo_str,
                &upload_ref(staged.id),
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
