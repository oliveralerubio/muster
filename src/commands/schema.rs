//! `muster schema`: export the manifest JSON Schema, command registry and config path.

use serde_json::json;

use crate::context::Context;
use crate::errors::MusterError;
use crate::manifest;
use crate::registry::{self, CommandSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "schema",
    about: "Export the manifest JSON Schema and the command list",
    params: &[],
    mutating: false,
    handler: run,
};

fn run(ctx: &mut Context, _params: &Params) -> Result<serde_json::Value, MusterError> {
    Ok(json!({
        "manifest_schema": manifest::json_schema(),
        "commands": registry::commands_json(),
        "config_path": ctx.config_dir,
    }))
}
