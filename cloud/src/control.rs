//! Lifecycle authority: the durable record of facts that must survive the
//! restore of any older database dump or Git snapshot (later: deletions,
//! tombstones, backup inventory). Lives OUTSIDE the tenant database, the
//! repository root and backups:
//!
//! - a hash-chained, append-only **journal** in `MUSTER_CONTROL_DIR`
//!   (`journal.v1.jsonl`, one JSON record per line, `prev` = SHA-256 of the
//!   previous line), guarded by an exclusive file lock (`control.lock`);
//! - an independent **witness** file (`MUSTER_CONTROL_WITNESS`, on another
//!   path/mount): `{deployment_id, control_store_id, sequence,
//!   journal_hash}` of the last acknowledged record;
//! - in each database, a **binding** (`control_binding`): deployment, store,
//!   that database's own incarnation and how far it projected the journal.
//!
//! Commit order of a record: journal append + fsync → witness replaced
//! atomically + directory fsync → database projection → acknowledgement. On
//! open, a journal ahead of the witness is accepted only if its prefix at
//! the witness sequence has the witnessed hash (then the witness advances);
//! a journal behind, divergent, from another store, or either file missing
//! fails closed. A database behind the journal replays before use; one
//! ahead of it or divergent fails closed. Nothing here ever creates an
//! authority implicitly: only [`Authority::init`] (ops, pristine targets).
//!
//! Physical assumption: losing or rolling back the journal AND the witness
//! together, with an equally old database, is undetectable; keep the
//! witness on a different device than the control directory, and both out
//! of backups and restores.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

const JOURNAL: &str = "journal.v1.jsonl";
const LOCK: &str = "control.lock";
const SCHEMA: u32 = 1;
/// Longest wait for the cross-process control lock before refusing.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
const GENESIS_PREV: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Where the authority lives (validated absolute and disjoint by config).
#[derive(Debug, Clone)]
pub struct ControlPaths {
    pub dir: PathBuf,
    pub witness: PathBuf,
}

/// Why the authority refused, with the exit code ops tools use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// Missing, stale, divergent or foreign authority/witness/binding: the
    /// service must not serve and a restore must not proceed.
    FailClosed(String),
    /// `init` on targets that already hold data or an authority.
    NotPristine(String),
    /// Bad arguments (unknown incarnation, wrong target).
    Usage(String),
    Io(String),
}

impl ControlError {
    pub fn exit_code(&self) -> u8 {
        match self {
            ControlError::Usage(_) => 64,
            ControlError::FailClosed(_) => 65,
            ControlError::NotPristine(_) => 66,
            ControlError::Io(_) => 74,
        }
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::FailClosed(m) => write!(f, "lifecycle authority refused: {m}"),
            ControlError::NotPristine(m) => write!(f, "not pristine: {m}"),
            ControlError::Usage(m) => write!(f, "{m}"),
            ControlError::Io(m) => write!(f, "control store I/O: {m}"),
        }
    }
}

impl std::error::Error for ControlError {}

fn io(context: &str) -> impl Fn(std::io::Error) -> ControlError + '_ {
    move |e| ControlError::Io(format!("{context}: {e}"))
}

fn db_err(e: sqlx::Error) -> ControlError {
    ControlError::Io(format!("database: {e}"))
}

fn fail(m: impl Into<String>) -> ControlError {
    ControlError::FailClosed(m.into())
}

/// One journal record (schema v1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Genesis {
        deployment_id: Uuid,
        control_store_id: Uuid,
    },
    /// A database joins the authority as a candidate incarnation.
    IncarnationRegistered { incarnation_id: Uuid },
    /// The incarnation that may serve; every other one is fenced.
    IncarnationActivated { incarnation_id: Uuid },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    v: u32,
    seq: u64,
    prev: String,
    /// Nested, not flattened: serde cannot combine `flatten` with
    /// `deny_unknown_fields`, and the schema must stay strict.
    record: Record,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Witness {
    v: u32,
    deployment_id: Uuid,
    control_store_id: Uuid,
    sequence: u64,
    journal_hash: String,
}

/// Crash injection points for recovery tests (ops binary, development only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    AfterJournal,
    AfterWitness,
}

impl CrashPoint {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "after_journal" => Some(Self::AfterJournal),
            "after_witness" => Some(Self::AfterWitness),
            _ => None,
        }
    }
}

fn crash_if(at: Option<CrashPoint>, here: CrashPoint) {
    if at == Some(here) {
        eprintln!("muster-cloud-ops: injected crash at {here:?}");
        std::process::abort();
    }
}

/// Role of a database incarnation according to the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Active,
    Candidate,
    Fenced,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbStatus {
    pub incarnation_id: Uuid,
    pub applied_sequence: u64,
    pub role: Role,
    /// Records projected by this call (0 when already current).
    pub replayed: u64,
}

/// Versioned, machine-readable state (`muster-cloud-ops status`).
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub v: u32,
    pub deployment_id: Uuid,
    pub control_store_id: Uuid,
    pub sequence: u64,
    pub journal_hash: String,
    pub active_incarnation: Option<Uuid>,
    /// Witness sequence before this open advanced it over a validated
    /// journal prefix (a crash between journal and witness), if it did.
    pub witness_advanced_from: Option<u64>,
    pub truncated_torn_bytes: u64,
    pub db: Option<DbStatus>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn sync_dir(dir: &Path) -> Result<(), ControlError> {
    fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(io("syncing directory"))
}

/// Replaces `path` atomically and durably with `bytes`.
fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), ControlError> {
    let parent = path
        .parent()
        .ok_or_else(|| ControlError::Io(format!("{} has no parent", path.display())))?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        Uuid::new_v4()
    ));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(io("creating temporary file"))?;
    f.write_all(bytes).map_err(io("writing temporary file"))?;
    f.sync_all().map_err(io("syncing temporary file"))?;
    drop(f);
    fs::rename(&tmp, path).map_err(io("replacing file"))?;
    sync_dir(parent)
}

/// The journal as read: complete, verified entries with their raw lines.
struct Journal {
    lines: Vec<String>,
    entries: Vec<Entry>,
    /// Bytes after the last newline (a torn, never-acknowledged append).
    torn: usize,
}

impl Journal {
    fn hash_at(&self, seq: u64) -> Option<String> {
        let i = usize::try_from(seq).ok()?.checked_sub(1)?;
        self.lines.get(i).map(|l| sha256_hex(l.as_bytes()))
    }

    fn head(&self) -> (u64, String) {
        let seq = self.entries.len() as u64;
        (
            seq,
            self.hash_at(seq).unwrap_or_else(|| GENESIS_PREV.into()),
        )
    }

    fn ids(&self) -> Result<(Uuid, Uuid), ControlError> {
        match self.entries.first().map(|e| &e.record) {
            Some(Record::Genesis {
                deployment_id,
                control_store_id,
            }) => Ok((*deployment_id, *control_store_id)),
            _ => Err(fail("control journal has no genesis record")),
        }
    }

    fn active(&self) -> Option<Uuid> {
        self.entries.iter().rev().find_map(|e| match e.record {
            Record::IncarnationActivated { incarnation_id } => Some(incarnation_id),
            _ => None,
        })
    }

    fn role_of(&self, inc: Uuid) -> Result<Role, ControlError> {
        let mut known = false;
        let mut was_active = false;
        for e in &self.entries {
            match e.record {
                Record::IncarnationRegistered { incarnation_id } if incarnation_id == inc => {
                    known = true
                }
                Record::IncarnationActivated { incarnation_id } if incarnation_id == inc => {
                    known = true;
                    was_active = true;
                }
                _ => {}
            }
        }
        if !known {
            return Err(fail(format!(
                "database incarnation {inc} is unknown to this authority"
            )));
        }
        Ok(match self.active() {
            Some(a) if a == inc => Role::Active,
            _ if was_active => Role::Fenced,
            _ => Role::Candidate,
        })
    }

    fn read(path: &Path) -> Result<Self, ControlError> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(fail(format!(
                    "control journal missing ({})",
                    path.display()
                )))
            }
            Err(e) => return Err(io("reading journal")(e)),
        };
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let text = std::str::from_utf8(&bytes[..complete])
            .map_err(|_| fail("control journal is not UTF-8"))?;
        let mut lines = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let entry: Entry = serde_json::from_str(line)
                .map_err(|e| fail(format!("control journal record {} unreadable: {e}", i + 1)))?;
            let prev = lines
                .last()
                .map(|l: &String| sha256_hex(l.as_bytes()))
                .unwrap_or_else(|| GENESIS_PREV.to_string());
            let genesis_ok = matches!(entry.record, Record::Genesis { .. }) == (i == 0);
            if entry.v != SCHEMA || entry.seq != i as u64 + 1 || entry.prev != prev || !genesis_ok {
                return Err(fail(format!(
                    "control journal record {} breaks the chain",
                    i + 1
                )));
            }
            lines.push(line.to_string());
            entries.push(entry);
        }
        if entries.is_empty() {
            return Err(fail("control journal is empty"));
        }
        Ok(Self {
            lines,
            entries,
            torn: bytes.len() - complete,
        })
    }
}

fn read_witness(path: &Path) -> Result<Witness, ControlError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(fail(format!(
                "control witness missing ({})",
                path.display()
            )))
        }
        Err(e) => return Err(io("reading witness")(e)),
    };
    let w: Witness = serde_json::from_slice(&bytes)
        .map_err(|e| fail(format!("control witness unreadable: {e}")))?;
    if w.v != SCHEMA {
        return Err(fail("control witness has an unknown schema version"));
    }
    Ok(w)
}

fn write_witness(path: &Path, w: &Witness) -> Result<(), ControlError> {
    let bytes = serde_json::to_vec(w).map_err(|e| ControlError::Io(e.to_string()))?;
    replace_file(path, &bytes)
}

/// An open, reconciled authority holding the control-store lock.
pub struct Authority {
    paths: ControlPaths,
    journal: Journal,
    _lock: fs::File,
    witness_advanced_from: Option<u64>,
    truncated: u64,
}

fn lock(dir: &Path) -> Result<fs::File, ControlError> {
    let f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK))
        .map_err(io("opening control lock"))?;
    // Bounded wait: a holder stuck elsewhere (e.g. an ops command waiting on
    // its database) must turn into a refusal, never a hung probe or command.
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(fail(format!(
                    "control store busy: locked by another process for over {}s",
                    LOCK_WAIT.as_secs()
                )))
            }
            Err(fs::TryLockError::Error(e)) => return Err(io("locking control store")(e)),
        }
    }
}

impl Authority {
    /// Opens and reconciles journal and witness (see the module docs).
    pub fn open(paths: &ControlPaths) -> Result<Self, ControlError> {
        if !paths.dir.is_dir() {
            return Err(fail(format!(
                "control directory missing ({})",
                paths.dir.display()
            )));
        }
        let lock = lock(&paths.dir)?;
        let journal_path = paths.dir.join(JOURNAL);
        let mut journal = Journal::read(&journal_path)?;
        let witness = read_witness(&paths.witness)?;
        let (deployment_id, control_store_id) = journal.ids()?;
        if witness.deployment_id != deployment_id || witness.control_store_id != control_store_id {
            return Err(fail(
                "witness and journal belong to different deployments or control stores",
            ));
        }
        let (head_seq, _) = journal.head();
        if witness.sequence > head_seq {
            return Err(fail(format!(
                "control journal is behind its witness ({head_seq} < {}): rolled back or lost",
                witness.sequence
            )));
        }
        if journal.hash_at(witness.sequence).as_deref() != Some(witness.journal_hash.as_str()) {
            return Err(fail(format!(
                "control journal diverges from its witness at record {}",
                witness.sequence
            )));
        }
        let mut truncated = 0;
        if journal.torn > 0 {
            // Bytes after the last complete record were never acknowledged
            // (the witness only advances after a complete, synced record).
            let keep = fs::metadata(&journal_path)
                .map_err(io("reading journal size"))?
                .len()
                - journal.torn as u64;
            let f = fs::OpenOptions::new()
                .write(true)
                .open(&journal_path)
                .map_err(io("opening journal"))?;
            f.set_len(keep)
                .map_err(io("truncating torn journal tail"))?;
            f.sync_all().map_err(io("syncing journal"))?;
            truncated = journal.torn as u64;
            journal.torn = 0;
        }
        let mut witness_advanced_from = None;
        if witness.sequence < head_seq {
            let (seq, hash) = journal.head();
            write_witness(
                &paths.witness,
                &Witness {
                    sequence: seq,
                    journal_hash: hash,
                    ..witness.clone()
                },
            )?;
            witness_advanced_from = Some(witness.sequence);
        }
        Ok(Self {
            paths: paths.clone(),
            journal,
            _lock: lock,
            witness_advanced_from,
            truncated,
        })
    }

    pub fn status(&self) -> Status {
        let (deployment_id, control_store_id) = self.journal.ids().expect("verified on open");
        let (sequence, journal_hash) = self.journal.head();
        Status {
            v: SCHEMA,
            deployment_id,
            control_store_id,
            sequence,
            journal_hash,
            active_incarnation: self.journal.active(),
            witness_advanced_from: self.witness_advanced_from,
            truncated_torn_bytes: self.truncated,
            db: None,
        }
    }

    /// Appends one record: journal + fsync, then witness. The caller
    /// projects it into the database(s) afterwards.
    pub fn append(
        &mut self,
        record: Record,
        crash: Option<CrashPoint>,
    ) -> Result<u64, ControlError> {
        let (head, prev) = self.journal.head();
        let entry = Entry {
            v: SCHEMA,
            seq: head + 1,
            prev,
            record,
        };
        let line = serde_json::to_string(&entry).map_err(|e| ControlError::Io(e.to_string()))?;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(self.paths.dir.join(JOURNAL))
            .map_err(io("opening journal"))?;
        f.write_all(format!("{line}\n").as_bytes())
            .map_err(io("appending journal"))?;
        f.sync_all().map_err(io("syncing journal"))?;
        crash_if(crash, CrashPoint::AfterJournal);
        self.journal.lines.push(line);
        self.journal.entries.push(entry);
        let (deployment_id, control_store_id) = self.journal.ids()?;
        let (sequence, journal_hash) = self.journal.head();
        write_witness(
            &self.paths.witness,
            &Witness {
                v: SCHEMA,
                deployment_id,
                control_store_id,
                sequence,
                journal_hash,
            },
        )?;
        crash_if(crash, CrashPoint::AfterWitness);
        Ok(sequence)
    }

    /// Checks the database binding against the journal and, unless
    /// `read_only`, projects the records it is missing.
    pub async fn bind_db(&self, db: &PgPool, read_only: bool) -> Result<DbStatus, ControlError> {
        let (deployment_id, control_store_id) = self.journal.ids()?;
        let mut tx = db.begin().await.map_err(db_err)?;
        // A read-only gate takes no row lock (PostgreSQL refuses FOR UPDATE in a
        // read-only transaction); a writer serialises replays with it.
        let query = if read_only {
            "SELECT deployment_id, control_store_id, incarnation_id, applied_seq, applied_hash
               FROM control_binding WHERE singleton"
        } else {
            "SELECT deployment_id, control_store_id, incarnation_id, applied_seq, applied_hash
               FROM control_binding WHERE singleton FOR UPDATE"
        };
        let row: Option<(Uuid, Uuid, Uuid, i64, String)> =
            match sqlx::query_as(query).fetch_optional(&mut *tx).await {
                Ok(row) => row,
                // No binding table at all: a database from before the authority
                // (or never migrated). Refused like an unbound one, never migrated
                // here.
                Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P01") => None,
                Err(e) => return Err(db_err(e)),
            };
        let Some((dep, store, incarnation_id, applied, applied_hash)) = row else {
            return Err(fail(
                "database is not bound to a lifecycle authority (run muster-cloud-ops init or register)",
            ));
        };
        if dep != deployment_id || store != control_store_id {
            return Err(fail(
                "database belongs to another deployment or control store",
            ));
        }
        let applied = applied.max(0) as u64;
        let (head, head_hash) = self.journal.head();
        if applied > head {
            return Err(fail(format!(
                "database is ahead of the control journal ({applied} > {head}): journal rolled back or lost"
            )));
        }
        if self.journal.hash_at(applied).as_deref() != Some(applied_hash.as_str()) {
            return Err(fail(format!(
                "database projection diverges from the control journal at record {applied}"
            )));
        }
        let role = self.journal.role_of(incarnation_id)?;
        let mut replayed = 0;
        if applied < head && !read_only {
            // Unit 1 records carry no tenant projection beyond the applied
            // position; later record kinds are applied here, in order.
            sqlx::query(
                "UPDATE control_binding SET applied_seq = $1, applied_hash = $2, updated_at = now()
                  WHERE singleton",
            )
            .bind(head as i64)
            .bind(&head_hash)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            replayed = head - applied;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(DbStatus {
            incarnation_id,
            applied_sequence: applied + replayed,
            role,
            replayed,
        })
    }

    /// Joins a new, unbound database as a candidate incarnation.
    pub async fn register(
        &mut self,
        db: &PgPool,
        crash: Option<CrashPoint>,
    ) -> Result<DbStatus, ControlError> {
        let bound: Option<Uuid> =
            sqlx::query_scalar("SELECT incarnation_id FROM control_binding WHERE singleton")
                .fetch_optional(db)
                .await
                .map_err(db_err)?;
        if bound.is_some() {
            return Err(ControlError::Usage(
                "database is already bound to an authority".into(),
            ));
        }
        let incarnation_id = Uuid::new_v4();
        self.append(Record::IncarnationRegistered { incarnation_id }, crash)?;
        self.insert_binding(db, incarnation_id).await?;
        self.bind_db(db, false).await
    }

    /// Activates a registered incarnation (fencing every other one) and
    /// projects the activation into that incarnation's database.
    pub async fn activate(
        &mut self,
        incarnation_id: Uuid,
        db: &PgPool,
        crash: Option<CrashPoint>,
    ) -> Result<DbStatus, ControlError> {
        let target: Option<Uuid> =
            sqlx::query_scalar("SELECT incarnation_id FROM control_binding WHERE singleton")
                .fetch_optional(db)
                .await
                .map_err(db_err)?;
        if target != Some(incarnation_id) {
            return Err(ControlError::Usage(
                "--database-url is not the database of that incarnation".into(),
            ));
        }
        self.journal.role_of(incarnation_id)?;
        self.bind_db(db, false).await?;
        self.append(Record::IncarnationActivated { incarnation_id }, crash)?;
        self.bind_db(db, false).await
    }

    async fn insert_binding(&self, db: &PgPool, incarnation_id: Uuid) -> Result<(), ControlError> {
        let (deployment_id, control_store_id) = self.journal.ids()?;
        let (seq, hash) = self.journal.head();
        sqlx::query(
            "INSERT INTO control_binding (deployment_id, control_store_id, incarnation_id, applied_seq, applied_hash)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(deployment_id)
        .bind(control_store_id)
        .bind(incarnation_id)
        .bind(seq as i64)
        .bind(&hash)
        .execute(db)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Creates a new authority for pristine targets only (no accounts or
    /// workspaces in the database, no repositories under the repository
    /// root). Never called by the service.
    ///
    /// Order: journal, database binding, witness. The two initial journal
    /// lines are a pure function of the ids the binding holds, so an init
    /// interrupted after binding resumes only on its own database and in
    /// its own control directory; a journal that no database is bound to
    /// was never used and is replaced. A different pristine database can
    /// never join an authority (two active incarnations), and a bound one
    /// can never get a second copy of its authority elsewhere. Re-running on
    /// a complete, still-pristine authority is a no-op.
    pub async fn init(
        paths: &ControlPaths,
        db: &PgPool,
        repo_root: &Path,
        crash: Option<CrashPoint>,
    ) -> Result<Status, ControlError> {
        let not_pristine = |m: &str| ControlError::NotPristine(m.into());
        let tenants: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM users) + (SELECT count(*) FROM workspaces)",
        )
        .fetch_one(db)
        .await
        .map_err(db_err)?;
        if tenants > 0 {
            return Err(not_pristine(
                "the database already holds accounts or workspaces",
            ));
        }
        if let Ok(entries) = fs::read_dir(repo_root) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().ends_with(".git") {
                    return Err(not_pristine(
                        "the repository root already holds workspace repositories",
                    ));
                }
            }
        }
        fs::create_dir_all(&paths.dir).map_err(io("creating control directory"))?;
        if let Some(parent) = paths.witness.parent() {
            fs::create_dir_all(parent).map_err(io("creating witness directory"))?;
        }
        let journal_path = paths.dir.join(JOURNAL);
        let lock_guard = lock(&paths.dir)?;
        let binding: Option<(Uuid, Uuid, Uuid, i64, String)> = sqlx::query_as(
            "SELECT deployment_id, control_store_id, incarnation_id, applied_seq, applied_hash
             FROM control_binding WHERE singleton",
        )
        .fetch_optional(db)
        .await
        .map_err(db_err)?;
        let witness_exists = paths.witness.exists();

        let in_use = || not_pristine("the database is already bound to an authority in use");
        match binding {
            None if witness_exists => {
                return Err(not_pristine(
                    "a control witness already exists for an authority in use",
                ))
            }
            None => {
                // A journal without witness and without a bound database was
                // never used by anyone: replaced with a fresh authority.
                let (dep, store, inc) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
                let (text, head) = initial_journal(dep, store, inc)?;
                replace_file(&journal_path, text.as_bytes())?;
                sqlx::query(
                    "INSERT INTO control_binding
                       (deployment_id, control_store_id, incarnation_id, applied_seq, applied_hash)
                     VALUES ($1, $2, $3, 2, $4)",
                )
                .bind(dep)
                .bind(store)
                .bind(inc)
                .bind(&head)
                .execute(db)
                .await
                .map_err(db_err)?;
                crash_if(crash, CrashPoint::AfterJournal);
                write_witness(&paths.witness, &initial_witness(dep, store, head))?;
                crash_if(crash, CrashPoint::AfterWitness);
            }
            Some((dep, store, inc, seq, hash)) => {
                // Resume or no-op, only where this database's own initial
                // journal already is: never a second copy elsewhere.
                let (text, head) = initial_journal(dep, store, inc)?;
                let on_disk = fs::read(&journal_path).ok();
                if seq != 2 || hash != head || on_disk.as_deref() != Some(text.as_bytes()) {
                    return Err(in_use());
                }
                let witness = initial_witness(dep, store, head);
                if witness_exists {
                    if read_witness(&paths.witness).ok().as_ref() != Some(&witness) {
                        return Err(in_use());
                    }
                } else {
                    write_witness(&paths.witness, &witness)?;
                    crash_if(crash, CrashPoint::AfterWitness);
                }
            }
        }
        drop(lock_guard);
        let authority = Self::open(paths)?;
        let db_status = authority.bind_db(db, false).await?;
        Ok(Status {
            db: Some(db_status),
            ..authority.status()
        })
    }
}

fn initial_witness(deployment_id: Uuid, control_store_id: Uuid, journal_hash: String) -> Witness {
    Witness {
        v: SCHEMA,
        deployment_id,
        control_store_id,
        sequence: 2,
        journal_hash,
    }
}

/// The two initial journal lines (genesis + activation of the first
/// incarnation) and the head hash: deterministic in the three ids.
fn initial_journal(
    deployment_id: Uuid,
    control_store_id: Uuid,
    incarnation_id: Uuid,
) -> Result<(String, String), ControlError> {
    let mut text = String::new();
    let mut prev = GENESIS_PREV.to_string();
    for (i, record) in [
        Record::Genesis {
            deployment_id,
            control_store_id,
        },
        Record::IncarnationActivated { incarnation_id },
    ]
    .into_iter()
    .enumerate()
    {
        let line = serde_json::to_string(&Entry {
            v: SCHEMA,
            seq: i as u64 + 1,
            prev: prev.clone(),
            record,
        })
        .map_err(|e| ControlError::Io(e.to_string()))?;
        prev = sha256_hex(line.as_bytes());
        text.push_str(&line);
        text.push('\n');
    }
    Ok((text, prev))
}

/// What the service runs at startup and on every readiness probe: open and
/// reconcile the authority, then check (and catch up) its own database.
/// Serving requires the `active` role.
pub async fn check_for_service(paths: &ControlPaths, db: &PgPool) -> Result<Status, ControlError> {
    // `open` blocks: it waits on the cross-process control lock (held by
    // another probe or an ops command while it waits on PostgreSQL) and
    // does file I/O with fsync. Run it on the blocking pool so it can never
    // stall the async workers serving requests (with one worker, concurrent
    // probes would otherwise deadlock). The lock stays held, by the
    // returned value, across `bind_db`.
    let owned = paths.clone();
    let authority = tokio::task::spawn_blocking(move || Authority::open(&owned))
        .await
        .map_err(|e| ControlError::Io(format!("control store task: {e}")))??;
    let db_status = authority.bind_db(db, false).await?;
    if db_status.role != Role::Active {
        return Err(fail(
            format!(
                "this database incarnation is {:?}, not active",
                db_status.role
            )
            .to_lowercase(),
        ));
    }
    Ok(Status {
        db: Some(db_status),
        ..authority.status()
    })
}
