mod support;

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use support::MusterEnv;
use tempfile::TempDir;

fn bare_remote() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("remote tempdir");
    let path = dir.path().join("central.git");
    let status = Command::new("git")
        .args(["init", "--bare", path.to_str().unwrap()])
        .status()
        .expect("git init --bare");
    assert!(status.success());
    (dir, path)
}

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

fn state_dir(env: &MusterEnv) -> PathBuf {
    env.path().join(".local/state/muster")
}

fn manifest_text(env: &MusterEnv) -> String {
    std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap()
}

#[test]
fn init_creates_private_signing_key_and_publishes_public_keys() {
    let env = MusterEnv::new();
    let init = ok(&env, &["init", "--yes", "--machine", "alpha"]);
    let key = state_dir(&env).join("signing.key");
    let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(state_dir(&env))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);

    let manifest: toml::Table = manifest_text(&env).parse().unwrap();
    let machine_key = manifest["machines"]["alpha"]["signing_key"]
        .as_str()
        .unwrap();
    assert!(machine_key.starts_with("musterpk1"));
    let recovery_key = manifest["recovery"]["signing_key"].as_str().unwrap();
    assert!(recovery_key.starts_with("musterpk1"));
    assert_ne!(machine_key, recovery_key);

    // No private material (seed hex, age secret) in the command output.
    let seed = std::fs::read_to_string(&key).unwrap();
    assert!(!init.to_string().contains(seed.trim()));
}

#[test]
fn enroll_request_carries_signing_key_and_nonce() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    let bob = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    ok(&bob, &["sync", "--remote", remote, "--pull"]);
    ok(&bob, &["enroll", "--name", "bob"]);
    let text = std::fs::read_to_string(bob.config_dir().join("enroll/bob.toml")).unwrap();
    let req: toml::Table = text.parse().unwrap();
    assert!(req["signing_key"]
        .as_str()
        .unwrap()
        .starts_with("musterpk1"));
    assert_eq!(req["nonce"].as_str().unwrap().len(), 32);
    let key = state_dir(&bob).join("signing.key");
    assert_eq!(
        std::fs::metadata(key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let _ = fail;
}
