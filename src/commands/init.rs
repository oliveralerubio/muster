//! `muster init`: create the config repo, this machine's identity, the offline
//! recovery key and an empty vault.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::{self, Machine, Manifest, Recovery};
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "init",
    about: "Create the config repo, this machine's identity, a recovery key and an empty vault",
    params: &[ParamSpec {
        name: "machine",
        about: "Name of this machine in the fleet [default: hostname]",
        kind: ParamKind::String,
        required: false,
        positional: false,
    }],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    if !ctx.yes && !ctx.is_tty {
        return Err(MusterError::Usage(
            "non-interactive init requires --yes (the recovery key must be confirmed as saved)"
                .into(),
        ));
    }
    let config_dir = ctx.config_dir.clone();
    if config_dir.join(manifest::FILE_NAME).exists() {
        return Err(MusterError::Operational(format!(
            "already initialized: {} exists",
            config_dir.join(manifest::FILE_NAME).display()
        )));
    }
    let name = match params.get_string("machine") {
        Some(name) => machine::validate_name(name)?,
        None => machine::default_name(),
    };
    let agents = machine::detect_agents();
    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "config_path": config_dir,
            "machine": name,
            "detected_agents": agents,
        }));
    }

    // Reuse an existing local identity rather than destroying it.
    let state_dir = ctx.state_dir.clone();
    let (identity, new_identity) = match Identity::load(&state_dir) {
        Ok(identity) => (identity, false),
        Err(_) => (Identity::generate(), true),
    };
    let recovery = Identity::generate();

    // Interactive: show the key once on the terminal and require confirmation
    // before anything is written. With --yes it is returned in the result.
    let interactive = !ctx.yes;
    if interactive {
        eprintln!(
            "Recovery key (shown only once; store it offline, it is never saved):\n\n  {}\n",
            recovery.secret_key().expose()
        );
        ctx.confirm("Have you stored the recovery key somewhere safe?")?;
    }

    if new_identity {
        identity.save(&state_dir)?;
    }
    machine::save_name(&state_dir, &name)?;

    create_dir(&config_dir)?;
    git_init(&config_dir)?;
    create_dir(&config_dir.join("vault"))?;

    let mut manifest = Manifest::new();
    manifest.recovery = Some(Recovery {
        recipient: recovery.recipient(),
    });
    manifest.machines.insert(
        name.clone(),
        Machine {
            recipient: identity.recipient(),
            platform: machine::platform(),
            muster_version: env!("CARGO_PKG_VERSION").into(),
            agents: agents.clone(),
            last_sync: None,
            role: None,
            profile: None,
            applied_profile: None,
        },
    );
    manifest.save(&config_dir)?;

    if !agents.is_empty() {
        ctx.diag(Diagnostic::new(
            Severity::Info,
            "agents_detected",
            format!(
                "detected {}; their existing configuration can be imported",
                agents.join(", ")
            ),
        ));
    }

    let recovery_key = if interactive {
        Value::Null
    } else {
        Value::from(recovery.secret_key().expose())
    };
    Ok(json!({
        "config_path": config_dir,
        "machine": {
            "name": name,
            "recipient": identity.recipient(),
            "platform": machine::platform(),
        },
        "recovery_key": recovery_key,
        "recovery_recipient": recovery.recipient(),
        "detected_agents": agents,
    }))
}

fn create_dir(path: &Path) -> Result<(), MusterError> {
    fs::create_dir_all(path)
        .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))
}

fn git_init(dir: &Path) -> Result<(), MusterError> {
    if dir.join(".git").exists() {
        return Ok(());
    }
    let output = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(dir)
        .output()
        .map_err(|e| MusterError::Unavailable(format!("cannot run git: {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(MusterError::Operational(format!(
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
