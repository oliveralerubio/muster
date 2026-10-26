//! A fresh machine installs pinned agent binaries as part of sync.
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};
use support::MusterEnv;

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

/// `muster init --yes`; returns the recovery key.
fn init(env: &MusterEnv) -> String {
    env.fake_exe("claude", "#!/bin/sh\nexit 1\n");
    ok(env, &["init", "--yes", "--machine", "laptop"])["recovery_key"]
        .as_str()
        .unwrap()
        .to_string()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn claude_artifact(env: &MusterEnv) -> (String, String) {
    let artifact = env.path().join("artifacts/claude");
    let script = "#!/bin/sh\necho \"claude 1.0.0\"\n";
    write(&artifact, script);
    std::fs::set_permissions(&artifact, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sha256 = format!("{:x}", Sha256::digest(script.as_bytes()));
    (format!("file://{}", artifact.display()), sha256)
}

fn assert_claude_installed(env: &MusterEnv) {
    let installed = env.path().join(".local/bin/claude");
    assert!(
        installed.is_file(),
        "sync did not install {}",
        installed.display()
    );
    assert_ne!(
        std::fs::metadata(&installed).unwrap().permissions().mode() & 0o111,
        0,
        "installed claude binary is not executable"
    );
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        std::fs::read(env.path().join("artifacts/claude")).unwrap(),
        "sync did not install the pinned artifact"
    );

    let status = ok(env, &["status"]);
    let agents = status["convergence"]["agents"]
        .as_array()
        .unwrap_or_else(|| panic!("status has no agent convergence report: {status}"));
    let claude = agents
        .iter()
        .find(|agent| agent["id"] == "claude")
        .unwrap_or_else(|| panic!("status does not report claude: {status}"));
    assert_eq!(claude["installed_version"], "1.0.0", "{status}");
    assert_eq!(claude["pinned_version"], "1.0.0", "{status}");
}

#[test]
fn sync_installs_pinned_agent_matching_machine_platform_on_fresh_machine() {
    let env = MusterEnv::new();
    init(&env);
    let (url, sha256) = claude_artifact(&env);
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let lock = format!(
        "[agents.claude]\nversion = \"1.0.0\"\n\n[agents.claude.artifacts.\"{platform}\"]\nurl = \"{url}\"\nsha256 = \"{sha256}\"\n"
    );
    write(&env.config_dir().join("muster.lock"), &lock);

    ok(&env, &["sync", "--yes"]);

    assert_claude_installed(&env);
}

#[test]
fn sync_respects_legacy_single_artifact_pin() {
    let env = MusterEnv::new();
    init(&env);
    let (url, sha256) = claude_artifact(&env);
    let lock =
        format!("[agents.claude]\nversion = \"1.0.0\"\nurl = \"{url}\"\nsha256 = \"{sha256}\"\n");
    write(&env.config_dir().join("muster.lock"), &lock);

    ok(&env, &["sync", "--yes"]);

    assert_claude_installed(&env);
}

fn own_skill(env: &MusterEnv) -> std::path::PathBuf {
    ok(env, &["skill", "add", "review"]);
    env.config_dir().join("skills/review/SKILL.md")
}

#[test]
fn modified_skill_is_skipped_and_all_previous_files_stay_owned() {
    let env = MusterEnv::new();
    init(&env);
    let source = own_skill(&env);
    write(
        &source.parent().unwrap().join("old.md"),
        "old reviewed helper\n",
    );
    ok(&env, &["skill", "pin", "review"]);
    ok(&env, &["sync", "--yes"]);
    let installed = env.path().join(".claude/skills/review/SKILL.md");
    let original = std::fs::read_to_string(&installed).unwrap();
    write(
        &source,
        &original.replace("Instructions for the agent.", "Unreviewed change."),
    );
    std::fs::remove_file(source.parent().unwrap().join("old.md")).unwrap();

    let (out, envelope) = env.run_json(&["--format", "json", "sync", "--yes"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert!(envelope
        .raw
        .to_string()
        .contains("skill_verification_failed"));
    assert_eq!(
        envelope.result()["skipped_skills"],
        serde_json::json!(["review"])
    );
    assert_eq!(std::fs::read_to_string(&installed).unwrap(), original);
    assert_eq!(
        std::fs::read_to_string(installed.parent().unwrap().join("old.md")).unwrap(),
        "old reviewed helper\n"
    );
    let status = ok(&env, &["status"]);
    assert_eq!(status["convergence"]["converged"], false);
    assert_eq!(
        status["convergence"]["skipped_skills"],
        serde_json::json!(["review"])
    );

    // A later explicit approval must still own and remove the obsolete file.
    ok(&env, &["skill", "pin", "review"]);
    ok(&env, &["sync", "--yes"]);
    assert!(!installed.parent().unwrap().join("old.md").exists());
    assert!(std::fs::read_to_string(installed)
        .unwrap()
        .contains("Unreviewed change."));
    assert_eq!(ok(&env, &["status"])["convergence"]["converged"], true);
}

#[test]
fn unpinned_skill_preserves_previous_copy_and_does_not_install_new_files() {
    let env = MusterEnv::new();
    init(&env);
    let source = own_skill(&env);
    ok(&env, &["sync", "--yes"]);
    let installed = env.path().join(".claude/skills/review/SKILL.md");
    let original = std::fs::read(&installed).unwrap();
    write(&env.config_dir().join("skills.lock"), "");
    write(&source.parent().unwrap().join("new.md"), "unreviewed\n");
    let (out, envelope) = env.run_json(&["--format", "json", "sync", "--yes"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert!(envelope.raw.to_string().contains("skill_unreviewed"));
    assert_eq!(std::fs::read(installed).unwrap(), original);
    assert!(!env.path().join(".claude/skills/review/new.md").exists());
}

#[test]
fn pinning_an_edited_own_skill_allows_sync_to_install_it() {
    let env = MusterEnv::new();
    init(&env);
    let source = own_skill(&env);
    let edited = "---\nname: review\ndescription: Review code\n---\nReviewed new instructions.\n";
    write(&source, edited);
    let skipped = ok(&env, &["sync", "--yes"]);
    assert_eq!(skipped["skipped_skills"], serde_json::json!(["review"]));
    assert!(!env.path().join(".claude/skills/review/SKILL.md").exists());
    let before = std::fs::read(env.config_dir().join("skills.lock")).unwrap();
    ok(&env, &["--dry-run", "skill", "pin", "review"]);
    assert_eq!(
        std::fs::read(env.config_dir().join("skills.lock")).unwrap(),
        before
    );
    ok(&env, &["skill", "pin", "review"]);
    ok(&env, &["sync", "--yes"]);
    assert_eq!(
        std::fs::read_to_string(env.path().join(".claude/skills/review/SKILL.md")).unwrap(),
        edited
    );
}

#[test]
fn pin_can_review_an_existing_unpinned_own_skill_but_not_a_subscription() {
    let env = MusterEnv::new();
    init(&env);
    let source = env.config_dir().join("skills/review/SKILL.md");
    write(
        &source,
        "---\nname: review\ndescription: Review code\n---\nReview.\n",
    );
    ok(&env, &["skill", "pin", "review"]);
    let path = env.config_dir().join("skills.lock");
    let lock = std::fs::read_to_string(&path)
        .unwrap()
        .replace("origin = \"local\"", "origin = \"upstream\"");
    write(&path, &lock);
    env.run(&["skill", "pin", "review"]).assert().code(2);
    assert_eq!(std::fs::read_to_string(path).unwrap(), lock);
}

#[test]
fn convergence_reports_missing_agent_and_pending_approval_then_converges() {
    let env = MusterEnv::new();
    env.fake_exe("claude", "#!/bin/sh\nexit 1\n");
    init(&env);
    let (url, sha256) = claude_artifact(&env);
    write(
        &env.config_dir().join("muster.lock"),
        &format!("[agents.claude]\nversion = \"1.0.0\"\nurl = \"{url}\"\nsha256 = \"{sha256}\"\n"),
    );
    let status = ok(&env, &["status"]);
    assert_eq!(status["convergence"]["converged"], false);
    assert_eq!(
        status["convergence"]["agents"][0]["installed_version"],
        Value::Null
    );
    assert_eq!(
        status["convergence"]["pending_approvals"][0]["name"],
        "claude"
    );
    ok(&env, &["sync", "--yes"]);
    assert_eq!(ok(&env, &["status"])["convergence"]["converged"], true);
}

#[test]
fn doctor_reports_agents_without_pins() {
    let env = MusterEnv::new();
    init(&env);
    let (_, envelope) = env.run_json(&["--format", "json", "doctor", "--offline"]);
    assert!(
        envelope.raw.to_string().contains("agent_unpinned"),
        "{}",
        envelope.raw
    );
}

#[test]
fn convergence_checks_reported_version_and_uses_checksum_when_version_is_unknown() {
    let env = MusterEnv::new();
    init(&env);
    let path = env.path().join(".local/bin/claude");
    let script = "#!/bin/sh\necho claude 2.0.0\n";
    write(&path, script);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    write(
        &env.config_dir().join("muster.lock"),
        &format!(
        "[agents.claude]\nversion = \"1.0.0\"\nurl = \"file:///not-needed\"\nsha256 = \"{:x}\"\n",
        Sha256::digest(script.as_bytes())
    ),
    );
    let status = ok(&env, &["status"]);
    assert_eq!(
        status["convergence"]["agents"][0]["installed_version"],
        "2.0.0"
    );
    assert_eq!(status["convergence"]["converged"], true);
    assert_eq!(status["convergence"]["agents"][0]["kept_newer"], true);
    let (output, envelope) = env.run_json(&["--format", "json", "sync", "--yes"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert!(envelope.raw.to_string().contains("agent_newer_than_pin"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), script);

    let script = "#!/bin/sh\nexit 1\n";
    write(&path, script);
    write(
        &env.config_dir().join("muster.lock"),
        &format!(
        "[agents.claude]\nversion = \"1.0.0\"\nurl = \"file:///not-needed\"\nsha256 = \"{:x}\"\n",
        Sha256::digest(script.as_bytes())
    ),
    );
    let status = ok(&env, &["status"]);
    assert_eq!(
        status["convergence"]["agents"][0]["installed_version"],
        "1.0.0"
    );
    assert_eq!(status["convergence"]["converged"], true);
}

#[test]
fn convergence_lists_pins_without_an_artifact_for_this_platform() {
    let env = MusterEnv::new();
    init(&env);
    write(&env.config_dir().join("muster.lock"), &format!(
        "[agents.claude]\nversion = \"1.0.0\"\n[agents.claude.artifacts.unsupported]\nurl = \"file:///not-needed\"\nsha256 = \"{}\"\n",
        "0".repeat(64)
    ));
    let convergence = &ok(&env, &["status"])["convergence"];
    assert_eq!(convergence["agents"][0]["id"], "claude");
    assert_eq!(convergence["agents"][0]["installed_version"], Value::Null);
    assert_eq!(convergence["agents"][0]["artifact"], Value::Null);
    assert!(convergence["agents"][0]["note"]
        .as_str()
        .unwrap()
        .contains("no artifact"));
    assert_eq!(convergence["pending_approvals"], serde_json::json!([]));
    assert_eq!(convergence["converged"], false);
}

#[test]
fn matching_reported_version_with_different_hash_is_not_replanned_or_downloaded() {
    let env = MusterEnv::new();
    init(&env);
    let path = env.path().join(".local/bin/claude");
    let script = "#!/bin/sh\necho 'Claude Code (v1.0.0)'\n";
    write(&path, script);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    write(&env.config_dir().join("muster.lock"), &format!("[agents.claude]\nversion = \"1.0.0\"\nurl = \"file:///must-not-fetch\"\nsha256 = \"{}\"\n", "0".repeat(64)));
    for args in [&["plan"][..], &["sync", "--yes"][..]] {
        let result = ok(&env, args);
        assert_eq!(result["installs"], serde_json::json!([]));
        assert_eq!(result["approvals"], serde_json::json!([]));
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), script);
    assert_eq!(ok(&env, &["status"])["convergence"]["converged"], true);
    assert_eq!(
        ok(&env, &["agents", "install"])["installed"][0]["action"],
        "unchanged"
    );
}

#[test]
fn empty_skill_directory_is_ignored_not_skipped_or_unreviewed() {
    let env = MusterEnv::new();
    init(&env);
    own_skill(&env);
    std::fs::create_dir_all(env.config_dir().join("skills/empty/nested")).unwrap();
    for args in [&["plan"][..], &["sync"][..], &["status"][..]] {
        let mut full = vec!["--format", "json"];
        full.extend_from_slice(args);
        let (out, envelope) = env.run_json(&full);
        assert!(out.status.success(), "{}", envelope.raw);
        assert!(!envelope.raw.to_string().contains("skill_unreviewed"));
        assert!(!envelope.raw.to_string().contains("skill_invalid"));
        let report = if args[0] == "status" {
            &envelope.result()["convergence"]
        } else {
            envelope.result()
        };
        assert_eq!(report["skipped_skills"], serde_json::json!([]));
    }
}

#[test]
fn rollback_reports_that_agent_binaries_are_not_downgraded() {
    let env = MusterEnv::new();
    init(&env);
    let (url, sha256) = claude_artifact(&env);
    write(
        &env.config_dir().join("muster.lock"),
        &format!("[agents.claude]\nversion = \"1.0.0\"\nurl = {url:?}\nsha256 = {sha256:?}\n"),
    );
    let generation = ok(&env, &["sync", "--yes"])["generation"]
        .as_u64()
        .unwrap()
        .to_string();
    let binary = env.path().join(".local/bin/claude");
    let newer = "#!/bin/sh\necho claude 2.0.0\n";
    write(&binary, newer);
    for dry in [true, false] {
        let mut args = vec!["--format", "json", "rollback", &generation, "--yes"];
        if dry {
            args.push("--dry-run");
        }
        let (out, envelope) = env.run_json(&args);
        assert!(out.status.success(), "{}", envelope.raw);
        assert!(envelope.result()["note"]
            .as_str()
            .unwrap()
            .contains("agent binaries are not downgraded"));
        assert!(envelope.raw["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "rollback_agents_unchanged" && d["severity"] == "info"));
        assert_eq!(std::fs::read_to_string(&binary).unwrap(), newer);
    }
}

#[test]
fn custom_binary_name_is_shared_by_manual_install_sync_status_and_doctor() {
    let env = MusterEnv::new();
    init(&env);
    write(
        &env.config_dir().join("muster.toml"),
        "version = 1\n[agents]\nenabled = [\"claude\"]\n",
    );
    let (url, sha256) = claude_artifact(&env);
    write(&env.config_dir().join("muster.lock"), &format!(
        "[agents.claude]\nversion = \"1.0.0\"\nbin = \"reviewed-claude\"\nurl = {url:?}\nsha256 = {sha256:?}\n"
    ));
    let installed = ok(&env, &["agents", "install"]);
    assert_eq!(
        installed["bin_dir"],
        env.path().join(".local/bin").display().to_string()
    );
    assert!(env.path().join(".local/bin/reviewed-claude").is_file());
    assert!(!env.path().join(".local/bin/claude").exists());
    assert_eq!(ok(&env, &["sync"])["installs"], serde_json::json!([]));
    assert_eq!(
        ok(&env, &["status"])["convergence"]["agents"][0]["installed_version"],
        "1.0.0"
    );
    let (_, doctor) = env.run_json(&["--format", "json", "doctor", "--offline"]);
    let diagnostics = doctor.raw["diagnostics"].as_array().unwrap();
    assert!(
        !diagnostics
            .iter()
            .any(|d| d["code"] == "missing_binary" || d["code"] == "version_unknown"),
        "{}",
        doctor.raw
    );
}
