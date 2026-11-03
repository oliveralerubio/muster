//! `muster.toml`: the manifest model and its JSON Schema.
//!
//! Validation is strict: unknown top-level keys are rejected unless they start
//! with `x-` (free extension space, preserved but ignored by Muster).

use std::collections::BTreeMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::adapters::config::{AgentsConfig, AiProvider, McpServer, SkillRemote, SkillsConfig};
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(rename = "providers")]
    pub ai_providers: BTreeMap<String, AiProvider>,
    /// Managed dotfiles, keyed by target path relative to `$HOME`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, ManagedDotfile>,
    /// Per-secret agent access, keyed by secret reference
    /// (`native://app/api`, `env://VAR`, ...). Deny by default: a reference
    /// with no `allow` entry reaches no agent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, SecretPolicy>,
    /// Configuration for the built-in MCP server (`muster mcp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<McpServerConfig>,
    /// Team governance settings (`[team]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<TeamConfig>,
    // Later tickets add members, groups and profiles.
    #[serde(flatten)]
    #[schemars(skip)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

/// Team governance settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TeamConfig {
    /// Distinct admins that must approve a machine enrollment or a member
    /// before it takes effect (>= 1). The recovery key always counts as a
    /// full approval (break-glass).
    #[serde(default = "default_required_approvals")]
    #[schemars(range(min = 1))]
    pub required_approvals: u32,
}

fn default_required_approvals() -> u32 {
    1
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
    /// Admins who approved the invitation so far, while it waits for the
    /// team quorum (`[team] required_approvals`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<String>,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(rename = "providers")]
    pub ai_providers: BTreeMap<String, crate::adapters::config::AiProviderOverride>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

/// Which agents may receive a secret (`[secrets."<reference>"]`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretPolicy {
    /// Agents allowed to receive the secret: an adapter id (`claude`) grants
    /// it everywhere; `agent@project` only inside that project (the project
    /// directory's name). Empty = no agent (the human CLI always can).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default)]
    pub approval: SecretApproval,
    pub approval_ttl: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SecretApproval {
    #[default]
    None,
    Each,
}

impl SecretPolicy {
    /// Whether `agent` may receive the secret when rendering for `project`
    /// (`None` = user scope).
    pub fn allows(&self, agent: &str, project: Option<&str>) -> bool {
        self.allow.iter().any(|entry| match entry.split_once('@') {
            None => entry == agent,
            Some((a, p)) => a == agent && project == Some(p),
        })
    }
}

/// `agent` or `agent@project`, with `agent` a known adapter id.
pub fn validate_allow_entry(entry: &str) -> Result<(), String> {
    let (agent, project) = match entry.split_once('@') {
        Some((a, p)) => (a, Some(p)),
        None => (entry, None),
    };
    if crate::adapters::find(agent).is_none() {
        let known: Vec<_> = crate::adapters::ADAPTERS.iter().map(|a| a.id).collect();
        return Err(format!(
            "unknown agent `{agent}` in allow entry `{entry}` (known: {})",
            known.join(", ")
        ));
    }
    if project.is_some_and(|p| p.is_empty() || p.contains(['/', '\\', '@'])) {
        return Err(format!(
            "invalid allow entry `{entry}`: expected <agent> or <agent>@<project>"
        ));
    }
    Ok(())
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
            ai_providers: BTreeMap::new(),
            files: BTreeMap::new(),
            secrets: BTreeMap::new(),
            mcp_server: None,
            team: None,
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
        for (id, key) in manifest
            .ai_providers
            .iter()
            .map(|(id, p)| (id, &p.key))
            .chain(
                manifest
                    .profiles
                    .values()
                    .flat_map(|p| p.ai_providers.iter().map(|(id, p)| (id, &p.key))),
            )
        {
            crate::machine::validate_name(id)
                .map_err(|_| invalid(format!("invalid provider id `{id}`")))?;
            if let Some(key) = key {
                crate::secrets::SecretRef::parse(key).map_err(|_| {
                    invalid(format!(
                        "providers.{id}.key must be a valid secret reference"
                    ))
                })?;
            }
        }
        let declared_ai_providers: std::collections::BTreeSet<_> = manifest
            .ai_providers
            .keys()
            .chain(manifest.profiles.values().flat_map(|profile| {
                profile
                    .ai_providers
                    .iter()
                    .filter_map(|(id, provider)| provider.kind.is_some().then_some(id))
            }))
            .collect();
        for profile in manifest.profiles.values() {
            for (id, provider) in &profile.ai_providers {
                if provider.kind.is_none() && !declared_ai_providers.contains(id) {
                    return Err(invalid(format!(
                        "providers.{id}: kind required on first declaration"
                    )));
                }
            }
        }
        // Validate actual layer order, not just the union of profile names.
        for machine in
            std::iter::once(None).chain(manifest.machines.keys().map(|m| Some(m.as_str())))
        {
            let layers = crate::engine::layer::global_layers(Path::new("."), &manifest, machine);
            let mut declared = std::collections::BTreeSet::new();
            for layer in layers {
                for (id, provider) in layer.ai_providers {
                    if provider.kind.is_none() && !declared.contains(&id) {
                        return Err(invalid(format!(
                            "providers.{id}: kind required on first declaration"
                        )));
                    }
                    declared.insert(id);
                }
            }
        }
        for agents in manifest
            .agents
            .iter()
            .chain(manifest.profiles.values().filter_map(|p| p.agents.as_ref()))
        {
            if let Some(toggles) = &agents.toggles {
                for item in toggles.disable.iter().chain(&toggles.enable) {
                    crate::adapters::config::parse_toggle(item).map_err(invalid)?;
                }
            }
            for (agent, entries) in &agents.packages {
                if entries.iter().any(|e| e.trim().is_empty()) {
                    return Err(invalid(format!(
                        "agents.packages.{agent}: package entries must not be empty"
                    )));
                }
            }
            for (agent, id) in &agents.ai_providers {
                let known = manifest.ai_providers.contains_key(id)
                    || manifest
                        .profiles
                        .values()
                        .any(|p| p.ai_providers.contains_key(id));
                if crate::adapters::find(agent).is_none() || !known {
                    return Err(invalid(format!(
                        "agents.providers.{agent}: unknown agent or provider id `{id}`"
                    )));
                }
            }
        }
        for (reference, policy) in &manifest.secrets {
            if let Some(ttl) = &policy.approval_ttl {
                crate::secret_leases::parse_ttl(ttl)
                    .map_err(|e| invalid(format!("secrets.\"{reference}\".approval_ttl: {e}")))?;
            }
            crate::secrets::SecretRef::parse(reference)
                .map_err(|e| invalid(format!("secrets: {e}")))?;
            for entry in &policy.allow {
                validate_allow_entry(entry)
                    .map_err(|e| invalid(format!("secrets.\"{reference}\".allow: {e}")))?;
            }
        }
        if manifest.required_approvals() == 0 {
            return Err(invalid(
                "`team.required_approvals` must be at least 1".to_string(),
            ));
        }
        Ok(manifest)
    }

    /// Distinct admin approvals a sensitive team change needs (default 1).
    pub fn required_approvals(&self) -> u32 {
        self.team.as_ref().map_or(1, |t| t.required_approvals)
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
    fn provider_runtime_helper_round_trips_and_profiles_can_disable_it() {
        let text = "version = 1\n[providers.gateway]\nkind = 'anthropic'\napi_key_helper = true\n[profiles.work.providers.gateway]\napi_key_helper = false\n";
        let manifest = Manifest::parse(text, "muster.toml").unwrap();
        let serialized = toml::to_string(&manifest).unwrap();
        let restored = Manifest::parse(&serialized, "muster.toml").unwrap();
        let mut provider = restored.ai_providers["gateway"].clone();
        assert_eq!(provider.api_key_helper, Some(true));
        restored.profiles["work"].ai_providers["gateway"].merge_into(&mut provider);
        assert_eq!(provider.api_key_helper, Some(false));
        provider.api_key_helper = None;
        assert!(!toml::to_string(&provider)
            .unwrap()
            .contains("api_key_helper"));
    }

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
    fn team_required_approvals_defaults_to_one_and_rejects_zero() {
        let m = Manifest::parse("version = 1\n[team]\n", "muster.toml").unwrap();
        assert_eq!(m.required_approvals(), 1);
        let m = Manifest::parse(
            "version = 1\n[team]\nrequired_approvals = 2\n",
            "muster.toml",
        )
        .unwrap();
        assert_eq!(m.required_approvals(), 2);
        let err = Manifest::parse(
            "version = 1\n[team]\nrequired_approvals = 0\n",
            "muster.toml",
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn secret_allow_is_deny_by_default_and_scopes_agent_at_project() {
        let m = Manifest::parse(
            "version = 1\n[secrets.\"env://A\"]\nallow = [\"claude\", \"codex@repo\"]\n[secrets.\"env://B\"]\n",
            "muster.toml",
        )
        .unwrap();
        let a = &m.secrets["env://A"];
        assert!(a.allows("claude", None) && a.allows("claude", Some("other")));
        assert!(!a.allows("codex", None) && !a.allows("codex", Some("other")));
        assert!(a.allows("codex", Some("repo")));
        assert!(!m.secrets["env://B"].allows("claude", None));
        for bad in [
            "version = 1\n[secrets.\"env://A\"]\nallow = [\"nobody\"]\n",
            "version = 1\n[secrets.\"env://A\"]\nallow = [\"claude@\"]\n",
            "version = 1\n[secrets.GLOBAL]\nallow = [\"claude\"]\n",
        ] {
            assert_eq!(
                Manifest::parse(bad, "muster.toml").unwrap_err().exit_code(),
                3,
                "{bad}"
            );
        }
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
