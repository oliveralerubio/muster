//! Pi coding agent adapter (Pi 1.2.0+).
//!
//! Native paths and schemas: upstream `packages/coding-agent/docs/`
//! `configuration.md`, `mcp.md`, `settings.md` and `skills.md`.
//! Subagents are installed byte-exact where Pi's subagent extension discovers
//! them (`~/.pi/agent/agents`, `.pi/agents`); the extension itself can be
//! installed with `[agents.packages]`, which Muster writes to the `packages`
//! array of `settings.json`. Hooks and Claude-style permissions have no native
//! mapping and stay unsupported. See docs/SPEC.md.

use std::path::Path;

use serde_json::{Map, Value};

use super::{
    child_object, json_object, json_text, markdown_block, push_skills, push_subagents_raw,
    resolve_mcp_map, transport, Adapter, Base, Capability, Current, Desired, FileFormat,
    ManagedFile, RenderPlan, RenderedFile, Scope, Transport,
};
use crate::errors::MusterError;

const MCP: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".pi/agent/mcp.json",
    project: Some(".pi/mcp.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "Pi MCP servers",
};
const SETTINGS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".pi/agent/settings.json",
    project: Some(".pi/settings.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "Pi default model/provider and packages",
};
const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".pi/agent/AGENTS.md",
    project: Some("AGENTS.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "Pi instructions",
};

pub static ADAPTER: Adapter = Adapter {
    id: "pi",
    name: "Pi",
    binary: "pi",
    experimental: false,
    platforms: &["linux", "macos"],
    min_version: "1.2.0",
    capabilities: &[
        Capability::Instructions,
        Capability::McpStdio,
        Capability::McpRemote,
        Capability::Models,
        Capability::Skills,
        Capability::Subagents,
        Capability::Packages,
    ],
    files: &[MCP, SETTINGS, INSTRUCTIONS],
    render: Some(render),
};

fn render(desired: &Desired, current: &Current, plan: &mut RenderPlan) -> Result<(), MusterError> {
    let mut object = json_object(current.get(MCP.path), MCP.path)?;
    let servers = child_object(&mut object, "mcpServers");
    for (name, server) in desired.mcp {
        let mut entry = Map::new();
        match transport(name, server)? {
            Transport::Stdio { command } => {
                entry.insert("command".into(), Value::String(command.into()));
                entry.insert(
                    "args".into(),
                    serde_json::to_value(&server.args).expect("args"),
                );
                let env = resolve_mcp_map(&server.env, desired, &ADAPTER, name, plan);
                entry.insert("env".into(), serde_json::to_value(env).expect("env"));
            }
            Transport::Remote { url } => {
                entry.insert("url".into(), Value::String(url.into()));
                let headers = resolve_mcp_map(&server.headers, desired, &ADAPTER, name, plan);
                entry.insert(
                    "headers".into(),
                    serde_json::to_value(headers).expect("headers"),
                );
            }
        }
        servers.insert(name.clone(), Value::Object(entry));
    }
    plan.files.push(RenderedFile {
        file: &MCP,
        contents: json_text(object),
    });

    let model = desired.agents.models.get(ADAPTER.id);
    let packages = desired.agents.packages.get(ADAPTER.id);
    if model.is_some() || packages.is_some() {
        let mut settings = json_object(current.get(SETTINGS.path), SETTINGS.path)?;
        if let Some(model) = model {
            // Qualified identifiers select a provider explicitly. Bare IDs keep
            // the user's existing provider (Pi accepts both provider/model and ID).
            let model = if let Some((provider, model)) = model.split_once('/') {
                settings.insert("defaultProvider".into(), Value::String(provider.into()));
                model
            } else {
                model
            };
            settings.insert("defaultModel".into(), Value::String(model.into()));
        }
        if let Some(packages) = packages {
            let entries = packages
                .iter()
                .map(|p| Value::String(package_source(p, desired.config_dir)))
                .collect();
            settings.insert("packages".into(), Value::Array(entries));
        }
        plan.files.push(RenderedFile {
            file: &SETTINGS,
            contents: json_text(settings),
        });
    }
    if let Some(text) = desired.instructions {
        plan.files.push(RenderedFile {
            file: &INSTRUCTIONS,
            contents: markdown_block(current.get(INSTRUCTIONS.path), text),
        });
    }
    push_skills(desired, Base::Home, ".pi/agent/skills", ".pi/skills", plan);
    push_subagents_raw(desired, Base::Home, ".pi/agent/agents", ".pi/agents", plan);
    Ok(())
}

/// Local `./` and `../` sources resolve against the config repository and are
/// written absolute (Pi would resolve them against the settings file).
/// Everything else (`npm:`, `git:`, URLs, absolute paths) is verbatim.
fn package_source(entry: &str, config_dir: &Path) -> String {
    if entry.starts_with("./") || entry.starts_with("../") {
        config_dir.join(entry).to_string_lossy().into_owned()
    } else {
        entry.to_owned()
    }
}
