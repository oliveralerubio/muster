//! `muster proposal`: changes proposed by agents (or people), stored under
//! `<config>/proposals/<id>/` for review. Creating one returns the engine plan
//! computed from the proposed config; nothing is applied. `proposal accept`
//! (confirmation required) copies it into the config repo, after which the
//! usual `muster plan` / `muster sync` approvals still apply.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::context::Context;
use crate::engine::{self, Options, Target};
use crate::errors::MusterError;
use crate::git;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

pub const CREATE: CommandSpec = CommandSpec {
    name: "proposal.create",
    about: "Record a proposed configuration change and return its plan",
    params: &[
        ParamSpec {
            name: "path",
            about: "Relative path in the config repository",
            kind: ParamKind::String,
            required: true,
            positional: false,
        },
        ParamSpec {
            name: "contents",
            about: "Proposed file contents",
            kind: ParamKind::String,
            required: true,
            positional: false,
        },
        ParamSpec {
            name: "message",
            about: "Explanation of the proposal",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: create,
};

pub const ACCEPT: CommandSpec = CommandSpec {
    name: "proposal.accept",
    about: "Apply a proposed configuration change to the config repository",
    params: &[ParamSpec {
        name: "id",
        about: "Proposal identifier",
        kind: ParamKind::String,
        required: true,
        positional: true,
    }],
    mutating: true,
    handler: accept,
};

pub const LIST: CommandSpec = CommandSpec {
    name: "proposal.list",
    about: "List pending configuration proposals",
    params: &[],
    mutating: false,
    handler: list,
};

/// Validate that a path can be proposed for review.
pub fn validate_proposal_path(path: &str) -> Result<PathBuf, MusterError> {
    if path.is_empty() {
        return Err(MusterError::Usage("proposal path cannot be empty".into()));
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(MusterError::Usage(
            "proposal path must be relative to the config repository".into(),
        ));
    }
    for comp in p.components() {
        match comp {
            std::path::Component::Normal(s) => {
                let name = s.to_string_lossy();
                if name.starts_with(".git") || name == "vault" || name == "proposals" {
                    return Err(MusterError::Usage(format!(
                        "path '{path}' modifies protected area '{name}'"
                    )));
                }
            }
            _ => {
                return Err(MusterError::Usage(format!(
                    "path '{path}' contains forbidden component"
                )))
            }
        }
    }
    Ok(p.to_path_buf())
}

/// Compute a plan as if `files` were written into `config_dir`, without
/// touching the real `config_dir`.
pub fn plan_with_overrides(
    ctx: &Context,
    overrides: &[(&Path, &str)],
) -> Result<Value, MusterError> {
    let temp = Scratch::new(&ctx.state_dir)?;
    copy_dir_recursive(&ctx.config_dir, temp.path())?;

    for (rel_path, contents) in overrides {
        let target = temp.path().join(rel_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| MusterError::Operational(format!("cannot create parent dir: {e}")))?;
        }
        fs::write(&target, contents).map_err(|e| {
            MusterError::Operational(format!("cannot write proposal override: {e}"))
        })?;
    }

    let mut temp_ctx = Context::new(crate::context::Globals {
        config: Some(temp.path().to_path_buf()),
        format: Some(ctx.format),
        yes: false,
        dry_run: true,
        quiet: true,
        verbose: false,
        audit_actor: None,
    })?;
    temp_ctx.state_dir = ctx.state_dir.clone();

    let options = Options {
        target: Target::Global,
        agent: None,
    };
    let plan = engine::plan(&mut temp_ctx, &options)?;
    Ok(plan.to_json(true, false, plan.base_generation()))
}

/// Throwaway directory under the local state dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(state_dir: &Path) -> Result<Self, MusterError> {
        let now = time::OffsetDateTime::now_utc();
        let dir = state_dir.join("tmp").join(format!(
            "proposal-{}-{}-{}",
            std::process::id(),
            now.unix_timestamp(),
            now.nanosecond()
        ));
        fs::create_dir_all(&dir)
            .map_err(|e| MusterError::Operational(format!("{}: {e}", dir.display())))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), MusterError> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)
        .map_err(|e| MusterError::Operational(format!("cannot create dir: {e}")))?;
    for entry in fs::read_dir(src).map_err(|e| MusterError::Operational(e.to_string()))? {
        let entry = entry.map_err(|e| MusterError::Operational(e.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| MusterError::Operational(e.to_string()))?;
        let file_name = entry.file_name();
        if file_name == "proposals" || file_name == ".git" {
            continue;
        }
        let src_child = entry.path();
        let dst_child = dst.join(&file_name);
        if file_type.is_dir() {
            copy_dir_recursive(&src_child, &dst_child)?;
        } else if file_type.is_file() {
            fs::copy(&src_child, &dst_child)
                .map_err(|e| MusterError::Operational(format!("{}: {e}", src_child.display())))?;
        }
    }
    Ok(())
}

fn create(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let rel_path = params
        .get_string("path")
        .ok_or_else(|| MusterError::Usage("path is required".into()))?;
    let contents = params
        .get_string("contents")
        .ok_or_else(|| MusterError::Usage("contents is required".into()))?;
    let message = params
        .get_string("message")
        .unwrap_or("proposed configuration change");

    let clean_path = validate_proposal_path(rel_path)?;

    // If proposing muster.toml, validate that it parses.
    if clean_path == Path::new("muster.toml") {
        Manifest::parse(contents, "muster.toml")?;
    }

    let plan = plan_with_overrides(ctx, &[(&clean_path, contents)])?;
    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "path": clean_path.to_string_lossy(),
            "message": message,
            "plan": plan,
        }));
    }

    let now = time::OffsetDateTime::now_utc();
    let id = format!("{}-{}", now.unix_timestamp(), now.nanosecond() % 1_000_000);

    let proposal_dir = ctx.config_dir.join("proposals").join(&id);
    let files_dir = proposal_dir.join("files");
    let file_dest = files_dir.join(&clean_path);
    if let Some(parent) = file_dest.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            MusterError::Operational(format!("cannot create proposal directory: {e}"))
        })?;
    }
    fs::write(&file_dest, contents)
        .map_err(|e| MusterError::Operational(format!("cannot write proposal file: {e}")))?;

    let meta = json!({
        "id": id,
        "path": clean_path.to_string_lossy(),
        "message": message,
        "created_at": now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    });
    fs::write(
        proposal_dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| MusterError::Operational(format!("cannot write proposal meta: {e}")))?;

    Ok(json!({
        "id": id,
        "path": clean_path.to_string_lossy(),
        "message": message,
        "plan": plan,
    }))
}

fn accept(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let id = params
        .get_string("id")
        .ok_or_else(|| MusterError::Usage("id is required".into()))?;

    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(MusterError::Usage(format!("invalid proposal id '{id}'")));
    }
    let proposal_dir = ctx.config_dir.join("proposals").join(id);
    if !proposal_dir.is_dir() {
        return Err(MusterError::Operational(format!(
            "proposal '{id}' does not exist"
        )));
    }

    ctx.confirm(&format!(
        "Accept proposal '{id}' and update the config repository?"
    ))?;
    if ctx.dry_run {
        return Ok(json!({ "dry_run": true, "accepted": id }));
    }

    let files_dir = proposal_dir.join("files");
    let mut copied = Vec::new();
    if files_dir.exists() {
        for entry in walk_files(&files_dir)? {
            let rel = entry
                .strip_prefix(&files_dir)
                .map_err(|e| MusterError::Operational(e.to_string()))?;
            let rel = validate_proposal_path(&rel.to_string_lossy())?;
            if rel == Path::new(crate::manifest::FILE_NAME) {
                let text = fs::read_to_string(&entry)
                    .map_err(|e| MusterError::Operational(format!("{}: {e}", entry.display())))?;
                Manifest::parse(&text, crate::manifest::FILE_NAME)?;
            }
            let dest = ctx.config_dir.join(&rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    MusterError::Operational(format!("cannot create directory: {e}"))
                })?;
            }
            fs::copy(&entry, &dest)
                .map_err(|e| MusterError::Operational(format!("cannot copy proposal file: {e}")))?;
            copied.push(rel.to_string_lossy().to_string());
        }
    }

    fs::remove_dir_all(&proposal_dir)
        .map_err(|e| MusterError::Operational(format!("{}: {e}", proposal_dir.display())))?;

    git::commit_if_configured(
        &ctx.config_dir,
        &format!("feat(proposal): accept proposal {id}"),
    )?;

    Ok(json!({
        "accepted": id,
        "files": copied,
    }))
}

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let proposals_dir = ctx.config_dir.join("proposals");
    if !proposals_dir.exists() {
        return Ok(Value::Array(Vec::new()));
    }
    let mut proposals = Vec::new();
    for entry in
        fs::read_dir(&proposals_dir).map_err(|e| MusterError::Operational(e.to_string()))?
    {
        let entry = entry.map_err(|e| MusterError::Operational(e.to_string()))?;
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let meta_path = entry.path().join("meta.json");
            if let Ok(text) = fs::read_to_string(&meta_path) {
                if let Ok(val) = serde_json::from_str::<Value>(&text) {
                    proposals.push(val);
                }
            }
        }
    }
    Ok(Value::Array(proposals))
}

fn walk_files(dir: &Path) -> Result<Vec<PathBuf>, MusterError> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(dir).map_err(|e| MusterError::Operational(e.to_string()))? {
        let entry = entry.map_err(|e| MusterError::Operational(e.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}
