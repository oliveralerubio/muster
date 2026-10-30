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
