//! Facts about the local machine: its fleet name, platform and installed agents.

use std::fs;
use std::path::{Path, PathBuf};

use crate::errors::MusterError;

const NAME_FILE: &str = "machine";

/// Agent CLIs Muster knows how to manage.
pub const KNOWN_AGENTS: &[&str] = &["claude", "codex", "opencode"];

/// `<os>-<arch>`, e.g. `linux-x86_64`.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A fleet name derived from the hostname: lowercase `[a-z0-9-]`, else `default`.
pub fn default_name() -> String {
    let raw = fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        })
        .unwrap_or_default();
    let host = raw.trim().split('.').next().unwrap_or_default();
    let name: String = host
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "default".into()
    } else {
        name.into()
    }
}

/// This machine's fleet name, recorded in `<state_dir>/machine`.
pub fn load_name(state_dir: &Path) -> Option<String> {
    fs::read_to_string(state_dir.join(NAME_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn save_name(state_dir: &Path, name: &str) -> Result<(), MusterError> {
    let path = state_dir.join(NAME_FILE);
    fs::write(&path, format!("{name}\n"))
        .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))
}

/// Known agent CLIs found as executables on `PATH`.
pub fn detect_agents() -> Vec<String> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    KNOWN_AGENTS
        .iter()
        .filter(|agent| dirs.iter().any(|d| is_executable(&d.join(agent))))
        .map(|agent| agent.to_string())
        .collect()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file() || path.with_extension("exe").is_file()
}
