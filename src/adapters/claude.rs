//! Claude Code adapter (`claude`).
//!
//! Managed files:
//! - `~/.claude/settings.json`: `model`, `permissions` (`allow`, `ask`, `deny`),
//!   `hooks` (`PreToolUse`, `Stop`...).
//! - `~/.claude.json`: `mcpServers.<name>`.
//! - `~/.claude/AGENTS.md` + `~/.claude/CLAUDE.md` (managed block `@AGENTS.md`).
//! - `~/.claude/skills/<skill>/...` (project `.claude/skills/`): Agent Skills
//!   declared in `[agents.skills]`, one whole file per skill file.
//! - `~/.claude/agents/<name>.md` (project `.claude/agents/`): custom
//!   subagents declared in `[agents.subagents]`, byte-exact (the canonical
//!   format is Claude Code's native subagent format).

use serde_json::{Map, Value};

use super::{
    child_object, json_object, json_text, markdown_block, pascal_case, push_skills,
    push_subagents_raw, resolve_mcp_map, transport, Adapter, Base, Capability, Current, Desired,
    FileFormat, ManagedFile, RenderPlan, RenderedFile, Scope, Transport,
};
use crate::errors::MusterError;

const SKILLS_DIR: &str = ".claude/skills";
const AGENTS_DIR: &str = ".claude/agents";

const SETTINGS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".claude/settings.json",
    project: Some(".claude/settings.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "Claude Code settings (model, permissions, hooks)",
};

const MCP: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".claude.json",
    project: Some(".mcp.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "User-scoped MCP servers",
};

const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".claude/CLAUDE.md",
    project: Some("CLAUDE.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "User instructions; imports AGENTS.md",
};

const AGENTS_COPY: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".claude/AGENTS.md",
    project: Some("AGENTS.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "Copy of the canonical AGENTS.md imported by CLAUDE.md",
};

pub static ADAPTER: Adapter = Adapter {
    id: "claude",
    name: "Claude Code",
    binary: "claude",
    experimental: false,
    platforms: &["linux", "macos", "windows"],
    min_version: "1.0.0",
    capabilities: &[
        Capability::Instructions,
        Capability::McpStdio,
        Capability::McpRemote,
        Capability::Models,
        Capability::Providers,
        Capability::Permissions,
        Capability::Hooks,
        Capability::Skills,
        Capability::Subagents,
    ],
    files: &[SETTINGS, MCP, INSTRUCTIONS, AGENTS_COPY],
    render: Some(render),
};

fn render(desired: &Desired, current: &Current, plan: &mut RenderPlan) -> Result<(), MusterError> {
    render_settings(desired, current.get(SETTINGS.path), plan)?;
    render_mcp(desired, current.get(MCP.path), plan)?;
    render_instructions(desired, current, plan);
    push_skills(desired, Base::Home, SKILLS_DIR, SKILLS_DIR, plan);
    push_subagents_raw(desired, Base::Home, AGENTS_DIR, AGENTS_DIR, plan);
    Ok(())
}

fn render_settings(
    desired: &Desired,
    current: Option<&String>,
    plan: &mut RenderPlan,
) -> Result<(), MusterError> {
    let mut object = json_object(current, SETTINGS.path)?;

    if let Some((_, provider)) = super::selected_provider(desired, &ADAPTER, plan) {
        let env = child_object(&mut object, "env");
        if let Some(url) = &provider.base_url {
            env.insert("ANTHROPIC_BASE_URL".into(), url.clone().into());
        }
        if let Some(key) = provider.key.as_ref().and_then(|k| desired.secrets.get(k)) {
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), key.expose().into());
        }
        if let Some(model) = &provider.default_model {
            object.insert("model".into(), model.clone().into());
        }
    }

    if let Some(model) = desired.agents.models.get("claude") {
        object.insert("model".into(), Value::String(model.clone()));
    }

    if let Some(perms) = &desired.agents.permissions {
        let permissions = child_object(&mut object, "permissions");
        for (key, list) in [
            ("allow", &perms.allow),
            ("ask", &perms.ask),
            ("deny", &perms.deny),
        ] {
            if !list.is_empty() {
                let items: Vec<Value> = list.iter().cloned().map(Value::String).collect();
                permissions.insert(key.into(), Value::Array(items));
            }
        }
    }

    if !desired.agents.hooks.is_empty() {
        let hooks = child_object(&mut object, "hooks");
        // Native shape: {"PreToolUse": [{"matcher": "...", "hooks": [{"type": "command", "command": "..."}]}]}.
        // The managed unit is the whole event entry.
        for (event, hook) in &desired.agents.hooks {
            let mut command = Map::new();
            command.insert("type".into(), "command".into());
            command.insert("command".into(), hook.command.clone().into());
            let mut entry = Map::new();
            if let Some(matcher) = &hook.matcher {
                entry.insert("matcher".into(), matcher.clone().into());
            }
            entry.insert("hooks".into(), Value::Array(vec![Value::Object(command)]));
            hooks.insert(pascal_case(event), Value::Array(vec![Value::Object(entry)]));
        }
    }

    plan.files.push(RenderedFile {
        file: &SETTINGS,
        contents: json_text(object),
    });
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
                    let map: Map<String, Value> = env
                        .into_iter()
                        .map(|(k, v)| (k, Value::String(v)))
                        .collect();
                    entry.insert("env".into(), Value::Object(map));
                }
            }
            Transport::Remote { url } => {
                entry.insert("type".into(), Value::String("http".into()));
                entry.insert("url".into(), Value::String(url.to_string()));
                let headers = resolve_mcp_map(&server.headers, desired, &ADAPTER, name, plan);
                if !headers.is_empty() {
                    let map: Map<String, Value> = headers
                        .into_iter()
                        .map(|(k, v)| (k, Value::String(v)))
                        .collect();
                    entry.insert("headers".into(), Value::Object(map));
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

/// Claude Code does not read AGENTS.md, so the canonical text is copied next
/// to CLAUDE.md and CLAUDE.md imports it (`@AGENTS.md` resolves relative to
/// the importing file).
fn render_instructions(desired: &Desired, current: &Current, plan: &mut RenderPlan) {
    let Some(text) = desired.instructions else {
        return;
    };
    plan.files.push(RenderedFile {
        file: &AGENTS_COPY,
        contents: markdown_block(current.get(AGENTS_COPY.path), text),
    });
    plan.files.push(RenderedFile {
        file: &INSTRUCTIONS,
        contents: markdown_block(current.get(INSTRUCTIONS.path), "@AGENTS.md"),
    });
}
