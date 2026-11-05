//! Round 9 hardening: one authenticated transition rule for every consumer.
//! Adversarial setups are built by hand (raw files and git commits), not only
//! through the happy-path CLI.

mod support;

use serde_json::Value;
use support::MusterEnv;

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (output, envelope) = env.run_json(&full);
    assert!(
        output.status.success(),
        "{args:?}: {} {}",
        String::from_utf8_lossy(&output.stderr),
        envelope.raw
    );
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.raw
}

fn fail(env: &MusterEnv, args: &[&str]) -> (i32, Value) {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (output, envelope) = env.run_json(&full);
    assert!(!envelope.ok(), "{args:?} should fail: {}", envelope.raw);
    (output.status.code().unwrap_or(-1), envelope.raw)
}

fn manifest_text(env: &MusterEnv) -> String {
    std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap()
}

#[test]
fn doctor_with_invalid_signed_state_reports_security_skips_checks_and_refuses_fix() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);

    // Tamper with muster.toml directly without resigning -> invalid governance digest
    let path = env.config_dir().join("muster.toml");
    let mut text = manifest_text(&env);
    text.push_str(
        "\n[machines.evil]\nrecipient = \"age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\"\nplatform = \"linux-x86_64\"\nmuster_version = \"0.1.0\"\n",
    );
    std::fs::write(&path, text).unwrap();

    // doctor --fix --yes must refuse
    let (code, raw) = fail(&env, &["doctor", "--fix", "--yes"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("doctor --fix"), "{raw}");

    // doctor reports the invalid state and skips sensitive checks
    let (code, raw) = fail(&env, &["doctor", "--offline"]);
    assert_eq!(code, 1, "{raw}");
    let diags = raw["diagnostics"].to_string();
    assert!(diags.contains("fleet_state_invalid"), "{raw}");
    let checks = raw["result"]["checks"].to_string();
    for name in ["adapters", "keys", "providers", "provider_api_keys"] {
        let needle = format!("{{\"name\":\"{name}\",\"status\":\"skipped\"}}");
        assert!(checks.contains(&needle), "{name} must be skipped: {checks}");
    }
}

#[test]
fn missing_or_unparsable_manifest_fails_closed_while_a_pin_exists() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);
    ok(&env, &["status"]);
    let path = env.config_dir().join("muster.toml");
    let original = manifest_text(&env);

    // Missing: every security-consuming command and an unnamed reveal refuse.
    std::fs::remove_file(&path).unwrap();
    let (code, raw) = fail(&env, &["status"]);
    assert_eq!(code, 5, "{raw}");
    let (code, raw) = fail(
        &env,
        &["secret", "get", "native://db/pw", "--reveal", "--yes"],
    );
    assert_eq!(code, 5, "{raw}");

    // Unparsable: same.
    std::fs::write(&path, "this is [not toml").unwrap();
    let (code, raw) = fail(&env, &["status"]);
    assert_eq!(code, 5, "{raw}");
    let (code, raw) = fail(
        &env,
        &["secret", "get", "native://db/pw", "--reveal", "--yes"],
    );
    assert_ne!(code, 0, "{raw}");

    // Restored: back to normal.
    std::fs::write(&path, original).unwrap();
    ok(&env, &["status"]);
}

#[test]
fn poisoned_payload_recipient_refuses_rotate_and_never_encrypts_to_it() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);

    let out = env
        .run(&[
            "--format",
            "json",
            "secret",
            "add",
            "db/pw",
            "--value-stdin",
        ])
        .write_stdin(b"initial_secret")
        .output()
        .unwrap();
    assert!(out.status.success());

    let eve_recipient = "age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p";
    // Eve is an enrolled machine in the fleet (authorized fleet-wide), but NOT a reader of `db`.
    let mut manifest = manifest_text(&env);
    manifest.push_str(&format!(
        "
[machines.eve]
recipient = \"{eve_recipient}\"
platform = \"linux-x86_64\"
muster_version = \"0.1.0\"
"
    ));
    manifest.push_str(
        "
[groups.db.readers]
machines = [\"alpha\"]
",
    );
    std::fs::write(env.config_dir().join("muster.toml"), &manifest).unwrap();
    ok(&env, &["trust", "resign", "--yes"]);

    let meta_path = env.config_dir().join("vault/db.meta.json");
    let meta_str = std::fs::read_to_string(&meta_path).unwrap();
    let mut meta: Value = serde_json::from_str(&meta_str).unwrap();
    meta["recipients"]
        .as_array_mut()
        .unwrap()
        .push(Value::String(eve_recipient.to_string()));
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();

    let key_file =
        std::fs::read_to_string(env.path().join(".local/state/muster/identity.txt")).unwrap();
    let id: age::x25519::Identity = key_file
        .lines()
        .find(|l| l.starts_with("AGE-SECRET-KEY-"))
        .unwrap()
        .parse()
        .unwrap();
    let age_path = env.config_dir().join("vault/db.age");
    let age_bytes = std::fs::read(&age_path).unwrap();
    let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(&age_bytes[..])).unwrap();
    let mut decrypted = Vec::new();
    let mut reader = decryptor
        .decrypt(std::iter::once(&id as &dyn age::Identity))
        .unwrap();
    std::io::Read::read_to_end(&mut reader, &mut decrypted).unwrap();

    let mut payload: Value = serde_json::from_slice(&decrypted).unwrap();
    payload["recipients"]
        .as_array_mut()
        .unwrap()
        .push(Value::String(eve_recipient.to_string()));
    let new_payload = serde_json::to_vec(&payload).unwrap();

    let alpha_pub = id.to_public();
    let eve_pub: age::x25519::Recipient = eve_recipient.parse().unwrap();
    let recipients = vec![alpha_pub, eve_pub];
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
            .unwrap();
    let mut armored = Vec::new();
    let armor =
        age::armor::ArmoredWriter::wrap_output(&mut armored, age::armor::Format::AsciiArmor)
            .unwrap();
    let mut writer = encryptor.wrap_output(armor).unwrap();
    std::io::Write::write_all(&mut writer, &new_payload).unwrap();
    writer.finish().and_then(|a| a.finish()).unwrap();
    std::fs::write(&age_path, &armored).unwrap();

    // Re-sign the state so trust::guard verifies the vault hashes on disk.
    // This simulates the compromised admin having signed the vault-only update.
    ok(&env, &["trust", "resign", "--yes"]);

    // Now try to rotate: it MUST be refused with Security!
    let out = env
        .run(&[
            "--format",
            "json",
            "secret",
            "rotate",
            "db/pw",
            "--value-stdin",
        ])
        .write_stdin(b"new_secret_value")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    let err_str = String::from_utf8_lossy(&out.stdout);
    assert!(err_str.contains("muster group rekey"), "{err_str}");

    // Verify the ciphertext on disk was NOT encrypted to eve with new_secret_value
    let current_age = std::fs::read_to_string(&age_path).unwrap();
    assert!(!current_age.contains("new_secret_value"));
}

#[test]
fn concurrent_mutating_command_fails_with_another_command_is_running() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);

    let state = env.path().join(".local/state/muster");
    // Acquire the mutation lock as another process (using parent process PID)
    let parent = std::os::unix::process::parent_id();
    let lock_file = state.join("mutation.lock");
    std::fs::write(&lock_file, format!("{parent} 0\n")).unwrap();

    // A mutating command (secret add) must wait briefly and fail with Operational:
    // "another muster command is running"
    let out = env
        .run(&[
            "--format",
            "json",
            "secret",
            "add",
            "app/key",
            "--value-stdin",
        ])
        .env("MUSTER_LOCK_WAIT_MS", "100")
        .write_stdin(b"secret_val")
        .output()
        .unwrap();

    assert_ne!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("another muster command is running"),
        "{stdout}"
    );

    // Stale lock detection: if the lock has a dead PID, it is broken and command succeeds
    let mut dead = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    std::fs::write(&lock_file, format!("{dead_pid} 1\n")).unwrap();

    let out2 = env
        .run(&[
            "--format",
            "json",
            "secret",
            "add",
            "app/key",
            "--value-stdin",
        ])
        .env("MUSTER_LOCK_WAIT_MS", "100")
        .write_stdin(b"secret_val")
        .output()
        .unwrap();
    assert!(
        out2.status.success(),
        "{}",
        String::from_utf8_lossy(&out2.stderr)
    );
}

#[test]
fn read_only_command_preserves_live_revocation_stage_owned_by_another_command() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);

    let state = env.path().join(".local/state/muster");
    let tx_dir = state.join("revocation-tx");
    std::fs::create_dir_all(&tx_dir).unwrap();
    std::fs::write(tx_dir.join("staged_marker.txt"), b"live transaction stage").unwrap();

    // The mutation lock is held by another live process
    let parent = std::os::unix::process::parent_id();
    let lock_file = state.join("mutation.lock");
    std::fs::write(&lock_file, format!("{parent} 0\n")).unwrap();

    // A read-only command runs
    let out = env.run(&["--format", "json", "status"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The live staging directory MUST NOT be deleted by the read-only command!
    assert!(tx_dir.join("staged_marker.txt").exists());
}

#[test]
fn cosign_refuses_when_working_tree_does_not_match_payload_digests() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let (alice, bob, carol) = (MusterEnv::new(), MusterEnv::new(), MusterEnv::new());
    quorum_fleet(remote, &alice, &bob, &carol);

    // Alice proposes a governance change
    ok(&alice, &["member", "rm", "carol", "--yes"]);
    ok(&alice, &["sync", "--push"]);

    // Bob pulls the pending proposal
    ok(&bob, &["sync", "--pull"]);

    // Tamper with Bob's working tree muster.toml before cosigning
    let bob_manifest_path = bob.config_dir().join("muster.toml");
    let original = std::fs::read_to_string(&bob_manifest_path).unwrap();
    let tampered = original.replace("required_approvals = 2", "required_approvals = 1");
    std::fs::write(&bob_manifest_path, tampered).unwrap();

    // Bob tries to cosign: MUST refuse with Security BEFORE signing
    let (code, raw) = fail(&bob, &["trust", "cosign", "--yes"]);
    assert_eq!(code, 5, "{raw}");
    assert!(
        raw.to_string().contains("working tree does not match"),
        "{raw}"
    );

    // fleet.state.json on disk must not have been signed by Bob
    let state_file = std::fs::read_to_string(bob.config_dir().join("fleet.state.json")).unwrap();
    let state: Value = serde_json::from_str(&state_file).unwrap();
    assert_eq!(state["cosignatures"].as_array().map_or(0, |a| a.len()), 0);
}

fn bare_remote() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().expect("remote tempdir");
    let path = dir.path().join("central.git");
    let status = std::process::Command::new("git")
        .args(["init", "--bare", path.to_str().unwrap()])
        .status()
        .expect("git init --bare");
    assert!(status.success());
    (dir, path)
}

fn quorum_fleet(remote: &str, alice: &MusterEnv, bob: &MusterEnv, carol: &MusterEnv) -> String {
    ok(alice, &["init", "--yes", "--machine", "alice-box"]);
    ok(alice, &["sync", "--remote", remote]);
    let signer_a = support::fleet_signer(&alice.config_dir());

    ok(alice, &["member", "add", "bob", "--role", "admin"]);
    ok(alice, &["member", "approve", "bob", "--yes"]);
    ok(alice, &["member", "add", "carol", "--role", "admin"]);
    ok(alice, &["member", "approve", "carol", "--yes"]);

    ok(
        bob,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &signer_a,
        ],
    );
    ok(bob, &["enroll", "--name", "bob-box"]);
    ok(alice, &["sync", "--pull"]);
    ok(alice, &["approve", "bob-box", "--member", "bob", "--yes"]);

    ok(
        carol,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &signer_a,
        ],
    );
    ok(carol, &["enroll", "--name", "carol-box"]);
    ok(alice, &["sync", "--pull"]);
    ok(
        alice,
        &["approve", "carol-box", "--member", "carol", "--yes"],
    );

    let mut manifest = manifest_text(alice);
    manifest.push_str("\n[team]\nrequired_approvals = 2\n");
    std::fs::write(alice.config_dir().join("muster.toml"), manifest).unwrap();
    ok(alice, &["trust", "resign", "--yes"]);
    ok(alice, &["sync", "--push"]);

    ok(bob, &["sync", "--pull"]);
    ok(carol, &["sync", "--pull"]);
    signer_a
}
