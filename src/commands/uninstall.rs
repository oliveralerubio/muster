//! `muster uninstall`: remove what Muster owns on this machine (scheduler
//! units and local state) and leave every agent file exactly as the last
//! generation applied it, so the agents keep working without Muster.
//!
//! The config repository is never touched: it is the user's data (and a git
//! repo they may share with other machines).

use std::path::Path;

use serde_json::{json, Value};

use crate::context::Context;
use crate::engine::store;
use crate::errors::MusterError;
use crate::registry::{CommandSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "uninstall",
    about: "Remove Muster's scheduler units and local state; agent files stay as last applied",
    params: &[],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    ctx.confirm(
        "Uninstall Muster from this machine? Its identity and local state are deleted \
         (re-enroll to read the vault again); agent files are left as last applied",
    )?;

    // Read the last generation before its state is deleted.
    let latest = store::latest(&ctx.state_dir)?;
    let generation = latest.as_ref().map(|(n, _)| *n);
    let agent_files: Vec<String> = latest
        .map(|(_, state)| state.files.into_keys().collect())
        .unwrap_or_default();

    let units = crate::commands::schedule::remove_units(ctx)?;
    let state_dir = ctx.state_dir.clone();
    let state_exists = state_dir.exists();
    if state_exists && !ctx.dry_run {
        remove_state(&state_dir)?;
    }

    ctx.progress(&format!(
        "Agent files were left as generation {} applied them; Muster no longer manages them.",
        generation.map_or_else(|| "none".to_string(), |n| n.to_string())
    ));
    Ok(json!({
        "uninstalled": !ctx.dry_run,
        "dry_run": ctx.dry_run,
        "removed": {
            "scheduler_units": units,
            "state_dir": state_exists.then_some(&state_dir),
        },
        "agent_files_preserved": true,
        "kept": {
            "agent_files": agent_files,
            "generation": generation,
            "config_repo": ctx.config_dir,
        },
    }))
}

/// Delete the local state directory, refusing anything that is not a
/// `muster` directory (a misconfigured XDG path must never wipe `$HOME`).
fn remove_state(dir: &Path) -> Result<(), MusterError> {
    if dir.file_name().and_then(|n| n.to_str()) != Some("muster") {
        return Err(MusterError::Operational(format!(
            "refusing to delete {}: not a Muster state directory",
            dir.display()
        )));
    }
    std::fs::remove_dir_all(dir)
        .map_err(|e| MusterError::Operational(format!("{}: {e}", dir.display())))
}
