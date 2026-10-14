//! `muster plan` (show the exact diff, never write) and `muster rollback <n>`.

use std::path::PathBuf;

use serde_json::Value;

use crate::context::Context;
use crate::engine::{self, Options, Target};
use crate::errors::MusterError;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

/// `--project DIR`, shared by `plan` and `sync`.
pub const PROJECT_PARAM: ParamSpec = ParamSpec {
    name: "project",
    about: "Apply the project layer in DIR/.muster/muster.toml to DIR's project files",
    kind: ParamKind::String,
    required: false,
    positional: false,
};

pub const SPEC: CommandSpec = CommandSpec {
    name: "plan",
    about: "Show what sync would change (exact diff, secrets redacted) without writing",
    params: &[PROJECT_PARAM],
    mutating: false,
    handler: plan,
};

pub const ROLLBACK_SPEC: CommandSpec = CommandSpec {
    name: "rollback",
    about: "Restore the managed files of a previous generation",
    params: &[ParamSpec {
        name: "generation",
        about: "Generation number to restore",
        kind: ParamKind::Int,
        required: true,
        positional: true,
    }],
    mutating: true,
    handler: rollback,
};

/// Target from `--project`.
pub fn target(params: &Params) -> Result<Target, MusterError> {
    match params.get_string("project") {
        None => Ok(Target::Global),
        Some(dir) => {
            let dir = PathBuf::from(dir);
            let dir = dir.canonicalize().map_err(|e| {
                MusterError::Usage(format!("project directory {}: {e}", dir.display()))
            })?;
            Ok(Target::Project(dir))
        }
    }
}

fn plan(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let options = Options {
        target: target(params)?,
        agent: None,
    };
    let plan = engine::plan(ctx, &options)?;
    Ok(plan.to_json(true, false, plan.base_generation()))
}

fn rollback(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let n = params
        .get_int("generation")
        .filter(|n| *n > 0)
        .ok_or_else(|| MusterError::Usage("generation must be a positive number".into()))?;
    engine::rollback(ctx, n as u64)
}
