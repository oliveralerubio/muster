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
//! - `~/.gemini/antigravity-cli/skills/<skill>/...` (project `.agents/skills/`,
//!   shared with Codex): Agent Skills declared in `[agents.skills]`, one whole
//!   file per skill file. Skills Muster does not declare are left alone.
//! - `~/.gemini/antigravity-cli/agents/<name>.md` (project `.agents/agents/`):
//!   custom subagents declared in `[agents.subagents]`, byte-exact (the
//!   canonical Markdown format is the Antigravity custom-agent format).
//!
//! Model, permissions and hooks have no documented configuration, so they are
//! not declared and the framework warns that they are skipped.

use serde_json::{Map, Value};

use super::{
    child_object, json_object, json_text, markdown_block, push_skills, push_subagents_raw,
    resolve_mcp_map, transport, Adapter, Base, Capability, Current, Desired, FileFormat,
    ManagedFile, RenderPlan, RenderedFile, Scope, Transport,
};
use crate::errors::MusterError;

const SKILLS_DIR: &str = ".gemini/antigravity-cli/skills";
/// Workspace skills directory, shared with Codex.
const PROJECT_SKILLS_DIR: &str = ".agents/skills";

const AGENTS_DIR: &str = ".gemini/antigravity-cli/agents";
/// Workspace custom-subagents directory.
const PROJECT_AGENTS_DIR: &str = ".agents/agents";

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
        Capability::Subagents,
    ],
    files: &[MCP, INSTRUCTIONS],
    provider_kinds: &[],
    runtime_key_helper: false,
    login_kind: None,
    provider_diagnostics: |_, _| Vec::new(),
    sensitive_values: |_, _| Default::default(),
    context_roots: super::ContextRoots {
        base: Base::Home,
        skills: SKILLS_DIR,
        project_skills: PROJECT_SKILLS_DIR,
        subagents: Some(AGENTS_DIR),
        project_subagents: Some(PROJECT_AGENTS_DIR),
    },
    shared_project_instructions: true,
    render_project: None,
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
    push_skills(desired, Base::Home, SKILLS_DIR, PROJECT_SKILLS_DIR, plan);
    push_subagents_raw(desired, Base::Home, AGENTS_DIR, PROJECT_AGENTS_DIR, plan);
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
                let env = resolve_mcp_map(&server.env, desired, &ADAPTER, name, plan);
                if !env.is_empty() {
                    entry.insert("env".into(), string_map(env));
                }
            }
            Transport::Remote { url } => {
                // Antigravity accepts only `serverUrl` for remote servers.
                entry.insert("serverUrl".into(), Value::String(url.to_string()));
                let headers = resolve_mcp_map(&server.headers, desired, &ADAPTER, name, plan);
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
