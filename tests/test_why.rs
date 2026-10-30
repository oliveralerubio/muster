mod support;
use support::MusterEnv;

#[test]
fn finds_shared_instruction_with_its_layer() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['codex']\n",
    )
    .unwrap();
    std::fs::write(cfg.join("AGENTS.md"), "First\nBe concise.\n").unwrap();
    let (output, result) = env.run_json(&["why", "CONCISE", "--format", "json"]);
    assert!(output.status.success(), "{:?}", result.raw);
    let found = &result.result()["matches"][0];
    assert_eq!(found["file"], cfg.join("AGENTS.md").to_str().unwrap());
    assert_eq!(found["line"], 2);
    assert_eq!(found["excerpt"], "Be concise.");
    assert_eq!(found["kind"], "instructions");
    assert_eq!(found["layer"], "team");
    assert_eq!(found["agents"], serde_json::json!(["codex"]));
    assert_eq!(found["managed"], true);
}

#[test]
fn finds_an_overlay_only_for_its_agent() {
    let env = MusterEnv::new();
    let cfg = env.config_dir();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("muster.toml"),
        "version = 1\n[agents]\nenabled = ['claude', 'codex']\n",
    )
    .unwrap();
    std::fs::write(cfg.join("AGENTS.md"), "Shared\n").unwrap();
    std::fs::write(cfg.join("AGENTS.claude.md"), "Specific advice\n").unwrap();
    let (output, result) = env.run_json(&["why", "advice", "--format", "json"]);
    assert!(output.status.success());
    let found = &result.result()["matches"][0];
    assert_eq!(
        found["file"],
        cfg.join("AGENTS.claude.md").to_str().unwrap()
    );
    assert_eq!(found["kind"], "overlay");
    assert_eq!(found["agents"], serde_json::json!(["claude"]));
}
