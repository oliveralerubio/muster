//! Ticket #7 — agent adapters (Claude Code, Codex, OpenCode) and `muster agents`.
//!
//! RED phase: these tests describe the intended behaviour. They must fail until
//! `src/adapters/` and the `agents` command exist.
//!
//! Assumed surface (to be confirmed/refined by the implementer):
//! - `muster agents` lists adapters (text); `muster agents --format json` gives one envelope.
//! - `muster agents apply [--agent <name>]` renders managed files into $HOME.
//! - Adapter declarations (which agents are enabled) come from `muster.toml`.

mod support;

use serde_json::Value;
use support::MusterEnv;

/// Minimal config repo declaring the three adapters, shared AGENTS.md and one MCP stdio server.
fn seed_config(env: &MusterEnv) {
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).expect("config repo dir");
    std::fs::write(cfg.join("AGENTS.md"), "# Shared rules\n\nBe concise.\n").expect("AGENTS.md");
    std::fs::write(
        cfg.join("muster.toml"),
        r#"
version = 1

[agents]
enabled = ["claude", "codex", "opencode"]

[agents.instructions]
source = "AGENTS.md"

[mcp.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp.github.env]
GITHUB_TOKEN = "${GITHUB_TOKEN}"
"#,
    )
    .expect("write manifest");
}

fn severity_ok(v: &Value) -> bool {
    v.as_array().is_some()
        && v.as_array().unwrap().iter().all(|d| {
            d["severity"]
                .as_str()
                .map(|s| matches!(s, "error" | "warning" | "info"))
                .unwrap_or(false)
        })
}

// ---------------------------------------------------------------------------
// 1. `muster agents` listing
// ---------------------------------------------------------------------------

#[test]
fn agents_lists_adapters_with_capabilities_and_detection() {
    let env = MusterEnv::new();
    seed_config(&env);
    env.run(&["agents"])
        .assert()
        .code(0)
        .stdout(predicates::str::contains("Claude Code"))
        .stdout(predicates::str::contains("Codex"))
        .stdout(predicates::str::contains("OpenCode"))
        // Adapter declaration details required by the ticket.
        .stdout(predicates::str::contains("claude"))
        .stdout(predicates::str::contains("codex"))
        .stdout(predicates::str::contains("opencode"));
}

#[test]
fn agents_json_envelope_lists_adapters() {
    let env = MusterEnv::new();
    seed_config(&env);
    let (output, envelope) = env.run_json(&["agents", "--format", "json"]);
    assert!(output.status.success(), "agents must succeed");
    assert!(envelope.ok(), "envelope ok=true");
    assert_eq!(envelope.schema_version(), 1);
    assert_eq!(envelope.command(), "agents");

    let result = envelope.result();
    let adapters = result
        .get("agents")
        .or(result.get("adapters"))
        .expect("result.agents list");
    let list = adapters.as_array().expect("adapters must be a list");
    let names: Vec<&str> = list
        .iter()
        .filter_map(|a| a["id"].as_str().or(a["name"].as_str()))
        .collect();
    for expected in ["claude", "codex", "opencode"] {
        assert!(
            names.iter().any(|n| n.contains(expected)),
            "adapter {expected} must be listed: {names:?}"
        );
    }
    // Each adapter declares capabilities, scopes, managed files, platforms.
    let first = &list[0];
    assert!(
        first.get("capabilities").is_some() || first.get("files").is_some(),
        "adapter entries must declare capabilities/files: {first}"
    );
    assert!(
        severity_ok(envelope.diagnostics()),
        "diagnostics must use the envelope shape"
    );
}

#[test]
fn antigravity_replaces_gemini_in_agent_list() {
    let env = MusterEnv::new();
    seed_config(&env);
    let (output, envelope) = env.run_json(&["agents", "--format", "json"]);
    assert!(output.status.success(), "agents must succeed");
    assert!(envelope.ok(), "envelope ok=true");

    let list = envelope.result()["agents"]
        .as_array()
        .expect("result.agents list");

    let antigravity: Vec<&serde_json::Value> = list
        .iter()
        .filter(|a| a["id"].as_str() == Some("antigravity"))
        .collect();
    let antigravity: &serde_json::Value = antigravity
        .first()
        .expect("adapter with id \"antigravity\" must be listed in result.agents");
    assert_eq!(
        antigravity["name"].as_str(),
        Some("Antigravity CLI"),
        "antigravity adapter name"
    );
    assert_eq!(
        antigravity["experimental"].as_bool(),
        Some(false),
        "antigravity must not be experimental"
    );

    // Antigravity CLI replaced the previous Google agent CLI.
    assert!(
        list.iter().all(|a| a["id"].as_str() != Some("gemini")),
        "no adapter may keep id \"gemini\""
    );
    assert!(
        list.iter().all(|a| !a["name"]
            .as_str()
            .map(|n| n.contains("Gemini"))
            .unwrap_or(false)),
        "no adapter name may contain \"Gemini\""
    );

    let capabilities: Vec<&str> = antigravity["capabilities"]
        .as_array()
        .expect("capabilities list")
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    for expected in ["instructions", "mcp_stdio", "mcp_remote"] {
        assert!(
            capabilities.contains(&expected),
            "antigravity capabilities must include {expected}: {capabilities:?}"
        );
    }

    let user_paths: Vec<&str> = antigravity["files"]
        .as_array()
        .expect("files list")
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    for expected in [".gemini/AGENTS.md", ".gemini/config/mcp_config.json"] {
        assert!(
            user_paths.contains(&expected),
            "antigravity managed user files must include {expected}: {user_paths:?}"
        );
    }
    let project_paths: Vec<&str> = antigravity["files"]
        .as_array()
        .expect("files list")
        .iter()
        .filter_map(|f| f["project"].as_str())
        .collect();
    for expected in ["AGENTS.md", ".agents/mcp_config.json"] {
        assert!(
            project_paths.contains(&expected),
            "antigravity managed project files must include {expected}: {project_paths:?}"
        );
    }
}

fn seed_antigravity_config(env: &MusterEnv) {
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).expect("config repo dir");
    std::fs::write(cfg.join("AGENTS.md"), "Be concise.\n").expect("AGENTS.md");
    std::fs::write(
        cfg.join("muster.toml"),
        r#"
version = 1
[agents]
enabled = ["antigravity"]
[agents.instructions]
source = "AGENTS.md"
[mcp.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
[mcp.remote]
url = "https://example.com/mcp"
"#,
    )
    .expect("write manifest");
}

#[test]
fn antigravity_adapter_writes_mcp_config_with_server_url() {
    let env = MusterEnv::new();
    seed_antigravity_config(&env);
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    let path = env.path().join(".gemini/config/mcp_config.json");
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("valid JSON");
    assert_eq!(cfg["mcpServers"]["github"]["command"], "npx");
    let remote = &cfg["mcpServers"]["remote"];
    assert_eq!(remote["serverUrl"], "https://example.com/mcp");
    assert!(
        remote.get("url").is_none(),
        "remote must not use url: {remote}"
    );
    assert!(
        remote.get("httpUrl").is_none(),
        "remote must not use httpUrl: {remote}"
    );
}

#[test]
fn antigravity_adapter_preserves_unmanaged_mcp_keys() {
    let env = MusterEnv::new();
    seed_antigravity_config(&env);
    let dir = env.path().join(".gemini/config");
    std::fs::create_dir_all(&dir).expect("gemini config dir");
    std::fs::write(
        dir.join("mcp_config.json"),
        r#"{ "mcpServers": { "mine": { "command": "my-tool", "disabled": true } } }"#,
    )
    .expect("seed MCP config");
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("mcp_config.json")).expect("read"))
            .expect("valid JSON");
    assert_eq!(
        cfg["mcpServers"]["mine"],
        serde_json::json!({"command":"my-tool","disabled":true})
    );
    assert!(
        cfg["mcpServers"]["github"].is_object(),
        "github must be added: {cfg}"
    );
}

#[test]
fn antigravity_adapter_writes_global_rules() {
    let env = MusterEnv::new();
    seed_antigravity_config(&env);
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    let rules = std::fs::read_to_string(env.path().join(".gemini/AGENTS.md"))
        .expect("global AGENTS.md must exist");
    assert!(
        rules.contains("Be concise."),
        "global rules must be copied: {rules}"
    );
}

// ---------------------------------------------------------------------------
// 1b. Agent Skills (`[agents.skills]`)
// ---------------------------------------------------------------------------

fn seed_antigravity_skills_config(env: &MusterEnv, enabled: &str) {
    let cfg = env.config_dir();
    let skills = cfg.join("skills");
    std::fs::create_dir_all(skills.join("review")).expect("skills/review dir");
    std::fs::write(
        skills.join("review/SKILL.md"),
        "---\nname: review\ndescription: Review code\n---\nReview carefully.\n",
    )
    .expect("write SKILL.md");
    std::fs::write(skills.join("review/checklist.md"), "- tests\n").expect("write checklist.md");
    std::fs::write(cfg.join("AGENTS.md"), "Be concise.\n").expect("AGENTS.md");
    std::fs::write(
        cfg.join("muster.toml"),
        format!(
            r#"
version = 1
[agents]
enabled = ["{enabled}"]
[agents.instructions]
source = "AGENTS.md"
[agents.skills]
source = "skills"
"#
        ),
    )
    .expect("write manifest");
    env.run(&["skill", "pin", "review"]).assert().success();
}

#[test]
fn antigravity_adapter_installs_skills_from_config_repo() {
    let env = MusterEnv::new();
    seed_antigravity_skills_config(&env, "antigravity");
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    let base = env.path().join(".gemini/antigravity-cli/skills/review");
    assert_eq!(
        std::fs::read_to_string(base.join("SKILL.md")).expect("installed SKILL.md"),
        "---\nname: review\ndescription: Review code\n---\nReview carefully.\n"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("checklist.md")).expect("installed checklist.md"),
        "- tests\n"
    );
}

#[test]
fn antigravity_skills_keep_user_owned_skills() {
    let env = MusterEnv::new();
    seed_antigravity_skills_config(&env, "antigravity");
    let mine = env.path().join(".gemini/antigravity-cli/skills/mine");
    std::fs::create_dir_all(&mine).expect("user skill dir");
    std::fs::write(mine.join("SKILL.md"), "mine").expect("write user skill");
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    assert_eq!(
        std::fs::read_to_string(mine.join("SKILL.md")).expect("user skill must survive"),
        "mine",
        "user-owned skill must be left untouched"
    );
}

#[test]
fn opencode_supports_skills_without_unsupported_warning() {
    let env = MusterEnv::new();
    seed_antigravity_skills_config(&env, "opencode");
    let (output, envelope) = env.run_json(&[
        "agents", "apply", "--yes", "--agent", "opencode", "--format", "json",
    ]);
    assert!(output.status.success(), "apply must succeed");
    assert!(envelope.ok());
    let diags = envelope.diagnostics().as_array().expect("diagnostics list");
    assert!(
        !diags.iter().any(|d| d["code"] == "unsupported_feature"),
        "opencode supports skills: {diags:?}"
    );
    let installed = env.path().join(".config/opencode/skills/review/SKILL.md");
    assert!(installed.is_file(), "skill installed for opencode");
}

// ---------------------------------------------------------------------------
// 2. Format translation & non-destructive merge
// ---------------------------------------------------------------------------

#[test]
fn claude_adapter_writes_settings_mcp_and_links_claude_md() {
    let env = MusterEnv::new();
    seed_config(&env);
    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);

    let home = env.path();
    // Settings file exists.
    let settings = home.join(".claude/settings.json");
    assert!(settings.is_file(), "~/.claude/settings.json must exist");
    // MCP servers land in ~/.claude.json under mcpServers.
    let claude_json: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).expect("read"))
            .expect("valid JSON");
    assert!(
        claude_json["mcpServers"]["github"].is_object(),
        "github MCP server must be declared under mcpServers: {claude_json}"
    );
    // CLAUDE.md points to / includes AGENTS.md.
    let claude_md = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).expect("CLAUDE.md");
    assert!(
        claude_md.contains("AGENTS.md"),
        "CLAUDE.md must reference AGENTS.md: {claude_md}"
    );
}

#[test]
fn claude_adapter_preserves_unmanaged_keys_in_settings() {
    let env = MusterEnv::new();
    seed_config(&env);
    let claude_dir = env.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("claude dir");
    std::fs::write(
        claude_dir.join("settings.json"),
        r#"{ "theme": "dark", "permissions": { "allow": ["Bash(ls:*)"] } }"#,
    )
    .expect("seed settings");

    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);

    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(claude_dir.join("settings.json")).expect("read"),
    )
    .expect("valid JSON");
    assert_eq!(settings["theme"], "dark", "unmanaged key must survive");
    assert_eq!(
        settings["permissions"]["allow"][0], "Bash(ls:*)",
        "unmanaged nested keys must survive"
    );
}

#[test]
fn codex_adapter_writes_config_toml_with_mcp_server() {
    let env = MusterEnv::new();
    seed_config(&env);
    env.run(&["agents", "apply", "--agent", "codex", "--yes"])
        .assert()
        .code(0);

    let config = std::fs::read_to_string(env.path().join(".codex/config.toml")).expect("config");
    assert!(
        config.contains("[mcp_servers.github]"),
        "codex MCP server must be a TOML table: {config}"
    );
    assert!(config.contains("npx"), "command must be present: {config}");
}

#[test]
fn codex_adapter_preserves_unmanaged_toml_sections() {
    let env = MusterEnv::new();
    seed_config(&env);
    let codex_dir = env.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).expect("codex dir");
    std::fs::write(
        codex_dir.join("config.toml"),
        "model = \"gpt-5.1-codex\"\napproval_policy = \"on-request\"\n\n[mcp_servers.manual]\ncommand = \"uvx\"\n",
    )
    .expect("seed config");

    env.run(&["agents", "apply", "--agent", "codex", "--yes"])
        .assert()
        .code(0);

    let config = std::fs::read_to_string(codex_dir.join("config.toml")).expect("read");
    assert!(
        config.contains("model = \"gpt-5.1-codex\""),
        "unmanaged top-level key must survive: {config}"
    );
    assert!(
        config.contains("approval_policy = \"on-request\""),
        "unmanaged key must survive: {config}"
    );
    assert!(
        config.contains("[mcp_servers.manual]"),
        "unmanaged MCP section must survive: {config}"
    );
    assert!(
        config.contains("[mcp_servers.github]"),
        "managed MCP section must be added: {config}"
    );
}

#[test]
fn opencode_adapter_writes_opencode_json_with_mcp_key() {
    let env = MusterEnv::new();
    seed_config(&env);
    env.run(&["agents", "apply", "--agent", "opencode", "--yes"])
        .assert()
        .code(0);

    let path = env.path().join(".config/opencode/opencode.json");
    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("valid JSON");
    let github = &cfg["mcp"]["github"];
    assert!(github.is_object(), "mcp.github must exist: {cfg}");
    assert_eq!(github["type"], "local", "stdio server type");
    assert!(
        github["command"]
            .as_array()
            .map(|c| c[0] == "npx")
            .unwrap_or(false),
        "local command must be a string array: {github}"
    );
}

#[test]
fn opencode_adapter_preserves_unmanaged_json_keys() {
    let env = MusterEnv::new();
    seed_config(&env);
    let dir = env.path().join(".config/opencode");
    std::fs::create_dir_all(&dir).expect("opencode dir");
    std::fs::write(
        dir.join("opencode.json"),
        r#"{ "$schema": "https://opencode.ai/config.json", "model": "anthropic/claude-sonnet-4" }"#,
    )
    .expect("seed config");

    env.run(&["agents", "apply", "--agent", "opencode", "--yes"])
        .assert()
        .code(0);

    let cfg: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("opencode.json")).expect("read"))
            .expect("valid JSON");
    assert_eq!(
        cfg["model"], "anthropic/claude-sonnet-4",
        "unmanaged key must survive"
    );
    assert!(
        cfg["mcp"]["github"].is_object(),
        "managed mcp must be added"
    );
}

// ---------------------------------------------------------------------------
// 3. Unsupported option handling
// ---------------------------------------------------------------------------

#[test]
fn unsupported_feature_produces_diagnostic_but_still_applies_supported_parts() {
    let env = MusterEnv::new();
    seed_config(&env);
    // Declare a hook: Claude Code supports them; Codex/OpenCode do not.
    let cfg = env.config_dir();
    std::fs::write(
        cfg.join("muster.toml"),
        r#"
version = 1

[agents]
enabled = ["claude", "codex", "opencode"]

[agents.hooks.pre_tool_use]
command = "lint"

[mcp.github]
command = "npx"
"#,
    )
    .expect("write manifest");

    let (output, envelope) = env.run_json(&[
        "agents", "apply", "--yes", "--agent", "codex", "--format", "json",
    ]);
    assert!(
        output.status.success(),
        "apply must succeed even with untranslated features"
    );
    assert!(envelope.ok());
    let diags = envelope.diagnostics().as_array().expect("diagnostics list");
    assert!(
        diags.iter().any(|d| {
            let sev = d["severity"].as_str().unwrap_or("");
            (sev == "warning" || sev == "info")
                && d["message"]
                    .as_str()
                    .map(|m| {
                        m.to_lowercase().contains("hook")
                            || m.to_lowercase().contains("not support")
                    })
                    .unwrap_or(false)
        }),
        "must warn that hooks are unsupported for codex: {diags:?}"
    );
    // Supported parts (MCP) are still written.
    let config = std::fs::read_to_string(env.path().join(".codex/config.toml")).expect("config");
    assert!(config.contains("[mcp_servers.github]"));
}

// ---------------------------------------------------------------------------
// 4. Failure isolation
// ---------------------------------------------------------------------------

#[test]
fn failure_in_one_adapter_does_not_block_the_others() {
    let env = MusterEnv::new();
    seed_config(&env);
    // Make ~/.claude impossible to use as a directory: a plain file is in the way.
    std::fs::write(env.path().join(".claude"), "not a directory").expect("blocker");

    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(
        output.status.success(),
        "apply must not fail wholesale; per-adapter failures are diagnostics"
    );
    let diags = envelope.diagnostics().as_array().expect("diagnostics list");
    assert!(
        diags.iter().any(|d| d["severity"] == "error"
            && d["message"]
                .as_str()
                .map(|m| m.to_lowercase().contains("claude"))
                .unwrap_or(false)),
        "must report the claude failure as an error diagnostic: {diags:?}"
    );
    // The other adapters still succeeded.
    assert!(
        env.path().join(".codex/config.toml").is_file(),
        "codex output must exist despite claude failing"
    );
    assert!(
        env.path().join(".config/opencode/opencode.json").is_file(),
        "opencode output must exist despite claude failing"
    );
}

// ---------------------------------------------------------------------------
// 5. Snapshots of the generated files, per adapter
// ---------------------------------------------------------------------------

/// Every canonical feature at once: instructions, stdio + remote MCP, per-agent
/// models, permissions and a hook.
fn seed_full_config(env: &MusterEnv) {
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).expect("config repo dir");
    std::fs::write(cfg.join("AGENTS.md"), "# Shared rules\n\nBe concise.\n").expect("AGENTS.md");
    std::fs::write(
        cfg.join("muster.toml"),
        r#"
version = 1

[agents]
enabled = ["claude", "codex", "opencode", "antigravity"]

[agents.models]
claude = "opus"
codex = "gpt-5.1-codex"
opencode = "anthropic/claude-sonnet-4"

[agents.permissions]
allow = ["Bash(ls:*)"]
deny = ["Read(./.env)"]

[agents.hooks.pre_tool_use]
matcher = "Bash"
command = "lint"

[mcp.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }

[mcp.docs]
url = "https://mcp.example.com/mcp"
headers = { X-Team = "core" }
"#,
    )
    .expect("write manifest");
}

#[test]
fn generated_files_match_snapshots_per_adapter() {
    let env = MusterEnv::new();
    seed_full_config(&env);
    env.run(&["agents", "apply", "--yes"]).assert().code(0);

    let read = |rel: &str| {
        std::fs::read_to_string(env.path().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    };
    for (name, rel) in [
        ("claude_settings_json", ".claude/settings.json"),
        ("claude_user_json", ".claude.json"),
        ("claude_claude_md", ".claude/CLAUDE.md"),
        ("claude_agents_md", ".claude/AGENTS.md"),
        ("codex_config_toml", ".codex/config.toml"),
        ("codex_agents_md", ".codex/AGENTS.md"),
        ("opencode_opencode_json", ".config/opencode/opencode.json"),
        ("opencode_agents_md", ".config/opencode/AGENTS.md"),
        (
            "antigravity_mcp_config_json",
            ".gemini/config/mcp_config.json",
        ),
        ("antigravity_agents_md", ".gemini/AGENTS.md"),
    ] {
        insta::assert_snapshot!(name, read(rel));
    }
}

// ---------------------------------------------------------------------------
// 6. Dry run & secret references hygiene
// ---------------------------------------------------------------------------

#[test]
fn dry_run_does_not_modify_disk() {
    let env = MusterEnv::new();
    seed_config(&env);
    env.run(&["agents", "apply", "--dry-run", "--format", "json"])
        .assert()
        .code(0);

    let home = env.path();
    assert!(
        !home.join(".claude").exists(),
        ".claude must not be created"
    );
    assert!(
        !home.join(".claude.json").exists(),
        ".claude.json must not exist"
    );
    assert!(!home.join(".codex").exists(), ".codex must not be created");
    assert!(
        !home.join(".config/opencode").exists(),
        ".config/opencode must not be created"
    );
}

#[test]
fn secret_references_in_mcp_env_are_never_written_as_plaintext() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).expect("config repo dir");
    std::fs::write(
        cfg.join("muster.toml"),
        r#"
version = 1

[agents]
enabled = ["claude", "codex", "opencode"]

[secrets."native://core/api_key"]
allow = ["claude", "codex", "opencode"]

[mcp.vault_service]
command = "npx"
env = { SECRET_KEY = "native://core/api_key", PLAIN = "allowed" }
"#,
    )
    .expect("write manifest");

    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(output.status.success());
    assert!(envelope.ok());

    let diags = envelope.diagnostics().as_array().expect("diagnostics");
    assert!(
        diags.iter().any(|d| {
            d["severity"] == "warning"
                && d["code"] == "secret_reference_unresolved"
                && d["message"].as_str().unwrap().contains("SECRET_KEY")
        }),
        "must warn about unresolved secret reference: {diags:?}"
    );

    let home = env.path();
    let claude = std::fs::read_to_string(home.join(".claude.json")).expect("claude.json");
    assert!(
        !claude.contains("SECRET_KEY"),
        "must not write secret reference"
    );
    assert!(claude.contains("PLAIN"), "plain env is still written");

    let codex = std::fs::read_to_string(home.join(".codex/config.toml")).expect("codex config");
    assert!(!codex.contains("SECRET_KEY"));
    assert!(codex.contains("PLAIN"));
}

#[test]
fn reapplying_changes_nothing_and_keeps_hand_written_instructions() {
    let env = MusterEnv::new();
    seed_full_config(&env);
    let claude_md = env.path().join(".claude/CLAUDE.md");
    std::fs::create_dir_all(claude_md.parent().unwrap()).expect("claude dir");
    std::fs::write(&claude_md, "# My own notes\n").expect("seed CLAUDE.md");

    env.run(&["agents", "apply", "--yes"]).assert().code(0);
    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(output.status.success());
    for agent in envelope.result()["agents"].as_array().expect("agents") {
        for file in agent["files"].as_array().expect("files") {
            assert_eq!(
                file["changed"], false,
                "second apply must be a no-op: {file}"
            );
        }
    }
    let text = std::fs::read_to_string(&claude_md).expect("CLAUDE.md");
    assert!(
        text.starts_with("# My own notes\n"),
        "hand-written text kept: {text}"
    );
    assert_eq!(
        text.matches("@AGENTS.md").count(),
        1,
        "one managed import: {text}"
    );
}

#[test]
fn unparsable_native_file_fails_only_that_adapter_and_is_left_untouched() {
    let env = MusterEnv::new();
    seed_config(&env);
    let dir = env.path().join(".config/opencode");
    std::fs::create_dir_all(&dir).expect("opencode dir");
    std::fs::write(dir.join("opencode.json"), "{ not json").expect("seed");

    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(output.status.success());
    let diags = envelope.diagnostics().as_array().expect("diagnostics");
    assert!(
        diags
            .iter()
            .any(|d| d["severity"] == "error" && d["path"] == "opencode"),
        "opencode failure reported: {diags:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("opencode.json")).unwrap(),
        "{ not json",
        "broken user file must not be overwritten"
    );
    assert!(env.path().join(".codex/config.toml").is_file());
}

#[test]
fn claude_runtime_helper_removes_plaintext_and_defers_env_resolution() {
    let env = MusterEnv::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(
        env.config_dir().join("muster.toml"),
        r#"
version = 1
[agents]
enabled = ["claude"]
[agents.providers]
claude = "gateway"
[providers.gateway]
kind = "anthropic"
key = "env://MUSTER_RUNTIME_KEY"
api_key_helper = true
[secrets."env://MUSTER_RUNTIME_KEY"]
allow = ["claude"]
"#,
    )
    .unwrap();
    let path = env.path().join(".claude/settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"old-test-token","ANTHROPIC_API_KEY":"old-test-key","KEEP":"value"}}"#).unwrap();
    for args in [
        vec!["plan", "--format", "json"],
        vec!["agents", "apply", "--yes", "--format", "json"],
    ] {
        let output = env
            .run(&args)
            .env_remove("MUSTER_RUNTIME_KEY")
            .assert()
            .success()
            .get_output()
            .clone();
        assert!(!String::from_utf8_lossy(&output.stdout).contains("old-test-"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("old-test-"));
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let settings: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        settings["apiKeyHelper"],
        format!("muster secret get 'env://MUSTER_RUNTIME_KEY' --reveal --yes --format text --config '{}' --agent claude", env.config_dir().display())
    );
    assert!(settings["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
    assert!(settings["env"].get("ANTHROPIC_API_KEY").is_none());
    assert_eq!(settings["env"]["KEEP"], "value");
    assert!(!text.contains("old-test-"));

    env.run(&[
        "secret",
        "get",
        "env://MUSTER_RUNTIME_KEY",
        "--reveal",
        "--yes",
        "--format",
        "text",
    ])
    .env("MUSTER_RUNTIME_KEY", "runtime-test-value")
    .assert()
    .success()
    .stdout("runtime-test-value\n");
    let manifest_path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(
        &manifest_path,
        manifest.replace("allow = [\"claude\"]", "allow = []"),
    )
    .unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(settings.get("apiKeyHelper").is_none());
}

#[cfg(unix)]
#[test]
fn claude_helper_command_uses_the_original_config_repository() {
    let env = MusterEnv::new();
    let config = env.path().join("custom config's vault");
    let config_arg = config.to_str().unwrap();
    env.run(&[
        "init",
        "--config",
        config_arg,
        "--machine",
        "helper-test",
        "--yes",
    ])
    .assert()
    .success();
    env.run(&[
        "secret",
        "add",
        "app/key",
        "--config",
        config_arg,
        "--value-stdin",
        "--yes",
    ])
    .write_stdin("runtime-native-value")
    .assert()
    .success();
    let manifest_path = config.join("muster.toml");
    let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
    manifest.push_str(
        r#"
[agents]
enabled = ["claude"]
[agents.providers]
claude = "gateway"
[providers.gateway]
kind = "anthropic"
key = "native://app/key"
api_key_helper = true
[secrets."native://app/key"]
allow = ["claude"]
"#,
    );
    std::fs::write(&manifest_path, manifest).unwrap();
    env.run(&["agents", "apply", "--config", config_arg, "--yes"])
        .assert()
        .success();
    let settings: Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let helper = settings["apiKeyHelper"].as_str().unwrap();
    std::os::unix::fs::symlink(
        assert_cmd::cargo::cargo_bin("muster"),
        env.bin.join("muster"),
    )
    .unwrap();
    let output = std::process::Command::new("sh")
        .args(["-c", helper])
        .env(
            "PATH",
            format!("{}:{}", env.bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOME", env.path())
        .env("XDG_STATE_HOME", env.path().join(".local/state"))
        .env("MUSTER_CONFIG", env.config_dir())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"runtime-native-value\n");
}

#[test]
fn claude_default_provider_preserves_a_custom_helper() {
    let env = MusterEnv::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(
        env.config_dir().join("muster.toml"),
        r#"
version = 1
[agents]
enabled = ["claude"]
[agents.providers]
claude = "gateway"
[providers.gateway]
kind = "anthropic"
key = "env://MUSTER_RUNTIME_KEY"
[secrets."env://MUSTER_RUNTIME_KEY"]
allow = ["claude"]
"#,
    )
    .unwrap();
    let path = env.path().join(".claude/settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"apiKeyHelper":"custom-credential-helper"}"#).unwrap();
    env.run(&["agents", "apply", "--yes"])
        .env("MUSTER_RUNTIME_KEY", "static-test-value")
        .assert()
        .success();
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(settings["apiKeyHelper"], "custom-credential-helper");
    assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "static-test-value");
}

#[test]
fn claude_approval_helper_has_a_short_cache_and_keeps_legacy_cleanup() {
    let env = MusterEnv::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    let manifest = env.config_dir().join("muster.toml");
    let text = "version = 1\n[agents]\nenabled = ['claude']\n[agents.providers]\nclaude = 'gateway'\n[providers.gateway]\nkind = 'anthropic'\nkey = 'env://MUSTER_LEASE_HELPER_KEY'\napi_key_helper = true\n[secrets.'env://MUSTER_LEASE_HELPER_KEY']\nallow = ['claude']\napproval = 'each'\napproval_ttl = '30s'\n";
    std::fs::write(&manifest, text).unwrap();
    let settings_path = env.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    // A phase-one command must still be recognized as managed.
    std::fs::write(&settings_path, r#"{"apiKeyHelper":"muster secret get 'env://OLD' --reveal --yes --format text --config '/old'"}"#).unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert!(settings["apiKeyHelper"]
        .as_str()
        .unwrap()
        .ends_with("--agent claude"));
    assert_eq!(
        settings["env"]["CLAUDE_CODE_API_KEY_HELPER_TTL_MS"],
        "30000"
    );
    let denied = env
        .run(&[
            "secret",
            "get",
            "env://MUSTER_LEASE_HELPER_KEY",
            "--agent",
            "claude",
            "--reveal",
            "--yes",
            "--format",
            "json",
        ])
        .env("MUSTER_LEASE_HELPER_KEY", "helper-lease-private-value")
        .output()
        .unwrap();
    assert_eq!(denied.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&denied.stdout).contains("secret_approval_required"));
    assert!(!String::from_utf8_lossy(&denied.stdout).contains("helper-lease-private-value"));
    assert!(!String::from_utf8_lossy(&denied.stderr).contains("helper-lease-private-value"));
    std::fs::write(&manifest, text.replace("allow = ['claude']", "allow = []")).unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert!(settings.get("apiKeyHelper").is_none());
    assert!(settings["env"]
        .get("CLAUDE_CODE_API_KEY_HELPER_TTL_MS")
        .is_none());
}

#[test]
fn instruction_overlays_match_snapshots() {
    let env = MusterEnv::new();
    seed_full_config(&env);
    std::fs::write(
        env.config_dir().join("AGENTS.claude.md"),
        "Claude-only instruction\n",
    )
    .unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    for (name, path) in [
        ("claude_overlay", ".claude/AGENTS.md"),
        ("codex_without_claude_overlay", ".codex/AGENTS.md"),
    ] {
        insta::assert_snapshot!(
            name,
            std::fs::read_to_string(env.path().join(path)).unwrap()
        );
    }
}

// ---------------------------------------------------------------------------
// Generic per-adapter settings (`[agents.settings.<adapter>]`)
// ---------------------------------------------------------------------------

fn write_manifest(env: &MusterEnv, body: &str) {
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).expect("config repo dir");
    std::fs::write(cfg.join("AGENTS.md"), "# Shared rules\n").expect("AGENTS.md");
    std::fs::write(cfg.join("muster.toml"), body).expect("write manifest");
}

fn read_claude_settings(env: &MusterEnv) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".claude/settings.json")).expect("read"),
    )
    .expect("valid JSON")
}

const OUTPUT_STYLE_MANIFEST: &str = r#"
version = 1

[agents]
enabled = ["claude"]

[agents.instructions]
source = "AGENTS.md"

[agents.models]
claude = "opus"

[agents.hooks.stop]
command = "echo done"

[agents.settings.claude]
outputStyle = "Proactive"
"#;

#[test]
fn claude_settings_block_renders_and_coexists_with_other_keys() {
    let env = MusterEnv::new();
    write_manifest(&env, OUTPUT_STYLE_MANIFEST);
    let claude_dir = env.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("claude dir");
    std::fs::write(claude_dir.join("settings.json"), r#"{ "theme": "dark" }"#).expect("seed");

    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);

    let settings = read_claude_settings(&env);
    assert_eq!(settings["outputStyle"], "Proactive");
    assert_eq!(settings["theme"], "dark", "unmanaged key must survive");
    assert_eq!(settings["model"], "opus", "managed model must coexist");
    assert!(settings["hooks"]["Stop"].is_array(), "{settings}");
}

#[test]
fn claude_settings_block_is_pruned_when_removed_and_keeps_user_keys() {
    let env = MusterEnv::new();
    write_manifest(&env, OUTPUT_STYLE_MANIFEST);
    let claude_dir = env.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("claude dir");
    std::fs::write(claude_dir.join("settings.json"), r#"{ "theme": "dark" }"#).expect("seed");
    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);
    assert_eq!(read_claude_settings(&env)["outputStyle"], "Proactive");

    write_manifest(
        &env,
        "version = 1\n\n[agents]\nenabled = [\"claude\"]\n\n[agents.instructions]\nsource = \"AGENTS.md\"\n",
    );
    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);

    let settings = read_claude_settings(&env);
    assert!(
        settings.get("outputStyle").is_none(),
        "stale key: {settings}"
    );
    assert!(settings.get("model").is_none(), "stale model: {settings}");
    assert_eq!(settings["theme"], "dark", "user key must survive pruning");
}

#[test]
fn claude_settings_block_rejects_keys_muster_manages() {
    let env = MusterEnv::new();
    write_manifest(
        &env,
        r#"
version = 1

[agents]
enabled = ["claude"]

[agents.settings.claude]
apiKeyHelper = "evil"
"#,
    );
    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .failure();
    assert!(!env.path().join(".claude/settings.json").exists());
}
