//! `muster agents` (list adapters) and `muster agents apply` (write native files).
//!
//! `agents apply` routes through the apply engine ([`crate::engine`]): it
//! inherits atomic writes, backups, ownership records, generations, and the
//! gate on unapproved hooks/MCP servers.

use serde_json::{json, Value};

use crate::adapters::{self, ManagedFile};
use crate::context::Context;
use crate::engine::{self, Options, Target};
use crate::errors::MusterError;
use crate::machine;
use crate::manifest::{self, Manifest};
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
        positional: false,
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
    let enabled = engine::enabled_adapters(manifest.as_ref().and_then(|m| m.agents.as_ref()))?
        .iter()
        .map(|a| a.id)
        .collect::<Vec<_>>();
    let detected = machine::detect_agents();
    let roots = engine::Roots::from_env()?;

    let agents: Vec<Value> = adapters::ADAPTERS
        .iter()
        .map(|a| {
            let files: Vec<Value> = a
                .files
                .iter()
                .map(|f: &ManagedFile| {
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
    let options = Options {
        target: Target::Global,
        agent: params.get_string("agent"),
    };
    let plan = engine::plan(ctx, &options)?;
    engine::apply(ctx, plan)
}
