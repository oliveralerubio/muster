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
    const MAX_PATTERN_BYTES: usize = 1024;
    if query.len() > MAX_PATTERN_BYTES {
        return Err(MusterError::Usage(format!(
            "search text is too long (max {MAX_PATTERN_BYTES} bytes)"
        )));
    }
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
        // A project layer that does not set `enabled` inherits the machine's
        // effective agents (team -> group -> member -> machine), not just the
        // team profile's raw list.
        merged.agents.enabled = match &target {
            Target::Project(_) => {
                layer::global_merge(&ctx.config_dir, &ctx.state_dir, &manifest)
                    .agents
                    .enabled
            }
            Target::Global => manifest.agents.as_ref().and_then(|a| a.enabled.clone()),
        };
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
    // The engine skips overlays it cannot render: globally skipped agents,
    // and (matching `instructions_overlay_shared_project_file`) agents that
    // share one project AGENTS.md with several enabled agents. Claude's
    // overlay is still rendered into the project CLAUDE.md.
    let enabled_count = engine::enabled_adapters(Some(&merged.agents))?.len();
    for id in &ids {
        let skip = (matches!(target, Target::Global)
            && merged
                .agents
                .skip_instructions
                .as_ref()
                .is_some_and(|v| v.iter().any(|x| x == id)))
            || (matches!(target, Target::Project(_)) && *id != "claude" && enabled_count > 1);
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
        // Native skills/subagents directories, matching what the adapters
        // render (global and project scopes; pi and hermes have no native
        // subagent directory).
        let (base, skills, subagents) = match (adapter.id, &target) {
            ("claude", Target::Global) => (
                roots.base(adapters::Base::Home),
                ".claude/skills",
                Some(".claude/agents"),
            ),
            ("codex", Target::Global) => (
                roots.base(adapters::Base::Home),
                ".agents/skills",
                Some(".codex/agents"),
            ),
            ("antigravity", Target::Global) => (
                roots.base(adapters::Base::Home),
                ".gemini/antigravity-cli/skills",
                Some(".gemini/antigravity-cli/agents"),
            ),
            ("opencode", Target::Global) => (
                roots.base(adapters::Base::XdgConfig),
                "opencode/skills",
                Some("opencode/agents"),
            ),
            ("pi", Target::Global) => (roots.base(adapters::Base::Home), ".pi/agent/skills", None),
            ("hermes", Target::Global) => {
                (roots.base(adapters::Base::Home), ".hermes/skills", None)
            }
            ("claude", Target::Project(dir)) => {
                (dir.as_path(), ".claude/skills", Some(".claude/agents"))
            }
            ("codex", Target::Project(dir)) => {
                (dir.as_path(), ".agents/skills", Some(".codex/agents"))
            }
            ("antigravity", Target::Project(dir)) => {
                (dir.as_path(), ".agents/skills", Some(".agents/agents"))
            }
            ("opencode", Target::Project(dir)) => {
                (dir.as_path(), ".opencode/skills", Some(".opencode/agents"))
            }
            ("pi", Target::Project(dir)) => (dir.as_path(), ".pi/skills", None),
            ("hermes", Target::Project(dir)) => (dir.as_path(), ".hermes/skills", None),
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
        if let Some(subagents) = subagents {
            if let Ok(entries) = std::fs::read_dir(base.join(subagents)) {
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
    }
    // Search roots, most specific first: a file is screened by its path
    // inside the first root that contains it.
    let mut search_roots: Vec<&Path> = Vec::new();
    if let Target::Project(dir) = &target {
        search_roots.push(dir.as_path());
    }
    search_roots.push(ctx.config_dir.as_path());
    search_roots.push(roots.base(adapters::Base::XdgConfig));
    search_roots.push(roots.base(adapters::Base::Home));
    let mut matches = Vec::new();
    for (file, (kind, layer, agents, canonical)) in files {
        if !safe_file(&file, &search_roots) {
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

/// Searchable file? Screens the name of secret material (the path inside a
/// Muster root, or the file name outside every root) and rejects symlinked
/// components below the root. Ancestors of the roots themselves are not
/// screened, so a project checked out under e.g. `~/my-vault-repo/` is
/// still searchable.
fn safe_file(path: &Path, roots: &[&Path]) -> bool {
    let Some((root, relative)) = roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok().map(|rel| (*root, rel)))
    else {
        return path.file_name().is_some_and(|name| !sensitive(name))
            && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if sensitive(component.as_os_str()) {
            return false;
        }
        current.push(component);
        if std::fs::symlink_metadata(&current).is_ok_and(|m| m.file_type().is_symlink()) {
            return false;
        }
    }
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

fn sensitive(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_lowercase();
    name.contains("secret") || name.contains("vault")
}
