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
//! authority on the accepted head; publication updates the derived
//! `refs/heads/main` as a convenience. Reads pin the PostgreSQL head under
//! that ref in a private view of the objects. The upload refs of
//! accepted states and of open proposals are never deleted, so they survive
//! any repack; only a closed proposal's upload is released
//! ([`RepoStore::release_uploads`]).
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
    instance: Instance,
}

/// This process's private scratch area, `<root>/tmp/instances/<id>/`,
/// owned through an exclusive lock on `<id>.lease` that the kernel releases
/// when the process ends in any way (including SIGKILL). Several processes
/// may share one root; each reclaims only areas whose lease nobody holds.
struct Instance {
    dir: PathBuf,
    _lease: fs::File,
}

const INSTANCES: &str = "tmp/instances";

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Flushes a file or directory (its entries) to disk.
fn sync_path(path: &Path) -> Result<(), AppError> {
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| AppError::internal(format!("syncing {}: {e}", path.display())))
}

fn ignore_missing(r: std::io::Result<()>) -> std::io::Result<()> {
    match r {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn initialize_repo(path: &Path) -> Result<(), AppError> {
    fs::create_dir_all(path).map_err(AppError::internal)?;
    git_ok(path, &["init", "--quiet", "--bare", "--template="])?;
    git_ok(path, &["config", "transfer.fsckObjects", "true"])?;
    git_ok(path, &["config", "gc.auto", "0"])?;
    Ok(())
}

fn initialize_private_repo(path: &Path, source: &Path) -> Result<(), AppError> {
    fs::create_dir(path).map_err(AppError::internal)?;
    git_ok(path, &["init", "--quiet", "--bare", "--template="])?;
    if source.join("HEAD").exists() {
        let objects = source
            .join("objects")
            .canonicalize()
            .map_err(|e| AppError::internal(format!("workspace objects: {e}")))?;
        fs::create_dir_all(path.join("objects/info")).map_err(AppError::internal)?;
        fs::write(
            path.join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )
        .map_err(AppError::internal)?;
    }
    Ok(())
}

fn directory_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        total += if meta.is_dir() {
            directory_bytes(&entry.path())?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

impl Instance {
    /// Takes a fresh lease, then creates the area it owns. The lease is
    /// created before the area and removed after it, so an area without a
    /// lease never belongs to a live process.
    fn take(area: &Path) -> std::io::Result<Self> {
        for _ in 0..8 {
            let id = uuid::Uuid::new_v4();
            let path = area.join(format!("{id}.lease"));
            let lease = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            if lease.try_lock().is_err() {
                continue;
            }
            // A reaper may have removed the file between create and lock:
            // the lock must be on the file the path names.
            match fs::metadata(&path) {
                Ok(on_disk) if same_file(&on_disk, &lease.metadata()?) => {}
                _ => continue,
            }
            let dir = area.join(id.to_string());
            fs::create_dir(&dir)?;
            return Ok(Self { dir, _lease: lease });
        }
        Err(std::io::Error::other("could not take a scratch lease"))
    }
}

/// Removes scratch areas of processes that are gone: a lease whose lock is
/// free (its process ended; the kernel released it), and an area with no
/// lease. Live leases and every name it does not own (anything that is not
/// `<uuid>` or `<uuid>.lease`) are left alone. Each step is idempotent, so a
/// crash during reclaim is finished by the next one. Returns how many
/// areas were reclaimed.
fn reclaim(area: &Path) -> std::io::Result<usize> {
    let mut reclaimed = 0;
    for entry in fs::read_dir(area)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(stem) = name.strip_suffix(".lease") {
            if uuid::Uuid::parse_str(stem).is_err() {
                continue;
            }
            let lease = match fs::OpenOptions::new().write(true).open(entry.path()) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if lease.try_lock().is_err() {
                continue; // held by a live process
            }
            ignore_missing(fs::remove_dir_all(area.join(stem)))?;
            ignore_missing(fs::remove_file(entry.path()))?;
            reclaimed += 1;
        } else if uuid::Uuid::parse_str(name).is_ok()
            && entry.file_type()?.is_dir()
            && !area.join(format!("{name}.lease")).exists()
        {
            ignore_missing(fs::remove_dir_all(entry.path()))?;
            reclaimed += 1;
        }
    }
    Ok(reclaimed)
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
    /// Opens the store: reclaims scratch left by processes that are gone
    /// (crashed or killed mid-request), then takes this process's lease.
    pub fn open(root: PathBuf) -> std::io::Result<Self> {
        let area = root.join(INSTANCES);
        fs::create_dir_all(&area)?;
        let reclaimed = reclaim(&area)?;
        if reclaimed > 0 {
            tracing::info!(reclaimed, "reclaimed scratch of stopped instances");
        }
        let instance = Instance::take(&area)?;
        Ok(Self { root, instance })
    }

    fn repo(&self, ws: uuid::Uuid) -> PathBuf {
        self.root.join(format!("{ws}.git"))
    }

    /// A private directory in this process's leased area, removed on drop
    /// (or reclaimed by the next start if the process dies first).
    fn scratch(&self) -> Result<tempfile::TempDir, AppError> {
        tempfile::tempdir_in(&self.instance.dir).map_err(AppError::internal)
    }

    fn ensure(&self, ws: uuid::Uuid) -> Result<PathBuf, AppError> {
        let path = self.repo(ws);
        if !path.join("HEAD").exists() {
            // No template: hook samples are never run (hooks are disabled)
            // and would only count as storage.
            initialize_repo(&path)?;
        }
        Ok(path)
    }

    /// Bytes the workspace repository occupies on disk: every object the
    /// service keeps (accepted, retained proposals, stored-but-refused
    /// uploads) plus refs. Zero before the first upload.
    pub fn usage(&self, ws: uuid::Uuid) -> Result<u64, AppError> {
        let repo = self.repo(ws);
        if !repo.exists() {
            return Ok(0);
        }
        directory_bytes(&repo).map_err(AppError::internal)
    }

    /// Unpacks an upload into a private staging repository that borrows the
    /// workspace's objects (`objects/info/alternates`) for the bundle's
    /// prerequisites, so only the upload's **new** objects land in staging,
    /// as one pack. Nothing is written to the workspace repository: dropping
    /// the returned [`Staged`] discards the upload completely. The bundle
    /// must carry exactly one ref, `refs/heads/main`.
    pub fn stage(&self, ws: uuid::Uuid, bundle: &[u8], id: uuid::Uuid) -> Result<Staged, AppError> {
        // The workspace repository is not created here: a refused upload
        // must leave nothing behind. Without one there are no stored objects
        // to borrow (and a bundle with prerequisites fails `verify`).
        let dir = self.scratch()?;
        let stage = dir.path().join("stage.git");
        initialize_private_repo(&stage, &self.repo(ws))?;

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
    /// `refs/heads/main` when `creates_main`), including repository metadata
    /// on the first upload. Measure that metadata privately so a rejection
    /// still creates nothing in the workspace.
    pub fn added_bytes(
        &self,
        ws: uuid::Uuid,
        staged: &Staged,
        creates_main: bool,
    ) -> Result<u64, AppError> {
        let mut total = 0;
        if !self.repo(ws).join("HEAD").exists() {
            let scratch = self.scratch()?;
            let empty = scratch.path().join("empty.git");
            initialize_repo(&empty)?;
            total += directory_bytes(&empty).map_err(AppError::internal)?;
        }
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
        let created = !self.repo(ws).join("HEAD").exists();
        let repo = self.ensure(ws)?;
        if created {
            // The skeleton Git needs to open the repository, and its entry
            // in the root, before anything refers to it.
            for path in [repo.join("HEAD"), repo.join("config"), repo.join("refs")] {
                sync_path(&path)?;
            }
            sync_path(&repo)?;
            sync_path(&self.root)?;
        }
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
        dirs.insert(target.clone());
        for dir in dirs {
            fs::File::open(&dir)
                .and_then(|f| f.sync_all())
                .map_err(durable)?;
        }
        let name = upload_ref(staged.id);
        git_ok(&repo, &["update-ref", &name, &staged.tip])?;
        // `update-ref` does not flush the ref or the directories holding it
        // (Git's `core.fsync` covers files, not parent directories). PG must
        // never refer to an upload whose ref may vanish on power loss, so
        // flush the ref file and every directory up to the repository.
        let mut path = repo.join(&name);
        sync_path(&path)?;
        while let Some(parent) = path.parent() {
            sync_path(parent)?;
            if parent == repo {
                break;
            }
            path = parent.to_path_buf();
        }
        Ok(())
    }

    /// Releases the uploads of closed proposals `ids`: deletes their refs,
    /// then repacks so objects no remaining ref reaches (accepted uploads,
    /// `main`, open proposals) stop taking space. Idempotent; a crash at any
    /// point leaves a valid repository (Git writes the new pack before it
    /// removes the old ones) and the next pass finishes. The caller holds
    /// the workspace row lock (no push stages or keeps meanwhile) and the
    /// backup coordination lock shared (no capture lists objects meanwhile).
    pub fn release_uploads(&self, ws: uuid::Uuid, ids: &[uuid::Uuid]) -> Result<(), AppError> {
        let repo = self.repo(ws);
        if !repo.join("HEAD").exists() {
            return Ok(());
        }
        for id in ids {
            git_ok(&repo, &["update-ref", "-d", &upload_ref(*id)])?;
        }
        sync_path(&repo.join("refs/muster/uploads")).or_else(|e| {
            if repo.join("refs/muster/uploads").exists() {
                Err(e)
            } else {
                Ok(())
            }
        })?;
        // `-a -d`: one pack of everything reachable; packs it makes
        // redundant (with the unreachable objects they held) are removed.
        git_ok(
            &repo,
            &[
                "-c",
                "core.fsync=objects,derived-metadata,reference",
                "repack",
                "-a",
                "-d",
                "-q",
            ],
        )?;
        git_ok(&repo, &["prune", "--expire=now"])?;
        let pack = repo.join("objects/pack");
        if pack.exists() {
            sync_path(&pack)?;
        }
        Ok(())
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

    /// Sets the derived `refs/heads/main` after publication.
    pub fn set_main(&self, ws: uuid::Uuid, head: &str) -> Result<(), AppError> {
        git_ok(&self.repo(ws), &["update-ref", SYNC_REF, head]).map(|_| ())
    }

    /// Bundle of `refs/heads/main` pinned privately at `head`: the complete
    /// history, or `since..head` when `since` is a stored ancestor of head.
    pub fn bundle(
        &self,
        ws: uuid::Uuid,
        head: &str,
        since: Option<&str>,
    ) -> Result<Vec<u8>, AppError> {
        let scratch = self.scratch()?;
        let view = scratch.path().join("view.git");
        initialize_private_repo(&view, &self.repo(ws))?;
        git_ok(&view, &["update-ref", SYNC_REF, head])?;
        let file = scratch.path().join("out.bundle");
        let file_str = file.to_str().ok_or_else(|| AppError::internal("path"))?;
        let range = match since {
            Some(since) if since != head && self.is_ancestor(ws, since, head)? => {
                format!("{since}..{SYNC_REF}")
            }
            _ => SYNC_REF.to_string(),
        };
        git_ok(&view, &["bundle", "create", "--quiet", file_str, &range])?;
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
