//! `muster doctor`: independent health checks, each reporting diagnostics
//! with `code`, `message`, `severity`, optional `path` and a `suggestion`.
//!
//! Checks (in result order): `git`, `config`, `permissions`, `agents`,
//! `repo`, `vault`, `adapters` (render health + drift, via the engine),
//! `keys` (secret references through `Provider::check`/`resolve`) and
//! `api_keys` (opt-in provider metadata, skipped with `--offline`). A check
//! whose prerequisites are missing is `skipped`.
//!
//! `--fix` applies non-destructive repairs (chmod) right away. Destructive
//! ones (re-applying managed configuration over drifted files) need
//! confirmation first; when it is refused nothing at all is changed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapters;
use crate::context::Context;
use crate::engine::{self, Options, Plan, Target};
use crate::errors::MusterError;
use crate::git;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::{self, Manifest};
use crate::output::{Diagnostic, Severity};
use crate::providers::Providers;
use crate::secrets::{self, Scheme, SecretRef};
use crate::vault::Vault;

mod api_keys;

/// Diagnostic codes owned by the `keys` check; the engine reports the same
/// failures while planning, so they are dropped from `adapters`.
const KEY_CODES: &[&str] = &[
    "provider_unavailable",
    "provider_unauthorized",
    "secret_not_found",
    "provider_rate_limited",
    "invalid_secret_reference",
    "secret_reference_unresolved",
];

/// Run every check, optionally repair, and return the doctor result:
///
/// ```json
/// {"healthy": true,
///  "summary": {"errors": 0, "warnings": 1, "info": 0},
///  "checks": [{"name": "git", "status": "ok|warning|error|skipped"}, ...],
///  "fixes": [{"code", "path", "action", "destructive", "applied", "error"?}]}
/// ```
/// Diagnostics go to the envelope. Any error diagnostic marks the run failed
/// (`ok: false`, exit 1) while keeping this result.
pub fn run(
    ctx: &mut Context,
    fix: bool,
    check_keys: bool,
    offline: bool,
) -> Result<Value, MusterError> {
    let mut checkup = examine(ctx);
    let mut fixes: Vec<Value> = checkup
        .repairs
        .iter()
        .map(|r| r.to_json(false, None))
        .collect();

    if fix && !ctx.dry_run && !checkup.repairs.is_empty() {
        let destructive = checkup.repairs.iter().filter(|r| r.destructive()).count();
        if destructive > 0 {
            // Report what was found even if the confirmation is refused.
            for d in checkup.diagnostics() {
                ctx.diag(d);
            }
            ctx.confirm(&format!(
                "re-apply the managed configuration over {destructive} drifted file(s), \
                 overwriting manual edits to managed keys?"
            ))?;
            ctx.take_diagnostics();
        }
        fixes = repair(ctx, &mut checkup);
        checkup = examine(ctx);
    }

    // Run the optional network check once, even when repairs re-examine local state.
    checkup.checks.push(api_keys::check(check_keys, offline));
    checkup
        .checks
        .push(api_keys::check_declared(ctx, check_keys, offline));
    let diagnostics = checkup.diagnostics();
    let count = |s: Severity| diagnostics.iter().filter(|d| d.severity == s).count();
    let errors = count(Severity::Error);
    let result = json!({
        "healthy": errors == 0,
        "summary": {
            "errors": errors,
            "warnings": count(Severity::Warning),
            "info": count(Severity::Info),
        },
        "checks": checkup
            .checks
            .iter()
            .map(|c| json!({ "name": c.name, "status": c.status() }))
            .collect::<Vec<_>>(),
        "fixes": fixes,
    });
    for d in diagnostics {
        ctx.diag(d);
    }
    if errors > 0 {
        ctx.mark_failed();
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Checkup
// ---------------------------------------------------------------------------

struct Check {
    name: &'static str,
    /// `None`: skipped (a prerequisite failed).
    diagnostics: Option<Vec<Diagnostic>>,
}

impl Check {
    fn new(name: &'static str, diagnostics: Vec<Diagnostic>) -> Self {
        Self {
            name,
            diagnostics: Some(diagnostics),
        }
    }

    fn skipped(name: &'static str) -> Self {
        Self {
            name,
            diagnostics: None,
        }
    }

    fn status(&self) -> &'static str {
        let Some(diags) = &self.diagnostics else {
            return "skipped";
        };
        if diags.iter().any(|d| d.severity == Severity::Error) {
            "error"
        } else if diags.iter().any(|d| d.severity == Severity::Warning) {
            "warning"
        } else {
            "ok"
        }
    }
}

enum Repair {
    Chmod {
        path: PathBuf,
        mode: u32,
    },
    /// Re-apply the plan computed by the `adapters` check.
    Resync,
}

impl Repair {
    fn destructive(&self) -> bool {
        matches!(self, Self::Resync)
    }

    fn to_json(&self, applied: bool, error: Option<String>) -> Value {
        let mut value = match self {
            Self::Chmod { path, mode } => json!({
                "code": "insecure_permissions",
                "path": path,
                "action": format!("chmod {mode:04o} {}", path.display()),
            }),
            Self::Resync => json!({
                "code": "drift",
                "path": null,
                "action": "re-apply the managed configuration (muster sync)",
            }),
        };
        value["destructive"] = json!(self.destructive());
        value["applied"] = json!(applied);
        if let Some(error) = error {
            value["error"] = json!(error);
        }
        value
    }
}

struct Checkup {
    checks: Vec<Check>,
    repairs: Vec<Repair>,
    plan: Option<Plan>,
}

impl Checkup {
    fn diagnostics(&self) -> Vec<Diagnostic> {
        self.checks
            .iter()
            .filter_map(|c| c.diagnostics.as_ref())
            .flatten()
            .cloned()
            .collect()
    }
}

fn examine(ctx: &mut Context) -> Checkup {
    let mut repairs = Vec::new();
    let git_ok = git::version().is_some();
    let initialized = ctx.config_dir.join(manifest::FILE_NAME).is_file();
    let (config, manifest) = check_config(ctx, initialized);

    let mut checks = vec![
        check_git(git_ok),
        config,
        check_permissions(ctx, &mut repairs),
    ];
    checks.push(match &manifest {
        Some(m) => check_agents(ctx, m),
        None => Check::skipped("agents"),
    });
    checks.push(if git_ok && initialized {
        check_repo(ctx)
    } else {
        Check::skipped("repo")
    });
    checks.push(if initialized {
        check_vault(ctx)
    } else {
        Check::skipped("vault")
    });
    let mut plan = None;
    checks.push(match &manifest {
        Some(_) => {
            let (check, planned) = check_adapters(ctx);
            if check
                .diagnostics
                .iter()
                .flatten()
                .any(|d| d.code == "drift")
            {
                repairs.push(Repair::Resync);
            }
            plan = planned;
            check
        }
        None => Check::skipped("adapters"),
    });
    checks.push(match &manifest {
        Some(m) => check_keys(ctx, m),
        None => Check::skipped("keys"),
    });
    checks.push(match &manifest {
        Some(m) => check_ai_providers(ctx, m),
        None => Check::skipped("providers"),
    });
    Checkup {
        checks,
        repairs,
        plan,
    }
}

/// Apply every repair; the destructive ones were confirmed by the caller.
fn repair(ctx: &mut Context, checkup: &mut Checkup) -> Vec<Value> {
    let mut fixes = Vec::new();
    for repair in &checkup.repairs {
        let outcome = match repair {
            Repair::Chmod { path, mode } => chmod(path, *mode),
            Repair::Resync => match checkup.plan.take() {
                Some(plan) => engine::apply(ctx, plan).map(|_| ()),
                None => Err(MusterError::Operational("no plan to apply".into())),
            },
        };
        // Whatever apply reported is re-examined below.
        ctx.take_diagnostics();
        fixes.push(match outcome {
            Ok(()) => repair.to_json(true, None),
            Err(e) => repair.to_json(false, Some(e.to_string())),
        });
    }
    fixes
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) -> Result<(), MusterError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))
}

#[cfg(not(unix))]
fn chmod(_: &Path, _: u32) -> Result<(), MusterError> {
    Ok(())
}

// ---------------------------------------------------------------------------
// git, config
// ---------------------------------------------------------------------------

fn check_git(ok: bool) -> Check {
    let mut diags = Vec::new();
    if !ok {
        diags.push(
            Diagnostic::error("git_missing", "git is not installed or not on PATH")
                .with_suggestion(
                    "install git (Muster's only external dependency) and put it on PATH",
                ),
        );
    }
    Check::new("git", diags)
}

fn check_config(ctx: &Context, initialized: bool) -> (Check, Option<Manifest>) {
    let path = ctx.config_dir.join(manifest::FILE_NAME);
    if !initialized {
        let diag = Diagnostic::error(
            "not_initialized",
            format!("no Muster configuration at {}", path.display()),
        )
        .with_path(path.display().to_string())
        .with_suggestion("run `muster init` (new fleet) or `muster enroll` (join an existing one)");
        return (Check::new("config", vec![diag]), None);
    }
    match Manifest::load(&ctx.config_dir) {
        Ok(manifest) => (Check::new("config", Vec::new()), Some(manifest)),
        Err(e) => {
            let diag = Diagnostic::error("config_invalid", e.to_string())
                .with_path(path.display().to_string())
                .with_suggestion(format!(
                    "fix {} (`muster schema` prints the JSON Schema) or restore it with git",
                    path.display()
                ));
            (Check::new("config", vec![diag]), None)
        }
    }
}

// ---------------------------------------------------------------------------
// permissions
// ---------------------------------------------------------------------------

fn check_permissions(ctx: &Context, repairs: &mut Vec<Repair>) -> Check {
    let mut diags = Vec::new();
    let identity = ctx.state_dir.join("identity.txt");
    if ctx.state_dir.is_dir() {
        expect_mode(&ctx.state_dir, 0o700, &mut diags, repairs);
    }
    if identity.is_file() {
        expect_mode(&identity, 0o600, &mut diags, repairs);
    } else {
        diags.push(
            Diagnostic::error("identity_missing", "this machine has no identity")
                .with_path(identity.display().to_string())
                .with_suggestion(
                    "run `muster init` (new fleet) or `muster enroll` (join an existing one)",
                ),
        );
    }
    let leases = ctx.state_dir.join(crate::secret_leases::FILE);
    if leases.is_file() {
        expect_mode(&leases, 0o600, &mut diags, repairs);
    }
    match crate::secret_leases::Store::open(&ctx.state_dir) {
        Ok(store) if !store.data.requests.is_empty() => diags.push(
            Diagnostic::new(
                Severity::Info,
                "secret_requests_pending",
                format!(
                    "{} secret requests await approval",
                    store.data.requests.len()
                ),
            )
            .with_suggestion("muster secret requests"),
        ),
        Err(err) => diags.push(err.to_diagnostic()),
        _ => {}
    }
    // Agent files holding resolved secrets (from the last generation).
    for path in engine::private_files(&ctx.state_dir).unwrap_or_default() {
        if path.is_file() {
            expect_mode(&path, 0o600, &mut diags, repairs);
        }
    }
    Check::new("permissions", diags)
}

/// Flags `path` when group or others have any access (`mode` is the target).
#[cfg(unix)]
fn expect_mode(path: &Path, mode: u32, diags: &mut Vec<Diagnostic>, repairs: &mut Vec<Repair>) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let actual = meta.permissions().mode() & 0o777;
    if actual & 0o077 == 0 {
        return;
    }
    diags.push(
        Diagnostic::error(
            "insecure_permissions",
            format!(
                "{} is {actual:04o}; it must not be accessible to other users ({mode:04o})",
                path.display()
            ),
        )
        .with_path(path.display().to_string())
        .with_suggestion(format!(
            "chmod {mode:04o} {} (or run `muster doctor --fix`)",
            path.display()
        )),
    );
    repairs.push(Repair::Chmod {
        path: path.to_path_buf(),
        mode,
    });
}

#[cfg(not(unix))]
fn expect_mode(_: &Path, _: u32, _: &mut Vec<Diagnostic>, _: &mut Vec<Repair>) {}

// ---------------------------------------------------------------------------
// agents
// ---------------------------------------------------------------------------

fn check_agents(ctx: &Context, manifest: &Manifest) -> Check {
    let merged = crate::engine::layer::global_merge(&ctx.config_dir, &ctx.state_dir, manifest);
    let declared: Vec<String> = merged
        .agents
        .enabled
        .or_else(|| manifest.agents.as_ref().and_then(|a| a.enabled.clone()))
        .unwrap_or_else(|| {
            adapters::default_enabled()
                .into_iter()
                .map(String::from)
                .collect()
        });
    let mut diags = Vec::new();
    let pins = match crate::installer::Lock::load_or_default(&ctx.config_dir) {
        Ok(lock) => Some(lock),
        Err(error) => {
            diags.push(Diagnostic::error("invalid_agent_lock", error.to_string()));
            None
        }
    };
    for id in &declared {
        if pins
            .as_ref()
            .is_some_and(|lock| !lock.agents.contains_key(id))
        {
            diags.push(
                Diagnostic::new(
                    Severity::Warning,
                    "agent_unpinned",
                    format!("agent `{id}` is configured but has no pin in muster.lock"),
                )
                .with_path(id.clone())
                .with_suggestion("pin the agent version in muster.lock"),
            );
        }
        // Unknown ids are reported by the `adapters` check (invalid config).
        let Some(adapter) = adapters::find(id) else {
            continue;
        };
        if !adapter.available() {
            diags.push(
                Diagnostic::new(
                    Severity::Warning,
                    "adapter_unavailable",
                    format!(
                        "the {} adapter is experimental and not available yet",
                        adapter.name
                    ),
                )
                .with_path(id.clone())
                .with_suggestion(format!(
                    "remove `{id}` from [agents] enabled in muster.toml"
                )),
            );
            continue;
        }
        let binary = pins
            .as_ref()
            .and_then(|lock| lock.agents.get(id))
            .map(|pin| crate::installer::default_bin(id, pin))
            .unwrap_or(adapter.binary);
        let local = crate::installer::bin_dir().ok().map(|dir| dir.join(binary));
        let executable = local
            .filter(|path| path.is_file())
            .or_else(|| machine::find_executable(binary));
        let Some(exe) = executable else {
            diags.push(
                Diagnostic::new(
                    Severity::Warning,
                    "missing_binary",
                    format!("{} is enabled but `{binary}` is not on PATH", adapter.name),
                )
                .with_path(id.clone())
                .with_suggestion(format!(
                    "install {} (`{binary}`, version {} or newer) or remove `{id}` from [agents] enabled",
                    adapter.name, adapter.min_version
                )),
            );
            continue;
        };
        let Some(minimum) = parse_version(adapter.min_version) else {
            continue;
        };
        let found = machine::version_output(&exe);
        match found.as_deref().ok().and_then(parse_version) {
            Some(version) if version < minimum => diags.push(
                Diagnostic::error(
                    "outdated_binary",
                    format!(
                        "{} {} is older than the minimum supported version {}",
                        adapter.name,
                        format_version(version),
                        adapter.min_version
                    ),
                )
                .with_path(exe.display().to_string())
                .with_suggestion(format!(
                    "upgrade {} to {} or newer",
                    adapter.name, adapter.min_version
                )),
            ),
            Some(_) => {}
            None => diags.push(
                Diagnostic::new(
                    Severity::Warning,
                    "version_unknown",
                    format!(
                        "cannot read the version of {} ({}): {}",
                        adapter.name,
                        exe.display(),
                        found.unwrap_or_else(|e| e)
                    ),
                )
                .with_path(exe.display().to_string())
                .with_suggestion(format!(
                    "check that `{id} --version` works; Muster needs {} or newer",
                    adapter.min_version
                )),
            ),
        }
    }
    Check::new("agents", diags)
}

type Version = (u64, u64, u64);

/// First `MAJOR.MINOR[.PATCH]` token (optionally `v`-prefixed, with any
/// pre-release/build suffix ignored) in a `--version` line.
fn parse_version(text: &str) -> Option<Version> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '/'))
        .find_map(|word| {
            let word = word.strip_prefix('v').unwrap_or(word);
            let core = word.split(['-', '+']).next()?;
            let mut parts = core.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = match parts.next() {
                Some(p) => p.parse().ok()?,
                None => 0,
            };
            parts.next().is_none().then_some((major, minor, patch))
        })
}

fn format_version((major, minor, patch): Version) -> String {
    format!("{major}.{minor}.{patch}")
}

// ---------------------------------------------------------------------------
// repo, vault
// ---------------------------------------------------------------------------

fn check_repo(ctx: &Context) -> Check {
    let dir = ctx.config_dir.display().to_string();
    let status = match git::status(&ctx.config_dir) {
        Ok(status) => status,
        Err(e) => {
            let diag = Diagnostic::error("repo_invalid", e.to_string())
                .with_path(dir.clone())
                .with_suggestion(format!(
                    "make {dir} a git repository (`git init {dir}`) or restore it from your remote"
                ));
            return Check::new("repo", vec![diag]);
        }
    };
    let mut diags = Vec::new();
    let warn = |code: &str, message: String, suggestion: String| {
        Diagnostic::new(Severity::Warning, code, message)
            .with_path(dir.clone())
            .with_suggestion(suggestion)
    };
    if status.changed > 0 {
        diags.push(warn(
            "repo_dirty",
            format!(
                "the config repo has {} uncommitted change(s)",
                status.changed
            ),
            format!("review and commit them (`git -C {dir} add -A && git -C {dir} commit`) or discard them"),
        ));
    }
    match status.ahead_behind {
        Some((ahead, behind)) => {
            if ahead > 0 {
                diags.push(warn(
                    "repo_unpushed",
                    format!("{ahead} commit(s) not pushed to the remote"),
                    "run `muster sync` to push them".into(),
                ));
            }
            if behind > 0 {
                diags.push(warn(
                    "repo_unpulled",
                    format!(
                        "{behind} commit(s) on the remote not pulled yet (as of the last fetch)"
                    ),
                    "run `muster sync` to pull them".into(),
                ));
            }
        }
        None if status.has_remote => diags.push(warn(
            "repo_unpushed",
            "the current branch has never been pushed to the remote".into(),
            "run `muster sync` to push it".into(),
        )),
        None => {}
    }
    Check::new("repo", diags)
}

fn check_vault(ctx: &Context) -> Check {
    let dir = ctx.config_dir.join("vault");
    // A fresh clone has no vault/ until the first secret: nothing to verify.
    if !dir.is_dir() {
        return Check::new("vault", Vec::new());
    }
    let identity = Identity::load(&ctx.state_dir).ok();
    let problems = Vault::open(&ctx.config_dir).and_then(|v| v.verify(identity.as_ref()));
    let diags = match problems {
        Ok(problems) => problems
            .into_iter()
            .map(|p| {
                Diagnostic::error(
                    "vault_corrupt",
                    format!("secret group `{}`: {}", p.group, p.message),
                )
                .with_path(p.path.display().to_string())
                .with_suggestion(format!(
                    "restore vault/{group}.age and vault/{group}.meta.json from the config repo \
                     history (`git -C {} log -- vault/{group}.*`) or remove both files",
                    ctx.config_dir.display(),
                    group = p.group
                ))
            })
            .collect(),
        Err(e) => vec![Diagnostic::error("vault_corrupt", e.to_string())
            .with_path(dir.display().to_string())
            .with_suggestion("check that the vault directory is readable")],
    };
    Check::new("vault", diags)
}

// ---------------------------------------------------------------------------
// adapters (render health + drift), keys
// ---------------------------------------------------------------------------

fn check_adapters(ctx: &mut Context) -> (Check, Option<Plan>) {
    let earlier = ctx.take_diagnostics();
    let options = Options {
        target: Target::Global,
        agent: None,
    };
    let planned = engine::plan(ctx, &options);
    let produced = ctx.take_diagnostics();
    for d in earlier {
        ctx.diag(d);
    }
    let mut diags: Vec<Diagnostic> = produced
        .into_iter()
        .filter(|d| !KEY_CODES.contains(&d.code.as_str()))
        .map(|d| {
            if d.suggestion.is_some() {
                return d;
            }
            let hint = engine_suggestion(&d.code);
            d.with_suggestion(hint)
        })
        .collect();
    let plan =
        match planned {
            Ok(plan) => plan,
            Err(e) => {
                diags.push(e.to_diagnostic().with_suggestion(
                    "fix muster.toml (see `muster schema`), then run `muster plan`",
                ));
                return (Check::new("adapters", diags), None);
            }
        };
    if plan.has_changes() && !diags.iter().any(|d| d.code == "drift") {
        diags.push(
            Diagnostic::new(
                Severity::Info,
                "sync_pending",
                "managed agent files differ from the desired configuration",
            )
            .with_suggestion(
                "run `muster plan` to review the changes and `muster sync` to apply them",
            ),
        );
    }
    (Check::new("adapters", diags), Some(plan))
}

fn engine_suggestion(code: &str) -> &'static str {
    match code {
        "drift" => {
            "run `muster sync` (or `muster doctor --fix --yes`) to restore the managed keys; \
             edits outside them are kept"
        }
        "adapter_failed" => {
            "fix or move aside the agent file named in the message, then run `muster sync`"
        }
        "unsupported_feature" => {
            "nothing to do unless this agent needs that feature; other agents still get it"
        }
        "instructions_missing" => {
            "create the instructions file or fix `[agents.instructions] source` in muster.toml"
        }
        _ => "run `muster plan` for details",
    }
}

/// Every secret reference in MCP `env`/`headers`: one provider check per
/// scheme, then one resolution per reference (values dropped at once).
fn check_keys(ctx: &Context, manifest: &Manifest) -> Check {
    let merged = crate::engine::layer::global_merge(&ctx.config_dir, &ctx.state_dir, manifest);
    let mut diags = Vec::new();
    let mut by_scheme: BTreeMap<Scheme, Vec<(String, SecretRef)>> = BTreeMap::new();
    for (server, config) in &merged.mcp {
        for (key, value) in config.env.iter().chain(&config.headers) {
            if !secrets::is_reference(value) {
                continue;
            }
            let at = format!("{}: mcp.{server}", manifest::FILE_NAME);
            match SecretRef::parse(value) {
                Ok(reference) => by_scheme
                    .entry(reference.scheme())
                    .or_default()
                    .push((at, reference)),
                Err(e) => diags.push(
                    Diagnostic::error(e.code(), format!("MCP server `{server}` key `{key}`: {e}"))
                        .with_path(at)
                        .with_suggestion("fix the secret reference in muster.toml"),
                ),
            }
        }
    }

    let mut secret_providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    for (scheme, references) in by_scheme {
        let provider = secret_providers.provider(scheme);
        let description = provider.describe();
        let hint = |code: &str| key_suggestion(code, description.name, description.binary);
        if let Err(e) = provider.check() {
            diags.push(
                Diagnostic::error(
                    e.code(),
                    format!(
                        "{} cannot be used for {} reference(s): {e}",
                        description.name,
                        references.len()
                    ),
                )
                .with_path(references[0].0.clone())
                .with_suggestion(hint(e.code())),
            );
            continue;
        }
        for (at, reference) in references {
            if let Err(e) = provider.resolve(&reference) {
                diags.push(
                    Diagnostic::error(e.code(), format!("{}: {e}", reference.as_str()))
                        .with_path(at)
                        .with_suggestion(hint(e.code())),
                );
            }
        }
    }
    Check::new("keys", diags)
}

fn key_suggestion(code: &str, provider: &str, binary: Option<&str>) -> String {
    match (code, binary) {
        ("provider_unavailable", Some(bin)) => {
            format!("install the `{bin}` CLI ({provider}) and make sure it is on PATH")
        }
        ("provider_unavailable", None) => {
            "run `muster init` (new fleet) or `muster enroll` (join an existing one)".into()
        }
        ("provider_unauthorized", Some(bin)) => {
            format!("sign in to {provider} with the `{bin}` CLI, then run `muster doctor` again")
        }
        ("provider_unauthorized", None) => {
            "ask an authorized machine to run `muster approve <this machine>`".into()
        }
        ("secret_not_found", None) => {
            "add it with `muster secret add <group>/<name> --value-stdin` or fix the reference in muster.toml".into()
        }
        ("secret_not_found", Some(_)) => {
            format!("create the secret in {provider} or fix the reference in muster.toml")
        }
        ("provider_rate_limited", _) => {
            format!("{provider} is throttling requests; wait a moment and run `muster doctor` again")
        }
        _ => "fix the secret reference in muster.toml".into(),
    }
}

/// Resolve each configured endpoint on this machine, without retaining values
/// or reporting external secret CLI output. Network validation is separate.
fn check_ai_providers(ctx: &Context, manifest: &Manifest) -> Check {
    let merged = crate::engine::layer::global_merge(&ctx.config_dir, &ctx.state_dir, manifest);
    let mut secret_providers = Providers::new(&ctx.config_dir, &ctx.state_dir);
    let mut diagnostics = Vec::new();
    for (id, ai_provider) in &merged.ai_providers {
        if let Some(reference) = &ai_provider.key {
            if secret_providers.resolve(reference).is_err() {
                diagnostics.push(
                    Diagnostic::new(
                        Severity::Warning,
                        "secret_reference_unresolved",
                        format!("provider `{id}` key cannot be resolved on this machine"),
                    )
                    .with_path(format!("providers.{id}"))
                    .with_suggestion("make the referenced secret available on this machine"),
                );
            }
        }
    }
    for (agent, id) in &merged.agents.ai_providers {
        if merged
            .agents
            .enabled
            .as_ref()
            .is_some_and(|enabled| !enabled.contains(agent))
        {
            continue;
        }
        let Some(ai_provider) = merged.ai_providers.get(id) else {
            continue;
        };
        if agent == "claude"
            && ai_provider.key.is_some()
            && ai_provider.api_key_helper != Some(true)
            && adapters::find(agent)
                .is_some_and(|adapter| adapter.supports_ai_provider(ai_provider.kind))
        {
            diagnostics.push(
                Diagnostic::new(
                    Severity::Info,
                    "provider_plaintext_key",
                    format!("Claude Code stores provider `{id}` credentials in plaintext settings"),
                )
                .with_path(format!("providers.{id}"))
                .with_suggestion(format!(
                    "set providers.{id}.api_key_helper = true to resolve the key at runtime"
                )),
            );
        }
        if agent == "opencode"
            && ai_provider
                .key
                .as_deref()
                .is_some_and(|key| !key.starts_with("env://"))
        {
            diagnostics.push(
                Diagnostic::new(
                    Severity::Info,
                    "provider_plaintext_key",
                    format!("OpenCode stores provider `{id}` credentials in plaintext settings"),
                )
                .with_path(format!("providers.{id}"))
                .with_suggestion("use an env:// reference and export it in OpenCode's environment"),
            );
        }
        if ai_provider.key.is_some()
            || !matches!(
                (agent.as_str(), ai_provider.kind),
                ("claude", adapters::config::AiProviderKind::Anthropic)
                    | ("codex", adapters::config::AiProviderKind::Openai)
            )
        {
            continue;
        }
        let Some((status, _)) = crate::commands::provider::login_commands(agent) else {
            continue;
        };
        let Some(adapter) = adapters::find(agent) else {
            continue;
        };
        if machine::find_executable(adapter.binary)
            .is_some_and(|binary| crate::commands::provider::logged_in(&binary, status))
        {
            continue;
        }
        diagnostics.push(
            Diagnostic::new(
                Severity::Warning,
                "provider_login_required",
                format!("agent `{agent}` needs a subscription login on this machine"),
            )
            .with_path(format!("providers.{id}"))
            .with_suggestion(format!("run `muster provider login {agent}`")),
        );
    }
    Check::new("providers", diagnostics)
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn parses_versions_out_of_cli_banners() {
        assert_eq!(parse_version("2.1.0 (Claude Code)"), Some((2, 1, 0)));
        assert_eq!(parse_version("codex-cli 1.4.2"), Some((1, 4, 2)));
        assert_eq!(parse_version("v0.5"), Some((0, 5, 0)));
        assert_eq!(parse_version("tool 1.2.3-beta.1+build"), Some((1, 2, 3)));
        assert_eq!(parse_version("git version 2.43.0"), Some((2, 43, 0)));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert!(parse_version("0.9.3").unwrap() < parse_version("1.0.0").unwrap());
        let codex = crate::adapters::find("codex").unwrap();
        assert!(
            parse_version("codex-cli 0.166.0").unwrap()
                >= parse_version(codex.min_version).unwrap()
        );
    }
}
