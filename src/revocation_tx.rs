//! Revocation publication as a recoverable transaction.
//!
//! A revocation touches many files (every re-encrypted `.age` and
//! `.meta.json`, `muster.toml`, `fleet.state.json`). Writing them one by one
//! can leave a repo that is half revoked. Instead:
//!
//! 1. [`Txn::begin`] copies the security files into a shadow config directory
//!    under the state dir; the command re-encrypts and edits only the shadow.
//! 2. [`Txn::commit`] seals and validates the shadow, writes a journal (the
//!    commit point), then [`apply`] copies each file into place atomically,
//!    advances the pin and commits to git.
//! 3. [`recover`], called at the start of every command, finishes a journaled
//!    transaction (roll forward) or discards an unjournaled one (roll back).
//!    Nothing for the user to do.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet_state::{self, TrustAnchor, VerifyOptions};
use crate::manifest::{Manifest, FILE_NAME};
use crate::vault::write_atomic;

const ROOT: &str = "revocation-tx";
const SHADOW: &str = "config";
const JOURNAL: &str = "journal.json";
/// Security files the transaction owns, relative to the config dir.
const TRACKED_FILES: [&str; 2] = [FILE_NAME, fleet_state::STATE_FILE];
const TRACKED_DIRS: [&str; 2] = ["vault", "enroll"];

#[derive(Serialize, Deserialize)]
struct Journal {
    /// Relative paths to copy from the shadow into the config dir.
    write: Vec<String>,
    /// Relative paths to delete from the config dir.
    delete: Vec<String>,
    message: String,
    pin: TrustAnchor,
}

/// A staged revocation. Dropping it without [`Txn::commit`] discards it.
pub struct Txn {
    root: PathBuf,
    shadow: PathBuf,
}

fn root(state_dir: &Path) -> PathBuf {
    state_dir.join(ROOT)
}

fn io(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), MusterError> {
    fs::create_dir_all(to).map_err(|e| io(to, e))?;
    let entries = match fs::read_dir(from) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(from, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io(from, e))?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().map_err(|e| io(&src, e))?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).map_err(|e| io(&src, e))?;
        }
    }
    Ok(())
}

/// Every file below `dir`, as paths relative to `dir`.
fn files_under(dir: &Path, rel: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir.join(rel)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = rel.join(entry.file_name());
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            files_under(dir, &path, out);
        } else {
            out.push(path.to_string_lossy().into_owned());
        }
    }
}

impl Txn {
    /// Stages a fresh shadow copy of the security files. A leftover shadow
    /// from a crashed run is dropped first (it was never journaled).
    pub fn begin(ctx: &Context) -> Result<Self, MusterError> {
        let root = root(&ctx.state_dir);
        let _ = fs::remove_dir_all(&root);
        let shadow = root.join(SHADOW);
        fs::create_dir_all(&shadow).map_err(|e| io(&shadow, e))?;
        for file in TRACKED_FILES {
            let src = ctx.config_dir.join(file);
            if src.exists() {
                fs::copy(&src, shadow.join(file)).map_err(|e| io(&src, e))?;
            }
        }
        for dir in TRACKED_DIRS {
            copy_tree(&ctx.config_dir.join(dir), &shadow.join(dir))?;
        }
        Ok(Self { root, shadow })
    }

    /// The shadow config dir: point the vault, manifest and enrollment edits here.
    pub fn dir(&self) -> &Path {
        &self.shadow
    }

    /// Seals and validates the shadow, journals it, and publishes it.
    pub fn commit(self, ctx: &mut Context, message: &str) -> Result<(), MusterError> {
        let manifest = Manifest::load(&self.shadow)?;
        let state = match crate::trust::seal_in(ctx, &self.shadow, false)? {
            Some(state) => state,
            None => fleet_state::FleetState::load(&self.shadow)?.ok_or_else(|| {
                MusterError::Security("revocation produced no signed fleet state".into())
            })?,
        };
        // Validate the staged result exactly as a peer would, without moving the pin.
        fleet_state::verify_with(
            &self.shadow,
            &ctx.state_dir,
            &manifest,
            &VerifyOptions {
                expect_signer: None,
                persist: false,
                check_content: true,
            },
        )?;
        let pin = TrustAnchor::for_state(&state, &manifest, false)?;

        let mut staged = Vec::new();
        for file in TRACKED_FILES {
            if self.shadow.join(file).exists() {
                staged.push(file.to_string());
            }
        }
        for dir in TRACKED_DIRS {
            files_under(&self.shadow, Path::new(dir), &mut staged);
        }
        let mut live = Vec::new();
        for dir in TRACKED_DIRS {
            files_under(&ctx.config_dir, Path::new(dir), &mut live);
        }
        let write: Vec<String> = staged
            .iter()
            // The state file goes last: it is the one peers key off.
            .filter(|rel| rel.as_str() != fleet_state::STATE_FILE)
            .chain(std::iter::once(&fleet_state::STATE_FILE.to_string()))
            .filter(|rel| {
                let new = fs::read(self.shadow.join(rel)).ok();
                new.is_some() && new != fs::read(ctx.config_dir.join(rel)).ok()
            })
            .cloned()
            .collect();
        let delete: Vec<String> = live
            .into_iter()
            .filter(|rel| !self.shadow.join(rel).exists())
            .collect();
        let journal = Journal {
            write,
            delete,
            message: message.to_string(),
            pin,
        };
        validate_journal(&journal)?;
        // The commit point: from here a crash is rolled forward on the next run.
        let bytes = serde_json::to_vec_pretty(&journal)
            .map_err(|e| MusterError::Operational(format!("cannot serialize journal: {e}")))?;
        write_atomic(&self.root.join(JOURNAL), &bytes)?;
        apply(&ctx.config_dir, &ctx.state_dir, &self.root, &journal)
    }
}

/// Test hook (debug builds only): fail after N files, simulating a crash
/// between group writes.
fn injected_failure(written: usize) -> Result<(), MusterError> {
    #[cfg(debug_assertions)]
    if std::env::var("MUSTER_TEST_REVOCATION_FAIL_AFTER")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        == Some(written)
    {
        return Err(MusterError::Operational(
            "injected failure during revocation publication".into(),
        ));
    }
    let _ = written;
    Ok(())
}

/// A journal path must be a plain relative path to a tracked file: either one
/// of [`TRACKED_FILES`] or something below a [`TRACKED_DIRS`] directory. No
/// absolute paths, `..`, control characters or empty/`.` components, so
/// `config_dir.join(rel)` can never leave the config dir.
fn validate_rel_path(rel: &str) -> Result<(), MusterError> {
    use std::path::Component;
    let bad = |why: &str| {
        MusterError::Security(format!(
            "revocation journal has an unsafe path {rel:?}: {why}"
        ))
    };
    if rel.is_empty() || rel.chars().any(|c| c.is_control() || c == '\\') {
        return Err(bad("empty, control or backslash characters"));
    }
    let mut parts = Vec::new();
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(part) => parts.push(part),
            _ => return Err(bad("absolute, `.` or `..` components")),
        }
    }
    let allowed = match parts.as_slice() {
        [file] => TRACKED_FILES.iter().any(|f| file.to_str() == Some(f)),
        [dir, _rest @ ..] => TRACKED_DIRS.iter().any(|d| dir.to_str() == Some(d)),
        [] => false,
    };
    if !allowed {
        return Err(bad("not a file the transaction owns"));
    }
    Ok(())
}

fn validate_journal(journal: &Journal) -> Result<(), MusterError> {
    journal
        .write
        .iter()
        .chain(&journal.delete)
        .try_for_each(|rel| validate_rel_path(rel))
}

/// Idempotent: every step can be repeated after a crash.
fn apply(
    config_dir: &Path,
    state_dir: &Path,
    root: &Path,
    journal: &Journal,
) -> Result<(), MusterError> {
    validate_journal(journal)?;
    let shadow = root.join(SHADOW);
    for (i, rel) in journal.write.iter().enumerate() {
        let bytes = fs::read(shadow.join(rel)).map_err(|e| io(&shadow.join(rel), e))?;
        let dest = config_dir.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        write_atomic(&dest, &bytes)?;
        injected_failure(i + 1)?;
    }
    for rel in &journal.delete {
        match fs::remove_file(config_dir.join(rel)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&config_dir.join(rel), e)),
        }
    }
    journal.pin.save(state_dir)?;
    crate::git::commit_if_configured(config_dir, &journal.message)?;
    fs::remove_dir_all(root).map_err(|e| io(root, e))
}

/// Finishes a journaled transaction, or discards an unjournaled one. Returns
/// whether a journaled transaction was completed.
pub fn recover(config_dir: &Path, state_dir: &Path) -> Result<bool, MusterError> {
    let root = root(state_dir);
    let journal_path = root.join(JOURNAL);
    let bytes = match fs::read(&journal_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Staged but never journaled: nothing live was touched.
            let _ = fs::remove_dir_all(&root);
            return Ok(false);
        }
        Err(e) => return Err(io(&journal_path, e)),
    };
    let journal: Journal = serde_json::from_slice(&bytes).map_err(|e| {
        MusterError::Security(format!(
            "corrupt revocation journal {}: {e}",
            journal_path.display()
        ))
    })?;
    apply(config_dir, state_dir, &root, &journal)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_journal_paths() {
        for bad in [
            "../../etc/passwd",
            "vault/../../x",
            "/etc/passwd",
            "vault/a\nb",
            "",
            "./muster.toml",
            "other.txt",
            "vault",
        ] {
            assert!(
                matches!(validate_rel_path(bad), Err(MusterError::Security(_))),
                "{bad:?} must be refused"
            );
        }
        for ok in [
            "muster.toml",
            "fleet.state.json",
            "vault/g.age",
            "enroll/x/y.json",
        ] {
            assert!(validate_rel_path(ok).is_ok(), "{ok:?} must be allowed");
        }
    }

    #[test]
    fn forged_journal_is_refused_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, state) = (tmp.path().join("config"), tmp.path().join("state"));
        fs::create_dir_all(&config).unwrap();
        let shadow = root(&state).join(SHADOW);
        fs::create_dir_all(&shadow).unwrap();
        let pin = serde_json::json!({
            "epoch": 1, "payload_hash": "", "governance": "", "admins": [], "writers": []
        });
        let forged = serde_json::json!({
            "write": ["muster.toml", "../../escaped"],
            "delete": [],
            "message": "x",
            "pin": pin,
        });
        fs::write(shadow.join("muster.toml"), "x").unwrap();
        fs::write(root(&state).join(JOURNAL), forged.to_string()).unwrap();
        let err = recover(&config, &state).unwrap_err();
        assert!(matches!(err, MusterError::Security(_)), "{err:?}");
        assert!(!config.join("muster.toml").exists());
    }
}
