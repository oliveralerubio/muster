//! `muster profile`: manage named agent configuration profiles under `[profiles.<name>]`.
//!
//! Profiles are the group, member and machine layers of `plan --explain`. In
//! team mode only admins change them; a profile still assigned is not removed.

use serde::Serialize;
use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::machine;
use crate::manifest::{Manifest, Profile};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::team;

const NAME: ParamSpec = ParamSpec {
    name: "name",
    about: "Profile name",
    kind: ParamKind::String,
    required: true,
    positional: true,
};

pub const LIST: CommandSpec = CommandSpec {
    name: "profile.list",
    about: "List named configuration profiles and where they are used",
    params: &[],
    mutating: false,
    handler: list,
};

pub const SHOW: CommandSpec = CommandSpec {
    name: "profile.show",
    about: "Show one configuration profile and where it is assigned",
    params: &[NAME],
    mutating: false,
    handler: show,
};

pub const SET: CommandSpec = CommandSpec {
    name: "profile.set",
    about: "Create or replace a profile from a TOML file",
    params: &[
        NAME,
        ParamSpec {
            name: "from-file",
            about: "TOML file with the profile's [agents] and/or [mcp] tables",
            kind: ParamKind::String,
            required: true,
            positional: false,
        },
    ],
    mutating: true,
    handler: set,
};

pub const RM: CommandSpec = CommandSpec {
    name: "profile.rm",
    about: "Remove a profile from the manifest (refused while assigned)",
    params: &[NAME],
    mutating: true,
    handler: rm,
};

fn profile_name(params: &Params) -> Result<String, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("profile name is required".into()))?;
    machine::validate_name(name)
}

/// Where a profile is assigned (the layers `plan --explain` would show).
#[derive(Serialize)]
struct Usage {
    groups: Vec<String>,
    members: Vec<String>,
    machines: Vec<String>,
}

impl Usage {
    fn of(manifest: &Manifest, name: &str) -> Self {
        fn named<'a, T: 'a>(
            entries: impl IntoIterator<Item = (&'a String, &'a T)>,
            profile: impl Fn(&T) -> Option<&str>,
            name: &str,
        ) -> Vec<String> {
            entries
                .into_iter()
                .filter(|(_, e)| profile(e) == Some(name))
                .map(|(k, _)| k.clone())
                .collect()
        }
        Self {
            groups: named(&manifest.groups, |g| g.profile.as_deref(), name),
            members: named(&manifest.members, |m| m.profile.as_deref(), name),
            machines: named(&manifest.machines, |m| m.profile.as_deref(), name),
        }
    }

    fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.members.is_empty() && self.machines.is_empty()
    }
}

/// Team mode: profile changes are admin changes.
fn actor(ctx: &Context, manifest: &Manifest) -> Result<Option<team::Actor>, MusterError> {
    if manifest.members.is_empty() {
        return Ok(None);
    }
    team::admin(manifest, &ctx.state_dir).map(Some)
}

fn commit(
    ctx: &mut Context,
    actor: Option<&team::Actor>,
    summary: &str,
) -> Result<(), MusterError> {
    match actor {
        Some(actor) => team::commit(ctx, actor, summary),
        None => crate::git::commit_if_configured(&ctx.config_dir, summary).map(|_| ()),
    }
}

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let profiles: Vec<_> = manifest
        .profiles
        .iter()
        .map(|(name, p)| {
            json!({
                "name": name,
                "profile": p,
                "used_by": Usage::of(&manifest, name),
            })
        })
        .collect();
    Ok(json!({ "profiles": profiles }))
}

fn show(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = profile_name(params)?;
    let manifest = Manifest::load(&ctx.config_dir)?;
    let profile = manifest
        .profiles
        .get(&name)
        .ok_or_else(|| not_found(&name))?;
    Ok(json!({
        "name": name,
        "profile": profile,
        "used_by": Usage::of(&manifest, &name),
    }))
}

fn set(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = profile_name(params)?;
    let path = params
        .get_string("from-file")
        .ok_or_else(|| MusterError::Usage("--from-file is required".into()))?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = actor(ctx, &manifest)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| MusterError::Usage(format!("cannot read {path}: {e}")))?;
    let profile: Profile = toml::from_str(&text).map_err(|e| MusterError::InvalidConfig {
        message: format!("invalid profile: {}", e.message()),
        path: Some(path.to_string()),
    })?;

    let result = json!({ "name": name, "profile": profile });
    if ctx.dry_run {
        return Ok(result);
    }
    manifest.profiles.insert(name.clone(), profile);
    manifest.save(&ctx.config_dir)?;
    commit(
        ctx,
        actor.as_ref(),
        &format!("feat(profile): set profile {name}"),
    )?;
    Ok(result)
}

fn rm(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = profile_name(params)?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = actor(ctx, &manifest)?;
    if !manifest.profiles.contains_key(&name) {
        return Err(not_found(&name));
    }
    let usage = Usage::of(&manifest, &name);
    if !usage.is_empty() {
        return Err(MusterError::Usage(format!(
            "profile '{name}' is still assigned ({}); unassign it first",
            serde_json::to_string(&usage).expect("usage serializes")
        )));
    }
    ctx.confirm(&format!("Remove profile '{name}'?"))?;

    let result = json!({ "removed": name });
    if ctx.dry_run {
        return Ok(result);
    }
    manifest.profiles.remove(&name);
    manifest.save(&ctx.config_dir)?;
    commit(
        ctx,
        actor.as_ref(),
        &format!("feat(profile): remove profile {name}"),
    )?;
    Ok(result)
}

fn not_found(name: &str) -> MusterError {
    MusterError::Usage(format!("profile '{name}' is not declared"))
}
