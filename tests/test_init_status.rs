mod support;

use predicates::prelude::*;

const RECOVERY_PREFIX: &str = "AGE-SECRET-KEY-1";

/// Extract every offline recovery key found in `text` (an `AGE-SECRET-KEY-1…`
/// token; age x25519 secret keys are that prefix plus 58 base58 chars).
fn recovery_keys(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
        if token.starts_with(RECOVERY_PREFIX) {
            keys.push(token.to_string());
        }
    }
    keys
}

fn state_dir(env: &support::MusterEnv) -> std::path::PathBuf {
    env.path().join(".local/state").join("muster")
}

fn init_yes(env: &support::MusterEnv) -> (std::process::Output, support::Envelope) {
    env.run_json(&["--format", "json", "--yes", "init"])
}

#[test]
fn init_creates_git_repo_manifest_identity_and_empty_vault() {
    let env = support::MusterEnv::new();
    let (output, envelope) = init_yes(&env);
    assert!(output.status.success(), "init must exit 0");
    assert!(envelope.ok(), "envelope must be ok");
    assert_eq!(envelope.command(), "init");
    assert_eq!(envelope.schema_version(), 1);

    let cfg = env.config_dir();
    assert!(cfg.join(".git").exists(), "config repo must be a git repo");
    let manifest = std::fs::read_to_string(cfg.join("muster.toml")).expect("muster.toml exists");
    assert!(
        manifest.contains("version"),
        "manifest must be valid structure:\n{manifest}"
    );
    // The created repo must load: `schema` runs against the config dir.
    env.run(&["schema"]).assert().code(0);

    // Vault dir exists (empty vault setup).
    assert!(
        cfg.join("vault").is_dir(),
        "vault dir must exist under the config repo"
    );

    // Machine identity lives in the state dir with 0600 file / 0700 dir perms.
    let state = state_dir(&env);
    let identity = state.join("identity.txt");
    assert!(identity.is_file(), "identity.txt must exist in state dir");
    use std::os::unix::fs::PermissionsExt;
    let file_mode = std::fs::metadata(&identity).unwrap().permissions().mode();
    assert_eq!(file_mode & 0o777, 0o600, "identity.txt must be 0600");
    let dir_mode = std::fs::metadata(&state).unwrap().permissions().mode();
    assert_eq!(dir_mode & 0o777, 0o700, "state dir must be 0700");
}

#[test]
fn init_shows_recovery_key_exactly_once_and_never_stores_it() {
    let env = support::MusterEnv::new();
    let (output, envelope) = init_yes(&env);
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let mut keys = recovery_keys(&stdout);
    keys.extend(recovery_keys(&stderr));
    assert_eq!(
        keys.len(),
        1,
        "recovery key must be shown exactly once:\nstdout={stdout}\nstderr={stderr}"
    );
    let key = &keys[0];
    assert_eq!(
        key.len(),
        RECOVERY_PREFIX.len() + 58,
        "age x25519 secret key shape"
    );
    // It is reported in the JSON result too (machine-readable path).
    let result = envelope.result().to_string();
    assert!(
        result.contains(key.as_str()),
        "JSON result must carry the recovery key: {result}"
    );

    // Never saved on any machine: not in the config repo nor the state dir.
    for root in [env.config_dir(), state_dir(&env)] {
        let stored = walk_contains(&root, key);
        assert!(!stored, "recovery key must not be stored under {root:?}");
    }
}

fn walk_contains(root: &std::path::Path, needle: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                if text.contains(needle) {
                    return true;
                }
            }
        }
    }
    false
}

#[test]
fn init_without_tty_and_without_yes_fails_with_usage_exit_code_2() {
    let env = support::MusterEnv::new();
    // No TTY (stdin is null) and no --yes: must refuse instead of hanging.
    let assert = env
        .run(&["init"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty()); // text mode failure: nothing on stdout
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("confirm") || stderr.contains("yes") || !stderr.is_empty(),
        "a diagnostic must explain the refusal:\n{stderr}"
    );
    // Nothing was created: the refusal happens before any writes.
    assert!(
        !env.config_dir().join("muster.toml").exists(),
        "refused init must not create the manifest"
    );
}

#[test]
fn init_without_tty_with_yes_succeeds_and_reports_recovery_key() {
    let env = support::MusterEnv::new();
    let (output, envelope) = env.run_json(&["--format", "json", "init", "--yes"]);
    assert!(output.status.success(), "init --yes must succeed");
    assert!(envelope.ok());
    let keys = recovery_keys(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(keys.len(), 1, "exactly one recovery key in JSON stdout");
}

#[test]
fn status_lists_fleet_machines_in_text_and_json_and_exits_zero() {
    let env = support::MusterEnv::new();
    init_yes(&env);

    // Text mode: exit 0 and human-readable fleet listing.
    let assert = env.run(&["status"]).assert().code(0);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        !stdout.trim().is_empty(),
        "text status must print something"
    );

    // JSON mode: one envelope with machine entries carrying platform,
    // muster version and current machine status.
    let (output, envelope) = env.run_json(&["--format", "json", "status"]);
    assert!(output.status.success(), "status must exit 0");
    assert!(envelope.ok());
    assert_eq!(envelope.command(), "status");

    let machines = envelope
        .result()
        .get("machines")
        .cloned()
        .or_else(|| {
            // Tolerate a bare array result.
            if envelope.result().is_array() {
                Some(envelope.result().clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("status result must list machines: {}", envelope.raw));
    let machines = machines.as_array().expect("machines must be an array");
    assert!(
        !machines.is_empty(),
        "this machine must appear in the fleet after init"
    );
    for machine in machines {
        for field in ["platform", "version", "status"] {
            assert!(
                machine.get(field).is_some(),
                "machine entry must include {field}: {machine}"
            );
        }
    }
}

#[test]
fn status_before_init_reports_operational_failure_exit_1() {
    let env = support::MusterEnv::new();
    let (output, envelope) = env.run_json(&["--format", "json", "status"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "no config repo yet: operational error"
    );
    assert!(!envelope.ok(), "envelope must report failure");
    assert_eq!(envelope.diagnostics()[0]["severity"], "error");
}

#[test]
fn init_detects_installed_agents_and_offers_import() {
    let env = support::MusterEnv::new();
    env.fake_exe("claude", "exit 0");
    env.fake_exe("codex", "exit 0");
    let (output, envelope) = init_yes(&env);
    assert!(output.status.success());
    let agents = envelope.result()["detected_agents"]
        .as_array()
        .expect("detected_agents array");
    assert!(agents.contains(&"claude".into()) && agents.contains(&"codex".into()));
    let diags = envelope.diagnostics().as_array().unwrap();
    assert!(
        diags.iter().any(|d| d["code"] == "agents_detected"),
        "init must offer to import detected agents: {diags:?}"
    );

    let (_, status) = env.run_json(&["--format", "json", "status"]);
    let machine = &status.result()["machines"][0];
    assert!(machine["agents"]
        .as_array()
        .unwrap()
        .contains(&"claude".into()));
    assert!(machine.get("last_sync").is_some());
}

#[test]
fn init_records_recovery_recipient_for_all_groups() {
    let env = support::MusterEnv::new();
    let (_, envelope) = init_yes(&env);
    let recipient = envelope.result()["recovery_recipient"]
        .as_str()
        .expect("recovery recipient");
    assert!(recipient.starts_with("age1"));
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(
        manifest.contains(recipient),
        "manifest must list the recovery recipient"
    );
}

#[test]
fn init_twice_refuses_to_overwrite() {
    let env = support::MusterEnv::new();
    init_yes(&env);
    env.run(&["--yes", "init"]).assert().code(1);
}
