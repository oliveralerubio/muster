//! `muster.toml`: the manifest model and its JSON Schema.
//!
//! Validation is strict: unknown top-level keys are rejected unless they start
//! with `x-` (free extension space, preserved but ignored by Muster).

use std::collections::BTreeMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::MusterError;

pub const FILE_NAME: &str = "muster.toml";
const EXTENSION_PREFIX: &str = "x-";

/// Muster manifest (`muster.toml`). Top-level keys starting with `x-` are free extensions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(
    deny_unknown_fields,
    extend("patternProperties" = { "^x-": {} })
)]
pub struct Manifest {
    /// Manifest format version.
    pub version: u32,
    /// Offline recovery key: its public recipient is added to every secret group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<Recovery>,
    /// Fleet machines, keyed by machine name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub machines: BTreeMap<String, Machine>,
    // Later tickets add members, groups and profiles.
    #[serde(flatten)]
    #[schemars(skip)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Public half of the offline recovery key. The secret half is never stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Recovery {
    /// age x25519 recipient (`age1...`).
    pub recipient: String,
}

/// One machine of the fleet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    /// age x25519 recipient (`age1...`) of the machine identity.
    pub recipient: String,
    /// `<os>-<arch>`, e.g. `linux-x86_64`.
    pub platform: String,
    /// Muster version that last wrote this entry.
    pub muster_version: String,
    /// Agent CLIs detected on the machine (`claude`, `codex`, `opencode`).
    #[serde(default)]
    pub agents: Vec<String>,
    /// RFC 3339 timestamp of the last successful sync, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<String>,
    /// Free-form role of the machine in the fleet (e.g. `workstation`, `server`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Agent configuration profile applied to the machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

impl Manifest {
    pub fn new() -> Self {
        Self {
            version: 1,
            recovery: None,
            machines: BTreeMap::new(),
            extensions: BTreeMap::new(),
        }
    }

    /// Load and validate `<config_dir>/muster.toml`. A missing file is an
    /// operational error (the repo is not initialized).
    pub fn load(config_dir: &Path) -> Result<Self, MusterError> {
        let path = config_dir.join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MusterError::Operational(format!(
                    "not initialized: {} does not exist; run `muster init` first",
                    path.display()
                )))
            }
            Err(e) => return Err(MusterError::Operational(format!("{}: {e}", path.display()))),
        };
        Self::parse(&text, &path.display().to_string())
    }

    /// Write `<config_dir>/muster.toml`.
    pub fn save(&self, config_dir: &Path) -> Result<(), MusterError> {
        let path = config_dir.join(FILE_NAME);
        let text = toml::to_string_pretty(self)
            .map_err(|e| MusterError::Operational(format!("cannot serialize manifest: {e}")))?;
        std::fs::write(&path, text)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))
    }

    /// Parse and validate manifest text; `path` is only used in diagnostics.
    pub fn parse(text: &str, path: &str) -> Result<Self, MusterError> {
        let invalid = |message: String| MusterError::InvalidConfig {
            message,
            path: Some(path.to_string()),
        };
        let manifest: Self = toml::from_str(text).map_err(|e| invalid(e.message().to_string()))?;
        if let Some(key) = manifest
            .extensions
            .keys()
            .find(|k| !k.starts_with(EXTENSION_PREFIX))
        {
            return Err(invalid(format!(
                "unknown key `{key}` (extension keys must start with `{EXTENSION_PREFIX}`)"
            )));
        }
        Ok(manifest)
    }
}

/// JSON Schema of `muster.toml`, as exported by `muster schema`.
pub fn json_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(Manifest)).expect("schema serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_version_and_extension_keys() {
        let m = Manifest::parse("version = 1\nx-team = \"core\"\n", "muster.toml").unwrap();
        assert_eq!(m.version, 1);
        assert!(m.extensions.contains_key("x-team"));
    }

    #[test]
    fn rejects_unknown_keys_as_invalid_config() {
        let err = Manifest::parse("version = 1\nbogus = true\n", "muster.toml").unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn rejects_missing_version() {
        let err = Manifest::parse("", "muster.toml").unwrap_err();
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn machines_round_trip_through_toml() {
        let mut m = Manifest::new();
        m.recovery = Some(Recovery {
            recipient: "age1recovery".into(),
        });
        m.machines.insert(
            "laptop".into(),
            Machine {
                recipient: "age1laptop".into(),
                platform: "linux-x86_64".into(),
                muster_version: "0.1.0".into(),
                agents: vec!["claude".into()],
                last_sync: None,
                role: Some("workstation".into()),
                profile: None,
            },
        );
        let text = toml::to_string_pretty(&m).unwrap();
        assert_eq!(Manifest::parse(&text, "muster.toml").unwrap(), m);
    }
}
