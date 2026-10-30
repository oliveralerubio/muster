//! The apply engine: desired state -> plan -> apply, with ownership records,
//! backups, numbered generations, rollback and drift detection.
//!
//! Interface (everything else is private):
//! - [`plan`]: merge the configuration layers, resolve secrets, render every
//!   adapter (pure) against the current files, and classify each managed file
//!   as add / modify / remove / unchanged. Never writes.
//! - [`apply`]: gate new hooks/MCP servers behind approval, back up and write
//!   the files atomically, and record a generation. Honours `--dry-run`.
//! - [`rollback`]: restore the exact bytes of generation N (recorded as a new
//!   generation).
//!
//! Plans hold resolved secrets in memory only; their JSON ([`Plan::to_json`])
//! redacts every value as `<redacted:REF>` and hides removed lines of files
//! that hold secrets.

pub(crate) mod diff;
pub mod dotfiles;
pub mod layer;
mod ownership;
pub(crate) mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlLoader};

use crate::adapters::config::{AgentsConfig, McpServer, Transport};
use crate::adapters::{self, Adapter, Base, Capability, Current, Desired, FileFormat, ManagedFile};
use crate::context::Context;
use crate::errors::MusterError;
use crate::installer;
use crate::manifest::{self, Manifest};
use crate::output::{Diagnostic, Severity};
use crate::providers::Providers;
use crate::secrets::{self, SecretValue};
use store::{Approved, Record, State};

/// Where the rendered files go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// User-scope files under `$HOME` / `$XDG_CONFIG_HOME`.
    Global,
    /// Project-scope files inside this directory (layer in `<dir>/.muster/`).
    Project(PathBuf),
}

impl Target {
    /// Key used in ownership records and approvals.
    fn key(&self) -> String {
        match self {
            Self::Global => "global".into(),
            Self::Project(dir) => dir.display().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Add,
    Modify,
    Remove,
    Unchanged,
}

/// One managed file in a plan. `before`/`after` may hold resolved secrets.
struct Entry {
    path: PathBuf,
    agents: Vec<&'static str>,
    format: FileFormat,
    before: Option<String>,
    after: Option<String>,
    /// Still rendered by an adapter (false: only pruned, no longer managed).
    managed: bool,
    owned: Vec<Vec<String>>,
    private: bool,
    /// Written executable (managed dotfiles only).
    executable: bool,
    created: bool,
    drift: bool,
}

impl Entry {
    fn action(&self) -> Action {
        match (&self.before, &self.after) {
            (None, Some(_)) => Action::Add,
            (Some(_), None) => Action::Remove,
            (Some(b), Some(a)) if b != a => Action::Modify,
            _ => Action::Unchanged,
        }
    }
}

/// Executable code that is new (or changed) since the last approval.
struct Approval {
    kind: &'static str,
    name: String,
    detail: String,
}

/// The planned change set. Not `Debug`: entries hold resolved secrets.
pub struct Plan {
    target: Target,
    entries: Vec<Entry>,
    /// Adapter id -> failure message (None = rendered fine).
    agents: BTreeMap<&'static str, Option<String>>,
    approvals: Vec<Approval>,
    /// Resolved pins needing installation, including previously approved versions.
    installs: Vec<(String, installer::Pin)>,
    bin_dir: PathBuf,
    skipped_skills: Vec<String>,
    disabled: Vec<Value>,
    /// Approvals for this target once the plan is applied.
    approved: Approved,
    base: Option<(u64, State)>,
    secrets: BTreeMap<String, SecretValue>,
    layers: Vec<layer::Layer>,
}

/// `muster plan` / `sync` / `agents apply` options.
pub struct Options<'a> {
    pub target: Target,
    /// Restrict to one adapter (`agents apply --agent`); stale entries of
    /// other adapters are left alone.
    pub agent: Option<&'a str>,
}

/// Compute the plan. Never writes. Per-adapter failures become error
/// diagnostics; the other adapters are still planned.
pub fn plan(ctx: &mut Context, options: &Options) -> Result<Plan, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let layers = match &options.target {
        Target::Global => layer::global_layers(
            &ctx.config_dir,
            &manifest,
            crate::machine::load_name(&ctx.state_dir).as_deref(),
        ),
        Target::Project(dir) => layer::project_layers(dir)?,
    };
    // The single place layers become desired state.
    let mut merged = layer::merge(&layers);
    let mut inherited_known: BTreeSet<(&'static str, String)> = BTreeSet::new();
    if matches!(options.target, Target::Project(_)) {
        let global = layer::global_merge(&ctx.config_dir, &ctx.state_dir, &manifest);
        inherited_known = user_scope_names(&global);
        let mut toggles = global.toggles;
        toggles.extend(merged.toggles);
        merged.toggles = toggles;
    }
    if merged.agents.enabled.is_none() {
        merged.agents.enabled = manifest.agents.as_ref().and_then(|a| a.enabled.clone());
    }
    for (name, server) in &merged.mcp {
        server
            .transport()
            .map_err(|message| MusterError::InvalidConfig {
                message: format!("MCP server `{name}` {message}"),
                path: Some(format!("{}: mcp.{name}", manifest::FILE_NAME)),
            })?;
    }
    for (agent, id) in &merged.agents.ai_providers {
        if crate::adapters::find(agent).is_none() || !merged.ai_providers.contains_key(id) {
            return Err(MusterError::InvalidConfig {
                message: format!("unknown agent or provider in agents.providers.{agent}"),
                path: Some(manifest::FILE_NAME.into()),
            });
        }
    }
    let selected = match options.agent {
        Some(id) => vec![single_adapter(id)?],
        None => enabled_adapters(Some(&merged.agents))?,
    };
    let instructions = read_instructions(ctx, &merged)?;
    let source = merged.instructions_path()?.0;
    if let Some(dir) = source.parent() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(id) = name
                    .strip_prefix("AGENTS.")
                    .and_then(|s| s.strip_suffix(".md"))
                {
                    if adapters::find(id).is_none() {
                        ctx.diag(
                            Diagnostic::new(
                                Severity::Warning,
                                "instructions_overlay_unknown_agent",
                                format!("unknown adapter `{id}` in instruction overlay"),
                            )
                            .with_path(entry.path().display().to_string()),
                        );
                    }
                }
            }
        }
    }
    let (skills, skipped_skills) = read_skills(ctx, &merged)?;
    let (subagents, skipped_subagents) = read_subagents(ctx, &merged)?;
    let mut disabled = Vec::new();
    let known: BTreeSet<(&str, &str)> = skills
        .keys()
        .chain(skipped_skills.iter())
        .map(|n| ("skill", n.as_str()))
        .chain(
            subagents
                .keys()
                .chain(skipped_subagents.iter())
                .map(|n| ("subagent", n.as_str())),
        )
        .chain(merged.mcp.keys().map(|n| ("mcp", n.as_str())))
        .collect();
    for (_, toggles) in &merged.toggles {
        for item in toggles.disable.iter().chain(&toggles.enable) {
            let (kind, name, _) =
                crate::adapters::config::parse_toggle(item).expect("validated toggle");
            // A project run also inherits global and machine toggles, whose
            // items live in user scope: known either here or there is no typo.
            if !known.contains(&(kind, name))
                && !inherited_known.contains(&(kind, name.to_string()))
            {
                ctx.diag(Diagnostic::new(
                    Severity::Warning,
                    "toggle_unknown_item",
                    format!("unknown toggle item `{item}`"),
                ));
            }
        }
    }
    for adapter in &selected {
        for (kind, name) in &known {
            if let Some(layer) = merged.disabled(kind, name, adapter.id) {
                disabled.push(json!({"kind":kind, "name":name, "agent":adapter.id, "layer":layer}));
            }
        }
    }
    let mut secrets = resolve_secrets(ctx, &merged.mcp);
    let mut secret_providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    for (id, ai_provider) in &merged.ai_providers {
        let needs_key = merged.agents.ai_providers.iter().any(|(agent, selected)| {
            selected == id && (agent != "claude" || ai_provider.api_key_helper != Some(true))
        });
        if !needs_key {
            continue;
        }
        if let Some(reference) = &ai_provider.key {
            if !secrets.contains_key(reference) {
                // The adapter reports an unresolved reference without exposing
                // provider CLI output (which may include sensitive values).
                if let Ok(value) = secret_providers.resolve(reference) {
                    secrets.insert(reference.clone(), value);
                }
            }
        }
    }
    // Grants come only from the config repo's manifest: a project's own
    // `.muster/muster.toml` cannot grant itself secrets.
    let project = match &options.target {
        Target::Global => None,
        Target::Project(dir) => dir.file_name().map(|n| n.to_string_lossy().into_owned()),
    };
    let access = crate::secrets::SecretAccess::new(manifest.secrets.clone(), project);
    let config_dir = std::path::absolute(&ctx.config_dir)
        .map_err(|e| MusterError::Operational(format!("resolve config path: {e}")))?;
    let desired = Desired {
        config_dir: &config_dir,
        agents: &merged.agents,
        ai_providers: &merged.ai_providers,
        mcp: &merged.mcp,
        instructions: instructions.as_deref(),
        secrets: &secrets,
        access: &access,
        skills: &skills,
        subagents: &subagents,
    };

    let roots = Roots::from_env()?;
    let base = store::latest(&ctx.state_dir)?;
    let key = options.target.key();
    let empty = BTreeMap::new();
    let records = base.as_ref().map_or(&empty, |(_, s)| &s.files);
    let ours = |path: &Path| {
        records
            .get(&path.display().to_string())
            .filter(|r| r.target == key)
    };

    // Render only a sentinel file for skipped skills to discover their adapter-
    // specific directories. Keep all previous files there, including files no
    // longer present in the unverified source, and retain their ownership.
    let skipped: adapters::Skills = skipped_skills
        .iter()
        .map(|name| {
            (
                name.clone(),
                BTreeMap::from([("SKILL.md".into(), String::new())]),
            )
        })
        .collect();
    let no_subagents = adapters::Subagents::new();
    let skipped_desired = Desired {
        skills: &skipped,
        subagents: &no_subagents,
        ..desired
    };
    let mut protected = BTreeSet::new();
    let mut visited = BTreeSet::new();
    if !skipped.is_empty() {
        for adapter in selected
            .iter()
            .copied()
            .chain(records.values().filter_map(|r| adapters::find(&r.agent)))
        {
            if !visited.insert(adapter.id) {
                continue;
            }
            let allowed_skills = skipped
                .iter()
                .filter(|(name, _)| merged.disabled("skill", name, adapter.id).is_none())
                .map(|(name, files)| (name.clone(), files.clone()))
                .collect();
            let adapter_skipped = Desired {
                skills: &allowed_skills,
                ..skipped_desired
            };
            let rendered = match adapter.render(&adapter_skipped, &Current::new()) {
                Ok(rendered) => rendered,
                Err(_) => {
                    // Keep adapter failures isolated and never prune a skipped
                    // skill when its adapter cannot determine output paths.
                    protected.extend(
                        records
                            .iter()
                            .filter(|(_, record)| {
                                record.agent == adapter.id && record.target == key
                            })
                            .map(|(path, _)| PathBuf::from(path)),
                    );
                    continue;
                }
            };
            for file in rendered.tree {
                let path = match &options.target {
                    Target::Global => roots.base(file.base).join(file.path),
                    Target::Project(dir) => match file.project {
                        Some(path) => dir.join(path),
                        None => continue,
                    },
                };
                if let Some(parent) = path.parent() {
                    protected.insert(parent.to_path_buf());
                }
            }
        }
    }
    // Render unreadable subagents separately: protect their exact output files,
    // not the entire agents directory, so unrelated stale files still prune.
    let skipped_agents: adapters::Subagents = skipped_subagents
        .iter()
        .map(|name| {
            (
                name.clone(),
                adapters::Subagent {
                    name: name.clone(),
                    description: String::new(),
                    tools: Vec::new(),
                    model: None,
                    extra_fields: Vec::new(),
                    body: String::new(),
                    source: String::new(),
                },
            )
        })
        .collect();
    if !skipped_agents.is_empty() {
        let empty_skills = adapters::Skills::new();
        let skipped_desired = Desired {
            skills: &empty_skills,
            subagents: &skipped_agents,
            ..desired
        };
        let mut visited = BTreeSet::new();
        for adapter in selected
            .iter()
            .copied()
            .chain(records.values().filter_map(|r| adapters::find(&r.agent)))
        {
            if !visited.insert(adapter.id) {
                continue;
            }
            let allowed_subagents = skipped_agents
                .iter()
                .filter(|(name, _)| merged.disabled("subagent", name, adapter.id).is_none())
                .map(|(name, subagent)| (name.clone(), subagent.clone()))
                .collect();
            let adapter_skipped = Desired {
                subagents: &allowed_subagents,
                ..skipped_desired
            };
            let rendered = match adapter.render(&adapter_skipped, &Current::new()) {
                Ok(rendered) => rendered,
                Err(_) => {
                    protected.extend(
                        records
                            .iter()
                            .filter(|(_, record)| {
                                record.agent == adapter.id && record.target == key
                            })
                            .map(|(path, _)| PathBuf::from(path)),
                    );
                    continue;
                }
            };
            for file in rendered.tree {
                let path = match &options.target {
                    Target::Global => roots.base(file.base).join(file.path),
                    Target::Project(dir) => match file.project {
                        Some(path) => dir.join(path),
                        None => continue,
                    },
                };
                protected.insert(path);
            }
        }
    }
    let preserve = |path: &Path| protected.iter().any(|dir| path.starts_with(dir));
    let enabled_project_agents = enabled_adapters(Some(&merged.agents))?.len();
    let mut entries: BTreeMap<PathBuf, Entry> = BTreeMap::new();
    let mut agents = BTreeMap::new();
    for adapter in &selected {
        let ctx_target = &options.target;
        let skip_instructions = matches!(ctx_target, Target::Global)
            && merged
                .agents
                .skip_instructions
                .as_ref()
                .is_some_and(|ids| ids.iter().any(|id| id == adapter.id));
        let overlay_path = merged
            .instructions_path()?
            .0
            .with_file_name(format!("AGENTS.{}.md", adapter.id));
        let overlay = read_optional(&overlay_path)?;
        let project_overlay = matches!(ctx_target, Target::Project(_));
        let shared_project_overlay = project_overlay && adapter.id != "claude";
        let project_file_shared = shared_project_overlay && enabled_project_agents > 1;
        if project_file_shared && overlay.is_some() {
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    "instructions_overlay_shared_project_file",
                    format!(
                        "instruction overlay for {} cannot be written to shared project AGENTS.md",
                        adapter.id
                    ),
                )
                .with_path(overlay_path.display().to_string()),
            );
        }
        if skip_instructions && overlay.is_some() {
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    "instructions_overlay_skipped",
                    format!("instruction overlay for {} was skipped", adapter.id),
                )
                .with_path(overlay_path.display().to_string()),
            );
        }
        let combined = match (
            instructions.as_deref(),
            if project_file_shared || (project_overlay && adapter.id == "claude") {
                None
            } else {
                overlay.as_deref()
            },
        ) {
            (Some(shared), Some(extra)) => Some(format!(
                "{}\n\n## AGENTS.{}.md\n\n{}",
                shared.trim_end(),
                adapter.id,
                extra
            )),
            _ => instructions.clone(),
        };
        let agent_skills = skills
            .iter()
            .filter(|(name, _)| merged.disabled("skill", name, adapter.id).is_none())
            .map(|(name, skill)| (name.clone(), skill.clone()))
            .collect();
        let agent_mcp = merged
            .mcp
            .iter()
            .filter(|(name, _)| merged.disabled("mcp", name, adapter.id).is_none())
            .map(|(name, server)| (name.clone(), server.clone()))
            .collect();
        let agent_subagents = subagents
            .iter()
            .filter(|(name, _)| merged.disabled("subagent", name, adapter.id).is_none())
            .map(|(name, subagent)| (name.clone(), subagent.clone()))
            .collect();
        let agent_desired = Desired {
            mcp: &agent_mcp,
            subagents: &agent_subagents,
            skills: &agent_skills,
            instructions: if skip_instructions {
                None
            } else {
                combined.as_deref()
            },
            ..desired
        };
        match plan_adapter(
            adapter,
            &agent_desired,
            &roots,
            ctx_target,
            &ours,
            &entries,
            if project_overlay && adapter.id == "claude" {
                overlay.as_deref()
            } else {
                None
            },
        ) {
            Ok((planned, diagnostics)) => {
                for d in diagnostics {
                    ctx.diag(d);
                }
                for entry in planned {
                    merge_entry(&mut entries, entry);
                }
                // Render-time files (skills) this adapter no longer renders.
                for (path, record) in records {
                    let path = PathBuf::from(path);
                    if record.target != key
                        || record.agent != adapter.id
                        || entries.contains_key(&path)
                        || preserve(&path)
                    {
                        continue;
                    }
                    match read_optional(&path) {
                        Ok(before) => {
                            if let Some(entry) = unmanaged_entry(adapter.id, &path, before, record)
                            {
                                merge_entry(&mut entries, entry);
                            }
                        }
                        Err(err) => {
                            ctx.diag(adapter_failure(adapter.id, adapter.name, &err.to_string()))
                        }
                    }
                }
                agents.insert(adapter.id, None);
            }
            Err(err) => {
                ctx.diag(adapter_failure(adapter.id, adapter.name, &err.to_string()));
                agents.insert(adapter.id, Some(err.to_string()));
            }
        }
    }

    // Managed dotfiles: user scope only, and not when one adapter is selected.
    if options.target == Target::Global && options.agent.is_none() {
        dotfiles::plan(ctx, &manifest, &roots, &ours, records, &key, &mut entries)?;
    }

    // Adapters no longer enabled: prune what they own in this target.
    if options.agent.is_none() {
        for (path, record) in records {
            let path = PathBuf::from(path);
            if record.target != key
                || entries.contains_key(&path)
                || preserve(&path)
                || selected.iter().any(|a| a.id == record.agent)
            {
                continue;
            }
            let Some(agent) = adapters::find(&record.agent).map(|a| a.id) else {
                continue;
            };
            match read_optional(&path) {
                Ok(before) => {
                    if let Some(entry) = unmanaged_entry(agent, &path, before, record) {
                        merge_entry(&mut entries, entry);
                    }
                    agents.entry(agent).or_insert(None);
                }
                Err(err) => {
                    ctx.diag(adapter_failure(agent, agent, &err.to_string()));
                    agents.insert(agent, Some(err.to_string()));
                }
            }
        }
    }

    // Native credentials can predate Muster ownership. Keep them out of diffs
    // even when runtime helpers deliberately skip provider resolution.
    for entry in entries.values() {
        if entry.agents.contains(&"claude") && entry.path.ends_with(".claude/settings.json") {
            if let Some(before) = entry
                .before
                .as_deref()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
            {
                for key in ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"] {
                    if let Some(value) = before["env"][key]
                        .as_str()
                        .filter(|value| !value.is_empty())
                    {
                        secrets.insert(
                            format!("claude-settings://{key}"),
                            SecretValue::new(value.to_owned()),
                        );
                    }
                }
            }
        }
    }
    for entry in entries.values_mut() {
        entry.private |= holds_secret(entry.before.as_deref(), &secrets)
            || holds_secret(entry.after.as_deref(), &secrets);
        if entry.drift {
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    "drift",
                    format!(
                        "{} changed outside Muster since the last apply; managed keys are \
                         restored, other edits are kept",
                        entry.path.display()
                    ),
                )
                .with_path(entry.path.display().to_string()),
            );
        }
    }

    let previous = base
        .as_ref()
        .and_then(|(_, s)| s.approved.get(&key))
        .cloned()
        .unwrap_or_default();
    let (mut approvals, approved) = approvals(&selected, &merged, previous);
    let bin_dir = installer::bin_dir()?;
    let installs = agent_installs(ctx, options.agent, &bin_dir)?;
    for (agent, pin) in &installs {
        if approved.agents.get(agent) != Some(&pin.version) || (!ctx.is_tty && !ctx.yes) {
            approvals.push(Approval {
                kind: "agent",
                name: agent.clone(),
                detail: {
                    let old = installer::installed_version(agent, pin, &bin_dir)?;
                    let artifact = pin
                        .resolve_artifact(&crate::machine::platform())
                        .expect("planned installs have a platform artifact");
                    format!(
                        "install {agent}: {} -> {}; source {}; sha256 {}",
                        old.as_deref().unwrap_or("not installed"),
                        pin.version,
                        artifact.url,
                        artifact.sha256
                    )
                },
            });
        }
    }

    Ok(Plan {
        target: options.target.clone(),
        entries: entries.into_values().collect(),
        agents,
        approvals,
        installs,
        bin_dir,
        skipped_skills,
        disabled,
        approved,
        base,
        secrets,
        layers,
    })
}

/// Resolve platform artifacts and detect version drift without downloading.
fn agent_installs(
    ctx: &mut Context,
    selected: Option<&str>,
    bin_dir: &Path,
) -> Result<Vec<(String, installer::Pin)>, MusterError> {
    let lock = installer::Lock::load_or_default(&ctx.config_dir)?;
    let platform = crate::machine::platform();
    let mut installs = Vec::new();
    for (agent, pin) in lock.agents {
        if selected.is_some_and(|id| id != agent) {
            continue;
        }
        let installed = installer::installed_version(&agent, &pin, bin_dir)?;
        if let Some(version) = installed
            .as_deref()
            .filter(|v| installer::is_newer(v, &pin.version))
        {
            ctx.diag(installer::newer_diagnostic(&agent, version, &pin.version));
            continue;
        }
        if pin.resolve_artifact(&platform).is_none() {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "agent_platform_unsupported",
                format!(
                    "agent {agent} {} has no artifact for platform {platform}; not installed",
                    pin.version
                ),
            ));
            continue;
        }
        if installed.as_deref() != Some(pin.version.as_str()) {
            installs.push((agent, pin));
        }
    }
    Ok(installs)
}

type Ours<'r> = dyn Fn(&Path) -> Option<&'r Record> + 'r;

/// Plan one adapter: prune its stale keys, render, and build its entries.
/// All-or-nothing: any error fails only this adapter.
fn plan_adapter(
    adapter: &'static Adapter,
    desired: &Desired,
    roots: &Roots,
    target: &Target,
    ours: &Ours<'_>,
    pending: &BTreeMap<PathBuf, Entry>,
    project_overlay: Option<&str>,
) -> Result<(Vec<Entry>, Vec<Diagnostic>), MusterError> {
    // What the adapter owns today = what it writes into an empty file.
    let owned_now: BTreeMap<&str, Vec<Vec<String>>> = adapter
        .render(desired, &Current::new())?
        .files
        .iter()
        .map(|f| {
            (
                f.file.path,
                ownership::owned_keys(f.file.format, &f.contents),
            )
        })
        .collect();

    let mut files: Vec<(
        &'static ManagedFile,
        PathBuf,
        Option<String>,
        Option<String>,
    )> = Vec::new();
    let mut current = Current::new();
    for file in adapter.files {
        let Some(path) = roots.locate(file, target) else {
            continue;
        };
        // A file another adapter already planned (shared project AGENTS.md)
        // continues from that adapter's output.
        let (before, text) = match pending.get(&path) {
            Some(entry) => (entry.before.clone(), entry.after.clone()),
            None => {
                let text = read_optional(&path)?;
                (text.clone(), text)
            }
        };
        let text = match (text, ours(&path)) {
            (Some(text), Some(record)) => {
                let now = owned_now.get(file.path).cloned().unwrap_or_default();
                let stale: Vec<Vec<String>> = record
                    .owned
                    .iter()
                    .filter(|k| !now.contains(k))
                    .cloned()
                    .collect();
                Some(ownership::prune(file.format, &text, &stale))
            }
            (text, _) => text,
        };
        if let Some(text) = &text {
            current.insert(file.path, text.clone());
        }
        files.push((file, path, before, text));
    }

    let mut rendered = adapter.render(desired, &current)?;
    if let Some(overlay) = project_overlay {
        if let Some(file) = rendered
            .files
            .iter_mut()
            .find(|f| f.file.project == Some("CLAUDE.md"))
        {
            let body = format!("@AGENTS.md\n\n## AGENTS.claude.md\n\n{overlay}");
            file.contents = adapters::markdown_block(current.get(file.file.path), &body);
        }
    }
    if matches!(target, Target::Project(_))
        && adapter.id == "hermes"
        && (!desired.mcp.is_empty() || desired.agents.models.contains_key(adapter.id))
    {
        rendered.diagnostics.push(Diagnostic::new(
            Severity::Warning,
            "unsupported_feature",
            "Hermes Agent MCP servers and model settings are user-scoped only; skipped for this project",
        ).with_path(adapter.id));
    }
    let mut entries = Vec::new();
    for (file, path, before, pruned) in files {
        let record = ours(&path);
        let created = record.map_or(before.is_none(), |r| r.created);
        let drift = record.is_some_and(|r| before.as_deref() != Some(r.contents.as_str()));
        let output = rendered.files.iter().find(|r| r.file.path == file.path);
        let (after, managed) = match output {
            Some(out) => (Some(out.contents.clone()), true),
            None if record.is_none() => continue,
            None => match pruned {
                Some(text) if created && ownership::is_empty(file.format, &text) => (None, false),
                other => (other, false),
            },
        };
        entries.push(Entry {
            path,
            agents: vec![adapter.id],
            format: file.format,
            before,
            after,
            managed,
            owned: if managed {
                owned_now.get(file.path).cloned().unwrap_or_default()
            } else {
                Vec::new()
            },
            private: record.is_some_and(|r| r.private),
            executable: false,
            created,
            drift,
        });
    }
    // Whole files at render-time paths (skills): Muster owns the file.
    for file in &rendered.tree {
        let path = match target {
            Target::Global => roots.base(file.base).join(&file.path),
            Target::Project(dir) => match &file.project {
                Some(project) => dir.join(project),
                None => continue,
            },
        };
        let before = match pending.get(&path) {
            Some(entry) => entry.before.clone(),
            None => read_optional(&path)?,
        };
        let record = ours(&path);
        entries.push(Entry {
            created: record.map_or(before.is_none(), |r| r.created),
            drift: record.is_some_and(|r| before.as_deref() != Some(r.contents.as_str())),
            path,
            agents: vec![adapter.id],
            format: FileFormat::Text,
            before,
            after: Some(file.contents.clone()),
            managed: true,
            owned: ownership::owned_keys(FileFormat::Text, &file.contents),
            private: record.is_some_and(|r| r.private),
            executable: false,
        });
    }
    Ok((entries, rendered.diagnostics))
}

/// Entry for a file whose adapter is no longer enabled: drop its owned keys.
fn unmanaged_entry(
    agent: &'static str,
    path: &Path,
    before: Option<String>,
    record: &Record,
) -> Option<Entry> {
    let text = before.as_ref()?;
    let pruned = ownership::prune(record.format, text, &record.owned);
    let after = if record.created && ownership::is_empty(record.format, &pruned) {
        None
    } else {
        Some(pruned)
    };
    Some(Entry {
        path: path.to_path_buf(),
        agents: vec![agent],
        format: record.format,
        drift: before.as_deref() != Some(record.contents.as_str()),
        before,
        after,
        managed: false,
        owned: Vec::new(),
        private: record.private,
        executable: false,
        created: record.created,
    })
}

fn merge_entry(entries: &mut BTreeMap<PathBuf, Entry>, entry: Entry) {
    match entries.get_mut(&entry.path) {
        Some(existing) => {
            existing.after = entry.after;
            existing.managed |= entry.managed;
            for key in entry.owned {
                if !existing.owned.contains(&key) {
                    existing.owned.push(key);
                }
            }
            existing.private |= entry.private;
            existing.agents.extend(entry.agents);
        }
        None => {
            entries.insert(entry.path.clone(), entry);
        }
    }
}

/// New or changed hooks / MCP servers since the last approval for this target.
/// Only kinds some selected adapter can install are gated (and recorded).
fn approvals(
    selected: &[&'static Adapter],
    merged: &layer::Merged,
    previous: Approved,
) -> (Vec<Approval>, Approved) {
    let supports = |caps: &[Capability]| {
        selected
            .iter()
            .any(|a| caps.iter().any(|c| a.capabilities.contains(c)))
    };
    let mut approvals = Vec::new();
    let mut next = previous.clone();
    if supports(&[Capability::Hooks]) {
        next.hooks = BTreeMap::new();
        for (event, hook) in &merged.agents.hooks {
            let value = serde_json::to_value(hook).expect("hook serializes");
            if previous.hooks.get(event) != Some(&value) {
                approvals.push(Approval {
                    kind: "hook",
                    name: event.clone(),
                    detail: hook.command.clone(),
                });
            }
            next.hooks.insert(event.clone(), value);
        }
    }
    if supports(&[Capability::McpStdio, Capability::McpRemote]) {
        next.mcp = BTreeMap::new();
        for (name, server) in &merged.mcp {
            if !selected.iter().any(|adapter| {
                merged.disabled("mcp", name, adapter.id).is_none()
                    && adapter
                        .capabilities
                        .iter()
                        .any(|c| matches!(c, Capability::McpStdio | Capability::McpRemote))
            }) {
                continue;
            }
            let value = serde_json::to_value(server).expect("server serializes");
            if previous.mcp.get(name) != Some(&value) {
                let detail = match server.transport() {
                    Ok(Transport::Stdio { command }) => std::iter::once(command.to_string())
                        .chain(server.args.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" "),
                    Ok(Transport::Remote { url }) => url.to_string(),
                    Err(_) => String::new(),
                };
                approvals.push(Approval {
                    kind: "mcp_server",
                    name: name.clone(),
                    detail,
                });
            }
            next.mcp.insert(name.clone(), value);
        }
    }
    (approvals, next)
}

impl Plan {
    pub fn has_changes(&self) -> bool {
        !self.installs.is_empty() || self.entries.iter().any(|e| e.action() != Action::Unchanged)
    }

    /// JSON for CLI/MCP/web. Secret values are always redacted.
    ///
    /// ```json
    /// {"target": "global", "dry_run": true, "applied": false, "generation": 3,
    ///  "changes": 1,
    ///  "agents": [{"id": "claude", "status": "planned|applied|unchanged|failed",
    ///              "files": [{"path", "action": "add|modify|remove|unchanged",
    ///                         "changed", "drift", "private", "diff"}]}],
    ///  "files": [<managed dotfiles, same shape as agent files>],
    ///  "approvals": [{"kind": "hook|mcp_server", "name", "detail"}]}
    /// ```
    /// Last applied generation the plan was computed against.
    /// The layers this plan merged and the fixed precedence (`--explain`).
    pub fn explain(&self) -> Value {
        layer::explain(&self.layers)
    }

    pub fn base_generation(&self) -> Option<u64> {
        self.base.as_ref().map(|(n, _)| *n)
    }

    pub fn to_json(&self, dry_run: bool, applied: bool, generation: Option<u64>) -> Value {
        let mut agents = Vec::new();
        for (id, failure) in &self.agents {
            let files: Vec<Value> = self
                .entries
                .iter()
                .filter(|e| e.agents.contains(id))
                .map(|e| self.file_json(e))
                .collect();
            let changed = files.iter().any(|f| f["changed"] == true);
            let status = match (failure, changed, applied) {
                (Some(_), _, _) => "failed",
                (None, false, _) => "unchanged",
                (None, true, true) => "applied",
                (None, true, false) => "planned",
            };
            let mut agent = json!({ "id": id, "status": status, "files": files });
            if let Some(message) = failure {
                agent["error"] = json!(message);
            }
            agents.push(agent);
        }
        let approvals: Vec<Value> = self
            .approvals
            .iter()
            .map(|a| json!({ "kind": a.kind, "name": a.name, "detail": a.detail }))
            .collect();
        let files: Vec<Value> = self
            .entries
            .iter()
            .filter(|e| e.agents == [dotfiles::OWNER])
            .map(|e| self.file_json(e))
            .collect();
        json!({
            "target": match &self.target {
                Target::Global => json!("global"),
                Target::Project(dir) => json!({ "project": dir }),
            },
            "dry_run": dry_run,
            "applied": applied,
            "generation": generation,
            "changes": self.entries.iter().filter(|e| e.action() != Action::Unchanged).count()
                + self.installs.len(),
            "installs": self.installs.iter().map(|(agent, pin)| json!({
                "agent": agent,
                "version": pin.version,
                "action": "install",
            })).collect::<Vec<_>>(),
            "agents": agents,
            "files": files,
            "approvals": approvals,
            "skipped_skills": self.skipped_skills,
            "disabled": self.disabled,
        })
    }

    fn file_json(&self, entry: &Entry) -> Value {
        let redact = |text: &Option<String>| {
            text.as_deref()
                .map(|t| redact(t, &self.secrets))
                .unwrap_or_default()
        };
        let action = entry.action();
        let diff = diff::unified(
            &redact(&entry.before),
            &redact(&entry.after),
            &entry.path.display().to_string(),
            entry.private,
        );
        json!({
            "path": entry.path,
            "action": action,
            "changed": action != Action::Unchanged,
            "drift": entry.drift,
            "private": entry.private,
            "diff": diff,
        })
    }
}

/// Apply a plan: approvals, backups, atomic writes, ownership records and a
/// new generation. With `--dry-run` this only reports the plan. Returns the
/// plan JSON with `applied`/`generation` filled in.
pub fn apply(ctx: &mut Context, mut plan: Plan) -> Result<Value, MusterError> {
    if ctx.dry_run {
        return Ok(plan.to_json(true, false, plan.base_generation()));
    }
    let unattended = !ctx.is_tty && !ctx.yes;
    if !plan.approvals.is_empty()
        && (!unattended || plan.approvals.iter().any(|a| a.kind != "agent"))
    {
        let list: Vec<String> = plan
            .approvals
            .iter()
            .map(|a| format!("{} `{}` ({})", a.kind.replace('_', " "), a.name, a.detail))
            .collect();
        ctx.confirm(&format!(
            "Install new or changed code that runs on this machine: {}",
            list.join(", ")
        ))?;
    }

    if unattended {
        if !plan.installs.is_empty() {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "agent_approval_needed",
                "agent installs need approval; current binaries were left alone (run sync --yes to approve)",
            ));
        }
        // Configuration and verified skills still converge without installing code.
        plan.installs.clear();
    } else {
        for (agent, pin) in &plan.installs {
            ctx.progress(&format!("Installing {agent} {}", pin.version));
            match installer::install(agent, pin, &plan.bin_dir, false) {
                Ok(_) => {
                    plan.approved
                        .agents
                        .insert(agent.clone(), pin.version.clone());
                }
                Err(error) => {
                    ctx.diag(Diagnostic::error(
                        "agent_install_failed",
                        format!("{agent}: {error}"),
                    ));
                    ctx.mark_failed();
                }
            }
        }
    }

    let key = plan.target.key();
    let base_state = plan
        .base
        .as_ref()
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    let mut next = base_state.clone();
    next.approved.insert(key.clone(), plan.approved.clone());
    for entry in &plan.entries {
        set_record(&mut next, &key, entry);
    }
    if !plan.has_changes() && next == base_state {
        return Ok(plan.to_json(false, false, plan.base_generation()));
    }

    // Roots never pruned when a whole file's emptied directories are removed.
    let mut keep: Vec<PathBuf> = match Roots::from_env() {
        Ok(roots) => vec![roots.home, roots.xdg_config],
        Err(_) => Vec::new(),
    };
    if let Target::Project(dir) = &plan.target {
        keep.push(dir.clone());
    }
    let mut pending = store::begin(&ctx.state_dir)?;
    let mut failed: BTreeSet<&'static str> = BTreeSet::new();
    for entry in &plan.entries {
        if entry.action() == Action::Unchanged {
            continue;
        }
        if entry.agents.iter().any(|a| failed.contains(a)) {
            restore_record(&mut next, &base_state, entry);
            continue;
        }
        let result = (|| {
            if let Some(before) = &entry.before {
                pending.backup(&entry.path, before)?;
            }
            match &entry.after {
                Some(after) => write_file(&entry.path, after, entry.private, entry.executable),
                None => {
                    std::fs::remove_file(&entry.path).map_err(|e| io_error(&entry.path, e))?;
                    // Whole files (skills) leave no empty skill directories.
                    if entry.format == FileFormat::Text {
                        remove_empty_parents(&entry.path, &keep);
                    }
                    Ok(())
                }
            }
        })();
        if let Err(err) = result {
            for agent in &entry.agents {
                failed.insert(agent);
                ctx.diag(adapter_failure(agent, agent, &err.to_string()));
                plan.agents.insert(agent, Some(err.to_string()));
            }
            restore_record(&mut next, &base_state, entry);
        }
    }
    let generation = pending.commit("sync", None, &next)?;
    ctx.progress(&format!("Applied generation {generation}"));
    Ok(plan.to_json(false, true, Some(generation)))
}

fn set_record(state: &mut State, key: &str, entry: &Entry) {
    let path = entry.path.display().to_string();
    match (&entry.after, entry.managed) {
        (Some(after), true) => {
            state.files.insert(
                path,
                Record {
                    agent: entry.agents[0].to_string(),
                    target: key.to_string(),
                    format: entry.format,
                    owned: entry.owned.clone(),
                    private: entry.private,
                    created: entry.created,
                    executable: entry.executable,
                    contents: after.clone(),
                },
            );
        }
        _ => {
            state.files.remove(&path);
        }
    }
}

fn restore_record(next: &mut State, base: &State, entry: &Entry) {
    let path = entry.path.display().to_string();
    match base.files.get(&path) {
        Some(record) => next.files.insert(path, record.clone()),
        None => next.files.remove(&path),
    };
}

/// Metadata of every generation (for history views); no file contents.
pub fn generations(state_dir: &std::path::Path) -> Result<Vec<Value>, MusterError> {
    store::summaries(state_dir)
}

/// Restore the exact managed files of generation `n`; recorded as a new
/// generation. Files Muster created after `n` are removed; files it only
/// adopted later are left as they are.
pub fn rollback(ctx: &mut Context, n: u64) -> Result<Value, MusterError> {
    let wanted = store::load(&ctx.state_dir, n)?;
    let note = "Rollback restores managed files; agent binaries are not downgraded.";
    ctx.diag(Diagnostic::new(
        Severity::Info,
        "rollback_agents_unchanged",
        note,
    ));
    let current = store::latest(&ctx.state_dir)?
        .map(|(_, s)| s)
        .unwrap_or_default();

    let mut actions: Vec<(PathBuf, Option<String>, Option<&Record>, Action)> = Vec::new();
    for (path, record) in &wanted.files {
        let path = PathBuf::from(path);
        let before = read_optional(&path)?;
        let action = match &before {
            None => Action::Add,
            Some(text) if *text == record.contents => Action::Unchanged,
            Some(_) => Action::Modify,
        };
        actions.push((path, before, Some(record), action));
    }
    for (path, record) in &current.files {
        if wanted.files.contains_key(path) {
            continue;
        }
        let path = PathBuf::from(path);
        let before = read_optional(&path)?;
        if record.created && before.is_some() {
            actions.push((path, before, None, Action::Remove));
        } else {
            ctx.diag(
                Diagnostic::new(
                    Severity::Info,
                    "rollback_kept",
                    format!(
                        "{} was not managed in generation {n} and existed before Muster; left as is",
                        path.display()
                    ),
                )
                .with_path(path.display().to_string()),
            );
        }
    }
    let files_json = |actions: &[(PathBuf, Option<String>, Option<&Record>, Action)]| {
        actions
            .iter()
            .map(|(path, _, _, action)| json!({ "path": path, "action": action }))
            .collect::<Vec<_>>()
    };
    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "rollback_to": n,
            "note": note,
            "generation": null,
            "files": files_json(&actions),
        }));
    }
    ctx.confirm(&format!("Restore the managed files of generation {n}"))?;

    let mut pending = store::begin(&ctx.state_dir)?;
    for (path, before, record, action) in &actions {
        if *action == Action::Unchanged {
            continue;
        }
        if let Some(before) = before {
            pending.backup(path, before)?;
        }
        match record {
            Some(record) => write_file(path, &record.contents, record.private, record.executable)?,
            None => std::fs::remove_file(path).map_err(|e| io_error(path, e))?,
        }
    }
    let generation = pending.commit("rollback", Some(n), &wanted)?;
    Ok(json!({
        "dry_run": false,
        "rollback_to": n,
        "note": note,
        "generation": generation,
        "files": files_json(&actions),
    }))
}

// ---------------------------------------------------------------------------
// Helpers shared with `muster agents`
// ---------------------------------------------------------------------------

/// Adapters to apply: `[agents] enabled`, else every stable adapter.
pub fn enabled_adapters(
    agents: Option<&AgentsConfig>,
) -> Result<Vec<&'static Adapter>, MusterError> {
    let Some(declared) = agents.and_then(|a| a.enabled.as_ref()) else {
        return Ok(adapters::default_enabled()
            .into_iter()
            .filter_map(adapters::find)
            .collect());
    };
    let mut out = Vec::new();
    for id in declared {
        let adapter = adapters::find(id).ok_or_else(|| MusterError::InvalidConfig {
            message: format!("unknown agent `{id}` in agents.enabled"),
            path: Some(format!("{}: agents.enabled", manifest::FILE_NAME)),
        })?;
        if adapter.available() {
            out.push(adapter);
        }
    }
    Ok(out)
}

fn single_adapter(id: &str) -> Result<&'static Adapter, MusterError> {
    let adapter = adapters::find(id).ok_or_else(|| {
        MusterError::Usage(format!(
            "unknown agent `{id}`; run `muster agents` to list adapters"
        ))
    })?;
    if !adapter.available() {
        return Err(MusterError::Unavailable(format!(
            "the {} adapter is experimental and not available yet",
            adapter.name
        )));
    }
    Ok(adapter)
}

/// Filesystem roots managed paths are relative to.
pub struct Roots {
    home: PathBuf,
    xdg_config: PathBuf,
}

impl Roots {
    pub fn from_env() -> Result<Self, MusterError> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME").ok_or_else(|| MusterError::Operational("HOME is not set".into()))?;
        let xdg_config = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        Ok(Self { home, xdg_config })
    }

    /// User-scope path of a managed file.
    pub fn resolve(&self, file: &ManagedFile) -> PathBuf {
        self.base(file.base).join(file.path)
    }

    fn base(&self, base: Base) -> &Path {
        match base {
            Base::Home => &self.home,
            Base::XdgConfig => &self.xdg_config,
        }
    }

    /// Path of `file` for `target`; `None` when the agent has no
    /// project-scoped equivalent.
    fn locate(&self, file: &ManagedFile, target: &Target) -> Option<PathBuf> {
        match target {
            Target::Global => Some(self.resolve(file)),
            Target::Project(dir) => file.project.map(|p| dir.join(p)),
        }
    }
}

/// Managed files that held resolved secrets as of the last generation (they
/// must stay 0600). Used by `doctor`.
pub fn private_files(state_dir: &Path) -> Result<Vec<PathBuf>, MusterError> {
    Ok(store::latest(state_dir)?
        .map(|(_, state)| {
            state
                .files
                .into_iter()
                .filter(|(_, record)| record.private)
                .map(|(path, _)| PathBuf::from(path))
                .collect()
        })
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn adapter_failure(id: &str, name: &str, message: &str) -> Diagnostic {
    Diagnostic::error(
        "adapter_failed",
        format!("{name} ({id}) adapter failed: {message}"),
    )
    .with_path(id)
}

fn holds_secret(text: Option<&str>, secrets: &BTreeMap<String, SecretValue>) -> bool {
    text.is_some_and(|t| {
        secrets
            .values()
            .any(|s| !s.expose().is_empty() && t.contains(s.expose()))
    })
}

fn redact(text: &str, secrets: &BTreeMap<String, SecretValue>) -> String {
    let mut out = text.to_string();
    for (reference, secret) in secrets {
        if !secret.expose().is_empty() {
            out = out.replace(secret.expose(), &format!("<redacted:{reference}>"));
        }
    }
    out
}

/// Resolve every secret reference in MCP `env`/`headers` through the provider
/// layer. A failure is a warning carrying the provider's typed code; adapters
/// then leave that key out (and warn) instead of writing the reference.
fn resolve_secrets(
    ctx: &mut Context,
    mcp: &BTreeMap<String, McpServer>,
) -> BTreeMap<String, SecretValue> {
    let mut secret_providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    let mut resolved = BTreeMap::new();
    let mut failed = BTreeSet::new();
    for (name, server) in mcp {
        for (key, value) in server.env.iter().chain(&server.headers) {
            if !secrets::is_reference(value)
                || resolved.contains_key(value)
                || failed.contains(value)
            {
                continue;
            }
            match secret_providers.resolve(value) {
                Ok(secret) => {
                    resolved.insert(value.clone(), secret);
                }
                Err(err) => {
                    failed.insert(value.clone());
                    ctx.diag(
                        Diagnostic::new(
                            Severity::Warning,
                            err.code(),
                            format!("MCP server `{name}` key `{key}`: {err}"),
                        )
                        .with_path(format!("{}: mcp.{name}", manifest::FILE_NAME)),
                    );
                }
            }
        }
    }
    resolved
}

/// Canonical AGENTS.md text. A declared but missing source is a warning
/// (instructions are skipped); an absent default source is silent.
fn read_instructions(
    ctx: &mut Context,
    merged: &layer::Merged,
) -> Result<Option<String>, MusterError> {
    let (path, declared) = merged.instructions_path()?;
    match read_optional(&path)? {
        Some(text) => Ok(Some(text)),
        None => {
            if declared {
                ctx.diag(
                    Diagnostic::new(
                        Severity::Warning,
                        "instructions_missing",
                        format!(
                            "instructions source {} does not exist; instructions were not written",
                            path.display()
                        ),
                    )
                    .with_path(path.display().to_string()),
                );
            }
            Ok(None)
        }
    }
}

/// Names of the skills, subagents and MCP servers declared in user scope (the
/// global merge). Names are listed without validation or diagnostics: the
/// `toggle_unknown_item` warning for a project run only asks whether an
/// inherited toggle names something that exists in either scope.
fn user_scope_names(merged: &layer::Merged) -> BTreeSet<(&'static str, String)> {
    let mut names = BTreeSet::new();
    if let Ok(Some(dir)) = merged.skills_path() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    if !crate::skills::is_ignored_skill_name(name, true) {
                        names.insert(("skill", name.to_string()));
                    }
                }
            }
        }
    }
    if let Ok(Some(dir)) = merged.subagents_path() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    if !name.starts_with('.') {
                        names.insert(("subagent", name.to_string()));
                    }
                }
            }
        }
    }
    for name in merged.mcp.keys() {
        names.insert(("mcp", name.clone()));
    }
    names
}

/// Files of every skill under `[agents.skills] source`: one subdirectory per
/// skill, regular files only (any depth), keyed by `/`-separated relative path.
fn read_skills(
    ctx: &mut Context,
    merged: &layer::Merged,
) -> Result<(adapters::Skills, Vec<String>), MusterError> {
    let mut skills = adapters::Skills::new();
    let mut skipped = Vec::new();
    let lock = crate::skills::lock::SkillsLock::load(&ctx.config_dir)?;
    let Some(dir) = merged.skills_path()? else {
        return Ok((skills, skipped));
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    "skills_missing",
                    format!(
                        "skills source {} does not exist; skills were not installed",
                        dir.display()
                    ),
                )
                .with_path(dir.display().to_string()),
            );
            return Ok((skills, skipped));
        }
        Err(e) => return Err(io_error(&dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_error(&dir, e))?;
        let file_type = entry.file_type().map_err(|e| io_error(&entry.path(), e))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !file_type.is_dir() || crate::skills::is_ignored_skill_name(&name, true) {
            continue;
        }
        let files = match crate::skills::read_skill_files(&entry.path()) {
            Ok(files) => files,
            Err(error) => {
                ctx.diag(
                    Diagnostic::new(
                        Severity::Warning,
                        "skill_unreadable",
                        format!("skill `{name}` could not be read: {error}; skipped, keeping the previously installed copy"),
                    )
                    .with_path(entry.path().display().to_string()),
                );
                skipped.push(name);
                continue;
            }
        };
        if files.is_empty() {
            continue;
        }
        let valid = is_valid_skill(&name, &files, ctx, &entry.path());
        let hash = crate::skills::content_hash(&files);
        let reason = match lock.get(&name) {
            None => Some(("skill_unreviewed", "has no pin in skills.lock")),
            Some(pin) if pin.content_hash != hash => Some((
                "skill_verification_failed",
                "does not match its pin in skills.lock",
            )),
            Some(_) => None,
        };
        if let Some((code, reason)) = reason {
            let hint = if code == "skill_verification_failed"
                && lock.get(&name).is_some_and(|pin| pin.is_local())
            {
                format!("; review it and run `muster skill pin {name}`")
            } else {
                String::new()
            };
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    code,
                    format!(
                        "skill `{name}` {reason}; skipped, keeping the previously installed copy{hint}"
                    ),
                )
                .with_path(entry.path().display().to_string()),
            );
            skipped.push(name);
            continue;
        }
        if !valid {
            continue;
        }
        skills.insert(name, files);
    }
    skipped.sort();
    Ok((skills, skipped))
}

fn read_optional(path: &Path) -> Result<Option<String>, MusterError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(path, e)),
    }
}

/// Every declared subagent under `[agents.subagents] source`: one Markdown
/// file per subagent (`<name>.md`), validated frontmatter, keyed by file stem.
fn read_subagents(
    ctx: &mut Context,
    merged: &layer::Merged,
) -> Result<(adapters::Subagents, Vec<String>), MusterError> {
    let mut subagents = adapters::Subagents::new();
    let mut skipped = Vec::new();
    let Some(dir) = merged.subagents_path()? else {
        return Ok((subagents, skipped));
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    "subagents_missing",
                    format!(
                        "subagents source {} does not exist; subagents were not installed",
                        dir.display()
                    ),
                )
                .with_path(dir.display().to_string()),
            );
            return Ok((subagents, skipped));
        }
        Err(e) => return Err(io_error(&dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_error(&dir, e))?;
        let path = entry.path();
        if !entry.file_type().map_err(|e| io_error(&path, e))?.is_file() {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("md") || name.starts_with('.') {
            continue;
        }
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                ctx.diag(
                    Diagnostic::new(
                        Severity::Warning,
                        "subagent_unreadable",
                        format!("subagent `{name}` could not be read: {error}; skipped"),
                    )
                    .with_path(path.display().to_string()),
                );
                skipped.push(name);
                continue;
            }
        };
        if let Some(subagent) = read_subagent(ctx, &name, &path, &source) {
            subagents.insert(name, subagent);
        }
    }
    Ok((subagents, skipped))
}

/// Parse and validate one canonical subagent file; `None` (with a
/// `subagent_invalid` warning) when it must be skipped.
fn read_subagent(
    ctx: &mut Context,
    stem: &str,
    path: &Path,
    source: &str,
) -> Option<adapters::Subagent> {
    let (frontmatter, body) = match parse_frontmatter(source) {
        ParsedFrontmatter::Missing => {
            ctx.diag(subagent_invalid(stem, path, "no YAML frontmatter"));
            return None;
        }
        ParsedFrontmatter::Invalid(why) => {
            ctx.diag(subagent_invalid(
                stem,
                path,
                &format!("invalid YAML frontmatter: {why}"),
            ));
            return None;
        }
        ParsedFrontmatter::Ok(frontmatter, body) => (frontmatter, body),
    };
    // Check the raw (untrimmed) values first: `yaml_scalar` trims, and the
    // trimmed edges would hide leading/trailing controls (VT, FF, NUL, DEL)
    // from this check while the translating adapters still emit them.
    let has_unsupported_control = |s: &str| {
        s.chars()
            .any(|c| c.is_ascii_control() && c != '\n' && c != '\r' && c != '\t')
    };
    let raw_scalar = |key: &str| yaml_raw_scalar(&frontmatter, key);
    let mut raw_values: Vec<String> = ["name", "description", "model"]
        .into_iter()
        .filter_map(raw_scalar)
        .collect();
    // Raw `tools` entries: a string form value or each list item; a field
    // that is neither is reported by `yaml_tools` below.
    if let Some(entries) = yaml_raw_tools(&frontmatter) {
        raw_values.extend(entries);
    }
    if raw_values
        .iter()
        .any(|value| has_unsupported_control(value))
    {
        ctx.diag(subagent_invalid(
            stem,
            path,
            "frontmatter contains unsupported control characters",
        ));
        return None;
    }
    let scalar = |key: &str| yaml_scalar(&frontmatter, key);
    let (Some(subagent_name), Some(description)) = (scalar("name"), scalar("description")) else {
        ctx.diag(subagent_invalid(
            stem,
            path,
            "frontmatter must have non-empty `name` and `description`",
        ));
        return None;
    };
    // The name becomes a file name under every agents directory: confine it
    // to the source directory by requiring the installed shape and an exact
    // match with the file stem.
    if !is_valid_agent_name(&subagent_name) || subagent_name != stem {
        ctx.diag(subagent_invalid(
            stem,
            path,
            &format!(
                "frontmatter `name` `{subagent_name}` is invalid: names must match \
                 `[a-z0-9][a-z0-9._-]*` (no `/`, `\\`, `..` or leading dot) and equal the file stem"
            ),
        ));
        return None;
    }
    let tools = match yaml_tools(&frontmatter) {
        Ok(tools) => tools,
        Err(why) => {
            ctx.diag(subagent_invalid(stem, path, &why));
            return None;
        }
    };
    // Everything the canonical format does not define is reported by the
    // translating adapters (the byte-exact ones install the whole file).
    let extra_fields = frontmatter
        .keys()
        .filter_map(|key| key.as_str().map(str::to_owned))
        .filter(|key| !matches!(key.as_str(), "name" | "description" | "tools" | "model"))
        .collect();
    Some(adapters::Subagent {
        name: subagent_name,
        description,
        tools,
        model: scalar("model"),
        extra_fields,
        body,
        source: source.to_owned(),
    })
}

/// A subagent name is also its installed file name: lowercase letters and
/// digits first, then lowercase letters, digits, dots, underscores or
/// hyphens. `/`, `\` and any `..` are rejected so a frontmatter name can
/// never escape the agents directory.
fn is_valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return false,
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        && !name.contains("..")
}

/// Raw scalar text for a frontmatter key: strings verbatim (untrimmed),
/// numbers and booleans as written. `None` when the key is absent or not a
/// scalar.
fn yaml_raw_scalar(frontmatter: &Hash, key: &str) -> Option<String> {
    match frontmatter.get(&Yaml::String(key.into()))? {
        Yaml::String(text) => Some(text.clone()),
        Yaml::Integer(number) => Some(number.to_string()),
        Yaml::Real(number) => Some(number.clone()),
        Yaml::Boolean(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Trimmed scalar text for a frontmatter key: strings verbatim, numbers and
/// booleans as written. `None` when the key is absent, empty or not a scalar.
fn yaml_scalar(frontmatter: &Hash, key: &str) -> Option<String> {
    let text = yaml_raw_scalar(frontmatter, key)?;
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Raw (untrimmed) `tools` values in string or list form for the raw control
/// check: the whole string, or each list item. `None` when the field is
/// neither, which `yaml_tools` reports as an error.
fn yaml_raw_tools(frontmatter: &Hash) -> Option<Vec<String>> {
    match frontmatter.get(&Yaml::String("tools".into())) {
        None | Some(Yaml::Null) => Some(Vec::new()),
        Some(Yaml::String(raw)) => Some(vec![raw.clone()]),
        Some(Yaml::Array(items)) => items
            .iter()
            .map(|item| match item {
                Yaml::String(text) => Some(text.clone()),
                _ => None,
            })
            .collect(),
        Some(_) => None,
    }
}

/// `tools` in string (`Read, Grep`) or list form; an error means the field
/// is neither, which invalidates the subagent.
fn yaml_tools(frontmatter: &Hash) -> Result<Vec<String>, String> {
    match frontmatter.get(&Yaml::String("tools".into())) {
        None | Some(Yaml::Null) => Ok(Vec::new()),
        Some(Yaml::String(raw)) => Ok(raw
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .collect()),
        Some(Yaml::Array(items)) => {
            let mut tools = Vec::new();
            for item in items {
                match yaml_scalar_item(item) {
                    Some(tool) => tools.push(tool),
                    None => return Err("`tools` must be a list of strings".into()),
                }
            }
            Ok(tools)
        }
        Some(_) => Err("`tools` must be a string or a list of strings".into()),
    }
}

fn yaml_scalar_item(item: &Yaml) -> Option<String> {
    match item {
        Yaml::String(text) => {
            let text = text.trim().to_owned();
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn subagent_invalid(name: &str, path: &Path, why: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Warning,
        "subagent_invalid",
        format!("subagent `{name}`: {why}; skipped"),
    )
    .with_path(path.display().to_string())
}

/// Frontmatter outcome for a Markdown source file.
enum ParsedFrontmatter {
    /// No `---` block at the start of the file.
    Missing,
    /// A block is present but is not a YAML mapping (or fails to parse).
    Invalid(String),
    /// The parsed frontmatter mapping and the Markdown body after it.
    Ok(Hash, String),
}

/// Split YAML frontmatter from the Markdown body: the frontmatter is the
/// mapping between the opening `---` and the closing `---`, parsed with
/// `yaml-rust2` (so block scalars, lists and quoting work); the body is
/// everything after it. `Missing` when there is no frontmatter block.
fn parse_frontmatter(text: &str) -> ParsedFrontmatter {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return ParsedFrontmatter::Missing;
    };
    let Some(rest) = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    else {
        return ParsedFrontmatter::Missing;
    };
    let Some(end) = rest.find("\n---") else {
        return ParsedFrontmatter::Missing;
    };
    let (block, body) = rest.split_at(end);
    let body = body[4..]
        .strip_prefix('\n')
        .unwrap_or(&body[4..])
        .to_owned();
    match YamlLoader::load_from_str(block) {
        Ok(docs) if docs.len() <= 1 => match docs.into_iter().next().unwrap_or(Yaml::Null) {
            Yaml::Hash(frontmatter) => ParsedFrontmatter::Ok(frontmatter, body),
            Yaml::Null => ParsedFrontmatter::Ok(Hash::new(), body),
            _ => ParsedFrontmatter::Invalid("expected a YAML mapping".into()),
        },
        Ok(_) => ParsedFrontmatter::Invalid("expected a single YAML document".into()),
        Err(e) => ParsedFrontmatter::Invalid(e.to_string()),
    }
}
/// Write via a sibling temp file + rename, keeping the existing file mode, or
/// forcing 0600 when `private` (the file holds resolved secrets). `executable`
/// adds the execute bits (0755, or 0700 when private).
fn write_file(
    path: &Path,
    contents: &str,
    private: bool,
    executable: bool,
) -> Result<(), MusterError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".muster-tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        // The plaintext never exists with looser permissions, even briefly.
        let mode = if executable { 0o700 } else { 0o600 };
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    }
    let mut file = options.open(&tmp).map_err(|e| io_error(&tmp, e))?;
    std::io::Write::write_all(&mut file, contents.as_bytes()).map_err(|e| io_error(&tmp, e))?;
    drop(file);
    if !private {
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, meta.permissions()).map_err(|e| io_error(&tmp, e))?;
        }
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&tmp).map_err(|e| io_error(&tmp, e))?;
        let mode = meta.permissions().mode();
        let exec = if private { 0o100 } else { 0o111 };
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode | exec))
            .map_err(|e| io_error(&tmp, e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_error(path, e)
    })
}

fn io_error(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}

/// Remove the directories above `path` that are left empty, stopping below a
/// root in `keep` (and never removing a root's direct child, like `~/.gemini`).
fn remove_empty_parents(path: &Path, keep: &[PathBuf]) {
    let mut dir = path.parent();
    while let Some(current) = dir {
        let Some(parent) = current.parent() else {
            break;
        };
        if keep.iter().any(|k| k == current || k == parent) {
            break;
        }
        // Fails (and stops) as soon as a directory still holds anything.
        if std::fs::remove_dir(current).is_err() {
            break;
        }
        dir = Some(parent);
    }
}

fn is_valid_skill(
    name: &str,
    files: &BTreeMap<String, String>,
    ctx: &mut Context,
    skill_dir: &Path,
) -> bool {
    let Some(skill_md) = files.get("SKILL.md") else {
        ctx.diag(
            Diagnostic::new(
                Severity::Warning,
                "skill_invalid",
                format!("skill `{name}` has no SKILL.md; skipped"),
            )
            .with_path(skill_dir.display().to_string()),
        );
        return false;
    };

    if !has_valid_frontmatter(skill_md) {
        ctx.diag(
            Diagnostic::new(
                Severity::Warning,
                "skill_invalid",
                format!(
                    "skill `{name}` SKILL.md must have YAML frontmatter with non-empty `name` and `description`; skipped"
                ),
            )
            .with_path(skill_dir.join("SKILL.md").display().to_string()),
        );
        return false;
    }

    true
}

fn has_valid_frontmatter(content: &str) -> bool {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return false;
    }
    let after_start = &trimmed[3..];
    let after_newline = if let Some(rest) = after_start.strip_prefix("\r\n") {
        rest
    } else if let Some(rest) = after_start.strip_prefix('\n') {
        rest
    } else {
        return false;
    };

    let Some(end_idx) = after_newline.find("\n---") else {
        return false;
    };
    let fm_block = &after_newline[..end_idx];

    let mut has_name = false;
    let mut has_desc = false;

    for line in fm_block.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim().trim_matches(|c| c == '\'' || c == '"').trim();
            if !val.is_empty() {
                if key == "name" {
                    has_name = true;
                } else if key == "description" {
                    has_desc = true;
                }
            }
        }
    }

    has_name && has_desc
}
