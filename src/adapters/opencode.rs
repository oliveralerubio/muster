//! OpenCode adapter (`opencode`).
//!
//! Managed files (under `$XDG_CONFIG_HOME/opencode/`):
//! - `opencode.json`: `model`, `mcp.<name>` (`type: local|remote`).
//! - `AGENTS.md`: canonical instructions.
//! - `skills/<skill>/...` (project `.opencode/skills/`): Agent Skills declared
//!   in `[agents.skills]`.
//! - `agents/<name>.md` (project `.opencode/agents/`): custom subagents
//!   declared in `[agents.subagents]`, translated to OpenCode's Markdown
//!   agent files (`mode: subagent`; the file name is the agent name).
//!   Fields with no equivalent (`tools` — OpenCode gates tools with
//!   `permission`, not a list — and any non-canonical frontmatter key) are
//!   skipped with a warning; generated frontmatter is serialized as YAML.

use serde_json::{Map, Value};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter};

use super::{
    child_object, json_object, json_text, markdown_block, push_skills, resolve_mcp_map, transport,
    untranslated_field, Adapter, Base, Capability, Current, Desired, FileFormat, ManagedFile,
    RenderPlan, RenderedFile, Scope, Transport,
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

const SKILLS_DIR: &str = "opencode/skills";
const PROJECT_SKILLS_DIR: &str = ".opencode/skills";

const AGENTS_DIR: &str = "opencode/agents";
const PROJECT_AGENTS_DIR: &str = ".opencode/agents";

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
        Capability::AiProviders,
        Capability::Skills,
        Capability::Subagents,
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

    if let Some(provider) = super::selected_ai_provider(desired, &ADAPTER, plan) {
        let id = provider.id;
        let ai_providers = child_object(&mut object, "provider");
        let entry = child_object(ai_providers, id);
        let npm = match provider.kind {
            super::config::AiProviderKind::Anthropic => "@ai-sdk/anthropic",
            super::config::AiProviderKind::Openai => "@ai-sdk/openai",
            super::config::AiProviderKind::Google => "@ai-sdk/google",
            _ => "@ai-sdk/openai-compatible",
        };
        entry.insert("npm".into(), npm.into());
        let options = child_object(entry, "options");
        if let Some(url) = provider.base_url {
            options.insert("baseURL".into(), url.into());
        }
        if let Some(var) = provider.env_key {
            options.insert("apiKey".into(), format!("{{env:{var}}}").into());
        } else if let Some(key) = provider.key {
            options.insert("apiKey".into(), key.expose().into());
        }
        if let Some(model) = provider.default_model {
            object.insert("model".into(), format!("{id}/{model}").into());
        }
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
                    let env = resolve_mcp_map(&server.env, desired, &ADAPTER, name, plan);
                    if !env.is_empty() {
                        entry.insert("environment".into(), string_map(env));
                    }
                }
                Transport::Remote { url } => {
                    entry.insert("type".into(), "remote".into());
                    entry.insert("url".into(), url.into());
                    let headers = resolve_mcp_map(&server.headers, desired, &ADAPTER, name, plan);
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
    push_skills(
        desired,
        Base::XdgConfig,
        SKILLS_DIR,
        PROJECT_SKILLS_DIR,
        plan,
    );
    for subagent in desired.subagents.values() {
        warn_untranslated(subagent, plan);
        // Serialize with yaml-rust2 so any description or model (quotes,
        // colons, multi-line block scalars) stays valid YAML.
        let mut frontmatter = Hash::new();
        frontmatter.insert(
            Yaml::String("description".into()),
            Yaml::String(subagent.description.clone()),
        );
        frontmatter.insert(Yaml::String("mode".into()), Yaml::String("subagent".into()));
        if let Some(model) = &subagent.model {
            frontmatter.insert(Yaml::String("model".into()), Yaml::String(model.clone()));
        }
        let mut rendered = String::new();
        {
            let mut emitter = YamlEmitter::new(&mut rendered);
            emitter.multiline_strings(true);
            emitter.dump(&Yaml::Hash(frontmatter)).map_err(|e| {
                MusterError::Operational(format!(
                    "cannot render OpenCode agent `{}`: {e}",
                    subagent.name
                ))
            })?;
        }
        // `YamlEmitter` writes its own `---` document marker (and no trailing
        // newline); the file provides both.
        let yaml = rendered.strip_prefix("---\n").unwrap_or(&rendered);
        plan.tree.push(super::RenderedTreeFile {
            base: Base::XdgConfig,
            path: format!("{AGENTS_DIR}/{}.md", subagent.name),
            project: Some(format!("{PROJECT_AGENTS_DIR}/{}.md", subagent.name)),
            contents: format!("---\n{yaml}\n---\n\n{}\n", subagent.body.trim()),
        });
    }
    Ok(())
}

/// Fields the canonical format defines that this adapter cannot carry over:
/// `tools` (OpenCode gates tools with `permission`, not a list) and any
/// non-canonical frontmatter key.
fn warn_untranslated(subagent: &super::Subagent, plan: &mut RenderPlan) {
    if !subagent.tools.is_empty() {
        untranslated_field(
            &ADAPTER,
            &subagent.name,
            "tools",
            "OpenCode agents gate tools with `permission`, not a tool list",
            plan,
        );
    }
    for field in &subagent.extra_fields {
        untranslated_field(
            &ADAPTER,
            &subagent.name,
            field,
            "no equivalent in OpenCode agent files",
            plan,
        );
    }
}

fn string_map(map: std::collections::BTreeMap<String, String>) -> Value {
    Value::Object(
        map.into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect(),
    )
}
