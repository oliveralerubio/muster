//! Agent Skills for every adapter + shared instruction files (RED phase).
//!
//! Spec (items 1-5):
//! 1. `agents apply --yes` installs every file of every skill byte-exact at
//!    each adapter's documented skills path, user scope and project scope.
//! 2. Removing a skill from the source deletes previously installed
//!    Muster-owned files on next apply; untracked user files survive.
//! 3. Codex and Antigravity share the project path `.agents/skills` without
//!    conflict.
//! 4. Invalid skills (no SKILL.md, or missing name/description frontmatter)
//!    emit a `skill_invalid` warning and are skipped; exit stays 0.
//! 5. Shared instructions regression: canonical text once in project
//!    `AGENTS.md`, `@AGENTS.md` import in `CLAUDE.md`, user-scope copies for
//!    every adapter, and a second apply reports no changes.

mod support;

use std::path::{Path, PathBuf};
use support::MusterEnv;

const SKILL_MD: &str = "---\nname: review\ndescription: Review code\n---\nReview carefully.\n";
const CHECKLIST: &str = "- tests\n";
const RUN_SH: &str = "#!/bin/sh\necho hi\n";
const INSTRUCTIONS: &str = "# Shared rules\n\nBe concise.\n";

/// Seed a skills source with `review` (SKILL.md + checklist.md + nested
/// `scripts/run.sh`) and a second skill `extra`.
fn seed_skills(dir: &Path) {
    let review = dir.join("review");
    std::fs::create_dir_all(review.join("scripts")).expect("review/scripts dir");
    std::fs::write(review.join("SKILL.md"), SKILL_MD).expect("write SKILL.md");
    std::fs::write(review.join("checklist.md"), CHECKLIST).expect("write checklist.md");
    std::fs::write(review.join("scripts/run.sh"), RUN_SH).expect("write scripts/run.sh");
    std::fs::create_dir_all(dir.join("extra")).expect("extra dir");
    std::fs::write(
        dir.join("extra/SKILL.md"),
        "---\nname: extra\ndescription: Extra skill\n---\nExtra.\n",
    )
    .expect("write extra SKILL.md");
}

/// Write a muster.toml (instructions + skills) next to `AGENTS.md`/`skills`
/// sources in `dir`, enabling `agents`.
fn seed_manifest(dir: &Path, agents: &[&str]) {
    std::fs::create_dir_all(dir).expect("config dir");
    std::fs::write(dir.join("AGENTS.md"), INSTRUCTIONS).expect("AGENTS.md");
    seed_skills(&dir.join("skills"));
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
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{} must exist: {e}", path.display()))
}

/// Installed user-scope skills base dir for one adapter.
fn user_skills_dir(env: &MusterEnv, adapter: &str) -> PathBuf {
    match adapter {
        "claude" => env.path().join(".claude/skills"),
        "codex" => env.path().join(".agents/skills"),
        "opencode" => env.path().join(".config/opencode/skills"),
        "antigravity" => env.path().join(".gemini/antigravity-cli/skills"),
        other => panic!("unknown adapter {other}"),
    }
}

// ---------------------------------------------------------------------------
// 1. User-scope install, byte-exact, including nested files
// ---------------------------------------------------------------------------

fn assert_installed(base: &Path) {
    assert_eq!(read(&base.join("review/SKILL.md")), SKILL_MD);
    assert_eq!(read(&base.join("review/checklist.md")), CHECKLIST);
    assert_eq!(read(&base.join("review/scripts/run.sh")), RUN_SH);
    assert_eq!(
        read(&base.join("extra/SKILL.md")),
        "---\nname: extra\ndescription: Extra skill\n---\nExtra.\n"
    );
}

#[test]
fn skills_user_scope_claude() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude"]);
    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);
    assert_installed(&user_skills_dir(&env, "claude"));
}

#[test]
fn skills_user_scope_codex() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["codex"]);
    env.run(&["agents", "apply", "--agent", "codex", "--yes"])
        .assert()
        .code(0);
    assert_installed(&user_skills_dir(&env, "codex"));
}

#[test]
fn skills_user_scope_opencode() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["opencode"]);
    env.run(&["agents", "apply", "--agent", "opencode", "--yes"])
        .assert()
        .code(0);
    assert_installed(&user_skills_dir(&env, "opencode"));
}

#[test]
fn skills_user_scope_antigravity() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    assert_installed(&user_skills_dir(&env, "antigravity"));
}

// ---------------------------------------------------------------------------
// 1b. Project-scope install (layer in <project>/.muster/muster.toml)
// ---------------------------------------------------------------------------

/// New project with its own layer; skills source lives in `<project>/.muster`.
fn project_with_manifest(env: &MusterEnv, agents: &[&str]) -> PathBuf {
    env.run(&["--yes", "init"]).assert().code(0);
    let project = env.path().join("my-project");
    seed_manifest(&project.join(".muster"), agents);
    project
}

fn assert_installed_in(project: &Path, rel: &str) {
    assert_installed(&project.join(rel));
}

#[test]
fn skills_project_scope_claude() {
    let env = MusterEnv::new();
    let project = project_with_manifest(&env, &["claude"]);
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .code(0);
    assert_installed_in(&project, ".claude/skills");
}

#[test]
fn skills_project_scope_codex() {
    let env = MusterEnv::new();
    let project = project_with_manifest(&env, &["codex"]);
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .code(0);
    assert_installed_in(&project, ".agents/skills");
}

#[test]
fn skills_project_scope_opencode() {
    let env = MusterEnv::new();
    let project = project_with_manifest(&env, &["opencode"]);
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .code(0);
    assert_installed_in(&project, ".opencode/skills");
}

#[test]
fn skills_project_scope_antigravity() {
    let env = MusterEnv::new();
    let project = project_with_manifest(&env, &["antigravity"]);
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .code(0);
    assert_installed_in(&project, ".agents/skills");
}

// ---------------------------------------------------------------------------
// 2. Stale deletion; untracked user files survive
// ---------------------------------------------------------------------------

#[test]
fn skills_removed_from_source_are_deleted_user_files_survive() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    let base = user_skills_dir(&env, "antigravity");
    // Untracked user file next to the files Muster will own.
    std::fs::create_dir_all(base.join("review")).expect("pre-create review dir");
    let user_file = base.join("review/notes.md");
    std::fs::write(&user_file, "my private notes\n").expect("write user notes");

    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    assert!(
        base.join("extra/SKILL.md").is_file(),
        "extra installed first"
    );

    // `extra` disappears from the source.
    std::fs::remove_dir_all(env.config_dir().join("skills/extra")).expect("remove extra source");
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);

    assert!(
        !base.join("extra").exists(),
        "removed skill must be deleted on next apply"
    );
    assert_eq!(read(&base.join("review/SKILL.md")), SKILL_MD);
    assert_eq!(
        read(&user_file),
        "my private notes\n",
        "untracked user files in the same skill dir must survive"
    );
}

// ---------------------------------------------------------------------------
// 3. Codex + Antigravity share the project `.agents/skills` path
// ---------------------------------------------------------------------------

#[test]
fn skills_codex_and_antigravity_share_project_path() {
    let env = MusterEnv::new();
    let project = project_with_manifest(&env, &["codex", "antigravity"]);
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .code(0);
    let shared = project.join(".agents/skills");
    assert_eq!(
        read(&shared.join("review/SKILL.md")),
        SKILL_MD,
        "shared path must hold the exact bytes once"
    );
    assert_eq!(read(&shared.join("review/scripts/run.sh")), RUN_SH);

    // Removing the skills layer from the project manifest deletes the shared
    // files on the next apply.
    std::fs::remove_dir_all(project.join(".muster/skills")).expect("remove skills source");
    std::fs::write(
        project.join(".muster/muster.toml"),
        r#"
version = 1

[agents]
enabled = ["codex", "antigravity"]

[agents.instructions]
source = "AGENTS.md"
"#,
    )
    .expect("manifest without skills");
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .code(0);
    assert!(
        !shared.join("review").exists(),
        "removed shared skills must be deleted"
    );
}

// ---------------------------------------------------------------------------
// 4. Invalid skills: warn once, skip, exit 0
// ---------------------------------------------------------------------------

fn invalid_skill_diags(env: &MusterEnv, agent: &str) -> Vec<serde_json::Value> {
    let (output, envelope) = env.run_json(&[
        "agents", "apply", "--yes", "--agent", agent, "--format", "json",
    ]);
    assert!(output.status.success(), "apply must exit 0");
    assert!(envelope.ok(), "envelope must be ok: {}", envelope.raw);
    envelope
        .diagnostics()
        .as_array()
        .expect("diagnostics list")
        .iter()
        .filter(|d| d["code"] == "skill_invalid")
        .cloned()
        .collect()
}

#[test]
fn skill_without_skill_md_warns_and_is_skipped() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    let skills = env.config_dir().join("skills");
    std::fs::create_dir_all(skills.join("noskill")).expect("noskill dir");
    std::fs::write(skills.join("noskill/readme.txt"), "no SKILL.md here\n").expect("stray file");

    let diags = invalid_skill_diags(&env, "antigravity");
    let hits: Vec<_> = diags
        .iter()
        .filter(|d| {
            d["message"]
                .as_str()
                .map(|m| m.contains("noskill"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "warn exactly once per invalid skill: {diags:?}"
    );
    assert!(
        !user_skills_dir(&env, "antigravity")
            .join("noskill")
            .exists(),
        "invalid skill must be skipped, not installed"
    );
    assert!(
        user_skills_dir(&env, "antigravity")
            .join("review/SKILL.md")
            .is_file(),
        "valid skills in the same source must still install"
    );
}

#[test]
fn skill_without_frontmatter_fields_warns_and_is_skipped() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    let skills = env.config_dir().join("skills");
    std::fs::create_dir_all(skills.join("noname")).expect("noname dir");
    std::fs::write(
        skills.join("noname/SKILL.md"),
        "---\ndescription: x\n---\nbody\n",
    )
    .expect("write SKILL.md");
    std::fs::create_dir_all(skills.join("nodesc")).expect("nodesc dir");
    std::fs::write(
        skills.join("nodesc/SKILL.md"),
        "---\nname: nodesc\n---\nbody\n",
    )
    .expect("write SKILL.md");

    let diags = invalid_skill_diags(&env, "antigravity");
    for skill in ["noname", "nodesc"] {
        let hits = diags
            .iter()
            .filter(|d| {
                d["message"]
                    .as_str()
                    .map(|m| m.contains(skill))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(hits, 1, "warn exactly once for {skill}: {diags:?}");
        assert!(
            !user_skills_dir(&env, "antigravity").join(skill).exists(),
            "invalid skill {skill} must be skipped"
        );
    }
    assert!(
        user_skills_dir(&env, "antigravity")
            .join("review/SKILL.md")
            .is_file(),
        "valid skills must still install"
    );
}

// ---------------------------------------------------------------------------
// 5. Shared instructions regression (all four agents, user + project scope)
// ---------------------------------------------------------------------------

const ALL: &[&str] = &["claude", "codex", "opencode", "antigravity"];

#[test]
fn instructions_project_scope_all_agents() {
    let env = MusterEnv::new();
    let project = project_with_manifest(&env, ALL);
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .code(0);
    let agents_md = read(&project.join("AGENTS.md"));
    assert_eq!(
        agents_md.matches("Be concise.").count(),
        1,
        "canonical text must appear exactly once in project AGENTS.md:\n{agents_md}"
    );
    let claude_md = read(&project.join("CLAUDE.md"));
    assert!(
        claude_md.contains("@AGENTS.md"),
        "project CLAUDE.md must import AGENTS.md:\n{claude_md}"
    );
}

#[test]
fn instructions_user_scope_all_agents() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), ALL);
    env.run(&["agents", "apply", "--yes"]).assert().code(0);
    let claude_md = read(&env.path().join(".claude/CLAUDE.md"));
    assert!(
        claude_md.contains("@AGENTS.md"),
        "user CLAUDE.md must import @AGENTS.md:\n{claude_md}"
    );
    for path in [
        env.path().join(".claude/AGENTS.md"),
        env.path().join(".codex/AGENTS.md"),
        env.path().join(".config/opencode/AGENTS.md"),
        env.path().join(".gemini/AGENTS.md"),
    ] {
        assert!(
            read(&path).contains("Be concise."),
            "{} must contain the canonical text",
            path.display()
        );
    }
}

#[test]
fn instructions_second_apply_reports_no_changes() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), ALL);
    env.run(&["agents", "apply", "--yes"]).assert().code(0);
    let (output, envelope) = env.run_json(&["--format", "json", "agents", "apply", "--yes"]);
    assert!(output.status.success(), "second apply must exit 0");
    assert!(envelope.ok(), "second apply envelope ok=true");
    assert_eq!(
        envelope.result()["changes"],
        0,
        "second apply must report no changes: {}",
        envelope.raw
    );
    for agent in envelope.result()["agents"].as_array().expect("agents") {
        assert_eq!(
            agent["status"], "unchanged",
            "every adapter must be unchanged on second apply: {agent}"
        );
    }
}
