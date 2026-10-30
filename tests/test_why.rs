mod support;
use support::MusterEnv;

#[test]
fn searches_sources_and_native_content_with_agent_and_regex_filters() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude', 'codex']\n",
    )
    .unwrap();
    std::fs::write(cfg.join("AGENTS.md"), "Common Needle\n").unwrap();
    std::fs::write(cfg.join("AGENTS.claude.md"), "Claude needle\n").unwrap();
    std::fs::create_dir_all(env.path().join(".codex")).unwrap();
    std::fs::write(env.path().join(".codex/AGENTS.md"), "Outside NEEDLE\n<!-- BEGIN MUSTER MANAGED BLOCK: edits inside are overwritten -->\nInside needle\n<!-- END MUSTER MANAGED BLOCK -->\n").unwrap();
    let (output, result) = env.run_json(&["why", "needle", "--agent", "codex", "--format", "json"]);
    assert!(output.status.success());
    let matches = result.result()["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 3);
    assert!(matches
        .iter()
        .any(|m| m["excerpt"] == "Outside NEEDLE" && m["managed"] == false));
    assert!(matches
        .iter()
        .any(|m| m["excerpt"] == "Inside needle" && m["managed"] == true));
    assert!(matches
        .iter()
        .all(|m| m["agents"] == serde_json::json!(["codex"])));
    let (output, result) = env.run_json(&["why", "claude.*needle", "--regex", "--format", "json"]);
    assert!(output.status.success());
    assert_eq!(result.result()["matches"].as_array().unwrap().len(), 1);
    env.run(&["why", "[", "--regex"]).assert().failure();
}

#[test]
fn searches_installed_skills_and_subagents() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude']\n",
    )
    .unwrap();
    let skill = env.path().join(".claude/skills/example");
    let agent = env.path().join(".claude/agents");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(skill.join("SKILL.md"), "Skill searchable\n").unwrap();
    std::fs::write(agent.join("helper.md"), "Agent searchable\n").unwrap();
    let (output, result) = env.run_json(&["why", "searchable", "--format", "json"]);
    assert!(output.status.success());
    let matches = result.result()["matches"].as_array().unwrap();
    assert!(matches.iter().any(|m| m["kind"] == "skill"));
    assert!(matches.iter().any(|m| m["kind"] == "subagent"));
}

#[test]
fn rejects_overlong_patterns() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude']\n",
    )
    .unwrap();
    let long = "a".repeat(2000);
    env.run(&["why", &long, "--regex"]).assert().failure();
    env.run(&["why", &long]).assert().failure();
}

/// The machine's effective agents (team -> group -> member -> machine), not
/// the raw team list, are what a project without its own `enabled` searches.
#[test]
fn project_search_inherits_effective_agents_from_global_layers() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude', 'codex']\n[profiles.solo.agents]\nenabled = ['codex']\n[machines.box]\nrecipient = 'age1box'\nplatform = 'linux-x86_64'\nmuster_version = '0.1.0'\nprofile = 'solo'\n",
    )
    .unwrap();
    let state = env.path().join(".local/state/muster");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("machine"), "box\n").unwrap();
    let project = env.path().join("project");
    let source = project.join(".muster");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("muster.toml"), "version = 1\n").unwrap();
    std::fs::write(source.join("AGENTS.md"), "Project needle\n").unwrap();
    std::fs::write(source.join("AGENTS.claude.md"), "Claude needle\n").unwrap();
    std::fs::write(source.join("AGENTS.codex.md"), "Codex needle\n").unwrap();
    let (output, result) = env.run_json(&[
        "why",
        "needle",
        "--project",
        project.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    let matches = result.result()["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    assert!(
        matches
            .iter()
            .all(|m| m["agents"] == serde_json::json!(["codex"])),
        "{matches:?}"
    );
    assert!(matches
        .iter()
        .any(|m| m["kind"] == "instructions" && m["layer"] == "project"));
    // Only one enabled agent: the overlay is rendered into AGENTS.md, so it
    // is active and reported.
    assert!(matches.iter().any(|m| m["kind"] == "overlay"));
}

/// With several enabled agents sharing one project AGENTS.md, only the
/// overlays the engine actually renders are reported (claude's goes into
/// CLAUDE.md; the shared-file ones are skipped).
#[test]
fn project_search_skips_overlays_the_engine_cannot_render() {
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
    std::fs::write(source.join("AGENTS.md"), "Project needle\n").unwrap();
    std::fs::write(source.join("AGENTS.claude.md"), "Claude needle\n").unwrap();
    std::fs::write(source.join("AGENTS.codex.md"), "Codex needle\n").unwrap();
    let (output, result) = env.run_json(&[
        "why",
        "needle",
        "--project",
        project.to_str().unwrap(),
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    let matches = result.result()["matches"].as_array().unwrap();
    assert!(
        matches
            .iter()
            .any(|m| m["excerpt"] == "Project needle" && m["layer"] == "project"),
        "{matches:?}"
    );
    assert!(
        matches.iter().any(|m| m["excerpt"] == "Claude needle"),
        "claude overlay is rendered into the project CLAUDE.md: {matches:?}"
    );
    assert!(
        !matches.iter().any(|m| m["excerpt"] == "Codex needle"),
        "the codex overlay is not written to the shared project AGENTS.md: {matches:?}"
    );
}

#[test]
fn text_output_lists_file_line_layer_agents_and_excerpt() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude', 'codex']\n",
    )
    .unwrap();
    std::fs::write(cfg.join("AGENTS.md"), "Common needle\n").unwrap();
    let output = env
        .run(&["why", "needle", "--format", "text"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!(
        "{}:1  [team, claude, codex]  Common needle",
        cfg.join("AGENTS.md").display()
    );
    assert!(stdout.lines().any(|l| l == expected), "{stdout}");
}

/// An overlay without a canonical AGENTS.md is still searched by `why` and
/// rendered by the engine under its own heading.
#[test]
fn overlay_without_canonical_agents_md_is_still_active() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['codex']\n",
    )
    .unwrap();
    std::fs::write(cfg.join("AGENTS.codex.md"), "Codex only rule\n").unwrap();
    let (output, result) = env.run_json(&["why", "only rule", "--format", "json"]);
    assert!(output.status.success());
    let matches = result.result()["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["kind"], "overlay");
    assert_eq!(matches[0]["agents"], serde_json::json!(["codex"]));
    env.run(&["agents", "apply"]).assert().success();
    let native = std::fs::read_to_string(env.path().join(".codex/AGENTS.md")).unwrap();
    assert!(
        native.contains("## AGENTS.codex.md\n\nCodex only rule"),
        "{native}"
    );
}
