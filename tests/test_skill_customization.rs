//! Skill customization overlays: stitched on top at render time while the
//! vendored files, their pin and upstream updates stay clean.
mod support;

use std::path::Path;
use std::process::Command;
use support::MusterEnv;

const BASE: &str = "---\nname: foo\ndescription: Foo skill\n---\n## Base\nUpstream instructions.\n";
const BASE_V2: &str =
    "---\nname: foo\ndescription: Foo skill\n---\n## Base v2\nUpdated upstream instructions.\n";
const TEAM_RULES: &str = "## Team Rules\nUse strict mode.";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Up",
            "-c",
            "user.email=up@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn upstream(text: &str) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    publish(dir.path(), text);
    dir
}

fn publish(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir.join("skills/foo")).unwrap();
    std::fs::write(dir.join("skills/foo/SKILL.md"), text).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", "publish"]);
}

/// Initialized config with `foo` subscribed from a local upstream.
fn subscribed(extra_manifest: &str) -> (MusterEnv, tempfile::TempDir) {
    let env = MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let up = upstream(BASE);
    let url = format!("file://{}", up.path().display());
    env.run(&[
        "skill",
        "add",
        "--url",
        &url,
        "--ref",
        "main",
        "--path",
        "skills",
        "--include",
        "foo",
        "--source-name",
        "up",
        "--yes",
    ])
    .assert()
    .success();
    if !extra_manifest.is_empty() {
        let path = env.config_dir().join("muster.toml");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(extra_manifest);
        std::fs::write(&path, text).unwrap();
        // `agents apply` verifies the signed fleet state: sign the hand edit.
        env.run(&["trust", "resign", "--yes"]).assert().success();
    }
    (env, up)
}

fn rendered(env: &MusterEnv) -> String {
    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .success();
    std::fs::read_to_string(env.path().join(".claude/skills/foo/SKILL.md")).unwrap()
}

#[test]
fn file_overlay_is_stitched_after_a_divider() {
    let (env, _up) = subscribed("");
    let dir = env.config_dir().join("skill-overlays/foo");
    std::fs::create_dir_all(dir.join("references")).unwrap();
    std::fs::write(dir.join("SKILL.append.md"), TEAM_RULES).unwrap();
    std::fs::write(dir.join("references/custom.md"), "extra\n").unwrap();
    let md = rendered(&env);
    assert!(md.contains("Upstream instructions."), "{md}");
    assert!(md.contains(TEAM_RULES), "{md}");
    assert!(md.contains("\n---\n"), "{md}");
    assert!(md.find("Upstream").unwrap() < md.find("Team Rules").unwrap());
    let extra = std::fs::read_to_string(env.path().join(".claude/skills/foo/references/custom.md"));
    assert_eq!(extra.unwrap(), "extra\n");
    // The vendored copy and its pin are untouched.
    let vendored = std::fs::read_to_string(env.config_dir().join("skills/foo/SKILL.md")).unwrap();
    assert_eq!(vendored, BASE);
}

#[test]
fn manifest_overlay_is_stitched_and_reported() {
    let (env, _up) =
        subscribed("\n[agents.skills.customize.foo]\nappend = \"## Extra\\nAlways test.\"\n");
    let md = rendered(&env);
    assert!(md.contains("Upstream instructions."), "{md}");
    assert!(md.contains("## Extra\nAlways test."), "{md}");
    let (_, list) = env.run_json(&["skill", "list", "--format", "json"]);
    let foo = &list.result()["skills"][0];
    assert_eq!(foo["customized"], true, "{foo}");
    assert_eq!(foo["modified"], false, "{foo}");
    // No false drift in `effective`.
    let (_, eff) = env.run_json(&["effective", "--format", "json"]);
    assert!(
        eff.raw.to_string().contains("\"customized\":true"),
        "{}",
        eff.raw
    );
    assert!(!eff.raw.to_string().contains("drifted"), "{}", eff.raw);
}

#[test]
fn upstream_update_keeps_the_overlay_without_conflict() {
    let (env, up) = subscribed("");
    let dir = env.config_dir().join("skill-overlays/foo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.append.md"), TEAM_RULES).unwrap();
    let before = std::fs::read_to_string(env.config_dir().join("skills.lock")).unwrap();
    publish(up.path(), BASE_V2);
    let (out, result) = env.run_json(&["skill", "update", "--all", "--yes", "--format", "json"]);
    assert!(out.status.success(), "{}", result.raw);
    assert!(
        !result.diagnostics().to_string().contains("skill_modified"),
        "{}",
        result.raw
    );
    let after = std::fs::read_to_string(env.config_dir().join("skills.lock")).unwrap();
    assert_ne!(before, after, "lock must advance");
    let md = rendered(&env);
    assert!(md.contains("Updated upstream instructions."), "{md}");
    assert!(!md.contains("\nUpstream instructions."), "{md}");
    assert!(md.contains(TEAM_RULES), "{md}");
}

#[test]
fn customize_merges_by_skill_across_team_and_profile_layers() {
    let (env, _up) = subscribed(
        "\n[agents.skills.customize.foo]\nappend = \"## Team\\nTeam rule.\"\n\
         [machines.laptop]\nrecipient = 'age1laptop'\nplatform = 'linux-x86_64'\n\
         muster_version = '0.1.0'\nprofile = 'work'\n\
         [profiles.work.agents.skills.customize.foo]\nappend = \"## Work\\nWork rule.\"\n\
         [profiles.work.agents.skills.customize.bar]\nappend = \"unused\"\n",
    );
    std::fs::create_dir_all(env.path().join(".local/state/muster")).unwrap();
    std::fs::write(env.path().join(".local/state/muster/machine"), "laptop\n").unwrap();
    let md = rendered(&env);
    assert!(md.contains("Upstream instructions."), "{md}");
    assert!(md.contains("Work rule."), "{md}");
}

#[test]
fn customize_command_sets_and_clears() {
    let (env, _up) = subscribed("");
    env.run(&[
        "skill",
        "customize",
        "foo",
        "--append",
        "## CLI\nHello.",
        "--yes",
    ])
    .assert()
    .success();
    assert!(rendered(&env).contains("## CLI\nHello."));
    env.run(&["skill", "customize", "foo", "--clear", "--yes"])
        .assert()
        .success();
    assert!(!rendered(&env).contains("Hello."));
}
