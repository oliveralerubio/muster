//! `muster status`: display fleet machines, their platform, version and sync status.

use serde_json::{json, Value};

use crate::context::Context;
use crate::engine::layer;
use crate::errors::MusterError;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "status",
    about: "Show fleet status and this machine's membership",
    params: &[],
    mutating: false,
    handler: run,
};

fn run(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;

    let local_identity = Identity::load(&ctx.state_dir).ok();
    let local_recipient = local_identity.as_ref().map(|id| id.recipient());
    let local_name = machine::load_name(&ctx.state_dir);

    let local_sync = machine::local_last_sync(&ctx.state_dir);
    let mut machines = Vec::new();
    for (name, m) in &manifest.machines {
        let is_current = match (&local_name, &local_recipient) {
            (Some(ln), _) if ln == name => true,
            (_, Some(lr)) if lr == &m.recipient => true,
            _ => false,
        };
        let last_sync =
            machine::effective_last_sync(m.last_sync.as_deref(), local_sync.as_deref(), is_current);
        let status = if is_current {
            "current"
        } else if m.applied_profile.is_none() {
            "enrolled"
        } else {
            let target_digest = layer::digest(&ctx.config_dir, &manifest, name);
            if m.applied_profile.as_deref() == Some(target_digest.as_str()) {
                "active"
            } else {
                "drifted"
            }
        };
        machines.push(json!({
            "name": name,
            "host": m.host,
            "recipient": m.recipient,
            "platform": m.platform,
            "version": m.muster_version,
            "agents": m.agents,
            "experimental_agents": m.agents.iter().filter(|id| {
                crate::adapters::find(id).is_some_and(|a| a.experimental)
            }).collect::<Vec<_>>(),
            "last_sync": last_sync,
            "status": status,
            "is_current": is_current,
        }));
    }

    let team = crate::team::drift(&ctx.config_dir, &manifest);
    let sync = match std::fs::read(ctx.state_dir.join("sync-status.json")) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| MusterError::Operational(format!("invalid sync status: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => {
            return Err(MusterError::Operational(format!(
                "cannot read sync status: {e}"
            )))
        }
    };
    let convergence = convergence(ctx)?;
    Ok(json!({
        "convergence": convergence,
        "sync": sync,
        "config_path": ctx.config_dir,
        "machines": machines,
        "pending": team["pending"],
        "drift": team["drift"],
    }))
}

/// One convergence report shared by the CLI, web status and MCP status.
fn convergence(ctx: &mut Context) -> Result<Value, MusterError> {
    let plan = crate::engine::plan(
        ctx,
        &crate::engine::Options {
            target: crate::engine::Target::Global,
            agent: None,
        },
    )?;
    let planned = plan.to_json(true, false, plan.base_generation());
    let lock = crate::installer::Lock::load_or_default(&ctx.config_dir)?;
    let bin_dir = crate::installer::bin_dir()?;
    let platform = machine::platform();
    let mut agents = Vec::new();
    for (id, pin) in lock.agents {
        let artifact = pin.resolve_artifact(&platform);
        let installed_version = crate::installer::installed_version(&id, &pin, &bin_dir)?;
        let newer = installed_version
            .as_deref()
            .is_some_and(|v| crate::installer::is_newer(v, &pin.version));
        let note = if artifact.is_none() {
            Some(format!("no artifact for platform {platform}"))
        } else if newer {
            Some("newer installed version kept; bump the pin in muster.lock".to_string())
        } else {
            None
        };
        agents.push(json!({
            "id": id, "installed_version": installed_version,
            "pinned_version": pin.version, "artifact": artifact, "note": note, "kept_newer": newer,
        }));
    }
    let converged = planned["approvals"].as_array().is_some_and(Vec::is_empty)
        && planned["skipped_skills"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && agents.iter().all(|agent| {
            !agent["artifact"].is_null()
                && (agent["installed_version"] == agent["pinned_version"]
                    || agent["kept_newer"] == true)
        });
    Ok(json!({
        "converged": converged,
        "agents": agents,
        "skipped_skills": planned["skipped_skills"],
        "pending_approvals": planned["approvals"],
    }))
}
