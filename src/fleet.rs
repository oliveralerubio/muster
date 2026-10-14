//! Machine enrollment requests: files under `<config_dir>/enroll/<name>.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::MusterError;

const ENROLL_DIR: &str = "enroll";

/// Machine enrollment request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrollmentRequest {
    pub name: String,
    pub recipient: String,
    pub platform: String,
    pub muster_version: String,
    #[serde(default)]
    pub agents: Vec<String>,
    pub created_at: String,
}

impl EnrollmentRequest {
    pub fn file_path(config_dir: &Path, name: &str) -> PathBuf {
        config_dir.join(ENROLL_DIR).join(format!("{name}.toml"))
    }

    pub fn save(&self, config_dir: &Path) -> Result<PathBuf, MusterError> {
        let dir = config_dir.join(ENROLL_DIR);
        fs::create_dir_all(&dir)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", dir.display())))?;
        let path = Self::file_path(config_dir, &self.name);
        let text = toml::to_string_pretty(self)
            .map_err(|e| MusterError::Operational(format!("cannot serialize enrollment: {e}")))?;
        fs::write(&path, text)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
        Ok(path)
    }

    pub fn load(config_dir: &Path, name: &str) -> Result<Self, MusterError> {
        let path = Self::file_path(config_dir, name);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MusterError::Operational(format!(
                    "no enrollment request found for '{name}' at {}",
                    path.display()
                )))
            }
            Err(e) => return Err(MusterError::Operational(format!("{}: {e}", path.display()))),
        };
        toml::from_str(&text).map_err(|e| MusterError::InvalidConfig {
            message: format!("corrupt enrollment request: {e}"),
            path: Some(path.display().to_string()),
        })
    }

    pub fn remove(config_dir: &Path, name: &str) -> Result<(), MusterError> {
        let path = Self::file_path(config_dir, name);
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
        }
        Ok(())
    }
}
