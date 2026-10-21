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
use std::io::Write;
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

impl Lock {
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
    pub sha256: String,
    /// `install` or `unchanged`.
    pub action: &'static str,
}

/// Fetch, verify and (unless `dry_run`) install `pin` as `<bin_dir>/<bin>`.
pub fn install(
    agent: &str,
    pin: &Pin,
    bin_dir: &Path,
    dry_run: bool,
) -> Result<Outcome, MusterError> {
    let platform = crate::machine::platform();
    let artifact = pin.resolve_artifact(&platform).ok_or_else(|| {
        MusterError::Operational(format!(
            "no artifact for platform {platform} for agent {agent} {} in {LOCK_FILE}",
            pin.version
        ))
    })?;
    let path = bin_dir.join(default_bin(agent, pin));
    let expected = artifact.sha256.to_ascii_lowercase();
    let outcome = |action| Outcome {
        agent: agent.to_string(),
        version: pin.version.clone(),
        path: path.clone(),
        sha256: expected.clone(),
        action,
    };
    if std::fs::read(&path).is_ok_and(|bytes| sha256_hex(&bytes) == expected) {
        return Ok(outcome("unchanged"));
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
    Ok(outcome("install"))
}

/// `pin.bin`, else the adapter's executable name (`antigravity` -> `agy`),
/// else the agent id.
fn default_bin<'a>(agent: &'a str, pin: &'a Pin) -> &'a str {
    pin.bin.as_deref().unwrap_or_else(|| {
        crate::adapters::find(agent)
            .map(|a| a.binary)
            .unwrap_or(agent)
    })
}

/// `file://` is read directly; `https://` goes through `curl` (exit 4 if missing).
fn fetch(url: &str) -> Result<Vec<u8>, MusterError> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path)
            .map_err(|e| MusterError::Operational(format!("cannot read {url}: {e}")));
    }
    if !url.starts_with("https://") {
        return Err(MusterError::Usage(format!(
            "unsupported artifact URL {url}: use https:// or file://"
        )));
    }
    let curl = crate::machine::find_executable("curl").ok_or_else(|| {
        MusterError::Unavailable(format!("downloading {url} needs `curl` on PATH"))
    })?;
    let output = std::process::Command::new(curl)
        .args(["--fail", "--silent", "--show-error", "--location", url])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| MusterError::Unavailable(format!("cannot run curl: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MusterError::Operational(format!(
            "download of {url} failed: {}",
            stderr.trim()
        )));
    }
    Ok(output.stdout)
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
