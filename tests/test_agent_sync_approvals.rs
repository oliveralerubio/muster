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
        a["kind"] == "agent" && a["name"] == "claude" && a["detail"] == "install claude 1.0.0"
    }));
    env.run(&["sync"]).assert().code(2);
    assert!(!binary.exists());

    let first = ok(&env, &["sync", "--yes"]);
    assert!(binary.is_file());
    let generation = first["generation"].as_u64().unwrap();
    let metadata = env.path().join(format!(
        ".local/state/muster/generations/{generation}/generation.json"
    ));
    let state: Value = serde_json::from_slice(&std::fs::read(metadata).unwrap()).unwrap();
    assert_eq!(state["approved"]["global"]["agents"]["claude"], "1.0.0");

    let unchanged = ok(&env, &["sync"]);
    assert_eq!(unchanged["applied"], false);
    assert_eq!(unchanged["generation"], first["generation"]);

    // Reinstalling an approved version is permitted unattended and records a generation.
    std::fs::remove_file(&binary).unwrap();
    let reinstall = ok(&env, &["sync"]);
    assert!(binary.is_file());
    assert_eq!(reinstall["applied"], true);
    assert!(reinstall["generation"].as_u64().unwrap() > generation);
    assert!(reinstall["approvals"].as_array().unwrap().is_empty());

    // A changed version must pass the gate again, without replacing the old binary first.
    let previous = std::fs::read(&binary).unwrap();
    pin(&env, "2.0.0");
    env.run(&["sync"]).assert().code(2);
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
    env.run(&["sync"]).assert().code(2);
    // Approval does not bypass integrity or persist a failed install's approval.
    pin(&env, "1.0.0");
    write(&env.path().join("artifacts/claude"), "tampered");
    env.run(&["sync", "--yes"]).assert().code(5);
    assert!(!env.path().join(".local/bin/claude").exists());
    env.run(&["sync"]).assert().code(2);
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
    let plan = ok(&env, &["plan"]);
    assert!(plan["installs"].as_array().unwrap().is_empty());
    assert!(plan["approvals"].as_array().unwrap().is_empty());
    ok(&env, &["sync"]);
    assert!(!env.path().join(".local/bin/claude").exists());
}
