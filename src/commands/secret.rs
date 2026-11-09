//! `muster secret`: manage secrets in the native vault and inspect providers.
//!
//! Values come from stdin (never argv in the happy path). Reveal is explicit
//! (`muster secret get <ref> --reveal`), requires confirmation and is the only
//! path that prints plaintext.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::{validate_allow_entry, Manifest, SecretApproval, SecretPolicy};
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
            name: "prompt",
            about: "Prompt for the secret value in a graphical dialog (never echoed or returned)",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "reason",
            about: "Context shown in the graphical prompt explaining why the secret is needed",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "value",
            about: "The secret value, supplied out of band (stdin for the CLI, request body for the panel); never an argument",
            kind: ParamKind::Secret,
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
        ParamSpec {
            name: "prompt",
            about: "Prompt for the new secret value in a graphical dialog (never echoed or returned)",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "reason",
            about: "Context shown in the graphical prompt explaining why the secret is rotated",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "value",
            about: "The secret value, supplied out of band (stdin for the CLI, request body for the panel); never an argument",
            kind: ParamKind::Secret,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: rotate,
};

pub const PROMPT: CommandSpec = CommandSpec {
    name: "secret.prompt",
    about: "Request a secret from the user via a graphical dialog and store it in the native vault without exposing the value",
    params: &[
        ParamSpec {
            name: "path",
            about: "Secret path as group/name (e.g. app/api)",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "reason",
            about: "Context shown in the graphical prompt explaining why the secret is needed",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "allow",
            about: "Agent allowed to receive the secret (repeatable): <agent> everywhere or \
                    <agent>@<project> only in that project",
            kind: ParamKind::List,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "rotate",
            about: "Replace the secret value if it already exists in the native vault",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: prompt_command,
};

pub const RUN: CommandSpec = CommandSpec {
    name: "secret.run",
    about: "Run a command with resolved secrets injected into its environment or stdin and redacted from output",
    params: &[
        ParamSpec {
            name: "agent",
            about: "Declared calling agent (checked against secret access policies and approval leases)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "env",
            about: "Environment variable binding as VAR=<reference> (repeatable, e.g. API_KEY=native://app/api)",
            kind: ParamKind::List,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "stdin",
            about: "Secret reference to pipe to the command's stdin (e.g. native://app/api)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "command",
            about: "Command and arguments to execute (pass after --)",
            kind: ParamKind::List,
            required: true,
            positional: true,
        },
    ],
    mutating: false,
    handler: run_with_secrets,
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
            name: "agent",
            about: "Declared calling agent (checked at reveal time; not an identity boundary)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
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
    let reason = validate_reason(params.get_string("reason"))?;
    if params.get_bool("prompt") && params.get_bool("value-stdin") {
        return Err(MusterError::Usage(
            "use either --value-stdin or --prompt".into(),
        ));
    }
    if reason.is_some() && !params.get_bool("prompt") {
        return Err(MusterError::Usage("--reason requires --prompt".into()));
    }
    let vault = Vault::open(&ctx.config_dir)?;
    if vault.groups()?.contains(&group) && vault.list(&group)?.iter().any(|s| s.name == name) {
        return Err(MusterError::Usage(format!(
            "secret '{group}/{name}' already exists; rotate it instead"
        )));
    }
    if params.get_bool("prompt") {
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
        let _ = resolve_recipients(ctx, &vault, &group, &identity)?;
        let value = prompt_secret_gui(ctx, &group, &name, false, reason, &allow)?;
        return add_with_value(ctx, params, value);
    }
    let value = read_value(ctx, params)?;
    add_with_value(ctx, params, value)
}

pub(crate) fn add_with_value(
    ctx: &mut Context,
    params: &Params,
    value: SecretValue,
) -> Result<Value, MusterError> {
    let (group, name) = parse_path(params)?;
    let allow = params.get_list("allow");
    for entry in &allow {
        validate_allow_entry(entry).map_err(MusterError::Usage)?;
    }

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
    crate::audit::record(&ctx.state_dir, "secret.add", &format!("{group}/{name}"))?;
    vault.add(&group, &name, value, &recipients, &identity)?;
    if !allow.is_empty() {
        let mut manifest = Manifest::load(&ctx.config_dir)?;
        manifest.secrets.insert(
            format!("native://{group}/{name}"),
            SecretPolicy {
                allow: Some(allow.clone()),
                ..Default::default()
            },
        );
        manifest.save(&ctx.config_dir)?;
    }

    crate::trust::commit(ctx, &format!("feat(secret): add {group}/{name}"))?;

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
                .map(|p| p.allow_list().to_vec())
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
    let reason = validate_reason(params.get_string("reason"))?;
    if params.get_bool("prompt") && params.get_bool("value-stdin") {
        return Err(MusterError::Usage(
            "use either --value-stdin or --prompt".into(),
        ));
    }
    if reason.is_some() && !params.get_bool("prompt") {
        return Err(MusterError::Usage("--reason requires --prompt".into()));
    }
    let vault = Vault::open(&ctx.config_dir)?;
    if !vault.groups()?.contains(&group) || !vault.list(&group)?.iter().any(|s| s.name == name) {
        return Err(MusterError::Operational(format!(
            "secret '{group}/{name}' does not exist"
        )));
    }
    if params.get_bool("prompt") {
        if ctx.dry_run {
            return Ok(json!({
                "dry_run": true,
                "group": group,
                "name": name,
                "reference": format!("native://{group}/{name}"),
            }));
        }
        let _ = Identity::load(&ctx.state_dir)?;
        let value = prompt_secret_gui(ctx, &group, &name, true, reason, &[])?;
        return rotate_with_value(ctx, params, value);
    }
    let value = read_value(ctx, params)?;
    rotate_with_value(ctx, params, value)
}

pub(crate) fn rotate_with_value(
    ctx: &mut Context,
    params: &Params,
    value: SecretValue,
) -> Result<Value, MusterError> {
    let (group, name) = parse_path(params)?;

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
    crate::audit::record(&ctx.state_dir, "secret.rotate", &format!("{group}/{name}"))?;
    vault.rotate(&group, &name, value, &identity)?;

    crate::trust::commit(ctx, &format!("feat(secret): rotate {group}/{name}"))?;

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

    crate::trust::commit(ctx, &format!("feat(secret): remove {group}/{name}"))?;

    Ok(json!({ "removed": format!("{group}/{name}") }))
}

fn authorize_secret_use(
    ctx: &mut Context,
    reference: &SecretRef,
    actor: Option<&str>,
) -> Result<Option<crate::secret_leases::Store>, MusterError> {
    // A secret with an allow list or per-use approval is never used or revealed
    // by an unnamed caller: `--yes` alone does not satisfy a policy.
    if actor.is_none() {
        // Fail closed: an unreadable manifest must never turn into "no policy".
        let manifest = match Manifest::load(&ctx.config_dir) {
            Ok(m) => Some(m),
            Err(MusterError::Operational(_)) if !ctx.config_dir.join("muster.toml").exists() => {
                let pin_exists = crate::fleet_state::TrustAnchor::load(&ctx.state_dir)?.is_some();
                let state_exists = ctx.config_dir.join(crate::fleet_state::STATE_FILE).exists();
                if pin_exists || state_exists {
                    return Err(MusterError::Security(
                        "cannot reveal secret: muster.toml is missing but a trust pin or signed fleet state exists".into(),
                    ));
                }
                None
            }
            Err(e) => return Err(e),
        };
        let policy = manifest
            .as_ref()
            .and_then(|m| m.secrets.get(reference.as_str()));
        if policy.is_some_and(|p| p.allow.as_ref().is_some_and(Vec::is_empty)) {
            ctx.diag(Diagnostic::error(
                "secret_allow_list_empty",
                format!("{reference} has `allow = []`: no one may reveal it"),
            ));
            return Err(MusterError::Security(
                "secret has an empty allow list; access denied".into(),
            ));
        }
        let restricted = policy
            .is_some_and(|p| !p.allow_list().is_empty() || p.approval == SecretApproval::Each);
        if restricted {
            ctx.diag(Diagnostic::error(
                "secret_policy_requires_agent",
                format!(
                    "{reference} has an access policy; reveal it with --agent <id> so the \
                     policy (and any approval lease) can be enforced"
                ),
            ));
            return Err(MusterError::Security(
                "secret policy requires an agent identity".into(),
            ));
        }
    }
    if let Some(agent) = actor {
        crate::secret_leases::authorize(ctx, reference.as_str(), agent)
    } else {
        Ok(None)
    }
}

fn get(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let reference = SecretRef::parse(required(params, "reference")?)?;
    let reveal = params.get_bool("reveal");
    let actor = params
        .get_string("agent")
        .map(str::to_owned)
        .or_else(|| ctx.audit_actor.clone());
    // Check current policy before resolving any value, including MCP calls.
    let _lease_guard = if reveal {
        authorize_secret_use(ctx, &reference, actor.as_deref())?
    } else {
        None
    };
    if reveal {
        ctx.confirm(&format!("Print the plaintext of {reference} on stdout?"))?;
    }
    let mut providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    let value = providers.resolve_ref(&reference)?;
    if reveal {
        // The one explicit reveal path: the value becomes the envelope result,
        // and only once the reveal is on record.
        if let Some(actor) = &actor {
            crate::audit::record_with_actor(
                &ctx.state_dir,
                "secret.reveal",
                reference.as_str(),
                actor,
            )?;
        } else {
            crate::audit::record(&ctx.state_dir, "secret.reveal", reference.as_str())?;
        }
        return Ok(Value::String(value.expose().to_owned()));
    }
    Ok(json!({
        "reference": reference.as_str(),
        "provider": reference.scheme(),
        "resolved": true,
    }))
}

fn prompt_command(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let (group, name) = parse_path(params)?;
    let allow = params.get_list("allow");
    for entry in &allow {
        validate_allow_entry(entry).map_err(MusterError::Usage)?;
    }
    let reason = validate_reason(params.get_string("reason"))?;
    let vault = Vault::open(&ctx.config_dir)?;
    let exists =
        vault.groups()?.contains(&group) && vault.list(&group)?.iter().any(|s| s.name == name);
    if exists && !params.get_bool("rotate") {
        return Err(MusterError::Usage(format!(
            "secret '{group}/{name}' already exists; pass --rotate to replace it"
        )));
    }
    let action = if exists { "rotated" } else { "added" };
    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "action": action,
            "group": group,
            "name": name,
            "reference": format!("native://{group}/{name}"),
            "allow": allow,
        }));
    }
    let identity = Identity::load(&ctx.state_dir)?;
    if !exists {
        let _ = resolve_recipients(ctx, &vault, &group, &identity)?;
    }
    let value = prompt_secret_gui(ctx, &group, &name, exists, reason, &allow)?;
    if exists {
        let mut out = rotate_with_value(ctx, params, value)?;
        if !allow.is_empty() {
            let mut manifest = Manifest::load(&ctx.config_dir)?;
            let policy = manifest
                .secrets
                .entry(format!("native://{group}/{name}"))
                .or_default();
            policy.allow = Some(allow.clone());
            manifest.save(&ctx.config_dir)?;
            crate::trust::commit(
                ctx,
                &format!("feat(secret): update allow for {group}/{name}"),
            )?;
        }
        out["action"] = json!("rotated");
        out["allow"] = json!(allow);
        Ok(out)
    } else {
        let mut out = add_with_value(ctx, params, value)?;
        out["action"] = json!("added");
        Ok(out)
    }
}

fn run_with_secrets(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    use secrecy::zeroize::Zeroize;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Write;
    use std::process::{Command, Stdio};

    let command = params.get_list("command");
    if command.is_empty() || command[0].trim().is_empty() {
        return Err(MusterError::Usage("command is required".into()));
    }
    let env_specs = params.get_list("env");
    let stdin_spec = params.get_string("stdin");
    if env_specs.is_empty() && stdin_spec.is_none() {
        return Err(MusterError::Usage(
            "at least one --env VAR=<reference> or --stdin <reference> is required".into(),
        ));
    }

    let mut env_bindings: Vec<(String, SecretRef)> = Vec::with_capacity(env_specs.len());
    let mut unique_refs: Vec<SecretRef> = Vec::new();
    let mut seen_refs = BTreeSet::new();

    for entry in &env_specs {
        let (var, raw_ref) = entry.split_once('=').ok_or_else(|| {
            MusterError::Usage(format!(
                "invalid --env binding '{entry}': expected VAR=<reference>"
            ))
        })?;
        validate_run_env_var(var)?;
        let parsed = SecretRef::parse(raw_ref)?;
        if seen_refs.insert(parsed.as_str().to_owned()) {
            unique_refs.push(parsed.clone());
        }
        env_bindings.push((var.to_owned(), parsed));
    }

    let stdin_ref = match stdin_spec {
        Some(raw) => {
            let parsed = SecretRef::parse(raw)?;
            if seen_refs.insert(parsed.as_str().to_owned()) {
                unique_refs.push(parsed.clone());
            }
            Some(parsed)
        }
        None => None,
    };

    let actor = params
        .get_string("agent")
        .map(str::to_owned)
        .or_else(|| ctx.audit_actor.clone());

    let mut lease_guards = Vec::new();
    for reference in &unique_refs {
        if let Some(guard) = authorize_secret_use(ctx, reference, actor.as_deref())? {
            lease_guards.push(guard);
        }
    }

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "command": command,
            "env": env_bindings
                .iter()
                .map(|(var, r)| json!({ "var": var, "reference": r.as_str() }))
                .collect::<Vec<_>>(),
            "stdin": stdin_ref.as_ref().map(SecretRef::as_str),
        }));
    }

    let mut providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    let mut resolved: BTreeMap<String, SecretValue> = BTreeMap::new();
    for reference in &unique_refs {
        let secret = providers.resolve_ref(reference)?;
        if let Some(actor) = &actor {
            crate::audit::record_with_actor(
                &ctx.state_dir,
                "secret.run",
                reference.as_str(),
                actor,
            )?;
        } else {
            crate::audit::record(&ctx.state_dir, "secret.run", reference.as_str())?;
        }
        resolved.insert(reference.as_str().to_owned(), secret);
    }
    drop(lease_guards);

    let mut child_cmd = Command::new(&command[0]);
    child_cmd.args(&command[1..]);
    for (var, reference) in &env_bindings {
        if let Some(secret) = resolved.get(reference.as_str()) {
            child_cmd.env(var, secret.expose());
        }
    }
    if stdin_ref.is_some() {
        child_cmd.stdin(Stdio::piped());
    } else {
        child_cmd.stdin(Stdio::null());
    }
    child_cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = child_cmd
        .spawn()
        .map_err(|e| MusterError::Operational(format!("cannot run '{}': {e}", command[0])))?;

    if let Some(ref reference) = stdin_ref {
        if let (Some(mut stdin), Some(secret)) =
            (child.stdin.take(), resolved.get(reference.as_str()))
        {
            let _ = stdin.write_all(secret.expose().as_bytes());
        }
    }

    let mut output = child
        .wait_with_output()
        .map_err(|e| MusterError::Operational(format!("waiting for '{}': {e}", command[0])))?;

    let mut raw_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut raw_stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    output.stdout.zeroize();
    output.stderr.zeroize();

    let (clean_stdout, redacted_out) = scrub_secrets(&raw_stdout, &resolved);
    let (clean_stderr, redacted_err) = scrub_secrets(&raw_stderr, &resolved);
    raw_stdout.zeroize();
    raw_stderr.zeroize();
    let redacted = redacted_out || redacted_err;

    if redacted {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "secret_output_redacted",
            "redacted secret material from command output",
        ));
    }

    let exit_code = output.status.code().unwrap_or(1);
    if !output.status.success() {
        ctx.mark_failed();
        ctx.diag(Diagnostic::error(
            "command_failed",
            format!("command '{}' exited with status {exit_code}", command[0]),
        ));
    }

    Ok(json!({
        "exit_code": exit_code,
        "stdout": clean_stdout,
        "stderr": clean_stderr,
        "redacted": redacted,
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

/// The value for add/rotate: stdin or out-of-band slot only, never argv.
fn read_value(ctx: &Context, params: &Params) -> Result<SecretValue, MusterError> {
    if let Some(value) = params.take_secret() {
        return Ok(value);
    }
    if !params.get_bool("value-stdin") {
        return Err(MusterError::Usage(
            "pass --value-stdin and pipe the value, or pass --prompt for a graphical dialog (values are never taken from arguments)".into(),
        ));
    }
    ctx.read_secret_stdin()
}

pub(crate) fn validate_reason(reason: Option<&str>) -> Result<Option<&str>, MusterError> {
    let Some(r) = reason else {
        return Ok(None);
    };
    let trimmed = r.trim();
    if trimmed.is_empty() {
        return Err(MusterError::Usage("reason must not be empty".into()));
    }
    if trimmed.chars().count() > 200 {
        return Err(MusterError::Usage(
            "reason must be at most 200 characters".into(),
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(MusterError::Usage(
            "reason must not contain control characters".into(),
        ));
    }
    Ok(Some(trimmed))
}

fn validate_run_env_var(var: &str) -> Result<(), MusterError> {
    let mut chars = var.chars();
    let valid_syntax = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_syntax {
        return Err(MusterError::Usage(format!(
            "invalid environment variable name '{var}'"
        )));
    }
    let upper = var.to_ascii_uppercase();
    let forbidden = matches!(
        upper.as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "LOGNAME"
            | "SHELL"
            | "IFS"
            | "ENV"
            | "BASH_ENV"
            | "PROMPT_COMMAND"
            | "CDPATH"
    ) || upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || upper.starts_with("MUSTER_");
    if forbidden {
        return Err(MusterError::Security(format!(
            "refusing to override protected environment variable '{var}'"
        )));
    }
    Ok(())
}

fn scrub_secrets(
    text: &str,
    secrets: &std::collections::BTreeMap<String, SecretValue>,
) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }
    let mut out = text.to_owned();
    let mut redacted = false;
    for (reference, secret) in secrets {
        let replacement = format!("<redacted:{reference}>");
        for form in secret_redaction_forms(secret.expose()) {
            if out.contains(&form) {
                out = out.replace(&form, &replacement);
                redacted = true;
            }
        }
    }
    (out, redacted)
}

fn secret_redaction_forms(value: &str) -> Vec<String> {
    let json_escaped = serde_json::to_string(value)
        .unwrap_or_default()
        .trim_matches('"')
        .to_owned();
    let toml_escaped = toml::Value::String(value.to_owned())
        .to_string()
        .trim_matches('"')
        .to_owned();
    let url_encoded = percent_encode_secret(value);
    let b64_raw = base64_encode(value.as_bytes());
    let b64_nl = base64_encode(format!("{value}\n").as_bytes());
    let mut forms = vec![
        value.to_owned(),
        json_escaped,
        toml_escaped,
        value.replace('\'', "''"),
        url_encoded,
        b64_raw,
        b64_nl,
    ];
    for line in value.lines() {
        let trimmed = line.trim();
        if trimmed.len() >= 4 {
            forms.push(trimmed.to_owned());
        }
    }
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    forms.dedup();
    forms.retain(|f| !f.is_empty());
    forms
}

fn percent_encode_secret(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(char::from(TABLE[usize::from(b0 >> 2)]));
        out.push(char::from(
            TABLE[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))],
        ));
        if chunk.len() > 1 {
            out.push(char::from(
                TABLE[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))],
            ));
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(char::from(TABLE[usize::from(b2 & 0x3f)]));
        } else {
            out.push('=');
        }
    }
    out
}

const ASKPASS_ENV: &str = "MUSTER_ASKPASS";
const BROWSER_ENV: &str = "MUSTER_BROWSER";
const BACKEND_ENV: &str = "MUSTER_PROMPT_BACKEND";
const TIMEOUT_ENV: &str = "MUSTER_PROMPT_TIMEOUT_MS";
const MAX_SECRET_BYTES: usize = 64 * 1024;

/// Prompt the human for a secret value via a graphical dialog so the value
/// never passes through chat transcripts, argv, stdin or stdout.
pub(crate) fn prompt_secret_gui(
    ctx: &Context,
    group: &str,
    name: &str,
    rotate: bool,
    reason: Option<&str>,
    allow: &[String],
) -> Result<SecretValue, MusterError> {
    let reference = format!("native://{group}/{name}");
    let action_verb = if rotate { "Rotate" } else { "Add" };
    let allow_desc = if allow.is_empty() {
        "none (human only)".to_owned()
    } else {
        allow.join(", ")
    };
    let title = format!("Muster: {action_verb} {reference}");
    let mut description =
        format!("{action_verb} secret {reference} in Muster vault.\nAllowed agents: {allow_desc}");
    if let Some(r) = reason {
        description.push_str(&format!("\nReason: {r}"));
    }

    if let Some(program) = std::env::var_os(ASKPASS_ENV).filter(|v| !v.is_empty()) {
        return run_askpass_program(
            &program,
            &reference,
            rotate,
            reason,
            allow,
            &title,
            &description,
        );
    }

    if !has_graphical_session() {
        return Err(MusterError::Unavailable(
            "no graphical session available to prompt for secret (DISPLAY and WAYLAND_DISPLAY are not set)".into(),
        ));
    }

    let forced_backend = std::env::var(BACKEND_ENV)
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());

    if forced_backend.as_deref() == Some("web") {
        return prompt_via_local_web(ctx, &reference, rotate, reason, allow);
    }

    if cfg!(target_os = "macos") && forced_backend.as_deref().is_none_or(|b| b == "osascript") {
        if let Some(bin) = crate::machine::find_executable("osascript") {
            return prompt_via_osascript(&bin, &title, &description);
        }
    }

    if forced_backend.as_deref().is_none_or(|b| b == "zenity") {
        if let Some(bin) = crate::machine::find_executable("zenity") {
            let zenity_title = match reason {
                Some(r) => format!("{title} ({r})"),
                None => title.clone(),
            };
            return run_dialog_command(&bin, &["--password", &format!("--title={zenity_title}")]);
        }
    }

    if forced_backend.as_deref().is_none_or(|b| b == "kdialog") {
        if let Some(bin) = crate::machine::find_executable("kdialog") {
            return run_dialog_command(&bin, &["--title", &title, "--password", &description]);
        }
    }

    if forced_backend.as_deref().is_none_or(|b| b == "pinentry") {
        for candidate in [
            "pinentry-gnome3",
            "pinentry-qt",
            "pinentry-mac",
            "pinentry-gtk-2",
            "pinentry",
        ] {
            if let Some(bin) = crate::machine::find_executable(candidate) {
                return prompt_via_pinentry(&bin, &title, &description);
            }
        }
    }

    prompt_via_local_web(ctx, &reference, rotate, reason, allow)
}

fn has_graphical_session() -> bool {
    cfg!(target_os = "macos")
        || std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os(BROWSER_ENV).is_some_and(|v| !v.is_empty())
}

fn run_askpass_program(
    program: &std::ffi::OsStr,
    reference: &str,
    rotate: bool,
    reason: Option<&str>,
    allow: &[String],
    title: &str,
    description: &str,
) -> Result<SecretValue, MusterError> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(program);
    cmd.arg(description)
        .env("MUSTER_SECRET_REF", reference)
        .env(
            "MUSTER_SECRET_ACTION",
            if rotate { "rotate" } else { "add" },
        )
        .env("MUSTER_SECRET_ALLOW", allow.join(","))
        .env("MUSTER_SECRET_TITLE", title)
        .env("MUSTER_SECRET_REASON", reason.unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    read_secret_from_child(cmd, "graphical prompt")
}

fn prompt_via_osascript(
    bin: &std::path::Path,
    title: &str,
    description: &str,
) -> Result<SecretValue, MusterError> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(bin);
    cmd.args([
        "-e",
        "on run argv",
        "-e",
        "text returned of (display dialog (item 1 of argv) default answer \"\" with title (item 2 of argv) with hidden answer buttons {\"Cancel\", \"Save\"} default button \"Save\" cancel button \"Cancel\")",
        "-e",
        "end run",
        "--",
        description,
        title,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    read_secret_from_child(cmd, "osascript")
}

fn run_dialog_command(bin: &std::path::Path, args: &[&str]) -> Result<SecretValue, MusterError> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    read_secret_from_child(cmd, "graphical dialog")
}

fn read_secret_from_child(
    mut cmd: std::process::Command,
    label: &str,
) -> Result<SecretValue, MusterError> {
    use secrecy::zeroize::Zeroize;
    use std::io::Read;

    let mut child = cmd
        .spawn()
        .map_err(|e| MusterError::Unavailable(format!("cannot start {label}: {e}")))?;
    let mut bytes = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        if let Err(e) = stdout
            .take((MAX_SECRET_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
        {
            bytes.zeroize();
            let _ = child.kill();
            let _ = child.wait();
            return Err(MusterError::Operational(format!(
                "cannot read from {label}: {e}"
            )));
        }
    }
    let status = child.wait().map_err(|e| {
        bytes.zeroize();
        MusterError::Operational(format!("waiting for {label}: {e}"))
    })?;
    if !status.success() {
        bytes.zeroize();
        return Err(MusterError::Operational(
            "secret prompt cancelled by user".into(),
        ));
    }
    if bytes.len() > MAX_SECRET_BYTES {
        bytes.zeroize();
        return Err(MusterError::Usage("secret value exceeds 64 KB".into()));
    }
    parse_secret_bytes(bytes, "the secret value from the graphical prompt is empty")
}

fn prompt_via_pinentry(
    bin: &std::path::Path,
    title: &str,
    description: &str,
) -> Result<SecretValue, MusterError> {
    use secrecy::zeroize::Zeroize;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new(bin)
        .env_remove("GPG_TTY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| MusterError::Unavailable(format!("cannot start pinentry: {e}")))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| MusterError::Operational("pinentry stdin unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| MusterError::Operational("pinentry stdout unavailable".into()))?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    let read_status = |reader: &mut BufReader<std::process::ChildStdout>,
                       line: &mut String|
     -> Result<(), MusterError> {
        loop {
            line.zeroize();
            line.clear();
            let n = reader
                .read_line(line)
                .map_err(|e| MusterError::Operational(format!("pinentry read failed: {e}")))?;
            if n == 0 {
                return Err(MusterError::Operational(
                    "pinentry closed before completing".into(),
                ));
            }
            if line.starts_with("OK") {
                return Ok(());
            }
            if line.starts_with("ERR") {
                return Err(MusterError::Operational(
                    "secret prompt cancelled by user".into(),
                ));
            }
        }
    };

    read_status(&mut reader, &mut line)?;
    for cmd in [
        format!("SETTITLE {}\n", percent_encode_assuan(title)),
        format!("SETDESC {}\n", percent_encode_assuan(description)),
        "SETPROMPT Secret:\n".to_owned(),
    ] {
        let _ = stdin.write_all(cmd.as_bytes());
        let _ = stdin.flush();
        read_status(&mut reader, &mut line)?;
    }

    let _ = stdin.write_all(b"GETPIN\n");
    let _ = stdin.flush();
    let mut secret_bytes: Vec<u8> = Vec::new();
    let outcome = loop {
        line.zeroize();
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                break Err(MusterError::Operational(
                    "pinentry closed before returning a secret".into(),
                ))
            }
            Ok(_) => {
                let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
                if let Some(data) = trimmed.strip_prefix("D ") {
                    percent_decode_into(data, &mut secret_bytes);
                } else if trimmed == "D" {
                    // Empty data line from pinentry.
                } else if trimmed.starts_with("OK") {
                    break Ok(());
                } else if trimmed.starts_with("ERR") {
                    break Err(MusterError::Operational(
                        "secret prompt cancelled by user".into(),
                    ));
                }
            }
            Err(e) => {
                break Err(MusterError::Operational(format!(
                    "pinentry read failed: {e}"
                )))
            }
        }
    };
    line.zeroize();
    let _ = stdin.write_all(b"BYE\n");
    drop(stdin);
    let _ = child.wait();
    if let Err(err) = outcome {
        secret_bytes.zeroize();
        return Err(err);
    }
    parse_secret_bytes(
        secret_bytes,
        "the secret value from the graphical prompt is empty",
    )
}

fn percent_encode_assuan(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if matches!(b, b'%' | b'\r' | b'\n') || b < 0x20 {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(char::from(b));
        }
    }
    out
}

fn percent_decode_into(text: &str, out: &mut Vec<u8>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(h) = bytes.get(i + 1..i + 3) {
                if h.iter().all(u8::is_ascii_hexdigit) {
                    if let Ok(hex) = std::str::from_utf8(h) {
                        if let Ok(val) = u8::from_str_radix(hex, 16) {
                            out.push(val);
                            i += 3;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
}

fn parse_secret_bytes(bytes: Vec<u8>, empty_msg: &str) -> Result<SecretValue, MusterError> {
    use secrecy::zeroize::Zeroize;
    let mut text = String::from_utf8(bytes).map_err(|e| {
        e.into_bytes().zeroize();
        MusterError::Usage("the secret value must be UTF-8 text".into())
    })?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    if text.is_empty() {
        text.zeroize();
        return Err(MusterError::Usage(empty_msg.into()));
    }
    Ok(SecretValue::new(text))
}

/// Ephemeral localhost web dialog fallback (`127.0.0.1:0`).
/// Never writes the one-time token or URL to stdout/stderr; opens the browser
/// directly and accepts a single write-only submission.
fn prompt_via_local_web(
    ctx: &Context,
    reference: &str,
    rotate: bool,
    reason: Option<&str>,
    allow: &[String],
) -> Result<SecretValue, MusterError> {
    use secrecy::zeroize::Zeroize;
    use std::io::Read;
    use std::time::{Duration, Instant};
    use tiny_http::{Method, Response, Server};

    let server = Server::http("127.0.0.1:0").map_err(|e| {
        MusterError::Operational(format!("cannot bind localhost prompt server: {e}"))
    })?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .ok_or_else(|| MusterError::Operational("prompt server has no TCP port".into()))?;

    let mut rand_bytes = [0u8; 48];
    getrandom::getrandom(&mut rand_bytes)
        .map_err(|e| MusterError::Operational(format!("CSPRNG failure: {e}")))?;
    let open_token: String = rand_bytes[0..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let session_token: String = rand_bytes[16..32]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let csrf_token: String = rand_bytes[32..48]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    rand_bytes.zeroize();

    let url = format!("http://127.0.0.1:{port}/prompt?token={open_token}");
    ctx.progress(&format!(
        "Waiting for secret input in graphical browser prompt for {reference}..."
    ));
    launch_prompt_browser(&url)?;

    let timeout_ms = std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300_000);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut token_available = true;
    let mut session_issued = false;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(250));
        let Some(mut request) = server
            .recv_timeout(wait)
            .map_err(|e| MusterError::Operational(format!("prompt server error: {e}")))?
        else {
            continue;
        };

        if !prompt_host_allowed(&request, port) {
            let _ = request.respond(
                Response::from_string("forbidden")
                    .with_status_code(403)
                    .with_header(prompt_header("Cache-Control", "no-store")),
            );
            continue;
        }

        let raw_url = request.url().to_owned();
        let (path, query) = raw_url.split_once('?').unwrap_or((&raw_url, ""));
        match (request.method(), path) {
            (&Method::Get, "/prompt") => {
                let given = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("token="))
                    .unwrap_or("");
                if !token_available || !constant_time_eq_str(&open_token, given) {
                    let _ = request.respond(
                        Response::from_string("unauthorized")
                            .with_status_code(401)
                            .with_header(prompt_header("Cache-Control", "no-store")),
                    );
                    continue;
                }
                token_available = false;
                session_issued = true;
                let html = render_prompt_html(reference, rotate, reason, allow, &csrf_token);
                let cookie =
                    format!("muster_prompt={session_token}; Path=/; HttpOnly; SameSite=Strict");
                let response = Response::from_string(html)
                    .with_status_code(200)
                    .with_header(prompt_header("Content-Type", "text/html; charset=utf-8"))
                    .with_header(prompt_header("Cache-Control", "no-store"))
                    .with_header(prompt_header("X-Frame-Options", "DENY"))
                    .with_header(prompt_header("X-Content-Type-Options", "nosniff"))
                    .with_header(prompt_header(
                        "Content-Security-Policy",
                        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
                    ))
                    .with_header(prompt_header("Set-Cookie", &cookie));
                let _ = request.respond(response);
            }
            (&Method::Post, "/submit") | (&Method::Post, "/cancel") => {
                let is_cancel = path == "/cancel";
                if !session_issued || !prompt_has_cookie(&request, &session_token) {
                    let _ = request.respond(
                        Response::from_string("unauthorized")
                            .with_status_code(401)
                            .with_header(prompt_header("Cache-Control", "no-store")),
                    );
                    continue;
                }
                let mut body = secrecy::zeroize::Zeroizing::new(Vec::new());
                if request
                    .as_reader()
                    .take((MAX_SECRET_BYTES + 1024) as u64)
                    .read_to_end(&mut body)
                    .is_err()
                {
                    let _ =
                        request.respond(Response::from_string("bad request").with_status_code(400));
                    continue;
                }
                if body.len() > MAX_SECRET_BYTES + 512 {
                    let _ = request
                        .respond(Response::from_string("payload too large").with_status_code(413));
                    continue;
                }
                let (csrf, value_bytes, action_cancel) = parse_prompt_form(&body);
                if !constant_time_eq_str(&csrf_token, &csrf) {
                    let mut value_bytes = value_bytes;
                    value_bytes.zeroize();
                    let _ = request
                        .respond(Response::from_string("invalid csrf").with_status_code(403));
                    continue;
                }
                if is_cancel || action_cancel {
                    let mut value_bytes = value_bytes;
                    value_bytes.zeroize();
                    let _ = request.respond(
                        Response::from_string("<!DOCTYPE html><meta charset=utf-8><title>Cancelled</title><p>Secret prompt cancelled. You may close this window.</p>")
                            .with_status_code(200)
                            .with_header(prompt_header("Content-Type", "text/html; charset=utf-8"))
                            .with_header(prompt_header("Cache-Control", "no-store")),
                    );
                    return Err(MusterError::Operational(
                        "secret prompt cancelled by user".into(),
                    ));
                }
                let parsed = parse_secret_bytes(
                    value_bytes,
                    "the secret value from the graphical prompt is empty",
                );
                let reply_html = if parsed.is_ok() {
                    "<!DOCTYPE html><meta charset=utf-8><title>Saved</title><p>Secret encrypted in Muster vault. You may close this window.</p>"
                } else {
                    "<!DOCTYPE html><meta charset=utf-8><title>Error</title><p>Secret value was empty or invalid.</p>"
                };
                let _ = request.respond(
                    Response::from_string(reply_html)
                        .with_status_code(if parsed.is_ok() { 200 } else { 400 })
                        .with_header(prompt_header("Content-Type", "text/html; charset=utf-8"))
                        .with_header(prompt_header("Cache-Control", "no-store")),
                );
                return parsed;
            }
            _ => {
                let _ = request.respond(
                    Response::from_string("not found")
                        .with_status_code(404)
                        .with_header(prompt_header("Cache-Control", "no-store")),
                );
            }
        }
    }

    Err(MusterError::Operational(
        "timed out waiting for graphical secret prompt".into(),
    ))
}

fn launch_prompt_browser(url: &str) -> Result<(), MusterError> {
    use std::process::{Command, Stdio};
    let program = if let Some(custom) = std::env::var_os(BROWSER_ENV).filter(|v| !v.is_empty()) {
        custom
    } else if cfg!(target_os = "macos") {
        "open".into()
    } else {
        "xdg-open".into()
    };
    Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            MusterError::Unavailable(format!("cannot open graphical prompt in browser: {e}"))
        })
}

fn prompt_header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid static header")
}

fn prompt_host_allowed(request: &tiny_http::Request, port: u16) -> bool {
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let host_ok = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .is_some_and(|h| allowed.iter().any(|a| h.value.as_str() == a));
    let origin_ok = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Origin"))
        .all(|h| {
            allowed
                .iter()
                .any(|a| h.value.as_str() == format!("http://{a}"))
        });
    host_ok && origin_ok
}

fn prompt_has_cookie(request: &tiny_http::Request, expected: &str) -> bool {
    request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Cookie"))
        .flat_map(|h| h.value.as_str().split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(k, v)| k == "muster_prompt" && constant_time_eq_str(expected, v))
}

fn constant_time_eq_str(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn parse_prompt_form(body: &[u8]) -> (String, Vec<u8>, bool) {
    if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) {
        let csrf = map
            .get("csrf")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let cancel = map.get("cancel").and_then(Value::as_bool).unwrap_or(false);
        let val = map
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        return (csrf, val, cancel);
    }
    let text = String::from_utf8_lossy(body);
    let mut csrf = String::new();
    let mut value = Vec::new();
    let mut cancel = false;
    for pair in text.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let plus_decoded = v.replace('+', " ");
        match k {
            "csrf" => {
                let mut bytes = Vec::new();
                percent_decode_into(&plus_decoded, &mut bytes);
                csrf = String::from_utf8_lossy(&bytes).into_owned();
            }
            "value" => {
                value.clear();
                percent_decode_into(&plus_decoded, &mut value);
            }
            "cancel" => {
                cancel = true;
            }
            _ => {}
        }
    }
    (csrf, value, cancel)
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn render_prompt_html(
    reference: &str,
    rotate: bool,
    reason: Option<&str>,
    allow: &[String],
    csrf: &str,
) -> String {
    let heading = if rotate {
        "Replace secret in Muster vault"
    } else {
        "Store secret in Muster vault"
    };
    let allow_text = if allow.is_empty() {
        "none (human only)".to_owned()
    } else {
        allow.join(", ")
    };
    let reason_block = reason
        .map(|r| format!("<p><strong>Reason:</strong> {}</p>", html_escape(r)))
        .unwrap_or_default();
    format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\"><head><meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{heading}</title>\n\
         <style>\n\
         body{{font-family:system-ui,sans-serif;max-width:30rem;margin:3rem auto;padding:1.5rem;line-height:1.45;background:#111827;color:#f9fafb}}\n\
         main{{background:#1f2937;border:1px solid #374151;border-radius:0.5rem;padding:1.5rem}}\n\
         h1{{font-size:1.2rem;margin-top:0}}\n\
         code{{background:#111827;padding:0.15rem 0.35rem;border-radius:0.25rem}}\n\
         label{{display:block;margin:1rem 0 0.35rem;font-weight:600}}\n\
         input[type=password]{{width:100%;box-sizing:border-box;padding:0.55rem;border-radius:0.35rem;border:1px solid #4b5563;background:#111827;color:#f9fafb}}\n\
         .actions{{display:flex;gap:0.75rem;margin-top:1.25rem}}\n\
         button{{padding:0.5rem 1rem;border-radius:0.35rem;border:1px solid #4b5563;cursor:pointer;font-weight:600}}\n\
         button.primary{{background:#2563eb;border-color:#2563eb;color:#fff}}\n\
         button.secondary{{background:transparent;color:#d1d5db}}\n\
         </style></head><body><main>\n\
         <h1>{heading}</h1>\n\
         <p><strong>Reference:</strong> <code>{}</code></p>\n\
         <p><strong>Allowed agents:</strong> <code>{}</code></p>\n\
         {reason_block}\n\
         <p>Paste the secret below. The value is encrypted directly in the local Muster vault and is never shown in chat or command output.</p>\n\
         <form method=\"post\" action=\"/submit\" autocomplete=\"off\">\n\
           <input type=\"hidden\" name=\"csrf\" value=\"{}\">\n\
           <label for=\"secret-value\">Secret value</label>\n\
           <input id=\"secret-value\" name=\"value\" type=\"password\" autocomplete=\"new-password\" required autofocus>\n\
           <div class=\"actions\">\n\
             <button type=\"submit\" class=\"primary\">Save secret</button>\n\
           </div>\n\
         </form>\n\
         <form method=\"post\" action=\"/cancel\">\n\
           <input type=\"hidden\" name=\"csrf\" value=\"{}\">\n\
           <div class=\"actions\">\n\
             <button type=\"submit\" name=\"cancel\" value=\"1\" class=\"secondary\">Cancel</button>\n\
           </div>\n\
         </form>\n\
         </main></body></html>\n",
        html_escape(reference),
        html_escape(&allow_text),
        html_escape(csrf),
        html_escape(csrf),
    )
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

const REQUEST_ID: ParamSpec = ParamSpec {
    name: "id",
    about: "Secret request id",
    kind: ParamKind::String,
    required: true,
    positional: true,
};
pub const REQUESTS: CommandSpec = CommandSpec {
    name: "secret.requests",
    about: "List pending secret approvals",
    params: &[],
    mutating: false,
    handler: requests,
};
pub const LEASES: CommandSpec = CommandSpec {
    name: "secret.leases",
    about: "List active secret leases",
    params: &[],
    mutating: false,
    handler: leases,
};
pub const APPROVE: CommandSpec = CommandSpec {
    name: "secret.approve",
    about: "Approve a secret request from a human terminal",
    params: &[
        REQUEST_ID,
        ParamSpec {
            name: "ttl",
            about: "Approval duration, at most 24h (default approval_ttl or 15m)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: approve_request,
};
pub const DENY: CommandSpec = CommandSpec {
    name: "secret.deny",
    about: "Deny a pending secret request",
    params: &[REQUEST_ID],
    mutating: true,
    handler: deny_request,
};
pub const REVOKE: CommandSpec = CommandSpec {
    name: "secret.revoke",
    about: "Revoke a secret lease or all leases",
    params: &[
        ParamSpec {
            name: "id",
            about: "Lease id",
            kind: ParamKind::String,
            required: false,
            positional: true,
        },
        ParamSpec {
            name: "all",
            about: "Revoke all active leases",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: revoke_lease,
};

fn requests(ctx: &mut Context, _: &Params) -> Result<Value, MusterError> {
    Ok(json!(
        crate::secret_leases::Store::open(&ctx.state_dir)?
            .data
            .requests
    ))
}
fn leases(ctx: &mut Context, _: &Params) -> Result<Value, MusterError> {
    Ok(json!(
        crate::secret_leases::Store::open(&ctx.state_dir)?
            .data
            .leases
    ))
}
fn approve_request(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    if !ctx.is_tty && !ctx.web_approval {
        return Err(MusterError::Security("secret approval requires a human terminal or a confirmed web session; --yes alone is insufficient".into()));
    }
    ctx.confirm("Approve this agent's secret use?")?;
    if ctx.dry_run {
        return Ok(json!({"dry_run": true}));
    }
    let actor = machine::load_name(&ctx.state_dir).unwrap_or_else(|| "human".into());
    let mut store = crate::secret_leases::Store::open(&ctx.state_dir)?;
    Ok(json!(store.approve(
        &ctx.state_dir,
        required(params, "id")?,
        params.get_string("ttl"),
        &actor,
        &ctx.config_dir
    )?))
}
fn deny_request(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let id = required(params, "id")?;
    ctx.confirm("Deny this secret request?")?;
    let mut store = crate::secret_leases::Store::open(&ctx.state_dir)?;
    let request = store
        .data
        .requests
        .iter()
        .find(|r| r.id == id)
        .ok_or_else(|| MusterError::Usage("unknown secret request".into()))?;
    if !ctx.dry_run {
        let actor = machine::load_name(&ctx.state_dir).unwrap_or_else(|| "human".into());
        crate::audit::record_with_actor(&ctx.state_dir, "secret.deny", &request.reference, &actor)?;
        store.data.requests.retain(|r| r.id != id);
        store.save()?;
    }
    Ok(json!({"denied": id}))
}
fn revoke_lease(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let id = params.get_string("id");
    let all = params.get_bool("all");
    if all == id.is_some() {
        return Err(MusterError::Usage("provide a lease id or --all".into()));
    }
    ctx.confirm("Revoke secret approval?")?;
    let mut store = crate::secret_leases::Store::open(&ctx.state_dir)?;
    let selected: Vec<_> = store
        .data
        .leases
        .iter()
        .filter(|l| all || Some(l.id.as_str()) == id)
        .cloned()
        .collect();
    if selected.is_empty() && !all {
        return Err(MusterError::Usage("unknown secret lease".into()));
    }
    if !ctx.dry_run {
        for lease in &selected {
            let actor = machine::load_name(&ctx.state_dir).unwrap_or_else(|| "human".into());
            crate::audit::record_with_actor(
                &ctx.state_dir,
                "secret.revoke",
                &lease.reference,
                &actor,
            )?;
        }
        store
            .data
            .leases
            .retain(|l| !all && Some(l.id.as_str()) != id);
        store.save()?;
    }
    Ok(json!({"revoked": selected.len()}))
}
