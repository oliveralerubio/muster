//! `muster secret`: manage secrets in the native vault and inspect providers.
//!
//! Values come from stdin (never argv in the happy path). Reveal is explicit
//! (`muster secret get <ref> --reveal`), requires confirmation and is the only
//! path that prints plaintext.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::git;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::{validate_allow_entry, Manifest, SecretPolicy};
use crate::output::{Diagnostic, Severity};
use crate::providers::Providers;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::secrets::{is_name, is_reference, Scheme, SecretRef, SecretValue};
use crate::vault::Vault;

pub const ADD: CommandSpec = CommandSpec {
    name: "secret.add",
    about: "Store a new secret in the native vault",
    params: &[
        ParamSpec {
            name: "path",
            about: "Secret path as group/name (e.g. app/api)",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "value-stdin",
            about: "Read the secret value from stdin",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "allow",
            about: "Agent allowed to receive the secret (repeatable): <agent> everywhere or \
                    <agent>@<project> only in that project. Without it no agent receives it",
            kind: ParamKind::List,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: add,
};

pub const LIST: CommandSpec = CommandSpec {
    name: "secret.list",
    about: "List secret names, groups, readers and allowed agents (never values)",
    params: &[],
    mutating: false,
    handler: list,
};

pub const ROTATE: CommandSpec = CommandSpec {
    name: "secret.rotate",
    about: "Replace an existing secret's value in the native vault",
    params: &[
        ParamSpec {
            name: "path",
            about: "Secret path as group/name (e.g. app/api)",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "value-stdin",
            about: "Read the new secret value from stdin",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: rotate,
};

pub const RM: CommandSpec = CommandSpec {
    name: "secret.rm",
    about: "Remove a secret from the native vault",
    params: &[ParamSpec {
        name: "path",
        about: "Secret path as group/name (e.g. app/api)",
        kind: ParamKind::String,
        required: true,
        positional: true,
    }],
    mutating: true,
    handler: rm,
};

pub const GET: CommandSpec = CommandSpec {
    name: "secret.get",
    about: "Resolve a secret reference; values require --reveal",
    params: &[
        ParamSpec {
            name: "reference",
            about: "Secret reference (e.g. native://app/api, env://VAR, op://...)",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "reveal",
            about: "Print the plaintext secret (requires confirmation)",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: false,
    handler: get,
};

pub const CHECK: CommandSpec = CommandSpec {
    name: "secret.check",
    about: "Check secret provider availability or verify a reference",
    params: &[ParamSpec {
        name: "reference",
        about: "Optional secret reference to verify",
        kind: ParamKind::String,
        required: false,
        positional: true,
    }],
    mutating: false,
    handler: check,
};

fn add(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let (group, name) = parse_path(params)?;
    let allow = params.get_list("allow");
    for entry in &allow {
        validate_allow_entry(entry).map_err(MusterError::Usage)?;
    }
    let value = read_value(ctx, params)?;

    let vault = Vault::open(&ctx.config_dir)?;
    if vault.groups()?.contains(&group) && vault.list(&group)?.iter().any(|s| s.name == name) {
        return Err(MusterError::Usage(format!(
            "secret '{group}/{name}' already exists; rotate it instead"
        )));
    }

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "group": group,
            "name": name,
            "reference": format!("native://{group}/{name}"),
            "allow": allow,
        }));
    }

    let identity = Identity::load(&ctx.state_dir)?;
    let mut vault = vault.with_actor(machine::load_name(&ctx.state_dir));
    let recipients = resolve_recipients(ctx, &vault, &group, &identity)?;
    vault.add(&group, &name, value, &recipients, &identity)?;
    if !allow.is_empty() {
        let mut manifest = Manifest::load(&ctx.config_dir)?;
        manifest.secrets.insert(
            format!("native://{group}/{name}"),
            SecretPolicy {
                allow: allow.clone(),
            },
        );
        manifest.save(&ctx.config_dir)?;
    }

    git::commit_if_configured(
        &ctx.config_dir,
        &format!("feat(secret): add {group}/{name}"),
    )?;

    Ok(json!({
        "group": group,
        "name": name,
        "reference": format!("native://{group}/{name}"),
        "allow": allow,
    }))
}

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir).ok();
    let vault = Vault::open(&ctx.config_dir)?;
    let mut secrets = Vec::new();
    for group in vault.groups()? {
        for meta in vault.list(&group)? {
            let reference = format!("native://{group}/{}", meta.name);
            let allow = manifest
                .as_ref()
                .and_then(|m| m.secrets.get(&reference))
                .map(|p| p.allow.clone())
                .unwrap_or_default();
            secrets.push(json!({
                "group": group,
                "name": meta.name,
                "reference": reference,
                "allow": allow,
                "created_at": meta.created_at,
                "updated_at": meta.updated_at,
                "created_by": meta.created_by,
                "updated_by": meta.updated_by,
                "readers": readers(&manifest, &meta.recipients),
            }));
        }
    }
    Ok(Value::Array(secrets))
}

fn rotate(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let (group, name) = parse_path(params)?;
    let value = read_value(ctx, params)?;

    let vault = Vault::open(&ctx.config_dir)?;
    if !vault.groups()?.contains(&group) || !vault.list(&group)?.iter().any(|s| s.name == name) {
        return Err(MusterError::Operational(format!(
            "secret '{group}/{name}' does not exist"
        )));
    }

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "group": group,
            "name": name,
            "reference": format!("native://{group}/{name}"),
        }));
    }

    let identity = Identity::load(&ctx.state_dir)?;
    let mut vault = vault.with_actor(machine::load_name(&ctx.state_dir));
    vault.rotate(&group, &name, value, &identity)?;

    git::commit_if_configured(
        &ctx.config_dir,
        &format!("feat(secret): rotate {group}/{name}"),
    )?;

    Ok(json!({
        "rotated": format!("{group}/{name}"),
        "group": group,
        "name": name,
        "reference": format!("native://{group}/{name}"),
    }))
}

fn rm(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let (group, name) = parse_path(params)?;

    let mut vault = Vault::open(&ctx.config_dir)?;
    if !vault.groups()?.contains(&group) || !vault.list(&group)?.iter().any(|s| s.name == name) {
        return Err(MusterError::Operational(format!(
            "secret '{group}/{name}' does not exist"
        )));
    }

    ctx.confirm(&format!(
        "Remove secret '{group}/{name}' from the native vault?"
    ))?;

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "removed": format!("{group}/{name}"),
        }));
    }

    let identity = Identity::load(&ctx.state_dir)?;
    vault.remove(&group, &name, &identity)?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    if manifest
        .secrets
        .remove(&format!("native://{group}/{name}"))
        .is_some()
    {
        manifest.save(&ctx.config_dir)?;
    }

    git::commit_if_configured(
        &ctx.config_dir,
        &format!("feat(secret): remove {group}/{name}"),
    )?;

    Ok(json!({ "removed": format!("{group}/{name}") }))
}

fn get(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let reference = SecretRef::parse(required(params, "reference")?)?;
    let reveal = params.get_bool("reveal");
    if reveal {
        ctx.confirm(&format!("Print the plaintext of {reference} on stdout?"))?;
    }
    let mut providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    let value = providers.resolve_ref(&reference)?;
    if reveal {
        // The one explicit reveal path: the value becomes the envelope result,
        // and only once the reveal is on record.
        crate::audit::record(&ctx.state_dir, "secret.reveal", reference.as_str())?;
        return Ok(Value::String(value.expose().to_owned()));
    }
    Ok(json!({
        "reference": reference.as_str(),
        "provider": reference.scheme(),
        "resolved": true,
    }))
}

fn check(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let mut providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    if let Some(raw) = params.get_string("reference") {
        // Never resolves: plaintext is not materialised just to check it.
        let reference = SecretRef::parse(raw)?;
        let exists = providers.locate(&reference)?;
        if !exists {
            ctx.diag(Diagnostic::new(
                Severity::Info,
                "existence_unverified",
                format!(
                    "{} is available, but it cannot confirm {reference} without reading the value; \
                     use `muster secret get` to resolve it",
                    reference.scheme().as_str()
                ),
            ));
        }
        return Ok(json!({
            "reference": reference.as_str(),
            "provider": reference.scheme(),
            "available": true,
            "exists": if exists { Value::Bool(true) } else { Value::Null },
        }));
    }
    let mut list = Vec::new();
    for scheme in Scheme::ALL {
        let provider = providers.provider(scheme);
        let description = provider.describe();
        let entry = match provider.check() {
            Ok(()) => json!({ "provider": description, "available": true }),
            Err(e) => json!({
                "provider": description,
                "available": false,
                "code": e.code(),
                "message": e.to_string(),
            }),
        };
        list.push(entry);
    }
    Ok(json!({ "providers": list }))
}

/// Readers as `{recipient, machine?}`; `machine` is the fleet name or
/// `"recovery"` for the offline recovery key.
fn readers(manifest: &Option<Manifest>, recipients: &[String]) -> Vec<Value> {
    recipients
        .iter()
        .map(|recipient| {
            let machine = manifest.as_ref().and_then(|m| {
                m.machines
                    .iter()
                    .find(|(_, machine)| &machine.recipient == recipient)
                    .map(|(name, _)| name.clone())
                    .or_else(|| {
                        m.recovery
                            .as_ref()
                            .filter(|r| &r.recipient == recipient)
                            .map(|_| "recovery".to_owned())
                    })
            });
            json!({ "recipient": recipient, "machine": machine })
        })
        .collect()
}

fn required<'p>(params: &'p Params, name: &str) -> Result<&'p str, MusterError> {
    params
        .get_string(name)
        .ok_or_else(|| MusterError::Usage(format!("{name} is required")))
}

fn parse_path(params: &Params) -> Result<(String, String), MusterError> {
    let raw = required(params, "path")?;
    if is_reference(raw) && !raw.starts_with("native://") {
        return Err(MusterError::Usage(format!(
            "'{raw}' lives in an external provider; only native:// secrets are managed by muster"
        )));
    }
    let path = raw.strip_prefix("native://").unwrap_or(raw);
    let (group, name) = path.split_once('/').ok_or_else(|| {
        MusterError::Usage(format!(
            "invalid secret path '{raw}': expected <group>/<name> (e.g. app/api)"
        ))
    })?;
    if !is_name(group) || !is_name(name) {
        return Err(MusterError::Usage(format!(
            "invalid secret path '{raw}': names use letters, digits, '_', '-' or '.'"
        )));
    }
    Ok((group.to_owned(), name.to_owned()))
}

/// The value for add/rotate: stdin only, never argv.
fn read_value(ctx: &Context, params: &Params) -> Result<SecretValue, MusterError> {
    if !params.get_bool("value-stdin") {
        return Err(MusterError::Usage(
            "pass --value-stdin and pipe the value (values are never taken from arguments)".into(),
        ));
    }
    ctx.read_secret_stdin()
}

fn resolve_recipients(
    ctx: &Context,
    vault: &Vault,
    group: &str,
    identity: &Identity,
) -> Result<Vec<String>, MusterError> {
    if vault.groups()?.contains(&group.to_string()) {
        return vault.readers(group);
    }
    let manifest = Manifest::load(&ctx.config_dir)?;
    let mut recipients = crate::team::group_recipients(&manifest, group);
    // When the group is not declared, the creating machine also reads it.
    if !manifest.groups.contains_key(group) {
        let mine = identity.recipient();
        if !recipients.contains(&mine) {
            recipients.push(mine);
        }
    }
    if recipients.is_empty() {
        return Err(MusterError::Usage(
            "no fleet recipients or recovery key found; run `muster init` first".into(),
        ));
    }
    Ok(recipients)
}
