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

#[test]
fn project_overlay_stays_out_of_shared_instructions() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude', 'codex']\n",
    )
    .unwrap();
    let project = env.path().join("project");
    let source = project.join(".muster");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("muster.toml"), "version = 1\n").unwrap();
    std::fs::write(source.join("AGENTS.md"), "Shared rule\n").unwrap();
    std::fs::write(source.join("AGENTS.claude.md"), "Claude rule\n").unwrap();
    std::fs::write(source.join("AGENTS.codex.md"), "Codex rule\n").unwrap();
    let (_, result) = env.run_json(&[
        "sync",
        "--project",
        project.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(result.ok(), "{:?}", result.raw);
    let shared = std::fs::read_to_string(project.join("AGENTS.md")).unwrap();
    let claude = std::fs::read_to_string(project.join("CLAUDE.md")).unwrap();
    assert!(shared.contains("Shared rule"));
    assert!(!shared.contains("Claude rule") && !shared.contains("Codex rule"));
    assert!(claude.contains("Claude rule"));
    assert!(result
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "instructions_overlay_shared_project_file"));
}

#[test]
fn project_overlay_can_use_an_unshared_root_file() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['codex']\n",
    )
    .unwrap();
    let project = env.path().join("project");
    let source = project.join(".muster");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("muster.toml"), "version = 1\n").unwrap();
    std::fs::write(source.join("AGENTS.md"), "Shared\n").unwrap();
    std::fs::write(source.join("AGENTS.codex.md"), "Codex only\n").unwrap();
    env.run(&["sync", "--project", project.to_str().unwrap()])
        .assert()
        .success();
    assert!(std::fs::read_to_string(project.join("AGENTS.md"))
        .unwrap()
        .contains("Codex only"));
}
