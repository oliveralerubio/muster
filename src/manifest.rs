//! `muster.toml`: the manifest model and its JSON Schema.
//!
//! Validation is strict: unknown top-level keys are rejected unless they start
//! with `x-` (free extension space, preserved but ignored by Muster).

use std::collections::BTreeMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::adapters::config::{AgentsConfig, McpServer};
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
    /// Team members, keyed by member name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub members: BTreeMap<String, Member>,
    /// Secret groups, keyed by group name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Group>,
    /// Agent configuration profiles, keyed by profile name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Profile>,
    /// Agent configuration: enabled adapters, shared instructions, hooks,
    /// permissions. In team mode this is the approved team profile (the
    /// lowest-precedence layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentsConfig>,
    /// Declared MCP servers (stdio and remote), keyed by server name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServer>,
    /// Managed dotfiles, keyed by target path relative to `$HOME`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, ManagedDotfile>,
    /// Configuration for the built-in MCP server (`muster mcp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<McpServerConfig>,
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

/// A whole file Muster writes under `$HOME` (`muster sync`), with its
/// contents stored in the config repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManagedDotfile {
    /// Path of the contents inside the config repo (e.g. `files/.gitconfig`).
    pub source: String,
    /// Written with mode 0600 and never shown in diffs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub private: bool,
    /// Written executable (0755, or 0700 when private).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
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
    /// Digest of the team-approved layers this machine last applied
    /// (written by `sync`/`agents apply`; compared by `status`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_profile: Option<String>,
}

/// Team role of a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    #[default]
    Member,
}

/// Lifecycle of a member: invited (pending) until an admin approves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemberStatus {
    #[default]
    Pending,
    Active,
}

/// A person of the team with one or more machines.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Member {
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub status: MemberStatus,
    /// Names of the member's machines (entries of `[machines]`, or names
    /// reserved for machines that have not enrolled yet).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<String>,
    /// Personal agent profile (layered above group profiles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Member who invited this member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invited_by: Option<String>,
    /// Admin who approved the invitation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
}

/// A secret group: one vault file encrypted for its readers (+ the recovery key).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Group {
    #[serde(default)]
    pub readers: Readers,
    /// Agent profile applied to the group's member readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Readers of a secret group.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Readers {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// Other groups whose readers also read this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<String>,
    /// Raw age recipients (`age1...`) outside the fleet (e.g. CI keys).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<String>,
}

impl Readers {
    /// Remove `reader` from members, groups, machines and raw recipients.
    /// Returns true if any list changed.
    pub fn remove(&mut self, reader: &str) -> bool {
        let mut lists = [
            &mut self.members,
            &mut self.groups,
            &mut self.machines,
            &mut self.recipients,
        ];
        let mut removed = false;
        for list in &mut lists {
            let before = list.len();
            list.retain(|r| r != reader);
            removed |= list.len() != before;
        }
        removed
    }
}

/// A named agent configuration layer.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentsConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServer>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

/// Built-in MCP server configuration (`muster mcp`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Secret references explicitly allowed to be revealed through MCP.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reveal: Vec<String>,
}

impl Manifest {
    pub fn new() -> Self {
        Self {
            version: 1,
            recovery: None,
            machines: BTreeMap::new(),
            members: BTreeMap::new(),
            groups: BTreeMap::new(),
            profiles: BTreeMap::new(),
            agents: None,
            mcp: BTreeMap::new(),
            files: BTreeMap::new(),
            mcp_server: None,
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
                applied_profile: None,
            },
        );
        let text = toml::to_string_pretty(&m).unwrap();
        assert_eq!(Manifest::parse(&text, "muster.toml").unwrap(), m);
    }
}
