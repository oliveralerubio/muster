use serde_json::{json, Value};

use crate::context::Context;
use crate::engine::{self, layer};
use crate::errors::MusterError;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "why",
    about: "Find where an agent instruction came from",
    params: &[ParamSpec { name: "text", about: "Text to find", kind: ParamKind::String, required: true, positional: true }],
    mutating: false,
    handler: why,
};

fn why(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let layers = layer::global_layers(&ctx.config_dir, &manifest, crate::machine::load_name(&ctx.state_dir).as_deref());
    let merged = layer::merge(&layers);
    let agents: Vec<_> = engine::enabled_adapters(Some(&merged.agents))?.iter().map(|a| a.id).collect();
    let source_layer = layers.iter().rev().find(|l| l.agents.instructions.is_some()).or_else(|| layers.first()).map_or("team", |l| l.kind);
    let path = merged.instructions_path()?.0;
    let query = params.get_string("text").unwrap_or_default().to_lowercase();
    let mut matches = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for (line, excerpt) in text.lines().enumerate() {
            if excerpt.to_lowercase().contains(&query) {
                matches.push(json!({"file": path, "line": line + 1, "excerpt": excerpt.trim(), "kind": "instructions", "layer": source_layer, "agents": agents, "managed": true}));
            }
        }
    }
    for agent in &agents {
        let overlay = path.with_file_name(format!("AGENTS.{agent}.md"));
        if let Ok(text) = std::fs::read_to_string(&overlay) {
            for (line, excerpt) in text.lines().enumerate() {
                if excerpt.to_lowercase().contains(&query) {
                    matches.push(json!({"file": overlay, "line": line + 1, "excerpt": excerpt.trim(), "kind": "overlay", "layer": source_layer, "agents": [agent], "managed": true}));
                }
            }
        }
    }
    Ok(json!({"matches": matches}))
}
