use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

use crate::errors::MusterError;

#[derive(Default)]
struct Count {
    uses: u64,
    sessions: BTreeSet<String>,
    last: Option<OffsetDateTime>,
    agents: BTreeMap<String, (u64, BTreeSet<String>)>,
}

struct Options<'a> {
    cutoff: OffsetDateTime,
    max: u64,
    project: Option<&'a Path>,
}

/// Why a transcript file (or directory) was not read. Reported as an
/// aggregate count only: transcript paths contain session ids and encoded
/// project paths, so they are never emitted.
#[derive(Default)]
pub(super) struct Skipped {
    older_than_window: u64,
    too_large: u64,
    unreadable: u64,
}

impl Skipped {
    pub(super) fn count(&mut self, reason: SkipReason) {
        match reason {
            SkipReason::OlderThanWindow => self.older_than_window += 1,
            SkipReason::TooLarge => self.too_large += 1,
            SkipReason::Unreadable => self.unreadable += 1,
        }
    }

    fn into_json(self) -> Value {
        json!({
            "older_than_window": self.older_than_window,
            "too_large": self.too_large,
            "unreadable": self.unreadable,
        })
    }
}

pub(super) enum SkipReason {
    OlderThanWindow,
    TooLarge,
    Unreadable,
}

pub(super) fn valid_item_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Does a transcript `cwd` belong to `project`? Trailing slashes are
/// normalized away and subdirectories count as the same project; `project`
/// is already canonicalized, so component-wise comparison is safe.
pub(super) fn cwd_in_project(cwd: &str, project: &Path) -> bool {
    let cwd = cwd.strip_suffix('/').unwrap_or(cwd);
    let cwd = Path::new(cwd);
    cwd == project || cwd.starts_with(project)
}

mod claude;
mod codex;

/// Transcript support per agent, mirroring the verified findings in
/// docs/research/session-transcripts.md.
fn transcript_status(id: &str) -> &'static str {
    match id {
        "claude" => "supported",
        "codex" => "partial",
        _ => "unsupported",
    }
}

/// Home-relative transcript directory, when one is known.
fn transcript_directory(id: &str) -> Option<&'static str> {
    match id {
        "claude" => Some("~/.claude/projects"),
        "codex" => Some("~/.codex/sessions"),
        "opencode" => Some("~/.local/share/opencode"),
        "antigravity" => Some("~/.gemini"),
        _ => None,
    }
}

/// The effective merged configuration: the machine's user-scope layers, plus
/// the project layer with `--project` (which inherits global toggles, like
/// the engine does for project sync). A project directory without its own
/// configuration contributes no managed items; the global merge is kept and
/// `--project` still filters transcripts.
fn effective_merge(
    config: &Path,
    state: &Path,
    manifest: Option<&crate::manifest::Manifest>,
    project: Option<&Path>,
) -> Option<crate::engine::layer::Merged> {
    let global = manifest.map(|m| crate::engine::layer::global_merge(config, state, m));
    let project = project.and_then(|dir| {
        let layers = crate::engine::layer::project_layers(dir).ok()?;
        let mut merged = crate::engine::layer::merge(&layers);
        let mut toggles = global
            .as_ref()
            .map(|g| g.toggles.clone())
            .unwrap_or_default();
        toggles.extend(merged.toggles.iter().cloned());
        merged.toggles = toggles;
        Some(merged)
    });
    project.or(global)
}

/// Managed skills: direct subdirectories of the skills directory with a
/// regular (non-symlink) `SKILL.md`. The directory itself is contained in the
/// config repo by `Merged::skills_path`; entry names are filtered so only
/// safe identifiers are ever reported.
fn collect_skills(dir: &Path, managed: &mut BTreeSet<(String, String)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Never follow symlinks: a link could point outside the repo.
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if crate::skills::is_ignored_skill_name(&name, true) || !valid_item_name(&name) {
            continue;
        }
        let skill_md = entry.path().join("SKILL.md");
        if std::fs::symlink_metadata(&skill_md).is_ok_and(|m| m.is_file()) {
            managed.insert(("skill".to_owned(), name));
        }
    }
}

/// Managed subagents: stems of regular (non-symlink) `.md` files.
fn collect_subagents(dir: &Path, managed: &mut BTreeSet<(String, String)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if crate::skills::is_ignored_skill_name(&stem, false) || !valid_item_name(&stem) {
            continue;
        }
        managed.insert(("subagent".to_owned(), stem));
    }
}

pub fn report(
    days: i64,
    config: &Path,
    state: &Path,
    agent: Option<&str>,
    max: u64,
    project: Option<&Path>,
) -> Result<Value, MusterError> {
    let options = Options {
        cutoff: OffsetDateTime::now_utc() - Duration::days(days),
        max,
        project,
    };
    let manifest = crate::manifest::Manifest::load(config).ok();
    let merged = effective_merge(config, state, manifest.as_ref(), project);

    // Considered adapters: the requested agent, or the effective enabled set.
    let considered: Vec<&'static crate::adapters::Adapter> = match agent {
        Some(id) => vec![crate::adapters::find(id).ok_or_else(|| {
            MusterError::Usage(format!(
                "unknown agent `{id}`; run `muster agents` to list adapters"
            ))
        })?],
        None => crate::engine::enabled_adapters(merged.as_ref().map(|m| &m.agents))?,
    };

    let mut counts = BTreeMap::new();
    let mut stats: BTreeMap<&'static str, (u64, Value)> = BTreeMap::new();
    let mut session_counts: BTreeMap<&'static str, usize> =
        considered.iter().map(|a| (a.id, 0usize)).collect();
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        for adapter in &considered {
            let mut malformed = 0;
            let mut skipped = Skipped::default();
            let mut sessions = BTreeSet::new();
            match adapter.id {
                "claude" => claude::scan(
                    &home.join(".claude/projects"),
                    &options,
                    &mut counts,
                    &mut malformed,
                    &mut skipped,
                    &mut sessions,
                ),
                "codex" => codex::scan_codex(
                    &home.join(".codex/sessions"),
                    &options,
                    &mut counts,
                    &mut malformed,
                    &mut skipped,
                    &mut sessions,
                ),
                _ => {}
            }
            session_counts.insert(adapter.id, sessions.len());
            stats.insert(adapter.id, (malformed, skipped.into_json()));
        }
    }

    // Managed items come from the effective merged configuration.
    let mut managed = BTreeSet::new();
    if let Some(merged) = &merged {
        for name in merged.mcp.keys() {
            if valid_item_name(name) {
                managed.insert(("mcp".to_owned(), name.clone()));
            }
        }
        // A declared-but-escaping source (absolute or `..`) yields no
        // managed subagents at all.
        let subagents_dir: Option<PathBuf> = merged.subagents_path().unwrap_or_default();
        if let Some(dir) = subagents_dir {
            collect_subagents(&dir, &mut managed);
        }
        // Undeclared skills default to `<layer>/skills`, contained like a
        // declared source.
        let skills_dir = match merged.skills_path() {
            Ok(Some(dir)) => Some(dir),
            Ok(None) => Some(merged.skills_dir.join("skills")),
            Err(_) => None,
        };
        if let Some(dir) = skills_dir {
            collect_skills(&dir, &mut managed);
        }
    }

    // Coverage: `none` when nothing was scanned (no unused advice on zero
    // evidence), `partial` when a considered agent has unsupported or partial
    // transcripts.
    let total_sessions: usize = session_counts.values().sum();
    let coverage = if total_sessions == 0 {
        "none"
    } else if considered
        .iter()
        .any(|a| transcript_status(a.id) != "supported")
    {
        "partial"
    } else {
        "full"
    };

    let unused: Vec<Value> = if coverage == "none" {
        Vec::new()
    } else {
        managed
            .iter()
            .filter(|key| !counts.contains_key(key))
            .filter(|(kind, name)| {
                // An item disabled for every considered adapter is not
                // "unused" advice; it is already off everywhere.
                !considered.iter().all(|a| {
                    merged
                        .as_ref()
                        .is_some_and(|m| m.disabled(kind, name, a.id).is_some())
                })
            })
            .map(|(kind, name)| {
                let mut item = json!({"kind": kind, "name": name});
                // `muster disable` only accepts toggle-safe names; for any
                // other managed name the suggestion is omitted.
                if crate::adapters::config::is_valid_item_name(name) {
                    item["suggestion"] = json!(format!("muster disable {kind}:{name}"));
                }
                item
            })
            .collect()
    };
    for key in &managed {
        counts.entry(key.clone()).or_default();
    }
    let items: Vec<Value> = counts.into_iter().map(|((kind, name), c)| json!({"kind": kind, "name": name, "uses": c.uses, "sessions": c.sessions.len(), "last_used": c.last.and_then(|t| t.format(&Rfc3339).ok()), "agents": c.agents.into_iter().map(|(a, (uses, sessions))| (a, json!({"uses": uses, "sessions": sessions.len()}))).collect::<BTreeMap<_,_>>()})).collect();
    let sources: Vec<Value> = considered
        .iter()
        .map(|adapter| {
            let mut entry = json!({
                "agent": adapter.id,
                "status": transcript_status(adapter.id),
            });
            if let Some(dir) = transcript_directory(adapter.id) {
                entry["directory"] = json!(dir);
            }
            if let Some((malformed, skipped)) = stats.remove(adapter.id) {
                entry["malformed_lines"] = json!(malformed);
                entry["skipped"] = skipped;
            }
            entry
        })
        .collect();
    Ok(json!({
        "items": items,
        "unused": unused,
        "coverage": coverage,
        "sessions_scanned": session_counts,
        "sources": sources,
    }))
}
