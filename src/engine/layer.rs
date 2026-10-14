//! Configuration layers and the single seam where they become desired state.
//!
//! Precedence (spec glossary): team -> group -> member -> machine -> project.
//! The team layer is the manifest's own `[agents]`/`[mcp]` (the approved team
//! profile); group, member and machine layers are `[profiles.<name>]` named by
//! `groups.<g>.profile` (groups the machine reads), `members.<m>.profile` (the
//! machine's owner) and `machines.<name>.profile`. The project layer lives in
//! `<project>/.muster/muster.toml` (spec story 29).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::adapters::config::{AgentsConfig, McpServer, DEFAULT_INSTRUCTIONS_SOURCE};
use crate::errors::MusterError;
use crate::manifest::{self, Manifest, Profile};
use crate::team;

/// Fixed layer precedence, lowest first.
pub const PRECEDENCE: [&str; 5] = ["team", "group", "member", "machine", "project"];

/// Directory, inside a project, that holds its Muster layer.
pub const PROJECT_DIR: &str = ".muster";

/// One configuration layer. `dir` is where its relative paths (instructions
/// source) resolve.
#[derive(Debug, Clone)]
pub struct Layer {
    /// One of [`PRECEDENCE`].
    pub kind: &'static str,
    /// Group, member, machine or project the layer belongs to.
    pub name: String,
    /// Profile providing the layer, if any.
    pub profile: Option<String>,
    pub dir: PathBuf,
    pub agents: AgentsConfig,
    pub mcp: BTreeMap<String, McpServer>,
}

impl Layer {
    pub fn from_manifest(
        kind: &'static str,
        name: impl Into<String>,
        dir: &Path,
        manifest: &Manifest,
    ) -> Self {
        Self {
            kind,
            name: name.into(),
            profile: None,
            dir: dir.to_path_buf(),
            agents: manifest.agents.clone().unwrap_or_default(),
            mcp: manifest.mcp.clone(),
        }
    }

    fn from_profile(
        kind: &'static str,
        name: &str,
        profile: &str,
        dir: &Path,
        p: &Profile,
    ) -> Self {
        Self {
            kind,
            name: name.to_string(),
            profile: Some(profile.to_string()),
            dir: dir.to_path_buf(),
            agents: p.agents.clone().unwrap_or_default(),
            mcp: p.mcp.clone(),
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

/// Layers rendered into `machine`'s user-scope files, lowest precedence
/// first: team, then the profiles of its groups, member and machine.
/// Profiles not declared under `[profiles]` are skipped (`status` reports them).
pub fn global_layers(config_dir: &Path, manifest: &Manifest, machine: Option<&str>) -> Vec<Layer> {
    let mut layers = vec![Layer::from_manifest("team", "team", config_dir, manifest)];
    let Some((machine, entry)) = machine.and_then(|m| manifest.machines.get_key_value(m)) else {
        return layers;
    };
    let mut push = |kind, name: &str, profile: &Option<String>| {
        if let Some((pname, p)) = profile
            .as_ref()
            .and_then(|p| manifest.profiles.get_key_value(p))
        {
            layers.push(Layer::from_profile(kind, name, pname, config_dir, p));
        }
    };
    for (name, group) in &manifest.groups {
        if team::group_recipients(manifest, name).contains(&entry.recipient) {
            push("group", name, &group.profile);
        }
    }
    if let Some(member) = team::owner_of(manifest, machine) {
        let profile = manifest
            .members
            .get(&member)
            .and_then(|m| m.profile.clone());
        push("member", &member, &profile);
    }
    push("machine", machine, &entry.profile);
    layers
}

/// Fingerprint of everything the team approved for `machine` (its merged
/// layers and the shared instructions). Stable across versions and platforms
/// (FNV-1a over canonical JSON), so machines compare digests through the repo.
pub fn digest(config_dir: &Path, manifest: &Manifest, machine: &str) -> String {
    let merged = merge(&global_layers(config_dir, manifest, Some(machine)));
    let instructions = merged
        .instructions_path()
        .ok()
        .and_then(|(path, _)| std::fs::read_to_string(path).ok());
    let canonical = serde_json::json!({
        "agents": merged.agents,
        "mcp": merged.mcp,
        "instructions": instructions,
    })
    .to_string();
    let hash = canonical.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// `plan --explain`: the fixed precedence and the layers that apply.
pub fn explain(layers: &[Layer]) -> serde_json::Value {
    let layers: Vec<_> = layers
        .iter()
        .map(|l| serde_json::json!({ "layer": l.kind, "name": l.name, "profile": l.profile }))
        .collect();
    serde_json::json!({ "precedence": PRECEDENCE, "layers": layers })
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
        "project",
        project.display().to_string(),
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
        Layer::from_manifest("team", name, Path::new(name), &m)
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
