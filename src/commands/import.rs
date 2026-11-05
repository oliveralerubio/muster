//! `muster import bws|chezmoi|cuartel`: migrate from the tools Muster
//! replaces. Every importer honours `--dry-run` (report only, no writes) and
//! otherwise commits its result to the config repo. Existing secrets, files
//! and manifest entries are never overwritten: differences are reported as
//! `import_conflict` warnings and the current value is kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapters::config::{AgentsConfig, DEFAULT_INSTRUCTIONS_SOURCE};
use crate::context::Context;
use crate::engine::dotfiles;
use crate::errors::MusterError;
use crate::identity::Identity;
use crate::importers::{chezmoi, cuartel};
use crate::machine;
use crate::manifest::{self, ManagedDotfile, Manifest};
use crate::output::{Diagnostic, Severity};
use crate::providers::cli::bws_list;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::secrets;
use crate::vault::Vault;

/// Config repo directory keeping chezmoi templates for manual conversion.
const TEMPLATES_DIR: &str = "imports/chezmoi";

pub const BWS: CommandSpec = CommandSpec {
    name: "import.bws",
    about: "Import a Bitwarden Secrets Manager project into the native vault (values never shown)",
    params: &[
        ParamSpec {
            name: "project",
            about: "Bitwarden project ID [default: every project the token can read]",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "group",
            about: "Vault group to store the secrets in [default: bws]",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: import_bws,
};

pub const CHEZMOI: CommandSpec = CommandSpec {
    name: "import.chezmoi",
    about: "Import a chezmoi source directory as managed dotfiles",
    params: &[SOURCE_PARAM],
    mutating: true,
    handler: import_chezmoi,
};

pub const CUARTEL: CommandSpec = CommandSpec {
    name: "import.cuartel",
    about: "Import a Cuartel configuration (instructions, MCP servers, secret references)",
    params: &[SOURCE_PARAM],
    mutating: true,
    handler: import_cuartel,
};

const SOURCE_PARAM: ParamSpec = ParamSpec {
    name: "source",
    about: "Directory to import from",
    kind: ParamKind::String,
    required: true,
    positional: true,
};

fn source_dir(params: &Params) -> Result<PathBuf, MusterError> {
    params
        .get_string("source")
        .map(PathBuf::from)
        .ok_or_else(|| MusterError::Usage("a source directory is required".into()))
}

fn conflict(ctx: &mut Context, message: String, path: String) {
    ctx.diag(Diagnostic::new(Severity::Warning, "import_conflict", message).with_path(path));
}

// ---------------------------------------------------------------------------
// bws
// ---------------------------------------------------------------------------

fn import_bws(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let project = params.get_string("project");
    if let Some(project) = project {
        crate::providers::cli::validate_bws_project(project)?;
    }
    let group = params.get_string("group").unwrap_or("bws").to_string();
    if !secrets::is_name(&group) {
        return Err(MusterError::Usage(format!("invalid vault group `{group}`")));
    }
    let manifest = Manifest::load(&ctx.config_dir)?;
    let vault = Vault::open(&ctx.config_dir)?;
    let existing: Vec<String> = if vault.groups()?.contains(&group) {
        vault.list(&group)?.into_iter().map(|s| s.name).collect()
    } else {
        Vec::new()
    };

    let mut accepted = Vec::new();
    let mut skipped = Vec::new();
    for (name, value) in bws_list(project)? {
        let reason = if !secrets::is_name(&name) {
            Some("not a valid secret name (letters, digits, `_`, `-`, `.`)")
        } else if existing.contains(&name) || accepted.iter().any(|(n, _)| *n == name) {
            Some("already in the vault; use `muster secret rotate` to replace it")
        } else {
            None
        };
        match reason {
            Some(reason) => {
                ctx.diag(
                    Diagnostic::new(
                        Severity::Warning,
                        "secret_skipped",
                        format!("{group}/{name}: {reason}"),
                    )
                    .with_path(format!("native://{group}/{name}")),
                );
                skipped.push(json!({ "name": name, "reason": reason }));
            }
            None => accepted.push((name, value)),
        }
    }
    let imported: Vec<Value> = accepted
        .iter()
        .map(|(name, _)| json!({ "name": name, "reference": format!("native://{group}/{name}") }))
        .collect();
    let result = |dry_run: bool| {
        json!({
            "source": "bws",
            "project": project,
            "group": group,
            "dry_run": dry_run,
            "imported": imported,
            "skipped": skipped,
        })
    };
    if ctx.dry_run || accepted.is_empty() {
        return Ok(result(ctx.dry_run));
    }

    // New group: this machine plus the recovery key; an existing group keeps
    // its readers.
    let identity = Identity::load(&ctx.state_dir)?;
    let mut readers = vec![identity.recipient()];
    if let Some(recovery) = &manifest.recovery {
        readers.push(recovery.recipient.clone());
    }
    let mut vault = vault.with_actor(machine::load_name(&ctx.state_dir));
    for (name, value) in accepted {
        vault.add(&group, &name, value, &readers, &identity)?;
        ctx.detail(&format!("imported native://{group}/{name}"));
    }
    crate::trust::commit(
        ctx,
        &format!(
            "feat(import): import {} secret(s) from Bitwarden into {group}",
            imported.len()
        ),
    )?;
    Ok(result(false))
}

// ---------------------------------------------------------------------------
// chezmoi
// ---------------------------------------------------------------------------

fn import_chezmoi(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let scan = chezmoi::scan(&source_dir(params)?)?;
    let mut manifest = Manifest::load(&ctx.config_dir)?;

    let mut files = Vec::new();
    let mut writes: Vec<(PathBuf, String)> = Vec::new();
    let mut skipped: Vec<Value> = scan
        .skipped
        .iter()
        .map(|s| json!({ "source": s.source, "reason": s.reason }))
        .collect();

    let mut detected = BTreeMap::new();
    for file in &scan.files {
        if crate::importers::ai_providers::is_credential_store(&file.target) {
            skipped.push(json!({"source": file.source, "reason": "device-bound login credentials are never imported"}));
            continue;
        }
        if crate::importers::ai_providers::NATIVE_FILES.contains(&file.target.as_str()) {
            detected.extend(crate::importers::ai_providers::detect(
                &file.target,
                &file.contents,
            )?);
            skipped.push(json!({"source": file.source, "reason": "native AI provider settings imported canonically; raw file not copied"}));
            continue;
        }
        if let Err(reason) = dotfiles::locate(ctx, &file.target) {
            skipped.push(json!({ "source": file.source, "reason": reason }));
            continue;
        }
        let spec = ManagedDotfile {
            source: format!("{}/{}", dotfiles::STORE_DIR, file.target),
            private: file.private,
            executable: file.executable,
        };
        let stored = ctx.config_dir.join(&spec.source);
        let same_contents = read(&stored)?.as_deref() == Some(file.contents.as_str());
        let action = match manifest.files.get(&file.target) {
            Some(current) if *current == spec && same_contents => "unchanged",
            Some(_) => {
                conflict(
                    ctx,
                    format!(
                        "{} is already managed with different settings or contents; kept as is",
                        file.target
                    ),
                    format!("{}: files.\"{}\"", manifest::FILE_NAME, file.target),
                );
                "conflict"
            }
            None if stored.exists() && !same_contents => {
                conflict(
                    ctx,
                    format!(
                        "{} already exists in the config repo; kept as is",
                        spec.source
                    ),
                    spec.source.clone(),
                );
                "conflict"
            }
            None => {
                writes.push((stored, file.contents.clone()));
                manifest.files.insert(file.target.clone(), spec.clone());
                "add"
            }
        };
        files.push(json!({
            "target": file.target,
            "source": file.source,
            "stored_at": spec.source,
            "private": file.private,
            "executable": file.executable,
            "action": action,
        }));
    }

    let mut templates = Vec::new();
    for template in &scan.templates {
        if crate::importers::ai_providers::is_credential_store(&template.target)
            || crate::importers::ai_providers::NATIVE_FILES.contains(&template.target.as_str())
        {
            skipped.push(json!({"source": template.source, "reason": "native credentials or templated AI providers require manual conversion; raw file not copied"}));
            continue;
        }
        let kept = format!("{TEMPLATES_DIR}/{}", template.source);
        ctx.diag(
            Diagnostic::new(
                Severity::Warning,
                "chezmoi_template_untranslated",
                format!(
                    "{} uses chezmoi template syntax Muster cannot translate; kept at {kept} \
                     for manual conversion, {} is not managed",
                    template.source, template.target
                ),
            )
            .with_path(template.source.clone()),
        );
        writes.push((ctx.config_dir.join(&kept), template.contents.clone()));
        templates
            .push(json!({ "source": template.source, "target": template.target, "kept_at": kept }));
    }
    for rule in &scan.ignored_rules {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "chezmoi_ignore_unsupported",
            format!("`.chezmoiignore` rule `{rule}` is templated or negated and was not applied"),
        ));
    }
    for s in &scan.skipped {
        ctx.diag(
            Diagnostic::new(
                Severity::Info,
                "import_skipped",
                format!("{}: {}", s.source, s.reason),
            )
            .with_path(s.source.clone()),
        );
    }

    let ai_providers = merge_ai_providers(ctx, &mut manifest, detected);
    let providers_added = ai_providers.values().any(|p| p["action"] == "add");
    let added = files.iter().filter(|f| f["action"] == "add").count();
    let result = |dry_run: bool| {
        json!({
            "source": "chezmoi",
            "providers": ai_providers,
            "dry_run": dry_run,
            "files": files,
            "templates": templates,
            "skipped": skipped,
        })
    };
    if ctx.dry_run || (writes.is_empty() && !providers_added) {
        return Ok(result(ctx.dry_run));
    }
    for (path, contents) in &writes {
        write(path, contents)?;
    }
    if added > 0 || providers_added {
        manifest.save(&ctx.config_dir)?;
    }
    crate::trust::commit(
        ctx,
        &format!("feat(import): import {added} dotfile(s) from chezmoi"),
    )?;
    ctx.progress(&format!(
        "Imported {added} dotfile(s); run `muster plan` to review them"
    ));
    Ok(result(false))
}

// ---------------------------------------------------------------------------
// Cuartel
// ---------------------------------------------------------------------------

fn import_cuartel(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let proposal = cuartel::scan(&source_dir(params)?)?;
    for d in proposal.diagnostics {
        ctx.diag(d);
    }
    let mut manifest = Manifest::load(&ctx.config_dir)?;
    let mut changed = false;

    let ai_providers = merge_ai_providers(ctx, &mut manifest, proposal.ai_providers);
    changed |= ai_providers.values().any(|p| p["action"] == "add");

    let mut agents_added = Vec::new();
    if let Some(imported) = proposal.agents {
        let current = manifest.agents.get_or_insert_with(AgentsConfig::default);
        let mut conflicts = Vec::new();
        agents_added = merge_agents(current, imported, &mut conflicts);
        for key in conflicts {
            conflict(
                ctx,
                format!("agents.{key} differs from the current manifest; kept as is"),
                format!("{}: agents.{key}", manifest::FILE_NAME),
            );
        }
        if manifest.agents.as_ref() == Some(&AgentsConfig::default()) {
            manifest.agents = None;
        }
        changed |= !agents_added.is_empty();
    }

    let mut mcp = Vec::new();
    for (name, server) in proposal.mcp {
        let action = match manifest.mcp.get(&name) {
            Some(current) if *current == server => "unchanged",
            Some(_) => {
                conflict(
                    ctx,
                    format!("MCP server `{name}` differs from the current manifest; kept as is"),
                    format!("{}: mcp.{name}", manifest::FILE_NAME),
                );
                "conflict"
            }
            None => {
                manifest.mcp.insert(name.clone(), server);
                changed = true;
                "add"
            }
        };
        mcp.push(json!({ "name": name, "action": action }));
    }

    let mut instructions = Value::Null;
    let mut write_instructions = None;
    if let Some(text) = proposal.instructions {
        let source = manifest
            .agents
            .as_ref()
            .and_then(|a| a.instructions.as_ref())
            .map_or(DEFAULT_INSTRUCTIONS_SOURCE, |i| i.source.as_str())
            .to_string();
        if !dotfiles::is_relative_inside(&source) {
            return Err(MusterError::InvalidConfig {
                message: format!("instructions source `{source}` must be inside the config repo"),
                path: Some(format!(
                    "{}: agents.instructions.source",
                    manifest::FILE_NAME
                )),
            });
        }
        let path = ctx.config_dir.join(&source);
        let action = match read(&path)? {
            Some(current) if current == text => "unchanged",
            Some(_) => {
                conflict(
                    ctx,
                    format!("{source} already exists with different text; kept as is"),
                    source.clone(),
                );
                "conflict"
            }
            None => {
                write_instructions = Some((path, text));
                "add"
            }
        };
        instructions = json!({ "path": source, "action": action });
    }

    let result = json!({
        "source": "cuartel",
        "providers": ai_providers,
        "dry_run": ctx.dry_run,
        "agents": agents_added,
        "mcp": mcp,
        "instructions": instructions,
    });
    if ctx.dry_run {
        return Ok(result);
    }
    if changed {
        manifest.save(&ctx.config_dir)?;
    }
    if let Some((path, text)) = &write_instructions {
        write(path, text)?;
    }
    crate::trust::commit(ctx, "feat(import): import Cuartel configuration")?;
    Ok(result)
}

/// Fill in what `current` does not declare yet. Returns the keys added and
/// pushes the keys that differ into `conflicts`.
fn merge_agents(
    current: &mut AgentsConfig,
    imported: AgentsConfig,
    conflicts: &mut Vec<String>,
) -> Vec<String> {
    let mut added = Vec::new();
    fn fill<T: PartialEq>(
        slot: &mut Option<T>,
        value: Option<T>,
        key: &str,
        added: &mut Vec<String>,
        conflicts: &mut Vec<String>,
    ) {
        match (slot.as_ref(), value) {
            (_, None) => {}
            (None, Some(v)) => {
                *slot = Some(v);
                added.push(key.to_string());
            }
            (Some(c), Some(v)) if *c != v => conflicts.push(key.to_string()),
            _ => {}
        }
    }
    fn fill_map<T: PartialEq>(
        slot: &mut BTreeMap<String, T>,
        values: BTreeMap<String, T>,
        key: &str,
        added: &mut Vec<String>,
        conflicts: &mut Vec<String>,
    ) {
        for (k, v) in values {
            match slot.get(&k) {
                None => {
                    slot.insert(k.clone(), v);
                    added.push(format!("{key}.{k}"));
                }
                Some(c) if *c != v => conflicts.push(format!("{key}.{k}")),
                _ => {}
            }
        }
    }
    fill(
        &mut current.enabled,
        imported.enabled,
        "enabled",
        &mut added,
        conflicts,
    );
    fill(
        &mut current.instructions,
        imported.instructions,
        "instructions",
        &mut added,
        conflicts,
    );
    fill(
        &mut current.permissions,
        imported.permissions,
        "permissions",
        &mut added,
        conflicts,
    );
    fill_map(
        &mut current.models,
        imported.models,
        "models",
        &mut added,
        conflicts,
    );
    fill_map(
        &mut current.ai_providers,
        imported.ai_providers,
        "providers",
        &mut added,
        conflicts,
    );
    fill_map(
        &mut current.hooks,
        imported.hooks,
        "hooks",
        &mut added,
        conflicts,
    );
    added
}

fn read(path: &Path) -> Result<Option<String>, MusterError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MusterError::Operational(format!("{}: {e}", path.display()))),
    }
}

fn write(path: &Path, contents: &str) -> Result<(), MusterError> {
    let io =
        |p: &Path, e: std::io::Error| MusterError::Operational(format!("{}: {e}", p.display()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    std::fs::write(path, contents).map_err(|e| io(path, e))
}

/// Merge proposals without overwriting existing endpoints. Only references survive detection.
fn merge_ai_providers(
    ctx: &mut Context,
    manifest: &mut Manifest,
    detected: BTreeMap<String, crate::adapters::config::AiProvider>,
) -> BTreeMap<String, Value> {
    let mut proposals = BTreeMap::new();
    for (id, provider) in detected {
        let action = match manifest.ai_providers.get(&id) {
            Some(current) if current == &provider => "unchanged",
            Some(_) => {
                conflict(
                    ctx,
                    "AI provider differs from the current manifest; kept as is".into(),
                    format!("providers.{id}"),
                );
                "conflict"
            }
            None => {
                manifest.ai_providers.insert(id.clone(), provider.clone());
                "add"
            }
        };
        let mut value = serde_json::to_value(provider).expect("AI provider JSON");
        value["action"] = json!(action);
        proposals.insert(id, value);
    }
    proposals
}
