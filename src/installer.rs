//! Agent CLI installer: pinned versions from `muster.lock` in the config repo
//! (mise-style: agent -> version, url, sha256). Every artifact is verified
//! against its SHA-256 before anything is written; a mismatch installs nothing.
//!
//! ```toml
//! [agents.claude]
//! version = "1.2.3"
//! url = "https://example.com/claude-1.2.3"
//! sha256 = "<64 hex chars>"
//! ```

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::MusterError;
use crate::hash::sha256_hex;

pub const LOCK_FILE: &str = "muster.lock";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    #[serde(default)]
    pub agents: BTreeMap<String, Pin>,
}

/// One downloadable agent executable and its checksum.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
}

/// One pinned agent CLI, with platform-specific artifacts and a legacy fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub version: String,
    /// Legacy `file://` or `https://` URL of the executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Legacy hex SHA-256 of the executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Installed file name (defaults to the adapter's binary, else the agent id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifacts: BTreeMap<String, Artifact>,
}

impl Pin {
    pub fn resolve_artifact(&self, platform: &str) -> Option<Artifact> {
        self.artifacts.get(platform).cloned().or_else(|| {
            Some(Artifact {
                url: self.url.clone()?,
                sha256: self.sha256.clone()?,
            })
        })
    }
}

/// Stable identity for an approval, scoped to the caller's target in its state.
pub fn approval_identity(agent: &str, pin: &Pin) -> Option<String> {
    let platform = crate::machine::platform();
    let artifact = pin.resolve_artifact(&platform)?;
    let identity = serde_json::to_vec(&(
        agent,
        &platform,
        &pin.version,
        &artifact.url,
        artifact.sha256.to_ascii_lowercase(),
    ))
    .expect("serializing strings cannot fail");
    Some(sha256_hex(&identity))
}

impl Lock {
    /// An absent lock is valid when planning configuration-only syncs.
    pub fn load_or_default(config_dir: &Path) -> Result<Self, MusterError> {
        let path = config_dir.join(LOCK_FILE);
        if path
            .try_exists()
            .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?
        {
            Self::load(config_dir)
        } else {
            Ok(Self::default())
        }
    }

    pub fn load(config_dir: &Path) -> Result<Self, MusterError> {
        let path = config_dir.join(LOCK_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MusterError::Operational(format!(
                    "no {LOCK_FILE} in {}: pin agent versions there first",
                    config_dir.display()
                ))
            } else {
                MusterError::Operational(format!("{}: {e}", path.display()))
            }
        })?;
        let invalid = |message: String| MusterError::InvalidConfig {
            message,
            path: Some(path.display().to_string()),
        };
        let lock: Self = toml::from_str(&text).map_err(|e| invalid(e.message().to_string()))?;
        for (agent, pin) in &lock.agents {
            let checksums = pin
                .sha256
                .iter()
                .map(|sha256| (format!("agents.{agent}.sha256"), sha256))
                .chain(pin.artifacts.iter().map(|(platform, artifact)| {
                    (
                        format!("agents.{agent}.artifacts.{platform}.sha256"),
                        &artifact.sha256,
                    )
                }));
            for (field, sha256) in checksums {
                if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(invalid(format!(
                        "{field} must be 64 hexadecimal characters"
                    )));
                }
            }
            if let Some(bin) = &pin.bin {
                crate::machine::validate_name(bin)?;
            }
        }
        Ok(lock)
    }
}

/// What happened (or would happen) to one agent.
#[derive(Debug, Serialize)]
pub struct Outcome {
    pub agent: String,
    pub version: String,
    pub path: PathBuf,
    pub sha256: Option<String>,
    /// The detected version when a newer installation was kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// `install`, `unchanged`, or `kept_newer`.
    pub action: &'static str,
}

/// Fetch, verify and (unless `dry_run`) install `pin` as `<bin_dir>/<bin>`.
pub fn install(
    agent: &str,
    pin: &Pin,
    bin_dir: &Path,
    dry_run: bool,
) -> Result<Outcome, MusterError> {
    let installed = installed_version(agent, pin, bin_dir)?;
    let platform = crate::machine::platform();
    let artifact = pin.resolve_artifact(&platform);
    let path = bin_dir.join(default_bin(agent, pin));
    let outcome = |action, installed_version| Outcome {
        agent: agent.to_string(),
        version: pin.version.clone(),
        path: path.clone(),
        sha256: artifact.as_ref().map(|a| a.sha256.to_ascii_lowercase()),
        installed_version,
        action,
    };
    if let Some(version) = installed.as_deref().filter(|v| is_newer(v, &pin.version)) {
        return Ok(outcome("kept_newer", Some(version.to_string())));
    }
    let artifact = artifact.as_ref().ok_or_else(|| {
        MusterError::Operational(format!(
            "no artifact for platform {platform} for agent {agent} {} in {LOCK_FILE}",
            pin.version
        ))
    })?;
    let expected = artifact.sha256.to_ascii_lowercase();
    if installed.as_deref() == Some(pin.version.as_str()) {
        return Ok(outcome("unchanged", None));
    }

    let bytes = fetch(&artifact.url)?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(MusterError::Security(format!(
            "checksum mismatch for {agent} {}: {LOCK_FILE} pins sha256 {expected}, the download from {} has {actual}; nothing was installed",
            pin.version, artifact.url
        )));
    }
    if !dry_run {
        write_executable(&path, &bytes)?;
    }
    Ok(outcome("install", None))
}

/// Compare recognizable versions conservatively; unknown versions are not newer.
pub fn is_newer(installed: &str, pinned: &str) -> bool {
    fn parse(value: &str) -> Option<([u64; 3], Option<&str>)> {
        let value = value.strip_prefix('v').unwrap_or(value);
        let (value, build) = value
            .split_once('+')
            .map_or((value, None), |(a, b)| (a, Some(b)));
        let (core, pre) = value
            .split_once('-')
            .map_or((value, None), |(a, b)| (a, Some(b)));
        let valid_identifiers = |s: &str| {
            !s.is_empty()
                && s.split('.').all(|part| {
                    !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        };
        if build.is_some_and(|s| !valid_identifiers(s))
            || pre.is_some_and(|s| !valid_identifiers(s))
        {
            return None;
        }
        let parts: Vec<_> = core.split('.').collect();
        if !(2..=3).contains(&parts.len()) {
            return None;
        }
        let mut numbers = [0; 3];
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            numbers[i] = part.parse().ok()?;
        }
        Some((numbers, pre))
    }
    match (parse(installed), parse(pinned)) {
        (Some((a, pre_a)), Some((b, pre_b))) => {
            use std::cmp::Ordering;
            match a.cmp(&b) {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => match (pre_a, pre_b) {
                    (None, Some(_)) => true,
                    (Some(a), Some(b)) => {
                        let mut left = a.split('.');
                        let mut right = b.split('.');
                        loop {
                            let order = match (left.next(), right.next()) {
                                (None, None) => break false,
                                (None, Some(_)) => break false,
                                (Some(_), None) => break true,
                                (Some(a), Some(b)) => {
                                    let numeric_a = a.bytes().all(|c| c.is_ascii_digit());
                                    let numeric_b = b.bytes().all(|c| c.is_ascii_digit());
                                    match (numeric_a, numeric_b) {
                                        (true, true) => {
                                            let a = a.trim_start_matches('0');
                                            let b = b.trim_start_matches('0');
                                            a.len().cmp(&b.len()).then_with(|| a.cmp(b))
                                        }
                                        (true, false) => Ordering::Less,
                                        (false, true) => Ordering::Greater,
                                        (false, false) => a.cmp(b),
                                    }
                                }
                            };
                            if order != Ordering::Equal {
                                break order == Ordering::Greater;
                            }
                        }
                    }
                    _ => false,
                },
            }
        }
        _ => false,
    }
}

pub fn newer_diagnostic(agent: &str, installed: &str, pinned: &str) -> crate::output::Diagnostic {
    crate::output::Diagnostic::new(
        crate::output::Severity::Warning,
        "agent_newer_than_pin",
        format!("agent {agent} {installed} is newer than pin {pinned}; keeping installed version; bump the pin in muster.lock"),
    )
}

/// `pin.bin`, else the adapter's executable name (`antigravity` -> `agy`),
/// else the agent id.
pub fn default_bin<'a>(agent: &'a str, pin: &'a Pin) -> &'a str {
    pin.bin.as_deref().unwrap_or_else(|| {
        crate::adapters::find(agent)
            .map(|a| a.binary)
            .unwrap_or(agent)
    })
}

/// Shared destination for pinned agent executables.
pub fn bin_dir() -> Result<PathBuf, MusterError> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".local/bin"))
        .ok_or_else(|| MusterError::Operational("HOME is not set".into()))
}

/// Prefer the installed executable's reported version, using its hash only
/// when no semver-like token can be determined. Never fetches an artifact.
pub fn installed_version(
    agent: &str,
    pin: &Pin,
    bin_dir: &Path,
) -> Result<Option<String>, MusterError> {
    let binary = default_bin(agent, pin);
    crate::machine::validate_name(binary)?;
    let local = bin_dir.join(binary);
    let executable = if local.is_file() {
        Some(local)
    } else {
        crate::machine::find_executable(binary)
    };
    let Some(path) = executable else {
        return Ok(None);
    };
    let version = crate::machine::version_output(&path).ok().and_then(|text| {
        text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '/'))
            .map(|word| word.trim_start_matches('v'))
            .find(|word| {
                let core = word.split(['-', '+']).next().unwrap_or("");
                let parts: Vec<_> = core.split('.').collect();
                (2..=3).contains(&parts.len())
                    && parts
                        .iter()
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            })
            .map(str::to_string)
    });
    if version.is_some() {
        return Ok(version);
    }
    let artifact = pin.resolve_artifact(&crate::machine::platform());
    Ok(artifact.and_then(|artifact| {
        std::fs::read(path)
            .ok()
            .filter(|bytes| sha256_hex(bytes).eq_ignore_ascii_case(&artifact.sha256))
            .map(|_| pin.version.clone())
    }))
}

/// `file://` is read directly; `https://` goes through `curl` (exit 4 if missing).
fn fetch(url: &str) -> Result<Vec<u8>, MusterError> {
    const MAX_BYTES: u64 = 512 * 1024 * 1024;
    if let Some(path) = url.strip_prefix("file://") {
        let file = std::fs::File::open(path)
            .map_err(|e| MusterError::Operational(format!("cannot read {url}: {e}")))?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| MusterError::Operational(format!("cannot read {url}: {e}")))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(MusterError::Operational(format!(
                "artifact from {url} exceeds size limit"
            )));
        }
        return Ok(bytes);
    }
    if !url.starts_with("https://") {
        return Err(MusterError::Usage(format!(
            "unsupported artifact URL {url}: use https:// or file://"
        )));
    }
    let curl = crate::machine::find_executable("curl").ok_or_else(|| {
        MusterError::Unavailable(format!("downloading {url} needs `curl` on PATH"))
    })?;
    let mut child = std::process::Command::new(curl)
        .args([
            "--fail",
            "--silent",
            "--location",
            "--connect-timeout",
            "10",
            "--max-time",
            "300",
            "--speed-limit",
            "1024",
            "--speed-time",
            "30",
            "--max-filesize",
            "536870912",
            "--",
            url,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| MusterError::Unavailable(format!("cannot run curl: {e}")))?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .expect("piped stdout")
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > MAX_BYTES {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|e| MusterError::Operational(format!("download of {url} failed: {e}")))?;
    read.map_err(|e| MusterError::Operational(format!("download of {url} failed: {e}")))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(MusterError::Operational(format!(
            "artifact from {url} exceeds size limit"
        )));
    }
    if !status.success() {
        return Err(MusterError::Operational(format!(
            "download of {url} failed ({status})"
        )));
    }
    Ok(bytes)
}

/// Atomic write (temp file + rename) with mode 0755.
fn write_executable(path: &Path, bytes: &[u8]) -> Result<(), MusterError> {
    let io = |e: std::io::Error| MusterError::Operational(format!("{}: {e}", path.display()));
    let dir = path.parent().expect("bin path has a parent");
    std::fs::create_dir_all(dir).map_err(io)?;
    let tmp = dir.join(format!(
        ".{}.muster-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("agent"),
        std::process::id()
    ));
    let mut file = std::fs::File::create(&tmp).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(io)
}

#[cfg(test)]
mod version_order_tests {
    use super::is_newer;

    #[test]
    fn approval_identity_changes_with_artifact() {
        use super::{approval_identity, Pin};
        let mut pin = Pin {
            version: "1.0.0".into(),
            url: Some("https://example.com/a".into()),
            sha256: Some("a".repeat(64)),
            bin: None,
            artifacts: Default::default(),
        };
        let original = approval_identity("agent", &pin);
        pin.url = Some("https://example.com/b".into());
        assert_ne!(original, approval_identity("agent", &pin));
        pin.url = Some("https://example.com/a".into());
        pin.sha256 = Some("b".repeat(64));
        assert_ne!(original, approval_identity("agent", &pin));
    }

    #[test]
    fn compares_versions_conservatively() {
        for (a, b, expected) in [
            ("v2.0", "1.9.9", true),
            ("1.2.1", "1.2", true),
            ("1.2", "1.2.0", false),
            ("1.2.0", "1.2.0-rc.1", true),
            ("1.2.0-rc.1", "1.2.0", false),
            ("1.2.0-rc.10", "1.2.0-rc.2", true),
            ("1.2.0+build", "1.2.0", false),
            ("1.0", "2.0", false),
            ("unknown", "1.0", false),
            ("2.0", "broken", false),
            ("2.0-", "1.0", false),
            ("999999999999999999999999.0", "1.0", false),
        ] {
            assert_eq!(is_newer(a, b), expected, "{a} vs {b}");
        }
    }
}
