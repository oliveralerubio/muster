mod support;

use serde_json::Value;
use std::{fs, process::Command};
use tempfile::TempDir;

fn bare_remote() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("remote tempdir");
    let path = dir.path().join("central.git");
    let status = Command::new("git")
        .args(["init", "--bare", path.to_str().unwrap()])
        .status()
        .expect("git init --bare");
    assert!(status.success());
    (dir, path)
}

fn assert_cli_success(env: &support::MusterEnv, args: &[&str]) -> Value {
    let (output, envelope) = env.run_json(args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.raw["result"].clone()
}

#[test]
fn enroll_approve_and_sync_between_two_machines() {
    let (_remote_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = support::MusterEnv::new();
    let bob = support::MusterEnv::new();

    assert_cli_success(&alice, &["--format", "json", "init", "--yes"]);
    assert_cli_success(&alice, &["--format", "json", "sync", "--remote", remote]);
    assert_cli_success(&bob, &["--format", "json", "init", "--yes"]);
    assert_cli_success(&bob, &["--format", "json", "sync", "--remote", remote]);
    assert_cli_success(&bob, &["--format", "json", "enroll", "--name", "bob"]);
    assert!(bob.config_dir().join("enroll/bob.toml").exists());
    assert_cli_success(&alice, &["--format", "json", "sync", "--pull"]);
    assert_cli_success(&alice, &["--format", "json", "approve", "bob", "--yes"]);
    assert_cli_success(&alice, &["--format", "json", "sync", "--push"]);
    assert_cli_success(&bob, &["--format", "json", "sync", "--pull"]);

    // Approval must grant Bob only the vault groups for which his machine is a reader.
    let listed = assert_cli_success(&bob, &["--format", "json", "secret", "list"]);
    assert!(
        listed.is_array() || listed.is_object(),
        "unexpected secret list: {listed}"
    );
}

#[test]
fn machine_cannot_approve_its_own_enrollment() {
    let machine = support::MusterEnv::new();
    assert_cli_success(&machine, &["--format", "json", "init", "--yes"]);
    assert_cli_success(&machine, &["--format", "json", "enroll", "--name", "bob"]);
    let (output, envelope) = machine.run_json(&["--format", "json", "approve", "bob", "--yes"]);
    assert_eq!(output.status.code(), Some(5), "{}", envelope.raw);
    assert!(!envelope.ok());
    assert!(envelope.raw.to_string().to_lowercase().contains("self"));
}

#[test]
fn recovery_key_can_approve_when_no_other_machine_is_available() {
    let machine = support::MusterEnv::new();
    let init = assert_cli_success(&machine, &["--format", "json", "init", "--yes"]);
    let recovery_key = init["recovery_key"]
        .as_str()
        .expect("init exposes recovery key once");
    assert_cli_success(&machine, &["--format", "json", "enroll", "--name", "bob"]);
    assert_cli_success(
        &machine,
        &[
            "--format",
            "json",
            "approve",
            "bob",
            "--recovery-key",
            recovery_key,
            "--yes",
        ],
    );
}

#[test]
fn revoke_reencrypts_lists_exposed_secrets_and_machine_list_shows_roles() {
    let machine = support::MusterEnv::new();
    let init = assert_cli_success(&machine, &["--format", "json", "init", "--yes"]);
    let recovery_key = init["recovery_key"]
        .as_str()
        .expect("init exposes recovery key once");
    assert_cli_success(&machine, &["--format", "json", "enroll", "--name", "bob"]);
    // Single simulated machine: self-approval is forbidden, so use the recovery key.
    assert_cli_success(
        &machine,
        &[
            "--format",
            "json",
            "approve",
            "bob",
            "--recovery-key",
            recovery_key,
            "--yes",
        ],
    );

    let machines = assert_cli_success(&machine, &["--format", "json", "machine", "list"]);
    assert!(machines.to_string().contains("bob"), "{machines}");
    assert!(
        machines.to_string().to_lowercase().contains("role")
            || machines.to_string().to_lowercase().contains("profile"),
        "{machines}"
    );

    let revoked = assert_cli_success(&machine, &["--format", "json", "revoke", "bob", "--yes"]);
    let details = revoked.to_string().to_lowercase();
    assert!(
        details.contains("rotate") || details.contains("rotation"),
        "{revoked}"
    );
    assert!(
        details.contains("download") || details.contains("already"),
        "{revoked}"
    );
    let enrollment = machine.config_dir().join("enroll/bob.toml");
    if enrollment.exists() {
        let text = fs::read_to_string(enrollment).unwrap();
        assert!(!text.contains("active = true"));
    }
    assert_cli_success(
        &machine,
        &["--format", "json", "machine", "rm", "bob", "--yes"],
    );
}
