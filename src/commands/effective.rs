//! Read-only view of the configuration applied to this machine: an adapter
//! over [`crate::engine::effective`], which owns the computation. This
//! command parses the parameters, hands the skill listing to the engine and
//! serializes the result; every precedence and scrubbing rule lives in the
//! engine module.

use crate::context::Context;
use crate::engine::effective::{self, Inputs};
use crate::errors::MusterError;
use crate::registry::{CommandSpec, Params};
use serde_json::Value;

pub const SPEC: CommandSpec = CommandSpec {
    name: "effective",
    about: "Show the effective configuration and its sources",
    params: &[
        crate::commands::plan::PROJECT_PARAM,
        crate::registry::ParamSpec {
            name: "agent",
            about: "Restrict output to one adapter",
            kind: crate::registry::ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: false,
    handler: effective,
};

fn effective(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    // The skills domain owns the skill views; the engine adds the effective
    // state on top of them.
    let skill_listing = (crate::commands::skill::LIST.handler)(ctx, &Params::default())?;
    let project = match crate::commands::plan::target(params)? {
        crate::engine::Target::Project(dir) => Some(dir),
        crate::engine::Target::Global => None,
    };
    let config = effective::compute(&Inputs {
        config_dir: &ctx.config_dir,
        state_dir: &ctx.state_dir,
        agent: params.get_string("agent"),
        project: project.as_deref(),
        skill_listing: &skill_listing,
    })?;
    let result = config.to_json();
    if ctx.format == crate::output::Format::Text {
        Ok(Value::String(format_text(&result)))
    } else {
        Ok(result)
    }
}

fn format_text(result: &Value) -> String {
    let mut text = String::new();
    text.push_str("Layers\n");
    if let Some(layers) = result["layers"].as_array() {
        for layer in layers {
            text.push_str(&format!(
                "  {}: {}\n",
                layer["kind"].as_str().unwrap_or(""),
                layer["name"].as_str().unwrap_or("")
            ));
        }
    }
    for (heading, key) in [
        ("Instructions", "instructions"),
        ("Skills", "skills"),
        ("MCP", "mcp"),
        ("Subagents", "subagents"),
        ("AI providers", "providers"),
    ] {
        text.push_str(&format!("{heading}\n"));
        let Some(items) = result[key].as_array() else {
            continue;
        };
        for item in items {
            let name = item["name"]
                .as_str()
                .or_else(|| item["source"].as_str())
                .or_else(|| item["id"].as_str())
                .unwrap_or("");
            // Provenance: the supplying layer plus per-kind detail.
            let mut parts: Vec<String> = Vec::new();
            if let Some(kind) = item["layer"].as_str() {
                parts.push(kind.to_owned());
            }
            match key {
                "instructions" => {
                    if let Some(tokens) = item["approx_tokens"].as_u64() {
                        parts.push(format!("~{tokens} tokens"));
                    }
                }
                "skills" => {
                    parts.push(
                        item["verification"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_owned(),
                    );
                    parts.push(format!(
                        "~{} description tokens",
                        item["description_tokens"].as_u64().unwrap_or(0)
                    ));
                    parts.push(format!(
                        "~{} body tokens",
                        item["body_tokens"].as_u64().unwrap_or(0)
                    ));
                }
                "mcp" => {
                    if let Some(transport) = item["transport"].as_str() {
                        parts.push(transport.to_owned());
                    }
                    let overridden: Vec<_> = item["overridden_layers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    if !overridden.is_empty() {
                        parts.push(format!("overrides {}", overridden.join(", ")));
                    }
                }
                "providers" => {
                    // Metadata and origin only; `key` is a reference, not a value.
                    if let Some(kind) = item["kind"].as_str() {
                        parts.push(kind.to_owned());
                    }
                    if let Some(model) = item["default_model"].as_str() {
                        parts.push(format!("default {model}"));
                    }
                    if let Some(key) = item["key"].as_str() {
                        parts.push(format!("key {key}"));
                    }
                }
                _ => {}
            }
            let provenance = if parts.is_empty() {
                String::new()
            } else {
                format!(" ({})", parts.join(", "))
            };
            text.push_str(&format!("  {name}{provenance}\n"));
            if let Some(adapters) = item["adapters"].as_object() {
                for (id, view) in adapters {
                    text.push_str(&format!(
                        "    {id}: {}\n",
                        view["state"].as_str().unwrap_or("unknown")
                    ));
                }
            }
        }
    }
    text.push_str("Totals\n");
    for (id, tokens) in result["totals"].as_object().unwrap() {
        text.push_str(&format!("  {id}: {tokens} tokens\n"));
    }
    text
}

/// Skills directory of each adapter in user scope (or in `project`). Kept as
/// a delegation for `skill list`, which reports unmanaged skills from it.
pub fn skill_roots(
    config_dir: &std::path::Path,
    adapters: &[&'static crate::adapters::Adapter],
    roots: &crate::engine::Roots,
    project: Option<&std::path::Path>,
) -> Result<std::collections::BTreeMap<&'static str, std::path::PathBuf>, MusterError> {
    effective::skill_roots(config_dir, adapters, roots, project)
}
