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
