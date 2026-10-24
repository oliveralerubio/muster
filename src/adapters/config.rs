//! Canonical agent configuration as declared in `muster.toml` (`[agents]`, `[mcp.*]`).
//! Adapters translate it into each agent's native files.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `[agents]`: which adapters run and the configuration they share.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    /// Adapter ids to apply (`claude`, `codex`, `opencode`, `antigravity`). Default: every stable adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<Vec<String>>,
    /// Shared instructions (AGENTS.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Instructions>,
    /// Model per adapter id (model identifiers are not portable between agents).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, String>,
    /// Selected provider id per adapter.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(rename = "providers")]
    pub ai_providers: BTreeMap<String, String>,
    /// Tool permission rules (Claude Code syntax).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    /// Lifecycle hooks keyed by snake_case event (`pre_tool_use`, `stop`...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, Hook>,
    /// Agent Skills installed from the config repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillsConfig>,
    /// Custom subagents installed from the config repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<SubagentsConfig>,
}

/// `[agents.skills]`: a directory of the config repository holding one
/// subdirectory per skill (`<source>/<skill>/SKILL.md` plus any extra files).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// Path of the skills directory, relative to the config repository.
    pub source: String,
    /// External git repositories whose skills are vendored into `source`
    /// (`muster skill add --url`). Provenance is pinned in `skills.lock`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote: Vec<SkillRemote>,
}

/// `[[agents.skills.remote]]`: one subscribed upstream skills repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillRemote {
    /// Source id, recorded as the `origin` of its skills in `skills.lock`.
    pub name: String,
    /// Git source over HTTPS, SSH or a local file URL/path.
    pub url: String,
    /// Branch or tag to follow. Default: the remote's default branch.
    #[serde(
        default,
        rename = "ref",
        deserialize_with = "deserialize_git_ref",
        skip_serializing_if = "Option::is_none"
    )]
    pub git_ref: Option<String>,
    /// Subdirectory of the repository holding one directory per skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Only these skills are vendored. Empty: every skill under `path`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
}

pub fn validate_git_ref(reference: &str) -> Result<(), String> {
    if reference.starts_with('-')
        || reference
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(
            "skill source ref must not start with '-' or contain whitespace or control characters"
                .into(),
        );
    }
    Ok(())
}

fn deserialize_git_ref<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let reference = Option::<String>::deserialize(deserializer)?;
    if let Some(reference) = &reference {
        validate_git_ref(reference).map_err(serde::de::Error::custom)?;
    }
    Ok(reference)
}

/// `[agents.subagents]`: a directory of the config repository holding one
/// Markdown file per subagent (`<source>/<name>.md`, YAML frontmatter with
/// `name` and `description`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentsConfig {
    /// Path of the subagents directory, relative to the config repository.
    pub source: String,
}

/// `[agents.instructions]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Instructions {
    /// Path of the canonical AGENTS.md, relative to the config repository.
    #[serde(default = "default_instructions_source")]
    pub source: String,
}

pub const DEFAULT_INSTRUCTIONS_SOURCE: &str = "AGENTS.md";

fn default_instructions_source() -> String {
    DEFAULT_INSTRUCTIONS_SOURCE.to_string()
}

/// `[agents.permissions]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

/// `[agents.hooks.<event>]`: one shell command run on that event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub command: String,
    /// Tool-name pattern for tool events (e.g. `Bash`, `Edit|Write`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
}

/// `[mcp.<name>]`: either a stdio server (`command`) or a remote one (`url`).
/// `env`/`headers` values may be secret references (`native://group/name`);
/// those are never written in plaintext.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

/// Transport of an MCP server after validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport<'a> {
    Stdio { command: &'a str },
    Remote { url: &'a str },
}

impl McpServer {
    /// `Err` with a message when the server declares both or neither of `command`/`url`.
    pub fn transport(&self) -> Result<Transport<'_>, String> {
        match (&self.command, &self.url) {
            (Some(command), None) => Ok(Transport::Stdio { command }),
            (None, Some(url)) => Ok(Transport::Remote { url }),
            (Some(_), Some(_)) => Err("declares both `command` and `url`".into()),
            (None, None) => Err("needs either `command` (stdio) or `url` (remote)".into()),
        }
    }
}

/// A named AI endpoint. Credentials must remain secret references here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AiProvider {
    pub kind: AiProviderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Resolve Claude Code credentials at runtime instead of storing plaintext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_helper: Option<bool>,
}

/// Field-wise profile override. Only a first declaration needs `kind`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AiProviderOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<AiProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Resolve Claude Code credentials at runtime instead of storing plaintext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_helper: Option<bool>,
}

impl From<AiProvider> for AiProviderOverride {
    fn from(provider: AiProvider) -> Self {
        Self {
            kind: Some(provider.kind),
            base_url: provider.base_url,
            default_model: provider.default_model,
            key: provider.key,
            api_key_helper: provider.api_key_helper,
        }
    }
}

impl AiProviderOverride {
    pub fn merge_into(&self, current: &mut AiProvider) {
        if let Some(kind) = self.kind {
            current.kind = kind;
        }
        if self.base_url.is_some() {
            current.base_url = self.base_url.clone();
        }
        if self.default_model.is_some() {
            current.default_model = self.default_model.clone();
        }
        if self.api_key_helper.is_some() {
            current.api_key_helper = self.api_key_helper;
        }
        if self.key.is_some() {
            current.key = self.key.clone();
        }
    }
    pub fn declared(&self) -> Option<AiProvider> {
        Some(AiProvider {
            kind: self.kind?,
            base_url: self.base_url.clone(),
            default_model: self.default_model.clone(),
            key: self.key.clone(),
            api_key_helper: self.api_key_helper,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AiProviderKind {
    Anthropic,
    Openai,
    OpenaiCompatible,
    Ollama,
    Google,
}
