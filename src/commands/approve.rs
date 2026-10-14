//! `muster approve`: approve an enrollment request, add the machine to the fleet,
//! and re-encrypt vault groups for the new machine.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet::EnrollmentRequest;
use crate::git;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::{Machine, Manifest};
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::secrets::SecretValue;
use crate::team;
use crate::vault::Vault;

pub const SPEC: CommandSpec = CommandSpec {
    name: "approve",
    about: "Approve a machine's enrollment request and grant it vault access",
    params: &[
        ParamSpec {
            name: "name",
            about: "Name of the machine to approve",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "recovery-key",
            about: "Offline recovery key to authorize approval without another machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "role",
            about: "Role to assign to the approved machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "profile",
            about: "Configuration profile to assign to the approved machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "member",
            about: "Team member who owns the machine (team mode; admins only)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("machine name is required".into()))?;
    machine::validate_name(name)?;

    ctx.confirm(&format!("Approve enrollment of machine '{name}'?"))?;

    let manifest = Manifest::load(&ctx.config_dir)?;
    if manifest.machines.contains_key(name) {
        return Err(MusterError::Usage(format!(
            "machine '{name}' is already a member of the fleet"
        )));
    }
    let req = EnrollmentRequest::load(&ctx.config_dir, name)?;

    // Authorization: either via caller machine identity or via recovery key.
    let approver_identity = match params.get_string("recovery-key") {
        Some(raw_key) => {
            let secret = SecretValue::new(raw_key.to_string());
            let recovery_id = Identity::from_secret(&secret)?;
            if let Some(recovery) = &manifest.recovery {
                if recovery.recipient != recovery_id.recipient() {
                    return Err(MusterError::Security(
                        "provided recovery key does not match manifest recovery recipient".into(),
                    ));
                }
            } else {
                return Err(MusterError::Security(
                    "no recovery key configured in manifest".into(),
                ));
            }
            recovery_id
        }
        None => {
            let caller_id = Identity::load(&ctx.state_dir)?;
            if caller_id.recipient() == req.recipient {
                return Err(MusterError::Security(
                    "a machine cannot approve its own enrollment request (self-approval)".into(),
                ));
            }
            if machine::load_name(&ctx.state_dir).as_deref() == Some(name) {
                return Err(MusterError::Security(
                    "a machine cannot approve its own enrollment request (self-approval)".into(),
                ));
            }
            let is_member = manifest
                .machines
                .values()
                .any(|m| m.recipient == caller_id.recipient());
            if !is_member {
                return Err(MusterError::Security(
                    "this machine is not an approved fleet member; approve from another machine or with --recovery-key".into(),
                ));
            }
            caller_id
        }
    };

    // Team mode: only admins approve machines, and nobody for themselves.
    let before = manifest;
    let member = params.get_string("member");
    let team_mode = !before.members.is_empty() || member.is_some();
    let actor = if team_mode && params.get_string("recovery-key").is_none() {
        Some(team::admin(&before, &ctx.state_dir)?)
    } else {
        None
    };
    let mut manifest = before.clone();
    manifest.machines.insert(
        name.to_string(),
        Machine {
            recipient: req.recipient.clone(),
            platform: req.platform.clone(),
            muster_version: req.muster_version.clone(),
            agents: req.agents.clone(),
            last_sync: None,
            role: params.get_string("role").map(str::to_string),
            profile: params.get_string("profile").map(str::to_string),
            applied_profile: None,
        },
    );
    if let Some(member) = member {
        team::materialize(&mut manifest);
        let entry = manifest
            .members
            .get_mut(member)
            .ok_or_else(|| MusterError::Usage(format!("member '{member}' does not exist")))?;
        entry.machines.push(name.to_string());
    }
    if let Some(actor) = &actor {
        team::check_self_grant(ctx, &before, &manifest, actor)?;
    }

    // Check every changed group can be re-encrypted before writing anything.
    let mut vault = Vault::open(&ctx.config_dir)?;
    let changes = team::pending_rekeys(&vault, &manifest)?;
    for (group, _) in &changes {
        if !vault
            .readers(group)?
            .contains(&approver_identity.recipient())
        {
            return Err(MusterError::Security(format!(
                "the approver is not a reader of secret group '{group}'; approve with --recovery-key"
            )));
        }
    }
    let groups: Vec<String> = changes.iter().map(|(g, _)| g.clone()).collect();

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "machine": name,
            "recipient": req.recipient,
            "groups": groups,
        }));
    }

    manifest.save(&ctx.config_dir)?;
    let reencrypted_groups = team::rekey(&mut vault, &manifest, &approver_identity)?;

    EnrollmentRequest::remove(&ctx.config_dir, name)?;

    let summary = format!("feat(fleet): approve machine {name}");
    match &actor {
        Some(actor) => {
            team::commit(ctx, actor, &summary)?;
            return Ok(json!({
                "approved": name,
                "recipient": req.recipient,
                "member": member,
                "reencrypted_groups": reencrypted_groups,
            }));
        }
        None => git::commit_if_configured(&ctx.config_dir, &summary)?,
    };

    if git::has_remote(&ctx.config_dir) {
        if let Err(e) = git::push(&ctx.config_dir) {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "push_failed",
                format!("approval committed but not pushed: {e}; run `muster sync --push`"),
            ));
        }
    }

    Ok(json!({
        "approved": name,
        "recipient": req.recipient,
        "reencrypted_groups": reencrypted_groups,
    }))
}
