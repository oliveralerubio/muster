//! `muster agents` (list adapters) and `muster agents apply` (write native files).
//!
//! Apply is the file-I/O shell around the pure `Adapter::render`: read current
//! files, render, write what changed. Failures are isolated per adapter and
//! reported as error diagnostics; the other adapters are still applied.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapters::{self, config, Adapter, Base, Current, Desired, ManagedFile};
use crate::context::Context;
use crate::errors::MusterError;
use crate::machine;
use crate::manifest::{self, Manifest};
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "agents",
    about: "List agents and their adapters (files, scopes, capabilities)",
    params: &[],
    mutating: false,
    handler: list,
};

pub const APPLY_SPEC: CommandSpec = CommandSpec {
    name: "agents.apply",
    about: "Write the canonical agent configuration into each agent's native files",
    params: &[ParamSpec {
        name: "agent",
        about: "Apply a single adapter (claude, codex, opencode)",
        kind: ParamKind::String,
        required: false,
    }],
    mutating: true,
    handler: apply,
};

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    // Listing works before `muster init`; an existing manifest must still be valid.
    let manifest = if ctx.config_dir.join(manifest::FILE_NAME).is_file() {
        Some(Manifest::load(&ctx.config_dir)?)
    } else {
        None
    };
    let enabled = enabled_ids(manifest.as_ref())?;
    let detected = machine::detect_agents();
    let roots = Roots::from_env()?;

    let agents: Vec<Value> = adapters::ADAPTERS
        .iter()
        .map(|a| {
            let files: Vec<Value> = a
                .files
                .iter()
                .map(|f| {
                    json!({
                        "path": roots.resolve(f),
                        "scope": f.scope,
                        "format": f.format,
                        "purpose": f.purpose,
                    })
                })
                .collect();
            json!({
                "id": a.id,
                "name": a.name,
                "experimental": a.experimental,
                "available": a.available(),
                "enabled": enabled.contains(&a.id),
                "detected": detected.iter().any(|d| d == a.id),
                "platforms": a.platforms,
                "min_version": a.min_version,
                "capabilities": a.capabilities,
                "files": files,
            })
        })
        .collect();
    Ok(json!({ "agents": agents }))
}

fn apply(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let targets: Vec<&'static Adapter> = match params.get_string("agent") {
        Some(id) => {
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
            vec![adapter]
        }
        None => enabled_ids(Some(&manifest))?
            .into_iter()
            .filter_map(adapters::find)
            .filter(|a| a.available())
            .collect(),
    };

    for (name, server) in &manifest.mcp {
        server
            .transport()
            .map_err(|message| MusterError::InvalidConfig {
                message: format!("MCP server `{name}` {message}"),
                path: Some(format!("{}: mcp.{name}", manifest::FILE_NAME)),
            })?;
    }
    let default_agents = config::AgentsConfig::default();
    let agents = manifest.agents.as_ref().unwrap_or(&default_agents);
    let instructions = read_instructions(ctx, agents)?;
    let desired = Desired {
        agents,
        mcp: &manifest.mcp,
        instructions: instructions.as_deref(),
    };

    let roots = Roots::from_env()?;
    let mut results = Vec::new();
    for adapter in targets {
        let outcome = apply_one(adapter, &desired, &roots, ctx.dry_run);
        let (status, files) = match outcome {
            Ok((files, diagnostics)) => {
                for d in diagnostics {
                    ctx.diag(d);
                }
                (if ctx.dry_run { "planned" } else { "applied" }, files)
            }
            Err(err) => {
                let mut diag = Diagnostic::error(
                    "adapter_failed",
                    format!("{} ({}) adapter failed: {err}", adapter.name, adapter.id),
                );
                diag.path = Some(adapter.id.to_string());
                ctx.diag(diag);
                ("failed", Vec::new())
            }
        };
        results.push(json!({ "id": adapter.id, "status": status, "files": files }));
    }
    Ok(json!({ "dry_run": ctx.dry_run, "agents": results }))
}

/// Read, render and (unless dry-run) write one adapter's files.
fn apply_one(
    adapter: &Adapter,
    desired: &Desired,
    roots: &Roots,
    dry_run: bool,
) -> Result<(Vec<Value>, Vec<Diagnostic>), MusterError> {
    let mut current = Current::new();
    for file in adapter.files {
        let path = roots.resolve(file);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                current.insert(file.path, text);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(&path, e)),
        }
    }
    let plan = adapter.render(desired, &current)?;
    let mut files = Vec::new();
    for rendered in &plan.files {
        let path = roots.resolve(rendered.file);
        let changed = current.get(rendered.file.path) != Some(&rendered.contents);
        if changed && !dry_run {
            write_file(&path, &rendered.contents)?;
        }
        files.push(json!({ "path": path, "changed": changed }));
    }
    Ok((files, plan.diagnostics))
}

/// Write via a sibling temp file + rename, keeping the existing file mode.
fn write_file(path: &Path, contents: &str) -> Result<(), MusterError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".muster-tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents).map_err(|e| io_error(&tmp, e))?;
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, meta.permissions()).map_err(|e| io_error(&tmp, e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_error(path, e)
    })
}

fn io_error(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}

/// Canonical AGENTS.md text. An explicitly declared but missing source is a
/// warning (instructions are skipped); an absent default source is silent.
fn read_instructions(
    ctx: &mut Context,
    agents: &config::AgentsConfig,
) -> Result<Option<String>, MusterError> {
    let source = agents
        .instructions
        .as_ref()
        .map_or(config::DEFAULT_INSTRUCTIONS_SOURCE, |i| i.source.as_str());
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
    let path = ctx.config_dir.join(relative);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if agents.instructions.is_some() {
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
        Err(e) => Err(io_error(&path, e)),
    }
}

/// Adapter ids to apply: `[agents] enabled`, else every stable adapter.
fn enabled_ids(manifest: Option<&Manifest>) -> Result<Vec<&'static str>, MusterError> {
    let Some(declared) = manifest
        .and_then(|m| m.agents.as_ref())
        .and_then(|a| a.enabled.as_ref())
    else {
        return Ok(adapters::default_enabled());
    };
    declared
        .iter()
        .map(|id| {
            adapters::find(id)
                .map(|a| a.id)
                .ok_or_else(|| MusterError::InvalidConfig {
                    message: format!("unknown agent `{id}` in agents.enabled"),
                    path: Some(format!("{}: agents.enabled", manifest::FILE_NAME)),
                })
        })
        .collect()
}

/// Filesystem roots managed paths are relative to.
struct Roots(BTreeMap<&'static str, PathBuf>);

impl Roots {
    fn from_env() -> Result<Self, MusterError> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME").ok_or_else(|| MusterError::Operational("HOME is not set".into()))?;
        let xdg_config = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        Ok(Self(BTreeMap::from([
            ("home", home),
            ("xdg_config", xdg_config),
        ])))
    }

    fn resolve(&self, file: &ManagedFile) -> PathBuf {
        let key = match file.base {
            Base::Home => "home",
            Base::XdgConfig => "xdg_config",
        };
        self.0[key].join(file.path)
    }
}
