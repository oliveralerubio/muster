//! `muster enroll`: register this machine for enrollment into a fleet.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet::EnrollmentRequest;
use crate::git;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::Manifest;
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::signing::{self, Signer};

pub const SPEC: CommandSpec = CommandSpec {
    name: "enroll",
    about: "Generate machine identity and create an enrollment request in the config repo",
    params: &[ParamSpec {
        name: "name",
        about: "Name of this machine in the fleet [default: hostname]",
        kind: ParamKind::String,
        required: false,
        positional: false,
    }],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = match params.get_string("name") {
        Some(n) => machine::validate_name(n)?,
        None => machine::default_name(),
    };

    let manifest = Manifest::load(&ctx.config_dir)?;
    if manifest.machines.contains_key(&name) {
        return Err(MusterError::Usage(format!(
            "machine '{name}' is already a member of the fleet"
        )));
    }
    if let Ok(existing) = EnrollmentRequest::load(&ctx.config_dir, &name) {
        let local = Identity::load(&ctx.state_dir).ok().map(|id| id.recipient());
        if local.as_deref() != Some(existing.recipient.as_str()) {
            return Err(MusterError::Usage(format!(
                "another machine already requested enrollment as '{name}'"
            )));
        }
    }

    let agents = machine::detect_agents();

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "machine": name,
            "config_path": ctx.config_dir,
            "detected_agents": agents,
        }));
    }

    let identity = match Identity::load(&ctx.state_dir) {
        Ok(id) => id,
        Err(_) => {
            let id = Identity::generate();
            id.save(&ctx.state_dir)?;
            id
        }
    };
    // A machine that is not yet a fleet member takes the requested name locally.
    if manifest.revoked_recipients.contains(&identity.recipient()) {
        return Err(MusterError::Security(format!(
            "recipient '{}' has been revoked and cannot be enrolled",
            identity.recipient()
        )));
    }
    let is_member = manifest
        .machines
        .values()
        .any(|m| m.recipient == identity.recipient());
    if !is_member {
        machine::save_name(&ctx.state_dir, &name)?;
    }

    let signer = Signer::load_or_create(&ctx.state_dir)?;
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| MusterError::Operational(format!("no randomness available: {e}")))?;
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("RFC 3339 formatting of current UTC time cannot fail");

    let req = EnrollmentRequest {
        name: name.clone(),
        recipient: identity.recipient(),
        signing_key: Some(signer.public()),
        nonce: signing::to_hex(&nonce),
        platform: machine::platform(),
        muster_version: env!("CARGO_PKG_VERSION").into(),
        agents,
        created_at: now,
        approvals: Vec::new(),
        role: None,
        profile: None,
        member: None,
    };

    let path = req.save(&ctx.config_dir)?;

    git::commit_if_configured(
        &ctx.config_dir,
        &format!("chore(fleet): enrollment request for {name}"),
    )?;
    // The request must reach the machines that can approve it.
    let pushed = if git::has_remote(&ctx.config_dir) {
        match git::push(&ctx.config_dir) {
            Ok(()) => true,
            Err(e) => {
                ctx.diag(Diagnostic::new(
                    Severity::Warning,
                    "push_failed",
                    format!("enrollment request committed but not pushed: {e}; run `muster sync --push`"),
                ));
                false
            }
        }
    } else {
        false
    };

    Ok(json!({
        "machine": name,
        "recipient": identity.recipient(),
        "platform": machine::platform(),
        "status": "pending",
        "request_path": path,
        "pushed": pushed,
    }))
}
