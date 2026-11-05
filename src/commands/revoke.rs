//! `muster revoke`: remove a machine from the fleet, re-encrypt secret groups
//! without its recipient, and report exposed secrets for rotation.

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
use crate::vault::{self, Vault};

pub const SPEC: CommandSpec = CommandSpec {
    name: "revoke",
    about: "Remove a machine from the fleet and re-encrypt secrets to exclude it",
    params: &[
        ParamSpec {
            name: "name",
            about: "Name of the machine to revoke",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        crate::team::RECOVERY_KEY_PARAM,
        crate::team::RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("machine name is required".into()))?;
    machine::validate_name(name)?;

    let mut manifest = Manifest::load(&ctx.config_dir)?;

    // Safely re-runnable: if the machine is already gone from manifest, check
    // an enrollment request or the revoked_recipients list.
    let target_recipient = match manifest.machines.get(name) {
        Some(m) => m.recipient.clone(),
        None => {
            if let Ok(req) = EnrollmentRequest::load(&ctx.config_dir, name) {
                req.recipient
            } else {
                let vault = Vault::open(&ctx.config_dir)?;
                let mut found = None;
                for group in vault.groups()? {
                    if let Ok(readers) = vault.readers(&group) {
                        for r in &manifest.revoked_recipients {
                            if readers.contains(r) {
                                found = Some(r.clone());
                                break;
                            }
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                }
                match found {
                    Some(r) => r,
                    None => {
                        return Err(MusterError::Usage(format!(
                            "machine '{name}' is not enrolled in the fleet"
                        )))
                    }
                }
            }
        }
    };

    ctx.confirm(&format!(
        "Revoke machine '{name}'? Its access will be removed and exposed secrets must be rotated."
    ))?;

    // Team mode: only admins revoke machines (the recovery key always can).
    let recovery = crate::team::recovery_identity(ctx, params, &manifest)?;
    if !manifest.members.is_empty() && recovery.is_none() {
        crate::team::admin(&manifest, &ctx.state_dir)?;
    }

    // Authorization: either recovery key or caller's identity
    let authorizer_identity = match recovery {
        Some(recovery_id) => recovery_id,
        None => {
            let caller_id = Identity::load(&ctx.state_dir)?;
            let is_member = manifest
                .machines
                .values()
                .any(|m| m.recipient == caller_id.recipient());
            if !is_member {
                return Err(MusterError::Security(
                    "this machine is not an approved fleet member; revoke from an approved machine or with --recovery-key-stdin".into(),
                ));
            }
            caller_id
        }
    };

    // Another remaining machine with the same key keeps vault access.
    let shared_key = manifest
        .machines
        .iter()
        .any(|(n, m)| n != name && m.recipient == target_recipient);

    let vault = Vault::open(&ctx.config_dir)?;
        let mut affected_groups = Vec::new();
    let mut exposed_secrets = Vec::new();

    // Determine affected groups from authentic decrypted ciphertext. Preflight
    // every group so any failure aborts before durable mutation.
    for group in vault.groups()? {
        vault.ensure_pair(&group)?;
        let view = match vault.group_view(&group, &authorizer_identity) {
            Ok(view) => view,
            Err(ref err) if vault::is_not_a_recipient(err) => {
                if shared_key {
                    continue;
                }
                return Err(MusterError::Security(format!(
                    "the authorizer is not a reader of secret group '{group}'; revoke with --recovery-key-stdin"
                )));
            }
            Err(err) => return Err(err),
        };
        if view.readers.contains(&target_recipient) {
            for secret_name in view.secrets {
                exposed_secrets.push(json!({
                    "group": group,
                    "name": secret_name,
                    "reference": format!("native://{group}/{secret_name}"),
                }));
            }
            affected_groups.push(group);
        }
    }

    let advice = "Rotate every exposed secret (e.g. `muster secret rotate <group>/<name>`).         Revocation only protects future secrets: it cannot recover or erase values         the machine already downloaded or decrypted.";

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "revoked": name,
            "recipient": target_recipient,
            "affected_groups": affected_groups,
            "exposed_secrets": exposed_secrets,
            "rotation_advice": advice,
        }));
    }

    // Rekey every affected group BEFORE modifying or saving the manifest.
    let mut vault = vault.with_revoked([target_recipient.clone()]);
    let mut reencrypted_groups = Vec::new();
    if shared_key {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "shared_key",
            format!(
                "another machine uses the same key as '{name}'; vault groups were not re-encrypted"
            ),
        ));
    } else {
        for group in &affected_groups {
            let view = vault.group_view(group, &authorizer_identity)?;
            let mut readers: Vec<String> = view
                .readers
                .into_iter()
                .filter(|r| r != &target_recipient && !manifest.revoked_recipients.contains(r))
                .collect();
            if let Some(recovery) = &manifest.recovery {
                if !readers.contains(&recovery.recipient) {
                    readers.push(recovery.recipient.clone());
                }
            }
            vault.reencrypt(group, &readers, &authorizer_identity)?;
            reencrypted_groups.push(group.clone());
        }
    }

    // Rekeying succeeded: now update and persist the manifest.
    if !manifest.revoked_recipients.contains(&target_recipient) {
        manifest.revoked_recipients.push(target_recipient.clone());
    }
    manifest.machines.remove(name);
    crate::team::forget_machine(&mut manifest, name);
    manifest.save(&ctx.config_dir)?;

    let _ = EnrollmentRequest::remove(&ctx.config_dir, name);

    git::commit_if_configured(
        &ctx.config_dir,
        &format!("feat(fleet): revoke machine {name}"),
    )?;
    if git::has_remote(&ctx.config_dir) {
        if let Err(e) = git::push(&ctx.config_dir) {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "push_failed",
                format!("revocation committed but not pushed: {e}; run `muster sync --push`"),
            ));
        }
    }
    if !exposed_secrets.is_empty() {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "rotate_exposed_secrets",
            format!(
                "{} secret(s) were readable by '{name}'; rotate them",
                exposed_secrets.len()
            ),
        ));
    }

    Ok(json!({
        "revoked": name,
        "recipient": target_recipient,
        "reencrypted_groups": reencrypted_groups,
        "exposed_secrets": exposed_secrets,
        "rotation_advice": advice,
    }))
}
