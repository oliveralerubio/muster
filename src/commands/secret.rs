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

pub const ALLOW: CommandSpec = CommandSpec {
    name: "secret.allow",
    about: "Edit which agents may receive a secret (its allow policy)",
    params: &[
        ParamSpec {
            name: "reference",
            about: "Secret reference (e.g. native://app/api)",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "entry",
            about: "Agent allowed to receive the secret (repeatable): <agent> everywhere or \
                    <agent>@<project> only in that project. The entries replace the current list",
            kind: ParamKind::List,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "deny-all",
            about: "Set an explicit empty allow list: no agent (and no reveal) without a named, \
                    allowed agent",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "remove",
            about: "Remove the access policy entry (approval and approval_ttl go with it)",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: allow,
};

/// The three distinguishable states of a secret's access policy. They are not
/// flattened: "no policy" (deny by default, human CLI can still reveal) is a
/// different state from an explicit `allow = []` (deny everyone) and from a
/// list of allowed agents.
fn policy_state(policy: Option<&SecretPolicy>) -> &'static str {
    match policy {
        None => "none",
        Some(p) => match p.allow.as_ref() {
            None => "none",
            Some(entries) if entries.is_empty() => "deny_all",
            Some(_) => "allows",
        },
    }
}

fn describe_policy(policy: Option<&SecretPolicy>) -> String {
    match policy_state(policy) {
        "none" => "no access policy".to_owned(),
        "deny_all" => "allow = [] (no agent may receive it)".to_owned(),
        _ => {
            let entries = policy
                .and_then(|p| p.allow.as_deref())
                .map(|entries| entries.join(", "))
                .unwrap_or_default();
            format!("allow = [{entries}]")
        }
    }
}

fn allow(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let raw = required(params, "reference")?;
    let reference = SecretRef::parse(raw)?;
    let entries = params.get_list("entry");
    let deny_all = params.get_bool("deny-all");
    let remove = params.get_bool("remove");
    // Exactly one mode: a list of entries (possibly several), an explicit
    // deny-all, or removal. Several entries are still one mode.
    let modes = usize::from(!entries.is_empty()) + usize::from(deny_all) + usize::from(remove);
    if modes != 1 {
        return Err(MusterError::Usage(
            "choose exactly one: --entry <agent>[...] to allow agents, --deny-all for an \
             explicit empty allow list, or --remove to drop the policy"
                .into(),
        ));
    }
    for entry in &entries {
        validate_allow_entry(entry).map_err(MusterError::Usage)?;
    }

    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let reference = reference.as_str().to_owned();
    let current = manifest.secrets.get(&reference).cloned();
    if remove && current.is_none() {
        return Err(MusterError::Usage(format!(
            "no access policy declared for {reference}; nothing to remove"
        )));
    }

    // The exact change, taught before anything is written.
    let kept = current
        .as_ref()
        .map(|p| {
            let approval = if p.approval == SecretApproval::Each {
                format!(
                    "approval=each, ttl={}",
                    p.approval_ttl.as_deref().unwrap_or("15m")
                )
            } else {
                "approval=none".to_owned()
            };
            format!(" ({approval} preserved)")
        })
        .unwrap_or_default();
    let question = if remove {
        format!(
            "Remove the access policy for {reference} (was: {})?",
            describe_policy(current.as_ref())
        )
    } else if deny_all {
        format!("Set an explicit empty allow list for {reference} (no agent may receive it){kept}?")
    } else {
        format!("Allow {} to receive {reference}{kept}?", entries.join(", "))
    };
    ctx.confirm(&question)?;

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "reference": reference,
            "policy_state": if remove { "none" } else if deny_all { "deny_all" } else { "allows" },
            "allow": entries,
        }));
    }

    // Mutate only the allow field; approval and approval_ttl of an existing
    // policy are preserved as-is.
    if remove {
        manifest.secrets.remove(&reference);
    } else {
        let policy = manifest.secrets.entry(reference.clone()).or_default();
        policy.allow = Some(entries.clone());
    }

    // Absent (None) and explicit empty (Some([])) are different states: the
    // comparison must see an absent → deny-all transition as a change.
    let before = current.as_ref().and_then(|p| p.allow.clone());
    let after = manifest.secrets.get(&reference).cloned();
    let changed = before != after.as_ref().and_then(|p| p.allow.clone());
    let original_text =
        std::fs::read_to_string(ctx.config_dir.join(crate::manifest::FILE_NAME)).ok();
    let result_policy = if changed {
        crate::audit::record(&ctx.state_dir, "secret.allow", &reference)?;
        manifest.save(&ctx.config_dir)?;
        // If this machine cannot sign the change, nothing lands: the working
        // tree goes back to byte-identical so no unsigned policy is left
        // behind for the next command to trip over.
        match crate::trust::commit(ctx, &format!("feat(secret): access policy for {reference}")) {
            Ok(committed) => committed,
            Err(e) => {
                if let Some(text) = original_text {
                    let _ = std::fs::write(ctx.config_dir.join(crate::manifest::FILE_NAME), text);
                }
                return Err(e);
            }
        }
    } else {
        false
    };

    // Applied now, or a proposal waiting for the remaining admin co-signatures?
    // `trust::commit` never advances the local pin to a state the fleet would
    // refuse; `pending_quorum` reports exactly that case.
    let quorum = match crate::fleet_state::FleetState::load(&ctx.config_dir)? {
        Some(state) => crate::fleet_state::pending_quorum(&ctx.state_dir, &state, &manifest)?
            .map(|(received, required)| {
                json!({ "received": received, "required": required, "pending": true })
            }),
        None => None,
    };
    let policy = after.as_ref();
    let mut result = json!({
        "reference": reference,
        "policy_state": policy_state(policy),
        "allow": policy.and_then(|p| p.allow.clone()).unwrap_or_default(),
        "approval": match policy.map(|p| p.approval) {
            Some(SecretApproval::Each) => "each",
            _ => "none",
        },
        "approval_ttl": policy.and_then(|p| p.approval_ttl.clone()),
        "changed": changed,
        "committed": result_policy,
        "quorum": quorum,
    });
    if !changed {
        result["changed"] = Value::Bool(false);
        ctx.diag(Diagnostic::new(
            Severity::Info,
            "secret_allow_unchanged",
            format!(
                "the access policy for {reference} already was {}",
                describe_policy(policy)
            ),
        ));
    } else if result["quorum"]["pending"] == Value::Bool(true) {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "governance_pending",
            format!(
                "the policy change is a proposal pending co-signatures ({} of {} admin \
                 signatures); other machines keep the last accepted state until the remaining \
                 admin(s) run `muster trust cosign`",
                result["quorum"]["received"], result["quorum"]["required"]
            ),
        ));
    }
    if ctx.format == crate::output::Format::Text {
        return Ok(Value::String(format_allow_text(&result)));
    }
    Ok(result)
}

fn format_allow_text(result: &Value) -> String {
    let reference = result["reference"].as_str().unwrap_or_default();
    let state = result["policy_state"].as_str().unwrap_or("none");
    let described = match state {
        "deny_all" => "allow = [] (no agent may receive it)".to_owned(),
        "allows" => format!(
            "allow = [{}]",
            result["allow"]
                .as_array()
                .map(|entries| entries
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ),
        _ => "no access policy".to_owned(),
    };
    let approval = match result["approval"].as_str() {
        Some("each") => format!(
            " (approval=each, ttl={})",
            result["approval_ttl"].as_str().unwrap_or("15m")
        ),
        _ => String::new(),
    };
    let tail = if result["quorum"]["pending"] == Value::Bool(true) {
        format!(
            "\nProposal pending co-signatures: {} of {} admin signatures; other machines keep \
             the last accepted state until an admin runs `muster trust cosign`.",
            result["quorum"]["received"], result["quorum"]["required"]
        )
    } else if result["committed"] == Value::Bool(true) {
        "\nApplied and committed.".to_owned()
    } else {
        String::new()
    };
    format!("Access policy for {reference}: {described}{approval}.{tail}")
}

fn add(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
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
            let policy = manifest.as_ref().and_then(|m| m.secrets.get(&reference));
            let allow = policy.map(|p| p.allow_list().to_vec()).unwrap_or_default();
            secrets.push(json!({
                "group": group,
                "name": meta.name,
                "reference": reference,
                "allow": allow,
                // The three distinguishable policy states (not flattened):
                // "none" (no policy declared), "deny_all" (explicit `allow =
                // []`), "allows" (a non-empty allow list).
                "policy_state": policy_state(policy),
                "approval": match policy.map(|p| p.approval) {
                    Some(SecretApproval::Each) => "each",
                    _ => "none",
                },
                "approval_ttl": policy.and_then(|p| p.approval_ttl.clone()),
                "created_at": meta.created_at,
                "updated_at": meta.updated_at,
                "created_by": meta.created_by,
                "updated_by": meta.updated_by,
                "readers": readers(&manifest, &meta.recipients),
            }));
        }
    }
    if ctx.format == crate::output::Format::Text {
        return Ok(Value::String(format_list_text(&secrets)));
    }
    Ok(Value::Array(secrets))
}

fn format_list_text(secrets: &[Value]) -> String {
    if secrets.is_empty() {
        return "No secrets in the native vault.".to_owned();
    }
    let mut text = String::new();
    for s in secrets {
        let access = match s["policy_state"].as_str() {
            Some("deny_all") => "no agent may receive it (allow = [])".to_owned(),
            Some("allows") => format!(
                "allowed: {}",
                s["allow"]
                    .as_array()
                    .map(|entries| entries
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default()
            ),
            _ => "no access policy (agents never receive it)".to_owned(),
        };
        let approval = if s["approval"].as_str() == Some("each") {
            format!(
                " [per-use approval, ttl {}]",
                s["approval_ttl"].as_str().unwrap_or("15m")
            )
        } else {
            String::new()
        };
        text.push_str(&format!(
            "{}  {}  (updated {} by {})\n  {access}{approval}\n",
            s["reference"].as_str().unwrap_or_default(),
            s["readers"]
                .as_array()
                .map(|readers| readers
                    .iter()
                    .map(|r| r["machine"].as_str().unwrap_or("unknown"))
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default(),
            s["updated_at"].as_str().unwrap_or("unknown time"),
            s["updated_by"].as_str().unwrap_or("unknown"),
        ));
    }
    text
}

fn rotate(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
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

fn get(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let reference = SecretRef::parse(required(params, "reference")?)?;
    let reveal = params.get_bool("reveal");
    let actor = params
        .get_string("agent")
        .map(str::to_owned)
        .or_else(|| ctx.audit_actor.clone());
    // A secret with an allow list or per-use approval is never revealed to an
    // unnamed caller: `--yes` alone does not satisfy a policy.
    if reveal && actor.is_none() {
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
    // Check current policy before resolving any value, including MCP calls.
    let _lease_guard = if let Some(agent) = actor.as_deref().filter(|_| reveal) {
        crate::secret_leases::authorize(ctx, reference.as_str(), agent)?
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
    if let Some(value) = params.take_secret() {
        return Ok(value);
    }
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
    let requests = crate::secret_leases::Store::open(&ctx.state_dir)?
        .data
        .requests;
    if ctx.format == crate::output::Format::Text {
        let serialized = serde_json::to_value(&requests).unwrap_or(Value::Array(Vec::new()));
        let empty = Vec::new();
        let items = serialized.as_array().unwrap_or(&empty);
        return Ok(Value::String(format_requests_text(items)));
    }
    Ok(json!(requests))
}
fn leases(ctx: &mut Context, _: &Params) -> Result<Value, MusterError> {
    let leases = crate::secret_leases::Store::open(&ctx.state_dir)?
        .data
        .leases;
    if ctx.format == crate::output::Format::Text {
        let serialized = serde_json::to_value(&leases).unwrap_or(Value::Array(Vec::new()));
        let empty = Vec::new();
        let items = serialized.as_array().unwrap_or(&empty);
        return Ok(Value::String(format_leases_text(items)));
    }
    Ok(json!(leases))
}

fn format_requests_text(requests: &[Value]) -> String {
    if requests.is_empty() {
        return "No pending secret requests.".to_owned();
    }
    let mut text = String::new();
    for r in requests {
        text.push_str(&format!(
            "{}  agent {}  (from {})\n  approve: muster secret approve {}\n",
            r["reference"].as_str().unwrap_or_default(),
            r["agent"].as_str().unwrap_or_default(),
            r["source"].as_str().unwrap_or("unknown"),
            r["id"].as_str().unwrap_or_default(),
        ));
    }
    text
}

fn format_leases_text(leases: &[Value]) -> String {
    if leases.is_empty() {
        return "No active secret approvals.".to_owned();
    }
    let mut text = String::new();
    for l in leases {
        text.push_str(&format!(
            "{}  agent {}  expires {}  (granted by {}; revoke: muster secret revoke {})\n",
            l["reference"].as_str().unwrap_or_default(),
            l["agent"].as_str().unwrap_or_default(),
            l["expires_at"]
                .as_i64()
                .map(|t| {
                    time::OffsetDateTime::from_unix_timestamp(t)
                        .map(|t| {
                            t.format(&time::format_description::well_known::Rfc3339)
                                .unwrap_or_default()
                        })
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
            l["granted_by"].as_str().unwrap_or("unknown"),
            l["id"].as_str().unwrap_or_default(),
        ));
    }
    text
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
