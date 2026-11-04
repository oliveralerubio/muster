//! `muster skill`: the skills lifecycle.
//!
//! Own skills are authored in the config repo under `[agents.skills] source`
//! (default `skills/<name>/`); skills from external git repositories
//! (`[[agents.skills.remote]]`) are vendored there too, so `agents apply` and
//! fleet sync treat both alike. `skills.lock` pins each skill's origin,
//! upstream commit and content hash; `<state>/skills-status.json` remembers
//! this machine's last upstream check.
//!
//! Upstream changes are never applied on their own: `check` reports them and
//! `update` applies them on request. A skill removed upstream is kept until
//! someone runs `rm` (delete) or `rm --detach` (keep as an own skill). In team
//! mode only admins add, update or remove skills; members list and check.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table};

use crate::context::Context;
use crate::errors::MusterError;
use crate::manifest::{self, Manifest, SkillRemote};
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::skills::lock::{self, LockEntry, SkillsLock};
use crate::skills::upstream::{
    classify, relative, CachedStatus, Checkout, GitFetcher, SkillStatus, StatusCache, Upstream,
    UpstreamFetcher,
};
use crate::skills::{content_hash, parse_frontmatter, read_skill_files};
use crate::team;

const DEFAULT_SOURCE: &str = "skills";

const fn param(
    name: &'static str,
    about: &'static str,
    kind: ParamKind,
    required: bool,
    positional: bool,
) -> ParamSpec {
    ParamSpec {
        name,
        about,
        kind,
        required,
        positional,
    }
}

pub const LIST: CommandSpec = CommandSpec {
    name: "skill.list",
    about: "List skills with origin, version and last known upstream status (offline)",
    params: &[],
    mutating: false,
    handler: list,
};

pub const ADD: CommandSpec = CommandSpec {
    name: "skill.add",
    about: "Scaffold an own skill, or subscribe to a git repository of skills with --url",
    params: &[
        param(
            "name",
            "Name of a new own skill (omit with --url)",
            ParamKind::String,
            false,
            true,
        ),
        param(
            "description",
            "Description of a new own skill (SKILL.md frontmatter)",
            ParamKind::String,
            false,
            false,
        ),
        param(
            "url",
            "Git URL of an external skills repository to subscribe to",
            ParamKind::String,
            false,
            false,
        ),
        param(
            "ref",
            "Branch or tag to follow (default: the remote's default branch)",
            ParamKind::String,
            false,
            false,
        ),
        param(
            "path",
            "Directory of the repository holding one directory per skill",
            ParamKind::String,
            false,
            false,
        ),
        param(
            "include",
            "Only vendor these skills (repeatable or comma-separated)",
            ParamKind::List,
            false,
            false,
        ),
        param(
            "source-name",
            "Id of the source in muster.toml and skills.lock (default: from the URL)",
            ParamKind::String,
            false,
            false,
        ),
    ],
    mutating: true,
    handler: add,
};

pub const IMPORT: CommandSpec = CommandSpec {
    name: "skill.import",
    about: "Import skills found in an agent's skills folder (subscribes when the upstream repository is known, else as own skills)",
    params: &[
        param(
            "name",
            "Name of the skill folder to import (omit with --all)",
            ParamKind::String,
            false,
            true,
        ),
        param(
            "all",
            "Import every importable unmanaged skill in one run",
            ParamKind::Bool,
            false,
            false,
        ),
        param(
            "adapter",
            "Agent whose skills folder holds it (default: the only one that has it)",
            ParamKind::String,
            false,
            false,
        ),
    ],
    mutating: true,
    handler: import,
};

pub const AUTO_UPDATE: CommandSpec = CommandSpec {
    name: "skill.auto-update",
    about: "Turn unattended updates of subscribed skills during `muster sync` on or off",
    params: &[
        param(
            "state",
            "on or off (true/false also accepted)",
            ParamKind::String,
            false,
            true,
        ),
        param(
            "enabled",
            "Enable or disable auto-update (true or false)",
            ParamKind::String,
            false,
            false,
        ),
    ],
    mutating: true,
    handler: auto_update_cmd,
};

pub const PIN: CommandSpec = CommandSpec {
    name: "skill.pin",
    about: "Approve the current contents of an own skill in skills.lock",
    params: &[param(
        "name",
        "Own skill to pin",
        ParamKind::String,
        true,
        true,
    )],
    mutating: true,
    handler: pin,
};

pub const CHECK: CommandSpec = CommandSpec {
    name: "skill.check",
    about: "Fetch upstream sources and report updates, deprecations and removals",
    params: &[param(
        "name",
        "Only check this skill",
        ParamKind::String,
        false,
        true,
    )],
    mutating: false,
    handler: check,
};

pub const UPDATE: CommandSpec = CommandSpec {
    name: "skill.update",
    about: "Apply upstream changes to subscribed skills (never removes; see `skill rm`)",
    params: &[
        param("name", "Skill to update", ParamKind::String, false, true),
        param(
            "all",
            "Update every subscribed skill",
            ParamKind::Bool,
            false,
            false,
        ),
        param(
            "force",
            "Overwrite local modifications",
            ParamKind::Bool,
            false,
            false,
        ),
    ],
    mutating: true,
    handler: update,
};

pub const RM: CommandSpec = CommandSpec {
    name: "skill.rm",
    about: "Remove a skill, or with --detach keep a subscribed skill as an own skill",
    params: &[
        param("name", "Skill name", ParamKind::String, true, true),
        param(
            "detach",
            "Keep the files as an own skill and stop following its source",
            ParamKind::Bool,
            false,
            false,
        ),
    ],
    mutating: true,
    handler: rm,
};

pub const FILE_GET: CommandSpec = CommandSpec {
    name: "skill.file.get",
    about: "Read a UTF-8 file inside a skill",
    params: &[
        param("name", "Skill name", ParamKind::String, true, true),
        param(
            "path",
            "Relative file path inside the skill",
            ParamKind::String,
            true,
            false,
        ),
    ],
    mutating: false,
    handler: file_get,
};

pub const FILE_SET: CommandSpec = CommandSpec {
    name: "skill.file.set",
    about: "Write a UTF-8 file inside a skill (at most 256 KB)",
    params: &[
        param("name", "Skill name", ParamKind::String, true, true),
        param(
            "path",
            "Relative file path inside the skill",
            ParamKind::String,
            true,
            false,
        ),
        param(
            "content",
            "UTF-8 file content",
            ParamKind::String,
            true,
            false,
        ),
    ],
    mutating: true,
    handler: file_set,
};

pub const FILES_LIST: CommandSpec = CommandSpec {
    name: "skill.files",
    about: "List all files inside a skill with sizes",
    params: &[param("name", "Skill name", ParamKind::String, true, true)],
    mutating: false,
    handler: files_list,
};

const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;

fn file_get(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let name = required_name(params)?;
    let rel = params
        .get_string("path")
        .ok_or_else(|| MusterError::Usage("path is required".into()))?;
    let (dir, _) = skills_dir(ctx, &manifest)?;
    let target = contained_skill_file(&dir, &name, rel)?;
    let metadata = std::fs::symlink_metadata(&target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            MusterError::Usage(format!("{rel:?} does not exist in skill '{name}'"))
        } else {
            io_error(&target, e)
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(MusterError::Usage(
            "symlinks inside skills are forbidden".into(),
        ));
    }
    if metadata.len() as usize > MAX_SKILL_FILE_BYTES {
        return Err(MusterError::Usage(format!(
            "file exceeds maximum allowed size of {MAX_SKILL_FILE_BYTES} bytes"
        )));
    }
    let content = std::fs::read_to_string(&target).map_err(|e| io_error(&target, e))?;
    Ok(json!({ "name": name, "path": rel, "content": content, "size": metadata.len() }))
}

fn file_set(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = admin(ctx, &manifest)?;
    let name = required_name(params)?;
    let rel = params
        .get_string("path")
        .ok_or_else(|| MusterError::Usage("path is required".into()))?;
    let content = params
        .get_string("content")
        .ok_or_else(|| MusterError::Usage("content is required".into()))?;
    if content.len() > MAX_SKILL_FILE_BYTES {
        return Err(MusterError::Usage(format!(
            "content exceeds maximum allowed size of {MAX_SKILL_FILE_BYTES} bytes"
        )));
    }
    ctx.confirm(&format!("Save {rel} in skill '{name}'?"))?;
    let (dir, _) = skills_dir(ctx, &manifest)?;
    let target = contained_skill_file(&dir, &name, rel)?;
    // `symlink_metadata` also sees dangling symlinks, which `exists()` misses.
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(MusterError::Usage("cannot overwrite symlinks".into()));
        }
    }
    if !ctx.dry_run {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        crate::manifest::write_atomic(&target, content.as_bytes())
            .map_err(|e| io_error(&target, e))?;
        // Own skills follow their edits: re-pin so the next sync applies them.
        // Subscribed skills are left alone; an update would overwrite them.
        let mut lock = SkillsLock::load(&ctx.config_dir)?;
        if lock.get(&name).is_some_and(LockEntry::is_local) {
            let files = read_skill_files(&contained(&dir, &name, None)?)?;
            if let Some(entry) = lock.skills.iter_mut().find(|e| e.name == name) {
                entry.content_hash = content_hash(&files);
                entry.version = files
                    .get("SKILL.md")
                    .and_then(|text| parse_frontmatter(text))
                    .and_then(|m| m.version);
                entry.updated_at = now();
            }
            lock.save(&ctx.config_dir)?;
        }
        commit(
            ctx,
            actor.as_ref(),
            &format!("feat(skills): edit {name}/{rel}"),
        )?;
    }
    Ok(json!({ "name": name, "path": rel, "saved": !ctx.dry_run, "size": content.len() }))
}

fn files_list(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let name = required_name(params)?;
    let (dir, _) = skills_dir(ctx, &manifest)?;
    let skill_dir = contained(&dir, &name, None)?;
    if !skill_dir.is_dir() {
        return Err(MusterError::Usage(format!(
            "skill '{name}' does not exist on disk"
        )));
    }
    let files = read_skill_files(&skill_dir)?;
    let list: Vec<Value> = files
        .into_iter()
        .map(|(path, content)| json!({ "path": path, "size": content.len() }))
        .collect();
    Ok(json!({ "name": name, "files": list }))
}

pub fn contained_skill_file(
    skills_dir: &Path,
    name: &str,
    rel: &str,
) -> Result<PathBuf, MusterError> {
    let escape = || MusterError::Usage("path escapes the skill directory".into());
    let skill_dir = contained(skills_dir, name, None).map_err(|_| escape())?;
    let target = contained(skills_dir, name, Some(rel)).map_err(|_| escape())?;

    let canonical_root = match skills_dir.canonicalize() {
        Ok(p) => p,
        // Nothing exists yet, so nothing can be redirected.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(target),
        Err(e) => return Err(io_error(skills_dir, e)),
    };
    // The skill directory itself must be a real directory inside the skills root.
    match std::fs::symlink_metadata(&skill_dir) {
        Ok(meta) if meta.file_type().is_symlink() => return Err(escape()),
        Ok(_) => {
            let canonical_dir = skill_dir
                .canonicalize()
                .map_err(|e| io_error(&skill_dir, e))?;
            if !canonical_dir.starts_with(canonical_root.join(name)) {
                return Err(escape());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(target),
        Err(e) => return Err(io_error(&skill_dir, e)),
    }
    // Walk every component below the skill directory: none may be a symlink.
    let mut current = skill_dir;
    for component in Path::new(rel).components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(escape()),
            Ok(_) => {}
            // The rest does not exist yet, so it cannot be a symlink.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(escape()),
        }
    }
    Ok(target)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Skill names follow the Agent Skills convention: lowercase, digits, '-'.
pub fn validate_skill_name(name: &str) -> Result<String, MusterError> {
    if lock::is_valid_name(name) {
        Ok(name.to_string())
    } else {
        Err(MusterError::Usage(format!(
            "invalid skill name '{name}': use lowercase letters, digits and '-'"
        )))
    }
}

fn required_name(params: &Params) -> Result<String, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("skill name is required".into()))?;
    validate_skill_name(name)
}

/// The skills directory of the config repo and its manifest-relative path.
fn skills_dir(ctx: &Context, manifest: &Manifest) -> Result<(PathBuf, String), MusterError> {
    let source = manifest
        .agents
        .as_ref()
        .and_then(|a| a.skills.as_ref())
        .map_or(DEFAULT_SOURCE, |s| s.source.as_str())
        .to_string();
    let rel =
        relative(&source, "agents.skills.source").map_err(|_| MusterError::InvalidConfig {
            message: format!(
                "skills source `{source}` must be a path inside the config repository"
            ),
            path: Some(format!("{}: agents.skills.source", manifest::FILE_NAME)),
        })?;
    Ok((ctx.config_dir.join(rel), source))
}

fn remotes(manifest: &Manifest) -> Vec<SkillRemote> {
    manifest
        .agents
        .as_ref()
        .and_then(|a| a.skills.as_ref())
        .map(|s| s.remote.clone())
        .unwrap_or_default()
}

/// Team mode: skill changes are admin changes. Single-person mode: no actor.
fn admin(ctx: &Context, manifest: &Manifest) -> Result<Option<team::Actor>, MusterError> {
    if manifest.members.is_empty() {
        return Ok(None);
    }
    let actor = team::actor(manifest, &ctx.state_dir)?;
    if !actor.admin {
        return Err(MusterError::Security(format!(
            "only team admins can add, update or remove skills ('{}' is not an admin)",
            actor.member
        )));
    }
    Ok(Some(actor))
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

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn io_error(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}

/// `<skills_dir>/<name>[/<rel>]`, refusing any path that could leave
/// `skills_dir`: `name` must be a valid skill name and `rel` (a file inside the
/// skill, from an upstream checkout or the lock) must be relative and made only
/// of plain components (no `..`, root, or drive prefix).
fn contained(skills_dir: &Path, name: &str, rel: Option<&str>) -> Result<PathBuf, MusterError> {
    let name = validate_skill_name(name)?;
    let mut path = skills_dir.join(name);
    if let Some(rel) = rel {
        let rel_path = Path::new(rel);
        let plain = !rel.is_empty()
            && !rel.contains('\\')
            && !rel.chars().any(char::is_control)
            && rel_path
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !plain {
            return Err(MusterError::Security(format!(
                "skill '{}' file path {rel:?} escapes the skill directory",
                path.file_name().unwrap_or_default().to_string_lossy()
            )));
        }
        path = path.join(rel_path);
    }
    if !path.starts_with(skills_dir) {
        return Err(MusterError::Security(format!(
            "{} is outside the skills directory {}",
            path.display(),
            skills_dir.display()
        )));
    }
    Ok(path)
}

/// Replace `<skills_dir>/<name>` with exactly `files`. Every path is checked to
/// stay inside `skills_dir` before anything is removed or written.
fn write_skill(
    skills_dir: &Path,
    name: &str,
    files: &BTreeMap<String, String>,
) -> Result<(), MusterError> {
    let dir = contained(skills_dir, name, None)?;
    let targets = files
        .iter()
        .map(|(rel, contents)| Ok((contained(skills_dir, name, Some(rel))?, contents)))
        .collect::<Result<Vec<_>, MusterError>>()?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
    }
    for (path, contents) in targets {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
        }
        std::fs::write(&path, contents).map_err(|e| io_error(&path, e))?;
    }
    Ok(())
}

/// Per-file change summary from `old` to `new`.
fn diff_summary(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> Value {
    let normalize = |s: &str| s.replace("\r\n", "\n");
    let added: Vec<&String> = new.keys().filter(|k| !old.contains_key(*k)).collect();
    let removed: Vec<&String> = old.keys().filter(|k| !new.contains_key(*k)).collect();
    let changed: Vec<&String> = new
        .iter()
        .filter(|(k, v)| old.get(*k).is_some_and(|o| normalize(o) != normalize(v)))
        .map(|(k, _)| k)
        .collect();
    json!({ "added": added, "changed": changed, "removed": removed })
}

/// One skill as fetched from its upstream source.
struct UpstreamSkill {
    files: BTreeMap<String, String>,
    hash: String,
    version: Option<String>,
    deprecated: Option<String>,
}

impl UpstreamSkill {
    fn upstream(&self) -> Upstream {
        Upstream::Present {
            content_hash: self.hash.clone(),
            deprecated: self.deprecated.clone(),
        }
    }
}

fn read_upstream(
    checkout: &Checkout,
    remote: &SkillRemote,
    name: &str,
) -> Result<Option<UpstreamSkill>, MusterError> {
    let dir = checkout.contained_skills_root(remote)?.join(name);
    let Ok(skill_md) = std::fs::read_to_string(dir.join("SKILL.md")) else {
        return Ok(None);
    };
    let meta = parse_frontmatter(&skill_md).unwrap_or_default();
    let files = read_skill_files(&dir)?;
    Ok(Some(UpstreamSkill {
        hash: content_hash(&files),
        files,
        version: meta.version,
        deprecated: meta.deprecated,
    }))
}

fn diag_for(ctx: &mut Context, name: &str, status: SkillStatus, reason: Option<&str>) {
    let diagnostic = match status {
        SkillStatus::UpdateAvailable => Diagnostic::new(
            Severity::Info,
            "skill_update_available",
            format!("a new upstream version of skill '{name}' is available"),
        )
        .with_suggestion(format!("muster skill update {name}")),
        SkillStatus::Deprecated => Diagnostic::new(
            Severity::Warning,
            "skill_deprecated",
            match reason.filter(|r| !r.is_empty()) {
                Some(reason) => format!("skill '{name}' is deprecated upstream: {reason}"),
                None => format!("skill '{name}' is deprecated upstream"),
            },
        )
        .with_suggestion(format!(
            "keep it, or remove it with `muster skill rm {name} --yes`"
        )),
        SkillStatus::UpstreamRemoved => Diagnostic::new(
            Severity::Warning,
            "skill_upstream_removed",
            format!("skill '{name}' was removed from its upstream source"),
        )
        .with_suggestion(format!(
            "keep it as an own skill with `muster skill rm {name} --detach --yes`, or delete it with `muster skill rm {name} --yes`"
        )),
        _ => return,
    };
    ctx.diag(diagnostic);
}

// ---------------------------------------------------------------------------
// muster.toml edits (toml_edit keeps the user's formatting and comments)
// ---------------------------------------------------------------------------

fn edit_manifest(
    ctx: &Context,
    edit: impl FnOnce(&mut Table) -> Result<(), MusterError>,
) -> Result<(), MusterError> {
    let path = ctx.config_dir.join(manifest::FILE_NAME);
    let text = std::fs::read_to_string(&path).map_err(|e| io_error(&path, e))?;
    let invalid = |message: String| MusterError::InvalidConfig {
        message,
        path: Some(path.display().to_string()),
    };
    let mut doc: DocumentMut = text.parse().map_err(|e| invalid(format!("{e}")))?;
    if !doc.contains_key("agents") {
        let mut agents = Table::new();
        agents.set_implicit(true);
        doc.insert("agents", Item::Table(agents));
    }
    let agents = doc["agents"]
        .as_table_mut()
        .ok_or_else(|| invalid("`agents` is not a table".into()))?;
    if !agents.contains_key("skills") {
        let mut skills = Table::new();
        skills.insert("source", toml_edit::value(DEFAULT_SOURCE));
        agents.insert("skills", Item::Table(skills));
    }
    let skills = agents["skills"]
        .as_table_mut()
        .ok_or_else(|| invalid("`agents.skills` is not a table".into()))?;
    edit(skills)?;
    let out = doc.to_string();
    Manifest::parse(&out, &path.display().to_string())?;
    std::fs::write(&path, out).map_err(|e| io_error(&path, e))
}

fn add_remote_entry(skills: &mut Table, remote: &SkillRemote) -> Result<(), MusterError> {
    if !skills.contains_key("remote") {
        skills.insert("remote", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let list =
        skills["remote"]
            .as_array_of_tables_mut()
            .ok_or_else(|| MusterError::InvalidConfig {
                message: "`agents.skills.remote` must be an array of tables".into(),
                path: Some(manifest::FILE_NAME.into()),
            })?;
    let mut table = Table::new();
    table.insert("name", toml_edit::value(remote.name.clone()));
    table.insert("url", toml_edit::value(remote.url.clone()));
    if let Some(r) = &remote.git_ref {
        table.insert("ref", toml_edit::value(r.clone()));
    }
    if let Some(p) = &remote.path {
        table.insert("path", toml_edit::value(p.clone()));
    }
    if !remote.include.is_empty() {
        let mut include = Array::new();
        for name in &remote.include {
            include.push(name.clone());
        }
        table.insert("include", toml_edit::value(include));
    }
    list.push(table);
    Ok(())
}

/// Stop following `skill` from source `origin`: drop it from `include`, and
/// drop the whole source once none of its skills remain locked.
fn forget_remote_skill(skills: &mut Table, origin: &str, skill: &str, source_unused: bool) {
    let Some(list) = skills
        .get_mut("remote")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return;
    };
    let is_origin = |t: &Table| t.get("name").and_then(Item::as_str) == Some(origin);
    if source_unused {
        list.retain(|t| !is_origin(t));
    } else if let Some(table) = list.iter_mut().find(|t| is_origin(t)) {
        if let Some(include) = table.get_mut("include").and_then(Item::as_array_mut) {
            include.retain(|v| v.as_str() != Some(skill));
            include.fmt();
        }
    }
    if list.is_empty() {
        skills.remove("remote");
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn skill_view(
    name: &str,
    entry: Option<&LockEntry>,
    disk: Option<&BTreeMap<String, String>>,
    cache: &StatusCache,
    disabled: bool,
) -> Value {
    let is_local = entry.is_none_or(LockEntry::is_local);
    let cached = cache.skills.get(name);
    let status = if is_local {
        SkillStatus::Local
    } else {
        cached.map_or(SkillStatus::Unknown, |c| c.status)
    };
    let modified = match (entry, disk) {
        (Some(e), Some(files)) => content_hash(files) != e.content_hash,
        _ => false,
    };
    let version = entry.and_then(|e| e.version.clone()).or_else(|| {
        disk.and_then(|f| f.get("SKILL.md"))
            .and_then(|md| parse_frontmatter(md))
            .and_then(|m| m.version)
    });
    json!({
        "name": name,
        "origin": entry.map_or(lock::LOCAL, |e| e.origin.as_str()),
        "version": version,
        "commit": entry.and_then(|e| e.commit.clone()),
        "updated_at": entry.map(|e| e.updated_at.clone()),
        "status": status.as_str(),
        "reason": if is_local { None } else { cached.and_then(|c| c.reason.clone()) },
        "checked_at": if is_local { None } else { cached.map(|c| c.checked_at.clone()) },
        "modified": modified,
        "locked": entry.is_some(),
        "missing": disk.is_none(),
        "needs_attention": status.needs_attention(),
        "enabled": !disabled,
    })
}

/// Skill directories present in the skills source (non-hidden directories).
fn disk_skills(dir: &Path) -> Result<BTreeMap<String, BTreeMap<String, String>>, MusterError> {
    let mut out = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(io_error(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_error(dir, e))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if crate::skills::is_ignored_skill_name(&name, true)
            || !entry.file_type().is_ok_and(|t| t.is_dir())
        {
            continue;
        }
        out.insert(name, read_skill_files(&entry.path())?);
    }
    Ok(out)
}

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let (dir, source) = skills_dir(ctx, &manifest)?;
    let lock = SkillsLock::load(&ctx.config_dir)?;
    let cache = StatusCache::load(&ctx.state_dir);
    let disk = disk_skills(&dir)?;
    let layers = crate::engine::layer::global_layers(
        &ctx.config_dir,
        &manifest,
        crate::machine::load_name(&ctx.state_dir).as_deref(),
    );
    let merged = crate::engine::layer::merge(&layers);
    let names: BTreeSet<&String> = disk
        .keys()
        .chain(lock.skills.iter().map(|e| &e.name))
        .collect();
    let skills: Vec<Value> = names
        .iter()
        .map(|&name| {
            let disabled = merged.disabled("skill", name, "").is_some();
            skill_view(name, lock.get(name), disk.get(name), &cache, disabled)
        })
        .collect();
    let attention = skills
        .iter()
        .filter(|s| s["needs_attention"] == true)
        .count();
    let sources: Vec<Value> = remotes(&manifest)
        .iter()
        .map(|r| json!({ "name": r.name, "url": r.url, "ref": r.git_ref, "path": r.path, "include": r.include }))
        .collect();
    let managed: BTreeSet<&str> = names.iter().map(|n| n.as_str()).collect();
    let provenance = read_provenance(ctx);
    let unmanaged = unmanaged_skills(ctx, &merged.agents, &managed, &provenance);
    let auto_update = merged.agents.skills.as_ref().is_some_and(|s| s.auto_update);
    Ok(json!({
        "auto_update": auto_update,
        "source": source,
        "skills": skills,
        "unmanaged": unmanaged,
        "sources": sources,
        "attention": attention,
        "checked_at": cache.checked_at,
    }))
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

fn add(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = admin(ctx, &manifest)?;
    match params.get_string("url") {
        Some(url) => {
            if params.get_string("name").is_some() {
                return Err(MusterError::Usage(
                    "pass either a skill name (own skill) or --url (subscription), not both; \
                     use --include to pick skills of the source"
                        .into(),
                ));
            }
            let fetcher = GitFetcher::from_env()?;
            add_remote(ctx, params, &manifest, actor.as_ref(), url, &fetcher)
        }
        None => add_local(ctx, params, &manifest, actor.as_ref()),
    }
}

fn pin(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = admin(ctx, &manifest)?;
    let name = required_name(params)?;
    let (dir, _) = skills_dir(ctx, &manifest)?;
    let mut lock = SkillsLock::load(&ctx.config_dir)?;
    let previous = lock.get(&name);
    if previous.is_some_and(|entry| !entry.is_local()) {
        return Err(MusterError::Usage(format!(
            "skill '{name}' is subscribed; use `skill update` instead"
        )));
    }
    let files = read_skill_files(&dir.join(&name))?;
    let skill_md = files
        .get("SKILL.md")
        .ok_or_else(|| MusterError::Usage(format!("skill '{name}' has no SKILL.md")))?;
    let metadata = parse_frontmatter(skill_md).ok_or_else(|| {
        MusterError::Usage(format!(
            "invalid skill '{name}': missing name or description"
        ))
    })?;
    if metadata.name.as_deref() != Some(&name) || metadata.description.is_none() {
        return Err(MusterError::Usage(format!(
            "invalid skill '{name}': SKILL.md needs a matching name and description"
        )));
    }
    let entry = LockEntry {
        name: name.clone(),
        origin: lock::LOCAL.into(),
        commit: None,
        content_hash: content_hash(&files),
        version: metadata.version,
        updated_at: now(),
    };
    let result = json!({"name": name, "origin": lock::LOCAL, "content_hash": entry.content_hash});
    if !ctx.dry_run {
        lock.upsert(entry);
        lock.save(&ctx.config_dir)?;
        edit_manifest(ctx, |_| Ok(()))?;
        commit(
            ctx,
            actor.as_ref(),
            &format!("feat(skills): pin skill {name}"),
        )?;
    }
    Ok(result)
}

fn add_local(
    ctx: &mut Context,
    params: &Params,
    manifest: &Manifest,
    actor: Option<&team::Actor>,
) -> Result<Value, MusterError> {
    let name = required_name(params).map_err(|e| match params.get_string("name") {
        None => {
            MusterError::Usage("pass a skill name, or --url to subscribe to a repository".into())
        }
        Some(_) => e,
    })?;
    let (dir, source) = skills_dir(ctx, manifest)?;
    let mut lock = SkillsLock::load(&ctx.config_dir)?;
    let skill_dir = dir.join(&name);
    if skill_dir.exists() || lock.get(&name).is_some() {
        return Err(MusterError::Usage(format!("skill '{name}' already exists")));
    }
    let description = params
        .get_string("description")
        .map(|d| d.replace(['\n', '\r'], " "))
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| format!("Describe when to use the {name} skill"));
    let skill_md = format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nInstructions for the agent.\n"
    );
    let files = BTreeMap::from([("SKILL.md".to_string(), skill_md)]);
    let entry = LockEntry {
        name: name.clone(),
        origin: lock::LOCAL.into(),
        commit: None,
        content_hash: content_hash(&files),
        version: None,
        updated_at: now(),
    };
    let result = json!({
        "name": name,
        "origin": lock::LOCAL,
        "path": format!("{source}/{name}/SKILL.md"),
    });
    if ctx.dry_run {
        return Ok(result);
    }
    write_skill(&dir, &name, &files)?;
    lock.upsert(entry);
    lock.save(&ctx.config_dir)?;
    edit_manifest(ctx, |_| Ok(()))?;
    commit(ctx, actor, &format!("feat(skills): add skill {name}"))?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// import: skills installed directly in agent folders
// ---------------------------------------------------------------------------

/// A skill folder found in an enabled agent's user-scope skills directory.
struct OnDisk {
    adapter: &'static str,
    /// Canonical directory holding the skill's files.
    dir: PathBuf,
}

/// Skill folders (symlinks to folders included) in the enabled agents' user
/// skills directories that Muster neither manages nor ships built in, by name.
/// Paths never leave this module: callers choose an adapter id, not a path.
fn find_on_disk(
    config_dir: &Path,
    agents: &crate::manifest::AgentsConfig,
    managed: &BTreeSet<&str>,
) -> Result<BTreeMap<String, Vec<OnDisk>>, MusterError> {
    let enabled = crate::engine::enabled_adapters(Some(agents))?;
    let roots = crate::engine::Roots::from_env()?;
    let dirs = crate::commands::effective::skill_roots(config_dir, &enabled, &roots, None)?;
    let mut found: BTreeMap<String, Vec<OnDisk>> = BTreeMap::new();
    for (adapter, root) in dirs {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if crate::skills::is_ignored_skill_name(&name, true)
                || crate::skills::builtin::is_builtin(&name)
                || managed.contains(name.as_str())
                || !lock::is_valid_name(&name)
            {
                continue;
            }
            // Follows a symlink; only one that lands on a directory counts.
            let Ok(dir) = entry.path().canonicalize() else {
                continue;
            };
            if dir.is_dir() {
                found.entry(name).or_default().push(OnDisk { adapter, dir });
            }
        }
    }
    Ok(found)
}

/// Why a skill's files cannot be imported, or `None` when they can.
fn import_problem(name: &str, files: &BTreeMap<String, String>) -> Option<String> {
    let Some(skill_md) = files.get("SKILL.md") else {
        return Some("missing SKILL.md".into());
    };
    let metadata = parse_frontmatter(skill_md);
    match metadata {
        Some(m) if m.description.is_some() && m.name.as_deref() == Some(name) => None,
        Some(m) if m.name.is_some() && m.description.is_some() => Some(format!(
            "SKILL.md name `{}` does not match the folder name `{name}`",
            m.name.unwrap_or_default()
        )),
        _ => Some("SKILL.md needs frontmatter with a name and a description".into()),
    }
}

/// Unmanaged skills for the dashboard: one entry per name, listing every
/// agent that has it. Best effort: an unreadable HOME just yields none.
fn unmanaged_skills(
    ctx: &Context,
    agents: &crate::manifest::AgentsConfig,
    managed: &BTreeSet<&str>,
    provenance: &BTreeMap<String, Provenance>,
) -> Vec<Value> {
    let Ok(found) = find_on_disk(&ctx.config_dir, agents, managed) else {
        return Vec::new();
    };
    found
        .into_iter()
        .map(|(name, places)| {
            let checked: Vec<_> = places
                .iter()
                .map(|place| {
                    let md = place.dir.join("SKILL.md");
                    let files = std::fs::read_to_string(&md)
                        .ok()
                        .filter(|_| md.is_file())
                        .map(|text| BTreeMap::from([("SKILL.md".to_string(), text)]))
                        .unwrap_or_default();
                    let problem = import_problem(&name, &files);
                    let description = files
                        .get("SKILL.md")
                        .and_then(|text| parse_frontmatter(text))
                        .and_then(|m| m.description);
                    (place.adapter, problem, description)
                })
                .collect();
            let best = checked
                .iter()
                .find(|(_, problem, _)| problem.is_none())
                .or(checked.first());
            json!({
                "name": name,
                "adapters": checked.iter().map(|c| c.0).collect::<Vec<_>>(),
                "adapter": best.map(|c| c.0),
                "valid": best.is_some_and(|c| c.1.is_none()),
                "problem": best.and_then(|c| c.1.clone()),
                "description": best.and_then(|c| c.2.clone()),
                "source": provenance.get(&name).map(Provenance::view),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// provenance: `npx skills` lock (`~/.agents/.skill-lock.json`)
// ---------------------------------------------------------------------------

/// Where an on-disk skill was installed from, according to the `npx skills`
/// CLI's lock file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Provenance {
    /// Human label, e.g. `mattpocock/skills`.
    label: String,
    url: String,
    /// Directory of the repository that holds one directory per skill.
    path: Option<String>,
}

impl Provenance {
    fn view(&self) -> Value {
        json!({ "name": self.label, "url": self.url, "path": self.path })
    }
}

fn provenance_from_entry(name: &str, entry: &Value) -> Option<Provenance> {
    let text = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let url = text("sourceUrl")?;
    let skill_path = text("skillPath")?.trim_matches('/');
    let skill_dir = skill_path
        .strip_suffix("SKILL.md")
        .map_or(skill_path, |d| d.trim_end_matches('/'));
    let (parent, last) = match skill_dir.rsplit_once('/') {
        Some((parent, last)) => (Some(parent), last),
        None => (None, skill_dir),
    };
    if last != name {
        return None;
    }
    if let Some(parent) = parent {
        relative(parent, "path").ok()?;
    }
    let label = text("source")
        .map(str::to_owned)
        .unwrap_or_else(|| source_name_from_url(url));
    Some(Provenance {
        label,
        url: url.to_string(),
        path: parent.map(str::to_owned),
    })
}

/// Provenance by skill name from `~/.agents/.skill-lock.json`. A missing file
/// means no provenance; an unreadable one is reported and ignored.
fn read_provenance(ctx: &mut Context) -> BTreeMap<String, Provenance> {
    let Ok(roots) = crate::engine::Roots::from_env() else {
        return BTreeMap::new();
    };
    let path = roots
        .base(crate::adapters::Base::Home)
        .join(".agents/.skill-lock.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(e) => {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "skill_lock_unreadable",
                format!(
                    "cannot read {}: {e}; skills are treated as own",
                    path.display()
                ),
            ));
            return BTreeMap::new();
        }
    };
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "skill_lock_unreadable",
                format!(
                    "cannot parse {}: {e}; skills are treated as own",
                    path.display()
                ),
            ));
            return BTreeMap::new();
        }
    };
    parsed
        .get("skills")
        .and_then(Value::as_object)
        .map(|skills| {
            skills
                .iter()
                .filter_map(|(name, entry)| {
                    provenance_from_entry(name, entry).map(|p| (name.clone(), p))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn slugify(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let c = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if c != '-' || !out.ends_with('-') {
            out.push(c);
        }
    }
    out.trim_matches('-').to_string()
}

/// Deterministic id for a new source: `<owner>-<repo>[-<last path segment>]`,
/// numbered when already taken.
fn remote_name_for(prov: &Provenance, taken: &BTreeSet<String>) -> String {
    let mut base = slugify(&prov.label);
    if base.is_empty() {
        base = source_name_from_url(&prov.url);
    }
    if let Some(last) = prov.path.as_deref().and_then(|p| p.rsplit('/').next()) {
        let last = slugify(last);
        if !last.is_empty() {
            base = format!("{base}-{last}");
        }
    }
    if base == lock::LOCAL {
        base = "local-source".into();
    }
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

fn same_url(a: &str, b: &str) -> bool {
    let norm = |u: &str| {
        let u = u.trim().trim_end_matches('/');
        u.strip_suffix(".git").unwrap_or(u).to_string()
    };
    norm(a) == norm(b)
}

fn same_path(a: Option<&str>, b: Option<&str>) -> bool {
    let norm = |p: Option<&str>| {
        p.map(|p| p.trim_matches('/').to_string())
            .filter(|p| !p.is_empty())
    };
    norm(a) == norm(b)
}

/// Add `names` to the `include` of remote `remote`. A remote without
/// `include` already covers every skill under its path.
fn extend_remote_include(skills: &mut Table, remote: &str, names: &[String]) {
    let Some(list) = skills
        .get_mut("remote")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return;
    };
    let Some(table) = list
        .iter_mut()
        .find(|t| t.get("name").and_then(Item::as_str) == Some(remote))
    else {
        return;
    };
    if let Some(include) = table.get_mut("include").and_then(Item::as_array_mut) {
        for name in names {
            if !include.iter().any(|v| v.as_str() == Some(name)) {
                include.push(name.clone());
            }
        }
        include.fmt();
    }
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// A validated on-disk skill ready to import.
struct Prepared {
    adapter: &'static str,
    files: BTreeMap<String, String>,
}

fn prepare_import(
    name: &str,
    mut places: Vec<OnDisk>,
    adapter: Option<&str>,
    dir: &Path,
    lock: &SkillsLock,
) -> Result<Prepared, MusterError> {
    if dir.join(name).exists() || lock.get(name).is_some() {
        return Err(MusterError::Usage(format!("skill '{name}' already exists")));
    }
    if crate::skills::builtin::is_builtin(name) {
        return Err(MusterError::Usage(format!(
            "'{name}' is a built-in skill and cannot be imported"
        )));
    }
    if let Some(wanted) = adapter {
        places.retain(|p| p.adapter == wanted);
        if places.is_empty() {
            return Err(MusterError::Usage(format!(
                "skill '{name}' was not found in the skills folder of agent '{wanted}'"
            )));
        }
    } else if places.is_empty() {
        return Err(MusterError::Usage(format!(
            "skill '{name}' was not found in any enabled agent's skills folder"
        )));
    }
    let mut candidates = Vec::new();
    for place in &places {
        candidates.push((place.adapter, read_skill_files(&place.dir)?));
    }
    let (adapter, files) = candidates.remove(0);
    if let Some((other, _)) = candidates
        .iter()
        .find(|(_, other)| content_hash(other) != content_hash(&files))
    {
        return Err(MusterError::Usage(format!(
            "skill '{name}' differs between agents ({adapter}, {other}); pass --adapter to choose one"
        )));
    }
    if let Some(problem) = import_problem(name, &files) {
        return Err(MusterError::Usage(format!(
            "cannot import '{name}': {problem}"
        )));
    }
    Ok(Prepared { adapter, files })
}

/// One source the import subscribes to: an existing `[[remote]]` or a new one.
struct RemotePlan {
    remote: SkillRemote,
    is_new: bool,
    /// Skills of this run to add to an existing remote's `include`.
    names: Vec<String>,
}

fn import(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = admin(ctx, &manifest)?;
    let all = params.get_bool("all");
    let single = match (params.get_string("name"), all) {
        (Some(_), true) => {
            return Err(MusterError::Usage(
                "pass a skill name or --all (not both)".into(),
            ))
        }
        (None, false) => {
            return Err(MusterError::Usage("pass a skill name or --all".into()));
        }
        (Some(_), false) => Some(required_name(params)?),
        (None, true) => None,
    };
    let (dir, source) = skills_dir(ctx, &manifest)?;
    let lock = SkillsLock::load(&ctx.config_dir)?;
    let layers = crate::engine::layer::global_layers(
        &ctx.config_dir,
        &manifest,
        crate::machine::load_name(&ctx.state_dir).as_deref(),
    );
    let merged = crate::engine::layer::merge(&layers);
    let managed: BTreeSet<String> = if all {
        disk_skills(&dir)?
            .into_keys()
            .chain(lock.skills.iter().map(|e| e.name.clone()))
            .collect()
    } else {
        BTreeSet::new()
    };
    let managed_refs: BTreeSet<&str> = managed.iter().map(String::as_str).collect();
    let mut found = find_on_disk(&ctx.config_dir, &merged.agents, &managed_refs)?;
    let wanted: Vec<String> = match &single {
        Some(name) => vec![name.clone()],
        None => found.keys().cloned().collect(),
    };
    let provenance = read_provenance(ctx);

    let mut prepared: Vec<(String, Prepared)> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    for name in wanted {
        let places = found.remove(&name).unwrap_or_default();
        match prepare_import(&name, places, params.get_string("adapter"), &dir, &lock) {
            Ok(p) => prepared.push((name, p)),
            Err(e) if all => {
                ctx.diag(Diagnostic::new(
                    Severity::Warning,
                    "skill_import_skipped",
                    format!("skipped '{name}': {e}"),
                ));
                skipped.push(json!({ "name": name, "reason": e.to_string() }));
            }
            Err(e) => return Err(e),
        }
    }

    // Resolve a source for every skill with provenance, one fetch per source.
    let mut plans: Vec<RemotePlan> = Vec::new();
    let mut taken: BTreeSet<String> = remotes(&manifest).iter().map(|r| r.name.clone()).collect();
    let mut existing = remotes(&manifest);
    let mut subscribed: Vec<(usize, String, Prepared, Provenance)> = Vec::new();
    let mut own: Vec<(String, Prepared)> = Vec::new();
    for (name, prep) in prepared {
        let Some(prov) = provenance.get(&name).cloned() else {
            own.push((name, prep));
            continue;
        };
        let index = match plans.iter().position(|p| {
            same_url(&p.remote.url, &prov.url)
                && same_path(p.remote.path.as_deref(), prov.path.as_deref())
                && p.remote.git_ref.is_none()
        }) {
            Some(i) => i,
            None => {
                let (remote, is_new) = match existing.iter().position(|r| {
                    same_url(&r.url, &prov.url)
                        && same_path(r.path.as_deref(), prov.path.as_deref())
                        && r.git_ref.is_none()
                }) {
                    Some(i) => (existing.remove(i), false),
                    None => {
                        let remote_name = remote_name_for(&prov, &taken);
                        taken.insert(remote_name.clone());
                        (
                            SkillRemote {
                                name: remote_name,
                                url: prov.url.clone(),
                                git_ref: None,
                                path: prov.path.clone(),
                                include: Vec::new(),
                            },
                            true,
                        )
                    }
                };
                plans.push(RemotePlan {
                    remote,
                    is_new,
                    names: Vec::new(),
                });
                plans.len() - 1
            }
        };
        let plan = &mut plans[index];
        plan.names.push(name.clone());
        if plan.is_new {
            plan.remote.include.push(name.clone());
        }
        subscribed.push((index, name, prep, prov));
    }

    // Fetch every source once; nothing is written when one fails.
    let mut checkouts: Vec<Checkout> = Vec::new();
    if !plans.is_empty() {
        let fetcher = GitFetcher::from_env()?;
        for plan in &plans {
            let checkout = fetcher.fetch(&plan.remote).map_err(|e| {
                MusterError::Operational(format!(
                    "cannot fetch upstream {} for skill(s) {}: {e}; nothing was imported \
                     (the on-disk copy is not used as a fallback)",
                    plan.remote.url,
                    plan.names.join(", ")
                ))
            })?;
            checkouts.push(checkout);
        }
    }

    struct Vendored {
        name: String,
        adapter: &'static str,
        plan: usize,
        skill: UpstreamSkill,
        replaced: bool,
        prov: Provenance,
    }
    let mut vendored: Vec<Vendored> = Vec::new();
    for (plan, name, prep, prov) in subscribed {
        match read_upstream(&checkouts[plan], &plans[plan].remote, &name)? {
            Some(skill) => {
                let replaced = skill.hash != content_hash(&prep.files);
                vendored.push(Vendored {
                    name,
                    adapter: prep.adapter,
                    plan,
                    skill,
                    replaced,
                    prov,
                });
            }
            None => {
                let message = format!(
                    "skill '{name}' was not found upstream at {} ({})",
                    plans[plan].remote.url,
                    plans[plan]
                        .remote
                        .path
                        .as_deref()
                        .unwrap_or("repository root")
                );
                if !all {
                    return Err(MusterError::Usage(message));
                }
                ctx.diag(Diagnostic::new(
                    Severity::Warning,
                    "skill_import_skipped",
                    format!("skipped '{name}': {message}"),
                ));
                skipped.push(json!({ "name": name, "reason": message }));
            }
        }
    }

    let mut imported: Vec<Value> = Vec::new();
    for v in &vendored {
        imported.push(json!({
            "name": v.name,
            "origin": plans[v.plan].remote.name,
            "adapter": v.adapter,
            "path": format!("{source}/{}/SKILL.md", v.name),
            "files": v.skill.files.len(),
            "source": v.prov.view(),
            "commit": checkouts[v.plan].commit,
            "version": v.skill.version,
            "replaced_local_copy": v.replaced,
            "new_remote": plans[v.plan].is_new,
        }));
    }
    for (name, prep) in &own {
        imported.push(json!({
            "name": name,
            "origin": lock::LOCAL,
            "adapter": prep.adapter,
            "path": format!("{source}/{name}/SKILL.md"),
            "files": prep.files.len(),
            "source": Value::Null,
        }));
    }
    imported.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let result_for = |imported: Vec<Value>, skipped: Vec<Value>, dry_run: bool| match &single {
        Some(_) => imported.into_iter().next().unwrap_or(Value::Null),
        None => json!({ "imported": imported, "skipped": skipped, "dry_run": dry_run }),
    };
    if ctx.dry_run || (vendored.is_empty() && own.is_empty()) {
        return Ok(result_for(imported, skipped, ctx.dry_run));
    }

    let mut lock = lock;
    let updated_at = now();
    let mut cache = StatusCache::load(&ctx.state_dir);
    for v in &vendored {
        write_skill(&dir, &v.name, &v.skill.files)?;
        lock.upsert(LockEntry {
            name: v.name.clone(),
            origin: plans[v.plan].remote.name.clone(),
            commit: Some(checkouts[v.plan].commit.clone()),
            content_hash: v.skill.hash.clone(),
            version: v.skill.version.clone(),
            updated_at: updated_at.clone(),
        });
        let status = classify(false, &v.skill.hash, &v.skill.upstream());
        cache.skills.insert(
            v.name.clone(),
            CachedStatus {
                status,
                reason: v.skill.deprecated.clone(),
                upstream_commit: Some(checkouts[v.plan].commit.clone()),
                checked_at: updated_at.clone(),
            },
        );
        diag_for(ctx, &v.name, status, v.skill.deprecated.as_deref());
    }
    for (name, prep) in &own {
        write_skill(&dir, name, &prep.files)?;
        lock.upsert(LockEntry {
            name: name.clone(),
            origin: lock::LOCAL.into(),
            commit: None,
            content_hash: content_hash(&prep.files),
            version: prep
                .files
                .get("SKILL.md")
                .and_then(|text| parse_frontmatter(text))
                .and_then(|m| m.version),
            updated_at: updated_at.clone(),
        });
    }
    lock.save(&ctx.config_dir)?;
    // Only remotes that gained a skill are touched.
    let used: BTreeSet<usize> = vendored.iter().map(|v| v.plan).collect();
    edit_manifest(ctx, |skills| {
        for (i, plan) in plans.iter().enumerate().filter(|(i, _)| used.contains(i)) {
            if plan.is_new {
                let mut remote = plan.remote.clone();
                remote
                    .include
                    .retain(|n| vendored.iter().any(|v| &v.name == n && v.plan == i));
                add_remote_entry(skills, &remote)?;
            } else {
                extend_remote_include(skills, &plan.remote.name, &plan.names);
            }
        }
        Ok(())
    })?;
    if !vendored.is_empty() {
        if let Err(e) = cache.save(&ctx.state_dir) {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "skill_status_cache",
                format!("cannot record skill status: {e}"),
            ));
        }
    }
    let names: Vec<&str> = vendored
        .iter()
        .map(|v| v.name.as_str())
        .chain(own.iter().map(|(n, _)| n.as_str()))
        .collect();
    let summary = if single.is_some() && vendored.len() == 1 {
        format!(
            "feat(skills): subscribe to {} ({})",
            plans[vendored[0].plan].remote.name, vendored[0].name
        )
    } else {
        format!("feat(skills): import skill(s) {}", names.join(", "))
    };
    commit(ctx, actor.as_ref(), &summary)?;
    Ok(result_for(imported, skipped, false))
}

/// Default source id: the last URL segment without `.git`, as a valid name.
fn source_name_from_url(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".git");
    let mut name: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_matches('-').to_string();
    if name.is_empty() {
        "upstream".into()
    } else {
        name
    }
}

fn add_remote(
    ctx: &mut Context,
    params: &Params,
    manifest: &Manifest,
    actor: Option<&team::Actor>,
    url: &str,
    fetcher: &dyn UpstreamFetcher,
) -> Result<Value, MusterError> {
    let source_name = match params.get_string("source-name") {
        Some(name) => validate_skill_name(name)
            .map_err(|_| MusterError::Usage(format!("invalid source name '{name}'")))?,
        None => source_name_from_url(url),
    };
    if remotes(manifest).iter().any(|r| r.name == source_name) {
        return Err(MusterError::Usage(format!(
            "skill source '{source_name}' is already subscribed; pass --source-name to add another"
        )));
    }
    if source_name == lock::LOCAL {
        return Err(MusterError::Usage(
            "'local' is reserved for own skills".into(),
        ));
    }
    let path = params.get_string("path").map(str::to_owned);
    if let Some(p) = &path {
        relative(p, "--path")?;
    }
    let mut include: Vec<String> = Vec::new();
    for item in params.get_list("include") {
        for name in item.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let name = validate_skill_name(name)?;
            if !include.contains(&name) {
                include.push(name);
            }
        }
    }
    let remote = SkillRemote {
        name: source_name.clone(),
        url: url.to_string(),
        git_ref: params.get_string("ref").map(str::to_owned),
        path,
        include,
    };
    if let Some(reference) = &remote.git_ref {
        crate::adapters::config::validate_git_ref(reference).map_err(MusterError::Usage)?;
    }

    let checkout = fetcher.fetch(&remote)?;
    let available = checkout.skill_names(&remote)?;
    let wanted: Vec<String> = if remote.include.is_empty() {
        available.clone()
    } else {
        if let Some(missing) = remote.include.iter().find(|n| !available.contains(n)) {
            return Err(MusterError::Usage(format!(
                "skill '{missing}' is not in source '{source_name}' (available: {})",
                available.join(", ")
            )));
        }
        remote.include.clone()
    };
    let (dir, _) = skills_dir(ctx, manifest)?;
    let mut lock = SkillsLock::load(&ctx.config_dir)?;
    let mut vendored = Vec::new();
    for name in &wanted {
        if validate_skill_name(name).is_err() {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "skill_invalid",
                format!("upstream skill '{name}' has an invalid name; skipped"),
            ));
            continue;
        }
        if dir.join(name).exists() || lock.get(name).is_some() {
            return Err(MusterError::Usage(format!(
                "skill '{name}' already exists in the config repo; remove it first or narrow --include"
            )));
        }
        let skill = read_upstream(&checkout, &remote, name)?.ok_or_else(|| {
            MusterError::Operational(format!("cannot read upstream skill '{name}'"))
        })?;
        vendored.push((name.clone(), skill));
    }
    if vendored.is_empty() {
        return Err(MusterError::Usage(format!(
            "source '{source_name}' has no skills under {}",
            remote.path.as_deref().unwrap_or("its root")
        )));
    }
    let result = json!({
        "source": source_name,
        "url": url,
        "commit": checkout.commit,
        "skills": vendored.iter().map(|(n, s)| json!({ "name": n, "version": s.version })).collect::<Vec<_>>(),
    });
    if ctx.dry_run {
        return Ok(result);
    }

    let updated_at = now();
    let mut cache = StatusCache::load(&ctx.state_dir);
    for (name, skill) in &vendored {
        write_skill(&dir, name, &skill.files)?;
        lock.upsert(LockEntry {
            name: name.clone(),
            origin: source_name.clone(),
            commit: Some(checkout.commit.clone()),
            content_hash: skill.hash.clone(),
            version: skill.version.clone(),
            updated_at: updated_at.clone(),
        });
        let status = classify(false, &skill.hash, &skill.upstream());
        cache.skills.insert(
            name.clone(),
            CachedStatus {
                status,
                reason: skill.deprecated.clone(),
                upstream_commit: Some(checkout.commit.clone()),
                checked_at: updated_at.clone(),
            },
        );
        diag_for(ctx, name, status, skill.deprecated.as_deref());
    }
    lock.save(&ctx.config_dir)?;
    edit_manifest(ctx, |skills| add_remote_entry(skills, &remote))?;
    if let Err(e) = cache.save(&ctx.state_dir) {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "skill_status_cache",
            format!("cannot record skill status: {e}"),
        ));
    }
    let names: Vec<&str> = vendored.iter().map(|(n, _)| n.as_str()).collect();
    commit(
        ctx,
        actor,
        &format!(
            "feat(skills): subscribe to {source_name} ({})",
            names.join(", ")
        ),
    )?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// Fetch each source at most once.
struct Sources<'a> {
    remotes: Vec<SkillRemote>,
    fetcher: &'a dyn UpstreamFetcher,
    fetched: BTreeMap<String, Result<Checkout, String>>,
}

impl<'a> Sources<'a> {
    fn new(manifest: &Manifest, fetcher: &'a dyn UpstreamFetcher) -> Self {
        Self {
            remotes: remotes(manifest),
            fetcher,
            fetched: BTreeMap::new(),
        }
    }

    fn remote(&self, origin: &str) -> Option<&SkillRemote> {
        self.remotes.iter().find(|r| r.name == origin)
    }

    /// The checkout of `origin`, or why it is unavailable.
    fn checkout(&mut self, origin: &str) -> Result<(SkillRemote, Checkout), String> {
        let remote = self
            .remote(origin)
            .cloned()
            .ok_or_else(|| format!("source '{origin}' is no longer declared in muster.toml"))?;
        let fetcher = self.fetcher;
        let fetched = self
            .fetched
            .entry(origin.to_string())
            .or_insert_with(|| fetcher.fetch(&remote).map_err(|e| e.to_string()));
        fetched.clone().map(|c| (remote, c))
    }
}

fn check(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let fetcher = GitFetcher::from_env()?;
    check_with(ctx, params, &fetcher)
}

fn check_with(
    ctx: &mut Context,
    params: &Params,
    fetcher: &dyn UpstreamFetcher,
) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let lock = SkillsLock::load(&ctx.config_dir)?;
    let only = match params.get_string("name") {
        Some(name) => {
            let name = validate_skill_name(name)?;
            if lock.get(&name).is_none() {
                return Err(MusterError::Usage(format!(
                    "skill '{name}' is not in skills.lock"
                )));
            }
            Some(name)
        }
        None => None,
    };
    let mut sources = Sources::new(&manifest, fetcher);
    let mut cache = StatusCache::load(&ctx.state_dir);
    let checked_at = now();
    let mut reported_unreachable = BTreeSet::new();
    let mut skills = Vec::new();
    for entry in &lock.skills {
        if only.as_ref().is_some_and(|n| n != &entry.name) {
            continue;
        }
        if entry.is_local() {
            skills.push(json!({ "name": entry.name, "origin": entry.origin, "status": "local" }));
            continue;
        }
        let (upstream, skill, commit) = match sources.checkout(&entry.origin) {
            Ok((remote, checkout)) => match read_upstream(&checkout, &remote, &entry.name) {
                Ok(Some(skill)) => (skill.upstream(), Some(skill), Some(checkout.commit)),
                Ok(None) => (Upstream::Missing, None, Some(checkout.commit)),
                Err(e) => {
                    if reported_unreachable.insert(entry.origin.clone()) {
                        unreachable(ctx, &entry.origin, &e.to_string());
                    }
                    (Upstream::Unreachable, None, None)
                }
            },
            Err(e) => {
                if reported_unreachable.insert(entry.origin.clone()) {
                    unreachable(ctx, &entry.origin, &e);
                }
                (Upstream::Unreachable, None, None)
            }
        };
        let status = classify(false, &entry.content_hash, &upstream);
        let reason = skill.as_ref().and_then(|s| s.deprecated.clone());
        diag_for(ctx, &entry.name, status, reason.as_deref());
        let last_known = cache.skills.get(&entry.name).map(|c| c.status.as_str());
        let mut view = json!({
            "name": entry.name,
            "origin": entry.origin,
            "status": status.as_str(),
            "reason": reason,
            "commit": entry.commit,
            "upstream_commit": commit,
            "upstream_version": skill.as_ref().and_then(|s| s.version.clone()),
        });
        if status == SkillStatus::Unknown {
            // Keep the last known status in the cache; report it alongside.
            view["last_known"] = json!(last_known);
        } else {
            cache.skills.insert(
                entry.name.clone(),
                CachedStatus {
                    status,
                    reason,
                    upstream_commit: commit,
                    checked_at: checked_at.clone(),
                },
            );
        }
        skills.push(view);
    }
    cache.checked_at = Some(checked_at.clone());
    cache.skills.retain(|name, _| lock.get(name).is_some());
    cache.save(&ctx.state_dir)?;
    Ok(json!({ "checked_at": checked_at, "skills": skills }))
}

fn unreachable(ctx: &mut Context, origin: &str, why: &str) {
    ctx.diag(
        Diagnostic::new(
            Severity::Warning,
            "skill_source_unreachable",
            format!("cannot check skill source '{origin}': {why}"),
        )
        .with_suggestion("check the URL and network access; the last known status is kept"),
    );
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

struct Planned {
    name: String,
    origin: String,
    commit: String,
    skill: UpstreamSkill,
    files: Value,
    forced: bool,
}

fn update(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let fetcher = GitFetcher::from_env()?;
    update_with(ctx, params, &fetcher)
}

fn update_with(
    ctx: &mut Context,
    params: &Params,
    fetcher: &dyn UpstreamFetcher,
) -> Result<Value, MusterError> {
    update_inner(ctx, params, fetcher, false)
}

/// `unattended`: the caller (sync) already decided to apply, so no
/// confirmation is asked. This is NOT the global `--yes`.
fn update_inner(
    ctx: &mut Context,
    params: &Params,
    fetcher: &dyn UpstreamFetcher,
    unattended: bool,
) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = admin(ctx, &manifest)?;
    let all = params.get_bool("all");
    let force = params.get_bool("force");
    let named = params
        .get_string("name")
        .map(validate_skill_name)
        .transpose()?;
    if named.is_some() == all {
        return Err(MusterError::Usage(
            "pass a skill name or --all (not both)".into(),
        ));
    }
    let (dir, _) = skills_dir(ctx, &manifest)?;
    let mut lock = SkillsLock::load(&ctx.config_dir)?;
    let targets: Vec<LockEntry> = match &named {
        Some(name) => {
            let entry = lock.get(name).ok_or_else(|| {
                MusterError::Usage(format!("skill '{name}' is not in skills.lock"))
            })?;
            if entry.is_local() {
                return Err(MusterError::Usage(format!(
                    "skill '{name}' is an own skill; edit it in place"
                )));
            }
            vec![entry.clone()]
        }
        None => lock
            .skills
            .iter()
            .filter(|e| !e.is_local())
            .cloned()
            .collect(),
    };

    let mut sources = Sources::new(&manifest, fetcher);
    let mut planned = Vec::new();
    let mut skipped = Vec::new();
    for entry in targets {
        let skill_dir = contained(&dir, &entry.name, None)?;
        let refuse = |ctx: &mut Context, code: &str, message: String| -> Result<(), MusterError> {
            if named.is_some() {
                return Err(MusterError::Usage(message));
            }
            ctx.diag(Diagnostic::new(Severity::Warning, code, message));
            Ok(())
        };
        let (remote, checkout) = match sources.checkout(&entry.origin) {
            Ok(found) => found,
            Err(e) => {
                if named.is_some() {
                    return Err(MusterError::Operational(format!(
                        "cannot update skill '{}': {e}",
                        entry.name
                    )));
                }
                unreachable(ctx, &entry.origin, &e);
                skipped.push(json!({ "name": entry.name, "reason": "unreachable" }));
                continue;
            }
        };
        let Some(skill) = read_upstream(&checkout, &remote, &entry.name)? else {
            refuse(
                ctx,
                "skill_upstream_removed",
                format!(
                    "skill '{}' was removed from source '{}'; nothing to update to. Keep it with \
                     `muster skill rm {} --detach --yes` or delete it with `muster skill rm {} --yes`",
                    entry.name, entry.origin, entry.name, entry.name
                ),
            )?;
            skipped.push(json!({ "name": entry.name, "reason": "upstream_removed" }));
            continue;
        };
        let disk = read_skill_files(&skill_dir)?;
        let modified = content_hash(&disk) != entry.content_hash;
        if skill.hash == content_hash(&disk)
            && skill.hash == entry.content_hash
            && entry.commit.as_deref() == Some(&checkout.commit)
        {
            continue;
        }
        if skill.hash == entry.content_hash && !modified {
            // Same content at a newer commit: nothing to apply, nothing to bump.
            continue;
        }
        if modified && skill.hash != content_hash(&disk) && !force {
            refuse(
                ctx,
                "skill_modified",
                format!(
                    "skill '{}' has local modifications that the update would overwrite; \
                     re-run with --force to discard them",
                    entry.name
                ),
            )?;
            skipped.push(json!({ "name": entry.name, "reason": "modified" }));
            continue;
        }
        planned.push(Planned {
            files: diff_summary(&disk, &skill.files),
            name: entry.name.clone(),
            origin: entry.origin.clone(),
            commit: checkout.commit.clone(),
            skill,
            forced: modified && force,
        });
    }

    let updated: Vec<Value> = planned
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "origin": p.origin,
                "commit": p.commit,
                "version": p.skill.version,
                "deprecated": p.skill.deprecated,
                "files": p.files,
                "forced": p.forced,
            })
        })
        .collect();
    let result = json!({ "updated": updated, "skipped": skipped, "dry_run": ctx.dry_run });
    if planned.is_empty() || ctx.dry_run {
        return Ok(result);
    }
    let summary: Vec<String> = planned
        .iter()
        .map(|p| {
            let count = |k: &str| p.files[k].as_array().map_or(0, Vec::len);
            format!(
                "{} (+{} ~{} -{} files)",
                p.name,
                count("added"),
                count("changed"),
                count("removed")
            )
        })
        .collect();
    if !unattended {
        ctx.confirm(&format!("Update skill(s) {}?", summary.join(", ")))?;
    }

    let updated_at = now();
    let mut cache = StatusCache::load(&ctx.state_dir);
    for p in &planned {
        let skill_dir = contained(&dir, &p.name, None)?;
        if content_hash(&read_skill_files(&skill_dir)?) != p.skill.hash {
            write_skill(&dir, &p.name, &p.skill.files)?;
        }
        lock.upsert(LockEntry {
            name: p.name.clone(),
            origin: p.origin.clone(),
            commit: Some(p.commit.clone()),
            content_hash: p.skill.hash.clone(),
            version: p.skill.version.clone(),
            updated_at: updated_at.clone(),
        });
        let status = classify(false, &p.skill.hash, &p.skill.upstream());
        cache.skills.insert(
            p.name.clone(),
            CachedStatus {
                status,
                reason: p.skill.deprecated.clone(),
                upstream_commit: Some(p.commit.clone()),
                checked_at: updated_at.clone(),
            },
        );
    }
    lock.save(&ctx.config_dir)?;
    if let Err(e) = cache.save(&ctx.state_dir) {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "skill_status_cache",
            format!("cannot record skill status: {e}"),
        ));
    }
    let names: Vec<&str> = planned.iter().map(|p| p.name.as_str()).collect();
    commit(
        ctx,
        actor.as_ref(),
        &format!("feat(skills): update {}", names.join(", ")),
    )?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// auto-update
// ---------------------------------------------------------------------------

fn auto_update_cmd(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let raw = params
        .get_string("enabled")
        .or_else(|| params.get_string("state"))
        .ok_or_else(|| {
            MusterError::Usage("state must be `on` or `off` (or --enabled true|false)".into())
        })?;
    let enabled = match raw.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" | "enable" | "enabled" => true,
        "off" | "false" | "no" | "0" | "disable" | "disabled" => false,
        _ => return Err(MusterError::Usage("state must be `on` or `off`".into())),
    };
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = admin(ctx, &manifest)?;
    let result = json!({ "auto_update": enabled, "dry_run": ctx.dry_run });
    if ctx.dry_run {
        return Ok(result);
    }
    if enabled {
        ctx.confirm(
            "Apply upstream updates of subscribed skills automatically during `muster sync`?",
        )?;
    }
    edit_manifest(ctx, |skills| {
        if enabled {
            skills.insert("auto_update", toml_edit::value(true));
        } else {
            skills.remove("auto_update");
        }
        Ok(())
    })?;
    commit(
        ctx,
        actor.as_ref(),
        &format!(
            "feat(skills): turn skill auto-update {}",
            if enabled { "on" } else { "off" }
        ),
    )?;
    Ok(result)
}

/// Unattended skill updates for `muster sync`: when `auto_update` is on, check
/// and update every subscribed skill without confirmation. Locally modified
/// skills are skipped (never forced), a non-admin team member does nothing,
/// and any failure (network, git) is a warning: sync carries on. Returns the
/// names of the skills that were updated.
pub fn auto_update_in_sync(ctx: &mut Context) -> Vec<Value> {
    let fetcher = match GitFetcher::from_env() {
        Ok(fetcher) => fetcher,
        Err(e) => {
            auto_update_warning(ctx, &e.to_string());
            return Vec::new();
        }
    };
    match auto_update_with(ctx, &fetcher) {
        Ok(updated) => updated,
        Err(e) => {
            auto_update_warning(ctx, &e.to_string());
            Vec::new()
        }
    }
}

fn auto_update_warning(ctx: &mut Context, why: &str) {
    ctx.diag(Diagnostic::new(
        Severity::Warning,
        "skill_auto_update_failed",
        format!("skill auto-update did not complete: {why}"),
    ));
}

fn auto_update_with(
    ctx: &mut Context,
    fetcher: &dyn UpstreamFetcher,
) -> Result<Vec<Value>, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let layers = crate::engine::layer::global_layers(
        &ctx.config_dir,
        &manifest,
        crate::machine::load_name(&ctx.state_dir).as_deref(),
    );
    let merged = crate::engine::layer::merge(&layers);
    if !merged.agents.skills.as_ref().is_some_and(|s| s.auto_update) {
        return Ok(Vec::new());
    }
    if !SkillsLock::load(&ctx.config_dir)?
        .skills
        .iter()
        .any(|e| !e.is_local())
    {
        return Ok(Vec::new());
    }
    if let Err(e) = admin(ctx, &manifest) {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "skill_auto_update_skipped",
            format!("skill auto-update skipped: {e}"),
        ));
        return Ok(Vec::new());
    }
    check_with(ctx, &Params::default(), fetcher)?;
    let mut params = Params::default();
    params.set("all", Value::Bool(true));
    let result = update_inner(ctx, &params, fetcher, true)?;
    Ok(result["updated"]
        .as_array()
        .map(|items| items.iter().map(|u| u["name"].clone()).collect())
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

fn rm(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = required_name(params)?;
    let detach = params.get_bool("detach");
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = admin(ctx, &manifest)?;
    let (dir, _) = skills_dir(ctx, &manifest)?;
    let skill_dir = dir.join(&name);
    let mut lock = SkillsLock::load(&ctx.config_dir)?;
    let entry = lock.get(&name).cloned();
    if entry.is_none() && !skill_dir.exists() {
        return Err(MusterError::Usage(format!("skill '{name}' does not exist")));
    }
    let origin = entry
        .as_ref()
        .map_or(lock::LOCAL.to_string(), |e| e.origin.clone());
    let external = origin != lock::LOCAL;
    if detach && !external {
        return Err(MusterError::Usage(format!(
            "skill '{name}' is already an own skill; --detach only applies to subscribed skills"
        )));
    }
    let source_unused = external
        && !lock
            .skills
            .iter()
            .any(|e| e.origin == origin && e.name != name);

    if detach {
        ctx.confirm(&format!(
            "Keep skill '{name}' as an own skill and stop following '{origin}'?"
        ))?;
        let result = json!({ "detached": name, "origin": origin, "source_removed": source_unused });
        if ctx.dry_run {
            return Ok(result);
        }
        let files = read_skill_files(&skill_dir)?;
        lock.upsert(LockEntry {
            name: name.clone(),
            origin: lock::LOCAL.into(),
            commit: None,
            content_hash: content_hash(&files),
            version: entry.and_then(|e| e.version),
            updated_at: now(),
        });
        lock.save(&ctx.config_dir)?;
        edit_manifest(ctx, |skills| {
            forget_remote_skill(skills, &origin, &name, source_unused);
            Ok(())
        })?;
        forget_status(ctx, &name);
        commit(
            ctx,
            actor.as_ref(),
            &format!("feat(skills): keep {name} as an own skill (detached from {origin})"),
        )?;
        return Ok(result);
    }

    ctx.confirm(&format!("Delete skill '{name}' and its files?"))?;
    let result = json!({ "removed": name, "origin": origin, "source_removed": source_unused });
    if ctx.dry_run {
        return Ok(result);
    }
    if skill_dir.exists() {
        std::fs::remove_dir_all(&skill_dir).map_err(|e| io_error(&skill_dir, e))?;
    }
    if lock.remove(&name).is_some() {
        lock.save(&ctx.config_dir)?;
    }
    if external {
        edit_manifest(ctx, |skills| {
            forget_remote_skill(skills, &origin, &name, source_unused);
            Ok(())
        })?;
    }
    forget_status(ctx, &name);
    commit(
        ctx,
        actor.as_ref(),
        &format!("feat(skills): remove skill {name}"),
    )?;
    Ok(result)
}

fn forget_status(ctx: &mut Context, name: &str) {
    let mut cache = StatusCache::load(&ctx.state_dir);
    if cache.skills.remove(name).is_some() {
        let _ = cache.save(&ctx.state_dir);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn contained_skill_file_prevents_escape_and_symlinks() {
        let root = tempfile::TempDir::new().unwrap();
        let skills = root.path().join("skills");
        let my_skill = skills.join("test-skill");
        std::fs::create_dir_all(&my_skill).unwrap();
        std::fs::write(my_skill.join("SKILL.md"), "name: test-skill").unwrap();

        // Safe relative path works
        assert!(contained_skill_file(&skills, "test-skill", "SKILL.md").is_ok());
        assert!(contained_skill_file(&skills, "test-skill", "sub/file.txt").is_ok());

        // Traversal attempts fail
        assert!(contained_skill_file(&skills, "test-skill", "../other.txt").is_err());
        assert!(contained_skill_file(&skills, "test-skill", "/etc/passwd").is_err());
        assert!(contained_skill_file(&skills, "test-skill", "sub/../../escape.txt").is_err());

        // Symlink pointing outside escapes
        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        #[cfg(unix)]
        {
            let sym = my_skill.join("link_outside");
            std::os::unix::fs::symlink(&outside, &sym).unwrap();
            assert!(contained_skill_file(&skills, "test-skill", "link_outside").is_err());
        }
    }
    use super::*;

    #[cfg(unix)]
    #[test]
    fn contained_skill_file_rejects_symlinked_skill_dirs_and_dangling_links() {
        let root = tempfile::TempDir::new().unwrap();
        let skills = root.path().join("skills");
        let skill = skills.join("my-skill");
        std::fs::create_dir_all(&skill).unwrap();
        // A skill directory that is a symlink to `/` must not make everything pass.
        std::os::unix::fs::symlink("/", skills.join("esc")).unwrap();
        for rel in ["etc/passwd", "nonexistent-file"] {
            let err = contained_skill_file(&skills, "esc", rel).unwrap_err();
            assert!(matches!(err, MusterError::Usage(_)), "{rel}: {err:?}");
        }
        // A dangling leaf symlink is refused too.
        std::os::unix::fs::symlink("/nonexistent", skill.join("dangling")).unwrap();
        let err = contained_skill_file(&skills, "my-skill", "dangling").unwrap_err();
        assert!(matches!(err, MusterError::Usage(_)), "{err:?}");
        // An intermediate directory symlink is refused.
        std::os::unix::fs::symlink("/", skill.join("dir")).unwrap();
        let err = contained_skill_file(&skills, "my-skill", "dir/etc/passwd").unwrap_err();
        assert!(matches!(err, MusterError::Usage(_)), "{err:?}");
        // Traversal to a nonexistent target is a usage error, not an I/O error.
        let err = contained_skill_file(&skills, "my-skill", "../../nope").unwrap_err();
        assert!(matches!(err, MusterError::Usage(_)), "{err:?}");
    }

    #[test]
    fn write_skill_refuses_paths_outside_the_skills_dir() {
        let root = tempfile::TempDir::new().unwrap();
        let skills = root.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let ok = BTreeMap::from([("refs/a.md".to_string(), "x".to_string())]);
        for name in ["../escape", "a/b", "..", ""] {
            assert!(write_skill(&skills, name, &ok).is_err(), "{name:?}");
        }
        for rel in ["../../escape.md", "/etc/passwd", "a/../../b", "a\\b", ""] {
            let files = BTreeMap::from([(rel.to_string(), "x".to_string())]);
            assert!(write_skill(&skills, "good", &files).is_err(), "{rel:?}");
        }
        assert!(!root.path().join("escape.md").exists());
        // A rejected file set leaves an existing skill untouched.
        assert!(!skills.join("good").exists());
        write_skill(&skills, "good", &ok).unwrap();
        assert_eq!(
            std::fs::read_to_string(skills.join("good/refs/a.md")).unwrap(),
            "x"
        );
    }

    #[test]
    fn source_name_defaults_from_url() {
        assert_eq!(
            source_name_from_url("https://github.com/anthropics/skills.git"),
            "skills"
        );
        assert_eq!(
            source_name_from_url("git@github.com:org/My_Skills"),
            "my-skills"
        );
        assert_eq!(source_name_from_url("file:///tmp/.tmpAbC"), "tmpabc");
        assert_eq!(source_name_from_url(""), "upstream");
    }

    #[test]
    fn diff_summary_lists_added_changed_removed() {
        let old = BTreeMap::from([
            ("SKILL.md".to_string(), "a\n".to_string()),
            ("gone.md".to_string(), "x".to_string()),
            ("same.md".to_string(), "s\r\n".to_string()),
        ]);
        let new = BTreeMap::from([
            ("SKILL.md".to_string(), "b\n".to_string()),
            ("new.md".to_string(), "n".to_string()),
            ("same.md".to_string(), "s\n".to_string()),
        ]);
        assert_eq!(
            diff_summary(&old, &new),
            json!({ "added": ["new.md"], "changed": ["SKILL.md"], "removed": ["gone.md"] })
        );
    }

    #[test]
    fn forget_remote_skill_narrows_include_then_drops_source() {
        let mut doc: DocumentMut =
            "[[remote]]\nname = \"a\"\nurl = \"u\"\ninclude = [\"x\", \"y\"]\n"
                .parse()
                .unwrap();
        forget_remote_skill(doc.as_table_mut(), "a", "x", false);
        assert!(doc.to_string().contains("include = [\"y\"]"), "{doc}");
        forget_remote_skill(doc.as_table_mut(), "a", "y", true);
        assert!(!doc.to_string().contains("remote"), "{doc}");
    }
}
