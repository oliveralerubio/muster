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
    let (output, result) = env.run_json(&["--format", "json", "usage"]);
    assert!(output.status.success());
    assert_eq!(result.result()["items"][0]["name"], "review");
    assert_eq!(result.result()["items"][0]["uses"], 0);
    assert_eq!(result.result()["unused"][0]["name"], "review");
    assert_eq!(
        result.result()["unused"][0]["suggestion"],
        "muster disable skill:review"
    );
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
    assert_eq!(
        result.result()["sources"][0]["skipped"][0]["reason"],
        "too_large"
    );
}

#[test]
fn project_filter_selects_matching_cwd() {
    let env = MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"cwd\":\"/project/other\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\"}}]}}\n").unwrap();
    let (output, result) =
        env.run_json(&["--format", "json", "usage", "--project", "/project/wanted"]);
    assert!(output.status.success());
    assert_eq!(result.result()["items"].as_array().unwrap().len(), 0);
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

