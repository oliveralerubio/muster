//! Experimental Nous Research Hermes Agent adapter.
//!
//! Hermes uses config.yaml (`mcp_servers`, `model.default`), global SOUL.md,
//! project AGENTS.md, and Agent Skills directories. Its runtime delegation
//! does not provide a portable custom-subagent definition format.
//! See docs/SPEC.md for sources and scope limitations.

use yaml_rust2::{yaml::Hash, Yaml};

use super::{
    markdown_block, push_skills, resolve_mcp_map, transport, yaml, Adapter, Base, Capability,
    Current, Desired, FileFormat, ManagedFile, RenderPlan, RenderedFile, Scope, Transport,
};
use crate::errors::MusterError;

const CONFIG: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".hermes/config.yaml",
    // Hermes has no project config.yaml overlay. Never invent one.
    project: None,
    scope: Scope::User,
    format: FileFormat::Yaml,
    purpose: "Experimental Hermes MCP servers and default model (user scope only)",
};
const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".hermes/SOUL.md",
    project: Some("AGENTS.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "Hermes global identity/instructions; project AGENTS.md",
};

pub static ADAPTER: Adapter = Adapter {
    id: "hermes",
    name: "Hermes Agent",
    binary: "hermes",
    experimental: true,
    platforms: &["linux", "macos"],
    // Hermes publishes checkout identities rather than a stable semver floor.
    min_version: "0.0.0",
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
    let mut config = yaml::mapping(
        current.get(CONFIG.path).map_or("", String::as_str),
        CONFIG.path,
    )?;
    let servers = yaml::child(&mut config, "mcp_servers", CONFIG.path)?;
    for (name, server) in desired.mcp {
        let mut entry = Hash::new();
        match transport(name, server)? {
            Transport::Stdio { command } => {
                entry.insert(yaml::key("command"), yaml::key(command));
                entry.insert(
                    yaml::key("args"),
                    Yaml::Array(server.args.iter().map(|s| yaml::key(s)).collect()),
                );
                let env = resolve_mcp_map(&server.env, desired, &ADAPTER, name, plan);
                entry.insert(yaml::key("env"), string_map(env));
            }
            Transport::Remote { url } => {
                entry.insert(yaml::key("url"), yaml::key(url));
                let headers = resolve_mcp_map(&server.headers, desired, &ADAPTER, name, plan);
                entry.insert(yaml::key("headers"), string_map(headers));
            }
        }
        servers.insert(yaml::key(name), Yaml::Hash(entry));
    }
    if let Some(model) = desired.agents.models.get(ADAPTER.id) {
        // Hermes also accepts the legacy scalar form of `model`.
        if matches!(config.get(&yaml::key("model")), Some(Yaml::String(_))) {
            config.insert(yaml::key("model"), Yaml::Hash(Hash::new()));
        }
        yaml::child(&mut config, "model", CONFIG.path)?
            .insert(yaml::key("default"), yaml::key(model));
    }
    plan.files.push(RenderedFile {
        file: &CONFIG,
        contents: yaml::merge(current.get(CONFIG.path).map_or("", String::as_str), config),
    });
    if let Some(text) = desired.instructions {
        plan.files.push(RenderedFile {
            file: &INSTRUCTIONS,
            contents: markdown_block(current.get(INSTRUCTIONS.path), text),
        });
    }
    push_skills(
        desired,
        Base::Home,
        ".hermes/skills",
        ".hermes/skills",
        plan,
    );
    Ok(())
}

fn string_map(map: std::collections::BTreeMap<String, String>) -> Yaml {
    Yaml::Hash(
        map.into_iter()
            .map(|(k, v)| (yaml::key(&k), yaml::key(&v)))
            .collect(),
    )
}
