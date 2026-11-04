//! Facts about the local machine: its fleet name, platform and installed agents.

use std::fs;
use std::path::{Path, PathBuf};

use crate::errors::MusterError;

const NAME_FILE: &str = "machine";

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

/// Fleet machine names: lowercase letters, digits and `-`.
pub fn validate_name(name: &str) -> Result<String, MusterError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(name.to_string())
    } else {
        Err(MusterError::Usage(format!(
            "invalid machine name '{name}': use lowercase letters, digits and '-'"
        )))
    }
}

/// This machine's fleet name, recorded in `<state_dir>/machine`.
pub fn load_name(state_dir: &Path) -> Option<String> {
    fs::read_to_string(state_dir.join(NAME_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Timestamp of this machine's last sync, from `<state_dir>/sync-status.json`.
/// The manifest's per-machine `last_sync` is never written, so this is the
/// only record of a sync on the current machine.
pub fn local_last_sync(state_dir: &Path) -> Option<String> {
    let bytes = fs::read(state_dir.join("sync-status.json")).ok()?;
    let status: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    status["ts"]
        .as_str()
        .filter(|ts| !ts.is_empty())
        .map(str::to_owned)
}

pub fn save_name(state_dir: &Path, name: &str) -> Result<(), MusterError> {
    let path = state_dir.join(NAME_FILE);
    fs::write(&path, format!("{name}\n"))
        .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))
}

/// Ids of the available agents whose CLI is found as an executable on `PATH`.
pub fn detect_agents() -> Vec<String> {
    crate::adapters::ADAPTERS
        .iter()
        .filter(|a| a.available())
        .filter(|a| find_executable(a.binary).is_some())
        .map(|a| a.id.to_string())
        .collect()
}

/// First executable named `name` on `PATH`.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// First line `<exe> --version` prints, or why it could not be read. Gives up
/// after a few seconds so a hung CLI never hangs the caller.
pub fn version_output(exe: &Path) -> Result<String, String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "printed nothing".into())
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
