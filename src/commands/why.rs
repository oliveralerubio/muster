//! Read-only search of instruction sources and installed context.
use crate::adapters::{self, FileFormat};
use crate::commands::plan::{self, PROJECT_PARAM};
use crate::context::Context;
use crate::engine::{self, layer, Roots, Target};
use crate::errors::MusterError;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use regex::RegexBuilder;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SPEC: CommandSpec = CommandSpec {
    name: "why",
    about: "Find text in agent context",
    mutating: false,
    handler: why,
    params: &[
        ParamSpec {
            name: "text",
            about: "Text to find",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        PROJECT_PARAM,
        ParamSpec {
            name: "agent",
            about: "Limit to one agent",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "regex",
            about: "Interpret text as a regular expression",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
};

fn why(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let query = params.get_string("text").unwrap_or_default();
    let pattern = if params.get_bool("regex") {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let re = RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .map_err(|e| MusterError::Usage(format!("invalid regex: {e}")))?;
    let manifest = Manifest::load(&ctx.config_dir)?;
    let target = plan::target(params)?;
    let layers = match &target {
        Target::Global => layer::global_layers(
            &ctx.config_dir,
            &manifest,
            crate::machine::load_name(&ctx.state_dir).as_deref(),
        ),
        Target::Project(dir) => layer::project_layers(dir)?,
    };
    let mut merged = layer::merge(&layers);
    if merged.agents.enabled.is_none() {
        merged.agents.enabled = manifest.agents.as_ref().and_then(|a| a.enabled.clone());
    }
    let selected = if let Some(id) = params.get_string("agent") {
        let adapter = adapters::find(id)
            .filter(|a| a.available())
            .ok_or_else(|| MusterError::Usage(format!("unknown or unavailable agent `{id}`")))?;
        vec![adapter]
    } else {
        engine::enabled_adapters(Some(&merged.agents))?
    };
    let ids: Vec<_> = selected.iter().map(|a| a.id).collect();
    let source_layer = layers
        .iter()
        .rev()
        .find(|l| l.agents.instructions.is_some())
        .or_else(|| layers.first())
        .map_or("team", |l| l.kind);
    let path = merged.instructions_path()?.0;
    let mut files: BTreeMap<PathBuf, (&str, &str, Vec<&str>, bool)> = BTreeMap::new();
    files.insert(
        path.clone(),
        ("instructions", source_layer, ids.clone(), true),
    );
    for id in &ids {
        let skip = matches!(target, Target::Global)
            && merged
                .agents
                .skip_instructions
                .as_ref()
                .is_some_and(|v| v.iter().any(|x| x == id));
        if !skip {
            files.insert(
                path.with_file_name(format!("AGENTS.{id}.md")),
                ("overlay", source_layer, vec![id], true),
            );
        }
    }
    let roots = Roots::from_env()?;
    for adapter in &selected {
        for file in adapter
            .files
            .iter()
            .filter(|f| f.format == FileFormat::Markdown)
        {
            let native = match &target {
                Target::Global => Some(roots.resolve(file)),
                Target::Project(dir) => file.project.map(|p| dir.join(p)),
            };
            if let Some(native) = native {
                files
                    .entry(native)
                    .and_modify(|v| {
                        if !v.2.contains(&adapter.id) {
                            v.2.push(adapter.id);
                        }
                    })
                    .or_insert(("instructions", "native", vec![adapter.id], false));
            }
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let (base, skills, agents) = match (adapter.id, &target) {
            ("claude", Target::Global) => (home.clone(), ".claude/skills", ".claude/agents"),
            ("codex", Target::Global) => (home.clone(), ".agents/skills", ".codex/agents"),
            ("antigravity", Target::Global) => (
                home.clone(),
                ".gemini/antigravity-cli/skills",
                ".gemini/antigravity-cli/agents",
            ),
            ("opencode", Target::Global) => (xdg, "opencode/skills", "opencode/agents"),
            ("claude", Target::Project(dir)) => (dir.clone(), ".claude/skills", ".claude/agents"),
            ("codex", Target::Project(dir)) => (dir.clone(), ".agents/skills", ".codex/agents"),
            ("antigravity", Target::Project(dir)) => {
                (dir.clone(), ".agents/skills", ".agents/agents")
            }
            ("opencode", Target::Project(dir)) => {
                (dir.clone(), ".opencode/skills", ".opencode/agents")
            }
            _ => continue,
        };
        if let Ok(entries) = std::fs::read_dir(base.join(skills)) {
            for e in entries.flatten() {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    let p = e.path().join("SKILL.md");
                    files
                        .entry(p)
                        .and_modify(|v| {
                            if !v.2.contains(&adapter.id) {
                                v.2.push(adapter.id);
                            }
                        })
                        .or_insert(("skill", "native", vec![adapter.id], false));
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(base.join(agents)) {
            for e in entries.flatten() {
                let p = e.path();
                if matches!(p.extension().and_then(|s| s.to_str()), Some("md" | "toml"))
                    && e.file_type().is_ok_and(|t| t.is_file())
                {
                    files
                        .entry(p)
                        .and_modify(|v| {
                            if !v.2.contains(&adapter.id) {
                                v.2.push(adapter.id);
                            }
                        })
                        .or_insert(("subagent", "native", vec![adapter.id], false));
                }
            }
        }
    }
    let mut matches = Vec::new();
    for (file, (kind, layer, agents, canonical)) in files {
        if !safe_file(&file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let mut managed = canonical;
        for (index, line) in text.lines().enumerate() {
            if line.starts_with("<!-- BEGIN MUSTER MANAGED BLOCK:") {
                managed = true;
                continue;
            }
            if line == "<!-- END MUSTER MANAGED BLOCK -->" {
                managed = false;
                continue;
            }
            if re.is_match(line) {
                matches.push(json!({"file": file, "line": index + 1, "excerpt": line.trim(), "kind": kind, "layer": layer, "agents": agents, "managed": managed}));
            }
        }
    }
    Ok(json!({"query": query, "matches": matches}))
}

fn safe_file(path: &Path) -> bool {
    path.components().all(|c| {
        let s = c.as_os_str().to_string_lossy().to_lowercase();
        !s.contains("secret") && !s.contains("vault")
    }) && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}
