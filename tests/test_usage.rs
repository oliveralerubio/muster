mod support;
use support::MusterEnv;

#[test]
fn claude_skill_use_is_counted_without_exposing_prompt() {
    let env = MusterEnv::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("muster.toml"), "version = 1\n").unwrap();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\",\"prompt\":\"SECRET-LOOKING-PROMPT\"}}]}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SECRET-LOOKING-PROMPT"));
    assert_eq!(result.result()["items"][0]["name"], "review");
    assert_eq!(result.result()["items"][0]["uses"], 1);
}

#[test]
fn claude_counts_mcp_server_and_subagent() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"mcp__docs__search\",\"input\":{}},{\"type\":\"tool_use\",\"name\":\"Agent\",\"input\":{\"subagent_type\":\"reviewer\"}}]}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    assert_eq!(result.result()["items"][0]["name"], "docs");
    assert_eq!(result.result()["items"][1]["name"], "reviewer");
}

#[test]
fn usage_groups_events_and_ignores_old_timestamps_and_bad_lines() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    let event = |time: &str| {
        format!("{{\"type\":\"assistant\",\"timestamp\":\"{time}\",\"sessionId\":\"session\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{{\"skill\":\"review\"}}}}]}}}}\n")
    };
    std::fs::write(
        dir.join("session.jsonl"),
        format!(
            "{}{}{}bad json\n",
            event("2000-01-01T00:00:00Z"),
            event("2099-01-01T00:00:00Z"),
            event("2099-01-02T00:00:00Z")
        ),
    )
    .unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage", "--since", "14d"]);
    assert!(output.status.success());
    assert_eq!(result.result()["items"][0]["uses"], 2);
    assert_eq!(result.result()["items"][0]["sessions"], 1);
    assert_eq!(
        result.result()["items"][0]["last_used"],
        "2099-01-02T00:00:00Z"
    );
    assert_eq!(result.result()["sources"][0]["malformed_lines"], 1);
}

#[test]
fn managed_unused_skill_has_disable_suggestion() {
    let env = MusterEnv::new();
    let skill = env.config_dir().join("skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        env.config_dir().join("muster.toml"),
        "version = 1\n[agents.skills]\nsource = 'skills'\n",
    )
    .unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: Review\n---\n",
    )
    .unwrap();
    // One scanned session, so the report has transcript coverage and unused
    // suggestions are emitted at all.
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"other\"}}]}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    let items = result.result()["items"].as_array().unwrap();
    assert!(items
        .iter()
        .any(|i| i["name"] == "review" && i["uses"] == 0));
    assert_eq!(result.result()["unused"][0]["name"], "review");
    assert_eq!(
        result.result()["unused"][0]["suggestion"],
        "muster disable skill:review"
    );
}

#[test]
fn skills_source_escaping_the_repo_discovers_nothing() {
    let env = MusterEnv::new();
    let config = env.config_dir();
    std::fs::create_dir_all(config.join("skills/review")).unwrap();
    std::fs::write(config.join("skills/review/SKILL.md"), "x\n").unwrap();
    // Outside the config repo: must never be discovered.
    let outside = env.path().join(".config/outside/escaped");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "x\n").unwrap();
    std::fs::write(
        config.join("muster.toml"),
        "version = 1\n[agents.skills]\nsource = '../outside'\n",
    )
    .unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    let names: Vec<&str> = result.result()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(!names.contains(&"escaped"), "{names:?}");
    // A broken source means no skills can be trusted as managed at all.
    assert!(!names.contains(&"review"), "{names:?}");
}

#[test]
fn managed_discovery_stays_inside_the_config_repo() {
    let env = MusterEnv::new();
    let config = env.config_dir();
    std::fs::create_dir_all(config.join("skills/review")).unwrap();
    std::fs::write(config.join("skills/review/SKILL.md"), "x\n").unwrap();
    // Ignored or unsafe names inside the skills directory.
    for ignored in [".hidden", "node_modules", "backup.bak"] {
        std::fs::create_dir_all(config.join(format!("skills/{ignored}"))).unwrap();
        std::fs::write(config.join(format!("skills/{ignored}/SKILL.md")), "x\n").unwrap();
    }
    // A symlinked skill directory must not be followed.
    std::fs::create_dir_all(env.path().join("elsewhere/linked")).unwrap();
    std::fs::write(env.path().join("elsewhere/linked/SKILL.md"), "x\n").unwrap();
    std::os::unix::fs::symlink(
        env.path().join("elsewhere/linked"),
        config.join("skills/linked"),
    )
    .unwrap();
    // A symlinked SKILL.md inside an otherwise fine directory.
    std::fs::create_dir_all(config.join("skills/symfile")).unwrap();
    std::fs::write(env.path().join("elsewhere/SKILL.md"), "x\n").unwrap();
    std::os::unix::fs::symlink(
        env.path().join("elsewhere/SKILL.md"),
        config.join("skills/symfile/SKILL.md"),
    )
    .unwrap();
    std::fs::write(
        config.join("muster.toml"),
        "version = 1\n[agents.skills]\nsource = 'skills'\n[agents.subagents]\nsource = 'subagents'\n[mcp.\"bad key\"]\ncommand = 'x'\n",
    )
    .unwrap();
    // Subagent names are stems of regular .md files; symlinks and junk names
    // are skipped.
    std::fs::create_dir_all(config.join("subagents")).unwrap();
    std::fs::write(config.join("subagents/ok.md"), "x\n").unwrap();
    std::fs::write(config.join("subagents/.hidden.md"), "x\n").unwrap();
    std::fs::write(config.join("subagents/draft.md~"), "x\n").unwrap();
    std::os::unix::fs::symlink(
        env.path().join("elsewhere/linked/SKILL.md"),
        config.join("subagents/linked.md"),
    )
    .unwrap();

    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    let names: Vec<&str> = result.result()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    for banned in [
        "escaped",
        ".hidden",
        "node_modules",
        "backup.bak",
        "linked",
        "symfile",
        "bad key",
        ".hidden.md",
        "draft.md~",
        "linked.md",
    ] {
        assert!(
            !names.contains(&banned),
            "{banned} must not be reported: {names:?}"
        );
    }
    assert!(names.contains(&"review"));
    assert!(names.contains(&"ok"));
}

#[test]
fn suggestions_require_toggle_valid_names() {
    let env = MusterEnv::new();
    let config = env.config_dir();
    for name in ["review", "Review"] {
        std::fs::create_dir_all(config.join(format!("skills/{name}"))).unwrap();
        std::fs::write(config.join(format!("skills/{name}/SKILL.md")), "x\n").unwrap();
    }
    std::fs::write(config.join("muster.toml"), "version = 1\n").unwrap();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"used\"}}]}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    let unused = result.result()["unused"].as_array().unwrap();
    let lower = unused.iter().find(|i| i["name"] == "review").unwrap();
    assert_eq!(lower["suggestion"], "muster disable skill:review");
    let upper = unused.iter().find(|i| i["name"] == "Review").unwrap();
    assert!(
        upper.get("suggestion").is_none(),
        "uppercase name cannot be a toggle: {upper}"
    );
}

#[test]
fn unused_uses_merged_config_and_excludes_disabled_items() {
    let env = MusterEnv::new();
    let config = env.config_dir();
    std::fs::create_dir_all(config.join("skills/used")).unwrap();
    std::fs::write(config.join("skills/used/SKILL.md"), "x\n").unwrap();
    std::fs::create_dir_all(config.join("skills/gone")).unwrap();
    std::fs::write(config.join("skills/gone/SKILL.md"), "x\n").unwrap();
    std::fs::create_dir_all(config.join("agents")).unwrap();
    std::fs::write(config.join("agents/onlycodex.md"), "x\n").unwrap();
    // `gone` is disabled for every agent; `onlycodex` only for codex, so it
    // is still unused advice for the other selected adapters.
    std::fs::write(
        config.join("muster.toml"),
        "version = 1\n[agents.toggles]\ndisable = ['skill:gone', 'subagent:onlycodex@codex']\n[agents.skills]\nsource = 'skills'\n[agents.subagents]\nsource = 'agents'\n[mcp.docs]\ncommand = 'docs'\n",
    )
    .unwrap();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"used\"}}]}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    let unused = result.result()["unused"].as_array().unwrap();
    let names: Vec<&str> = unused.iter().filter_map(|i| i["name"].as_str()).collect();
    assert!(
        !names.contains(&"gone"),
        "disabled-for-all items are not unused advice: {unused:?}"
    );
    assert!(!names.contains(&"used"), "an observed item is not unused");
    assert!(names.contains(&"docs"));
    assert!(names.contains(&"onlycodex"));
}

#[test]
fn no_coverage_means_no_unused_suggestions() {
    let env = MusterEnv::new();
    std::fs::create_dir_all(env.config_dir().join("skills/review")).unwrap();
    std::fs::write(env.config_dir().join("skills/review/SKILL.md"), "x\n").unwrap();
    std::fs::write(env.config_dir().join("muster.toml"), "version = 1\n").unwrap();
    // No transcripts at all: nothing was scanned, so suggesting removals
    // would be advice built on zero evidence.
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    assert_eq!(result.result()["coverage"], "none");
    assert_eq!(result.result()["unused"].as_array().unwrap().len(), 0);
}

#[test]
fn agent_filter_lists_only_that_source_and_gates_on_its_coverage() {
    let env = MusterEnv::new();
    let dir = env.path().join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session\",\"cwd\":\"/work\"}}\n{\"type\":\"response_item\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"exec\",\"input\":\"const r = await tools.exec_command({cmd: 'cat /home/user/skills/review/SKILL.md'});\"}}\n",
    )
    .unwrap();

    // A known adapter with unsupported transcripts: accepted, reported as
    // unsupported, no unused advice.
    let (output, result) = env.run_json(&["--format", "json", "usage", "--agent", "opencode"]);
    assert!(output.status.success());
    let sources = result.result()["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["agent"], "opencode");
    assert_eq!(sources[0]["status"], "unsupported");
    assert_eq!(result.result()["sessions_scanned"]["opencode"], 0);
    assert_eq!(result.result()["coverage"], "none");
    assert_eq!(result.result()["unused"].as_array().unwrap().len(), 0);

    // Same for an experimental adapter id.
    let (output, result) = env.run_json(&["--format", "json", "usage", "--agent", "pi"]);
    assert!(output.status.success());
    assert_eq!(result.result()["sources"][0]["agent"], "pi");
    assert_eq!(result.result()["sources"][0]["status"], "unsupported");
    assert_eq!(result.result()["coverage"], "none");

    // A supported agent with zero scanned sessions also gets no advice.
    let (output, result) = env.run_json(&["--format", "json", "usage", "--agent", "claude"]);
    assert!(output.status.success());
    let sources = result.result()["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["agent"], "claude");
    assert_eq!(result.result()["sessions_scanned"]["claude"], 0);
    assert_eq!(result.result()["coverage"], "none");
    assert_eq!(result.result()["unused"].as_array().unwrap().len(), 0);

    // With transcripts scanned, the single agent's source is listed and its
    // coverage is the agent's own (Codex is partial).
    let (output, result) = env.run_json(&["--format", "json", "usage", "--agent", "codex"]);
    assert!(output.status.success());
    let sources = result.result()["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["agent"], "codex");
    assert_eq!(sources[0]["status"], "partial");
    assert_eq!(result.result()["sessions_scanned"]["codex"], 1);
    assert_eq!(result.result()["coverage"], "partial");
    assert_eq!(result.result()["items"][0]["name"], "review");
}

#[test]
fn unknown_agent_is_a_usage_error() {
    let env = MusterEnv::new();
    env.run(&["--format", "json", "usage", "--agent", "bad"])
        .assert()
        .code(2);
}

#[test]
fn invalid_since_and_max_file_bytes_are_usage_errors() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{}\n").unwrap();
    let before = std::fs::read_dir(env.path()).unwrap().count();
    for args in [
        vec!["--since", "0d"],
        vec!["--since", "bad"],
        vec!["--max-file-bytes", "0"],
    ] {
        let mut full = vec!["--format", "json", "usage"];
        full.extend(&args);
        let output = env.run(&full).assert().code(2).get_output().clone();
        assert!(
            !output.stdout.is_empty() || !output.stderr.is_empty(),
            "{args:?}"
        );
    }
    // Read-only: nothing is created in the machine's home by a rejected run.
    assert_eq!(std::fs::read_dir(env.path()).unwrap().count(), before);
    assert!(!env.config_dir().join("muster.toml").exists());
}

#[test]
fn agent_filter_excludes_other_transcripts() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\"}}]}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage", "--agent", "codex"]);
    assert!(output.status.success());
    assert_eq!(result.result()["items"].as_array().unwrap().len(), 0);
}

#[test]
fn oversized_transcripts_are_skipped_with_reason() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "0123456789").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage", "--max-file-bytes", "5"]);
    assert!(output.status.success());
    assert_eq!(result.result()["sources"][0]["skipped"]["too_large"], 1);
}

#[test]
fn skipped_files_are_counted_without_exposing_transcript_paths() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    // A real transcript filename: a session uuid, itself identifying data.
    let uuid = "3fa85f64-5717-4562-b3fc-2c963f66afa6";
    let old = dir.join(format!("{uuid}.jsonl"));
    std::fs::write(&old, "{}\n").unwrap();
    let file = std::fs::File::options().append(true).open(&old).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    std::fs::write(
        dir.join("current.jsonl"),
        "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\"}}]}}\n",
    )
    .unwrap();
    for fmt in ["json", "text"] {
        let output = env.run(&["--format", fmt, "usage"]).output().unwrap();
        assert!(output.status.success());
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!combined.contains(uuid), "{fmt}: {combined}");
        assert!(!combined.contains(".jsonl"), "{fmt}: {combined}");
    }
    let (_, result) = env.run_json(&["--format", "json", "usage"]);
    assert_eq!(
        result.result()["sources"][0]["skipped"]["older_than_window"],
        1
    );
    assert_eq!(result.result()["sources"][0]["skipped"]["too_large"], 0);
    assert_eq!(result.result()["sources"][0]["skipped"]["unreadable"], 0);
}

#[test]
fn project_filter_canonicalizes_relative_paths_and_matches_subdirectories() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    // The muster process inherits this test's working directory, so a
    // relative `--project .` canonicalizes to it.
    let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    let event = |cwd: &str, skill: &str| {
        format!("{{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session-{skill}\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{{\"skill\":\"{skill}\"}}}}]}}}}\n")
    };
    let other = env.path().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        dir.join("s1.jsonl"),
        event(&cwd.display().to_string(), "here"),
    )
    .unwrap();
    // A subdirectory of the project with a trailing slash in `cwd` still
    // belongs to the project.
    let nested = format!("{}/", cwd.join("sub").display());
    std::fs::write(dir.join("s2.jsonl"), event(&nested, "nested")).unwrap();
    std::fs::write(
        dir.join("s3.jsonl"),
        event(&other.display().to_string(), "elsewhere"),
    )
    .unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage", "--project", "."]);
    assert!(output.status.success());
    let names: Vec<&str> = result.result()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec!["here", "nested"],
        "cwd must match the project dir or its subdirectories"
    );
}

#[test]
fn project_filter_excludes_other_cwds() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    let other = env.path().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"cwd\":\"/project/other\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\"}}]}}\n").unwrap();
    let wanted = env.path().join("wanted");
    std::fs::create_dir_all(&wanted).unwrap();
    let (output, result) = env.run_json(&[
        "--format",
        "json",
        "usage",
        "--project",
        &wanted.display().to_string(),
    ]);
    assert!(output.status.success());
    assert_eq!(result.result()["items"].as_array().unwrap().len(), 0);
}

#[test]
fn project_filter_rejects_missing_directories() {
    let env = MusterEnv::new();
    env.run(&[
        "--format",
        "json",
        "usage",
        "--project",
        "/definitely/not/a/dir",
    ])
    .assert()
    .code(2);
}

#[test]
fn report_identifies_unsupported_sources_and_agent_breakdown() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\"}}]}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    assert_eq!(result.result()["items"][0]["agents"]["claude"]["uses"], 1);
    assert_eq!(result.result()["sessions_scanned"]["claude"], 1);
    assert_eq!(result.result()["sources"][2]["status"], "unsupported");
}

#[test]
fn codex_skill_read_is_counted() {
    let env = MusterEnv::new();
    let dir = env.path().join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session\",\"cwd\":\"/work\"}}\n{\"type\":\"response_item\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"payload\":{\"type\":\"custom_tool_call\",\"name\":\"exec\",\"input\":\"const r = await tools.exec_command({cmd: 'cat /home/user/skills/review/SKILL.md'});\"}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage", "--agent", "codex"]);
    assert!(output.status.success());
    assert_eq!(result.result()["items"][0]["name"], "review");
    assert_eq!(result.result()["items"][0]["agents"]["codex"]["uses"], 1);
}

#[test]
fn managed_subagent_and_server_are_reported_unused() {
    let env = MusterEnv::new();
    let config = env.config_dir();
    std::fs::create_dir_all(config.join("agents")).unwrap();
    std::fs::write(
        config.join("muster.toml"),
        "version = 1\n[agents.subagents]\nsource = 'agents'\n[mcp.docs]\ncommand = 'docs'\n",
    )
    .unwrap();
    std::fs::write(
        config.join("agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Review\n---\n",
    )
    .unwrap();
    // One scanned session so unused suggestions are emitted.
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"other\"}}]}}\n").unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    assert_eq!(result.result()["unused"][0]["name"], "docs");
    assert_eq!(result.result()["unused"][1]["name"], "reviewer");
}

#[test]
fn codex_mcp_call_is_counted_without_exposing_arguments_or_result() {
    let env = MusterEnv::new();
    let dir = env.path().join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session\",\"cwd\":\"/work\"}}\n{\"type\":\"event_msg\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"payload\":{\"type\":\"mcp_tool_call_end\",\"call_id\":\"call_1\",\"invocation\":{\"server\":\"docs\",\"tool\":\"search\",\"arguments\":{\"query\":\"SECRET-MCP-ARG\"}},\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"SECRET-MCP-RESULT\"}]}}}\n",
    )
    .unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage", "--agent", "codex"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("SECRET-MCP-ARG"));
    assert!(!stdout.contains("SECRET-MCP-RESULT"));
    assert_eq!(result.result()["items"][0]["kind"], "mcp");
    assert_eq!(result.result()["items"][0]["name"], "docs");
    assert_eq!(result.result()["items"][0]["uses"], 1);
    assert_eq!(result.result()["items"][0]["agents"]["codex"]["uses"], 1);
    assert_eq!(
        result.result()["items"][0]["agents"]["codex"]["sessions"],
        1
    );
}

#[test]
fn unreadable_and_malformed_transcripts_are_counted_without_aborting() {
    let env = MusterEnv::new();
    let claude_dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join("unreadable.jsonl"), [0xff, 0xfe, b'\n']).unwrap();
    std::fs::write(
        claude_dir.join("valid.jsonl"),
        "not json\n{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"s1\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\"}}]}}\n",
    )
    .unwrap();

    let codex_dir = env.path().join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(codex_dir.join("unreadable.jsonl"), [0xff, 0xfe, b'\n']).unwrap();
    std::fs::write(codex_dir.join("oversized.jsonl"), "x".repeat(400)).unwrap();
    std::fs::write(
        codex_dir.join("valid.jsonl"),
        "bad line\n{\"type\":\"session_meta\",\"payload\":{\"id\":\"s2\",\"cwd\":\"/work\"}}\n{\"type\":\"event_msg\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"payload\":{\"type\":\"mcp_tool_call_end\",\"call_id\":\"c1\",\"invocation\":{\"server\":\"docs\",\"tool\":\"search\",\"arguments\":{}}}}\n",
    )
    .unwrap();

    let (output, result) = env.run_json(&["--format", "json", "usage", "--max-file-bytes", "300"]);
    assert!(output.status.success());

    let claude_src = &result.result()["sources"][0];
    assert_eq!(claude_src["agent"], "claude");
    assert_eq!(claude_src["malformed_lines"], 1);
    assert!(claude_src["skipped"]["unreadable"].as_u64().unwrap() >= 1);

    let codex_src = &result.result()["sources"][1];
    assert_eq!(codex_src["agent"], "codex");
    assert_eq!(codex_src["malformed_lines"], 1);
    assert!(codex_src["skipped"]["unreadable"].as_u64().unwrap() >= 1);
    assert_eq!(codex_src["skipped"]["too_large"], 1);
    // No transcript path may appear in the report.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains(".jsonl"));
    assert!(!stdout.contains("unreadable.jsonl"));

    assert_eq!(result.result()["items"].as_array().unwrap().len(), 2);

    let env2 = MusterEnv::new();
    std::fs::create_dir_all(env2.path().join(".claude")).unwrap();
    std::fs::write(env2.path().join(".claude/projects"), "not a dir").unwrap();
    let (output2, result2) = env2.run_json(&["--format", "json", "usage"]);
    assert!(output2.status.success());
    assert_eq!(result2.result()["sources"][0]["skipped"]["unreadable"], 1);
}

#[test]
fn privacy_rejects_unsafe_item_names_and_never_outputs_or_stores_secrets() {
    let env = MusterEnv::new();
    let claude_dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(
        claude_dir.join("session.jsonl"),
        concat!(
            "{\"type\":\"user\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"RAW-SESSION-SECRET-1\",\"message\":{\"content\":\"sk-proj-SECRET-USER-PROMPT\"}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"RAW-SESSION-SECRET-1\",\"message\":{\"content\":[",
            "{\"type\":\"text\",\"text\":\"sk-proj-SECRET-ASSISTANT-TEXT\"},",
            "{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\",\"args\":\"sk-proj-SECRET-SKILL-ARGS\"}},",
            "{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review sk-proj-SECRET-IN-SKILL-NAME\"}},",
            "{\"type\":\"tool_use\",\"name\":\"Agent\",\"input\":{\"subagent_type\":\"reviewer/sk-proj-SECRET-IN-AGENT\",\"prompt\":\"sk-proj-SECRET-AGENT-PROMPT\"}},",
            "{\"type\":\"tool_use\",\"name\":\"mcp__docs:sk-proj-SECRET-IN-MCP__search\",\"input\":{\"q\":\"sk-proj-SECRET-MCP-INPUT\"}}",
            "]}}\n"
        ),
    )
    .unwrap();

    let codex_dir = env.path().join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("session.jsonl"),
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"RAW-SESSION-SECRET-2\",\"cwd\":\"/work\"}}\n",
            "{\"type\":\"event_msg\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"payload\":{\"type\":\"mcp_tool_call_end\",\"call_id\":\"c1\",\"invocation\":{\"server\":\"docs\",\"tool\":\"search\",\"arguments\":{\"token\":\"sk-proj-SECRET-CODEX-ARG\"}},\"result\":\"sk-proj-SECRET-CODEX-RES\"}}\n",
            "{\"type\":\"event_msg\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"payload\":{\"type\":\"mcp_tool_call_end\",\"call_id\":\"c2\",\"invocation\":{\"server\":\"bad/sk-proj-SECRET-SERVER\",\"tool\":\"search\",\"arguments\":{}}}}\n"
        ),
    )
    .unwrap();

    for fmt in ["json", "text"] {
        let output = env.run(&["--format", fmt, "usage"]).output().unwrap();
        assert!(output.status.success());
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!combined.contains("sk-proj-SECRET"), "{fmt}: {combined}");
        assert!(
            !combined.contains("RAW-SESSION-SECRET"),
            "{fmt}: {combined}"
        );
    }
    assert!(!env.path().join(".local/state/muster").exists());
}

#[test]
fn codex_function_call_skill_reads_are_counted_without_arguments() {
    let env = MusterEnv::new();
    let dir = env.path().join(".codex/sessions/2026/01/01");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session.jsonl"),
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session\",\"cwd\":\"/work\"}}\n",
            "{\"type\":\"response_item\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":[\\\"sed\\\",\\\"-n\\\",\\\"1,50p\\\",\\\"/root/.claude/skills/review/SKILL.md\\\"],\\\"token\\\":\\\"sk-proj-SECRET-SHELL-ARGS\\\"}\"}}\n",
            "{\"type\":\"response_item\",\"timestamp\":\"2099-01-01T00:00:01Z\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"arguments\":\"rg -l PATTERN /root/.claude/skills/lint/SKILL.md\"}}\n",
            "{\"type\":\"response_item\",\"timestamp\":\"2099-01-01T00:00:02Z\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell\",\"arguments\":\"python /root/.claude/skills/notcounted/SKILL.md\"}}\n",
            "{\"type\":\"response_item\",\"timestamp\":\"2099-01-01T00:00:03Z\",\"payload\":{\"type\":\"function_call\",\"name\":\"other_tool\",\"arguments\":\"cat /root/.claude/skills/alsonotcounted/SKILL.md\"}}\n",
        ),
    )
    .unwrap();
    let (output, result) = env.run_json(&["--format", "json", "usage", "--agent", "codex"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("sk-proj-SECRET-SHELL-ARGS"), "{stdout}");
    let names: Vec<&str> = result.result()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    // Only reader commands (cat/sed/head/less/bat/rg) reading a skill's
    // SKILL.md count, whatever the function is called.
    assert_eq!(names, vec!["lint", "review"]);
    assert_eq!(result.result()["items"][1]["uses"], 1);
    assert_eq!(result.result()["items"][1]["agents"]["codex"]["uses"], 1);
}
