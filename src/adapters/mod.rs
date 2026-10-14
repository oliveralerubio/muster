//! Agent adapters: translate the canonical agent configuration into each
//! agent's native files.
//!
//! Every adapter is a static [`Adapter`] declaring its managed files, scopes,
//! capabilities, platforms and minimum version, plus a PURE render step:
//! `(Desired, Current) -> RenderPlan`. Render never touches the disk; callers
//! (`muster agents apply`, the sync engine) read the current contents, call
//! [`Adapter::render`] and write the results. Render only rewrites the keys it
//! manages and keeps everything else in the file.

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod config;
pub mod opencode;

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::errors::MusterError;
use crate::output::{Diagnostic, Severity};
use crate::secrets::SecretValue;
use config::{AgentsConfig, McpServer, Transport};

/// Where a managed path is rooted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Base {
    /// `$HOME`.
    Home,
    /// `$XDG_CONFIG_HOME`, else `$HOME/.config`.
    XdgConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Applies to every project of the user.
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    Json,
    Toml,
    Markdown,
    /// Whole file owned by Muster (managed dotfiles).
    Text,
}

/// A file an adapter manages (only some keys/blocks inside it).
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ManagedFile {
    pub base: Base,
    /// Path relative to `base`. Also the key of this file in [`Current`].
    pub path: &'static str,
    /// Path relative to a project root when the agent reads a project-scoped
    /// equivalent of this file; `None` = user scope only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<&'static str>,
    pub scope: Scope,
    pub format: FileFormat,
    pub purpose: &'static str,
}

/// Canonical features an adapter may (or may not) translate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Instructions,
    McpStdio,
    McpRemote,
    Models,
    Permissions,
    Hooks,
    Skills,
}

impl Capability {
    fn label(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::McpStdio => "stdio MCP servers",
            Self::McpRemote => "remote MCP servers",
            Self::Models => "models",
            Self::Permissions => "permissions",
            Self::Hooks => "hooks",
            Self::Skills => "skills",
        }
    }
}

/// Desired canonical state handed to every adapter. Secret references in MCP
/// `env`/`headers` stay references in `mcp`; their values, already resolved by
/// the caller through the provider layer, come in `secrets` (render never
/// calls a provider).
#[derive(Debug, Clone, Copy)]
pub struct Desired<'a> {
    pub agents: &'a AgentsConfig,
    pub mcp: &'a BTreeMap<String, McpServer>,
    /// Contents of the canonical AGENTS.md, when declared and present.
    pub instructions: Option<&'a str>,
    /// Resolved secrets injected into MCP env and headers; render stays pure.
    pub secrets: &'a BTreeMap<String, SecretValue>,
    /// Agent Skills read from `[agents.skills]`: skill name -> file path
    /// relative to the skill directory (`SKILL.md`, `refs/x.md`) -> contents.
    pub skills: &'a Skills,
}

/// Skill name -> relative file path -> contents.
pub type Skills = BTreeMap<String, BTreeMap<String, String>>;

impl Desired<'_> {
    /// Features this configuration uses (drives "not supported" diagnostics).
    fn features(&self) -> Vec<Capability> {
        let mut used = Vec::new();
        if self.instructions.is_some() {
            used.push(Capability::Instructions);
        }
        let transports: Vec<_> = self
            .mcp
            .values()
            .filter_map(|s| s.transport().ok())
            .collect();
        if transports
            .iter()
            .any(|t| matches!(t, Transport::Stdio { .. }))
        {
            used.push(Capability::McpStdio);
        }
        if transports
            .iter()
            .any(|t| matches!(t, Transport::Remote { .. }))
        {
            used.push(Capability::McpRemote);
        }
        if !self.agents.models.is_empty() {
            used.push(Capability::Models);
        }
        if self.agents.permissions.is_some() {
            used.push(Capability::Permissions);
        }
        if !self.agents.hooks.is_empty() {
            used.push(Capability::Hooks);
        }
        if !self.skills.is_empty() {
            used.push(Capability::Skills);
        }
        used
    }
}

/// Current contents of an adapter's managed files, keyed by `ManagedFile::path`.
/// A missing key means the file does not exist.
pub type Current = BTreeMap<&'static str, String>;

/// New full contents for one managed file. Contents may hold resolved secrets,
/// so `Debug` shows only their length.
#[derive(Clone, PartialEq, Eq)]
pub struct RenderedFile {
    pub file: &'static ManagedFile,
    pub contents: String,
}

impl std::fmt::Debug for RenderedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderedFile")
            .field("file", &self.file.path)
            .field("contents", &format_args!("<{} bytes>", self.contents.len()))
            .finish()
    }
}

/// A whole file Muster owns at a path computed at render time (one file of
/// an installed skill), outside the adapter's static [`ManagedFile`] list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTreeFile {
    pub base: Base,
    /// Path relative to `base`.
    pub path: String,
    /// Path relative to a project root; `None` = user scope only.
    pub project: Option<String>,
    pub contents: String,
}

#[derive(Debug, Default)]
pub struct RenderPlan {
    pub files: Vec<RenderedFile>,
    /// Whole files at render-time paths (always [`FileFormat::Text`]).
    pub tree: Vec<RenderedTreeFile>,
    pub diagnostics: Vec<Diagnostic>,
}

type RenderFn = fn(&Desired, &Current, &mut RenderPlan) -> Result<(), MusterError>;

/// One agent adapter. `render: None` means listed but not implemented.
#[derive(Serialize)]
pub struct Adapter {
    pub id: &'static str,
    pub name: &'static str,
    /// Executable name on `PATH` (detection, doctor, installer default).
    pub binary: &'static str,
    pub experimental: bool,
    pub platforms: &'static [&'static str],
    pub min_version: &'static str,
    pub capabilities: &'static [Capability],
    pub files: &'static [ManagedFile],
    #[serde(skip)]
    render: Option<RenderFn>,
}

impl Adapter {
    pub fn available(&self) -> bool {
        self.render.is_some()
    }

    pub fn file(&self, path: &str) -> &'static ManagedFile {
        self.files
            .iter()
            .find(|f| f.path == path)
            .expect("adapter declares the files it renders")
    }

    /// Pure render: new contents for every managed file this config touches,
    /// plus warnings for canonical features the agent cannot express. An
    /// unparsable current file fails this adapter only (no file is clobbered).
    pub fn render(&self, desired: &Desired, current: &Current) -> Result<RenderPlan, MusterError> {
        let render = self.render.ok_or_else(|| {
            MusterError::Unavailable(format!("the {} adapter is not available yet", self.name))
        })?;
        let mut plan = RenderPlan::default();
        for feature in desired.features() {
            if !self.capabilities.contains(&feature) {
                plan.diagnostics.push(
                    Diagnostic::new(
                        Severity::Warning,
                        "unsupported_feature",
                        format!(
                            "{} does not support {}; skipped for this agent",
                            self.name,
                            feature.label()
                        ),
                    )
                    .with_path(self.id),
                );
            }
        }
        render(desired, current, &mut plan)?;
        Ok(plan)
    }
}

/// Every adapter Muster knows, stable ones first.
pub static ADAPTERS: &[&Adapter] = &[
    &claude::ADAPTER,
    &codex::ADAPTER,
    &opencode::ADAPTER,
    &antigravity::ADAPTER,
];

pub fn find(id: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().copied().find(|a| a.id == id)
}

/// Adapters applied when `[agents] enabled` is not set.
pub fn default_enabled() -> Vec<&'static str> {
    ADAPTERS
        .iter()
        .filter(|a| !a.experimental && a.available())
        .map(|a| a.id)
        .collect()
}

// ---------------------------------------------------------------------------
// Shared render helpers
// ---------------------------------------------------------------------------

/// Secret references are resolved before render (see [`Desired::secrets`]) and
/// never written as-is.
pub fn is_secret_reference(value: &str) -> bool {
    crate::secrets::is_reference(value)
}

/// `map` with secret references replaced by their resolved values; a reference
/// that could not be resolved drops its key with a warning.
fn resolve_mcp_map(
    map: &BTreeMap<String, String>,
    secrets: &BTreeMap<String, SecretValue>,
    adapter: &str,
    server: &str,
    plan: &mut RenderPlan,
) -> BTreeMap<String, String> {
    let mut kept = BTreeMap::new();
    for (key, value) in map {
        if !is_secret_reference(value) {
            kept.insert(key.clone(), value.clone());
        } else if let Some(secret) = secrets.get(value) {
            kept.insert(key.clone(), secret.expose().to_owned());
        } else {
            plan.diagnostics.push(
                Diagnostic::new(
                    Severity::Warning,
                    "secret_reference_unresolved",
                    format!(
                        "{adapter}: MCP server `{server}` key `{key}` references a secret \
                         that could not be resolved, so it was not written"
                    ),
                )
                .with_path(format!("mcp.{server}")),
            );
        }
    }
    kept
}

fn transport<'a>(name: &str, server: &'a McpServer) -> Result<Transport<'a>, MusterError> {
    server
        .transport()
        .map_err(|message| MusterError::InvalidConfig {
            message: format!("MCP server `{name}` {message}"),
            path: Some(format!("mcp.{name}")),
        })
}

/// Parse an existing JSON file that must hold an object; empty/missing = `{}`.
fn json_object(current: Option<&String>, path: &str) -> Result<Map<String, Value>, MusterError> {
    let Some(text) = current.filter(|t| !t.trim().is_empty()) else {
        return Ok(Map::new());
    };
    match serde_json::from_str(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(unparsable(path, "expected a JSON object")),
        Err(e) => Err(unparsable(path, &e.to_string())),
    }
}

fn unparsable(path: &str, why: &str) -> MusterError {
    MusterError::InvalidConfig {
        message: format!("cannot merge into existing {path}: {why}; file left untouched"),
        path: Some(path.to_string()),
    }
}

/// `object[key]` as an object, replacing a non-object value.
fn child_object<'m>(object: &'m mut Map<String, Value>, key: &str) -> &'m mut Map<String, Value> {
    let entry = object
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry.as_object_mut().expect("just ensured an object")
}

fn json_text(object: Map<String, Value>) -> String {
    let mut text = serde_json::to_string_pretty(&Value::Object(object)).expect("JSON serializes");
    text.push('\n');
    text
}

const BLOCK_BEGIN: &str = "<!-- BEGIN MUSTER MANAGED BLOCK: edits inside are overwritten -->";
const BLOCK_END: &str = "<!-- END MUSTER MANAGED BLOCK -->";

/// Replace (or append) Muster's managed block in a Markdown file, keeping the
/// rest of the file as the user wrote it.
fn markdown_block(current: Option<&String>, body: &str) -> String {
    let block = format!("{BLOCK_BEGIN}\n{}\n{BLOCK_END}\n", body.trim_end());
    let Some(text) = current.filter(|t| !t.trim().is_empty()) else {
        return block;
    };
    if let (Some(start), Some(end)) = (text.find(BLOCK_BEGIN), text.find(BLOCK_END)) {
        if start < end {
            let after = &text[end + BLOCK_END.len()..];
            let after = after.strip_prefix('\n').unwrap_or(after);
            return format!("{}{block}{after}", &text[..start]);
        }
    }
    format!("{}\n\n{block}", text.trim_end())
}

/// `text` without Muster's managed block (the inverse of [`markdown_block`]).
pub(crate) fn strip_markdown_block(text: &str) -> String {
    match (text.find(BLOCK_BEGIN), text.find(BLOCK_END)) {
        (Some(start), Some(end)) if start < end => {
            let after = text[end + BLOCK_END.len()..].trim_start_matches('\n');
            let before = text[..start].trim_end();
            match (before.is_empty(), after.is_empty()) {
                (true, _) => after.to_string(),
                (false, true) => format!("{before}\n"),
                (false, false) => format!("{before}\n\n{after}"),
            }
        }
        _ => text.to_string(),
    }
}

/// `pre_tool_use` -> `PreToolUse`.
fn pascal_case(snake: &str) -> String {
    snake
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_block_replaces_only_its_own_block() {
        let first = markdown_block(Some(&"# Mine\n".to_string()), "v1");
        assert!(first.starts_with("# Mine\n\n<!-- BEGIN"));
        let edited = format!("{first}\ntrailing notes\n");
        let second = markdown_block(Some(&edited), "v2");
        assert!(second.starts_with("# Mine\n"));
        assert!(second.contains("\nv2\n") && !second.contains("v1"));
        assert!(second.ends_with("trailing notes\n"));
        assert_eq!(markdown_block(Some(&second), "v2"), second, "idempotent");
        assert_eq!(strip_markdown_block(&second), "# Mine\n\ntrailing notes\n");
        assert_eq!(strip_markdown_block(&markdown_block(None, "v1")), "");
    }

    #[test]
    fn pascal_case_maps_hook_events() {
        assert_eq!(pascal_case("pre_tool_use"), "PreToolUse");
        assert_eq!(pascal_case("stop"), "Stop");
    }
}
