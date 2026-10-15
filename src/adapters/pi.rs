//! Experimental Pi coding agent adapter (Pi 1.2.0).
//!
//! Native paths and schemas: upstream `packages/coding-agent/docs/`
//! `configuration.md`, `mcp.md`, `settings.md` and `skills.md`.
//! Subagents require third-party extensions, so canonical definitions, hooks
//! and Claude-style permissions are unsupported. See docs/SPEC.md.

use serde_json::{Map, Value};

use super::{
    child_object, json_object, json_text, markdown_block, push_skills, resolve_mcp_map, transport,
    Adapter, Base, Capability, Current, Desired, FileFormat, ManagedFile, RenderPlan, RenderedFile,
    Scope, Transport,
};
use crate::errors::MusterError;

const MCP: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".pi/agent/mcp.json",
    project: Some(".pi/mcp.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "Experimental Pi MCP servers (Pi 1.2.0)",
};
const SETTINGS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".pi/agent/settings.json",
    project: Some(".pi/settings.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "Experimental Pi default model/provider",
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
    experimental: true,
    platforms: &["linux", "macos"],
    min_version: "1.2.0",
    capabilities: &[
        Capability::Instructions,
        Capability::McpStdio,
        Capability::McpRemote,
        Capability::Models,
        Capability::Skills,
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

    if let Some(model) = desired.agents.models.get(ADAPTER.id) {
        let mut settings = json_object(current.get(SETTINGS.path), SETTINGS.path)?;
        // Qualified identifiers select a provider explicitly. Bare IDs keep
        // the user's existing provider (Pi accepts both provider/model and ID).
        let model = if let Some((provider, model)) = model.split_once('/') {
            settings.insert("defaultProvider".into(), Value::String(provider.into()));
            model
        } else {
            model
        };
        settings.insert("defaultModel".into(), Value::String(model.into()));
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
    Ok(())
}
