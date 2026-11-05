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
    let signer = support::fleet_signer(&alice.config_dir());
    ok(
        &bob,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &signer,
        ],
    );
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
fn hand_edit_is_not_laundered_by_an_unlisted_mutating_command() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);
    let attacker = "age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
    let path = env.config_dir().join("muster.toml");
    let mut text = manifest_text(&env);
    text.push_str(&format!(
        "\n[machines.evil]\nrecipient = \"{attacker}\"\nplatform = \"linux-x86_64\"\nmuster_version = \"0.1.0\"\n"
    ));
    std::fs::write(&path, text).unwrap();

    let (code, raw) = fail(
        &env,
        &["profile", "set", "dev", "--toml", "[agents]\n", "--yes"],
    );
    assert_eq!(code, 5, "{raw}");
    assert_eq!(state_epoch_on_disk(&env.config_dir()), 1);
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
fn sync_expect_signer_refuses_unsigned_remote_repo() {
    let (_guard, remote) = bare_remote();
    let remote_str = remote.to_str().unwrap();

    // Push a commit with muster.toml but NO fleet.state.json (unsigned legacy repo).
    let pusher = TempDir::new().unwrap();
    let dir = pusher.path();
    git(dir, &["init"]);
    git(dir, &["remote", "add", "origin", remote_str]);
    std::fs::write(
        dir.join("muster.toml"),
        "version = 1
[machines.node1]
recipient = 'age1abc'
platform = 'linux-x86_64'
muster_version = '0.1.0'
",
    )
    .unwrap();
    git(dir, &["add", "muster.toml"]);
    git(dir, &["commit", "-m", "legacy unsigned commit"]);
    git(dir, &["push", "origin", "HEAD"]);

    // Client syncs with --expect-signer: must refuse with code 5
    let client = MusterEnv::new();
    let (code, raw) = fail(
        &client,
        &[
            "sync",
            "--remote",
            remote_str,
            "--pull",
            "--expect-signer",
            "musterpk10000000000000000000000000000000000000000000000000000000000000000",
        ],
    );
    assert_eq!(code, 5, "{raw}");
    let msg = raw["diagnostics"][0]["message"].as_str().unwrap();
    assert!(
        msg.contains("expected signed fleet state") && msg.contains("unsigned"),
        "{msg}"
    );
    assert!(!state_dir(&client).join("trust.json").exists());
}

#[test]
fn expect_signer_accepts_full_key_or_256bit_fingerprint_and_rejects_prefixes() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    let show = ok(&alice, &["trust", "show"])["result"]["fleet_state"].clone();
    let public_key = show["signer"].as_str().unwrap().to_string();
    let fingerprint = show["signer_fingerprint"].as_str().unwrap().to_string();

    // 1. Short public key prefix rejected
    let bob = MusterEnv::new();
    let (code, raw) = fail(
        &bob,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &public_key[..20],
        ],
    );
    assert_eq!(code, 5, "{raw}");

    // 2. Short 64-bit fingerprint prefix rejected
    let (code, raw) = fail(
        &bob,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &fingerprint[..23],
        ],
    );
    assert_eq!(code, 5, "{raw}");

    // 3. Full public key accepted
    let carol = MusterEnv::new();
    let joined = ok(
        &carol,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &public_key,
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

#[test]
fn resign_after_a_local_rollback_is_refused() {
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    let config = alice.config_dir();
    let epoch_one_commit = git(&config, &["rev-parse", "HEAD"]);
    add_secret(&alice, "app/api", "SEED-value-one");
    assert_eq!(epoch(&alice), 2);

    // The repo silently goes back to epoch 1 (the pin stays at 2).
    git(&config, &["reset", "--hard", "--quiet", &epoch_one_commit]);
    assert_eq!(state_epoch_on_disk(&config), 1);

    let (code, raw) = fail(&alice, &["trust", "resign", "--yes"]);
    assert_eq!(code, 5, "{raw}");
    assert!(
        raw.to_string().contains("cannot re-sign a rolled-back"),
        "{raw}"
    );
    // Nothing was signed: the repo still shows epoch 1 and the pin epoch 2.
    assert_eq!(state_epoch_on_disk(&config), 1);
    let pin = ok(&alice, &["trust", "show"]);
    assert_eq!(pin["result"]["trust_pin"]["epoch"], 2);
}

#[test]
fn tampered_sidecar_meta_json_is_refused_on_get_and_on_sync() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    add_secret(&alice, "app/api", "SEED-real");
    ok(&alice, &["sync", "--push"]);
    let config = alice.config_dir();

    // Attacker modifies app.meta.json to add a fake recipient or secret
    let meta_path = config.join("vault/app.meta.json");
    let mut meta: Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    meta["recipients"]
        .as_array_mut()
        .unwrap()
        .push(Value::String("age1evil".into()));
    std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

    let (code, raw) = fail(
        &alice,
        &["secret", "get", "native://app/api", "--reveal", "--yes"],
    );
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("app.meta.json"), "{raw}");

    let (code, raw) = fail(&alice, &["sync"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("app.meta.json"), "{raw}");
}

#[test]
fn non_security_mutating_command_refuses_when_repo_is_rolled_back() {
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    let config = alice.config_dir();
    let epoch_one_commit = git(&config, &["rev-parse", "HEAD"]);
    add_secret(&alice, "app/api", "SEED-value-one");
    assert_eq!(epoch(&alice), 2);

    // Attacker rolls back local repo to epoch 1 commit. Pin remains at 2.
    git(&config, &["reset", "--hard", "--quiet", &epoch_one_commit]);
    assert_eq!(state_epoch_on_disk(&config), 1);

    // Non-security mutating command must be refused.
    let (code, raw) = fail(
        &alice,
        &["profile", "set", "dev", "--toml", "[agents]\n", "--yes"],
    );
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("rollback refused"), "{raw}");
}

#[test]
fn doctor_flags_unconfirmed_tofu_pin_until_confirmed_with_expect_signer() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    let fingerprint = ok(&alice, &["trust", "show"])["result"]["fleet_state"]["signer_fingerprint"]
        .as_str()
        .unwrap()
        .to_string();

    // Bob holds a pin that was confirmed interactively on a terminal (TOFU,
    // not named with --expect-signer): simulate it by marking his pin unconfirmed.
    let bob = MusterEnv::new();
    let signer = support::fleet_signer(&alice.config_dir());
    ok(
        &bob,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &signer,
        ],
    );
    let pin_path = state_dir(&bob).join("trust.json");
    let mut pin: Value =
        serde_json::from_str(&std::fs::read_to_string(&pin_path).unwrap()).unwrap();
    pin["tofu"] = Value::Bool(true);
    std::fs::write(&pin_path, serde_json::to_string(&pin).unwrap()).unwrap();

    // Bob runs doctor: must flag unconfirmed pin with info diagnostic
    let (_, envelope) = bob.run_json(&["--format", "json", "doctor"]);
    let doc = envelope.raw;
    let diags: Vec<&str> = doc["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(
        diags.contains(&"trust_pin_unconfirmed"),
        "expected trust_pin_unconfirmed in {diags:?}"
    );

    // Finding contains the expected message with fingerprint
    let d = doc["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "trust_pin_unconfirmed")
        .unwrap();
    assert!(
        d["message"].as_str().unwrap().contains(&format!(
            "trust_pin_unconfirmed: verify signer fingerprint out-of-band: {fingerprint}"
        )),
        "{}",
        d["message"]
    );

    // Once Bob confirms the pin with --expect-signer, the diagnostic clears
    ok(&bob, &["sync", "--expect-signer", &fingerprint]);
    let (_, envelope_after) = bob.run_json(&["--format", "json", "doctor"]);
    let doc_after = envelope_after.raw;
    let diags_after: Vec<&str> = doc_after["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(
        !diags_after.contains(&"trust_pin_unconfirmed"),
        "unexpected trust_pin_unconfirmed after confirmation: {diags_after:?}"
    );
}

#[test]
fn resign_refuses_a_signer_that_is_not_an_admin() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);
    let path = env.config_dir().join("muster.toml");
    let mut text = manifest_text(&env);
    text.push_str("\n# hand edit\n");
    std::fs::write(&path, text).unwrap();
    // A signing key the manifest has never heard of.
    std::fs::write(state_dir(&env).join("signing.key"), "42".repeat(32)).unwrap();

    let (code, raw) = fail(&env, &["trust", "resign", "--yes"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("not an admin"), "{raw}");
}

#[test]
fn resign_reports_the_governance_diff() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);
    let attacker = "age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
    let path = env.config_dir().join("muster.toml");
    let mut text = manifest_text(&env);
    text.push_str(&format!(
        "\n[machines.evil]\nrecipient = \"{attacker}\"\nplatform = \"linux-x86_64\"\nmuster_version = \"0.1.0\"\n"
    ));
    std::fs::write(&path, text).unwrap();

    let resigned = ok(&env, &["trust", "resign", "--yes"]);
    let diff = resigned["result"]["governance_diff"].as_str().unwrap();
    assert!(diff.contains("+") && diff.contains("evil"), "{diff}");
}

#[test]
fn first_sync_of_a_fresh_machine_needs_expect_signer_when_not_interactive() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    let signer = support::fleet_signer(&alice.config_dir());

    let bob = MusterEnv::new();
    let (code, raw) = fail(&bob, &["sync", "--remote", remote, "--pull", "--yes"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("--expect-signer"), "{raw}");
    assert!(raw.to_string().contains("muster trust show"), "{raw}");
    assert!(!state_dir(&bob).join("trust.json").exists());
    assert!(
        !bob.config_dir().join("muster.toml").exists(),
        "refused pull is discarded"
    );

    ok(
        &bob,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &signer,
        ],
    );
    assert!(state_dir(&bob).join("trust.json").exists());
}

fn hand_clone(remote: &str, env: &MusterEnv) {
    let status = Command::new("git")
        .args(["clone", "--quiet", remote])
        .arg(env.config_dir())
        .status()
        .expect("git clone");
    assert!(status.success());
}

#[test]
fn first_use_is_confirmed_by_every_command_not_only_sync() {
    let strict_fail = |env: &MusterEnv, args: &[&str]| {
        let mut full = vec!["--format", "json"];
        full.extend_from_slice(args);
        let (output, envelope) = env.run_json_strict(&full);
        assert!(!envelope.ok(), "{args:?} should fail: {}", envelope.raw);
        (output.status.code().unwrap_or(-1), envelope.raw)
    };
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();
    let alice = MusterEnv::new();
    ok(&alice, &["init", "--yes"]);
    ok(&alice, &["sync", "--remote", remote]);
    let signer = support::fleet_signer(&alice.config_dir());

    // A hand-cloned repo on a machine with no pin: `status` must not pin silently.
    let bob = MusterEnv::new();
    hand_clone(remote, &bob);
    for args in [&["status"][..], &["secret", "requests"][..]] {
        let (code, raw) = strict_fail(&bob, args);
        assert_eq!(code, 5, "{args:?}: {raw}");
        assert!(raw.to_string().contains("--expect-signer"), "{raw}");
        assert!(!state_dir(&bob).join("trust.json").exists(), "{args:?}");
    }
    // `trust show` never pins either.
    let _ = bob.run_json_strict(&["--format", "json", "trust", "show"]);
    assert!(!state_dir(&bob).join("trust.json").exists());

    // A wrong signer is refused; the right one (global flag) pins.
    let (code, _) = strict_fail(&bob, &["status", "--expect-signer", "musterpk1deadbeef"]);
    assert_eq!(code, 5);
    ok(&bob, &["status", "--expect-signer", &signer]);
    assert!(state_dir(&bob).join("trust.json").exists());

    // The environment variable is the same thing as the flag.
    let carol = MusterEnv::new();
    hand_clone(remote, &carol);
    let out = carol
        .run_strict(&["--format", "json", "status"])
        .env("MUSTER_EXPECT_SIGNER", &signer)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(state_dir(&carol).join("trust.json").exists());
}

fn try_add_secret(env: &MusterEnv, reference: &str, value: &str) -> (i32, Value) {
    let mut cmd = env.run(&[
        "--format",
        "json",
        "secret",
        "add",
        reference,
        "--value-stdin",
    ]);
    cmd.write_stdin(value.as_bytes());
    let (output, envelope) = support::json(cmd);
    (output.status.code().unwrap_or(-1), envelope.raw)
}

/// A repo from before signed state, as `legacy_repo_migrates_...` builds it.
fn rewind_to_legacy(env: &MusterEnv) {
    let config = env.config_dir();
    std::fs::remove_file(config.join("fleet.state.json")).unwrap();
    std::fs::remove_file(state_dir(env).join("trust.json")).unwrap();
    let text = manifest_text(env)
        .lines()
        .filter(|l| !l.trim_start().starts_with("signing_key"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(config.join("muster.toml"), text).unwrap();
}

#[test]
fn legacy_migration_needs_confirmation_when_the_repo_has_a_remote() {
    let (_guard, remote) = bare_remote();
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "this-box"]);
    rewind_to_legacy(&env);
    git(
        &env.config_dir(),
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let (code, raw) = try_add_secret(&env, "app/token", "v");
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("without confirmation"), "{raw}");
    assert!(!env.config_dir().join("fleet.state.json").exists());
}

#[test]
fn legacy_migration_needs_confirmation_when_history_had_other_machines() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "this-box"]);
    let config = env.config_dir();
    // History: a second machine existed, then the manifest was pruned to this one.
    let original = manifest_text(&env);
    let attacker = "age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
    std::fs::write(
        config.join("muster.toml"),
        format!(
            "{original}\n[machines.other]\nrecipient = \"{attacker}\"\nplatform = \"linux\"\nmuster_version = \"0.1.0\"\nagents = []\n"
        ),
    )
    .unwrap();
    git(&config, &["commit", "-qam", "add other"]);
    std::fs::write(config.join("muster.toml"), &original).unwrap();
    git(&config, &["commit", "-qam", "prune"]);
    rewind_to_legacy(&env);
    let (code, raw) = try_add_secret(&env, "app/token", "v");
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("without confirmation"), "{raw}");
    assert!(!config.join("fleet.state.json").exists());
}

#[test]
fn resign_without_a_pin_is_refused_when_other_machines_are_enrolled() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);
    let attacker = "age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
    let mut text = manifest_text(&env);
    text.push_str(&format!(
        "\n[machines.evil]\nrecipient = \"{attacker}\"\nplatform = \"linux-x86_64\"\nmuster_version = \"0.1.0\"\n"
    ));
    std::fs::write(env.config_dir().join("muster.toml"), text).unwrap();
    std::fs::remove_file(state_dir(&env).join("trust.json")).unwrap();

    let (code, raw) = fail(&env, &["trust", "resign", "--yes"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("no trust pin"), "{raw}");
    let state = std::fs::read_to_string(env.config_dir().join("fleet.state.json")).unwrap();
    assert!(!state.contains("\"epoch\": 2"), "nothing was signed");
}

#[test]
fn resign_without_a_pin_on_a_single_machine_fleet_shows_the_full_diff() {
    let env = MusterEnv::new();
    ok(&env, &["init", "--yes", "--machine", "alpha"]);
    let mut text = manifest_text(&env);
    text.push_str("\n[secrets.\"native://app/key\"]\nallow = [\"pi\"]\n");
    std::fs::write(env.config_dir().join("muster.toml"), text).unwrap();
    std::fs::remove_file(state_dir(&env).join("trust.json")).unwrap();

    let resigned = ok(&env, &["trust", "resign", "--yes"]);
    let diff = resigned["result"]["governance_diff"].as_str().unwrap();
    assert!(diff.contains("native://app/key"), "{diff}");
    // Only the edit shows up: the diff is against the last signed manifest,
    // so the unchanged machine entry is not listed as added.
    assert!(!diff.contains("+    \"alpha\""), "{diff}");
}

#[test]
fn quorum_two_admin_a_removing_admin_b_stays_pending_until_cosigned() {
    let (_guard, remote) = bare_remote();
    let remote = remote.to_str().unwrap();

    let alice = MusterEnv::new();
    let bob = MusterEnv::new();
    let carol = MusterEnv::new();

    // 1. Setup: 3 admins in team mode, required_approvals = 2.
    ok(&alice, &["init", "--yes", "--machine", "alice-box"]);
    ok(&alice, &["sync", "--remote", remote]);
    let signer_a = support::fleet_signer(&alice.config_dir());

    ok(&alice, &["member", "add", "bob", "--role", "admin"]);
    ok(&alice, &["member", "approve", "bob", "--yes"]);
    ok(&alice, &["member", "add", "carol", "--role", "admin"]);
    ok(&alice, &["member", "approve", "carol", "--yes"]);

    // Enroll Bob and Carol
    ok(
        &bob,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &signer_a,
        ],
    );
    ok(&bob, &["enroll", "--name", "bob-box"]);
    ok(&alice, &["sync", "--pull"]);
    ok(&alice, &["approve", "bob-box", "--member", "bob", "--yes"]);

    ok(
        &carol,
        &[
            "sync",
            "--remote",
            remote,
            "--pull",
            "--expect-signer",
            &signer_a,
        ],
    );
    ok(&carol, &["enroll", "--name", "carol-box"]);
    ok(&alice, &["sync", "--pull"]);
    ok(&alice, &["approve", "carol-box", "--member", "carol", "--yes"]);

    // Raise required_approvals = 2.
    let mut manifest = manifest_text(&alice);
    manifest.push_str("\n[team]\nrequired_approvals = 2\n");
    std::fs::write(alice.config_dir().join("muster.toml"), manifest).unwrap();
    ok(&alice, &["trust", "resign", "--yes"]);
    ok(&alice, &["sync", "--push"]);

    // Bob and Carol sync and accept this pin.
    ok(&bob, &["sync", "--pull"]);
    ok(&carol, &["sync", "--pull"]);
    let pinned_bob = std::fs::read_to_string(state_dir(&bob).join("trust.json")).unwrap();
    assert!(pinned_bob.contains("bob-box"));

    // 2. Admin A (Alice) removes Admin B (Bob) ALONE.
    ok(&alice, &["member", "rm", "bob", "--yes"]);
    ok(&alice, &["sync", "--push"]);

    // 3. Carol pulls the proposal: she does NOT advance her pin, Bob is STILL an admin for her.
    let pull_env = ok(&carol, &["sync", "--pull"]);
    assert_eq!(pull_env["result"]["applied"], false, "{pull_env}");
    assert!(
        pull_env["diagnostics"]
            .to_string()
            .contains("governance_pending"),
        "{pull_env}"
    );

    // Carol's trust pin has NOT advanced: Bob is still an admin in her accepted state.
    let carol_pin = std::fs::read_to_string(state_dir(&carol).join("trust.json")).unwrap();
    let bob_pk = muster::signing::Signer::load(&state_dir(&bob))
        .unwrap()
        .unwrap()
        .public();
    assert!(
        carol_pin.contains(&bob_pk),
        "Bob must still be an authorized admin in Carol's accepted pin"
    );

    // Commands that would consume the new governance (like secret reads using new policies) fail closed.
    let (code, raw) = fail(&carol, &["status"]);
    assert_eq!(code, 5, "{raw}");
    assert!(raw.to_string().contains("governance change"), "{raw}");

    // Forged, non-admin and duplicate co-signatures do not count.
    let state_path = carol.config_dir().join("fleet.state.json");
    let original = std::fs::read_to_string(&state_path).unwrap();
    let mut state = muster::fleet_state::FleetState::load(&carol.config_dir())
        .unwrap()
        .unwrap();
    let alice_key = muster::signing::Signer::load(&state_dir(&alice)).unwrap().unwrap();
    let outsider_dir = tempfile::TempDir::new().unwrap();
    let outsider = muster::signing::Signer::load_or_create(outsider_dir.path()).unwrap();
    state.cosignatures.push(muster::fleet_state::Cosignature {
        signer: bob_pk.clone(),
        signature: "00".repeat(64),
    });
    state.add_cosignature(&outsider).unwrap();
    state.add_cosignature(&alice_key).unwrap();
    state.cosignatures.push(state.cosignatures.last().unwrap().clone());
    state.save(&carol.config_dir()).unwrap();
    let held = ok(&carol, &["sync", "--pull"]);
    assert!(held["diagnostics"].to_string().contains("1 of 2"), "{held}");
    assert_eq!(held["result"]["applied"], false, "{held}");
    std::fs::write(&state_path, original).unwrap();

    let (code, _) = fail(&alice, &["trust", "cosign", "--yes"]);
    assert_eq!(code, 5, "the proposer cannot co-sign its own proposal");

    // 4. Carol co-signs the governance change.
    let cosigned = ok(&carol, &["trust", "cosign", "--yes"]);
    assert_eq!(cosigned["result"]["cosigned"], true, "{cosigned}");
    assert_eq!(cosigned["result"]["accepted"], true, "{cosigned}");
    ok(&carol, &["sync", "--push"]);

    // Now Carol's pin HAS advanced and Bob is removed.
    let carol_pin_after = std::fs::read_to_string(state_dir(&carol).join("trust.json")).unwrap();
    assert!(!carol_pin_after.contains(&bob_pk));
}
