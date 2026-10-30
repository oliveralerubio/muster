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
    params: &[],
    mutating: false,
    handler: effective,
};

fn effective(ctx: &mut Context, _: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let layers = layer::global_layers(
        &ctx.config_dir,
        &manifest,
        crate::machine::load_name(&ctx.state_dir).as_deref(),
    );
    let merged = layer::merge(&layers);
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
            for adapter in engine::enabled_adapters(Some(&merged.agents))? {
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
                for adapter in engine::enabled_adapters(Some(&merged.agents))? {
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
    Ok(
        json!({"extra_on_disk": extra_on_disk, "skills": skill_views, "layers": layers.iter().map(|l| json!({"kind": l.kind, "name": l.name})).collect::<Vec<_>>(), "instructions": instructions}),
    )
}
