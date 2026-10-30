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
