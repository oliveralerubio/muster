//! Named AI endpoints. Manifest output contains references, never credentials.
use serde_json::{json, Value};

use crate::adapters::config::AiProvider;
use crate::context::Context;
use crate::errors::MusterError;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::secrets::{Scheme, SecretRef, SecretValue};
use crate::team;

const ID: ParamSpec = ParamSpec {
    name: "id",
    about: "AI provider id (or adapter id for login)",
    kind: ParamKind::String,
    required: true,
    positional: true,
};
const fn option(name: &'static str, about: &'static str, required: bool) -> ParamSpec {
    ParamSpec {
        name,
        about,
        kind: ParamKind::String,
        required,
        positional: false,
    }
}

pub const LIST: CommandSpec = CommandSpec {
    name: "provider.list",
    about: "List AI providers and their receiving agents (never key values)",
    params: &[],
    mutating: false,
    handler: list,
};
pub const ADD: CommandSpec = CommandSpec {
    name: "provider.add",
    about: "Add or replace an AI provider",
    params: &[
        ID,
        option(
            "kind",
            "anthropic, openai, openai-compatible, ollama or google",
            true,
        ),
        option("base-url", "AI provider endpoint", false),
        option("default-model", "Default model", false),
        option("key", "Secret reference (never a plaintext key)", false),
        option(
            "value",
            "Store this value in the native vault (prefer --value-stdin)",
            false,
        ),
        ParamSpec {
            name: "value-stdin",
            about: "Read the native vault value from stdin",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "allow",
            about: "Agent allowed to receive the native secret (repeatable)",
            kind: ParamKind::List,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: add,
};
pub const RM: CommandSpec = CommandSpec {
    name: "provider.rm",
    about: "Remove an AI provider and its default selections",
    params: &[ID],
    mutating: true,
    handler: rm,
};

fn id(params: &Params) -> Result<String, MusterError> {
    crate::machine::validate_name(
        params
            .get_string("id")
            .ok_or_else(|| MusterError::Usage("provider id required".into()))?,
    )
}
fn actor(ctx: &Context, manifest: &Manifest) -> Result<Option<team::Actor>, MusterError> {
    if manifest.members.is_empty() {
        Ok(None)
    } else {
        team::admin(manifest, &ctx.state_dir).map(Some)
    }
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
fn list(ctx: &mut Context, _: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let merged = crate::engine::layer::global_merge(&ctx.config_dir, &ctx.state_dir, &manifest);
    let access = crate::secrets::SecretAccess::new(manifest.secrets.clone(), None);
    let ai_providers: Vec<_> = merged
        .ai_providers
        .iter()
        .map(|(id, provider)| {
            let selected_agents: Vec<_> = merged
                .agents
                .ai_providers
                .iter()
                .filter(|(agent, selected)| {
                    *selected == id
                        && merged
                            .agents
                            .enabled
                            .as_ref()
                            .is_none_or(|enabled| enabled.contains(agent))
                })
                .map(|(agent, _)| agent)
                .collect();
            let agents: Vec<_> = selected_agents
                .iter()
                .filter(|agent| {
                    crate::adapters::find(agent)
                        .is_some_and(|adapter| adapter.supports_ai_provider(provider.kind))
                        && provider
                            .key
                            .as_ref()
                            .is_none_or(|key| access.allows(agent, key))
                })
                .collect();
            let mut entry = serde_json::to_value(provider).expect("provider JSON");
            entry["id"] = json!(id);
            entry["agents"] = json!(agents);
            entry["selected_agents"] = json!(selected_agents);
            entry
        })
        .collect();
    Ok(json!({ "providers": ai_providers }))
}
fn add(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let id = id(params)?;
    let provider: AiProvider = serde_json::from_value(json!({
        "kind": params.get_string("kind"),
        "base_url": params.get_string("base-url"),
        "default_model": params.get_string("default-model"),
        "key": params.get_string("key"),
    }))
    .map_err(|_| MusterError::InvalidConfig {
        message: "invalid provider kind".into(),
        path: Some(format!("providers.{id}")),
    })?;
    if let Some(key) = &provider.key {
        SecretRef::parse(key).map_err(|_| MusterError::InvalidConfig {
            message: "provider key must be a valid secret reference".into(),
            path: Some(format!("providers.{id}.key")),
        })?;
    }
    let stores_value = params.get_string("value").is_some() || params.get_bool("value-stdin");
    if stores_value
        && provider
            .key
            .as_ref()
            .and_then(|key| SecretRef::parse(key).ok())
            .is_none_or(|key| key.scheme() != Scheme::Native)
    {
        return Err(MusterError::Usage(
            "a native:// key reference is required to store a value".into(),
        ));
    }
    if params.get_string("value").is_some() && params.get_bool("value-stdin") {
        return Err(MusterError::Usage(
            "use either --value or --value-stdin".into(),
        ));
    }
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = actor(ctx, &manifest)?;
    let result = json!({ "id": id, "provider": provider });
    if ctx.dry_run {
        return Ok(result);
    }
    ctx.confirm(&format!("Set provider '{id}'?"))?;
    if stores_value {
        let reference = SecretRef::parse(provider.key.as_deref().expect("validated key"))?;
        let mut secret_params = Params::default();
        secret_params.set("path", json!(reference.segments().join("/")));
        secret_params.set("allow", json!(params.get_list("allow")));
        let value = if let Some(value) = params.get_string("value") {
            ctx.diag(crate::output::Diagnostic::new(crate::output::Severity::Warning,
                "secret_in_argv", "--value may be visible in shell history and process arguments; prefer --value-stdin"));
            SecretValue::new(value.to_owned())
        } else {
            ctx.read_secret_stdin()?
        };
        super::secret::add_with_value(ctx, &secret_params, value)?;
        // secret.add may have added an access policy; do not overwrite it.
        manifest = Manifest::load(&ctx.config_dir)?;
    }
    manifest.ai_providers.insert(id.clone(), provider);
    manifest.save(&ctx.config_dir)?;
    commit(ctx, actor.as_ref(), &format!("feat(provider): set {id}"))?;
    Ok(result)
}
fn rm(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let id = id(params)?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = actor(ctx, &manifest)?;
    if manifest.ai_providers.remove(&id).is_none() {
        return Err(MusterError::Usage(format!(
            "provider '{id}' is not declared"
        )));
    }
    if let Some(agents) = &mut manifest.agents {
        agents.ai_providers.retain(|_, selected| selected != &id);
    }
    // A profile may still declare its own endpoint with this id. Preserve
    // those selections, but never leave references to an endpoint just removed.
    if !manifest
        .profiles
        .values()
        .any(|profile| profile.ai_providers.contains_key(&id))
    {
        for profile in manifest.profiles.values_mut() {
            if let Some(agents) = &mut profile.agents {
                agents.ai_providers.retain(|_, selected| selected != &id);
            }
        }
    }
    let text = toml::to_string(&manifest)
        .map_err(|_| MusterError::Operational("cannot serialize manifest".into()))?;
    Manifest::parse(&text, crate::manifest::FILE_NAME)?;
    let result = json!({ "removed": id });
    if ctx.dry_run {
        return Ok(result);
    }
    ctx.confirm(&format!("Remove provider '{id}'?"))?;
    manifest.save(&ctx.config_dir)?;
    commit(ctx, actor.as_ref(), &format!("feat(provider): remove {id}"))?;
    Ok(result)
}

pub const LOGIN: CommandSpec = CommandSpec {
    name: "provider.login",
    about: "Run an agent's native subscription login on this machine",
    params: &[ID],
    mutating: true,
    handler: login,
};

/// Only documented native status/login commands; credential files are opaque.
pub(crate) fn login_commands(
    agent: &str,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match agent {
        "claude" => Some((&["auth", "status"], &["auth", "login"])),
        "codex" => Some((&["login", "status"], &["login"])),
        _ => None,
    }
}

pub(crate) fn logged_in(binary: &std::path::Path, args: &[&str]) -> bool {
    std::process::Command::new(binary)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn login(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let id = id(params)?;
    let manifest = Manifest::load(&ctx.config_dir)?;
    let merged = crate::engine::layer::global_merge(&ctx.config_dir, &ctx.state_dir, &manifest);
    let agent = if crate::adapters::find(&id).is_some() {
        id.as_str()
    } else {
        if !merged.ai_providers.contains_key(&id) {
            return Err(MusterError::Usage("unknown provider or agent".into()));
        }
        let agents: Vec<_> = merged
            .agents
            .ai_providers
            .iter()
            .filter(|(_, selected)| *selected == &id)
            .map(|(agent, _)| agent.as_str())
            .collect();
        if agents.len() != 1 {
            return Err(MusterError::Usage(
                "provider must select exactly one agent; pass the agent id instead".into(),
            ));
        }
        agents[0]
    };
    let (status_args, login_args) = login_commands(agent)
        .ok_or_else(|| MusterError::Usage(format!("no documented native login for '{agent}'")))?;
    let adapter = crate::adapters::find(agent).expect("validated adapter");
    if ctx.dry_run {
        return Ok(json!({"agent": agent, "dry_run": true}));
    }
    let binary = crate::machine::find_executable(adapter.binary)
        .ok_or_else(|| MusterError::Unavailable(format!("{} is not installed", adapter.binary)))?;
    if logged_in(&binary, status_args) {
        return Ok(json!({"agent": agent, "status": "already_logged_in"}));
    }
    ctx.confirm(&format!("Run {agent}'s native login on this machine?"))?;
    let mut command = std::process::Command::new(binary);
    command.args(login_args);
    if ctx.is_tty && ctx.format == crate::output::Format::Text {
        // Keep the command result on stdout; native interactive output goes
        // only to the terminal, never into a JSON response or captured log.
        command.stdout(std::process::Stdio::from(std::io::stderr()));
    } else {
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null());
    }
    let status = command
        .status()
        .map_err(|_| MusterError::Unavailable("could not start native login".into()))?;
    if !status.success() {
        return Err(MusterError::Operational(
            "native login failed; run the agent's login command in a terminal".into(),
        ));
    }
    Ok(json!({"agent": agent, "status": "logged_in"}))
}

pub const USE: CommandSpec = CommandSpec {
    name: "provider.use",
    about: "Select or clear an AI provider for one agent",
    params: &[
        ID,
        option("agent", "Agent id", true),
        ParamSpec {
            name: "clear",
            about: "Clear this agent's selection",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: use_provider,
};

fn use_provider(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let id = id(params)?;
    let agent = params
        .get_string("agent")
        .ok_or_else(|| MusterError::Usage("--agent required".into()))?;
    if crate::adapters::find(agent).is_none() {
        return Err(MusterError::Usage("unknown agent".into()));
    }
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let actor = actor(ctx, &manifest)?;
    let merged = crate::engine::layer::global_merge(&ctx.config_dir, &ctx.state_dir, &manifest);
    if !merged.ai_providers.contains_key(&id) {
        return Err(MusterError::Usage("unknown provider".into()));
    }
    let clear = params.get_bool("clear");
    let agents = manifest.agents.get_or_insert_with(Default::default);
    if clear {
        agents.ai_providers.remove(agent);
    } else {
        agents.ai_providers.insert(agent.to_owned(), id.clone());
    }
    let result = json!({"agent": agent, "provider": if clear { None } else { Some(&id) }});
    if ctx.dry_run {
        return Ok(result);
    }
    ctx.confirm(&format!("Change AI provider selection for '{agent}'?"))?;
    manifest.save(&ctx.config_dir)?;
    commit(
        ctx,
        actor.as_ref(),
        &format!("feat(provider): select for {agent}"),
    )?;
    Ok(result)
}
