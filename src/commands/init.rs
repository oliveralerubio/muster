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
use crate::signing::Signer;

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
    let state_dir = ctx.state_dir.clone();
    ensure_clean_for_init(&config_dir, &state_dir)?;

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

    let identity = Identity::generate();
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

    let _lock = crate::lock::acquire_for(&state_dir, crate::lock::wait_budget())?;
    ensure_clean_for_init(&config_dir, &state_dir)?;

    identity.save(&state_dir)?;
    machine::save_name(&state_dir, &name)?;

    create_dir(&config_dir)?;
    git_init(&config_dir)?;
    create_dir(&config_dir.join("vault"))?;

    let mut manifest = Manifest::new();
    let signer = Signer::load_or_create(&state_dir)?;
    manifest.recovery = Some(Recovery {
        recipient: recovery.recipient(),
        signing_key: Some(Signer::from_recovery(&recovery)?.public()),
    });
    manifest.machines.insert(
        name.clone(),
        Machine {
            recipient: identity.recipient(),
            signing_key: Some(signer.public()),
            platform: machine::platform(),
            muster_version: env!("CARGO_PKG_VERSION").into(),
            agents: agents.clone(),
            host: None,
            last_sync: None,
            role: None,
            profile: None,
            applied_profile: None,
        },
    );
    manifest.save(&config_dir)?;
    let _ = crate::fleet_state::seal(&config_dir, &state_dir, &manifest, &signer, true)?;
    let _ = crate::git::commit_if_configured(&config_dir, "chore(fleet): initialize fleet")?;

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

const STATE_ARTIFACTS: &[&str] = &[
    crate::fleet_state::TRUST_FILE,
    "machine-name",
    "identity.txt",
    "signing.key",
    "generations",
    "secret-leases.json",
];

fn ensure_clean_for_init(config_dir: &Path, state_dir: &Path) -> Result<(), MusterError> {
    if config_dir.join(manifest::FILE_NAME).exists() {
        return Err(MusterError::Operational(format!(
            "already initialized: {} exists",
            config_dir.join(manifest::FILE_NAME).display()
        )));
    }
    if config_dir.join(crate::fleet_state::STATE_FILE).exists() {
        return Err(MusterError::Operational(format!(
            "already initialized: {} exists",
            config_dir.join(crate::fleet_state::STATE_FILE).display()
        )));
    }
    let found: Vec<&str> = STATE_ARTIFACTS
        .iter()
        .copied()
        .filter(|name| state_dir.join(name).exists())
        .collect();
    if !found.is_empty() {
        return Err(MusterError::Operational(format!(
            "cannot initialize a new fleet: local Muster state in {} already belongs to an \
             existing fleet (found {}); to join or restore that fleet run \
             `muster sync --remote <url> --pull`, or run `muster uninstall --yes` before \
             initializing a new fleet",
            state_dir.display(),
            found.join(", ")
        )));
    }
    Ok(())
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
