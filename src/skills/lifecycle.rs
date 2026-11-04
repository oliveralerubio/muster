//! Skills lifecycle domain logic: reading a skill from its upstream source,
//! caching one checkout per source, and deciding what `skill update` does.
//!
//! The command handlers in `commands/skill.rs` only translate these answers
//! into diagnostics, prompts and file writes.

use std::collections::BTreeMap;

use crate::errors::MusterError;
use crate::manifest::{Manifest, SkillRemote};

use super::upstream::{Checkout, Upstream, UpstreamFetcher};
use super::{content_hash, parse_frontmatter, read_skill_files};

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
