//! Team mode: members, secret groups and their governance.
//!
//! Single-person mode is a manifest without `[members]`: one implicit admin
//! ([`IMPLICIT_MEMBER`]) owns every machine. The first `muster member add`
//! materializes that member, so existing manifests keep working unchanged.
//!
//! Group readers (members, groups, machines, raw recipients) resolve to age
//! recipients, always plus the recovery key; [`rekey`] re-encrypts every vault
//! group whose resolved recipients changed. Vault groups not declared under
//! `[groups]` keep the single-person rule: every fleet machine reads them.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{json, Value};

use crate::context::Context;
use crate::engine::layer;
use crate::errors::MusterError;
use crate::git;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::{Manifest, Member, MemberStatus, Role};
use crate::output::{Diagnostic, Severity};
use crate::registry::{ParamKind, ParamSpec, Params};
use crate::secrets::SecretValue;
use crate::vault::Vault;

/// The single member of a manifest without `[members]`.
pub const IMPLICIT_MEMBER: &str = "owner";

/// `--recovery-key`, for re-encrypting groups the caller cannot read.
pub const RECOVERY_KEY_PARAM: ParamSpec = ParamSpec {
    name: "recovery-key",
    about: "Offline recovery key, to re-encrypt groups this machine cannot read",
    kind: ParamKind::String,
    required: false,
    positional: false,
};

pub const ROTATION_ADVICE: &str = "Rotate every exposed secret (e.g. `muster secret rotate \
    <group>/<name>`). Revocation only protects future secrets: it cannot recover or erase values \
    already downloaded or decrypted.";

/// Who is running a command: the member owning this machine.
#[derive(Debug, Clone)]
pub struct Actor {
    pub member: String,
    pub machine: String,
    pub admin: bool,
}

impl Actor {
    /// Git commit trailer naming the actor.
    fn trailer(&self) -> String {
        format!("Muster-Actor: {} (machine {})", self.member, self.machine)
    }
}

/// Members, with the implicit single member when none are declared.
pub fn members(manifest: &Manifest) -> Vec<(String, Member)> {
    if manifest.members.is_empty() {
        return vec![(IMPLICIT_MEMBER.to_string(), implicit_member(manifest))];
    }
    manifest
        .members
        .iter()
        .map(|(n, m)| (n.clone(), m.clone()))
        .collect()
}

fn implicit_member(manifest: &Manifest) -> Member {
    Member {
        role: Role::Admin,
        status: MemberStatus::Active,
        machines: manifest.machines.keys().cloned().collect(),
        ..Member::default()
    }
}

/// Turn single-person mode into team mode (the implicit member becomes explicit).
pub fn materialize(manifest: &mut Manifest) {
    if manifest.members.is_empty() {
        let member = implicit_member(manifest);
        manifest.members.insert(IMPLICIT_MEMBER.to_string(), member);
    }
}

/// Active member owning `machine`, if any.
pub fn owner_of(manifest: &Manifest, machine: &str) -> Option<String> {
    members(manifest)
        .into_iter()
        .find(|(_, m)| m.status == MemberStatus::Active && m.machines.iter().any(|x| x == machine))
        .map(|(name, _)| name)
}

/// The member behind this machine's identity. Not in the fleet → Security.
pub fn actor(manifest: &Manifest, state_dir: &Path) -> Result<Actor, MusterError> {
    let not_member = || {
        MusterError::Security(
            "this machine is not an approved machine of an active team member".into(),
        )
    };
    let recipient = Identity::load(state_dir)
        .map_err(|_| not_member())?
        .recipient();
    let local = machine::load_name(state_dir);
    let machine = manifest
        .machines
        .iter()
        .filter(|(_, m)| m.recipient == recipient)
        .map(|(name, _)| name.clone())
        .max_by_key(|name| Some(name) == local.as_ref())
        .ok_or_else(not_member)?;
    let member = owner_of(manifest, &machine).ok_or_else(not_member)?;
    let admin = members(manifest)
        .iter()
        .any(|(n, m)| n == &member && m.role == Role::Admin);
    Ok(Actor {
        member,
        machine,
        admin,
    })
}

/// The actor, who must be an admin (governance changes).
pub fn admin(manifest: &Manifest, state_dir: &Path) -> Result<Actor, MusterError> {
    let actor = actor(manifest, state_dir)?;
    if !actor.admin {
        return Err(MusterError::Security(format!(
            "only team admins can change members, groups, readers or team profiles \
             ('{}' is not an admin)",
            actor.member
        )));
    }
    Ok(actor)
}

fn active_admins(manifest: &Manifest) -> usize {
    members(manifest)
        .iter()
        .filter(|(_, m)| m.role == Role::Admin && m.status == MemberStatus::Active)
        .count()
}

/// Age recipients of an active member's enrolled machines.
pub fn member_recipients(manifest: &Manifest, member: &str) -> Vec<String> {
    members(manifest)
        .into_iter()
        .filter(|(n, m)| n == member && m.status == MemberStatus::Active)
        .flat_map(|(_, m)| m.machines)
        .filter_map(|machine| manifest.machines.get(&machine).map(|m| m.recipient.clone()))
        .collect()
}

/// Recipients of vault group `group`: its declared readers (resolved
/// recursively) or, for an undeclared group, every fleet machine; always
/// plus the recovery key.
pub fn group_recipients(manifest: &Manifest, group: &str) -> Vec<String> {
    let mut out = Vec::new();
    if manifest.groups.contains_key(group) {
        let mut seen = BTreeSet::new();
        collect_readers(manifest, group, &mut seen, &mut out);
    } else {
        out.extend(manifest.machines.values().map(|m| m.recipient.clone()));
    }
    if let Some(recovery) = &manifest.recovery {
        out.push(recovery.recipient.clone());
    }
    let mut seen = BTreeSet::new();
    out.retain(|r| seen.insert(r.clone()));
    out
}

fn collect_readers(
    manifest: &Manifest,
    group: &str,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<String>,
) {
    let Some(g) = manifest.groups.get(group) else {
        return;
    };
    if !seen.insert(group.to_string()) {
        return;
    }
    for member in &g.readers.members {
        out.extend(member_recipients(manifest, member));
    }
    for machine in &g.readers.machines {
        out.extend(manifest.machines.get(machine).map(|m| m.recipient.clone()));
    }
    out.extend(g.readers.recipients.iter().cloned());
    for nested in &g.readers.groups {
        collect_readers(manifest, nested, seen, out);
    }
}

/// Whether `member` (directly, through a machine or a nested group) reads `group`.
pub fn reads(manifest: &Manifest, group: &str, member: &str) -> bool {
    let mine = member_recipients(manifest, member);
    !mine.is_empty()
        && group_recipients(manifest, group)
            .iter()
            .any(|r| mine.contains(r))
}

/// Nobody grants themselves access: a change from `before` to `after` must
/// not add the actor's machines to any group, nor change the actor's role.
/// A sole admin (single-person mode) is exempt, with a warning.
pub fn check_self_grant(
    ctx: &mut Context,
    before: &Manifest,
    after: &Manifest,
    actor: &Actor,
) -> Result<(), MusterError> {
    let role = |m: &Manifest| {
        members(m)
            .into_iter()
            .find(|(n, _)| n == &actor.member)
            .map(|(_, m)| m.role)
    };
    let mut mine: BTreeSet<String> = member_recipients(after, &actor.member)
        .into_iter()
        .collect();
    mine.extend(member_recipients(before, &actor.member));
    let mut granted: Vec<String> = Vec::new();
    for group in after.groups.keys() {
        let old = if before.groups.contains_key(group) {
            group_recipients(before, group)
        } else {
            Vec::new()
        };
        if group_recipients(after, group)
            .iter()
            .any(|r| mine.contains(r) && !old.contains(r))
        {
            granted.push(group.clone());
        }
    }
    let role_changed = role(before).is_some() && role(before) != role(after);
    if granted.is_empty() && !role_changed {
        return Ok(());
    }
    let what = if role_changed {
        "change their own role".to_string()
    } else {
        format!("grant themselves access to group(s) {}", granted.join(", "))
    };
    if active_admins(before) <= 1 && !role_changed {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "self_grant_sole_admin",
            format!("'{}' is the only admin and can {what}", actor.member),
        ));
        return Ok(());
    }
    Err(MusterError::Security(format!(
        "'{}' cannot {what}; another admin must make this change",
        actor.member
    )))
}

/// Identity used to re-encrypt: the recovery key if given, else this machine's.
pub fn unlock(
    ctx: &Context,
    params: &Params,
    manifest: &Manifest,
) -> Result<Identity, MusterError> {
    let Some(raw) = params.get_string("recovery-key") else {
        return Identity::load(&ctx.state_dir);
    };
    let identity = Identity::from_secret(&SecretValue::new(raw.to_string()))?;
    match &manifest.recovery {
        Some(r) if r.recipient == identity.recipient() => Ok(identity),
        Some(_) => Err(MusterError::Security(
            "provided recovery key does not match manifest recovery recipient".into(),
        )),
        None => Err(MusterError::Security(
            "no recovery key configured in manifest".into(),
        )),
    }
}

/// Vault groups whose recipients differ from what `manifest` resolves to,
/// with their desired recipients.
pub fn pending_rekeys(
    vault: &Vault,
    manifest: &Manifest,
) -> Result<Vec<(String, Vec<String>)>, MusterError> {
    let mut changes = Vec::new();
    for group in vault.groups()? {
        let desired = group_recipients(manifest, &group);
        let current: BTreeSet<String> = vault.readers(&group)?.into_iter().collect();
        if current != desired.iter().cloned().collect() {
            changes.push((group, desired));
        }
    }
    Ok(changes)
}

/// Re-encrypt every vault group whose recipients changed. Checks first that
/// `identity` reads all of them, so a refusal writes nothing.
pub fn rekey(
    vault: &mut Vault,
    manifest: &Manifest,
    identity: &Identity,
) -> Result<Vec<String>, MusterError> {
    let changes = pending_rekeys(vault, manifest)?;
    check_can_rekey(vault, &changes, identity)?;
    for (group, recipients) in &changes {
        vault.reencrypt(group, recipients, identity)?;
    }
    Ok(changes.into_iter().map(|(g, _)| g).collect())
}

pub fn check_can_rekey(
    vault: &Vault,
    changes: &[(String, Vec<String>)],
    identity: &Identity,
) -> Result<(), MusterError> {
    let me = identity.recipient();
    for (group, _) in changes {
        if !vault.readers(group)?.contains(&me) {
            return Err(MusterError::Security(format!(
                "this machine cannot read secret group '{group}' to re-encrypt it; \
                 run from a reader or pass --recovery-key"
            )));
        }
    }
    Ok(())
}

/// Secrets readable by any of `recipients` (to rotate after a revocation).
pub fn exposed_secrets(vault: &Vault, recipients: &[String]) -> Result<Vec<Value>, MusterError> {
    let mut exposed = Vec::new();
    for group in vault.groups()? {
        if !vault
            .readers(&group)?
            .iter()
            .any(|r| recipients.contains(r))
        {
            continue;
        }
        for secret in vault.list(&group)? {
            exposed.push(json!({
                "group": group,
                "name": secret.name,
                "reference": format!("native://{group}/{}", secret.name),
            }));
        }
    }
    Ok(exposed)
}

/// Commit an administrative change, naming its actor, and push if a remote exists.
pub fn commit(ctx: &mut Context, actor: &Actor, summary: &str) -> Result<(), MusterError> {
    let message = format!("{summary}\n\n{}", actor.trailer());
    git::commit_if_configured(&ctx.config_dir, &message)?;
    if git::has_remote(&ctx.config_dir) {
        if let Err(e) = git::push(&ctx.config_dir) {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "push_failed",
                format!("change committed but not pushed: {e}; run `muster sync --push`"),
            ));
        }
    }
    Ok(())
}

/// After a global apply (`result` from the engine) that wrote every adapter,
/// record in the manifest that this machine applied the current
/// team-approved layers (read by `status` on every machine). Commits only
/// when it changed.
pub fn record_applied(ctx: &Context, result: &Value) -> Result<(), MusterError> {
    let failed = result["agents"]
        .as_array()
        .is_some_and(|a| a.iter().any(|a| a["status"] == "failed"));
    if ctx.dry_run || failed {
        return Ok(());
    }
    let Some(name) = machine::load_name(&ctx.state_dir) else {
        return Ok(());
    };
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let digest = layer::digest(&ctx.config_dir, &manifest, &name);
    let Some(entry) = manifest.machines.get_mut(&name) else {
        return Ok(());
    };
    if entry.applied_profile.as_deref() == Some(digest.as_str()) {
        return Ok(());
    }
    entry.applied_profile = Some(digest);
    manifest.save(&ctx.config_dir)?;
    git::commit_if_configured(
        &ctx.config_dir,
        &format!("chore(fleet): {name} applied the approved profile"),
    )?;
    Ok(())
}

/// Machines and members out of line with the approved team profile, and
/// pending invitations.
pub fn drift(config_dir: &Path, manifest: &Manifest) -> Value {
    let declared =
        |p: &Option<String>| p.as_ref().is_none_or(|p| manifest.profiles.contains_key(p));
    let mut machines = Vec::new();
    for (name, m) in &manifest.machines {
        let member = owner_of(manifest, name);
        let member_profile_ok = member
            .as_ref()
            .and_then(|n| manifest.members.get(n))
            .is_none_or(|m| declared(&m.profile));
        let reason = if !declared(&m.profile) || !member_profile_ok {
            Some("unknown_profile")
        } else {
            match &m.applied_profile {
                None => Some("profile_not_applied"),
                Some(d) if *d != layer::digest(config_dir, manifest, name) => {
                    Some("profile_outdated")
                }
                Some(_) => None,
            }
        };
        if let Some(reason) = reason {
            machines.push(json!({ "name": name, "member": member, "reason": reason }));
        }
    }
    let mut drifted_members = Vec::new();
    let mut pending = Vec::new();
    for (name, m) in members(manifest) {
        if m.status == MemberStatus::Pending {
            pending.push(json!({ "name": name, "role": m.role, "invited_by": m.invited_by }));
            continue;
        }
        let drifted: Vec<&Value> = machines
            .iter()
            .filter(|d| d["member"] == name.as_str())
            .map(|d| &d["name"])
            .collect();
        if !drifted.is_empty() {
            drifted_members.push(json!({
                "name": name,
                "reason": "machines_drifted",
                "machines": drifted,
            }));
        }
    }
    json!({
        "pending": { "members": pending },
        "drift": { "members": drifted_members, "machines": machines },
    })
}

/// Remove `machine` from every member and group reader list.
pub fn forget_machine(manifest: &mut Manifest, machine: &str) {
    for member in manifest.members.values_mut() {
        member.machines.retain(|m| m != machine);
    }
    for group in manifest.groups.values_mut() {
        group.readers.machines.retain(|m| m != machine);
    }
}
