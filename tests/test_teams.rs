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
fn member_reject_removes_a_pending_invitation_and_non_admins_are_refused() {
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

    // A plain member cannot reject invitations.
    denied(&alice, &config, &["member", "reject", "bob", "--yes"]);

    let rejected = ok(&owner, &config, &["member", "reject", "bob", "--yes"]);
    assert_eq!(rejected["rejected"], "bob", "{rejected}");
    assert_eq!(rejected["kind"], "member", "{rejected}");
    let members = ok(&owner, &config, &["member", "list"]);
    assert!(
        !members["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "bob"),
        "{members}"
    );
    let log = std::process::Command::new("git")
        .args(["-C", config.to_str().unwrap(), "log", "--format=%B"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.contains("reject member bob") && log.contains("Muster-Actor"),
        "{log}"
    );

    // Reject is not revoke: the invitation can be issued again from scratch.
    ok(
        &owner,
        &config,
        &["member", "add", "bob", "--role", "member"],
    );
    let manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    let bob_section = manifest
        .split("[members.bob]")
        .nth(1)
        .unwrap_or_else(|| panic!("bob must be invited again: {manifest}"));
    let bob_section = &bob_section[..bob_section.find("\n[").unwrap_or(bob_section.len())];
    assert!(!bob_section.contains("approvals"), "{manifest}");

    // Active and unknown names are not pending requests.
    let (out, envelope) = on(&owner, &config, &["member", "reject", "alice", "--yes"]);
    assert_eq!(out.status.code(), Some(2), "{}", envelope.raw);
    let (out, envelope) = on(&owner, &config, &["member", "reject", "ghost", "--yes"]);
    assert_eq!(out.status.code(), Some(2), "{}", envelope.raw);
}

#[test]
fn member_reject_cleans_partial_quorum_approvals() {
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

    let manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    std::fs::write(
        config.join("muster.toml"),
        format!("{manifest}\n[team]\nrequired_approvals = 2\n"),
    )
    .unwrap();
    ok(&owner, &config, &["trust", "resign", "--yes"]);

    ok(
        &owner,
        &config,
        &["member", "add", "charlie", "--role", "member"],
    );
    let first = ok(&owner, &config, &["member", "approve", "charlie", "--yes"]);
    assert_eq!(first["status"], "pending", "{first}");
    assert_eq!(first["approvals_received"], 1, "{first}");
    let manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(
        manifest.contains("approvals"),
        "approval stored: {manifest}"
    );

    let rejected = ok(&owner, &config, &["member", "reject", "charlie", "--yes"]);
    assert_eq!(rejected["rejected"], "charlie", "{rejected}");
    let members = ok(&owner, &config, &["member", "list"]);
    assert!(!members.to_string().contains("charlie"), "{members}");
    let manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(!manifest.contains("[members.charlie]"), "{manifest}");

    // The discarded approvals are gone: a fresh invitation starts from zero.
    ok(
        &owner,
        &config,
        &["member", "add", "charlie", "--role", "member"],
    );
    let again = ok(&owner, &config, &["member", "approve", "charlie", "--yes"]);
    assert_eq!(again["status"], "pending", "{again}");
    assert_eq!(again["approvals_received"], 1, "{again}");
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

    let mut cmd = owner.run(&[
        "--format",
        "json",
        "--config",
        config.to_str().unwrap(),
        "member",
        "rm",
        "alice",
        "--recovery-key-stdin",
        "--yes",
    ]);
    cmd.write_stdin(recovery_key.as_bytes());
    let (out, envelope) = support::json(cmd);
    assert!(out.status.success(), "{}", envelope.raw);
    assert!(envelope.ok());
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
    let manifest_toml = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(manifest_toml.contains("revoked_recipients = ["));
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
    ok(&owner, &config, &["trust", "resign", "--yes"]);

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
    ok(&owner, &config, &["trust", "resign", "--yes"]);
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
fn machine_status_tracks_convergence_and_apply_records_last_sync() {
    let owner = MusterEnv::new();
    let alice = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "alice", "member");
    join(&owner, &alice, "alice-laptop", "alice");

    let alice_row = |env: &MusterEnv| -> Value {
        let listed = ok(env, &config, &["machine", "list"]);
        listed["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "alice-laptop")
            .cloned()
            .expect("alice-laptop is listed")
    };
    let status_row = |env: &MusterEnv| -> Value {
        let status = ok(env, &config, &["status"]);
        status["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "alice-laptop")
            .cloned()
            .expect("alice-laptop is in status")
    };

    // Enrolled but never applied.
    let row = alice_row(&owner);
    assert_eq!(row["status"], "enrolled", "{row}");
    assert!(row["last_sync"].is_null(), "{row}");
    assert_eq!(status_row(&owner)["status"], "enrolled");

    // Applying records both the profile digest and the sync time.
    ok(&alice, &config, &["agents", "apply", "--yes"]);
    let row = alice_row(&owner);
    assert_eq!(row["status"], "active", "{row}");
    assert!(row["applied_profile"].is_string(), "{row}");
    let stamp = row["last_sync"].as_str().expect("last_sync is recorded");
    assert!(stamp.contains('T'), "RFC 3339 timestamp expected: {stamp}");
    let on_disk = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(on_disk.contains("last_sync"), "{on_disk}");
    assert_eq!(status_row(&owner)["status"], "active");

    // The team profile changes: the machine is behind until it applies again.
    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str("\n[agents.models]\nclaude = \"new-team-model\"\n");
    std::fs::write(config.join("muster.toml"), manifest).unwrap();
    ok(&owner, &config, &["trust", "resign", "--yes"]);
    assert_eq!(alice_row(&owner)["status"], "drifted");
    assert_eq!(status_row(&owner)["status"], "drifted");

    ok(&alice, &config, &["agents", "apply", "--yes"]);
    assert_eq!(alice_row(&owner)["status"], "active");
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
    add_member(&owner, "worker", "member");

    // Raise the quorum once a second admin can approve (a quorum of 2 with a
    // single admin could only be met with the recovery key).
    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str("\n[team]\nrequired_approvals = 2\n");
    std::fs::write(config.join("muster.toml"), manifest).unwrap();
    ok(&owner, &config, &["trust", "resign", "--yes"]);

    let enrollment = ok(
        &joining_machine,
        &config,
        &["enroll", "--name", "quorum-machine"],
    );
    assert_eq!(enrollment["status"], "pending", "{enrollment}");
    let request_id = "quorum-machine";
    let first = ok(
        &owner,
        &config,
        &["approve", request_id, "--member", "worker", "--yes"],
    );
    assert_eq!(first["status"], "pending", "{first}");
    assert_eq!(first["approvals_received"], 1, "{first}");
    assert_eq!(first["approvals_required"], 2, "{first}");

    let (out, duplicate) = on(
        &owner,
        &config,
        &["approve", request_id, "--member", "worker", "--yes"],
    );
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
        &["approve", request_id, "--member", "worker", "--yes"],
    );
    assert_eq!(approved["status"], "active", "{approved}");
    let machines = ok(&owner, &config, &["machine", "list"]);
    assert!(
        machines["machines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "quorum-machine" && m["status"] == "enrolled"),
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
            .any(|m| m["name"] == "default-quorum-machine" && m["status"] == "enrolled"),
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
    ok(&owner, &config, &["trust", "resign", "--yes"]);

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

#[test]
fn creating_a_group_preserves_unrelated_reader_drift() {
    let owner = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    ok(
        &owner,
        &config,
        &["group", "create", "prod", "--reader", "owner"],
    );
    add_secret(&owner, &config, "prod/token", "test-value");
    let ciphertext = std::fs::read(config.join("vault/prod.age")).unwrap();
    let metadata = std::fs::read(config.join("vault/prod.meta.json")).unwrap();
    let extra = muster::identity::Identity::generate().recipient();
    let mut manifest = muster::manifest::Manifest::load(&config).unwrap();
    manifest
        .groups
        .get_mut("prod")
        .unwrap()
        .readers
        .recipients
        .push(extra.clone());
    manifest.save(&config).unwrap();
    ok(&owner, &config, &["trust", "resign", "--yes"]);

    let (out, envelope) = on(&owner, &config, &["group", "create", "unrelated"]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert!(
        std::fs::read(config.join("vault/prod.age")).unwrap() == ciphertext,
        "an unrelated command must not re-encrypt prod"
    );
    assert_eq!(
        std::fs::read(config.join("vault/prod.meta.json")).unwrap(),
        metadata
    );
    assert_eq!(
        envelope.result()["reencrypted_groups"],
        serde_json::json!([])
    );
    let warning = envelope
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "reader_drift")
        .expect("reader drift must warn");
    assert_eq!(warning["severity"], "warning");
    let message = warning["message"].as_str().unwrap();
    assert!(message.contains("prod") && message.contains(&extra) && message.contains("gain"));
}

#[test]
fn explicit_group_rekey_previews_access_and_requires_confirmation() {
    let owner = MusterEnv::new();
    let bob = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "bob", "member");
    join(&owner, &bob, "bob-laptop", "bob");
    for group in ["prod", "other"] {
        ok(
            &owner,
            &config,
            &["group", "create", group, "--reader", "owner"],
        );
        add_secret(&owner, &config, &format!("{group}/token"), "test-value");
    }
    let prod = std::fs::read(config.join("vault/prod.age")).unwrap();
    let other = std::fs::read(config.join("vault/other.age")).unwrap();
    let mut manifest = muster::manifest::Manifest::load(&config).unwrap();
    let owner_recipient = manifest.machines["owner-laptop"].recipient.clone();
    let bob_recipient = manifest.machines["bob-laptop"].recipient.clone();
    let readers = &mut manifest.groups.get_mut("prod").unwrap().readers;
    readers.members.clear();
    readers.machines.push("bob-laptop".into());
    manifest
        .groups
        .get_mut("other")
        .unwrap()
        .readers
        .recipients
        .push(muster::identity::Identity::generate().recipient());
    manifest.save(&config).unwrap();
    ok(&owner, &config, &["trust", "resign", "--yes"]);
    let config_before = std::fs::read(config.join("muster.toml")).unwrap();

    let preview = ok(&owner, &config, &["group", "rekey", "prod", "--dry-run"]);
    assert_eq!(preview["gains_access"], serde_json::json!([bob_recipient]));
    assert_eq!(
        preview["loses_access"],
        serde_json::json!([owner_recipient])
    );
    assert_eq!(preview["reencrypted_groups"], serde_json::json!(["prod"]));
    let (out, refused) = on(&owner, &config, &["group", "rekey", "prod"]);
    assert_eq!(out.status.code(), Some(2), "{}", refused.raw);
    let message = refused.raw.to_string();
    assert!(message.contains("bob-laptop") && message.contains("owner-laptop"));
    assert!(std::fs::read(config.join("vault/prod.age")).unwrap() == prod);
    assert_eq!(
        std::fs::read(config.join("muster.toml")).unwrap(),
        config_before
    );

    let result = ok(&owner, &config, &["group", "rekey", "prod", "--yes"]);
    assert_eq!(result["gains_access"], preview["gains_access"]);
    assert_eq!(result["loses_access"], preview["loses_access"]);
    assert!(std::fs::read(config.join("vault/other.age")).unwrap() == other);
    ok(&bob, &config, &["secret", "get", "native://prod/token"]);
    let (out, _) = on(&owner, &config, &["secret", "get", "native://prod/token"]);
    assert!(!out.status.success(), "the removed reader must lose access");
    let applied = std::fs::read(config.join("vault/prod.age")).unwrap();
    let again = ok(&owner, &config, &["group", "rekey", "prod", "--yes"]);
    assert_eq!(again["reencrypted_groups"], serde_json::json!([]));
    assert!(std::fs::read(config.join("vault/prod.age")).unwrap() == applied);
}

#[test]
fn member_rm_fails_closed_when_sidecar_disagrees_with_ciphertext() {
    let owner = MusterEnv::new();
    let laptop = MusterEnv::new();
    let recovery_key = init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "alice", "member");
    join(&owner, &laptop, "alice-laptop", "alice");
    ok(
        &owner,
        &config,
        &["group", "create", "engineering", "--reader", "alice"],
    );
    add_secret(&owner, &config, "engineering/api-key", "TOP_SECRET");

    // Attacker tampers engineering.meta.json to remove alice's recipient
    let meta_path = config.join("vault/engineering.meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    meta["recipients"] = serde_json::json!([]);
    std::fs::write(&meta_path, meta.to_string()).unwrap();

    // Even if the tamperer re-signs the state, member rm reads the real
    // recipients from the ciphertext: it never silently skips the group.
    ok(&owner, &config, &["trust", "resign", "--yes"]);
    let mut cmd = owner.run(&[
        "--format",
        "json",
        "--config",
        config.to_str().unwrap(),
        "member",
        "rm",
        "alice",
        "--recovery-key-stdin",
        "--yes",
    ]);
    cmd.write_stdin(recovery_key.as_bytes());
    let (out, envelope) = support::json(cmd);
    assert_eq!(out.status.code(), Some(5), "{}", envelope.raw);
    // Nothing was committed: alice is still a member.
    let manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(manifest.contains("[members.alice]"), "{manifest}");
}

#[test]
fn removing_member_revokes_every_machine_it_requested() {
    let owner = MusterEnv::new();
    let carol_laptop = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "carol", "member");
    join(&owner, &carol_laptop, "carol-box", "carol");

    // Carol's machine is active and has recipient
    let manifest_before = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(manifest_before.contains("carol-box"));

    // Revoke member carol
    ok(&owner, &config, &["member", "rm", "carol", "--yes"]);

    // carol-box MUST be revoked and purged from machines
    let manifest_after = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(!manifest_after.contains("[machines.carol-box]"));
    assert!(manifest_after.contains("revoked_recipients"));
    assert!(manifest_after.contains("revoked_signing_keys"));

    let parsed: toml::Table = manifest_after.parse().unwrap();
    let machines = parsed.get("machines").and_then(|m| m.as_table()).unwrap();
    assert!(!machines.contains_key("carol-box"));
}

#[test]
fn approving_a_machine_in_team_mode_needs_the_member_it_joins() {
    let owner = MusterEnv::new();
    let laptop = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    add_member(&owner, "carol", "member");
    ok(&laptop, &config, &["enroll", "--name", "carol-box"]);
    let (output, envelope) = owner.run_json(&[
        "--format",
        "json",
        "--config",
        config.to_str().unwrap(),
        "approve",
        "carol-box",
        "--yes",
    ]);
    assert!(!output.status.success(), "{}", envelope.raw);
    assert!(
        envelope.raw.to_string().contains("--member"),
        "{}",
        envelope.raw
    );
    // Nothing was enrolled.
    let manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(!manifest.contains("[machines.carol-box]"));
}

#[test]
fn recovery_approvals_validated_by_same_rule_locally_and_by_peers() {
    let owner = MusterEnv::new();
    let second_admin = MusterEnv::new();
    let joining = MusterEnv::new();
    let recovery_key = init(&owner);
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
    add_member(&owner, "worker", "member");

    // Raise required_approvals to 2
    let mut manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    manifest.push_str(
        "
[team]
required_approvals = 2
",
    );
    std::fs::write(config.join("muster.toml"), manifest).unwrap();
    ok(&owner, &config, &["trust", "resign", "--yes"]);

    // Enrolling machine requests to join worker
    ok(&joining, &config, &["enroll", "--name", "worker-box"]);

    // Owner approves with recovery key via stdin (break-glass, 1 signature satisfies k=2)
    let out = owner
        .run(&[
            "--format",
            "json",
            "--config",
            config.to_str().unwrap(),
            "approve",
            "worker-box",
            "--member",
            "worker",
            "--recovery-key-stdin",
            "--yes",
        ])
        .write_stdin(recovery_key.as_bytes())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The state was signed with the recovery key as primary signer!
    let state_file = std::fs::read_to_string(config.join("fleet.state.json")).unwrap();
    let state: Value = serde_json::from_str(&state_file).unwrap();
    let manifest_parsed = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    let toml: toml::Table = manifest_parsed.parse().unwrap();
    let recovery_pub = toml["recovery"]["signing_key"].as_str().unwrap();
    assert_eq!(state["payload"]["signer"], recovery_pub);

    // Publisher's pin advanced
    let owner_pin =
        std::fs::read_to_string(owner.path().join(".local/state/muster/trust.json")).unwrap();
    assert!(owner_pin.contains("worker-box"));

    // Peer (second_admin) verifies and accepts the exact same state without pending!
    let trust = ok(&second_admin, &config, &["trust", "show"]);
    assert_eq!(trust["status"], "verified", "{trust}");
}
