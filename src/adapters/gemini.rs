//! Gemini CLI / Antigravity adapter (`gemini`).
//!
//! Managed files under `$HOME/.gemini/config/`:
//! - `mcp_config.json`: `mcpServers.<name>`.
//! - `AGENTS.md`: canonical instructions.

use serde_json::{Map, Value};

use super::{
    child_object, json_object, json_text, markdown_block, resolve_mcp_map, transport, Adapter,
    Base, Capability, Current, Desired, FileFormat, ManagedFile, RenderPlan, RenderedFile, Scope,
    Transport,
};
use crate::errors::MusterError;

const MCP: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".gemini/config/mcp_config.json",
    project: Some(".gemini/config/mcp_config.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "Gemini MCP servers",
};

const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".gemini/config/AGENTS.md",
    project: Some("AGENTS.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "User instructions",
};

pub static ADAPTER: Adapter = Adapter {
    id: "gemini",
    name: "Gemini CLI / Antigravity",
    experimental: true,
    platforms: &["linux", "macos", "windows"],
    min_version: "",
    capabilities: &[
        Capability::Instructions,
        Capability::McpStdio,
        Capability::McpRemote,
        Capability::Models,
    ],
    files: &[MCP, INSTRUCTIONS],
    render: Some(render),
};

fn render(desired: &Desired, current: &Current, plan: &mut RenderPlan) -> Result<(), MusterError> {
    let mut object = json_object(current.get(MCP.path), MCP.path)?;
    if !desired.mcp.is_empty() {
        let servers = child_object(&mut object, "mcpServers");
        for (name, server) in desired.mcp {
            let mut entry = Map::new();
            match transport(name, server)? {
                Transport::Stdio { command } => {
                    entry.insert("command".into(), Value::String(command.to_string()));
                    if !server.args.is_empty() {
                        let args = server.args.iter().cloned().map(Value::String).collect();
                        entry.insert("args".into(), Value::Array(args));
                    }
                    let env = resolve_mcp_map(&server.env, desired.secrets, "Gemini", name, plan);
                    if !env.is_empty() {
                        entry.insert(
                            "env".into(),
                            Value::Object(env.into_iter().map(|(k, v)| (k, v.into())).collect()),
                        );
                    }
                }
                Transport::Remote { url } => {
                    entry.insert("url".into(), Value::String(url.to_string()));
                    let headers =
                        resolve_mcp_map(&server.headers, desired.secrets, "Gemini", name, plan);
                    if !headers.is_empty() {
                        entry.insert(
                            "headers".into(),
                            Value::Object(
                                headers.into_iter().map(|(k, v)| (k, v.into())).collect(),
                            ),
                        );
                    }
                }
            }
            servers.insert(name.clone(), Value::Object(entry));
        }
    }
    plan.files.push(RenderedFile {
        file: &MCP,
        contents: json_text(object),
    });
    if let Some(text) = desired.instructions {
        plan.files.push(RenderedFile {
            file: &INSTRUCTIONS,
            contents: markdown_block(current.get(INSTRUCTIONS.path), text),
        });
    }
    Ok(())
}
