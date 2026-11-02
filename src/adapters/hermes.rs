//! Hermes Agent adapter (`hermes`).
//!
//! Managed file: `$HOME/.hermes/SOUL.md` (canonical instructions).

use super::{
    markdown_block, Adapter, Base, Capability, Current, Desired, FileFormat, ManagedFile,
    RenderPlan, RenderedFile, Scope,
};
use crate::errors::MusterError;

const INSTRUCTIONS: ManagedFile = ManagedFile {
    base: Base::Home,
    path: ".hermes/SOUL.md",
    project: Some(".hermes/SOUL.md"),
    scope: Scope::User,
    format: FileFormat::Markdown,
    purpose: "User instructions",
};

pub static ADAPTER: Adapter = Adapter {
    id: "hermes",
    name: "Hermes Agent",
    experimental: true,
    platforms: &["linux", "macos", "windows"],
    min_version: "",
    capabilities: &[Capability::Instructions, Capability::Models],
    files: &[INSTRUCTIONS],
    render: Some(render),
};

fn render(desired: &Desired, current: &Current, plan: &mut RenderPlan) -> Result<(), MusterError> {
    if let Some(text) = desired.instructions {
        plan.files.push(RenderedFile {
            file: &INSTRUCTIONS,
            contents: markdown_block(current.get(INSTRUCTIONS.path), text),
        });
    }
    Ok(())
}
