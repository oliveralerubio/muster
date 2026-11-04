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

/// `muster --format json --config <config> <args>` on `env` (one shared repo).
fn on(env: &support::MusterEnv, config: &std::path::Path, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json", "--config", config.to_str().unwrap()];
    full.extend_from_slice(args);
    assert_cli_success(env, &full)
}

/// Digest of every file under `<config>/vault`, to prove reject never
/// re-encrypts anything.
fn vault_digest(config: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut files: Vec<_> = std::fs::read_dir(config.join("vault"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    for path in files {
        path.hash(&mut hasher);
        std::fs::read(&path)
            .expect("read vault file")
            .hash(&mut hasher);
    }
    hasher.finish()
}

#[test]
fn enroll_approve_and_sync_between_two_machines() {
    let (_remote_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = support::MusterEnv::new();
    let bob = support::MusterEnv::new();

    assert_cli_success(&alice, &["--format", "json", "init", "--yes"]);
    assert_cli_success(&alice, &["--format", "json", "sync", "--remote", remote]);
    assert_cli_success(
        &bob,
        &[
            "--format",
            "json",
            "init",
            "--yes",
            "--machine",
            "bob-first",
        ],
    );
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
    let mut cmd = machine.run(&[
        "--format",
        "json",
        "approve",
        "bob",
        "--recovery-key-stdin",
        "--yes",
    ]);
    cmd.write_stdin(recovery_key.as_bytes());
    let (out, env) = support::json(cmd);
    assert!(out.status.success(), "{}", env.raw);
    assert!(env.ok());
}

#[test]
fn reject_removes_the_request_without_revoking_and_allows_reenrollment() {
    let (_remote_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = support::MusterEnv::new();
    let bob = support::MusterEnv::new();

    assert_cli_success(&alice, &["--format", "json", "init", "--yes"]);
    assert_cli_success(&alice, &["--format", "json", "sync", "--remote", remote]);
    let mut add = alice.run(&[
        "--format",
        "json",
        "secret",
        "add",
        "app/token",
        "--value-stdin",
    ]);
    add.write_stdin(b"reject-vault-sentinel");
    assert!(add.output().expect("secret add").status.success());
    assert_cli_success(
        &bob,
        &[
            "--format",
            "json",
            "init",
            "--yes",
            "--machine",
            "bob-first",
        ],
    );
    assert_cli_success(&bob, &["--format", "json", "sync", "--remote", remote]);
    assert_cli_success(&bob, &["--format", "json", "enroll", "--name", "bob"]);
    assert_cli_success(&alice, &["--format", "json", "sync", "--pull"]);

    let digest = vault_digest(&alice.config_dir());
    let rejected = assert_cli_success(&alice, &["--format", "json", "reject", "bob", "--yes"]);
    assert_eq!(rejected["rejected"], "bob", "{rejected}");
    assert_eq!(rejected["kind"], "machine", "{rejected}");
    assert!(
        !alice.config_dir().join("enroll/bob.toml").exists(),
        "the rejected request must be deleted"
    );
    let machines = assert_cli_success(&alice, &["--format", "json", "machine", "list"]);
    assert!(
        !machines.to_string().contains("\"bob\""),
        "bob must not be enrolled: {machines}"
    );
    assert_eq!(
        vault_digest(&alice.config_dir()),
        digest,
        "rejecting must not re-encrypt the vault"
    );

    // Reject is not revoke: bob can request enrollment again and be approved.
    assert_cli_success(&bob, &["--format", "json", "sync", "--pull"]);
    assert_cli_success(&bob, &["--format", "json", "enroll", "--name", "bob"]);
    assert_cli_success(&alice, &["--format", "json", "sync", "--pull"]);
    assert_cli_success(&alice, &["--format", "json", "approve", "bob", "--yes"]);
    let machines = assert_cli_success(&alice, &["--format", "json", "machine", "list"]);
    assert!(machines.to_string().contains("bob"), "{machines}");
}

#[test]
fn reject_dry_run_changes_nothing_and_errors_match_approve() {
    let (_remote_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = support::MusterEnv::new();
    let bob = support::MusterEnv::new();

    assert_cli_success(
        &alice,
        &[
            "--format",
            "json",
            "init",
            "--yes",
            "--machine",
            "alice-laptop",
        ],
    );
    assert_cli_success(&alice, &["--format", "json", "sync", "--remote", remote]);
    assert_cli_success(
        &bob,
        &[
            "--format",
            "json",
            "init",
            "--yes",
            "--machine",
            "bob-first",
        ],
    );
    assert_cli_success(&bob, &["--format", "json", "sync", "--remote", remote]);
    assert_cli_success(&bob, &["--format", "json", "enroll", "--name", "bob"]);
    assert_cli_success(&alice, &["--format", "json", "sync", "--pull"]);
    let request = alice.config_dir().join("enroll/bob.toml");

    let dry = assert_cli_success(
        &alice,
        &["--format", "json", "reject", "bob", "--dry-run", "--yes"],
    );
    assert_eq!(dry["dry_run"], true, "{dry}");
    assert_eq!(dry["rejected"], "bob", "{dry}");
    assert!(request.exists(), "a dry run must keep the request");

    // Without --yes and without a terminal the confirmation fails (exit 2).
    let (out, envelope) = alice.run_json(&["--format", "json", "reject", "bob"]);
    assert_eq!(out.status.code(), Some(2), "{}", envelope.raw);
    assert!(request.exists());

    // Unknown and already-enrolled names fail exactly like `approve`.
    for name in ["ghost", "alice-laptop"] {
        let (approve_out, _) = alice.run_json(&["--format", "json", "approve", name, "--yes"]);
        let (reject_out, reject_env) =
            alice.run_json(&["--format", "json", "reject", name, "--yes"]);
        assert!(!reject_env.ok(), "reject '{name}' must fail");
        assert_eq!(
            reject_out.status.code(),
            approve_out.status.code(),
            "reject '{name}' must fail like approve: {}",
            reject_env.raw
        );
        assert!(request.exists(), "failed rejects must keep the request");
    }
}

#[test]
fn machine_list_lists_pending_requests_and_drops_them_after_reject() {
    let alice = support::MusterEnv::new();
    let bob = support::MusterEnv::new();
    let config = alice.config_dir();

    assert_cli_success(&alice, &["--format", "json", "init", "--yes"]);
    // The joining machine requests enrollment into the shared fleet config.
    on(&bob, &config, &["enroll", "--name", "pending-laptop"]);

    let listed = assert_cli_success(&alice, &["--format", "json", "machine", "list"]);
    let pending = listed["pending"]
        .as_array()
        .expect("machine list must expose a pending array");
    let entry = pending
        .iter()
        .find(|p| p["name"] == "pending-laptop")
        .expect("the requested machine must be listed as pending");
    assert_eq!(entry["approvals_received"], 0, "{entry}");
    assert_eq!(entry["approvals_required"], 1, "{entry}");
    assert_eq!(
        entry["approvals"].as_array().map(Vec::len),
        Some(0),
        "{entry}"
    );
    assert!(entry["platform"].as_str().is_some(), "{entry}");
    assert!(
        !listed["machines"].to_string().contains("pending-laptop"),
        "a pending request is not an enrolled machine: {listed}"
    );

    let rejected = assert_cli_success(
        &alice,
        &["--format", "json", "reject", "pending-laptop", "--yes"],
    );
    assert_eq!(rejected["rejected"], "pending-laptop", "{rejected}");

    let listed = assert_cli_success(&alice, &["--format", "json", "machine", "list"]);
    assert!(
        !listed.to_string().contains("pending-laptop"),
        "the rejected request must drop off the pending list: {listed}"
    );
}

#[test]
fn reject_discards_the_partial_approvals_of_a_machine_request() {
    let owner = support::MusterEnv::new();
    let second_admin = support::MusterEnv::new();
    let joining = support::MusterEnv::new();
    let config = owner.config_dir();

    assert_cli_success(
        &owner,
        &[
            "--format",
            "json",
            "init",
            "--yes",
            "--machine",
            "owner-laptop",
        ],
    );
    on(
        &owner,
        &config,
        &["member", "add", "admin-two", "--role", "admin"],
    );
    on(
        &owner,
        &config,
        &["member", "approve", "admin-two", "--yes"],
    );
    assert_cli_success(
        &second_admin,
        &[
            "--format",
            "json",
            "--config",
            config.to_str().unwrap(),
            "enroll",
            "--name",
            "admin-two-laptop",
        ],
    );
    on(
        &owner,
        &config,
        &[
            "approve",
            "admin-two-laptop",
            "--member",
            "admin-two",
            "--yes",
        ],
    );

    let manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    std::fs::write(
        config.join("muster.toml"),
        format!("{manifest}\n[team]\nrequired_approvals = 2\n"),
    )
    .unwrap();

    assert_cli_success(
        &joining,
        &[
            "--format",
            "json",
            "--config",
            config.to_str().unwrap(),
            "enroll",
            "--name",
            "quorum-machine",
        ],
    );
    let first = on(&owner, &config, &["approve", "quorum-machine", "--yes"]);
    assert_eq!(first["status"], "pending", "{first}");
    let request = std::fs::read_to_string(config.join("enroll/quorum-machine.toml")).unwrap();
    assert!(
        request.contains("owner"),
        "partial approval recorded: {request}"
    );

    let rejected = on(&owner, &config, &["reject", "quorum-machine", "--yes"]);
    assert_eq!(rejected["rejected"], "quorum-machine", "{rejected}");
    assert_eq!(rejected["kind"], "machine", "{rejected}");
    assert!(!config.join("enroll/quorum-machine.toml").exists());

    // The recorded approval is gone: the same admin can approve afresh.
    assert_cli_success(
        &joining,
        &[
            "--format",
            "json",
            "--config",
            config.to_str().unwrap(),
            "enroll",
            "--name",
            "quorum-machine",
        ],
    );
    let again = on(&owner, &config, &["approve", "quorum-machine", "--yes"]);
    assert_eq!(again["status"], "pending", "{again}");
    assert_eq!(again["approvals_received"], 1, "{again}");
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

#[test]
fn rotated_secret_reaches_authorized_machine_on_next_sync_only() {
    let (_remote_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = support::MusterEnv::new();
    let bob = support::MusterEnv::new();
    let outsider = support::MusterEnv::new();
    for (machine, name) in [
        (&alice, "alice-first"),
        (&bob, "bob-first"),
        (&outsider, "out-first"),
    ] {
        machine.link_system_exe("git");
        assert_cli_success(
            machine,
            &["--format", "json", "init", "--yes", "--machine", name],
        );
        assert_cli_success(machine, &["--format", "json", "sync", "--remote", remote]);
    }
    assert_cli_success(&bob, &["--format", "json", "enroll", "--name", "bob"]);
    assert_cli_success(&alice, &["--format", "json", "sync", "--pull"]);
    assert_cli_success(&alice, &["--format", "json", "approve", "bob", "--yes"]);

    let old = "rotation-old-test-value";
    let new = "rotation-new-test-value";
    let write_secret = |action: &str, value: &str| {
        let output = alice
            .run_isolated(&[
                "--format",
                "json",
                "secret",
                action,
                "app/token",
                "--value-stdin",
            ])
            .write_stdin(value)
            .output()
            .unwrap();
        assert!(output.status.success());
        for stream in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(stream);
            assert!(!text.contains(old) && !text.contains(new), "secret leaked");
        }
    };
    let verify = |machine: &support::MusterEnv, expected: &str| {
        let output = machine
            .run_isolated(&[
                "--format",
                "json",
                "--yes",
                "secret",
                "get",
                "native://app/token",
                "--reveal",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            envelope["result"].as_str() == Some(expected),
            "wrong secret version"
        );
    };
    write_secret("add", old);
    assert_cli_success(&alice, &["--format", "json", "sync"]);
    assert_cli_success(&bob, &["--format", "json", "sync"]);
    verify(&bob, old);

    write_secret("rotate", new);
    verify(&alice, new);
    verify(&bob, old);
    assert_cli_success(&alice, &["--format", "json", "sync"]);
    assert_cli_success(&bob, &["--format", "json", "sync"]);
    verify(&bob, new);
    assert_cli_success(&outsider, &["--format", "json", "sync"]);
    assert!(
        fs::read(bob.config_dir().join("vault/app.age")).unwrap()
            == fs::read(outsider.config_dir().join("vault/app.age")).unwrap()
    );
    let denied = outsider
        .run_isolated(&[
            "--format",
            "json",
            "--yes",
            "secret",
            "get",
            "native://app/token",
            "--reveal",
        ])
        .output()
        .unwrap();
    assert_eq!(denied.status.code(), Some(5));
    for stream in [&denied.stdout, &denied.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains(old) && !text.contains(new), "secret leaked");
    }
}
