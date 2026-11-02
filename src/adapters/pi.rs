//! Pi adapter (`pi`).
//!
//! Managed files under `$HOME/.pi/agent/`:
//! - `settings.json`: `defaultModel`.
//! - `AGENTS.md`: canonical instructions.

use serde_json::Value;

use super::{
    json_object, json_text, markdown_block, Adapter, Base, Capability, Current, Desired,
    FileFormat, ManagedFile, RenderPlan, RenderedFile, Scope,
};
use crate::errors::MusterError;

const SETTINGS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".pi/agent/settings.json",
    project: Some(".pi/agent/settings.json"),
    scope: Scope::User,
    format: FileFormat::Json,
    purpose: "Pi default model",
};

const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".pi/agent/AGENTS.md",
    project: Some("AGENTS.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "User instructions",
};

pub static ADAPTER: Adapter = Adapter {
    id: "pi",
    name: "Pi",
    experimental: true,
    platforms: &["linux", "macos", "windows"],
    min_version: "",
    capabilities: &[Capability::Instructions, Capability::Models],
    files: &[SETTINGS, INSTRUCTIONS],
    render: Some(render),
};

fn render(desired: &Desired, current: &Current, plan: &mut RenderPlan) -> Result<(), MusterError> {
    if let Some(model) = desired.agents.models.get("pi") {
        let mut object = json_object(current.get(SETTINGS.path), SETTINGS.path)?;
        object.insert("defaultModel".into(), Value::String(model.clone()));
        plan.files.push(RenderedFile {
            file: &SETTINGS,
            contents: json_text(object),
        });
    }
    if let Some(text) = desired.instructions {
        plan.files.push(RenderedFile {
            file: &INSTRUCTIONS,
            contents: markdown_block(current.get(INSTRUCTIONS.path), text),
        });
    }
    Ok(())
}
