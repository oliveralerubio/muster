//! Configuration layers and the single seam where they become desired state.
//!
//! Precedence (spec glossary): team -> group -> member -> machine -> project.
//! Ticket #9 implements the global layer (the config repo manifest) and the
//! project layer (`<project>/.muster/muster.toml`, spec story 29). #10 inserts
//! team/group/member/machine layers in [`global_layers`]; nothing else changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::adapters::config::{AgentsConfig, McpServer, DEFAULT_INSTRUCTIONS_SOURCE};
use crate::errors::MusterError;
use crate::manifest::{self, Manifest};

/// Directory, inside a project, that holds its Muster layer.
pub const PROJECT_DIR: &str = ".muster";

/// One configuration layer. `dir` is where its relative paths (instructions
/// source) resolve.
#[derive(Debug, Clone)]
pub struct Layer {
    pub name: String,
    pub dir: PathBuf,
    pub agents: AgentsConfig,
    pub mcp: BTreeMap<String, McpServer>,
}

impl Layer {
    pub fn from_manifest(name: impl Into<String>, dir: &Path, manifest: &Manifest) -> Self {
        Self {
            name: name.into(),
            dir: dir.to_path_buf(),
            agents: manifest.agents.clone().unwrap_or_default(),
            mcp: manifest.mcp.clone(),
        }
    }
}

/// Merged layers: what the adapters render.
#[derive(Debug, Clone)]
pub struct Merged {
    pub agents: AgentsConfig,
    pub mcp: BTreeMap<String, McpServer>,
    /// Directory the instructions source resolves against (the layer that
    /// declared `[agents.instructions]`, else the first layer).
    pub instructions_dir: PathBuf,
}

/// Layers rendered into user-scope files, lowest precedence first.
pub fn global_layers(config_dir: &Path, manifest: &Manifest) -> Vec<Layer> {
    // #10: append team, group, member and machine profile layers here.
    vec![Layer::from_manifest("global", config_dir, manifest)]
}

/// The project layer. Agents read user-scope and project-scope files
/// themselves, so project files hold only this layer.
pub fn project_layers(project: &Path) -> Result<Vec<Layer>, MusterError> {
    let dir = project.join(PROJECT_DIR);
    let path = dir.join(manifest::FILE_NAME);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            MusterError::Operational(format!(
                "no project configuration: {} does not exist",
                path.display()
            ))
        } else {
            MusterError::Operational(format!("{}: {e}", path.display()))
        }
    })?;
    let manifest = Manifest::parse(&text, &path.display().to_string())?;
    Ok(vec![Layer::from_manifest(
        format!("project:{}", project.display()),
        &dir,
        &manifest,
    )])
}

/// Merge layers, lowest precedence first: later scalars and lists replace
/// earlier ones; maps (models, hooks, MCP servers) merge by key.
pub fn merge(layers: &[Layer]) -> Merged {
    let mut agents = AgentsConfig::default();
    let mut mcp = BTreeMap::new();
    let mut instructions_dir = layers.first().map(|l| l.dir.clone()).unwrap_or_default();
    for layer in layers {
        let over = &layer.agents;
        if over.enabled.is_some() {
            agents.enabled = over.enabled.clone();
        }
        if over.instructions.is_some() {
            agents.instructions = over.instructions.clone();
            instructions_dir = layer.dir.clone();
        }
        if over.permissions.is_some() {
            agents.permissions = over.permissions.clone();
        }
        agents.models.extend(over.models.clone());
        agents.hooks.extend(over.hooks.clone());
        mcp.extend(layer.mcp.clone());
    }
    Merged {
        agents,
        mcp,
        instructions_dir,
    }
}

impl Merged {
    /// Path of the canonical instructions and whether it was declared.
    pub fn instructions_path(&self) -> Result<(PathBuf, bool), MusterError> {
        let declared = self.agents.instructions.as_ref();
        let source = declared.map_or(DEFAULT_INSTRUCTIONS_SOURCE, |i| i.source.as_str());
        let relative = Path::new(source);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(MusterError::InvalidConfig {
                message: format!(
                    "instructions source `{source}` must be a path inside the config repository"
                ),
                path: Some(format!(
                    "{}: agents.instructions.source",
                    manifest::FILE_NAME
                )),
            });
        }
        Ok((self.instructions_dir.join(relative), declared.is_some()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::Hook;

    fn layer(name: &str, toml: &str) -> Layer {
        let m = Manifest::parse(toml, "muster.toml").unwrap();
        Layer::from_manifest(name, Path::new(name), &m)
    }

    #[test]
    fn later_layers_win_and_maps_merge_by_key() {
        let base = layer(
            "base",
            "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.models]\nclaude = \"a\"\ncodex = \"c\"\n[mcp.one]\ncommand = \"x\"\n",
        );
        let top = layer(
            "top",
            "version = 1\n[agents.models]\nclaude = \"b\"\n[agents.hooks.stop]\ncommand = \"s\"\n[agents.instructions]\nsource = \"I.md\"\n[mcp.two]\nurl = \"https://e\"\n",
        );
        let merged = merge(&[base, top]);
        assert_eq!(merged.agents.enabled, Some(vec!["claude".to_string()]));
        assert_eq!(merged.agents.models["claude"], "b");
        assert_eq!(merged.agents.models["codex"], "c");
        assert_eq!(
            merged.agents.hooks["stop"],
            Hook {
                command: "s".into(),
                matcher: None
            }
        );
        assert_eq!(merged.mcp.keys().collect::<Vec<_>>(), vec!["one", "two"]);
        assert_eq!(merged.instructions_dir, PathBuf::from("top"));
    }
}
