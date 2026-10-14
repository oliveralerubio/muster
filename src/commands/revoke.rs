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
use crate::secrets::SecretValue;
use crate::vault::Vault;

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
        ParamSpec {
            name: "recovery-key",
            about: "Offline recovery key to authorize re-encryption without another machine",
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

    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let target = manifest.machines.get(name).cloned().ok_or_else(|| {
        MusterError::Usage(format!("machine '{name}' is not enrolled in the fleet"))
    })?;

    ctx.confirm(&format!(
        "Revoke machine '{name}'? Its access will be removed and exposed secrets must be rotated."
    ))?;

    // Team mode: only admins revoke machines (the recovery key always can).
    if !manifest.members.is_empty() && params.get_string("recovery-key").is_none() {
        crate::team::admin(&manifest, &ctx.state_dir)?;
    }

    // Authorization: either recovery key or caller's identity
    let authorizer_identity = match params.get_string("recovery-key") {
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
            let is_member = manifest
                .machines
                .values()
                .any(|m| m.recipient == caller_id.recipient());
            if !is_member {
                return Err(MusterError::Security(
                    "this machine is not an approved fleet member; revoke from an approved machine or with --recovery-key".into(),
                ));
            }
            caller_id
        }
    };

    // Another remaining machine with the same key keeps vault access.
    let shared_key = manifest
        .machines
        .iter()
        .any(|(n, m)| n != name && m.recipient == target.recipient);

    let mut vault = Vault::open(&ctx.config_dir)?;
    let authorizer = authorizer_identity.recipient();
    let mut affected_groups = Vec::new();
    let mut exposed_secrets = Vec::new();
    for group in vault.groups()? {
        let readers = vault.readers(&group)?;
        if !readers.contains(&target.recipient) {
            continue;
        }
        if !shared_key && !readers.contains(&authorizer) {
            return Err(MusterError::Security(format!(
                "the authorizer is not a reader of secret group '{group}'; revoke with --recovery-key"
            )));
        }
        for secret in vault.list(&group)? {
            exposed_secrets.push(json!({
                "group": group,
                "name": secret.name,
                "reference": format!("native://{group}/{}", secret.name),
            }));
        }
        affected_groups.push(group);
    }
    let advice = "Rotate every exposed secret (e.g. `muster secret rotate <group>/<name>`). \
        Revocation only protects future secrets: it cannot recover or erase values \
        the machine already downloaded or decrypted.";

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "revoked": name,
            "recipient": target.recipient,
            "affected_groups": affected_groups,
            "exposed_secrets": exposed_secrets,
            "rotation_advice": advice,
        }));
    }

    manifest.machines.remove(name);
    crate::team::forget_machine(&mut manifest, name);
    manifest.save(&ctx.config_dir)?;

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
            let mut readers: Vec<String> = vault
                .readers(group)?
                .into_iter()
                .filter(|r| r != &target.recipient)
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
    EnrollmentRequest::remove(&ctx.config_dir, name)?;

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
        "recipient": target.recipient,
        "reencrypted_groups": reencrypted_groups,
        "exposed_secrets": exposed_secrets,
        "rotation_advice": advice,
    }))
}
