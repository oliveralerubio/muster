//! `muster mcp-server`: manage the manifest's declared MCP servers
//! (`[mcp.<name>]`). The same tables power profiles (`[profiles.<n>.mcp]`);
//! these commands edit the fleet-wide declaration. In team mode only admins
//! change them. Env and header values are stored exactly as given, so secret
//! references (`native://group/name`) stay references; raw values are only
//! ever plain settings (e.g. `NODE_ENV=production`).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::adapters::config::McpServer;
use crate::context::Context;
use crate::engine::diff::unified;
use crate::errors::MusterError;
use crate::machine;
use crate::manifest::{Manifest, FILE_NAME};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::team;

const NAME: ParamSpec = ParamSpec {
    name: "name",
    about: "Server name (lowercase letters, digits and '-')",
    kind: ParamKind::String,
    required: true,
    positional: true,
};

pub const LIST: CommandSpec = CommandSpec {
    name: "mcp-server.list",
    about: "List the MCP servers declared in the manifest",
    params: &[],
    mutating: false,
    handler: list,
};

pub const ADD: CommandSpec = CommandSpec {
    name: "mcp-server.add",
    about: "Add or replace an MCP server ([mcp.<name>], command xor url)",
    params: &[
        NAME,
        ParamSpec {
            name: "command",
            about: "Stdio command to run (e.g. `npx`; arguments via --args)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "url",
            about: "Remote server URL (instead of --command)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "args",
            about: "Command argument (repeat, or comma-separated)",
            kind: ParamKind::List,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "env",
            about: "Environment entry KEY=VALUE (repeat; VALUE may be a secret reference)",
            kind: ParamKind::List,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "header",
            about: "Remote header KEY=VALUE (repeat; VALUE may be a secret reference)",
            kind: ParamKind::List,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: add,
};

pub const RM: CommandSpec = CommandSpec {
    name: "mcp-server.rm",
    about: "Remove an MCP server from the manifest",
    params: &[NAME],
    mutating: true,
    handler: rm,
};

/// Team mode: declared MCP servers are part of the team profile.
fn actor(ctx: &Context, manifest: &Manifest) -> Result<Option<team::Actor>, MusterError> {
    if manifest.members.is_empty() {
        return Ok(None);
    }
    team::admin(manifest, &ctx.state_dir).map(Some)
}

fn commit(
    ctx: &mut Context,
    actor: Option<&team::Actor>,
    summary: &str,
) -> Result<(), MusterError> {
    match actor {
        Some(actor) => team::commit(ctx, actor, summary),
        None => crate::git::commit_if_configured(&ctx.config_dir, summary).map(|_| ()),
    }
}

fn server_name(params: &Params) -> Result<String, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("server name is required".into()))?;
    machine::validate_name(name)
}

/// `KEY=VALUE` entries → map. The value may be empty; the key may not.
fn pairs(params: &Params, param: &str) -> Result<BTreeMap<String, String>, MusterError> {
    let mut map = BTreeMap::new();
    for entry in params.get_list(param) {
        let Some((key, value)) = entry.split_once('=') else {
            return Err(MusterError::Usage(format!(
                "--{param} entries must be KEY=VALUE, got '{entry}'"
            )));
        };
        if key.is_empty() || key.trim() != key {
            return Err(MusterError::Usage(format!(
                "--{param} key must be a non-empty, untrimmed name, got '{key}'"
            )));
        }
        map.insert(key.to_owned(), value.to_owned());
    }
    Ok(map)
}

/// The server as TOML text, exactly what `muster.toml` stores under
/// `[mcp.<name>]`.
fn server_toml(server: &McpServer) -> String {
    toml::to_string_pretty(server).unwrap_or_default()
}

fn transport_of(server: &McpServer) -> &'static str {
    match server.transport() {
        Ok(crate::adapters::config::Transport::Stdio { .. }) => "stdio",
        Ok(crate::adapters::config::Transport::Remote { .. }) => "remote",
        Err(_) => "invalid",
    }
}

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let layers = crate::engine::layer::global_layers(
        &ctx.config_dir,
        &manifest,
        crate::machine::load_name(&ctx.state_dir).as_deref(),
    );
    let merged = crate::engine::layer::merge(&layers);
    let servers: Vec<_> = manifest
        .mcp
        .iter()
        .map(|(name, server)| {
            json!({
                "name": name,
                "server": server,
                "transport": transport_of(server),
                "enabled": merged.disabled("mcp", name, "").is_none(),
            })
        })
        .collect();
    Ok(json!({ "servers": servers }))
}

fn add(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = server_name(params)?;
    let command = params.get_string("command");
    let url = params.get_string("url");
    let server = McpServer {
        command: command.map(str::to_owned),
        args: params.get_list("args"),
        env: pairs(params, "env")?,
        url: url.map(str::to_owned),
        headers: pairs(params, "header")?,
    };
    server
        .transport()
        .map_err(|why| MusterError::InvalidConfig {
            message: format!("MCP server '{name}' {why}"),
            path: Some(format!("{FILE_NAME}: mcp.{name}")),
        })?;

    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = actor(ctx, &manifest)?;
    let replaced = manifest.mcp.contains_key(&name);
    let diff = unified(
        &manifest.mcp.get(&name).map(server_toml).unwrap_or_default(),
        &server_toml(&server),
        &format!("/{FILE_NAME}"),
        false,
    );
    let result = json!({
        "name": name,
        "server": server,
        "transport": transport_of(&server),
        "replaced": replaced,
        "diff": diff,
    });
    if ctx.dry_run {
        return Ok(result);
    }
    ctx.confirm(&format!("Set MCP server '{name}'?"))?;
    manifest.mcp.insert(name.clone(), server);
    manifest.save(&ctx.config_dir)?;
    commit(
        ctx,
        actor.as_ref(),
        &format!("feat(mcp): set MCP server {name}"),
    )?;
    Ok(result)
}

fn rm(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = server_name(params)?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = actor(ctx, &manifest)?;
    if !manifest.mcp.contains_key(&name) {
        return Err(MusterError::Usage(format!(
            "MCP server '{name}' is not declared"
        )));
    }
    ctx.confirm(&format!("Remove MCP server '{name}'?"))?;

    let result = json!({ "removed": name });
    if ctx.dry_run {
        return Ok(result);
    }
    manifest.mcp.remove(&name);
    manifest.save(&ctx.config_dir)?;
    commit(
        ctx,
        actor.as_ref(),
        &format!("feat(mcp): remove MCP server {name}"),
    )?;
    Ok(result)
}
