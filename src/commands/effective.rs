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
    params: &[
        crate::commands::plan::PROJECT_PARAM,
        crate::registry::ParamSpec {
            name: "agent",
            about: "Restrict output to one adapter",
            kind: crate::registry::ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: false,
    handler: effective,
};

fn effective(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let layers = layer::global_layers(
        &ctx.config_dir,
        &manifest,
        crate::machine::load_name(&ctx.state_dir).as_deref(),
    );
    let mut result = scope(ctx, params, &layers, None, None)?;
    if let engine::Target::Project(project) = crate::commands::plan::target(params)? {
        let project_layers = layer::project_layers(&project)?;
        let global_enabled = layer::merge(&layers).agents.enabled;
        let project_result = scope(ctx, params, &project_layers, Some(&project), global_enabled)?;
        // The project layer has the highest precedence: its same-named MCP
        // servers replace the lower layers' entries, which are reported as
        // overridden instead of duplicated.
        let project_mcp: Vec<Value> = project_result["mcp"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let replaced: std::collections::BTreeSet<&str> = project_mcp
            .iter()
            .filter_map(|entry| entry["name"].as_str())
            .collect();
        let mut overridden: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if let Some(entries) = result["mcp"].as_array_mut() {
            entries.retain(|entry| {
                let Some(name) = entry["name"].as_str() else {
                    return true;
                };
                if !replaced.contains(name) {
                    return true;
                }
                let mut layers: Vec<String> = entry["overridden_layers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                if let Some(kind) = entry["layer"].as_str() {
                    layers.push(kind.to_owned());
                }
                overridden.insert(name.to_owned(), layers);
                false
            });
        }
        if let Some(entries) = result["mcp"].as_array_mut() {
            for mut entry in project_mcp {
                if let Some(name) = entry["name"].as_str().map(str::to_owned) {
                    if let Some(lower) = overridden.get(&name) {
                        let mut kinds = lower.clone();
                        kinds.extend(
                            entry["overridden_layers"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned),
                        );
                        entry["overridden_layers"] = json!(ordered_layers(kinds));
                    }
                }
                entries.push(entry);
            }
        }
        for key in ["layers", "instructions", "skills", "subagents"] {
            if let (Some(target), Some(source)) =
                (result[key].as_array_mut(), project_result[key].as_array())
            {
                target.extend(source.iter().cloned());
            }
        }
        // Unmanaged extras from both scopes, merged and deduplicated per adapter.
        if let (Some(target), Some(source)) = (
            result["extra_on_disk"].as_object_mut(),
            project_result["extra_on_disk"].as_object(),
        ) {
            for (id, names) in source {
                let mut merged: Vec<String> = target
                    .get(id)
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                for name in names
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if !merged.iter().any(|existing| existing == name) {
                        merged.push(name.to_owned());
                    }
                }
                merged.sort();
                target.insert(id.clone(), json!(merged));
            }
        }
        if let (Some(totals), Some(project_totals)) = (
            result["totals"].as_object_mut(),
            project_result["totals"].as_object(),
        ) {
            for (id, value) in project_totals {
                let sum = totals.get(id).and_then(Value::as_u64).unwrap_or(0)
                    + value.as_u64().unwrap_or(0);
                totals.insert(id.clone(), json!(sum));
            }
        }
    }
    if ctx.format == crate::output::Format::Text {
        Ok(Value::String(format_text(&result)))
    } else {
        Ok(result)
    }
}

fn scope(
    ctx: &mut Context,
    params: &Params,
    layers: &[layer::Layer],
    project: Option<&std::path::Path>,
    fallback_enabled: Option<Vec<String>>,
) -> Result<Value, MusterError> {
    let mut merged = layer::merge(layers);
    if merged.agents.enabled.is_none() {
        merged.agents.enabled = fallback_enabled;
    }
    let selected = if let Some(id) = params.get_string("agent") {
        let adapter = adapters::find(id)
            .ok_or_else(|| MusterError::Usage(format!("unknown agent `{id}`")))?;
        if !adapter.available() {
            return Err(MusterError::Unavailable(format!(
                "agent `{id}` is unavailable"
            )));
        }
        vec![adapter]
    } else {
        engine::enabled_adapters(Some(&merged.agents))?
    };
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
                    if let Ok(content) = std::fs::read_to_string(locate_file(&roots, file, project))
                    {
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
        let env_secrets = secret_keys(&server.env);
        let header_secrets = secret_keys(&server.headers);
        let mut views = BTreeMap::new();
        for &adapter in &selected {
            if merged.disabled("mcp", name, adapter.id).is_some() {
                views.insert(adapter.id, json!({"state": "disabled"}));
                continue;
            }
            let mut current = Current::new();
            for file in adapter.files {
                if let Ok(content) = std::fs::read_to_string(locate_file(&roots, file, project)) {
                    current.insert(
                        file.path,
                        scrub_mcp_secrets(&content, name, &env_secrets, &header_secrets),
                    );
                }
            }
            let empty_agents = Default::default();
            let providers = BTreeMap::new();
            let mcp = BTreeMap::from([(name.clone(), server.clone())]);
            let secrets = BTreeMap::new();
            let skills = BTreeMap::new();
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
                skills: &skills,
                subagents: &subagents,
            };
            let rendered = adapter.render(&desired, &current)?;
            let empty = BTreeMap::new();
            let baseline = adapter.render(
                &Desired {
                    mcp: &empty,
                    ..desired
                },
                &current,
            )?;
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
                    None => "missing",
                    Some(_) => "drifted",
                };
                views.insert(adapter.id, json!({"state": state}));
            } else {
                // Render against an empty current file to identify the MCP output.
                let rendered = adapter.render(&desired, &Current::new())?;
                let baseline = adapter.render(
                    &Desired {
                        mcp: &empty,
                        ..desired
                    },
                    &Current::new(),
                )?;
                if rendered.files.iter().any(|file| {
                    baseline
                        .files
                        .iter()
                        .find(|base| base.file.path == file.file.path)
                        .is_none_or(|base| base.contents != file.contents)
                }) {
                    views.insert(adapter.id, json!({"state": "in_sync"}));
                }
            }
        }
        let agents: Vec<_> = views.keys().copied().collect();
        let references: Vec<_> = server
            .env
            .values()
            .chain(server.headers.values())
            .filter(|v| adapters::is_secret_reference(v))
            .collect();
        mcp_views.push(json!({"name": name, "layer": sources.last().map(|l| l.kind),
            "overridden_layers": sources.iter().take(sources.len().saturating_sub(1)).map(|l| l.kind).collect::<Vec<_>>(),
            "transport": transport, "agents": agents, "adapters": views, "secret_references": references}));
    }
    let mut subagent_views = Vec::new();
    if let Some(dir) = merged.subagents_path()? {
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
            let empty_agents = Default::default();
            let providers = BTreeMap::new();
            let mcp = BTreeMap::new();
            let secrets = BTreeMap::new();
            let skills = BTreeMap::new();
            let access = SecretAccess::new(BTreeMap::new(), None);
            let desired = Desired {
                config_dir: &ctx.config_dir,
                agents: &empty_agents,
                ai_providers: &providers,
                mcp: &mcp,
                instructions: None,
                secrets: &secrets,
                access: &access,
                skills: &skills,
                subagents: &subagents,
            };
            let mut views = BTreeMap::new();
            for &adapter in &selected {
                if merged.disabled("subagent", name, adapter.id).is_some() {
                    views.insert(adapter.id, json!({"state": "disabled"}));
                    continue;
                }
                let rendered = adapter.render(&desired, &Current::new())?;
                if let Some(file) = rendered.tree.first() {
                    let path = locate_tree(&roots, file, project);
                    let state = match std::fs::read_to_string(path) {
                        Err(_) => "missing",
                        Ok(current) if current == file.contents => "in_sync",
                        Ok(_) => "drifted",
                    };
                    views.insert(adapter.id, json!({"state": state}));
                }
            }
            let layer = layers
                .iter()
                .rev()
                .find(|l| l.agents.subagents.is_some())
                .map(|l| l.kind);
            subagent_views.push(json!({"name": name, "layer": layer, "adapters": views}));
        }
    }
    let mut skill_views = Vec::new();
    let mut skill_dirs = skill_roots(&ctx.config_dir, &selected, &roots, project)?;
    let skill_layer = layers
        .iter()
        .rev()
        .find(|l| l.agents.skills.is_some())
        .map(|l| l.kind);
    let listed = if project.is_some() {
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
        (crate::commands::skill::LIST.handler)(ctx, &Params::default())?
    };
    let lock = crate::skills::SkillsLock::load(&ctx.config_dir)?;
    if let Some(dir) = merged.skills_path()? {
        if let Some(items) = listed["skills"].as_array() {
            for item in items {
                let name = item["name"].as_str().unwrap_or_default();
                let files = crate::skills::read_skill_files(&dir.join(name));
                let customized =
                    crate::skills::is_customized(name, &merged.overlay_roots, &merged.customize);
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
                        .map(|f| (locate_tree(&roots, f, project), &f.contents))
                        .collect();
                    let state = if merged.disabled("skill", name, adapter.id).is_some() {
                        // Toggles remove the skill from this adapter, so its
                        // absence on disk is the desired state, not drift.
                        "disabled"
                    } else if paths.is_empty() || paths.iter().any(|(p, _)| !p.exists()) {
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
                view["customized"] = json!(customized);
                view["verification"] = json!(verification);
                view["description_tokens"] = json!(description_tokens);
                view["body_tokens"] = json!(body_tokens);
                view["layer"] = json!(skill_layer);
                view["adapters"] = json!(adapter_views);
                skill_views.push(view);
            }
        }
    }
    let names: std::collections::BTreeSet<_> = skill_views
        .iter()
        .filter_map(|v| v["name"].as_str())
        .collect();
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
    let totals: BTreeMap<_, u64> = selected
        .iter()
        .map(|adapter| {
            let instruction_tokens: u64 = instructions
                .iter()
                .filter(|v| v["adapters"][adapter.id]["state"] != "skipped")
                .filter_map(|v| v["approx_tokens"].as_u64())
                .sum();
            let skill_tokens: u64 = skill_views
                .iter()
                .filter(|v| !v["adapters"][adapter.id].is_null())
                .filter(|v| v["adapters"][adapter.id]["state"] != "disabled")
                .filter_map(|v| v["description_tokens"].as_u64())
                .sum();
            (adapter.id, instruction_tokens + skill_tokens)
        })
        .collect();
    let result = json!({"totals": totals, "subagents": subagent_views, "mcp": mcp_views, "extra_on_disk": extra_on_disk, "skills": skill_views, "layers": layers.iter().map(|l| json!({"kind": l.kind, "name": l.name})).collect::<Vec<_>>(), "instructions": instructions});
    Ok(result)
}

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

/// Layer kinds in fixed precedence order, deduplicated.
fn ordered_layers(kinds: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for kind in layer::PRECEDENCE {
        if let Some(found) = kinds.iter().find(|k| k.as_str() == kind) {
            out.push(found.clone());
        }
    }
    for kind in &kinds {
        if !layer::PRECEDENCE.contains(&kind.as_str()) && !out.contains(kind) {
            out.push(kind.clone());
        }
    }
    out
}

fn format_text(result: &Value) -> String {
    let mut text = String::new();
    text.push_str("Layers\n");
    if let Some(layers) = result["layers"].as_array() {
        for layer in layers {
            text.push_str(&format!(
                "  {}: {}\n",
                layer["kind"].as_str().unwrap_or(""),
                layer["name"].as_str().unwrap_or("")
            ));
        }
    }
    for (heading, key) in [
        ("Instructions", "instructions"),
        ("Skills", "skills"),
        ("MCP", "mcp"),
        ("Subagents", "subagents"),
    ] {
        text.push_str(&format!("{heading}\n"));
        let Some(items) = result[key].as_array() else {
            continue;
        };
        for item in items {
            let name = item["name"]
                .as_str()
                .or_else(|| item["source"].as_str())
                .unwrap_or("");
            // Provenance: the supplying layer plus per-kind detail.
            let mut parts: Vec<String> = Vec::new();
            if let Some(kind) = item["layer"].as_str() {
                parts.push(kind.to_owned());
            }
            match key {
                "instructions" => {
                    if let Some(tokens) = item["approx_tokens"].as_u64() {
                        parts.push(format!("~{tokens} tokens"));
                    }
                }
                "skills" => {
                    parts.push(
                        item["verification"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_owned(),
                    );
                    parts.push(format!(
                        "~{} description tokens",
                        item["description_tokens"].as_u64().unwrap_or(0)
                    ));
                    parts.push(format!(
                        "~{} body tokens",
                        item["body_tokens"].as_u64().unwrap_or(0)
                    ));
                }
                "mcp" => {
                    if let Some(transport) = item["transport"].as_str() {
                        parts.push(transport.to_owned());
                    }
                    let overridden: Vec<_> = item["overridden_layers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    if !overridden.is_empty() {
                        parts.push(format!("overrides {}", overridden.join(", ")));
                    }
                }
                _ => {}
            }
            let provenance = if parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", parts.join(", "))
            };
            text.push_str(&format!("  {name}{provenance}\n"));
            if let Some(adapters) = item["adapters"].as_object() {
                for (id, view) in adapters {
                    text.push_str(&format!(
                        "    {id}: {}\n",
                        view["state"].as_str().unwrap_or("unknown")
                    ));
                }
            }
        }
    }
    text.push_str("Totals\n");
    for (id, tokens) in result["totals"].as_object().unwrap() {
        text.push_str(&format!("  {id}: {tokens} tokens\n"));
    }
    text
}

fn locate_file(
    roots: &Roots,
    file: &adapters::ManagedFile,
    project: Option<&std::path::Path>,
) -> std::path::PathBuf {
    project
        .and_then(|p| file.project.map(|path| p.join(path)))
        .unwrap_or_else(|| roots.resolve(file))
}
/// Skills directory of each adapter in user scope (or in `project`), found by
/// rendering a sentinel skill and taking the parent of its path. Adapters
/// without a skills directory in that scope are absent.
pub fn skill_roots(
    config_dir: &std::path::Path,
    adapters: &[&'static adapters::Adapter],
    roots: &Roots,
    project: Option<&std::path::Path>,
) -> Result<BTreeMap<&'static str, std::path::PathBuf>, MusterError> {
    let sentinel = BTreeMap::from([(
        "__muster_probe__".to_owned(),
        BTreeMap::from([("SKILL.md".to_owned(), String::new())]),
    )]);
    let empty_agents = Default::default();
    let providers = BTreeMap::new();
    let mcp = BTreeMap::new();
    let secrets = BTreeMap::new();
    let subagents = BTreeMap::new();
    let access = SecretAccess::new(BTreeMap::new(), None);
    let desired = Desired {
        config_dir,
        agents: &empty_agents,
        ai_providers: &providers,
        mcp: &mcp,
        instructions: None,
        secrets: &secrets,
        access: &access,
        skills: &sentinel,
        subagents: &subagents,
    };
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

fn locate_tree(
    roots: &Roots,
    file: &adapters::RenderedTreeFile,
    project: Option<&std::path::Path>,
) -> std::path::PathBuf {
    project
        .and_then(|p| file.project.as_ref().map(|path| p.join(path)))
        .unwrap_or_else(|| roots.base(file.base).join(&file.path))
}
