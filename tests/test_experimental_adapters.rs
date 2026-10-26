//! Experimental adapters use only formats verified in upstream and installed CLIs.
mod support;

use serde_json::Value;
use support::MusterEnv;
use yaml_rust2::{Yaml, YamlLoader};

#[test]
fn machine_profile_skips_only_user_instructions() {
    let env = MusterEnv::new();
    seed(&env);
    let path = env.config_dir().join("muster.toml");
    let mut manifest = std::fs::read_to_string(&path).unwrap();
    manifest.push_str("\n[machines.laptop]\nrecipient = 'age1laptop'\nplatform = 'linux-x86_64'\nmuster_version = '0.1.0'\nprofile = 'personal'\n[profiles.personal.agents]\nskip_instructions = ['hermes']\n");
    std::fs::write(&path, manifest).unwrap();
    write(&env, ".local/state/muster/machine", "laptop\n");
    write(&env, ".hermes/SOUL.md", "My own notes.\n");
    let (output, plan) = env.run_json(&["plan", "--format", "json"]);
    assert!(output.status.success(), "{:?}", plan.raw);
    assert!(!plan.result().to_string().contains("SOUL.md"));
    apply(&env);
    assert_eq!(read(&env, ".hermes/SOUL.md"), "My own notes.\n");
    assert!(read(&env, ".pi/agent/AGENTS.md").contains("Shared rules."));
    let config = yaml(&env);
    assert_eq!(
        config["model"]["default"].as_str(),
        Some("anthropic/claude-sonnet-4")
    );
    assert_eq!(
        config["mcp_servers"]["local"]["command"].as_str(),
        Some("npx")
    );
    assert!(env.path().join(".hermes/skills/review/SKILL.md").is_file());
    write(&env, ".local/state/muster/machine", "other\n");
    apply(&env);
    assert!(read(&env, ".hermes/SOUL.md").contains("Shared rules."));
    write(&env, ".local/state/muster/machine", "laptop\n");
    apply(&env);
    assert_eq!(read(&env, ".hermes/SOUL.md"), "My own notes.\n");
}

#[test]
fn skipping_instructions_removes_created_file_and_leaves_project_scope() {
    let env = MusterEnv::new();
    seed(&env);
    apply(&env);
    assert!(env.path().join(".hermes/SOUL.md").is_file());
    let path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&path).unwrap().replace(
        "enabled = [\"pi\", \"hermes\", \"codex\"]",
        "enabled = [\"pi\", \"hermes\", \"codex\"]\nskip_instructions = [\"hermes\"]",
    );
    std::fs::write(&path, manifest).unwrap();
    apply(&env);
    assert!(!env.path().join(".hermes/SOUL.md").exists());
    let project = env.path().join("project");
    std::fs::create_dir_all(project.join(".muster")).unwrap();
    std::fs::write(
        project.join(".muster/muster.toml"),
        "version = 1\n[agents]\nenabled = ['hermes']\nskip_instructions = ['hermes']\n",
    )
    .unwrap();
    std::fs::write(project.join(".muster/AGENTS.md"), "Project rules.\n").unwrap();
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .success();
    assert!(std::fs::read_to_string(project.join("AGENTS.md"))
        .unwrap()
        .contains("Project rules."));
}

fn seed(env: &MusterEnv) {
    let cfg = env.config_dir();
    std::fs::create_dir_all(cfg.join("skills/review/refs")).unwrap();
    std::fs::create_dir_all(cfg.join("subagents")).unwrap();
    std::fs::write(cfg.join("AGENTS.md"), "Shared rules.\n").unwrap();
    std::fs::write(
        cfg.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review code\n---\nCheck behavior.\n",
    )
    .unwrap();
    std::fs::write(cfg.join("skills/review/refs/check.md"), "Run tests.\n").unwrap();
    std::fs::write(
        cfg.join("subagents/reviewer.md"),
        "---\nname: reviewer\ndescription: Review code\n---\nReview carefully.\n",
    )
    .unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        r#"
version = 1
[agents]
enabled = ["pi", "hermes", "codex"]
[agents.instructions]
source = "AGENTS.md"
[agents.skills]
source = "skills"
[agents.subagents]
source = "subagents"
[agents.models]
pi = "anthropic/claude-sonnet-4"
hermes = "anthropic/claude-sonnet-4"
[agents.permissions]
deny = ["Read(.env)"]
[agents.hooks.pre_tool_use]
command = "lint"
[mcp.local]
command = "npx"
args = ["-y", "example-mcp"]
env = { PLAIN = "ok", DENIED = "env://MUSTER_TEST_DENIED", MISSING = "env://MUSTER_TEST_MISSING" }
[mcp.remote]
url = "https://example.com/mcp"
headers = { X-Team = "core", DENIED = "env://MUSTER_TEST_DENIED" }
[secrets."env://MUSTER_TEST_MISSING"]
allow = ["pi", "hermes"]
"#,
    )
    .unwrap();
    env.run(&["skill", "pin", "review"]).assert().success();
}

fn apply(env: &MusterEnv) -> support::Envelope {
    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(output.status.success());
    assert!(envelope.ok());
    envelope
}

fn read(env: &MusterEnv, path: &str) -> String {
    std::fs::read_to_string(env.path().join(path)).unwrap()
}
fn json(env: &MusterEnv, path: &str) -> Value {
    serde_json::from_str(&read(env, path)).unwrap()
}
fn yaml(env: &MusterEnv) -> Yaml {
    YamlLoader::load_from_str(&read(env, ".hermes/config.yaml"))
        .unwrap()
        .remove(0)
}
fn write(env: &MusterEnv, path: &str, text: &str) {
    let path = env.path().join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn experimental_adapters_are_available_detected_and_opt_in() {
    let env = MusterEnv::new();
    env.fake_exe("pi", "echo 1.2.0");
    env.fake_exe("hermes", "echo Hermes");
    let (_, envelope) = support::json(env.run_isolated(&["agents", "--format", "json"]));
    let list = envelope.result()["agents"].as_array().unwrap();
    for id in ["pi", "hermes"] {
        let agent = list.iter().find(|a| a["id"] == id).unwrap();
        assert_eq!(agent["experimental"], true);
        assert_eq!(agent["available"], true);
        assert_eq!(agent["detected"], true);
        assert_eq!(agent["enabled"], false);
        assert_eq!(
            agent["capabilities"],
            serde_json::json!([
                "instructions",
                "mcp_stdio",
                "mcp_remote",
                "models",
                "skills"
            ])
        );
    }
    env.link_system_exe("git");
    support::json(env.run_isolated(&["init", "--yes", "--format", "json"]));
    let (_, status) = support::json(env.run_isolated(&["status", "--format", "json"]));
    let machine = &status.result()["machines"][0];
    assert_eq!(
        machine["experimental_agents"],
        serde_json::json!(["pi", "hermes"])
    );
    let (_, after_init) = support::json(env.run_isolated(&["agents", "--format", "json"]));
    for a in after_init.result()["agents"].as_array().unwrap() {
        if a["experimental"] == true {
            assert_eq!(a["enabled"], false);
        }
    }
}

#[test]
fn render_apply_and_reapply_preserve_user_configuration() {
    let env = MusterEnv::new();
    seed(&env);
    write(
        &env,
        ".pi/agent/mcp.json",
        r#"{"autoEnableCodemode": false, "mcpServers": {"manual": {"command": "mine"}}}"#,
    );
    write(
        &env,
        ".pi/agent/settings.json",
        r#"{"theme":"dark", "compaction":{"enabled":false}}"#,
    );
    write(&env, ".hermes/config.yaml", "model:\n  provider: openrouter\nterminal:\n  backend: docker\nmcp_servers:\n  manual:\n    command: mine\n");
    for path in [".pi/agent/AGENTS.md", ".hermes/SOUL.md"] {
        write(&env, path, "My own notes.\n");
    }
    for root in [".pi/agent/skills", ".hermes/skills"] {
        write(&env, &format!("{root}/mine/SKILL.md"), "Mine.\n");
    }
    apply(&env);
    let pi = json(&env, ".pi/agent/mcp.json");
    assert_eq!(pi["autoEnableCodemode"], false);
    assert_eq!(pi["mcpServers"]["manual"]["command"], "mine");
    assert_eq!(pi["mcpServers"]["local"]["command"], "npx");
    assert_eq!(
        pi["mcpServers"]["local"]["args"],
        serde_json::json!(["-y", "example-mcp"])
    );
    assert_eq!(pi["mcpServers"]["remote"]["url"], "https://example.com/mcp");
    assert_eq!(pi["mcpServers"]["remote"]["headers"]["X-Team"], "core");
    let settings = json(&env, ".pi/agent/settings.json");
    assert_eq!(settings["defaultProvider"], "anthropic");
    assert_eq!(settings["defaultModel"], "claude-sonnet-4");
    assert_eq!(settings["theme"], "dark");
    assert_eq!(settings["compaction"]["enabled"], false);
    let hermes = yaml(&env);
    assert_eq!(
        hermes["model"]["default"].as_str(),
        Some("anthropic/claude-sonnet-4")
    );
    assert_eq!(hermes["model"]["provider"].as_str(), Some("openrouter"));
    assert_eq!(hermes["terminal"]["backend"].as_str(), Some("docker"));
    assert_eq!(
        hermes["mcp_servers"]["manual"]["command"].as_str(),
        Some("mine")
    );
    assert_eq!(
        hermes["mcp_servers"]["local"]["command"].as_str(),
        Some("npx")
    );
    assert_eq!(
        hermes["mcp_servers"]["local"]["args"][1].as_str(),
        Some("example-mcp")
    );
    assert_eq!(
        hermes["mcp_servers"]["remote"]["url"].as_str(),
        Some("https://example.com/mcp")
    );
    assert_eq!(
        hermes["mcp_servers"]["remote"]["headers"]["X-Team"].as_str(),
        Some("core")
    );
    for path in [".pi/agent/AGENTS.md", ".hermes/SOUL.md"] {
        let text = read(&env, path);
        assert!(text.starts_with("My own notes.\n"));
        assert!(text.contains("Shared rules."));
    }
    for root in [".pi/agent/skills", ".hermes/skills"] {
        assert_eq!(
            read(&env, &format!("{root}/review/refs/check.md")),
            "Run tests.\n"
        );
        assert_eq!(read(&env, &format!("{root}/mine/SKILL.md")), "Mine.\n");
        assert_eq!(
            read(&env, &format!("{root}/review/SKILL.md")),
            std::fs::read_to_string(env.config_dir().join("skills/review/SKILL.md")).unwrap()
        );
    }
    let second = apply(&env);
    for agent in second.result()["agents"].as_array().unwrap() {
        for file in agent["files"].as_array().unwrap() {
            assert_eq!(file["changed"], false);
        }
    }
}

#[test]
fn unsupported_features_and_unresolved_or_denied_credentials_warn() {
    let env = MusterEnv::new();
    seed(&env);
    let result = apply(&env);
    let diagnostics = result.diagnostics().as_array().unwrap();
    for id in ["pi", "hermes"] {
        for feature in ["hooks", "permissions", "subagents"] {
            assert!(diagnostics
                .iter()
                .any(|d| d["code"] == "unsupported_feature"
                    && d["path"] == id
                    && d["message"].as_str().unwrap().contains(feature)));
        }
        assert!(diagnostics
            .iter()
            .any(|d| d["code"] == "secret_not_allowed_for_agent"
                && d["message"].as_str().unwrap().to_lowercase().contains(id)));
        assert!(diagnostics
            .iter()
            .any(|d| d["code"] == "secret_reference_unresolved"
                && d["message"].as_str().unwrap().to_lowercase().contains(id)));
    }
    for path in [".pi/agent/mcp.json", ".hermes/config.yaml"] {
        let text = read(&env, path);
        assert!(!text.contains("DENIED"));
        assert!(!text.contains("MISSING"));
        assert!(!text.contains("env://"));
        assert!(text.contains("PLAIN"));
    }
    assert!(!env.path().join(".pi/agent/agents").exists());
    assert!(!env.path().join(".hermes/agents").exists());
}

#[test]
fn malformed_native_files_fail_only_their_adapter() {
    for (id, path, broken, other) in [
        (
            "pi",
            ".pi/agent/mcp.json",
            "{ broken",
            ".hermes/config.yaml",
        ),
        (
            "pi",
            ".pi/agent/settings.json",
            "[1]",
            ".hermes/config.yaml",
        ),
        (
            "hermes",
            ".hermes/config.yaml",
            "model: [",
            ".pi/agent/mcp.json",
        ),
        (
            "hermes",
            ".hermes/config.yaml",
            "- not-a-mapping",
            ".pi/agent/mcp.json",
        ),
        (
            "hermes",
            ".hermes/config.yaml",
            "mcp_servers: broken",
            ".pi/agent/mcp.json",
        ),
        (
            "hermes",
            ".hermes/config.yaml",
            "model: {}\n---\nmodel: {}",
            ".pi/agent/mcp.json",
        ),
    ] {
        let env = MusterEnv::new();
        seed(&env);
        write(&env, path, broken);
        let result = apply(&env);
        assert!(result
            .diagnostics()
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "adapter_failed" && d["path"] == id));
        assert_eq!(read(&env, path), broken);
        assert!(env.path().join(other).is_file());
        assert!(!env
            .path()
            .join(if id == "pi" {
                ".pi/agent/AGENTS.md"
            } else {
                ".hermes/SOUL.md"
            })
            .exists());
    }
}

#[test]
fn blocked_agent_directory_does_not_block_other_agents() {
    for blocked in [".pi", ".hermes"] {
        let env = MusterEnv::new();
        seed(&env);
        write(&env, blocked, "not a directory");
        let result = apply(&env);
        assert!(result
            .diagnostics()
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "adapter_failed"));
        assert!(env.path().join(".codex/config.toml").is_file());
    }
}

#[test]
fn stale_yaml_mcp_and_model_keys_are_pruned_but_manual_keys_survive() {
    let env = MusterEnv::new();
    seed(&env);
    write(&env, ".hermes/config.yaml", "model:\n  provider: openrouter\nmcp_servers:\n  manual:\n    command: mine\nterminal:\n  backend: docker\n");
    apply(&env);
    std::fs::write(
        env.config_dir().join("muster.toml"),
        "version = 1\n[agents]\nenabled = [\"hermes\"]\n",
    )
    .unwrap();
    apply(&env);
    let config = yaml(&env);
    assert_eq!(config["model"]["default"], Yaml::BadValue);
    assert_eq!(config["model"]["provider"].as_str(), Some("openrouter"));
    assert_eq!(config["mcp_servers"]["local"], Yaml::BadValue);
    assert_eq!(config["mcp_servers"]["remote"], Yaml::BadValue);
    assert_eq!(
        config["mcp_servers"]["manual"]["command"].as_str(),
        Some("mine")
    );
    assert_eq!(config["terminal"]["backend"].as_str(), Some("docker"));
    assert!(!env.path().join(".hermes/skills/review").exists());
    std::fs::write(
        env.config_dir().join("muster.toml"),
        "version = 1\n[agents]\nenabled = []\n",
    )
    .unwrap();
    apply(&env);
    assert_eq!(
        yaml(&env)["mcp_servers"]["manual"]["command"].as_str(),
        Some("mine")
    );
}

#[test]
fn project_scope_uses_confirmed_paths_and_warns_for_hermes_user_only_settings() {
    let env = MusterEnv::new();
    seed(&env);
    let project = env.path().join("project");
    std::fs::create_dir_all(project.join(".muster")).unwrap();
    for rel in [
        "muster.toml",
        "AGENTS.md",
        "skills/review/SKILL.md",
        "skills/review/refs/check.md",
        "subagents/reviewer.md",
    ] {
        let dest = project.join(".muster").join(rel);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(env.config_dir().join(rel), dest).unwrap();
    }
    let (output, result) = env.run_json(&[
        "sync",
        "--project",
        project.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    assert!(result
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "unsupported_feature"
            && d["path"] == "hermes"
            && d["message"].as_str().unwrap().contains("user-scoped")));
    for rel in [
        ".pi/mcp.json",
        ".pi/settings.json",
        "AGENTS.md",
        ".pi/skills/review/SKILL.md",
        ".hermes/skills/review/SKILL.md",
    ] {
        assert!(project.join(rel).is_file(), "{rel}");
    }
    assert_eq!(
        std::fs::read_to_string(project.join("AGENTS.md"))
            .unwrap()
            .matches("Shared rules.")
            .count(),
        1
    );
    assert!(!project.join(".hermes/config.yaml").exists());
    assert!(!env.path().join(".hermes/config.yaml").exists());
    assert!(!env.path().join(".pi/agent").exists());
}

#[test]
fn dry_run_leaves_agent_directories_absent() {
    let env = MusterEnv::new();
    seed(&env);
    env.run(&["agents", "apply", "--dry-run"])
        .assert()
        .success();
    assert!(!env.path().join(".pi").exists());
    assert!(!env.path().join(".hermes").exists());
}

#[test]
fn yaml_merge_preserves_manual_block_entries_comments_and_unicode() {
    let env = MusterEnv::new();
    seed(&env);
    let manual = "# Ajustes propios: café\nterminal:\n  # Keep shell preferences\n  backend: docker\n  enabled: yes\n";
    write(&env, ".hermes/config.yaml", &format!("{manual}model:\n  # Provider chosen by the user\n  provider: openrouter\nmcp_servers:\n  manual:\n    command: mine # hand-written\n...\n"));
    apply(&env);
    let text = read(&env, ".hermes/config.yaml");
    assert!(
        text.starts_with(manual),
        "unmanaged YAML must survive byte-exactly: {text}"
    );
    assert!(text.contains("  # Provider chosen by the user\n  provider: openrouter\n"));
    assert!(text.contains("  manual:\n    command: mine # hand-written\n"));
    assert!(text.ends_with("...\n"));
    assert_eq!(
        yaml(&env)["mcp_servers"]["local"]["command"].as_str(),
        Some("npx")
    );
    let second = apply(&env);
    assert_eq!(read(&env, ".hermes/config.yaml"), text);
    assert!(!second
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "adapter_failed"));
}

#[test]
fn legacy_hermes_model_and_flow_yaml_remain_valid() {
    for original in [
        "model: old-model\nterminal: {backend: docker}\n",
        "{model: {provider: openrouter}, terminal: {backend: docker}}\n",
        "mcp_servers:\n  local: &shared\n    command: old\n  manual: *shared\n",
    ] {
        let env = MusterEnv::new();
        seed(&env);
        write(&env, ".hermes/config.yaml", original);
        let result = apply(&env);
        assert!(!result
            .diagnostics()
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "adapter_failed"));
        let config = yaml(&env);
        assert_eq!(
            config["model"]["default"].as_str(),
            Some("anthropic/claude-sonnet-4")
        );
        if original.contains("manual") {
            assert_eq!(
                config["mcp_servers"]["manual"]["command"].as_str(),
                Some("old")
            );
        } else {
            assert_eq!(config["terminal"]["backend"].as_str(), Some("docker"));
        }
    }
}

#[test]
fn disabling_experimental_adapters_removes_only_files_they_created() {
    let env = MusterEnv::new();
    seed(&env);
    apply(&env);
    std::fs::write(
        env.config_dir().join("muster.toml"),
        "version = 1\n[agents]\nenabled = []\n",
    )
    .unwrap();
    apply(&env);
    for path in [
        ".pi/agent/mcp.json",
        ".pi/agent/settings.json",
        ".pi/agent/AGENTS.md",
        ".hermes/config.yaml",
        ".hermes/SOUL.md",
        ".pi/agent/skills/review/SKILL.md",
        ".hermes/skills/review/SKILL.md",
    ] {
        assert!(!env.path().join(path).exists(), "{path}");
    }
}

#[test]
fn pure_render_is_deterministic_for_both_experimental_adapters() {
    use muster::adapters::{self, config::AgentsConfig, Current, Desired};
    let agents = AgentsConfig::default();
    let mcp = std::collections::BTreeMap::new();
    let secrets = std::collections::BTreeMap::new();
    let skills = std::collections::BTreeMap::new();
    let subagents = std::collections::BTreeMap::new();
    let access = muster::secrets::SecretAccess::default();
    let desired = Desired {
        config_dir: std::path::Path::new("/config"),
        ai_providers: &Default::default(),
        agents: &agents,
        mcp: &mcp,
        instructions: Some("Follow the rules.\n"),
        secrets: &secrets,
        access: &access,
        skills: &skills,
        subagents: &subagents,
    };
    for id in ["pi", "hermes"] {
        let adapter = adapters::find(id).unwrap();
        let first = adapter.render(&desired, &Current::new()).unwrap();
        assert!(first.diagnostics.is_empty());
        assert!(first
            .files
            .iter()
            .any(|f| f.contents.contains("Follow the rules.")));
        let current = first
            .files
            .iter()
            .map(|f| (f.file.path, f.contents.clone()))
            .collect();
        let again = adapter.render(&desired, &current).unwrap();
        assert_eq!(again.files, first.files);
    }
}
