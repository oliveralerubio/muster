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
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn add_secret(env: &MusterEnv, reference: &str, value: &str) {
    let out = env
        .run(&[
            "--format",
            "json",
            "secret",
            "add",
            reference,
            "--value-stdin",
        ])
        .write_stdin(value.as_bytes())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn epoch(env: &MusterEnv) -> u64 {
    ok(env, &["trust", "show"])["result"]["fleet_state"]["epoch"]
        .as_u64()
        .expect("epoch")
}

fn state_epoch_on_disk(config: &std::path::Path) -> u64 {
    let text = std::fs::read_to_string(config.join("fleet.state.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["payload"]["epoch"]
        .as_u64()
        .unwrap()
}

#[test]
fn init_pins_epoch_one_and_every_mutation_resigns_in_the_same_commit() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes"]);
    assert_eq!(epoch(&env), 1);
    let shown = ok(&env, &["trust", "show"]);
    assert_eq!(shown["result"]["status"], "verified");
    assert_eq!(shown["result"]["trust_pin"]["epoch"], 1);

    add_secret(&env, "app/api", "SEED-value-one");
    assert_eq!(epoch(&env), 2);
    // The signed state is part of the same commit as the vault change.
    let config = env.config_dir();
    let changed = git(&config, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(changed.contains("fleet.state.json"), "{changed}");
    assert!(changed.contains("vault/app.age"), "{changed}");
    assert_eq!(git(&config, &["status", "--porcelain"]), "");

    // Nothing leaks: no seed, secret value or private key in outputs or the state.
    let shown = ok(&env, &["trust", "show"]).to_string();
    let seed = std::fs::read_to_string(state_dir(&env).join("signing.key")).unwrap();
    assert!(!shown.contains(seed.trim()));
    assert!(!shown.contains("SEED-value-one"));
    assert!(!shown.contains("AGE-SECRET-KEY"));
}

#[test]
fn rollback_in_a_new_descendant_commit_is_refused_and_nothing_is_applied() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    let config = alice.config_dir();
    let epoch_one_commit = git(&config, &["rev-parse", "HEAD"]);
    add_secret(&alice, "app/api", "SEED-value-one");
    ok(&alice, &["sync", "--push"]);
    assert_eq!(epoch(&alice), 2);

    // An attacker with write access recommits the epoch-1 tree on top of history.
    let clone = TempDir::new().unwrap();
    let work = clone.path().join("w");
    git(
        clone.path(),
        &["clone", "--quiet", remote, work.to_str().unwrap()],
    );
    git(
        &work,
        &["checkout", "--quiet", &epoch_one_commit, "--", "."],
    );
    git(&work, &["rm", "-r", "--quiet", "--cached", "vault"]);
    let _ = std::fs::remove_dir_all(work.join("vault/app.age"));
    let _ = std::fs::remove_file(work.join("vault/app.age"));
    let _ = std::fs::remove_file(work.join("vault/app.meta.json"));
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "--quiet", "-m", "revert to old state"]);
    git(&work, &["push", "--quiet", "origin", "HEAD"]);

    let head_before = git(&config, &["rev-parse", "HEAD"]);
    let (code, raw) = fail(&alice, &["sync", "--pull"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("rollback"), "{raw}");
    // The repo keeps its last good state.
    assert_eq!(git(&config, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(state_epoch_on_disk(&config), 2);
    assert!(config.join("vault/app.age").exists());
}

#[test]
fn deleting_the_signed_state_while_a_pin_exists_is_refused() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes"]);
    std::fs::remove_file(env.config_dir().join("fleet.state.json")).unwrap();
    let (code, raw) = fail(&env, &["status"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("downgrade"), "{raw}");
    // Never silently re-initialized, not even by a mutating command.
    let (code, _) = fail(&env, &["secret", "rm", "app/none", "--yes"]);
    assert_eq!(code, 5);
    assert!(!env.config_dir().join("fleet.state.json").exists());
}

#[test]
fn legacy_repo_migrates_on_the_first_mutating_command() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "old"]);
    // Rewind to a repo from before signed state.
    let config = env.config_dir();
    std::fs::remove_file(config.join("fleet.state.json")).unwrap();
    std::fs::remove_file(state_dir(&env).join("trust.json")).unwrap();
    std::fs::remove_file(state_dir(&env).join("signing.key")).unwrap();
    let text = manifest_text(&env)
        .lines()
        .filter(|l| !l.trim_start().starts_with("signing_key"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(config.join("muster.toml"), text).unwrap();

    // Reads work on a legacy repo and do not migrate it.
    ok(&env, &["status"]);
    assert!(!config.join("fleet.state.json").exists());

    let out = ok(&env, &["secret", "list"]);
    let _ = out;
    add_secret(&env, "app/api", "SEED-legacy");
    assert!(config.join("fleet.state.json").exists());
    assert!(state_dir(&env).join("signing.key").exists());
    assert!(manifest_text(&env).contains("musterpk1"));
    let (_, envelope) = env.run_json(&["--format", "json", "secret", "list"]);
    assert!(envelope.ok());
    ok(&env, &["trust", "show"]);
}

#[test]
fn hand_edit_of_machines_is_refused_until_trust_resign() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);
    let attacker = "age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
    let path = env.config_dir().join("muster.toml");
    let mut text = manifest_text(&env);
    text.push_str(&format!(
        "\n[machines.evil]\nrecipient = \"{attacker}\"\nplatform = \"linux-x86_64\"\nmuster_version = \"0.1.0\"\n"
    ));
    std::fs::write(&path, text).unwrap();

    let (code, raw) = fail(&env, &["status"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("muster trust resign"), "{raw}");
    // Without confirmation nothing is signed.
    let (code, _) = fail(&env, &["trust", "resign"]);
    assert_eq!(code, 2);
    assert_eq!(state_epoch_on_disk(&env.config_dir()), 1);

    let resigned = ok(&env, &["trust", "resign", "--yes"]);
    assert_eq!(resigned["result"]["epoch"], 2);
    ok(&env, &["status"]);
}

#[test]
fn new_machine_pins_the_signer_and_expect_signer_is_strict() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    let fingerprint = ok(&alice, &["trust", "show"])["result"]["fleet_state"]["signer_fingerprint"]
        .as_str()
        .unwrap()
        .to_string();

    let bob = MusterEnv::new();
    let (code, raw) = fail(
        &bob,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            "SHA256:0000000000000000",
        ],
    );
    assert_eq!(code, 5, "{raw}");
    assert!(!state_dir(&bob).join("trust.json").exists());
    assert!(
        !bob.config_dir().join("muster.toml").exists(),
        "refused pull is discarded"
    );

    let carol = MusterEnv::new();
    let joined = ok(
        &carol,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &fingerprint,
        ],
    );
    assert!(state_dir(&carol).join("trust.json").exists());
    let codes: Vec<&str> = joined["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"fleet_signer_pinned"), "{codes:?}");
}

#[test]
fn crafted_ciphertext_is_refused_on_get_and_on_sync() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    add_secret(&alice, "app/api", "SEED-real");
    ok(&alice, &["sync", "--push"]);
    let config = alice.config_dir();

    // Anyone with the public keys can build a validly encrypted group.
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(config.join("vault/app.meta.json")).unwrap())
            .unwrap();
    let recipients: Vec<String> = meta["recipients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect();
    let forge = TempDir::new().unwrap();
    let attacker = muster::identity::Identity::generate();
    let mut vault = muster::vault::Vault::open(forge.path()).unwrap();
    vault
        .add(
            "app",
            "api",
            muster::secrets::SecretValue::new("ATTACKER-CONTROLLED".into()),
            &recipients,
            &attacker,
        )
        .unwrap();
    std::fs::copy(
        forge.path().join("vault/app.age"),
        config.join("vault/app.age"),
    )
    .unwrap();

    let (code, raw) = fail(
        &alice,
        &["secret", "get", "native://app/api", "--reveal", "--yes"],
    );
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("'app'"), "{raw}");
    assert!(!raw.to_string().contains("ATTACKER-CONTROLLED"));
    let (code, raw) = fail(&alice, &["sync"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("'app'"), "{raw}");
}

#[test]
fn signed_approvals_require_valid_signatures_and_bound_fields_prevent_tampering() {
    let owner = MusterEnv::new();
    let second_admin = MusterEnv::new();
    let candidate = MusterEnv::new();
    let config = owner.config_dir();

    ok(&owner, &["init", "--yes", "--machine", "owner-laptop"]);
    ok(&owner, &["member", "add", "admin-two", "--role", "admin"]);
    ok(&owner, &["member", "approve", "admin-two", "--yes"]);
    ok(&owner, &["member", "add", "candidate", "--role", "member"]);
    ok(&owner, &["member", "approve", "candidate", "--yes"]);

    // Enroll and approve second admin's machine
    ok(
        &second_admin,
        &[
            "--config",
            config.to_str().unwrap(),
            "enroll",
            "--name",
            "admin-two-laptop",
        ],
    );
    ok(
        &owner,
        &[
            "approve",
            "admin-two-laptop",
            "--member",
            "admin-two",
            "--yes",
        ],
    );

    // Require 2 approvals
    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str("\n[team]\nrequired_approvals = 2\n");
    std::fs::write(config.join("muster.toml"), manifest).unwrap();
    ok(&owner, &["trust", "resign", "--yes"]);

    // Candidate enrolls
    ok(
        &candidate,
        &[
            "--config",
            config.to_str().unwrap(),
            "enroll",
            "--name",
            "candidate-laptop",
        ],
    );

    // 1. Owner approves (partial approval 1/2)
    let first = ok(
        &owner,
        &[
            "approve",
            "candidate-laptop",
            "--member",
            "candidate",
            "--yes",
        ],
    );
    assert_eq!(first["result"]["status"], "pending");
    assert_eq!(first["result"]["approvals_received"], 1);

    // Verify TOML carries signed approval with signature
    let req_path = config.join("enroll/candidate-laptop.toml");
    let req_toml = std::fs::read_to_string(&req_path).unwrap();
    assert!(req_toml.contains("signature = "));

    // 2. Tampering test: attacker edits the recipient in the pending enrollment file!
    let tampered: String = req_toml
        .lines()
        .map(|line| {
            if line.starts_with("recipient = ") {
                "recipient = \"age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\""
                    .to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&req_path, tampered).unwrap();

    // Second admin tries to approve: the tampered recipient invalidates the first approval!
    // So the first approval is ignored and second admin's approval only brings count to 1/2.
    let second = ok(
        &second_admin,
        &[
            "--config",
            config.to_str().unwrap(),
            "approve",
            "candidate-laptop",
            "--member",
            "candidate",
            "--yes",
        ],
    );
    assert_eq!(second["result"]["status"], "pending");
    assert_eq!(
        second["result"]["approvals_received"], 1,
        "tampered approval was dropped"
    );
}

#[test]
fn legacy_string_approvals_are_ignored_with_warning() {
    let owner = MusterEnv::new();
    let second_admin = MusterEnv::new();
    let candidate = MusterEnv::new();
    let config = owner.config_dir();

    ok(&owner, &["init", "--yes", "--machine", "owner-laptop"]);
    ok(&owner, &["member", "add", "admin-two", "--role", "admin"]);
    ok(&owner, &["member", "approve", "admin-two", "--yes"]);
    ok(&owner, &["member", "add", "candidate", "--role", "member"]);
    ok(&owner, &["member", "approve", "candidate", "--yes"]);
    ok(
        &second_admin,
        &[
            "--config",
            config.to_str().unwrap(),
            "enroll",
            "--name",
            "admin-two-laptop",
        ],
    );
    ok(
        &owner,
        &[
            "approve",
            "admin-two-laptop",
            "--member",
            "admin-two",
            "--yes",
        ],
    );

    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str("\n[team]\nrequired_approvals = 2\n");
    std::fs::write(config.join("muster.toml"), manifest).unwrap();
    ok(&owner, &["trust", "resign", "--yes"]);

    ok(
        &candidate,
        &[
            "--config",
            config.to_str().unwrap(),
            "enroll",
            "--name",
            "candidate-laptop",
        ],
    );

    // Inject legacy unsigned approval string into the request TOML
    let req_path = config.join("enroll/candidate-laptop.toml");
    let mut req_toml = std::fs::read_to_string(&req_path).unwrap();
    req_toml.push_str("\napprovals = [\"owner\"]\n");
    std::fs::write(&req_path, req_toml).unwrap();

    // Second admin approves: the legacy approval must NOT count toward quorum.
    let second = ok(
        &second_admin,
        &[
            "--config",
            config.to_str().unwrap(),
            "approve",
            "candidate-laptop",
            "--member",
            "candidate",
            "--yes",
        ],
    );
    assert_eq!(second["result"]["status"], "pending");
    assert_eq!(second["result"]["approvals_received"], 1);
    let diags: Vec<&str> = second["diagnostics"]
        .as_array()
        .map(|a| a.iter().filter_map(|d| d["code"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        diags.contains(&"legacy_approval_ignored"),
        "expected warning: {diags:?}"
    );
}
