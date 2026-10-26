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

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Skill names follow the Agent Skills convention: lowercase, digits, '-'.
fn validate_skill_name(name: &str) -> Result<String, MusterError> {
    let valid = !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
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

/// Replace `dir` with exactly `files`.
fn write_skill(dir: &Path, files: &BTreeMap<String, String>) -> Result<(), MusterError> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| io_error(dir, e))?;
    }
    for (rel, contents) in files {
        let path = dir.join(rel);
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
    let names: BTreeSet<&String> = disk
        .keys()
        .chain(lock.skills.iter().map(|e| &e.name))
        .collect();
    let skills: Vec<Value> = names
        .into_iter()
        .map(|name| skill_view(name, lock.get(name), disk.get(name), &cache))
        .collect();
    let attention = skills
        .iter()
        .filter(|s| s["needs_attention"] == true)
        .count();
    let sources: Vec<Value> = remotes(&manifest)
        .iter()
        .map(|r| json!({ "name": r.name, "url": r.url, "ref": r.git_ref, "path": r.path, "include": r.include }))
        .collect();
    Ok(json!({
        "source": source,
        "skills": skills,
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
    write_skill(&skill_dir, &files)?;
    lock.upsert(entry);
    lock.save(&ctx.config_dir)?;
    edit_manifest(ctx, |_| Ok(()))?;
    commit(ctx, actor, &format!("feat(skills): add skill {name}"))?;
    Ok(result)
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
        write_skill(&dir.join(name), &skill.files)?;
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
        let disk = read_skill_files(&dir.join(&entry.name))?;
        let modified = content_hash(&disk) != entry.content_hash;
        if skill.hash == content_hash(&disk) && entry.commit.as_deref() == Some(&checkout.commit) {
            continue;
        }
        if skill.hash == entry.content_hash && !modified {
            // Same content at a newer commit: nothing to apply, nothing to bump.
            continue;
        }
        if modified && !force {
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
            forced: modified,
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
    ctx.confirm(&format!("Update skill(s) {}?", summary.join(", ")))?;

    let updated_at = now();
    let mut cache = StatusCache::load(&ctx.state_dir);
    for p in &planned {
        write_skill(&dir.join(&p.name), &p.skill.files)?;
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
    use super::*;

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
