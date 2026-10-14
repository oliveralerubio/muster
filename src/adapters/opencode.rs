//! OpenCode adapter (`opencode`).
//!
//! Managed files (under `$XDG_CONFIG_HOME/opencode/`):
//! - `opencode.json`: `model`, `mcp.<name>` (`type: local|remote`).
//! - `AGENTS.md`: canonical instructions.

use serde_json::{Map, Value};

use super::{
    child_object, json_object, json_text, markdown_block, resolve_mcp_map, transport, Adapter,
    Base, Capability, Current, Desired, FileFormat, ManagedFile, RenderPlan, RenderedFile, Scope,
    Transport,
};
use crate::errors::MusterError;

const CONFIG: ManagedFile = ManagedFile {
    base: Base::XdgConfig,
    path: "opencode/opencode.json",
    project: Some("opencode.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "OpenCode configuration and MCP servers",
};

const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::XdgConfig,
    path: "opencode/AGENTS.md",
    project: Some("AGENTS.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "User instructions",
};

const SCHEMA_URL: &str = "https://opencode.ai/config.json";

pub static ADAPTER: Adapter = Adapter {
    id: "opencode",
    name: "OpenCode",
    binary: "opencode",
    experimental: false,
    platforms: &["linux", "macos", "windows"],
    min_version: "0.1.0",
    capabilities: &[
        Capability::Instructions,
        Capability::McpStdio,
        Capability::McpRemote,
        Capability::Models,
    ],
    files: &[CONFIG, INSTRUCTIONS],
    render: Some(render),
};

fn render(desired: &Desired, current: &Current, plan: &mut RenderPlan) -> Result<(), MusterError> {
    let mut object = json_object(current.get(CONFIG.path), CONFIG.path)?;
    if !object.contains_key("$schema") {
        // Keep `$schema` first in a fresh file, as OpenCode's docs do.
        let mut fresh = Map::new();
        fresh.insert("$schema".into(), Value::String(SCHEMA_URL.into()));
        fresh.extend(object);
        object = fresh;
    }

    if let Some(model) = desired.agents.models.get("opencode") {
        object.insert("model".into(), Value::String(model.clone()));
    }

    if !desired.mcp.is_empty() {
        let servers = child_object(&mut object, "mcp");
        for (name, server) in desired.mcp {
            let mut entry = Map::new();
            match transport(name, server)? {
                Transport::Stdio { command } => {
                    entry.insert("type".into(), "local".into());
                    let argv: Vec<Value> = std::iter::once(command)
                        .chain(server.args.iter().map(String::as_str))
                        .map(Value::from)
                        .collect();
                    entry.insert("command".into(), Value::Array(argv));
                    let env = resolve_mcp_map(&server.env, desired.secrets, "OpenCode", name, plan);
                    if !env.is_empty() {
                        entry.insert("environment".into(), string_map(env));
                    }
                }
                Transport::Remote { url } => {
                    entry.insert("type".into(), "remote".into());
                    entry.insert("url".into(), url.into());
                    let headers =
                        resolve_mcp_map(&server.headers, desired.secrets, "OpenCode", name, plan);
                    if !headers.is_empty() {
                        entry.insert("headers".into(), string_map(headers));
                    }
                }
            }
            servers.insert(name.clone(), Value::Object(entry));
        }
    }

    plan.files.push(RenderedFile {
        file: &CONFIG,
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

fn string_map(map: std::collections::BTreeMap<String, String>) -> Value {
    Value::Object(
        map.into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
    )
}
