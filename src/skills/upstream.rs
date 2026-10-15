//! Upstream skill sources: shallow git checkouts in the user cache, status
//! classification, and the per-machine status cache (`<state>/skills-status.json`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::MusterError;
use crate::hash::sha256_hex;
use crate::manifest::SkillRemote;

/// Lifecycle status of one skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillStatus {
    /// An own skill: nothing upstream to compare with.
    Local,
    UpToDate,
    UpdateAvailable,
    Deprecated,
    UpstreamRemoved,
    /// Never checked, or the last check could not reach the source.
    Unknown,
}

impl SkillStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::UpToDate => "up_to_date",
            Self::UpdateAvailable => "update_available",
            Self::Deprecated => "deprecated",
            Self::UpstreamRemoved => "upstream_removed",
            Self::Unknown => "unknown",
        }
    }

    /// Statuses that need a human decision (the web tab shows a dot).
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::UpdateAvailable | Self::Deprecated | Self::UpstreamRemoved
        )
    }
}

/// What the upstream source says about one skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// The source could not be fetched.
    Unreachable,
    /// The source no longer has this skill.
    Missing,
    Present {
        content_hash: String,
        /// `Some(reason)` when SKILL.md frontmatter has `deprecated`.
        deprecated: Option<String>,
    },
}

/// Classify a vendored skill. `locked_hash` is the content hash pinned in
/// `skills.lock` (what was vendored), not the on-disk hash, so local edits
/// never look like upstream updates. Deprecation wins over an update.
pub fn classify(is_local: bool, locked_hash: &str, upstream: &Upstream) -> SkillStatus {
    if is_local {
        return SkillStatus::Local;
    }
    match upstream {
        Upstream::Unreachable => SkillStatus::Unknown,
        Upstream::Missing => SkillStatus::UpstreamRemoved,
        Upstream::Present {
            deprecated: Some(_),
            ..
        } => SkillStatus::Deprecated,
        Upstream::Present { content_hash, .. } if content_hash != locked_hash => {
            SkillStatus::UpdateAvailable
        }
        Upstream::Present { .. } => SkillStatus::UpToDate,
    }
}

/// A fetched upstream source: a working tree and its commit.
#[derive(Debug, Clone)]
pub struct Checkout {
    pub dir: PathBuf,
    pub commit: String,
}

impl Checkout {
    /// Directory holding one subdirectory per skill (`path` of the remote).
    pub fn skills_root(&self, remote: &SkillRemote) -> Result<PathBuf, MusterError> {
        match remote
            .path
            .as_deref()
            .filter(|p| !p.is_empty() && *p != ".")
        {
            Some(p) => Ok(self.dir.join(relative(p, "path")?)),
            None => Ok(self.dir.clone()),
        }
    }

    /// `skills_root`, canonicalized and required to stay inside the checkout
    /// (a symlinked `path` must not lead outside it).
    pub fn contained_skills_root(&self, remote: &SkillRemote) -> Result<PathBuf, MusterError> {
        let root = self.skills_root(remote)?;
        let missing = |e: std::io::Error| {
            MusterError::Operational(format!(
                "source '{}' has no skills directory {}: {e}",
                remote.name,
                remote.path.as_deref().unwrap_or(".")
            ))
        };
        let canonical = std::fs::canonicalize(&root).map_err(missing)?;
        let base = std::fs::canonicalize(&self.dir).map_err(missing)?;
        if !canonical.starts_with(&base) {
            return Err(MusterError::Operational(format!(
                "source '{}': path {} resolves outside the checkout",
                remote.name,
                remote.path.as_deref().unwrap_or(".")
            )));
        }
        Ok(canonical)
    }

    /// Skill names available under the source's `path` (directories with a SKILL.md).
    pub fn skill_names(&self, remote: &SkillRemote) -> Result<Vec<String>, MusterError> {
        let root = self.contained_skills_root(remote)?;
        let entries = std::fs::read_dir(&root).map_err(|e| {
            MusterError::Operational(format!(
                "source '{}' has no skills directory {}: {e}",
                remote.name,
                remote.path.as_deref().unwrap_or(".")
            ))
        })?;
        let mut names = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && !name.starts_with('.') && entry.path().join("SKILL.md").is_file() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }
}

/// Seam over the network so status logic is testable without git.
pub trait UpstreamFetcher {
    fn fetch(&self, remote: &SkillRemote) -> Result<Checkout, MusterError>;
}

/// Shallow git checkouts in `$XDG_CACHE_HOME/muster/skill-sources/<sha256(url)[..16]>`.
pub struct GitFetcher {
    pub cache_dir: PathBuf,
}

impl GitFetcher {
    pub fn from_env() -> Result<Self, MusterError> {
        Ok(Self {
            cache_dir: cache_root()?.join("skill-sources"),
        })
    }

    pub fn checkout_dir(&self, url: &str) -> PathBuf {
        self.cache_dir.join(&sha256_hex(url.as_bytes())[..16])
    }
}

impl UpstreamFetcher for GitFetcher {
    fn fetch(&self, remote: &SkillRemote) -> Result<Checkout, MusterError> {
        let dir = self.checkout_dir(&remote.url);
        let commit = crate::git::shallow_fetch(&remote.url, remote.git_ref.as_deref(), &dir)?;
        Ok(Checkout { dir, commit })
    }
}

/// `$XDG_CACHE_HOME/muster`, else `$HOME/.cache/muster`.
fn cache_root() -> Result<PathBuf, MusterError> {
    let env = |v: &str| {
        std::env::var_os(v)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    };
    if let Some(base) = env("XDG_CACHE_HOME") {
        return Ok(base.join("muster"));
    }
    env("HOME")
        .map(|h| h.join(".cache").join("muster"))
        .ok_or_else(|| MusterError::Operational("HOME is not set".into()))
}

/// `p` as a relative path that cannot escape its base directory.
pub fn relative<'a>(p: &'a str, key: &str) -> Result<&'a Path, MusterError> {
    let path = Path::new(p);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(MusterError::Usage(format!(
            "{key} `{p}` must be a relative path without `..`"
        )));
    }
    Ok(path)
}

/// Last check of one skill on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedStatus {
    pub status: SkillStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_commit: Option<String>,
    pub checked_at: String,
}

/// `<state>/skills-status.json`: per-machine, never committed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    #[serde(default)]
    pub skills: BTreeMap<String, CachedStatus>,
}

pub const STATUS_FILE: &str = "skills-status.json";

impl StatusCache {
    /// A missing or unreadable cache is an empty one (it is only a hint).
    pub fn load(state_dir: &Path) -> Self {
        std::fs::read_to_string(state_dir.join(STATUS_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, state_dir: &Path) -> Result<(), MusterError> {
        let io =
            |e: std::io::Error| MusterError::Operational(format!("{}: {e}", state_dir.display()));
        std::fs::create_dir_all(state_dir).map_err(io)?;
        let text = serde_json::to_string_pretty(self).expect("status cache serializes");
        std::fs::write(state_dir.join(STATUS_FILE), text).map_err(io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(hash: &str, deprecated: Option<&str>) -> Upstream {
        Upstream::Present {
            content_hash: hash.into(),
            deprecated: deprecated.map(str::to_owned),
        }
    }

    #[test]
    fn classification_table() {
        let cases = [
            (true, "h", present("x", None), SkillStatus::Local),
            (true, "h", Upstream::Missing, SkillStatus::Local),
            (false, "h", Upstream::Unreachable, SkillStatus::Unknown),
            (false, "h", Upstream::Missing, SkillStatus::UpstreamRemoved),
            (false, "h", present("h", None), SkillStatus::UpToDate),
            (false, "h", present("x", None), SkillStatus::UpdateAvailable),
            (false, "h", present("h", Some("")), SkillStatus::Deprecated),
            // Deprecation wins over an available update.
            (
                false,
                "h",
                present("x", Some("use y")),
                SkillStatus::Deprecated,
            ),
        ];
        for (is_local, hash, upstream, expected) in cases {
            assert_eq!(
                classify(is_local, hash, &upstream),
                expected,
                "{is_local} {hash} {upstream:?}"
            );
        }
    }

    #[test]
    fn attention_statuses() {
        assert!(SkillStatus::UpdateAvailable.needs_attention());
        assert!(SkillStatus::Deprecated.needs_attention());
        assert!(SkillStatus::UpstreamRemoved.needs_attention());
        assert!(!SkillStatus::UpToDate.needs_attention());
        assert!(!SkillStatus::Local.needs_attention());
        assert!(!SkillStatus::Unknown.needs_attention());
    }

    #[test]
    fn relative_paths_cannot_escape() {
        assert!(relative("skills", "path").is_ok());
        assert!(relative("../x", "path").is_err());
        assert!(relative("/abs", "path").is_err());
    }
}
