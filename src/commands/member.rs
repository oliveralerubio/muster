//! `muster member`: team members (invite → pending → approve; role, profile, revoke).

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet::EnrollmentRequest;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::{Manifest, Member, MemberStatus, Role};
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::team::{self, RECOVERY_KEY_PARAM, RECOVERY_KEY_STDIN_PARAM, ROTATION_ADVICE};
use crate::vault::Vault;

const NAME: ParamSpec = ParamSpec {
    name: "name",
    about: "Member name",
    kind: ParamKind::String,
    required: true,
    positional: true,
};

const ROLE: ParamSpec = ParamSpec {
    name: "role",
    about: "Team role: admin or member",
    kind: ParamKind::String,
    required: false,
    positional: false,
};

pub const ADD: CommandSpec = CommandSpec {
    name: "member.add",
    about: "Invite a team member (pending until an admin approves)",
    params: &[NAME, ROLE],
    mutating: true,
    handler: add,
};

pub const APPROVE: CommandSpec = CommandSpec {
    name: "member.approve",
    about: "Approve a pending member and grant their machines their groups",
    params: &[NAME, RECOVERY_KEY_PARAM, RECOVERY_KEY_STDIN_PARAM],
    mutating: true,
    handler: approve,
};

pub const REJECT: CommandSpec = CommandSpec {
    name: "member.reject",
    about: "Reject a pending member and discard their recorded approvals",
    params: &[NAME],
    mutating: true,
    handler: reject,
};

pub const LIST: CommandSpec = CommandSpec {
    name: "member.list",
    about: "List team members, roles, status and machines",
    params: &[],
    mutating: false,
    handler: list,
};

pub const ROLE_SPEC: CommandSpec = CommandSpec {
    name: "member.role",
    about: "Change a member's role (admin or member)",
    params: &[
        NAME,
        ParamSpec {
            required: true,
            ..ROLE
        },
    ],
    mutating: true,
    handler: role,
};

pub const PROFILE: CommandSpec = CommandSpec {
    name: "member.profile",
    about: "Assign a member's personal agent profile",
    params: &[
        NAME,
        ParamSpec {
            name: "profile",
            about: "Profile name from [profiles] (empty to clear)",
            kind: ParamKind::String,
            required: true,
            positional: false,
        },
    ],
    mutating: true,
    handler: profile,
};

pub const RM: CommandSpec = CommandSpec {
    name: "member.rm",
    about: "Revoke a member and all their machines; lists secrets to rotate",
    params: &[NAME, RECOVERY_KEY_PARAM, RECOVERY_KEY_STDIN_PARAM],
    mutating: true,
    handler: rm,
};

fn name(params: &Params) -> Result<String, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("member name is required".into()))?;
    machine::validate_name(name)
}

fn parse_role(params: &Params) -> Result<Role, MusterError> {
    match params.get_string("role").unwrap_or("member") {
        "admin" => Ok(Role::Admin),
        "member" => Ok(Role::Member),
        other => Err(MusterError::Usage(format!(
            "invalid role '{other}': expected admin or member"
        ))),
    }
}

fn existing<'m>(manifest: &'m mut Manifest, name: &str) -> Result<&'m mut Member, MusterError> {
    manifest
        .members
        .get_mut(name)
        .ok_or_else(|| MusterError::Usage(format!("member '{name}' does not exist")))
}

fn add(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = name(params)?;
    let role = parse_role(params)?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&manifest, &ctx.state_dir)?;
    team::materialize(&mut manifest);
    if manifest.members.contains_key(&name) {
        return Err(MusterError::Usage(format!(
            "member '{name}' already exists"
        )));
    }
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| MusterError::Operational(format!("no randomness available: {e}")))?;
    let nonce = crate::signing::to_hex(&nonce);
    manifest.members.insert(
        name.clone(),
        Member {
            role,
            status: MemberStatus::Pending,
            invited_by: Some(actor.member.clone()),
            invite_nonce: Some(nonce),
            invited_at_state: crate::fleet_state::current_hash(&ctx.config_dir)?,
            ..Member::default()
        },
    );
    let result = json!({
        "member": name,
        "role": role,
        "status": MemberStatus::Pending,
        "invited_by": actor.member,
    });
    if ctx.dry_run {
        return Ok(result);
    }
    manifest.save(&ctx.config_dir)?;
    team::commit(ctx, &actor, &format!("feat(team): invite member {name}"))?;
    Ok(result)
}

fn approve(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = name(params)?;
    let before = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&before, &ctx.state_dir)?;
    if actor.member == name {
        return Err(MusterError::Security(
            "nobody can approve their own membership".into(),
        ));
    }
    // An admin holding the recovery key approves break-glass: it counts as a
    // full approval regardless of the quorum.
    let recovery = team::recovery_identity(ctx, params, &before)?;
    let by_recovery = recovery.is_some();
    let mut manifest = before.clone();
    let (mut approvals, member_role, member_invited, member_nonce, member_state) = {
        let member = existing(&mut manifest, &name)?;
        if member.status != MemberStatus::Pending {
            return Err(MusterError::Usage(format!(
                "member '{name}' is not pending"
            )));
        }
        let role = member.role;
        let invited = member.invited_by.clone();
        let nonce = member.invite_nonce.clone();
        let at_state = member.invited_at_state.clone();
        member.status = MemberStatus::Active;
        member.approved_by = Some(actor.member.clone());
        (
            std::mem::take(&mut member.approvals),
            role,
            invited,
            nonce,
            at_state,
        )
    };
    team::check_self_grant(ctx, &before, &manifest, &actor)?;
    ctx.confirm(&format!("Approve member '{name}'?"))?;

    // Quorum: below `[team] required_approvals` the invitation stays pending,
    // with this admin's signed approval recorded in the manifest.
    let quorum = if by_recovery {
        team::Quorum {
            received: before.required_approvals() as usize,
            required: before.required_approvals() as usize,
        }
    } else {
        let snapshot = crate::fleet_state::governance_content(&before);
        let role_val = serde_json::to_value(member_role).unwrap_or(Value::Null);
        let invited_val = serde_json::to_value(&member_invited).unwrap_or(Value::Null);
        let nonce_val = serde_json::to_value(&member_nonce).unwrap_or(Value::Null);
        let state_val = serde_json::to_value(&member_state).unwrap_or(Value::Null);
        let invitation = crate::fleet_state::Invitation {
            role: &role_val,
            invited_by: &invited_val,
            nonce: &nonce_val,
            state: &state_val,
        };
        let already =
            crate::fleet_state::valid_member_approvals(&snapshot, &name, invitation, &approvals);
        if already.contains(&actor.member) {
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Error,
                "already_approved_by_caller",
                format!(
                    "'{}' already approved this request; another admin must approve it",
                    actor.member
                ),
            ));
            return Err(MusterError::Security(format!(
                "'{}' already approved this request; the same admin cannot count twice",
                actor.member
            )));
        }
        let signer = crate::signing::Signer::load_or_create(&ctx.state_dir)?;
        let entry = crate::fleet_state::sign_member_approval(
            &actor.member,
            &actor.machine,
            &name,
            invitation,
            &signer,
        )?;
        approvals.push(entry);
        let received =
            crate::fleet_state::valid_member_approvals(&snapshot, &name, invitation, &approvals)
                .len();
        let q = team::Quorum {
            received,
            required: before.required_approvals() as usize,
        };
        if !q.reached() {
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Info,
                "approval_pending",
                format!(
                    "{} of {} required approvals; waiting for another admin",
                    q.received, q.required
                ),
            ));
        }
        q
    };
    if !quorum.reached() {
        let mut pending = before.clone();
        existing(&mut pending, &name)?.approvals = approvals.clone();
        let result = json!({
            "member": name,
            "status": MemberStatus::Pending,
            "approvals": approvals,
            "approvals_received": quorum.received,
            "approvals_required": quorum.required,
        });
        if ctx.dry_run {
            return Ok(result);
        }
        pending.save(&ctx.config_dir)?;
        team::commit(
            ctx,
            &actor,
            &format!(
                "chore(team): approval {}/{} for member {name}",
                quorum.received, quorum.required
            ),
        )?;
        return Ok(result);
    }

    let identity = match recovery {
        Some(identity) => identity,
        None => Identity::load(&ctx.state_dir)?,
    };
    let mut vault = Vault::open(&ctx.config_dir)?;
    let changes = team::pending_rekeys(ctx, &vault, &before, &manifest)?;
    team::check_can_rekey(&vault, &changes, &identity)?;
    let result = |groups: Vec<String>| {
        json!({
            "member": name,
            "status": MemberStatus::Active,
            "approved_by": actor.member,
            "approvals": approvals,
            "approvals_received": quorum.received,
            "approvals_required": quorum.required,
            "reencrypted_groups": groups,
            "break_glass": by_recovery,
        })
    };
    if ctx.dry_run {
        return Ok(result(changes.into_iter().map(|(g, _)| g).collect()));
    }
    existing(&mut manifest, &name)?.approvals = approvals.clone();
    manifest.save(&ctx.config_dir)?;
    let groups = team::rekey(&mut vault, &changes, &identity)?;
    team::commit(ctx, &actor, &format!("feat(team): approve member {name}"))?;
    Ok(result(groups))
}

/// Reject a pending invitation: the member entry goes away together with any
/// approvals recorded on it while the team quorum was pending. Nothing is
/// revoked and the vault is not re-encrypted.
fn reject(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = name(params)?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&manifest, &ctx.state_dir)?;
    let approvals = {
        let member = existing(&mut manifest, &name)?;
        if member.status != MemberStatus::Pending {
            return Err(MusterError::Usage(format!(
                "member '{name}' is not pending"
            )));
        }
        member.approvals.clone()
    };
    ctx.confirm(&format!(
        "Reject member '{name}'? Their invitation and recorded approvals are discarded."
    ))?;
    let result = |dry_run: bool| {
        let mut value = json!({
            "rejected": name,
            "kind": "member",
            "approvals_discarded": approvals,
        });
        if dry_run {
            value["dry_run"] = Value::Bool(true);
        }
        value
    };
    if ctx.dry_run {
        return Ok(result(true));
    }
    manifest.members.remove(&name);
    manifest.save(&ctx.config_dir)?;
    team::commit(ctx, &actor, &format!("feat(team): reject member {name}"))?;
    Ok(result(false))
}

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let members: Vec<Value> = team::members(&manifest)
        .into_iter()
        .map(|(name, m)| {
            let machines: Vec<Value> = m
                .machines
                .iter()
                .map(|x| json!({ "name": x, "enrolled": manifest.machines.contains_key(x) }))
                .collect();
            let groups: Vec<&String> = manifest
                .groups
                .keys()
                .filter(|g| team::reads(&manifest, g, &name))
                .collect();
            json!({
                "name": name,
                "role": m.role,
                "status": m.status,
                "implicit": manifest.members.is_empty(),
                "machines": machines,
                "groups": groups,
                "profile": m.profile,
                "invited_by": m.invited_by,
                "approved_by": m.approved_by,
            })
        })
        .collect();
    Ok(json!({ "members": members }))
}

fn role(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = name(params)?;
    let role = parse_role(params)?;
    let mut before = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&before, &ctx.state_dir)?;
    team::materialize(&mut before);
    let mut manifest = before.clone();
    existing(&mut manifest, &name)?.role = role;
    team::check_self_grant(ctx, &before, &manifest, &actor)?;
    if !manifest.members.values().any(|m| m.role == Role::Admin) {
        return Err(MusterError::Usage(
            "the team needs at least one admin".into(),
        ));
    }
    ctx.confirm(&format!("Change the role of '{name}'?"))?;
    let result = json!({ "member": name, "role": role });
    if ctx.dry_run {
        return Ok(result);
    }
    manifest.save(&ctx.config_dir)?;
    team::commit(ctx, &actor, &format!("feat(team): set role of {name}"))?;
    Ok(result)
}

fn profile(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = name(params)?;
    let profile = params.get_string("profile").filter(|p| !p.is_empty());
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&manifest, &ctx.state_dir)?;
    if let Some(p) = profile {
        if !manifest.profiles.contains_key(p) {
            return Err(MusterError::Usage(format!("profile '{p}' is not declared")));
        }
    }
    team::materialize(&mut manifest);
    existing(&mut manifest, &name)?.profile = profile.map(str::to_string);
    let result = json!({ "member": name, "profile": profile });
    if ctx.dry_run {
        return Ok(result);
    }
    manifest.save(&ctx.config_dir)?;
    team::commit(ctx, &actor, &format!("feat(team): set profile of {name}"))?;
    Ok(result)
}

fn rm(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = name(params)?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&manifest, &ctx.state_dir)?;
    let before = manifest.clone();
    if actor.member == name {
        return Err(MusterError::Usage(
            "you cannot revoke yourself; ask another admin".into(),
        ));
    }
    let member = manifest
        .members
        .remove(&name)
        .ok_or_else(|| MusterError::Usage(format!("member '{name}' does not exist")))?;
    ctx.confirm(&format!(
        "Revoke member '{name}' and all their machines? Exposed secrets must be rotated."
    ))?;

    let mut revoked_recipients = Vec::new();
    for machine in &member.machines {
        if let Some(m) = manifest.machines.remove(machine) {
            revoked_recipients.push(m.recipient);
            if let Some(key) = m.signing_key {
                if !manifest.revoked_signing_keys.contains(&key) {
                    manifest.revoked_signing_keys.push(key);
                }
            }
        }
        team::forget_machine(&mut manifest, machine);
    }
    for group in manifest.groups.values_mut() {
        group.readers.members.retain(|m| m != &name);
    }
    // A key still used by a remaining machine keeps its access.
    revoked_recipients.retain(|r| !manifest.machines.values().any(|m| &m.recipient == r));
    // Persist the revocation so no later command can encrypt to these keys again.
    for r in &revoked_recipients {
        if !manifest.revoked_recipients.contains(r) {
            manifest.revoked_recipients.push(r.clone());
        }
    }

    let identity = team::unlock(ctx, params, &manifest)?;
    let vault = Vault::open(&ctx.config_dir)?.with_revoked(manifest.revoked_recipients.clone());
    let mut changes = team::pending_rekeys(ctx, &vault, &before, &manifest)?;

    // Readers and secrets come from the decrypted ciphertext, never from the
    // unauthenticated sidecar (a tampered sidecar could hide a revoked key).
    let mut exposed = Vec::new();
    for group in vault.groups()? {
        let view = match vault.group_view(&group, &identity) {
            Ok(view) => view,
            Err(ref err) if crate::vault::is_not_a_recipient(err) => {
                if revoked_recipients.is_empty() && !changes.iter().any(|(g, _)| g == &group) {
                    continue;
                }
                return Err(MusterError::Security(format!(
                    "this machine cannot read secret group '{group}' to re-encrypt it; \
                     run from a reader or pass --recovery-key"
                )));
            }
            Err(err) => return Err(err),
        };
        if !view.readers.iter().any(|r| revoked_recipients.contains(r)) {
            continue;
        }
        for secret in view.secrets {
            exposed.push(json!({
                "group": group,
                "name": secret,
                "reference": format!("native://{group}/{secret}"),
            }));
        }
        let keep = |r: &String| !revoked_recipients.contains(r);
        match changes.iter_mut().find(|(g, _)| g == &group) {
            Some((_, planned)) => planned.retain(keep),
            None => changes.push((group, view.readers.into_iter().filter(keep).collect())),
        }
    }
    team::check_can_rekey(&vault, &changes, &identity)?;
    let result = |groups: Vec<String>| {
        json!({
            "revoked_member": name,
            "revoked_machines": member.machines,
            "reencrypted_groups": groups,
            "exposed_secrets": exposed,
            "rotation_advice": ROTATION_ADVICE,
        })
    };
    if ctx.dry_run {
        return Ok(result(changes.into_iter().map(|(g, _)| g).collect()));
    }
    // Re-encrypt BEFORE saving the manifest: a failure leaves the fleet unchanged.
    // Stage everything in a shadow copy; one recoverable transaction publishes it.
    crate::fleet_state::ensure_not_behind_pin(&ctx.config_dir, &ctx.state_dir, "commit on top of")?;
    let txn = crate::revocation_tx::Txn::begin(ctx)?;
    let shadow = txn.dir().to_path_buf();
    drop(vault);
    let mut vault = Vault::open(&shadow)?.with_revoked(manifest.revoked_recipients.clone());
    let groups = team::rekey(&mut vault, &changes, &identity)?;
    manifest.save(&shadow)?;
    for machine in &member.machines {
        EnrollmentRequest::remove(&shadow, machine)?;
    }
    txn.commit(
        ctx,
        &actor.message(&format!("feat(team): revoke member {name}")),
    )?;
    team::publish(ctx);
    if !exposed.is_empty() {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "rotate_exposed_secrets",
            format!(
                "{} secret(s) were readable by '{name}'; rotate them",
                exposed.len()
            ),
        ));
    }
    Ok(result(groups))
}
