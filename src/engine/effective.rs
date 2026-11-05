//! The effective configuration: the one deep module that computes it.
//!
//! The glossary defines the effective configuration as "the configuration
//! selected for a machine or project after layers are merged, together with
//! its origins and per-adapter state on disk". [`compute`] is the single
//! seam where that happens: it loads the layers ([`crate::engine::layer`]
//! owns the precedence rules), renders each enabled adapter purely against
//! the real files on disk, and returns typed items — instructions, skills,
//! MCP servers, subagents, AI providers — each carrying its origin layer
//! and per-adapter on-disk state, plus a scrubbed JSON view
//! ([`EffectiveConfig::to_json`]) for the read-only surfaces.
//!
//! Every rule about precedence lives here or in `layer.rs`, never in a
//! command: which layer supplies an item, how a project layer's MCP servers
//! replace lower layers' entries, which toggles disable an item for one
//! adapter, and which fields of a native config file may be compared
//! without ever reading a secret value. `muster effective` (and, through
//! the registry, the MCP tool and the web panel) only parses parameters,
//! calls [`compute`] and serializes the result.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::adapters::config::{AgentsConfig, AiProvider, AiProviderOverride, McpServer};
use crate::adapters::{self, Adapter, Current, Desired};
use crate::errors::MusterError;
use crate::manifest::Manifest;
use crate::secrets::{SecretAccess, SecretValue};

use super::{enabled_adapters, layer, Roots};

/// What [`compute`] needs: where the configuration lives and which slice of
/// the effective configuration the caller wants.
pub struct Inputs<'a> {
    /// Config repository root (the team layer's directory).
    pub config_dir: &'a Path,
    /// Local per-machine state; the machine's identity selects its layers.
    pub state_dir: &'a Path,
    /// Restrict every per-adapter view to one adapter id.
    pub agent: Option<&'a str>,
    /// Project root whose `.muster` layer joins the view with the highest
    /// precedence.
    pub project: Option<&'a Path>,
    /// Skill views of the global scope, as reported by the `skill list`
    /// command (the skills domain owns them; this module adds the effective
    /// state on top). Ignored when `project` is set: the project scope
    /// lists its own skill directories.
    pub skill_listing: &'a Value,
}

/// On-disk state of an item for one adapter, as the read-only view reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No managed file exists yet.
    Missing,
    /// The managed files on disk match what apply would write.
    InSync,
    /// The files exist but differ from what apply would write.
    Drifted,
    /// A toggle removes the item for this adapter.
    Disabled,
    /// The adapter is skipped for this item by configuration.
    Skipped,
    /// The item's files are on disk exactly as managed (skills).
    Installed,
}

/// Verification of a skill against `skills.lock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// Files hash to the pinned content hash.
    Verified,
    /// Files hash to something else than the pin.
    HashMismatch,
    /// On disk but not pinned in `skills.lock`.
    Unreviewed,
    /// Frontmatter invalid (name/description missing or mismatched).
    Invalid,
    /// The skill directory cannot be read.
    Unreadable,
}

/// One layer that contributed to the effective configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerOrigin {
    /// One of [`layer::PRECEDENCE`].
    pub kind: &'static str,
    /// Group, member, machine or project the layer belongs to.
    pub name: String,
}

impl LayerOrigin {
    fn to_json(&self) -> Value {
        json!({ "kind": self.kind, "name": &self.name })
    }
}

/// The canonical instructions and how each adapter's copy fares on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionsItem {
    /// Path of the canonical instructions file.
    pub source: PathBuf,
    /// Layer whose `[agents.instructions]` supplies the file.
    pub layer: Option<&'static str>,
    /// `len() / 4` estimate of the token count.
    pub approx_tokens: usize,
    pub adapters: BTreeMap<String, InstructionsView>,
}

/// Per-adapter view of the instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionsView {
    pub state: State,
    pub files: Vec<InstructionFile>,
}

impl InstructionsView {
    fn skipped() -> Self {
        Self {
            state: State::Skipped,
            files: Vec::new(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "state": state_json(self.state),
            "files": self.files.iter().map(|f| json!({ "path": &f.path, "state": state_json(f.state) })).collect::<Vec<_>>()
        })
    }
}

/// One managed instructions file of an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub state: State,
}

/// One MCP server of the effective configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpItem {
    pub name: String,
    /// Highest-precedence layer declaring the server.
    pub layer: Option<&'static str>,
    /// Lower layers the winning entry replaced, lowest first.
    pub overridden_layers: Vec<&'static str>,
    /// `stdio` or `remote`.
    pub transport: &'static str,
    /// Adapter ids the server is delivered to (sorted).
    pub agents: Vec<String>,
    pub adapters: BTreeMap<String, State>,
    /// Secret references used in `env`/`headers` (references, never values).
    pub secret_references: Vec<String>,
}

impl McpItem {
    fn to_json(&self) -> Value {
        json!({
            "name": &self.name,
            "layer": self.layer,
            "overridden_layers": self.overridden_layers,
            "transport": self.transport,
            "agents": &self.agents,
            "adapters": self.adapters.iter().map(|(id, state)| (id.as_str(), json!({ "state": state_json(*state) }))).collect::<BTreeMap<_, _>>(),
            "secret_references": &self.secret_references,
        })
    }
}

/// One subagent of the effective configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentItem {
    pub name: String,
    /// Layer whose `[agents.subagents]` supplies the directory.
    pub layer: Option<&'static str>,
    pub adapters: BTreeMap<String, State>,
}

impl SubagentItem {
    fn to_json(&self) -> Value {
        json!({
            "name": &self.name,
            "layer": self.layer,
            "adapters": self.adapters.iter().map(|(id, state)| (id.as_str(), json!({ "state": state_json(*state) }))).collect::<BTreeMap<_, _>>(),
        })
    }
}

/// Unmanaged skill folders found beside the managed ones, per adapter, in
/// report order (the serialized view preserves it).
pub type ExtraOnDisk = Vec<(String, Vec<String>)>;

/// Skills of one scope plus the unmanaged extras found next to them.
struct SkillsSection {
    items: Vec<SkillItem>,
    extra_on_disk: BTreeMap<String, Vec<String>>,
}

/// One skill of the effective configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillItem {
    pub name: String,
    /// The skill's views as the `skill list` command reports them (origin,
    /// status, version, ...), serialized verbatim alongside the fields below.
    pub base: Value,
    /// Whether a file overlay or `[agents.skills.customize]` touches it.
    pub customized: bool,
    pub verification: Verification,
    /// `len() / 4` estimate of the description token count.
    pub description_tokens: usize,
    /// `len() / 4` estimate of the body token count, after overlays.
    pub body_tokens: usize,
    /// Layer whose `[agents.skills]` supplies the directory.
    pub layer: Option<&'static str>,
    pub adapters: BTreeMap<String, SkillAdapterView>,
}

impl SkillItem {
    fn to_json(&self) -> Value {
        let mut view = self.base.as_object().cloned().unwrap_or_default();
        view.insert("customized".into(), json!(self.customized));
        view.insert("verification".into(), state_json(self.verification));
        view.insert("description_tokens".into(), json!(self.description_tokens));
        view.insert("body_tokens".into(), json!(self.body_tokens));
        view.insert("layer".into(), json!(self.layer));
        view.insert(
            "adapters".into(),
            json!(self
                .adapters
                .iter()
                .map(|(id, view)| (id.as_str(), view.to_json()))
                .collect::<BTreeMap<_, _>>()),
        );
        Value::Object(view)
    }
}

/// Per-adapter view of a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillAdapterView {
    /// Installation directory (skill subdirectory's parent).
    pub path: Option<PathBuf>,
    pub state: State,
}

impl SkillAdapterView {
    fn to_json(&self) -> Value {
        json!({ "path": &self.path, "state": state_json(self.state) })
    }
}

/// One AI provider of the effective configuration (see the glossary: a
/// named model endpoint, distinct from secret providers). Not part of the
/// serialized view; carried typed for callers that need it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderItem {
    pub id: String,
    /// Layer that last declared or overrode the provider.
    pub layer: Option<&'static str>,
    pub provider: AiProvider,
}

/// The effective configuration of a machine (and, optionally, a project).
///
/// `totals` and `extra_on_disk` keep report order (the global scope's
/// adapters first, project-scope additions appended), which the serialized
/// view preserves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectiveConfig {
    /// Layers that contributed, lowest precedence first.
    pub layers: Vec<LayerOrigin>,
    pub instructions: Vec<InstructionsItem>,
    pub mcp: Vec<McpItem>,
    pub subagents: Vec<SubagentItem>,
    pub skills: Vec<SkillItem>,
    pub providers: Vec<ProviderItem>,
    /// Adapter id -> unmanaged skill folders found beside the managed ones.
    pub extra_on_disk: ExtraOnDisk,
    /// Adapter id -> instruction and skill-description token estimates.
    pub totals: Vec<(String, u64)>,
}

impl EffectiveConfig {
    /// The scrubbed JSON view of the effective configuration: exactly what
    /// `muster effective` reports, with no secret value ever included.
    pub fn to_json(&self) -> Value {
        json!({
            "totals": object_of(&self.totals),
            "subagents": self.subagents.iter().map(SubagentItem::to_json).collect::<Vec<_>>(),
            "mcp": self.mcp.iter().map(McpItem::to_json).collect::<Vec<_>>(),
            "extra_on_disk": self.extra_on_disk.iter().map(|(id, names)| (id.as_str(), names)).collect::<BTreeMap<_, _>>(),
            "skills": self.skills.iter().map(SkillItem::to_json).collect::<Vec<_>>(),
            "layers": self.layers.iter().map(LayerOrigin::to_json).collect::<Vec<_>>(),
            "instructions": self.instructions.iter().map(InstructionsItem::to_json).collect::<Vec<_>>(),
        })
    }

    /// Fold the project scope in: the project layer has the highest
    /// precedence, so its same-named MCP servers replace the lower layers'
    /// entries (reported as overridden instead of duplicated) and its items
    /// extend every other list.
    fn absorb_project(&mut self, project: EffectiveConfig) {
        let replaced: BTreeSet<&str> = project.mcp.iter().map(|item| item.name.as_str()).collect();
        let mut overridden: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
        self.mcp.retain(|item| {
            if !replaced.contains(item.name.as_str()) {
                return true;
            }
            let mut layers = item.overridden_layers.clone();
            if let Some(kind) = item.layer {
                layers.push(kind);
            }
            overridden.insert(item.name.clone(), layers);
            false
        });
        for mut item in project.mcp {
            if let Some(lower) = overridden.get(&item.name) {
                let mut kinds = lower.clone();
                kinds.extend(item.overridden_layers.iter().copied());
                item.overridden_layers = ordered_layers(kinds);
            }
            self.mcp.push(item);
        }
        self.layers.extend(project.layers);
        self.instructions.extend(project.instructions);
        self.skills.extend(project.skills);
        self.subagents.extend(project.subagents);
        // Unmanaged extras from both scopes, merged and deduplicated per adapter.
        for (id, names) in project.extra_on_disk {
            match self.extra_on_disk.iter_mut().find(|(key, _)| *key == id) {
                Some((_, merged)) => {
                    for name in names {
                        if !merged.contains(&name) {
                            merged.push(name);
                        }
                    }
                    merged.sort();
                }
                None => self.extra_on_disk.push((id, names)),
            }
        }
        for (id, value) in project.totals {
            match self.totals.iter_mut().find(|(key, _)| *key == id) {
                Some((_, sum)) => *sum += value,
                None => self.totals.push((id, value)),
            }
        }
    }
}

impl InstructionsItem {
    fn to_json(&self) -> Value {
        json!({
            "source": &self.source,
            "layer": self.layer,
            "approx_tokens": self.approx_tokens,
            "adapters": self.adapters.iter().map(|(id, view)| (id.as_str(), view.to_json())).collect::<BTreeMap<_, _>>(),
        })
    }
}

/// Compute the effective configuration: global layers of this machine, plus
/// the project layer on top when `inputs.project` is set.
pub fn compute(inputs: &Inputs<'_>) -> Result<EffectiveConfig, MusterError> {
    let manifest = Manifest::load(inputs.config_dir)?;
    let machine = crate::machine::load_name(inputs.state_dir);
    let mut layers = layer::global_layers(inputs.config_dir, &manifest, machine.as_deref());
    let mut config = scope(inputs, &layers, None, None)?;
    if let Some(project) = inputs.project {
        let project_layers = layer::project_layers(project)?;
        let global_enabled = layer::merge(&layers).agents.enabled;
        let project_view = scope(inputs, &project_layers, Some(project), global_enabled)?;
        config.absorb_project(project_view);
        layers.extend(project_layers);
    }
    config.providers = providers_of(&layers);
    Ok(config)
}

/// The effective configuration of one set of layers (one scope).
fn scope(
    inputs: &Inputs<'_>,
    layers: &[layer::Layer],
    project: Option<&Path>,
    fallback_enabled: Option<Vec<String>>,
) -> Result<EffectiveConfig, MusterError> {
    let empty = Empty::new();
    let mut merged = layer::merge(layers);
    if merged.agents.enabled.is_none() {
        merged.agents.enabled = fallback_enabled;
    }
    let selected = select_adapters(inputs.agent, &merged)?;
    let instructions = instructions(inputs, &merged, layers, &selected, project, &empty)?;
    let roots = Roots::from_env()?;
    let mcp = mcp(inputs, &merged, layers, &selected, project, &roots, &empty)?;
    let subagents = subagents(inputs, &merged, layers, &selected, project, &empty)?;
    let skills = skills(inputs, &merged, layers, &selected, project, &roots, &empty)?;
    let totals: BTreeMap<String, u64> = selected
        .iter()
        .map(|adapter| {
            let instruction_tokens: u64 = instructions
                .iter()
                .filter(|item| {
                    item.adapters
                        .get(adapter.id)
                        .is_none_or(|view| view.state != State::Skipped)
                })
                .map(|item| item.approx_tokens as u64)
                .sum();
            let skill_tokens: u64 = skills
                .items
                .iter()
                .filter(|item| {
                    item.adapters
                        .get(adapter.id)
                        .is_some_and(|view| view.state != State::Disabled)
                })
                .map(|item| item.description_tokens as u64)
                .sum();
            (adapter.id.to_owned(), instruction_tokens + skill_tokens)
        })
        .collect();
    Ok(EffectiveConfig {
        layers: layers
            .iter()
            .map(|l| LayerOrigin {
                kind: l.kind,
                name: l.name.clone(),
            })
            .collect(),
        instructions,
        mcp,
        subagents,
        skills: skills.items,
        providers: Vec::new(),
        extra_on_disk: skills.extra_on_disk.into_iter().collect(),
        totals: totals.into_iter().collect(),
    })
}

/// Adapters of this scope: the requested one, else those enabled.
fn select_adapters(
    agent: Option<&str>,
    merged: &layer::Merged,
) -> Result<Vec<&'static Adapter>, MusterError> {
    if let Some(id) = agent {
        let adapter = adapters::find(id)
            .ok_or_else(|| MusterError::Usage(format!("unknown agent `{id}`")))?;
        if !adapter.available() {
            return Err(MusterError::Unavailable(format!(
                "agent `{id}` is unavailable"
            )));
        }
        return Ok(vec![adapter]);
    }
    enabled_adapters(Some(&merged.agents))
}

/// The canonical instructions: read from the layer that declares them and
/// compared, per adapter, against the native files on disk.
fn instructions(
    inputs: &Inputs<'_>,
    merged: &layer::Merged,
    layers: &[layer::Layer],
    selected: &[&'static Adapter],
    project: Option<&Path>,
    empty: &Empty,
) -> Result<Vec<InstructionsItem>, MusterError> {
    let (path, _) = merged.instructions_path()?;
    let source = layers
        .iter()
        .rev()
        .find(|l| l.agents.instructions.is_some())
        .or_else(|| layers.first());
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(Vec::new()),
    };
    let text = String::from_utf8(bytes).map_err(|e| MusterError::Operational(e.to_string()))?;
    let desired = empty.desired(inputs.config_dir, Some(&text));
    let roots = Roots::from_env()?;
    let mut views: BTreeMap<String, InstructionsView> = BTreeMap::new();
    for &adapter in selected {
        if merged
            .agents
            .skip_instructions
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id == adapter.id))
        {
            views.insert(adapter.id.to_owned(), InstructionsView::skipped());
            continue;
        }
        let mut current = Current::new();
        for file in adapter.files {
            if let Ok(content) = std::fs::read_to_string(locate_file(&roots, file, project)) {
                current.insert(file.path, content);
            }
        }
        let rendered = adapter.render(&desired, &current)?;
        let files: Vec<_> = rendered
            .files
            .iter()
            .filter(|f| f.file.format == adapters::FileFormat::Markdown)
            .map(|f| {
                let path = locate_file(&roots, f.file, project);
                let state = match current.get(f.file.path) {
                    None => State::Missing,
                    Some(old) if old == &f.contents => State::InSync,
                    Some(_) => State::Drifted,
                };
                InstructionFile { path, state }
            })
            .collect();
        let state = if files.iter().any(|f| f.state == State::Missing) {
            State::Missing
        } else if files.iter().any(|f| f.state == State::Drifted) {
            State::Drifted
        } else {
            State::InSync
        };
        views.insert(adapter.id.to_owned(), InstructionsView { state, files });
    }
    Ok(vec![InstructionsItem {
        source: path,
        layer: source.map(|l| l.kind),
        approx_tokens: text.len() / 4,
        adapters: views,
    }])
}

/// MCP servers of the merged layers, each with the layers it came from and
/// its state per adapter. On-disk configs are read through
/// [`scrub_mcp_secrets`] so a resolved secret this view cannot reproduce
/// never surfaces and never reads as drift.
fn mcp(
    inputs: &Inputs<'_>,
    merged: &layer::Merged,
    layers: &[layer::Layer],
    selected: &[&'static Adapter],
    project: Option<&Path>,
    roots: &Roots,
    empty: &Empty,
) -> Result<Vec<McpItem>, MusterError> {
    let mut items = Vec::new();
    for (name, server) in &merged.mcp {
        let transport = match server.transport().map_err(MusterError::Operational)? {
            crate::adapters::config::Transport::Stdio { .. } => "stdio",
            crate::adapters::config::Transport::Remote { .. } => "remote",
        };
        let sources: Vec<_> = layers.iter().filter(|l| l.mcp.contains_key(name)).collect();
        let env_secrets = secret_keys(&server.env);
        let header_secrets = secret_keys(&server.headers);
        let mcp = BTreeMap::from([(name.clone(), server.clone())]);
        let desired = empty.desired_with_mcp(inputs.config_dir, &mcp);
        let baseline_desired = empty.desired(inputs.config_dir, None);
        let mut views: BTreeMap<String, State> = BTreeMap::new();
        for &adapter in selected {
            if merged.disabled("mcp", name, adapter.id).is_some() {
                views.insert(adapter.id.to_owned(), State::Disabled);
                continue;
            }
            let mut current = Current::new();
            for file in adapter.files {
                if let Ok(content) = std::fs::read_to_string(locate_file(roots, file, project)) {
                    current.insert(
                        file.path,
                        scrub_mcp_secrets(&content, name, &env_secrets, &header_secrets),
                    );
                }
            }
            let rendered = adapter.render(&desired, &current)?;
            let baseline = adapter.render(&baseline_desired, &current)?;
            let changed: Vec<_> = rendered
                .files
                .iter()
                .filter(|file| {
                    baseline
                        .files
                        .iter()
                        .find(|base| base.file.path == file.file.path)
                        .is_none_or(|base| {
                            !same_config(
                                &base.contents,
                                &file.contents,
                                name,
                                &env_secrets,
                                &header_secrets,
                            )
                        })
                })
                .collect();
            if let Some(file) = changed.first() {
                let state = match current.get(file.file.path) {
                    None => State::Missing,
                    Some(_) => State::Drifted,
                };
                views.insert(adapter.id.to_owned(), state);
            } else {
                // Render against an empty current file to identify the MCP output.
                let rendered = adapter.render(&desired, &Current::new())?;
                let baseline = adapter.render(&baseline_desired, &Current::new())?;
                if rendered.files.iter().any(|file| {
                    baseline
                        .files
                        .iter()
                        .find(|base| base.file.path == file.file.path)
                        .is_none_or(|base| base.contents != file.contents)
                }) {
                    views.insert(adapter.id.to_owned(), State::InSync);
                }
            }
        }
        let agents: Vec<_> = views.keys().cloned().collect();
        let references: Vec<_> = server
            .env
            .values()
            .chain(server.headers.values())
            .filter(|v| adapters::is_secret_reference(v))
            .cloned()
            .collect();
        items.push(McpItem {
            name: name.clone(),
            layer: sources.last().map(|l| l.kind),
            overridden_layers: sources
                .iter()
                .take(sources.len().saturating_sub(1))
                .map(|l| l.kind)
                .collect(),
            transport,
            agents,
            adapters: views,
            secret_references: references,
        });
    }
    Ok(items)
}

/// Subagents of the layer that declares `[agents.subagents]`: parsed from
/// their canonical Markdown files, rendered per adapter and compared with
/// the native files on disk.
fn subagents(
    inputs: &Inputs<'_>,
    merged: &layer::Merged,
    layers: &[layer::Layer],
    selected: &[&'static Adapter],
    project: Option<&Path>,
    empty: &Empty,
) -> Result<Vec<SubagentItem>, MusterError> {
    let mut items = Vec::new();
    let Some(dir) = merged.subagents_path()? else {
        return Ok(items);
    };
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(rest) = source.strip_prefix("---\n") else {
            continue;
        };
        let Some((front, body)) = rest.split_once("\n---\n") else {
            continue;
        };
        let Ok(docs) = yaml_rust2::YamlLoader::load_from_str(front) else {
            continue;
        };
        let Some(yaml) = docs.first() else {
            continue;
        };
        let Some(description) = yaml["description"].as_str() else {
            continue;
        };
        if yaml["name"].as_str() != Some(name) {
            continue;
        }
        let tools = match &yaml["tools"] {
            yaml_rust2::Yaml::Array(items) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            yaml_rust2::Yaml::String(s) => s.split(',').map(|s| s.trim().to_owned()).collect(),
            _ => Vec::new(),
        };
        let subagent = adapters::Subagent {
            name: name.to_owned(),
            description: description.to_owned(),
            tools,
            model: yaml["model"].as_str().map(str::to_owned),
            extra_fields: Vec::new(),
            body: body.to_owned(),
            source,
        };
        let subagents = BTreeMap::from([(name.to_owned(), subagent)]);
        let desired = empty.desired_with_subagents(inputs.config_dir, &subagents);
        let roots = Roots::from_env()?;
        let mut views: BTreeMap<String, State> = BTreeMap::new();
        for &adapter in selected {
            if merged.disabled("subagent", name, adapter.id).is_some() {
                views.insert(adapter.id.to_owned(), State::Disabled);
                continue;
            }
            let rendered = adapter.render(&desired, &Current::new())?;
            if let Some(file) = rendered.tree.first() {
                let path = locate_tree(&roots, file, project);
                let state = match std::fs::read_to_string(path) {
                    Err(_) => State::Missing,
                    Ok(current) if current == file.contents => State::InSync,
                    Ok(_) => State::Drifted,
                };
                views.insert(adapter.id.to_owned(), state);
            }
        }
        let layer = layers
            .iter()
            .rev()
            .find(|l| l.agents.subagents.is_some())
            .map(|l| l.kind);
        items.push(SubagentItem {
            name: name.to_owned(),
            layer,
            adapters: views,
        });
    }
    Ok(items)
}

/// Skills of the layer that declares `[agents.skills]`: verified against
/// `skills.lock`, overlaid as apply would write them, rendered per adapter
/// and compared with the native files on disk. Also reports the unmanaged
/// skill folders found beside them, per adapter.
fn skills(
    inputs: &Inputs<'_>,
    merged: &layer::Merged,
    layers: &[layer::Layer],
    selected: &[&'static Adapter],
    project: Option<&Path>,
    roots: &Roots,
    empty: &Empty,
) -> Result<SkillsSection, MusterError> {
    let mut items = Vec::new();
    let mut skill_dirs: BTreeMap<String, PathBuf> =
        skill_roots(inputs.config_dir, selected, roots, project)?
            .into_iter()
            .map(|(id, dir)| (id.to_owned(), dir))
            .collect();
    let skill_layer = layers
        .iter()
        .rev()
        .find(|l| l.agents.skills.is_some())
        .map(|l| l.kind);
    let listed = if project.is_some() {
        // The project scope lists its own skill directories; the global
        // listing (origin, status, ...) describes the user scope only.
        let mut names: Vec<_> = merged
            .skills_path()?
            .into_iter()
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten())
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        json!({"skills": names.into_iter().map(|name| json!({"name": name})).collect::<Vec<_>>()})
    } else {
        inputs.skill_listing.clone()
    };
    let lock = crate::skills::SkillsLock::load(inputs.config_dir)?;
    if let Some(dir) = merged.skills_path()? {
        if let Some(listed) = listed["skills"].as_array() {
            for base in listed {
                let name = base["name"].as_str().unwrap_or_default();
                let files = crate::skills::read_skill_files(&dir.join(name));
                let customized =
                    crate::skills::is_customized(name, &merged.overlay_roots, &merged.customize);
                let (verification, description_tokens, body_tokens, data) = match files {
                    Err(_) => (Verification::Unreadable, 0, 0, BTreeMap::new()),
                    Ok(files) => {
                        let md = files.get("SKILL.md");
                        let meta = md.and_then(|s| crate::skills::parse_frontmatter(s));
                        let valid = meta.as_ref().is_some_and(|m| {
                            m.name.as_deref() == Some(name) && m.description.is_some()
                        });
                        let verification = if !valid {
                            Verification::Invalid
                        } else if let Some(pin) = lock.get(name) {
                            if pin.content_hash == crate::skills::content_hash(&files) {
                                Verification::Verified
                            } else {
                                Verification::HashMismatch
                            }
                        } else {
                            Verification::Unreviewed
                        };
                        let description_tokens =
                            meta.and_then(|m| m.description).map_or(0, |s| s.len() / 4);
                        // Render what apply writes: overlays go on after the
                        // pin check, so drift compares against the stitched files.
                        let mut files = files;
                        crate::skills::apply_overlays(
                            name,
                            &mut files,
                            &merged.overlay_roots,
                            &merged.customize,
                        )?;
                        let body_tokens = files.get("SKILL.md").map_or(0, |s| s.len() / 4);
                        (verification, description_tokens, body_tokens, files)
                    }
                };
                let mut adapter_views: BTreeMap<String, SkillAdapterView> = BTreeMap::new();
                for &adapter in selected {
                    let skill_map = BTreeMap::from([(name.to_owned(), data.clone())]);
                    let desired = empty.desired_with_skills(inputs.config_dir, &skill_map);
                    let rendered = adapter.render(&desired, &Current::new())?;
                    let paths: Vec<_> = rendered
                        .tree
                        .iter()
                        .map(|f| (locate_tree(roots, f, project), &f.contents))
                        .collect();
                    let state = if merged.disabled("skill", name, adapter.id).is_some() {
                        // Toggles remove the skill from this adapter, so its
                        // absence on disk is the desired state, not drift.
                        State::Disabled
                    } else if paths.is_empty() || paths.iter().any(|(p, _)| !p.exists()) {
                        State::Missing
                    } else if paths
                        .iter()
                        .any(|(p, c)| std::fs::read_to_string(p).ok().as_ref() != Some(*c))
                    {
                        State::Drifted
                    } else {
                        State::Installed
                    };
                    let path = paths
                        .first()
                        .and_then(|(p, _)| p.parent())
                        .map(|p| p.to_path_buf());
                    if let Some(dir) = path.as_ref().and_then(|p| p.parent()) {
                        skill_dirs.insert(adapter.id.to_owned(), dir.to_path_buf());
                    }
                    adapter_views.insert(adapter.id.to_owned(), SkillAdapterView { path, state });
                }
                items.push(SkillItem {
                    name: name.to_owned(),
                    base: base.clone(),
                    customized,
                    verification,
                    description_tokens,
                    body_tokens,
                    layer: skill_layer,
                    adapters: adapter_views,
                });
            }
        }
    }
    let names: BTreeSet<_> = items.iter().map(|item| item.name.as_str()).collect();
    let extra_on_disk: BTreeMap<_, Vec<_>> = skill_dirs
        .into_iter()
        .map(|(id, dir)| {
            let mut extra: Vec<_> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| !names.contains(name.as_str()))
                // Built-in skills are installed in user scope and are managed.
                .filter(|name| project.is_some() || !crate::skills::builtin::is_builtin(name))
                .collect();
            extra.sort();
            (id, extra)
        })
        .collect();
    Ok(SkillsSection {
        items,
        extra_on_disk,
    })
}

/// AI providers of the effective configuration: merged field-wise across
/// layers, each tagged with the layer that last declared or overrode it.
fn providers_of(layers: &[layer::Layer]) -> Vec<ProviderItem> {
    let mut merged: BTreeMap<String, (Option<&'static str>, AiProvider)> = BTreeMap::new();
    for layer in layers {
        for (id, over) in &layer.ai_providers {
            if let Some(current) = merged.get_mut(id) {
                if overrides_any_field(over) {
                    current.0 = Some(layer.kind);
                }
                over.merge_into(&mut current.1);
            } else if let Some(declared) = over.declared() {
                merged.insert(id.clone(), (Some(layer.kind), declared));
            }
        }
    }
    merged
        .into_iter()
        .map(|(id, (layer, provider))| ProviderItem {
            id,
            layer,
            provider,
        })
        .collect()
}

/// Whether an override sets any field (a no-field override merges nothing).
fn overrides_any_field(over: &AiProviderOverride) -> bool {
    over.kind.is_some()
        || over.base_url.is_some()
        || over.default_model.is_some()
        || over.key.is_some()
        || over.api_key_helper.is_some()
}

/// Canonical state with every section empty: the view renders one item at a
/// time, so an item's rendered output never depends on the other sections.
struct Empty {
    agents: AgentsConfig,
    providers: BTreeMap<String, AiProvider>,
    mcp: BTreeMap<String, McpServer>,
    secrets: BTreeMap<String, SecretValue>,
    skills: adapters::Skills,
    subagents: adapters::Subagents,
    access: SecretAccess,
}

impl Empty {
    fn new() -> Self {
        Self {
            agents: AgentsConfig::default(),
            providers: BTreeMap::new(),
            mcp: BTreeMap::new(),
            secrets: BTreeMap::new(),
            skills: BTreeMap::new(),
            subagents: BTreeMap::new(),
            access: SecretAccess::new(BTreeMap::new(), None),
        }
    }

    /// Desired state exposing only the canonical instructions.
    fn desired<'a>(&'a self, config_dir: &'a Path, instructions: Option<&'a str>) -> Desired<'a> {
        Desired {
            config_dir,
            agents: &self.agents,
            ai_providers: &self.providers,
            mcp: &self.mcp,
            instructions,
            secrets: &self.secrets,
            access: &self.access,
            skills: &self.skills,
            subagents: &self.subagents,
        }
    }

    /// Desired state exposing only `mcp`'s servers.
    fn desired_with_mcp<'a>(
        &'a self,
        config_dir: &'a Path,
        mcp: &'a BTreeMap<String, McpServer>,
    ) -> Desired<'a> {
        Desired {
            mcp,
            ..self.desired(config_dir, None)
        }
    }

    /// Desired state exposing only `skills`' skills.
    fn desired_with_skills<'a>(
        &'a self,
        config_dir: &'a Path,
        skills: &'a adapters::Skills,
    ) -> Desired<'a> {
        Desired {
            skills,
            ..self.desired(config_dir, None)
        }
    }

    /// Desired state exposing only `subagents`' subagents.
    fn desired_with_subagents<'a>(
        &'a self,
        config_dir: &'a Path,
        subagents: &'a adapters::Subagents,
    ) -> Desired<'a> {
        Desired {
            subagents,
            ..self.desired(config_dir, None)
        }
    }
}

/// Skills directory of each adapter in user scope (or in `project`), found by
/// rendering a sentinel skill and taking the parent of its path. Adapters
/// without a skills directory in that scope are absent.
pub fn skill_roots(
    config_dir: &Path,
    adapters: &[&'static Adapter],
    roots: &Roots,
    project: Option<&Path>,
) -> Result<BTreeMap<&'static str, PathBuf>, MusterError> {
    let empty = Empty::new();
    let sentinel = BTreeMap::from([(
        "__muster_probe__".to_owned(),
        BTreeMap::from([("SKILL.md".to_owned(), String::new())]),
    )]);
    let desired = empty.desired_with_skills(config_dir, &sentinel);
    let mut dirs = BTreeMap::new();
    for &adapter in adapters {
        let rendered = adapter.render(&desired, &Current::new())?;
        if let Some(file) = rendered.tree.first() {
            if project.is_some() && file.project.is_none() {
                continue;
            }
            let path = locate_tree(roots, file, project);
            if let Some(dir) = path.parent().and_then(|p| p.parent()) {
                dirs.insert(adapter.id, dir.to_path_buf());
            }
        }
    }
    Ok(dirs)
}

/// Layer kinds in fixed precedence order, deduplicated.
fn ordered_layers(kinds: Vec<&'static str>) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for kind in layer::PRECEDENCE {
        if let Some(found) = kinds.iter().find(|k| **k == kind) {
            out.push(*found);
        }
    }
    for kind in &kinds {
        if !layer::PRECEDENCE.contains(kind) && !out.contains(kind) {
            out.push(*kind);
        }
    }
    out
}

fn locate_file(roots: &Roots, file: &adapters::ManagedFile, project: Option<&Path>) -> PathBuf {
    project
        .and_then(|p| file.project.map(|path| p.join(path)))
        .unwrap_or_else(|| roots.resolve(file))
}

fn locate_tree(
    roots: &Roots,
    file: &adapters::RenderedTreeFile,
    project: Option<&Path>,
) -> PathBuf {
    project
        .and_then(|p| file.project.as_ref().map(|path| p.join(path)))
        .unwrap_or_else(|| roots.base(file.base).join(&file.path))
}

fn state_json(state: impl Serialize) -> Value {
    serde_json::to_value(state).expect("state serializes")
}

/// An object from ordered pairs; `Vec` keeps report order for the callers
/// that serialize it.
fn object_of<V: Serialize>(pairs: &[(String, V)]) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key.clone(), state_json(value));
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Secret scrubbing: the read-only view never touches (or reports) the
// resolved values behind secret references.
// ---------------------------------------------------------------------------

/// Env/header keys of `server` whose values are secret references.
fn secret_keys(map: &BTreeMap<String, String>) -> Vec<String> {
    map.iter()
        .filter(|(_, value)| adapters::is_secret_reference(value))
        .map(|(key, _)| key.clone())
        .collect()
}

/// Remove `name`'s secret-bearing values from a current config file, so the
/// read-only comparison never touches (or reports) them. Handles the three
/// native config formats: JSON (Claude, OpenCode, Pi, Antigravity — OpenCode
/// names the env map `environment`), TOML (Codex `mcp_servers` with
/// `env`/`http_headers`) and YAML (Hermes `mcp_servers` with `env`/`headers`).
fn scrub_mcp_secrets(content: &str, name: &str, env: &[String], headers: &[String]) -> String {
    if env.is_empty() && headers.is_empty() {
        return content.to_owned();
    }
    if let Ok(mut value) = serde_json::from_str::<Value>(content) {
        scrub_json_entry(&mut value, name, env, headers);
        return value.to_string();
    }
    if let Ok(mut doc) = content.parse::<toml_edit::DocumentMut>() {
        if let Some(entry) = doc
            .get_mut("mcp_servers")
            .and_then(toml_edit::Item::as_table_like_mut)
            .and_then(|servers| servers.get_mut(name))
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            scrub_toml_entry(entry, env, headers);
        }
        return doc.to_string();
    }
    if let Ok(mut docs) = yaml_rust2::YamlLoader::load_from_str(content) {
        let scrubbed = docs
            .first_mut()
            .and_then(yaml_rust2::Yaml::as_mut_hash)
            .map(|mapping| {
                if let Some(entry) = mapping
                    .get_mut(&yaml_rust2::Yaml::String("mcp_servers".to_owned()))
                    .and_then(yaml_rust2::Yaml::as_mut_hash)
                {
                    scrub_yaml_entry(entry, env, headers);
                }
                yaml_rust2::Yaml::Hash(mapping.clone())
            });
        if let Some(doc) = scrubbed {
            let mut out = String::new();
            if yaml_rust2::YamlEmitter::new(&mut out).dump(&doc).is_ok() {
                out.push('\n');
                return out;
            }
        }
    }
    content.to_owned()
}

/// Strip the secret-bearing fields of `name`'s entry from a parsed JSON
/// config (any of the roots and field spellings the adapters write),
/// dropping fields left empty so an env map this view cannot resolve never
/// differs from an absent one.
fn scrub_json_entry(value: &mut Value, name: &str, env: &[String], headers: &[String]) {
    for root in ["mcpServers", "mcp", "mcp_servers"] {
        let Some(entry) = value
            .get_mut(root)
            .and_then(|root| root.get_mut(name))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        for (field, keys) in [
            ("env", env),
            ("environment", env),
            ("headers", headers),
            ("http_headers", headers),
        ] {
            if let Some(map) = entry.get_mut(field).and_then(Value::as_object_mut) {
                for key in keys {
                    map.remove(key);
                }
                if map.is_empty() {
                    entry.remove(field);
                }
            }
        }
    }
}

fn scrub_toml_entry(entry: &mut dyn toml_edit::TableLike, env: &[String], headers: &[String]) {
    for (field, keys) in [("env", env), ("http_headers", headers)] {
        let Some(item) = entry.get_mut(field) else {
            continue;
        };
        let empty = if let Some(table) = item.as_inline_table_mut() {
            for key in keys {
                table.remove(key);
            }
            table.is_empty()
        } else if let Some(table) = item.as_table_mut() {
            for key in keys {
                table.remove(key);
            }
            table.is_empty()
        } else {
            continue;
        };
        if empty {
            entry.remove(field);
        }
    }
}

fn scrub_yaml_entry(entry: &mut yaml_rust2::yaml::Hash, env: &[String], headers: &[String]) {
    let key = |field: &str| yaml_rust2::Yaml::String(field.to_owned());
    for (field, keys) in [("env", env), ("headers", headers)] {
        if let Some(yaml_rust2::Yaml::Hash(map)) = entry.get_mut(&key(field)) {
            for secret in keys {
                map.remove(&key(secret));
            }
            if map.is_empty() {
                entry.remove(&key(field));
            }
        }
    }
}

/// Do two rendered config files describe the same content, ignoring
/// formatting and the values of `name`'s secret-bearing fields? Parsed
/// structures are compared (JSON, TOML and YAML), so an on-disk secret this
/// read-only view cannot resolve never reads as drift.
fn same_config(a: &str, b: &str, name: &str, env: &[String], headers: &[String]) -> bool {
    if a == b {
        return true;
    }
    match (
        config_signature(a, name, env, headers),
        config_signature(b, name, env, headers),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn config_signature(text: &str, name: &str, env: &[String], headers: &[String]) -> Option<String> {
    let mut value = parse_config(text)?;
    scrub_json_entry(&mut value, name, env, headers);
    serde_json::to_string(&value).ok()
}

/// Parse any of the three native config formats into a JSON value.
fn parse_config(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Some(value);
    }
    if let Ok(value) = toml::from_str::<toml::Value>(text) {
        return serde_json::to_value(value).ok();
    }
    let docs = yaml_rust2::YamlLoader::load_from_str(text).ok()?;
    Some(yaml_value(docs.into_iter().next()?))
}

fn yaml_value(yaml: yaml_rust2::Yaml) -> Value {
    use yaml_rust2::Yaml;
    match yaml {
        Yaml::Real(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        Yaml::Integer(i) => Value::from(i),
        Yaml::String(s) => Value::String(s),
        Yaml::Boolean(b) => Value::from(b),
        Yaml::Array(items) => Value::Array(items.into_iter().map(yaml_value).collect()),
        Yaml::Hash(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let key = match key {
                        Yaml::String(s) => s,
                        other => yaml_value(other).to_string(),
                    };
                    (key, yaml_value(value))
                })
                .collect(),
        ),
        _ => Value::Null,
    }
}
