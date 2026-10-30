mod support;

use support::MusterEnv;

#[test]
fn effective_reports_layers_and_instruction_source() {
    let env = MusterEnv::new();
    env.run(&["--format", "json", "--yes", "init"])
        .assert()
        .success();
    let config = env.config_dir();
    std::fs::write(config.join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.instructions]\nsource = \"AGENTS.md\"\n").unwrap();
    std::fs::write(config.join("AGENTS.md"), "Be kind.\n").unwrap();
    let (output, envelope) = env.run_json(&["effective", "--format", "json"]);
    assert!(output.status.success());
    assert_eq!(envelope.command(), "effective");
    assert_eq!(
        envelope.result()["layers"],
        serde_json::json!([{ "kind": "team", "name": "team" }])
    );
    assert_eq!(
        envelope.result()["instructions"][0]["source"],
        config.join("AGENTS.md").to_str().unwrap()
    );
    assert_eq!(envelope.result()["instructions"][0]["layer"], "team");
    assert_eq!(envelope.result()["instructions"][0]["approx_tokens"], 2);
}

#[test]
fn effective_reports_instruction_disk_states() {
    let env = MusterEnv::new();
    env.run(&["--format", "json", "--yes", "init"])
        .assert()
        .success();
    let config = env.config_dir();
    std::fs::write(config.join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\", \"codex\"]\nskip_instructions = [\"codex\"]\n[agents.instructions]\nsource = \"AGENTS.md\"\n").unwrap();
    std::fs::write(config.join("AGENTS.md"), "Be kind.\n").unwrap();
    let (_, before) = env.run_json(&["effective", "--format", "json"]);
    assert_eq!(
        before.result()["instructions"][0]["adapters"]["claude"]["state"],
        "missing"
    );
    assert_eq!(
        before.result()["instructions"][0]["adapters"]["codex"]["state"],
        "skipped"
    );
    env.run(&["--format", "json", "--yes", "sync"])
        .assert()
        .success();
    let (_, synced) = env.run_json(&["effective", "--format", "json"]);
    assert_eq!(
        synced.result()["instructions"][0]["adapters"]["claude"]["state"],
        "in_sync"
    );
    std::fs::write(env.path().join(".claude/AGENTS.md"), "Changed\n").unwrap();
    let (_, changed) = env.run_json(&["effective", "--format", "json"]);
    assert_eq!(
        changed.result()["instructions"][0]["adapters"]["claude"]["state"],
        "drifted"
    );
}

#[test]
fn effective_reports_skills_and_installation() {
    let env = MusterEnv::new();
    env.run(&["--format", "json", "--yes", "init"])
        .assert()
        .success();
    let config = env.config_dir();
    std::fs::write(
        config.join("muster.toml"),
        "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.skills]\nsource = \"skills\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(config.join("skills/demo")).unwrap();
    std::fs::write(
        config.join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: Helpful skill\n---\nHello\n",
    )
    .unwrap();
    let (_, result) = env.run_json(&["effective", "--format", "json"]);
    let skill = &result.result()["skills"][0];
    assert_eq!(skill["name"], "demo");
    assert_eq!(skill["verification"], "unreviewed");
    assert_eq!(skill["description_tokens"], 3);
    assert_eq!(skill["body_tokens"], 13);
    assert_eq!(skill["adapters"]["claude"]["state"], "missing");
}

#[test]
fn effective_reports_skill_disk_changes_and_extras() {
    let env = MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let config = env.config_dir();
    std::fs::write(config.join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.skills]\nsource = \"skills\"\n").unwrap();
    std::fs::create_dir_all(config.join("skills/demo")).unwrap();
    let source = "---\nname: demo\ndescription: Helpful skill\n---\nHello\n";
    std::fs::write(config.join("skills/demo/SKILL.md"), source).unwrap();
    let base = env.path().join(".claude/skills");
    std::fs::create_dir_all(base.join("demo")).unwrap();
    std::fs::create_dir_all(base.join("personal")).unwrap();
    std::fs::write(base.join("demo/SKILL.md"), source).unwrap();
    let (_, result) = env.run_json(&["effective", "--format", "json"]);
    assert_eq!(result.result()["skills"][0]["adapters"]["claude"]["state"], "installed");
    assert_eq!(result.result()["extra_on_disk"]["claude"], serde_json::json!(["personal"]));
    std::fs::write(base.join("demo/SKILL.md"), "changed").unwrap();
    let (_, result) = env.run_json(&["effective", "--format", "json"]);
    assert_eq!(result.result()["skills"][0]["adapters"]["claude"]["state"], "drifted");
}

#[test]
fn effective_reports_mcp_without_secret_values() {
    let env = MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    std::fs::write(env.config_dir().join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\"]\n[mcp.tool]\ncommand = \"tool\"\n[mcp.tool.env]\nTOKEN = \"native://app/token\"\n").unwrap();
    let (output, result) = env.run_json(&["effective", "--format", "json"]);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("super-secret-value"));
    assert_eq!(result.result()["mcp"][0]["name"], "tool");
    assert_eq!(result.result()["mcp"][0]["layer"], "team");
    assert_eq!(result.result()["mcp"][0]["overridden_layers"], serde_json::json!([]));
    assert_eq!(result.result()["mcp"][0]["transport"], "stdio");
    assert_eq!(result.result()["mcp"][0]["agents"], serde_json::json!(["claude"]));
    assert_eq!(result.result()["mcp"][0]["adapters"]["claude"]["state"], "missing");
}

#[test]
fn effective_reports_subagent_state() {
    let env = MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    std::fs::write(env.config_dir().join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.subagents]\nsource = \"agents\"\n").unwrap();
    std::fs::create_dir_all(env.config_dir().join("agents")).unwrap();
    let source = "---\nname: reviewer\ndescription: Review changes\n---\nReview.\n";
    std::fs::write(env.config_dir().join("agents/reviewer.md"), source).unwrap();
    let (_, result) = env.run_json(&["effective", "--format", "json"]);
    assert_eq!(result.result()["subagents"][0]["name"], "reviewer");
    assert_eq!(result.result()["subagents"][0]["layer"], "team");
    assert_eq!(result.result()["subagents"][0]["adapters"]["claude"]["state"], "missing");
    std::fs::create_dir_all(env.path().join(".claude/agents")).unwrap();
    std::fs::write(env.path().join(".claude/agents/reviewer.md"), source).unwrap();
    let (_, result) = env.run_json(&["effective", "--format", "json"]);
    assert_eq!(result.result()["subagents"][0]["adapters"]["claude"]["state"], "in_sync");
}

#[test]
fn effective_lists_project_layer() {
    let env = MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let project = env.path().join("project");
    std::fs::create_dir_all(project.join(".muster")).unwrap();
    std::fs::write(project.join(".muster/muster.toml"), "version = 1\n[mcp.local]\ncommand = \"project-tool\"\n").unwrap();
    let (_, result) = env.run_json(&["effective", "--project", project.to_str().unwrap(), "--format", "json"]);
    assert_eq!(result.result()["layers"][1]["kind"], "project");
    assert_eq!(result.result()["mcp"][0]["name"], "local");
    assert_eq!(result.result()["mcp"][0]["layer"], "project");
}

#[test]
fn effective_filters_adapters_and_rejects_unknown() {
    let env = MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    std::fs::write(env.config_dir().join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\", \"codex\"]\n[agents.instructions]\nsource = \"AGENTS.md\"\n").unwrap();
    std::fs::write(env.config_dir().join("AGENTS.md"), "Hello\n").unwrap();
    let (_, result) = env.run_json(&["effective", "--agent", "claude", "--format", "json"]);
    assert_eq!(result.result()["instructions"][0]["adapters"].as_object().unwrap().keys().collect::<Vec<_>>(), vec!["claude"]);
    env.run(&["effective", "--agent", "unknown"]).assert().code(2);
}
