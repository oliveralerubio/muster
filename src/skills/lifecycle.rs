//! Skills lifecycle domain logic: reading a skill from its upstream source,
//! caching one checkout per source, and deciding what `skill update` does.
//!
//! It also owns the two lifecycle operations, `check` and
//! `plan_update` + `apply_update`, over explicit paths, manifest, lock and an
//! [`UpstreamFetcher`]. They return typed findings; the command handlers in
//! `commands/skill.rs` (and `sync`, for auto-update) only translate those into
//! diagnostics, prompts, team commits and JSON. The per-machine status cache
//! rules live here and nowhere else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::errors::MusterError;
use crate::manifest::{Manifest, SkillRemote};

use super::lock::{LockEntry, SkillsLock};
use super::upstream::{
    classify, CachedStatus, Checkout, SkillStatus, StatusCache, Upstream, UpstreamFetcher,
};
use super::{
    contained, content_hash, parse_frontmatter, read_skill_files, validate_skill_name, write_skill,
};

/// One skill as fetched from its upstream source.
#[derive(Debug, Clone)]
pub struct UpstreamSkill {
    pub files: BTreeMap<String, String>,
    pub hash: String,
    pub version: Option<String>,
    pub deprecated: Option<String>,
}

impl UpstreamSkill {
    /// The upstream state this skill represents, for status classification.
    pub fn upstream(&self) -> Upstream {
        Upstream::Present {
            content_hash: self.hash.clone(),
            deprecated: self.deprecated.clone(),
        }
    }
}

/// Read skill `name` from `checkout`. `Ok(None)` when upstream no longer has it
/// (no `SKILL.md`).
pub fn read_upstream(
    checkout: &Checkout,
    remote: &SkillRemote,
    name: &str,
) -> Result<Option<UpstreamSkill>, MusterError> {
    let dir = checkout.contained_skills_root(remote)?.join(name);
    let Ok(skill_md) = std::fs::read_to_string(dir.join("SKILL.md")) else {
        return Ok(None);
    };
    let meta = parse_frontmatter(&skill_md).unwrap_or_default();
    let files = read_skill_files(&dir)?;
    Ok(Some(UpstreamSkill {
        hash: content_hash(&files),
        files,
        version: meta.version,
        deprecated: meta.deprecated,
    }))
}

/// The declared upstream sources of a manifest.
pub fn remotes(manifest: &Manifest) -> Vec<SkillRemote> {
    manifest
        .agents
        .as_ref()
        .and_then(|a| a.skills.as_ref())
        .map(|s| s.remote.clone())
        .unwrap_or_default()
}

/// Fetch each source at most once.
pub struct Sources<'a> {
    remotes: Vec<SkillRemote>,
    fetcher: &'a dyn UpstreamFetcher,
    fetched: BTreeMap<String, Result<Checkout, String>>,
}

impl<'a> Sources<'a> {
    pub fn new(manifest: &Manifest, fetcher: &'a dyn UpstreamFetcher) -> Self {
        Self {
            remotes: remotes(manifest),
            fetcher,
            fetched: BTreeMap::new(),
        }
    }

    pub fn remote(&self, origin: &str) -> Option<&SkillRemote> {
        self.remotes.iter().find(|r| r.name == origin)
    }

    /// The checkout of `origin`, or why it is unavailable. A failed fetch is
    /// cached too, so an unreachable source is tried once per run.
    pub fn checkout(&mut self, origin: &str) -> Result<(SkillRemote, Checkout), String> {
        let remote = self
            .remote(origin)
            .cloned()
            .ok_or_else(|| format!("source '{origin}' is no longer declared in muster.toml"))?;
        let fetcher = self.fetcher;
        let fetched = self
            .fetched
            .entry(origin.to_string())
            .or_insert_with(|| fetcher.fetch(&remote).map_err(|e| e.to_string()));
        fetched.clone().map(|c| (remote, c))
    }
}

/// What `skill update` should do with one subscribed skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// Disk, lock and upstream agree: nothing to apply.
    UpToDate,
    /// Vendor the upstream content and re-pin the lock.
    ApplyUpdate,
    /// Local edits would be overwritten and `--force` was not given.
    SkipModified,
    /// The source no longer has the skill.
    SkipUpstreamRemoved,
}

/// Decide the update action from the three content hashes.
///
/// - `locked_hash`: what `skills.lock` pins.
/// - `disk_hash`: what is vendored on disk now.
/// - `upstream_hash`: what the source offers now (`None` if removed upstream).
pub fn evaluate_update_action(
    locked_hash: &str,
    disk_hash: &str,
    upstream_hash: Option<&str>,
    force: bool,
) -> UpdateAction {
    let Some(upstream_hash) = upstream_hash else {
        return UpdateAction::SkipUpstreamRemoved;
    };
    if upstream_hash == disk_hash && upstream_hash == locked_hash {
        return UpdateAction::UpToDate;
    }
    if disk_hash != locked_hash && disk_hash != upstream_hash && !force {
        return UpdateAction::SkipModified;
    }
    UpdateAction::ApplyUpdate
}

// ---------------------------------------------------------------------------
// Status cache (the only place that decides what is remembered)
// ---------------------------------------------------------------------------

/// Classify a subscribed skill against what upstream offers, and remember the
/// result in `cache`. An `Unknown` result (unreachable source) is returned but
/// never stored, so the last known status survives an offline check.
pub fn observe(
    cache: &mut StatusCache,
    name: &str,
    locked_hash: &str,
    upstream: &Upstream,
    reason: Option<String>,
    upstream_commit: Option<String>,
    checked_at: &str,
) -> SkillStatus {
    let status = classify(false, locked_hash, upstream);
    if status != SkillStatus::Unknown {
        cache.skills.insert(
            name.to_string(),
            CachedStatus {
                status,
                reason,
                upstream_commit,
                checked_at: checked_at.to_string(),
            },
        );
    }
    status
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// Which skills `check` looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckTarget {
    All,
    Named(String),
}

/// The result of checking one locked skill.
#[derive(Debug, Clone)]
pub struct CheckedSkill {
    pub name: String,
    pub origin: String,
    pub status: SkillStatus,
    pub reason: Option<String>,
    /// The commit pinned in `skills.lock`.
    pub commit: Option<String>,
    pub upstream_commit: Option<String>,
    pub upstream_version: Option<String>,
    /// The cached status from before this check (reported when `Unknown`).
    pub last_known: Option<String>,
}

/// Something the caller should surface, in the order it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckFinding {
    /// A source could not be read; reported once per origin.
    SourceUnreachable { origin: String, why: String },
    /// The status of one subscribed skill (own skills have none).
    Status {
        name: String,
        status: SkillStatus,
        reason: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct CheckReport {
    pub checked_at: String,
    pub skills: Vec<CheckedSkill>,
    pub findings: Vec<CheckFinding>,
}

/// Compare every (or one) locked skill with its upstream source and refresh
/// the status cache in `state_dir`.
pub fn check(
    target: &CheckTarget,
    manifest: &Manifest,
    lock: &SkillsLock,
    fetcher: &dyn UpstreamFetcher,
    state_dir: &Path,
    checked_at: &str,
) -> Result<CheckReport, MusterError> {
    let only = match target {
        CheckTarget::All => None,
        CheckTarget::Named(name) => {
            let name = validate_skill_name(name)?;
            if lock.get(&name).is_none() {
                return Err(MusterError::Usage(format!(
                    "skill '{name}' is not in skills.lock"
                )));
            }
            Some(name)
        }
    };
    let mut sources = Sources::new(manifest, fetcher);
    let mut cache = StatusCache::load(state_dir);
    let mut reported_unreachable = BTreeSet::new();
    let mut skills = Vec::new();
    let mut findings = Vec::new();
    for entry in &lock.skills {
        if only.as_ref().is_some_and(|n| n != &entry.name) {
            continue;
        }
        if entry.is_local() {
            skills.push(CheckedSkill {
                name: entry.name.clone(),
                origin: entry.origin.clone(),
                status: SkillStatus::Local,
                reason: None,
                commit: None,
                upstream_commit: None,
                upstream_version: None,
                last_known: None,
            });
            continue;
        }
        let mut unreachable = |origin: &str, why: String| {
            if reported_unreachable.insert(origin.to_string()) {
                findings.push(CheckFinding::SourceUnreachable {
                    origin: origin.to_string(),
                    why,
                });
            }
        };
        let (upstream, skill, commit) = match sources.checkout(&entry.origin) {
            Ok((remote, checkout)) => match read_upstream(&checkout, &remote, &entry.name) {
                Ok(Some(skill)) => (skill.upstream(), Some(skill), Some(checkout.commit)),
                Ok(None) => (Upstream::Missing, None, Some(checkout.commit)),
                Err(e) => {
                    unreachable(&entry.origin, e.to_string());
                    (Upstream::Unreachable, None, None)
                }
            },
            Err(e) => {
                unreachable(&entry.origin, e);
                (Upstream::Unreachable, None, None)
            }
        };
        let reason = skill.as_ref().and_then(|s| s.deprecated.clone());
        let last_known = cache
            .skills
            .get(&entry.name)
            .map(|c| c.status.as_str().to_string());
        let status = observe(
            &mut cache,
            &entry.name,
            &entry.content_hash,
            &upstream,
            reason.clone(),
            commit.clone(),
            checked_at,
        );
        findings.push(CheckFinding::Status {
            name: entry.name.clone(),
            status,
            reason: reason.clone(),
        });
        skills.push(CheckedSkill {
            name: entry.name.clone(),
            origin: entry.origin.clone(),
            status,
            reason,
            commit: entry.commit.clone(),
            upstream_commit: commit,
            upstream_version: skill.and_then(|s| s.version),
            last_known,
        });
    }
    cache.checked_at = Some(checked_at.to_string());
    cache.skills.retain(|name, _| lock.get(name).is_some());
    cache.save(state_dir)?;
    Ok(CheckReport {
        checked_at: checked_at.to_string(),
        skills,
        findings,
    })
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

/// Which skills `plan_update` considers. A named skill makes every refusal an
/// error; `All` turns them into skipped entries with a warning finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateSelection {
    All,
    Named(String),
}

/// Per-file change from the vendored copy to the upstream one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiff {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

impl SkillDiff {
    pub fn between(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> Self {
        let normalize = |s: &str| s.replace("\r\n", "\n");
        Self {
            added: new
                .keys()
                .filter(|k| !old.contains_key(*k))
                .cloned()
                .collect(),
            changed: new
                .iter()
                .filter(|(k, v)| old.get(*k).is_some_and(|o| normalize(o) != normalize(v)))
                .map(|(k, _)| k.clone())
                .collect(),
            removed: old
                .keys()
                .filter(|k| !new.contains_key(*k))
                .cloned()
                .collect(),
        }
    }
}

/// One skill that `apply_update` will vendor.
#[derive(Debug, Clone)]
pub struct PlannedUpdate {
    pub name: String,
    pub origin: String,
    pub commit: String,
    pub skill: UpstreamSkill,
    pub diff: SkillDiff,
    /// Local edits are being discarded because of `--force`.
    pub forced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Unreachable,
    UpstreamRemoved,
    Modified,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unreachable => "unreachable",
            Self::UpstreamRemoved => "upstream_removed",
            Self::Modified => "modified",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedUpdate {
    pub name: String,
    pub reason: SkipReason,
}

/// A warning raised while planning, in the order it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDiagnostic {
    SourceUnreachable { origin: String, why: String },
    UpstreamRemoved { name: String, origin: String },
    SkillModified { name: String },
}

impl UpdateDiagnostic {
    pub fn code(&self) -> &'static str {
        match self {
            Self::SourceUnreachable { .. } => "skill_source_unreachable",
            Self::UpstreamRemoved { .. } => "skill_upstream_removed",
            Self::SkillModified { .. } => "skill_modified",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UpdatePlan {
    pub planned: Vec<PlannedUpdate>,
    pub skipped: Vec<SkippedUpdate>,
    pub diagnostics: Vec<UpdateDiagnostic>,
}

fn upstream_removed_message(name: &str, origin: &str) -> String {
    format!(
        "skill '{name}' was removed from source '{origin}'; nothing to update to. Keep it with \
         `muster skill rm {name} --detach --yes` or delete it with `muster skill rm {name} --yes`"
    )
}

fn modified_message(name: &str) -> String {
    format!(
        "skill '{name}' has local modifications that the update would overwrite; \
         re-run with --force to discard them"
    )
}

/// Work out what `skill update` would do, touching nothing on disk.
///
/// Local edits are never overwritten without `force`. For `Named`, a refusal
/// is an error; for `All` it is a skipped entry plus a diagnostic.
pub fn plan_update(
    selection: &UpdateSelection,
    force: bool,
    manifest: &Manifest,
    skills_dir: &Path,
    lock: &SkillsLock,
    fetcher: &dyn UpstreamFetcher,
) -> Result<UpdatePlan, MusterError> {
    let named = matches!(selection, UpdateSelection::Named(_));
    let targets: Vec<LockEntry> = match selection {
        UpdateSelection::Named(name) => {
            let entry = lock.get(name).ok_or_else(|| {
                MusterError::Usage(format!("skill '{name}' is not in skills.lock"))
            })?;
            if entry.is_local() {
                return Err(MusterError::Usage(format!(
                    "skill '{name}' is an own skill; edit it in place"
                )));
            }
            vec![entry.clone()]
        }
        UpdateSelection::All => lock
            .skills
            .iter()
            .filter(|e| !e.is_local())
            .cloned()
            .collect(),
    };

    let mut sources = Sources::new(manifest, fetcher);
    let mut plan = UpdatePlan::default();
    for entry in targets {
        let skill_dir = contained(skills_dir, &entry.name, None)?;
        let (remote, checkout) = match sources.checkout(&entry.origin) {
            Ok(found) => found,
            Err(e) => {
                if named {
                    return Err(MusterError::Operational(format!(
                        "cannot update skill '{}': {e}",
                        entry.name
                    )));
                }
                plan.diagnostics.push(UpdateDiagnostic::SourceUnreachable {
                    origin: entry.origin.clone(),
                    why: e,
                });
                plan.skipped.push(SkippedUpdate {
                    name: entry.name.clone(),
                    reason: SkipReason::Unreachable,
                });
                continue;
            }
        };
        let upstream = read_upstream(&checkout, &remote, &entry.name)?;
        let disk = if upstream.is_some() {
            Some(read_skill_files(&skill_dir)?)
        } else {
            None
        };
        let disk_hash = disk.as_ref().map(content_hash).unwrap_or_default();
        let action = evaluate_update_action(
            &entry.content_hash,
            &disk_hash,
            upstream.as_ref().map(|s| s.hash.as_str()),
            force,
        );
        match action {
            UpdateAction::SkipUpstreamRemoved => {
                if named {
                    return Err(MusterError::Usage(upstream_removed_message(
                        &entry.name,
                        &entry.origin,
                    )));
                }
                plan.diagnostics.push(UpdateDiagnostic::UpstreamRemoved {
                    name: entry.name.clone(),
                    origin: entry.origin.clone(),
                });
                plan.skipped.push(SkippedUpdate {
                    name: entry.name,
                    reason: SkipReason::UpstreamRemoved,
                });
            }
            UpdateAction::UpToDate => {}
            UpdateAction::SkipModified => {
                if named {
                    return Err(MusterError::Usage(modified_message(&entry.name)));
                }
                plan.diagnostics.push(UpdateDiagnostic::SkillModified {
                    name: entry.name.clone(),
                });
                plan.skipped.push(SkippedUpdate {
                    name: entry.name,
                    reason: SkipReason::Modified,
                });
            }
            UpdateAction::ApplyUpdate => {
                let skill = upstream.expect("apply update requires upstream skill");
                let disk = disk.expect("apply update requires disk files");
                plan.planned.push(PlannedUpdate {
                    diff: SkillDiff::between(&disk, &skill.files),
                    name: entry.name.clone(),
                    origin: entry.origin.clone(),
                    commit: checkout.commit.clone(),
                    skill,
                    forced: disk_hash != entry.content_hash && force,
                });
            }
        }
    }
    Ok(plan)
}

impl UpdateDiagnostic {
    /// Human message for the diagnostic.
    pub fn message(&self) -> String {
        match self {
            Self::SourceUnreachable { origin, why } => {
                format!("cannot check skill source '{origin}': {why}")
            }
            Self::UpstreamRemoved { name, origin } => upstream_removed_message(name, origin),
            Self::SkillModified { name } => modified_message(name),
        }
    }
}

/// What `apply_update` did.
#[derive(Debug, Clone)]
pub struct AppliedUpdates {
    pub names: Vec<String>,
    /// The status cache could not be written (a hint only, not fatal).
    pub cache_error: Option<String>,
}

/// Vendor every planned skill, re-pin `skills.lock` and refresh the status
/// cache. Files that already match upstream are not rewritten.
pub fn apply_update(
    plan: &UpdatePlan,
    skills_dir: &Path,
    config_dir: &Path,
    state_dir: &Path,
    lock: &mut SkillsLock,
    updated_at: &str,
) -> Result<AppliedUpdates, MusterError> {
    let mut cache = StatusCache::load(state_dir);
    for p in &plan.planned {
        let skill_dir = contained(skills_dir, &p.name, None)?;
        if content_hash(&read_skill_files(&skill_dir)?) != p.skill.hash {
            write_skill(skills_dir, &p.name, &p.skill.files)?;
        }
        lock.upsert(LockEntry {
            name: p.name.clone(),
            origin: p.origin.clone(),
            commit: Some(p.commit.clone()),
            content_hash: p.skill.hash.clone(),
            version: p.skill.version.clone(),
            updated_at: updated_at.to_string(),
        });
        observe(
            &mut cache,
            &p.name,
            &p.skill.hash,
            &p.skill.upstream(),
            p.skill.deprecated.clone(),
            Some(p.commit.clone()),
            updated_at,
        );
    }
    lock.save(config_dir)?;
    Ok(AppliedUpdates {
        names: plan.planned.iter().map(|p| p.name.clone()).collect(),
        cache_error: cache.save(state_dir).err().map(|e| e.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_lists_added_changed_removed() {
        let old = BTreeMap::from([
            ("SKILL.md".to_string(), "a\n".to_string()),
            ("gone.md".to_string(), "x".to_string()),
            ("same.md".to_string(), "s\r\n".to_string()),
        ]);
        let new = BTreeMap::from([
            ("SKILL.md".to_string(), "b\n".to_string()),
            ("new.md".to_string(), "n".to_string()),
            ("same.md".to_string(), "s\n".to_string()),
        ]);
        let diff = SkillDiff::between(&old, &new);
        assert_eq!(diff.added, ["new.md"]);
        assert_eq!(diff.changed, ["SKILL.md"]);
        assert_eq!(diff.removed, ["gone.md"]);
    }
}
