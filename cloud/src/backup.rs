//! Managed backups (`muster-cloud-ops backup …` / `retention run`):
//! snapshot-bound encrypted capture, a strict catalogue, and retention that
//! honours the deletion cap.
//!
//! The backup root (`MUSTER_BACKUP_ROOT`) is owned by this module:
//!
//! ```text
//! snapshots/<backup_id>/   published: manifest.json, db.dump[.age], git.tar[.age]
//! staging/<op_id>/         a capture in progress
//! ops/<op_id>.json|.lock   owned operations; the lock (flock) is the lease
//! trash/<backup_id>.<op>   removals in progress (rename, then delete)
//! audit/                   one result per retention run
//! ```
//!
//! Anything else (in `snapshots/` or at the root, e.g. legacy
//! `muster-backup-*` directories) is uncatalogued: never removed, always
//! reported, and it makes the backup status of a deletion `unknown`.
//!
//! Capture: the coordination lock (`<repo_root>/locks/capture.lock`, held
//! shared by a purge from its move to trash until its trash is erased) is
//! taken exclusive; a read-only repeatable-read transaction exports its
//! snapshot; tenants, heads and caps are read in it; `pg_dump --snapshot`
//! dumps exactly that state; the Git archive holds exactly the snapshot's
//! tenants' `<uuid>.git`. Artifacts stream through `age` (no plaintext on
//! disk) and are hashed while written. One host per backup root.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

pub const FORMAT: &str = "muster-backup";
pub const VERSION: u32 = 2;
const SNAPSHOTS: &str = "snapshots";
const STAGING: &str = "staging";
const OPS: &str = "ops";
const TRASH: &str = "trash";
const AUDIT: &str = "audit";
const MANIFEST: &str = "manifest.json";
/// Removals per retention run (the next run continues).
const MAX_REMOVALS: usize = 100;
use crate::lifecycle_purge::O_NOFOLLOW;

#[derive(Debug)]
pub enum BackupError {
    Usage(String),
    /// Policy refusal (encryption, a tenant past its cap, invalid snapshot).
    Refused(String),
    Io(String),
    /// Another holder of the coordination lock (a purge erasing).
    Busy(String),
    Config(String),
}

impl BackupError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 64,
            Self::Refused(_) => 65,
            Self::Io(_) => 74,
            Self::Busy(_) => 75,
            Self::Config(_) => 78,
        }
    }
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(m) => write!(f, "{m}"),
            Self::Refused(m) => write!(f, "refused: {m}"),
            Self::Io(m) => write!(f, "{m}"),
            Self::Busy(m) => write!(f, "busy: {m}"),
            Self::Config(m) => write!(f, "configuration error: {m}"),
        }
    }
}

fn io(context: &str) -> impl Fn(std::io::Error) -> BackupError + '_ {
    move |e| BackupError::Io(format!("{context}: {e}"))
}

fn dbe(e: sqlx::Error) -> BackupError {
    BackupError::Io(format!("database: {e}"))
}

/// Development crash points of a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupCrash {
    AfterSnapshot,
    AfterDb,
    AfterGit,
    BeforePublish,
}

impl BackupCrash {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "after_snapshot" => Some(Self::AfterSnapshot),
            "after_db" => Some(Self::AfterDb),
            "after_git" => Some(Self::AfterGit),
            "before_publish" => Some(Self::BeforePublish),
            _ => None,
        }
    }
}

/// Development pause points of a capture (`MUSTER_DEV_BACKUP_PAUSE_AT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PausePoint {
    /// Right after the operation's lease file appears in `ops/`.
    LeaseVisible,
    /// After the database snapshot, holding the coordination lock.
    AfterSnapshot,
    /// After the Git archive's file list is built, before it is read.
    GitListed,
}

impl PausePoint {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "lease_visible" => Some(Self::LeaseVisible),
            "after_snapshot" => Some(Self::AfterSnapshot),
            "git_listed" => Some(Self::GitListed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Encryption {
    Age {
        recipient: String,
    },
    /// Explicit, development only: synthetic data.
    PlaintextSynthetic,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub root: PathBuf,
    pub repo_root: PathBuf,
    pub database_url: String,
    pub retention: time::Duration,
    /// Past a deadline by more than this: `overdue` (else `due`).
    pub overdue_grace: time::Duration,
    pub crash: Option<BackupCrash>,
    pub pause_file: Option<PathBuf>,
    /// Where the pause file applies: `after_snapshot` (default),
    /// `lease_visible` or `git_listed`.
    pub pause_at: PausePoint,
    pub capture_wait: std::time::Duration,
}

// ---------------------------------------------------------------------------
// Manifest (strict, versioned)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub backup_id: String,
    pub op_id: Uuid,
    pub snapshot_at: String,
    pub created_at: String,
    pub encryption: EncryptionInfo,
    pub normal_expires_at: String,
    pub tenants: Vec<Tenant>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptionInfo {
    /// `age-x25519` or `none` (synthetic, development only).
    pub scheme: String,
    pub recipient_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tenant {
    pub workspace_id: Uuid,
    pub head: Option<String>,
    /// Whether its repository existed (and is in the archive).
    pub git: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    /// `db` or `git`.
    pub kind: String,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_sha256: Option<String>,
}

fn rfc(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).expect("RFC 3339 timestamp")
}

fn parse_rfc(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `YYYYMMDDTHHMMSSZ-<8 hex>`.
fn is_backup_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 25
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z'
        && b[16] == b'-'
        && b[17..]
            .iter()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

fn backup_id(at: OffsetDateTime, op: Uuid) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z-{}",
        at.year(),
        at.month() as u8,
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        &op.simple().to_string()[..8]
    )
}

fn artifact_names(scheme: &str) -> Option<(&'static str, &'static str)> {
    match scheme {
        "age-x25519" => Some(("db.dump.age", "git.tar.age")),
        "none" => Some(("db.dump", "git.tar")),
        _ => None,
    }
}

/// A published snapshot directory, checked strictly (structure, names,
/// sizes; hashes only with `hashes`). `Err`: not catalogued.
fn read_snapshot(dir: &Path, hashes: bool) -> Result<Manifest, String> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("unnamed entry")?;
    if !is_backup_id(name) {
        return Err(format!("{name}: not a backup id"));
    }
    match fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_dir() => {}
        _ => return Err(format!("{name}: not a real directory")),
    }
    let raw = read_regular(&dir.join(MANIFEST)).map_err(|e| format!("{name}: manifest: {e}"))?;
    let m: Manifest = serde_json::from_slice(&raw).map_err(|e| format!("{name}: manifest: {e}"))?;
    if m.format != FORMAT || m.version != VERSION {
        return Err(format!("{name}: unsupported format"));
    }
    if m.backup_id != name {
        return Err(format!("{name}: manifest names another backup"));
    }
    for t in [&m.snapshot_at, &m.created_at, &m.normal_expires_at] {
        parse_rfc(t).ok_or_else(|| format!("{name}: invalid timestamp"))?;
    }
    let (db, git) =
        artifact_names(&m.encryption.scheme).ok_or_else(|| format!("{name}: unknown scheme"))?;
    let mut want = BTreeMap::from([(db, "db"), (git, "git")]);
    if m.artifacts.len() != 2 {
        return Err(format!("{name}: expected two artifacts"));
    }
    for a in &m.artifacts {
        if want.remove(a.name.as_str()) != Some(a.kind.as_str()) || !is_sha256(&a.sha256) {
            return Err(format!("{name}: unexpected artifact {}", a.name));
        }
        let path = dir.join(&a.name);
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{name}/{}: {e}", a.name))?;
        if !meta.file_type().is_file() || meta.len() != a.bytes {
            return Err(format!("{name}/{}: missing or truncated", a.name));
        }
        if hashes && hash_file(&path).map_err(|e| e.to_string())? != a.sha256 {
            return Err(format!("{name}/{}: sha256 mismatch", a.name));
        }
    }
    let entries = fs::read_dir(dir).map_err(|e| e.to_string())?.count();
    if entries != 3 {
        return Err(format!("{name}: unexpected files"));
    }
    Ok(m)
}

fn read_regular(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)?;
    if !f.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

// ---------------------------------------------------------------------------
// Owned layout, operations and leases
// ---------------------------------------------------------------------------

/// `path` as a real directory (created 0700 when missing; never a symlink).
fn ensure_dir(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_dir() => return Ok(()),
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "{} is not a real directory",
                path.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_dir() => Ok(()),
        _ => Err(std::io::Error::other(format!(
            "{} is not a real directory",
            path.display()
        ))),
    }
}

fn layout(root: &Path) -> std::io::Result<()> {
    if let Some(parent) = root.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    ensure_dir(root)?;
    for sub in [SNAPSHOTS, STAGING, OPS, TRASH, AUDIT] {
        ensure_dir(&root.join(sub))?;
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Writes `bytes` to `dir/name` atomically (tmp, fsync, rename, fsync dir).
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp-{}", Uuid::new_v4().simple()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, dir.join(name))?;
    sync_dir(dir)
}

fn remove_no_follow(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[derive(Serialize)]
struct OpRecord<'a> {
    v: u32,
    op_id: Uuid,
    kind: &'a str,
    started_at: String,
    pid: u32,
    checkpoint: &'a str,
}

/// An owned operation: its lease is the flock on `ops/<id>.lock`, held for
/// the life of the process (a crash releases it: the op becomes dead).
struct Op {
    id: Uuid,
    root: PathBuf,
    kind: &'static str,
    started_at: String,
    lock: Option<fs::File>,
}

impl Op {
    fn register(
        root: &Path,
        kind: &'static str,
        visible: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<Self> {
        let id = Uuid::new_v4();
        // The lease is held BEFORE its name is visible: created and locked
        // under a temporary name, then renamed into place (the lock follows
        // the inode). A concurrent cleanup never sees it unheld.
        let ops = root.join(OPS);
        let tmp = ops.join(format!(".{id}.lock.tmp-{}", Uuid::new_v4().simple()));
        let lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&tmp)?;
        lock.try_lock().map_err(std::io::Error::other)?;
        fs::rename(&tmp, ops.join(format!("{id}.lock")))?;
        sync_dir(&ops)?;
        visible()?;
        let op = Self {
            id,
            root: root.to_path_buf(),
            kind,
            started_at: rfc(OffsetDateTime::now_utc()),
            lock: Some(lock),
        };
        op.checkpoint("registered")?;
        Ok(op)
    }

    fn checkpoint(&self, checkpoint: &str) -> std::io::Result<()> {
        let rec = OpRecord {
            v: 1,
            op_id: self.id,
            kind: self.kind,
            started_at: self.started_at.clone(),
            pid: std::process::id(),
            checkpoint,
        };
        write_atomic(
            &self.root.join(OPS),
            &format!("{}.json", self.id),
            &serde_json::to_vec(&rec).map_err(std::io::Error::other)?,
        )
    }

    /// Removes the op record, then its lock file (while holding it).
    fn finish(mut self) -> std::io::Result<()> {
        let ops = self.root.join(OPS);
        remove_no_follow(&ops.join(format!("{}.json", self.id)))?;
        remove_no_follow(&ops.join(format!("{}.lock", self.id)))?;
        sync_dir(&ops)?;
        self.lock.take();
        Ok(())
    }
}

/// Whether op `id` is live (someone holds its lease).
fn op_live(root: &Path, id: Uuid) -> std::io::Result<bool> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(root.join(OPS).join(format!("{id}.lock")))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Cleans every dead op: its staging, its trash entries, then its record
/// and lock file. Live ops and unrecognised names are never touched.
fn clean_dead(root: &Path, own: Uuid) -> std::io::Result<Vec<Uuid>> {
    let ops = root.join(OPS);
    let mut ids = BTreeSet::new();
    for entry in fs::read_dir(&ops)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = name
            .strip_suffix(".json")
            .or_else(|| name.strip_suffix(".lock"))
            .and_then(|i| Uuid::parse_str(i).ok())
        {
            ids.insert(id);
        } else if name.starts_with('.') && name.contains(".tmp-") {
            // An interrupted atomic write, left more than an hour ago.
            let old = entry
                .metadata()?
                .modified()?
                .elapsed()
                .is_ok_and(|d| d > std::time::Duration::from_secs(3600));
            if old {
                remove_no_follow(&entry.path())?;
            }
        }
    }
    let mut cleaned = Vec::new();
    for id in ids {
        if id == own || op_live(root, id)? {
            continue;
        }
        remove_no_follow(&root.join(STAGING).join(id.to_string()))?;
        for entry in fs::read_dir(root.join(TRASH))? {
            let entry = entry?;
            let ours = entry
                .file_name()
                .to_str()
                .and_then(|n| n.rsplit_once('.'))
                .is_some_and(|(b, op)| is_backup_id(b) && op == id.to_string());
            if ours {
                remove_no_follow(&entry.path())?;
            }
        }
        remove_no_follow(&ops.join(format!("{id}.json")))?;
        remove_no_follow(&ops.join(format!("{id}.lock")))?;
        cleaned.push(id);
    }
    sync_dir(&ops)?;
    Ok(cleaned)
}

// ---------------------------------------------------------------------------
// Streams: producer → (age) → hashed file
// ---------------------------------------------------------------------------

/// Removes inherited libpq settings and sets them from a `postgres://` URL
/// (never argv).
fn pg_env(cmd: &mut Command, url: &str) -> Result<(), BackupError> {
    let bad = |why: &str| BackupError::Usage(format!("invalid database URL: {why}"));
    let u = url::Url::parse(url).map_err(|_| bad("unparsable"))?;
    if !matches!(u.scheme(), "postgres" | "postgresql") {
        return Err(bad("only postgres:// URLs are supported"));
    }
    for (k, _) in std::env::vars_os() {
        if k.to_str().is_some_and(|k| k.starts_with("PG")) {
            cmd.env_remove(k);
        }
    }
    let decode = |s: &str| percent_decode(s).ok_or_else(|| bad("invalid percent-encoding"));
    if !u.username().is_empty() {
        cmd.env("PGUSER", decode(u.username())?);
    }
    if let Some(p) = u.password() {
        cmd.env("PGPASSWORD", decode(p)?);
    }
    if let Some(h) = u.host_str().filter(|h| !h.is_empty()) {
        cmd.env(
            "PGHOST",
            decode(h.trim_start_matches('[').trim_end_matches(']'))?,
        );
    }
    if let Some(p) = u.port() {
        cmd.env("PGPORT", p.to_string());
    }
    let db = u.path().trim_start_matches('/');
    if !db.is_empty() {
        cmd.env("PGDATABASE", decode(db)?);
    }
    for (k, v) in u.query_pairs() {
        let var = match k.as_ref() {
            "host" => "PGHOST",
            "port" => "PGPORT",
            "user" => "PGUSER",
            "password" => "PGPASSWORD",
            "dbname" => "PGDATABASE",
            "sslmode" => "PGSSLMODE",
            "sslrootcert" => "PGSSLROOTCERT",
            "sslcert" => "PGSSLCERT",
            "sslkey" => "PGSSLKEY",
            "connect_timeout" => "PGCONNECT_TIMEOUT",
            "application_name" => "PGAPPNAME",
            "options" => "PGOPTIONS",
            other => return Err(bad(&format!("unsupported parameter {other:?}"))),
        };
        cmd.env(var, v.as_ref());
    }
    Ok(())
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Runs `producer` (stdout piped), optionally feeding `stdin`, through
/// `age -r` when encrypting, into a new file `out`: `(bytes, sha256)`.
fn capture_stream(
    mut producer: Command,
    stdin: Option<Vec<u8>>,
    enc: &Encryption,
    out: &Path,
    what: &str,
) -> Result<(u64, String), BackupError> {
    producer.stdout(Stdio::piped()).stderr(Stdio::inherit());
    producer.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = producer
        .spawn()
        .map_err(|e| BackupError::Io(format!("{what}: cannot start: {e}")))?;
    let feeder = match (stdin, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => Some(std::thread::spawn(move || {
            let r = pipe.write_all(&bytes);
            drop(pipe);
            r
        })),
        _ => None,
    };
    let produced = child.stdout.take().expect("piped stdout");
    let mut age = None;
    let mut source: Box<dyn Read> = match enc {
        Encryption::Age { recipient } => {
            let mut a = Command::new("age")
                .arg("-r")
                .arg(recipient)
                .stdin(Stdio::from(produced))
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|e| BackupError::Io(format!("age: cannot start (install age): {e}")))?;
            let out = a.stdout.take().expect("piped stdout");
            age = Some(a);
            Box::new(out)
        }
        Encryption::PlaintextSynthetic => Box::new(produced),
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(out)
        .map_err(io("artifact"))?;
    let mut h = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = vec![0u8; 1 << 16];
    let copied: std::io::Result<()> = (|| loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        h.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        bytes += n as u64;
    })();
    drop(source);
    let status = child.wait().map_err(io(what))?;
    let age_status = age.map(|mut a| a.wait()).transpose().map_err(io("age"))?;
    if let Some(f) = feeder {
        let _ = f.join();
    }
    copied.map_err(io("writing artifact"))?;
    if !status.success() {
        return Err(BackupError::Io(format!("{what} failed ({status})")));
    }
    if let Some(s) = age_status.filter(|s| !s.success()) {
        return Err(BackupError::Io(format!("age encryption failed ({s})")));
    }
    if bytes == 0 {
        return Err(BackupError::Io(format!("{what} produced nothing")));
    }
    file.sync_all().map_err(io("artifact"))?;
    Ok((bytes, hex(&h.finalize())))
}

fn transient(name: &str, is_dir: bool) -> bool {
    name.ends_with(".lock")
        || name.ends_with(".tmp")
        || name.starts_with("tmp_")
        || (is_dir && name.starts_with("incoming-"))
}

/// Prefix (a symlink in the capture tree to the repository root) of the
/// archive entries read live from the repository; tar strips it.
const LIVE: &str = ".src";

/// Builds the archive's file list for the snapshot's tenants, relative to
/// `tree` (a private directory in staging). Per repository, everything but
/// `objects/` (refs, `HEAD`, `config`, …) is COPIED into `tree` first, then
/// `objects/` is listed live (through `tree/.src`). A push writes its
/// objects before its refs and objects are immutable, so every ref copied
/// has its objects listed: the archive is closed under reachability even
/// while pushes continue. Symlinks and special files are refused;
/// transient Git files are skipped.
fn git_entries(
    repo_root: &Path,
    tree: &Path,
    tenants: &mut [Tenant],
) -> Result<Vec<PathBuf>, BackupError> {
    fn walk(
        base: &Path,
        rel: &Path,
        skip_objects: bool,
        visit: &mut dyn FnMut(&Path, bool) -> Result<(), BackupError>,
    ) -> Result<(), BackupError> {
        let mut names: Vec<_> = fs::read_dir(base.join(rel))
            .map_err(io("repository"))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(io("repository"))?;
        names.sort();
        for name in names {
            let child = rel.join(&name);
            let meta = match fs::symlink_metadata(base.join(&child)) {
                Ok(m) => m,
                // A transient file renamed away meanwhile.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io("repository")(e)),
            };
            let name = name.to_string_lossy();
            if transient(&name, meta.is_dir()) || (skip_objects && name == "objects") {
                continue;
            }
            if meta.is_dir() {
                visit(&child, true)?;
                walk(base, &child, false, visit)?;
            } else if meta.file_type().is_file() {
                visit(&child, false)?;
            } else {
                return Err(BackupError::Refused(format!(
                    "{} is not a regular file or directory",
                    child.display()
                )));
            }
        }
        Ok(())
    }
    ensure_dir(tree).map_err(io("capture tree"))?;
    std::os::unix::fs::symlink(repo_root, tree.join(LIVE)).map_err(io("capture tree"))?;
    let mut out = Vec::new();
    for t in tenants.iter_mut() {
        let name = format!("{}.git", t.workspace_id);
        let rel = PathBuf::from(&name);
        let repo = repo_root.join(&rel);
        match fs::symlink_metadata(&repo) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(BackupError::Refused(format!(
                    "{name} is not a real directory"
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                t.git = false;
                continue;
            }
            Err(e) => return Err(io("repository")(e)),
        }
        t.git = true;
        // 1. Everything but objects/, copied now (the refs as of now).
        ensure_dir(&tree.join(&rel)).map_err(io("capture tree"))?;
        out.push(rel.clone());
        walk(repo_root, &rel, true, &mut |p, is_dir| {
            let dest = tree.join(p);
            if is_dir {
                ensure_dir(&dest).map_err(io("capture tree"))?;
            } else {
                let mut src = match fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(O_NOFOLLOW)
                    .open(repo_root.join(p))
                {
                    Ok(f) => f,
                    // Replaced by a rename meanwhile (a ref update): the
                    // new name is listed on its own or is not needed.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(io("repository")(e)),
                };
                let mut dst = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(O_NOFOLLOW)
                    .open(&dest)
                    .map_err(io("capture tree"))?;
                std::io::copy(&mut src, &mut dst).map_err(io("capture tree"))?;
            }
            out.push(p.to_path_buf());
            Ok(())
        })?;
        // 2. Then objects/, live: a real directory entry, its content read
        // through the `.src` link.
        let objects = rel.join("objects");
        match fs::symlink_metadata(repo.join("objects")) {
            Ok(m) if m.is_dir() => {}
            _ => {
                return Err(BackupError::Refused(format!(
                    "{name}/objects is not a real directory"
                )))
            }
        }
        ensure_dir(&tree.join(&objects)).map_err(io("capture tree"))?;
        out.push(objects.clone());
        walk(repo_root, &objects, false, &mut |p, _| {
            out.push(Path::new(LIVE).join(p));
            Ok(())
        })?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

fn crash_if(s: &Settings, here: BackupCrash) {
    if s.crash == Some(here) {
        eprintln!("injected backup crash at {here:?}");
        std::process::abort();
    }
}

/// Development: at `point`, waits (bounded) until the pause file exists,
/// announcing it with `<file>.waiting`.
fn pause(s: &Settings, point: PausePoint) -> std::io::Result<()> {
    let Some(file) = s.pause_file.as_ref().filter(|_| s.pause_at == point) else {
        return Ok(());
    };
    fs::write(file.with_extension("waiting"), b"")?;
    let start = std::time::Instant::now();
    while !file.exists() {
        if start.elapsed() > std::time::Duration::from_secs(600) {
            return Err(std::io::Error::other("pause file never appeared"));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

type TenantRow = (Uuid, Option<String>);

/// `backup run`: one published, catalogued snapshot.
pub async fn run_backup(
    s: &Settings,
    enc: &Encryption,
    db: &PgPool,
) -> Result<serde_json::Value, BackupError> {
    layout(&s.root).map_err(io("backup root"))?;
    let op = Op::register(&s.root, "backup", || pause(s, PausePoint::LeaseVisible))
        .map_err(io("registering the operation"))?;
    clean_dead(&s.root, op.id).map_err(io("cleaning dead operations"))?;
    let staging = s.root.join(STAGING).join(op.id.to_string());
    ensure_dir(&staging).map_err(io("staging"))?;

    // Coordination: no purge is between its move to trash and its erasure.
    let capture = crate::lifecycle_purge::capture_lock(&s.repo_root, true, s.capture_wait)
        .await
        .map_err(io("capture lock"))?
        .ok_or_else(|| BackupError::Busy("a purge is erasing a workspace; retry later".into()))?;

    let mut conn = db.acquire().await.map_err(dbe)?;
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *conn)
        .await
        .map_err(dbe)?;
    let (snapshot, snapshot_at): (String, OffsetDateTime) =
        sqlx::query_as("SELECT pg_export_snapshot(), now()")
            .fetch_one(&mut *conn)
            .await
            .map_err(dbe)?;
    let rows: Vec<TenantRow> = sqlx::query_as(
        "SELECT w.id, h.head_revision FROM workspaces w
           LEFT JOIN workspace_heads h ON h.workspace_id = w.id ORDER BY w.id",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(dbe)?;
    let past_cap: Vec<Uuid> = sqlx::query_scalar(
        "SELECT l.workspace_id FROM workspace_lifecycle l
           JOIN workspaces w ON w.id = l.workspace_id WHERE l.backup_cap_at <= now()",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(dbe)?;
    if let Some(ws) = past_cap.first() {
        return Err(BackupError::Refused(format!(
            "workspace {ws} is past its backup cap (its purge is overdue): no new backup may hold it"
        )));
    }
    let mut tenants: Vec<Tenant> = rows
        .into_iter()
        .map(|(workspace_id, head)| Tenant {
            workspace_id,
            head,
            git: false,
        })
        .collect();
    op.checkpoint("snapshot").map_err(io("operation"))?;
    crash_if(s, BackupCrash::AfterSnapshot);
    pause(s, PausePoint::AfterSnapshot).map_err(io("pause"))?;

    let (db_name, git_name) = match enc {
        Encryption::Age { .. } => ("db.dump.age", "git.tar.age"),
        Encryption::PlaintextSynthetic => ("db.dump", "git.tar"),
    };
    let mut dump = Command::new("pg_dump");
    dump.arg(format!("--snapshot={snapshot}"))
        .arg("--format=custom")
        .arg("--no-password");
    pg_env(&mut dump, &s.database_url)?;
    let (enc2, out) = (enc.clone(), staging.join(db_name));
    let (db_bytes, db_sha) =
        tokio::task::spawn_blocking(move || capture_stream(dump, None, &enc2, &out, "pg_dump"))
            .await
            .map_err(|e| BackupError::Io(e.to_string()))??;
    sqlx::query("COMMIT")
        .execute(&mut *conn)
        .await
        .map_err(dbe)?;
    drop(conn);
    op.checkpoint("db").map_err(io("operation"))?;
    crash_if(s, BackupCrash::AfterDb);

    let repo_root = s.repo_root.clone();
    let s2 = s.clone();
    let (enc2, out) = (enc.clone(), staging.join(git_name));
    let (tenants, git_bytes, git_sha, entries, list_sha) =
        tokio::task::spawn_blocking(move || -> Result<_, BackupError> {
            let tree = out.with_file_name("tree");
            let list = git_entries(&repo_root, &tree, &mut tenants)?;
            pause(&s2, PausePoint::GitListed).map_err(io("pause"))?;
            let mut nul = Vec::new();
            for p in &list {
                nul.extend_from_slice(p.as_os_str().as_encoded_bytes());
                nul.push(0);
            }
            let list_sha = hex(&Sha256::digest(&nul));
            let mut tar = Command::new("tar");
            tar.arg("--null")
                .arg("--no-recursion")
                .arg(format!("--transform=s,^\\{LIVE}/,,"))
                .arg("-C")
                .arg(&tree)
                .arg("-cf")
                .arg("-")
                .arg("-T")
                .arg("-");
            let streamed = capture_stream(tar, Some(nul), &enc2, &out, "tar");
            remove_no_follow(&tree).map_err(io("capture tree"))?;
            let (bytes, sha) = streamed?;
            Ok((tenants, bytes, sha, list.len() as u64, list_sha))
        })
        .await
        .map_err(|e| BackupError::Io(e.to_string()))??;
    op.checkpoint("git").map_err(io("operation"))?;
    crash_if(s, BackupCrash::AfterGit);
    drop(capture);

    let id = backup_id(snapshot_at, op.id);
    let manifest = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        backup_id: id.clone(),
        op_id: op.id,
        snapshot_at: rfc(snapshot_at),
        created_at: rfc(OffsetDateTime::now_utc()),
        encryption: match enc {
            Encryption::Age { recipient } => EncryptionInfo {
                scheme: "age-x25519".into(),
                recipient_sha256: Some(hex(&Sha256::digest(recipient.as_bytes()))),
            },
            Encryption::PlaintextSynthetic => EncryptionInfo {
                scheme: "none".into(),
                recipient_sha256: None,
            },
        },
        normal_expires_at: rfc(snapshot_at + s.retention),
        tenants,
        artifacts: vec![
            Artifact {
                name: db_name.into(),
                kind: "db".into(),
                bytes: db_bytes,
                sha256: db_sha,
                entries: None,
                list_sha256: None,
            },
            Artifact {
                name: git_name.into(),
                kind: "git".into(),
                bytes: git_bytes,
                sha256: git_sha,
                entries: Some(entries),
                list_sha256: Some(list_sha),
            },
        ],
    };
    let body = serde_json::to_vec_pretty(&manifest).map_err(|e| BackupError::Io(e.to_string()))?;
    write_atomic(&staging, MANIFEST, &body).map_err(io("manifest"))?;
    crash_if(s, BackupCrash::BeforePublish);
    let published = s.root.join(SNAPSHOTS).join(&id);
    fs::rename(&staging, &published).map_err(io("publishing"))?;
    sync_dir(&s.root.join(SNAPSHOTS)).map_err(io("publishing"))?;
    sync_dir(&s.root.join(STAGING)).map_err(io("publishing"))?;
    op.checkpoint("published").map_err(io("operation"))?;

    let inv = inventory(&s.root).map_err(io("inventory"))?;
    let projection = project(db, &inv, s.overdue_grace, None).await;
    op.finish().map_err(io("operation"))?;
    let out = serde_json::json!({
        "backup_id": id,
        "path": published,
        "snapshot_at": manifest.snapshot_at,
        "tenants": manifest.tenants.len(),
        "encryption": manifest.encryption.scheme,
    });
    projection.map(|()| out)
}

/// `backup verify --snapshot <dir>`: strict structure and every hash.
pub fn verify(dir: &Path) -> Result<serde_json::Value, BackupError> {
    let m = read_snapshot(dir, true).map_err(BackupError::Refused)?;
    Ok(serde_json::json!({
        "backup_id": m.backup_id,
        "scheme": m.encryption.scheme,
        "tenants": m.tenants.len(),
        "artifacts": m.artifacts.iter().map(|a| &a.name).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// Inventory, projection and retention
// ---------------------------------------------------------------------------

struct Inventory {
    catalogued: Vec<Manifest>,
    uncatalogued: Vec<String>,
    /// Every name present in `snapshots/` (valid or not).
    on_disk: Vec<String>,
}

fn inventory(root: &Path) -> std::io::Result<Inventory> {
    let mut catalogued = Vec::new();
    let mut uncatalogued = Vec::new();
    let mut on_disk = Vec::new();
    for entry in fs::read_dir(root.join(SNAPSHOTS))? {
        let entry = entry?;
        on_disk.push(entry.file_name().to_string_lossy().into_owned());
        match read_snapshot(&entry.path(), false) {
            Ok(m) => catalogued.push(m),
            Err(why) => uncatalogued.push(why),
        }
    }
    for entry in fs::read_dir(root)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if ![SNAPSHOTS, STAGING, OPS, TRASH, AUDIT].contains(&name.as_str()) {
            uncatalogued.push(format!("{name}: not owned by this tool"));
        }
    }
    catalogued.sort_by(|a, b| a.backup_id.cmp(&b.backup_id));
    Ok(Inventory {
        catalogued,
        uncatalogued,
        on_disk,
    })
}

/// Re-synchronises the database projection with the backup root (the
/// truth). `retention`: the errors of a retention run that just ended.
async fn project(
    db: &PgPool,
    inv: &Inventory,
    grace: time::Duration,
    retention: Option<&[String]>,
) -> Result<(), BackupError> {
    let mut tx = db.begin().await.map_err(dbe)?;
    for m in &inv.catalogued {
        let tenants: Vec<Uuid> = m.tenants.iter().map(|t| t.workspace_id).collect();
        sqlx::query(
            "INSERT INTO backup_snapshots (backup_id, snapshot_at, normal_expires_at, tenants)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (backup_id) DO UPDATE SET removed_at = NULL",
        )
        .bind(&m.backup_id)
        .bind(parse_rfc(&m.snapshot_at))
        .bind(parse_rfc(&m.normal_expires_at))
        .bind(&tenants)
        .execute(&mut *tx)
        .await
        .map_err(dbe)?;
    }
    // Removed only once its directory is gone: a snapshot that stops
    // validating is uncatalogued (status `unknown`), not removed.
    sqlx::query(
        "UPDATE backup_snapshots SET removed_at = now()
          WHERE removed_at IS NULL AND NOT (backup_id = ANY($1))",
    )
    .bind(&inv.on_disk)
    .execute(&mut *tx)
    .await
    .map_err(dbe)?;
    let error = retention.and_then(|e| (!e.is_empty()).then(|| e.join("; ")));
    sqlx::query(
        "INSERT INTO backup_inventory
             (id, inventoried_at, uncatalogued, overdue_grace_secs, last_error, last_retention_at)
         VALUES (1, now(), $1, $2, $3, CASE WHEN $4 THEN now() END)
         ON CONFLICT (id) DO UPDATE SET
             inventoried_at = now(),
             uncatalogued = EXCLUDED.uncatalogued,
             overdue_grace_secs = EXCLUDED.overdue_grace_secs,
             last_error = CASE WHEN $4 THEN EXCLUDED.last_error ELSE backup_inventory.last_error END,
             last_retention_at = CASE WHEN $4 THEN now() ELSE backup_inventory.last_retention_at END",
    )
    .bind(inv.uncatalogued.len() as i32)
    .bind(grace.whole_seconds())
    .bind(error.map(|e| e.chars().take(500).collect::<String>()))
    .bind(retention.is_some())
    .execute(&mut *tx)
    .await
    .map_err(dbe)?;
    tx.commit().await.map_err(dbe)
}

fn remove_snapshot(root: &Path, id: &str, op: Uuid) -> std::io::Result<()> {
    let from = root.join(SNAPSHOTS).join(id);
    let to = root.join(TRASH).join(format!("{id}.{op}"));
    fs::rename(&from, &to)?;
    sync_dir(&root.join(SNAPSHOTS))?;
    sync_dir(&root.join(TRASH))?;
    remove_no_follow(&to)?;
    sync_dir(&root.join(TRASH))
}

/// `retention run`: removes catalogued, published snapshots whose
/// effective expiry — `min(normal expiry, the backup cap of every current
/// deletion request of one of its tenants)` — has passed. Never extends,
/// never touches anything uncatalogued; the result is audited.
pub async fn run_retention(s: &Settings, db: &PgPool) -> Result<serde_json::Value, BackupError> {
    layout(&s.root).map_err(io("backup root"))?;
    let op =
        Op::register(&s.root, "retention", || Ok(())).map_err(io("registering the operation"))?;
    let cleaned = clean_dead(&s.root, op.id).map_err(io("cleaning dead operations"))?;
    let inv = inventory(&s.root).map_err(io("inventory"))?;
    let caps: BTreeMap<Uuid, OffsetDateTime> = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
        "SELECT workspace_id, backup_cap_at FROM workspace_lifecycle",
    )
    .fetch_all(db)
    .await
    .map_err(dbe)?
    .into_iter()
    .collect();
    let now = OffsetDateTime::now_utc();
    let (mut removed, mut kept, mut errors) = (Vec::new(), Vec::new(), Vec::new());
    for m in &inv.catalogued {
        let normal = parse_rfc(&m.normal_expires_at).expect("validated");
        let expires = m
            .tenants
            .iter()
            .filter_map(|t| caps.get(&t.workspace_id).copied())
            .fold(normal, OffsetDateTime::min);
        if expires > now || removed.len() >= MAX_REMOVALS {
            kept.push(m.backup_id.clone());
            continue;
        }
        let root = s.root.clone();
        let (id, op_id) = (m.backup_id.clone(), op.id);
        match tokio::task::spawn_blocking(move || remove_snapshot(&root, &id, op_id)).await {
            Ok(Ok(())) => removed.push(m.backup_id.clone()),
            Ok(Err(e)) => errors.push(format!("{}: {e}", m.backup_id)),
            Err(e) => errors.push(format!("{}: {e}", m.backup_id)),
        }
    }
    let after = inventory(&s.root).map_err(io("inventory"))?;
    let projection = project(db, &after, s.overdue_grace, Some(&errors)).await;
    if let Err(e) = &projection {
        errors.push(format!("projection: {e}"));
    }
    let ran_at = OffsetDateTime::now_utc();
    let result = serde_json::json!({
        "format": "muster-retention",
        "version": 1,
        "op_id": op.id,
        "ran_at": rfc(ran_at),
        "removed": removed,
        "kept": kept,
        "uncatalogued": after.uncatalogued.len(),
        "uncatalogued_entries": after.uncatalogued,
        "dead_operations_cleaned": cleaned,
        "errors": errors,
    });
    let audit_name = format!(
        "retention-{}-{}.json",
        &backup_id(ran_at, op.id)[..16],
        op.id
    );
    write_atomic(
        &s.root.join(AUDIT),
        &audit_name,
        &serde_json::to_vec_pretty(&result).map_err(|e| BackupError::Io(e.to_string()))?,
    )
    .map_err(io("audit"))?;
    op.finish().map_err(io("operation"))?;
    if !errors.is_empty() {
        return Err(BackupError::Io(format!("retention: {}", result)));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Lifecycle status (read by the service)
// ---------------------------------------------------------------------------

/// The backup side of a deletion, from the projection.
pub struct BackupView {
    pub status: &'static str,
    pub deadline: Option<OffsetDateTime>,
    pub purged_at: Option<OffsetDateTime>,
    pub reason: Option<String>,
    pub last_error: Option<String>,
}

type InventoryRow = (i32, Option<String>, i64);

/// `None`: no inventory yet (the caller reports its own `unknown`).
pub async fn view<'e, E>(
    db: E,
    ws: Uuid,
    now: OffsetDateTime,
) -> Result<Option<BackupView>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e> + Copy,
{
    let inv: Option<InventoryRow> = sqlx::query_as(
        "SELECT uncatalogued, last_error, overdue_grace_secs FROM backup_inventory WHERE id = 1",
    )
    .fetch_optional(db)
    .await?;
    let Some((uncatalogued, last_error, grace)) = inv else {
        return Ok(None);
    };
    if uncatalogued > 0 {
        return Ok(Some(BackupView {
            status: "unknown",
            deadline: None,
            purged_at: None,
            reason: Some(format!(
                "{uncatalogued} uncatalogued backup entries: no deadline can be promised for them"
            )),
            last_error: None,
        }));
    }
    let rows: Vec<(Option<OffsetDateTime>, OffsetDateTime)> = sqlx::query_as(
        "SELECT s.removed_at,
                LEAST(s.normal_expires_at,
                      COALESCE((SELECT min(l.backup_cap_at) FROM workspace_lifecycle l
                                 WHERE l.workspace_id = ANY(s.tenants)), 'infinity'))
           FROM backup_snapshots s WHERE $1 = ANY(s.tenants)",
    )
    .bind(ws)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(Some(BackupView {
            status: "none",
            deadline: None,
            purged_at: None,
            reason: Some("no catalogued snapshot holds this workspace".into()),
            last_error: None,
        }));
    }
    let deadline = rows.iter().filter(|r| r.0.is_none()).map(|r| r.1).max();
    let Some(deadline) = deadline else {
        return Ok(Some(BackupView {
            status: "purged",
            deadline: None,
            purged_at: rows.iter().filter_map(|r| r.0).max(),
            reason: None,
            last_error: None,
        }));
    };
    let status = if now < deadline {
        "scheduled"
    } else if last_error.is_some() {
        "error"
    } else if now > deadline + time::Duration::seconds(grace) {
        "overdue"
    } else {
        "due"
    };
    Ok(Some(BackupView {
        status,
        deadline: Some(deadline),
        purged_at: None,
        reason: None,
        last_error: if status == "error" { last_error } else { None },
    }))
}
