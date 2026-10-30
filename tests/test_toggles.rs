mod support;
use support::MusterEnv;

#[test]
fn invalid_toggle_kind_is_a_config_error() {
    let env = MusterEnv::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(
        env.config_dir().join("muster.toml"),
        "version = 1\n[agents.toggles]\ndisable = [\"invalid:pdf\"]\n",
    )
    .unwrap();
    let (output, envelope) = env.run_json(&["plan", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(envelope
        .diagnostics()
        .to_string()
        .contains("unknown toggle kind"));
}

#[test]
fn disabled_skill_is_removed_from_each_adapter() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(dir.join("skills/pdf")).unwrap();
    std::fs::write(
        dir.join("skills/pdf/SKILL.md"),
        "---\nname: pdf\ndescription: PDF\n---\nPDF\n",
    )
    .unwrap();
    let manifest = "version = 1\n[agents]\nenabled = [\"claude\", \"codex\"]\n[agents.skills]\nsource = \"skills\"\n";
    std::fs::write(dir.join("muster.toml"), manifest).unwrap();
    env.run(&["skill", "pin", "pdf", "--yes"])
        .assert()
        .success();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    for path in [".claude/skills/pdf/SKILL.md", ".agents/skills/pdf/SKILL.md"] {
        assert!(env.path().join(path).exists());
    }
    std::fs::write(
        dir.join("muster.toml"),
        format!("{manifest}\n[agents.toggles]\ndisable = [\"skill:pdf\"]\n"),
    )
    .unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    for path in [".claude/skills/pdf/SKILL.md", ".agents/skills/pdf/SKILL.md"] {
        assert!(!env.path().join(path).exists());
    }
}

#[test]
fn adapter_specific_toggle_only_disables_that_adapter() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(dir.join("skills/pdf")).unwrap();
    std::fs::write(
        dir.join("skills/pdf/SKILL.md"),
        "---\nname: pdf\ndescription: PDF\n---\nPDF\n",
    )
    .unwrap();
    std::fs::write(dir.join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\", \"codex\"]\n[agents.skills]\nsource = \"skills\"\n[agents.toggles]\ndisable = [\"skill:pdf@claude\"]\n").unwrap();
    env.run(&["skill", "pin", "pdf", "--yes"])
        .assert()
        .success();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    assert!(!env.path().join(".claude/skills/pdf/SKILL.md").exists());
    assert!(env.path().join(".agents/skills/pdf/SKILL.md").exists());
}

#[test]
fn disabled_mcp_is_absent_from_native_configuration() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.toggles]\ndisable = [\"mcp:figma\"]\n[mcp.figma]\ncommand = \"figma\"\n").unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let text = std::fs::read_to_string(env.path().join(".claude.json")).unwrap_or_default();
    assert!(!text.contains("figma"));
}

#[test]
fn plan_reports_disabled_items_and_layer() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.toggles]\ndisable = [\"mcp:figma\"]\n[mcp.figma]\ncommand = \"figma\"\n").unwrap();
    let (output, envelope) = env.run_json(&["plan", "--format", "json"]);
    assert!(output.status.success());
    assert_eq!(
        envelope.result()["disabled"],
        serde_json::json!([{"kind":"mcp","name":"figma","agent":"claude","layer":"team"}])
    );
}

#[test]
fn disable_and_enable_edit_manifest_idempotently() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("muster.toml"),
        "version = 1\n# keep this comment\n",
    )
    .unwrap();
    env.run(&["disable", "skill:pdf", "--yes"])
        .assert()
        .success();
    let first = std::fs::read_to_string(dir.join("muster.toml")).unwrap();
    env.run(&["disable", "skill:pdf", "--yes"])
        .assert()
        .success();
    assert_eq!(
        first,
        std::fs::read_to_string(dir.join("muster.toml")).unwrap()
    );
    env.run(&["enable", "skill:pdf", "--yes"])
        .assert()
        .success();
    let text = std::fs::read_to_string(dir.join("muster.toml")).unwrap();
    assert!(text.contains("# keep this comment"));
    assert!(text.contains("enable = [\"skill:pdf\"]"));
    assert!(!text.contains("disable = [\"skill:pdf\"]"));
}

#[test]
fn disabling_unreviewed_skill_removes_its_previous_copy() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(dir.join("skills/pdf")).unwrap();
    let skill = dir.join("skills/pdf/SKILL.md");
    std::fs::write(&skill, "---\nname: pdf\ndescription: PDF\n---\nPDF\n").unwrap();
    let manifest =
        "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.skills]\nsource = \"skills\"\n";
    std::fs::write(dir.join("muster.toml"), manifest).unwrap();
    env.run(&["skill", "pin", "pdf", "--yes"])
        .assert()
        .success();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    std::fs::write(skill, "---\nname: pdf\ndescription: PDF\n---\nChanged\n").unwrap();
    std::fs::write(
        dir.join("muster.toml"),
        format!("{manifest}\n[agents.toggles]\ndisable = [\"skill:pdf\"]\n"),
    )
    .unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    assert!(!env.path().join(".claude/skills/pdf/SKILL.md").exists());
}

#[test]
fn project_enable_overrides_team_disable() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.toggles]\ndisable = [\"mcp:figma\"]\n").unwrap();
    let project = env.path().join("project");
    std::fs::create_dir_all(project.join(".muster")).unwrap();
    let project_manifest = "version = 1\n[agents.toggles]\nenable = [\"mcp:figma\"]\n[mcp.figma]\ncommand = \"figma\"\n";
    std::fs::write(project.join(".muster/muster.toml"), project_manifest).unwrap();
    let project_arg = project.to_str().unwrap();
    let (_, enabled) = env.run_json(&["plan", "--project", project_arg, "--format", "json"]);
    assert_eq!(enabled.result()["disabled"], serde_json::json!([]));
    std::fs::write(
        project.join(".muster/muster.toml"),
        project_manifest.replace("enable = [\"mcp:figma\"]", ""),
    )
    .unwrap();
    let (_, disabled) = env.run_json(&["plan", "--project", project_arg, "--format", "json"]);
    assert_eq!(
        disabled.result()["disabled"],
        serde_json::json!([{"kind":"mcp","name":"figma","agent":"claude","layer":"team"}])
    );
}

#[test]
fn machine_profile_enable_overrides_team_disable() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("muster.toml"), "version = 1\n[agents]\nenabled = [\"claude\"]\n[agents.toggles]\ndisable = [\"mcp:figma\"]\n[mcp.figma]\ncommand = \"figma\"\n[machines.laptop]\nrecipient = 'age1laptop'\nplatform = 'linux-x86_64'\nmuster_version = '0.1.0'\nprofile = 'work'\n[profiles.work.agents.toggles]\nenable = ['mcp:figma']\n").unwrap();
    std::fs::create_dir_all(env.path().join(".local/state/muster")).unwrap();
    std::fs::write(env.path().join(".local/state/muster/machine"), "laptop\n").unwrap();
    let (_, plan) = env.run_json(&["plan", "--format", "json"]);
    assert_eq!(plan.result()["disabled"], serde_json::json!([]));
    env.run(&["agents", "apply", "--yes"]).assert().success();
    assert!(std::fs::read_to_string(env.path().join(".claude.json"))
        .unwrap()
        .contains("figma"));
}

#[test]
fn unknown_name_warns_and_unknown_adapter_fails() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("muster.toml");
    std::fs::write(
        &path,
        "version = 1\n[agents.toggles]\ndisable = ['skill:missing']\n",
    )
    .unwrap();
    let (output, plan) = env.run_json(&["plan", "--format", "json"]);
    assert!(output.status.success());
    assert!(plan
        .diagnostics()
        .to_string()
        .contains("toggle_unknown_item"));
    std::fs::write(
        &path,
        "version = 1\n[agents.toggles]\ndisable = ['skill:pdf@unknown']\n",
    )
    .unwrap();
    let (output, plan) = env.run_json(&["plan", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(plan
        .diagnostics()
        .to_string()
        .contains("unknown toggle adapter"));
}

#[test]
fn disabled_mcp_does_not_require_approval() {
    let env = MusterEnv::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("muster.toml"), "version = 1\n[agents]\nenabled = ['claude']\n[agents.toggles]\ndisable = ['mcp:figma']\n[mcp.figma]\ncommand = 'figma'\n").unwrap();
    let (_, plan) = env.run_json(&["plan", "--format", "json"]);
    assert_eq!(plan.result()["approvals"], serde_json::json!([]));
}

#[test]
fn specific_disable_beats_bare_enable_in_same_layer() {
    let env = MusterEnv::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("muster.toml"), "version = 1\n[agents]\nenabled = ['claude', 'codex']\n[agents.toggles]\ndisable = ['mcp:figma@claude']\nenable = ['mcp:figma']\n[mcp.figma]\ncommand = 'figma'\n").unwrap();
    let (_, plan) = env.run_json(&["plan", "--format", "json"]);
    assert_eq!(
        plan.result()["disabled"],
        serde_json::json!([{"kind":"mcp","name":"figma","agent":"claude","layer":"team"}])
    );
}

#[test]
fn disabled_subagent_is_removed() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(dir.join("subagents")).unwrap();
    std::fs::write(
        dir.join("subagents/reviewer.md"),
        "---\nname: reviewer\ndescription: Reviews code\n---\nReview.\n",
    )
    .unwrap();
    let manifest =
        "version = 1\n[agents]\nenabled = ['claude']\n[agents.subagents]\nsource = 'subagents'\n";
    std::fs::write(dir.join("muster.toml"), manifest).unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let installed = env.path().join(".claude/agents/reviewer.md");
    assert!(installed.exists());
    std::fs::write(
        dir.join("muster.toml"),
        format!("{manifest}\n[agents.toggles]\ndisable = ['subagent:reviewer']\n"),
    )
    .unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    assert!(!installed.exists());
}

#[test]
fn toggle_commands_target_profile_and_project_and_honor_dry_run() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("muster.toml");
    std::fs::write(&path, "version = 1\n[profiles.work]\n# profile comment\n").unwrap();
    let before = std::fs::read_to_string(&path).unwrap();
    env.run(&["disable", "mcp:figma", "--profile", "work", "--dry-run"])
        .assert()
        .success();
    assert_eq!(before, std::fs::read_to_string(&path).unwrap());
    env.run(&["disable", "mcp:figma", "--profile", "work", "--yes"])
        .assert()
        .success();
    let result = std::fs::read_to_string(&path).unwrap();
    assert!(result.contains("# profile comment"));
    assert!(result.contains("[profiles.work.agents.toggles]"));
    let project = env.path().join("project");
    std::fs::create_dir_all(project.join(".muster")).unwrap();
    std::fs::write(
        project.join(".muster/muster.toml"),
        "version = 1\n# project comment\n",
    )
    .unwrap();
    env.run(&[
        "enable",
        "mcp:figma",
        "--project",
        project.to_str().unwrap(),
        "--yes",
    ])
    .assert()
    .success();
    let result = std::fs::read_to_string(project.join(".muster/muster.toml")).unwrap();
    assert!(result.contains("# project comment"));
    assert!(result.contains("enable = [\"mcp:figma\"]"));
}
