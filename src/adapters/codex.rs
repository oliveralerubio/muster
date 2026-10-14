//! OpenAI Codex CLI adapter (`codex`).
//!
//! Managed files:
//! - `~/.codex/config.toml`: `model`, `[mcp_servers.<name>]`. Uses `toml_edit`
//!   to preserve formatting, comments and unmanaged sections.
//! - `~/.codex/AGENTS.md`: canonical instructions.
//! - `~/.agents/skills/<skill>/...` (project `.agents/skills/`, shared with
//!   Antigravity): Agent Skills declared in `[agents.skills]`.

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use super::{
    markdown_block, push_skills, resolve_mcp_map, transport, Adapter, Base, Capability, Current,
    Desired, FileFormat, ManagedFile, RenderPlan, RenderedFile, Scope, Transport,
};
use crate::errors::MusterError;

/// Cross-agent skills directory, the same in `$HOME` and in a project.
const SKILLS_DIR: &str = ".agents/skills";

const CONFIG: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".codex/config.toml",
    project: Some(".codex/config.toml"),
    scope: Scope::User,
    format: FileFormat::Toml,
    purpose: "Codex CLI configuration and MCP servers",
};

const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".codex/AGENTS.md",
    project: Some("AGENTS.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "User instructions",
};

pub static ADAPTER: Adapter = Adapter {
    id: "codex",
    name: "Codex",
    binary: "codex",
    experimental: false,
    platforms: &["linux", "macos", "windows"],
    min_version: "1.0.0",
    capabilities: &[
        Capability::Instructions,
        Capability::McpStdio,
        Capability::McpRemote,
        Capability::Models,
        Capability::Skills,
    ],
    files: &[CONFIG, INSTRUCTIONS],
    render: Some(render),
};

fn render(desired: &Desired, current: &Current, plan: &mut RenderPlan) -> Result<(), MusterError> {
    render_config(desired, current.get(CONFIG.path), plan)?;
    render_instructions(desired, current.get(INSTRUCTIONS.path), plan);
    push_skills(desired, Base::Home, SKILLS_DIR, SKILLS_DIR, plan);
    Ok(())
}

fn render_config(
    desired: &Desired,
    current: Option<&String>,
    plan: &mut RenderPlan,
) -> Result<(), MusterError> {
    let mut doc: DocumentMut = match current {
        Some(text) if !text.trim().is_empty() => {
            text.parse().map_err(|e| MusterError::InvalidConfig {
                message: format!(
                    "cannot merge into existing .codex/config.toml: {e}; file left untouched"
                ),
                path: Some(CONFIG.path.into()),
            })?
        }
        _ => DocumentMut::new(),
    };

    if let Some(model) = desired.agents.models.get("codex") {
        doc["model"] = toml_edit::value(model.clone());
    }

    if !desired.mcp.is_empty() {
        if !doc.contains_key("mcp_servers") {
            let mut table = Table::new();
            table.set_implicit(true);
            doc.insert("mcp_servers", Item::Table(table));
        }

        let Some(mcp) = doc["mcp_servers"].as_table_mut() else {
            return Err(MusterError::InvalidConfig {
                message: "cannot merge into existing .codex/config.toml: `mcp_servers` is not a \
                          table; file left untouched"
                    .into(),
                path: Some(CONFIG.path.into()),
            });
        };
        {
            for (name, server) in desired.mcp {
                let mut table = Table::new();
                match transport(name, server)? {
                    Transport::Stdio { command } => {
                        table.insert("command", toml_edit::value(command.to_string()));
                        if !server.args.is_empty() {
                            let mut arr = Array::new();
                            for arg in &server.args {
                                arr.push(arg.clone());
                            }
                            table.insert("args", Item::Value(Value::Array(arr)));
                        }
                        let env = resolve_mcp_map(&server.env, desired, &ADAPTER, name, plan);
                        if !env.is_empty() {
                            let mut inline = InlineTable::new();
                            for (k, v) in env {
                                inline.insert(k, v.into());
                            }
                            table.insert("env", Item::Value(Value::InlineTable(inline)));
                        }
                    }
                    Transport::Remote { url } => {
                        table.insert("url", toml_edit::value(url.to_string()));
                        let headers =
                            resolve_mcp_map(&server.headers, desired, &ADAPTER, name, plan);
                        if !headers.is_empty() {
                            let mut inline = InlineTable::new();
                            for (k, v) in headers {
                                inline.insert(k, v.into());
                            }
                            table.insert("http_headers", Item::Value(Value::InlineTable(inline)));
                        }
                    }
                }
                mcp.insert(name, Item::Table(table));
            }
        }
    }

    plan.files.push(RenderedFile {
        file: &CONFIG,
        contents: doc.to_string(),
    });
    Ok(())
}

fn render_instructions(desired: &Desired, current: Option<&String>, plan: &mut RenderPlan) {
    if let Some(text) = desired.instructions {
        plan.files.push(RenderedFile {
            file: &INSTRUCTIONS,
            contents: markdown_block(current, text),
        });
    }
}
