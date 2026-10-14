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
    /// Tool permission rules (Claude Code syntax).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Permissions>,
    /// Lifecycle hooks keyed by snake_case event (`pre_tool_use`, `stop`...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, Hook>,
    /// Agent Skills installed from the config repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillsConfig>,
}

/// `[agents.skills]`: a directory of the config repository holding one
/// subdirectory per skill (`<source>/<skill>/SKILL.md` plus any extra files).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// Path of the skills directory, relative to the config repository.
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
