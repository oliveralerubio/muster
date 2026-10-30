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
    assert!(claude_src["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["reason"] == "unreadable"));

    let codex_src = &result.result()["sources"][1];
    assert_eq!(codex_src["agent"], "codex");
    assert_eq!(codex_src["malformed_lines"], 1);
    assert!(codex_src["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["reason"] == "unreadable"));
    assert!(codex_src["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["reason"] == "too_large"));

    assert_eq!(result.result()["items"].as_array().unwrap().len(), 2);

    let env2 = MusterEnv::new();
    std::fs::create_dir_all(env2.path().join(".claude")).unwrap();
    std::fs::write(env2.path().join(".claude/projects"), "not a dir").unwrap();
    let (output2, result2) = env2.run_json(&["--format", "json", "usage"]);
    assert!(output2.status.success());
    assert_eq!(
        result2.result()["sources"][0]["skipped"][0]["reason"],
        "unreadable"
    );
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
