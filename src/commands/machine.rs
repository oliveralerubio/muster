//! `muster machine`: fleet inventory (`machine list`, `machine rm`, `machine assign`).

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet::EnrollmentRequest;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::team;
use crate::vault::Vault;

pub const LIST: CommandSpec = CommandSpec {
    name: "machine.list",
    about: "List all enrolled machines in the fleet",
    params: &[],
    mutating: false,
    handler: list,
};

pub const RM: CommandSpec = CommandSpec {
    name: "machine.rm",
    about: "Remove a machine from the fleet (alias for revoke)",
    params: &[
        ParamSpec {
            name: "name",
            about: "Name of the machine to remove",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        crate::team::RECOVERY_KEY_PARAM,
        crate::team::RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: rm,
};

pub const ASSIGN: CommandSpec = CommandSpec {
    name: "machine.assign",
    about: "Assign a role or profile to an enrolled machine",
    params: &[
        ParamSpec {
            name: "name",
            about: "Name of the machine",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "role",
            about: "Role to assign to the machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "profile",
            about: "Configuration profile to assign to the machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "member",
            about: "Team member who owns the machine (admins only)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        crate::team::RECOVERY_KEY_PARAM,
        crate::team::RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: assign,
};

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
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
        machines.push(json!({
            "name": name,
            "status": "active",
            "recipient": m.recipient,
            "platform": m.platform,
            "version": m.muster_version,
            "agents": m.agents,
            "last_sync": m.last_sync,
            "role": m.role,
            "profile": m.profile,
            "is_current": is_current,
        }));
    }

    // Pending enrollment requests waiting for approval (or rejection).
    let approvals_required = manifest.required_approvals() as usize;
    let pending: Vec<Value> = EnrollmentRequest::pending(&ctx.config_dir)?
        .into_iter()
        .map(|req| {
            json!({
                "name": req.name,
                "recipient": req.recipient,
                "platform": req.platform,
                "version": req.muster_version,
                "agents": req.agents,
                "created_at": req.created_at,
                "approvals": req.approvals,
                "approvals_received": req.approvals.len(),
                "approvals_required": approvals_required,
                "role": req.role,
                "profile": req.profile,
                "member": req.member,
            })
        })
        .collect();

    Ok(json!({
        "config_path": ctx.config_dir,
        "machines": machines,
        "pending": pending,
    }))
}

fn rm(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("machine name is required".into()))?;
    machine::validate_name(name)?;

    let manifest = Manifest::load(&ctx.config_dir)?;
    if manifest.machines.contains_key(name) {
        // Machine is enrolled; delegate to revoke so secrets are re-encrypted
        (crate::commands::revoke::SPEC.handler)(ctx, params)
    } else {
        // Machine is not enrolled (may have already been revoked); remove any lingering enrollment
        ctx.confirm(&format!("Remove machine record '{name}'?"))?;
        let pending = EnrollmentRequest::file_path(&ctx.config_dir, name).exists();
        if ctx.dry_run {
            return Ok(json!({ "dry_run": true, "removed": name, "pending_request": pending }));
        }
        let _ = EnrollmentRequest::remove(&ctx.config_dir, name);
        crate::git::commit_if_configured(
            &ctx.config_dir,
            &format!("chore(fleet): remove machine record {name}"),
        )?;
        Ok(json!({
            "removed": name,
            "pending_request": pending,
        }))
    }
}

fn assign(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("machine name is required".into()))?;
    machine::validate_name(name)?;

    let role = params.get_string("role");
    let profile = params.get_string("profile");
    let member = params.get_string("member");

    if role.is_none() && profile.is_none() && member.is_none() {
        return Err(MusterError::Usage(
            "specify at least one of --role, --profile or --member".into(),
        ));
    }

    let mut manifest = Manifest::load(&ctx.config_dir)?;
    // Team mode: profiles and ownership are admin changes.
    let actor = if manifest.members.is_empty() && member.is_none() {
        None
    } else {
        Some(team::admin(&manifest, &ctx.state_dir)?)
    };
    let before = manifest.clone();
    if let Some(member) = member {
        if !manifest.machines.contains_key(name) {
            return Err(MusterError::Usage(format!(
                "machine '{name}' is not enrolled"
            )));
        }
        team::materialize(&mut manifest);
        team::forget_machine(&mut manifest, name);
        manifest
            .members
            .get_mut(member)
            .ok_or_else(|| MusterError::Usage(format!("member '{member}' does not exist")))?
            .machines
            .push(name.to_string());
    }
    let entry = manifest
        .machines
        .get_mut(name)
        .ok_or_else(|| MusterError::Usage(format!("machine '{name}' is not enrolled")))?;

    if let Some(r) = role {
        entry.role = Some(r.to_string());
    }
    if let Some(p) = profile {
        entry.profile = Some(p.to_string());
    }
    let (role, profile) = (entry.role.clone(), entry.profile.clone());

    let mut vault = Vault::open(&ctx.config_dir)?;
    let changes = team::pending_rekeys(ctx, &vault, &before, &manifest)?;
    let identity = if changes.is_empty() {
        None
    } else {
        let identity = team::unlock(ctx, params, &manifest)?;
        team::check_can_rekey(&vault, &changes, &identity)?;
        Some(identity)
    };
    if let Some(actor) = &actor {
        team::check_self_grant(ctx, &before, &manifest, actor)?;
    }
    let result = json!({
        "machine": name,
        "role": role,
        "profile": profile,
        "member": team::owner_of(&manifest, name),
        "reencrypted_groups": changes.iter().map(|(g, _)| g.clone()).collect::<Vec<_>>(),
    });
    if ctx.dry_run {
        return Ok(result);
    }

    manifest.save(&ctx.config_dir)?;
    if let Some(identity) = identity {
        team::rekey(&mut vault, &changes, &identity)?;
    }
    let summary = format!("chore(fleet): update machine {name}");
    match &actor {
        Some(actor) => team::commit(ctx, actor, &summary)?,
        None => {
            crate::git::commit_if_configured(&ctx.config_dir, &summary)?;
        }
    }
    Ok(result)
}
