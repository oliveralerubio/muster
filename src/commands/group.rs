//! `muster group`: secret groups and their readers.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::machine;
use crate::manifest::{Group, Manifest, Readers};
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::team::{self, RECOVERY_KEY_PARAM, RECOVERY_KEY_STDIN_PARAM, ROTATION_ADVICE};
use crate::vault::Vault;

const GROUP: ParamSpec = ParamSpec {
    name: "group",
    about: "Secret group name",
    kind: ParamKind::String,
    required: true,
    positional: true,
};

const READER: ParamSpec = ParamSpec {
    name: "reader",
    about: "Reader to add or remove (member, group, machine or age recipient)",
    kind: ParamKind::String,
    required: false,
    positional: false,
};

pub const CREATE: CommandSpec = CommandSpec {
    name: "group.create",
    about: "Create a secret group with its readers and optional agent profile",
    params: &[
        GROUP,
        READER,
        ParamSpec {
            name: "profile",
            about: "Profile name from [profiles] applied to this group's readers",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        RECOVERY_KEY_PARAM,
        RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: create,
};

pub const LIST: CommandSpec = CommandSpec {
    name: "group.list",
    about: "List secret groups, readers, profiles and secret counts",
    params: &[],
    mutating: false,
    handler: list,
};

pub const GRANT: CommandSpec = CommandSpec {
    name: "group.grant",
    about: "Grant a member, group, machine or recipient access to a secret group",
    params: &[
        GROUP,
        ParamSpec {
            required: true,
            ..READER
        },
        RECOVERY_KEY_PARAM,
        RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: grant,
};

pub const REVOKE: CommandSpec = CommandSpec {
    name: "group.revoke",
    about: "Revoke access to a secret group; lists secrets to rotate",
    params: &[
        GROUP,
        ParamSpec {
            required: true,
            ..READER
        },
        RECOVERY_KEY_PARAM,
        RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: revoke,
};

pub const REKEY: CommandSpec = CommandSpec {
    name: "group.rekey",
    about: "Review and apply manual reader drift for one secret group",
    params: &[GROUP, RECOVERY_KEY_PARAM, RECOVERY_KEY_STDIN_PARAM],
    mutating: true,
    handler: rekey,
};

fn group_name(params: &Params) -> Result<String, MusterError> {
    let name = params
        .get_string("group")
        .ok_or_else(|| MusterError::Usage("group name is required".into()))?;
    machine::validate_name(name)
}

/// Add `reader` to `group`, resolving a bare name as member, then machine,
/// then group; `age1...` is a raw recipient.
fn add_reader(group: &mut Group, manifest: &Manifest, reader: &str) -> Result<(), MusterError> {
    let list = if reader.starts_with("age1") {
        &mut group.readers.recipients
    } else if manifest.members.contains_key(reader) {
        &mut group.readers.members
    } else if manifest.machines.contains_key(reader) {
        &mut group.readers.machines
    } else if manifest.groups.contains_key(reader) {
        &mut group.readers.groups
    } else {
        return Err(MusterError::Usage(format!(
            "unknown reader '{reader}': expected a member, machine, group or age recipient"
        )));
    };
    if !list.iter().any(|r| r == reader) {
        list.push(reader.to_string());
    }
    Ok(())
}

fn create(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = group_name(params)?;
    let profile = params.get_string("profile").map(str::to_string);
    let mut before = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&before, &ctx.state_dir)?;
    if before.groups.contains_key(&name) {
        return Err(MusterError::Usage(format!("group '{name}' already exists")));
    }
    if let Some(p) = &profile {
        if !before.profiles.contains_key(p) {
            return Err(MusterError::Usage(format!("profile '{p}' is not declared")));
        }
    }
    team::materialize(&mut before);
    let mut manifest = before.clone();
    let mut group = Group {
        profile: profile.clone(),
        ..Group::default()
    };
    if let Some(reader) = params.get_string("reader") {
        add_reader(&mut group, &manifest, reader)?;
    }
    manifest.groups.insert(name.clone(), group.clone());
    team::check_self_grant(ctx, &before, &manifest, &actor)?;

    let identity = team::unlock(ctx, params, &manifest)?;
    let mut vault = Vault::open(&ctx.config_dir)?;
    let changes = team::pending_rekeys(ctx, &vault, &before, &manifest)?;
    team::check_can_rekey(&vault, &changes, &identity)?;
    let result = json!({
        "group": name,
        "readers": group.readers,
        "profile": profile,
        "reencrypted_groups": changes.iter().map(|(g, _)| g.clone()).collect::<Vec<_>>(),
    });
    if ctx.dry_run {
        return Ok(result);
    }
    manifest.save(&ctx.config_dir)?;
    team::rekey(&mut vault, &changes, &identity)?;
    team::commit(ctx, &actor, &format!("feat(team): create group {name}"))?;
    Ok(result)
}

fn grant(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = group_name(params)?;
    let reader = params
        .get_string("reader")
        .ok_or_else(|| MusterError::Usage("reader is required".into()))?;
    let mut before = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&before, &ctx.state_dir)?;
    if !before.groups.contains_key(&name) {
        return Err(MusterError::Usage(format!("group '{name}' does not exist")));
    }
    team::materialize(&mut before);
    let mut manifest = before.clone();
    let group = manifest.groups.get_mut(&name).unwrap();
    add_reader(group, &before, reader)?;
    team::check_self_grant(ctx, &before, &manifest, &actor)?;
    ctx.confirm(&format!("Grant '{reader}' access to group '{name}'?"))?;

    let identity = team::unlock(ctx, params, &manifest)?;
    let mut vault = Vault::open(&ctx.config_dir)?;
    let changes = team::pending_rekeys(ctx, &vault, &before, &manifest)?;
    team::check_can_rekey(&vault, &changes, &identity)?;
    let result = |groups: Vec<String>| {
        json!({
            "group": name,
            "reader": reader,
            "reencrypted_groups": groups,
        })
    };
    if ctx.dry_run {
        return Ok(result(changes.into_iter().map(|(g, _)| g).collect()));
    }
    manifest.save(&ctx.config_dir)?;
    let groups = team::rekey(&mut vault, &changes, &identity)?;
    team::commit(
        ctx,
        &actor,
        &format!("feat(team): grant {name} to {reader}"),
    )?;
    Ok(result(groups))
}

fn revoke(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = group_name(params)?;
    let reader = params
        .get_string("reader")
        .ok_or_else(|| MusterError::Usage("reader is required".into()))?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&manifest, &ctx.state_dir)?;
    let before = manifest.clone();
    let group = manifest
        .groups
        .get_mut(&name)
        .ok_or_else(|| MusterError::Usage(format!("group '{name}' does not exist")))?;
    if !group.readers.remove(reader) {
        return Err(MusterError::Usage(format!(
            "'{reader}' is not a reader of '{name}'"
        )));
    }
    ctx.confirm(&format!(
        "Revoke '{reader}' from '{name}'? Exposed secrets must be rotated."
    ))?;

    let mut revoked_recipients = team::member_recipients(&manifest, reader);
    if let Some(m) = manifest.machines.get(reader) {
        revoked_recipients.push(m.recipient.clone());
    }
    if reader.starts_with("age1") {
        revoked_recipients.push(reader.to_string());
    }
    // Recipients still present in remaining readers keep their access.
    let remaining = team::group_recipients(&manifest, &name);
    revoked_recipients.retain(|r| !remaining.contains(r));

    let identity = team::unlock(ctx, params, &manifest)?;
    let mut vault = Vault::open(&ctx.config_dir)?;
    let exposed = team::exposed_secrets(&vault, &revoked_recipients)?;
    let changes = team::pending_rekeys(ctx, &vault, &before, &manifest)?;
    team::check_can_rekey(&vault, &changes, &identity)?;
    let result = |groups: Vec<String>| {
        json!({
            "group": name,
            "revoked_reader": reader,
            "reencrypted_groups": groups,
            "exposed_secrets": exposed,
            "rotation_advice": ROTATION_ADVICE,
        })
    };
    if ctx.dry_run {
        return Ok(result(changes.into_iter().map(|(g, _)| g).collect()));
    }
    manifest.save(&ctx.config_dir)?;
    let groups = team::rekey(&mut vault, &changes, &identity)?;
    team::commit(
        ctx,
        &actor,
        &format!("feat(team): revoke {reader} from {name}"),
    )?;
    if !exposed.is_empty() {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "rotate_exposed_secrets",
            format!(
                "{} secret(s) in '{name}' were readable by '{reader}'; rotate them",
                exposed.len()
            ),
        ));
    }
    Ok(result(groups))
}

fn rekey(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = group_name(params)?;
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = team::admin(&manifest, &ctx.state_dir)?;
    let mut vault = Vault::open(&ctx.config_dir)?;
    let current = vault.readers(&name)?;
    let desired = team::group_recipients(&manifest, &name);
    let gains: Vec<_> = desired
        .iter()
        .filter(|r| !current.contains(r))
        .cloned()
        .collect();
    let losses: Vec<_> = current
        .iter()
        .filter(|r| !desired.contains(r))
        .cloned()
        .collect();
    let changes = if gains.is_empty() && losses.is_empty() {
        Vec::new()
    } else {
        vec![(name.clone(), desired.clone())]
    };
    team::warn_reader_drift(ctx, &vault, &manifest, &changes)?;
    let result = json!({
        "group": name,
        "gains_access": gains,
        "loses_access": losses,
        "reencrypted_groups": changes.iter().map(|(g, _)| g.clone()).collect::<Vec<_>>(),
    });
    if ctx.dry_run {
        return Ok(result);
    }

    // Compare the proposed readers to the vault, so manual config edits cannot
    // bypass the same self-grant rule enforced by `group grant`.
    let mut before = manifest.clone();
    before.groups.insert(
        name.clone(),
        Group {
            readers: Readers {
                recipients: current.clone(),
                ..Readers::default()
            },
            ..Group::default()
        },
    );
    team::check_self_grant(ctx, &before, &manifest, &actor)?;
    ctx.confirm(&format!(
        "Apply reader drift: {}?",
        team::reader_diff(&manifest, &name, &current, &desired)
    ))?;
    let identity = team::unlock(ctx, params, &manifest)?;
    team::rekey(&mut vault, &changes, &identity)?;
    if !changes.is_empty() {
        team::commit(ctx, &actor, &format!("feat(team): rekey group {name}"))?;
    }
    Ok(result)
}

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let vault = Vault::open(&ctx.config_dir)?;
    let mut groups = Vec::new();
    for (name, g) in &manifest.groups {
        let count = vault.list(name).map(|s| s.len()).unwrap_or(0);
        let resolved = team::group_recipients(&manifest, name);
        groups.push(json!({
            "name": name,
            "readers": g.readers,
            "profile": g.profile,
            "secrets_count": count,
            "recipients_count": resolved.len(),
        }));
    }
    Ok(json!({ "groups": groups }))
}
