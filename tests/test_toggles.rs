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
    // And the reverse: disabling removes the matching enable.
    env.run(&["disable", "skill:pdf", "--yes"])
        .assert()
        .success();
    let text = std::fs::read_to_string(dir.join("muster.toml")).unwrap();
    assert!(text.contains("disable = [\"skill:pdf\"]"));
    assert!(!text.contains("enable = [\"skill:pdf\"]"));
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

#[test]
fn member_layer_enable_overrides_team_disable() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("muster.toml"), "version = 1\n[agents]\nenabled = ['claude']\n[agents.toggles]\ndisable = ['mcp:figma']\n[mcp.figma]\ncommand = 'figma'\n[members.alice]\nstatus = 'active'\nmachines = ['laptop']\nprofile = 'personal'\n[machines.laptop]\nrecipient = 'age1laptop'\nplatform = 'linux-x86_64'\nmuster_version = '0.1.0'\n[profiles.personal.agents.toggles]\nenable = ['mcp:figma']\n").unwrap();
    std::fs::create_dir_all(env.path().join(".local/state/muster")).unwrap();
    std::fs::write(env.path().join(".local/state/muster/machine"), "laptop\n").unwrap();
    let (output, plan) = env.run_json(&["plan", "--format", "json"]);
    assert!(output.status.success());
    assert_eq!(plan.result()["disabled"], serde_json::json!([]));
}

#[test]
fn schema_exposes_toggles_and_toggle_commands() {
    let env = MusterEnv::new();
    let (output, schema) = env.run_json(&["schema", "--format", "json"]);
    assert!(output.status.success());
    let agents = &schema.result()["manifest_schema"]["$defs"]["AgentsConfig"]["properties"];
    let text = serde_json::to_string(agents).unwrap();
    assert!(text.contains("toggles"), "{text}");
    let commands: Vec<_> = schema.result()["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for name in ["enable", "disable"] {
        let spec = schema.result()["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"].as_str() == Some(name))
            .unwrap();
        assert_eq!(spec["mutating"], serde_json::json!(true));
        assert!(commands.contains(&name));
    }
}

#[test]
fn sync_removes_disabled_skill_and_leaves_skills_lock_alone() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(dir.join("skills/pdf")).unwrap();
    std::fs::write(
        dir.join("skills/pdf/SKILL.md"),
        "---\nname: pdf\ndescription: PDF\n---\nPDF\n",
    )
    .unwrap();
    let manifest =
        "version = 1\n[agents]\nenabled = ['claude']\n[agents.skills]\nsource = 'skills'\n";
    std::fs::write(dir.join("muster.toml"), manifest).unwrap();
    env.run(&["skill", "pin", "pdf", "--yes"])
        .assert()
        .success();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let installed = env.path().join(".claude/skills/pdf/SKILL.md");
    assert!(installed.exists());
    let lock = std::fs::read_to_string(dir.join("skills.lock")).unwrap();
    std::fs::write(
        dir.join("muster.toml"),
        format!("{manifest}\n[agents.toggles]\ndisable = ['skill:pdf']\n"),
    )
    .unwrap();
    env.run(&["sync", "--yes"]).assert().success();
    assert!(!installed.exists());
    assert_eq!(
        lock,
        std::fs::read_to_string(dir.join("skills.lock")).unwrap()
    );
}

#[test]
fn team_manifest_toggle_requires_an_admin() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let identity = muster::identity::Identity::generate();
    std::fs::write(
        dir.join("muster.toml"),
        format!(
            "version = 1\n[members.alice]\nstatus = 'active'\nmachines = ['box']\n[machines.box]\nrecipient = '{}'\nplatform = 'linux-x86_64'\nmuster_version = '0.1.0'\n",
            identity.recipient()
        ),
    )
    .unwrap();
    let state = env.path().join(".local/state/muster");
    identity.save(&state).unwrap();
    std::fs::write(state.join("machine"), "box\n").unwrap();
    let (output, envelope) = env.run_json(&["disable", "skill:pdf", "--yes", "--format", "json"]);
    assert_eq!(output.status.code(), Some(5));
    assert!(envelope
        .diagnostics()
        .to_string()
        .contains("only team admins"));
    assert!(!std::fs::read_to_string(dir.join("muster.toml"))
        .unwrap()
        .contains("toggles"));
}

#[test]
fn unsafe_toggle_names_are_rejected() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("muster.toml"), "version = 1\n").unwrap();
    let before = std::fs::read_to_string(dir.join("muster.toml")).unwrap();
    for item in [
        "skill:../x",
        "skill:x/y",
        "skill:x\\y",
        "skill:..",
        "skill:UPPER",
        "skill:-lead",
        "skill:.hidden",
        "subagent:two..dots",
    ] {
        let (output, envelope) = env.run_json(&["disable", item, "--yes", "--format", "json"]);
        assert_eq!(output.status.code(), Some(2), "{item}");
        let diagnostics = envelope.diagnostics().to_string();
        assert!(
            diagnostics.contains("invalid toggle name"),
            "{item}: {diagnostics}"
        );
    }
    assert_eq!(
        before,
        std::fs::read_to_string(dir.join("muster.toml")).unwrap()
    );
}

#[test]
fn manifest_rejects_unsafe_toggle_names() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("muster.toml"),
        "version = 1\n[agents.toggles]\ndisable = ['skill:../x']\n",
    )
    .unwrap();
    let (output, envelope) = env.run_json(&["plan", "--format", "json"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(envelope
        .diagnostics()
        .to_string()
        .contains("invalid toggle name"));
}

#[test]
fn project_plan_does_not_warn_for_inherited_global_items() {
    let env = MusterEnv::new();
    let dir = env.config_dir();
    std::fs::create_dir_all(dir.join("skills/pdf")).unwrap();
    std::fs::write(
        dir.join("skills/pdf/SKILL.md"),
        "---\nname: pdf\ndescription: PDF\n---\nPDF\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude']\n[agents.skills]\nsource = 'skills'\n[agents.toggles]\ndisable = ['skill:pdf', 'mcp:figma']\n[mcp.figma]\ncommand = 'figma'\n",
    )
    .unwrap();
    let project = env.path().join("project");
    std::fs::create_dir_all(project.join(".muster")).unwrap();
    std::fs::write(project.join(".muster/muster.toml"), "version = 1\n").unwrap();
    let project_arg = project.to_str().unwrap();
    let (output, plan) = env.run_json(&["plan", "--project", project_arg, "--format", "json"]);
    assert!(output.status.success());
    let diagnostics = plan.diagnostics().to_string();
    assert!(
        !diagnostics.contains("toggle_unknown_item"),
        "inherited global items must not warn: {diagnostics}"
    );
    // A name unknown in both scopes still warns.
    std::fs::write(
        project.join(".muster/muster.toml"),
        "version = 1\n[agents.toggles]\ndisable = ['skill:nope']\n",
    )
    .unwrap();
    let (_, plan) = env.run_json(&["plan", "--project", project_arg, "--format", "json"]);
    assert!(plan
        .diagnostics()
        .to_string()
        .contains("toggle_unknown_item"));
}
