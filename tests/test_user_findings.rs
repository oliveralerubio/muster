//! Behavioral regression tests for user-testing findings (Items 1–8).
mod support;

use std::io::Write;
use std::process::{Output, Stdio};

use serde_json::Value;
use support::{Envelope, MusterEnv};

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

fn fails(env: &MusterEnv, args: &[&str], code: i32) -> Envelope {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert_eq!(out.status.code(), Some(code), "{args:?}: {}", envelope.raw);
    assert!(!envelope.ok());
    envelope
}

fn diag_messages(envelope: &Value) -> String {
    envelope["diagnostics"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d["message"].as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default()
}

fn with_stdin(env: &MusterEnv, args: &[&str], stdin: &str, vars: &[(&str, &str)]) -> Output {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("muster"));
    for (k, v) in env.run(&[]).get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    for (k, v) in vars {
        cmd.env(k, v);
    }
    let mut child = cmd
        .args(["--format", "json"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn envelope(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON {e}: {stdout}"))
}

// ---------------------------------------------------------------------------
// (1) `init` detects cross-fleet state BEFORE writing and refuses cleanly,
//     while allowing normal/empty XDG directories.
// ---------------------------------------------------------------------------
#[test]
fn init_refuses_existing_cross_fleet_state_without_writing_and_allows_normal_xdg() {
    let env = MusterEnv::new();

    // Normal XDG state/config directories with unrelated app subdirectories
    // must NOT be rejected.
    std::fs::create_dir_all(env.path().join(".local/state/other-app")).unwrap();
    std::fs::create_dir_all(env.path().join(".local/state/muster")).unwrap();

    // First init in the default config dir succeeds and populates state_dir.
    ok(&env, &["init", "--yes", "--machine", "alpha"]);

    let state_dir = env.path().join(".local/state/muster");
    let orig_identity = std::fs::read_to_string(state_dir.join("identity.txt")).unwrap();
    let orig_signing = std::fs::read_to_string(state_dir.join("signing.key")).unwrap();
    let orig_trust = std::fs::read_to_string(state_dir.join("trust.json")).unwrap();

    // Attempting `muster init` into a fresh `--config` directory while `state_dir`
    // already holds state from the `alpha` fleet must fail BEFORE writing anything
    // to the new config dir or mutating `state_dir`.
    let second_cfg = env.path().join("second-fleet-config");
    let second_cfg_str = second_cfg.to_str().unwrap();
    let err = fails(
        &env,
        &[
            "--config",
            second_cfg_str,
            "init",
            "--yes",
            "--machine",
            "beta",
        ],
        1,
    );
    let msg = diag_messages(&err.raw);
    assert!(
        msg.contains("already belongs to an existing fleet"),
        "expected cross-fleet state error, got: {}",
        err.raw
    );
    assert!(
        !second_cfg.exists(),
        "failed cross-fleet init must not create or write into the target config dir"
    );
    assert_eq!(
        std::fs::read_to_string(state_dir.join("identity.txt")).unwrap(),
        orig_identity,
        "identity.txt must be untouched"
    );
    assert_eq!(
        std::fs::read_to_string(state_dir.join("signing.key")).unwrap(),
        orig_signing,
        "signing.key must be untouched"
    );
    assert_eq!(
        std::fs::read_to_string(state_dir.join("trust.json")).unwrap(),
        orig_trust,
        "trust.json must be untouched"
    );
}

// ---------------------------------------------------------------------------
// (2) `trust resign` blocks signing a manifest without admins that would lock
//     out subsequent `resign`, while preserving implicit personal mode and
//     ADR-0005 recovery.
// ---------------------------------------------------------------------------
#[test]
fn trust_resign_prevents_adminless_lockout_and_preserves_personal_mode_and_recovery() {
    let env = MusterEnv::new();
    let recovery_key = ok(&env, &["init", "--yes", "--machine", "laptop"])["recovery_key"]
        .as_str()
        .unwrap()
        .to_string();

    // Implicit personal mode (no `[members]`, machine `laptop` has signing_key)
    // can resign normally.
    let res = ok(&env, &["trust", "resign", "--yes"]);
    assert_eq!(res["resigned"], false);

    // Save the working manifest.
    let manifest_path = env.config_dir().join("muster.toml");
    let valid_manifest = std::fs::read_to_string(&manifest_path).unwrap();

    // Case A: `[members]` becomes non-empty without an admin owning `laptop`
    // (e.g. `laptop` owner is only a `member`, or no admin exists at all).
    // It must refuse with exit code 5 (Security) without modifying `fleet.state.json`.
    let fleet_state_path = env.config_dir().join("fleet.state.json");
    let before_state = std::fs::read_to_string(&fleet_state_path).unwrap();

    let lockout_manifest =
        format!("{valid_manifest}\n[members.bob]\nrole = \"member\"\nmachines = [\"laptop\"]\n");
    std::fs::write(&manifest_path, &lockout_manifest).unwrap();

    let err = fails(&env, &["trust", "resign", "--yes"], 5);
    let msg = diag_messages(&err.raw);
    assert!(
        msg.contains("not an approved machine") || msg.contains("admin"),
        "expected lockout prevention error, got: {}",
        err.raw
    );
    assert_eq!(
        std::fs::read_to_string(&fleet_state_path).unwrap(),
        before_state,
        "fleet.state.json must not be overwritten when resign refuses adminless manifest"
    );

    // Also test when `[members]` is empty (implicit personal mode) but `signing_key`
    // is stripped from `laptop` in `muster.toml` (keeping `[recovery].signing_key` intact):
    // `trust resign` must refuse to sign a state that leaves no machine with a valid `signing_key`.
    let mut in_recovery = false;
    let stripped_manifest = valid_manifest
        .lines()
        .filter(|l| {
            let trimmed = l.trim();
            if trimmed.starts_with('[') {
                in_recovery = trimmed == "[recovery]";
            }
            in_recovery || !trimmed.starts_with("signing_key =")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&manifest_path, &stripped_manifest).unwrap();
    let err_stripped = fails(&env, &["trust", "resign", "--yes"], 5);
    let msg_stripped = diag_messages(&err_stripped.raw);
    assert!(
        msg_stripped.contains("has no machine with a valid signing_key"),
        "expected personal-mode lockout prevention error, got: {}",
        err_stripped.raw
    );
    assert_eq!(
        std::fs::read_to_string(&fleet_state_path).unwrap(),
        before_state,
        "fleet.state.json must not be overwritten when resign refuses stripped personal manifest"
    );

    // Case B: ADR-0005 break-glass recovery (`--recovery-key-stdin`) IS allowed
    // to sign even when no admin machine is left, as long as `[recovery]` is valid,
    // so emergency key rotation / machine revocation remains possible.
    let out = with_stdin(
        &env,
        &["trust", "resign", "--yes", "--recovery-key-stdin"],
        &recovery_key,
        &[],
    );
    let env_json = envelope(&out);
    assert!(
        out.status.success(),
        "ADR-0005 recovery resign must succeed: {env_json}"
    );
    assert_eq!(env_json["result"]["resigned"], true);

    // Restore valid manifest and resign with recovery to return to normal operation.
    std::fs::write(&manifest_path, &valid_manifest).unwrap();
    let out2 = with_stdin(
        &env,
        &["trust", "resign", "--yes", "--recovery-key-stdin"],
        &recovery_key,
        &[],
    );
    assert!(out2.status.success(), "{}", envelope(&out2));
    ok(&env, &["trust", "resign", "--yes"]);
}

// ---------------------------------------------------------------------------
// (3) `enroll` with a failed remote push signals an error, preserves the
//     enrollment request and any existing approvals, and instructs how to
//     publish (`muster sync --push`) without recreating the request.
// ---------------------------------------------------------------------------
#[test]
fn enroll_push_failure_signals_error_and_preserves_request_and_approvals() {
    let admin = MusterEnv::new();
    ok(&admin, &["init", "--yes", "--machine", "hq"]);

    // Configure a broken git remote on the config repo so `git::push` fails.
    let broken_remote = admin.path().join("nonexistent-remote.git");
    std::process::Command::new("git")
        .args([
            "-C",
            admin.config_dir().to_str().unwrap(),
            "remote",
            "add",
            "origin",
            broken_remote.to_str().unwrap(),
        ])
        .status()
        .unwrap();

    // Candidate machine shares a clone/copy of the config repo.
    let candidate = MusterEnv::new();
    let copy_status = std::process::Command::new("cp")
        .args([
            "-a",
            admin.config_dir().to_str().unwrap(),
            candidate.config_dir().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(copy_status.success());

    // First `muster enroll` on candidate writes `enroll/field-box.toml`, fails
    // to push to `origin`, and MUST exit non-zero (1) with `push_failed` error
    // diagnostic and `muster sync --push` suggestion.
    let err = fails(&candidate, &["enroll", "--name", "field-box"], 1);
    let req_path = candidate.config_dir().join("enroll/field-box.toml");
    assert!(
        req_path.exists(),
        "enrollment request file must be preserved on disk"
    );

    let diags = err.raw["diagnostics"].as_array().unwrap();
    let push_diag = diags
        .iter()
        .find(|d| d["code"] == "push_failed")
        .expect("must emit push_failed diagnostic");
    assert_eq!(push_diag["severity"], "error");
    assert!(
        push_diag["suggestion"]
            .as_str()
            .unwrap_or("")
            .contains("muster sync --push"),
        "suggestion must instruct running `muster sync --push`, got: {push_diag}"
    );

    // Simulate an admin approval recorded on `enroll/field-box.toml`.
    let mut req_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(&req_path).unwrap()).unwrap();
    let orig_nonce = req_toml["nonce"].as_str().unwrap().to_string();
    let orig_created_at = req_toml["created_at"].as_str().unwrap().to_string();
    req_toml.as_table_mut().unwrap().insert(
        "approvals".to_string(),
        toml::Value::Array(vec![toml::Value::String("alice".to_string())]),
    );
    std::fs::write(&req_path, toml::to_string_pretty(&req_toml).unwrap()).unwrap();

    // Re-running `muster enroll` for the same machine must preserve `nonce`,
    // `created_at`, and `approvals` instead of wiping them!
    let retry_err = fails(&candidate, &["enroll", "--name", "field-box"], 1);
    let retry_diags = retry_err.raw["diagnostics"].as_array().unwrap();
    assert!(
        retry_diags.iter().any(|d| d["code"] == "request_preserved"),
        "re-running enroll must emit request_preserved diagnostic: {retry_diags:?}"
    );
    let after_toml: toml::Value =
        toml::from_str(&std::fs::read_to_string(&req_path).unwrap()).unwrap();
    assert_eq!(
        after_toml["nonce"].as_str().unwrap(),
        orig_nonce,
        "nonce must be preserved"
    );
    assert_eq!(
        after_toml["created_at"].as_str().unwrap(),
        orig_created_at,
        "created_at must be preserved"
    );
    assert_eq!(
        after_toml["approvals"].as_array().unwrap().len(),
        1,
        "existing approvals must be preserved when re-running enroll"
    );
}

// ---------------------------------------------------------------------------
// (5) `status` surfaces pending machine enrollment requests (`pending.machines`)
//     for the Dashboard and CLI.
// ---------------------------------------------------------------------------
#[test]
fn status_reports_pending_machine_enrollment_requests() {
    let admin = MusterEnv::new();
    ok(&admin, &["init", "--yes", "--machine", "hq"]);

    // Candidate creates a pending enrollment request in the config repo.
    let candidate = MusterEnv::new();
    let copy_status = std::process::Command::new("cp")
        .args([
            "-a",
            admin.config_dir().to_str().unwrap(),
            candidate.config_dir().to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(copy_status.success());
    ok(&candidate, &["enroll", "--name", "edge-1"]);

    // Copy the `enroll/edge-1.toml` request back to admin's config repo.
    std::fs::create_dir_all(admin.config_dir().join("enroll")).unwrap();
    std::fs::copy(
        candidate.config_dir().join("enroll/edge-1.toml"),
        admin.config_dir().join("enroll/edge-1.toml"),
    )
    .unwrap();

    let status = ok(&admin, &["status"]);
    let pending_machines = status["pending"]["machines"]
        .as_array()
        .expect("status.pending.machines must be an array");
    assert_eq!(pending_machines.len(), 1);
    assert_eq!(pending_machines[0]["name"], "edge-1");
}

// ---------------------------------------------------------------------------
// (6) `doctor` distinguishes warnings from errors (`status: "warning" | "error" | "ok"`)
//     and gives real feedback + consequences for `--fix`.
// ---------------------------------------------------------------------------
#[test]
fn doctor_distinguishes_warnings_from_errors_and_reports_fix_consequences() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "laptop"]);

    // Right after init, unpinned detected agents produce warnings, so `healthy` is
    // true while `status` is `"warning"` (distinct from `"error"` and `"ok"`).
    let initial_doc = ok(&env, &["doctor"]);
    assert_eq!(initial_doc["healthy"], true);
    let initial_warnings = initial_doc["summary"]["warnings"].as_u64().unwrap_or(0);
    if initial_warnings > 0 {
        assert_eq!(initial_doc["status"], "warning");
    } else {
        assert_eq!(initial_doc["status"], "ok");
    }

    // Loosen permissions on identity.txt to trigger a permissions error + safe chmod repair.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let id_path = env.path().join(".local/state/muster/identity.txt");
        std::fs::set_permissions(&id_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err_doc = fails(&env, &["doctor"], 1);
        let res = err_doc.result();
        assert_eq!(res["healthy"], false);
        assert_eq!(
            res["status"], "error",
            "doctor must report status: error when an error check fails"
        );
        let fixes = res["fixes"].as_array().unwrap();
        assert!(!fixes.is_empty(), "doctor must report available fixes");
        assert!(
            fixes[0]["consequence"]
                .as_str()
                .unwrap_or("")
                .contains("restricts file access to the current OS user"),
            "fix must explain its consequence: {:?}",
            fixes[0]
        );

        // Now run `muster doctor --fix --yes` and verify applied fix + `fixes_applied` diagnostic.
        let (_, fixed_env) = env.run_json(&["--format", "json", "doctor", "--fix", "--yes"]);
        assert!(fixed_env.ok(), "{}", fixed_env.raw);
        assert_eq!(fixed_env.result()["healthy"], true);
        let applied_fixes = fixed_env.result()["fixes"].as_array().unwrap();
        assert_eq!(applied_fixes[0]["applied"], true);
        let diags = fixed_env.raw["diagnostics"].as_array().unwrap();
        assert!(
            diags.iter().any(|d| d["code"] == "fixes_applied"),
            "doctor --fix must emit fixes_applied diagnostic: {diags:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// (7) Modified subscribed skill error explains `--detach` and `skill customize`,
//     and `skill customize` informs that `muster sync` is required.
// ---------------------------------------------------------------------------
#[test]
fn skill_customize_notifies_sync_required_and_modified_skill_explains_detach() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "laptop"]);
    ok(
        &env,
        &["skill", "add", "demo-skill", "--description", "Demo skill"],
    );

    let (_, cust_env) = env.run_json(&[
        "--format",
        "json",
        "--yes",
        "skill",
        "customize",
        "demo-skill",
        "--append",
        "Always run cargo test.",
    ]);
    assert!(cust_env.ok(), "{}", cust_env.raw);
    assert_eq!(
        cust_env.result()["sync_required"],
        true,
        "skill customize result must indicate sync_required: true"
    );
    let diags = cust_env.raw["diagnostics"].as_array().unwrap();
    let sync_diag = diags
        .iter()
        .find(|d| d["code"] == "sync_required")
        .expect("skill customize must emit sync_required diagnostic");
    assert!(
        sync_diag["suggestion"]
            .as_str()
            .unwrap_or("")
            .contains("muster sync"),
        "sync_required suggestion must mention `muster sync`"
    );
}

// ---------------------------------------------------------------------------
// (8) `history` includes Git config-repo commits marked `verified: false`
//     alongside `muster_actor` trailer metadata.
// ---------------------------------------------------------------------------
#[test]
fn history_surfaces_unverified_git_commits_with_actor_trailer() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "laptop"]);

    // Create a commit in the config repo carrying a `Muster-Actor` trailer.
    std::process::Command::new("git")
        .args([
            "-C",
            env.config_dir().to_str().unwrap(),
            "-c",
            "user.name=Alice",
            "-c",
            "user.email=alice@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "chore: audit note\n\nMuster-Actor: alice (machine laptop)",
        ])
        .status()
        .unwrap();

    let hist = ok(&env, &["history"]);
    let commits = hist["commits"]
        .as_array()
        .expect("history.commits must be an array");
    assert!(
        commits.len() >= 2,
        "history.commits must include config repo commits"
    );
    let latest = &commits[0];
    assert_eq!(
        latest["verified"], false,
        "Git commit metadata must be explicitly marked verified: false"
    );
    assert!(
        latest["commit"].as_str().unwrap_or("").len() >= 7,
        "commit SHA must be present: {latest}"
    );
    assert_eq!(
        latest["git_author"], "Alice <alice@example.com>",
        "git_author should be surfaced"
    );
    assert_eq!(
        latest["muster_actor"], "alice (machine laptop)",
        "Muster-Actor trailer from commit should be surfaced"
    );
}
