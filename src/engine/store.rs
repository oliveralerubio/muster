//! Generations in local state: `<state>/generations/<n>/`.
//!
//! - `generation.json`: metadata, ownership records per managed file, the
//!   hooks/MCP servers approved per target, and the backups list.
//! - `files/<i>`: exact bytes of every managed file after generation `n`.
//! - `backup/<i>`: bytes a file had right before generation `n` overwrote it.
//!
//! Everything is 0600 in 0700 directories: snapshots may hold resolved
//! secrets. A generation is written under `<n>.partial-<pid>` and renamed to
//! `<n>` last, so a crash never leaves a half-written generation behind.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapters::FileFormat;
use crate::errors::MusterError;

const DIR: &str = "generations";
const META: &str = "generation.json";

/// Ownership record of one managed file (in memory, with its contents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub agent: String,
    /// `global` or the project path.
    pub target: String,
    pub format: FileFormat,
    /// Keys Muster wrote (JSON/TOML key paths; `["block"]` for Markdown).
    pub owned: Vec<Vec<String>>,
    /// Holds resolved secrets: kept 0600 and never shown in diffs.
    pub private: bool,
    /// Muster created the file (it did not exist before the first apply).
    pub created: bool,
    /// Written executable (managed dotfiles).
    pub executable: bool,
    pub contents: String,
}

/// Hooks, MCP server definitions and agent versions the user approved, per target.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Approved {
    #[serde(default)]
    pub hooks: BTreeMap<String, Value>,
    #[serde(default)]
    pub mcp: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, String>,
}

/// Applied state of a machine as of one generation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// Absolute path -> record.
    pub files: BTreeMap<String, Record>,
    /// Target -> approvals.
    pub approved: BTreeMap<String, Approved>,
}

#[derive(Serialize, Deserialize)]
struct DiskRecord {
    agent: String,
    target: String,
    format: FileFormat,
    owned: Vec<Vec<String>>,
    private: bool,
    created: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    executable: bool,
    snapshot: String,
}

#[derive(Serialize, Deserialize)]
struct DiskBackup {
    path: String,
    file: String,
}

#[derive(Serialize, Deserialize)]
struct DiskGeneration {
    number: u64,
    created_at: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollback_of: Option<u64>,
    files: BTreeMap<String, DiskRecord>,
    approved: BTreeMap<String, Approved>,
    backups: Vec<DiskBackup>,
}

fn root(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR)
}

/// Committed generation numbers, ascending.
pub fn numbers(state_dir: &Path) -> Result<Vec<u64>, MusterError> {
    let dir = root(state_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_error(&dir, e)),
    };
    let mut numbers: Vec<u64> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    numbers.sort_unstable();
    Ok(numbers)
}

/// Metadata of every committed generation, ascending: number, creation time,
/// kind and rollback source. Never file contents (snapshots may hold secrets).
pub fn summaries(state_dir: &Path) -> Result<Vec<Value>, MusterError> {
    let mut out = Vec::new();
    for n in numbers(state_dir)? {
        let meta_path = root(state_dir).join(n.to_string()).join(META);
        let text = std::fs::read_to_string(&meta_path).map_err(|e| io_error(&meta_path, e))?;
        let disk: DiskGeneration = serde_json::from_str(&text).map_err(|e| {
            MusterError::Operational(format!("{}: corrupt generation: {e}", meta_path.display()))
        })?;
        out.push(serde_json::json!({
            "number": disk.number,
            "created_at": disk.created_at,
            "kind": disk.kind,
            "rollback_of": disk.rollback_of,
            "files": disk.files.keys().collect::<Vec<_>>(),
        }));
    }
    Ok(out)
}

/// The most recent generation and its state.
pub fn latest(state_dir: &Path) -> Result<Option<(u64, State)>, MusterError> {
    match numbers(state_dir)?.last() {
        Some(&n) => Ok(Some((n, load(state_dir, n)?))),
        None => Ok(None),
    }
}

/// State recorded by generation `n`. A missing generation is a usage error.
pub fn load(state_dir: &Path, n: u64) -> Result<State, MusterError> {
    let dir = root(state_dir).join(n.to_string());
    let meta_path = dir.join(META);
    let text = match std::fs::read_to_string(&meta_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(MusterError::Usage(format!("generation {n} does not exist")))
        }
        Err(e) => return Err(io_error(&meta_path, e)),
    };
    let disk: DiskGeneration = serde_json::from_str(&text).map_err(|e| {
        MusterError::Operational(format!("{}: corrupt generation: {e}", meta_path.display()))
    })?;
    let mut files = BTreeMap::new();
    for (path, r) in disk.files {
        let snap = dir.join(&r.snapshot);
        let contents = std::fs::read_to_string(&snap).map_err(|e| io_error(&snap, e))?;
        files.insert(
            path,
            Record {
                agent: r.agent,
                target: r.target,
                format: r.format,
                owned: r.owned,
                private: r.private,
                created: r.created,
                executable: r.executable,
                contents,
            },
        );
    }
    Ok(State {
        files,
        approved: disk.approved,
    })
}

/// A generation being written.
pub struct Pending {
    state_dir: PathBuf,
    number: u64,
    dir: PathBuf,
    backups: Vec<DiskBackup>,
}

pub fn begin(state_dir: &Path) -> Result<Pending, MusterError> {
    let root = root(state_dir);
    create_private_dir(state_dir)?;
    create_private_dir(&root)?;
    let number = numbers(state_dir)?.last().map_or(1, |n| n + 1);
    let dir = root.join(format!("{number}.partial-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
    }
    create_private_dir(&dir)?;
    create_private_dir(&dir.join("files"))?;
    create_private_dir(&dir.join("backup"))?;
    Ok(Pending {
        state_dir: state_dir.to_path_buf(),
        number,
        dir,
        backups: Vec::new(),
    })
}

impl Pending {
    /// Keep the bytes `path` has before it is overwritten or removed.
    pub fn backup(&mut self, path: &Path, contents: &str) -> Result<(), MusterError> {
        let file = format!("backup/{}", self.backups.len());
        write_private(&self.dir.join(&file), contents)?;
        self.backups.push(DiskBackup {
            path: path.display().to_string(),
            file,
        });
        Ok(())
    }

    /// Write snapshots + metadata and publish the generation.
    pub fn commit(
        self,
        kind: &str,
        rollback_of: Option<u64>,
        state: &State,
    ) -> Result<u64, MusterError> {
        let mut files = BTreeMap::new();
        for (i, (path, record)) in state.files.iter().enumerate() {
            let snapshot = format!("files/{i}");
            write_private(&self.dir.join(&snapshot), &record.contents)?;
            files.insert(
                path.clone(),
                DiskRecord {
                    agent: record.agent.clone(),
                    target: record.target.clone(),
                    format: record.format,
                    owned: record.owned.clone(),
                    private: record.private,
                    created: record.created,
                    executable: record.executable,
                    snapshot,
                },
            );
        }
        let created_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let disk = DiskGeneration {
            number: self.number,
            created_at,
            kind: kind.to_string(),
            rollback_of,
            files,
            approved: state.approved.clone(),
            backups: self.backups,
        };
        let text = serde_json::to_string_pretty(&disk).expect("generation serializes");
        write_private(&self.dir.join(META), &text)?;
        let final_dir = root(&self.state_dir).join(self.number.to_string());
        std::fs::rename(&self.dir, &final_dir).map_err(|e| io_error(&final_dir, e))?;
        Ok(self.number)
    }
}

fn create_private_dir(dir: &Path) -> Result<(), MusterError> {
    std::fs::create_dir_all(dir).map_err(|e| io_error(dir, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io_error(dir, e))?;
    }
    Ok(())
}

fn write_private(path: &Path, contents: &str) -> Result<(), MusterError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(|e| io_error(path, e))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| io_error(path, e))
}

fn io_error(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}
