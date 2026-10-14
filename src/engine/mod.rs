//! The apply engine: desired state -> plan -> apply, with ownership records,
//! backups, numbered generations, rollback and drift detection.
//!
//! Interface (everything else is private):
//! - [`plan`]: merge the configuration layers, resolve secrets, render every
//!   adapter (pure) against the current files, and classify each managed file
//!   as add / modify / remove / unchanged. Never writes.
//! - [`apply`]: gate new hooks/MCP servers behind approval, back up and write
//!   the files atomically, and record a generation. Honours `--dry-run`.
//! - [`rollback`]: restore the exact bytes of generation N (recorded as a new
//!   generation).
//!
//! Plans hold resolved secrets in memory only; their JSON ([`Plan::to_json`])
//! redacts every value as `<redacted:REF>` and hides removed lines of files
//! that hold secrets.

mod diff;
pub mod dotfiles;
pub mod layer;
mod ownership;
mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::adapters::config::{AgentsConfig, McpServer, Transport};
use crate::adapters::{self, Adapter, Base, Capability, Current, Desired, FileFormat, ManagedFile};
use crate::context::Context;
use crate::errors::MusterError;
use crate::manifest::{self, Manifest};
use crate::output::{Diagnostic, Severity};
use crate::providers::Providers;
use crate::secrets::{self, SecretValue};
use store::{Approved, Record, State};

/// Where the rendered files go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// User-scope files under `$HOME` / `$XDG_CONFIG_HOME`.
    Global,
    /// Project-scope files inside this directory (layer in `<dir>/.muster/`).
    Project(PathBuf),
}

impl Target {
    /// Key used in ownership records and approvals.
    fn key(&self) -> String {
        match self {
            Self::Global => "global".into(),
            Self::Project(dir) => dir.display().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Add,
    Modify,
    Remove,
    Unchanged,
}

/// One managed file in a plan. `before`/`after` may hold resolved secrets.
struct Entry {
    path: PathBuf,
    agents: Vec<&'static str>,
    format: FileFormat,
    before: Option<String>,
    after: Option<String>,
    /// Still rendered by an adapter (false: only pruned, no longer managed).
    managed: bool,
    owned: Vec<Vec<String>>,
    private: bool,
    /// Written executable (managed dotfiles only).
    executable: bool,
    created: bool,
    drift: bool,
}

impl Entry {
    fn action(&self) -> Action {
        match (&self.before, &self.after) {
            (None, Some(_)) => Action::Add,
            (Some(_), None) => Action::Remove,
            (Some(b), Some(a)) if b != a => Action::Modify,
            _ => Action::Unchanged,
        }
    }
}

/// A hook or MCP server that is new (or changed) since the last approval.
struct Approval {
    kind: &'static str,
    name: String,
    detail: String,
}

/// The planned change set. Not `Debug`: entries hold resolved secrets.
pub struct Plan {
    target: Target,
    entries: Vec<Entry>,
    /// Adapter id -> failure message (None = rendered fine).
    agents: BTreeMap<&'static str, Option<String>>,
    approvals: Vec<Approval>,
    /// Approvals for this target once the plan is applied.
    approved: Approved,
    base: Option<(u64, State)>,
    secrets: BTreeMap<String, SecretValue>,
    layers: Vec<layer::Layer>,
}

/// `muster plan` / `sync` / `agents apply` options.
pub struct Options<'a> {
    pub target: Target,
    /// Restrict to one adapter (`agents apply --agent`); stale entries of
    /// other adapters are left alone.
    pub agent: Option<&'a str>,
}

/// Compute the plan. Never writes. Per-adapter failures become error
/// diagnostics; the other adapters are still planned.
pub fn plan(ctx: &mut Context, options: &Options) -> Result<Plan, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let layers = match &options.target {
        Target::Global => layer::global_layers(
            &ctx.config_dir,
            &manifest,
            crate::machine::load_name(&ctx.state_dir).as_deref(),
        ),
        Target::Project(dir) => layer::project_layers(dir)?,
    };
    // The single place layers become desired state.
    let mut merged = layer::merge(&layers);
    if merged.agents.enabled.is_none() {
        merged.agents.enabled = manifest.agents.as_ref().and_then(|a| a.enabled.clone());
    }
    for (name, server) in &merged.mcp {
        server
            .transport()
            .map_err(|message| MusterError::InvalidConfig {
                message: format!("MCP server `{name}` {message}"),
                path: Some(format!("{}: mcp.{name}", manifest::FILE_NAME)),
            })?;
    }
    let selected = match options.agent {
        Some(id) => vec![single_adapter(id)?],
        None => enabled_adapters(Some(&merged.agents))?,
    };
    let instructions = read_instructions(ctx, &merged)?;
    let secrets = resolve_secrets(ctx, &merged.mcp);
    let desired = Desired {
        agents: &merged.agents,
        mcp: &merged.mcp,
        instructions: instructions.as_deref(),
        secrets: &secrets,
    };

    let roots = Roots::from_env()?;
    let base = store::latest(&ctx.state_dir)?;
    let key = options.target.key();
    let empty = BTreeMap::new();
    let records = base.as_ref().map_or(&empty, |(_, s)| &s.files);
    let ours = |path: &Path| {
        records
            .get(&path.display().to_string())
            .filter(|r| r.target == key)
    };

    let mut entries: BTreeMap<PathBuf, Entry> = BTreeMap::new();
    let mut agents = BTreeMap::new();
    for adapter in &selected {
        let ctx_target = &options.target;
        match plan_adapter(adapter, &desired, &roots, ctx_target, &ours, &entries) {
            Ok((planned, diagnostics)) => {
                for d in diagnostics {
                    ctx.diag(d);
                }
                for entry in planned {
                    merge_entry(&mut entries, entry);
                }
                agents.insert(adapter.id, None);
            }
            Err(err) => {
                ctx.diag(adapter_failure(adapter.id, adapter.name, &err.to_string()));
                agents.insert(adapter.id, Some(err.to_string()));
            }
        }
    }

    // Managed dotfiles: user scope only, and not when one adapter is selected.
    if options.target == Target::Global && options.agent.is_none() {
        dotfiles::plan(ctx, &manifest, &roots, &ours, records, &key, &mut entries)?;
    }

    // Adapters no longer enabled: prune what they own in this target.
    if options.agent.is_none() {
        for (path, record) in records {
            let path = PathBuf::from(path);
            if record.target != key
                || entries.contains_key(&path)
                || selected.iter().any(|a| a.id == record.agent)
            {
                continue;
            }
            let Some(agent) = adapters::find(&record.agent).map(|a| a.id) else {
                continue;
            };
            match read_optional(&path) {
                Ok(before) => {
                    if let Some(entry) = unmanaged_entry(agent, &path, before, record) {
                        merge_entry(&mut entries, entry);
                    }
                    agents.entry(agent).or_insert(None);
                }
                Err(err) => {
                    ctx.diag(adapter_failure(agent, agent, &err.to_string()));
                    agents.insert(agent, Some(err.to_string()));
                }
            }
        }
    }

    for entry in entries.values_mut() {
        entry.private |= holds_secret(entry.after.as_deref(), &secrets);
        if entry.drift {
            ctx.diag(
                Diagnostic::new(
                    Severity::Warning,
                    "drift",
                    format!(
                        "{} changed outside Muster since the last apply; managed keys are \
                         restored, other edits are kept",
                        entry.path.display()
                    ),
                )
                .with_path(entry.path.display().to_string()),
            );
        }
    }

    let previous = base
        .as_ref()
        .and_then(|(_, s)| s.approved.get(&key))
        .cloned()
        .unwrap_or_default();
    let (approvals, approved) = approvals(&selected, &merged, previous);

    Ok(Plan {
        target: options.target.clone(),
        entries: entries.into_values().collect(),
        agents,
        approvals,
        approved,
        base,
        secrets,
        layers,
    })
}

type Ours<'r> = dyn Fn(&Path) -> Option<&'r Record> + 'r;

/// Plan one adapter: prune its stale keys, render, and build its entries.
/// All-or-nothing: any error fails only this adapter.
fn plan_adapter(
    adapter: &'static Adapter,
    desired: &Desired,
    roots: &Roots,
    target: &Target,
    ours: &Ours<'_>,
    pending: &BTreeMap<PathBuf, Entry>,
) -> Result<(Vec<Entry>, Vec<Diagnostic>), MusterError> {
    // What the adapter owns today = what it writes into an empty file.
    let owned_now: BTreeMap<&str, Vec<Vec<String>>> = adapter
        .render(desired, &Current::new())?
        .files
        .iter()
        .map(|f| {
            (
                f.file.path,
                ownership::owned_keys(f.file.format, &f.contents),
            )
        })
        .collect();

    let mut files: Vec<(
        &'static ManagedFile,
        PathBuf,
        Option<String>,
        Option<String>,
    )> = Vec::new();
    let mut current = Current::new();
    for file in adapter.files {
        let Some(path) = roots.locate(file, target) else {
            continue;
        };
        // A file another adapter already planned (shared project AGENTS.md)
        // continues from that adapter's output.
        let (before, text) = match pending.get(&path) {
            Some(entry) => (entry.before.clone(), entry.after.clone()),
            None => {
                let text = read_optional(&path)?;
                (text.clone(), text)
            }
        };
        let text = match (text, ours(&path)) {
            (Some(text), Some(record)) => {
                let now = owned_now.get(file.path).cloned().unwrap_or_default();
                let stale: Vec<Vec<String>> = record
                    .owned
                    .iter()
                    .filter(|k| !now.contains(k))
                    .cloned()
                    .collect();
                Some(ownership::prune(file.format, &text, &stale))
            }
            (text, _) => text,
        };
        if let Some(text) = &text {
            current.insert(file.path, text.clone());
        }
        files.push((file, path, before, text));
    }

    let rendered = adapter.render(desired, &current)?;
    let mut entries = Vec::new();
    for (file, path, before, pruned) in files {
        let record = ours(&path);
        let created = record.map_or(before.is_none(), |r| r.created);
        let drift = record.is_some_and(|r| before.as_deref() != Some(r.contents.as_str()));
        let output = rendered.files.iter().find(|r| r.file.path == file.path);
        let (after, managed) = match output {
            Some(out) => (Some(out.contents.clone()), true),
            None if record.is_none() => continue,
            None => match pruned {
                Some(text) if created && ownership::is_empty(file.format, &text) => (None, false),
                other => (other, false),
            },
        };
        entries.push(Entry {
            path,
            agents: vec![adapter.id],
            format: file.format,
            before,
            after,
            managed,
            owned: if managed {
                owned_now.get(file.path).cloned().unwrap_or_default()
            } else {
                Vec::new()
            },
            private: record.is_some_and(|r| r.private),
            executable: false,
            created,
            drift,
        });
    }
    Ok((entries, rendered.diagnostics))
}

/// Entry for a file whose adapter is no longer enabled: drop its owned keys.
fn unmanaged_entry(
    agent: &'static str,
    path: &Path,
    before: Option<String>,
    record: &Record,
) -> Option<Entry> {
    let text = before.as_ref()?;
    let pruned = ownership::prune(record.format, text, &record.owned);
    let after = if record.created && ownership::is_empty(record.format, &pruned) {
        None
    } else {
        Some(pruned)
    };
    Some(Entry {
        path: path.to_path_buf(),
        agents: vec![agent],
        format: record.format,
        drift: before.as_deref() != Some(record.contents.as_str()),
        before,
        after,
        managed: false,
        owned: Vec::new(),
        private: record.private,
        executable: false,
        created: record.created,
    })
}

fn merge_entry(entries: &mut BTreeMap<PathBuf, Entry>, entry: Entry) {
    match entries.get_mut(&entry.path) {
        Some(existing) => {
            existing.after = entry.after;
            existing.managed |= entry.managed;
            for key in entry.owned {
                if !existing.owned.contains(&key) {
                    existing.owned.push(key);
                }
            }
            existing.private |= entry.private;
            existing.agents.extend(entry.agents);
        }
        None => {
            entries.insert(entry.path.clone(), entry);
        }
    }
}

/// New or changed hooks / MCP servers since the last approval for this target.
/// Only kinds some selected adapter can install are gated (and recorded).
fn approvals(
    selected: &[&'static Adapter],
    merged: &layer::Merged,
    previous: Approved,
) -> (Vec<Approval>, Approved) {
    let supports = |caps: &[Capability]| {
        selected
            .iter()
            .any(|a| caps.iter().any(|c| a.capabilities.contains(c)))
    };
    let mut approvals = Vec::new();
    let mut next = previous.clone();
    if supports(&[Capability::Hooks]) {
        next.hooks = BTreeMap::new();
        for (event, hook) in &merged.agents.hooks {
            let value = serde_json::to_value(hook).expect("hook serializes");
            if previous.hooks.get(event) != Some(&value) {
                approvals.push(Approval {
                    kind: "hook",
                    name: event.clone(),
                    detail: hook.command.clone(),
                });
            }
            next.hooks.insert(event.clone(), value);
        }
    }
    if supports(&[Capability::McpStdio, Capability::McpRemote]) {
        next.mcp = BTreeMap::new();
        for (name, server) in &merged.mcp {
            let value = serde_json::to_value(server).expect("server serializes");
            if previous.mcp.get(name) != Some(&value) {
                let detail = match server.transport() {
                    Ok(Transport::Stdio { command }) => std::iter::once(command.to_string())
                        .chain(server.args.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" "),
                    Ok(Transport::Remote { url }) => url.to_string(),
                    Err(_) => String::new(),
                };
                approvals.push(Approval {
                    kind: "mcp_server",
                    name: name.clone(),
                    detail,
                });
            }
            next.mcp.insert(name.clone(), value);
        }
    }
    (approvals, next)
}

impl Plan {
    pub fn has_changes(&self) -> bool {
        self.entries.iter().any(|e| e.action() != Action::Unchanged)
    }

    /// JSON for CLI/MCP/web. Secret values are always redacted.
    ///
    /// ```json
    /// {"target": "global", "dry_run": true, "applied": false, "generation": 3,
    ///  "changes": 1,
    ///  "agents": [{"id": "claude", "status": "planned|applied|unchanged|failed",
    ///              "files": [{"path", "action": "add|modify|remove|unchanged",
    ///                         "changed", "drift", "private", "diff"}]}],
    ///  "files": [<managed dotfiles, same shape as agent files>],
    ///  "approvals": [{"kind": "hook|mcp_server", "name", "detail"}]}
    /// ```
    /// Last applied generation the plan was computed against.
    /// The layers this plan merged and the fixed precedence (`--explain`).
    pub fn explain(&self) -> Value {
        layer::explain(&self.layers)
    }

    pub fn base_generation(&self) -> Option<u64> {
        self.base.as_ref().map(|(n, _)| *n)
    }

    pub fn to_json(&self, dry_run: bool, applied: bool, generation: Option<u64>) -> Value {
        let mut agents = Vec::new();
        for (id, failure) in &self.agents {
            let files: Vec<Value> = self
                .entries
                .iter()
                .filter(|e| e.agents.contains(id))
                .map(|e| self.file_json(e))
                .collect();
            let changed = files.iter().any(|f| f["changed"] == true);
            let status = match (failure, changed, applied) {
                (Some(_), _, _) => "failed",
                (None, false, _) => "unchanged",
                (None, true, true) => "applied",
                (None, true, false) => "planned",
            };
            let mut agent = json!({ "id": id, "status": status, "files": files });
            if let Some(message) = failure {
                agent["error"] = json!(message);
            }
            agents.push(agent);
        }
        let approvals: Vec<Value> = self
            .approvals
            .iter()
            .map(|a| json!({ "kind": a.kind, "name": a.name, "detail": a.detail }))
            .collect();
        let files: Vec<Value> = self
            .entries
            .iter()
            .filter(|e| e.agents == [dotfiles::OWNER])
            .map(|e| self.file_json(e))
            .collect();
        json!({
            "target": match &self.target {
                Target::Global => json!("global"),
                Target::Project(dir) => json!({ "project": dir }),
            },
            "dry_run": dry_run,
            "applied": applied,
            "generation": generation,
            "changes": self.entries.iter().filter(|e| e.action() != Action::Unchanged).count(),
            "agents": agents,
            "files": files,
            "approvals": approvals,
        })
    }

    fn file_json(&self, entry: &Entry) -> Value {
        let redact = |text: &Option<String>| {
            text.as_deref()
                .map(|t| redact(t, &self.secrets))
                .unwrap_or_default()
        };
        let action = entry.action();
        let diff = diff::unified(
            &redact(&entry.before),
            &redact(&entry.after),
            &entry.path.display().to_string(),
            entry.private,
        );
        json!({
            "path": entry.path,
            "action": action,
            "changed": action != Action::Unchanged,
            "drift": entry.drift,
            "private": entry.private,
            "diff": diff,
        })
    }
}

/// Apply a plan: approvals, backups, atomic writes, ownership records and a
/// new generation. With `--dry-run` this only reports the plan. Returns the
/// plan JSON with `applied`/`generation` filled in.
pub fn apply(ctx: &mut Context, mut plan: Plan) -> Result<Value, MusterError> {
    if ctx.dry_run {
        return Ok(plan.to_json(true, false, plan.base_generation()));
    }
    if !plan.approvals.is_empty() {
        let list: Vec<String> = plan
            .approvals
            .iter()
            .map(|a| format!("{} `{}` ({})", a.kind.replace('_', " "), a.name, a.detail))
            .collect();
        ctx.confirm(&format!(
            "Install new or changed code that runs on this machine: {}",
            list.join(", ")
        ))?;
    }

    let key = plan.target.key();
    let base_state = plan
        .base
        .as_ref()
        .map(|(_, s)| s.clone())
        .unwrap_or_default();
    let mut next = base_state.clone();
    next.approved.insert(key.clone(), plan.approved.clone());
    for entry in &plan.entries {
        set_record(&mut next, &key, entry);
    }
    if !plan.has_changes() && next == base_state {
        return Ok(plan.to_json(false, false, plan.base_generation()));
    }

    let mut pending = store::begin(&ctx.state_dir)?;
    let mut failed: BTreeSet<&'static str> = BTreeSet::new();
    for entry in &plan.entries {
        if entry.action() == Action::Unchanged {
            continue;
        }
        if entry.agents.iter().any(|a| failed.contains(a)) {
            restore_record(&mut next, &base_state, entry);
            continue;
        }
        let result = (|| {
            if let Some(before) = &entry.before {
                pending.backup(&entry.path, before)?;
            }
            match &entry.after {
                Some(after) => write_file(&entry.path, after, entry.private, entry.executable),
                None => std::fs::remove_file(&entry.path).map_err(|e| io_error(&entry.path, e)),
            }
        })();
        if let Err(err) = result {
            for agent in &entry.agents {
                failed.insert(agent);
                ctx.diag(adapter_failure(agent, agent, &err.to_string()));
                plan.agents.insert(agent, Some(err.to_string()));
            }
            restore_record(&mut next, &base_state, entry);
        }
    }
    let generation = pending.commit("sync", None, &next)?;
    ctx.progress(&format!("Applied generation {generation}"));
    Ok(plan.to_json(false, true, Some(generation)))
}

fn set_record(state: &mut State, key: &str, entry: &Entry) {
    let path = entry.path.display().to_string();
    match (&entry.after, entry.managed) {
        (Some(after), true) => {
            state.files.insert(
                path,
                Record {
                    agent: entry.agents[0].to_string(),
                    target: key.to_string(),
                    format: entry.format,
                    owned: entry.owned.clone(),
                    private: entry.private,
                    created: entry.created,
                    executable: entry.executable,
                    contents: after.clone(),
                },
            );
        }
        _ => {
            state.files.remove(&path);
        }
    }
}

fn restore_record(next: &mut State, base: &State, entry: &Entry) {
    let path = entry.path.display().to_string();
    match base.files.get(&path) {
        Some(record) => next.files.insert(path, record.clone()),
        None => next.files.remove(&path),
    };
}

/// Metadata of every generation (for history views); no file contents.
pub fn generations(state_dir: &std::path::Path) -> Result<Vec<Value>, MusterError> {
    store::summaries(state_dir)
}

/// Restore the exact managed files of generation `n`; recorded as a new
/// generation. Files Muster created after `n` are removed; files it only
/// adopted later are left as they are.
pub fn rollback(ctx: &mut Context, n: u64) -> Result<Value, MusterError> {
    let wanted = store::load(&ctx.state_dir, n)?;
    let current = store::latest(&ctx.state_dir)?
        .map(|(_, s)| s)
        .unwrap_or_default();

    let mut actions: Vec<(PathBuf, Option<String>, Option<&Record>, Action)> = Vec::new();
    for (path, record) in &wanted.files {
        let path = PathBuf::from(path);
        let before = read_optional(&path)?;
        let action = match &before {
            None => Action::Add,
            Some(text) if *text == record.contents => Action::Unchanged,
            Some(_) => Action::Modify,
        };
        actions.push((path, before, Some(record), action));
    }
    for (path, record) in &current.files {
        if wanted.files.contains_key(path) {
            continue;
        }
        let path = PathBuf::from(path);
        let before = read_optional(&path)?;
        if record.created && before.is_some() {
            actions.push((path, before, None, Action::Remove));
        } else {
            ctx.diag(
                Diagnostic::new(
                    Severity::Info,
                    "rollback_kept",
                    format!(
                        "{} was not managed in generation {n} and existed before Muster; left as is",
                        path.display()
                    ),
                )
                .with_path(path.display().to_string()),
            );
        }
    }
    let files_json = |actions: &[(PathBuf, Option<String>, Option<&Record>, Action)]| {
        actions
            .iter()
            .map(|(path, _, _, action)| json!({ "path": path, "action": action }))
            .collect::<Vec<_>>()
    };
    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "rollback_to": n,
            "generation": null,
            "files": files_json(&actions),
        }));
    }
    ctx.confirm(&format!("Restore the managed files of generation {n}"))?;

    let mut pending = store::begin(&ctx.state_dir)?;
    for (path, before, record, action) in &actions {
        if *action == Action::Unchanged {
            continue;
        }
        if let Some(before) = before {
            pending.backup(path, before)?;
        }
        match record {
            Some(record) => write_file(path, &record.contents, record.private, record.executable)?,
            None => std::fs::remove_file(path).map_err(|e| io_error(path, e))?,
        }
    }
    let generation = pending.commit("rollback", Some(n), &wanted)?;
    Ok(json!({
        "dry_run": false,
        "rollback_to": n,
        "generation": generation,
        "files": files_json(&actions),
    }))
}

// ---------------------------------------------------------------------------
// Helpers shared with `muster agents`
// ---------------------------------------------------------------------------

/// Adapters to apply: `[agents] enabled`, else every stable adapter.
pub fn enabled_adapters(
    agents: Option<&AgentsConfig>,
) -> Result<Vec<&'static Adapter>, MusterError> {
    let Some(declared) = agents.and_then(|a| a.enabled.as_ref()) else {
        return Ok(adapters::default_enabled()
            .into_iter()
            .filter_map(adapters::find)
            .collect());
    };
    let mut out = Vec::new();
    for id in declared {
        let adapter = adapters::find(id).ok_or_else(|| MusterError::InvalidConfig {
            message: format!("unknown agent `{id}` in agents.enabled"),
            path: Some(format!("{}: agents.enabled", manifest::FILE_NAME)),
        })?;
        if adapter.available() {
            out.push(adapter);
        }
    }
    Ok(out)
}

fn single_adapter(id: &str) -> Result<&'static Adapter, MusterError> {
    let adapter = adapters::find(id).ok_or_else(|| {
        MusterError::Usage(format!(
            "unknown agent `{id}`; run `muster agents` to list adapters"
        ))
    })?;
    if !adapter.available() {
        return Err(MusterError::Unavailable(format!(
            "the {} adapter is experimental and not available yet",
            adapter.name
        )));
    }
    Ok(adapter)
}

/// Filesystem roots managed paths are relative to.
pub struct Roots {
    home: PathBuf,
    xdg_config: PathBuf,
}

impl Roots {
    pub fn from_env() -> Result<Self, MusterError> {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME").ok_or_else(|| MusterError::Operational("HOME is not set".into()))?;
        let xdg_config = var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        Ok(Self { home, xdg_config })
    }

    /// User-scope path of a managed file.
    pub fn resolve(&self, file: &ManagedFile) -> PathBuf {
        match file.base {
            Base::Home => self.home.join(file.path),
            Base::XdgConfig => self.xdg_config.join(file.path),
        }
    }

    /// Path of `file` for `target`; `None` when the agent has no
    /// project-scoped equivalent.
    fn locate(&self, file: &ManagedFile, target: &Target) -> Option<PathBuf> {
        match target {
            Target::Global => Some(self.resolve(file)),
            Target::Project(dir) => file.project.map(|p| dir.join(p)),
        }
    }
}

/// Managed files that held resolved secrets as of the last generation (they
/// must stay 0600). Used by `doctor`.
pub fn private_files(state_dir: &Path) -> Result<Vec<PathBuf>, MusterError> {
    Ok(store::latest(state_dir)?
        .map(|(_, state)| {
            state
                .files
                .into_iter()
                .filter(|(_, record)| record.private)
                .map(|(path, _)| PathBuf::from(path))
                .collect()
        })
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn adapter_failure(id: &str, name: &str, message: &str) -> Diagnostic {
    Diagnostic::error(
        "adapter_failed",
        format!("{name} ({id}) adapter failed: {message}"),
    )
    .with_path(id)
}

fn holds_secret(text: Option<&str>, secrets: &BTreeMap<String, SecretValue>) -> bool {
    text.is_some_and(|t| {
        secrets
            .values()
            .any(|s| !s.expose().is_empty() && t.contains(s.expose()))
    })
}

fn redact(text: &str, secrets: &BTreeMap<String, SecretValue>) -> String {
    let mut out = text.to_string();
    for (reference, secret) in secrets {
        if !secret.expose().is_empty() {
            out = out.replace(secret.expose(), &format!("<redacted:{reference}>"));
        }
    }
    out
}

/// Resolve every secret reference in MCP `env`/`headers` through the provider
/// layer. A failure is a warning carrying the provider's typed code; adapters
/// then leave that key out (and warn) instead of writing the reference.
fn resolve_secrets(
    ctx: &mut Context,
    mcp: &BTreeMap<String, McpServer>,
) -> BTreeMap<String, SecretValue> {
    let mut providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    let mut resolved = BTreeMap::new();
    let mut failed = BTreeSet::new();
    for (name, server) in mcp {
        for (key, value) in server.env.iter().chain(&server.headers) {
            if !secrets::is_reference(value)
                || resolved.contains_key(value)
                || failed.contains(value)
            {
                continue;
            }
            match providers.resolve(value) {
                Ok(secret) => {
                    resolved.insert(value.clone(), secret);
                }
                Err(err) => {
                    failed.insert(value.clone());
                    ctx.diag(
                        Diagnostic::new(
                            Severity::Warning,
                            err.code(),
                            format!("MCP server `{name}` key `{key}`: {err}"),
                        )
                        .with_path(format!("{}: mcp.{name}", manifest::FILE_NAME)),
                    );
                }
            }
        }
    }
    resolved
}

/// Canonical AGENTS.md text. A declared but missing source is a warning
/// (instructions are skipped); an absent default source is silent.
fn read_instructions(
    ctx: &mut Context,
    merged: &layer::Merged,
) -> Result<Option<String>, MusterError> {
    let (path, declared) = merged.instructions_path()?;
    match read_optional(&path)? {
        Some(text) => Ok(Some(text)),
        None => {
            if declared {
                ctx.diag(
                    Diagnostic::new(
                        Severity::Warning,
                        "instructions_missing",
                        format!(
                            "instructions source {} does not exist; instructions were not written",
                            path.display()
                        ),
                    )
                    .with_path(path.display().to_string()),
                );
            }
            Ok(None)
        }
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, MusterError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(path, e)),
    }
}

/// Write via a sibling temp file + rename, keeping the existing file mode, or
/// forcing 0600 when `private` (the file holds resolved secrets). `executable`
/// adds the execute bits (0755, or 0700 when private).
fn write_file(
    path: &Path,
    contents: &str,
    private: bool,
    executable: bool,
) -> Result<(), MusterError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_error(parent, e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".muster-tmp");
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        // The plaintext never exists with looser permissions, even briefly.
        let mode = if executable { 0o700 } else { 0o600 };
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    }
    let mut file = options.open(&tmp).map_err(|e| io_error(&tmp, e))?;
    std::io::Write::write_all(&mut file, contents.as_bytes()).map_err(|e| io_error(&tmp, e))?;
    drop(file);
    if !private {
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, meta.permissions()).map_err(|e| io_error(&tmp, e))?;
        }
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&tmp).map_err(|e| io_error(&tmp, e))?;
        let mode = meta.permissions().mode();
        let exec = if private { 0o100 } else { 0o111 };
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode | exec))
            .map_err(|e| io_error(&tmp, e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io_error(path, e)
    })
}

fn io_error(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}
