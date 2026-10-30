//! Read-only view of the configuration applied to this machine.

use crate::adapters::{self, Current, Desired};
use crate::engine::{self, Roots};
use crate::secrets::SecretAccess;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::context::Context;
use crate::engine::layer;
use crate::errors::MusterError;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "effective",
    about: "Show the effective configuration and its sources",
    params: &[crate::commands::plan::PROJECT_PARAM, crate::registry::ParamSpec {
        name: "agent", about: "Restrict output to one adapter", kind: crate::registry::ParamKind::String,
        required: false, positional: false,
    }],
    mutating: false,
    handler: effective,
};

fn effective(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let mut layers = layer::global_layers(
        &ctx.config_dir,
        &manifest,
        crate::machine::load_name(&ctx.state_dir).as_deref(),
    );
    if let engine::Target::Project(project) = crate::commands::plan::target(params)? {
        layers.extend(layer::project_layers(&project)?);
    }
    let merged = layer::merge(&layers);
    let selected = if let Some(id) = params.get_string("agent") {
        let adapter = adapters::find(id).ok_or_else(|| MusterError::Usage(format!("unknown agent `{id}`")))?;
        if !adapter.available() { return Err(MusterError::Unavailable(format!("agent `{id}` is unavailable"))); }
        vec![adapter]
    } else { engine::enabled_adapters(Some(&merged.agents))? };
    let (path, _) = merged.instructions_path()?;
    let source = layers
        .iter()
        .rev()
        .find(|l| l.agents.instructions.is_some())
        .or_else(|| layers.first());
    let instructions = match std::fs::read(&path) {
        Ok(bytes) => {
            let text =
                String::from_utf8(bytes).map_err(|e| MusterError::Operational(e.to_string()))?;
            let empty_agents = Default::default();
            let empty = BTreeMap::new();
            let providers = BTreeMap::new();
            let secrets = BTreeMap::new();
            let skills = BTreeMap::new();
            let subagents = BTreeMap::new();
            let access = SecretAccess::new(BTreeMap::new(), None);
            let desired = Desired {
                config_dir: &ctx.config_dir,
                agents: &empty_agents,
                ai_providers: &providers,
                mcp: &empty,
                instructions: Some(&text),
                secrets: &secrets,
                access: &access,
                skills: &skills,
                subagents: &subagents,
            };
            let roots = Roots::from_env()?;
            let mut views = BTreeMap::new();
            for &adapter in &selected {
                if merged
                    .agents
                    .skip_instructions
                    .as_ref()
                    .is_some_and(|ids| ids.iter().any(|id| id == adapter.id))
                {
                    views.insert(adapter.id, json!({"state": "skipped", "files": []}));
                    continue;
                }
                let mut current = Current::new();
                for file in adapter.files {
                    if let Ok(content) = std::fs::read_to_string(roots.resolve(file)) {
                        current.insert(file.path, content);
                    }
                }
                let rendered = adapter.render(&desired, &current)?;
                let files: Vec<_> = rendered
                    .files
                    .iter()
                    .filter(|f| f.file.format == adapters::FileFormat::Markdown)
                    .map(|f| {
                        let path = roots.resolve(f.file);
                        let state = match current.get(f.file.path) {
                            None => "missing",
                            Some(old) if old == &f.contents => "in_sync",
                            Some(_) => "drifted",
                        };
                        json!({"path": path, "state": state})
                    })
                    .collect();
                let state = if files.iter().any(|f| f["state"] == "missing") {
                    "missing"
                } else if files.iter().any(|f| f["state"] == "drifted") {
                    "drifted"
                } else {
                    "in_sync"
                };
                views.insert(adapter.id, json!({"state": state, "files": files}));
            }
            vec![
                json!({"source": path, "layer": source.map(|l| l.kind), "approx_tokens": text.len() / 4, "adapters": views}),
            ]
        }
        Err(_) => vec![],
    };
    let roots = Roots::from_env()?;
    let mut mcp_views = Vec::new();
    for (name, server) in &merged.mcp {
        let transport = match server.transport().map_err(MusterError::Operational)? {
            crate::adapters::config::Transport::Stdio { .. } => "stdio",
            crate::adapters::config::Transport::Remote { .. } => "remote",
        };
        let sources: Vec<_> = layers.iter().filter(|l| l.mcp.contains_key(name)).collect();
        let mut views = BTreeMap::new();
        for &adapter in &selected {
            let mut current = Current::new();
            for file in adapter.files {
                if let Ok(content) = std::fs::read_to_string(roots.resolve(file)) {
                    current.insert(file.path, content);
                }
            }
            let empty_agents = Default::default();
            let providers = BTreeMap::new();
            let mcp = BTreeMap::from([(name.clone(), server.clone())]);
            let secrets = BTreeMap::new();
            let skills = BTreeMap::new();
            let subagents = BTreeMap::new();
            let access = SecretAccess::new(BTreeMap::new(), None);
            let desired = Desired { config_dir: &ctx.config_dir, agents: &empty_agents,
                ai_providers: &providers, mcp: &mcp, instructions: None, secrets: &secrets,
                access: &access, skills: &skills, subagents: &subagents };
            let rendered = adapter.render(&desired, &current)?;
            let empty = BTreeMap::new();
            let baseline = adapter.render(&Desired { mcp: &empty, ..desired }, &current)?;
            let changed: Vec<_> = rendered.files.iter().filter(|file| {
                baseline.files.iter().find(|base| base.file.path == file.file.path)
                    .is_none_or(|base| base.contents != file.contents)
            }).collect();
            if let Some(file) = changed.first() {
                let state = match current.get(file.file.path) {
                    None => "missing",
                    Some(_) => "drifted",
                };
                views.insert(adapter.id, json!({"state": state}));
            } else {
                // Render against an empty current file to identify the MCP output.
                let rendered = adapter.render(&desired, &Current::new())?;
                let baseline = adapter.render(&Desired { mcp: &empty, ..desired }, &Current::new())?;
                if rendered.files.iter().any(|file| baseline.files.iter()
                    .find(|base| base.file.path == file.file.path)
                    .is_none_or(|base| base.contents != file.contents)) {
                    views.insert(adapter.id, json!({"state": "in_sync"}));
                }
            }
        }
        let agents: Vec<_> = views.keys().copied().collect();
        let references: Vec<_> = server.env.values().chain(server.headers.values())
            .filter(|v| adapters::is_secret_reference(v)).collect();
        mcp_views.push(json!({"name": name, "layer": sources.last().map(|l| l.kind),
            "overridden_layers": sources.iter().take(sources.len().saturating_sub(1)).map(|l| l.kind).collect::<Vec<_>>(),
            "transport": transport, "agents": agents, "adapters": views, "secret_references": references}));
    }
    let mut subagent_views = Vec::new();
    if let Some(dir) = merged.subagents_path()? {
        let mut entries: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") { continue; }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue; };
            let Ok(source) = std::fs::read_to_string(&path) else { continue; };
            let Some(rest) = source.strip_prefix("---\n") else { continue; };
            let Some((front, body)) = rest.split_once("\n---\n") else { continue; };
            let Ok(docs) = yaml_rust2::YamlLoader::load_from_str(front) else { continue; };
            let Some(yaml) = docs.first() else { continue; };
            let Some(description) = yaml["description"].as_str() else { continue; };
            if yaml["name"].as_str() != Some(name) { continue; }
            let tools = match &yaml["tools"] {
                yaml_rust2::Yaml::Array(items) => items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
                yaml_rust2::Yaml::String(s) => s.split(',').map(|s| s.trim().to_owned()).collect(),
                _ => Vec::new(),
            };
            let subagent = adapters::Subagent { name: name.to_owned(), description: description.to_owned(),
                tools, model: yaml["model"].as_str().map(str::to_owned), extra_fields: Vec::new(),
                body: body.to_owned(), source };
            let subagents = BTreeMap::from([(name.to_owned(), subagent)]);
            let empty_agents = Default::default();
            let providers = BTreeMap::new();
            let mcp = BTreeMap::new();
            let secrets = BTreeMap::new();
            let skills = BTreeMap::new();
            let access = SecretAccess::new(BTreeMap::new(), None);
            let desired = Desired { config_dir: &ctx.config_dir, agents: &empty_agents, ai_providers: &providers,
                mcp: &mcp, instructions: None, secrets: &secrets, access: &access, skills: &skills, subagents: &subagents };
            let mut views = BTreeMap::new();
            for &adapter in &selected {
                let rendered = adapter.render(&desired, &Current::new())?;
                if let Some(file) = rendered.tree.first() {
                    let path = roots.base(file.base).join(&file.path);
                    let state = match std::fs::read_to_string(path) {
                        Err(_) => "missing", Ok(current) if current == file.contents => "in_sync", Ok(_) => "drifted"
                    };
                    views.insert(adapter.id, json!({"state": state}));
                }
            }
            let layer = layers.iter().rev().find(|l| l.agents.subagents.is_some()).map(|l| l.kind);
            subagent_views.push(json!({"name": name, "layer": layer, "adapters": views}));
        }
    }
    let mut skill_views = Vec::new();
    let mut skill_dirs = BTreeMap::new();
    let listed = (crate::commands::skill::LIST.handler)(ctx, &Params::default())?;
    let lock = crate::skills::SkillsLock::load(&ctx.config_dir)?;
    if let Some(dir) = merged.skills_path()? {
        if let Some(items) = listed["skills"].as_array() {
            for item in items {
                let name = item["name"].as_str().unwrap_or_default();
                let files = crate::skills::read_skill_files(&dir.join(name));
                let mut view = item.clone();
                let (verification, description_tokens, body_tokens, data) = match files {
                    Err(_) => ("unreadable", 0, 0, BTreeMap::new()),
                    Ok(files) => {
                        let md = files.get("SKILL.md");
                        let meta = md.and_then(|s| crate::skills::parse_frontmatter(s));
                        let valid = meta.as_ref().is_some_and(|m| {
                            m.name.as_deref() == Some(name) && m.description.is_some()
                        });
                        let verification = if !valid {
                            "invalid"
                        } else if let Some(pin) = lock.get(name) {
                            if pin.content_hash == crate::skills::content_hash(&files) {
                                "verified"
                            } else {
                                "hash_mismatch"
                            }
                        } else {
                            "unreviewed"
                        };
                        (
                            verification,
                            meta.and_then(|m| m.description).map_or(0, |s| s.len() / 4),
                            md.map_or(0, |s| s.len() / 4),
                            files,
                        )
                    }
                };
                let mut adapter_views = BTreeMap::new();
                for &adapter in &selected {
                    let skill_map = BTreeMap::from([(name.to_owned(), data.clone())]);
                    let empty_agents = Default::default();
                    let providers = BTreeMap::new();
                    let mcp = BTreeMap::new();
                    let secrets = BTreeMap::new();
                    let subagents = BTreeMap::new();
                    let access = SecretAccess::new(BTreeMap::new(), None);
                    let desired = Desired {
                        config_dir: &ctx.config_dir,
                        agents: &empty_agents,
                        ai_providers: &providers,
                        mcp: &mcp,
                        instructions: None,
                        secrets: &secrets,
                        access: &access,
                        skills: &skill_map,
                        subagents: &subagents,
                    };
                    let rendered = adapter.render(&desired, &Current::new())?;
                    let roots = Roots::from_env()?;
                    let paths: Vec<_> = rendered
                        .tree
                        .iter()
                        .map(|f| (roots.base(f.base).join(&f.path), &f.contents))
                        .collect();
                    let state = if paths.is_empty() || paths.iter().any(|(p, _)| !p.exists()) {
                        "missing"
                    } else if paths
                        .iter()
                        .any(|(p, c)| std::fs::read_to_string(p).ok().as_ref() != Some(*c))
                    {
                        "drifted"
                    } else {
                        "installed"
                    };
                    let path = paths
                        .first()
                        .and_then(|(p, _)| p.parent())
                        .map(|p| p.to_path_buf());
                    if let Some(dir) = path.as_ref().and_then(|p| p.parent()) {
                        skill_dirs.insert(adapter.id, dir.to_path_buf());
                    }
                    adapter_views.insert(adapter.id, json!({"path": path, "state": state}));
                }
                view["verification"] = json!(verification);
                view["description_tokens"] = json!(description_tokens);
                view["body_tokens"] = json!(body_tokens);
                view["adapters"] = json!(adapter_views);
                skill_views.push(view);
            }
        }
    }
    let names: std::collections::BTreeSet<_> = skill_views.iter()
        .filter_map(|v| v["name"].as_str()).collect();
    let extra_on_disk: BTreeMap<_, Vec<_>> = skill_dirs.into_iter().map(|(id, dir)| {
        let mut extra: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten()
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| !names.contains(name.as_str())).collect();
        extra.sort();
        (id, extra)
    }).collect();
    let totals: BTreeMap<_, u64> = selected.iter().map(|adapter| {
        let instruction_tokens: u64 = instructions.iter().filter(|v| v["adapters"][adapter.id]["state"] != "skipped")
            .filter_map(|v| v["approx_tokens"].as_u64()).sum();
        let skill_tokens: u64 = skill_views.iter().filter(|v| !v["adapters"][adapter.id].is_null())
            .filter_map(|v| v["description_tokens"].as_u64()).sum();
        (adapter.id, instruction_tokens + skill_tokens)
    }).collect();
    Ok(
        json!({"totals": totals, "subagents": subagent_views, "mcp": mcp_views, "extra_on_disk": extra_on_disk, "skills": skill_views, "layers": layers.iter().map(|l| json!({"kind": l.kind, "name": l.name})).collect::<Vec<_>>(), "instructions": instructions}),
    )
}
