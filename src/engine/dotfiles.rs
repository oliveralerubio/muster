//! Managed dotfiles: whole files outside the agent adapters.
//!
//! The manifest declares `[files."<path under $HOME>"] source = "files/..."`;
//! the contents live in the config repo. The engine plans them next to the
//! adapter files (owner `files`), owns them whole, and prunes them when they
//! leave the manifest: a file Muster created and nobody edited is removed,
//! anything else is left in place and simply stops being managed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use super::{read_optional, Entry, Ours, Roots};
use crate::adapters::{self, FileFormat};
use crate::context::Context;
use crate::errors::MusterError;
use crate::manifest::{self, Manifest};
use crate::output::Diagnostic;

/// Owner id in plans and ownership records.
pub const OWNER: &str = "files";
/// Directory of the config repo that holds dotfile contents.
pub const STORE_DIR: &str = "files";

/// Absolute path of a managed dotfile, or why it cannot be managed: the
/// target must be relative to `$HOME` without `..`, outside Muster's own
/// config repo and state, and not a file an agent adapter manages.
pub fn locate(ctx: &Context, target: &str) -> Result<PathBuf, String> {
    let roots = Roots::from_env().map_err(|e| e.to_string())?;
    locate_in(&roots, &ctx.config_dir, &ctx.state_dir, target)
}

fn locate_in(
    roots: &Roots,
    config_dir: &Path,
    state_dir: &Path,
    target: &str,
) -> Result<PathBuf, String> {
    if !is_relative_inside(target) {
        return Err(format!(
            "`{target}` must be a path relative to the home directory without `..`"
        ));
    }
    let path = roots.home.join(target);
    if path.starts_with(config_dir) || path.starts_with(state_dir) {
        return Err(format!(
            "`{target}` is inside Muster's own config repository or state directory"
        ));
    }
    for adapter in adapters::ADAPTERS {
        if adapter.files.iter().any(|f| roots.resolve(f) == path) {
            return Err(format!(
                "`{target}` is managed by the {} adapter; declare it in the manifest instead",
                adapter.name
            ));
        }
    }
    Ok(path)
}

/// `source` paths stay inside the config repo; targets inside `$HOME`.
pub fn is_relative_inside(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Effective mode of an existing dotfile and the permissions sync will write.
pub(super) struct Mode {
    pub(super) permissions: std::fs::Permissions,
    pub(super) changed: bool,
    added_execute: u32,
}

impl Mode {
    #[cfg(unix)]
    pub(super) fn read(
        path: &Path,
        private: bool,
        executable: bool,
    ) -> Result<Option<Self>, MusterError> {
        use std::os::unix::fs::PermissionsExt;

        let current = match std::fs::metadata(path) {
            Ok(metadata) => metadata.permissions().mode() & 0o7777,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(super::io_error(path, e)),
        };
        let execute = execute_bits(private, executable);
        let retained = if private { current & 0o600 } else { current };
        let desired = retained | execute;
        Ok(Some(Self {
            permissions: std::fs::Permissions::from_mode(desired),
            changed: desired != current,
            added_execute: execute & !current,
        }))
    }

    #[cfg(not(unix))]
    pub(super) fn read(_: &Path, _: bool, _: bool) -> Result<Option<Self>, MusterError> {
        Ok(None)
    }
}

fn execute_bits(private: bool, executable: bool) -> u32 {
    match (private, executable && cfg!(unix)) {
        (_, false) => 0,
        (true, true) => 0o100,
        (false, true) => 0o111,
    }
}

/// Plan every declared dotfile plus the pruning of those no longer declared.
/// Problems with one file become error diagnostics; the rest is planned.
pub(super) fn plan(
    ctx: &mut Context,
    manifest: &Manifest,
    roots: &Roots,
    ours: &Ours<'_>,
    records: &BTreeMap<String, super::store::Record>,
    key: &str,
    entries: &mut BTreeMap<PathBuf, Entry>,
) -> Result<BTreeSet<PathBuf>, MusterError> {
    let mut declared = BTreeSet::new();
    for (target, spec) in &manifest.files {
        let at = format!("{}: files.\"{target}\"", manifest::FILE_NAME);
        let path = match locate_in(roots, &ctx.config_dir, &ctx.state_dir, target) {
            Ok(path) if !entries.contains_key(&path) => path,
            Ok(_) => {
                let message = format!("`{target}` is already managed by an agent adapter");
                ctx.diag(Diagnostic::error("dotfile_conflict", message).with_path(at));
                continue;
            }
            Err(message) => {
                ctx.diag(Diagnostic::error("dotfile_conflict", message).with_path(at));
                continue;
            }
        };
        declared.insert(path.clone());
        if !is_relative_inside(&spec.source) {
            let message = format!("source `{}` must be inside the config repo", spec.source);
            ctx.diag(Diagnostic::error("dotfile_source_invalid", message).with_path(at));
            continue;
        }
        let source = ctx.config_dir.join(&spec.source);
        let after = match read_optional(&source) {
            Ok(Some(after)) => after,
            Ok(None) => {
                let message = format!("source {} does not exist", source.display());
                ctx.diag(Diagnostic::error("dotfile_source_missing", message).with_path(at));
                continue;
            }
            Err(err) => {
                ctx.diag(
                    Diagnostic::error("dotfile_source_invalid", err.to_string()).with_path(at),
                );
                continue;
            }
        };
        let before = read_optional(&path)?;
        let record = ours(&path);
        entries.insert(
            path.clone(),
            Entry {
                created: record.map_or(before.is_none(), |r| r.created),
                drift: record.is_some_and(|r| before.as_deref() != Some(r.contents.as_str())),
                path,
                agents: vec![OWNER],
                format: FileFormat::Text,
                before,
                after: Some(after),
                managed: true,
                owned: vec![vec!["file".into()]],
                private: spec.private,
                executable: spec.executable,
                dotfile_mode: None,
            },
        );
    }

    for (path, record) in records {
        let path = PathBuf::from(path);
        if record.agent != OWNER || record.target != key || declared.contains(&path) {
            continue;
        }
        let before = read_optional(&path)?;
        let untouched = before.as_deref() == Some(record.contents.as_str());
        let after = if record.created && untouched {
            None
        } else {
            before.clone()
        };
        entries.entry(path.clone()).or_insert(Entry {
            path,
            agents: vec![OWNER],
            format: FileFormat::Text,
            drift: before.is_some() && !untouched,
            before,
            after,
            managed: false,
            owned: Vec::new(),
            private: record.private,
            executable: record.executable,
            dotfile_mode: None,
            created: record.created,
        });
    }
    Ok(declared)
}

/// Managed dotfiles land in `$HOME` (`.bashrc`, `.ssh/authorized_keys`), so a
/// new one, changed content or new execute permissions may need approval.
/// Keep the previous hashes for declarations whose sources were skipped.
pub(super) fn approvals(
    entries: &BTreeMap<PathBuf, Entry>,
    declared: &BTreeSet<PathBuf>,
    ours: &Ours<'_>,
    approved: &mut super::store::Approved,
    approvals: &mut Vec<super::Approval>,
) {
    approved
        .files
        .retain(|path, _| declared.contains(Path::new(path)));
    for (path, entry) in entries {
        if !(entry.managed && entry.agents == [OWNER]) {
            continue;
        }
        let Some(after) = entry.after.as_deref() else {
            continue;
        };
        let key = path.display().to_string();
        let hash = crate::hash::sha256_hex(after.as_bytes());
        let content_approved = approved.files.get(&key) == Some(&hash);
        let added_execute = entry.dotfile_mode.as_ref().map_or_else(
            || execute_bits(entry.private, entry.executable),
            |mode| mode.added_execute,
        );
        let approved_execute = ours(path)
            .filter(|r| r.agent == OWNER && r.contents == after && content_approved)
            .map_or(0, |r| execute_bits(r.private, r.executable));
        let needs_execute_approval = added_execute & !approved_execute != 0;
        if needs_execute_approval || (!content_approved && entry.before.as_deref() != Some(after)) {
            approvals.push(super::Approval {
                kind: "dotfile",
                name: key.clone(),
                detail: if needs_execute_approval {
                    format!("adds execute permissions, sha256 {hash}")
                } else {
                    format!("sha256 {hash}")
                },
            });
        }
        approved.files.insert(key, hash);
    }
}
