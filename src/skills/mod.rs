//! Enterprise skills lifecycle: local authoring and remote subscription.
//!
//! Skills live under `<source>/<name>/` (default: `skills/<name>/`) in the
//! config repo, versioned and vendored. Upstream sources are declared in
//! `[agents.skills.remote]` and pinned in `skills.lock`.

pub mod lock;
pub mod upstream;

use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::errors::MusterError;
use crate::hash::sha256_hex;

pub use lock::{LockEntry, SkillsLock};
pub use upstream::{SkillStatus, UpstreamFetcher};

/// Read every file of `skill_dir` recursively, relative to `skill_dir`.
pub fn read_skill_files(skill_dir: &Path) -> Result<BTreeMap<String, String>, MusterError> {
    let mut files = BTreeMap::new();
    collect_files(skill_dir, "", &mut files)?;
    Ok(files)
}

fn collect_files(
    dir: &Path,
    prefix: &str,
    files: &mut BTreeMap<String, String>,
) -> Result<(), MusterError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(MusterError::Operational(format!("{}: {e}", dir.display()))),
    };
    for entry in entries {
        let entry =
            entry.map_err(|e| MusterError::Operational(format!("{}: {e}", dir.display())))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let rel = format!("{prefix}{name}");
        if file_type.is_dir() {
            collect_files(&path, &format!("{rel}/"), files)?;
        } else if file_type.is_file() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
            files.insert(rel, text);
        }
    }
    Ok(())
}

/// Compute `sha256:<hex>` over sorted relative paths and LF-normalized contents.
pub fn content_hash(files: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (rel_path, content) in files {
        hasher.update(rel_path.as_bytes());
        hasher.update(b"\0");
        let normalized = content.replace("\r\n", "\n");
        hasher.update(normalized.as_bytes());
        hasher.update(b"\0");
    }
    format!("sha256:{}", sha256_hex(&hasher.finalize()))
}

/// Parse `version` and `deprecated` fields from a `SKILL.md` frontmatter block.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FrontmatterMetadata {
    pub name: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    /// `None`: not deprecated. `Some(reason)`: deprecated (empty reason if just `true`).
    pub deprecated: Option<String>,
}

pub fn parse_frontmatter(content: &str) -> Option<FrontmatterMetadata> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_start = &trimmed[3..];
    let after_newline = match after_start.strip_prefix("\r\n") {
        Some(rest) => rest,
        None => after_start.strip_prefix('\n')?,
    };

    let end_idx = after_newline.find("\n---")?;
    let fm_block = &after_newline[..end_idx];

    let mut meta = FrontmatterMetadata::default();
    for line in fm_block.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let raw_val = val.trim();
        let unquoted = raw_val.trim_matches(|c| c == '\'' || c == '"').trim();
        match key {
            "name" if !unquoted.is_empty() => meta.name = Some(unquoted.to_string()),
            "description" if !unquoted.is_empty() => meta.description = Some(unquoted.to_string()),
            "version" if !unquoted.is_empty() => meta.version = Some(unquoted.to_string()),
            "deprecated" => {
                if unquoted.eq_ignore_ascii_case("true") {
                    meta.deprecated = Some(String::new());
                } else if unquoted.eq_ignore_ascii_case("false") {
                    meta.deprecated = None;
                } else if !unquoted.is_empty() {
                    meta.deprecated = Some(unquoted.to_string());
                }
            }
            _ => {}
        }
    }
    Some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_invariant_to_order_and_crlf() {
        let mut map_a = BTreeMap::new();
        map_a.insert("b.txt".into(), "hello\r\nworld\r\n".into());
        map_a.insert("a.txt".into(), "first\nline\n".into());

        let mut map_b = BTreeMap::new();
        // insert in different order
        map_b.insert("a.txt".into(), "first\r\nline\r\n".into());
        map_b.insert("b.txt".into(), "hello\nworld\n".into());

        assert_eq!(content_hash(&map_a), content_hash(&map_b));
        assert!(content_hash(&map_a).starts_with("sha256:"));
    }

    #[test]
    fn parse_frontmatter_reads_version_and_deprecated() {
        let text = "---\nname: my-skill\ndescription: A test\nversion: 1.2.3\ndeprecated: \"use v2 instead\"\n---\nBody";
        let meta = parse_frontmatter(text).expect("parsed frontmatter");
        assert_eq!(meta.name.as_deref(), Some("my-skill"));
        assert_eq!(meta.description.as_deref(), Some("A test"));
        assert_eq!(meta.version.as_deref(), Some("1.2.3"));
        assert_eq!(meta.deprecated.as_deref(), Some("use v2 instead"));

        let text_bool = "---\nname: my-skill\ndescription: A test\ndeprecated: true\n---\nBody";
        let meta = parse_frontmatter(text_bool).unwrap();
        assert_eq!(meta.deprecated.as_deref(), Some(""));
    }
}
