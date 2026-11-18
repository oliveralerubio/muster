//! Primary purge of workspaces whose deletion grace has expired.
//!
//! Order (each step idempotent; a crash or failure at any point is resumed
//! by the next pass, in this or any other process):
//!
//! 1. **Tombstone first**: `workspace_tombstoned` is journaled in the
//!    lifecycle authority (outside the database and its backups) and
//!    projected under the workspace row lock. Its projection ends every
//!    access: memberships and invitations deleted, devices revoked, cached
//!    account responses redacted. Irreversible.
//! 2. **Drain**: the workspace's cross-process lock
//!    (`<repo_root>/locks/<ws>.lock`) is taken exclusive. Every Git reader,
//!    stager and writer holds it shared ([`repo_access`]) and checks the
//!    tombstone after acquiring it, so once the purge holds it none is left
//!    and none can start.
//! 3. **Owned trash**: `<root>/<ws>.git` is renamed to
//!    `<root>/trash/<ws>.<request_id>` (the rename moves the entry itself,
//!    never what a symlink points to).
//! 4. **Database**: residual payloads redacted (`billing_events` of the
//!    workspace and of its provider items, `account_idempotency`), then the
//!    workspace row deleted (every tenant table cascades). Accounts,
//!    sessions, the lifecycle request and the provider cancellation items
//!    (no foreign key) stay.
//! 5. **Trash erasure** of the owned entries (no symlink is followed), and
//!    of the lock file.
//! 6. **Verified completion**: no residue in the database or the root, then
//!    `workspace_purged` is journaled.
//!
//! A restored database older than the journal gets the tombstone and the
//! purge replayed (projection) and [`reconcile_before_serving`] erases its
//! resurrected rows and repository before the service serves.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::control::{self, ControlPaths, CrashPoint, Record};
use crate::error::AppError;

const LOCKS: &str = "locks";
const TRASH: &str = "trash";
const CAPTURE_LOCK: &str = "capture.lock";
/// Longest a Git request waits for its workspace lock (held exclusive only
/// by a purge).
const ACCESS_WAIT: Duration = Duration::from_secs(10);
const MAX_BACKOFF_SECS: i64 = 300;

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
pub(crate) const O_NOFOLLOW: i32 = 0o400000;
#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "arm")))]
pub(crate) const O_NOFOLLOW: i32 = 0o100000;
#[cfg(target_os = "macos")]
pub(crate) const O_NOFOLLOW: i32 = 0x0100;

/// Development-only crash points (`MUSTER_DEV_PURGE_CRASH`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeCrash {
    /// Right after the tombstone is journaled, before its projection commits.
    AfterTombstone,
    AfterRename,
    AfterDb,
    AfterTrash,
}

impl PurgeCrash {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "after_tombstone" => Some(Self::AfterTombstone),
            "after_rename" => Some(Self::AfterRename),
            "after_db" => Some(Self::AfterDb),
            "after_trash" => Some(Self::AfterTrash),
            _ => None,
        }
    }
}

/// What a purge needs; shared by the service worker and `muster-cloud-ops`.
#[derive(Clone)]
pub struct PurgeCtx {
    pub db: PgPool,
    pub control: ControlPaths,
    pub repo_root: PathBuf,
    pub crash: Option<PurgeCrash>,
    /// How long one attempt waits for live Git access to drain.
    pub drain_wait: Duration,
    /// `false` for a candidate incarnation (a restore being reconciled):
    /// it erases what the journal already tombstoned, never journals.
    pub journal: bool,
    /// Development only (`MUSTER_DEV_RESTORE_ERASE_FAIL` for
    /// `restore-finalize`, refused in production): every erase attempt
    /// fails before touching anything, to test that a restore whose
    /// reconciliation cannot erase a tenant fails closed.
    pub dev_fail_erase: bool,
    /// The service's fencing gate: a fenced instance's worker claims and
    /// erases nothing. `None` for ops commands, which check the role first.
    pub gate: Option<std::sync::Arc<crate::control::RoleGate>>,
}

async fn fenced(ctx: &PurgeCtx) -> bool {
    match &ctx.gate {
        Some(gate) => !gate.allows(&ctx.control, &ctx.db, "purge").await,
        None => false,
    }
}

fn crash_if(ctx: &PurgeCtx, here: PurgeCrash) {
    if ctx.crash == Some(here) {
        crate::dev_crash::die(format_args!("injected purge crash at {here:?}"));
    }
}

fn io_err(context: &str) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{context}: {e}")
}

/// `root/name` as a real directory owned by the service: created when
/// missing, refused when it is a symlink or anything else.
fn owned_dir(root: &Path, name: &str) -> std::io::Result<PathBuf> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_dir() => Ok(path),
        Ok(_) => Err(std::io::Error::other(format!(
            "{} is not a real directory",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            match fs::symlink_metadata(&path) {
                Ok(m) if m.file_type().is_dir() => Ok(path),
                _ => Err(std::io::Error::other(format!(
                    "{} is not a real directory",
                    path.display()
                ))),
            }
        }
        Err(e) => Err(e),
    }
}

/// Opens (creating) the workspace lock file without following symlinks.
fn open_lock(root: &Path, ws: Uuid) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = owned_dir(root, LOCKS)?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(O_NOFOLLOW)
        .open(dir.join(format!("{ws}.lock")))
}

async fn is_tombstoned(db: &PgPool, ws: Uuid) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT purge_committed_at IS NOT NULL FROM workspace_lifecycle WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(db)
    .await?
    .unwrap_or(false))
}

/// The workspace exists and is not tombstoned.
async fn is_live(db: &PgPool, ws: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workspaces w WHERE w.id = $1)
            AND NOT EXISTS (SELECT 1 FROM workspace_lifecycle l
                             WHERE l.workspace_id = $1 AND l.purge_committed_at IS NOT NULL)",
    )
    .bind(ws)
    .fetch_one(db)
    .await
}

/// The open lock file is still the one at its path (a purge unlinks the
/// path while holding it; a holder of the old file must not count).
fn still_at_path(root: &Path, ws: Uuid, file: &fs::File) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let open = file.metadata()?;
    match fs::symlink_metadata(root.join(LOCKS).join(format!("{ws}.lock"))) {
        Ok(at) => Ok(at.dev() == open.dev() && at.ino() == open.ino()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Opens the lock file and takes it (`exclusive` or shared), waiting up to
/// `wait`; reopens when the path was replaced meanwhile. `None`: timed out.
async fn take_lock(
    root: &Path,
    ws: Uuid,
    exclusive: bool,
    wait: Duration,
) -> std::io::Result<Option<fs::File>> {
    let start = Instant::now();
    loop {
        let owned = root.to_path_buf();
        let file = tokio::task::spawn_blocking(move || open_lock(&owned, ws))
            .await
            .map_err(std::io::Error::other)??;
        loop {
            let got = if exclusive {
                file.try_lock()
            } else {
                file.try_lock_shared()
            };
            match got {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
                    if start.elapsed() > wait {
                        return Ok(None);
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        if still_at_path(root, ws, &file)? {
            return Ok(Some(file));
        }
        if start.elapsed() > wait {
            return Ok(None);
        }
    }
}

/// The backup/purge coordination lock, `<root>/locks/capture.lock`: a
/// capture holds it exclusive from its database snapshot through its Git
/// archive; a purge holds it shared from its move to trash until its trash
/// is erased. Never unlinked. `None`: not taken within `wait`.
pub async fn capture_lock(
    root: &Path,
    exclusive: bool,
    wait: Duration,
) -> std::io::Result<Option<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let owned = root.to_path_buf();
    let file = tokio::task::spawn_blocking(move || {
        let dir = owned_dir(&owned, LOCKS)?;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(O_NOFOLLOW)
            .open(dir.join(CAPTURE_LOCK))
    })
    .await
    .map_err(std::io::Error::other)??;
    let start = Instant::now();
    loop {
        let got = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match got {
            Ok(()) => return Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) if start.elapsed() > wait => return Ok(None),
            Err(fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// Shared hold on a workspace's Git storage for one request.
pub struct RepoAccess {
    _file: fs::File,
}

/// Takes the workspace lock shared (waiting while a purge holds it), then
/// refuses with 404 if the workspace is tombstoned. Hold the result for the
/// whole Git operation (a push: until its transaction commits).
/// Call it only after authorising the caller for `ws`.
pub async fn repo_access(db: &PgPool, root: &Path, ws: Uuid) -> Result<RepoAccess, AppError> {
    // Before touching the root: only existing, untombstoned workspaces get a
    // lock file.
    if !is_live(db, ws).await? {
        return Err(AppError::not_found());
    }
    let file = take_lock(root, ws, false, ACCESS_WAIT)
        .await
        .map_err(|e| AppError::internal(format!("workspace lock: {e}")))?
        .ok_or_else(|| AppError::not_ready("workspace storage is busy; retry"))?;
    if is_tombstoned(db, ws).await? {
        return Err(AppError::not_found());
    }
    Ok(RepoAccess { _file: file })
}

/// Residue of a workspace: counts only, never data.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Residue {
    pub tenant_rows: i64,
    pub tables: std::collections::BTreeMap<&'static str, i64>,
    pub unredacted_idempotency: i64,
    pub billing_event_payloads: i64,
    pub repo: bool,
    pub trash: usize,
    pub lock: bool,
}

impl Residue {
    pub fn any(&self) -> bool {
        self.tenant_rows > 0
            || self.unredacted_idempotency > 0
            || self.billing_event_payloads > 0
            || self.repo
            || self.trash > 0
            || self.lock
    }
}

const TENANT_TABLES: &[(&str, &str)] = &[
    (
        "workspaces",
        "SELECT count(*) FROM workspaces WHERE id = $1",
    ),
    (
        "memberships",
        "SELECT count(*) FROM memberships WHERE workspace_id = $1",
    ),
    (
        "invitations",
        "SELECT count(*) FROM invitations WHERE workspace_id = $1",
    ),
    (
        "devices",
        "SELECT count(*) FROM devices WHERE workspace_id = $1",
    ),
    (
        "workspace_heads",
        "SELECT count(*) FROM workspace_heads WHERE workspace_id = $1",
    ),
    (
        "accepted_revisions",
        "SELECT count(*) FROM accepted_revisions WHERE workspace_id = $1",
    ),
    (
        "sync_idempotency",
        "SELECT count(*) FROM sync_idempotency WHERE workspace_id = $1",
    ),
    (
        "proposals",
        "SELECT count(*) FROM proposals WHERE workspace_id = $1",
    ),
    (
        "receipts",
        "SELECT count(*) FROM receipts WHERE workspace_id = $1",
    ),
    (
        "subscriptions",
        "SELECT count(*) FROM subscriptions WHERE workspace_id = $1",
    ),
    (
        "billing_customers",
        "SELECT count(*) FROM billing_customers WHERE workspace_id = $1",
    ),
    (
        "billing_checkouts",
        "SELECT count(*) FROM billing_checkouts WHERE workspace_id = $1",
    ),
    (
        "billing_reconcile_jobs",
        "SELECT count(*) FROM billing_reconcile_jobs WHERE workspace_id = $1",
    ),
];

/// `billing_events` rows that belong to `ws`: associated with it, naming it
/// in `custom_data`, or about one of its provider objects ([`provider_ids`]).
const EVENTS_OF_WORKSPACE: &str = "(workspace_id = $1
    OR payload->'data'->'custom_data'->>'workspace_id' = $1::text
    OR payload->'data'->>'id' = ANY($2)
    OR payload->'data'->>'transaction_id' = ANY($2)
    OR payload->'data'->>'subscription_id' = ANY($2))";

/// Records every provider object id `ws` has now (its rows are about to be
/// erased): subscriptions of any status and checkout transactions.
const CAPTURE_PROVIDER_IDS: &str =
    "INSERT INTO lifecycle_purged_provider_ids (workspace_id, provider_id)
    SELECT $1, paddle_subscription_id FROM subscriptions WHERE workspace_id = $1
    UNION SELECT $1, transaction_id FROM billing_checkouts WHERE workspace_id = $1
    ON CONFLICT DO NOTHING";

async fn provider_ids<'e>(
    db: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    // Cancellation items, ids captured at the purge, and the live rows (not
    // yet erased).
    sqlx::query_scalar(
        "SELECT provider_id FROM lifecycle_provider_items WHERE workspace_id = $1
         UNION SELECT provider_id FROM lifecycle_purged_provider_ids WHERE workspace_id = $1
         UNION SELECT paddle_subscription_id FROM subscriptions WHERE workspace_id = $1
         UNION SELECT transaction_id FROM billing_checkouts WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_all(db)
    .await
}

fn trash_entries(root: &Path, ws: Uuid) -> std::io::Result<Vec<PathBuf>> {
    let trash = root.join(TRASH);
    match fs::symlink_metadata(&trash) {
        Ok(m) if m.file_type().is_dir() => {}
        Ok(_) => return Err(std::io::Error::other("trash is not a real directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    }
    let prefix = format!("{ws}.");
    let mut out = Vec::new();
    for entry in fs::read_dir(&trash)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Only names this service creates: `<ws>.<uuid>`.
        if let Some(rest) = name.strip_prefix(&prefix) {
            if Uuid::parse_str(rest).is_ok() {
                out.push(entry.path());
            }
        }
    }
    Ok(out)
}

fn exists_no_follow(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// What remains of `ws` in the database and under the repository root.
pub async fn residue(db: &PgPool, root: &Path, ws: Uuid) -> Result<Residue, String> {
    let dbe = |e: sqlx::Error| format!("database: {e}");
    let mut r = Residue::default();
    for (name, sql) in TENANT_TABLES {
        let n: i64 = sqlx::query_scalar(sql)
            .bind(ws)
            .fetch_one(db)
            .await
            .map_err(dbe)?;
        r.tenant_rows += n;
        r.tables.insert(name, n);
    }
    r.unredacted_idempotency = sqlx::query_scalar(
        "SELECT count(*) FROM account_idempotency
          WHERE (path LIKE '/v1/workspaces/' || $1::text || '%'
                 OR (path = '/v1/workspaces' AND response->>'id' = $1::text))
            AND response <> '{\"purged\": true}'::jsonb",
    )
    .bind(ws)
    .fetch_one(db)
    .await
    .map_err(dbe)?;
    let ids = provider_ids(db, ws).await.map_err(dbe)?;
    r.billing_event_payloads = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM billing_events WHERE {EVENTS_OF_WORKSPACE} AND payload <> '{{}}'::jsonb"
    ))
    .bind(ws)
    .bind(&ids)
    .fetch_one(db)
    .await
    .map_err(dbe)?;
    let root = root.to_path_buf();
    let (repo, trash, lock) = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        Ok((
            exists_no_follow(&root.join(format!("{ws}.git")))?,
            trash_entries(&root, ws)?.len(),
            exists_no_follow(&root.join(LOCKS).join(format!("{ws}.lock")))?,
        ))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(io_err("repository root"))?;
    r.repo = repo;
    r.trash = trash;
    r.lock = lock;
    Ok(r)
}

fn now_us() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1000) as i64
}

fn authority_err(e: control::ControlError) -> String {
    format!("lifecycle authority: {e}")
}

/// Step 1: the irreversible tombstone (journal, then projection).
async fn tombstone(ctx: &PurgeCtx, ws: Uuid, request_id: Uuid) -> Result<(), String> {
    let dbe = |e: sqlx::Error| format!("database: {e}");
    if !ctx.journal {
        // A candidate only erases what the journal already tombstoned
        // (projected by replay); it never records anything.
        return if is_tombstoned(&ctx.db, ws).await.map_err(dbe)? {
            Ok(())
        } else {
            Err("not tombstoned by the journal: left to the active incarnation".into())
        };
    }
    let mut tx = ctx.db.begin().await.map_err(dbe)?;
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(ws)
        .execute(&mut *tx)
        .await
        .map_err(dbe)?;
    let mut authority = control::open_for_write(&ctx.control, &mut tx)
        .await
        .map_err(authority_err)?;
    if !authority.tombstoned(ws) {
        let crash =
            (ctx.crash == Some(PurgeCrash::AfterTombstone)).then_some(CrashPoint::AfterJournal);
        authority
            .append_projected(
                &mut tx,
                Record::WorkspaceTombstoned {
                    workspace_id: ws,
                    request_id,
                    tombstoned_at_us: now_us(),
                },
                crash,
            )
            .await
            .map_err(authority_err)?;
    }
    tx.commit().await.map_err(dbe)?;
    drop(authority);
    Ok(())
}

/// Step 2: exclusive hold on the workspace lock, waiting at most
/// `drain_wait` for live Git access to finish.
async fn drain(ctx: &PurgeCtx, ws: Uuid) -> Result<fs::File, String> {
    take_lock(&ctx.repo_root, ws, true, ctx.drain_wait)
        .await
        .map_err(io_err("workspace lock"))?
        .ok_or_else(|| "waiting for live Git access to drain".into())
}

/// Step 3: move the repository into the owned trash.
fn to_trash(root: &Path, ws: Uuid, request_id: Uuid) -> std::io::Result<()> {
    let repo = root.join(format!("{ws}.git"));
    if !exists_no_follow(&repo)? {
        return Ok(());
    }
    let trash = owned_dir(root, TRASH)?;
    let dest = trash.join(format!("{ws}.{request_id}"));
    if exists_no_follow(&dest)? {
        remove_no_follow(&dest)?;
    }
    fs::rename(&repo, &dest)?;
    fs::File::open(root)?.sync_all()?;
    fs::File::open(&trash)?.sync_all()
}

/// Removes `path` itself: a directory tree without following any symlink
/// (std's `remove_dir_all` never follows them), or a file or symlink.
fn remove_no_follow(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Step 4: residual payloads, then every tenant row (cascade).
async fn erase_db(ctx: &PurgeCtx, ws: Uuid) -> Result<(), String> {
    let dbe = |e: sqlx::Error| format!("database: {e}");
    let mut tx = ctx.db.begin().await.map_err(dbe)?;
    sqlx::query(CAPTURE_PROVIDER_IDS)
        .bind(ws)
        .execute(&mut *tx)
        .await
        .map_err(dbe)?;
    let ids = provider_ids(&mut *tx, ws).await.map_err(dbe)?;
    sqlx::query(&format!(
        "UPDATE billing_events SET payload = '{{}}'::jsonb
          WHERE {EVENTS_OF_WORKSPACE} AND payload <> '{{}}'::jsonb"
    ))
    .bind(ws)
    .bind(&ids)
    .execute(&mut *tx)
    .await
    .map_err(dbe)?;
    sqlx::query(control::REDACT_ACCOUNT_IDEMPOTENCY)
        .bind(ws)
        .execute(&mut *tx)
        .await
        .map_err(dbe)?;
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(ws)
        .execute(&mut *tx)
        .await
        .map_err(dbe)?;
    tx.commit().await.map_err(dbe)
}

/// Step 5: owned trash entries and the lock file (held exclusive).
fn erase_trash(root: &Path, ws: Uuid) -> std::io::Result<()> {
    for entry in trash_entries(root, ws)? {
        remove_no_follow(&entry)?;
    }
    let lock = root.join(LOCKS).join(format!("{ws}.lock"));
    match fs::symlink_metadata(&lock) {
        Ok(m) if m.file_type().is_file() => fs::remove_file(&lock),
        _ => Ok(()),
    }
}

/// Step 6: verify, then journal `workspace_purged` (once).
async fn complete(ctx: &PurgeCtx, ws: Uuid, request_id: Uuid) -> Result<(), String> {
    let r = residue(&ctx.db, &ctx.repo_root, ws).await?;
    if r.any() {
        return Err(format!(
            "verification found residue: {} rows, {} cached responses, {} event payloads, repo {}, trash {}",
            r.tenant_rows, r.unredacted_idempotency, r.billing_event_payloads, r.repo, r.trash
        ));
    }
    let dbe = |e: sqlx::Error| format!("database: {e}");
    if !ctx.journal {
        return Ok(());
    }
    let mut tx = ctx.db.begin().await.map_err(dbe)?;
    let mut authority = control::open_for_write(&ctx.control, &mut tx)
        .await
        .map_err(authority_err)?;
    if !authority.purged(ws) {
        authority
            .append_projected(
                &mut tx,
                Record::WorkspacePurged {
                    workspace_id: ws,
                    request_id,
                    purged_at_us: now_us(),
                },
                None,
            )
            .await
            .map_err(authority_err)?;
    }
    tx.commit().await.map_err(dbe)?;
    drop(authority);
    Ok(())
}

/// Runs every step for one workspace whose grace has expired.
pub async fn purge_one(ctx: &PurgeCtx, ws: Uuid, request_id: Uuid) -> Result<(), String> {
    if ctx.dev_fail_erase {
        return Err("development fault: erase refused (MUSTER_DEV_RESTORE_ERASE_FAIL)".into());
    }
    tombstone(ctx, ws, request_id).await?;
    let lock = drain(ctx, ws).await?;
    // No backup capture may see this workspace half-erased: hold the
    // coordination lock from the move to trash until the trash is erased.
    let capture = capture_lock(&ctx.repo_root, false, ctx.drain_wait)
        .await
        .map_err(io_err("backup coordination lock"))?
        .ok_or("waiting for a running backup capture to finish")?;
    let root = ctx.repo_root.clone();
    tokio::task::spawn_blocking(move || to_trash(&root, ws, request_id))
        .await
        .map_err(|e| e.to_string())?
        .map_err(io_err("moving the repository to trash"))?;
    crash_if(ctx, PurgeCrash::AfterRename);
    erase_db(ctx, ws).await?;
    crash_if(ctx, PurgeCrash::AfterDb);
    let root = ctx.repo_root.clone();
    tokio::task::spawn_blocking(move || erase_trash(&root, ws))
        .await
        .map_err(|e| e.to_string())?
        .map_err(io_err("erasing the trash"))?;
    crash_if(ctx, PurgeCrash::AfterTrash);
    drop(capture);
    let result = complete(ctx, ws, request_id).await;
    drop(lock);
    result
}

/// One workspace handled by a pass.
#[derive(Debug, Clone, Serialize)]
pub struct PurgeOutcome {
    pub workspace_id: Uuid,
    pub request_id: Uuid,
    /// `purged`, or `purge_committed` / `due` with `last_error`.
    pub state: &'static str,
    pub last_error: Option<String>,
}

async fn state_of(db: &PgPool, ws: Uuid) -> &'static str {
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT purge_committed_at IS NOT NULL, purged_at IS NOT NULL
           FROM workspace_lifecycle WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    match row {
        Some((_, true)) => "purged",
        Some((true, false)) => "purge_committed",
        _ => "due",
    }
}

/// One pass: every due, unfinished purge, and every purged workspace whose
/// rows came back (a restored database). Failures are recorded on the
/// request and retried with backoff.
pub async fn run_due(ctx: &PurgeCtx) -> Vec<PurgeOutcome> {
    run_due_with(ctx, false).await
}

/// [`run_due`], also retrying now (ignoring the backoff) every workspace
/// already past its backup cap: a backup must not capture it, so the
/// capture purges it first (`muster-cloud-ops backup run`).
pub async fn run_due_past_cap(ctx: &PurgeCtx) -> Vec<PurgeOutcome> {
    run_due_with(ctx, true).await
}

async fn run_due_with(ctx: &PurgeCtx, past_cap_now: bool) -> Vec<PurgeOutcome> {
    let on_disk = match ids_on_disk(&ctx.repo_root).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "purge: reading the repository root failed");
            Vec::new()
        }
    };
    let due: Vec<(Uuid, Uuid, i32)> = match sqlx::query_as(&format!(
        "SELECT workspace_id, request_id, purge_attempts FROM workspace_lifecycle l
          WHERE primary_purge_at <= now()
            AND (purge_next_attempt_at <= now() OR ($2 AND backup_cap_at <= now()))
            AND (purged_at IS NULL OR {RESURRECTED})
          ORDER BY primary_purge_at LIMIT 20"
    ))
    .bind(&on_disk)
    .bind(past_cap_now)
    .fetch_all(&ctx.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "purge: listing due workspaces failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for (ws, request_id, attempts) in due {
        if fenced(ctx).await {
            break;
        }
        let result = purge_one(ctx, ws, request_id).await;
        let last_error = result.err();
        if let Some(why) = &last_error {
            let backoff = 2i64
                .saturating_pow((attempts + 1).min(12) as u32)
                .min(MAX_BACKOFF_SECS);
            tracing::warn!(workspace = %ws, attempt = attempts + 1, retry_in_secs = backoff,
                reason = %why, "purge: not complete, will retry");
            let _ = sqlx::query(
                "UPDATE workspace_lifecycle
                    SET purge_attempts = purge_attempts + 1, purge_last_error = $2,
                        purge_next_attempt_at = now() + make_interval(secs => $3)
                  WHERE workspace_id = $1",
            )
            .bind(ws)
            .bind(why.chars().take(300).collect::<String>())
            .bind(backoff as f64)
            .execute(&ctx.db)
            .await;
        } else {
            tracing::info!(workspace = %ws, "purge: workspace purged and verified");
            let _ = sqlx::query(
                "UPDATE workspace_lifecycle
                    SET purge_attempts = 0, purge_last_error = NULL, purge_next_attempt_at = now()
                  WHERE workspace_id = $1",
            )
            .bind(ws)
            .execute(&ctx.db)
            .await;
        }
        out.push(PurgeOutcome {
            workspace_id: ws,
            request_id,
            state: state_of(&ctx.db, ws).await,
            last_error,
        });
    }
    out
}

/// Background loop for the service.
pub async fn worker(ctx: PurgeCtx, every: Duration) {
    loop {
        if !fenced(&ctx).await {
            run_due(&ctx).await;
        }
        tokio::time::sleep(every).await;
    }
}

/// Before serving: every tombstoned workspace that still has residue (a
/// database or repository root restored from before its purge, or a purge
/// interrupted by a crash) is erased now. Fails when a workspace already
/// journaled as purged cannot be erased: the service must not serve it.
pub async fn reconcile_before_serving(ctx: &PurgeCtx) -> Result<usize, String> {
    let rows: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(
        "SELECT workspace_id, request_id, purged_at IS NOT NULL FROM workspace_lifecycle
          WHERE purge_committed_at IS NOT NULL",
    )
    .fetch_all(&ctx.db)
    .await
    .map_err(|e| format!("database: {e}"))?;
    let mut reconciled = 0;
    let mut stuck = Vec::new();
    for (ws, request_id, purged) in rows {
        if !residue(&ctx.db, &ctx.repo_root, ws).await?.any() {
            continue;
        }
        match purge_one(ctx, ws, request_id).await {
            Ok(()) => reconciled += 1,
            // A restore being reconciled (no journal): whatever cannot be
            // erased fails the restore, tombstoned or purged alike.
            Err(e) if !ctx.journal => stuck.push(format!("{ws}: {e}")),
            // Left to the worker (it selects it again: residue of a purged
            // workspace keeps /readyz at 503 until it is gone).
            Err(e) if purged => stuck.push(format!("{ws}: {e}")),
            Err(e) => tracing::warn!(workspace = %ws, reason = %e, "purge: resumes in the worker"),
        }
    }
    if stuck.is_empty() {
        Ok(reconciled)
    } else {
        Err(format!(
            "workspaces with restored data not erased yet: {}",
            stuck.join("; ")
        ))
    }
}

/// Whether a workspace journaled as purged has rows or files again
/// (restored and not yet erased): the service is not ready while it does.
pub async fn resurrected_present(db: &PgPool, root: &Path) -> Result<bool, String> {
    let on_disk = ids_on_disk(root).await?;
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM workspace_lifecycle l
                         WHERE l.purged_at IS NOT NULL AND {RESURRECTED})"
    ))
    .bind(&on_disk)
    .fetch_one(db)
    .await
    .map_err(|e| format!("database: {e}"))
}

/// A purged workspace (alias `l`) has something again: its row, or files
/// under the repository root (`$1`: ids found there, [`ids_on_disk`]).
const RESURRECTED: &str = "(EXISTS (SELECT 1 FROM workspaces w WHERE w.id = l.workspace_id)
    OR l.workspace_id = ANY($1))";

/// Workspace ids that have a repository, a trash entry or a lock file under
/// `root` (names only; nothing is followed or opened).
pub async fn ids_on_disk(root: &Path) -> Result<Vec<Uuid>, String> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<Vec<Uuid>> {
        let mut ids = Vec::new();
        let mut scan = |dir: &Path, parse: &dyn Fn(&str) -> Option<Uuid>| -> std::io::Result<()> {
            match fs::symlink_metadata(dir) {
                Ok(m) if m.file_type().is_dir() => {}
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            }
            for entry in fs::read_dir(dir)? {
                if let Some(id) = entry?.file_name().to_str().and_then(parse) {
                    ids.push(id);
                }
            }
            Ok(())
        };
        scan(&root, &|n| {
            n.strip_suffix(".git").and_then(|i| Uuid::parse_str(i).ok())
        })?;
        scan(&root.join(LOCKS), &|n| {
            n.strip_suffix(".lock")
                .and_then(|i| Uuid::parse_str(i).ok())
        })?;
        scan(&root.join(TRASH), &|n| {
            n.split_once('.').and_then(|(i, _)| Uuid::parse_str(i).ok())
        })?;
        ids.sort();
        ids.dedup();
        Ok(ids)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("repository root: {e}"))
}

/// The public residue report (`muster-cloud-ops lifecycle-report`): the
/// lifecycle state, counts and presence only, never data.
/// What the external journal says about one workspace (the source of truth;
/// the database is only its projection, possibly behind after a restore).
#[derive(Debug, Clone, Copy, Default)]
pub struct JournalState {
    pub request_id: Option<Uuid>,
    pub tombstoned: bool,
    pub purged: bool,
}

impl JournalState {
    pub fn of(authority: &control::Authority, ws: Uuid) -> Self {
        let request_id = match authority.deletion_request(ws) {
            Some(Record::WorkspaceDeletionRequested { request_id, .. }) => Some(*request_id),
            _ => None,
        };
        Self {
            request_id,
            tombstoned: authority.tombstoned(ws),
            purged: authority.purged(ws),
        }
    }
}

pub async fn report(
    db: &PgPool,
    root: &Path,
    ws: Uuid,
    journal: JournalState,
) -> Result<serde_json::Value, String> {
    let dbe = |e: sqlx::Error| format!("database: {e}");
    type Row = (
        Uuid,
        Option<OffsetDateTime>,
        Option<OffsetDateTime>,
        Option<String>,
        bool,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT request_id, purge_committed_at, purged_at, purge_last_error,
                primary_purge_at <= now()
           FROM workspace_lifecycle WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_optional(db)
    .await
    .map_err(dbe)?;
    let fmt = |t: Option<OffsetDateTime>| {
        t.and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
    };
    // The journal decides the state; the database adds dates and the
    // operational reason. `projection_behind`: the database has not
    // replayed the journal yet (a restore, before the service or
    // `purge-due` ran).
    let (db_requested, db_committed, db_purged) = match &row {
        None => (false, false, false),
        Some((_, c, p, ..)) => (true, c.is_some(), p.is_some()),
    };
    let projection_behind = (journal.request_id.is_some() && !db_requested)
        || (journal.tombstoned && !db_committed)
        || (journal.purged && !db_purged);
    let requested = journal.request_id.is_some() || db_requested;
    let lifecycle = if !requested {
        serde_json::json!({ "state": "active", "primary": null, "projection_behind": false })
    } else {
        let due = row.as_ref().is_some_and(|r| r.4);
        let primary = if journal.purged || db_purged {
            "purged"
        } else if journal.tombstoned || db_committed {
            "purge_committed"
        } else if due {
            "due"
        } else {
            "scheduled"
        };
        serde_json::json!({
            "state": "deletion_scheduled",
            "primary": primary,
            "request_id": journal.request_id.or(row.as_ref().map(|r| r.0)),
            "purge_committed_at": row.as_ref().and_then(|r| fmt(r.1)),
            "purged_at": row.as_ref().and_then(|r| fmt(r.2)),
            "last_error": row.as_ref().and_then(|r| r.3.clone()),
            "projection_behind": projection_behind,
        })
    };
    let (pending, confirmed): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status <> 'confirmed'), count(*) FILTER (WHERE status = 'confirmed')
           FROM lifecycle_provider_items WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(db)
    .await
    .map_err(dbe)?;
    let r = residue(db, root, ws).await?;
    Ok(serde_json::json!({
        "v": 1,
        "workspace_id": ws,
        "lifecycle": lifecycle,
        "db": {
            "tenant_rows": r.tenant_rows,
            "tables": r.tables,
            "unredacted_idempotency": r.unredacted_idempotency,
            "billing_event_payloads": r.billing_event_payloads,
        },
        "fs": { "repo": r.repo, "trash": r.trash, "lock": r.lock },
        "provider_items": { "pending": pending, "confirmed": confirmed },
        "residue": r.any(),
    }))
}
