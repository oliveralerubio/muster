//! Ticket #11 — `muster doctor`.
//!
//! Doctor inspects a simulated machine (agent binaries and versions, git,
//! permissions of sensitive files, config repo, vault, adapters, drift and
//! secret provider keys) and reports diagnostics with a stable JSON shape,
//! each with a suggestion. `--fix` applies only non-destructive repairs on its
//! own; anything destructive needs confirmation (`--yes`).
//!
//! Every run uses an isolated PATH (temp bin dir only) so the host's real
//! agent CLIs never leak into the result.

mod support;

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use support::{Envelope, MusterEnv};

fn state_dir(env: &MusterEnv) -> PathBuf {
    env.path().join(".local/state").join("muster")
}

fn identity_path(env: &MusterEnv) -> PathBuf {
    state_dir(env).join("identity.txt")
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn chmod(path: &std::path::Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// An initialized machine with git and every stable agent CLI installed at a
/// supported version.
fn healthy_machine() -> MusterEnv {
    let env = MusterEnv::new();
    env.link_system_exe("git");
    env.fake_exe("claude", "echo '2.1.0 (Claude Code)'");
    env.fake_exe("codex", "echo 'codex-cli 1.4.2'");
    env.fake_exe("opencode", "echo '0.5.0'");
    env.fake_exe("agy", "echo 'agy 1.3.0'");
    env.run_isolated(&["--format", "json", "--yes", "init"])
        .assert()
        .code(0);
    env
}

fn doctor(env: &MusterEnv, extra: &[&str]) -> (std::process::Output, Envelope) {
    let mut args = vec!["--format", "json"];
    args.extend_from_slice(extra);
    args.push("doctor");
    support::json(env.run_isolated(&args))
}

fn doctor_fix(env: &MusterEnv, extra: &[&str]) -> (std::process::Output, Envelope) {
    let mut args = vec!["--format", "json"];
    args.extend_from_slice(extra);
    args.extend(["doctor", "--fix"]);
    support::json(env.run_isolated(&args))
}

fn with_code<'a>(envelope: &'a Envelope, code: &str) -> Vec<&'a Value> {
    envelope
        .diagnostics()
        .as_array()
        .expect("diagnostics must be an array")
        .iter()
        .filter(|d| d["code"] == code)
        .collect()
}

fn errors(envelope: &Envelope) -> Vec<&Value> {
    envelope
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["severity"] == "error")
        .collect()
}

fn suggestion(d: &Value) -> &str {
    d["suggestion"]
        .as_str()
        .unwrap_or_else(|| panic!("diagnostic must carry a suggestion: {d}"))
}

fn append_manifest(env: &MusterEnv, extra: &str) {
    let path = env.config_dir().join("muster.toml");
    let mut text = std::fs::read_to_string(&path).expect("read manifest");
    text.push_str(extra);
    std::fs::write(&path, text).expect("write manifest");
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@localhost"])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        status.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

fn commit_all(env: &MusterEnv) {
    git(&env.config_dir(), &["add", "-A"]);
    git(&env.config_dir(), &["commit", "--quiet", "-m", "baseline"]);
}

// ---------------------------------------------------------------------------
// Healthy machine and the JSON contract
// ---------------------------------------------------------------------------

#[test]
fn healthy_machine_exits_zero_with_no_error_diagnostics() {
    let env = healthy_machine();

    let (output, envelope) = doctor(&env, &[]);
    assert!(
        output.status.success(),
        "healthy machine must exit 0: {}",
        envelope.raw
    );
    assert!(envelope.ok());
    assert!(
        errors(&envelope).is_empty(),
        "healthy machine must report no error diagnostics: {}",
        envelope.diagnostics()
    );
    assert_eq!(envelope.result()["healthy"], true);
}

#[test]
fn json_result_is_stable_and_every_diagnostic_has_code_message_severity_and_suggestion() {
    let env = healthy_machine();
    // Several problems at once: insecure key, a missing agent, a dirty repo.
    chmod(&identity_path(&env), 0o644);
    std::fs::remove_file(env.bin.join("opencode")).unwrap();

    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    assert_eq!(envelope.schema_version(), 1);
    assert_eq!(envelope.command(), "doctor");
    assert!(!envelope.ok());

    let result = envelope.result();
    assert_eq!(result["healthy"], false);
    assert_eq!(result["summary"]["errors"], errors(&envelope).len());
    let checks = result["checks"].as_array().expect("result.checks list");
    let names: Vec<&str> = checks.iter().map(|c| c["name"].as_str().unwrap()).collect();
    for expected in [
        "git",
        "config",
        "permissions",
        "agents",
        "repo",
        "vault",
        "adapters",
        "keys",
    ] {
        assert!(
            names.contains(&expected),
            "missing check {expected}: {names:?}"
        );
    }
    for check in checks {
        let status = check["status"].as_str().unwrap();
        assert!(
            ["ok", "warning", "error", "skipped"].contains(&status),
            "bad check status: {check}"
        );
    }
    let permissions = checks.iter().find(|c| c["name"] == "permissions").unwrap();
    assert_eq!(permissions["status"], "error");
    assert!(result["fixes"].is_array(), "result.fixes must be a list");

    let diags = envelope.diagnostics().as_array().unwrap();
    assert!(diags.len() >= 2, "{diags:?}");
    for d in diags {
        for field in ["code", "message", "severity", "suggestion"] {
            assert!(
                d[field].is_string(),
                "diagnostic.{field} must be a string: {d}"
            );
        }
        assert!(d["path"].is_null() || d["path"].is_string(), "{d}");
        let severity = d["severity"].as_str().unwrap();
        assert!(["error", "warning", "info"].contains(&severity), "{d}");
    }
}

// ---------------------------------------------------------------------------
// Permissions of sensitive files
// ---------------------------------------------------------------------------

#[test]
fn detects_identity_file_with_insecure_permissions_and_suggests_chmod() {
    let env = healthy_machine();
    let identity = identity_path(&env);
    chmod(&identity, 0o644);

    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "problems exit 1");
    assert!(!envelope.ok(), "envelope must be ok=false with problems");

    let diags = with_code(&envelope, "insecure_permissions");
    assert_eq!(diags.len(), 1, "{}", envelope.diagnostics());
    let d = diags[0];
    assert_eq!(d["severity"], "error");
    assert_eq!(d["path"], identity.to_string_lossy().as_ref());
    let hint = suggestion(d);
    assert!(
        hint.contains("chmod") && hint.contains("0600"),
        "suggestion must propose chmod 0600, got: {hint}"
    );
}

#[test]
fn detects_state_dir_open_to_other_users() {
    let env = healthy_machine();
    chmod(&state_dir(&env), 0o755);

    let (_, envelope) = doctor(&env, &[]);
    let diags = with_code(&envelope, "insecure_permissions");
    assert_eq!(diags.len(), 1, "{}", envelope.diagnostics());
    assert_eq!(diags[0]["path"], state_dir(&env).to_string_lossy().as_ref());
    assert!(suggestion(diags[0]).contains("0700"));
}

#[test]
fn doctor_fix_repairs_permissions_without_confirmation_because_chmod_is_not_destructive() {
    let env = healthy_machine();
    let identity = identity_path(&env);
    chmod(&identity, 0o644);
    chmod(&state_dir(&env), 0o755);

    // --dry-run only reports the repair.
    let (output, envelope) = doctor_fix(&env, &["--dry-run"]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    assert_eq!(mode(&identity), 0o644, "--dry-run must not chmod");
    let fixes = envelope.result()["fixes"].as_array().unwrap();
    assert!(fixes.iter().all(|f| f["applied"] == false), "{fixes:?}");

    // No TTY, no --yes: chmod is still applied (non-destructive).
    let (output, envelope) = doctor_fix(&env, &[]);
    assert!(
        output.status.success(),
        "doctor --fix must exit 0 after repairing: {}",
        envelope.raw
    );
    assert!(envelope.ok());
    assert_eq!(mode(&identity), 0o600, "identity restored to 0600");
    assert_eq!(mode(&state_dir(&env)), 0o700, "state dir restored to 0700");
    assert!(with_code(&envelope, "insecure_permissions").is_empty());
    let fixes = envelope.result()["fixes"].as_array().unwrap();
    let applied: Vec<_> = fixes
        .iter()
        .filter(|f| f["applied"] == true && f["destructive"] == false)
        .collect();
    assert_eq!(applied.len(), 2, "{fixes:?}");
}

// ---------------------------------------------------------------------------
// Agent binaries, versions, git
// ---------------------------------------------------------------------------

#[test]
fn detects_missing_agent_binary_of_an_enabled_agent_with_install_suggestion() {
    let env = healthy_machine();
    append_manifest(&env, "\n[agents]\nenabled = [\"claude\", \"codex\"]\n");
    std::fs::remove_file(env.bin.join("claude")).unwrap();
    // opencode is not enabled: its absence is not a problem.
    std::fs::remove_file(env.bin.join("opencode")).unwrap();

    let (output, envelope) = doctor(&env, &[]);
    let missing = with_code(&envelope, "missing_binary");
    assert_eq!(missing.len(), 1, "{}", envelope.diagnostics());
    let d = missing[0];
    assert!(d["message"].as_str().unwrap().contains("claude"), "{d}");
    assert_eq!(d["severity"], "warning");
    assert!(suggestion(d).to_lowercase().contains("install"), "{d}");
    // Warnings alone keep the exit code at 0.
    assert!(output.status.success(), "{}", envelope.raw);
}

#[test]
fn detects_agent_binary_older_than_the_adapter_minimum_version() {
    let env = healthy_machine();
    env.fake_exe("claude", "echo '0.9.3 (Claude Code)'");

    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    let outdated = with_code(&envelope, "outdated_binary");
    assert_eq!(outdated.len(), 1, "{}", envelope.diagnostics());
    let d = outdated[0];
    let message = d["message"].as_str().unwrap();
    assert!(
        message.contains("0.9.3") && message.contains("1.0.0"),
        "{d}"
    );
    assert!(suggestion(d).contains("1.0.0"), "{d}");
    assert!(with_code(&envelope, "missing_binary").is_empty());
}

#[test]
fn missing_git_is_an_error_with_install_suggestion() {
    let env = healthy_machine();
    std::fs::remove_file(env.bin.join("git")).unwrap();

    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    let d = with_code(&envelope, "git_missing");
    assert_eq!(d.len(), 1, "{}", envelope.diagnostics());
    assert!(suggestion(d[0]).to_lowercase().contains("install git"));
}

// ---------------------------------------------------------------------------
// Config repo
// ---------------------------------------------------------------------------

#[test]
fn reports_uncommitted_changes_in_the_config_repo_as_a_warning() {
    let env = healthy_machine();
    commit_all(&env);
    let (_, clean) = doctor(&env, &[]);
    assert!(with_code(&clean, "repo_dirty").is_empty(), "{}", clean.raw);

    std::fs::write(env.config_dir().join("notes.md"), "draft\n").unwrap();
    let (output, envelope) = doctor(&env, &[]);
    let d = with_code(&envelope, "repo_dirty");
    assert_eq!(d.len(), 1, "{}", envelope.diagnostics());
    assert_eq!(d[0]["severity"], "warning");
    assert!(suggestion(d[0]).contains("commit"));
    assert!(output.status.success());
}

#[test]
fn reports_commits_not_pushed_to_the_remote() {
    let env = healthy_machine();
    commit_all(&env);
    let remote = env.path().join("remote.git");
    git(
        &env.path(),
        &["init", "--quiet", "--bare", remote.to_str().unwrap()],
    );
    git(
        &env.config_dir(),
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(
        &env.config_dir(),
        &["push", "--quiet", "-u", "origin", "HEAD"],
    );
    let (_, synced) = doctor(&env, &[]);
    assert!(
        with_code(&synced, "repo_unpushed").is_empty(),
        "{}",
        synced.raw
    );

    std::fs::write(env.config_dir().join("notes.md"), "draft\n").unwrap();
    commit_all(&env);
    let (_, envelope) = doctor(&env, &[]);
    let d = with_code(&envelope, "repo_unpushed");
    assert_eq!(d.len(), 1, "{}", envelope.diagnostics());
    assert!(d[0]["message"].as_str().unwrap().contains('1'), "{}", d[0]);
    assert!(suggestion(d[0]).contains("muster sync"));
}

#[test]
fn invalid_manifest_is_an_error_with_a_repair_suggestion() {
    let env = healthy_machine();
    std::fs::write(
        env.config_dir().join("muster.toml"),
        "version = 1\n[[garbage]]\n",
    )
    .unwrap();

    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    let d = with_code(&envelope, "config_invalid");
    assert_eq!(d.len(), 1, "{}", envelope.diagnostics());
    assert_eq!(d[0]["severity"], "error");
    assert!(suggestion(d[0]).contains("muster.toml"));
}

#[test]
fn uninitialized_machine_suggests_muster_init() {
    let env = MusterEnv::new();
    env.link_system_exe("git");

    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    let d = with_code(&envelope, "not_initialized");
    assert_eq!(d.len(), 1, "{}", envelope.diagnostics());
    assert!(suggestion(d[0]).contains("muster init"));
}

// ---------------------------------------------------------------------------
// Vault
// ---------------------------------------------------------------------------

#[test]
fn detects_vault_group_with_missing_files_or_bad_age_header() {
    let env = healthy_machine();
    env.run_isolated(&["--yes", "secret", "add", "db/password", "--value-stdin"])
        .write_stdin("hunter2")
        .assert()
        .code(0);
    let vault = env.config_dir().join("vault");
    // `broken`: metadata fine, ciphertext is not an age file.
    std::fs::copy(vault.join("db.meta.json"), vault.join("broken.meta.json")).unwrap();
    std::fs::write(vault.join("broken.age"), "this is not an age file").unwrap();
    // `orphan`: metadata without ciphertext.
    std::fs::write(vault.join("orphan.meta.json"), "{}").unwrap();

    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    let diags = with_code(&envelope, "vault_corrupt");
    let paths: Vec<&str> = diags.iter().map(|d| d["path"].as_str().unwrap()).collect();
    assert!(paths.iter().any(|p| p.ends_with("broken.age")), "{paths:?}");
    assert!(paths.iter().any(|p| p.contains("orphan")), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.contains("/db.")),
        "healthy group `db` must not be flagged: {paths:?}"
    );
    for d in diags {
        assert_eq!(d["severity"], "error");
        suggestion(d);
    }
}

// ---------------------------------------------------------------------------
// Adapters, drift and destructive repairs
// ---------------------------------------------------------------------------

fn synced_claude_machine() -> MusterEnv {
    let env = healthy_machine();
    append_manifest(
        &env,
        "\n[agents]\nenabled = [\"claude\"]\n\n[mcp.github]\ncommand = \"npx\"\nargs = [\"-y\", \"@modelcontextprotocol/server-github\"]\n",
    );
    env.run_isolated(&["--format", "json", "--yes", "sync"])
        .assert()
        .code(0);
    env
}

#[test]
fn reports_failed_adapters_and_drift() {
    let env = synced_claude_machine();
    let (_, clean) = doctor(&env, &[]);
    assert!(with_code(&clean, "drift").is_empty(), "{}", clean.raw);

    let mcp_path = env.path().join(".claude.json");
    std::fs::write(
        &mcp_path,
        r#"{"mcpServers": {"github": {"command": "evil"}}}"#,
    )
    .unwrap();
    let (output, envelope) = doctor(&env, &[]);
    let drift = with_code(&envelope, "drift");
    assert_eq!(drift.len(), 1, "{}", envelope.diagnostics());
    assert_eq!(drift[0]["path"], mcp_path.to_string_lossy().as_ref());
    assert!(suggestion(drift[0]).contains("muster sync"));
    assert!(output.status.success(), "drift alone is a warning");

    // A broken native file fails that adapter.
    std::fs::write(env.path().join(".claude/settings.json"), "{ not json").unwrap();
    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    let failed = with_code(&envelope, "adapter_failed");
    assert_eq!(failed.len(), 1, "{}", envelope.diagnostics());
    suggestion(failed[0]);
}

#[test]
fn destructive_repair_needs_confirmation_and_without_it_nothing_changes() {
    let env = synced_claude_machine();
    let mcp_path = env.path().join(".claude.json");
    let drifted = r#"{"mcpServers": {"github": {"command": "evil"}}}"#;
    std::fs::write(&mcp_path, drifted).unwrap();
    let identity = identity_path(&env);
    chmod(&identity, 0o644);

    // Re-applying over drift overwrites manual edits: no TTY + no --yes -> exit 2.
    let (output, envelope) = doctor_fix(&env, &[]);
    assert_eq!(output.status.code(), Some(2), "{}", envelope.raw);
    assert!(!envelope.ok());
    assert_eq!(std::fs::read_to_string(&mcp_path).unwrap(), drifted);
    assert_eq!(mode(&identity), 0o644, "nothing is changed when refused");

    let (output, envelope) = doctor_fix(&env, &["--yes"]);
    assert!(output.status.success(), "{}", envelope.raw);
    let repaired = std::fs::read_to_string(&mcp_path).unwrap();
    assert!(
        !repaired.contains("evil") && repaired.contains("server-github"),
        "{repaired}"
    );
    assert_eq!(mode(&identity), 0o600);
    assert!(with_code(&envelope, "drift").is_empty(), "{}", envelope.raw);
    let fixes = envelope.result()["fixes"].as_array().unwrap();
    assert!(
        fixes
            .iter()
            .any(|f| f["destructive"] == true && f["applied"] == true),
        "{fixes:?}"
    );
}

// ---------------------------------------------------------------------------
// Secret provider keys (spec story 39)
// ---------------------------------------------------------------------------

#[test]
fn checks_secret_references_through_their_providers() {
    let env = healthy_machine();
    append_manifest(
        &env,
        "\n[agents]\nenabled = [\"claude\"]\n\n[mcp.tools]\ncommand = \"tools\"\n\n[mcp.tools.env]\nOP_TOKEN = \"op://vault/item/field\"\nDB = \"native://db/missing\"\n",
    );

    let (output, envelope) = doctor(&env, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", envelope.raw);
    let unavailable = with_code(&envelope, "provider_unavailable");
    assert_eq!(unavailable.len(), 1, "{}", envelope.diagnostics());
    assert!(
        suggestion(unavailable[0]).contains("op"),
        "{}",
        unavailable[0]
    );
    let not_found = with_code(&envelope, "secret_not_found");
    assert_eq!(not_found.len(), 1, "{}", envelope.diagnostics());
    assert!(not_found[0]["message"]
        .as_str()
        .unwrap()
        .contains("native://db/missing"));
    suggestion(not_found[0]);

    // The CLI is there but its session expired.
    env.fake_exe(
        "op",
        "if [ \"$1\" = --version ]; then echo 2.30.0; exit 0; fi\necho 'not signed in' >&2; exit 1",
    );
    let (_, envelope) = doctor(&env, &[]);
    assert!(with_code(&envelope, "provider_unavailable").is_empty());
    let unauthorized = with_code(&envelope, "provider_unauthorized");
    assert_eq!(unauthorized.len(), 1, "{}", envelope.diagnostics());
    assert!(suggestion(unauthorized[0]).contains("sign in"));
}
