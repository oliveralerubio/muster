//! Team mode: members, secret groups and their governance.
//!
//! Single-person mode is a manifest without `[members]`: one implicit admin
//! ([`IMPLICIT_MEMBER`]) owns every machine. The first `muster member add`
//! materializes that member, so existing manifests keep working unchanged.
//!
//! Group readers (members, groups, machines, raw recipients) resolve to age
//! recipients, always plus the recovery key; [`rekey`] applies only the planned
//! recipient changes introduced by the command. Vault groups not declared under
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

pub const RECOVERY_KEY_ENV: &str = "MUSTER_RECOVERY_KEY";

/// `--recovery-key`, for re-encrypting groups the caller cannot read (deprecated: use stdin or env).
pub const RECOVERY_KEY_PARAM: ParamSpec = ParamSpec {
    name: "recovery-key",
    about: "Offline recovery key (deprecated: prefer --recovery-key-stdin or MUSTER_RECOVERY_KEY)",
    kind: ParamKind::String,
    required: false,
    positional: false,
};

/// `--recovery-key-stdin`, read offline recovery key from standard input.
pub const RECOVERY_KEY_STDIN_PARAM: ParamSpec = ParamSpec {
    name: "recovery-key-stdin",
    about: "Read offline recovery key from standard input",
    kind: ParamKind::Bool,
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

/// The offline recovery key, if one was supplied: `--recovery-key-stdin`, the
/// deprecated `--recovery-key` argument (warns: it lands in shell history and
/// `ps`), or [`RECOVERY_KEY_ENV`]. Call once per command: stdin is consumed.
pub fn recovery_key(
    ctx: &mut Context,
    params: &Params,
) -> Result<Option<SecretValue>, MusterError> {
    if params.get_bool(RECOVERY_KEY_STDIN_PARAM.name) {
        return ctx.read_secret_stdin().map(Some);
    }
    if let Some(raw) = params.get_string(RECOVERY_KEY_PARAM.name) {
        ctx.diag(
            Diagnostic::new(
                Severity::Warning,
                "insecure_recovery_key_argument",
                "the recovery key was passed as a command-line argument, where shell history and \
                 process listings can record it",
            )
            .with_suggestion(format!(
                "pipe it with --recovery-key-stdin or set {RECOVERY_KEY_ENV}"
            )),
        );
        return Ok(Some(SecretValue::new(raw.to_string())));
    }
    Ok(std::env::var(RECOVERY_KEY_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| SecretValue::new(v.trim().to_string())))
}

/// The recovery identity, if a key was supplied, checked against the
/// manifest's recovery recipient.
pub fn recovery_identity(
    ctx: &mut Context,
    params: &Params,
    manifest: &Manifest,
) -> Result<Option<Identity>, MusterError> {
    let Some(key) = recovery_key(ctx, params)? else {
        return Ok(None);
    };
    let identity = Identity::from_secret(&key)?;
    match &manifest.recovery {
        Some(r) if r.recipient == identity.recipient() => Ok(Some(identity)),
        Some(_) => Err(MusterError::Security(
            "provided recovery key does not match manifest recovery recipient".into(),
        )),
        None => Err(MusterError::Security(
            "no recovery key configured in manifest".into(),
        )),
    }
}

/// Identity used to re-encrypt: the recovery key if given, else this machine's.
pub fn unlock(
    ctx: &mut Context,
    params: &Params,
    manifest: &Manifest,
) -> Result<Identity, MusterError> {
    match recovery_identity(ctx, params, manifest)? {
        Some(identity) => Ok(identity),
        None => Identity::load(&ctx.state_dir),
    }
}

/// Apply only the recipient delta introduced by this command to the current
/// vault readers. Existing manifest drift remains pending, even in touched groups.
pub fn pending_rekeys(
    ctx: &mut Context,
    vault: &Vault,
    before: &Manifest,
    after: &Manifest,
) -> Result<Vec<(String, Vec<String>)>, MusterError> {
    let mut changes = Vec::new();
    for group in vault.groups()? {
        let old: BTreeSet<String> = group_recipients(before, &group).into_iter().collect();
        let desired: BTreeSet<String> = group_recipients(after, &group).into_iter().collect();
        let current: BTreeSet<String> = vault.readers(&group)?.into_iter().collect();
        if !current.is_subset(&old) {
            return Err(MusterError::Security(format!(
                "unexpected vault readers for group '{group}'; review reader drift before rekeying"
            )));
        }
        let mut planned = current.clone();
        for removed in old.difference(&desired) {
            planned.remove(removed);
        }
        planned.extend(desired.difference(&old).cloned());
        if planned != current {
            changes.push((group, planned.into_iter().collect()));
        }
    }
    warn_reader_drift(ctx, vault, after, &changes)?;
    Ok(changes)
}

/// Report the drift that will remain after applying `changes`, without unlocking it.
pub fn warn_reader_drift(
    ctx: &mut Context,
    vault: &Vault,
    manifest: &Manifest,
    changes: &[(String, Vec<String>)],
) -> Result<(), MusterError> {
    for group in vault.groups()? {
        let current = match changes.iter().find(|(g, _)| g == &group) {
            Some((_, recipients)) => recipients.clone(),
            None => vault.readers(&group)?,
        };
        let desired = group_recipients(manifest, &group);
        if current.iter().collect::<BTreeSet<_>>() != desired.iter().collect() {
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    "reader_drift",
                    format!(
                        "Pending reader drift, not applied: {}",
                        reader_diff(manifest, &group, &current, &desired)
                    ),
                )
                .with_suggestion(format!(
                    "Review with `muster group rekey {group} --dry-run`"
                )),
            );
        }
    }
    Ok(())
}

/// Describe access changes using public recipients and their known machines.
pub fn reader_diff(
    manifest: &Manifest,
    group: &str,
    current: &[String],
    desired: &[String],
) -> String {
    let describe = |from: &[String], to: &[String]| {
        let recipients: BTreeSet<_> = to.iter().filter(|r| !from.contains(r)).collect();
        let labels: Vec<_> = recipients
            .into_iter()
            .map(|r| {
                let machines: Vec<_> = manifest
                    .machines
                    .iter()
                    .filter(|(_, m)| &m.recipient == r)
                    .map(|(name, _)| name.as_str())
                    .collect();
                if machines.is_empty() {
                    r.to_string()
                } else {
                    format!("{r} (machines: {})", machines.join(", "))
                }
            })
            .collect();
        if labels.is_empty() {
            "none".to_string()
        } else {
            labels.join(", ")
        }
    };
    format!(
        "group '{group}': gain access: {}; lose access: {}",
        describe(current, desired),
        describe(desired, current)
    )
}

/// Re-encrypt exactly the planned groups. Checks that `identity` reads all of
/// them first, so a refusal writes nothing.
pub fn rekey(
    vault: &mut Vault,
    changes: &[(String, Vec<String>)],
    identity: &Identity,
) -> Result<Vec<String>, MusterError> {
    check_can_rekey(vault, changes, identity)?;
    for (group, recipients) in changes {
        vault.reencrypt(group, recipients, identity)?;
    }
    Ok(changes.iter().map(|(g, _)| g.clone()).collect())
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
        vault.verify_readers(group, identity)?;
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

/// Progress of a pending change towards its admin quorum
/// (`[team] required_approvals`).
#[derive(Debug, Clone, Copy)]
pub struct Quorum {
    pub received: usize,
    pub required: usize,
}

impl Quorum {
    pub fn reached(self) -> bool {
        self.received >= self.required
    }
}

/// Record `approver` (an admin member) among the `approvals` already given to
/// a pending change. The same admin never counts twice (Security, with an
/// `already_approved_by_caller` diagnostic). Below quorum, emits an info
/// diagnostic saying how many approvals are still missing.
pub fn add_approval(
    ctx: &mut Context,
    manifest: &Manifest,
    approvals: &mut Vec<String>,
    approver: &str,
) -> Result<Quorum, MusterError> {
    if approvals.iter().any(|a| a == approver) {
        ctx.diag(Diagnostic::new(
            Severity::Error,
            "already_approved_by_caller",
            format!("'{approver}' already approved this request; another admin must approve it"),
        ));
        return Err(MusterError::Security(format!(
            "'{approver}' already approved this request; the same admin cannot count twice"
        )));
    }
    approvals.push(approver.to_string());
    let quorum = Quorum {
        received: approvals.len(),
        required: manifest.required_approvals() as usize,
    };
    if !quorum.reached() {
        ctx.diag(Diagnostic::new(
            Severity::Info,
            "approval_pending",
            format!(
                "{} of {} required approvals; waiting for another admin",
                quorum.received, quorum.required
            ),
        ));
    }
    Ok(quorum)
}
