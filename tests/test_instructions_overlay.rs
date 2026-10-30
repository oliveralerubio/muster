mod support;
use support::MusterEnv;

#[test]
fn overlay_is_only_appended_to_its_adapter() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = [\"claude\", \"codex\"]\n",
    )
    .unwrap();
    std::fs::write(cfg.join("AGENTS.md"), "Shared rule\n").unwrap();
    std::fs::write(cfg.join("AGENTS.claude.md"), "Claude rule\n").unwrap();
    env.run(&["agents", "apply"]).assert().success();
    let claude = std::fs::read_to_string(env.path().join(".claude/AGENTS.md")).unwrap();
    let codex = std::fs::read_to_string(env.path().join(".codex/AGENTS.md")).unwrap();
    assert!(
        claude.contains("Shared rule\n\n## AGENTS.claude.md\n\nClaude rule"),
        "{claude}"
    );
    assert!(!codex.contains("Claude rule"), "{codex}");
}

#[test]
fn unknown_overlay_warns() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude']\n",
    )
    .unwrap();
    std::fs::write(cfg.join("AGENTS.md"), "Shared\n").unwrap();
    std::fs::write(cfg.join("AGENTS.typo.md"), "Extra\n").unwrap();
    let (output, result) = env.run_json(&["plan", "--format", "json"]);
    assert!(output.status.success());
    assert!(result
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "instructions_overlay_unknown_agent"));
}

#[test]
fn skipped_instructions_win_over_overlay_and_warn() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude']\nskip_instructions = ['claude']\n",
    )
    .unwrap();
    std::fs::write(cfg.join("AGENTS.md"), "Shared\n").unwrap();
    std::fs::write(cfg.join("AGENTS.claude.md"), "Extra\n").unwrap();
    let (output, result) = env.run_json(&["agents", "apply", "--format", "json"]);
    assert!(output.status.success());
    assert!(result
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "instructions_overlay_skipped"));
    assert!(!env.path().join(".claude/AGENTS.md").exists());
}
