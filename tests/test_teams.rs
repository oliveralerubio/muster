//! Ticket #10 — team members, secret groups, governance, profile precedence and drift.
//!
//! Several simulated machines (each its own `MusterEnv`: HOME, state, identity)
//! share one config repository through `--config`, as they would through git.
mod support;

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::Value;
use support::{Envelope, MusterEnv};

/// `muster --format json --config <config> <args>` on `env`.
fn on(env: &MusterEnv, config: &Path, args: &[&str]) -> (Output, Envelope) {
    let config = config.to_str().unwrap();
    let mut full = vec!["--format", "json", "--config", config];
    full.extend_from_slice(args);
    env.run_json(&full)
}

fn ok(env: &MusterEnv, config: &Path, args: &[&str]) -> Value {
    let (out, envelope) = on(env, config, args);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

fn denied(env: &MusterEnv, config: &Path, args: &[&str]) {
    let (out, envelope) = on(env, config, args);
    assert_eq!(
        out.status.code(),
        Some(5),
        "{args:?} must be forbidden: {}",
        envelope.raw
    );
    assert!(!envelope.ok());
}

/// The admin's machine: `muster init` (single-person mode). Returns the recovery key.
fn init(owner: &MusterEnv) -> String {
    let result = ok(
        owner,
        &owner.config_dir(),
        &["init", "--yes", "--machine", "owner-laptop"],
    );
    result["recovery_key"].as_str().unwrap().to_string()
}

/// Enroll `machine` from `env` into the shared config repo; the owner approves it for `member`.
fn join(owner: &MusterEnv, env: &MusterEnv, machine: &str, member: &str) {
    let config = owner.config_dir();
    ok(env, &config, &["enroll", "--name", machine]);
    ok(
        owner,
        &config,
        &["approve", machine, "--member", member, "--yes"],
    );
}

/// Invite and approve `name` as `role`, with the owner as admin.
fn add_member(owner: &MusterEnv, name: &str, role: &str) {
    let config = owner.config_dir();
    ok(owner, &config, &["member", "add", name, "--role", role]);
    ok(owner, &config, &["member", "approve", name, "--yes"]);
}

fn add_secret(env: &MusterEnv, config: &Path, path: &str, value: &str) {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("muster"));
    let base = env.run(&[]);
    for (k, v) in base.get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let mut child = cmd
        .args(["--format", "json", "--config", config.to_str().unwrap()])
        .args(["secret", "add", path, "--value-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(value.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn member_invitation_remains_pending_until_admin_approval() {
    let owner = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();

    let added = ok(
        &owner,
        &config,
        &["member", "add", "alice", "--role", "member"],
    );
    assert_eq!(added["status"], "pending", "{added}");
    let members = ok(&owner, &config, &["member", "list"]);
    let alice = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "alice");
    assert_eq!(alice.unwrap()["status"], "pending", "{members}");
    // The person who ran init became the first admin.
    let owner_entry = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "admin");
    assert!(owner_entry.is_some(), "{members}");

    let approved = ok(&owner, &config, &["member", "approve", "alice", "--yes"]);
    assert_eq!(approved["status"], "active", "{approved}");
    let members = ok(&owner, &config, &["member", "list"]);
    let alice = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "alice");
    assert_eq!(alice.unwrap()["status"], "active", "{members}");
}

#[test]
fn group_readers_control_decryption_with_real_age_identities() {
    let owner = MusterEnv::new();
    let alice = MusterEnv::new();
    let bob = MusterEnv::new();
    let recovery_key = init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "alice", "member");
    add_member(&owner, "bob", "member");
    join(&owner, &alice, "alice-laptop", "alice");
    join(&owner, &bob, "bob-laptop", "bob");

    ok(
        &owner,
        &config,
        &["group", "create", "engineering", "--reader", "alice"],
    );
    add_secret(
        &owner,
        &config,
        "engineering/api-key",
        "REAL_AGE_TEST_SECRET",
    );

    let get = [
        "secret",
        "get",
        "native://engineering/api-key",
        "--reveal",
        "--yes",
    ];
    let value = ok(&alice, &config, &get);
    assert_eq!(value, "REAL_AGE_TEST_SECRET");
    let (out, envelope) = on(&bob, &config, &get);
    assert!(
        !out.status.success(),
        "non-reader Bob must not decrypt: {}",
        envelope.raw
    );
    assert!(!envelope.raw.to_string().contains("REAL_AGE_TEST_SECRET"));

    // Granting the group to Bob re-encrypts it for his machine. The owner is
    // not a reader, so only the recovery key can re-encrypt it.
    let grant = ["group", "grant", "engineering", "--reader", "bob", "--yes"];
    let (out, _) = on(&owner, &config, &grant);
    assert_eq!(out.status.code(), Some(5), "a non-reader cannot re-encrypt");
    let mut cmd = owner.run(&[
        "--format",
        "json",
        "--config",
        config.to_str().unwrap(),
        "group",
        "grant",
        "engineering",
        "--reader",
        "bob",
        "--recovery-key-stdin",
        "--yes",
    ]);
    cmd.write_stdin(recovery_key.as_bytes());
    let (out, envelope) = support::json(cmd);
    assert!(out.status.success(), "{}", envelope.raw);
    assert!(envelope.ok());
    assert_eq!(ok(&bob, &config, &get), "REAL_AGE_TEST_SECRET");
    let groups = ok(&owner, &config, &["group", "list"]);
    let engineering = &groups["groups"][0];
    assert_eq!(engineering["name"], "engineering", "{groups}");
    assert_eq!(
        engineering["readers"]["members"],
        serde_json::json!(["alice", "bob"])
    );
}

#[test]
fn revoking_member_revokes_all_associated_machines_and_lists_secrets_to_rotate() {
    let owner = MusterEnv::new();
    let laptop = MusterEnv::new();
    let desktop = MusterEnv::new();
    let recovery_key = init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "alice", "member");
    join(&owner, &laptop, "alice-laptop", "alice");
    join(&owner, &desktop, "alice-desktop", "alice");
    ok(
        &owner,
        &config,
        &["group", "create", "engineering", "--reader", "alice"],
    );
    add_secret(&owner, &config, "engineering/api-key", "BEFORE_REVOCATION");
    let get = ["secret", "get", "native://engineering/api-key"];
    ok(&desktop, &config, &get);

    let (out, envelope) = on(
        &owner,
        &config,
        &[
            "member",
            "rm",
            "alice",
            "--recovery-key",
            &recovery_key,
            "--yes",
        ],
    );
    assert!(out.status.success());
    assert!(envelope.ok());
    assert!(
        envelope.raw["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "insecure_recovery_key_argument"),
        "argv recovery key must warn: {}",
        envelope.raw
    );
    let revoked = envelope.result().clone();
    let mut machines: Vec<_> = revoked["revoked_machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap())
        .collect();
    machines.sort();
    assert_eq!(machines, ["alice-desktop", "alice-laptop"], "{revoked}");
    assert_eq!(
        revoked["exposed_secrets"][0]["reference"],
        "native://engineering/api-key"
    );
    assert!(revoked["rotation_advice"]
        .as_str()
        .unwrap()
        .contains("Rotate"));

    for machine in [&laptop, &desktop] {
        let (out, envelope) = on(machine, &config, &get);
        assert!(
            !out.status.success(),
            "revoked machine still decrypts: {}",
            envelope.raw
        );
    }
    let fleet = ok(&owner, &config, &["machine", "list"]);
    assert_eq!(names(&fleet["machines"]), ["owner-laptop"]);
}

#[test]
fn personal_layers_apply_over_the_team_profile_in_fixed_precedence() {
    let owner = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "alice", "member");
    // Profiles are declared in the team manifest; the owner is a reader of `eng`.
    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str(
        r#"
[agents]
enabled = ["claude"]

[agents.models]
claude = "team-model"

[profiles.backend.agents.models]
claude = "group-model"

[profiles.mine.agents.models]
claude = "member-model"

[profiles.laptop.agents.models]
claude = "machine-model"

[groups.eng]
profile = "backend"

[groups.eng.readers]
members = ["owner"]
"#,
    );
    std::fs::write(config.join("muster.toml"), manifest).unwrap();

    let explain = ok(&owner, &config, &["plan", "--explain"])["explain"].clone();
    assert_eq!(
        explain["precedence"],
        serde_json::json!(["team", "group", "member", "machine", "project"])
    );
    let model = |owner: &MusterEnv| -> String {
        ok(owner, &config, &["agents", "apply", "--yes"]);
        let settings = std::fs::read_to_string(owner.path().join(".claude/settings.json")).unwrap();
        let settings: Value = serde_json::from_str(&settings).unwrap();
        settings["model"].as_str().unwrap().to_string()
    };
    assert_eq!(model(&owner), "group-model");

    ok(
        &owner,
        &config,
        &["member", "profile", "owner", "--profile", "mine"],
    );
    assert_eq!(model(&owner), "member-model");
    ok(
        &owner,
        &config,
        &["machine", "assign", "owner-laptop", "--profile", "laptop"],
    );
    assert_eq!(model(&owner), "machine-model");

    let explain = ok(&owner, &config, &["plan", "--explain"])["explain"].clone();
    let layers: Vec<_> = explain["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            format!(
                "{}:{}",
                l["layer"].as_str().unwrap(),
                l["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        layers,
        [
            "team:team",
            "group:eng",
            "member:owner",
            "machine:owner-laptop"
        ],
        "{explain}"
    );
}

#[test]
fn nobody_can_grant_themselves_access_and_several_admins_can_approve() {
    let owner = MusterEnv::new();
    let alice = MusterEnv::new();
    let carol = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "alice", "member");
    join(&owner, &alice, "alice-laptop", "alice");
    ok(
        &owner,
        &config,
        &["group", "create", "engineering", "--reader", "owner"],
    );

    // A plain member cannot change members, roles or readers.
    denied(
        &alice,
        &config,
        &["member", "role", "alice", "--role", "admin", "--yes"],
    );
    denied(
        &alice,
        &config,
        &[
            "group",
            "grant",
            "engineering",
            "--reader",
            "alice",
            "--yes",
        ],
    );
    denied(
        &alice,
        &config,
        &["member", "add", "mallory", "--role", "admin"],
    );

    // A second admin...
    ok(
        &owner,
        &config,
        &["member", "add", "carol", "--role", "admin"],
    );
    denied(&carol, &config, &["member", "approve", "carol", "--yes"]);
    ok(&owner, &config, &["member", "approve", "carol", "--yes"]);
    join(&owner, &carol, "carol-laptop", "carol");
    // ...cannot grant herself access, even as an admin...
    denied(
        &carol,
        &config,
        &[
            "group",
            "grant",
            "engineering",
            "--reader",
            "carol",
            "--yes",
        ],
    );
    // ...but can approve other people's changes.
    ok(
        &owner,
        &config,
        &["member", "add", "dave", "--role", "member"],
    );
    let approved = ok(&carol, &config, &["member", "approve", "dave", "--yes"]);
    assert_eq!(approved["approved_by"], "carol");
    ok(
        &owner,
        &config,
        &[
            "group",
            "grant",
            "engineering",
            "--reader",
            "carol",
            "--yes",
        ],
    );

    // Every administrative change is in git history with its actor.
    let log = std::process::Command::new("git")
        .args(["-C", config.to_str().unwrap(), "log", "--format=%B"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.contains("approve member dave") && log.contains("Muster-Actor: carol"),
        "{log}"
    );
    assert!(
        log.contains("grant engineering") && log.contains("Muster-Actor: owner"),
        "{log}"
    );
}

#[test]
fn team_status_reports_member_and_machine_profile_drift() {
    let owner = MusterEnv::new();
    let alice = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "alice", "member");
    join(&owner, &alice, "alice-laptop", "alice");
    ok(
        &owner,
        &config,
        &["member", "add", "bob", "--role", "member"],
    );

    let drift = |env: &MusterEnv| -> Value { ok(env, &config, &["status"])["drift"].clone() };
    let status = ok(&owner, &config, &["status"]);
    assert_eq!(names(&status["pending"]["members"]), ["bob"], "{status}");
    let machines = names(&drift(&owner)["machines"]);
    assert_eq!(
        machines,
        ["alice-laptop", "owner-laptop"],
        "nobody applied the profile yet"
    );
    assert_eq!(names(&drift(&owner)["members"]), ["alice", "owner"]);

    ok(&owner, &config, &["agents", "apply", "--yes"]);
    ok(&alice, &config, &["agents", "apply", "--yes"]);
    let now = drift(&owner);
    assert!(now["machines"].as_array().unwrap().is_empty(), "{now}");
    assert!(now["members"].as_array().unwrap().is_empty(), "{now}");

    // The team profile changes: every machine is out of date until it syncs.
    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str("\n[agents.models]\nclaude = \"new-team-model\"\n");
    std::fs::write(config.join("muster.toml"), manifest).unwrap();
    let now = drift(&owner);
    assert_eq!(
        names(&now["machines"]),
        ["alice-laptop", "owner-laptop"],
        "{now}"
    );
    assert_eq!(now["machines"][0]["reason"], "profile_outdated");
    ok(&alice, &config, &["agents", "apply", "--yes"]);
    let now = drift(&owner);
    assert_eq!(names(&now["machines"]), ["owner-laptop"], "{now}");
    assert_eq!(names(&now["members"]), ["owner"], "{now}");
}

#[test]
fn team_quorum_requires_multiple_admin_approvals() {
    let owner = MusterEnv::new();
    let second_admin = MusterEnv::new();
    let joining_machine = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    ok(
        &owner,
        &config,
        &["member", "add", "admin-two", "--role", "admin"],
    );
    ok(
        &owner,
        &config,
        &["member", "approve", "admin-two", "--yes"],
    );
    join(&owner, &second_admin, "admin-two-laptop", "admin-two");

    // Raise the quorum once a second admin can approve (a quorum of 2 with a
    // single admin could only be met with the recovery key).
    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str("\n[team]\nrequired_approvals = 2\n");
    std::fs::write(config.join("muster.toml"), manifest).unwrap();

    let enrollment = ok(
        &joining_machine,
        &config,
        &["enroll", "--name", "quorum-machine"],
    );
    assert_eq!(enrollment["status"], "pending", "{enrollment}");
    let request_id = "quorum-machine";
    let first = ok(&owner, &config, &["approve", request_id, "--yes"]);
    assert_eq!(first["status"], "pending", "{first}");
    assert_eq!(first["approvals_received"], 1, "{first}");
    assert_eq!(first["approvals_required"], 2, "{first}");

    let (out, duplicate) = on(&owner, &config, &["approve", request_id, "--yes"]);
    assert!(
        !out.status.success(),
        "duplicate approval must fail: {}",
        duplicate.raw
    );
    assert!(
        duplicate
            .raw
            .to_string()
            .contains("already_approved_by_caller"),
        "{}",
        duplicate.raw
    );

    let approved = ok(&second_admin, &config, &["approve", request_id, "--yes"]);
    assert_eq!(approved["status"], "active", "{approved}");
    let machines = ok(&owner, &config, &["machine", "list"]);
    assert!(
        machines["machines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "quorum-machine" && m["status"] == "active"),
        "{machines}"
    );
}

#[test]
fn team_quorum_defaults_to_single_approval_when_omitted() {
    let owner = MusterEnv::new();
    let joining_machine = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    let enrollment = ok(
        &joining_machine,
        &config,
        &["enroll", "--name", "default-quorum-machine"],
    );
    assert_eq!(enrollment["status"], "pending", "{enrollment}");
    let request_id = "default-quorum-machine";
    let approved = ok(&owner, &config, &["approve", request_id, "--yes"]);
    assert_eq!(approved["status"], "active", "{approved}");
    let machines = ok(&owner, &config, &["machine", "list"]);
    assert!(
        machines["machines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "default-quorum-machine" && m["status"] == "active"),
        "{machines}"
    );
}

#[test]
fn member_quorum_requires_multiple_admin_approvals() {
    let owner = MusterEnv::new();
    let second_admin = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    ok(
        &owner,
        &config,
        &["member", "add", "admin-two", "--role", "admin"],
    );
    ok(
        &owner,
        &config,
        &["member", "approve", "admin-two", "--yes"],
    );
    join(&owner, &second_admin, "admin-two-laptop", "admin-two");

    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str("\n[team]\nrequired_approvals = 2\n");
    std::fs::write(config.join("muster.toml"), manifest).unwrap();

    ok(
        &owner,
        &config,
        &["member", "add", "charlie", "--role", "member"],
    );
    let first = ok(&owner, &config, &["member", "approve", "charlie", "--yes"]);
    assert_eq!(first["status"], "pending", "{first}");
    assert_eq!(first["approvals_received"], 1, "{first}");
    assert_eq!(first["approvals_required"], 2, "{first}");

    let (out, duplicate) = on(&owner, &config, &["member", "approve", "charlie", "--yes"]);
    assert!(
        !out.status.success(),
        "duplicate approval must fail: {}",
        duplicate.raw
    );
    assert!(
        duplicate
            .raw
            .to_string()
            .contains("already_approved_by_caller"),
        "{}",
        duplicate.raw
    );

    let approved = ok(
        &second_admin,
        &config,
        &["member", "approve", "charlie", "--yes"],
    );
    assert_eq!(approved["status"], "active", "{approved}");
    assert_eq!(approved["approvals_received"], 2, "{approved}");
}
