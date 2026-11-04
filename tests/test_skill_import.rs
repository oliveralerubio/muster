mod support;

use std::path::{Path, PathBuf};
use support::MusterEnv;

fn seed_manifest(dir: &Path, agents: &[&str]) {
    std::fs::create_dir_all(dir.join("skills")).expect("skills dir");
    std::fs::write(dir.join("AGENTS.md"), "# Shared instructions\n").expect("AGENTS.md");
    let list: Vec<String> = agents.iter().map(|a| format!("\"{a}\"")).collect();
    std::fs::write(
        dir.join("muster.toml"),
        format!(
            r#"
version = 1

[agents]
enabled = [{}]

[agents.instructions]
source = "AGENTS.md"

[agents.skills]
source = "skills"
"#,
            list.join(", ")
        ),
    )
    .expect("write manifest");
    muster::skills::lock::SkillsLock::default()
        .save(dir)
        .unwrap();
}

fn user_skills_dir(env: &MusterEnv, adapter: &str) -> PathBuf {
    match adapter {
        "claude" => env.path().join(".claude/skills"),
        "codex" => env.path().join(".agents/skills"),
        "opencode" => env.path().join(".config/opencode/skills"),
        "antigravity" => env.path().join(".gemini/antigravity-cli/skills"),
        other => panic!("unknown adapter {other}"),
    }
}

#[test]
fn import_happy_path() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude"]);

    let claude_skills = user_skills_dir(&env, "claude");
    let custom = claude_skills.join("custom");
    std::fs::create_dir_all(&custom).unwrap();
    let skill_md = "---\nname: custom\ndescription: Custom skill\n---\n# Custom\n";
    let script = "print('hello')\n";
    std::fs::write(custom.join("SKILL.md"), skill_md).unwrap();
    std::fs::write(custom.join("script.py"), script).unwrap();

    // 1. List shows it as unmanaged
    let (_, list_before) = env.run_json(&["--format", "json", "skill", "list"]);
    assert!(list_before.ok());
    let unmanaged = list_before.result()["unmanaged"].as_array().unwrap();
    let found = unmanaged.iter().find(|u| u["name"] == "custom").unwrap();
    assert_eq!(found["valid"], true);
    assert_eq!(found["description"], "Custom skill");
    assert_eq!(found["adapters"].as_array().unwrap(), &["claude"]);

    // 2. Import into Muster
    let (_, import_res) = env.run_json(&["--format", "json", "--yes", "skill", "import", "custom"]);
    assert!(import_res.ok(), "{:?}", import_res.diagnostics());
    assert_eq!(import_res.result()["name"], "custom");
    assert_eq!(import_res.result()["origin"], "local");
    assert_eq!(import_res.result()["adapter"], "claude");
    assert_eq!(import_res.result()["files"], 2);

    // Config repo now has the skill
    let copied_dir = env.config_dir().join("skills/custom");
    assert_eq!(
        std::fs::read_to_string(copied_dir.join("SKILL.md")).unwrap(),
        skill_md
    );
    assert_eq!(
        std::fs::read_to_string(copied_dir.join("script.py")).unwrap(),
        script
    );

    // skills.lock pins it
    let lock = muster::skills::lock::SkillsLock::load(&env.config_dir()).unwrap();
    let entry = lock.get("custom").expect("pinned entry");
    assert_eq!(entry.origin, "local");

    // Original on-disk files are untouched
    assert_eq!(
        std::fs::read_to_string(custom.join("SKILL.md")).unwrap(),
        skill_md
    );
    assert_eq!(
        std::fs::read_to_string(custom.join("script.py")).unwrap(),
        script
    );

    // 3. List shows it under managed skills, no longer in unmanaged
    let (_, list_after) = env.run_json(&["--format", "json", "skill", "list"]);
    assert!(list_after.ok());
    let skills = list_after.result()["skills"].as_array().unwrap();
    assert!(skills.iter().any(|s| s["name"] == "custom"));
    let unmanaged_after = list_after.result()["unmanaged"].as_array().unwrap();
    assert!(!unmanaged_after.iter().any(|u| u["name"] == "custom"));
}

#[cfg(unix)]
#[test]
fn import_symlinked_source_dir() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude", "codex"]);

    let codex_skills = user_skills_dir(&env, "codex");
    let real_dir = codex_skills.join("shared-tool");
    std::fs::create_dir_all(&real_dir).unwrap();
    let skill_md = "---\nname: shared-tool\ndescription: Shared tool\n---\nShared.\n";
    std::fs::write(real_dir.join("SKILL.md"), skill_md).unwrap();

    let claude_skills = user_skills_dir(&env, "claude");
    std::fs::create_dir_all(&claude_skills).unwrap();
    std::os::unix::fs::symlink(&real_dir, claude_skills.join("shared-tool")).unwrap();

    let (_, res) = env.run_json(&[
        "--format",
        "json",
        "--yes",
        "skill",
        "import",
        "shared-tool",
        "--adapter",
        "claude",
    ]);
    assert!(res.ok(), "{:?}", res.diagnostics());
    assert_eq!(res.result()["name"], "shared-tool");

    let copied = env.config_dir().join("skills/shared-tool/SKILL.md");
    assert_eq!(std::fs::read_to_string(copied).unwrap(), skill_md);
    // Real directory and symlink are untouched
    assert_eq!(
        std::fs::read_to_string(real_dir.join("SKILL.md")).unwrap(),
        skill_md
    );
    assert!(claude_skills
        .join("shared-tool")
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn import_name_conflict_refused() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude"]);

    // Add an own skill first
    env.run(&[
        "--yes",
        "skill",
        "add",
        "existing",
        "--description",
        "Existing skill",
    ])
    .assert()
    .success();

    let claude_skills = user_skills_dir(&env, "claude");
    let custom = claude_skills.join("existing");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(
        custom.join("SKILL.md"),
        "---\nname: existing\ndescription: Other\n---\n",
    )
    .unwrap();

    let result = env
        .run(&["--yes", "skill", "import", "existing"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let err = String::from_utf8_lossy(&result.stderr);
    assert!(err.contains("already exists"), "{err}");
}

#[test]
fn import_missing_or_invalid_frontmatter_refused() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude"]);

    let claude_skills = user_skills_dir(&env, "claude");

    // No SKILL.md
    let no_md = claude_skills.join("no-md");
    std::fs::create_dir_all(&no_md).unwrap();
    std::fs::write(no_md.join("readme.txt"), "hello").unwrap();

    // Invalid frontmatter: missing description
    let bad_fm = claude_skills.join("bad-fm");
    std::fs::create_dir_all(&bad_fm).unwrap();
    std::fs::write(
        bad_fm.join("SKILL.md"),
        "---\nname: bad-fm\n---\nMissing desc\n",
    )
    .unwrap();

    // Name mismatch
    let wrong_name = claude_skills.join("wrong-name");
    std::fs::create_dir_all(&wrong_name).unwrap();
    std::fs::write(
        wrong_name.join("SKILL.md"),
        "---\nname: different\ndescription: Desc\n---\n",
    )
    .unwrap();

    let (_, list) = env.run_json(&["--format", "json", "skill", "list"]);
    let unmanaged = list.result()["unmanaged"].as_array().unwrap();
    for name in ["no-md", "bad-fm", "wrong-name"] {
        let entry = unmanaged.iter().find(|u| u["name"] == name).unwrap();
        assert_eq!(entry["valid"], false, "entry {name} must be invalid");
    }

    let out1 = env
        .run(&["--yes", "skill", "import", "no-md"])
        .output()
        .unwrap();
    assert!(!out1.status.success());
    assert!(String::from_utf8_lossy(&out1.stderr).contains("SKILL.md"));

    let out2 = env
        .run(&["--yes", "skill", "import", "bad-fm"])
        .output()
        .unwrap();
    assert!(!out2.status.success());
    assert!(String::from_utf8_lossy(&out2.stderr).contains("frontmatter"));

    let out3 = env
        .run(&["--yes", "skill", "import", "wrong-name"])
        .output()
        .unwrap();
    assert!(!out3.status.success());
    assert!(String::from_utf8_lossy(&out3.stderr).contains("does not match"));
}

#[test]
fn import_ambiguous_multi_adapter() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude", "codex"]);

    let claude = user_skills_dir(&env, "claude").join("multi");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("SKILL.md"),
        "---\nname: multi\ndescription: Claude version\n---\nClaude.\n",
    )
    .unwrap();

    let codex = user_skills_dir(&env, "codex").join("multi");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("SKILL.md"),
        "---\nname: multi\ndescription: Codex version\n---\nCodex.\n",
    )
    .unwrap();

    // Ambiguous import without --adapter fails
    let res = env
        .run(&["--yes", "skill", "import", "multi"])
        .output()
        .unwrap();
    assert!(!res.status.success());
    let err = String::from_utf8_lossy(&res.stderr);
    assert!(err.contains("--adapter"), "{err}");

    // Disambiguating with --adapter succeeds
    let (_, imported) = env.run_json(&[
        "--format",
        "json",
        "--yes",
        "skill",
        "import",
        "multi",
        "--adapter",
        "claude",
    ]);
    assert!(imported.ok());
    assert_eq!(imported.result()["adapter"], "claude");
    let content = std::fs::read_to_string(env.config_dir().join("skills/multi/SKILL.md")).unwrap();
    assert!(content.contains("Claude version"));
}

#[test]
fn import_identical_multi_adapter_succeeds_without_adapter_flag() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude", "codex"]);

    let skill_md = "---\nname: same\ndescription: Same in both\n---\nSame.\n";
    for adapter in ["claude", "codex"] {
        let dir = user_skills_dir(&env, adapter).join("same");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), skill_md).unwrap();
    }

    let (_, res) = env.run_json(&["--format", "json", "--yes", "skill", "import", "same"]);
    assert!(res.ok(), "{:?}", res.diagnostics());
    assert_eq!(res.result()["name"], "same");
}

#[test]
fn import_dry_run_writes_nothing() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude"]);

    let claude = user_skills_dir(&env, "claude").join("dry-tool");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("SKILL.md"),
        "---\nname: dry-tool\ndescription: Dry\n---\nDry.\n",
    )
    .unwrap();

    let (_, res) = env.run_json(&[
        "--format",
        "json",
        "--dry-run",
        "skill",
        "import",
        "dry-tool",
    ]);
    assert!(res.ok());
    assert_eq!(res.result()["name"], "dry-tool");

    assert!(!env.config_dir().join("skills/dry-tool").exists());
    let lock = muster::skills::lock::SkillsLock::load(&env.config_dir()).unwrap();
    assert!(lock.get("dry-tool").is_none());
}

#[test]
fn preexisting_skill_not_truncated_on_unmanage() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude"]);

    let claude_skills = user_skills_dir(&env, "claude");
    let skill_dir = claude_skills.join("preexisting");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let original_md =
        "---\nname: preexisting\ndescription: Original skill\n---\nOriginal content.\n";
    let original_sh = "#!/bin/sh\necho preexisting\n";
    std::fs::write(skill_dir.join("SKILL.md"), original_md).unwrap();
    std::fs::write(skill_dir.join("run.sh"), original_sh).unwrap();

    // 1. Import pre-existing skill into Muster
    env.run(&["--yes", "skill", "import", "preexisting"])
        .assert()
        .success();

    // 2. Apply: Muster manages it now
    env.run(&["--yes", "agents", "apply"]).assert().success();

    // 3. Remove skill from Muster config
    env.run(&["--yes", "skill", "rm", "preexisting"])
        .assert()
        .success();

    // 4. Apply again: Muster unmanages it
    env.run(&["--yes", "agents", "apply"]).assert().success();

    // 5. User's files must NOT be empty or deleted! They were pre-existing.
    assert!(
        skill_dir.join("SKILL.md").exists(),
        "SKILL.md must still exist"
    );
    assert!(skill_dir.join("run.sh").exists(), "run.sh must still exist");
    let md = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
    let sh = std::fs::read_to_string(skill_dir.join("run.sh")).unwrap();
    assert_eq!(md, original_md, "SKILL.md must retain original content");
    assert_eq!(sh, original_sh, "run.sh must retain original content");
}
