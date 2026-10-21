//! `muster status`: display fleet machines, their platform, version and sync status.

use serde_json::{json, Value};

use crate::context::Context;
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

    let mut machines = Vec::new();
    for (name, m) in &manifest.machines {
        let is_current = match (&local_name, &local_recipient) {
            (Some(ln), _) if ln == name => true,
            (_, Some(lr)) if lr == &m.recipient => true,
            _ => false,
        };
        let status = if is_current {
            "current"
        } else if m.last_sync.is_some() {
            "active"
        } else {
            "enrolled"
        };
        machines.push(json!({
            "name": name,
            "recipient": m.recipient,
            "platform": m.platform,
            "version": m.muster_version,
            "agents": m.agents,
            "experimental_agents": m.agents.iter().filter(|id| {
                crate::adapters::find(id).is_some_and(|a| a.experimental)
            }).collect::<Vec<_>>(),
            "last_sync": m.last_sync,
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
    let lock_path = ctx.config_dir.join(crate::installer::LOCK_FILE);
    let lock = if lock_path
        .try_exists()
        .map_err(|e| MusterError::Operational(format!("{}: {e}", lock_path.display())))?
    {
        crate::installer::Lock::load(&ctx.config_dir)?
    } else {
        crate::installer::Lock::default()
    };
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| MusterError::Operational("HOME is not set".into()))?;
    let platform = machine::platform();
    let mut agents = Vec::new();
    for (id, pin) in lock.agents {
        let Some(artifact) = pin.resolve_artifact(&platform) else {
            continue;
        };
        let binary = pin
            .bin
            .as_deref()
            .unwrap_or_else(|| crate::adapters::find(&id).map(|a| a.binary).unwrap_or(&id));
        machine::validate_name(binary)?;
        let local = home.join(".local/bin").join(binary);
        let executable = if local.is_file() {
            Some(local)
        } else {
            machine::find_executable(binary)
        };
        let installed_version = executable.and_then(|path| {
            machine::version_output(&path)
                .ok()
                .and_then(|text| {
                    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '/'))
                        .map(|word| word.trim_start_matches('v'))
                        .find(|word| {
                            let core = word.split(['-', '+']).next().unwrap_or("");
                            let parts: Vec<_> = core.split('.').collect();
                            (2..=3).contains(&parts.len())
                                && parts
                                    .iter()
                                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                        })
                        .map(str::to_string)
                })
                .or_else(|| {
                    // If the binary cannot report a version, an exact artifact
                    // checksum still identifies the installed pin.
                    std::fs::read(&path)
                        .ok()
                        .filter(|bytes| {
                            crate::hash::sha256_hex(bytes).eq_ignore_ascii_case(&artifact.sha256)
                        })
                        .map(|_| pin.version.clone())
                })
        });
        agents.push(json!({"id": id, "installed_version": installed_version, "pinned_version": pin.version}));
    }
    let converged = planned["approvals"].as_array().is_some_and(Vec::is_empty)
        && planned["skipped_skills"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && agents
            .iter()
            .all(|agent| agent["installed_version"] == agent["pinned_version"]);
    Ok(json!({
        "converged": converged,
        "agents": agents,
        "skipped_skills": planned["skipped_skills"],
        "pending_approvals": planned["approvals"],
    }))
}
