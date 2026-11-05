//! Pinned agent installs share sync's approval gate and local generations.
mod support;

use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};
use support::MusterEnv;

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (output, envelope) = env.run_json(&full);
    assert!(output.status.success(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn pin(env: &MusterEnv, version: &str) {
    let artifact = env.path().join("artifacts/claude");
    let script = format!("#!/bin/sh\necho 'claude {version}'\n");
    write(&artifact, &script);
    let sha256 = format!("{:x}", Sha256::digest(script.as_bytes()));
    write(
        &env.config_dir().join("muster.lock"),
        &format!(
            "[agents.claude]\nversion = {version:?}\nurl = \"file://{}\"\nsha256 = {sha256:?}\n",
            artifact.display()
        ),
    );
}

fn init(env: &MusterEnv) {
    env.fake_exe("claude", "#!/bin/sh\nexit 1\n");
    env.fake_exe("codex", "#!/bin/sh\nexit 1\n");
    ok(env, &["init", "--yes", "--machine", "laptop"]);
}

#[test]
fn agent_install_requires_approval_and_remembers_each_version() {
    let env = MusterEnv::new();
    init(&env);
    pin(&env, "1.0.0");
    let binary = env.path().join(".local/bin/claude");

    let plan = ok(&env, &["plan"]);
    assert!(plan["approvals"].as_array().unwrap().iter().any(|a| {
        a["kind"] == "agent"
            && a["name"] == "claude"
            && a["detail"]
                .as_str()
                .unwrap()
                .contains("not installed -> 1.0.0")
    }));
    ok(&env, &["sync"]);
    assert!(!binary.exists());

    let first = ok(&env, &["sync", "--yes"]);
    assert!(binary.is_file());
    let generation = first["generation"].as_u64().unwrap();

    let unchanged = ok(&env, &["sync"]);
    assert_eq!(unchanged["applied"], false);
    assert_eq!(unchanged["generation"], first["generation"]);

    // Even a previously approved binary is not reinstalled unattended.
    std::fs::remove_file(&binary).unwrap();
    ok(&env, &["sync"]);
    assert!(!binary.exists());
    let reinstall = ok(&env, &["sync", "--yes"]);
    assert!(binary.is_file());
    assert_eq!(reinstall["applied"], true);
    assert!(reinstall["generation"].as_u64().unwrap() > generation);
    assert!(reinstall["approvals"].as_array().unwrap().is_empty());

    // A changed version must pass the gate again, without replacing the old binary first.
    let previous = std::fs::read(&binary).unwrap();
    pin(&env, "2.0.0");
    ok(&env, &["sync"]);
    assert_eq!(std::fs::read(&binary).unwrap(), previous);
    ok(&env, &["sync", "--yes"]);
    assert_ne!(std::fs::read(&binary).unwrap(), previous);
}

#[test]
fn dry_run_lists_install_without_fetching_or_recording_approval() {
    let env = MusterEnv::new();
    init(&env);
    pin(&env, "1.0.0");
    // Fetching would fail: planning and dry-run must use the pin without reading its URL.
    std::fs::remove_file(env.path().join("artifacts/claude")).unwrap();
    let plan = ok(&env, &["--dry-run", "sync"]);
    assert_eq!(plan["installs"][0]["agent"], "claude");
    assert_eq!(plan["installs"][0]["version"], "1.0.0");
    assert_eq!(plan["dry_run"], true);
    assert!(!env.path().join(".local/bin/claude").exists());
    ok(&env, &["sync"]);
    // Approval does not bypass integrity or persist a failed install's approval.
    pin(&env, "1.0.0");
    write(&env.path().join("artifacts/claude"), "tampered");
    env.run(&["sync", "--yes"]).assert().code(1);
    assert!(!env.path().join(".local/bin/claude").exists());
    ok(&env, &["sync"]);
}

#[test]
fn pins_for_other_platforms_are_not_installed_or_approved() {
    let env = MusterEnv::new();
    init(&env);
    write(
        &env.config_dir().join("muster.lock"),
        &format!(
            "[agents.claude]\nversion = \"1.0.0\"\n[agents.claude.artifacts.other-platform]\nurl = \"file:///missing\"\nsha256 = \"{}\"\n",
            "0".repeat(64)
        ),
    );
    let (out, envelope) = env.run_json(&["--format", "json", "plan"]);
    assert!(out.status.success());
    assert!(envelope
        .raw
        .to_string()
        .contains("agent_platform_unsupported"));
    let plan = envelope.result();
    assert!(plan["installs"].as_array().unwrap().is_empty());
    assert!(plan["approvals"].as_array().unwrap().is_empty());
    let (out, envelope) = env.run_json(&["--format", "json", "sync"]);
    assert!(out.status.success());
    assert!(envelope
        .raw
        .to_string()
        .contains("agent_platform_unsupported"));
    assert!(!env.path().join(".local/bin/claude").exists());
}

#[test]
fn approval_detail_identifies_old_and_new_versions_source_and_checksum() {
    let env = MusterEnv::new();
    init(&env);
    pin(&env, "1.0.0");
    ok(&env, &["sync", "--yes"]);
    pin(&env, "2.0.0");
    let plan = ok(&env, &["plan"]);
    let detail = plan["approvals"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("claude"), "{detail}");
    assert!(detail.contains("1.0.0 -> 2.0.0"), "{detail}");
    assert!(
        detail.contains(&format!(
            "file://{}",
            env.path().join("artifacts/claude").display()
        )),
        "{detail}"
    );
    let hash = format!(
        "{:x}",
        Sha256::digest(std::fs::read(env.path().join("artifacts/claude")).unwrap())
    );
    assert!(detail.contains(&hash), "{detail}");
}

#[test]
fn unattended_agent_approval_does_not_block_config_secrets_or_verified_skills() {
    let env = MusterEnv::new();
    init(&env);
    let mut manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    manifest.push_str(
        r#"
[agents]
enabled = ["claude"]
[agents.instructions]
source = "AGENTS.md"
[agents.skills]
source = "skills"
[mcp.example]
command = "example"
[mcp.example.env]
TOKEN = "native://app/token"
[secrets."native://app/token"]
allow = ["claude"]
"#,
    );
    write(&env.config_dir().join("muster.toml"), &manifest);
    ok(&env, &["trust", "resign", "--yes"]);
    env.run(&["secret", "add", "app/token", "--value-stdin", "--yes"])
        .write_stdin("test-token")
        .assert()
        .success();
    write(&env.config_dir().join("AGENTS.md"), "Initial rules.\n");
    ok(&env, &["sync", "--yes"]); // Approve existing MCP before testing agents alone.
    pin(&env, "1.0.0");
    ok(&env, &["sync", "--yes"]);
    let previous = std::fs::read(env.path().join(".local/bin/claude")).unwrap();
    pin(&env, "2.0.0");
    write(&env.config_dir().join("AGENTS.md"), "Changed rules.\n");
    ok(&env, &["skill", "add", "review"]);
    std::fs::remove_file(env.path().join(".claude.json")).unwrap();
    let result = ok(&env, &["sync"]);
    assert_eq!(result["applied"], true);
    assert_eq!(
        std::fs::read(env.path().join(".local/bin/claude")).unwrap(),
        previous
    );
    assert!(
        std::fs::read_to_string(env.path().join(".claude/AGENTS.md"))
            .unwrap()
            .contains("Changed rules.")
    );
    assert!(std::fs::read_to_string(env.path().join(".claude.json"))
        .unwrap()
        .contains("test-token"));
    assert!(env.path().join(".claude/skills/review/SKILL.md").exists());
    let status = ok(&env, &["status"]);
    assert_eq!(
        status["convergence"]["pending_approvals"][0]["name"],
        "claude"
    );
    assert_eq!(status["sync"]["approval_needed"], true);
    assert_eq!(status["sync"]["pending_approvals"][0]["name"], "claude");
    // Existing executable-code gates remain strict, even alongside an agent approval.
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    write(
        &env.config_dir().join("muster.toml"),
        &(manifest + "\n[mcp.new]\ncommand = \"new-server\"\n"),
    );
    env.run(&["sync"]).assert().code(2);
}

#[test]
fn failed_agent_install_keeps_binary_and_continues_other_installs_config_and_skills() {
    let env = MusterEnv::new();
    init(&env);
    pin(&env, "1.0.0");
    ok(&env, &["sync", "--yes"]);
    let binary = env.path().join(".local/bin/claude");
    let previous = std::fs::read(&binary).unwrap();
    pin(&env, "2.0.0");
    write(&env.path().join("artifacts/claude"), "tampered");
    let lock = std::fs::read_to_string(env.config_dir().join("muster.lock")).unwrap();
    let other = "#!/bin/sh\necho codex 3.0.0\n";
    write(&env.path().join("artifacts/codex"), other);
    write(
        &env.config_dir().join("muster.lock"),
        &format!(
            "{lock}\n[agents.codex]\nversion = \"3.0.0\"\nurl = \"file://{}\"\nsha256 = \"{:x}\"\n",
            env.path().join("artifacts/codex").display(),
            Sha256::digest(other.as_bytes())
        ),
    );
    write(&env.config_dir().join("AGENTS.md"), "New rules.\n");
    ok(&env, &["skill", "add", "review"]);
    let (out, envelope) = env.run_json(&["--format", "json", "sync", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", envelope.raw);
    assert!(!envelope.ok());
    assert!(envelope.raw.to_string().contains("checksum mismatch"));
    assert_eq!(std::fs::read(binary).unwrap(), previous);
    assert!(env.path().join(".local/bin/codex").exists());
    assert!(env.path().join(".claude/skills/review/SKILL.md").exists());
    assert!(
        std::fs::read_to_string(env.path().join(".claude/AGENTS.md"))
            .unwrap()
            .contains("New rules.")
    );
    let status = ok(&env, &["status"]);
    assert_eq!(status["sync"]["result"], "error");
    assert_eq!(
        status["convergence"]["pending_approvals"][0]["name"],
        "claude"
    );
}

#[test]
fn newer_agent_is_kept_with_warning_and_older_agent_upgrades() {
    let env = MusterEnv::new();
    init(&env);
    ok(&env, &["sync", "--yes"]);
    env.fake_exe("claude", "#!/bin/sh\necho claude 3.0.0\n");
    pin(&env, "2.0.0");
    std::fs::remove_file(env.path().join("artifacts/claude")).unwrap();
    let binary = env.path().join(".local/bin/claude");
    let (out, envelope) = env.run_json(&["--format", "json", "sync", "--yes"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert!(envelope.raw.to_string().contains("agent_newer_than_pin"));
    assert!(envelope.result()["installs"].as_array().unwrap().is_empty());
    assert!(!binary.exists());
    let (out, envelope) = env.run_json(&["--format", "json", "agents", "install"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["installed"][0]["action"], "kept_newer");
    assert!(envelope.raw.to_string().contains("agent_newer_than_pin"));
    let status = ok(&env, &["status"]);
    assert_eq!(status["convergence"]["agents"][0]["kept_newer"], true);
    pin(&env, "4.0.0");
    ok(&env, &["sync", "--yes"]);
    assert!(std::fs::read_to_string(binary).unwrap().contains("4.0.0"));
}

#[test]
fn manual_install_keeps_newer_agent_without_platform_artifact() {
    let env = MusterEnv::new();
    init(&env);
    env.fake_exe("claude", "echo claude 3.0.0");
    write(
        &env.config_dir().join("muster.lock"),
        &format!("[agents.claude]\nversion = \"1.0.0\"\n[agents.claude.artifacts.other-platform]\nurl = \"file:///missing\"\nsha256 = \"{}\"\n", "0".repeat(64)),
    );
    let (out, envelope) = env.run_json(&["--format", "json", "agents", "install"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["installed"][0]["action"], "kept_newer");
    assert_eq!(
        envelope.result()["installed"][0]["installed_version"],
        "3.0.0"
    );
    assert!(envelope.result()["installed"][0]["sha256"].is_null());
    assert!(envelope.raw.to_string().contains("agent_newer_than_pin"));
    assert!(!env.path().join(".local/bin/claude").exists());
}
