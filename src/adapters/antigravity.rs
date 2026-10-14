//! Antigravity CLI adapter (`antigravity`, binary `agy`).
//!
//! Antigravity CLI replaced Google's previous agent CLI for consumer tiers on 2026-06-18 and
//! still reads its configuration under `~/.gemini/`.
//!
//! Managed files:
//! - `~/.gemini/config/mcp_config.json`: `mcpServers.<name>` (stdio
//!   `command`/`args`/`env`; remote `serverUrl`/`headers`, never `url`).
//! - `~/.gemini/AGENTS.md`: canonical instructions (plain Markdown, no
//!   frontmatter).
//! - `~/.gemini/antigravity-cli/skills/<skill>/...`: Agent Skills declared in
//!   `[agents.skills]`, one whole file per skill file. Skills Muster does not
//!   declare are left alone.
//!
//! Model, permissions and hooks have no documented configuration, so they are
//! not declared and the framework warns that they are skipped.

use serde_json::{Map, Value};

use super::{
    child_object, json_object, json_text, markdown_block, resolve_mcp_map, transport, Adapter,
    Base, Capability, Current, Desired, FileFormat, ManagedFile, RenderPlan, RenderedFile,
    RenderedTreeFile, Scope, Transport,
};
use crate::errors::MusterError;

const SKILLS_DIR: &str = ".gemini/antigravity-cli/skills";

const MCP: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".gemini/config/mcp_config.json",
    project: Some(".agents/mcp_config.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "User-scoped MCP servers",
};

const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".gemini/AGENTS.md",
    project: Some("AGENTS.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "User instructions",
};

pub static ADAPTER: Adapter = Adapter {
    id: "antigravity",
    name: "Antigravity CLI",
    binary: "agy",
    experimental: false,
    platforms: &["linux", "macos", "windows"],
    // Conservative floor: the documented config paths exist since the 1.x line.
    min_version: "1.0.0",
    capabilities: &[
        Capability::Instructions,
        Capability::McpStdio,
        Capability::McpRemote,
        Capability::Skills,
    ],
    files: &[MCP, INSTRUCTIONS],
    render: Some(render),
};

fn render(desired: &Desired, current: &Current, plan: &mut RenderPlan) -> Result<(), MusterError> {
    render_mcp(desired, current.get(MCP.path), plan)?;
    if let Some(text) = desired.instructions {
        plan.files.push(RenderedFile {
            file: &INSTRUCTIONS,
            contents: markdown_block(current.get(INSTRUCTIONS.path), text),
        });
    }
    for (skill, files) in desired.skills {
        for (rel, contents) in files {
            plan.tree.push(RenderedTreeFile {
                base: Base::Home,
                path: format!("{SKILLS_DIR}/{skill}/{rel}"),
                // No documented project-scoped skills directory.
                project: None,
                contents: contents.clone(),
            });
        }
    }
    Ok(())
}

fn render_mcp(
    desired: &Desired,
    current: Option<&String>,
    plan: &mut RenderPlan,
) -> Result<(), MusterError> {
    let mut object = json_object(current, MCP.path)?;
    let servers = child_object(&mut object, "mcpServers");

    for (name, server) in desired.mcp {
        let mut entry = Map::new();
        match transport(name, server)? {
            Transport::Stdio { command } => {
                entry.insert("command".into(), Value::String(command.to_string()));
                if !server.args.is_empty() {
                    let args: Vec<Value> = server.args.iter().cloned().map(Value::String).collect();
                    entry.insert("args".into(), Value::Array(args));
                }
                let env =
                    resolve_mcp_map(&server.env, desired.secrets, "Antigravity CLI", name, plan);
                if !env.is_empty() {
                    entry.insert("env".into(), string_map(env));
                }
            }
            Transport::Remote { url } => {
                // Antigravity accepts only `serverUrl` for remote servers.
                entry.insert("serverUrl".into(), Value::String(url.to_string()));
                let headers = resolve_mcp_map(
                    &server.headers,
                    desired.secrets,
                    "Antigravity CLI",
                    name,
                    plan,
                );
                if !headers.is_empty() {
                    entry.insert("headers".into(), string_map(headers));
                }
            }
        }
        servers.insert(name.clone(), Value::Object(entry));
    }

    plan.files.push(RenderedFile {
        file: &MCP,
        contents: json_text(object),
    });
    Ok(())
}

fn string_map(map: std::collections::BTreeMap<String, String>) -> Value {
    Value::Object(
        map.into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
    )
}
